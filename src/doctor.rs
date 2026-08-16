//! `corc doctor`: read-mostly diagnostics for the external pieces corc needs.
//! It checks tmux compatibility, agent binaries, PATH visibility and whether
//! the persistent state can be read and written.

use crate::{picker, provider, state, tmux};
use anyhow::{Result, bail};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

const MIN_TMUX: (u32, u32) = (3, 3);

pub fn run() -> Result<()> {
    println!("corc doctor\n");
    let mut errors = 0usize;
    let mut warnings = 0usize;

    check_tmux(&mut errors);
    check_path(&mut errors, &mut warnings);
    check_providers(&mut errors, &mut warnings);
    check_state(&mut errors);
    check_directories(&mut warnings);
    check_browser_view(&mut warnings);

    println!();
    if errors > 0 {
        bail!(
            "{errors} required check{} failed",
            if errors == 1 { "" } else { "s" }
        );
    }
    if warnings > 0 {
        println!(
            "[ok] required checks passed ({warnings} warning{})",
            if warnings == 1 { "" } else { "s" }
        );
    } else {
        println!("[ok] all checks passed");
    }
    Ok(())
}

/// The browser view (D24) has three external requirements, none of which corc
/// can fix on the user's behalf: a terminal that draws images, tmux forwarding
/// the escapes, and Playwright launching Chromium with a debugging port. All
/// three are warnings — corc works fine without the view — and the config file
/// is written here so the remedy is a single line to paste.
fn check_browser_view(warnings: &mut usize) {
    let terminal = tmux::client_terminal();
    if terminal.is_empty() {
        warn(
            "browser view",
            "no attached tmux client to ask about graphics",
        );
        *warnings += 1;
    } else if crate::kitty::terminal_supports_graphics(&terminal) {
        ok("browser view", &format!("{terminal} draws kitty graphics"));
    } else {
        warn(
            "browser view",
            &format!("{terminal} has no image support; the view will stay blank"),
        );
        *warnings += 1;
    }

    if tmux::passthrough_enabled() {
        ok("browser view", "tmux allow-passthrough is on");
    } else {
        warn(
            "browser view",
            "tmux allow-passthrough is off; add `set -g allow-passthrough on` to tmux.conf",
        );
        *warnings += 1;
    }

    // tmux resolves the prefix before the root table, so a C-b prefix quietly
    // swallows the in-corc toggle. Nothing is broken by it — the sidebar's `b`
    // and `corc browser` still work — so this only says what to expect.
    if tmux::browser_key_is_reachable() {
        ok("browser view", "Ctrl+b toggles the view inside corc");
    } else {
        warn(
            "browser view",
            "your tmux prefix is C-b, which shadows corc's Ctrl+b toggle; \
             use `b` in the sidebar or `!corc browser` in the agent",
        );
        *warnings += 1;
    }

    match crate::browser::ensure_config() {
        Ok(path) if crate::browser::config_is_wired() => {
            ok(
                "browser view",
                &format!("playwright loads {}", path.display()),
            );
        }
        Ok(path) => {
            warn(
                "browser view",
                &format!(
                    "playwright is not exposing a debugging port — add `--config {}` \
                     to its MCP server args",
                    path.display()
                ),
            );
            *warnings += 1;
        }
        Err(e) => {
            warn("browser view", &format!("could not write the config: {e}"));
            *warnings += 1;
        }
    }
}

fn check_tmux(errors: &mut usize) {
    match Command::new("tmux").arg("-V").output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            match parse_tmux_version(&text) {
                Some(version) if version >= MIN_TMUX => {
                    ok("tmux", &format!("{text} (popup support available)"));
                }
                Some(_) => {
                    error(
                        "tmux",
                        &format!(
                            "{text}; corc requires tmux {}.{}+ because it uses popup flags added in 3.3",
                            MIN_TMUX.0, MIN_TMUX.1
                        ),
                    );
                    *errors += 1;
                }
                None => {
                    error("tmux", &format!("could not parse version from {text:?}"));
                    *errors += 1;
                }
            }
        }
        Ok(out) => {
            let message = String::from_utf8_lossy(&out.stderr);
            error("tmux", message.trim());
            *errors += 1;
        }
        Err(e) => {
            error("tmux", &format!("not available: {e}"));
            *errors += 1;
        }
    }
}

fn check_path(errors: &mut usize, warnings: &mut usize) {
    let Some(path) = std::env::var_os("PATH") else {
        error("PATH", "not set");
        *errors += 1;
        return;
    };
    if path.is_empty() {
        error("PATH", "empty");
        *errors += 1;
        return;
    }

    match std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        Some(dir) if std::env::split_paths(&path).any(|entry| same_path(&entry, &dir)) => {
            ok(
                "PATH",
                &format!("corc's directory is present ({})", dir.display()),
            );
        }
        Some(dir) => {
            warn(
                "PATH",
                &format!(
                    "corc is running from {}, but that directory is not on PATH",
                    dir.display()
                ),
            );
            *warnings += 1;
        }
        None => {
            warn("PATH", "could not determine corc's executable directory");
            *warnings += 1;
        }
    }
}

