//! Metadata for known conversations, read from their jsonl transcripts.
//! corc only ever looks up the files of conversations it spawned (known
//! uuid + cwd) — there is no tree scan and no adoption of foreign history
//! (PLAN.md D1). The jsonl files are read-only: never modified, never
//! deleted.
//!
//! `Store` is the incremental machinery — find the file once, re-parse only
//! bytes that grew, cache per id — parameterized by a locate and a
//! line-reader function so jsonl-based providers share it: `Store::new()`
//! reads Claude Code's transcripts under ~/.claude/projects, Codex plugs in
//! its own pair over ~/.codex/sessions (`Store::with`).

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Where the last non-sidechain message left the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    /// A user prompt or tool call is in flight — Claude has work to do.
    Mid,
    /// The assistant ended its turn — the ball is on the user's side.
    Complete,
    Unknown,
}

/// What the jsonl tells us about a conversation.
#[derive(Debug, Clone)]
pub struct Meta {
    /// User-assigned Claude Code title (`custom-title` records). This stays
    /// separate so a later generated title can never overwrite an explicit
    /// `/rename`.
    pub custom_title: Option<String>,
    /// Generated title (`ai-title`/`summary` records). Claude Code writes it
    /// lazily, so young conversations have none — see `display_title`.
    pub title: Option<String>,
    /// First real user prompt, kept as a title stand-in until `title` exists.
    pub first_prompt: Option<String>,
    /// Whether the conversation contains a real user/assistant exchange.
    /// Deliberately independent of `title`, which providers may generate late
    /// or fail to generate at all.
    pub has_content: bool,
    pub turn_state: TurnState,
    /// Unix seconds of the last real (non-sidechain, non-meta, non-tool-
    /// result) user prompt — the start of the current or last turn (D7).
    pub turn_started_at: Option<u64>,
    /// Unix seconds of the end_turn / turn_duration record that finished
    /// that turn; `None` while the turn is in flight (D7).
    pub turn_completed_at: Option<u64>,
    /// Unix seconds of the latest record that genuinely advanced the current
    /// turn: a prompt, assistant response, tool result, or completion. Unlike
    /// the jsonl mtime this ignores background title/checkpoint writes, so it
    /// can safely be used to detect an abandoned in-flight turn.
    pub turn_progress_at: Option<u64>,
    /// Tokens currently in the conversation's context window, from the most
    /// recent API usage the transcript records (Claude: the last assistant
    /// record's `message.usage`; Codex: `token_count` events). Drops after a
    /// compaction just like the real window. None for providers whose
    /// transcripts carry no usage (Cursor).
    pub context_tokens: Option<u64>,
    /// mtime of the jsonl — coarse filesystem activity, including background
    /// writes that do not advance a turn.
    pub mtime: SystemTime,
}

impl Meta {
    /// What the sidebar should show: an explicit rename, the generated title,
    /// or the first user prompt while no title has been generated yet.
    pub fn display_title(&self) -> Option<&str> {
        self.custom_title
            .as_deref()
            .or(self.title.as_deref())
            .or(self.first_prompt.as_deref())
    }
}

/// A per-provider source of conversation metadata (title, turn state, activity
/// time). Each provider ships its own implementation — Claude parses jsonl
/// transcripts (`Store`), Cursor reads its SQLite chat stores — so adding a
/// provider never touches the sidebar. `refresh` is handed only the (id, cwd)
/// pairs belonging to that provider.
pub trait MetaSource: Send {
    fn refresh(&mut self, known: &[(String, PathBuf, Option<u64>)]) -> Result<()>;
    fn meta(&self, id: &str) -> Option<&Meta>;
}

impl MetaSource for Store {
    fn refresh(&mut self, known: &[(String, PathBuf, Option<u64>)]) -> Result<()> {
        let conversations: Vec<(String, PathBuf)> = known
            .iter()
            .map(|(id, cwd, _)| (id.clone(), cwd.clone()))
            .collect();
        Store::refresh(self, &conversations)
    }
    fn meta(&self, id: &str) -> Option<&Meta> {
        Store::meta(self, id)
    }
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            custom_title: None,
            title: None,
            first_prompt: None,
            has_content: false,
            turn_state: TurnState::Unknown,
            turn_started_at: None,
            turn_completed_at: None,
            turn_progress_at: None,
            context_tokens: None,
            mtime: SystemTime::UNIX_EPOCH,
        }
    }
}

struct FileState {
    path: PathBuf,
    offset: u64,
    size: u64,
    mtime: SystemTime,
    meta: Meta,
}

