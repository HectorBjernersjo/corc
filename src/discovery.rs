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

#[derive(Debug, Clone)]
pub struct Conversation {
    pub session_id: String,
    pub jsonl_path: PathBuf,
    pub cwd: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub title: Option<String>,
    pub last_prompt: Option<String>,
    pub first_prompt: Option<String>,
    pub turn_state: TurnState,
    pub mtime: SystemTime,
}

impl Conversation {
    pub fn display_title(&self) -> &str {
        self.title
            .as_deref()
            .or(self.last_prompt.as_deref())
            .or(self.first_prompt.as_deref())
            .unwrap_or("(untitled)")
    }

    /// Project directory: the recorded cwd, falling back to the escaped
    /// storage directory name when the session has no messages yet.
    pub fn project_dir(&self) -> String {
        match &self.cwd {
            Some(p) => p.display().to_string(),
            None => self
                .jsonl_path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

struct FileState {
    offset: u64,
    size: u64,
    mtime: SystemTime,
    conv: Conversation,
}

/// Incrementally parsed view of ~/.claude/projects.
pub struct Store {
    root: PathBuf,
    files: HashMap<PathBuf, FileState>,
}

impl Store {
    pub fn new() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(Self {
            root: PathBuf::from(home).join(".claude/projects"),
            files: HashMap::new(),
        })
    }

    /// Rescan the projects tree, parsing only new bytes of changed files.
    pub fn refresh(&mut self) -> Result<()> {
        let mut seen = Vec::new();
        for project in fs::read_dir(&self.root)?.flatten() {
            if !project.path().is_dir() {
                continue;
            }
            let Ok(entries) = fs::read_dir(project.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(session_id) = session_id_of(&path) else {
                    continue;
                };
                let Ok(meta) = entry.metadata() else { continue };
                let size = meta.len();
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                seen.push(path.clone());

                match self.files.get_mut(&path) {
                    Some(state) if state.size == size && state.mtime == mtime => {}
                    Some(state) if size >= state.offset => {
                        state.offset = parse_from(&path, state.offset, &mut state.conv)?;
                        state.size = size;
                        state.mtime = mtime;
                        state.conv.mtime = mtime;
                    }
                    _ => {
                        // New file, or it shrank (rewritten) — parse from scratch.
                        let mut conv = Conversation {
                            session_id,
                            jsonl_path: path.clone(),
                            cwd: None,
                            git_branch: None,
                            title: None,
                            last_prompt: None,
                            first_prompt: None,
                            turn_state: TurnState::Unknown,
                            mtime,
                        };
                        let offset = parse_from(&path, 0, &mut conv)?;
                        self.files.insert(path.clone(), FileState { offset, size, mtime, conv });
                    }
                }
            }
        }
        self.files.retain(|path, _| seen.contains(path));
        Ok(())
    }

    pub fn conversations(&self) -> Vec<&Conversation> {
        let mut convs: Vec<&Conversation> = self.files.values().map(|s| &s.conv).collect();
        convs.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        convs
    }
}

/// Session jsonl files are named <uuid>.jsonl; everything else (agent
/// transcripts etc.) is skipped.
fn session_id_of(path: &Path) -> Option<String> {
    if path.extension()?.to_str()? != "jsonl" {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let bytes = stem.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    for (i, b) in bytes.iter().enumerate() {
        let ok = match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        };
        if !ok {
            return None;
        }
    }
    Some(stem.to_string())
}

/// Parse complete lines starting at `offset`; returns the offset just past
/// the last complete line, so a partially written trailing line is retried
/// on the next poll.
fn parse_from(path: &Path, offset: u64, conv: &mut Conversation) -> Result<u64> {
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
            apply(conv, &v);
        }
    }
    Ok(pos)
}

fn apply(conv: &mut Conversation, v: &Value) {
    let sidechain = v["isSidechain"].as_bool().unwrap_or(false);
    match v["type"].as_str() {
        Some("user") if !sidechain => {
            if let Some(cwd) = v["cwd"].as_str() {
                conv.cwd = Some(PathBuf::from(cwd));
            }
            if let Some(branch) = v["gitBranch"].as_str() {
                if !branch.is_empty() && branch != "HEAD" {
                    conv.git_branch = Some(branch.to_string());
                }
            }
            if conv.first_prompt.is_none() {
                if let Some(text) = v["message"]["content"].as_str() {
                    if !text.starts_with('<') {
                        conv.first_prompt = Some(clean_prompt(text));
                    }
                }
            }
            conv.turn_state = TurnState::Mid;
        }
        Some("assistant") if !sidechain => {
            conv.turn_state = match v["message"]["stop_reason"].as_str() {
                Some("end_turn") | Some("stop_sequence") | Some("max_tokens") => {
                    TurnState::Complete
                }
                _ => TurnState::Mid,
            };
        }
        Some("system") if v["subtype"].as_str() == Some("turn_duration") => {
            conv.turn_state = TurnState::Complete;
        }
        Some("ai-title") => {
            if let Some(title) = v["aiTitle"].as_str() {
                conv.title = Some(title.to_string());
            }
        }
        Some("last-prompt") => {
            if let Some(prompt) = v["lastPrompt"].as_str() {
                conv.last_prompt = Some(clean_prompt(prompt));
            }
        }
        Some("summary") => {
            if conv.title.is_none() {
                if let Some(summary) = v["summary"].as_str() {
                    conv.title = Some(summary.to_string());
                }
            }
        }
        _ => {}
    }
}

/// Last human/assistant text messages of a session, for the preview pane.
/// Reads only the file tail, so long sessions may start mid-conversation.
pub fn tail_messages(path: &Path, max_msgs: usize) -> Vec<(&'static str, String)> {
    const TAIL: u64 = 512 * 1024;
    const MAX_TEXT: usize = 4000;
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL);
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    if reader.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }

    let mut messages: Vec<(&'static str, String)> = Vec::new();
    let mut line = Vec::new();
    let mut first = true;
    loop {
        line.clear();
        let Ok(n) = reader.read_until(b'\n', &mut line) else {
            break;
        };
        if n == 0 {
            break;
        }
        // The first chunk after seeking into the middle of the file is
        // usually a partial line.
        if std::mem::take(&mut first) && start > 0 {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if v["isSidechain"].as_bool().unwrap_or(false) {
            continue;
        }
        match v["type"].as_str() {
            Some("user") => {
                if let Some(text) = v["message"]["content"].as_str() {
                    if !text.starts_with('<') {
                        messages.push(("you", limit(text.trim(), MAX_TEXT)));
                    }
                }
            }
            Some("assistant") => {
                let Some(items) = v["message"]["content"].as_array() else {
                    continue;
                };
                let text: Vec<&str> = items
                    .iter()
                    .filter(|i| i["type"].as_str() == Some("text"))
                    .filter_map(|i| i["text"].as_str())
                    .collect();
                if !text.is_empty() {
                    messages.push(("claude", limit(text.join("\n").trim(), MAX_TEXT)));
                }
            }
            _ => {}
        }
        if messages.len() > max_msgs * 4 {
            messages.drain(..messages.len() - max_msgs);
        }
    }
    if messages.len() > max_msgs {
        messages.drain(..messages.len() - max_msgs);
    }
    messages
}

fn limit(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn clean_prompt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(120).collect()
}
