//! Cursor CLI (`cursor-agent`). Unlike Claude it won't take a caller-chosen
//! id, so a new conversation is minted with `create-chat` (which prints the
//! chat uuid) and every pane is started with `--resume <id>`.
//!
//! Metadata lives in per-chat SQLite stores under `~/.cursor/chats/<hash>/
//! <id>/store.db`: the chat name sits in the `meta` table, key `0`, as
//! hex-encoded JSON. corc opens those read-only and best-effort — a failed
//! read just leaves the row `(untitled)`, never an error. Turn state
//! comes from the latest message referenced by the store's root blob: user
//! prompts, tool calls and tool results are Mid; a final assistant text is
//! Complete once the store has stopped changing briefly. The exact turn start
//! comes from the matching JSONL agent transcript under `~/.cursor/projects`;
//! corc persists it while running and falls back to the store's activity time.

use super::Provider;
use crate::discovery::{Meta, MetaSource, TurnState};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Seek};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// Cursor writes a final assistant text incrementally. Waiting for the store
/// to stay unchanged avoids reporting Complete/Unseen while it is streaming.
const COMPLETE_SETTLE: Duration = Duration::from_secs(2);

pub struct Cursor;

impl Provider for Cursor {
    fn id(&self) -> &'static str {
        "cursor"
    }

    fn display_name(&self) -> &'static str {
        "Cursor CLI"
    }

    fn binary(&self) -> &'static str {
        "cursor-agent"
    }

    fn new_session_id(&self, dir: &Path) -> Result<String> {
        // cursor-agent can't be handed an id; it mints one and prints it.
        let bin = crate::tmux::resolve_binary(self.binary());
        let out = Command::new(&bin)
            .arg("create-chat")
            .current_dir(dir)
            .output()
            .with_context(|| format!("running {bin} create-chat"))?;
        if !out.status.success() {
            bail!(
                "{} create-chat failed: {}",
                self.binary(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if id.is_empty() {
            bail!("{} create-chat returned no id", self.binary());
        }
        Ok(id)
    }

    fn spawn_args(&self, id: &str, _resume: bool) -> Vec<String> {
        // New chats are pre-created via `create-chat`, so both the fresh and
        // the revive path attach with `--resume`.
        vec!["--resume".to_string(), id.to_string()]
    }

    fn meta_source(&self) -> Result<Box<dyn MetaSource>> {
        Ok(Box::new(CursorStore::new()?))
    }
}

struct Cached {
    /// Newest mtime across store.db / -wal / meta.json when last read; the
    /// store is re-read only when this advances.
    sig: SystemTime,
    /// Latest user-prompt timestamp found in Cursor's agent transcript.
    prompt_started_at: Option<u64>,
    meta: Meta,
}

#[derive(Default)]
struct TranscriptCache {
    /// Byte offset through the append-only JSONL transcript.
    offset: u64,
    /// Most recent user prompt carrying Cursor's injected `<timestamp>`.
    started_at: Option<u64>,
}

/// Reads chat titles and activity times from Cursor's local stores.
pub struct CursorStore {
    chats_root: PathBuf,
    projects_root: PathBuf,
    /// Located `<id>` directory per conversation, cached across refreshes.
    dirs: HashMap<String, PathBuf>,
    /// Located agent transcript per conversation, cached across refreshes.
    transcript_paths: HashMap<String, PathBuf>,
    transcripts: HashMap<String, TranscriptCache>,
    cache: HashMap<String, Cached>,
}

impl CursorStore {
    fn new() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(Self {
            chats_root: PathBuf::from(&home).join(".cursor/chats"),
            projects_root: PathBuf::from(&home).join(".cursor/projects"),
            dirs: HashMap::new(),
            transcript_paths: HashMap::new(),
            transcripts: HashMap::new(),
            cache: HashMap::new(),
        })
    }

    /// Incrementally scan Cursor's append-only agent transcript. Unlike the
    /// SQLite message blobs, each user prompt contains Cursor's injected wall
    /// clock timestamp, giving an exact turn start that survives corc restarts.
    fn refresh_transcript(&mut self, id: &str, cwd: &Path) -> Option<u64> {
        let path = match self.transcript_paths.get(id) {
            Some(path) if path.is_file() => path.clone(),
            _ => {
                let path = locate_transcript(&self.projects_root, cwd, id)?;
                self.transcript_paths.insert(id.to_string(), path.clone());
                path
            }
        };
        let len = fs::metadata(&path).ok()?.len();
        let cached = self.transcripts.entry(id.to_string()).or_default();
        if len < cached.offset {
            cached.offset = 0;
            cached.started_at = None;
        }
        if len == cached.offset {
            return cached.started_at;
        }

        let mut file = fs::File::open(path).ok()?;
        file.seek(std::io::SeekFrom::Start(cached.offset)).ok()?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line).ok()?;
            if read == 0 {
                break;
            }
            let parsed = serde_json::from_str::<serde_json::Value>(&line);
            if parsed.is_err() && !line.ends_with('\n') {
                // Cursor may be between writes; retry this partial line later.
                break;
            }
            cached.offset = cached.offset.saturating_add(read as u64);
            if let Ok(value) = parsed
                && let Some(started) = user_prompt_timestamp(&value)
            {
                cached.started_at = Some(started);
            }
        }
        cached.started_at
    }
}

