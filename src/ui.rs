//! The sidebar TUI. corc's pane is the sidebar (40 columns, left); the
//! content pane to its right holds either a plain-shell placeholder or the
//! currently viewed conversation's Claude pane, swapped in from the hidden
//! session (ADR-0001).

use crate::provider::{self, MetaStore};
use crate::state::{self, State};
use crate::status::{self, Status};
use crate::{picker, tmux, truncate};
use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
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

/// A menu row's on-screen hitbox: (row, column range, action). Screen
/// coordinates so a mouse click maps straight back to the row.
type MenuHit = (u16, std::ops::Range<u16>, MenuAction);

/// A row in the bottom menu. Each maps to the same action as its keyboard
/// shortcut, so the mouse-only path never diverges from the keys.
#[derive(Clone, Copy)]
enum MenuAction {
    /// The `N` directory picker.
    New,
    /// The `a` show-hidden toggle.
    ToggleHidden,
    /// The `s` provider switch.
    SwitchProvider,
    /// The `?` shortcuts cheat-sheet popup.
    Shortcuts,
}

/// Dead conversations older than this are hidden unless `a` is on (D12).
const HIDE_DEAD_AFTER_SECS: u64 = 7 * 24 * 3600;
/// Grace period before an empty conversation the user left is discarded
/// (D17). A message sent an instant before leaving can still be flushing to
/// disk — Cursor lags noticeably — so we wait and re-check emptiness rather
/// than discarding on the spot.
const DISCARD_GRACE: Duration = Duration::from_secs(30);
/// Most recent conversations shown per project before the rest are hidden
/// (D13) — the `a` toggle reveals them. Keeps each project's list short.
const MAX_PER_PROJECT: usize = 7;
/// How often corc force-repaints its whole pane, and the input poll timeout.
/// corc gets no event when an *adjacent* pane scrolls — which is exactly what
/// makes tmux/wezterm leave stale glyphs in corc's pane (D23) — so the only
/// fix is a timed full repaint. 10 Hz on a 40-column pane is a few KB/s.
const REPAINT_INTERVAL: Duration = Duration::from_millis(100);

