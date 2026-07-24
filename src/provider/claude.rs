//! Claude Code. corc generates the session uuid itself and passes it to
//! `--session-id` (new) / `--resume` (revive); metadata comes from the jsonl
//! transcripts under `~/.claude/projects` (`discovery::Store`).

use super::Provider;
use crate::discovery::{MetaSource, Store};
use crate::status::RuntimeHint;
use crate::{state, usage};
use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

pub struct Claude;

impl Provider for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn binary(&self) -> &'static str {
        "claude"
    }

    fn new_session_id(&self, _dir: &Path) -> Result<String> {
        state::new_uuid()
    }

    fn spawn_args(&self, id: &str, resume: bool) -> Vec<String> {
        let flag = if resume { "--resume" } else { "--session-id" };
        vec![flag.to_string(), id.to_string()]
    }

    fn runtime_hint(&self, pane_title: &str) -> Option<RuntimeHint> {
        match pane_title.chars().next()? {
            // Claude animates its terminal title with fixed-width Braille
            // spinners while the main agent loop is active. Different work
            // phases use different patterns, so accept the non-blank Braille
            // block rather than one observed animation sequence.
            c if ('\u{2801}'..='\u{28ff}').contains(&c) => Some(RuntimeHint::Working),
            // At the input prompt Claude prefixes the conversation title with
            // this static glyph.
            '✳' => Some(RuntimeHint::Idle),
            // Startup, disabled/custom titles, and future formats use the
            // transcript fallback rather than being guessed at.
            _ => None,
        }
    }

    fn meta_source(&self) -> Result<Box<dyn MetaSource>> {
        Ok(Box::new(Store::new()?))
    }

    /// Plan usage from the OAuth usage endpoint, with the token Claude Code
    /// keeps in `~/.claude/.credentials.json`. corc only reads the token;
    /// refresh is Claude Code's job — an expired one just makes the fetch
    /// 401 and the menu keeps the previous snapshot.
    fn fetch_usage(&self) -> Option<Vec<usage::Entry>> {
        let token = access_token()?;
        let body = usage::curl_get(
            USAGE_URL,
            &[
                format!("Authorization: Bearer {token}"),
                "anthropic-beta: oauth-2025-04-20".to_string(),
            ],
        )?;
        let resp: UsageResponse = serde_json::from_slice(&body).ok()?;
        let entries: Vec<usage::Entry> = resp.limits.iter().filter_map(usage_entry).collect();
        (!entries.is_empty()).then_some(entries)
    }
}

#[derive(Deserialize)]
struct UsageResponse {
    limits: Vec<Limit>,
}

#[derive(Deserialize)]
struct Limit {
    kind: String,
    percent: f64,
    scope: Option<Scope>,
}

#[derive(Deserialize)]
struct Scope {
    model: Option<Model>,
}

#[derive(Deserialize)]
struct Model {
    display_name: Option<String>,
}

/// The OAuth access token Claude Code maintains.
fn access_token() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let raw = std::fs::read_to_string(format!("{home}/.claude/.credentials.json")).ok()?;
    let creds: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(
        creds
            .get("claudeAiOauth")?
            .get("accessToken")?
            .as_str()?
            .to_string(),
    )
}

/// Map one API limit to a menu entry. The payload already orders them
/// session, weekly, scoped; unknown kinds are dropped so new server-side
/// limit types never render as garbage.
fn usage_entry(limit: &Limit) -> Option<usage::Entry> {
    let label = match limit.kind.as_str() {
        "session" => "5h".to_string(),
        "weekly_all" => "wk".to_string(),
        "weekly_scoped" => limit
            .scope
            .as_ref()
            .and_then(|s| s.model.as_ref())
            .and_then(|m| m.display_name.as_deref())
            .unwrap_or("model")
            .to_lowercase(),
        _ => return None,
    };
    Some(usage::Entry {
        label,
        percent: limit.percent.clamp(0.0, 100.0).round() as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::{Claude, UsageResponse, usage_entry};
    use crate::provider::Provider;
    use crate::status::RuntimeHint;

    #[test]
    fn terminal_title_reports_working_idle_or_unknown() {
        for spinner in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
            assert_eq!(
                Claude.runtime_hint(&format!("{spinner} backup-restore")),
                Some(RuntimeHint::Working)
            );
        }
        assert_eq!(
            Claude.runtime_hint("⠂ platform-restore-cleanup-runbook"),
            Some(RuntimeHint::Working)
        );
        assert_eq!(
            Claude.runtime_hint("✳ Review backup restore plan status"),
            Some(RuntimeHint::Idle)
        );
        assert_eq!(Claude.runtime_hint("Claude Code"), None);
        assert_eq!(Claude.runtime_hint("custom terminal title"), None);
        assert_eq!(Claude.runtime_hint(""), None);
    }

    #[test]
    fn parses_the_three_usage_limits() {
        let json = r#"{"limits":[
            {"kind":"session","group":"session","percent":21,"severity":"normal","resets_at":"x","scope":null,"is_active":false},
            {"kind":"weekly_all","group":"weekly","percent":24,"severity":"normal","resets_at":"x","scope":null,"is_active":false},
            {"kind":"weekly_scoped","group":"weekly","percent":41,"severity":"normal","resets_at":"x",
             "scope":{"model":{"id":null,"display_name":"Fable"},"surface":null},"is_active":true},
            {"kind":"something_new","group":"?","percent":7,"severity":"normal","resets_at":"x","scope":null,"is_active":false}
        ]}"#;
        let resp: UsageResponse = serde_json::from_str(json).unwrap();
        let shown: Vec<(String, u8)> = resp
            .limits
            .iter()
            .filter_map(usage_entry)
            .map(|e| (e.label, e.percent))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("5h".to_string(), 21),
                ("wk".to_string(), 24),
                ("fable".to_string(), 41),
            ]
        );
    }
}