fn check_providers(errors: &mut usize, warnings: &mut usize) {
    let mut available = 0usize;
    for provider in provider::all() {
        let name = provider.binary();
        let in_path = find_in_path(name);
        let resolved = in_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(tmux::resolve_binary(name)));
        if resolved.is_absolute() && tmux::is_executable(&resolved) {
            available += 1;
            let version = binary_version(&resolved)
                .map(|v| format!(" ({v})"))
                .unwrap_or_default();
            if in_path.is_some() {
                ok(
                    provider.display_name(),
                    &format!("{}{}", resolved.display(), version),
                );
            } else {
                warn(
                    provider.display_name(),
                    &format!(
                        "{}{}; found by the login shell but not the current PATH",
                        resolved.display(),
                        version
                    ),
                );
                *warnings += 1;
            }
        } else {
            warn(
                provider.display_name(),
                &format!("{name} was not found or is not executable"),
            );
            *warnings += 1;
        }
    }
    if available == 0 {
        error("agents", "no supported agent CLI is available");
        *errors += 1;
    }
}

fn check_state(errors: &mut usize) {
    let path = match state::state_file() {
        Ok(path) => path,
        Err(e) => {
            error("state", &e.to_string());
            *errors += 1;
            return;
        }
    };

    if let Err(e) = state::State::load() {
        error("state read", &format!("{}: {e}", path.display()));
        *errors += 1;
        return;
    }

    // State::save writes a sibling temp file and renames it into place, so
    // directory write access matters even when state.json itself is readable.
    let Some(dir) = path.parent() else {
        error("state write", "state path has no parent directory");
        *errors += 1;
        return;
    };
    let writable = fs::create_dir_all(dir).and_then(|_| {
        let probe = dir.join(format!(".doctor-{}.tmp", std::process::id()));
        let result = OpenOptions::new().write(true).create_new(true).open(&probe);
        if result.is_ok() {
            let _ = fs::remove_file(&probe);
        }
        result.map(|_| ())
    });
    match writable {
        Ok(()) => ok(
            "state",
            &format!("readable and writable ({})", path.display()),
        ),
        Err(e) => {
            error("state write", &format!("{}: {e}", path.display()));
            *errors += 1;
        }
    }
}

/// What the `N` picker and `corc projects` would list, and which machine-local
/// entries have gone stale — a moved or deleted directory is silently dropped
/// from the list, so this is the only place it shows up. Purely informational:
/// a stale entry costs nothing but noise in `state.json`.
fn check_directories(warnings: &mut usize) {
    let Ok(state) = state::State::load() else {
        return; // check_state already reported it.
    };
    let Ok(dirs) = picker::list_directories(&state.directories) else {
        return;
    };
    ok(
        "directories",
        &format!(
            "{} directories from {} local entr{}",
            dirs.len(),
            state.directories.len(),
            if state.directories.len() == 1 {
                "y"
            } else {
                "ies"
            }
        ),
    );

    let stale: Vec<&String> = state
        .directories
        .iter()
        .filter(|dir| {
            let expanded = picker::expand_tilde(dir.trim());
            let root = expanded.strip_suffix("/*").unwrap_or(&expanded);
            !Path::new(root).is_dir()
        })
        .collect();
    if !stale.is_empty() {
        let sample: Vec<&str> = stale.iter().take(3).map(|d| d.as_str()).collect();
        warn(
            "directories",
            &format!(
                "{} local entr{} no longer exist and are skipped: {}{}",
                stale.len(),
                if stale.len() == 1 { "y" } else { "ies" },
                sample.join(", "),
                if stale.len() > sample.len() {
                    ", …"
                } else {
                    ""
                }
            ),
        );
        *warnings += 1;
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|candidate| tmux::is_executable(candidate))
}

fn binary_version(binary: &Path) -> Option<String> {
    let out = Command::new(binary).arg("--version").output().ok()?;
    out.status
        .success()
        .then(|| first_nonempty_line(&String::from_utf8_lossy(&out.stdout)))
        .flatten()
}

fn first_nonempty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn parse_tmux_version(text: &str) -> Option<(u32, u32)> {
    let raw = text.strip_prefix("tmux ")?.trim();
    let mut numbers = raw.split('.');
    let major = numbers.next()?.parse().ok()?;
    let minor: String = numbers
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some((major, minor.parse().ok()?))
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn ok(check: &str, message: &str) {
    println!("[ok]   {check}: {message}");
}

fn warn(check: &str, message: &str) {
    println!("[warn] {check}: {message}");
}

fn error(check: &str, message: &str) {
    println!("[error] {check}: {message}");
}

#[cfg(test)]
mod tests {
    use super::{first_nonempty_line, parse_tmux_version};

    #[test]
    fn parses_tmux_versions_with_suffixes() {
        assert_eq!(parse_tmux_version("tmux 3.6a"), Some((3, 6)));
        assert_eq!(parse_tmux_version("tmux 3.3"), Some((3, 3)));
        assert_eq!(parse_tmux_version("tmux next-3.7"), None);
        assert_eq!(parse_tmux_version("garbage"), None);
    }

    #[test]
    fn picks_first_version_line() {
        assert_eq!(
            first_nonempty_line("\n2026.07.09-a3815c0\nmore"),
            Some("2026.07.09-a3815c0".to_string())
        );
        assert_eq!(first_nonempty_line("\n \n"), None);
    }
}