/// Background tint marking the conversation currently in the content pane — a
/// muted blue, distinct from the gray hover highlight so the active row reads
/// apart from the merely-selected one. Frees its dot to show real status (D6)
/// instead of a green "you are here" marker.
const VIEWED_BG: Color = Color::Rgb(38, 50, 71);

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
    items: Vec<Item>,
    selected: usize,
    /// When Some, the j/k cursor sits on this bottom-menu row instead of the
    /// list — reached by pressing j past the last conversation.
    menu_sel: Option<usize>,
    /// Pending vim-style count prefix: `3j` moves three rows. Digits
    /// accumulate here until a motion consumes them or another key clears it.
    count: Option<usize>,
    filter: String,
    filter_input: bool,
    /// Conversation id awaiting the `y/n` kill confirmation (`x` on a
    /// Running conversation, D12).
    pending_kill: Option<String>,
    /// `a` toggle: also show Dead conversations older than a week (D12).
    show_all: bool,
    /// How many week-old Dead conversations the current list is hiding.
    hidden: usize,
    /// The `s` provider-switch overlay, when open. The `N` directory picker
    /// (which now folds in the add-directory prompt) is no longer an inline
    /// overlay — it runs as a centered `tmux display-popup` process (D22).
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
    /// On-screen hitboxes of the bottom menu buttons, rebuilt every draw so a
    /// click at `(col, row)` maps back to the button's action.
    menu_hitboxes: Vec<MenuHit>,
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

    // Install the M-1..M-9 digit-jump bindings at runtime (D13) so 1-9 work
    // from inside the Claude pane too, without ever editing the user's tmux
    // config. Best-effort: a missing exe path just leaves the sidebar's keys.
    if let Ok(exe) = std::env::current_exe() {
        tmux::install_jump_bindings(&exe.to_string_lossy());
    }

    let placeholder_pane = tmux::split_content_pane(&sidebar_pane)?;

    let mut app = App {
        state,
        metas: MetaStore::new()?,
        statuses: Vec::new(),
        sidebar_pane,
        placeholder_pane,
        viewed: None,
        items: Vec::new(),
        selected: 0,
        menu_sel: None,
        count: None,
        filter: String::new(),
        filter_input: false,
        pending_kill: None,
        show_all: false,
        hidden: 0,
        provider_picker: None,
        move_mode: false,
        pending_discard: Vec::new(),
        list_state: ListState::default(),
        list_rows: 0,
        status_msg: None,
        last_refresh: Instant::now(),
        menu_hitboxes: Vec::new(),
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
    // Respect the normal grace period on shutdown too. A message sent just
    // before Ctrl+C may still be flushing, especially for Cursor; keeping a
    // genuinely empty row is safer than deleting a real conversation.
    app.process_pending_discards();
    if tmux::pane_exists(&app.placeholder_pane) {
        let _ = tmux::kill_pane(&app.placeholder_pane);
    }
    // Put the plain Alt+number window switch back, matching the user's config.
    tmux::restore_window_bindings();
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
            // Force a full repaint every frame, not a diff. corc's diff-based
            // draw leaves cells it considers unchanged untouched, so glyphs an
            // adjacent scrolling pane bled into corc's pane (D23) would linger.
            // `clear()` makes tmux resend every cell; wrapping the clear+draw in
            // a synchronized update (DEC 2026, honored by tmux 3.4+/wezterm)
            // presents them as one frame so the blank clear never flashes. On a
            // terminal that ignores 2026 the sequences are harmless no-ops.
            execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
            terminal.clear()?;
            terminal.draw(|f| self.draw(f))?;
            execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;

            if event::poll(REPAINT_INTERVAL)? {
                match event::read()? {
                    Event::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
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
        // The provider-switch picker owns the keyboard while open.
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
        // Ctrl+d / Ctrl+u: hop one project group down / up (feature request).
        if mods.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('d') => self.jump_project(1),
                KeyCode::Char('u') => self.jump_project(-1),
                _ => {}
            }
            self.count = None;
            return false;
        }
        // Vim-style count prefix: bare digits accumulate a repeat count that
        // the next motion consumes (`3j`, `4k`). A leading 0 is not a count.
        // Window-jump lives on Alt+1..9, handled by tmux, so the digits are
        // free here.
        if let KeyCode::Char(c @ '0'..='9') = code
            && !(c == '0' && self.count.is_none())
        {
            let d = (c as u8 - b'0') as usize;
            self.count = Some(self.count.unwrap_or(0).saturating_mul(10).saturating_add(d));
            return false;
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                let n = self.take_count();
                self.move_selection(1, n);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let n = self.take_count();
                self.move_selection(-1, n);
            }
            KeyCode::Char('g') | KeyCode::Home => self.select_edge(true),
            KeyCode::Char('G') | KeyCode::End => self.select_edge(false),
            KeyCode::Char('/') => {
                self.menu_sel = None;
                self.filter_input = true;
            }
            KeyCode::Esc if self.menu_sel.is_some() => self.menu_sel = None,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.rebuild_items();
            }
            KeyCode::Enter => match self.menu_sel {
                Some(i) => {
                    if let Some((action, ..)) = self.menu_entries().into_iter().nth(i) {
                        self.activate_menu(action);
                    }
                }
                None => self.view_selected(),
            },
            KeyCode::Char('n') => self.new_conversation_here(),
            KeyCode::Char('N') => self.open_picker(),
            KeyCode::Char('s') => self.open_provider_picker(),
            KeyCode::Char('x') => self.kill_or_remove(),
            KeyCode::Char('V') => self.move_mode = true,
            KeyCode::Char('a') => {
                self.show_all = !self.show_all;
                self.rebuild_keeping_selection();
            }
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('?') => self.show_shortcuts(),
            _ => {}
        }
        // Any non-digit key ends a dangling count prefix.
        self.count = None;
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
        if self.provider_picker.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => self.select_next(1),
            MouseEventKind::ScrollUp => self.select_next(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                // A click on a bottom-menu button fires its action; the menu
                // sits below the list, so this is checked before the row math.
                if let Some(action) = self.menu_hit(mouse.column, mouse.row) {
                    self.activate_menu(action);
                    return;
                }
                // Rows map to items through the list's persistent scroll
                // offset.
                if mouse.row >= self.list_rows {
                    return; // footer / below the list
                }
                if let Some(idx) = self.item_at_row(mouse.row)
                    && matches!(self.items.get(idx), Some(Item::Conv(_)))
                {
                    self.menu_sel = None;
                    self.selected = idx;
                    self.view_selected();
                }
            }
            _ => {}
        }
    }

    /// Conversations and their persisted in-flight starts, fanned out to the
    /// matching provider metadata source.
    fn known_conversations(&self) -> Vec<(String, PathBuf, &'static str, Option<u64>)> {
        self.state
            .conversations
            .iter()
            .map(|c| {
                (
                    c.id.clone(),
                    c.cwd.clone(),
                    provider::by_id(&c.provider).id(),
                    c.turn_started_at,
                )
            })
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

        // Keep only an in-flight turn start in state.json. Cursor's local
        // transcript normally supplies the exact prompt time; persisting it
        // here preserves the elapsed clock if corc restarts before completion.
        for conv in &mut self.state.conversations {
            let Some(meta) = self.metas.meta(&conv.id) else {
                continue;
            };
            let started = (meta.turn_state == crate::discovery::TurnState::Mid)
                .then_some(meta.turn_started_at)
                .flatten();
            if conv.turn_started_at != started {
                conv.turn_started_at = started;
                dirty = true;
            }
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

        if dirty {
            let _ = self.state.save();
        }

        self.rebuild_keeping_selection();
    }

    fn rebuild_items(&mut self) {
        let filter = self.filter.to_lowercase();
        let now = state::unix_now();
        self.items.clear();
        self.hidden = 0;

        for project in &self.state.projects {
            // Conversations of this project in a fixed order: newest created
            // at the top, never re-sorted (D9). `created_at` is immutable, so
            // a row never moves once placed — status flips and fresh activity
            // leave the order untouched, letting the user keep their bearings.
            let mut indices: Vec<usize> = self
                .state
                .conversations
                .iter()
                .enumerate()
                .filter(|(_, c)| c.cwd.display().to_string() == *project)
                .map(|(i, _)| i)
                .collect();
            indices.sort_by(|&a, &b| {
                let (ca, cb) = (&self.state.conversations[a], &self.state.conversations[b]);
                cb.created_at
                    .cmp(&ca.created_at)
                    .then_with(|| ca.id.cmp(&cb.id))
            });

            let name = project_display(project);
            let mut kept = Vec::new();
            for i in indices {
                // Week-old Dead conversations stay out of the list unless
                // the `a` toggle is on (D12) — the list stays short by itself.
                if !self.show_all && self.statuses.get(i) == Some(&Status::Dead) {
                    let conv = &self.state.conversations[i];
                    let age = now.saturating_sub(status::last_active_ts(
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
            // Cap each project to its most-recently-active conversations
            // (D13). Membership follows activity, but the survivors stay in
            // the fixed creation order for display: rank a copy by activity,
            // keep the top `MAX_PER_PROJECT`, then drop the rest from `kept`
            // without disturbing its order. The `a` toggle and an active
            // filter both bypass the cap.
            if !self.show_all && filter.is_empty() && kept.len() > MAX_PER_PROJECT {
                let mut ranked = kept.clone();
                ranked.sort_by(|&a, &b| {
                    let act = |i: usize| {
                        let c = &self.state.conversations[i];
                        status::last_active_ts(self.metas.meta(&c.id), c.created_at)
                    };
                    act(b).cmp(&act(a))
                });
                ranked.truncate(MAX_PER_PROJECT);
                self.hidden += kept.len() - MAX_PER_PROJECT;
                kept.retain(|i| ranked.contains(i));
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

    /// Consume the pending vim count, defaulting to 1 when none was typed.
    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1).max(1)
    }

    /// Move the j/k cursor `count` steps in `dir` (±1) over the combined
    /// space: conversation rows first, then the bottom-menu rows. The count is
    /// clamped so a stray large prefix (`999j`) can't spin.
    fn move_selection(&mut self, dir: i64, count: usize) {
        let span = self.items.len() + self.menu_entries().len();
        for _ in 0..count.min(span.max(1)) {
            self.nav(dir);
        }
    }

    /// One j/k step. Moving down past the last conversation crosses into the
    /// bottom menu; moving up from its top row crosses back out.
    fn nav(&mut self, dir: i64) {
        let menu_last = self.menu_entries().len() as i64 - 1;
        match self.menu_sel {
            Some(i) => {
                let next = i as i64 + dir;
                if next < 0 {
                    // Back into the list — unless it has no conversation rows
                    // to land on (the cursor would vanish).
                    if self.items.iter().any(|it| matches!(it, Item::Conv(_))) {
                        self.menu_sel = None;
                    }
                } else {
                    self.menu_sel = Some(next.min(menu_last) as usize);
                }
            }
            None if dir > 0 && self.at_last_conv() => self.menu_sel = Some(0),
            None => self.select_next(dir),
        }
    }

    /// Whether the cursor sits on the last conversation row (or the list has
    /// none) — the point where j crosses into the bottom menu.
    fn at_last_conv(&self) -> bool {
        self.items
            .iter()
            .rposition(|i| matches!(i, Item::Conv(_)))
            .is_none_or(|p| p == self.selected)
    }

    /// The item index of the first conversation in each project group, in
    /// screen order — the landing spots for the Ctrl+d/u folder hop.
    fn project_starts(&self) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut expect_first = false;
        for (i, item) in self.items.iter().enumerate() {
            match item {
                Item::Header(_) => expect_first = true,
                Item::Conv(_) if expect_first => {
                    starts.push(i);
                    expect_first = false;
                }
                Item::Conv(_) => {}
            }
        }
        starts
    }

    /// Ctrl+d/u: move the selection to the first conversation of the next or
    /// previous project group, clamping at the ends.
    fn jump_project(&mut self, dir: i64) {
        self.menu_sel = None;
        let starts = self.project_starts();
        if starts.is_empty() {
            return;
        }
        let cur = starts
            .iter()
            .rposition(|&s| s <= self.selected)
            .unwrap_or(0);
        let target = (cur as i64 + dir).clamp(0, starts.len() as i64 - 1) as usize;
        self.selected = starts[target];
    }

    fn select_edge(&mut self, top: bool) {
        self.menu_sel = None;
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
            // `.get`, not `[*i]`: right after a conversation is removed from
            // state the item list is briefly stale (rebuild_keeping_selection
            // reads the old selection before rebuilding), so a stale index can
            // outrun the shrunk Vec — indexing it would panic and take the TUI
            // down. A miss just means "nothing to keep".
            Item::Conv(i) => Some(self.state.conversations.get(*i)?.id.clone()),
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
        if let Some(pos) = self
            .items
            .iter()
            .position(|i| matches!(i, Item::Conv(idx) if self.state.conversations[*idx].id == id))
        {
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

    /// Whether provider metadata positively says the conversation contains no
    /// real exchange. Titles are presentation only and may arrive much later.
    fn is_empty_conversation(&self, id: &str) -> bool {
        self.metas
            .meta(id)
            .is_none_or(|meta| !meta.has_content)
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
        self.state.prune_empty_projects();
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
                self.state.prune_empty_projects();
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

    /// `N`: pick a project directory in a centered popup and spawn there
    /// (D14, D22). The popup (`corc pick-dir`) returns the chosen directory —
    /// either one already in the list or a new one typed into its "add
    /// directory" escape hatch. The spawn and the state write happen here, so
    /// the TUI stays the sole writer of state.json; recording is idempotent, so
    /// an already-listed directory is a no-op and a new one is added (D20).
    fn open_picker(&mut self) {
        if let Some(dir) = self.popup_choice("pick-dir") {
            self.state.add_directory(&dir);
            self.new_conversation_in(dir);
        }
    }

    /// Run a picker subcommand (`pick-dir`) in a centered
    /// `tmux display-popup`, blocking until it closes, and return the chosen
    /// path. The subcommand writes its choice to a temp file we then read —
    /// `display-popup -E` can't hand stdout back to the caller (D22). None on
    /// cancel, on a missing/old tmux, or when nothing was chosen.
    fn popup_choice(&mut self, subcmd: &str) -> Option<PathBuf> {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                self.status_msg = Some(e.to_string());
                return None;
            }
        };
        let out = std::env::temp_dir().join(format!("corc-pick-{}", state::new_uuid().ok()?));
        let cmd = format!("'{}' {subcmd} --out '{}'", exe.display(), out.display());
        let status = std::process::Command::new("tmux")
            .args(["display-popup", "-E", "-B", "-w", "50%", "-h", "40%", &cmd])
            .status();
        let choice = std::fs::read_to_string(&out)
            .ok()
            .map(|s| s.trim().to_string());
        let _ = std::fs::remove_file(&out);
        match status {
            Ok(s) if s.success() => {}
            Ok(_) => return None,
            Err(e) => {
                self.status_msg = Some(format!("popup failed: {e}"));
                return None;
            }
        }
        choice.filter(|s| !s.is_empty()).map(PathBuf::from)
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
            && let Some(pos) = self
                .items
                .iter()
                .position(|i| matches!(i, Item::Conv(c) if self.state.conversations[*c].id == id))
        {
            self.selected = pos;
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        // One row per menu entry plus the rule above them.
        let menu_h = self.menu_entries().len() as u16 + 1;
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(menu_h),
            ])
            .split(f.area());
        self.draw_list(f, outer[0]);
        self.draw_footer(f, outer[1]);
        self.draw_menu(f, outer[2]);
        self.draw_provider_picker(f);
    }

    /// The bottom-menu rows, top to bottom: (action, marker, label, key hint).
    /// The count of week-old dead rows currently folded away lives on the
    /// Hidden row; the provider row always names the active agent.
    fn menu_entries(&self) -> Vec<(MenuAction, &'static str, String, &'static str)> {
        let provider = provider::by_id(&self.state.active_provider).display_name();
        let hidden = if self.hidden > 0 {
            format!("Hidden ({})", self.hidden)
        } else {
            "Hidden".to_string()
        };
        vec![
            (MenuAction::New, "+", "New conversation".to_string(), "N"),
            (
                MenuAction::ToggleHidden,
                if self.show_all { "●" } else { "○" },
                hidden,
                "a",
            ),
            (MenuAction::SwitchProvider, "⇄", provider.to_string(), "s"),
            (MenuAction::Shortcuts, "?", "Shortcuts".to_string(), "?"),
        ]
    }

    /// The bottom menu: a dim rule, then one quiet row per action — marker and
    /// label left, key hint right-aligned — echoing the conversation rows
    /// instead of shouting like a button bar. j past the last conversation
    /// walks the cursor in; the cursor row carries the same gray highlight as
    /// the list. The provider row is tinted with the active agent's accent and
    /// the Hidden marker fills in while its `a` toggle is on.
    fn draw_menu(&mut self, f: &mut Frame, area: Rect) {
        let width = area.width as usize;
        let dim = Style::default().fg(Color::DarkGray);
        let mut lines = vec![Line::from(Span::styled("─".repeat(width), dim))];
        self.menu_hitboxes.clear();
        for (i, (action, marker, label, hint)) in self.menu_entries().into_iter().enumerate() {
            let selected = self.menu_sel == Some(i);
            let fg = match action {
                MenuAction::SwitchProvider => provider::accent(&self.state.active_provider),
                MenuAction::ToggleHidden if self.show_all => Color::Rgb(122, 162, 247),
                _ => Color::Rgb(206, 211, 221),
            };
            let mut row = Style::default().fg(fg);
            let mut hint_style = dim;
            if selected {
                row = row.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
                hint_style = Style::default().fg(Color::Gray).bg(Color::DarkGray);
            }
            let text = format!(" {marker} {label}");
            let pad = width.saturating_sub(text.chars().count() + hint.chars().count() + 1);
            lines.push(Line::from(vec![
                Span::styled(text, row),
                Span::styled(" ".repeat(pad), row),
                Span::styled(format!("{hint} "), hint_style),
            ]));
            let hit_row = area.y + 1 + i as u16;
            self.menu_hitboxes
                .push((hit_row, area.x..area.x + area.width, action));
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    /// The menu button under `(col, row)`, if any.
    fn menu_hit(&self, col: u16, row: u16) -> Option<MenuAction> {
        self.menu_hitboxes
            .iter()
            .find(|(r, range, _)| *r == row && range.contains(&col))
            .map(|(_, _, a)| *a)
    }

    /// Run a menu button's action — the same entry points its keyboard
    /// shortcut uses.
    fn activate_menu(&mut self, action: MenuAction) {
        match action {
            MenuAction::New => self.open_picker(),
            MenuAction::ToggleHidden => {
                self.show_all = !self.show_all;
                self.rebuild_items();
            }
            MenuAction::SwitchProvider => self.open_provider_picker(),
            MenuAction::Shortcuts => self.show_shortcuts(),
        }
    }

    /// Show the keyboard cheat-sheet in a centered `tmux display-popup`,
    /// reachable by `?` or the menu's `?` button. Blocks until the popup is
    /// dismissed, like the directory picker (D22); a missing/old tmux just
    /// surfaces an error in the footer.
    fn show_shortcuts(&mut self) {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                self.status_msg = Some(e.to_string());
                return;
            }
        };
        let cmd = format!("'{}' shortcuts", exe.display());
        let status = std::process::Command::new("tmux")
            .args([
                "display-popup",
                "-E",
                "-T",
                " corc shortcuts ",
                "-w",
                "64",
                "-h",
                "80%",
                &cmd,
            ])
            .status();
        if let Err(e) = status {
            self.status_msg = Some(format!("popup failed: {e}"));
        }
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
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {fill}"), dim),
            ]),
        ])
    }

    /// A conversation row: `● title………time`, the time right-aligned and
    /// dim. Status colors per D6: Running yellow ●, Unseen blue ●, Idle
    /// gray ●, Dead hollow ○. The viewed conversation carries a blue row
    /// background (`VIEWED_BG`) rather than a recolored dot, so its dot keeps
    /// showing status like any other row.
    fn render_conv(
        &self,
        idx: usize,
        i: usize,
        width: usize,
        now: u64,
        multi_provider: bool,
    ) -> ListItem<'static> {
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
        // Tint the title by which agent CLI spawned it (D6 addition) — but
        // only when the list actually mixes providers, so a single-provider
        // setup keeps its original untinted look. Dead rows stay gray:
        // deadness reads louder than provider.
        let accent = multi_provider.then(|| provider::accent(&conv.provider));
        let base = match accent {
            Some(c) => Style::default().fg(c),
            None => Style::default(),
        };
        let title_style = if idx == self.selected {
            base.add_modifier(Modifier::BOLD)
        } else if status == Status::Dead {
            Style::default().fg(Color::Gray)
        } else {
            base
        };
        let dim = Style::default().fg(Color::DarkGray);
        // The selected row uses DarkGray as its hover background, so the
        // normally dim time needs a lighter foreground to stay readable.
        let time_style = if self.menu_sel.is_none() && idx == self.selected {
            Style::default().fg(Color::Gray)
        } else {
            dim
        };
        let time_w = time.chars().count();
        let gap = if time_w > 0 { 1 } else { 0 };
        let t = truncate(&title, width.saturating_sub(3 + time_w + gap));
        let pad = width.saturating_sub(3 + t.chars().count() + time_w);
        let item = ListItem::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(dot, Style::default().fg(color)),
            Span::raw(" "),
            Span::styled(t, title_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(time, time_style),
        ]));
        // The row background marks the active conversation. When it is also
        // the selected row the list's gray hover highlight patches over this,
        // which is fine — the cursor's position wins while it sits here.
        if viewed {
            item.style(Style::default().bg(VIEWED_BG))
        } else {
            item
        }
    }

    /// Whether the tracked conversations span more than one provider. The
    /// provider title tints only apply when they do, so a setup that uses a
    /// single agent looks exactly as it did before the tints existed.
    fn uses_multiple_providers(&self) -> bool {
        let mut seen: Option<&str> = None;
        for c in &self.state.conversations {
            match seen {
                Some(p) if p != c.provider => return true,
                _ => seen = Some(&c.provider),
            }
        }
        false
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let now = state::unix_now();
        let width = area.width as usize;
        let multi = self.uses_multiple_providers();
        let items: Vec<ListItem> = (0..self.items.len())
            .map(|idx| match &self.items[idx] {
                Item::Header(name) => self.render_header(name, width),
                Item::Conv(i) => self.render_conv(idx, *i, width, now, multi),
            })
            .collect();

        self.list_rows = area.height;
        // While the cursor is down in the bottom menu the list drops its
        // highlight, so exactly one row on screen ever reads as selected.
        self.list_state.select(match self.menu_sel {
            Some(_) => None,
            None => Some(self.selected),
        });
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
            // The key hints now live on the menu buttons (and the `?` popup),
            // and the hidden count sits on the Hidden button — so the idle
            // footer only echoes an active filter and a pending vim count
            // prefix (`3j`), often nothing at all.
            let filter = if self.filter.is_empty() {
                String::new()
            } else {
                format!("filter: {}  ", self.filter)
            };
            let count = match self.count {
                Some(n) => format!("{n}  "),
                None => String::new(),
            };
            format!("{count}{filter}")
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
    Some(
        Path::new(repo_path)
            .file_name()?
            .to_string_lossy()
            .into_owned(),
    )
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
