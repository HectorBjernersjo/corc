use crate::discovery::{Conversation, TurnState};
use crate::procs::ClaudeProc;
use crate::tmux::{self, Pane};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// A live claude is working on this conversation.
    Running,
    /// A live claude sits at the prompt waiting for input.
    Waiting,
    /// No live process — history only.
    Idle,
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Waiting => "waiting",
            Status::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Annotated {
    pub conv: Conversation,
    pub status: Status,
    pub pane: Option<Pane>,
}

/// Match live claude processes to conversations by working directory.
///
/// A directory with K live processes gets its K most recently active
/// conversations marked alive. When several conversations share a directory
/// the pane pairing is best effort (exact pairing needs a hook).
pub fn annotate(convs: &[&Conversation], procs: &[ClaudeProc], panes: &[Pane]) -> Vec<Annotated> {
    let mut procs_by_dir: HashMap<PathBuf, Vec<&ClaudeProc>> = HashMap::new();
    for proc in procs {
        procs_by_dir.entry(proc.cwd.clone()).or_default().push(proc);
    }
    for list in procs_by_dir.values_mut() {
        list.sort_by_key(|p| p.pid);
    }

    // convs is already sorted most-recent-first, so counting per directory
    // hands each live process to the newest conversations in its cwd.
    let mut claimed: HashMap<PathBuf, usize> = HashMap::new();
    convs
        .iter()
        .map(|conv| {
            let live = conv.cwd.as_ref().and_then(|cwd| {
                let list = procs_by_dir.get(cwd)?;
                let idx = claimed.entry(cwd.clone()).or_insert(0);
                let proc = list.get(*idx)?;
                *idx += 1;
                Some(*proc)
            });
            let (status, pane) = match live {
                Some(proc) => {
                    let status = match conv.turn_state {
                        TurnState::Mid => Status::Running,
                        TurnState::Complete | TurnState::Unknown => Status::Waiting,
                    };
                    (status, tmux::pane_for_pid(panes, proc.pid).cloned())
                }
                None => (Status::Idle, None),
            };
            Annotated {
                conv: (*conv).clone(),
                status,
                pane,
            }
        })
        .collect()
}
