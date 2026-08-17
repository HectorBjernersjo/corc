//! Repo layout: what a set of project directories is labelled, which sibling
//! checkouts a repo directory expands to, and which checkouts live under a
//! scan root. Git worktrees and jj workspaces are handled symmetrically — the
//! rest of corc only cares that a checkout may have siblings, never which VCS
//! provides them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Project labels (D8): each path's basename, grown one directory to the left
/// at a time until it tells that path apart from every other in the set.
/// `~/projects/corc` stays `corc`; two repos' `main` worktrees become
/// `gbandit/main` and `work/main`; a lone `main` stays `main`.
///
/// The label is derived from the set, never from the VCS. A git worktree and a
/// jj workspace need no detection at all, because the thing that actually
/// distinguishes two checkouts sharing a basename is where they are. The path
/// is the identity everywhere in corc — state groups conversations by exact
/// cwd, tmux sessions are looked up by their directory — so a set of distinct
/// paths always yields distinct labels, and a label is safe to show, and safe
/// to change, without anything being lost by it.
pub fn labels(paths: &[String]) -> Vec<String> {
    let parts: Vec<Vec<&str>> = paths.iter().map(|p| components(p)).collect();
    let mut depth = vec![1usize; paths.len()];
    // Grow every label that still collides, and keep going until a pass
    // changes nothing. Paths that are genuinely equal run out of components
    // together, which is what ends the loop rather than a depth cap.
    loop {
        let mut groups: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();
        for (i, part) in parts.iter().enumerate() {
            groups.entry(suffix(part, depth[i])).or_default().push(i);
        }
        let mut grew = false;
        for group in groups.into_values().filter(|g| g.len() > 1) {
            for i in group {
                if depth[i] < parts[i].len() {
                    depth[i] += 1;
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    parts
        .iter()
        .zip(&depth)
        .zip(paths)
        .map(|((part, &d), path)| match suffix(part, d) {
            // A path with no components at all (`/`) has no suffix to show.
            s if s.is_empty() => path.clone(),
            s => s,
        })
        .collect()
}

/// One path's label within `set` — [`labels`] applied to the set, picking out
/// `path`. A path the set does not contain falls back to its basename: the
/// sessionizer's path mode can name a directory that is in no list yet.
pub fn label_for(path: &str, set: &[String]) -> String {
    match set.iter().position(|p| p == path) {
        Some(i) => labels(set).swap_remove(i),
        None => match suffix(&components(path), 1) {
            s if s.is_empty() => path.to_string(),
            s => s,
        },
    }
}

fn components(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// The last `n` components of a split path, joined — the label at depth `n`.
fn suffix(parts: &[&str], n: usize) -> String {
    parts[parts.len().saturating_sub(n)..].join("/")
}

/// How deep below a scan root a checkout is still found. `~/projects/*` has to
/// reach `~/projects/gbandit/main`, so one level is not enough; the walk stops
/// at every checkout it meets, so a deeper limit costs little.
const SCAN_MAX_DEPTH: usize = 3;

/// True when `dir` is a checkout of either VCS — the leaf the scan looks for,
/// and the point it stops descending at. A worktree/workspace qualifies on the
/// same footing as a main checkout: both carry `.git` / `.jj`, file or dir.
pub fn is_checkout(dir: &Path) -> bool {
    dir.join(".git").exists() || dir.join(".jj").exists()
}

/// Every checkout under `root`, most recently modified first — what a `/*`
/// entry in the directory list expands to. Directories that are not checkouts
/// are containers to look inside, never results of their own, and a checkout is
/// never descended into: its worktrees live beside it, and its `node_modules`
/// is not a project. Dotted directories and symlinks are skipped; a missing or
/// unreadable root yields nothing.
///
/// Because a git worktree and a jj workspace are ordinary directories with a
/// `.git`/`.jj` pointer, the walk finds them itself — a scanned root needs no
/// `checkouts()` expansion and so spawns no `git`/`jj` process at all.
pub fn scan_checkouts(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    scan_into(root, SCAN_MAX_DEPTH, &mut found);
    // Most recently touched project first, the way the sessionizer already
    // orders sessions by last attach; path breaks ties so the order is stable.
    found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    found.into_iter().map(|(path, _)| path).collect()
}

fn scan_into(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, SystemTime)>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // `file_type` does not follow symlinks, so a symlinked directory is not
        // a dir here — deliberately: following them duplicates checkouts under
        // a second path the pickers would show twice.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if is_checkout(&path) {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((path, mtime));
        } else {
            scan_into(&path, depth - 1, out);
        }
    }
}

/// Expanding one directory list, with each repo asked at most once.
///
/// Expansion is dominated by process startup — ~2 ms for `git`, ~9 ms for `jj`
/// even when it only reports "not a repo" — so the two things worth avoiding
/// are asking a VCS about a directory that cannot be its business, and asking
/// it twice about the same repo. Both are decided from the filesystem: each VCS
/// finds its repo by walking up for a marker (`.git` / `.jj`), and this walks up
/// the same way for a handful of `stat`s. A directory list holds plain folders
/// (`~/Downloads`), several paths inside one repo (`~/dotfiles`,
/// `~/dotfiles/.config/nvim`) and repos of one VCS only — all of which land
/// here as cache hits or skipped calls.
///
/// One instance covers one listing; nothing is invalidated, so do not keep it
/// across user actions that could create a worktree.
#[derive(Default)]
pub struct Expansions {
    git: std::collections::HashMap<PathBuf, Vec<String>>,
    jj: std::collections::HashMap<PathBuf, Vec<String>>,
}

impl Expansions {
    /// Every checkout of the repo containing `dir`: its git worktrees and its
    /// jj workspaces, absolute paths, in each VCS's own order. A colocated repo
    /// reports the same root twice — callers dedupe. Empty outside a repo, so
    /// the picker simply keeps the directory itself.
    pub fn checkouts(&mut self, dir: &str) -> Vec<String> {
        let path = Path::new(dir);
        let mut out = Vec::new();
        // A bare repo has no marker to key on: it is one call, uncached.
        match marker_root(path, ".git") {
            Some(root) => out.extend(
                self.git
                    .entry(root)
                    .or_insert_with(|| git_checkouts(dir))
                    .clone(),
            ),
            None if is_bare_git(path) => out.extend(git_checkouts(dir)),
            None => {}
        }
        if let Some(root) = marker_root(path, ".jj") {
            out.extend(
                self.jj
                    .entry(root)
                    .or_insert_with(|| jj_workspaces(dir))
                    .clone(),
            );
        }
        out
    }
}

/// The nearest ancestor of `dir` — itself included — carrying `marker`, i.e.
/// the repo that VCS would resolve `dir` to. None when it has no business here.
fn marker_root(dir: &Path, marker: &str) -> Option<PathBuf> {
    dir.ancestors()
        .find(|a| a.join(marker).exists())
        .map(Path::to_path_buf)
}

/// A bare repo has no `.git` anywhere: it *is* the git directory, recognised by
/// its `HEAD` file and `objects/` — the layout `git_repo_root` falls back to.
fn is_bare_git(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir()
}

/// The git half of an expansion: the worktrees of the repo `dir` resolves to.
fn git_checkouts(dir: &str) -> Vec<String> {
    git_repo_root(dir)
        .map(|root| git_worktrees(&root))
        .unwrap_or_default()
}

/// A bare repo has no `.git` anywhere: it *is* the git directory, recognised
/// by its `HEAD` file and `objects/` — the layout `git_repo_root` falls back
/// to, so the gate has to let it through.
/// The repo root to expand git worktrees from: `rev-parse --show-toplevel` for
/// a normal checkout, or the directory itself when it is a bare repo — matching
/// new.sh's worktree handling. None outside a git repo.
fn git_repo_root(dir: &str) -> Option<String> {
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
fn git_worktrees(repo_root: &str) -> Vec<String> {
    lines_of(
        Command::new("git")
            .args(["-C", repo_root, "worktree", "list", "--porcelain"])
            .output(),
    )
    .filter_map(|l| l.strip_prefix("worktree ").map(str::to_string))
    .collect::<Vec<_>>()
}

/// The absolute roots of every jj workspace of `dir`'s repo, listed from any
/// one of them. `--ignore-working-copy` is load-bearing: without it, merely
/// listing directories for the picker would snapshot the working copy — slow,
/// and a write to a repo the user did not ask corc to touch.
fn jj_workspaces(dir: &str) -> Vec<String> {
    lines_of(
        Command::new("jj")
            .args([
                "-R",
                dir,
                "workspace",
                "list",
                "--ignore-working-copy",
                "-T",
                r#"root ++ "\n""#,
            ])
            .output(),
    )
    .collect()
}

/// The non-empty stdout lines of a command that succeeded; nothing at all when
/// it failed or the binary is missing (jj is optional, git may be too).
fn lines_of(out: std::io::Result<std::process::Output>) -> impl Iterator<Item = String> {
    let text = match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => String::new(),
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>()
        .into_iter()
}

#[cfg(test)]
mod tests {
    use super::{Expansions, label_for, labels, scan_checkouts};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    /// D8: a label is the shortest trailing path that is unique in the set.
    /// Nothing here touches the filesystem or a VCS — the paths are the whole
    /// input, which is the point of deriving labels from the set.
    #[test]
    fn labels_grow_until_unique() {
        let of = |paths: &[&str]| labels(&paths.iter().map(|s| s.to_string()).collect::<Vec<_>>());

        // Unique basenames stay basenames — including a lone `main`, which is
        // uninformative but unambiguous, and that is the bar.
        assert_eq!(
            of(&["/home/h/projects/corc", "/home/h/projects/gbandit/main"]),
            ["corc", "main"]
        );

        // A collision grows both sides, and only far enough to separate them.
        // The worktree case falls out of this with no VCS detection at all.
        assert_eq!(
            of(&["/home/h/projects/gbandit/main", "/home/h/work/main"]),
            ["gbandit/main", "work/main"]
        );

        // Three-way, uneven: two need a third component, the rest stop early.
        assert_eq!(
            of(&[
                "/home/h/projects/gbandit/shadcn",
                "/home/h/.local/share/Trash/files/gbandit-before-monorepo/shadcn",
                "/home/h/projects/corc",
            ]),
            ["gbandit/shadcn", "gbandit-before-monorepo/shadcn", "corc"]
        );

        // One path a suffix of another: the shorter one runs out of components
        // and the longer keeps growing, so the loop still terminates.
        assert_eq!(of(&["/a/main", "/x/a/main"]), ["a/main", "x/a/main"]);

        // Genuinely equal paths cannot be told apart; both max out and stop.
        assert_eq!(of(&["/a/b", "/a/b"]), ["a/b", "a/b"]);

        // Degenerate paths are returned as-is rather than as an empty label.
        assert_eq!(of(&["/"]), ["/"]);

        // `label_for` picks one out of the set, and falls back to the basename
        // for a path the set never had — the sessionizer's path mode.
        let set: Vec<String> = ["/home/h/projects/gbandit/main", "/home/h/work/main"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(label_for("/home/h/work/main", &set), "work/main");
        assert_eq!(label_for("/home/h/elsewhere/solo", &set), "solo");
    }

    /// The real thing: create a git repo with a worktree and a jj repo with a
    /// second workspace, and check that each expands to both of its checkouts
    /// from either side — the expansion the `N` picker and `corc projects`
    /// list directories with. Skipped when the VCS isn't installed.
    #[test]
    fn checkouts_expand_git_worktrees_and_jj_workspaces() {
        let base = std::env::temp_dir().join("corc-test-checkouts");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();

        if have("git") {
            let main = base.join("gitrepo");
            let wt = base.join("gitrepo-fix");
            fs::create_dir_all(&main).unwrap();
            run("git", &["init", "-b", "main", path(&main)]);
            run(
                "git",
                &[
                    "-C",
                    path(&main),
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "commit",
                    "--allow-empty",
                    "-m",
                    "init",
                ],
            );
            run("git", &["-C", path(&main), "worktree", "add", path(&wt)]);

            for from in [&main, &wt] {
                let found = Expansions::default().checkouts(path(from));
                assert!(found.contains(&path(&main).to_string()), "{found:?}");
                assert!(found.contains(&path(&wt).to_string()), "{found:?}");
            }
            // A bare repo has no `.git` marker at all — it is the git dir —
            // and still has to expand, which is what its `HEAD` + `objects/`
            // let the cheap "is this git's business?" gate see.
            let bare = base.join("bare.git");
            run("git", &["init", "--bare", path(&bare)]);
            assert_eq!(
                Expansions::default().checkouts(path(&bare)),
                vec![path(&bare).to_string()]
            );
        }

        if have("jj") {
            let main = base.join("jjrepo");
            let ws = base.join("jjrepo-fix");
            fs::create_dir_all(&main).unwrap();
            run("jj", &["git", "init", path(&main)]);
            run("jj", &["-R", path(&main), "workspace", "add", path(&ws)]);

            for from in [&main, &ws] {
                let found = Expansions::default().checkouts(path(from));
                assert!(found.contains(&path(&main).to_string()), "{found:?}");
                assert!(found.contains(&path(&ws).to_string()), "{found:?}");
            }
        }

        let _ = fs::remove_dir_all(&base);
    }

    /// A `~/projects/*` scan root: every checkout below it, containers looked
    /// through, checkouts never descended into, hidden dirs and symlinks and
    /// anything past the depth limit left out — and the freshest one first.
    #[test]
    fn scan_finds_every_checkout_under_a_root_and_nothing_else() {
        let base = std::env::temp_dir().join("corc-test-scan");
        let _ = fs::remove_dir_all(&base);
        let root = base.join("projects");

        // A main checkout of each VCS, straight under the root.
        fs::create_dir_all(root.join("corc/.git")).unwrap();
        fs::create_dir_all(root.join("quim/.jj/repo")).unwrap();
        // A container directory: not a result itself, but looked inside.
        fs::create_dir_all(root.join("gbandit/main/.git")).unwrap();
        // A worktree of that repo, beside it — a plain dir with a .git file.
        fs::create_dir_all(root.join("gbandit/fix-ui")).unwrap();
        fs::write(root.join("gbandit/fix-ui/.git"), "gitdir: /elsewhere\n").unwrap();
        // Nested repos inside a checkout are vendored, not projects.
        fs::create_dir_all(root.join("corc/vendor/dep/.git")).unwrap();
        // Not a checkout at all, and nothing below it either.
        fs::create_dir_all(root.join("scratch/notes")).unwrap();
        // Hidden, and too deep (depth 4) — both out.
        fs::create_dir_all(root.join(".cache/repo/.git")).unwrap();
        fs::create_dir_all(root.join("a/b/c/deep/.git")).unwrap();
        // A symlink to a checkout would list it twice under two paths.
        std::os::unix::fs::symlink(root.join("corc"), root.join("corc-link")).unwrap();

        let found: Vec<String> = scan_checkouts(&root)
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        let mut sorted = found.clone();
        sorted.sort();
        assert_eq!(sorted, ["corc", "gbandit/fix-ui", "gbandit/main", "quim"]);

        // Touching a checkout floats it to the top of the list. Age every
        // checkout first: directory mtimes come from the kernel's coarse clock,
        // which can tick slower than this test runs, so without this the write
        // below is not guaranteed to look newer than the setup above.
        for checkout in &found {
            age(&root.join(checkout));
        }
        fs::write(root.join("quim/touched"), "").unwrap();
        assert_eq!(scan_checkouts(&root).first().unwrap(), &root.join("quim"));

        // A root that does not exist is simply empty, not an error.
        assert!(scan_checkouts(&base.join("nope")).is_empty());

        let _ = fs::remove_dir_all(&base);
    }

    fn have(bin: &str) -> bool {
        Command::new(bin)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Backdate a directory a minute, so a following write to it is
    /// unambiguously newer however coarse the filesystem clock is.
    fn age(dir: &Path) {
        let minute_ago = SystemTime::now() - Duration::from_secs(60);
        fs::File::open(dir)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(minute_ago))
            .unwrap();
    }

    fn path(p: &PathBuf) -> &str {
        Path::new(p).to_str().unwrap()
    }

    fn run(bin: &str, args: &[&str]) {
        let out = Command::new(bin)
            .args(args)
            .env("JJ_USER", "t")
            .env("JJ_EMAIL", "t@example.com")
            .output()
            .unwrap_or_else(|e| panic!("{bin} {args:?}: {e}"));
        assert!(
            out.status.success(),
            "{bin} {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