impl MetaSource for CursorStore {
    fn refresh(&mut self, known: &[(String, PathBuf, Option<u64>)]) -> Result<()> {
        for (id, cwd, persisted_start) in known {
            let dir = match self.dirs.get(id) {
                Some(d) if d.is_dir() => d.clone(),
                _ => match locate(&self.chats_root, id) {
                    Some(d) => {
                        self.dirs.insert(id.clone(), d.clone());
                        d
                    }
                    // No store yet (chat created but nothing written): leave
                    // it (untitled), aged from its spawn time.
                    None => continue,
                },
            };
            let db = dir.join("store.db");
            let meta_json = dir.join("meta.json");
            let prompt_started_at = self.refresh_transcript(id, cwd).or(*persisted_start);
            let sig = latest_mtime(&[db.clone(), dir.join("store.db-wal"), meta_json.clone()]);
            if self
                .cache
                .get(id)
                .is_some_and(|c| c.sig == sig && c.prompt_started_at == prompt_started_at)
            {
                // A final assistant message is kept Mid for COMPLETE_SETTLE.
                // Re-evaluate unchanged candidates so they eventually become
                // Complete without requiring another filesystem write.
                if sig != SystemTime::UNIX_EPOCH
                    && SystemTime::now().duration_since(sig).unwrap_or_default() >= COMPLETE_SETTLE
                    && self
                        .cache
                        .get(id)
                        .is_some_and(|c| c.meta.turn_state == TurnState::Mid)
                    && read_store_info(&db).is_some_and(|i| i.turn == CursorTurn::CompleteCandidate)
                {
                    let completed = system_time_secs(sig);
                    if let Some(cached) = self.cache.get_mut(id) {
                        cached.meta.turn_state = TurnState::Complete;
                        cached.meta.turn_completed_at = completed;
                    }
                }
                continue;
            }
            let previous = self.cache.get(id).map(|c| c.meta.clone());
            let info = read_meta_json(&meta_json).unwrap_or_default();
            let store = read_store_info(&db).unwrap_or_default();
            let mut meta = Meta::default();
            if let Some(ts) = info
                .updated
                .or((sig != SystemTime::UNIX_EPOCH).then_some(sig))
            {
                meta.mtime = ts;
            }
            // A chat with no exchange yet is "empty" — no title — so leaving
            // it discards it, exactly like an untouched Claude conversation
            // (D17). Cursor may stamp a placeholder name before the first
            // message, so `hasConversation` is the signal, not the name.
            meta.has_content = info.has_conversation || store.has_content;
            if meta.has_content {
                meta.title = store.title.or(info.title);
            }
            let updated = info
                .updated
                .or((sig != SystemTime::UNIX_EPOCH).then_some(sig))
                .and_then(system_time_secs);
            let stable = sig != SystemTime::UNIX_EPOCH
                && SystemTime::now().duration_since(sig).unwrap_or_default() >= COMPLETE_SETTLE;
            apply_turn_observation(
                &mut meta,
                store.turn,
                stable,
                updated,
                prompt_started_at,
                previous.as_ref(),
            );
            self.cache.insert(
                id.clone(),
                Cached {
                    sig,
                    prompt_started_at,
                    meta,
                },
            );
        }
        self.cache
            .retain(|id, _| known.iter().any(|(k, _, _)| k == id));
        self.dirs
            .retain(|id, _| known.iter().any(|(k, _, _)| k == id));
        self.transcript_paths
            .retain(|id, _| known.iter().any(|(k, _, _)| k == id));
        self.transcripts
            .retain(|id, _| known.iter().any(|(k, _, _)| k == id));
        Ok(())
    }

