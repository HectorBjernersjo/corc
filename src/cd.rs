//! Conversation relocation (`corc cd <dir>`, ADR-0003): the agent asks corc
//! to move its own conversation to another directory. Claude needs corc to
//! type its user-only `/cd` command into the pane; Cursor already knows the
//! directory it chose and needs only corc's bookkeeping to follow.
//!
//! For Claude, input typed into a running agent queues and executes when the
//! turn ends, so timing never matters. The row moves as soon as `/cd` is
//! typed, and the agent's own report settles it
//! (`state::Conversation::settled_cwd`): a declined trust prompt or a failed
//! `/cd` puts it back at the next prompt.

use crate::{provider, state, tmux};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The mailbox `corc cd` drops requests into for the TUI to pick up — the
/// same single-writer handover as the browser-view mailbox: the CLI only
/// appends, the TUI applies and clears on its next refresh, within a second.
fn mailbox() -> Result<PathBuf> {
    let state = state::state_file()?;
    let dir = state.parent().context("state file has no parent dir")?;
    Ok(dir.join("cd-requests"))
}

/// `corc cd <dir>`: ask corc to move the conversation this command runs in.
/// Strictly the calling pane's conversation — no viewed-conversation fallback
/// like `corc browser` has, because relocating the wrong conversation is a
/// real move, not a toggled view.
pub fn command(dir: Option<&str>) -> Result<()> {
    let dir = dir.context("usage: corc cd <directory>")?;
    let dir = canonical_dir(dir)?;
    let state = state::State::load()?;
    let pane = std::env::var("TMUX_PANE")
        .context("corc cd must run from inside an agent pane (TMUX_PANE is not set)")?;
    let conv = state
        .conversations
        .iter()
        .find(|c| c.pane_id.as_deref() == Some(pane.as_str()))
        .context("this pane is not a corc conversation — run corc cd inside an agent pane")?;
    let relocation = provider::by_id(&conv.provider)
        .relocation(&dir)
        .with_context(|| format!("{} cannot relocate a running session", conv.provider))?;
    append_request(&mailbox()?, &conv.id, &dir)?;
    match relocation {
        provider::Relocation::TypeIntoPane(command) => println!(
            "corc will type `{command}` into this conversation; \
             it runs once the current turn ends"
        ),
        provider::Relocation::BookkeepingOnly => {
            println!("corc will move this conversation to {}", dir.display())
        }
    }
    Ok(())
}

/// Append one request. Appending rather than overwriting means a second
/// request lands even if the TUI has not consumed the first yet; the last
/// line for an id wins simply because it is typed last.
fn append_request(path: &Path, id: &str, dir: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(file, "{id}\t{}", dir.display()).with_context(|| format!("writing {}", path.display()))
}

/// The directory corc will record: absolute and existing. `~` is expanded so
/// the agent can pass what a user would type; canonicalizing also rejects a
/// directory that does not exist before anything is delivered or recorded.
pub fn canonical_dir(dir: &str) -> Result<PathBuf> {
    let expanded = match dir.strip_prefix("~") {
        Some(rest) => {
            let home = std::env::var("HOME").context("HOME not set")?;
            PathBuf::from(format!("{home}{rest}"))
        }
        None => PathBuf::from(dir),
    };
    let path = std::fs::canonicalize(&expanded)
        .with_context(|| format!("no such directory: {}", expanded.display()))?;
    if !path.is_dir() {
        bail!("not a directory: {}", path.display());
    }
    Ok(path)
}

/// What one refresh's worth of requests did.
#[derive(Default)]
pub struct Applied {
    /// A row moved, so state.json needs writing.
    pub moved: bool,
    /// A message for the status line when something could not be delivered.
    pub failure: Option<String>,
}

/// The TUI's half of the handover, run once per refresh: apply each provider's
/// relocation strategy and empty the mailbox. Best-effort like the browser
/// mailbox — a dropped request costs one `corc cd`, while a stuck one could
/// otherwise land in the wrong turn later.
pub fn apply_requests(state: &mut state::State, now: u64) -> Applied {
    let mut applied = Applied::default();
    let Some(text) = mailbox().ok().and_then(|path| {
        let text = std::fs::read_to_string(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        Some(text)
    }) else {
        return applied;
    };
    for (id, dir) in parse_requests(&text) {
        match apply_request(state, &id, &dir, now) {
            Ok(moved) => applied.moved |= moved,
            Err(e) => applied.failure = Some(format!("corc cd: {e:#}")),
        }
    }
    applied
}

fn apply_request(state: &mut state::State, id: &str, dir: &Path, now: u64) -> Result<bool> {
    let conv = state.conversation(id).context("conversation is gone")?;
    match provider::by_id(&conv.provider)
        .relocation(dir)
        .context("provider cannot relocate")?
    {
        provider::Relocation::TypeIntoPane(command) => {
            let pane = conv
                .pane_id
                .as_deref()
                .context("conversation is dead — nothing to type into")?;
            tmux::type_into_pane(pane, &command)?;
            Ok(state.request_relocation(id, dir, now))
        }
        provider::Relocation::BookkeepingOnly => Ok(state.relocate(id, dir)),
    }
}

fn parse_requests(text: &str) -> Vec<(String, PathBuf)> {
    text.lines()
        .filter_map(|line| {
            let (id, dir) = line.split_once('\t')?;
            (!id.is_empty() && !dir.is_empty()).then(|| (id.to_string(), PathBuf::from(dir)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_roundtrip_through_the_mailbox_format() {
        let dir = std::env::temp_dir().join("corc-test-cd-mailbox");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("cd-requests");

        append_request(&path, "conv-1", Path::new("/work/a")).unwrap();
        append_request(&path, "conv-2", Path::new("/work/with space")).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            parse_requests(&text),
            vec![
                ("conv-1".to_string(), PathBuf::from("/work/a")),
                ("conv-2".to_string(), PathBuf::from("/work/with space")),
            ]
        );
        // Malformed lines are dropped, never delivered somewhere strange.
        assert!(parse_requests("no-tab-here\n\t/dir\nid\t\n").is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn canonical_dir_expands_home_and_rejects_missing_directories() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(canonical_dir("~").unwrap(), PathBuf::from(&home));
        assert!(canonical_dir("/definitely/not/a/real/dir").is_err());
    }

    #[test]
    fn cursor_relocation_only_moves_corcs_bookkeeping() {
        let mut state = state::State::default();
        state.add_conversation(
            "cursor-chat".into(),
            PathBuf::from("/work/old"),
            "%1".into(),
            "cursor".into(),
        );

        assert!(apply_request(&mut state, "cursor-chat", Path::new("/work/new"), 1000).unwrap());
        let conv = state.conversation("cursor-chat").unwrap();
        assert_eq!(conv.cwd, Path::new("/work/new"));
        assert_eq!(conv.relocation_requested_at, None);
        assert_eq!(state.projects, vec!["/work/new"]);
    }
}
