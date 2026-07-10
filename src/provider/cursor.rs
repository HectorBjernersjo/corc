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
//! Complete once the store has stopped changing briefly.

use super::Provider;
use crate::discovery::{Meta, MetaSource, TurnState};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::fs;
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
    meta: Meta,
}

/// Reads chat titles and activity times from Cursor's SQLite stores.
pub struct CursorStore {
    chats_root: PathBuf,
    /// Located `<id>` directory per conversation, cached across refreshes.
    dirs: HashMap<String, PathBuf>,
    cache: HashMap<String, Cached>,
}

impl CursorStore {
    fn new() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(Self {
            chats_root: PathBuf::from(home).join(".cursor/chats"),
            dirs: HashMap::new(),
            cache: HashMap::new(),
        })
    }
}

impl MetaSource for CursorStore {
    fn refresh(&mut self, known: &[(String, PathBuf)]) -> Result<()> {
        for (id, _cwd) in known {
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
            let sig = latest_mtime(&[db.clone(), dir.join("store.db-wal"), meta_json.clone()]);
            if self.cache.get(id).is_some_and(|c| c.sig == sig) {
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
            if info.has_conversation {
                meta.title = store.title.or(info.title);
            }
            let updated = info
                .updated
                .or((sig != SystemTime::UNIX_EPOCH).then_some(sig))
                .and_then(system_time_secs);
            let stable = sig != SystemTime::UNIX_EPOCH
                && SystemTime::now().duration_since(sig).unwrap_or_default() >= COMPLETE_SETTLE;
            apply_turn_observation(&mut meta, store.turn, stable, updated, previous.as_ref());
            self.cache.insert(id.clone(), Cached { sig, meta });
        }
        self.cache
            .retain(|id, _| known.iter().any(|(k, _)| k == id));
        self.dirs.retain(|id, _| known.iter().any(|(k, _)| k == id));
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
    turn: CursorTurn,
}

/// Carry timing across the store's state transitions. Cursor does not persist
/// per-message timestamps, so a running turn's start is captured on the first
/// refresh that observes it. A completed chat discovered on startup still gets
/// its completion timestamp from `updatedAtMs`, which is enough for Unseen.
fn apply_turn_observation(
    meta: &mut Meta,
    observed: CursorTurn,
    stable: bool,
    updated: Option<u64>,
    previous: Option<&Meta>,
) {
    match observed {
        CursorTurn::Mid => {
            meta.turn_state = TurnState::Mid;
            meta.turn_started_at = previous
                .filter(|m| m.turn_state == TurnState::Mid)
                .and_then(|m| m.turn_started_at)
                .or(updated);
        }
        CursorTurn::CompleteCandidate if !stable => {
            meta.turn_state = TurnState::Mid;
            meta.turn_started_at = previous
                .filter(|m| m.turn_state == TurnState::Mid)
                .and_then(|m| m.turn_started_at)
                .or(updated);
        }
        CursorTurn::CompleteCandidate => {
            meta.turn_state = TurnState::Complete;
            meta.turn_started_at = previous.and_then(|m| m.turn_started_at);
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
    let turn = value
        .get("latestRootBlobId")
        .and_then(|v| v.as_str())
        .and_then(|root| latest_message(&conn, root))
        .map(|message| classify_message(&message))
        .unwrap_or_default();
    Some(StoreInfo { title, turn })
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
        CursorTurn, Meta, TurnState, apply_turn_observation, classify_message, hex_decode,
        last_root_message_id, read_meta_json,
    };
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
        assert_eq!(
            classify_message(&json!({"role":"user","content":[{"type":"text"}]})),
            CursorTurn::Mid
        );
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
        apply_turn_observation(&mut running, CursorTurn::Mid, false, Some(100), None);
        assert_eq!(running.turn_state, TurnState::Mid);
        assert_eq!(running.turn_started_at, Some(100));

        let mut streaming = Meta::default();
        apply_turn_observation(
            &mut streaming,
            CursorTurn::CompleteCandidate,
            false,
            Some(110),
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
            Some(&streaming),
        );
        assert_eq!(complete.turn_state, TurnState::Complete);
        assert_eq!(complete.turn_started_at, Some(100));
        assert_eq!(complete.turn_completed_at, Some(120));
    }
}
