//! Metadata for known conversations, read from an append-only jsonl their
//! agent leaves behind: Claude's hook log and Codex's rollouts.
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
//!
//! The same incrementality carries across restarts (`cached_as`), and a
//! conversation the sidebar is currently hiding is not read at all
//! (`Known::visible`). Without those two, every corc start parsed every
//! transcript of every conversation it had ever spawned.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Where the last non-sidechain message left the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnState {
    /// A user prompt or tool call is in flight — Claude has work to do.
    Mid,
    /// The assistant ended its turn — the ball is on the user's side.
    Complete,
    Unknown,
}

/// What the jsonl tells us about a conversation.
///
/// Serializable because a `Store` persists what it parsed (see `cached_as`):
/// change a field's shape and last run's cache stops deserializing, which is
/// the point — it is thrown away and re-parsed rather than believed.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Claude Code has emitted an `AskUserQuestion` tool call that has not
    /// received its matching tool result yet. This is distinct from ordinary
    /// Mid-turn work: the agent is blocked on the user, so the conversation
    /// needs attention rather than a Running/Idle signal.
    pub active_question: bool,
    /// When the currently active question was asked, for its age column.
    pub question_asked_at: Option<u64>,
    /// mtime of the jsonl — coarse filesystem activity, including background
    /// writes that do not advance a turn.
    pub mtime: SystemTime,
    /// Where the conversation lives now (ADR-0003). Claude reports the
    /// session's own directory on every hook, and writes a `relocated` record
    /// into the transcript the moment a `/cd` runs, so a move shows up here
    /// from whichever source speaks first. None until the agent has reported
    /// one, or for providers that never do.
    pub cwd: Option<PathBuf>,
}

impl Meta {
    /// What the sidebar should show: an explicit rename, the generated title,
    /// or the first thing the user sent while no title has been generated yet.
    /// A conversation opened with a slash command and nothing else is named by
    /// that command, which is the first prompt as far as the agent is
    /// concerned.
    pub fn display_title(&self) -> Option<&str> {
        self.custom_title
            .as_deref()
            .or(self.title.as_deref())
            .or(self.first_prompt.as_deref())
    }
}

/// One conversation handed to a `MetaSource` on refresh.
#[derive(Debug, Clone)]
pub struct Known {
    pub id: String,
    pub cwd: PathBuf,
    /// Start of an in-flight turn as recorded in state.json, which carries an
    /// elapsed clock across a corc restart for providers that cannot recover
    /// it from their own store.
    pub turn_started_at: Option<u64>,
    /// Whether the sidebar could put this conversation on screen right now.
    /// A row the history window hides is not worth reading a transcript for,
    /// and reading them all is what made startup slow: parsing every
    /// conversation corc has ever spawned means hundreds of megabytes of jsonl
    /// on a list where a week's worth is a fraction of that. Metadata already
    /// held (this run or from the on-disk cache) keeps being updated whatever
    /// this says, since that only costs a `stat`.
    pub visible: bool,
}

impl Known {
    /// A conversation whose metadata is wanted whatever it costs, for callers
    /// that show everything (`corc list`) and for tests.
    pub fn shown(id: &str, cwd: impl Into<PathBuf>) -> Self {
        Self {
            id: id.to_string(),
            cwd: cwd.into(),
            turn_started_at: None,
            visible: true,
        }
    }
}

/// A per-provider source of conversation metadata (title, turn state, activity
/// time). Each provider ships its own implementation — Claude parses jsonl
/// transcripts (`Store`), Cursor reads its SQLite chat stores — so adding a
/// provider never touches the sidebar. `refresh` is handed only the
/// conversations belonging to that provider.
pub trait MetaSource: Send {
    fn refresh(&mut self, known: &[Known]) -> Result<()>;
    fn meta(&self, id: &str) -> Option<&Meta>;
    /// Persist what has been parsed so the next corc start reuses it instead
    /// of reading every transcript again. No-op for sources whose reads are
    /// cheap enough not to need one.
    fn save_cache(&mut self) {}
}

