//! tmux plumbing for the hidden-session / swap-pane topology (ADR-0001).
//!
//! All agent panes live in the hidden session `_corc-sessions`, one window per
//! conversation, window name = conversation uuid. Viewing swaps a Claude
//! pane with the placeholder in the content pane slot; parking swaps it
//! back. Nothing is ever destroyed by a view/park.

use crate::kitty::Terminal;
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
/// Linux process name that matches vim-tmux-navigator's stock Vim pattern.
/// The pattern accepts any prefix before `/view`, so corc receives C-hjkl and
/// can use them internally before handing edge navigation back to tmux.
pub const NAVIGATOR_PROCESS_NAME: &str = concat!(app_name!(), "/view");
pub const HIDDEN_SESSION: &str = concat!("_", app_name!(), "-sessions");
/// The visible session the TUI lives in (D15). Prefixed with `_` so it is
/// unlikely to clash with a project session named after a directory — only a
/// project literally called `.corc` would, and `ensure_session` suffixes it.
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

/// The two sessions corc runs itself, which are never project sessions. Matched
/// by name rather than by their `_` prefix: `ensure_session` turns a leading
/// `.` into `_`, so a project like `~/dotfiles/.agents` legitimately owns the
/// session `_agents` — treating that as internal made it invisible to
/// `session_for_dir`, and every C-q created another `_agents-N` beside it.
fn is_internal(name: &str) -> bool {
    name == TUI_SESSION || name == HIDDEN_SESSION
}

/// Visible session names for the `corc projects` sessionizer (D21), most
/// recently attached first. The sessions corc owns (`_corc`, `_corc-sessions`)
/// are hidden. Any tmux error (no server, no sessions) yields an empty list
/// rather than failing the picker.
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
            (!is_internal(name)).then(|| (ts.parse().unwrap_or(0), name.to_string()))
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
/// `exe` is the absolute path to the corc binary. tmux starts its private
/// `__tui` entry point as the pane command, so quitting it closes its window.
///
/// The session can exist without a TUI pane (something went wrong), in
/// which case the TUI gets a fresh window there. If a TUI pane already
/// exists its window is selected instead, so the upcoming switch-client
/// lands on it.
pub fn ensure_tui_session(exe: &str) -> Result<()> {
    if !session_exists(TUI_SESSION) {
        tmux(&["new-session", "-d", "-s", TUI_SESSION, exe, "__tui"])?;
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
        (pane != self_pane && is_tui_command(cmd, &tui_name)).then(|| window.to_string())
    });
    match tui_window {
        Some(window) => {
            tmux(&["select-window", "-t", &format!("={TUI_SESSION}:{window}")])?;
        }
        None => {
            tmux(&[
                "new-window",
                "-t",
                &format!("={TUI_SESSION}:"),
                exe,
                "__tui",
            ])?;
        }
    }
    Ok(())
}

fn is_tui_command(command: &str, binary_name: &str) -> bool {
    command == binary_name || command == NAVIGATOR_PROCESS_NAME
}

#[derive(Clone, Copy)]
pub enum PaneDirection {
    Left,
    Down,
    Up,
    Right,
}

impl PaneDirection {
    fn flag(self) -> &'static str {
        match self {
            Self::Left => "-L",
            Self::Down => "-D",
            Self::Up => "-U",
            Self::Right => "-R",
        }
    }

    /// The `pane_at_*` format that is true when there is no pane further in
    /// this direction.
    fn edge(self) -> &'static str {
        match self {
            Self::Left => "#{pane_at_left}",
            Self::Down => "#{pane_at_bottom}",
            Self::Up => "#{pane_at_top}",
            Self::Right => "#{pane_at_right}",
        }
    }
}

