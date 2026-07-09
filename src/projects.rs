//! `corc projects` (D21): the sessionizer that replaces new.sh. A centered
//! picker (run inside a `tmux display-popup`) over existing tmux sessions and
//! the merged project directories. Selecting a session switches to it;
//! selecting a directory creates its session — honoring the per-project
//! `.tmux.sh` hook — and switches there. Unlike the agent TUI this never
//! touches corc's conversation state; it only moves between real sessions.

use crate::state::State;
use crate::widget::{self, Choice};
use crate::{display_dir, picker, tmux};
use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;

pub fn run() -> Result<()> {
    let state = State::load()?;
    let sessions = tmux::list_sessions();
    let session_set: HashSet<&str> = sessions.iter().map(String::as_str).collect();

    // Existing sessions first, then project directories that don't already
    // have a session (new.sh rad 47-57), keyed by the name they'd create.
    let mut items: Vec<Choice> = sessions.iter().map(|s| Choice::new(s, s)).collect();
    for dir in picker::list_directories(&state.directories)? {
        if session_set.contains(tmux::session_name_for(&dir).as_str()) {
            continue;
        }
        let path = dir.to_string_lossy().into_owned();
        items.push(Choice::new(display_dir(&path), path));
    }

    let Some(choice) = widget::run_filter_picker("switch project", items)? else {
        return Ok(());
    };

    if session_set.contains(choice.as_str()) {
        return tmux::switch_client(&choice);
    }
    let dir = PathBuf::from(&choice);
    let name = tmux::session_name_for(&dir);
    if !tmux::session_exists(&name) {
        tmux::create_session(&name, &dir)?;
    }
    tmux::switch_client(&name)
}