impl MetaSource for Store {
    fn refresh(&mut self, known: &[Known]) -> Result<()> {
        Store::refresh(self, known)
    }
    fn meta(&self, id: &str) -> Option<&Meta> {
        Store::meta(self, id)
    }
    fn save_cache(&mut self) {
        Store::save_cache(self);
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
            active_question: false,
            question_asked_at: None,
            mtime: SystemTime::UNIX_EPOCH,
            cwd: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileState {
    path: PathBuf,
    offset: u64,
    size: u64,
    mtime: SystemTime,
    meta: Meta,
}

/// The on-disk form of a `Store`, written to `meta-<name>.json` beside
/// state.json. `version` guards against a stale cache being read back into a
/// `Meta` whose fields still deserialize but no longer mean the same thing;
/// bump it when that happens. A shape change needs no bump, since serde
/// rejects the old file on its own.
#[derive(Serialize, Deserialize)]
struct Cache {
    version: u32,
    files: HashMap<String, FileState>,
}

const CACHE_VERSION: u32 = 2;

/// Incrementally parsed metadata for the conversations corc owns.
pub struct Store {
    root: PathBuf,
    files: HashMap<String, FileState>,
    /// Find the transcript of a conversation: (root, cwd, id) → path.
    locate: fn(&Path, &Path, &str) -> Option<PathBuf>,
    /// Fold one parsed jsonl line into the metadata.
    apply: fn(&mut Meta, &Value),
    /// Name of the on-disk cache, when this store keeps one. None for a store
    /// that touches no user files, which is what tests want.
    cache_name: Option<&'static str>,
    /// Whether anything has been parsed since the cache was last written.
    dirty: bool,
}

impl Store {
    /// Claude's transcripts under ~/.claude/projects, read for titles and
    /// for the `relocated` record a `/cd` leaves behind. Everything else about
    /// a Claude conversation arrives through its hooks; a `/rename`, a
    /// generated `ai-title` and a `/cd` that no hook has followed yet are
    /// written nowhere but here.
    pub fn titles() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(Self::titles_in(PathBuf::from(home).join(".claude/projects")).cached_as("claude-titles"))
    }

    /// The same reader over another transcript root, uncached, for tests.
    pub(crate) fn titles_in(root: PathBuf) -> Self {
        Self::with(root, locate_jsonl, apply_title)
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
            cache_name: None,
            dirty: false,
        }
    }

    /// Back this store with the on-disk cache called `name`, loading whatever
    /// the last run left there.
    ///
    /// Parsing a transcript from scratch is the one expensive thing a store
    /// does, and it used to happen on every corc start for every conversation.
    /// A cached entry is trusted only while the file it describes still has
    /// the size and mtime it had when parsed; a transcript that has grown
    /// since is picked up from its recorded offset, exactly as it is while
    /// corc runs. So the cache can be stale, or written by a second corc, or
    /// left behind by a crash, without ever being wrong.
    pub fn cached_as(mut self, name: &'static str) -> Self {
        self.cache_name = Some(name);
        self.files = load_cache(name);
        self
    }

    /// Write the cache, if this store keeps one and has parsed anything since
    /// the last write. Best-effort: a cache that cannot be written just costs
    /// the next start its parse.
    pub fn save_cache(&mut self) {
        let Some(name) = self.cache_name.filter(|_| self.dirty) else {
            return;
        };
        if write_cache(name, &self.files).is_ok() {
            self.dirty = false;
        }
    }

