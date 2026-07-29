//! OpenCode CLI (`opencode`). The TUI creates its session only when the
//! first prompt is submitted and does not accept a caller-chosen id. Fresh
//! conversations therefore start under a corc-minted provisional id; once
//! OpenCode writes the matching top-level row to its SQLite database, corc
//! adopts the real `ses_...` id. Dead conversations resume with
//! `opencode --session <id>`.
//!
//! OpenCode keeps sessions, messages and parts in
//! `$XDG_DATA_HOME/opencode/opencode.db` (normally
//! `~/.local/share/opencode/opencode.db`). The metadata reader opens that
//! database read-only. A missing database or transient/schema read failure
//! simply leaves the last good sidebar snapshot in place.

use super::Provider;
use crate::discovery::{self, Meta, MetaSource, TurnState};
use crate::state;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const PENDING_PREFIX: &str = "pending-opencode-";
const DEFAULT_TITLE_PREFIX: &str = "New session - ";

pub struct OpenCode;

impl Provider for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn display_name(&self) -> &'static str {
        "OpenCode"
    }

    fn binary(&self) -> &'static str {
        "opencode"
    }

    fn new_session_id(&self, _dir: &Path) -> Result<String> {
        Ok(format!("{PENDING_PREFIX}{}", state::new_uuid()?))
    }

    fn spawn_args(&self, id: &str, resume: bool) -> Vec<String> {
        if resume && !self.is_pending(id) {
            vec!["--session".to_string(), id.to_string()]
        } else {
            // The home screen creates a real session when its first prompt
            // is submitted. A pending conversation has nothing to resume.
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
        let since_ms = since
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64;
        Ok(resolve(&database_path()?, dir, since_ms, taken))
    }

    fn meta_source(&self) -> Result<Box<dyn MetaSource>> {
        Ok(Box::new(OpenCodeStore {
            database: database_path()?,
            cache: HashMap::new(),
        }))
    }
}

fn database_path() -> Result<PathBuf> {
    if let Some(data) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        let data = PathBuf::from(data);
        if data.is_absolute() {
            return Ok(data.join("opencode/opencode.db"));
        }
    }
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".local/share/opencode/opencode.db"))
}

/// Open without CREATE so merely starting corc never creates or migrates an
/// optional OpenCode installation. A short busy timeout lets an in-progress
/// OpenCode write settle without stalling the TUI refresh loop.
fn open_read_only(path: &Path) -> Option<Connection> {
    if !path.is_file() {
        return None;
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(25));
    Some(conn)
}

/// Find the earliest unclaimed top-level OpenCode session created for this
/// cwd at/after the pane was spawned. Child sessions are OpenCode subagents,
/// never corc conversations. Five seconds of slack absorbs the state file's
/// second precision and small clock/order differences.
fn resolve(path: &Path, cwd: &Path, since_ms: i64, taken: &[String]) -> Option<String> {
    let conn = open_read_only(path)?;
    let mut stmt = conn
        .prepare(
            "SELECT id
             FROM session
             WHERE parent_id IS NULL
               AND directory = ?1
               AND time_created + 5000 >= ?2
             ORDER BY time_created ASC, id ASC",
        )
        .ok()?;
    let rows = stmt
        .query_map(params![cwd.to_string_lossy().as_ref(), since_ms], |row| {
            row.get::<_, String>(0)
        })
        .ok()?;
    rows.filter_map(Result::ok)
        .find(|id| id.starts_with("ses_") && !taken.iter().any(|claimed| claimed == id))
}

pub struct OpenCodeStore {
    database: PathBuf,
    cache: HashMap<String, Meta>,
}

impl MetaSource for OpenCodeStore {
    fn refresh(&mut self, known: &[(String, PathBuf, Option<u64>)]) -> Result<()> {
        let Some(conn) = open_read_only(&self.database) else {
            return Ok(());
        };

        for (id, _, persisted_start) in known {
            if id.starts_with(PENDING_PREFIX) {
                continue;
            }
            match read_meta(&conn, id, *persisted_start) {
                Ok(Some(meta)) => {
                    self.cache.insert(id.clone(), meta);
                }
                Ok(None) => {
                    self.cache.remove(id);
                }
                // OpenCode may be migrating or holding a brief write lock.
                // Preserve the previous good observation and retry next tick.
                Err(_) => {}
            }
        }
        self.cache
            .retain(|id, _| known.iter().any(|(known_id, _, _)| known_id == id));
        Ok(())
    }

