//! Pluggable agent CLIs. corc spawns, resumes and reads metadata for
//! conversations through a `Provider`, so the sidebar, tmux plumbing and
//! state file never mention a specific tool. Adding a provider is one new
//! file (a unit struct with an `impl Provider`) plus one line in `all()`.

mod claude;
mod cursor;

use crate::discovery::{Meta, MetaSource};
use anyhow::Result;
use ratatui::style::Color;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Everything corc needs to know about one agent CLI.
pub trait Provider: Send + Sync {
    /// Stable id persisted in `state.json` (`"claude"`, `"cursor"`). Never
    /// change an existing one — old state files resolve by it.
    fn id(&self) -> &'static str;
    /// Human label shown in the switch picker.
    fn display_name(&self) -> &'static str;
    /// Binary to resolve on the login shell's PATH (`claude`,
    /// `cursor-agent`).
    fn binary(&self) -> &'static str;

    /// Mint the id for a fresh conversation. Claude generates a uuid corc
    /// then passes to `--session-id`; Cursor can't be told an id, so this
    /// runs `create-chat` and returns the id it hands back.
    fn new_session_id(&self, dir: &Path) -> Result<String>;

    /// Arguments after the resolved binary to run in the conversation's pane.
    /// `resume` distinguishes reviving a Dead conversation from starting the
    /// freshly minted one.
    fn spawn_args(&self, id: &str, resume: bool) -> Vec<String>;

    /// This provider's metadata reader for the sidebar.
    fn meta_source(&self) -> Result<Box<dyn MetaSource>>;
}

static CLAUDE: claude::Claude = claude::Claude;
static CURSOR: cursor::Cursor = cursor::Cursor;
static ALL: [&dyn Provider; 2] = [&CLAUDE, &CURSOR];

/// The default provider id, used for state files predating multi-provider
/// support and as the fallback for an unknown id.
pub const DEFAULT_ID: &str = "claude";

/// Every registered provider. Add one here (plus its file) and it appears in
/// the switch picker automatically.
pub fn all() -> &'static [&'static dyn Provider] {
    &ALL
}

/// A subtle, near-white accent tint for a provider's conversation titles in
/// the sidebar, so each agent CLI reads slightly differently at a glance
/// without any of them being loud: Claude faintly warm/orange, Cursor faintly
/// cool grey, Codex faintly blue. Keyed by the persisted id (not the trait) so
/// tints for agents not yet wired up as providers already apply the moment
/// they are added. Anything unrecognized falls back to a plain near-white.
pub fn accent(id: &str) -> Color {
    match id {
        "claude" => Color::Rgb(234, 198, 164),
        "cursor" => Color::Rgb(198, 200, 206),
        "codex" => Color::Rgb(188, 202, 230),
        "opencode" => Color::Rgb(196, 216, 200),
        _ => Color::Rgb(214, 214, 214),
    }
}

/// Resolve a persisted provider id, falling back to the default so an old or
/// hand-edited state file always yields a usable provider.
pub fn by_id(id: &str) -> &'static dyn Provider {
    all()
        .iter()
        .copied()
        .find(|p| p.id() == id)
        .unwrap_or(all()[0])
}

/// The metadata readers of every provider, fanned out on refresh and merged
/// on lookup. Conversation ids are unique across providers, so `meta` just
/// asks each source in turn.
pub struct MetaStore {
    sources: HashMap<&'static str, Box<dyn MetaSource>>,
}

impl MetaStore {
    pub fn new() -> Result<Self> {
        let mut sources = HashMap::new();
        for p in all() {
            sources.insert(p.id(), p.meta_source()?);
        }
        Ok(Self { sources })
    }

    /// Refresh every source with its subset of known conversations. The final
    /// field carries a persisted in-flight turn start across corc restarts.
    pub fn refresh(
        &mut self,
        known: &[(String, PathBuf, &'static str, Option<u64>)],
    ) -> Result<()> {
        for (pid, source) in self.sources.iter_mut() {
            let subset: Vec<(String, PathBuf, Option<u64>)> = known
                .iter()
                .filter(|(_, _, p, _)| p == pid)
                .map(|(id, cwd, _, started)| (id.clone(), cwd.clone(), *started))
                .collect();
            source.refresh(&subset)?;
        }
        Ok(())
    }

    pub fn meta(&self, id: &str) -> Option<&Meta> {
        self.sources.values().find_map(|s| s.meta(id))
    }
}
