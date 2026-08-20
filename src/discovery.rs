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
    /// The slash command a conversation was started with (`/impeccable teach`)
    /// and that reached the model, for conversations that have no real prompt
    /// at all. Ranked below `first_prompt` — the command is the weakest of the
    /// stand-ins, but it beats `(untitled)`.
    pub first_command: Option<String>,
    /// A just-seen command, held until we know whether it ran locally (a
    /// `<local-command-stdout>` record follows and withdraws it) or reached the
    /// model (an assistant record follows and promotes it to `first_command`).
    /// Only `display_title` should consume it.
    pub command_candidate: Option<String>,
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
    /// Where the conversation lives now: the latest record cwd that is
    /// *confirmed by the transcript file's own location* (ADR-0003). A
    /// record's cwd follows every Bash `cd` the agent makes, so on its own it
    /// says where the shell stood, not where the session lives — but `/cd` is
    /// the only thing that moves the transcript file, so a record cwd counts
    /// only when its mangled form names the directory the file sits in.
    /// None until a matching record exists, or for providers that never
    /// report one.
    pub cwd: Option<PathBuf>,
    /// Name of the directory the transcript file sits in (the mangled
    /// session cwd), set by `Store` before parsing — what record cwds are
    /// confirmed against.
    pub(crate) project_dir_name: Option<String>,
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
            .or(self.command_candidate.as_deref())
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
            first_command: None,
            command_candidate: None,
            has_content: false,
            turn_state: TurnState::Unknown,
            turn_started_at: None,
            turn_completed_at: None,
            turn_progress_at: None,
            active_question: false,
            question_asked_at: None,
            active_question_tool_id: None,
            mtime: SystemTime::UNIX_EPOCH,
            cwd: None,
            project_dir_name: None,
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

