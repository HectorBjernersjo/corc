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
    /// The slash command a conversation was started with (`/impeccable teach`),
    /// for conversations that have no real prompt at all. Ranked below
    /// `first_prompt` — the command is the weakest of the stand-ins, but it
    /// beats `(untitled)`.
    pub first_command: Option<String>,
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
    /// Claude Code has emitted an `AskUserQuestion` tool call that has not
    /// received its matching tool result yet. This is distinct from ordinary
    /// Mid-turn work: the agent is blocked on the user, so the conversation
    /// needs attention rather than a Running/Idle signal.
    pub active_question: bool,
    /// When the currently active question was asked, for its age column.
    pub question_asked_at: Option<u64>,
    /// Tool-use id used to distinguish the question's answer from unrelated
    /// tool results in the same turn.
    pub(crate) active_question_tool_id: Option<String>,
    /// mtime of the jsonl — coarse filesystem activity, including background
    /// writes that do not advance a turn.
    pub mtime: SystemTime,
}

impl Meta {
    /// What the sidebar should show: an explicit rename, the generated title,
    /// the first user prompt while no title has been generated yet, or — for a
    /// conversation opened with a slash command and nothing else — the command.
    pub fn display_title(&self) -> Option<&str> {
        self.custom_title
            .as_deref()
            .or(self.title.as_deref())
            .or(self.first_prompt.as_deref())
            .or(self.first_command.as_deref())
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
            first_command: None,
            has_content: false,
            turn_state: TurnState::Unknown,
            turn_started_at: None,
            turn_completed_at: None,
            turn_progress_at: None,
            active_question: false,
            question_asked_at: None,
            active_question_tool_id: None,
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
        self.files
            .retain(|id, _| known.iter().any(|(k, _)| k == id));
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

    // AskUserQuestion stays open in Claude's transcript until its matching
    // tool result is written. Track that structured lifecycle instead of
    // scraping the terminal UI, whose pane title is identical to normal idle.
    if !sidechain
        && v["type"] == "user"
        && question_was_answered(v, meta.active_question_tool_id.as_deref())
    {
        meta.active_question = false;
        meta.question_asked_at = None;
        meta.active_question_tool_id = None;
    }

    // Slash-command records are meta — they must not count as content or move
    // the turn state — but they are the only trace of what a conversation is
    // about until a real prompt or a generated title shows up.
    if !sidechain && v["type"] == "user" && meta.first_command.is_none() {
        meta.first_command = slash_command(v);
    }

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
            if let Some(id) = ask_user_question_id(v) {
                meta.active_question = true;
                meta.question_asked_at = record_timestamp(v);
                meta.active_question_tool_id = Some(id.to_string());
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

/// The invocation a `<command-…>` user record stands for, rendered the way it
/// was typed: `/impeccable teach`. Only the name is required; a command with no
/// arguments writes an empty (or missing) `<command-args>`.
fn slash_command(v: &Value) -> Option<String> {
    let content = v["message"]["content"].as_str()?;
    let name = tagged(content, "command-name")?;
    let line = match tagged(content, "command-args") {
        Some(args) if !args.is_empty() => format!("{name} {args}"),
        _ => name.to_string(),
    };
    title_line(&line)
}

/// The text between `<tag>` and `</tag>`, trimmed.
fn tagged<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let rest = text.split_once(&format!("<{tag}>"))?.1;
    Some(rest.split_once(&format!("</{tag}>"))?.0.trim())
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

/// The id of an AskUserQuestion tool call in an assistant record.
fn ask_user_question_id(v: &Value) -> Option<&str> {
    v["message"]["content"]
        .as_array()?
        .iter()
        .find_map(|block| {
            (block["type"] == "tool_use" && block["name"] == "AskUserQuestion")
                .then(|| block["id"].as_str())?
        })
}

/// Whether a user record answers the currently open AskUserQuestion. A real
/// prompt also clears it as recovery for transcript versions that represent
/// a cancelled question without a matching tool-result block.
fn question_was_answered(v: &Value, question_id: Option<&str>) -> bool {
    let Some(question_id) = question_id else {
        return false;
    };
    if !is_tool_result(v) {
        return !is_meta_user(v);
    }
    v["message"]["content"].as_array().is_some_and(|blocks| {
        blocks.iter().any(|block| {
            block["type"] == "tool_result" && block["tool_use_id"].as_str() == Some(question_id)
        })
    })
}

/// The prompt text of a user record, reduced to a one-line title stand-in.
fn prompt_text(v: &Value) -> Option<String> {
    let content = &v["message"]["content"];
    let text = content.as_str().map(str::to_string).or_else(|| {
        content.as_array()?.iter().find_map(|b| {
            (b["type"] == "text")
                .then(|| b["text"].as_str())?
                .map(str::to_string)
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
    let num = |range: std::ops::Range<usize>| -> Option<i64> { s.get(range)?.parse().ok() };
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
        assert_eq!(meta.turn_progress_at, parse_iso8601("2026-07-08T10:03:00Z"));

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
        assert_eq!(meta.turn_progress_at, parse_iso8601("2026-07-08T10:05:00Z"));

        // Claude's background records can touch the jsonl but are not turn
        // progress and must not extend the Running timeout.
        apply(
            &mut meta,
            &json!({"type":"system","subtype":"away_summary",
                    "timestamp":"2026-07-08T10:07:00Z"}),
        );
        assert_eq!(meta.turn_progress_at, parse_iso8601("2026-07-08T10:05:00Z"));

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

    /// A conversation opened with a slash command has no prompt to fall back
    /// on, so the command itself names it — but it is the weakest stand-in:
    /// a real prompt, a generated title and a rename all outrank it.
    #[test]
    fn slash_command_names_a_conversation_until_something_better_arrives() {
        let mut meta = Meta::default();

        apply(
            &mut meta,
            &json!({"type":"user","message":{"content":
                "<command-message>impeccable</command-message>\n\
                 <command-name>/impeccable</command-name>\n\
                 <command-args>teach</command-args>"}}),
        );
        assert_eq!(meta.display_title(), Some("/impeccable teach"));
        // Still meta: the command neither counts as content nor starts a turn.
        assert!(!meta.has_content);
        assert_eq!(meta.turn_state, TurnState::Unknown);

        // The skill's own injected text is meta too, and a later command does
        // not rename the conversation.
        apply(
            &mut meta,
            &json!({"type":"user","isMeta":true,"message":{"content":
                [{"type":"text","text":"Base directory for this skill: …"}]}}),
        );
        apply(
            &mut meta,
            &json!({"type":"user","message":{"content":
                "<command-name>/clear</command-name>"}}),
        );
        assert_eq!(meta.display_title(), Some("/impeccable teach"));

        // A real prompt is more descriptive than the command that opened the
        // conversation, so it takes over.
        apply(
            &mut meta,
            &json!({"type":"user","message":{"content":"look for sloppy frontend bits"},
                    "timestamp":"2026-08-16T17:40:00Z"}),
        );
        assert_eq!(meta.display_title(), Some("look for sloppy frontend bits"));
    }

    #[test]
    fn custom_title_wins_over_generated_titles() {
        let mut meta = Meta::default();

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
        assert_eq!(
            meta.turn_completed_at,
            parse_iso8601("2026-07-08T10:02:00Z")
        );
        // The interrupt is not a prompt: it never becomes the title stand-in.
        assert_eq!(meta.first_prompt.as_deref(), Some("do the thing"));
    }

    #[test]
    fn ask_user_question_stays_active_until_its_matching_answer() {
        let mut meta = Meta::default();

        apply(
            &mut meta,
            &json!({
                "type":"assistant",
                "message":{
                    "stop_reason":"tool_use",
                    "content":[{
                        "type":"tool_use",
                        "id":"question-1",
                        "name":"AskUserQuestion",
                        "input":{"questions":[]}
                    }]
                },
                "timestamp":"2026-07-08T10:02:00Z"
            }),
        );
        assert!(meta.active_question);
        assert_eq!(
            meta.question_asked_at,
            parse_iso8601("2026-07-08T10:02:00Z")
        );

        // An unrelated tool result must not dismiss the question.
        apply(
            &mut meta,
            &json!({
                "type":"user",
                "toolUseResult":{},
                "message":{"content":[{
                    "type":"tool_result",
                    "tool_use_id":"some-other-tool"
                }]}
            }),
        );
        assert!(meta.active_question);

        apply(
            &mut meta,
            &json!({
                "type":"user",
                "toolUseResult":{},
                "message":{"content":[{
                    "type":"tool_result",
                    "tool_use_id":"question-1",
                    "content":"selected option 1"
                }]}
            }),
        );
        assert!(!meta.active_question);
        assert_eq!(meta.question_asked_at, None);
    }
}