    fn meta(&self, id: &str) -> Option<&Meta> {
        self.cache.get(id)
    }
}

struct SessionRow {
    title: String,
    updated_ms: i64,
}

struct MessageRow {
    id: String,
    created_ms: i64,
    data: Value,
}

fn read_meta(conn: &Connection, id: &str, persisted_start: Option<u64>) -> Result<Option<Meta>> {
    let session = conn
        .query_row(
            "SELECT title, time_updated
             FROM session
             WHERE id = ?1 AND parent_id IS NULL",
            [id],
            |row| {
                Ok(SessionRow {
                    title: row.get(0)?,
                    updated_ms: row.get(1)?,
                })
            },
        )
        .optional()?;
    let Some(session) = session else {
        return Ok(None);
    };

    let first_prompt = first_prompt(conn, id)?;
    let latest_user = latest_real_user(conn, id)?;
    let mut meta = Meta {
        has_content: latest_user.is_some(),
        first_prompt,
        mtime: millis_to_system_time(session.updated_ms),
        ..Meta::default()
    };
    if !session.title.starts_with(DEFAULT_TITLE_PREFIX) {
        meta.title = Some(session.title);
    }

    if let Some(user) = latest_user {
        meta.turn_started_at = json_millis(&user.data, &["time", "created"])
            .or_else(|| nonnegative(user.created_ms))
            .map(millis_to_secs)
            .or(persisted_start);
        let assistant = latest_assistant_for(conn, id, &user.id)?;
        let completed_ms = assistant
            .as_ref()
            .and_then(|message| json_millis(&message.data, &["time", "completed"]));
        let tool_running = match assistant.as_ref() {
            Some(message) => has_running_tool(conn, &message.id)?,
            None => false,
        };
        if let Some(completed) = completed_ms.filter(|_| !tool_running) {
            meta.turn_state = TurnState::Complete;
            meta.turn_completed_at = Some(millis_to_secs(completed));
        } else {
            meta.turn_state = TurnState::Mid;
        }
    }

    Ok(Some(meta))
}

fn first_prompt(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    let raw = conn
        .query_row(
            "SELECT p.data
             FROM message m
             JOIN part p ON p.message_id = m.id
             WHERE m.session_id = ?1
               AND json_extract(m.data, '$.role') = 'user'
               AND COALESCE(json_extract(m.data, '$.synthetic'), 0) = 0
               AND json_extract(p.data, '$.type') = 'text'
               AND COALESCE(json_extract(p.data, '$.ignored'), 0) = 0
             ORDER BY m.time_created ASC, m.id ASC, p.id ASC
             LIMIT 1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(raw
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|part| part.get("text")?.as_str().and_then(discovery::title_line)))
}

fn latest_real_user(conn: &Connection, session_id: &str) -> Result<Option<MessageRow>> {
    latest_message(
        conn,
        "SELECT m.id, m.time_created, m.data
         FROM message m
         WHERE m.session_id = ?1
           AND json_extract(m.data, '$.role') = 'user'
           AND COALESCE(json_extract(m.data, '$.synthetic'), 0) = 0
           AND EXISTS (
               SELECT 1 FROM part p
               WHERE p.message_id = m.id
                 AND json_extract(p.data, '$.type') = 'text'
                 AND COALESCE(json_extract(p.data, '$.ignored'), 0) = 0
           )
         ORDER BY m.time_created DESC, m.id DESC
         LIMIT 1",
        session_id,
    )
}

fn latest_assistant_for(
    conn: &Connection,
    session_id: &str,
    user_id: &str,
) -> Result<Option<MessageRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, time_created, data
         FROM message
         WHERE session_id = ?1
           AND json_extract(data, '$.role') = 'assistant'
           AND json_extract(data, '$.parentID') = ?2
         ORDER BY time_created DESC, id DESC
         LIMIT 1",
    )?;
    stmt.query_row(params![session_id, user_id], message_from_row)
        .optional()
        .map_err(Into::into)
}

fn latest_message(conn: &Connection, sql: &str, value: &str) -> Result<Option<MessageRow>> {
    let mut stmt = conn.prepare(sql)?;
    stmt.query_row([value], message_from_row)
        .optional()
        .map_err(Into::into)
}