    /// Refresh metadata for the given conversations, parsing only new bytes of
    /// files that grew since the last call. A conversation nothing is known
    /// about yet and that the sidebar is hiding anyway is skipped entirely:
    /// its transcript is neither located nor read.
    pub fn refresh(&mut self, known: &[Known]) -> Result<()> {
        for Known {
            id, cwd, visible, ..
        } in known
        {
            let path = match self.files.get(id) {
                Some(state) => state.path.clone(),
                None if !visible => continue,
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
                    self.dirty = true;
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
                    self.dirty = true;
                }
            }
        }
        // Keyed on every conversation corc still owns, not on the visible
        // ones: a row that scrolls out of the history window keeps the
        // metadata it already has, in memory and in the cache.
        self.files.retain(|id, _| known.iter().any(|k| k.id == *id));
        Ok(())
    }

    pub fn meta(&self, id: &str) -> Option<&Meta> {
        self.files.get(id).map(|s| &s.meta)
    }

    /// The file this store located for a conversation. Where the file sits is
    /// evidence in its own right for a transcript tree keyed by the session's
    /// mangled directory (ADR-0003).
    pub fn path(&self, id: &str) -> Option<&Path> {
        self.files.get(id).map(|s| s.path.as_path())
    }
}

/// Path of the named cache. It sits beside state.json rather than in a cache
/// directory of its own: it is machine-local derived data with the same
/// lifetime as the conversations it describes.
fn cache_file(name: &str) -> Result<PathBuf> {
    Ok(crate::state::state_dir()?.join(format!("meta-{name}.json")))
}

/// The named cache, or an empty one when it is missing, unreadable, written by
/// an older corc, or no longer matches `Meta`. Every one of those means the
/// same thing: parse the transcripts again.
fn load_cache(name: &str) -> HashMap<String, FileState> {
    let cached = cache_file(name)
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Cache>(&text).ok())
        .filter(|cache| cache.version == CACHE_VERSION);
    cached.map(|cache| cache.files).unwrap_or_default()
}

/// Atomic write, so a corc reading the cache while another writes it sees one
/// version or the other and never half of one.
fn write_cache(name: &str, files: &HashMap<String, FileState>) -> Result<()> {
    let path = cache_file(name)?;
    let dir = path.parent().context("cache file has no parent dir")?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("meta-{name}.json.tmp"));
    let cache = Cache {
        version: CACHE_VERSION,
        files: files.clone(),
    };
    fs::write(&tmp, serde_json::to_string(&cache)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming into place {}", path.display()))?;
    Ok(())
}

