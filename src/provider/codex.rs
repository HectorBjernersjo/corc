//! OpenAI Codex CLI (`codex`). Codex mints its session id internally and
//! only reveals it in its rollout jsonl — which it writes at the *first user
//! message*, not at process start. So a fresh conversation is keyed by a
//! provisional `pending-<uuid>` id and spawned as plain `codex`; every
//! refresh, `resolve_spawned_id` looks for a rollout matching the
//! conversation's cwd and spawn time and corc migrates the row to the real
//! id. Until then the conversation has no metadata, reads as empty, and is
//! discarded on leave like any untouched conversation (D17).
//!
//! Rollouts live at `~/.codex/sessions/YYYY/MM/DD/rollout-<local ts>-
//! <uuid>.jsonl`; `codex resume <uuid>` appends to the same file, so the
//! incremental `discovery::Store` machinery is reused with a Codex line
//! reader: `task_started` / `task_complete` / `turn_aborted` events map onto
//! turn state, and the first `user_message` becomes the title stand-in
//! (Codex generates no title).

use super::Provider;
use crate::discovery::{self, Meta, MetaSource, Store, TurnState};
use crate::{state, usage};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Marks a corc-minted provisional id, so it can never be mistaken for a
/// real session uuid (and `spawn_args` / `locate_rollout` can tell them
/// apart).
const PENDING_PREFIX: &str = "pending-";

pub struct Codex;

impl Provider for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn binary(&self) -> &'static str {
        "codex"
    }

    fn new_session_id(&self, _dir: &Path) -> Result<String> {
        // Codex can't be told an id and won't reveal its own until the
        // rollout file exists; key the conversation provisionally.
        Ok(format!("{PENDING_PREFIX}{}", state::new_uuid()?))
    }

    fn spawn_args(&self, id: &str, resume: bool) -> Vec<String> {
        if resume && !id.starts_with(PENDING_PREFIX) {
            vec!["resume".to_string(), id.to_string()]
        } else {
            // Fresh spawn under a provisional id: plain `codex` in the cwd.
            // Reviving a still-pending conversation also lands here — there
            // is no real session to resume, so a new one starts and the
            // pending id then resolves to it.
            Vec::new()
        }
    }

    fn is_pending(&self, id: &str) -> bool {
        id.starts_with(PENDING_PREFIX)
    }

    fn resolve_spawned_id(
        &self,
        dir: &Path,
        since: SystemTime,
        taken: &[String],
    ) -> Result<Option<String>> {
        let since = since
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Ok(resolve(&sessions_root()?, dir, since, taken))
    }

    fn meta_source(&self) -> Result<Box<dyn MetaSource>> {
        Ok(Box::new(
            Store::with(sessions_root()?, locate_rollout, apply).cached_as("codex"),
        ))
    }

    /// Plan usage from the ChatGPT backend's usage endpoint, with the OAuth
    /// token Codex keeps in `~/.codex/auth.json`. corc only reads the token;
    /// refresh is Codex's job — an expired one just makes the fetch 401 and
    /// the menu keeps the previous snapshot. Codex exposes two windows: a
    /// primary short one (5h) and a secondary weekly one.
    fn fetch_usage(&self) -> Option<Vec<usage::Entry>> {
        let (token, account_id) = codex_tokens()?;
        let body = usage::curl_get(
            USAGE_URL,
            &[
                format!("Authorization: Bearer {token}"),
                format!("chatgpt-account-id: {account_id}"),
            ],
        )?;
        let resp: UsageResponse = serde_json::from_slice(&body).ok()?;
        let rl = resp.rate_limit?;
        let entries: Vec<usage::Entry> = [rl.primary_window, rl.secondary_window]
            .into_iter()
            .flatten()
            .map(window_entry)
            .collect();
        (!entries.is_empty()).then_some(entries)
    }
}

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

#[derive(Deserialize)]
struct UsageResponse {
    rate_limit: Option<RateLimit>,
}

#[derive(Deserialize)]
struct RateLimit {
    primary_window: Option<Window>,
    secondary_window: Option<Window>,
}

#[derive(Deserialize)]
struct Window {
    used_percent: f64,
    limit_window_seconds: Option<u64>,
}

/// The OAuth access token and account id Codex maintains.
fn codex_tokens() -> Option<(String, String)> {
    let home = std::env::var("HOME").ok()?;
    let raw = std::fs::read_to_string(format!("{home}/.codex/auth.json")).ok()?;
    let auth: Value = serde_json::from_str(&raw).ok()?;
    let tokens = auth.get("tokens")?;
    Some((
        tokens.get("access_token")?.as_str()?.to_string(),
        tokens.get("account_id")?.as_str()?.to_string(),
    ))
}

