//! tmux plumbing for the hidden-session / swap-pane topology (ADR-0001).
//!
//! All Claude panes live in the hidden session `_corc-sessions`, one window per
//! conversation, window name = conversation uuid. Viewing swaps a Claude
//! pane with the placeholder in the content pane slot; parking swaps it
//! back. Nothing is ever destroyed by a view/park.

use crate::provider::Provider;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Base name for everything the program creates in tmux — change this one
/// macro to rename the app. (A macro because `concat!` below only takes
/// literals, not `const`s.)
macro_rules! app_name {
    () => {
        "corc"
    };
}
pub const APP_NAME: &str = app_name!();
pub const HIDDEN_SESSION: &str = concat!("_", app_name!(), "-sessions");
/// The visible session the TUI lives in (D15). Prefixed with `_` so it never
/// clashes with a project session named after a directory.
pub const TUI_SESSION: &str = concat!("_", app_name!());
/// Transient window that keeps `_corc-sessions` alive while it has no conversation
/// windows; killed as soon as a real window exists.
const STUB_WINDOW: &str = "_stub";

fn tmux(args: &[&str]) -> Result<String> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .context("failed to run tmux")?;
    if !output.status.success() {
        bail!(
            "tmux {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn session_exists(name: &str) -> bool {
    tmux(&["has-session", "-t", &format!("={name}")]).is_ok()
}

/// Visible session names for the `corc projects` sessionizer (D21), most
/// recently attached first. The `_`-prefixed sessions corc owns (`_corc`,
/// `_corc-sessions`) are hidden. Any tmux error (no server, no sessions)
/// yields an empty list rather than failing the picker.
pub fn list_sessions() -> Vec<String> {
    let Ok(out) = tmux(&[
        "list-sessions",
        "-F",
        "#{session_last_attached}\t#{session_name}",
    ]) else {
        return Vec::new();
    };
    let mut rows: Vec<(i64, String)> = out
        .lines()
        .filter_map(|l| {
            let (ts, name) = l.split_once('\t')?;
            (!name.starts_with('_')).then(|| (ts.parse().unwrap_or(0), name.to_string()))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.into_iter().map(|(_, n)| n).collect()
}

/// Make sure the hidden session exists. A tmux session needs at least one
/// window, so an empty hidden session gets a stub window that is removed
/// once a conversation window exists.
pub fn ensure_hidden_session() -> Result<()> {
    if !session_exists(HIDDEN_SESSION) {
        tmux(&["new-session", "-d", "-s", HIDDEN_SESSION, "-n", STUB_WINDOW])?;
    }
    Ok(())
}

/// Make sure the TUI session exists with the TUI running in it (D15).
/// `exe` is the absolute path to the corc binary (the TUI becomes
/// the pane command, so quitting it closes its window).
///
/// The session can exist without a TUI pane (something went wrong), in
/// which case the TUI gets a fresh window there. If a TUI pane already
/// exists its window is selected instead, so the upcoming switch-client
/// lands on it.
pub fn ensure_tui_session(exe: &str) -> Result<()> {
    if !session_exists(TUI_SESSION) {
        tmux(&["new-session", "-d", "-s", TUI_SESSION, exe])?;
        return Ok(());
    }
    let tui_name = Path::new(exe)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| APP_NAME.to_string());
    // Skip the pane this very command runs in (`corc open` from a shell in
    // the session would otherwise see itself as the TUI).
    let self_pane = std::env::var("TMUX_PANE").unwrap_or_default();
    let out = tmux(&[
        "list-panes",
        "-s",
        "-t",
        &format!("={TUI_SESSION}"),
        "-F",
        "#{pane_id} #{window_index} #{pane_current_command}",
    ])?;
    let tui_window = out.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let pane = parts.next()?;
        let window = parts.next()?;
        let cmd = parts.next()?;
        (pane != self_pane && cmd == tui_name).then(|| window.to_string())
    });
    match tui_window {
        Some(window) => {
            tmux(&["select-window", "-t", &format!("={TUI_SESSION}:{window}")])?;
        }
        None => {
            tmux(&["new-window", "-t", &format!("={TUI_SESSION}:"), exe])?;
        }
    }
    Ok(())
}

