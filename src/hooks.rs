//! Claude Code's hook feed, corc's source of truth for what a Claude
//! conversation is doing right now (ADR-0004).
//!
//! Claude runs a command of corc's choosing at fixed points in its own loop.
//! corc hands it one with `--settings` on the spawn line, so nothing in the
//! user's `~/.claude` is written to and their own hooks keep running
//! alongside. `corc __hook` is that command: it reads one JSON payload on stdin
//! and appends a line to the conversation's log under
//! `<state_dir>/hooks/<session-id>.jsonl`. The TUI folds those lines into
//! `Meta` with the same incremental reader it uses for transcripts.
//!
//! What this replaced: reading the pane. corc used to decide between working,
//! idle and blocked-on-a-question by matching Claude's spinner glyphs and the
//! `Enter to select · … · Esc to cancel` dialog footer against captured pane
//! text. Both are UI strings with no compatibility promise, and both had
//! already broken once. A hook fires on the event itself.
//!
//! The log is append-only and every writer opens it `O_APPEND`, so Claude's
//! parallel tool calls cannot lose each other's lines and corc never has to
//! lock anything.

use crate::discovery::{Meta, Store, TurnState, mangled, title_line};
use crate::state;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// One line of a conversation's hook log. Deliberately small: the log gains a
/// line per prompt, per tool call and per finished turn, and it lives as long
/// as the conversation does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum Event {
    /// The agent started, or was resumed, in this directory.
    Start {
        t: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<PathBuf>,
    },
    /// The user sent a prompt, which starts a turn (D7).
    Prompt {
        t: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<PathBuf>,
        text: Option<String>,
    },
    /// A tool call finished, so the turn is still genuinely moving. This is
    /// the only high-frequency event, and the only reason a long turn is not
    /// mistaken for a stalled one.
    Tool { t: u64 },
    /// An AskUserQuestion dialog opened: the agent is blocked on the user.
    Ask { t: u64 },
    /// ...and got its answer.
    Answer { t: u64 },
    /// The agent ended its turn.
    Stop {
        t: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<PathBuf>,
    },
}

/// Fold one logged event into a conversation's metadata, for `Store`.
pub fn apply(meta: &mut Meta, v: &Value) {
    let Ok(event) = serde_json::from_value::<Event>(v.clone()) else {
        return;
    };
    match event {
        Event::Start { dir, .. } => meta.cwd = dir.or(meta.cwd.take()),
        Event::Prompt { t, dir, text } => {
            meta.cwd = dir.or(meta.cwd.take());
            meta.has_content = true;
            meta.turn_state = TurnState::Mid;
            meta.turn_started_at = Some(t);
            meta.turn_completed_at = None;
            meta.turn_progress_at = Some(t);
            // Esc on an open dialog cancels the tool without a `PostToolUse`,
            // so a question can outlive the turn it was asked in. The next
            // prompt is proof the user has moved on.
            meta.active_question = false;
            meta.question_asked_at = None;
            if meta.first_prompt.is_none() {
                meta.first_prompt = text;
            }
        }
        Event::Tool { t } => meta.turn_progress_at = Some(t),
        Event::Ask { t } => {
            meta.active_question = true;
            meta.question_asked_at = Some(t);
            meta.turn_progress_at = Some(t);
        }
        Event::Answer { t } => {
            meta.active_question = false;
            meta.question_asked_at = None;
            meta.turn_progress_at = Some(t);
        }
        Event::Stop { t, dir } => {
            meta.cwd = dir.or(meta.cwd.take());
            meta.has_content = true;
            meta.turn_state = TurnState::Complete;
            meta.turn_completed_at = Some(t);
            meta.turn_progress_at = Some(t);
            // A turn cannot end with its question still open; Claude answers
            // it or the turn is gone.
            meta.active_question = false;
            meta.question_asked_at = None;
        }
    }
}

