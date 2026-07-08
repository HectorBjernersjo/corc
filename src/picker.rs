//! Directory source for the `n` picker overlay (D14): the same source and
//! expansion as new.sh, ported to Rust — `~/.config/tmux/directories.txt`,
//! each entry expanded with its repo's `git worktree list --porcelain`,
//! deduped in order. The picker shows directories only; unlike new.sh no
//! session filtering happens (multiple conversations per project is normal).

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

/// The candidate directories, in directories.txt order, each followed by its
/// git worktrees, deduped, `/.git/` internals dropped, non-directories dropped.
pub fn list_directories() -> Result<Vec<PathBuf>> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let file = PathBuf::from(home).join(".config/tmux/directories.txt");
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("directory list file not found: {}", file.display()))?;

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let dir = line.trim();
        if dir.is_empty() {
            continue;
        }
        let mut candidates = vec![dir.to_string()];
        if let Some(root) = git_toplevel(dir) {
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

/// `git -C dir rev-parse --show-toplevel`, None outside a repo.
fn git_toplevel(dir: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", dir, "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!root.is_empty()).then_some(root)
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
    use super::matches_words;

    /// Word-substring semantics shared with the `/` filter.
    #[test]
    fn word_matching() {
        assert!(matches_words("", "anything"));
        assert!(matches_words("orc", "~/Projects/orcim"));
        assert!(matches_words("proj orc", "~/Projects/orcim"));
        assert!(matches_words("ORC", "~/projects/orcim"));
        assert!(!matches_words("proj xyz", "~/Projects/orcim"));
    }
}
