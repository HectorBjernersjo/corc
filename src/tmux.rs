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

/// Create a detached session, honoring the per-project .tmux.sh hook just
/// like new.sh does. Never used for the hidden session.
pub fn create_session(name: &str, dir: &Path) -> Result<()> {
    tmux(&["new-session", "-d", "-s", name, "-c", &dir.to_string_lossy()])?;
    let hook = dir.join(".tmux.sh");
    if is_executable(&hook) {
        let _ = Command::new(&hook).arg(name).arg(dir).status();
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