/// `corc __hook`: read one payload from Claude and append what it means.
///
/// Never fails loudly. This runs inside the user's agent loop, where a
/// non-zero exit or a message on stderr is Claude's problem, not corc's — a
/// dropped event only costs the sidebar one refresh's worth of accuracy.
pub fn ingest() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let payload: Value = serde_json::from_str(&raw)?;
    let Some(session) = payload["session_id"].as_str().filter(|s| is_session_id(s)) else {
        return Ok(());
    };
    let Some(event) = translate(&payload, state::unix_now()) else {
        return Ok(());
    };
    if payload["hook_event_name"] == "SessionStart" {
        let _ = crate::resume::report_claude(session);
    }
    append(session, &event)
}

/// Map a hook payload to the event corc keeps, or nothing for the hooks it
/// subscribes to only for their side effects. The timestamp is taken here:
/// payloads carry no clock of their own, and the hook runs the moment the
/// event happens.
fn translate(payload: &Value, now: u64) -> Option<Event> {
    let dir = || session_dir(payload);
    let tool = payload["tool_name"].as_str().unwrap_or_default();
    match payload["hook_event_name"].as_str()? {
        "SessionStart" => Some(Event::Start { t: now, dir: dir() }),
        "UserPromptSubmit" => Some(Event::Prompt {
            t: now,
            dir: dir(),
            text: payload["prompt"].as_str().and_then(title_line),
        }),
        "PreToolUse" if tool == ASK_TOOL => Some(Event::Ask { t: now }),
        "PostToolUse" if tool == ASK_TOOL => Some(Event::Answer { t: now }),
        "PostToolUse" => Some(Event::Tool { t: now }),
        "Stop" => Some(Event::Stop { t: now, dir: dir() }),
        _ => None,
    }
}

/// Where the conversation lives, when the payload proves it (ADR-0003).
///
/// A payload's `cwd` is the *shell's*, and it follows every `cd` the agent
/// makes: a session that runs `cd platform/apps/auth-service/src` reports that
/// as its cwd until it comes back. Taking it at face value scattered
/// conversations across the sidebar into directories nobody had moved them to.
///
/// What actually moves a session is `/cd`, and what `/cd` uniquely moves is the
/// transcript file. So a cwd counts only when the transcript's own location
/// vouches for it: the directory that file sits in must be this cwd's mangled
/// form. The payload hands over both, so the check is local and exact — no
/// searching, and no cwd recorded at all when the agent is off wandering.
fn session_dir(payload: &Value) -> Option<PathBuf> {
    let cwd = payload["cwd"].as_str()?;
    let transcript = Path::new(payload["transcript_path"].as_str()?);
    let project = transcript.parent()?.file_name()?.to_str()?;
    (project == mangled(cwd)).then(|| PathBuf::from(cwd))
}

/// The tool Claude blocks the user on. Its `PreToolUse` is the only notice
/// corc gets that a question is open: Claude writes the question to the
/// transcript only once it has been answered.
const ASK_TOOL: &str = "AskUserQuestion";

/// A session id becomes a filename, so it may only be one. Claude's are
/// uuids; anything else is dropped rather than sanitized.
fn is_session_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn append(session: &str, event: &Event) -> Result<()> {
    let dir = dir()?;
    fs::create_dir_all(&dir)?;
    let mut line = serde_json::to_string(event)?;
    line.push('\n');
    // One O_APPEND write of a line this short is atomic, which is what lets
    // Claude's parallel tool calls append at the same time safely.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{session}.jsonl")))?
        .write_all(line.as_bytes())?;
    Ok(())
}

