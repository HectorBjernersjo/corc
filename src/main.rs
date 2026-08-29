mod base64;
mod browser;
mod cd;
mod discovery;
mod doctor;
mod kitty;
mod picker;
mod projects;
mod provider;
mod repo;
mod state;
mod status;
mod tmux;
mod ui;
mod usage;
mod widget;
mod ws;

use anyhow::{Context, Result, bail};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Absolute path to this corc binary, captured once and cached. On Linux
/// `/proc/self/exe` grows a ` (deleted)` suffix as soon as the file is
/// replaced (a `cargo install` while corc is running), which would make every
/// later popup and key binding point at a nonexistent path — so the suffix is
/// stripped, landing back on the replacement binary at the same path.
pub fn self_exe() -> PathBuf {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(tmux::APP_NAME));
        match exe.to_string_lossy().strip_suffix(" (deleted)") {
            Some(stripped) => PathBuf::from(stripped),
            None => exe,
        }
    })
    .clone()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => open(),
        // `corc open DIR` skips the toggle and goes straight to DIR's project
        // session — what an outside host (a GUI's embedded terminal) runs to
        // land on the workspace it is showing.
        Some("open") => match args.get(1) {
            Some(dir) => open_dir(dir),
            None => open(),
        },
        Some("list") => list(),
        Some("doctor") => doctor::run(),
        // Reachable from inside an agent pane, which is the point: it toggles
        // the browser view for the conversation you are talking to without a
        // trip to the sidebar (D24).
        Some("browser") => browser::command(args.get(1).map(String::as_str)),
        // Also reachable from inside an agent pane: the agent prepares a new
        // workspace and asks corc to move its own conversation there
        // (ADR-0003) — corc's TUI types the provider's `/cd` into the pane.
        Some("cd") => cd::command(args.get(1).map(String::as_str)),
        Some("-h" | "--help" | "help") => {
            print_help();
            Ok(())
        }
        // Private entry point for the process tmux runs inside the dedicated
        // corc session. Keeping this explicit means public startup never
        // depends on guessing its role from the surrounding tmux session.
        Some("__tui") => ui::run(),
        // Private entry point for the browser view pane (D24). Like `__tui`,
        // explicit rather than inferred from its surroundings.
        Some("__browser") => browser::run(),
        // What `Ctrl+b` runs. Private because it differs from `corc browser`
        // only in where it puts an error — see `browser::key_toggle`.
        Some("__browser-toggle") => browser::key_toggle(),
        Some("projects") => projects::run(),
        Some("pick-dir") => pick_dir(&args),
        Some("jump") => jump(&args),
        Some("shortcuts") => shortcuts(),
        Some(other) => anyhow::bail!("unknown command: {other} (run `corc --help` for usage)"),
    }
}

fn print_help() {
    println!(
        "\
corc — a tmux-native hub for agent CLIs

Usage:
  corc [COMMAND]

Commands:
  open     Open corc, or toggle back when already there
  open DIR Go to DIR's project session (created if missing); without a
           terminal of its own it moves the last-active tmux client
  list     List every conversation corc owns
  browser  Toggle this conversation's browser view [on|off]
  cd DIR   Move this conversation to DIR (corc types the agent's /cd for you)
  doctor   Check tmux, agents, PATH, and state access
  help     Print this help

Running corc without a command is the same as `corc open`.
`corc browser` is the toggle Ctrl+b does inside corc; it can also be typed at
an agent (in Claude Code, `!corc browser`) or run from any shell, where it
applies to the conversation you are viewing."
    );
}

/// `corc pick-dir [--out FILE]` (D22): a centered picker over the merged
/// project directories, run inside a `tmux display-popup` by the sidebar's
/// `N`. One screen does everything: plain text fuzzy-filters the list, while
/// input starting with `~` or `/` switches the same picker into filesystem
/// completion — for a directory not yet in the list, including one that
/// doesn't exist yet, created via an explicit `+ create` row (see
/// `run_filter_picker`). The chosen directory is written to FILE (empty file
/// when cancelled) so the still-running TUI — the sole writer of state.json —
/// records it in the machine-local list and spawns there. Without `--out` the
/// choice is printed to stdout for manual use.
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

/// Deliver a popup picker's result: to the `--out` file when given (empty
/// when the user cancelled), otherwise to stdout.
fn emit_choice(args: &[String], value: Option<&str>) -> Result<()> {
    let out = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1));
    match out {
        Some(path) => {
            std::fs::write(path, value.unwrap_or("")).with_context(|| format!("writing {path}"))
        }
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
    let label = repo::label_for(&conv.cwd.to_string_lossy(), &state.projects);
    tmux::jump_to_window(&conv.cwd, n, &label)
}

