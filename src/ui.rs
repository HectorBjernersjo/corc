//! The sidebar TUI. corc's pane is the sidebar (40 columns, left); the
//! content pane to its right holds either a plain-shell placeholder or the
//! currently viewed conversation's Claude pane, swapped in from the hidden
//! session (ADR-0001).

use crate::discovery::Store;
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

struct App {
    state: State,
    store: Store,
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
    /// The `n` directory-picker overlay, when open (D14).
    picker: Option<Picker>,
    /// Move mode (D9): `K`/`J` reorder the selected row's project.
    move_mode: bool,
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

    let mut state = State::load()?;
    reconcile(&mut state)?;
    state.save()?;

    let placeholder_pane = tmux::split_content_pane(&sidebar_pane)?;

    let mut app = App {
        state,
        store: Store::new()?,
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
        move_mode: false,
        list_state: ListState::default(),
        list_rows: 0,
        status_msg: None,
        last_refresh: Instant::now(),
    };
    app.refresh();

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?;

    let result = app.event_loop(&mut terminal);

    // Swap the viewed pane home and remove the content pane we created (D10).
    app.park();
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
            KeyCode::Char('q') => return true,
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
            KeyCode::Char('n') => self.open_picker(),
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

    /// Mouse (D11): click a row = select + view; the wheel moves the
    /// selection. Ignored while the picker overlay is open.
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.picker.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => self.select_next(1),
            MouseEventKind::ScrollUp => self.select_next(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                // Row 0 is the list title; content rows map through the
                // list's persistent scroll offset.
                let Some(row) = mouse.row.checked_sub(1) else {
                    return;
                };
                if row >= self.list_rows {
                    return; // footer / below the list
                }
                let idx = self.list_state.offset() + row as usize;
                if matches!(self.items.get(idx), Some(Item::Conv(_))) {
                    self.selected = idx;
                    self.view_selected();
                }
            }
            _ => {}
        }
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
        } else if self.viewed.is_none() && !tmux::pane_exists(&self.placeholder_pane) {
            // Someone closed the placeholder shell; put it back.
            if let Ok(pane) = tmux::split_content_pane(&self.sidebar_pane) {
                self.placeholder_pane = pane;
            }
        }

        let known: Vec<(String, PathBuf)> = self
            .state
            .conversations
            .iter()
            .map(|c| (c.id.clone(), c.cwd.clone()))
            .collect();
        if let Err(e) = self.store.refresh(&known) {
            self.status_msg = Some(e.to_string());
        }

        // The conversation in the content pane counts as continuously
        // viewed (D6): its last_viewed follows along in memory and is
        // persisted on swap and quit.
        if let Some(id) = self.viewed.clone()
            && let Some(c) = self.state.conversation_mut(&id)
        {
            c.last_viewed = state::unix_now();
        }

        let viewed = self.viewed.clone();
        self.statuses = self
            .state
            .conversations
            .iter()
            .map(|c| {
                status::derive(
                    c.pane_id.is_some(),
                    self.store.meta(&c.id),
                    c.last_viewed,
                    viewed.as_deref() == Some(c.id.as_str()),
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
            let activity = status::last_activity(self.store.meta(&conv.id), conv.created_at);
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
                        self.store.meta(&conv.id),
                        conv.created_at,
                    ));
                    if age > HIDE_DEAD_AFTER_SECS {
                        self.hidden += 1;
                        continue;
                    }
                }
                if !filter.is_empty() {
                    let title = self
                        .store
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
                let pane = tmux::spawn_conversation(&conv.cwd, id, true)?;
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

    /// `n`: open the directory-picker overlay (D14).
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

    /// Spawn a fresh conversation in `dir` and swap it in immediately (D14).
    fn new_conversation_in(&mut self, dir: PathBuf) {
        let result = (|| -> Result<()> {
            let id = state::new_uuid()?;
            let pane_id = tmux::spawn_conversation(&dir, &id, false)?;
            self.state.add_conversation(id.clone(), dir, pane_id);
            self.state.save()?;
            self.refresh();
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

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let now = state::unix_now();
        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(idx, item)| match item {
                Item::Header(name) => ListItem::new(Line::from(Span::styled(
                    name.clone(),
                    Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
                ))),
                Item::Conv(i) => {
                    let conv = &self.state.conversations[*i];
                    let status = self.statuses.get(*i).copied().unwrap_or(Status::Dead);
                    // The four looks (D6): Running yellow ●, Unseen blue ●,
                    // Idle gray ●, Dead hollow ○.
                    let (dot, color) = match status {
                        Status::Running => ("●", Color::Yellow),
                        Status::Unseen => ("●", Color::Blue),
                        Status::Idle => ("●", Color::Gray),
                        Status::Dead => ("○", Color::Gray),
                    };
                    let meta = self.store.meta(&conv.id);
                    let title = meta
                        .and_then(|m| m.display_title())
                        .unwrap_or("(untitled)");
                    let time = status::time_column(status, meta, conv.created_at, now);
                    let title_style = if idx == self.selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else if status == Status::Dead {
                        Style::default().fg(Color::Gray)
                    } else {
                        Style::default()
                    };
                    let marker = if self.viewed.as_deref() == Some(conv.id.as_str()) {
                        "▸ "
                    } else {
                        "  "
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(marker),
                        Span::styled(dot, Style::default().fg(color)),
                        Span::raw(" "),
                        Span::styled(format!("{time:>3} "), Style::default().fg(Color::Gray)),
                        Span::styled(truncate(title, 32), title_style),
                    ]))
                }
            })
            .collect();

        let running = self
            .statuses
            .iter()
            .filter(|s| **s == Status::Running)
            .count();
        let title = format!(" corc — {running} running ");
        // The title takes the block's first row; what remains is content.
        self.list_rows = area.height.saturating_sub(1);
        self.list_state.select(Some(self.selected));
        let list = List::new(items)
            .block(Block::default().title(title))
            .highlight_style(Style::default().bg(Color::DarkGray));
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
            format!("{filter}{hidden}enter view · n new · x kill · q quit")
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