/// Incrementally parsed metadata for the conversations corc owns.
pub struct Store {
    root: PathBuf,
    files: HashMap<String, FileState>,
    /// Find the transcript of a conversation: (root, cwd, id) → path.
    locate: fn(&Path, &Path, &str) -> Option<PathBuf>,
    /// Fold one parsed jsonl line into the metadata.
    apply: fn(&mut Meta, &Value),
}

impl Store {
    /// Claude Code's transcript store under ~/.claude/projects.
    pub fn new() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(Self::with(
            PathBuf::from(home).join(".claude/projects"),
            locate_jsonl,
            apply,
        ))
    }

    /// The same incremental machinery over another provider's jsonl tree
    /// (Codex rollouts).
    pub fn with(
        root: PathBuf,
        locate: fn(&Path, &Path, &str) -> Option<PathBuf>,
        apply: fn(&mut Meta, &Value),
    ) -> Self {
        Self {
            root,
            files: HashMap::new(),
            locate,
            apply,
        }
    }

    /// Refresh metadata for the given (uuid, cwd) pairs, parsing only new
    /// bytes of files that grew since the last call.
    pub fn refresh(&mut self, known: &[(String, PathBuf)]) -> Result<()> {
        for (id, cwd) in known {
            let path = match self.files.get(id) {
                Some(state) => state.path.clone(),
                None => match (self.locate)(&self.root, cwd, id) {
                    Some(p) => p,
                    // Freshly spawned conversations have no transcript yet.
                    None => continue,
                },
            };
            let Ok(fs_meta) = fs::metadata(&path) else {
                // The file vanished; forget it so we re-locate next time.
                self.files.remove(id);
                continue;
            };
            let size = fs_meta.len();
            let mtime = fs_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

            let apply = self.apply;
            match self.files.get_mut(id) {
                Some(state) if state.size == size && state.mtime == mtime => {}
                Some(state) if size >= state.offset => {
                    state.offset = parse_from(&path, state.offset, &mut state.meta, apply)?;
                    state.size = size;
                    state.mtime = mtime;
                    state.meta.mtime = mtime;
                }
                _ => {
                    // New file, or it shrank (rewritten) — parse from scratch.
                    let mut meta = Meta {
                        mtime,
                        ..Meta::default()
                    };
                    let offset = parse_from(&path, 0, &mut meta, apply)?;
                    self.files.insert(
                        id.clone(),
                        FileState {
                            path,
                            offset,
                            size,
                            mtime,
                            meta,
                        },
                    );
                }
            }
        }
        self.files.retain(|id, _| known.iter().any(|(k, _)| k == id));
        Ok(())
    }

    pub fn meta(&self, id: &str) -> Option<&Meta> {
        self.files.get(id).map(|s| &s.meta)
    }
}