    fn meta(&self, id: &str) -> Option<&Meta> {
        self.cache.get(id).map(|c| &c.meta)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum CursorTurn {
    #[default]
    Unknown,
    Mid,
    CompleteCandidate,
}

#[derive(Default)]
struct StoreInfo {
    title: Option<String>,
    has_content: bool,
    turn: CursorTurn,
}

/// Carry timing across the store's state transitions. `start_hint` comes from
/// the user prompt in Cursor's agent transcript, then from corc's persisted
/// in-flight start if the transcript is unavailable. `updatedAtMs` remains the
/// final coarse fallback and supplies completion time.
fn apply_turn_observation(
    meta: &mut Meta,
    observed: CursorTurn,
    stable: bool,
    updated: Option<u64>,
    start_hint: Option<u64>,
    previous: Option<&Meta>,
) {
    match observed {
        CursorTurn::Mid => {
            meta.turn_state = TurnState::Mid;
            meta.turn_started_at = start_hint
                .or_else(|| {
                    previous
                        .filter(|m| m.turn_state == TurnState::Mid)
                        .and_then(|m| m.turn_started_at)
                })
                .or(updated);
        }
        CursorTurn::CompleteCandidate if !stable => {
            meta.turn_state = TurnState::Mid;
            meta.turn_started_at = start_hint
                .or_else(|| {
                    previous
                        .filter(|m| m.turn_state == TurnState::Mid)
                        .and_then(|m| m.turn_started_at)
                })
                .or(updated);
        }
        CursorTurn::CompleteCandidate => {
            meta.turn_state = TurnState::Complete;
            meta.turn_started_at = start_hint.or_else(|| previous.and_then(|m| m.turn_started_at));
            meta.turn_completed_at = previous
                .filter(|m| m.turn_state == TurnState::Complete)
                .and_then(|m| m.turn_completed_at)
                .or(updated);
        }
        CursorTurn::Unknown => {}
    }
}

/// Find the `<id>` chat directory: `~/.cursor/chats/<hash>/<id>`. The outer
/// hash is opaque, so we scan one level for a child named by the id.
fn locate(chats_root: &Path, id: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(chats_root).ok()?.flatten() {
        let cand = entry.path().join(id);
        if cand.is_dir() {
            return Some(cand);
        }
    }
    None
}

/// Find `~/.cursor/projects/<workspace>/agent-transcripts/<id>/<id>.jsonl`.
/// Try Cursor's path-derived workspace name first, then scan one level as a
/// schema-drift fallback. The result is cached by the caller.
fn locate_transcript(projects_root: &Path, cwd: &Path, id: &str) -> Option<PathBuf> {
    let workspace: String = cwd
        .to_string_lossy()
        .trim_start_matches('/')
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let relative = Path::new("agent-transcripts")
        .join(id)
        .join(format!("{id}.jsonl"));
    let direct = projects_root.join(workspace).join(&relative);
    if direct.is_file() {
        return Some(direct);
    }
    for entry in fs::read_dir(projects_root).ok()?.flatten() {
        let candidate = entry.path().join(&relative);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn user_prompt_timestamp(value: &serde_json::Value) -> Option<u64> {
    if value.get("role").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }
    let content = value.get("message")?.get("content")?;
    if let Some(text) = content.as_str() {
        return timestamp_from_text(text);
    }
    content.as_array()?.iter().find_map(|part| {
        (part.get("type").and_then(|v| v.as_str()) == Some("text"))
            .then(|| part.get("text")?.as_str().and_then(timestamp_from_text))
            .flatten()
    })
}

fn timestamp_from_text(text: &str) -> Option<u64> {
    let start = text.find("<timestamp>")? + "<timestamp>".len();
    let end = text.get(start..)?.find("</timestamp>")? + start;
    parse_cursor_timestamp(text.get(start..end)?)
}

/// Parse Cursor's prompt context timestamp, for example
/// `Friday, Jul 10, 2026, 3:24 PM (UTC+2)`, into unix seconds.
fn parse_cursor_timestamp(value: &str) -> Option<u64> {
    let (date_time, zone) = value.trim().rsplit_once(" (UTC")?;
    let zone = zone.strip_suffix(')')?;
    let mut parts = date_time.split(',').map(str::trim);
    let _weekday = parts.next()?;
    let month_day = parts.next()?;
    let year: i64 = parts.next()?.parse().ok()?;
    let clock = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let mut month_day = month_day.split_whitespace();
    let month = match month_day.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day: i64 = month_day.next()?.parse().ok()?;
    if month_day.next().is_some() || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }

    let mut clock = clock.split_whitespace();
    let hms = clock.next()?;
    let meridiem = clock.next()?;
    if clock.next().is_some() {
        return None;
    }
    let mut hms = hms.split(':');
    let hour12: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    let second: i64 = hms.next().map(str::parse).transpose().ok()?.unwrap_or(0);
    if hms.next().is_some()
        || !(1..=12).contains(&hour12)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
    {
        return None;
    }
    let hour = match meridiem {
        "AM" => hour12 % 12,
        "PM" => hour12 % 12 + 12,
        _ => return None,
    };
    let offset = parse_utc_offset(zone)?;
    let epoch =
        days_from_civil(year, month, day) * 86400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(epoch).ok()
}

fn parse_utc_offset(value: &str) -> Option<i64> {
    if value.is_empty() {
        return Some(0);
    }
    let (sign, value) = match value.as_bytes().first()? {
        b'+' => (1, &value[1..]),
        b'-' => (-1, &value[1..]),
        _ => return None,
    };
    let (hours, minutes) = match value.split_once(':') {
        Some((hours, minutes)) => (hours.parse::<i64>().ok()?, minutes.parse::<i64>().ok()?),
        None => (value.parse::<i64>().ok()?, 0),
    };
    if !(0..=23).contains(&hours) || !(0..60).contains(&minutes) {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

/// Days since 1970-01-01 (Howard Hinnant's civil-date algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146097 + day_of_era - 719468
}

/// Newest mtime among the paths that exist; `UNIX_EPOCH` if none do.
fn latest_mtime(paths: &[PathBuf]) -> SystemTime {
    paths
        .iter()
        .filter_map(|p| fs::metadata(p).ok()?.modified().ok())
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// The chat name and latest turn shape from `meta` key `0` and its referenced
/// root/message blobs. Any schema drift or read failure degrades to defaults.
fn read_store_info(db: &Path) -> Option<StoreInfo> {
    if !db.is_file() {
        return None;
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let hex: String = conn
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |r| r.get(0))
        .ok()?;
    let json = hex_decode(&hex)?;
    let value: serde_json::Value = serde_json::from_slice(&json).ok()?;
    let title = value
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let message = value
        .get("latestRootBlobId")
        .and_then(|v| v.as_str())
        .and_then(|root| latest_message(&conn, root));
    let has_content = message.as_ref().is_some_and(message_has_content);
    let turn = message.as_ref().map(classify_message).unwrap_or_default();
    Some(StoreInfo {
        title,
        has_content,
        turn,
    })
}

/// Resolve the last message through Cursor's root blob. The root is protobuf;
/// field 1 contains repeated raw 32-byte SHA-256 blob ids in display order.
fn latest_message(conn: &Connection, root_id: &str) -> Option<serde_json::Value> {
    let root: Vec<u8> = conn
        .query_row("SELECT data FROM blobs WHERE id = ?1", [root_id], |r| {
            r.get(0)
        })
        .ok()?;
    let message_id = last_root_message_id(&root)?;
    let message: Vec<u8> = conn
        .query_row("SELECT data FROM blobs WHERE id = ?1", [message_id], |r| {
            r.get(0)
        })
        .ok()?;
    serde_json::from_slice(&message).ok()
}

fn classify_message(message: &serde_json::Value) -> CursorTurn {
    match message.get("role").and_then(|v| v.as_str()) {
        Some("user" | "tool") => CursorTurn::Mid,
        Some("assistant") => {
            let content = message.get("content").and_then(|v| v.as_array());
            let has_tool_call = content.is_some_and(|parts| {
                parts
                    .iter()
                    .any(|p| p.get("type").and_then(|v| v.as_str()) == Some("tool-call"))
            });
            let has_text = content.is_some_and(|parts| {
                parts
                    .iter()
                    .any(|p| p.get("type").and_then(|v| v.as_str()) == Some("text"))
            });
            if has_text && !has_tool_call {
                CursorTurn::CompleteCandidate
            } else {
                CursorTurn::Mid
            }
        }
        _ => CursorTurn::Unknown,
    }
}

fn message_has_content(message: &serde_json::Value) -> bool {
    matches!(
        message.get("role").and_then(|v| v.as_str()),
        Some("user" | "assistant" | "tool")
    )
}

fn last_root_message_id(data: &[u8]) -> Option<String> {
    let mut pos = 0usize;
    let mut last = None;
    while pos < data.len() {
        let key = read_varint(data, &mut pos)?;
        let field = key >> 3;
        match key & 7 {
            0 => {
                read_varint(data, &mut pos)?;
            }
            1 => pos = pos.checked_add(8)?,
            2 => {
                let len = usize::try_from(read_varint(data, &mut pos)?).ok()?;
                let end = pos.checked_add(len)?;
                let bytes = data.get(pos..end)?;
                if field == 1 && bytes.len() == 32 {
                    last = Some(bytes.iter().map(|b| format!("{b:02x}")).collect());
                }
                pos = end;
            }
            5 => pos = pos.checked_add(4)?,
            _ => return None,
        }
        if pos > data.len() {
            return None;
        }
    }
    last
}

fn read_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..=63).step_by(7) {
        let byte = *data.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn system_time_secs(time: SystemTime) -> Option<u64> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// The fields corc reads from a chat's `meta.json`.
#[derive(Default)]
struct MetaJson {
    /// `updatedAtMs` as wall-clock — the coarse "last activity" the Idle/Dead
    /// time column ages from.
    updated: Option<SystemTime>,
    /// Whether a real message exchange has happened — the emptiness signal.
    has_conversation: bool,
    /// The title cursor writes here once the chat has content; a cheap
    /// fallback if the SQLite store can't be read.
    title: Option<String>,
}

fn read_meta_json(meta_json: &Path) -> Option<MetaJson> {
    let text = fs::read_to_string(meta_json).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let updated = value
        .get("updatedAtMs")
        .and_then(|x| x.as_u64())
        .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms));
    let has_conversation = value
        .get("hasConversation")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let title = value
        .get("title")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(MetaJson {
        updated,
        has_conversation,
        title,
    })
}

/// Decode a lowercase/uppercase hex string to bytes; None on odd length or a
/// non-hex digit.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim().as_bytes();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.chunks_exact(2) {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{
        CursorStore, CursorTurn, Meta, TurnState, apply_turn_observation, classify_message,
        hex_decode, last_root_message_id, message_has_content, parse_cursor_timestamp,
        read_meta_json, user_prompt_timestamp,
    };
    use crate::discovery::MetaSource;
    use rusqlite::{Connection, params};
    use serde_json::json;
    use std::path::PathBuf;