/// Push one payload through the whole ingest path and read back what it
/// produced, for `corc doctor`. Uses an id no session can have, so it never
/// collides with a real log, and cleans up after itself.
pub fn self_test() -> Result<()> {
    let session = "0";
    let path = dir()?.join(format!("{session}.jsonl"));
    let _ = fs::remove_file(&path);
    let payload = serde_json::json!({
        "session_id": session,
        "cwd": "/",
        "transcript_path": "/x/-/y.jsonl",
        "hook_event_name": "Stop",
    });
    let event = translate(&payload, 1).context("a Stop payload produced no event")?;
    append(session, &event)?;
    let written = fs::read_to_string(&path).context("the log was not written")?;
    let _ = fs::remove_file(&path);
    let read_back: Event =
        serde_json::from_str(written.trim()).context("the log line does not read back")?;
    if read_back != event {
        anyhow::bail!("the log line does not round-trip");
    }
    Ok(())
}

/// Drop a conversation's log. Called when corc forgets the conversation
/// itself — unlike the transcripts under ~/.claude, this file is corc's own
/// (D1 still holds).
pub fn forget(session: &str) {
    if let Ok(dir) = dir() {
        let _ = fs::remove_file(dir.join(format!("{session}.jsonl")));
    }
}

/// Where the logs live, beside state.json with the metadata caches.
fn dir() -> Result<PathBuf> {
    Ok(state::state_dir()?.join("hooks"))
}

/// The incremental reader over the logs, keyed by session id: no directory
/// mangling, no search, the path is the id.
pub fn store() -> Result<Store> {
    Ok(store_in(dir()?).cached_as("claude-hooks"))
}

/// The same reader over another log directory, uncached, for tests.
pub(crate) fn store_in(root: PathBuf) -> Store {
    Store::with(root, locate, apply)
}

fn locate(root: &Path, _cwd: &Path, id: &str) -> Option<PathBuf> {
    Some(root.join(format!("{id}.jsonl")))
}

/// The settings file corc passes to `claude --settings`, written on demand and
/// refreshed whenever this binary moves (a `cargo install` to a new path).
/// Settings merge, so the user's own hooks in ~/.claude keep running.
pub fn settings_file() -> Result<PathBuf> {
    let path = state::state_dir()?.join("claude-hooks.json");
    state::write_if_changed(&path, &settings_json(&crate::self_exe()))?;
    Ok(path)
}

/// Claude's hook config for the events corc reads. `PreToolUse` is narrowed to
/// the one tool that blocks the user; `PostToolUse` is not, because every tool
/// call is what proves a long turn is still moving.
fn settings_json(exe: &Path) -> String {
    let command = format!("{} __hook", shell_quote(&exe.to_string_lossy()));
    let entry = |matcher: Option<&str>| {
        let matcher = matcher
            .map(|m| format!("\"matcher\": {}, ", serde_json::to_string(m).unwrap()))
            .unwrap_or_default();
        format!(
            "[{{{matcher}\"hooks\": [{{\"type\": \"command\", \"command\": {}}}]}}]",
            serde_json::to_string(&command).unwrap()
        )
    };
    format!(
        "{{\n  \"hooks\": {{\n    \
         \"SessionStart\": {},\n    \
         \"UserPromptSubmit\": {},\n    \
         \"PreToolUse\": {},\n    \
         \"PostToolUse\": {},\n    \
         \"Stop\": {}\n  }}\n}}\n",
        entry(None),
        entry(None),
        entry(Some(ASK_TOOL)),
        entry(Some("*")),
        entry(None),
    )
}

