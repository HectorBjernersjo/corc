mod discovery;
mod picker;
mod state;
mod status;
mod tmux;
mod ui;

use anyhow::{Context, Result};
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => ui::run(),
        Some("open") => open(),
        Some("list") => list(),
        Some(other) => anyhow::bail!("unknown command: {other} (expected: open, list)"),
    }
}

/// `corc open` (D15): make sure the visible `corc` session exists with the
/// TUI running in it, then take the client there. Bound to Ctrl+q in
/// tmux.conf via run-shell.
fn open() -> Result<()> {
    let exe = std::env::current_exe().context("locating the corc binary")?;
    tmux::ensure_tui_session(&exe.to_string_lossy())?;
    // switch-client only works from inside tmux; that covers both a shell in
    // a pane (TMUX set) and the Ctrl+q run-shell binding (TMUX_PANE set).
    // From a plain terminal, attach instead.
    if std::env::var_os("TMUX").is_some() || std::env::var_os("TMUX_PANE").is_some() {
        tmux::switch_client(tmux::TUI_SESSION)
    } else {
        tmux::attach(tmux::TUI_SESSION)
    }
}

/// Print every conversation corc owns, grouped by project in display order.
fn list() -> Result<()> {
    let state = state::State::load()?;
    let mut store = discovery::Store::new()?;
    let known: Vec<(String, PathBuf)> = state
        .conversations
        .iter()
        .map(|c| (c.id.clone(), c.cwd.clone()))
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
            let s = status::derive(alive, meta, conv.last_viewed, false);
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