/// Hand a Ctrl+h/j/k/l edge movement back to the surrounding tmux layout,
/// matching vim-tmux-navigator's behavior inside Vim. Fire-and-reap in the
/// background so navigation never stalls the TUI event loop.
///
/// `select-pane -L` wraps around to the opposite side of the window, so guard
/// it with `pane_at_*` and do nothing at the outer edge — pressing Ctrl+h in
/// the leftmost pane should stay put, not jump to the rightmost one. The guard
/// rides along in the same tmux invocation to keep this a single spawn.
pub fn select_adjacent_pane(direction: PaneDirection) {
    let Ok(mut child) = Command::new("tmux")
        .args([
            "if-shell",
            "-F",
            direction.edge(),
            "",
            &format!("select-pane {}", direction.flag()),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return;
    };
    std::thread::spawn(move || {
        let _ = child.wait();
    });
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
/// Agent startup goes through the user's shell, but an absolute path still
/// avoids depending on the tmux server's often-stripped `PATH`. Resolve it
/// from the login shell first, then the installers' known locations, and
/// cache it. Moving the binary after corc has started needs a corc restart to
/// pick up (rare; accepted tradeoff).
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

/// Run an agent from an initialized user shell in its project directory.
///
/// `tmux -c` sets the process cwd but does not run shell directory-change
/// hooks. In particular, Bash's direnv hook normally runs while drawing a
/// prompt, which never happens for a direct pane command. The explicit `cd`
/// gives interactive shells a real directory transition and `direnv exec`
/// guarantees an allowed `.envrc` is applied even when the hook is
/// prompt-based. `exec` then replaces both wrappers with the agent, preserving
/// the D12 rule that the pane dies when the agent exits.
const AGENT_SHELL_COMMAND: &str = concat!(
    "cd -- \"$1\" && shift && ",
    "if command -v direnv >/dev/null 2>&1; then ",
    "exec direnv exec . \"$@\"; ",
    "else exec \"$@\"; fi",
);

fn agent_shell_invocation(dir: &Path, bin: &str, extra: &[String]) -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut command = vec![
        shell,
        "-lic".to_string(),
        AGENT_SHELL_COMMAND.to_string(),
        "corc-agent".to_string(),
        dir.to_string_lossy().into_owned(),
        bin.to_string(),
    ];
    command.extend_from_slice(extra);
    command
}

/// Spawn a conversation in a new hidden window named by its id, running the
/// provider's agent through an initialized user shell. The shell loads project
/// environment, changes directory, applies direnv when available, and then
/// execs the agent so the window still dies when the agent exits (D12).
/// Returns the new pane id.
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
    let command = agent_shell_invocation(dir, &bin, &extra);
    let hidden_target = format!("={HIDDEN_SESSION}:");
    // Multiple trailing arguments make tmux exec the shell directly; the
    // shell in turn replaces itself with the agent after initialization.
    let base: Vec<&str> = if session_exists(HIDDEN_SESSION) {
        vec!["new-window", "-d", "-t", &hidden_target]
    } else {
        vec!["new-session", "-d", "-s", HIDDEN_SESSION]
    };
    let mut args = base;
    args.extend(["-n", id, "-c", &dir_str, "-P", "-F", "#{pane_id}"]);
    // The pane environment is the only thing that varies per conversation all
    // the way down into the agent's own tool calls, which is what gives each
    // conversation its own browser profile (D24). The login shell passes it on
    // untouched, so it reaches the Playwright MCP server the agent starts.
    let profile = crate::browser::profile_env(id);
    if let Some(profile) = profile.as_deref() {
        args.extend(["-e", profile]);
    }
    args.extend(command.iter().map(String::as_str));
    let pane_id = tmux(&args)?;
    if args[0] == "new-window" {
        kill_stub();
    }
    Ok(pane_id.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_shell_invocation_passes_paths_and_arguments_positionally() {
        let args = vec!["--resume".to_string(), "id with spaces".to_string()];
        // Avoid mutating SHELL in a parallel test: only assert the stable tail.
        let invocation = agent_shell_invocation(
            Path::new("/tmp/project with spaces; untouched"),
            "/tmp/bin with spaces/codex",
            &args,
        );

        assert_eq!(
            &invocation[1..4],
            &["-lic", AGENT_SHELL_COMMAND, "corc-agent"]
        );
        assert_eq!(invocation[4], "/tmp/project with spaces; untouched");
        assert_eq!(invocation[5], "/tmp/bin with spaces/codex");
        assert_eq!(&invocation[6..], &["--resume", "id with spaces"]);
    }

    #[test]
    fn agent_shell_command_changes_directory_and_applies_direnv() {
        assert!(AGENT_SHELL_COMMAND.starts_with("cd -- \"$1\" && shift"));
        assert!(AGENT_SHELL_COMMAND.contains("exec direnv exec . \"$@\""));
        assert!(AGENT_SHELL_COMMAND.contains("else exec \"$@\""));
    }

    #[test]
    fn pane_snapshot_keeps_ids_titles_and_pids() {
        let panes = parse_panes(
            "%32\t4711\t✳ Review backup restore plan status\n\
             %43\t4712\t⠂ platform-restore-cleanup-runbook\n\
             %44\t4713\t\n",
        );

        assert_eq!(
            panes.get("%32"),
            Some(&Pane {
                title: "✳ Review backup restore plan status".into(),
                pid: 4711,
            })
        );
        assert_eq!(
            panes.get("%43"),
            Some(&Pane {
                title: "⠂ platform-restore-cleanup-runbook".into(),
                pid: 4712,
            })
        );
        // A pane with no title is still a pane — dropping it would read as a
        // dead conversation.
        assert_eq!(
            panes.get("%44"),
            Some(&Pane {
                title: String::new(),
                pid: 4713,
            })
        );
    }

    #[test]
    fn tui_detection_accepts_binary_and_navigator_process_names() {
        assert!(is_tui_command("corc", "corc"));
        assert!(is_tui_command(NAVIGATOR_PROCESS_NAME, "corc"));
        assert!(!is_tui_command("bash", "corc"));
    }

    /// A session is found by its directory, so the key has to survive the ways
    /// a path can be spelled — a symlink, a trailing slash — and has to keep
    /// working for a directory that no longer exists, which is the state every
    /// session whose project was moved or trashed is in.
    #[test]
    fn session_dir_keys_survive_symlinks_and_deletion() {
        let base = std::env::temp_dir().join("corc-test-dir-key");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real")).unwrap();
        let base = base.canonicalize().unwrap();
        let real = base.join("real");
        std::os::unix::fs::symlink(&real, base.join("link")).unwrap();

        let key = |p: &std::path::Path| dir_key(&p.to_string_lossy());
        // The same directory reached three ways is one key.
        assert_eq!(key(&real), real.to_string_lossy());
        assert_eq!(key(&base.join("link")), key(&real));
        assert_eq!(key(&real.join("../real")), key(&real));

        // A directory that is gone cannot be canonicalized, and falls back to
        // its spelling — still equal to itself, still distinct from others,
        // which is all the lookup needs.
        let gone = base.join("trashed");
        assert_eq!(key(&gone), gone.to_string_lossy());
        assert_ne!(key(&gone), key(&real));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn vim_navigation_directions_map_to_tmux_flags() {
        assert_eq!(PaneDirection::Left.flag(), "-L");
        assert_eq!(PaneDirection::Down.flag(), "-D");
        assert_eq!(PaneDirection::Up.flag(), "-U");
        assert_eq!(PaneDirection::Right.flag(), "-R");
    }
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

/// Split the content pane to put the browser view beside the agent (D24),
/// running corc's private `__browser` entry point. Returns the new pane id.
/// The view follows whichever conversation is in the content slot, so this
/// pane is created once and never respawned on a conversation switch.
pub fn split_browser_pane(content_pane: &str, exe: &str) -> Result<String> {
    let out = tmux(&[
        "split-window",
        "-h",
        "-d",
        "-t",
        content_pane,
        "-P",
        "-F",
        "#{pane_id}",
        exe,
        "__browser",
    ])?;
    Ok(out.trim().to_string())
}

/// Process id of the program running in a pane — the root corc walks down
/// from to find the agent's Playwright browser.
pub fn pane_pid(pane_id: &str) -> Option<u32> {
    tmux(&["display-message", "-p", "-t", pane_id, "#{pane_pid}"])
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// A pane's size in cells and the identity of the terminal showing it, in one
/// query — the browser view asks for all of it on every tick, and one tmux
/// process is cheaper than three.
///
/// The identity is of the *outer* terminal, which is what decides whether
/// images can be drawn at all. It comes out unknown when no client is
/// attached, which means "don't know yet" rather than "no".
pub fn pane_info(pane_id: &str) -> Option<(u16, u16, Terminal)> {
    let out = tmux(&[
        "display-message",
        "-p",
        "-t",
        pane_id,
        &format!("#{{pane_width}}\t#{{pane_height}}\t{TERMINAL_FORMAT}"),
    ])
    .ok()?;
    let mut fields = out.trim_end_matches('\n').split('\t');
    Some((
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
        parse_terminal(&mut fields),
    ))
}

/// Identity of the terminal the current client is running in, for
/// `corc doctor`.
pub fn client_terminal() -> Terminal {
    let Ok(out) = tmux(&["display-message", "-p", TERMINAL_FORMAT]) else {
        return Terminal::default();
    };
    parse_terminal(&mut out.trim_end_matches('\n').split('\t'))
}

/// `$TERM` plus the XTVERSION answer, in that order. tmux substitutes an empty
/// string for a terminal that never identified itself, so a field can be
/// missing but the count is fixed.
const TERMINAL_FORMAT: &str = "#{client_termname}\t#{client_termtype}";

fn parse_terminal<'a>(fields: &mut impl Iterator<Item = &'a str>) -> Terminal {
    Terminal {
        term: fields.next().unwrap_or_default().trim().to_string(),
        program: fields.next().unwrap_or_default().trim().to_string(),
    }
}

/// Ask tmux to redraw the attached client from its grid.
///
/// A freshly created pane's first writes can land in tmux's grid without ever
/// reaching the screen, and tmux then diffs against that grid and sees nothing
/// to send — so writing the same cells again changes nothing. For placeholder
/// cells that means the image streams to a pane with nothing to show it through,
/// indefinitely: measured at 20 frames per second into a pane that stayed blank
/// until a redraw happened for some unrelated reason (in practice, the user
/// moving between panes). This is the redraw, asked for on purpose.
pub fn refresh_client() {
    let _ = tmux(&["refresh-client"]);
}

/// Whether tmux will forward graphics escape sequences to the terminal at all.
/// Without `allow-passthrough` every frame corc emits is swallowed silently.
pub fn passthrough_enabled() -> bool {
    tmux(&["show", "-gv", "allow-passthrough"])
        .map(|out| matches!(out.trim(), "on" | "all"))
        .unwrap_or(false)
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

/// Snapshot every pane currently known to tmux, including the terminal title
/// set by the process inside it. Callers use one snapshot for both liveness and
/// provider-specific runtime hints instead of spawning per-pane tmux queries.
pub fn all_panes() -> Result<HashMap<String, Pane>> {
    Ok(parse_panes(&tmux(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}\t#{pane_pid}\t#{pane_title}",
    ])?))
}

/// A live pane: the terminal title corc reads agent state out of, and the pid
/// of the process the pane started — the root of the tree a browser hides in.
/// Both come from the one `list-panes` call every refresh already makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    pub title: String,
    pub pid: u32,
}

fn parse_panes(output: &str) -> HashMap<String, Pane> {
    output
        .lines()
        .filter_map(|line| {
            // The title goes last and is the only field that can contain a
            // tab, so it takes whatever remains.
            let mut fields = line.splitn(3, '\t');
            let id = fields.next()?;
            let pid = fields.next()?.trim().parse().ok()?;
            let title = fields.next()?;
            Some((
                id.to_string(),
                Pane {
                    title: title.to_string(),
                    pid,
                },
            ))
        })
        .collect()
}

/// Type a line + Enter into a specific pane, literally (`-l`) so the text is
/// never interpreted as tmux key names. Used to hand a provider's relocation
/// command (`/cd …`, ADR-0003) to the agent in that pane; agents queue input
/// typed mid-turn, so this is safe whether the conversation is idle or busy.
pub fn type_into_pane(pane_id: &str, line: &str) -> Result<()> {
    tmux(&["send-keys", "-t", pane_id, "-l", line])?;
    tmux(&["send-keys", "-t", pane_id, "Enter"])?;
    Ok(())
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

/// Park an agent pane stranded outside `_corc-sessions` (corc crashed mid-view,
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

/// Rename a conversation's hidden window when its provisional provider id
/// resolves to the real session id. Renaming goes by name, so it
/// works whether the window currently holds the agent pane or — while the
/// conversation is viewed — the swapped-out placeholder.
pub fn rename_hidden_window(old: &str, new: &str) -> Result<()> {
    tmux(&[
        "rename-window",
        "-t",
        &format!("={HIDDEN_SESSION}:={old}"),
        new,
    ])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Real-session helpers, kept for digit jump (step 4).
// ---------------------------------------------------------------------------

/// A project session is identified by its **directory**, never by its name.
/// The name is a label — `repo::labels` derives it from whichever paths corc
/// knows about, so it changes when a new project makes an old label ambiguous
/// — and a key that moves under you is no key at all. Keying on the directory
/// instead means a relabelling renames the session rather than orphaning it
/// and starting a second one in the same directory.
///
/// tmux's `#{session_path}` is the `-c` value the session was created with,
/// not the active pane's cwd, which is exactly the identity wanted: a session
/// belongs to the project it was opened for, wherever the user has since cd'd.
///
/// corc's own sessions (`_corc`, `_corc-sessions`) are never candidates —
/// `_corc` sits in `$HOME`, which would otherwise be adopted by a conversation
/// rooted there.
fn session_dirs_with_names() -> Vec<(String, String)> {
    let Ok(out) = tmux(&["list-sessions", "-F", "#{session_path}\t#{session_name}"]) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(_, name)| !is_internal(name))
        .map(|(path, name)| (dir_key(path), name.to_string()))
        .collect()
}

/// Comparison key for a session's directory: the canonical path while it
/// exists, else the path as given. A session whose directory was renamed or
/// deleted keeps matching itself rather than matching everything or nothing.
pub fn dir_key(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.trim_end_matches('/').to_string())
}

/// Directories that already have a session — what the sessionizer filters its
/// directory list against, in one tmux call rather than one per directory.
pub fn session_dirs() -> std::collections::HashSet<String> {
    session_dirs_with_names()
        .into_iter()
        .map(|(d, _)| d)
        .collect()
}

/// The name of the session living in `dir`, if any.
pub fn session_for_dir(dir: &Path) -> Option<String> {
    let want = dir_key(&dir.to_string_lossy());
    session_dirs_with_names()
        .into_iter()
        .find(|(d, _)| *d == want)
        .map(|(_, name)| name)
}

/// The session for `dir`, created if it does not have one yet, returned with
/// whether this call created it (the digit jump starts nvim only on a fresh
/// session). An existing session is used whatever it is called, and renamed to
/// `label` when the label has moved on — so a project whose label grew from
/// `main` to `gbandit/main` keeps the very session its editor is open in.
///
/// `.` is replaced with `_` (tmux reserves it in session names). `/` is fine,
/// and every target here is an exact `=name` match, so the slash from a
/// grown label never reads as a pattern.
pub fn ensure_session(dir: &Path, label: &str) -> Result<(String, bool)> {
    let label = label.replace('.', "_");
    if let Some(existing) = session_for_dir(dir) {
        // Renaming onto a name someone else holds would fail; keep the old
        // name in that case — the lookup is by path, so nothing is lost.
        if existing != label && !session_exists(&label) {
            tmux(&["rename-session", "-t", &format!("={existing}"), &label])?;
            return Ok((label, false));
        }
        return Ok((existing, false));
    }
    // The label is unique among corc's paths, but a session corc did not make
    // can still hold the name. Suffix rather than fail to create.
    let mut name = label.clone();
    let mut n = 2;
    while session_exists(&name) {
        name = format!("{label}-{n}");
        n += 1;
    }
    create_session(&name, dir)?;
    Ok((name, true))
}

/// Create a detached session (D13). A per-project `.tmux.sh` hook, if present,
/// owns the layout — same convention as new.sh. Without a hook corc lays out
/// its default working session: nvim in window 1, an empty console in window
/// 2. The console is created with `-d` so window 1 (nvim) stays the active
/// window — a C-q that just created the session lands on the editor. Never
/// used for the hidden session.
pub fn create_session(name: &str, dir: &Path) -> Result<()> {
    // Canonical, because this is the value `#{session_path}` reports back and
    // `session_for_dir` matches on: a session created through a symlinked path
    // has to answer to the real one.
    let dir_str = dir_key(&dir.to_string_lossy());
    tmux(&["new-session", "-d", "-s", name, "-c", &dir_str])?;
    let hook = dir.join(".tmux.sh");
    if is_executable(&hook) {
        let _ = Command::new(&hook).arg(name).arg(dir).status();
    } else {
        // Window 1 (created by new-session) holds a shell — start nvim in it,
        // leaving the shell underneath so `:q` returns to a prompt.
        let _ = send_line(name, 1, "nvim");
        let _ = tmux(&[
            "new-window",
            "-d",
            "-t",
            &format!("={name}:"),
            "-c",
            &dir_str,
        ]);
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
/// re-launching corc is safe; `restore_bindings` undoes it on quit.
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

/// The key that toggles the browser view for the conversation in view (D24).
const BROWSER_KEY: &str = "C-b";

/// Key table an earlier version of this code pointed the corc session at, kept
/// only so `install_browser_binding` can undo it. See that function.
const STALE_KEY_TABLE: &str = "corc";

/// Put a line on tmux's own message line, where tmux puts `returned 1` — the
/// full width of the window and long enough to read, which the sidebar's
/// one-line footer is not.
pub fn show_message(text: &str) {
    let _ = tmux(&["display-message", "-d", "5000", "--", text]);
}

/// Bind `C-b` to the browser toggle (D24), session-scoped the same way the
/// digit jump is: a root binding whose `if-shell -F` condition runs the toggle
/// inside corc and, everywhere else, passes the key through as if unbound.
/// Idempotent, so relaunching corc is safe; `restore_bindings` unbinds it on
/// quit.
///
/// It runs `__browser-toggle` rather than the plain `corc browser` a user
/// would type, for what tmux does either side of a `run-shell`: it shows
/// stdout in view mode over the pane — which a "browser view: on" line does
/// not deserve — and it drops stderr entirely, replacing a failure with its
/// own `returned 1`. So the one thing worth reading, "playwright is not
/// exposing a debugging port", was the one thing that never arrived. The
/// private entry point says it on tmux's message line instead.
///
/// A session-local key table would be the tidier home for this, but a session's
/// `key-table` option *replaces* the root table rather than layering over it,
/// so every `bind-key -n` the user has — and corc's own digit jump — goes dead
/// inside the corc session. That is what this used to do, so the option is
/// unset here too: a corc session that outlives the upgrade would otherwise
/// keep swallowing root bindings until it is killed.
///
/// A user whose tmux prefix is `C-b` never reaches this binding — tmux checks
/// the prefix first, so the key stays their prefix and the toggle is simply
/// unavailable. `corc doctor` says so.
pub fn install_browser_binding(exe: &str) {
    let _ = tmux(&["set-option", "-u", "-t", TUI_SESSION, "key-table"]);
    let _ = tmux(&["unbind-key", "-T", STALE_KEY_TABLE, BROWSER_KEY]);

    let cond = format!("#{{==:#{{session_name}},{TUI_SESSION}}}");
    let toggle = format!("run-shell \"'{exe}' __browser-toggle >/dev/null\"");
    let passthrough = format!("send-keys {BROWSER_KEY}");
    let _ = tmux(&[
        "bind-key",
        "-n",
        BROWSER_KEY,
        "if-shell",
        "-F",
        &cond,
        &toggle,
        &passthrough,
    ]);
}

/// Whether the in-corc `C-b` binding can fire at all: a `C-b` prefix shadows
/// it, since tmux checks the prefix before the root table.
pub fn browser_key_is_reachable() -> bool {
    let prefix = tmux(&["show", "-gv", "prefix"]).unwrap_or_default();
    !prefix.trim().eq_ignore_ascii_case(BROWSER_KEY)
}

/// Undo the runtime bindings on quit, so once corc exits the tmux server
/// matches the user's config again: the plain Alt+number window switch goes
/// back, and `C-b` becomes unbound. A crash that skips this leaves the
/// conditional bindings in place — harmless, since outside corc they do what
/// the key did before (switch window, reach the application) and `corc jump`
/// and `corc browser` run headless regardless.
///
/// A user who had their own root `C-b` binding loses it until their config is
/// reloaded; `install_browser_binding` overwrote it on the way in either way.
pub fn restore_bindings() {
    for n in 1..=9u8 {
        let key = format!("M-{n}");
        let idx = n.to_string();
        let _ = tmux(&["bind-key", "-n", &key, "select-window", "-t", &idx]);
    }
    let _ = tmux(&["unbind-key", "-n", BROWSER_KEY]);
}

/// Digit jump (D13): take the client to window `n` of `dir`'s real session,
/// creating the session (with its `.tmux.sh` hook) and window as needed.
/// Window 1 is the editor window: created running nvim, and an idle shell
/// there gets `nvim` typed into it — but a busy foreground process is never
/// disturbed, just focused. Shared by the sidebar's `1`-`9` and the headless
/// `corc jump N` that a tmux binding runs from inside the Claude pane.
pub fn jump_to_window(dir: &Path, n: u8, label: &str) -> Result<()> {
    let (session, created) = ensure_session(dir, label)?;
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
pub fn jump_to_session(dir: &Path, label: &str) -> Result<()> {
    let (session, _) = ensure_session(dir, label)?;
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