    /// A chat with `hasConversation:false` reports no activity — it reads as
    /// empty, so leaving it discards the conversation (D17); once a message
    /// lands, `hasConversation` flips true and the title comes through.
    #[test]
    fn meta_json_emptiness() {
        let dir = std::env::temp_dir().join(format!("corc-cursor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, body: &str| -> PathBuf {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            p
        };

        let empty = write(
            "empty.json",
            r#"{"schemaVersion":1,"createdAtMs":1,"hasConversation":false,"updatedAtMs":2,"cwd":"/x"}"#,
        );
        let m = read_meta_json(&empty).unwrap();
        assert!(!m.has_conversation);
        assert_eq!(m.title, None);

        let started = write(
            "started.json",
            r#"{"schemaVersion":1,"hasConversation":true,"title":"Hello There","updatedAtMs":3,"cwd":"/x"}"#,
        );
        let m = read_meta_json(&started).unwrap();
        assert!(m.has_conversation);
        assert_eq!(m.title.as_deref(), Some("Hello There"));

        // A missing meta.json is treated as empty (default), not an error.
        assert!(read_meta_json(&dir.join("nope.json")).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex_decode("7b7d"), Some(b"{}".to_vec()));
        // The real payload is hex-encoded JSON.
        let json = br#"{"name":"Hello There"}"#;
        let hex: String = json.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex_decode(&hex).as_deref(), Some(&json[..]));
        assert_eq!(hex_decode("abc"), None); // odd length
        assert_eq!(hex_decode("zz"), None); // non-hex
    }