/// Claude runs the hook through a shell, so a corc installed under a path with
/// spaces still has to survive being pasted into one.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{self, Status};

    fn payload(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    /// Where Claude keeps the transcript of a session living in `/work/corc`.
    const TRANSCRIPT: &str = "/home/h/.claude/projects/-work-corc/x.jsonl";

    /// The payloads are real, captured from Claude Code 2.1.251 through a
    /// `--settings` hook. Field names here are a compatibility surface with
    /// something corc does not own, so the test carries the whole shape.
    #[test]
    fn a_turn_is_read_from_start_to_finish() {
        let session = "99999999-2222-3333-4444-555555555555";
        let events = [
            payload(&format!(
                r#"{{"session_id":"{session}","transcript_path":"{TRANSCRIPT}",
                    "cwd":"/work/corc","hook_event_name":"SessionStart","source":"startup"}}"#
            )),
            payload(&format!(
                r#"{{"session_id":"{session}","transcript_path":"{TRANSCRIPT}",
                    "cwd":"/work/corc","permission_mode":"auto",
                    "hook_event_name":"UserPromptSubmit","prompt":"Fix the sidebar\nsecond line"}}"#
            )),
            payload(&format!(
                r#"{{"session_id":"x","transcript_path":"{TRANSCRIPT}","cwd":"/work/corc",
                   "hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{{}},
                   "tool_response":{{}}}}"#
            )),
            payload(&format!(
                r#"{{"session_id":"x","transcript_path":"{TRANSCRIPT}","cwd":"/work/corc",
                   "hook_event_name":"Stop","stop_hook_active":false,
                   "last_assistant_message":"done"}}"#
            )),
        ];

        let mut meta = Meta::default();
        let mut t = 1_000u64;
        for event in &events {
            let translated = translate(event, t).expect("every payload maps to an event");
            apply(&mut meta, &serde_json::to_value(&translated).unwrap());
            t += 100;
        }

        assert_eq!(meta.first_prompt.as_deref(), Some("Fix the sidebar"));
        assert_eq!(meta.cwd.as_deref(), Some(Path::new("/work/corc")));
        assert!(meta.has_content);
        assert_eq!(meta.turn_state, TurnState::Complete);
        assert_eq!(meta.turn_started_at, Some(1100));
        assert_eq!(meta.turn_completed_at, Some(1300));
        // The finished turn took two minutes less than the log is long.
        assert_eq!(
            status::time_column(Status::Unseen, Some(&meta), 0, 2000),
            "3m"
        );
    }

    /// An open question is an attention state the transcript cannot see at
    /// all, and it must clear the moment the user answers.
    #[test]
    fn a_question_opens_and_closes_on_its_tool_calls() {
        let ask = payload(
            r#"{"session_id":"x","cwd":"/w","hook_event_name":"PreToolUse",
               "tool_name":"AskUserQuestion","tool_input":{"questions":[]}}"#,
        );
        let answered = payload(
            r#"{"session_id":"x","cwd":"/w","hook_event_name":"PostToolUse",
               "tool_name":"AskUserQuestion","tool_response":{}}"#,
        );
        let mut meta = Meta {
            turn_state: TurnState::Mid,
            turn_started_at: Some(500),
            ..Meta::default()
        };

        apply(
            &mut meta,
            &serde_json::to_value(translate(&ask, 700).unwrap()).unwrap(),
        );
        assert!(meta.active_question);
        assert_eq!(meta.question_asked_at, Some(700));
        assert_eq!(
            status::derive(true, Some(&meta), 900, true, 1000, 0),
            Status::Question
        );

        apply(
            &mut meta,
            &serde_json::to_value(translate(&answered, 800).unwrap()).unwrap(),
        );
        assert!(!meta.active_question);
        // Back to ordinary work in flight.
        assert_eq!(
            status::derive(true, Some(&meta), 900, true, 1000, 0),
            Status::Running
        );
    }

    /// Tool calls are what keep a long turn from ageing out as stalled.
    #[test]
    fn tool_calls_keep_a_long_turn_running() {
        let tool =
            payload(r#"{"session_id":"x","hook_event_name":"PostToolUse","tool_name":"Read"}"#);
        let mut meta = Meta {
            turn_state: TurnState::Mid,
            turn_started_at: Some(0),
            turn_progress_at: Some(0),
            ..Meta::default()
        };
        // Two hours in, with nothing since the prompt: stalled.
        assert_eq!(
            status::derive(true, Some(&meta), 0, false, 7200, 0),
            Status::Idle
        );
        apply(
            &mut meta,
            &serde_json::to_value(translate(&tool, 7100).unwrap()).unwrap(),
        );
        assert_eq!(
            status::derive(true, Some(&meta), 0, false, 7200, 0),
            Status::Running
        );
    }

    /// A payload's cwd is the shell's and follows every `cd` the agent makes,
    /// so it only counts when the transcript's own location vouches for it
    /// (ADR-0003). Getting this wrong scattered conversations across the
    /// sidebar into directories nobody had moved them to.
    #[test]
    fn a_wandering_shell_never_moves_the_conversation() {
        let home = |cwd: &str, transcript: &str| {
            payload(&format!(
                r#"{{"session_id":"x","transcript_path":"{transcript}","cwd":"{cwd}",
                   "hook_event_name":"Stop"}}"#
            ))
        };
        let mut meta = Meta::default();

        // At home: the transcript sits in this cwd's mangled directory.
        apply(
            &mut meta,
            &serde_json::to_value(translate(&home("/work/corc", TRANSCRIPT), 1).unwrap()).unwrap(),
        );
        assert_eq!(meta.cwd.as_deref(), Some(Path::new("/work/corc")));

        // The agent has `cd`:d into a subdirectory. The transcript has not
        // moved, so neither does the conversation.
        let wandered = home("/work/corc/src/platform", TRANSCRIPT);
        assert_eq!(session_dir(&wandered), None);
        apply(
            &mut meta,
            &serde_json::to_value(translate(&wandered, 2).unwrap()).unwrap(),
        );
        assert_eq!(meta.cwd.as_deref(), Some(Path::new("/work/corc")));

        // `/cd` is the one thing that moves the transcript file, and that is
        // the one thing that relocates the conversation.
        let relocated = home(
            "/work/other",
            "/home/h/.claude/projects/-work-other/x.jsonl",
        );
        apply(
            &mut meta,
            &serde_json::to_value(translate(&relocated, 3).unwrap()).unwrap(),
        );
        assert_eq!(meta.cwd.as_deref(), Some(Path::new("/work/other")));
    }

    /// Hooks corc subscribes to but has nothing to record, and payloads from a
    /// future Claude, are dropped rather than guessed at.
    #[test]
    fn unknown_payloads_are_dropped() {
        assert_eq!(
            translate(&payload(r#"{"hook_event_name":"SessionEnd"}"#), 1),
            None
        );
        assert_eq!(
            translate(&payload(r#"{"hook_event_name":"WhateverNext"}"#), 1),
            None
        );
        assert_eq!(translate(&payload("{}"), 1), None);

        // A line the current corc does not understand leaves metadata alone.
        let mut meta = Meta::default();
        apply(&mut meta, &payload(r#"{"e":"something_new","t":5}"#));
        assert_eq!(meta.turn_state, TurnState::Unknown);
    }

    /// The id names a file, so anything that could escape the directory is
    /// refused outright.
    #[test]
    fn only_a_session_id_shaped_id_names_a_log() {
        assert!(is_session_id("047fca63-e9ad-4980-9d79-bec44f2b32d7"));
        assert!(!is_session_id("../../etc/passwd"));
        assert!(!is_session_id("a/b"));
        assert!(!is_session_id(""));
        assert!(!is_session_id(&"a".repeat(65)));
    }

    /// The generated config is what Claude parses, so it has to be valid JSON
    /// with the matchers on the right events, and survive an install path with
    /// a space in it.
    #[test]
    fn the_settings_file_is_valid_and_quotes_its_path() {
        let json = settings_json(Path::new("/home/a b/.local/bin/corc"));
        let parsed: Value = serde_json::from_str(&json).unwrap();
        let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(command, "'/home/a b/.local/bin/corc' __hook");
        assert_eq!(
            parsed["hooks"]["PreToolUse"][0]["matcher"],
            "AskUserQuestion"
        );
        assert_eq!(parsed["hooks"]["PostToolUse"][0]["matcher"], "*");
        assert!(parsed["hooks"]["UserPromptSubmit"][0]["matcher"].is_null());
    }
}