fn message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    let raw: String = row.get(2)?;
    let data = serde_json::from_str(&raw).unwrap_or(Value::Null);
    Ok(MessageRow {
        id: row.get(0)?,
        created_ms: row.get(1)?,
        data,
    })
}

fn has_running_tool(conn: &Connection, message_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1
             FROM part
             WHERE message_id = ?1
               AND json_extract(data, '$.type') = 'tool'
               AND json_extract(data, '$.state.status') IN ('pending', 'running')
             LIMIT 1",
            [message_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn json_millis(value: &Value, path: &[&str]) -> Option<u64> {
    json_path(value, path).and_then(Value::as_u64)
}

fn json_path<'a>(mut value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    for key in path {
        value = value.get(*key)?;
    }
    Some(value)
}

fn nonnegative(value: i64) -> Option<u64> {
    u64::try_from(value).ok()
}

fn millis_to_secs(value: u64) -> u64 {
    value / 1000
}

fn millis_to_system_time(value: i64) -> SystemTime {
    nonnegative(value)
        .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::MetaSource;
    use rusqlite::{Connection, params};
    use serde_json::json;

    fn fixture(name: &str) -> (PathBuf, Connection) {
        let root =
            std::env::temp_dir().join(format!("corc-opencode-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("opencode.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                directory TEXT NOT NULL,
                title TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL
             );
             CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
             );
             CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
             );",
        )
        .unwrap();
        (path, conn)
    }

    fn insert_session(
        conn: &Connection,
        id: &str,
        parent: Option<&str>,
        directory: &str,
        title: &str,
        created: i64,
        updated: i64,
    ) {
        conn.execute(
            "INSERT INTO session
             (id, parent_id, directory, title, time_created, time_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, parent, directory, title, created, updated],
        )
        .unwrap();
    }

    fn insert_message(conn: &Connection, id: &str, session: &str, time: i64, data: Value) {
        conn.execute(
            "INSERT INTO message
             (id, session_id, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?3, ?4)",
            params![id, session, time, data.to_string()],
        )
        .unwrap();
    }

    fn insert_part(conn: &Connection, id: &str, message: &str, session: &str, data: Value) {
        conn.execute(
            "INSERT INTO part
             (id, message_id, session_id, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, 0, 0, ?4)",
            params![id, message, session, data.to_string()],
        )
        .unwrap();
    }

    #[test]
    fn spawn_and_resume_arguments() {
        let provider = OpenCode;
        let pending = "pending-opencode-123";
        assert!(provider.is_pending(pending));
        assert_eq!(provider.spawn_args(pending, false), Vec::<String>::new());
        assert_eq!(provider.spawn_args(pending, true), Vec::<String>::new());
        assert_eq!(
            provider.spawn_args("ses_real", true),
            vec!["--session".to_string(), "ses_real".to_string()]
        );
    }

    #[test]
    fn resolve_chooses_unclaimed_top_level_session_for_cwd() {
        let (path, conn) = fixture("resolve");
        insert_session(&conn, "ses_old", None, "/work/app", "old", 90_000, 90_000);
        insert_session(
            &conn,
            "ses_wrong_cwd",
            None,
            "/work/other",
            "wrong",
            100_000,
            100_000,
        );
        insert_session(
            &conn,
            "ses_child",
            Some("ses_parent"),
            "/work/app",
            "child",
            100_100,
            100_100,
        );
        insert_session(
            &conn,
            "ses_taken",
            None,
            "/work/app",
            "taken",
            100_200,
            100_200,
        );
        insert_session(
            &conn,
            "ses_match",
            None,
            "/work/app",
            "match",
            100_300,
            100_300,
        );
        drop(conn);

        assert_eq!(
            resolve(
                &path,
                Path::new("/work/app"),
                100_000,
                &["ses_taken".to_string()]
            ),
            Some("ses_match".to_string())
        );
    }

    #[test]
    fn metadata_follows_prompt_running_tool_completion_and_title() {
        let (path, conn) = fixture("metadata");
        let session = "ses_flow";
        insert_session(
            &conn,
            session,
            None,
            "/work/app",
            "New session - 2026-07-20T10:00:00.000Z",
            100_000,
            105_000,
        );
        insert_message(
            &conn,
            "msg_user",
            session,
            101_000,
            json!({"role":"user","time":{"created":101_000}}),
        );
        insert_part(
            &conn,
            "part_prompt",
            "msg_user",
            session,
            json!({"type":"text","text":"  Build the thing\nwith detail"}),
        );

        let mut store = OpenCodeStore {
            database: path,
            cache: HashMap::new(),
        };
        let known = [(session.to_string(), PathBuf::from("/work/app"), None)];
        store.refresh(&known).unwrap();
        let meta = store.meta(session).unwrap();
        assert!(meta.has_content);
        assert_eq!(meta.display_title(), Some("Build the thing"));
        assert_eq!(meta.turn_state, TurnState::Mid);
        assert_eq!(meta.turn_started_at, Some(101));
        assert_eq!(meta.turn_completed_at, None);

        insert_message(
            &conn,
            "msg_assistant",
            session,
            102_000,
            json!({
                "role":"assistant",
                "parentID":"msg_user",
                "time":{"created":102_000,"completed":104_000},
                "tokens":{
                    "input":1000,"output":200,"reasoning":50,
                    "cache":{"read":300,"write":25}
                }
            }),
        );
        insert_part(
            &conn,
            "part_tool",
            "msg_assistant",
            session,
            json!({"type":"tool","state":{"status":"running"}}),
        );
        store.refresh(&known).unwrap();
        let meta = store.meta(session).unwrap();
        assert_eq!(meta.turn_state, TurnState::Mid);

        conn.execute(
            "UPDATE part SET data = ?1 WHERE id = 'part_tool'",
            [json!({"type":"tool","state":{"status":"completed"}}).to_string()],
        )
        .unwrap();
        conn.execute(
            "UPDATE session SET title = 'Generated title', time_updated = 106000
             WHERE id = ?1",
            [session],
        )
        .unwrap();
        store.refresh(&known).unwrap();
        let meta = store.meta(session).unwrap();
        assert_eq!(meta.display_title(), Some("Generated title"));
        assert_eq!(meta.turn_state, TurnState::Complete);
        assert_eq!(meta.turn_started_at, Some(101));
        assert_eq!(meta.turn_completed_at, Some(104));
    }

    #[test]
    fn empty_session_is_known_but_has_no_content() {
        let (path, conn) = fixture("empty");
        insert_session(
            &conn,
            "ses_empty",
            None,
            "/work/app",
            "New session - 2026-07-20T10:00:00.000Z",
            100_000,
            100_000,
        );
        drop(conn);
        let mut store = OpenCodeStore {
            database: path,
            cache: HashMap::new(),
        };
        store
            .refresh(&[("ses_empty".to_string(), PathBuf::from("/work/app"), None)])
            .unwrap();
        let meta = store.meta("ses_empty").unwrap();
        assert!(!meta.has_content);
        assert_eq!(meta.turn_state, TurnState::Unknown);
        assert_eq!(meta.display_title(), None);
    }

    #[test]
    fn missing_or_incompatible_database_keeps_last_good_snapshot() {
        let missing = std::env::temp_dir().join(format!(
            "corc-opencode-missing-{}-never-created.db",
            std::process::id()
        ));
        let mut absent = OpenCodeStore {
            database: missing,
            cache: HashMap::new(),
        };
        absent
            .refresh(&[("ses_missing".to_string(), PathBuf::from("/work/app"), None)])
            .unwrap();
        assert!(absent.meta("ses_missing").is_none());

        let (path, conn) = fixture("schema-drift");
        insert_session(
            &conn,
            "ses_cached",
            None,
            "/work/app",
            "Stable title",
            100_000,
            100_000,
        );
        insert_message(
            &conn,
            "msg_user",
            "ses_cached",
            100_000,
            json!({"role":"user","time":{"created":100_000}}),
        );
        insert_part(
            &conn,
            "part_prompt",
            "msg_user",
            "ses_cached",
            json!({"type":"text","text":"hello"}),
        );
        let known = [("ses_cached".to_string(), PathBuf::from("/work/app"), None)];
        let mut store = OpenCodeStore {
            database: path,
            cache: HashMap::new(),
        };
        store.refresh(&known).unwrap();
        assert_eq!(
            store.meta("ses_cached").unwrap().display_title(),
            Some("Stable title")
        );

        conn.execute_batch("DROP TABLE part").unwrap();
        store.refresh(&known).unwrap();
        assert_eq!(
            store.meta("ses_cached").unwrap().display_title(),
            Some("Stable title")
        );
    }
}