    #[test]
    fn message_turn_state() {
        let user = json!({"role":"user","content":[{"type":"text"}]});
        assert!(message_has_content(&user));
        assert_eq!(classify_message(&user), CursorTurn::Mid);
        assert_eq!(
            classify_message(&json!({"role":"tool","content":[{"type":"tool-result"}]})),
            CursorTurn::Mid
        );
        assert_eq!(
            classify_message(
                &json!({"role":"assistant","content":[{"type":"reasoning"},{"type":"tool-call"}]})
            ),
            CursorTurn::Mid
        );
        assert_eq!(
            classify_message(
                &json!({"role":"assistant","content":[{"type":"reasoning"},{"type":"text"}]})
            ),
            CursorTurn::CompleteCandidate
        );
        assert!(!message_has_content(&json!({"role":"system","content":[]})));
    }

    #[test]
    fn root_uses_last_message_hash() {
        let first = [0x11; 32];
        let second = [0x22; 32];
        let mut root = vec![0x0a, 32];
        root.extend(first);
        root.extend([0x50, 1]); // unrelated protobuf field 10, varint 1
        root.extend([0x0a, 32]);
        root.extend(second);
        assert_eq!(last_root_message_id(&root), Some("22".repeat(32)));
        assert_eq!(last_root_message_id(&[0x0a, 33, 1, 2]), None);
    }