/// Claude Code stores transcripts under a directory named after the cwd with
/// every non-alphanumeric character replaced by '-'. Try that first, then
/// fall back to a one-level scan of the project directories (naming scheme
/// insurance, not discovery — the uuid is already known).
fn locate_jsonl(root: &Path, cwd: &Path, id: &str) -> Option<PathBuf> {
    let candidate = root
        .join(mangled(&cwd.to_string_lossy()))
        .join(format!("{id}.jsonl"));
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

/// A path the way Claude Code names its per-project transcript directories:
/// every non-alphanumeric character replaced by '-'. Lossy one way, exact as
/// a check: a record cwd is confirmed by the transcript's location when its
/// mangled form equals the directory name the file sits in.
pub(crate) fn mangled(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
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

/// Fold one transcript line into the titles corc reads from Claude, plus the
/// one move it cannot learn any other way. Turn state, timing, questions and
/// the working directory otherwise arrive through the hook log
/// (`crate::hooks`), which reports the event itself rather than the trace it
/// left.
fn apply_title(meta: &mut Meta, v: &Value) {
    match v["type"].as_str() {
        // `/cd` fires no hook, so until the next prompt this record is the
        // only word of the move. Claude writes it just before it moves the
        // file, and the file's new location is what confirms it
        // (`provider::claude`) — taken alone it is as unvouched as any
        // reported cwd.
        Some("relocated") => {
            if let Some(cwd) = v["relocatedCwd"].as_str() {
                meta.cwd = Some(PathBuf::from(cwd));
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
        // `/rename`. Kept apart from the generated title so a later `ai-title`
        // can never overwrite what the user typed.
        Some("custom-title") => {
            if let Some(title) = v["customTitle"].as_str() {
                meta.custom_title = Some(title.to_string());
            }
        }
        _ => {}
    }
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

    #[test]
    fn a_rename_wins_over_every_generated_title() {
        let mut meta = Meta::default();

        // Claude's first stab at a title, before it has a better one.
        apply_title(&mut meta, &json!({"type":"summary","summary":"Reviewing an API"}));
        assert_eq!(meta.display_title(), Some("Reviewing an API"));
        apply_title(
            &mut meta,
            &json!({"type":"ai-title","aiTitle":"Generated architecture review"}),
        );
        assert_eq!(meta.display_title(), Some("Generated architecture review"));

        // `/rename` outranks it, and a later regenerated title cannot take it
        // back.
        apply_title(
            &mut meta,
            &json!({"type":"custom-title","customTitle":"platform api architecture"}),
        );
        apply_title(&mut meta, &json!({"type":"ai-title","aiTitle":"Something else"}));
        assert_eq!(meta.display_title(), Some("platform api architecture"));

        // The prompt the hooks recorded is only a stand-in until a real title
        // exists, never an override of one.
        let mut fresh = Meta {
            first_prompt: Some("fix the sidebar".to_string()),
            ..Meta::default()
        };
        assert_eq!(fresh.display_title(), Some("fix the sidebar"));
        apply_title(&mut fresh, &json!({"type":"ai-title","aiTitle":"Sidebar fixes"}));
        assert_eq!(fresh.display_title(), Some("Sidebar fixes"));
    }

    /// The two things that keep startup off the transcripts: a conversation
    /// the sidebar is hiding is never read, and what has been read once is
    /// read back from the cache on the next start instead of parsed again.
    #[test]
    fn hidden_conversations_go_unread_and_parsed_ones_survive_a_restart() {
        let root = std::env::temp_dir().join("corc-test-meta-cache-store");
        let state = std::env::temp_dir().join("corc-test-meta-cache-state");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
        // The cache lands beside state.json, so point that at a temp dir.
        // SAFETY: this is the only test that touches the environment.
        unsafe { std::env::set_var("XDG_STATE_HOME", &state) };

        let title = |text: &str| json!({"type":"ai-title","aiTitle":text}).to_string();
        let id = "conv-cached";
        let cwd = "/work/app";
        let dir = root.join(mangled(cwd));
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join(format!("{id}.jsonl"));
        std::fs::write(&transcript, format!("{}\n", title("first"))).unwrap();

        let hidden = Known {
            visible: false,
            ..Known::shown(id, cwd)
        };
        let mut store =
            Store::with(root.clone(), locate_jsonl, apply_title).cached_as("test-cache");
        store.refresh(std::slice::from_ref(&hidden)).unwrap();
        assert!(
            store.meta(id).is_none(),
            "a hidden conversation must not be read at all"
        );

        // Shown, it is parsed — and stays up to date once hidden again, since
        // that only costs a stat.
        store.refresh(&[Known::shown(id, cwd)]).unwrap();
        assert_eq!(store.meta(id).unwrap().title.as_deref(), Some("first"));
        std::fs::write(
            &transcript,
            format!("{}\n{}\n", title("first"), title("a title")),
        )
        .unwrap();
        store.refresh(std::slice::from_ref(&hidden)).unwrap();
        assert_eq!(store.meta(id).unwrap().title.as_deref(), Some("a title"));
        store.save_cache();

        // A fresh store — the next corc start — knows the conversation before
        // it has read a single byte, and picks up from where the last one
        // stopped.
        let mut restarted =
            Store::with(root.clone(), locate_jsonl, apply_title).cached_as("test-cache");
        assert_eq!(
            restarted.meta(id).unwrap().title.as_deref(),
            Some("a title"),
            "the cache must survive the restart"
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .map(|mut f| {
                use std::io::Write;
                writeln!(f, "{}", json!({"type":"custom-title","customTitle":"renamed"})).unwrap();
            })
            .unwrap();
        restarted.refresh(&[Known::shown(id, cwd)]).unwrap();
        let meta = restarted.meta(id).unwrap();
        assert_eq!(meta.title.as_deref(), Some("a title"));
        assert_eq!(meta.display_title(), Some("renamed"));

        // A conversation corc no longer owns leaves the cache with it.
        restarted.refresh(&[]).unwrap();
        assert!(restarted.meta(id).is_none());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

}