/// Claude Code stores transcripts under a directory named after the cwd with
/// every non-alphanumeric character replaced by '-'. Try that first, then
/// fall back to a one-level scan of the project directories (naming scheme
/// insurance, not discovery — the uuid is already known).
fn locate_jsonl(root: &Path, cwd: &Path, id: &str) -> Option<PathBuf> {
    let escaped: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let candidate = root.join(escaped).join(format!("{id}.jsonl"));
    if candidate.is_file() {
        return Some(candidate);
    }
    for project in fs::read_dir(root).ok()?.flatten() {
        let candidate = project.path().join(format!("{id}.jsonl"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Parse complete lines starting at `offset`, folding each into `meta` with
/// `apply`; returns the offset just past the last complete line, so a
/// partially written trailing line is retried on the next poll.
fn parse_from(
    path: &Path,
    offset: u64,
    meta: &mut Meta,
    apply: fn(&mut Meta, &Value),
) -> Result<u64> {
    let file = fs::File::open(path)?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    reader.seek(SeekFrom::Start(offset))?;
    let mut pos = offset;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        pos += n as u64;
        if let Ok(v) = serde_json::from_slice::<Value>(&line) {
            apply(meta, &v);
        }
    }
    Ok(pos)
}

fn apply(meta: &mut Meta, v: &Value) {
    let sidechain = v["isSidechain"].as_bool().unwrap_or(false);
    match v["type"].as_str() {
        Some("user") if !sidechain && !is_meta_user(v) => {
            meta.has_content = true;
            if let Some(ts) = record_timestamp(v) {
                meta.turn_progress_at = Some(ts);
            }
            // A Ctrl+C interrupt is written as a user record too, but it ends
            // the turn — it never produces an end_turn / turn_duration record,
            // so if we let it fall through as a prompt the state would stay
            // Mid (Running) forever.
            if v.get("interruptedMessageId").is_some() {
                meta.turn_state = TurnState::Complete;
                meta.turn_completed_at = record_timestamp(v);
                return;
            }
            meta.turn_state = TurnState::Mid;
            // Only a real prompt starts a turn (D7); tool results arriving
            // mid-turn keep the state Mid but never reset the start time.
            if !is_tool_result(v) {
                if let Some(ts) = record_timestamp(v) {
                    meta.turn_started_at = Some(ts);
                    meta.turn_completed_at = None;
                }
                if meta.first_prompt.is_none()
                    && let Some(text) = prompt_text(v)
                {
                    meta.first_prompt = Some(text);
                }
            }
        }
        Some("assistant") if !sidechain => {
            meta.has_content = true;
            if let Some(ts) = record_timestamp(v) {
                meta.turn_progress_at = Some(ts);
            }
            if let Some(tokens) = context_tokens(v) {
                meta.context_tokens = Some(tokens);
            }
            match v["message"]["stop_reason"].as_str() {
                Some("end_turn") | Some("stop_sequence") | Some("max_tokens") => {
                    meta.turn_state = TurnState::Complete;
                    meta.turn_completed_at = record_timestamp(v);
                }
                _ => meta.turn_state = TurnState::Mid,
            }
        }
        Some("system") if v["subtype"].as_str() == Some("turn_duration") => {
            meta.turn_state = TurnState::Complete;
            if let Some(ts) = record_timestamp(v) {
                meta.turn_completed_at = Some(ts);
                meta.turn_progress_at = Some(ts);
            }
        }
        Some("ai-title") => {
            meta.has_content = true;
            if let Some(title) = v["aiTitle"].as_str() {
                meta.title = Some(title.to_string());
            }
        }
        Some("summary") => {
            meta.has_content = true;
            if meta.title.is_none()
                && let Some(summary) = v["summary"].as_str()
            {
                meta.title = Some(summary.to_string());
            }
        }
        Some("custom-title") => {
            if let Some(title) = v["customTitle"].as_str() {
                meta.custom_title = Some(title.to_string());
            }
        }
        _ => {}
    }
}

/// Context size after an assistant record: the request's usage counts every
/// input token — fresh (`input_tokens`) plus both cache flavors — which is
/// exactly what sat in the window. Synthetic records (API-error stand-ins)
/// carry an all-zero usage and yield None so they never blank a real reading.
fn context_tokens(v: &Value) -> Option<u64> {
    let usage = v["message"].get("usage")?;
    let n = |key: &str| usage[key].as_u64().unwrap_or(0);
    let total =
        n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
    (total > 0).then_some(total)
}

/// User records that don't represent a prompt: caveat/meta records and the
/// `<command-…>` transcript of local slash commands.
fn is_meta_user(v: &Value) -> bool {
    if v["isMeta"].as_bool().unwrap_or(false) {
        return true;
    }
    matches!(
        v["message"]["content"].as_str(),
        Some(s) if s.starts_with("<command-") || s.starts_with("<local-command")
    )
}

/// Tool results come back as user records with a `toolUseResult` key (and
/// `tool_result` content blocks).
fn is_tool_result(v: &Value) -> bool {
    if v.get("toolUseResult").is_some() {
        return true;
    }
    v["message"]["content"]
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
}

/// The prompt text of a user record, reduced to a one-line title stand-in.
fn prompt_text(v: &Value) -> Option<String> {
    let content = &v["message"]["content"];
    let text = content.as_str().map(str::to_string).or_else(|| {
        content.as_array()?.iter().find_map(|b| {
            (b["type"] == "text").then(|| b["text"].as_str())?.map(str::to_string)
        })
    })?;
    title_line(&text)
}

/// Reduce prompt text to a one-line title stand-in: first non-empty line, at
/// most 60 chars. Shared with providers that never generate a title (Codex).
pub(crate) fn title_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(match line.char_indices().nth(60) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    })
}

fn record_timestamp(v: &Value) -> Option<u64> {
    parse_iso8601(v["timestamp"].as_str()?)
}

/// Parse `YYYY-MM-DDTHH:MM:SS(.frac)?(Z|±HH:MM)?` into unix seconds. The
/// jsonl writes UTC with a `Z` suffix; offsets are handled for insurance.
/// Shared with other providers whose transcripts stamp the same ISO form
/// (Codex `event_msg` records).
pub(crate) fn parse_iso8601(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        s.get(range)?.parse().ok()
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);

    // Skip a fractional-seconds part, then read an optional offset.
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        rest = &frac[digits..];
    }
    let offset_secs = match rest.as_bytes().first() {
        Some(b'+' | b'-') if rest.len() >= 6 && rest.as_bytes()[3] == b':' => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            let h: i64 = rest.get(1..3)?.parse().ok()?;
            let m: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (h * 3600 + m * 60)
        }
        _ => 0, // "Z" or nothing: UTC
    };

    let epoch =
        days_from_civil(year, month, day) * 86400 + hour * 3600 + min * 60 + sec - offset_secs;
    u64::try_from(epoch).ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil` algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The inverse: (year, month, day) for days since 1970-01-01 (Hinnant's
/// `civil_from_days`). Codex prunes its date-named session directories with
/// it when resolving a pending id.
pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn civil_roundtrip() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        for days in [-719468, -1, 0, 20645, 100_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn iso8601() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("2026-07-08T16:32:21.180Z"), Some(1783528341));
        // Offset form: 18:32:21+02:00 is the same instant.
        assert_eq!(
            parse_iso8601("2026-07-08T18:32:21.180+02:00"),
            Some(1783528341)
        );
        assert_eq!(parse_iso8601("garbage"), None);
    }

    /// D7: turn start = last real user prompt; tool results keep the turn
    /// Mid without resetting the start; end_turn / turn_duration complete it.
    #[test]
    fn turn_timing() {
        let mut meta = Meta::default();
        let apply_all = |meta: &mut Meta, records: &[Value]| {
            for r in records {
                apply(meta, r);
            }
        };

        apply_all(
            &mut meta,
            &[
                // Meta/caveat and slash-command records never start a turn.
                json!({"type":"user","isMeta":true,"message":{"content":"caveat"},
                       "timestamp":"2026-07-08T10:00:00Z"}),
                json!({"type":"user","message":{"content":"<command-name>/clear</command-name>"},
                       "timestamp":"2026-07-08T10:00:01Z"}),
            ],
        );
        assert_eq!(meta.turn_state, TurnState::Unknown);
        assert_eq!(meta.turn_started_at, None);
        assert!(!meta.has_content);

        // A real prompt starts the turn.
        apply(
            &mut meta,
            &json!({"type":"user","message":{"content":"do the thing"},
                    "timestamp":"2026-07-08T10:01:00Z"}),
        );
        let start = parse_iso8601("2026-07-08T10:01:00Z");
        assert_eq!(meta.turn_state, TurnState::Mid);
        assert_eq!(meta.turn_started_at, start);
        assert_eq!(meta.turn_progress_at, start);
        assert_eq!(meta.turn_completed_at, None);
        assert!(meta.has_content);

        // Assistant tool_use + tool result stay Mid, start untouched.
        apply_all(
            &mut meta,
            &[
                json!({"type":"assistant","message":{"stop_reason":"tool_use"},
                       "timestamp":"2026-07-08T10:02:00Z"}),
                json!({"type":"user","toolUseResult":{},
                       "message":{"content":[{"type":"tool_result"}]},
                       "timestamp":"2026-07-08T10:03:00Z"}),
            ],
        );
        assert_eq!(meta.turn_state, TurnState::Mid);
        assert_eq!(meta.turn_started_at, start);
        assert_eq!(
            meta.turn_progress_at,
            parse_iso8601("2026-07-08T10:03:00Z")
        );

        // The turn_duration record completes the turn.
        apply(
            &mut meta,
            &json!({"type":"system","subtype":"turn_duration","durationMs":240000,
                    "timestamp":"2026-07-08T10:05:00Z"}),
        );
        assert_eq!(meta.turn_state, TurnState::Complete);
        assert_eq!(meta.turn_started_at, start);
        assert_eq!(
            meta.turn_completed_at,
            parse_iso8601("2026-07-08T10:05:00Z")
        );
        assert_eq!(meta.turn_progress_at, meta.turn_completed_at);

        // Sidechain traffic is invisible to turn state.
        apply(
            &mut meta,
            &json!({"type":"user","isSidechain":true,"message":{"content":"sub"},
                    "timestamp":"2026-07-08T10:06:00Z"}),
        );
        assert_eq!(meta.turn_state, TurnState::Complete);
        assert_eq!(
            meta.turn_progress_at,
            parse_iso8601("2026-07-08T10:05:00Z")
        );

        // Claude's background records can touch the jsonl but are not turn
        // progress and must not extend the Running timeout.
        apply(
            &mut meta,
            &json!({"type":"system","subtype":"away_summary",
                    "timestamp":"2026-07-08T10:07:00Z"}),
        );
        assert_eq!(
            meta.turn_progress_at,
            parse_iso8601("2026-07-08T10:05:00Z")
        );

        // The next prompt starts a fresh turn.
        apply(
            &mut meta,
            &json!({"type":"user","message":{"content":"next"},
                    "timestamp":"2026-07-08T10:10:00Z"}),
        );
        assert_eq!(meta.turn_state, TurnState::Mid);
        assert_eq!(meta.turn_started_at, parse_iso8601("2026-07-08T10:10:00Z"));
        assert_eq!(meta.turn_completed_at, None);
        assert_eq!(meta.turn_progress_at, meta.turn_started_at);
    }

    #[test]
    fn custom_title_names_a_slash_command_conversation_and_wins_over_generated_titles() {
        let mut meta = Meta::default();

        apply(
            &mut meta,
            &json!({"type":"user","message":{"content":
                "<command-message>improve-codebase-architecture</command-message>\n\
                 <command-name>/improve-codebase-architecture</command-name>"}}),
        );
        assert_eq!(meta.display_title(), None);

        apply(
            &mut meta,
            &json!({"type":"custom-title","customTitle":"platform api architecture"}),
        );
        assert_eq!(meta.display_title(), Some("platform api architecture"));
        assert!(!meta.has_content);

        apply(
            &mut meta,
            &json!({"type":"ai-title","aiTitle":"Generated architecture review"}),
        );
        assert_eq!(meta.display_title(), Some("platform api architecture"));
    }

    /// Context size tracks the latest assistant record's usage — every input
    /// flavor summed — while zero-usage synthetic records and sidechain
    /// traffic leave the reading alone.
    #[test]
    fn context_tokens_follow_latest_assistant_usage() {
        let mut meta = Meta::default();
        assert_eq!(meta.context_tokens, None);

        apply(
            &mut meta,
            &json!({"type":"assistant","message":{"stop_reason":"tool_use",
                    "usage":{"input_tokens":2,"cache_creation_input_tokens":5538,
                             "cache_read_input_tokens":29708,"output_tokens":911}},
                    "timestamp":"2026-07-08T10:02:00Z"}),
        );
        assert_eq!(meta.context_tokens, Some(2 + 5538 + 29708));

        // A synthetic record (API-error stand-in) has all-zero usage: the
        // real reading survives. So does sidechain (subagent) usage.
        apply(
            &mut meta,
            &json!({"type":"assistant","message":{"stop_reason":null,
                    "usage":{"input_tokens":0,"cache_creation_input_tokens":0,
                             "cache_read_input_tokens":0,"output_tokens":0}},
                    "timestamp":"2026-07-08T10:03:00Z"}),
        );
        apply(
            &mut meta,
            &json!({"type":"assistant","isSidechain":true,
                    "message":{"stop_reason":"end_turn",
                    "usage":{"input_tokens":999999}},
                    "timestamp":"2026-07-08T10:04:00Z"}),
        );
        assert_eq!(meta.context_tokens, Some(35248));

        // The next real reading replaces it — including a post-compaction
        // drop to a smaller window.
        apply(
            &mut meta,
            &json!({"type":"assistant","message":{"stop_reason":"end_turn",
                    "usage":{"input_tokens":3,"cache_read_input_tokens":11000}},
                    "timestamp":"2026-07-08T10:05:00Z"}),
        );
        assert_eq!(meta.context_tokens, Some(11003));
    }

    /// A Ctrl+C interrupt is written as a user record (with an
    /// `interruptedMessageId`) but no end_turn / turn_duration follows, so it
    /// must complete the turn itself — otherwise the state stays Running.
    #[test]
    fn interrupt_completes_turn() {
        let mut meta = Meta::default();

        // A prompt starts a turn, assistant works…
        apply(
            &mut meta,
            &json!({"type":"user","message":{"content":"do the thing"},
                    "timestamp":"2026-07-08T10:01:00Z"}),
        );
        apply(
            &mut meta,
            &json!({"type":"assistant","message":{"stop_reason":"tool_use"},
                    "timestamp":"2026-07-08T10:01:30Z"}),
        );
        assert_eq!(meta.turn_state, TurnState::Mid);

        // …then Ctrl+C. The interrupt record ends the turn.
        apply(
            &mut meta,
            &json!({"type":"user",
                    "message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]},
                    "interruptedMessageId":"msg_015bfD7CH2nhHASfMRsjVfT4",
                    "timestamp":"2026-07-08T10:02:00Z"}),
        );
        assert_eq!(meta.turn_state, TurnState::Complete);
        assert_eq!(meta.turn_completed_at, parse_iso8601("2026-07-08T10:02:00Z"));
        // The interrupt is not a prompt: it never becomes the title stand-in.
        assert_eq!(meta.first_prompt.as_deref(), Some("do the thing"));
    }
}
