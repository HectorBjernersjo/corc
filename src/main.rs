mod discovery;
mod picker;
mod projects;
mod provider;
mod state;
mod status;
mod tmux;
mod ui;
mod widget;

use anyhow::{Context, Result};
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => ui::run(),
        Some("open") => open(),
        Some("list") => list(),
        Some("projects") => projects::run(),
        Some("pick-dir") => pick_dir(&args),
        Some("add-dir") => add_dir(&args),
        Some("jump") => jump(&args),
        Some(other) => anyhow::bail!(
            "unknown command: {other} (expected: open, list, projects, pick-dir, add-dir, jump)"
        ),
    }
}

/// `corc pick-dir [--out FILE]` (D22): a centered picker over the merged
/// project directories, run inside a `tmux display-popup` by the sidebar's
/// `N`. The chosen directory is written to FILE (empty file when cancelled)
/// so the still-running TUI — the sole writer of state.json — can spawn there.
/// Without `--out` the choice is printed to stdout for manual use.
fn pick_dir(args: &[String]) -> Result<()> {
    let state = state::State::load()?;
    let items = picker::list_directories(&state.directories)?
        .iter()
        .map(|d| {
            let path = d.to_string_lossy().into_owned();
            widget::Choice::new(display_dir(&path), path)
        })
        .collect();
    let choice = widget::run_filter_picker("new conversation", items)?;
    emit_choice(args, choice.as_deref())
}

/// `corc add-dir [--out FILE]` (D22): a centered path prompt (Tab-completing
/// real subdirectories), run inside a `tmux display-popup` by the sidebar's
/// `p`. The typed directory is written to FILE; the TUI records it in the
/// machine-local list and spawns there.
fn add_dir(args: &[String]) -> Result<()> {
    let choice = widget::run_path_prompt("add directory")?;
    let value = choice.map(|p| p.to_string_lossy().into_owned());
    emit_choice(args, value.as_deref())
}

/// Deliver a popup picker's result: to the `--out` file when given (empty
/// when the user cancelled), otherwise to stdout.
fn emit_choice(args: &[String], value: Option<&str>) -> Result<()> {
    let out = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1));
    match out {
        Some(path) => std::fs::write(path, value.unwrap_or(""))
            .with_context(|| format!("writing {path}")),
        None => {
            if let Some(v) = value {
                println!("{v}");
            }
            Ok(())
        }
    }
}

/// `corc jump N` (D13): the digit jump reachable while focus is in the Claude
/// pane, where the sidebar TUI never sees the keystroke. A tmux binding scoped
/// to the `_corc` session runs this; it hops to window N of the project the
/// pane you are looking at belongs to (see `viewed_conversation`).
fn jump(args: &[String]) -> Result<()> {
    let n: u8 = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .filter(|n| (1..=9).contains(n))
        .context("usage: corc jump <1-9>")?;
    let state = state::State::load()?;
    let Some(conv) = viewed_conversation(&state) else {
        return Ok(()); // nothing spawned yet — nowhere to jump
    };
    tmux::jump_to_window(&conv.cwd, n)
}

/// The conversation whose Claude pane is currently swapped into corc's content
/// slot — the one the user is looking at. Its pane is the only conversation
/// pane living in the corc session; every other conversation's pane sits
/// parked in the hidden session. This is deterministic — exactly "the session
/// this Claude pane belongs to" — where a `last_viewed` guess could be stale
/// and point at whichever project the user last switched to. Falls back to the
/// most recently viewed when nothing is swapped in (placeholder showing).
fn viewed_conversation(state: &state::State) -> Option<&state::Conversation> {
    let corc_panes = tmux::session_pane_ids(tmux::TUI_SESSION);
    state
        .conversations
        .iter()
        .find(|c| {
            c.pane_id
                .as_deref()
                .is_some_and(|p| corc_panes.iter().any(|q| q == p))
        })
        .or_else(|| state.conversations.iter().max_by_key(|c| c.last_viewed))
}

/// `corc open` (D15): the Ctrl+q toggle. Already in the corc session ⇒ go back
/// to the session the client came from; anywhere else ⇒ make sure the visible
/// `corc` session exists with the TUI running and take the client there. Bound
/// to Ctrl+q in tmux.conf via run-shell.
fn open() -> Result<()> {
    let exe = std::env::current_exe().context("locating the corc binary")?;
    // switch-client only works from inside tmux; that covers both a shell in
    // a pane (TMUX set) and the Ctrl+q run-shell binding (TMUX_PANE set).
    let in_tmux =
        std::env::var_os("TMUX").is_some() || std::env::var_os("TMUX_PANE").is_some();
    // Toggle: already viewing corc ⇒ go to the viewed conversation's project
    // session, landing on its last-active window (like Alt+N, but without a
    // fixed window number). Nothing viewed ⇒ fall back to the previous session.
    if in_tmux && tmux::current_session().ok().as_deref() == Some(tmux::TUI_SESSION) {
        let state = state::State::load()?;
        return match viewed_conversation(&state) {
            Some(conv) => tmux::jump_to_session(&conv.cwd),
            None => tmux::switch_to_last(),
        };
    }
    tmux::ensure_tui_session(&exe.to_string_lossy())?;
    // From a plain terminal, attach instead of switching a client.
    if in_tmux {
        tmux::switch_client(tmux::TUI_SESSION)
    } else {
        tmux::attach(tmux::TUI_SESSION)
    }
}

/// Print every conversation corc owns, grouped by project in display order.
fn list() -> Result<()> {
    let state = state::State::load()?;
    let mut store = provider::MetaStore::new()?;
    let known: Vec<(String, PathBuf, &'static str)> = state
        .conversations
        .iter()
        .map(|c| (c.id.clone(), c.cwd.clone(), provider::by_id(&c.provider).id()))
        .collect();
    store.refresh(&known)?;

    if state.conversations.is_empty() {
        println!("no conversations");
        return Ok(());
    }
    let now = state::unix_now();
    for project in &state.projects {
        let convs: Vec<_> = state
            .conversations
            .iter()
            .filter(|c| c.cwd.display().to_string() == *project)
            .collect();
        if convs.is_empty() {
            continue;
        }
        println!("\n{}", display_dir(project));
        for conv in convs {
            let alive = conv
                .pane_id
                .as_deref()
                .is_some_and(tmux::pane_exists);
            let meta = store.meta(&conv.id);
            let s = status::derive(alive, meta, conv.last_viewed, false, now, conv.created_at);
            let title = meta
                .and_then(|m| m.display_title())
                .unwrap_or("(untitled)");
            let pane = conv.pane_id.as_deref().unwrap_or("-");
            println!(
                "  {} {:7}  {:>6}  {:5}  {}  {}",
                status_icon(s),
                s.label(),
                status::time_column(s, meta, conv.created_at, now),
                pane,
                conv.id,
                truncate(title, 60),
            );
        }
    }
    Ok(())
}

pub fn status_icon(status: status::Status) -> &'static str {
    match status {
        status::Status::Running => "\x1b[33m●\x1b[0m",
        status::Status::Unseen => "\x1b[34m●\x1b[0m",
        status::Status::Idle => "\x1b[90m●\x1b[0m",
        status::Status::Dead => "\x1b[90m○\x1b[0m",
    }
}

pub fn display_dir(dir: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) => dir.replacen(&home, "~", 1),
        Err(_) => dir.to_string(),
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}
