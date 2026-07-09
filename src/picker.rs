//! Directory source for the `N` picker and the `corc projects` sessionizer
//! (D14, D20). Two sources are merged: the hand-curated, dotfile-synced list
//! in `~/.config/corc/directories.txt`, and the machine-local list kept in
//! `state.json`. Each entry is expanded with its repo's
//! `git worktree list --porcelain`, deduped in order. The picker shows
//! directories only; multiple conversations per project is normal.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Path to the shared, hand-curated directory list (dotfile-synced, D20).
fn config_directories_file() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config/corc/directories.txt"))
}

/// The lines of the shared directory list. Falls back to the pre-D20 path
/// (`~/.config/tmux/directories.txt`, shared with the old new.sh) while the
/// file has not been migrated, so nothing is lost on upgrade. Missing files
/// are treated as empty rather than an error — the local list may be enough.
fn config_directories() -> Vec<String> {
    let read = |p: PathBuf| std::fs::read_to_string(p).ok();
    let text = config_directories_file()
        .ok()
        .and_then(read)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config/tmux/directories.txt"))
                .and_then(read)
        })
        .unwrap_or_default();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Expand a leading `~` to `$HOME`; other paths pass through unchanged.
pub fn expand_tilde(s: &str) -> String {
    let home = std::env::var("HOME").ok();
    match (s, home) {
        ("~", Some(home)) => home,
        (s, Some(home)) if s.starts_with("~/") => format!("{home}/{}", &s[2..]),
        _ => s.to_string(),
    }
}

/// Directory completions for a path being typed in the `p` overlay: the real
/// subdirectories of the input's parent whose name starts with its trailing
/// component (case-insensitive), sorted. A trailing `/` lists the whole dir.
pub fn complete_dirs(input: &str) -> Vec<PathBuf> {
    let expanded = expand_tilde(input);
    let (parent, prefix) = split_parent_prefix(&expanded);
    let prefix = prefix.to_lowercase();
    let mut out: Vec<PathBuf> = std::fs::read_dir(&parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_lowercase().starts_with(&prefix))
        })
        .collect();
    out.sort();
    out
}

/// Split a path string into (parent directory, trailing component). A trailing
/// `/` means the whole thing is the directory and the prefix is empty.
fn split_parent_prefix(s: &str) -> (PathBuf, String) {
    if s.ends_with('/') {
        return (PathBuf::from(s), String::new());
    }
    let path = Path::new(s);
    let parent = path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let prefix = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    (parent, prefix)
}

/// The candidate directories: the shared list (D20) followed by the
/// machine-local `local` entries, in order, each entry followed by its git
/// worktrees, deduped, `/.git/` internals dropped, non-directories dropped.
pub fn list_directories(local: &[String]) -> Result<Vec<PathBuf>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for dir in config_directories().iter().chain(local) {
        let dir = dir.trim();
        if dir.is_empty() {
            continue;
        }
        let mut candidates = vec![dir.to_string()];
        if let Some(root) = repo_root(dir) {
            candidates.extend(worktrees(&root));
        }
        for cand in candidates {
            // new.sh: grep -v '/\.git/' — drop worktree entries inside .git.
            if cand.contains("/.git/") {
                continue;
            }
            let path = PathBuf::from(&cand);
            if path.is_dir() && seen.insert(cand) {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// The same word-substring matching as the sidebar `/` filter: every
/// whitespace-separated word of `filter` must appear in `hay`,
/// case-insensitively.
pub fn matches_words(filter: &str, hay: &str) -> bool {
    let hay = hay.to_lowercase();
    filter
        .to_lowercase()
        .split_whitespace()
        .all(|w| hay.contains(w))
}

/// The repo root to expand worktrees from: `rev-parse --show-toplevel` for a
/// normal checkout, or the directory itself when it is a bare repo — matching
/// new.sh's worktree handling. None outside a repo.
fn repo_root(dir: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", dir, "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if out.status.success() {
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !root.is_empty() {
            return Some(root);
        }
    }
    // A bare repo has no working tree, so --show-toplevel fails; expand its
    // worktrees from the bare directory itself (new.sh rad 21-23).
    let bare = Command::new("git")
        .args(["-C", dir, "rev-parse", "--is-bare-repository"])
        .output()
        .ok()?;
    (bare.status.success() && String::from_utf8_lossy(&bare.stdout).trim() == "true")
        .then(|| dir.to_string())
}

/// The `worktree <path>` lines of `git worktree list --porcelain`.
fn worktrees(repo_root: &str) -> Vec<String> {
    let Ok(out) = Command::new("git")
        .args(["-C", repo_root, "worktree", "list", "--porcelain"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{matches_words, split_parent_prefix};
    use std::path::PathBuf;

    /// Word-substring semantics shared with the `/` filter.
    #[test]
    fn word_matching() {
        assert!(matches_words("", "anything"));
        assert!(matches_words("orc", "~/Projects/corc"));
        assert!(matches_words("proj orc", "~/Projects/corc"));
        assert!(matches_words("ORC", "~/projects/corc"));
        assert!(!matches_words("proj xyz", "~/Projects/corc"));
    }

    /// A trailing `/` lists the whole directory; otherwise the last component
    /// is the prefix to complete against.
    #[test]
    fn parent_prefix_split() {
        assert_eq!(
            split_parent_prefix("/home/hector/"),
            (PathBuf::from("/home/hector/"), String::new())
        );
        assert_eq!(
            split_parent_prefix("/home/hector/Pro"),
            (PathBuf::from("/home/hector"), "Pro".to_string())
        );
        assert_eq!(
            split_parent_prefix("/"),
            (PathBuf::from("/"), String::new())
        );
    }
}
