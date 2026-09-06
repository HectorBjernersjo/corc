//! OpenCode CLI (`opencode`). The TUI creates its session only when the
//! first prompt is submitted and does not accept a caller-chosen id. Fresh
//! conversations therefore start under a corc-minted provisional id; once
//! OpenCode writes the matching top-level row to its SQLite database, corc
//! adopts the real `ses_...` id. Dead conversations resume with
//! `opencode --session <id>`.
//!
//! OpenCode keeps sessions and messages in
//! `$XDG_DATA_HOME/opencode/opencode.db` (normally
//! `~/.local/share/opencode/opencode.db`), in the `session_v2` and
//! `session_message` tables. The older `session`/`message`/`part` tables are
//! left behind by the migration and stop being written, so reading them shows
//! nothing but pre-migration history. The metadata reader opens the database
//! read-only. A missing database or transient/schema read failure simply
//! leaves the last good sidebar snapshot in place.

use super::Provider;
use crate::discovery::{self, Known, Meta, MetaSource, TurnState};
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
             FROM session_v2
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
    fn refresh(&mut self, known: &[Known]) -> Result<()> {
        let Some(conn) = open_read_only(&self.database) else {
            return Ok(());
        };

        for Known {
            id,
            turn_started_at: persisted_start,
            visible,
            ..
        } in known
        {
            if id.starts_with(PENDING_PREFIX) {
                continue;
            }
            // A session nothing is known about yet and that the sidebar is
            // hiding is left unread until the history window reaches it.
            if !visible && !self.cache.contains_key(id) {
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
        self.cache.retain(|id, _| known.iter().any(|k| k.id == *id));
        Ok(())
    }

    fn meta(&self, id: &str) -> Option<&Meta> {
        self.cache.get(id)
    }
}

struct SessionRow {
    /// Nullable since `session_v2`: a session OpenCode has not named yet has
    /// either no title at all or its `New session - <timestamp>` placeholder.
    title: Option<String>,
    updated_ms: i64,
    /// When OpenCode last parked the session at the prompt. It is the end of
    /// the last turn, and it stays put while the next one runs.
    idle_ms: Option<i64>,
}

fn read_meta(conn: &Connection, id: &str, persisted_start: Option<u64>) -> Result<Option<Meta>> {
    let session = conn
        .query_row(
            "SELECT title, time_updated, time_idle
             FROM session_v2
             WHERE id = ?1 AND parent_id IS NULL",
            [id],
            |row| {
                Ok(SessionRow {
                    title: row.get(0)?,
                    updated_ms: row.get(1)?,
                    idle_ms: row.get(2)?,
                })
            },
        )
        .optional()?;
    let Some(session) = session else {
        return Ok(None);
    };

    // Every message row is rewritten as its turn streams, so the newest
    // `time_updated` is genuine progress. The session row is not: OpenCode
    // touches it for the title and the token counters, and leaves it alone
    // for minutes of tool calls.
    let progress_ms: Option<i64> = conn.query_row(
        "SELECT max(time_updated) FROM session_message WHERE session_id = ?1",
        [id],
        |row| row.get(0),
    )?;
    let latest_user_ms = latest_prompt_ms(conn, id)?;

    let mut meta = Meta {
        has_content: latest_user_ms.is_some(),
        first_prompt: first_prompt(conn, id)?,
        title: session.title.filter(|t| !t.starts_with(DEFAULT_TITLE_PREFIX)),
        turn_progress_at: progress_ms.and_then(nonnegative).map(millis_to_secs),
        mtime: millis_to_system_time(progress_ms.unwrap_or_default().max(session.updated_ms)),
        ..Meta::default()
    };

    if let Some(user_ms) = latest_user_ms {
        meta.turn_started_at = nonnegative(user_ms).map(millis_to_secs).or(persisted_start);
        // OpenCode writes an `idle` message and stamps `time_idle` when a turn
        // ends, whatever ended it — a finished answer, an error, an escape.
        // An idle older than the last prompt belongs to the previous turn.
        match session.idle_ms.filter(|idle| *idle > user_ms) {
            Some(idle) => {
                meta.turn_state = TurnState::Complete;
                meta.turn_completed_at = nonnegative(idle).map(millis_to_secs);
            }
            // Sessions that predate `session_v2` carry no idle stamp at all,
            // so they read as mid-turn until the staleness guard in `status`
            // settles them, or until their next turn ends.
            None => meta.turn_state = TurnState::Mid,
        }
    }

    Ok(Some(meta))
}

/// The first thing the user typed, as a stand-in until OpenCode generates a
/// title. `synthetic` messages (injected AGENTS.md instructions and the like)
/// carry their own type and never get in the way.
fn first_prompt(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    let raw = conn
        .query_row(
            "SELECT data
             FROM session_message
             WHERE session_id = ?1 AND type = 'user'
             ORDER BY seq ASC
             LIMIT 1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(raw
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|data| data.get("text")?.as_str().and_then(discovery::title_line)))
}

/// When the current turn started: the newest real prompt in the session.
fn latest_prompt_ms(conn: &Connection, session_id: &str) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT time_created
         FROM session_message
         WHERE session_id = ?1 AND type = 'user'
         ORDER BY seq DESC
         LIMIT 1",
        [session_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
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
    use crate::discovery::{Known, MetaSource};
    use rusqlite::{Connection, params};
    use serde_json::json;

    /// The columns of OpenCode's live schema that corc reads, with defaults
    /// standing in for the ones it does not.
    fn fixture(name: &str) -> (PathBuf, Connection) {
        let root =
            std::env::temp_dir().join(format!("corc-opencode-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("opencode.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL DEFAULT 'global',
                parent_id TEXT,
                slug TEXT NOT NULL DEFAULT 'slug',
                directory TEXT NOT NULL,
                title TEXT,
                version TEXT NOT NULL DEFAULT '0.0.0',
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                time_idle INTEGER,
                idle_outcome TEXT
             );
             CREATE TABLE session_message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                type TEXT NOT NULL,
                seq INTEGER NOT NULL,
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
        title: Option<&str>,
        created: i64,
        updated: i64,
    ) {
        conn.execute(
            "INSERT INTO session_v2
             (id, parent_id, directory, title, time_created, time_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, parent, directory, title, created, updated],
        )
        .unwrap();
    }

    fn insert_message(
        conn: &Connection,
        id: &str,
        session: &str,
        kind: &str,
        seq: i64,
        time: i64,
        data: Value,
    ) {
        conn.execute(
            "INSERT INTO session_message
             (id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            params![id, session, kind, seq, time, data.to_string()],
        )
        .unwrap();
    }

    /// The turn ends the way OpenCode ends it: an `idle` message, and the
    /// stamp it leaves on the session row.
    fn go_idle(conn: &Connection, session: &str, seq: i64, time: i64) {
        insert_message(
            conn,
            &format!("msg_idle_{seq}"),
            session,
            "idle",
            seq,
            time,
            json!({"time":{"created":time},"outcome":"succeeded"}),
        );
        conn.execute(
            "UPDATE session_v2 SET time_idle = ?2 WHERE id = ?1",
            params![session, time],
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
        insert_session(
            &conn,
            "ses_old",
            None,
            "/work/app",
            Some("old"),
            90_000,
            90_000,
        );
        insert_session(
            &conn,
            "ses_wrong_cwd",
            None,
            "/work/other",
            Some("wrong"),
            100_000,
            100_000,
        );
        insert_session(
            &conn,
            "ses_child",
            Some("ses_parent"),
            "/work/app",
            Some("child"),
            100_100,
            100_100,
        );
        insert_session(
            &conn,
            "ses_taken",
            None,
            "/work/app",
            Some("taken"),
            100_200,
            100_200,
        );
        insert_session(
            &conn,
            "ses_match",
            None,
            "/work/app",
            None,
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
    fn metadata_follows_prompt_turn_end_and_title() {
        let (path, conn) = fixture("metadata");
        let session = "ses_flow";
        insert_session(
            &conn,
            session,
            None,
            "/work/app",
            Some("New session - 2026-09-18T10:00:00.000Z"),
            100_000,
            100_000,
        );
        // OpenCode prepends the agent instructions it injected; they are their
        // own message type and must not become the title.
        insert_message(
            &conn,
            "msg_agents",
            session,
            "synthetic",
            1,
            100_500,
            json!({"time":{"created":100_500},"text":"Instructions from: /work/app/AGENTS.md"}),
        );
        insert_message(
            &conn,
            "msg_user",
            session,
            "user",
            2,
            101_000,
            json!({"time":{"created":101_000},"text":"  Build the thing\nwith detail"}),
        );

        let mut store = OpenCodeStore {
            database: path,
            cache: HashMap::new(),
        };
        let known = [Known::shown(session, "/work/app")];
        store.refresh(&known).unwrap();
        let meta = store.meta(session).unwrap();
        assert!(meta.has_content);
        // The placeholder title never wins over the prompt.
        assert_eq!(meta.display_title(), Some("Build the thing"));
        assert_eq!(meta.turn_state, TurnState::Mid);
        assert_eq!(meta.turn_started_at, Some(101));
        assert_eq!(meta.turn_completed_at, None);

        // Tool calls live inside the assistant message, which keeps being
        // rewritten while they run: progress, but not the end of the turn.
        insert_message(
            &conn,
            "msg_assistant",
            session,
            "assistant",
            3,
            102_000,
            json!({
                "time":{"created":102_000,"streamed":102_500},
                "agent":"build",
                "content":[{"type":"tool","name":"bash","state":{"status":"running"}}]
            }),
        );
        store.refresh(&known).unwrap();
        let meta = store.meta(session).unwrap();
        assert_eq!(meta.turn_state, TurnState::Mid);
        assert_eq!(meta.turn_progress_at, Some(102));

        go_idle(&conn, session, 4, 104_000);
        conn.execute(
            "UPDATE session_v2 SET title = 'Generated title', time_updated = 106000
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

        // The next prompt reopens the turn; the old idle stamp stays behind.
        insert_message(
            &conn,
            "msg_user_2",
            session,
            "user",
            5,
            108_000,
            json!({"time":{"created":108_000},"text":"and now the other thing"}),
        );
        store.refresh(&known).unwrap();
        let meta = store.meta(session).unwrap();
        assert_eq!(meta.turn_state, TurnState::Mid);
        assert_eq!(meta.turn_started_at, Some(108));
        assert_eq!(meta.turn_completed_at, None);
    }

    #[test]
    fn empty_session_is_known_but_has_no_content() {
        let (path, conn) = fixture("empty");
        insert_session(&conn, "ses_empty", None, "/work/app", None, 100_000, 100_000);
        drop(conn);
        let mut store = OpenCodeStore {
            database: path,
            cache: HashMap::new(),
        };
        store
            .refresh(&[Known::shown("ses_empty", "/work/app")])
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
            .refresh(&[Known::shown("ses_missing", "/work/app")])
            .unwrap();
        assert!(absent.meta("ses_missing").is_none());

        let (path, conn) = fixture("schema-drift");
        insert_session(
            &conn,
            "ses_cached",
            None,
            "/work/app",
            Some("Stable title"),
            100_000,
            100_000,
        );
        insert_message(
            &conn,
            "msg_user",
            "ses_cached",
            "user",
            1,
            100_000,
            json!({"time":{"created":100_000},"text":"hello"}),
        );
        let known = [Known::shown("ses_cached", "/work/app")];
        let mut store = OpenCodeStore {
            database: path,
            cache: HashMap::new(),
        };
        store.refresh(&known).unwrap();
        assert_eq!(
            store.meta("ses_cached").unwrap().display_title(),
            Some("Stable title")
        );

        conn.execute_batch("DROP TABLE session_message").unwrap();
        store.refresh(&known).unwrap();
        assert_eq!(
            store.meta("ses_cached").unwrap().display_title(),
            Some("Stable title")
        );
    }
}