/// Label a rate-limit window by its span — `5h` for sub-day windows, `wk`
/// for the seven-day one, `Nd` for anything else — mirroring the labels the
/// Claude readout uses so the two providers' rows read the same.
fn window_entry(w: Window) -> usage::Entry {
    let label = match w.limit_window_seconds {
        Some(s) if s < 24 * 3600 => format!("{}h", s.div_ceil(3600)),
        Some(s) if s == 7 * 24 * 3600 => "wk".to_string(),
        Some(s) => format!("{}d", s.div_ceil(24 * 3600)),
        None => "?".to_string(),
    };
    usage::Entry {
        label,
        percent: w.used_percent.clamp(0.0, 100.0).round() as u8,
    }
}

fn sessions_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".codex/sessions"))
}

/// Fold one rollout line into the conversation's metadata. Only `event_msg`
/// records matter: task_started / task_complete carry unix-second turn
/// boundaries, turn_aborted is the Esc interrupt (which never produces a
/// task_complete, so it must end the turn itself), and the first
/// user_message becomes the title stand-in.
fn apply(meta: &mut Meta, v: &Value) {
    if v["type"].as_str() != Some("event_msg") {
        return;
    }
    // Codex writes the rollout file only once a first message exists, so any
    // event at all means the conversation has a real exchange.
    meta.has_content = true;
    let p = &v["payload"];
    match p["type"].as_str() {
        Some("task_started") => {
            meta.turn_state = TurnState::Mid;
            if let Some(ts) = p["started_at"].as_u64() {
                meta.turn_started_at = Some(ts);
            }
            meta.turn_completed_at = None;
        }
        Some("user_message") => {
            if meta.first_prompt.is_none()
                && let Some(text) = p["message"].as_str()
            {
                meta.first_prompt = discovery::title_line(text);
            }
        }
        Some("item_completed") if p["item"]["type"].as_str() == Some("UserMessage") => {
            if meta.first_prompt.is_none()
                && let Some(text) = p["item"]["content"]
                    .as_array()
                    .and_then(|content| content.iter().find_map(|part| part["text"].as_str()))
            {
                meta.first_prompt = discovery::title_line(text);
            }
        }
        Some("task_complete") => {
            meta.turn_state = TurnState::Complete;
            meta.turn_completed_at = p["completed_at"].as_u64().or_else(|| line_timestamp(v));
        }
        Some("turn_aborted") => {
            meta.turn_state = TurnState::Complete;
            meta.turn_completed_at = line_timestamp(v);
        }
        _ => {}
    }
}

/// A record's own write timestamp (ISO, top level) as unix seconds.
fn line_timestamp(v: &Value) -> Option<u64> {
    discovery::parse_iso8601(v["timestamp"].as_str()?)
}

/// `Store` locate fn: find `rollout-<local ts>-<id>.jsonl` by scanning day
/// directories newest first. A provisional id never matches a real filename,
/// so it is skipped outright rather than scanning the whole tree every poll.
fn locate_rollout(root: &Path, _cwd: &Path, id: &str) -> Option<PathBuf> {
    if id.starts_with(PENDING_PREFIX) {
        return None;
    }
    let suffix = format!("-{id}.jsonl");
    for day in day_dirs_desc(root) {
        let Ok(entries) = fs::read_dir(&day) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().ends_with(&suffix) {
                return Some(entry.path());
            }
        }
    }
    None
}

/// The real session behind a pending conversation: among rollouts written
/// at/after the spawn, the *earliest* whose `session_meta` records the
/// conversation's cwd and a start at/after `since` (small slack for clock
/// jitter), skipping ids other conversations already claimed. A manual
/// `codex` started in the same directory in the same instant could in
/// principle be adopted instead — accepted; the window is seconds wide.
fn resolve(root: &Path, cwd: &Path, since: u64, taken: &[String]) -> Option<String> {
    // Day directories are named in local time while `since` is unix; a day
    // of slack absorbs the offset (and a session spawned just before
    // midnight).
    let cutoff = discovery::civil_from_days((since.saturating_sub(86_400) / 86_400) as i64);
    let mut best: Option<(u64, String)> = None;
    for day in day_dirs_desc(root) {
        let Some(date) = day_date(&day) else {
            continue;
        };
        if date < cutoff {
            break; // newest first: everything from here on is older
        }
        let Ok(entries) = fs::read_dir(&day) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = rollout_id(&name) else {
                continue;
            };
            if taken.iter().any(|t| t == id) {
                continue;
            }
            // Cheap pre-filter: the file is only ever written at/after the
            // session started, so an older mtime can't be a match.
            let fresh = fs::metadata(entry.path())
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .is_some_and(|d| d.as_secs() + 5 >= since);
            if !fresh {
                continue;
            }
            let Some((session_cwd, start)) = session_meta(&entry.path()) else {
                continue;
            };
            if session_cwd != cwd || start + 5 < since {
                continue;
            }
            if best.as_ref().is_none_or(|(b, _)| start < *b) {
                best = Some((start, id.to_string()));
            }
        }
    }
    best.map(|(_, id)| id)
}

