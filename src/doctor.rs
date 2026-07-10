//! `corc doctor`: read-mostly diagnostics for the external pieces corc needs.
//! It checks tmux compatibility, agent binaries, PATH visibility and whether
//! the persistent state can be read and written.

use crate::{provider, state, tmux};
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