    #[test]
    fn turn_transitions_preserve_timing() {
        let mut running = Meta::default();
        apply_turn_observation(
            &mut running,
            CursorTurn::Mid,
            false,
            Some(110),
            Some(100),
            None,
        );
        assert_eq!(running.turn_state, TurnState::Mid);
        assert_eq!(running.turn_started_at, Some(100));

        let mut streaming = Meta::default();
        apply_turn_observation(
            &mut streaming,
            CursorTurn::CompleteCandidate,
            false,
            Some(110),
            None,
            Some(&running),
        );
        assert_eq!(streaming.turn_state, TurnState::Mid);
        assert_eq!(streaming.turn_started_at, Some(100));

        let mut complete = Meta::default();
        apply_turn_observation(
            &mut complete,
            CursorTurn::CompleteCandidate,
            true,
            Some(120),
            None,
            Some(&streaming),
        );
        assert_eq!(complete.turn_state, TurnState::Complete);
        assert_eq!(complete.turn_started_at, Some(100));
        assert_eq!(complete.turn_completed_at, Some(120));
    }

    #[test]
    fn cursor_prompt_timestamp() {
        assert_eq!(
            parse_cursor_timestamp("Friday, Jul 10, 2026, 3:24 PM (UTC+2)"),
            Some(1783689840)
        );
        assert_eq!(
            parse_cursor_timestamp("Thursday, Jan 1, 2026, 12:00 AM (UTC)"),
            Some(1767225600)
        );
        assert!(parse_cursor_timestamp("Friday, Feb 30, 2026, 3:24 PM (UTC+2)").is_none());

        let prompt = json!({
            "role": "user",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "<timestamp>Friday, Jul 10, 2026, 3:24 PM (UTC+2)</timestamp>\n<user_query>hello</user_query>"
                }]
            }
        });
        assert_eq!(user_prompt_timestamp(&prompt), Some(1783689840));
        assert_eq!(
            user_prompt_timestamp(&json!({
                "role": "assistant",
                "message": {"content": "<timestamp>Friday, Jul 10, 2026, 3:24 PM (UTC+2)</timestamp>"}
            })),
            None
        );
    }

    #[test]
    fn transcript_scan_is_incremental() {
        let base = std::env::temp_dir().join(format!(
            "corc-cursor-transcript-test-{}",
            std::process::id()
        ));
        let id = "chat-id";
        let cwd = PathBuf::from("/tmp/example");
        let transcript = base
            .join("projects/tmp-example/agent-transcripts")
            .join(id)
            .join(format!("{id}.jsonl"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            concat!(
                "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":",
                "\"<timestamp>Friday, Jul 10, 2026, 3:24 PM (UTC+2)</timestamp>\"}]}}\n",
                "{\"role\":\"assistant\",\"message\":{\"content\":[]}}\n"
            ),
        )
        .unwrap();

        let mut store = CursorStore {
            chats_root: base.join("chats"),
            projects_root: base.join("projects"),
            dirs: Default::default(),
            transcript_paths: Default::default(),
            transcripts: Default::default(),
            cache: Default::default(),
        };
        assert_eq!(store.refresh_transcript(id, &cwd), Some(1783689840));
        let first_offset = store.transcripts[id].offset;

        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap(),
            "{}",
            json!({
                "role": "user",
                "message": {"content": [{
                    "type": "text",
                    "text": "<timestamp>Friday, Jul 10, 2026, 4:00 PM (UTC+2)</timestamp>"
                }]}
            })
        )
        .unwrap();
        assert_eq!(store.refresh_transcript(id, &cwd), Some(1783692000));
        assert!(store.transcripts[id].offset > first_offset);

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn conversation_content_does_not_require_a_title() {
        let base =
            std::env::temp_dir().join(format!("corc-cursor-content-test-{}", std::process::id()));
        let id = "chat-without-title";
        let chat = base.join("workspace").join(id);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&chat).unwrap();

        let db = Connection::open(chat.join("store.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        let message_id = "22".repeat(32);
        let mut root: Vec<u8> = vec![0x0a, 32];
        root.extend([0x22u8; 32]);
        db.execute(
            "INSERT INTO blobs (id, data) VALUES (?1, ?2)",
            params!["root", root],
        )
        .unwrap();
        db.execute(
            "INSERT INTO blobs (id, data) VALUES (?1, ?2)",
            params![
                message_id,
                br#"{"role":"user","content":[{"type":"text","text":"hello"}]}"#
            ],
        )
        .unwrap();
        let store_meta = br#"{"latestRootBlobId":"root"}"#;
        let encoded: String = store_meta.iter().map(|b| format!("{b:02x}")).collect();
        db.execute("INSERT INTO meta (key, value) VALUES ('0', ?1)", [encoded])
            .unwrap();
        drop(db);
        std::fs::write(
            chat.join("meta.json"),
            r#"{"hasConversation":true,"updatedAtMs":1000}"#,
        )
        .unwrap();

        let mut store = CursorStore {
            chats_root: base.clone(),
            projects_root: base.join("projects"),
            dirs: Default::default(),
            transcript_paths: Default::default(),
            transcripts: Default::default(),
            cache: Default::default(),
        };
        store
            .refresh(&[(id.to_string(), PathBuf::from("/tmp"), None)])
            .unwrap();
        let meta = store.meta(id).unwrap();
        assert!(meta.has_content);
        assert_eq!(meta.display_title(), None);

        let _ = std::fs::remove_dir_all(base);
    }
}
