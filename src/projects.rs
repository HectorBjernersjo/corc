//! `corc projects` (D21): the sessionizer that replaces new.sh. A centered
//! picker (run inside a `tmux display-popup`) over existing tmux sessions and
//! the merged project directories. Selecting a session switches to it;
//! selecting a directory creates its session — honoring the per-project
//! `.tmux.sh` hook — and switches there. Unlike the agent TUI this never
//! touches corc's conversation state; it only moves between real sessions.

use crate::state::State;
use crate::widget::{self, Choice};
use crate::{display_dir, picker, repo, tmux};
use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;

pub fn run() -> Result<()> {
    let mut state = State::load()?;
    let sessions = tmux::list_sessions();
    let session_set: HashSet<&str> = sessions.iter().map(String::as_str).collect();

    // Existing sessions first, then project directories that don't already
    // have a session (new.sh rad 47-57). A session belongs to a *directory*,
    // not to a name, so the filter is on the directory — a session the user
    // renamed by hand still hides its own project from the list.
    let mut items: Vec<Choice> = sessions.iter().map(|s| Choice::new(s, s)).collect();
    let dirs: Vec<String> = picker::list_directories(&state.directories)?
        .iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect();
    let taken = tmux::session_dirs();
    for path in &dirs {
        if taken.contains(&tmux::dir_key(path)) {
            continue;
        }
        items.push(Choice::new(display_dir(path), path.clone()));
    }

    // The picker's path mode (input starting with `~` or `/`) lets the user
    // type — or create — a directory that isn't listed, in the same screen.
    let known: HashSet<String> = items.iter().map(|c| c.value.clone()).collect();
    let Some(choice) = widget::run_filter_picker("switch project", items)? else {
        return Ok(());
    };

    if session_set.contains(choice.as_str()) {
        return tmux::switch_client(&choice);
    }
    let dir = PathBuf::from(&choice);
    // A directory that wasn't in the list came from path mode — record it in
    // the machine-local list so it shows up in the `N` picker from now on too,
    // the same as the TUI's picker. `save` unions with disk, so this is safe
    // even while the TUI runs (D22); it never touches conversation state.
    if !known.contains(&choice) && state.add_directory(&dir) {
        let _ = state.save();
    }
    // The label is disambiguated against the directory list this picker was
    // built from; a path typed in path mode falls back to its basename. Either
    // way it is only the name the session gets — `ensure_session` finds an
    // existing one by directory whatever it ended up called.
    let label = repo::label_for(&choice, &dirs);
    let (name, _) = tmux::ensure_session(&dir, &label)?;
    tmux::switch_client(&name)
}
