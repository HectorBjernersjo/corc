//! The sidebar TUI. corc's pane is the sidebar (40 columns, left); the
//! content pane to its right holds either a plain-shell placeholder or the
//! currently viewed conversation's Claude pane, swapped in from the hidden
//! session (ADR-0001).

use crate::provider::{self, MetaStore};
use crate::state::{self, State};
use crate::status::{self, Status};
use crate::{display_dir, picker, tmux, truncate};
use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

enum Item {
    Header(String),
    /// Index into `state.conversations`.
    Conv(usize),
}

/// Dead conversations older than this are hidden unless `a` is on (D12).
const HIDE_DEAD_AFTER_SECS: u64 = 7 * 24 * 3600;
/// Grace period before an empty conversation the user left is discarded
/// (D17). A message sent an instant before leaving can still be flushing to
/// disk — Cursor lags noticeably — so we wait and re-check emptiness rather
/// than discarding on the spot.
const DISCARD_GRACE: Duration = Duration::from_secs(5);
/// Most recent conversations shown per project before the rest are hidden
/// (D13) — the `a` toggle reveals them. Keeps each project's list short.
const MAX_PER_PROJECT: usize = 7;

/// Foreground commands that count as an idle shell prompt for the digit
/// jump's window-1 nvim rule (D13); anything else is busy and never touched.
const SHELLS: &[&str] = &["bash", "zsh", "fish", "sh", "dash", "ksh", "tcsh", "nu"];

/// The `n` directory-picker overlay (D14): directories.txt expanded with git
/// worktrees, filtered with the same word-substring matching as `/`.
struct Picker {
    dirs: Vec<std::path::PathBuf>,
    input: String,
    /// Index into the *filtered* list.
    selected: usize,
}

/// The `p` overlay: type a filesystem path (prefilled with `$HOME`), Tab/▼▲
/// to complete against real subdirectories. Enter appends the directory to
/// directories.txt and spawns a fresh conversation there.
struct PathPrompt {
    input: String,
    /// Subdirectories completing the current input, sorted.
    completions: Vec<PathBuf>,
    /// Index into `completions`.
    selected: usize,
}

impl Picker {
    /// Indices into `dirs` that match the current input, in source order.
    fn filtered(&self) -> Vec<usize> {
        self.dirs
            .iter()
            .enumerate()
            .filter(|(_, d)| picker::matches_words(&self.input, &display_dir(&d.to_string_lossy())))
            .map(|(i, _)| i)
            .collect()
    }
}

/// The `s` provider-switch overlay: a fuzzy picker over the registered
/// providers. Enter sets the provider for conversations spawned from now on.
struct ProviderPicker {
    input: String,
    /// Index into the *filtered* provider list.
    selected: usize,
}

impl ProviderPicker {
    fn filtered(&self) -> Vec<&'static dyn provider::Provider> {
        provider::all()
            .iter()
            .copied()
            .filter(|p| picker::matches_words(&self.input, p.display_name()))
            .collect()
    }
}

struct App {
    state: State,
    metas: MetaStore,
    /// Pane statuses derived on refresh, parallel to `state.conversations`.
    statuses: Vec<Status>,
    /// The pane corc runs in (left, fixed 40 columns).
    sidebar_pane: String,
    /// The plain-shell pane corc created on the right. While a conversation
    /// is viewed, this pane sits parked in that conversation's hidden window.
    placeholder_pane: String,
    /// Conversation currently swapped into the content slot.
    viewed: Option<String>,
    /// Within-project display order (conversation ids): live above dead,
    /// most recently active first. Recomputed only on a state change, so
    /// rows never re-sort under the user's cursor (D9).
    row_order: Vec<String>,
    /// (id, status) snapshot the current `row_order` was computed from.
    sort_signature: Vec<(String, Status)>,
    items: Vec<Item>,
    selected: usize,
    filter: String,
    filter_input: bool,
    /// Conversation id awaiting the `y/n` kill confirmation (`x` on a
    /// Running conversation, D12).
    pending_kill: Option<String>,
    /// `a` toggle: also show Dead conversations older than a week (D12).
    show_all: bool,
    /// How many week-old Dead conversations the current list is hiding.
    hidden: usize,
    /// The `N` directory-picker overlay, when open (D14).
    picker: Option<Picker>,
    /// The `p` add-directory path prompt, when open.
    path_prompt: Option<PathPrompt>,
    /// The `s` provider-switch overlay, when open.
    provider_picker: Option<ProviderPicker>,
    /// Move mode (D9): `K`/`J` reorder the selected row's project.
    move_mode: bool,
    /// Empty conversations the user has left, awaiting the `DISCARD_GRACE`
    /// re-check before being discarded (D17). (id, when it was marked.)
    pending_discard: Vec<(String, Instant)>,
    /// Persistent list state so the scroll offset survives between frames —
    /// what lets a mouse click map back to the item under the pointer (D11).
    list_state: ListState,
    /// Rows of list *content* on screen (below the title, above the footer),
    /// for bounds-checking mouse clicks.
    list_rows: u16,
    status_msg: Option<String>,
    last_refresh: Instant,
}