/// All `<root>/YYYY/MM/DD` day directories, newest first (names are
/// zero-padded, so lexicographic order is chronological).
fn day_dirs_desc(root: &Path) -> Vec<PathBuf> {
    fn subdirs_desc(dir: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(dir)
            .map(|it| {
                it.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v.reverse();
        v
    }
    subdirs_desc(root)
        .iter()
        .flat_map(|y| subdirs_desc(y))
        .flat_map(|m| subdirs_desc(&m))
        .collect()
}

/// (year, month, day) from a day directory's trailing `YYYY/MM/DD`.
fn day_date(day: &Path) -> Option<(i64, i64, i64)> {
    let mut parts = day.components().rev().filter_map(|c| match c {
        std::path::Component::Normal(s) => s.to_str()?.parse::<i64>().ok(),
        _ => None,
    });
    let (d, m, y) = (parts.next()?, parts.next()?, parts.next()?);
    Some((y, m, d))
}

/// The session uuid embedded at the end of a rollout filename
/// (`rollout-<local ts>-<uuid>.jsonl`).
fn rollout_id(name: &str) -> Option<&str> {
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    (stem.len() > 36).then(|| &stem[stem.len() - 36..])
}

/// The (cwd, start time) a rollout's first line — its `session_meta` record
/// — declares. Any failure (unreadable, partial write, schema drift) yields
/// None: the file is simply not a candidate this poll.
fn session_meta(path: &Path) -> Option<(PathBuf, u64)> {
    let file = fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    let v: Value = serde_json::from_str(&line).ok()?;
    if v["type"].as_str() != Some("session_meta") {
        return None;
    }
    let p = &v["payload"];
    Some((
        PathBuf::from(p["cwd"].as_str()?),
        discovery::parse_iso8601(p["timestamp"].as_str()?)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The two ChatGPT rate-limit windows map to `5h` and `wk` entries,
    /// matching the Claude readout's labels.
    #[test]
    fn parses_usage_windows() {
        let json = r#"{"plan_type":"team","rate_limit":{
            "allowed":true,"limit_reached":false,
            "primary_window":{"used_percent":1,"limit_window_seconds":18000,"reset_after_seconds":1,"reset_at":1},
            "secondary_window":{"used_percent":12.6,"limit_window_seconds":604800,"reset_after_seconds":1,"reset_at":1}
        }}"#;
        let resp: UsageResponse = serde_json::from_str(json).unwrap();
        let rl = resp.rate_limit.unwrap();
        let shown: Vec<(String, u8)> = [rl.primary_window, rl.secondary_window]
            .into_iter()
            .flatten()
            .map(window_entry)
            .map(|e| (e.label, e.percent))
            .collect();
        assert_eq!(shown, vec![("5h".to_string(), 1), ("wk".to_string(), 13)]);
    }

    /// task_started/task_complete bound the turn in unix seconds; the first
    /// user_message is the title stand-in; turn_aborted (Esc) ends the turn
    /// itself since no task_complete follows it.
    #[test]
    fn turn_lifecycle() {
        let mut meta = Meta::default();
        // Non-event records (session_meta, response_item …) don't count as
        // content — only actual events do.
        apply(
            &mut meta,
            &json!({"timestamp":"2026-07-10T18:38:00Z","type":"session_meta",
                    "payload":{"cwd":"/x"}}),
        );
        assert!(!meta.has_content);

        apply(
            &mut meta,
            &json!({"timestamp":"2026-07-10T18:38:56.391Z","type":"event_msg",
                    "payload":{"type":"task_started","started_at":1783708736}}),
        );
        assert!(meta.has_content);
        assert_eq!(meta.turn_state, TurnState::Mid);
        assert_eq!(meta.turn_started_at, Some(1783708736));
        assert_eq!(meta.turn_completed_at, None);

        apply(
            &mut meta,
            &json!({"timestamp":"2026-07-10T18:38:56.444Z","type":"event_msg",
                    "payload":{"type":"user_message","message":"say hi\nand more"}}),
        );
        assert_eq!(meta.first_prompt.as_deref(), Some("say hi"));
        assert_eq!(meta.display_title(), Some("say hi"));

        apply(
            &mut meta,
            &json!({"timestamp":"2026-07-10T18:39:03.820Z","type":"event_msg",
                    "payload":{"type":"task_complete","completed_at":1783708743}}),
        );
        assert_eq!(meta.turn_state, TurnState::Complete);
        assert_eq!(meta.turn_completed_at, Some(1783708743));

        // Next turn, interrupted with Esc: the abort record ends it, stamped
        // with its own write time. The title stand-in stays the first prompt.
        apply(
            &mut meta,
            &json!({"timestamp":"2026-07-10T18:40:00Z","type":"event_msg",
                    "payload":{"type":"task_started","started_at":1783708800}}),
        );
        apply(
            &mut meta,
            &json!({"timestamp":"2026-07-10T18:40:30Z","type":"event_msg",
                    "payload":{"type":"turn_aborted","reason":"interrupted"}}),
        );
        assert_eq!(meta.turn_state, TurnState::Complete);
        assert_eq!(meta.turn_completed_at, Some(1783708830));
        assert_eq!(meta.first_prompt.as_deref(), Some("say hi"));

        // Non-event records (session_meta, response_item …) are invisible.
        apply(
            &mut meta,
            &json!({"timestamp":"2026-07-10T18:41:00Z","type":"response_item",
                    "payload":{"type":"message","role":"user"}}),
        );
        assert_eq!(meta.turn_state, TurnState::Complete);
    }

    /// Newer Codex rollouts wrap user input in an item_completed event rather
    /// than the older flat user_message event. It must still provide the
    /// conversation's title stand-in.
    #[test]
    fn item_completed_user_message_becomes_title() {
        let mut meta = Meta::default();
        apply(
            &mut meta,
            &json!({
                "timestamp": "2026-08-08T13:46:04.989Z",
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "item": {
                        "type": "UserMessage",
                        "content": [
                            {"type": "image", "url": "attachment.png"},
                            {"type": "text", "text": "Fix the history title\nand add a test"}
                        ]
                    }
                }
            }),
        );

        assert_eq!(meta.first_prompt.as_deref(), Some("Fix the history title"));
        assert_eq!(meta.display_title(), Some("Fix the history title"));
    }

    #[test]
    fn rollout_filename_id() {
        assert_eq!(
            rollout_id("rollout-2026-07-10T20-38-01-019f4d52-83aa-7f51-aa7c-0d9dd2ff399f.jsonl"),
            Some("019f4d52-83aa-7f51-aa7c-0d9dd2ff399f")
        );
        assert_eq!(rollout_id("rollout-.jsonl"), None);
        assert_eq!(rollout_id("store.db"), None);
    }

    /// Pending resolution: match on cwd + start-at/after-spawn, skip taken
    /// ids, prefer the earliest candidate.
    #[test]
    fn resolve_pending() {
        let root = std::env::temp_dir().join(format!("corc-codex-test-{}", std::process::id()));
        let day = root.join("2026/07/10");
        std::fs::create_dir_all(&day).unwrap();
        let write = |uuid: &str, cwd: &str, ts: &str| {
            let line = format!(
                r#"{{"timestamp":"{ts}","type":"session_meta","payload":{{"session_id":"{uuid}","cwd":"{cwd}","timestamp":"{ts}"}}}}"#
            );
            std::fs::write(
                day.join(format!("rollout-2026-07-10T00-00-00-{uuid}.jsonl")),
                line,
            )
            .unwrap();
        };
        let (a, b, c) = (
            "019f4d52-83aa-7f51-aa7c-0d9dd2ff0001",
            "019f4d52-83aa-7f51-aa7c-0d9dd2ff0002",
            "019f4d52-83aa-7f51-aa7c-0d9dd2ff0003",
        );
        // 10:00:00Z = 1783764000? Use parse to stay honest.
        write(a, "/proj", "2026-07-10T10:00:00Z");
        write(b, "/proj", "2026-07-10T10:05:00Z");
        write(c, "/other", "2026-07-10T10:00:00Z");
        let t = |s: &str| discovery::parse_iso8601(s).unwrap();

        // Earliest matching session wins; the other-cwd one never matches.
        let spawn = t("2026-07-10T10:00:02Z"); // jitter: spawn stamped just after start
        assert_eq!(
            resolve(&root, Path::new("/proj"), spawn, &[]),
            Some(a.to_string())
        );
        // Once claimed, the next pending in the same cwd gets its own.
        assert_eq!(
            resolve(&root, Path::new("/proj"), spawn, &[a.to_string()]),
            Some(b.to_string())
        );
        // A spawn after every session start resolves to nothing.
        assert_eq!(
            resolve(&root, Path::new("/proj"), t("2026-07-10T11:00:00Z"), &[]),
            None
        );
        assert_eq!(resolve(&root, Path::new("/nope"), spawn, &[]), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn locate_skips_pending() {
        let root = std::env::temp_dir().join("corc-codex-locate-nonexistent");
        assert_eq!(locate_rollout(&root, Path::new("/x"), "pending-abc"), None);
    }
}
