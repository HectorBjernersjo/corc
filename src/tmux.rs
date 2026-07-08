use crate::procs;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Pane {
    pub session: String,
    pub window_index: u32,
    pub window_name: String,
    pub pane_id: String,
    pub pid: u32,
}

impl Pane {
    pub fn target(&self) -> String {
        format!("{}:{}", self.session, self.window_index)
    }
}

/// All panes across every tmux session; empty if tmux isn't running.
pub fn list_panes() -> Vec<Pane> {
    let Ok(output) = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{window_index}\t#{window_name}\t#{pane_id}\t#{pane_pid}",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            Some(Pane {
                session: parts.next()?.to_string(),
                window_index: parts.next()?.parse().ok()?,
                window_name: parts.next()?.to_string(),
                pane_id: parts.next()?.to_string(),
                pid: parts.next()?.parse().ok()?,
            })
        })
        .collect()
}

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

/// Bring an existing pane into view and switch the client to it.
pub fn focus(pane: &Pane) -> Result<()> {
    tmux(&["select-window", "-t", &pane.target()])?;
    tmux(&["select-pane", "-t", &pane.pane_id])?;
    tmux(&["switch-client", "-t", &pane.session])?;
    Ok(())
}

/// Session naming convention from new.sh: basename with '.' → '_'.
pub fn session_name_for(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().replace('.', "_"))
        .unwrap_or_else(|| "orcim-unknown".to_string())
}

fn session_exists(name: &str) -> bool {
    tmux(&["has-session", "-t", &format!("={name}")]).is_ok()
}

/// Create a detached session, honoring the per-project .tmux.sh hook just
/// like new.sh does.
fn create_session(name: &str, dir: &Path) -> Result<()> {
    tmux(&["new-session", "-d", "-s", name, "-c", &dir.to_string_lossy()])?;
    let hook = dir.join(".tmux.sh");
    if is_executable(&hook) {
        let _ = Command::new(&hook).arg(name).arg(dir).status();
    }
    Ok(())
}

/// Find or create the tmux session for a directory.
fn ensure_session(dir: &Path) -> Result<String> {
    let name = session_name_for(dir);
    if !session_exists(&name) {
        create_session(&name, dir)?;
    }
    Ok(name)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Open a new window in the directory's session (without focusing it) and
/// type `command` into it. The window keeps its shell so it survives claude
/// exiting. Returns (pane_id, session:window target).
pub fn spawn_window(dir: &Path, command: &str) -> Result<(String, String)> {
    if !dir.is_dir() {
        bail!("directory {} no longer exists", dir.display());
    }
    let name = ensure_session(dir)?;
    let dir_str = dir.to_string_lossy();
    let out = tmux(&[
        "new-window",
        "-d",
        "-t",
        &format!("{name}:"),
        "-c",
        &dir_str,
        "-P",
        "-F",
        "#{pane_id}\t#{session_name}:#{window_index}",
    ])?;
    let (pane_id, target) = out
        .trim()
        .split_once('\t')
        .context("unexpected new-window output")?;
    tmux(&["send-keys", "-t", pane_id, command, "Enter"])?;
    Ok((pane_id.to_string(), target.to_string()))
}

/// Open a new window in the directory's session, type `command` and switch
/// the client there.
pub fn open_in_new_window(dir: &Path, command: &str) -> Result<()> {
    let (_, target) = spawn_window(dir, command)?;
    tmux(&["switch-client", "-t", &target])?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct PaneHome {
    pub session: String,
    pub window_id: String,
    pub dir: String,
}

pub fn pane_home(pane_id: &str) -> Result<PaneHome> {
    let out = tmux(&[
        "display-message",
        "-p",
        "-t",
        pane_id,
        "#{session_name}\t#{window_id}\t#{pane_current_path}",
    ])?;
    let mut parts = out.trim().split('\t');
    let (Some(session), Some(window_id), Some(dir)) =
        (parts.next(), parts.next(), parts.next())
    else {
        bail!("unexpected display-message output");
    };
    Ok(PaneHome {
        session: session.to_string(),
        window_id: window_id.to_string(),
        dir: dir.to_string(),
    })
}

pub fn pane_exists(pane_id: &str) -> bool {
    tmux(&["list-panes", "-a", "-F", "#{pane_id}"])
        .map(|out| out.lines().any(|l| l == pane_id))
        .unwrap_or(false)
}

fn window_exists(window_id: &str) -> bool {
    tmux(&["list-windows", "-a", "-F", "#{window_id}"])
        .map(|out| out.lines().any(|l| l == window_id))
        .unwrap_or(false)
}

pub fn select_pane(pane_id: &str) -> Result<()> {
    tmux(&["select-pane", "-t", pane_id])?;
    Ok(())
}

/// Switch the client to a pane wherever it currently lives.
pub fn focus_pane(pane_id: &str) -> Result<()> {
    let home = pane_home(pane_id)?;
    tmux(&["select-window", "-t", pane_id])?;
    tmux(&["select-pane", "-t", pane_id])?;
    tmux(&["switch-client", "-t", &home.session])?;
    Ok(())
}

/// Move a pane in next to the sidebar pane and focus it.
pub fn embed(src_pane: &str, sidebar_pane: &str) -> Result<()> {
    tmux(&["join-pane", "-h", "-s", src_pane, "-t", sidebar_pane])?;
    let _ = tmux(&["resize-pane", "-t", sidebar_pane, "-x", "35%"]);
    tmux(&["select-pane", "-t", src_pane])?;
    Ok(())
}

/// Send an embedded pane back to where it came from. Joining a pane out of
/// a session can have destroyed its window or even the whole session (if the
/// pane was the only one there), so recreate what's missing.
pub fn unembed(pane_id: &str, home: &PaneHome) -> Result<()> {
    if !pane_exists(pane_id) {
        return Ok(());
    }
    if window_exists(&home.window_id) {
        tmux(&["join-pane", "-d", "-s", pane_id, "-t", &home.window_id])?;
        let _ = tmux(&["select-layout", "-t", &home.window_id, "-E"]);
        return Ok(());
    }
    if !session_exists(&home.session) {
        create_session(&home.session, Path::new(&home.dir))?;
    }
    tmux(&[
        "break-pane",
        "-d",
        "-s",
        pane_id,
        "-t",
        &format!("{}:", home.session),
    ])?;
    Ok(())
}

/// Find the pane a process lives in by walking up its parent chain until we
/// hit a pane's root shell.
pub fn pane_for_pid(panes: &[Pane], pid: u32) -> Option<&Pane> {
    let by_pid: HashMap<u32, &Pane> = panes.iter().map(|p| (p.pid, p)).collect();
    let mut current = pid;
    for _ in 0..20 {
        if let Some(pane) = by_pid.get(&current) {
            return Some(pane);
        }
        current = procs::ppid_of(current)?;
        if current <= 1 {
            return None;
        }
    }
    None
}