pub fn run() -> Result<()> {
    let sidebar_pane =
        std::env::var("TMUX_PANE").map_err(|_| anyhow::anyhow!("corc must run inside tmux"))?;

    // Resolve the active provider's binary once now, so the login-shell lookup
    // cost lands at startup rather than on the first conversation spawn.
    let mut state = State::load()?;
    let _ = tmux::resolve_binary(provider::by_id(&state.active_provider).binary());

    reconcile(&mut state)?;
    state.save()?;

    let placeholder_pane = tmux::split_content_pane(&sidebar_pane)?;

    let mut app = App {
        state,
        metas: MetaStore::new()?,
        statuses: Vec::new(),
        sidebar_pane,
        placeholder_pane,
        viewed: None,
        row_order: Vec::new(),
        sort_signature: Vec::new(),
        items: Vec::new(),
        selected: 0,
        filter: String::new(),
        filter_input: false,
        pending_kill: None,
        show_all: false,
        hidden: 0,
        picker: None,
        path_prompt: None,
        provider_picker: None,
        move_mode: false,
        pending_discard: Vec::new(),
        list_state: ListState::default(),
        list_rows: 0,
        status_msg: None,
        last_refresh: Instant::now(),
    };
    app.refresh();
    app.view_last();

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?;

    let result = app.event_loop(&mut terminal);

    // Swap the viewed pane home and remove the content pane we created (D10).
    app.park();
    // On quit there is no next tick to run the grace re-check, so discard any
    // still-empty conversations now rather than letting them persist as
    // (untitled) rows across restarts (D17).
    app.flush_pending_discards();
    if tmux::pane_exists(&app.placeholder_pane) {
        let _ = tmux::kill_pane(&app.placeholder_pane);
    }
    let _ = app.state.save();

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

/// Startup reconciliation (D16): drop pane ids that no longer exist (the
/// conversation is Dead) and park Claude panes stranded outside the hidden
/// session (corc crashed mid-view) back into uuid-named hidden windows.
fn reconcile(state: &mut State) -> Result<()> {
    for conv in &mut state.conversations {
        let Some(pane_id) = conv.pane_id.clone() else {
            continue;
        };
        if !tmux::pane_exists(&pane_id) {
            conv.pane_id = None;
            continue;
        }
        match tmux::pane_session(&pane_id) {
            Ok(session) if session == tmux::HIDDEN_SESSION => {}
            Ok(_) => {
                if let Err(e) = tmux::park_stray(&pane_id, &conv.id) {
                    eprintln!("corc: failed to park stray pane {pane_id}: {e}");
                    conv.pane_id = None;
                }
            }
            Err(_) => conv.pane_id = None,
        }
    }
    Ok(())
}

impl App {
    fn event_loop(
        &mut self,
        terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        loop {
            terminal.draw(|f| self.draw(f))?;

            if event::poll(Duration::from_millis(200))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if self.handle_key(key.code, key.modifiers) {
                            return Ok(());
                        }
                    }
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    Event::Resize(_, _) => {
                        let _ = tmux::enforce_sidebar_width(&self.sidebar_pane);
                    }
                    _ => {}
                }
            }
            if self.last_refresh.elapsed() >= Duration::from_secs(1) {
                self.refresh();
            }
        }
    }

    /// Returns true when the app should quit.
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            return true;
        }
        // The directory picker owns the keyboard while open (D14).
        if self.picker.is_some() {
            self.handle_picker_key(code);
            return false;
        }
        // The add-directory path prompt likewise owns the keyboard.
        if self.path_prompt.is_some() {
            self.handle_path_key(code);
            return false;
        }
        // The provider-switch picker likewise owns the keyboard.
        if self.provider_picker.is_some() {
            self.handle_provider_key(code);
            return false;
        }
        // A pending `x` on a Running conversation: only y/n answer it (D12).
        if let Some(id) = self.pending_kill.clone() {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.pending_kill = None;
                    self.kill_conversation(&id);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.pending_kill = None;
                }
                _ => {}
            }
            return false;
        }
        if self.filter_input {
            match code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filter_input = false;
                    self.rebuild_items();
                }
                KeyCode::Enter => self.filter_input = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.rebuild_items();
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.rebuild_items();
                }
                _ => {}
            }
            return false;
        }
        // Move mode (D9): K/J reorder projects, Esc/Enter/V leave; the
        // selection can still be moved to pick a different project.
        if self.move_mode {
            match code {
                KeyCode::Char('V') | KeyCode::Esc | KeyCode::Enter => self.move_mode = false,
                KeyCode::Char('K') => self.move_project(-1),
                KeyCode::Char('J') => self.move_project(1),
                KeyCode::Char('j') | KeyCode::Down => self.select_next(1),
                KeyCode::Char('k') | KeyCode::Up => self.select_next(-1),
                _ => {}
            }
            return false;
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.select_next(1),
            KeyCode::Char('k') | KeyCode::Up => self.select_next(-1),
            KeyCode::Char('g') | KeyCode::Home => self.select_edge(true),
            KeyCode::Char('G') | KeyCode::End => self.select_edge(false),
            KeyCode::Char('/') => self.filter_input = true,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.rebuild_items();
            }
            KeyCode::Enter => self.view_selected(),
            KeyCode::Char('n') => self.new_conversation_here(),
            KeyCode::Char('N') => self.open_picker(),
            KeyCode::Char('p') => self.open_path_prompt(),
            KeyCode::Char('s') => self.open_provider_picker(),
            KeyCode::Char('x') => self.kill_or_remove(),
            KeyCode::Char('V') => self.move_mode = true,
            KeyCode::Char(c @ '1'..='9') => self.digit_jump(c as u8 - b'0'),
            KeyCode::Char('a') => {
                self.show_all = !self.show_all;
                self.rebuild_items();
            }
            KeyCode::Char('r') => self.refresh(),
            _ => {}
        }
        false
    }

    /// Rows item `idx` occupies on screen: headers are two rows (a blank
    /// spacer line above the rule), conversations one.
    fn item_height(&self, idx: usize) -> u16 {
        match self.items.get(idx) {
            Some(Item::Header(_)) => 2,
            _ => 1,
        }
    }

    /// The item under list row `row`, walking item heights from the list's
    /// scroll offset — headers are taller than one row.
    fn item_at_row(&self, row: u16) -> Option<usize> {
        let mut top = 0u16;
        for idx in self.list_state.offset()..self.items.len() {
            let next = top + self.item_height(idx);
            if row < next {
                return Some(idx);
            }
            top = next;
        }
        None
    }

    /// Mouse (D11): click a row = select + view; the wheel moves the
    /// selection. Ignored while the picker overlay is open.
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.picker.is_some() || self.provider_picker.is_some() || self.path_prompt.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => self.select_next(1),
            MouseEventKind::ScrollUp => self.select_next(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                // Rows map to items through the list's persistent scroll
                // offset.
                if mouse.row >= self.list_rows {
                    return; // footer / below the list
                }
                if let Some(idx) = self.item_at_row(mouse.row)
                    && matches!(self.items.get(idx), Some(Item::Conv(_)))
                {
                    self.selected = idx;
                    self.view_selected();
                }
            }
            _ => {}
        }
    }

    /// The (id, cwd, provider-id) triples the metadata store fans out over.
    fn known_conversations(&self) -> Vec<(String, PathBuf, &'static str)> {
        self.state
            .conversations
            .iter()
            .map(|c| (c.id.clone(), c.cwd.clone(), provider::by_id(&c.provider).id()))
            .collect()
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        let mut dirty = false;

        // Notice vanished panes: the conversation is Dead (D12).
        let mut viewed_died = false;
        for conv in &mut self.state.conversations {
            if let Some(pane_id) = &conv.pane_id
                && !tmux::pane_exists(pane_id)
            {
                conv.pane_id = None;
                dirty = true;
                if self.viewed.as_deref() == Some(conv.id.as_str()) {
                    viewed_died = true;
                }
            }
        }
        let mut died_id = None;
        if viewed_died {
            // The Claude in the content slot exited: its pane is gone and our
            // placeholder shell is parked in its hidden window. Reclaim the
            // window and recreate the placeholder next to the sidebar.
            let id = self.viewed.take().unwrap();
            let _ = tmux::kill_hidden_window(&id);
            match tmux::split_content_pane(&self.sidebar_pane) {
                Ok(pane) => self.placeholder_pane = pane,
                Err(e) => self.status_msg = Some(e.to_string()),
            }
            died_id = Some(id);
        } else if self.viewed.is_none() && !tmux::pane_exists(&self.placeholder_pane) {
            // Someone closed the placeholder shell; put it back.
            if let Ok(pane) = tmux::split_content_pane(&self.sidebar_pane) {
                self.placeholder_pane = pane;
            }
        }

        let known = self.known_conversations();
        if let Err(e) = self.metas.refresh(&known) {
            self.status_msg = Some(e.to_string());
        }

        // A conversation whose agent exited before a single message was ever
        // sent is forgotten rather than left as an (untitled) Dead row (D17) —
        // after the grace re-check, in case a final message is still flushing.
        if let Some(id) = died_id
            && self.is_empty_conversation(&id)
        {
            self.mark_pending_discard(id);
        }
        self.process_pending_discards();

        // The conversation in the content pane counts as continuously
        // viewed (D6): its last_viewed follows along in memory and is
        // persisted on swap and quit.
        if let Some(id) = self.viewed.clone()
            && let Some(c) = self.state.conversation_mut(&id)
        {
            c.last_viewed = state::unix_now();
        }

        let viewed = self.viewed.clone();
        let now = state::unix_now();
        self.statuses = self
            .state
            .conversations
            .iter()
            .map(|c| {
                status::derive(
                    c.pane_id.is_some(),
                    self.metas.meta(&c.id),
                    c.last_viewed,
                    viewed.as_deref() == Some(c.id.as_str()),
                    now,
                    c.created_at,
                )
            })
            .collect();

        // Re-sort rows only when some conversation changed state (or was
        // added/removed) — never under the user's cursor (D9).
        let signature: Vec<(String, Status)> = self
            .state
            .conversations
            .iter()
            .zip(&self.statuses)
            .map(|(c, s)| (c.id.clone(), *s))
            .collect();
        if signature != self.sort_signature {
            self.sort_signature = signature;
            self.resort_rows();
        }

        if dirty {
            let _ = self.state.save();
        }

        self.rebuild_keeping_selection();
    }

    /// Recompute the within-project row order (D9): live conversations
    /// above dead, most recently active first. Called only when the
    /// (id, status) signature changed, so an unchanged list never shuffles.
    fn resort_rows(&mut self) {
        let mut indices: Vec<usize> = (0..self.state.conversations.len()).collect();
        indices.sort_by_key(|&i| {
            let conv = &self.state.conversations[i];
            let dead = self.statuses.get(i) == Some(&Status::Dead);
            let activity = status::last_activity(self.metas.meta(&conv.id), conv.created_at);
            (dead, std::cmp::Reverse(activity))
        });
        self.row_order = indices
            .into_iter()
            .map(|i| self.state.conversations[i].id.clone())
            .collect();
    }

    fn rebuild_items(&mut self) {
        let filter = self.filter.to_lowercase();
        let now = state::unix_now();
        self.items.clear();
        self.hidden = 0;

        for project in &self.state.projects {
            // Conversations of this project, in the cached display order
            // (plus any not yet ordered, at the bottom, for safety).
            let ordered = self.row_order.iter().filter_map(|id| {
                self.state.conversations.iter().position(|c| c.id == *id)
            });
            let unordered = self
                .state
                .conversations
                .iter()
                .enumerate()
                .filter(|(_, c)| !self.row_order.contains(&c.id))
                .map(|(i, _)| i);
            let indices: Vec<usize> = ordered
                .chain(unordered)
                .filter(|&i| self.state.conversations[i].cwd.display().to_string() == *project)
                .collect();

            let name = project_display(project);
            let mut kept = Vec::new();
            for i in indices {
                // Week-old Dead conversations stay out of the list unless
                // the `a` toggle is on (D12) — the list stays short by itself.
                if !self.show_all && self.statuses.get(i) == Some(&Status::Dead) {
                    let conv = &self.state.conversations[i];
                    let age = now.saturating_sub(status::last_activity(
                        self.metas.meta(&conv.id),
                        conv.created_at,
                    ));
                    if age > HIDE_DEAD_AFTER_SECS {
                        self.hidden += 1;
                        continue;
                    }
                }
                if !filter.is_empty() {
                    let title = self
                        .metas
                        .meta(&self.state.conversations[i].id)
                        .and_then(|m| m.display_title().map(str::to_string))
                        .unwrap_or_default();
                    let hay = format!("{name} {title}");
                    if !picker::matches_words(&filter, &hay) {
                        continue;
                    }
                }
                kept.push(i);
            }
            // Cap each project to its most-recent conversations (D13). `kept`
            // is already in the sticky recency order, so truncating drops the
            // ones with the oldest history without reshuffling the rest. The
            // `a` toggle and an active filter both bypass the cap.
            if !self.show_all && filter.is_empty() && kept.len() > MAX_PER_PROJECT {
                self.hidden += kept.len() - MAX_PER_PROJECT;
                kept.truncate(MAX_PER_PROJECT);
            }
            if kept.is_empty() {
                continue;
            }
            self.items.push(Item::Header(name));
            for i in kept {
                self.items.push(Item::Conv(i));
            }
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.items.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.min(self.items.len() - 1);
        if matches!(self.items[self.selected], Item::Header(_)) {
            self.select_next(1);
            if matches!(self.items[self.selected], Item::Header(_)) {
                self.select_next(-1);
            }
        }
    }

    fn select_next(&mut self, dir: i64) {
        let len = self.items.len() as i64;
        if len == 0 {
            return;
        }
        let mut idx = self.selected as i64;
        loop {
            idx += dir;
            if idx < 0 || idx >= len {
                return;
            }
            if matches!(self.items[idx as usize], Item::Conv(_)) {
                self.selected = idx as usize;
                return;
            }
        }
    }

    fn select_edge(&mut self, top: bool) {
        let pos = if top {
            self.items.iter().position(|i| matches!(i, Item::Conv(_)))
        } else {
            self.items.iter().rposition(|i| matches!(i, Item::Conv(_)))
        };
        if let Some(pos) = pos {
            self.selected = pos;
        }
    }

    fn selected_conv_id(&self) -> Option<String> {
        match self.items.get(self.selected)? {
            Item::Conv(i) => Some(self.state.conversations[*i].id.clone()),
            Item::Header(_) => None,
        }
    }

    /// Swap the selected conversation into the content pane, respawning it
    /// with `--resume` first if it is Dead.
    fn view_selected(&mut self) {
        let Some(id) = self.selected_conv_id() else {
            return;
        };
        self.status_msg = self.view(&id).err().map(|e| e.to_string());
        // Re-derive immediately so an Unseen row flips to Idle the moment
        // it is swapped in, not a tick later.
        self.refresh();
    }

    /// On startup, swap in whichever conversation the user last had open so
    /// corc resumes where they left off, and highlight its sidebar row.
    fn view_last(&mut self) {
        let Some(id) = self
            .state
            .conversations
            .iter()
            .max_by_key(|c| c.last_viewed)
            .map(|c| c.id.clone())
        else {
            return;
        };
        if let Some(pos) = self.items.iter().position(
            |i| matches!(i, Item::Conv(idx) if self.state.conversations[*idx].id == id),
        ) {
            self.selected = pos;
        }
        self.status_msg = self.view(&id).err().map(|e| e.to_string());
        self.refresh();
    }

    fn view(&mut self, id: &str) -> Result<()> {
        if self.viewed.as_deref() == Some(id) {
            // Already in the content slot — just focus it.
            let pane = self
                .state
                .conversation(id)
                .and_then(|c| c.pane_id.clone())
                .context("viewed conversation has no pane")?;
            return tmux::select_pane(&pane);
        }

        // Make sure there is a live pane to swap in, resuming if Dead.
        let conv = self
            .state
            .conversation(id)
            .context("unknown conversation")?
            .clone();
        let pane_id = match conv.pane_id {
            Some(p) if tmux::pane_exists(&p) => p,
            _ => {
                let prov = provider::by_id(&conv.provider);
                let pane = tmux::spawn_conversation(&conv.cwd, prov, id, true)?;
                let c = self.state.conversation_mut(id).unwrap();
                c.pane_id = Some(pane.clone());
                pane
            }
        };

        self.park();
        tmux::swap_panes(&pane_id, &self.placeholder_pane)?;
        tmux::select_pane(&pane_id)?;
        self.viewed = Some(id.to_string());
        if let Some(c) = self.state.conversation_mut(id) {
            c.last_viewed = state::unix_now();
        }
        self.state.save()?;
        Ok(())
    }

    /// Swap the viewed conversation back into its hidden window, restoring
    /// the placeholder to the content slot.
    fn park(&mut self) {
        let Some(id) = self.viewed.take() else {
            return;
        };
        // Pick up a message sent moments before leaving, so a conversation
        // that was just written to is never mistaken for empty (D17).
        let known = self.known_conversations();
        let _ = self.metas.refresh(&known);
        if let Some(c) = self.state.conversation_mut(&id) {
            c.last_viewed = state::unix_now();
        }
        let pane = self.state.conversation(&id).and_then(|c| c.pane_id.clone());
        match pane {
            Some(p) if tmux::pane_exists(&p) => {
                if let Err(e) = tmux::swap_panes(&self.placeholder_pane, &p) {
                    self.status_msg = Some(e.to_string());
                }
                let _ = tmux::select_pane(&self.sidebar_pane);
            }
            _ => {
                // Claude died while viewed: the placeholder is stranded in
                // the conversation's hidden window. Reclaim it.
                if let Some(c) = self.state.conversation_mut(&id) {
                    c.pane_id = None;
                }
                let _ = tmux::kill_hidden_window(&id);
                match tmux::split_content_pane(&self.sidebar_pane) {
                    Ok(pane) => self.placeholder_pane = pane,
                    Err(e) => self.status_msg = Some(e.to_string()),
                }
            }
        }
        // A conversation the user opened but never sent a message in is
        // discarded rather than left as an (untitled) row (D17) — but only
        // after the grace re-check, so a message sent just before leaving
        // (Cursor flushes with a lag) isn't mistaken for an empty one.
        if self.is_empty_conversation(&id) {
            self.mark_pending_discard(id);
        }
    }

    /// Queue an empty conversation for discard after `DISCARD_GRACE`, unless
    /// it is already queued.
    fn mark_pending_discard(&mut self, id: String) {
        if !self.pending_discard.iter().any(|(pid, _)| *pid == id) {
            self.pending_discard.push((id, Instant::now()));
        }
    }

    /// Discard every still-empty queued conversation without waiting out the
    /// grace — used at shutdown, where no further tick will run the re-check.
    fn flush_pending_discards(&mut self) {
        let ids: Vec<String> = std::mem::take(&mut self.pending_discard)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        for id in ids {
            if self.state.conversation(&id).is_some() && self.is_empty_conversation(&id) {
                self.discard_conversation(&id);
            }
        }
    }

    /// Discard queued conversations whose grace period has elapsed and that
    /// are still empty. A conversation cancels its own discard by gaining a
    /// message (no longer empty), being viewed again, or already being gone.
    fn process_pending_discards(&mut self) {
        let now = Instant::now();
        let mut discard = Vec::new();
        let mut keep = Vec::new();
        for (id, marked) in std::mem::take(&mut self.pending_discard) {
            // Cancel: gone, re-viewed, or now has content.
            if self.state.conversation(&id).is_none()
                || self.viewed.as_deref() == Some(id.as_str())
                || !self.is_empty_conversation(&id)
            {
                continue;
            }
            if now.duration_since(marked) >= DISCARD_GRACE {
                discard.push(id);
            } else {
                keep.push((id, marked)); // keep waiting
            }
        }
        self.pending_discard = keep;
        for id in discard {
            self.discard_conversation(&id);
        }
    }

    /// True for a conversation with no generated title and no first user
    /// prompt — one the user opened but never sent a message in. The jsonl
    /// (if Claude even wrote one) has no real prompt to lose (D17).
    fn is_empty_conversation(&self, id: &str) -> bool {
        self.metas
            .meta(id)
            .and_then(|m| m.display_title())
            .is_none()
    }

    /// Forget an empty conversation: kill its Claude pane and hidden window
    /// (if any survive) and drop it from the state file. The jsonl under
    /// ~/.claude is never touched (D1).
    fn discard_conversation(&mut self, id: &str) {
        if let Some(pane) = self.state.conversation(id).and_then(|c| c.pane_id.clone())
            && tmux::kill_hidden_window(id).is_err()
            && tmux::pane_exists(&pane)
        {
            let _ = tmux::kill_pane(&pane);
        }
        self.state.conversations.retain(|c| c.id != id);
        self.status_msg = self.state.save().err().map(|e| e.to_string());
    }

    /// `x` per state (D12): a live conversation's Claude and hidden window
    /// are killed — behind a y/n confirm while Running; a Dead conversation
    /// is removed from the state file and the list. The jsonl under
    /// ~/.claude is never touched.
    fn kill_or_remove(&mut self) {
        let Some(id) = self.selected_conv_id() else {
            return;
        };
        let Some(idx) = self.state.conversations.iter().position(|c| c.id == id) else {
            return;
        };
        match self.statuses.get(idx).copied().unwrap_or(Status::Dead) {
            Status::Dead => {
                self.state.conversations.remove(idx);
                if idx < self.statuses.len() {
                    self.statuses.remove(idx);
                }
                self.status_msg = self.state.save().err().map(|e| e.to_string());
                self.refresh();
            }
            Status::Running => self.pending_kill = Some(id),
            Status::Unseen | Status::Idle => self.kill_conversation(&id),
        }
    }

    /// Kill a live conversation's Claude and its hidden window; it becomes
    /// Dead in the state file — still listed, hollow, resumable (D12).
    fn kill_conversation(&mut self, id: &str) {
        // If it is in the content slot, park it first so the kill happens in
        // the hidden window and the placeholder is back beside the sidebar.
        if self.viewed.as_deref() == Some(id) {
            self.park();
        }
        if let Some(pane) = self.state.conversation(id).and_then(|c| c.pane_id.clone()) {
            // Claude is the pane command, so killing the uuid window kills
            // both. Fall back to the pane id if the window is already gone.
            if tmux::kill_hidden_window(id).is_err() && tmux::pane_exists(&pane) {
                let _ = tmux::kill_pane(&pane);
            }
        }
        if let Some(c) = self.state.conversation_mut(id) {
            c.pane_id = None;
        }
        self.status_msg = self.state.save().err().map(|e| e.to_string());
        self.refresh();
    }

    /// `N`: open the directory-picker overlay (D14).
    fn open_picker(&mut self) {
        match picker::list_directories() {
            Ok(dirs) => {
                self.picker = Some(Picker {
                    dirs,
                    input: String::new(),
                    selected: 0,
                });
            }
            Err(e) => self.status_msg = Some(e.to_string()),
        }
    }

    /// Keys while the picker overlay is open: type to filter, arrows to
    /// move, Enter to spawn a fresh Claude there, Esc to cancel (D14).
    fn handle_picker_key(&mut self, code: KeyCode) {
        let Some(p) = &mut self.picker else {
            return;
        };
        match code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Enter => {
                let filtered = p.filtered();
                let choice = filtered
                    .get(p.selected.min(filtered.len().saturating_sub(1)))
                    .map(|&i| p.dirs[i].clone());
                if let Some(dir) = choice {
                    self.picker = None;
                    self.new_conversation_in(dir);
                }
            }
            KeyCode::Backspace => {
                p.input.pop();
                p.selected = 0;
            }
            KeyCode::Down => {
                p.selected = (p.selected + 1).min(p.filtered().len().saturating_sub(1));
            }
            KeyCode::Up => p.selected = p.selected.saturating_sub(1),
            KeyCode::Char(c) => {
                p.input.push(c);
                p.selected = 0;
            }
            _ => {}
        }
    }

    /// `s`: open the provider-switch overlay.
    fn open_provider_picker(&mut self) {
        self.provider_picker = Some(ProviderPicker {
            input: String::new(),
            selected: 0,
        });
    }

    /// Keys while the provider picker is open: type to filter, arrows to move,
    /// Enter to make the highlighted provider active for new conversations,
    /// Esc to cancel.
    fn handle_provider_key(&mut self, code: KeyCode) {
        let Some(p) = &mut self.provider_picker else {
            return;
        };
        match code {
            KeyCode::Esc => self.provider_picker = None,
            KeyCode::Enter => {
                let filtered = p.filtered();
                let choice = filtered
                    .get(p.selected.min(filtered.len().saturating_sub(1)))
                    .map(|prov| prov.id());
                if let Some(id) = choice {
                    self.state.active_provider = id.to_string();
                    self.provider_picker = None;
                    self.status_msg = self.state.save().err().map(|e| e.to_string());
                }
            }
            KeyCode::Backspace => {
                p.input.pop();
                p.selected = 0;
            }
            KeyCode::Down => {
                p.selected = (p.selected + 1).min(p.filtered().len().saturating_sub(1));
            }
            KeyCode::Up => p.selected = p.selected.saturating_sub(1),
            KeyCode::Char(c) => {
                p.input.push(c);
                p.selected = 0;
            }
            _ => {}
        }
    }

    /// `n`: spawn a fresh conversation in the same directory as the selected
    /// conversation — "new here", no picker. A no-op when nothing is selected.
    fn new_conversation_here(&mut self) {
        let Some(id) = self.selected_conv_id() else {
            return;
        };
        let Some(dir) = self.state.conversation(&id).map(|c| c.cwd.clone()) else {
            return;
        };
        self.new_conversation_in(dir);
    }

    /// `p`: open the add-directory path prompt, prefilled with `$HOME/`.
    fn open_path_prompt(&mut self) {
        let home = std::env::var("HOME").unwrap_or_default();
        let input = if home.is_empty() {
            String::new()
        } else {
            format!("{home}/")
        };
        let completions = picker::complete_dirs(&input);
        self.path_prompt = Some(PathPrompt {
            input,
            completions,
            selected: 0,
        });
    }

    /// Keys while the path prompt is open: type/Backspace to edit, Tab to
    /// descend into the highlighted match, ▲/▼ to move, Enter to add the
    /// directory to directories.txt and spawn there, Esc to cancel.
    fn handle_path_key(&mut self, code: KeyCode) {
        let Some(p) = &mut self.path_prompt else {
            return;
        };
        match code {
            KeyCode::Esc => self.path_prompt = None,
            KeyCode::Down => {
                if !p.completions.is_empty() {
                    p.selected = (p.selected + 1).min(p.completions.len() - 1);
                }
            }
            KeyCode::Up => p.selected = p.selected.saturating_sub(1),
            KeyCode::Tab => {
                if let Some(dir) = p.completions.get(p.selected) {
                    p.input = format!("{}/", dir.to_string_lossy());
                    p.completions = picker::complete_dirs(&p.input);
                    p.selected = 0;
                }
            }
            KeyCode::Backspace => {
                p.input.pop();
                p.completions = picker::complete_dirs(&p.input);
                p.selected = 0;
            }
            KeyCode::Char(c) => {
                p.input.push(c);
                p.completions = picker::complete_dirs(&p.input);
                p.selected = 0;
            }
            KeyCode::Enter => {
                // Prefer the exact typed path if it is a directory; otherwise
                // take the highlighted completion.
                let typed = PathBuf::from(picker::expand_tilde(&p.input));
                let chosen = if typed.is_dir() {
                    Some(typed)
                } else {
                    p.completions.get(p.selected).cloned()
                };
                if let Some(dir) = chosen {
                    self.path_prompt = None;
                    match picker::add_directory(&dir) {
                        Ok(_) => self.new_conversation_in(dir),
                        Err(e) => self.status_msg = Some(e.to_string()),
                    }
                }
            }
            _ => {}
        }
    }

    /// Spawn a fresh conversation in `dir` with the active provider and swap
    /// it in immediately (D14).
    fn new_conversation_in(&mut self, dir: PathBuf) {
        let result = (|| -> Result<()> {
            let prov = provider::by_id(&self.state.active_provider);
            let id = prov.new_session_id(&dir)?;
            let pane_id = tmux::spawn_conversation(&dir, prov, &id, false)?;
            self.state
                .add_conversation(id.clone(), dir, pane_id, prov.id().to_string());
            self.state.save()?;
            self.refresh();
            // Move the highlight onto the freshly created row so it looks
            // "hovered" immediately, rather than leaving it on the old row.
            if let Some(pos) = self.items.iter().position(
                |it| matches!(it, Item::Conv(idx) if self.state.conversations[*idx].id == id),
            ) {
                self.selected = pos;
            }
            self.view(&id)
        })();
        self.status_msg = result.err().map(|e| e.to_string());
    }

    /// Digit jump (D13): take the client to window N of the selected row's
    /// project's real session, creating session (with its .tmux.sh hook) and
    /// window as needed. Window 1 is the editor window: created running
    /// nvim, and an idle shell there gets `nvim` typed into it — but a busy
    /// foreground process is never disturbed, just focused.
    fn digit_jump(&mut self, n: u8) {
        let Some(id) = self.selected_conv_id() else {
            return;
        };
        let Some(dir) = self.state.conversation(&id).map(|c| c.cwd.clone()) else {
            return;
        };
        let result = (|| -> Result<()> {
            let session = tmux::session_name_for(&dir);
            if !tmux::session_exists(&session) {
                tmux::create_session(&session, &dir)?;
            }
            if !tmux::window_exists(&session, n) {
                let cmd = (n == 1).then_some("nvim");
                tmux::create_window_at(&session, n, &dir, cmd)?;
            } else if n == 1 {
                let cmd = tmux::window_current_command(&session, 1)?;
                if SHELLS.contains(&cmd.as_str()) {
                    tmux::send_line(&session, 1, "nvim")?;
                }
            }
            tmux::select_window(&session, n)?;
            // corc keeps running in its own session.
            tmux::switch_client(&session)
        })();
        self.status_msg = result.err().map(|e| e.to_string());
    }

    /// Move mode `K`/`J` (D9): shift the selected row's project one step in
    /// the persisted display order.
    fn move_project(&mut self, delta: i64) {
        let Some(id) = self.selected_conv_id() else {
            return;
        };
        let Some(project) = self
            .state
            .conversation(&id)
            .map(|c| c.cwd.display().to_string())
        else {
            return;
        };
        let Some(pos) = self.state.projects.iter().position(|p| *p == project) else {
            return;
        };
        let target = pos as i64 + delta;
        if target < 0 || target >= self.state.projects.len() as i64 {
            return;
        }
        self.state.projects.swap(pos, target as usize);
        self.status_msg = self.state.save().err().map(|e| e.to_string());
        self.rebuild_keeping_selection();
    }

    /// Rebuild the item list, keeping the selection on the same conversation
    /// if it is still visible.
    fn rebuild_keeping_selection(&mut self) {
        let keep = self.selected_conv_id();
        self.rebuild_items();
        if let Some(id) = keep
            && let Some(pos) = self.items.iter().position(
                |i| matches!(i, Item::Conv(c) if self.state.conversations[*c].id == id),
            )
        {
            self.selected = pos;
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(f.area());
        self.draw_list(f, outer[0]);
        self.draw_footer(f, outer[1]);
        self.draw_picker(f);
        self.draw_path_prompt(f);
        self.draw_provider_picker(f);
    }

    /// The `s` overlay: an input line plus the matching providers, the active
    /// one marked. Enter switches which provider new conversations use.
    fn draw_provider_picker(&mut self, f: &mut Frame) {
        let Some(p) = &mut self.provider_picker else {
            return;
        };
        let filtered = p.filtered();
        p.selected = p.selected.min(filtered.len().saturating_sub(1));
        let active = self.state.active_provider.clone();

        let area = f.area();
        let rect = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: (filtered.len() as u16 + 3).clamp(4, area.height.saturating_sub(2).max(4)),
        };
        f.render_widget(Clear, rect);
        let block = Block::bordered().title(" switch provider ");
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(inner);
        f.render_widget(Paragraph::new(format!("▸ {}▏", p.input)), rows[0]);

        let items: Vec<ListItem> = filtered
            .iter()
            .map(|prov| {
                let marker = if prov.id() == active { "● " } else { "  " };
                ListItem::new(format!("{marker}{}", prov.display_name()))
            })
            .collect();
        let mut list_state = ListState::default().with_selected(Some(p.selected));
        let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));
        f.render_stateful_widget(list, rows[1], &mut list_state);
    }

    /// The `p` overlay: an input line prefilled with `$HOME/`, plus the
    /// matching subdirectories to Tab through.
    fn draw_path_prompt(&mut self, f: &mut Frame) {
        let Some(p) = &mut self.path_prompt else {
            return;
        };
        if !p.completions.is_empty() {
            p.selected = p.selected.min(p.completions.len() - 1);
        }

        let area = f.area();
        let rect = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: (p.completions.len() as u16 + 3).clamp(4, area.height.saturating_sub(2).max(4)),
        };
        f.render_widget(Clear, rect);
        let block = Block::bordered().title(" add directory ");
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(inner);
        f.render_widget(Paragraph::new(format!("▸ {}▏", display_dir(&p.input))), rows[0]);

        let width = inner.width as usize;
        let items: Vec<ListItem> = p
            .completions
            .iter()
            .map(|d| ListItem::new(truncate(&display_dir(&d.to_string_lossy()), width)))
            .collect();
        let mut list_state = ListState::default().with_selected(Some(p.selected));
        let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));
        f.render_stateful_widget(list, rows[1], &mut list_state);
    }

    /// The `n` overlay (D14): an input line plus the matching directories.
    fn draw_picker(&mut self, f: &mut Frame) {
        let Some(p) = &mut self.picker else {
            return;
        };
        let filtered = p.filtered();
        p.selected = p.selected.min(filtered.len().saturating_sub(1));

        let area = f.area();
        let rect = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: (filtered.len() as u16 + 3)
                .clamp(4, area.height.saturating_sub(2).max(4)),
        };
        f.render_widget(Clear, rect);
        let block = Block::bordered().title(" new conversation ");
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(inner);
        f.render_widget(Paragraph::new(format!("▸ {}▏", p.input)), rows[0]);

        let width = inner.width as usize;
        let items: Vec<ListItem> = filtered
            .iter()
            .map(|&i| ListItem::new(truncate(&display_dir(&p.dirs[i].to_string_lossy()), width)))
            .collect();
        let mut list_state = ListState::default().with_selected(Some(p.selected));
        let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));
        f.render_stateful_widget(list, rows[1], &mut list_state);
    }

    /// A project header: a blank spacer line, then `─ name ────` filled to
    /// the full width (`item_height` agrees on the two rows).
    fn render_header(&self, name: &str, width: usize) -> ListItem<'static> {
        let dim = Style::default().fg(Color::DarkGray);
        let fill = "─".repeat(width.saturating_sub(name.chars().count() + 3));
        ListItem::new(vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled("─ ", dim),
                Span::styled(
                    name.to_string(),
                    Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {fill}"), dim),
            ]),
        ])
    }

    /// A conversation row: `● title………time`, the time right-aligned and
    /// dim. Status colors per D6: Running yellow ●, Unseen blue ●, Idle
    /// gray ●, Dead hollow ○; the viewed conversation's ● is green.
    fn render_conv(&self, idx: usize, i: usize, width: usize, now: u64) -> ListItem<'static> {
        let conv = &self.state.conversations[i];
        let status = self.statuses.get(i).copied().unwrap_or(Status::Dead);
        let (dot, color) = match status {
            Status::Running => ("●", Color::Yellow),
            Status::Unseen => ("●", Color::Blue),
            Status::Idle => ("●", Color::Gray),
            Status::Dead => ("○", Color::Gray),
        };
        let meta = self.metas.meta(&conv.id);
        let title = meta
            .and_then(|m| m.display_title())
            .unwrap_or("(untitled)")
            .to_string();
        let time = status::time_column(status, meta, conv.created_at, now);
        let viewed = self.viewed.as_deref() == Some(conv.id.as_str());
        let color = if viewed { Color::Green } else { color };
        let title_style = if idx == self.selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else if status == Status::Dead {
            Style::default().fg(Color::Gray)
        } else {
            Style::default()
        };
        let dim = Style::default().fg(Color::DarkGray);
        let time_w = time.chars().count();
        let gap = if time_w > 0 { 1 } else { 0 };
        let t = truncate(&title, width.saturating_sub(3 + time_w + gap));
        let pad = width.saturating_sub(3 + t.chars().count() + time_w);
        ListItem::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(dot, Style::default().fg(color)),
            Span::raw(" "),
            Span::styled(t, title_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(time, dim),
        ]))
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let now = state::unix_now();
        let width = area.width as usize;
        let items: Vec<ListItem> = (0..self.items.len())
            .map(|idx| match &self.items[idx] {
                Item::Header(name) => self.render_header(name, width),
                Item::Conv(i) => self.render_conv(idx, *i, width, now),
            })
            .collect();

        self.list_rows = area.height;
        self.list_state.select(Some(self.selected));
        let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));
        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        if self.pending_kill.is_some() {
            let text = "conversation is running — kill? y/n";
            let style = Style::default().fg(Color::Yellow);
            f.render_widget(Paragraph::new(text).style(style), area);
            return;
        }
        if self.move_mode {
            let text = "MOVE — K/J reorder project · esc done";
            let style = Style::default().fg(Color::Yellow);
            f.render_widget(Paragraph::new(text).style(style), area);
            return;
        }
        if self.picker.is_some() {
            let text = "type to filter · enter spawn · esc cancel";
            f.render_widget(
                Paragraph::new(text).style(Style::default().fg(Color::Gray)),
                area,
            );
            return;
        }
        if self.path_prompt.is_some() {
            let text = "type path · tab complete · enter add · esc cancel";
            f.render_widget(
                Paragraph::new(text).style(Style::default().fg(Color::Gray)),
                area,
            );
            return;
        }
        if self.provider_picker.is_some() {
            let text = "type to filter · enter switch · esc cancel";
            f.render_widget(
                Paragraph::new(text).style(Style::default().fg(Color::Gray)),
                area,
            );
            return;
        }
        let text = if self.filter_input {
            format!("/{}▏  (enter: keep, esc: clear)", self.filter)
        } else if let Some(msg) = &self.status_msg {
            format!("error: {msg}")
        } else {
            let filter = if self.filter.is_empty() {
                String::new()
            } else {
                format!("filter: {}  ", self.filter)
            };
            let hidden = if self.hidden > 0 {
                format!("{} hidden (a)  ", self.hidden)
            } else if self.show_all {
                "all (a)  ".to_string()
            } else {
                String::new()
            };
            format!("{filter}{hidden}n/N new · s switch · x kill")
        };
        let style = if self.status_msg.is_some() && !self.filter_input {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Gray)
        };
        f.render_widget(Paragraph::new(text).style(style), area);
    }
}