/// The conversation whose agent pane is currently swapped into corc's content
/// slot — the one the user is looking at. Its pane is the only conversation
/// pane living in the corc session; every other conversation's pane sits
/// parked in the hidden session. This is deterministic — exactly "the session
/// this agent pane belongs to" — where a `last_viewed` guess could be stale
/// and point at whichever project the user last switched to. Falls back to the
/// most recently viewed when nothing is swapped in (placeholder showing).
pub fn viewed_conversation(state: &state::State) -> Option<&state::Conversation> {
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

/// `corc shortcuts`: print the keyboard cheat-sheet and wait for a key, run
/// inside a `tmux display-popup` by the sidebar's `?` and its menu button. Any
/// key (or Esc/q) closes the popup.
fn shortcuts() -> Result<()> {
    use ratatui::crossterm::event::{self, Event};
    use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    const BOLD: &str = "\x1b[1m";
    const DIM: &str = "\x1b[90m";
    const KEY: &str = "\x1b[36m";
    const RESET: &str = "\x1b[0m";

    let section = |title: &str| println!("\r\n{BOLD}{title}{RESET}\r");
    let row = |key: &str, desc: &str| println!("  {KEY}{key:<12}{RESET} {desc}\r");

    println!("\r\n{BOLD}corc — keyboard shortcuts{RESET}\r");

    section("Navigate");
    row("j / k  ↑ ↓", "move selection across panels");
    row("Ctrl+j / k", "next / previous panel (tmux at outer edge)");
    row("g / G", "jump to top / bottom");
    row("} / {  Ctrl+d / u", "next / previous project");
    row("Alt+1 – 9", "jump to window N of the project's session");
    row("/", "filter the list");

    section("Conversations");
    row("Enter · click", "view (resumes it if dead)");
    row("n", "new conversation in the selected directory");
    row(
        "N",
        "new conversation via the directory picker (add or create one from there)",
    );
    row("p", "pin / unpin the selected conversation at the top");
    row("s", "switch which agent new conversations use");
    row("x", "kill a live conversation / remove a dead one");

    section("Layout & misc");
    row("b", "browser view on/off for the selected conversation");
    row("Ctrl+b", "the same toggle, from anywhere inside corc");
    row("V, then K/J", "move mode: reorder projects");
    row("a", "cycle history: active / 3h / 1D / 3D / 1W / all time");
    row("r", "refresh now");
    row("?", "this help");
    row("Ctrl+C", "quit corc");

    println!("\r\n{DIM}press any key to close{RESET}\r");

    // A single keypress dismisses the popup. Raw mode so it closes on the
    // first key rather than waiting for Enter.
    let _ = enable_raw_mode();
    loop {
        match event::read() {
            Ok(Event::Key(_)) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = disable_raw_mode();
    Ok(())
}

/// `corc` / `corc open` (D15): the Ctrl+q toggle. Already in the corc session
/// ⇒ go back to the session the client came from; anywhere else ⇒ make sure
/// the visible `corc` session exists with the TUI running and take the client
/// there. Bound to Ctrl+q in tmux.conf via run-shell.
fn open() -> Result<()> {
    let exe = self_exe();
    // switch-client only works from inside tmux; that covers both a shell in
    // a pane (TMUX set) and the Ctrl+q run-shell binding (TMUX_PANE set).
    let in_tmux = std::env::var_os("TMUX").is_some() || std::env::var_os("TMUX_PANE").is_some();
    // Toggle: already viewing corc ⇒ go to the viewed conversation's project
    // session, landing on its last-active window (like Alt+N, but without a
    // fixed window number). Nothing viewed ⇒ fall back to the previous session.
    if in_tmux && tmux::current_session().ok().as_deref() == Some(tmux::TUI_SESSION) {
        let state = state::State::load()?;
        return match viewed_conversation(&state) {
            Some(conv) => {
                let label = repo::label_for(&conv.cwd.to_string_lossy(), &state.projects);
                tmux::jump_to_session(&conv.cwd, &label)
            }
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

/// `corc open DIR`: take this terminal to DIR's project session, creating it
/// (with its `.tmux.sh` hook) when missing. Attaches from a plain terminal,
/// switches the client from inside tmux, and moves the last-active client when
/// run without a terminal at all. Unlike the bare `open` there is no toggle:
/// the caller already knows where it wants to be.
fn open_dir(dir: &str) -> Result<()> {
    let dir = cd::canonical_dir(dir)?;
    let state = state::State::load()?;
    let label = repo::label_for(&dir.to_string_lossy(), &state.projects);
    let (session, _) = tmux::ensure_session(&dir, &label)?;
    let in_tmux = std::env::var_os("TMUX").is_some() || std::env::var_os("TMUX_PANE").is_some();
    if in_tmux {
        return tmux::switch_client(&session);
    }
    if std::io::stdin().is_terminal() {
        return tmux::attach(&session);
    }
    // No terminal of our own means a program ran us (a GUI's keybinding, a
    // window-manager script). Attaching can only fail, so move the client the
    // user last typed in: the terminal they are about to look at.
    match tmux::most_recent_client() {
        Some(client) => tmux::switch_client_of(&client, &session),
        None => bail!("no attached tmux client to move; open a terminal first"),
    }
}

/// Print every conversation corc owns, grouped by project in display order.
fn list() -> Result<()> {
    let state = state::State::load()?;
    let mut store = provider::MetaStore::new()?;
    // `list` prints every conversation, so every one of them is worth reading.
    let known: Vec<(discovery::Known, &'static str)> = state
        .conversations
        .iter()
        .map(|c| {
            (
                discovery::Known {
                    turn_started_at: c.turn_started_at,
                    ..discovery::Known::shown(&c.id, c.cwd.clone())
                },
                provider::by_id(&c.provider).id(),
            )
        })
        .collect();
    store.refresh(&known)?;
    // Leave the parse behind for the TUI (and the next `list`) to reuse.
    store.save_cache();

    if state.conversations.is_empty() {
        println!("no conversations");
        return Ok(());
    }
    let panes = tmux::all_panes().unwrap_or_default();
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
            let meta = store.meta(&conv.id);
            let live_pane = conv.pane_id.as_deref().and_then(|pane| panes.get(pane));
            let alive = live_pane.is_some();
            let runtime = conv.pane_id.as_deref().and_then(|id| {
                provider::pane_hint(provider::by_id(&conv.provider), id, panes.get(id)?, meta)
            });
            let s = status::derive_with_runtime(
                alive,
                runtime,
                meta,
                conv.last_viewed,
                false,
                now,
                conv.created_at,
            );
            let title = meta.and_then(|m| m.display_title()).unwrap_or("(untitled)");
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
        status::Status::Question => "\x1b[34m●\x1b[0m",
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
