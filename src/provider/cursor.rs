//! Cursor CLI (`cursor-agent`). Unlike Claude it won't take a caller-chosen
//! id, so a new conversation is minted with `create-chat` (which prints the
//! chat uuid) and every pane is started with `--resume <id>`.
//!
//! Metadata lives in per-chat SQLite stores under `~/.cursor/chats/<hash>/
//! <id>/store.db`: the chat name sits in the `meta` table, key `0`, as
//! hex-encoded JSON. corc opens those read-only and best-effort — a failed
//! read just leaves the row `(untitled)`, never an error. Turn state
//! (running/unseen) is not derived: the sidebar shows Idle/Dead by pane
//! liveness alone until the message log is understood.

use super::Provider;
use crate::discovery::{Meta, MetaSource};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

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
                continue;
            }
            let info = read_meta_json(&meta_json).unwrap_or_default();
            let mut meta = Meta::default();
            if let Some(ts) = info.updated {
                meta.mtime = ts;
            }
            // A chat with no exchange yet is "empty" — no title — so leaving
            // it discards it, exactly like an untouched Claude conversation
            // (D17). Cursor may stamp a placeholder name before the first
            // message, so `hasConversation` is the signal, not the name.
            if info.has_conversation {
                meta.title = read_title(&db).or(info.title);
            }
            self.cache.insert(id.clone(), Cached { sig, meta });
        }
        self.cache.retain(|id, _| known.iter().any(|(k, _)| k == id));
        self.dirs.retain(|id, _| known.iter().any(|(k, _)| k == id));
        Ok(())
    }

    fn meta(&self, id: &str) -> Option<&Meta> {
        self.cache.get(id).map(|c| &c.meta)
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

/// The chat name from `meta` key `0` (hex-encoded JSON, `name` field). Any
/// failure — missing db, WAL that needs recovery, schema drift — yields None.
fn read_title(db: &Path) -> Option<String> {
    if !db.is_file() {
        return None;
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let hex: String = conn
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |r| r.get(0))
        .ok()?;
    let json = hex_decode(&hex)?;
    let value: serde_json::Value = serde_json::from_slice(&json).ok()?;
    let name = value.get("name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_string())
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
    use super::{hex_decode, read_meta_json};
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
}
