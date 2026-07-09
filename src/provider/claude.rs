//! Claude Code. corc generates the session uuid itself and passes it to
//! `--session-id` (new) / `--resume` (revive); metadata comes from the jsonl
//! transcripts under `~/.claude/projects` (`discovery::Store`).

use super::Provider;
use crate::discovery::{MetaSource, Store};
use crate::state;
use anyhow::Result;
use std::path::Path;

pub struct Claude;

impl Provider for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn binary(&self) -> &'static str {
        "claude"
    }

    fn new_session_id(&self, _dir: &Path) -> Result<String> {
        state::new_uuid()
    }

    fn spawn_args(&self, id: &str, resume: bool) -> Vec<String> {
        let flag = if resume { "--resume" } else { "--session-id" };
        vec![flag.to_string(), id.to_string()]
    }

    fn meta_source(&self) -> Result<Box<dyn MetaSource>> {
        Ok(Box::new(Store::new()?))
    }
}
