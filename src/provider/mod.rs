//! Pluggable agent CLIs. corc spawns, resumes and reads metadata for
//! conversations through a `Provider`, so the sidebar, tmux plumbing and
//! state file never mention a specific tool. Adding a provider is one new
//! file (a unit struct with an `impl Provider`) plus one line in `all()`.

mod claude;
mod codex;
mod cursor;
mod opencode;

use crate::discovery::{Known, Meta, MetaSource};
use anyhow::Result;
use ratatui::style::Color;
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

/// What `corc cd` must do after moving its own row.
pub enum Relocation {
    /// Type a provider command into the pane, then wait for provider metadata
    /// to confirm where the session ended up.
    TypeIntoPane(String),
    /// The agent already chose the directory when it called `corc cd`; only
    /// corc's bookkeeping needs to move.
    BookkeepingOnly,
}

/// Everything corc needs to know about one agent CLI.
pub trait Provider: Send + Sync {
    /// Stable id persisted in `state.json` (`"claude"`, `"opencode"`, ...). Never
    /// change an existing one — old state files resolve by it.
    fn id(&self) -> &'static str;
    /// Human label shown in the switch picker.
    fn display_name(&self) -> &'static str;
    /// Binary to resolve on the login shell's PATH (`claude`, `opencode`, ...).
    fn binary(&self) -> &'static str;

    /// Mint the id for a fresh conversation. Some providers accept a
    /// corc-generated id, some return their own, and others start with a
    /// provisional id that `resolve_spawned_id` later replaces.
    fn new_session_id(&self, dir: &Path) -> Result<String>;

    /// Arguments after the resolved binary to run in the conversation's pane.
    /// `resume` distinguishes reviving a Dead conversation from starting the
    /// freshly minted one.
    fn spawn_args(&self, id: &str, resume: bool) -> Vec<String>;

    /// Whether `id` is still the provisional id from `new_session_id`,
    /// awaiting the agent's real one. Always false for agents whose ids are
    /// final at spawn time (Claude, Cursor).
    fn is_pending(&self, _id: &str) -> bool {
        false
    }

    /// Discover the real id of a spawned conversation, for agents that mint
    /// their own id and never hand it back immediately (Codex/OpenCode only
    /// persist it once the first message is sent). corc spawns the
    /// pane under the provisional id from `new_session_id` and retries this
    /// on every refresh while `is_pending`: find the agent's on-disk session
    /// created in `dir` at/after `since` and adopt its id (corc then renames
    /// the hidden window and re-keys state to it). `taken` are ids other
    /// conversations already claimed — without it, two pending conversations
    /// in the same directory would both resolve to the same session and one
    /// could never advance to its own. `Ok(None)` means "not discoverable
    /// yet" — usually that the user simply hasn't sent a message; until they
    /// do the conversation reads as empty and is subject to the usual
    /// empty-discard on leave (D17).
    fn resolve_spawned_id(
        &self,
        _dir: &Path,
        _since: SystemTime,
        _taken: &[String],
    ) -> Result<Option<String>> {
        Ok(None)
    }

    /// How this provider handles `corc cd` (ADR-0003). None refuses the move.
    fn relocation(&self, _dir: &Path) -> Option<Relocation> {
        None
    }

    /// This provider's metadata reader for the sidebar.
    fn meta_source(&self) -> Result<Box<dyn MetaSource>>;

    /// The provider's plan-usage limits as percent used (Claude: 5h session,
    /// weekly, model-scoped weekly), for the readout under the menu's switch
    /// row. Blocking — called from the usage thread, never the draw loop.
    /// None (the default) when the provider has no usage source, or the
    /// fetch failed and the previous snapshot should stand.
    fn fetch_usage(&self) -> Option<Vec<crate::usage::Entry>> {
        None
    }
}

static CLAUDE: claude::Claude = claude::Claude;
static CODEX: codex::Codex = codex::Codex;
static CURSOR: cursor::Cursor = cursor::Cursor;
static OPENCODE: opencode::OpenCode = opencode::OpenCode;
static ALL: [&dyn Provider; 4] = [&CLAUDE, &CODEX, &CURSOR, &OPENCODE];

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

    /// Refresh every source with its subset of known conversations, paired
    /// with the provider that owns each.
    pub fn refresh(&mut self, known: &[(Known, &'static str)]) -> Result<()> {
        for (pid, source) in self.sources.iter_mut() {
            let subset: Vec<Known> = known
                .iter()
                .filter(|(_, p)| p == pid)
                .map(|(conv, _)| conv.clone())
                .collect();
            source.refresh(&subset)?;
        }
        Ok(())
    }

    /// Let every source persist what it parsed, for the next corc start.
    pub fn save_cache(&mut self) {
        for source in self.sources.values_mut() {
            source.save_cache();
        }
    }

    pub fn meta(&self, id: &str) -> Option<&Meta> {
        self.sources.values().find_map(|s| s.meta(id))
    }
}