/// Project group header (D8): directory basename only. A git worktree —
/// detected by `.git` being a *file* with a `gitdir:` pointer — shows as
/// `{repo}/{worktree}`, e.g. `corc/fix-ui`. Branches are never shown.
fn project_display(path: &str) -> String {
    let dir = Path::new(path);
    let base = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    if let Some(repo) = worktree_repo(dir) {
        return format!("{repo}/{base}");
    }
    base
}

/// The main repo's basename if `dir` is a git worktree, else None. A
/// worktree's `.git` is a file `gitdir: <repo>/.git/worktrees/<name>`.
fn worktree_repo(dir: &Path) -> Option<String> {
    let gitfile = dir.join(".git");
    if !std::fs::metadata(&gitfile).ok()?.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&gitfile).ok()?;
    let gitdir = content.strip_prefix("gitdir:")?.trim();
    let (repo_path, _) = gitdir.split_once("/.git/worktrees/")?;
    Some(Path::new(repo_path).file_name()?.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::project_display;
    use std::fs;

    /// D8: basename for plain dirs, `{repo}/{worktree}` for git worktrees.
    #[test]
    fn project_headers() {
        let base = std::env::temp_dir().join("corc-test-project-display");
        let _ = fs::remove_dir_all(&base);

        let plain = base.join("myproj");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(project_display(&plain.to_string_lossy()), "myproj");

        // A normal repo has a .git *directory* — still basename only.
        fs::create_dir_all(plain.join(".git")).unwrap();
        assert_eq!(project_display(&plain.to_string_lossy()), "myproj");

        // A worktree has a .git *file* with a gitdir: pointer.
        let wt = base.join("fix-ui");
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join(".git"),
            "gitdir: /home/hector/Projects/corc/.git/worktrees/fix-ui\n",
        )
        .unwrap();
        assert_eq!(project_display(&wt.to_string_lossy()), "corc/fix-ui");

        let _ = fs::remove_dir_all(&base);
    }
}
