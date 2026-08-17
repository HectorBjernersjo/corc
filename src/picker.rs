//! Directory source for the `N` picker and the `corc projects` sessionizer
//! (D14, D20). Two sources are merged: the hand-curated, dotfile-synced list
//! in `~/.config/corc/directories.txt`, and the machine-local list kept in
//! `state.json`. An entry ending in `/*` is a scan root and stands for every
//! checkout under it (`repo::scan_checkouts`); any other entry is one directory
//! plus its repo's other checkouts (`repo::Expansions`). Everything is deduped
//! in order. The picker shows directories only; multiple conversations per
//! project is normal.

use crate::repo;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Path to the shared, hand-curated directory list (dotfile-synced, D20).
fn config_directories_file() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config/corc/directories.txt"))
}

/// The lines of the shared directory list, minus blanks and `#` comments. A
/// missing file is treated as empty rather than an error — the local list may
/// be enough.
fn config_directories() -> Vec<String> {
    let text = config_directories_file()
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
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
/// machine-local `local` entries, in order. A `~/dir/*` entry expands to every
/// checkout under it, most recently modified first; any other entry is itself
/// plus its other checkouts — git worktrees and jj workspaces. Deduped,
/// `/.git/` internals dropped, non-directories dropped.
pub fn list_directories(local: &[String]) -> Result<Vec<PathBuf>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    // One VCS expansion per repo for the whole listing, not one per entry.
    let mut expansions = repo::Expansions::default();
    for dir in config_directories().iter().chain(local) {
        let dir = expand_tilde(dir.trim());
        if dir.is_empty() {
            continue;
        }
        let candidates = match dir.strip_suffix("/*") {
            // A scanned checkout needs no VCS expansion: its siblings live
            // under the same root and the walk reaches them on its own.
            Some(root) => repo::scan_checkouts(Path::new(root))
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            None => {
                let mut candidates = vec![dir.clone()];
                candidates.extend(expansions.checkouts(&dir));
                candidates
            }
        };
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

/// A fuzzy hit: the match score and the char positions in `hay` it consumed.
pub struct FuzzyMatch {
    /// Higher is a better match.
    pub score: i32,
    /// Char indices into `hay` (ascending) that the query matched — what the
    /// picker highlights, the way Telescope/snacks do.
    pub indices: Vec<usize>,
}

/// Fuzzy subsequence match used by the directory/session picker: every
/// non-space character of `query` must appear in `hay` in order,
/// case-insensitively (so "pr4" matches "pr-4"). Returns the score and the
/// matched positions, or None when it doesn't match at all. The score rewards
/// consecutive characters and matches at word boundaries (start, or after
/// `-_/. ` or a case bump) so the tightest hit ranks first. An empty query
/// matches everything with a neutral score and no highlights, preserving
/// source order.
pub fn fuzzy_match(query: &str, hay: &str) -> Option<FuzzyMatch> {
    let q: Vec<char> = query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if q.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            indices: Vec::new(),
        });
    }
    let raw: Vec<char> = hay.chars().collect();
    let lower: Vec<char> = hay.to_lowercase().chars().collect();

    let mut qi = 0;
    let mut score = 0i32;
    let mut prev: Option<usize> = None;
    let mut indices = Vec::with_capacity(q.len());
    for hi in 0..lower.len() {
        if qi >= q.len() || lower[hi] != q[qi] {
            continue;
        }
        score += 1;
        if prev == Some(hi.wrapping_sub(1)) {
            score += 6; // consecutive run
        }
        let boundary = hi == 0
            || matches!(raw[hi - 1], '-' | '_' | '/' | '.' | ' ')
            || (raw[hi - 1].is_lowercase() && raw[hi].is_uppercase());
        if boundary {
            score += 4;
        }
        indices.push(hi);
        prev = Some(hi);
        qi += 1;
    }
    // Shorter haystacks with the same coverage read as tighter matches.
    (qi == q.len()).then(|| FuzzyMatch {
        score: score - (lower.len() as i32) / 32,
        indices,
    })
}

#[cfg(test)]
mod tests {
    use super::{fuzzy_match, matches_words, split_parent_prefix};
    use std::path::PathBuf;

    /// Convenience: the score of a match, or None when it doesn't match.
    fn fuzzy_score(query: &str, hay: &str) -> Option<i32> {
        fuzzy_match(query, hay).map(|m| m.score)
    }

    /// Word-substring semantics shared with the `/` filter.
    #[test]
    fn word_matching() {
        assert!(matches_words("", "anything"));
        assert!(matches_words("orc", "~/Projects/corc"));
        assert!(matches_words("proj orc", "~/Projects/corc"));
        assert!(matches_words("ORC", "~/projects/corc"));
        assert!(!matches_words("proj xyz", "~/Projects/corc"));
    }

    /// Fuzzy subsequence matching used by the directory picker: gaps and
    /// separators are skipped, so "pr4" matches "pr-4".
    #[test]
    fn fuzzy_matching() {
        assert!(fuzzy_score("pr4", "pr-4").is_some());
        assert!(fuzzy_score("pr4", "pr-1").is_none()); // no '4'
        assert!(fuzzy_score("PR4", "pr-4").is_some()); // case-insensitive
        assert!(fuzzy_score("", "anything").is_some()); // empty matches all
        assert!(fuzzy_score("xyz", "pr-4").is_none()); // out of order / absent

        // A contiguous, boundary-aligned hit outranks a scattered one.
        let tight = fuzzy_score("pr4", "pr-4").unwrap();
        let loose = fuzzy_score("pr4", "parser/output4").unwrap();
        assert!(tight > loose, "tight {tight} should beat loose {loose}");
    }

    /// The matched positions returned for highlighting are the char indices in
    /// the haystack the query consumed, in order — skipping separators.
    #[test]
    fn fuzzy_indices() {
        // "pr4" in "pr-4": p@0, r@1, 4@3 (the '-' at 2 is skipped).
        assert_eq!(fuzzy_match("pr4", "pr-4").unwrap().indices, vec![0, 1, 3]);
        // An empty query highlights nothing.
        assert!(fuzzy_match("", "anything").unwrap().indices.is_empty());
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
