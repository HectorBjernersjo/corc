//! Repo layout: what a directory's project label is, which sibling checkouts a
//! repo directory expands to, and which checkouts live under a scan root. Git
//! worktrees and jj workspaces are handled symmetrically — the rest of corc
//! only cares that a checkout may have a parent repo and siblings, never which
//! VCS provides them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Project label (D8): the directory basename, or `{repo}/{checkout}` when the
/// directory is a git worktree or a secondary jj workspace — e.g. `corc/fix-ui`.
/// Branches and jj workspace names are never shown; the directory name is.
pub fn project_display(path: &str) -> String {
    let dir = Path::new(path);
    let base = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    match parent_repo(dir) {
        Some(repo) => format!("{repo}/{base}"),
        None => base,
    }
}

/// The main repo's basename when `dir` is a secondary checkout, else None.
/// Filesystem reads only, no subprocess: this runs on every sidebar frame and
/// once per entry in the sessionizer's list.
///
/// A git worktree's `.git` is a *file* `gitdir: <repo>/.git/worktrees/<name>`;
/// a secondary jj workspace's `.jj/repo` is a *file* holding the path to the
/// main workspace's `.jj/repo`, usually relative (`../../myrepo/.jj/repo`).
/// A main checkout has both as directories, and yields None.
pub fn parent_repo(dir: &Path) -> Option<String> {
    git_worktree_repo(dir).or_else(|| jj_workspace_repo(dir))
}

fn git_worktree_repo(dir: &Path) -> Option<String> {
    let pointer = pointer_file(&dir.join(".git"))?;
    let gitdir = pointer.strip_prefix("gitdir:")?.trim();
    let (repo, _) = gitdir.split_once("/.git/worktrees/")?;
    basename(repo)
}

fn jj_workspace_repo(dir: &Path) -> Option<String> {
    let pointer = pointer_file(&dir.join(".jj/repo"))?;
    basename(pointer.strip_suffix("/.jj/repo")?)
}

/// The trimmed contents of `path` when it is a regular file — the shape both
/// VCSs use to point a secondary checkout at its repo. None for a directory,
/// which is what the main checkout has there.
fn pointer_file(path: &Path) -> Option<String> {
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    Some(std::fs::read_to_string(path).ok()?.trim().to_string())
}

fn basename(path: &str) -> Option<String> {
    Some(Path::new(path).file_name()?.to_string_lossy().into_owned())
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
    use super::{Expansions, project_display, scan_checkouts};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    /// D8: basename for plain dirs and main checkouts, `{repo}/{checkout}` for
    /// git worktrees and secondary jj workspaces alike.
    #[test]
    fn project_headers() {
        let base = std::env::temp_dir().join("corc-test-project-display");
        let _ = fs::remove_dir_all(&base);

        let plain = base.join("myproj");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(project_display(&plain.to_string_lossy()), "myproj");

        // A normal repo has a .git *directory*, and a main jj workspace a
        // .jj/repo *directory* — still basename only.
        fs::create_dir_all(plain.join(".git")).unwrap();
        fs::create_dir_all(plain.join(".jj/repo")).unwrap();
        assert_eq!(project_display(&plain.to_string_lossy()), "myproj");

        // A git worktree has a .git *file* with a gitdir: pointer.
        let wt = base.join("fix-ui");
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join(".git"),
            "gitdir: /home/hector/Projects/corc/.git/worktrees/fix-ui\n",
        )
        .unwrap();
        assert_eq!(project_display(&wt.to_string_lossy()), "corc/fix-ui");

        // A secondary jj workspace has a .jj/repo *file* pointing — relatively,
        // as `jj workspace add` writes it — at the main workspace's repo.
        let ws = base.join("fix-jj");
        fs::create_dir_all(ws.join(".jj")).unwrap();
        fs::write(ws.join(".jj/repo"), "../../corc/.jj/repo").unwrap();
        assert_eq!(project_display(&ws.to_string_lossy()), "corc/fix-jj");

        let _ = fs::remove_dir_all(&base);
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
            // The worktree is labelled by the repo it belongs to.
            assert_eq!(project_display(path(&wt)), "gitrepo/gitrepo-fix");

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
            assert_eq!(project_display(path(&ws)), "jjrepo/jjrepo-fix");
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