const CACHE_VERSION: u32 = 1;

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
                        // The directory the file sits in vouches for record
                        // cwds (ADR-0003); a relocated file gets re-inserted
                        // here, so this always names its current home.
                        project_dir_name: path
                            .parent()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().into_owned()),
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
fn mangled(path: &str) -> String {
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

fn apply(meta: &mut Meta, v: &Value) {
    let sidechain = v["isSidechain"].as_bool().unwrap_or(false);

    // Every record stamps the shell cwd it was written under — which follows
    // Bash `cd` into subdirectories, so it only counts as "the conversation
    // lives here" when the transcript file's location vouches for it: `/cd`
    // is the only thing that moves the file (ADR-0003). This also repairs a
    // Conversation.cwd that drifted: the latest *confirmed* cwd is the real
    // session directory, wherever the shell has wandered since.
    if let Some(cwd) = v["cwd"].as_str()
        && meta.project_dir_name.as_deref() == Some(mangled(cwd).as_str())
    {
        meta.cwd = Some(PathBuf::from(cwd));
    }

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
        // Slash-command transcripts. They are meta — excluded from
        // `first_prompt` by the arm above — but they are the only trace of what
        // a conversation is about until a real prompt or a generated title
        // shows up. Keep the command as a title candidate; a following
        // `<local-command-stdout>` means it ran locally and never reached the
        // model, so it withdraws the candidate.
        Some("user") if !sidechain && !v["isMeta"].as_bool().unwrap_or(false) => {
            if let Some(content) = v["message"]["content"].as_str() {
                if content.starts_with("<local-command-stdout>") {
                    meta.command_candidate = None;
                } else if meta.first_command.is_none() && meta.command_candidate.is_none() {
                    meta.command_candidate = slash_command(v);
                }
            }
        }
        Some("assistant") if !sidechain => {
            meta.has_content = true;
            // The model answered, so the pending command genuinely started
            // the conversation — pin it against later local commands whose
            // stdout would otherwise withdraw it.
            if meta.first_command.is_none() {
                meta.first_command = meta.command_candidate.take();
            }
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

    /// A conversation driven purely by slash commands has no free-text prompt
    /// and often never gets a generated title; the first command that reached
    /// the model stands in. Local commands (their `<local-command-stdout>`
    /// follows immediately) never name the conversation.
    #[test]
    fn command_only_conversation_falls_back_to_the_command_that_reached_the_model() {
        let mut meta = Meta::default();

        // /model runs locally: caveat, command, stdout.
        apply(
            &mut meta,
            &json!({"type":"user","isMeta":true,"message":{"content":"<local-command-caveat>…</local-command-caveat>"}}),
        );
        apply(
            &mut meta,
            &json!({"type":"user","message":{"content":
                "<command-name>/model</command-name>\n<command-args>fable</command-args>"}}),
        );
        assert_eq!(meta.display_title(), Some("/model fable"));
        apply(
            &mut meta,
            &json!({"type":"user","message":{"content":"<local-command-stdout>Set model</local-command-stdout>"}}),
        );
        assert_eq!(meta.display_title(), None);

        // /user-story goes to the model (tag order varies) and gets pinned by
        // the assistant reply.
        apply(
            &mut meta,
            &json!({"type":"user","message":{"content":
                "<command-message>user-story</command-message>\n<command-name>/user-story</command-name>"}}),
        );
        apply(
            &mut meta,
            &json!({"type":"assistant","message":{"stop_reason":"end_turn"},
                    "timestamp":"2026-07-08T10:00:10Z"}),
        );
        assert_eq!(meta.display_title(), Some("/user-story"));
        assert_eq!(meta.first_command.as_deref(), Some("/user-story"));

        // A later local command must not withdraw the pinned name…
        apply(
            &mut meta,
            &json!({"type":"user","message":{"content":"<local-command-stdout>usage</local-command-stdout>"}}),
        );
        assert_eq!(meta.display_title(), Some("/user-story"));

        // …and a real prompt still outranks it.
        apply(
            &mut meta,
            &json!({"type":"user","message":{"content":"actually, do this"},
                    "timestamp":"2026-07-08T10:01:00Z"}),
        );
        assert_eq!(meta.display_title(), Some("actually, do this"));
    }

    /// ADR-0003: a record's cwd follows every Bash `cd` the agent makes, so
    /// it only counts when the transcript file's location vouches for it —
    /// the mangled cwd must name the directory the file sits in. Anything
    /// else (a shell standing in a subdirectory) must not move the
    /// conversation.
    #[test]
    fn cwd_counts_only_when_the_files_location_vouches_for_it() {
        let mut meta = Meta {
            project_dir_name: Some(mangled("/work/HRM/benchmark")),
            ..Meta::default()
        };

        apply(
            &mut meta,
            &json!({"type":"user","cwd":"/work/HRM/benchmark",
                    "message":{"content":"start"},
                    "timestamp":"2026-07-08T10:00:00Z"}),
        );
        assert_eq!(meta.cwd.as_deref(), Some(Path::new("/work/HRM/benchmark")));

        // The agent cd:s into a subdirectory to build — records now stamp
        // the subdir, but the transcript file has not moved: not a
        // relocation. This was the bug that scattered benchmark
        // conversations across their .NET project folders.
        apply(
            &mut meta,
            &json!({"type":"assistant","cwd":"/work/HRM/benchmark/Flex.Net",
                    "message":{"stop_reason":"end_turn"},
                    "timestamp":"2026-07-08T10:01:00Z"}),
        );
        assert_eq!(meta.cwd.as_deref(), Some(Path::new("/work/HRM/benchmark")));

        // A record with no cwd (generated title) leaves it untouched.
        apply(&mut meta, &json!({"type":"ai-title","aiTitle":"a title"}));
        assert_eq!(meta.cwd.as_deref(), Some(Path::new("/work/HRM/benchmark")));
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

        let prompt = |text: &str, ts: &str| {
            json!({"type":"user","cwd":"/work/app","message":{"content":text},
                   "timestamp":ts})
            .to_string()
        };
        let id = "conv-cached";
        let cwd = "/work/app";
        let dir = root.join(mangled(cwd));
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join(format!("{id}.jsonl"));
        std::fs::write(
            &transcript,
            format!("{}\n", prompt("first", "2026-07-08T10:00:00Z")),
        )
        .unwrap();

        let hidden = Known {
            visible: false,
            ..Known::shown(id, cwd)
        };
        let mut store = Store::with(root.clone(), locate_jsonl, apply).cached_as("test-cache");
        store.refresh(std::slice::from_ref(&hidden)).unwrap();
        assert!(
            store.meta(id).is_none(),
            "a hidden conversation must not be read at all"
        );

        // Shown, it is parsed — and stays up to date once hidden again, since
        // that only costs a stat.
        store.refresh(&[Known::shown(id, cwd)]).unwrap();
        assert_eq!(
            store.meta(id).unwrap().first_prompt.as_deref(),
            Some("first")
        );
        std::fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                prompt("first", "2026-07-08T10:00:00Z"),
                json!({"type":"ai-title","aiTitle":"a title"}),
            ),
        )
        .unwrap();
        store.refresh(std::slice::from_ref(&hidden)).unwrap();
        assert_eq!(store.meta(id).unwrap().title.as_deref(), Some("a title"));
        store.save_cache();

        // A fresh store — the next corc start — knows the conversation before
        // it has read a single byte, and picks up from where the last one
        // stopped.
        let mut restarted = Store::with(root.clone(), locate_jsonl, apply).cached_as("test-cache");
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
                writeln!(f, "{}", prompt("second", "2026-07-08T10:05:00Z")).unwrap();
            })
            .unwrap();
        restarted.refresh(&[Known::shown(id, cwd)]).unwrap();
        let meta = restarted.meta(id).unwrap();
        assert_eq!(meta.title.as_deref(), Some("a title"));
        assert_eq!(meta.turn_started_at, parse_iso8601("2026-07-08T10:05:00Z"));

        // A conversation corc no longer owns leaves the cache with it.
        restarted.refresh(&[]).unwrap();
        assert!(restarted.meta(id).is_none());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The whole relocation story against real files: a Bash `cd` into a
    /// subdirectory never moves the conversation, a real `/cd` (the
    /// transcript file moves and new records match its new home) does, and a
    /// Conversation.cwd that drifted wrong self-repairs — the Store keeps
    /// reporting the confirmed directory whatever cwd it is refreshed with.
    #[test]
    fn relocation_follows_the_file_and_repairs_drift() {
        let root = std::env::temp_dir().join("corc-test-relocation-store");
        let _ = std::fs::remove_dir_all(&root);
        let record = |cwd: &str, ts: &str| {
            json!({"type":"user","cwd":cwd,"message":{"content":"x"},
                   "timestamp":ts})
            .to_string()
        };

        let id = "conv-1";
        let home = "/work/HRM/benchmark";
        let sub = "/work/HRM/benchmark/Flex.Net";
        let dir = root.join(mangled(home));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!(
                "{}\n{}\n",
                record(home, "2026-07-08T10:00:00Z"),
                record(sub, "2026-07-08T10:01:00Z"),
            ),
        )
        .unwrap();

        let mut store = Store::with(root.clone(), locate_jsonl, apply);
        let known = vec![Known::shown(id, home)];
        store.refresh(&known).unwrap();
        // The shell stood in the subdir, but the conversation lives at home.
        assert_eq!(
            store.meta(id).unwrap().cwd.as_deref(),
            Some(Path::new(home))
        );

        // Even asked with a drifted cwd (state damaged by the old bug), the
        // uuid fallback finds the file and the confirmed cwd repairs it.
        let mut drifted = Store::with(root.clone(), locate_jsonl, apply);
        drifted.refresh(&[Known::shown(id, sub)]).unwrap();
        assert_eq!(
            drifted.meta(id).unwrap().cwd.as_deref(),
            Some(Path::new(home))
        );

        // A real /cd: the file moves and new records stamp the new home.
        let new_home = "/work/HRM/feature-x";
        let new_dir = root.join(mangled(new_home));
        std::fs::create_dir_all(&new_dir).unwrap();
        let new_path = new_dir.join(format!("{id}.jsonl"));
        std::fs::rename(dir.join(format!("{id}.jsonl")), &new_path).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&new_path)
            .unwrap();
        use std::io::Write;
        writeln!(file, "{}", record(new_home, "2026-07-08T10:02:00Z")).unwrap();

        // First refresh notices the old path vanished, the next re-locates.
        store.refresh(&known).unwrap();
        store.refresh(&known).unwrap();
        assert_eq!(
            store.meta(id).unwrap().cwd.as_deref(),
            Some(Path::new(new_home))
        );

        let _ = std::fs::remove_dir_all(&root);
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