fn kill_stub() {
    let _ = tmux(&[
        "kill-window",
        "-t",
        &format!("={HIDDEN_SESSION}:={STUB_WINDOW}"),
    ]);
}

/// Absolute path to an agent binary (`claude`, `cursor-agent`), resolved once
/// per name and cached.
///
/// corc runs the agent directly as a pane command (no wrapping shell, D12), so
/// tmux resolves it against the *tmux server's* environment — whose `PATH` is
/// often the stripped default it was started with and omits `~/.local/bin`
/// etc., leaving a bare name unspawnable. We resolve an absolute path once,
/// preferring the login shell's `PATH` (arbitrary install locations), then the
/// installer's known locations, and cache it. Moving the binary after corc has
/// started needs a corc restart to pick up (rare; accepted tradeoff).
pub fn resolve_binary(name: &str) -> String {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(name) {
        return hit.clone();
    }
    let resolved = resolve_binary_uncached(name);
    cache
        .lock()
        .unwrap()
        .insert(name.to_string(), resolved.clone());
    resolved
}

fn resolve_binary_uncached(name: &str) -> String {
    // 1. The login shell's PATH — covers wherever the user installed it.
    if let Ok(shell) = std::env::var("SHELL")
        && let Ok(out) = Command::new(&shell)
            .args(["-lc", &format!("command -v {name}")])
            .output()
        && out.status.success()
    {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !path.is_empty() && Path::new(&path).exists() {
            return path;
        }
    }
    // 2. The installer's known locations.
    if let Ok(home) = std::env::var("HOME") {
        for rel in [".local/bin", ".cargo/bin", ".npm-global/bin"] {
            let cand = PathBuf::from(&home).join(rel).join(name);
            if cand.exists() {
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    // 3. Give up and let tmux try its own PATH (original behavior).
    name.to_string()
}

/// Spawn a conversation in a new hidden window named by its id, running the
/// provider's agent directly as the pane command (no wrapping shell, D12) so
/// the window dies when the agent exits. Returns the new pane id.
pub fn spawn_conversation(
    dir: &Path,
    provider: &dyn Provider,
    id: &str,
    resume: bool,
) -> Result<String> {
    if !dir.is_dir() {
        bail!("directory {} no longer exists", dir.display());
    }
    let dir_str = dir.to_string_lossy();
    let bin = resolve_binary(provider.binary());
    let extra = provider.spawn_args(id, resume);
    let hidden_target = format!("={HIDDEN_SESSION}:");
    // Multiple trailing arguments make tmux exec the command directly.
    let base: Vec<&str> = if session_exists(HIDDEN_SESSION) {
        vec!["new-window", "-d", "-t", &hidden_target]
    } else {
        vec!["new-session", "-d", "-s", HIDDEN_SESSION]
    };
    let mut args = base;
    args.extend(["-n", id, "-c", &dir_str, "-P", "-F", "#{pane_id}", &bin]);
    args.extend(extra.iter().map(String::as_str));
    let pane_id = tmux(&args)?;
    if args[0] == "new-window" {
        kill_stub();
    }
    Ok(pane_id.trim().to_string())
}

/// Split corc's own window: sidebar (this pane) fixed at 40 columns on the
/// left, a plain-shell placeholder content pane on the right (D10).
/// Returns the placeholder pane id.
pub fn split_content_pane(sidebar_pane: &str) -> Result<String> {
    let out = tmux(&[
        "split-window",
        "-h",
        "-d",
        "-t",
        sidebar_pane,
        "-P",
        "-F",
        "#{pane_id}",
    ])?;
    enforce_sidebar_width(sidebar_pane)?;
    Ok(out.trim().to_string())
}

/// Pin the sidebar back to 40 columns. tmux redistributes columns
/// proportionally on any width change (terminal resize, font zoom, outer
/// split), so the fixed width set at split time drifts and must be
/// re-enforced — otherwise the sidebar grows and never snaps back.
pub fn enforce_sidebar_width(sidebar_pane: &str) -> Result<()> {
    tmux(&["resize-pane", "-t", sidebar_pane, "-x", "40"])?;
    Ok(())
}

/// Swap two panes without touching active/last-pane state.
pub fn swap_panes(a: &str, b: &str) -> Result<()> {
    tmux(&["swap-pane", "-d", "-s", a, "-t", b])?;
    Ok(())
}

pub fn select_pane(pane_id: &str) -> Result<()> {
    tmux(&["select-pane", "-t", pane_id])?;
    Ok(())
}

pub fn kill_pane(pane_id: &str) -> Result<()> {
    tmux(&["kill-pane", "-t", pane_id])?;
    Ok(())
}

pub fn pane_exists(pane_id: &str) -> bool {
    tmux(&["list-panes", "-a", "-F", "#{pane_id}"])
        .map(|out| out.lines().any(|l| l == pane_id))
        .unwrap_or(false)
}

/// Which session a pane currently lives in.
pub fn pane_session(pane_id: &str) -> Result<String> {
    let out = tmux(&["display-message", "-p", "-t", pane_id, "#{session_name}"])?;
    Ok(out.trim().to_string())
}

/// Every pane id currently in `session`. Used to find the conversation
/// swapped into corc's content slot: its claude pane is the one conversation
/// pane living in the corc session (all others are parked in the hidden one).
pub fn session_pane_ids(session: &str) -> Vec<String> {
    tmux(&[
        "list-panes",
        "-s",
        "-t",
        &format!("={session}"),
        "-F",
        "#{pane_id}",
    ])
    .map(|out| out.lines().map(str::to_string).collect())
    .unwrap_or_default()
}

fn hidden_window_exists(name: &str) -> bool {
    tmux(&[
        "list-windows",
        "-t",
        &format!("={HIDDEN_SESSION}"),
        "-F",
        "#{window_name}",
    ])
    .map(|out| out.lines().any(|l| l == name))
    .unwrap_or(false)
}

/// Park a Claude pane stranded outside `_corc-sessions` (corc crashed mid-view,
/// D16) back into a hidden window named by its conversation uuid.
pub fn park_stray(pane_id: &str, id: &str) -> Result<()> {
    ensure_hidden_session()?;
    // If the uuid window still exists it can only hold the placeholder shell
    // that was swapped out when the conversation was viewed — remove it so
    // the window name stays unique.
    if hidden_window_exists(id) {
        let _ = tmux(&["kill-window", "-t", &format!("={HIDDEN_SESSION}:={id}")]);
    }
    tmux(&[
        "break-pane",
        "-d",
        "-n",
        id,
        "-s",
        pane_id,
        "-t",
        &format!("={HIDDEN_SESSION}:"),
    ])?;
    kill_stub();
    Ok(())
}

/// Kill the hidden window named by a conversation uuid (used to reclaim the
/// placeholder when the viewed Claude died, leaving its shell parked there).
pub fn kill_hidden_window(id: &str) -> Result<()> {
    tmux(&["kill-window", "-t", &format!("={HIDDEN_SESSION}:={id}")])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Real-session helpers, kept for digit jump (step 4).
// ---------------------------------------------------------------------------

/// Session naming convention from new.sh: basename with '.' → '_'.
pub fn session_name_for(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().replace('.', "_"))
        .unwrap_or_else(|| format!("{APP_NAME}-unknown"))
}

/// Create a detached session (D13). A per-project `.tmux.sh` hook, if present,
/// owns the layout — same convention as new.sh. Without a hook corc lays out
/// its default working session: nvim in window 1, an empty console in window
/// 2. The console is created with `-d` so window 1 (nvim) stays the active
/// window — a C-q that just created the session lands on the editor. Never
/// used for the hidden session.
pub fn create_session(name: &str, dir: &Path) -> Result<()> {
    let dir_str = dir.to_string_lossy();
    tmux(&["new-session", "-d", "-s", name, "-c", &dir_str])?;
    let hook = dir.join(".tmux.sh");
    if is_executable(&hook) {
        let _ = Command::new(&hook).arg(name).arg(dir).status();
    } else {
        // Window 1 (created by new-session) holds a shell — start nvim in it,
        // leaving the shell underneath so `:q` returns to a prompt.
        let _ = send_line(name, 1, "nvim");
        let _ = tmux(&["new-window", "-d", "-t", &format!("={name}:"), "-c", &dir_str]);
    }
    Ok(())
}

pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub fn window_exists(session: &str, index: u8) -> bool {
    tmux(&[
        "list-windows",
        "-t",
        &format!("={session}"),
        "-F",
        "#{window_index}",
    ])
    .map(|out| out.lines().any(|l| l == index.to_string()))
    .unwrap_or(false)
}

/// Create window `index` of a real session (digit jump, D13). `cmd` becomes
/// the pane command when given (window 1 is created running nvim).
pub fn create_window_at(session: &str, index: u8, dir: &Path, cmd: Option<&str>) -> Result<()> {
    let target = format!("={session}:{index}");
    let dir_str = dir.to_string_lossy();
    let mut args: Vec<&str> = vec!["new-window", "-d", "-t", &target, "-c", &dir_str];
    if let Some(cmd) = cmd {
        args.push(cmd);
    }
    tmux(&args)?;
    Ok(())
}

/// Foreground command of the active pane in a real-session window — how the
/// digit jump tells an idle shell prompt from a busy process (D13).
pub fn window_current_command(session: &str, index: u8) -> Result<String> {
    let out = tmux(&[
        "display-message",
        "-p",
        "-t",
        &format!("={session}:{index}"),
        "#{pane_current_command}",
    ])?;
    Ok(out.trim().to_string())
}

/// Type a line + Enter into a real-session window's active pane.
pub fn send_line(session: &str, index: u8, text: &str) -> Result<()> {
    tmux(&[
        "send-keys",
        "-t",
        &format!("={session}:{index}"),
        text,
        "Enter",
    ])?;
    Ok(())
}

pub fn select_window(session: &str, index: u8) -> Result<()> {
    tmux(&["select-window", "-t", &format!("={session}:{index}")])?;
    Ok(())
}

/// Switch the attached client to a session; corc keeps running in its own.
pub fn switch_client(session: &str) -> Result<()> {
    tmux(&["switch-client", "-t", &format!("={session}")])?;
    Ok(())
}

/// Foreground commands that count as an idle shell prompt for the digit
/// jump's window-1 nvim rule (D13); anything else is busy and never touched.
const SHELLS: &[&str] = &["bash", "zsh", "fish", "sh", "dash", "ksh", "tcsh", "nu"];

/// Install the digit-jump key bindings at runtime so the user's tmux config
/// file is never touched (D13). `M-1`..`M-9` become session-scoped via
/// `if-shell -F` (evaluated at key-press, no shell spawned): inside the corc
/// session they run `corc jump N` — the sidebar's `1`-`9`, now reachable while
/// focus is in the Claude pane — and in every other session they keep the
/// conventional Alt+number window switch. Overwriting is idempotent, so
/// re-launching corc is safe; `restore_window_bindings` undoes it on quit.
/// `exe` is the absolute corc binary path.
pub fn install_jump_bindings(exe: &str) {
    let cond = format!("#{{==:#{{session_name}},{TUI_SESSION}}}");
    for n in 1..=9u8 {
        let key = format!("M-{n}");
        let jump = format!("run-shell \"'{exe}' jump {n}\"");
        let fallback = format!("select-window -t {n}");
        let _ = tmux(&[
            "bind-key", "-n", &key, "if-shell", "-F", &cond, &jump, &fallback,
        ]);
    }
}

/// Undo `install_jump_bindings` on quit: put the plain Alt+number window
/// switch back, so once corc exits the tmux server matches the user's config
/// again. A crash that skips this leaves the conditional binding in place —
/// harmless, since its non-corc branch is the same window switch and `corc
/// jump` runs headless regardless.
pub fn restore_window_bindings() {
    for n in 1..=9u8 {
        let key = format!("M-{n}");
        let idx = n.to_string();
        let _ = tmux(&["bind-key", "-n", &key, "select-window", "-t", &idx]);
    }
}

/// Digit jump (D13): take the client to window `n` of `dir`'s real session,
/// creating the session (with its `.tmux.sh` hook) and window as needed.
/// Window 1 is the editor window: created running nvim, and an idle shell
/// there gets `nvim` typed into it — but a busy foreground process is never
/// disturbed, just focused. Shared by the sidebar's `1`-`9` and the headless
/// `corc jump N` that a tmux binding runs from inside the Claude pane.
pub fn jump_to_window(dir: &Path, n: u8) -> Result<()> {
    let session = session_name_for(dir);
    let created = !session_exists(&session);
    if created {
        create_session(&session, dir)?;
    }
    if !window_exists(&session, n) {
        let cmd = (n == 1).then_some("nvim");
        create_window_at(&session, n, dir, cmd)?;
    } else if n == 1 && !created {
        // An existing idle shell in window 1 gets nvim typed in; a busy
        // process is left alone. Skipped on a freshly created session, where
        // create_session already started nvim (avoids typing it twice).
        let cmd = window_current_command(&session, 1)?;
        if SHELLS.contains(&cmd.as_str()) {
            send_line(&session, 1, "nvim")?;
        }
    }
    select_window(&session, n)?;
    // corc keeps running in its own session.
    switch_client(&session)
}

/// Take the client to `dir`'s real session, landing on whatever window was
/// last active there (its current window). Creates the session — with its
/// `.tmux.sh` hook, or corc's default nvim+console layout — if missing. The
/// window-less counterpart to `jump_to_window`: the C-q toggle uses it to
/// reach the viewed conversation's project without a fixed window number.
pub fn jump_to_session(dir: &Path) -> Result<()> {
    let session = session_name_for(dir);
    if !session_exists(&session) {
        create_session(&session, dir)?;
    }
    switch_client(&session)
}

/// The session the triggering client is attached to right now. Run from the
/// `C-q` binding's `run-shell`, this resolves to the client that pressed the
/// key — the same client `switch_client`/`switch_to_last` act on — so `corc
/// open` can tell "already in corc" from "elsewhere" and toggle (D15).
pub fn current_session() -> Result<String> {
    let out = tmux(&["display-message", "-p", "#{session_name}"])?;
    Ok(out.trim().to_string())
}

/// Switch the client back to the session it was on before the current one
/// (tmux's per-client last session) — the return half of the `C-q` toggle.
/// Viewing a conversation in corc is a `swap-pane`, not a session switch, so
/// the last session stays the one the user came from.
pub fn switch_to_last() -> Result<()> {
    tmux(&["switch-client", "-l"])?;
    Ok(())
}

/// Attach the calling terminal to a session — what `corc open` does when
/// run outside tmux, where switch-client has no client to move.
pub fn attach(session: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["attach-session", "-t", &format!("={session}")])
        .status()
        .context("failed to run tmux")?;
    if !status.success() {
        bail!("could not attach; from a terminal run: tmux attach -t {session}");
    }
    Ok(())
}
