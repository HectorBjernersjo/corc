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
            // Since ~2.1.2xx Claude keeps this static glyph on the title even
            // mid-turn, so it no longer distinguishes prompt from work — it
            // only says "Claude Code runs here". Reading it as Idle made every
            // working conversation show as idle; fall back to the transcript.
            // The interrupted-turn case this hint used to catch (a Mid
            // transcript with no completion record) still settles via
            // STALE_SECS.
            '✳' => None,
            // Startup, disabled/custom titles, and future formats use the
            // transcript fallback rather than being guessed at.
            _ => None,
        }
    }

    /// Read working/idle out of the visible pane content. While a turn runs,
    /// Claude draws a status line above the input box — a spinner glyph, a
    /// verb phrase ending in an ellipsis, and the elapsed time in parens
    /// ("✽ Baking… (3m 18s · ↓ 8.6k tokens)") — and removes it when the turn
    /// ends ("✻ Baked for 10m 58s"). With the title static, this is the live
    /// signal that catches an interrupted turn whose transcript stays Mid.
    fn content_hint(&self, pane_content: &str) -> Option<RuntimeHint> {
        if pane_content.lines().any(is_spinner_line) {
            return Some(RuntimeHint::Working);
        }
        // No spinner but Claude's input prompt is on screen: at rest. Other
        // content (dialogs, partial redraws) stays unknown, not guessed at.
        pane_content
            .lines()
            .any(|l| l.starts_with('❯'))
            .then_some(RuntimeHint::Idle)
    }

    /// `/cd` (v2.1.169+) relocates the session: transcript, `--resume` lookup
    /// and CLAUDE.md all follow the new directory. It is user-only inside the
    /// agent, which is exactly why corc types it (ADR-0003).
    fn cd_command(&self, dir: &Path) -> Option<String> {
        Some(format!("/cd {}", dir.display()))
    }

    fn meta_source(&self) -> Result<Box<dyn MetaSource>> {
        Ok(Box::new(Store::new()?.cached_as("claude")))
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

/// The turn-status line: a spinner glyph at column 0, a phrase ending with an
/// ellipsis, then the elapsed time — "✽ Baking… (3m 18s · ↓ 8.6k tokens)".
/// The glyph cycles, so accept any leading symbol that is not a body marker
/// (assistant bullets, tool results, the prompt) and let the "… (<digit>"
/// shape keep prose in the scrollback from matching.
fn is_spinner_line(line: &str) -> bool {
    let mut chars = line.chars();
    let Some(glyph) = chars.next() else {
        return false;
    };
    if glyph.is_alphanumeric() || glyph.is_whitespace() || "●⎿❯│─".contains(glyph) {
        return false;
    }
    if chars.next() != Some(' ') {
        return false;
    }
    line.split_once("… (")
        .is_some_and(|(_, rest)| rest.starts_with(|c: char| c.is_ascii_digit()))
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
    fn terminal_title_reports_working_or_unknown() {
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
        // The ✳ prompt glyph stays on the title mid-turn in current Claude
        // Code, so it carries no work/idle signal — transcript decides.
        assert_eq!(
            Claude.runtime_hint("✳ Review backup restore plan status"),
            None
        );
        assert_eq!(Claude.runtime_hint("Claude Code"), None);
        assert_eq!(Claude.runtime_hint("custom terminal title"), None);
        assert_eq!(Claude.runtime_hint(""), None);
    }

    /// Pane content, captured from real sessions: a turn in flight draws a
    /// spinner status line above the input box; an idle pane shows the prompt
    /// without one, including right after an interrupted turn.
    #[test]
    fn pane_content_reports_working_idle_or_unknown() {
        let working = "  Netto -10 rader trots ny doc-kommentar.\n\n\
                       ✽ Boondoggling… (3m 18s · ↓ 8.6k tokens)\n\n\
                       ─────────────\n❯ \n─────────────\n  Fable 5 | 103k tokens\n";
        assert_eq!(Claude.content_hint(working), Some(RuntimeHint::Working));
        // A different spinner glyph and verb, elapsed still in seconds.
        assert_eq!(
            Claude.content_hint("✻ Compacting conversation… (8s · esc to interrupt)\n❯ \n"),
            Some(RuntimeHint::Working)
        );

        // The completed form of the status line is not work.
        let idle = "  Bygget går igenom med 0 fel.\n\n✻ Baked for 10m 58s\n\n\
                    ─────────────\n❯ \n─────────────\n  Fable 5 | 153k tokens\n";
        assert_eq!(Claude.content_hint(idle), Some(RuntimeHint::Idle));

        // Prose containing an ellipsis-parens shape is body text (indented or
        // bulleted), never a status line.
        assert_eq!(
            Claude.content_hint("● Klart… (3 filer ändrade)\n  mer text… (2 saker)\n❯ \n"),
            Some(RuntimeHint::Idle)
        );

        // No prompt on screen (dialogs, partial redraws): unknown, so status
        // falls back to the transcript instead of guessing.
        assert_eq!(Claude.content_hint(""), None);
        assert_eq!(
            Claude.content_hint("Do you want to proceed?\n  1. Yes\n"),
            None
        );
    }

    #[test]
    fn cd_command_types_the_slash_command() {
        assert_eq!(
            Claude.cd_command(std::path::Path::new("/work/HRM/feature x")),
            Some("/cd /work/HRM/feature x".to_string())
        );
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
