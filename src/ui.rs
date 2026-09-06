//! The sidebar TUI. corc's pane is the sidebar (40 columns, left); the
//! content pane to its right holds either a plain-shell placeholder or the
//! currently viewed conversation's agent pane, swapped in from the hidden
//! session (ADR-0001).

use crate::discovery::Known;
use crate::hooks;
use crate::provider::{self, MetaStore};
use crate::repo;
use crate::state::{self, State};
use crate::status::{self, Status};
use crate::{browser, cd, picker, tmux, truncate, usage};
use anyhow::{Context, Result};
use ratatui::backend::Backend;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

enum Item {
    Header(String),
    /// Conversation id. Deliberately not an index into `state.conversations`:
    /// removing a conversation shifts every later index, so an index held by a
    /// row from the previous frame silently starts naming a *different*
    /// conversation — which used to fling the cursor onto an unrelated row when
    /// one was deleted. An id either still resolves or does not.
    Conv(String),
}

/// Row index of a conversation in the canonical list, or None when no row
/// holds it — hidden by the history window or the per-project cap, or gone
/// from state entirely. A miss leaves a cursor where it is; it must never
/// resolve to some other conversation's row.
fn item_pos(items: &[Item], id: &str) -> Option<usize> {
    items
        .iter()
        .position(|item| matches!(item, Item::Conv(row) if row == id))
}

/// The three vertically stacked keyboard-navigation regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Panel {
    Attention,
    Conversations,
    Menu,
}

/// Ctrl+j/k moves one whole region at a time, skipping regions that currently
/// have no selectable rows and clamping at the first/last available region.
fn adjacent_panel(current: Panel, dir: i64, has_attention: bool, has_conversations: bool) -> Panel {
    let mut panels = Vec::with_capacity(3);
    if has_attention {
        panels.push(Panel::Attention);
    }
    if has_conversations {
        panels.push(Panel::Conversations);
    }
    panels.push(Panel::Menu);
    let current = panels.iter().position(|p| *p == current).unwrap_or(0);
    panels[(current as i64 + dir).clamp(0, panels.len() as i64 - 1) as usize]
}

/// Give attention enough rows for its rule plus content while keeping at
/// least one row for the canonical conversation list. On extremely short
/// terminals there is no useful two-row panel, so it yields the space.
fn attention_panel_height(count: usize, content_height: u16) -> u16 {
    if count == 0 || content_height < 3 {
        return 0;
    }
    let max = (content_height / 2)
        .max(2)
        .min(content_height.saturating_sub(1));
    (count as u16 + 1).min(max)
}

/// Derived pane status per conversation id. Keyed rather than a Vec parallel
/// to `state.conversations`: a parallel Vec has to be hand-patched on every
/// removal to stay aligned, and one missed patch reports another
/// conversation's status.
type Statuses = HashMap<String, Status>;

/// A conversation's status from the last derivation. An id the map has never
/// seen is Dead: a row can outlive one refresh, and a conversation that is not
/// there has no live pane by definition.
fn status_of(statuses: &Statuses, id: &str) -> Status {
    statuses.get(id).copied().unwrap_or(Status::Dead)
}

/// Build the top-panel rows. Pins are always included; rows then sort by dot
/// color — blue (Question/Unseen) first, then yellow (Running), then the pink
/// pins — and each group follows project order and newest-created-first order
/// for a stable, predictable list.
fn attention_ids(state: &State, statuses: &Statuses) -> Vec<String> {
    let status = |conversation: &state::Conversation| status_of(statuses, &conversation.id);
    let mut rows: Vec<&state::Conversation> = state
        .conversations
        .iter()
        .filter(|conversation| {
            conversation.pinned
                || matches!(
                    status(conversation),
                    Status::Running | Status::Question | Status::Unseen
                )
        })
        .collect();
    let project_rank = |conversation: &state::Conversation| {
        state
            .projects
            .iter()
            .position(|project| *project == conversation.cwd.display().to_string())
            .unwrap_or(usize::MAX)
    };
    // Blue, then yellow, then pink — matching the dot colors the eye scans by.
    let color_rank = |conversation: &state::Conversation| match status(conversation) {
        Status::Question | Status::Unseen => 0,
        Status::Running => 1,
        _ => 2,
    };
    rows.sort_by(|ca, cb| {
        color_rank(ca).cmp(&color_rank(cb)).then_with(|| {
            project_rank(ca)
                .cmp(&project_rank(cb))
                .then_with(|| cb.created_at.cmp(&ca.created_at))
                .then_with(|| ca.id.cmp(&cb.id))
        })
    });
    rows.into_iter()
        .map(|conversation| conversation.id.clone())
        .collect()
}

/// When the cursor lands on a project's first conversation, keep that
/// project's complete two-line header (spacer + name) immediately above it.
/// Ratatui normally considers only the selected item mandatory and can leave
/// the offset on the conversation itself, stripping away its group context.
fn keep_first_conversation_context_visible(
    items: &[Item],
    selected: usize,
    viewport_height: u16,
    state: &mut ListState,
) {
    let Some(header) = selected.checked_sub(1) else {
        return;
    };
    if viewport_height >= 3
        && state.selected() == Some(selected)
        && matches!(items.get(header), Some(Item::Header(_)))
        && state.offset() > header
    {
        *state.offset_mut() = header;
    }
}

/// Update the list highlight without throwing away its viewport. Ratatui's
/// `ListState::select(None)` also resets `offset` to zero, which makes the
/// conversation list jump to the top as soon as focus enters the menu.
fn set_list_highlight(state: &mut ListState, selected: Option<usize>) {
    match selected {
        Some(index) => state.select(Some(index)),
        None => *state.selected_mut() = None,
    }
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
    /// The `a` history-window cycle.
    CycleHistory,
    /// The `s` provider switch.
    SwitchProvider,
    /// The `?` shortcuts cheat-sheet popup.
    Shortcuts,
}

/// Which conversations remain visible in history. The Active option shows
/// only conversations with a live tmux pane; the age windows add progressively
/// older Dead conversations. `a` cycles through these in order. The choice is
/// persisted in `state.json`, so it survives a corc restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryWindow {
    Active,
    ThreeHours,
    OneDay,
    ThreeDays,
    #[default]
    OneWeek,
    AllTime,
}

impl HistoryWindow {
    fn next(self) -> Self {
        match self {
            Self::Active => Self::ThreeHours,
            Self::ThreeHours => Self::OneDay,
            Self::OneDay => Self::ThreeDays,
            Self::ThreeDays => Self::OneWeek,
            Self::OneWeek => Self::AllTime,
            Self::AllTime => Self::Active,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ThreeHours => "3h",
            Self::OneDay => "1D",
            Self::ThreeDays => "3D",
            Self::OneWeek => "1W",
            Self::AllTime => "all time",
        }
    }

    fn cutoff_secs(self) -> Option<u64> {
        match self {
            Self::Active => Some(0),
            Self::ThreeHours => Some(3 * 3600),
            Self::OneDay => Some(24 * 3600),
            Self::ThreeDays => Some(3 * 24 * 3600),
            Self::OneWeek => Some(7 * 24 * 3600),
            Self::AllTime => None,
        }
    }

    fn hides(self, status: Status, age_secs: u64) -> bool {
        status == Status::Dead
            && (self == Self::Active || self.cutoff_secs().is_some_and(|cutoff| age_secs > cutoff))
    }

    /// The two endpoint views mean exactly what their labels say: neither is
    /// allowed to fold conversations behind the per-project list cap.
    fn is_uncapped(self) -> bool {
        matches!(self, Self::Active | Self::AllTime)
    }
}

/// Whether the history window would let a conversation on screen, judged from
/// the most recent moment known about it. This is the mirror of
/// `HistoryWindow::hides`, which decides the same thing once the metadata is
/// in: anything this leaves out, `hides` leaves out too, so skipping the read
/// can never cost a row its title (see `known_conversations`).
fn within_window(cutoff: Option<u64>, alive: bool, last_active: u64, now: u64) -> bool {
    match cutoff {
        _ if alive => true,
        None => true,
        Some(cutoff) => now.saturating_sub(last_active) <= cutoff,
    }
}

/// Grace period before an empty conversation the user left is discarded
/// (D17). A message sent an instant before leaving can still be flushing to
/// disk — Cursor lags noticeably — so we wait and re-check emptiness rather
/// than discarding on the spot.
const DISCARD_GRACE: Duration = Duration::from_secs(30);
/// Most recent conversations shown per project before the rest are hidden
/// (D13) — the all-time window reveals them. Keeps each project's list short.
const MAX_PER_PROJECT: usize = 7;

/// Whether `project` is still a directory corc offers: present in the expanded
/// directory list, or added to the machine-local list since that list was built.
/// The local half is consulted live because the `N` picker records a directory
/// just before spawning in it — a brand-new project must never read as stale —
/// but only while the directory is really there: that list is append-only, so it
/// goes on naming checkouts long after they are deleted. No list at all (`None`)
/// means the question can't be answered, so every project passes.
fn project_is_listed(listed: Option<&HashSet<String>>, local: &[String], project: &str) -> bool {
    let Some(listed) = listed else {
        return true;
    };
    listed.contains(project)
        || (local.iter().any(|dir| dir == project) && Path::new(project).is_dir())
}

/// Timed damage repair (D23), currently off. corc gets no event when an
/// *adjacent* pane scrolls, and those updates left stale glyphs in corc's
/// pane, so the sidebar used to repaint itself fully at 10 Hz. Every repair
/// frame costs more than it looks: tmux walks the hardware cursor out of the
/// agent pane and back to draw the sidebar, ten times a second, which reads as
/// a flickering cursor while you type into an idle agent. D23 was diagnosed
/// against an older tmux, so the repair stays off until the stale-glyph bug is
/// re-confirmed on tmux 3.6a. `CORC_REPAINT_MS=100` turns it back on for an
/// A/B without a rebuild.
fn repair_interval() -> Option<Duration> {
    let millis: u64 = std::env::var("CORC_REPAINT_MS").ok()?.parse().ok()?;
    (millis > 0).then(|| Duration::from_millis(millis))
}

/// Input poll timeout when no repair clock sets one. It only wakes the event
/// loop — nothing is drawn — so the once-a-second state refresh stays on time.
const IDLE_POLL: Duration = Duration::from_millis(250);

/// Caps repair frames independently of input activity. `event::poll` returns
/// immediately while events are queued, so using its timeout as the repaint
/// clock lets mouse/key bursts accelerate the full redraw loop. A monotonic
/// deadline keeps damage repair at the intended rate and skips missed slots
/// instead of emitting catch-up frames.
struct RepaintSchedule {
    next: Instant,
    interval: Duration,
}

impl RepaintSchedule {
    fn new(now: Instant, interval: Duration) -> Self {
        Self {
            next: now,
            interval,
        }
    }

    fn take_due(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + self.interval;
        true
    }

    fn wait(&self, now: Instant) -> Duration {
        self.next.saturating_duration_since(now)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RenderKind {
    /// A small ratatui diff after sidebar input or a state refresh.
    Diff,
    /// A complete sidebar repair for damage caused by the adjacent pane.
    Full,
}

/// Keeps interactive rendering responsive while independently rate-limiting
/// expensive full-pane repairs. Key repeat may request many diff frames, but
/// it can never advance the full-repair clock.
struct RenderSchedule {
    /// None while timed damage repair is off; then only input and state
    /// refreshes draw, and corc writes nothing at all when it is idle.
    repair: Option<RepaintSchedule>,
    dirty: bool,
}

impl RenderSchedule {
    fn new(now: Instant, repair_interval: Option<Duration>) -> Self {
        Self {
            repair: repair_interval.map(|interval| RepaintSchedule::new(now, interval)),
            dirty: true,
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn take(&mut self, now: Instant) -> Option<RenderKind> {
        if self.repair.as_mut().is_some_and(|r| r.take_due(now)) {
            self.dirty = false;
            return Some(RenderKind::Full);
        }
        if std::mem::take(&mut self.dirty) {
            return Some(RenderKind::Diff);
        }
        None
    }

    fn wait(&self, now: Instant) -> Duration {
        self.repair.as_ref().map_or(IDLE_POLL, |r| r.wait(now))
    }
}

/// Background tint marking the conversation currently in the content pane — a
/// muted blue, distinct from the gray hover highlight so the active row reads
/// apart from the merely-selected one. Frees its dot to show real status (D6)
/// instead of a green "you are here" marker.
const VIEWED_BG: Color = Color::Rgb(38, 50, 71);
/// Pinned conversations reuse the existing status circle with an orchid pink
/// color, keeping the row geometry and background exactly as before.
const PINNED_DOT: Color = Color::Rgb(218, 112, 214);

/// Marker a row carries while its agent has a browser open (D24), sitting just
/// left of the time column with a space to separate it. White because it says
/// something about the agent rather than about the conversation's status, which
/// the dot on the left already owns every other color of.
const BROWSER_MARK: &str = "◆ ";

/// Activity is more urgent than organization: Running stays yellow and
/// Question/Unseen stay blue even when pinned. The pin color is the fallback
/// for quiet Idle/Dead conversations, preserving Dead's hollow circle.
fn conversation_dot(status: Status, pinned: bool) -> (&'static str, Color) {
    match status {
        Status::Running => ("●", Color::Yellow),
        Status::Question | Status::Unseen => ("●", Color::Blue),
        Status::Idle => ("●", if pinned { PINNED_DOT } else { Color::Gray }),
        Status::Dead => ("○", if pinned { PINNED_DOT } else { Color::Gray }),
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
    /// Pane statuses derived on refresh, by conversation id.
    statuses: Statuses,
    /// The pane corc runs in (left, fixed 40 columns).
    sidebar_pane: String,
    /// The plain-shell pane corc created on the right. While a conversation
    /// is viewed, this pane sits parked in that conversation's hidden window.
    placeholder_pane: String,
    /// Conversation currently swapped into the content slot.
    viewed: Option<String>,
    /// The browser view pane beside the agent, when open (D24). Whether it
    /// exists is derived from the viewed conversation's persisted `browser`
    /// flag by `sync_browser_pane` — never toggled directly.
    browser_pane: Option<String>,
    /// Conversations already known to have a browser up, so `auto_open_browser`
    /// fires on the browser appearing rather than on it merely being there.
    browser_seen: HashSet<String>,
    /// Agent panes with a live browser under them (D24), refreshed once a
    /// second from one `/proc` pass. Keyed by pane rather than conversation
    /// because that is what the pass answers about; a conversation without a
    /// pane cannot have a browser anyway. Both the row marker and
    /// `auto_open_browser` read it, so neither walks `/proc` on its own.
    browser_panes: HashSet<String>,
    /// Flat, status-driven panel above the project-grouped sidebar: the ids of
    /// the conversations whose status is Running, Question, or Unseen.
    attention: Vec<String>,
    /// When Some, the cursor is in the attention panel at this row.
    attention_sel: Option<usize>,
    items: Vec<Item>,
    /// Project label per path (D8), rebuilt with the item list because a label
    /// depends on the whole set of projects — adding one can lengthen another.
    /// Keyed by path since that, not the label, is what identifies a project.
    project_labels: HashMap<String, String>,
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
    /// `a` cycles active-only and how far back Dead conversations remain visible (D12).
    history_window: HistoryWindow,
    /// How many conversations the current history window/list cap is hiding.
    hidden: usize,
    /// The directories the pickers offer, expanded from `directories.txt` plus
    /// the machine-local list. A project outside it is stale — a checkout that
    /// was deleted, or a line dropped from the file — and stays out of the
    /// sidebar unless something in it is still live. Built once at startup:
    /// expanding walks the filesystem and asks git and jj about every repo,
    /// which is far too much work for a repaint. None when it could not be read.
    listed_dirs: Option<HashSet<String>>,
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
    /// Independent scroll state for the attention panel.
    attention_state: ListState,
    /// Current on-screen list rectangles, used to translate mouse clicks.
    list_area: Rect,
    attention_area: Rect,
    status_msg: Option<String>,
    last_refresh: Instant,
    /// On-screen hitboxes of the bottom menu buttons, rebuilt every draw so a
    /// click at `(col, row)` maps back to the button's action.
    menu_hitboxes: Vec<MenuHit>,
    /// Background fetch of Claude plan usage (5h / weekly / model-scoped),
    /// shown as a dim readout under the provider-switch menu row.
    usage: usage::Fetcher,
    /// When the provider stores last wrote their metadata caches. Rate-limits
    /// the write while a conversation streams; the parse it saves the next
    /// corc start is worth far more than writing it the instant it changes.
    metas_saved: Instant,
}

/// How often a running corc hands its parsed metadata to disk. corc normally
/// outlives whole work days inside its tmux session, so waiting for a clean
/// quit to write the cache would mean rarely writing it at all.
const CACHE_SAVE_INTERVAL: Duration = Duration::from_secs(60);

pub fn run() -> Result<()> {
    let sidebar_pane =
        std::env::var("TMUX_PANE").map_err(|_| anyhow::anyhow!("corc must run inside tmux"))?;

    // vim-tmux-navigator only sends C-hjkl into processes matching its Vim
    // pattern. Like quim, use the pattern's accepted "<prefix>/view" form;
    // corc consumes internal panel moves and hands edge moves back to tmux.
    #[cfg(target_os = "linux")]
    let _ = std::fs::write("/proc/self/comm", tmux::NAVIGATOR_PROCESS_NAME);

    // Resolve the active provider's binary once now, so the login-shell lookup
    // cost lands at startup rather than on the first conversation spawn.
    let mut state = State::load()?;
    let _ = tmux::resolve_binary(provider::by_id(&state.active_provider).binary());

    reconcile(&mut state)?;
    state.save()?;

    // Install the M-1..M-9 digit-jump bindings at runtime (D13) so 1-9 work
    // from inside the Claude pane too, without ever editing the user's tmux
    // config.
    tmux::install_jump_bindings(&crate::self_exe().to_string_lossy());
    // And C-b, which toggles the browser view from inside the agent pane (D24).
    tmux::install_browser_binding(&crate::self_exe().to_string_lossy());

    let placeholder_pane = tmux::split_content_pane(&sidebar_pane)?;

    // None when the list could not be read: then nothing counts as stale,
    // rather than everything.
    let listed_dirs: Option<HashSet<String>> = picker::list_directories(&state.directories)
        .ok()
        .map(|dirs| dirs.iter().map(|dir| dir.display().to_string()).collect());

    let history_window = state.history_window;
    let mut app = App {
        state,
        metas: MetaStore::new()?,
        statuses: Statuses::new(),
        sidebar_pane,
        placeholder_pane,
        viewed: None,
        browser_pane: None,
        browser_seen: HashSet::new(),
        browser_panes: HashSet::new(),
        attention: Vec::new(),
        attention_sel: None,
        items: Vec::new(),
        project_labels: HashMap::new(),
        selected: 0,
        menu_sel: None,
        count: None,
        filter: String::new(),
        filter_input: false,
        pending_kill: None,
        history_window,
        hidden: 0,
        listed_dirs,
        provider_picker: None,
        move_mode: false,
        pending_discard: Vec::new(),
        list_state: ListState::default(),
        attention_state: ListState::default(),
        list_area: Rect::default(),
        attention_area: Rect::default(),
        status_msg: None,
        last_refresh: Instant::now(),
        menu_hitboxes: Vec::new(),
        usage: usage::Fetcher::spawn(),
        metas_saved: Instant::now(),
    };
    app.refresh();
    app.view_last();

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let keyboard_enhanced = matches!(supports_keyboard_enhancement(), Ok(true));
    if keyboard_enhanced {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    let mut terminal = Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?;

    let result = app.event_loop(&mut terminal);

    // The browser view goes first: parking rearranges the content slot, and a
    // pane left streaming into a torn-down layout would repaint over it.
    app.close_browser_pane();
    // Swap the viewed pane home and remove the content pane we created (D10).
    app.park();
    // Respect the normal grace period on shutdown too. A message sent just
    // before Ctrl+C may still be flushing, especially for Cursor; keeping a
    // genuinely empty row is safer than deleting a real conversation.
    app.process_pending_discards();
    if tmux::pane_exists(&app.placeholder_pane) {
        let _ = tmux::kill_pane(&app.placeholder_pane);
    }
    // Put the plain Alt+number window switch back and drop C-b, matching the
    // user's config again.
    tmux::restore_bindings();
    let _ = app.state.save();
    app.metas.save_cache();

    if keyboard_enhanced {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
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
/// conversation is Dead), park Claude panes stranded outside the hidden
/// session (corc crashed mid-view) back into uuid-named hidden windows, and
/// delete browser profiles left behind by conversations that are gone (D24).
fn reconcile(state: &mut State) -> Result<()> {
    browser::prune_profiles(state);
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
        let mut render = RenderSchedule::new(Instant::now(), repair_interval());
        loop {
            match render.take(Instant::now()) {
                Some(RenderKind::Full) => {
                    // Only reached when `CORC_REPAINT_MS` opts back into timed
                    // repair of adjacent-pane damage (D23), capped at that
                    // interval so key repeat can never accelerate it.
                    force_full_redraw(terminal);
                    terminal.draw(|f| self.draw(f))?;
                }
                Some(RenderKind::Diff) => {
                    // Input-driven frames use ratatui's normal cell diff, so
                    // held j/k remains responsive without another full burst.
                    terminal.draw(|f| self.draw(f))?;
                }
                None => {}
            }

            if event::poll(render.wait(Instant::now()))? {
                match event::read()? {
                    Event::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        if self.handle_key(key.code, key.modifiers) {
                            return Ok(());
                        }
                        render.mark_dirty();
                    }
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse);
                        render.mark_dirty();
                    }
                    Event::Resize(_, _) => {
                        let _ = tmux::enforce_sidebar_width(&self.sidebar_pane);
                        render.mark_dirty();
                    }
                    _ => {}
                }
            }
            if self.last_refresh.elapsed() >= Duration::from_secs(1) {
                self.refresh();
                render.mark_dirty();
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
                KeyCode::Char('h') => {
                    tmux::select_adjacent_pane(tmux::PaneDirection::Left);
                }
                KeyCode::Char('j') => {
                    if !self.focus_panel(1) {
                        tmux::select_adjacent_pane(tmux::PaneDirection::Down);
                    }
                }
                KeyCode::Char('k') => {
                    if !self.focus_panel(-1) {
                        tmux::select_adjacent_pane(tmux::PaneDirection::Up);
                    }
                }
                KeyCode::Char('l') => {
                    tmux::select_adjacent_pane(tmux::PaneDirection::Right);
                }
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
            KeyCode::Char('}') => self.jump_project(1),
            KeyCode::Char('{') => self.jump_project(-1),
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
            KeyCode::Esc if self.attention_sel.is_some() => self.attention_sel = None,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.rebuild_items();
            }
            KeyCode::Enter => {
                if self.attention_sel.is_some() {
                    self.open_attention_selected();
                } else if let Some(i) = self.menu_sel {
                    if let Some((action, ..)) = self.menu_entries().into_iter().nth(i) {
                        self.activate_menu(action);
                    }
                } else {
                    self.view_selected();
                }
            }
            KeyCode::Char('n') => self.new_conversation_here(),
            KeyCode::Char('N') => self.open_picker(),
            KeyCode::Char('s') => self.open_provider_picker(),
            KeyCode::Char('p') => self.toggle_selected_pin(),
            KeyCode::Char('x') => self.kill_or_remove(),
            KeyCode::Char('V') => {
                self.focus_attention_in_list();
                self.move_mode = true;
            }
            KeyCode::Char('a') => self.cycle_history(),
            KeyCode::Char('b') => self.toggle_selected_browser(),
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
            MouseEventKind::ScrollDown => self.nav(1),
            MouseEventKind::ScrollUp => self.nav(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                // A click on a bottom-menu button fires its action; the menu
                // sits below the list, so this is checked before the row math.
                if let Some(action) = self.menu_hit(mouse.column, mouse.row) {
                    self.activate_menu(action);
                    return;
                }
                if contains(self.attention_area, mouse.column, mouse.row) {
                    let visible_row = mouse.row.saturating_sub(self.attention_area.y) as usize;
                    let pos = self.attention_state.offset() + visible_row;
                    if pos < self.attention.len() {
                        self.attention_sel = Some(pos);
                        self.menu_sel = None;
                        self.open_attention_selected();
                    }
                    return;
                }
                // Rows map to items through the main list's persistent scroll
                // offset. Its y origin is below the attention panel.
                if contains(self.list_area, mouse.column, mouse.row)
                    && let Some(idx) = self.item_at_row(mouse.row.saturating_sub(self.list_area.y))
                    && matches!(self.items.get(idx), Some(Item::Conv(_)))
                {
                    self.attention_sel = None;
                    self.menu_sel = None;
                    self.selected = idx;
                    self.view_selected();
                }
            }
            _ => {}
        }
    }

    /// Every conversation corc owns, fanned out to the matching provider
    /// metadata source, each flagged with whether the sidebar could put it on
    /// screen right now so the store knows which ones are worth reading a
    /// transcript for (`Known::visible`).
    ///
    /// Judging that needs an age, and the accurate age comes from the very
    /// metadata this decides whether to read. So it is judged from what is
    /// already at hand: a live pane, whatever metadata an earlier refresh or
    /// the on-disk cache left behind, and the timestamps in state.json. Each
    /// of those is at least as recent as the age `rebuild_items` will judge
    /// the row by, which is what makes the flag safe. A conversation left out
    /// here is one the history window hides anyway, so no row can end up on
    /// screen with its metadata missing.
    fn known_conversations(&self) -> Vec<(Known, &'static str)> {
        let now = state::unix_now();
        let cutoff = self.history_window.cutoff_secs();
        self.state
            .conversations
            .iter()
            .map(|c| {
                let active =
                    status::last_active_ts(self.metas.meta(&c.id), c.created_at).max(c.last_viewed);
                let visible = within_window(cutoff, c.pane_id.is_some(), active, now);
                (
                    Known {
                        id: c.id.clone(),
                        cwd: c.cwd.clone(),
                        turn_started_at: c.turn_started_at,
                        visible,
                    },
                    provider::by_id(&c.provider).id(),
                )
            })
            .collect()
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        let mut dirty = false;

        // Browser-view requests from `corc browser`, run from inside an agent
        // pane. The TUI is the only writer of state.json, so the CLI hands the
        // change over here rather than editing the file under us.
        if browser::apply_requests(&mut self.state) {
            dirty = true;
        }

        // Relocation requests from `corc cd`, run from inside an agent pane:
        // Cursor only moves the row; Claude also receives `/cd` in its pane
        // and reports where the session ended up (ADR-0003).
        let applied = cd::apply_requests(&mut self.state, state::unix_now());
        dirty |= applied.moved;
        if let Some(msg) = applied.failure {
            self.status_msg = Some(msg);
        }

        // Take one tmux snapshot for every liveness check in this refresh.
        // Spawning `tmux list-panes` once per live conversation blocked input
        // for a noticeable fraction of a second on larger lists.
        let panes = match tmux::all_panes() {
            Ok(panes) => Some(panes),
            Err(e) => {
                self.status_msg = Some(e.to_string());
                None
            }
        };

        // Notice vanished panes: the conversation is Dead (D12). If tmux
        // itself could not be queried, preserve the last-known state instead
        // of falsely declaring every conversation dead.
        let mut viewed_died = false;
        if let Some(panes) = &panes {
            for conv in &mut self.state.conversations {
                if let Some(pane_id) = &conv.pane_id
                    && !panes.contains_key(pane_id)
                {
                    conv.pane_id = None;
                    dirty = true;
                    if self.viewed.as_deref() == Some(conv.id.as_str()) {
                        viewed_died = true;
                    }
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
        } else if self.viewed.is_none()
            && panes
                .as_ref()
                .is_some_and(|panes| !panes.contains_key(&self.placeholder_pane))
        {
            // Someone closed the placeholder shell; put it back.
            if let Ok(pane) = tmux::split_content_pane(&self.sidebar_pane) {
                self.placeholder_pane = pane;
            }
        }

        // A pending provider id that can now be resolved migrates to the real
        // session id — state row, hidden window and viewed pointer together —
        // before the metadata refresh, so meta starts flowing under the new
        // key in the same tick.
        let resumed = panes
            .as_ref()
            .and_then(|panes| self.follow_resumed_sessions(panes));
        if resumed.is_some() {
            dirty = true;
        }
        if self.resolve_pending_ids() {
            dirty = true;
        }

        let known = self.known_conversations();
        if let Err(e) = self.metas.refresh(&known) {
            self.status_msg = Some(e.to_string());
        }
        if self.metas_saved.elapsed() >= CACHE_SAVE_INTERVAL {
            self.metas.save_cache();
            self.metas_saved = Instant::now();
        }

        // Persist the metadata that must survive a temporarily unavailable
        // provider store. The turn start preserves an elapsed clock across a
        // restart; content_seen is sticky proof that a title-less conversation
        // is real and must never be removed by empty-conversation cleanup.
        for conv in &mut self.state.conversations {
            let Some(meta) = self.metas.meta(&conv.id) else {
                continue;
            };
            if meta.has_content && !conv.content_seen {
                conv.content_seen = true;
                dirty = true;
            }
            let started = (meta.turn_state == crate::discovery::TurnState::Mid)
                .then_some(meta.turn_started_at)
                .flatten();
            if conv.turn_started_at != started {
                conv.turn_started_at = started;
                dirty = true;
            }
        }

        // Follow the provider's session directory, including OpenCode's
        // session_move and Claude's /cd (ADR-0003). Cursor's bookkeeping-only
        // move has no provider cwd to settle.
        let moves: Vec<(String, PathBuf)> = self
            .state
            .conversations
            .iter()
            .filter_map(|c| Some((c.id.clone(), c.settled_cwd(self.metas.meta(&c.id)?)?)))
            .collect();
        for (id, cwd) in moves {
            if self.state.relocate(&id, &cwd) {
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
                let status = status::derive(
                    c.pane_id.is_some(),
                    self.metas.meta(&c.id),
                    c.last_viewed,
                    viewed.as_deref() == Some(c.id.as_str()),
                    now,
                    c.created_at,
                );
                (c.id.clone(), status)
            })
            .collect();

        // Which agent panes have a browser under them (D24). A tmux query that
        // failed keeps the last answer rather than claiming every browser
        // closed, exactly as pane liveness above does.
        if let Some(panes) = &panes {
            self.browser_panes =
                browser::panes_with_browser(&live_pane_pids(&self.state.conversations, panes));
        }

        // The agent opening a browser turns the view on by itself (D24), before
        // the sync below derives the pane from the flag.
        if self.auto_open_browser() {
            dirty = true;
        }

        if dirty {
            let _ = self.state.save();
        }
        self.sync_browser_pane();

        self.rebuild_keeping_selection();
        if let Some(id) = resumed
            && self.viewed.as_deref() == Some(id.as_str())
            && let Some(pos) = item_pos(&self.items, &id)
        {
            self.selected = pos;
        }
    }

    fn follow_resumed_sessions(&mut self, panes: &HashMap<String, tmux::Pane>) -> Option<String> {
        let mut changed = None;
        for (pane, info) in panes {
            let Some(session) = &info.session else {
                continue;
            };
            let Some(old) = self
                .state
                .conversations
                .iter()
                .find(|c| c.pane_id.as_deref() == Some(pane) && c.provider == session.provider)
                .cloned()
            else {
                continue;
            };
            if old.id == session.id {
                continue;
            }
            let target = self.state.conversation(&session.id).cloned();
            if target
                .as_ref()
                .is_some_and(|c| c.provider != session.provider)
            {
                continue;
            }
            // If the duplicate is on screen, park it before removing its window
            // so the content placeholder stays alive.
            let target_viewed = self.viewed.as_deref() == Some(&session.id);
            if target_viewed {
                self.park();
            }
            if let Some(other) = target.and_then(|c| c.pane_id)
                && other != *pane
                && tmux::pane_exists(&other)
                && let Err(e) = tmux::kill_pane(&other)
            {
                self.status_msg = Some(e.to_string());
                continue;
            }
            // A dead destination may still have a placeholder window left over.
            let _ = tmux::kill_hidden_window(&session.id);
            if let Err(e) = tmux::rename_hidden_window(&old.id, &session.id) {
                self.status_msg = Some(e.to_string());
                continue;
            }
            self.state.resume_in_pane(pane, session);
            if self.viewed.as_deref() == Some(&old.id) {
                self.viewed = Some(session.id.clone());
            }
            if target_viewed {
                self.status_msg = self.view(&session.id).err().map(|e| e.to_string());
            }
            if self.is_empty_conversation(&old.id) {
                self.mark_pending_discard(old.id);
            }
            changed = Some(session.id.clone());
        }
        changed
    }

    /// Rebuild the top panel, keeping its cursor on the same conversation. A
    /// row that has left the panel gives the cursor up entirely rather than
    /// keeping its position, which would be some other conversation's row.
    fn rebuild_attention(&mut self) {
        let keep = self
            .attention_sel
            .and_then(|pos| self.attention.get(pos))
            .cloned();
        self.attention = attention_ids(&self.state, &self.statuses);
        self.attention_sel = keep.and_then(|id| self.attention.iter().position(|row| *row == id));
    }

    /// Widen or narrow the history window (D12). The refresh is the point of
    /// doing this in one place: conversations the old window hid have had no
    /// metadata read for them, so without it the newly revealed rows would sit
    /// there untitled until the next poll.
    fn cycle_history(&mut self) {
        self.history_window = self.history_window.next();
        self.state.history_window = self.history_window;
        self.status_msg = self.state.save().err().map(|e| e.to_string());
        self.refresh();
        self.rebuild_keeping_selection();
    }

    fn rebuild_items(&mut self) {
        let filter = self.filter.to_lowercase();
        let now = state::unix_now();
        self.items.clear();
        self.hidden = 0;
        // Labels come from the set, so they are computed once here rather than
        // per project or per row.
        let labels = repo::labels(&self.state.projects);
        self.project_labels = self
            .state
            .projects
            .iter()
            .cloned()
            .zip(labels.iter().cloned())
            .collect();

        for (pi, project) in self.state.projects.iter().enumerate() {
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

            // A project whose directory has dropped out of both directory lists
            // — a checkout that was deleted, or a line removed from
            // directories.txt — is stale, and only its live conversations keep
            // it on screen. `projects` is otherwise append-only, so without
            // this every directory the user ever worked in lingers in the
            // sidebar forever. The all-time window still reaches them, so
            // nothing becomes unreachable.
            if self.history_window != HistoryWindow::AllTime
                && !project_is_listed(self.listed_dirs.as_ref(), &self.state.directories, project)
                && !indices.iter().any(|&i| {
                    status_of(&self.statuses, &self.state.conversations[i].id) != Status::Dead
                })
            {
                self.hidden += indices.len();
                continue;
            }

            let name = labels[pi].clone();
            let mut kept = Vec::new();
            for i in indices {
                let conv = &self.state.conversations[i];
                // Dead conversations outside the selected history window stay
                // out of the list (D12). The Active window hides all of them.
                let status = status_of(&self.statuses, &conv.id);
                if status == Status::Dead {
                    let age = now.saturating_sub(status::last_active_ts(
                        self.metas.meta(&conv.id),
                        conv.created_at,
                    ));
                    if self.history_window.hides(status, age) {
                        self.hidden += 1;
                        continue;
                    }
                }
                if !filter.is_empty() {
                    let title = self
                        .metas
                        .meta(&conv.id)
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
            // without disturbing its order. The active-only and all-time
            // windows, plus an active text filter, bypass the cap.
            if !self.history_window.is_uncapped()
                && filter.is_empty()
                && kept.len() > MAX_PER_PROJECT
            {
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
                self.items
                    .push(Item::Conv(self.state.conversations[i].id.clone()));
            }
        }
        self.clamp_selection();
        self.rebuild_attention();
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
    /// space: attention rows, project-grouped conversations, then bottom-menu
    /// rows. The count is clamped so a stray large prefix (`999j`) can't spin.
    fn move_selection(&mut self, dir: i64, count: usize) {
        let span = self.attention.len() + self.items.len() + self.menu_entries().len();
        for _ in 0..count.min(span.max(1)) {
            self.nav(dir);
        }
    }

    /// One j/k step across attention → project conversations → bottom menu.
    /// Crossing a panel boundary lands on the nearest row in the next panel.
    fn nav(&mut self, dir: i64) {
        let menu_last = self.menu_entries().len() as i64 - 1;
        if let Some(i) = self.attention_sel {
            let next = i as i64 + dir;
            if next < 0 {
                return;
            }
            if next < self.attention.len() as i64 {
                self.attention_sel = Some(next as usize);
            } else if self.has_conversations() {
                self.attention_sel = None;
                self.select_edge(true);
            } else {
                self.attention_sel = None;
                self.menu_sel = Some(0);
            }
            return;
        }
        match self.menu_sel {
            Some(i) => {
                let next = i as i64 + dir;
                if next < 0 {
                    if self.has_conversations() {
                        self.menu_sel = None;
                        self.select_edge(false);
                    } else if !self.attention.is_empty() {
                        self.menu_sel = None;
                        self.attention_sel = Some(self.attention.len() - 1);
                    }
                } else {
                    self.menu_sel = Some(next.min(menu_last) as usize);
                }
            }
            None if dir > 0 && self.at_last_conv() => self.menu_sel = Some(0),
            None if dir < 0 && self.at_first_conv() && !self.attention.is_empty() => {
                self.attention_sel = Some(self.attention.len() - 1)
            }
            None => self.select_next(dir),
        }
    }

    fn current_panel(&self) -> Panel {
        if self.attention_sel.is_some() {
            Panel::Attention
        } else if self.menu_sel.is_some() {
            Panel::Menu
        } else {
            Panel::Conversations
        }
    }

    /// Ctrl+j/k: move directly between panels without walking every row.
    fn focus_panel(&mut self, dir: i64) -> bool {
        let current = self.current_panel();
        let target = adjacent_panel(
            current,
            dir,
            !self.attention.is_empty(),
            self.has_conversations(),
        );
        match target {
            Panel::Attention => {
                self.menu_sel = None;
                self.attention_sel = Some(0);
            }
            Panel::Conversations => {
                self.attention_sel = None;
                self.menu_sel = None;
            }
            Panel::Menu => {
                self.attention_sel = None;
                self.menu_sel = Some(0);
            }
        }
        target != current
    }

    fn has_conversations(&self) -> bool {
        self.items.iter().any(|it| matches!(it, Item::Conv(_)))
    }

    /// Whether the cursor sits on the last conversation row (or the list has
    /// none) — the point where j crosses into the bottom menu.
    fn at_last_conv(&self) -> bool {
        self.items
            .iter()
            .rposition(|i| matches!(i, Item::Conv(_)))
            .is_none_or(|p| p == self.selected)
    }

    fn at_first_conv(&self) -> bool {
        self.items
            .iter()
            .position(|i| matches!(i, Item::Conv(_)))
            .is_none_or(|p| p == self.selected)
    }

    /// The item index of the first conversation in each project group, in
    /// screen order — the landing spots for the Ctrl+d/u and }/{ folder hop.
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

    /// Ctrl+d/u or }/{: move the selection to the first conversation of the
    /// next or previous project group, clamping at the ends.
    fn jump_project(&mut self, dir: i64) {
        self.attention_sel = None;
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
        self.attention_sel = None;
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
        self.attention_selected_conv_id()
            .or_else(|| self.main_selected_conv_id())
    }

    fn attention_selected_conv_id(&self) -> Option<String> {
        Some(self.attention.get(self.attention_sel?)?.clone())
    }

    fn main_selected_conv_id(&self) -> Option<String> {
        match self.items.get(self.selected)? {
            Item::Conv(id) => Some(id.clone()),
            Item::Header(_) => None,
        }
    }

    /// Move the attention-panel target onto its duplicate in the normal
    /// project-grouped sidebar. A filter or the per-project cap may have hidden
    /// it, so widen only as much as needed before selecting it.
    fn focus_attention_in_list(&mut self) -> Option<String> {
        let id = self.selected_conv_id()?;
        self.attention_sel = None;
        self.menu_sel = None;
        let mut pos = item_pos(&self.items, &id);
        if pos.is_none() {
            self.filter.clear();
            // Not written back to state: this widening reveals one row the
            // user asked for, it is not them choosing a new window.
            self.history_window = HistoryWindow::AllTime;
            self.rebuild_items();
            pos = item_pos(&self.items, &id);
        }
        if let Some(pos) = pos {
            self.selected = pos;
        }
        Some(id)
    }

    /// Enter/click in the top panel means “take me to this row in the normal
    /// sidebar and open it”, leaving keyboard focus there afterwards.
    fn open_attention_selected(&mut self) {
        if self.focus_attention_in_list().is_some() {
            self.view_selected();
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
        if let Some(pos) = item_pos(&self.items, &id) {
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
        // The browser view belongs to the conversation, so a switch opens or
        // closes the pane rather than repointing it.
        self.sync_browser_pane();
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

    /// Migrate conversations whose provisional id can now be resolved to the
    /// agent's real session id (Codex/OpenCode persist it with the first
    /// message). Everything keyed by the id moves together — the state
    /// row, the hidden tmux window's name and the viewed pointer. An entry in
    /// `pending_discard` intentionally does not: keyed by the old id, it
    /// cancels itself on the next check, which is exactly right — a resolved
    /// conversation has a message and must not be discarded. Returns whether
    /// anything changed (the caller persists).
    fn resolve_pending_ids(&mut self) -> bool {
        let mut taken: Vec<String> = self
            .state
            .conversations
            .iter()
            .map(|c| c.id.clone())
            .collect();
        let mut changed = false;
        for i in 0..self.state.conversations.len() {
            let (id, cwd, created_at, provider_id) = {
                let c = &self.state.conversations[i];
                (
                    c.id.clone(),
                    c.cwd.clone(),
                    c.created_at,
                    c.provider.clone(),
                )
            };
            let prov = provider::by_id(&provider_id);
            if !prov.is_pending(&id) {
                continue;
            }
            let since = std::time::UNIX_EPOCH + std::time::Duration::from_secs(created_at);
            let real = match prov.resolve_spawned_id(&cwd, since, &taken) {
                Ok(Some(real)) => real,
                Ok(None) => continue,
                Err(e) => {
                    self.status_msg = Some(e.to_string());
                    continue;
                }
            };
            // Best-effort: a Dead conversation has no hidden window to rename.
            let _ = tmux::rename_hidden_window(&id, &real);
            if self.viewed.as_deref() == Some(id.as_str()) {
                self.viewed = Some(real.clone());
            }
            self.state.conversations[i].id = real.clone();
            taken.push(real);
            changed = true;
        }
        changed
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

    /// Whether the conversation has never been observed with a real exchange
    /// and current provider metadata still reads as empty. A missing metadata
    /// record is how an untouched provider session normally starts, so it also
    /// counts as empty after the grace period. Once `content_seen` is true it
    /// is sticky: later metadata/title loss can never make the conversation
    /// eligible for automatic cleanup again.
    fn is_empty_conversation(&self, id: &str) -> bool {
        conversation_is_empty(self.state.conversation(id), self.metas.meta(id))
    }

    /// Forget an empty conversation: kill its agent pane and hidden window
    /// (if any survive), drop it from the state file and delete the hook log
    /// corc kept for it. The jsonl under ~/.claude is never touched (D1).
    fn discard_conversation(&mut self, id: &str) {
        if let Some(pane) = self.state.conversation(id).and_then(|c| c.pane_id.clone())
            && tmux::kill_hidden_window(id).is_err()
            && tmux::pane_exists(&pane)
        {
            let _ = tmux::kill_pane(&pane);
        }
        self.state.conversations.retain(|c| c.id != id);
        hooks::forget(id);
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
        match status_of(&self.statuses, &id) {
            Status::Dead => {
                self.state.conversations.retain(|c| c.id != id);
                hooks::forget(&id);
                self.state.prune_empty_projects();
                self.status_msg = self.state.save().err().map(|e| e.to_string());
                self.refresh();
            }
            Status::Running => self.pending_kill = Some(id),
            Status::Question | Status::Unseen | Status::Idle => self.kill_conversation(&id),
        }
    }

    /// `p`: persistently pin or unpin the conversation under the active
    /// cursor. Pinned conversations always appear in the top panel — last,
    /// after the blue and yellow rows — even when Dead or outside the
    /// selected history window.
    fn toggle_selected_pin(&mut self) {
        let from_attention = self.attention_sel.is_some();
        let id = if from_attention {
            self.attention_selected_conv_id()
        } else if self.menu_sel.is_none() {
            self.main_selected_conv_id()
        } else {
            None
        };
        let Some(id) = id else {
            return;
        };
        let Some(pinned) = self.state.toggle_pin(&id) else {
            return;
        };
        self.status_msg = self.state.save().err().map(|e| e.to_string());
        self.rebuild_keeping_selection();

        // Unpinning an Idle/Dead row removes it from the top panel. Hand the
        // cursor back to its canonical project row when that row is visible.
        if from_attention
            && !pinned
            && self.attention_sel.is_none()
            && let Some(pos) = item_pos(&self.items, &id)
        {
            self.selected = pos;
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
        let exe = crate::self_exe();
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
            // Cancelling exits 0 (an empty --out file), so a nonzero exit is a
            // real failure — say so instead of N silently doing nothing.
            Ok(s) => {
                self.status_msg = Some(format!("popup failed: {s}"));
                return None;
            }
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
            if let Some(pos) = item_pos(&self.items, &id) {
                self.selected = pos;
            }
            self.view(&id)
        })();
        self.status_msg = result.err().map(|e| e.to_string());
    }

    /// `b` (D24): turn the browser view on or off for the conversation under
    /// the cursor. The flag is persisted per conversation; the pane itself is
    /// derived from it by `sync_browser_pane`, so toggling a conversation that
    /// is not in view takes effect the moment you open it.
    fn toggle_selected_browser(&mut self) {
        let Some(id) = self.selected_conv_id() else {
            return;
        };
        let Some(conv) = self.state.conversation_mut(&id) else {
            return;
        };
        conv.browser = !conv.browser;
        let on = conv.browser;
        self.status_msg = self.state.save().err().map(|e| e.to_string());
        self.sync_browser_pane();
        // A pane appearing beside the agent is its own feedback; a flag set on
        // some other conversation would otherwise be invisible.
        if self.viewed.as_deref() != Some(id.as_str()) && self.status_msg.is_none() {
            let label = if on { "on" } else { "off" };
            self.status_msg = Some(format!("browser view {label} for that conversation"));
        }
    }

    /// Turn the view on for the viewed conversation when its agent opens a
    /// browser (D24), so `b` is a way to keep the pane rather than the only way
    /// to get it. Returns whether the flag changed, to fold into the caller's
    /// save.
    ///
    /// Only the viewed conversation is turned on: the pane exists for that one
    /// alone, so a flag set on a background conversation buys nothing but a
    /// change where the user cannot see it — and switching to a conversation
    /// whose browser is already up fires this on the next tick anyway. The row
    /// marker is what carries the news about the others.
    fn auto_open_browser(&mut self) -> bool {
        let state = &self.state;
        self.browser_seen
            .retain(|id| state.conversation(id).is_some());
        let Some(id) = self.viewed.clone() else {
            return false;
        };
        let present = self
            .state
            .conversation(&id)
            .and_then(|c| c.pane_id.as_deref())
            .is_some_and(|pane| self.browser_panes.contains(pane));
        if !browser_appeared(&mut self.browser_seen, &id, present) {
            return false;
        }
        // A setup that cannot draw the view is skipped silently rather than
        // reported once per browser the agent opens; the manual toggle is where
        // that error belongs. Finding a browser already proves the Playwright
        // config is wired, so passthrough is all that is left to check.
        if !tmux::passthrough_enabled() {
            return false;
        }
        match self.state.conversation_mut(&id) {
            Some(conv) if !conv.browser => {
                conv.browser = true;
                true
            }
            _ => false,
        }
    }

    /// Make the pane match the viewed conversation's `browser` flag (D24).
    /// Runs on every refresh and after a conversation switch, which is what
    /// makes the view follow the conversation rather than the content slot.
    fn sync_browser_pane(&mut self) {
        // A pane the user closed by hand clears the flag instead of being
        // respawned a second later.
        if let Some(pane) = self.browser_pane.clone()
            && !tmux::pane_exists(&pane)
        {
            self.browser_pane = None;
            self.set_viewed_browser(false);
        }
        let wanted = self
            .viewed
            .as_deref()
            .and_then(|id| self.state.conversation(id))
            .is_some_and(|c| c.browser);

        match (wanted, self.browser_pane.clone()) {
            (true, None) => {
                let result = (|| -> Result<()> {
                    if !tmux::passthrough_enabled() {
                        anyhow::bail!(
                            "tmux swallows image escapes; add `set -g allow-passthrough on` to tmux.conf"
                        );
                    }
                    let exe = crate::self_exe();
                    let pane =
                        tmux::split_browser_pane(&self.content_pane(), &exe.to_string_lossy())?;
                    self.browser_pane = Some(pane);
                    tmux::enforce_sidebar_width(&self.sidebar_pane)
                })();
                if let Err(e) = result {
                    // Clear the flag rather than retrying — and failing — on
                    // every tick from here on.
                    self.set_viewed_browser(false);
                    self.status_msg = Some(e.to_string());
                }
            }
            (false, Some(pane)) => {
                self.browser_pane = None;
                if tmux::pane_exists(&pane) {
                    let _ = tmux::kill_pane(&pane);
                }
                let _ = tmux::enforce_sidebar_width(&self.sidebar_pane);
            }
            _ => {}
        }
    }

    fn set_viewed_browser(&mut self, on: bool) {
        let Some(id) = self.viewed.clone() else {
            return;
        };
        if let Some(conv) = self.state.conversation_mut(&id)
            && conv.browser != on
        {
            conv.browser = on;
            let _ = self.state.save();
        }
    }

    /// The pane sitting in the content slot right now: the viewed
    /// conversation's agent, or the placeholder when nothing is viewed.
    fn content_pane(&self) -> String {
        self.viewed
            .as_deref()
            .and_then(|id| self.state.conversation(id))
            .and_then(|c| c.pane_id.clone())
            .unwrap_or_else(|| self.placeholder_pane.clone())
    }

    fn close_browser_pane(&mut self) {
        if let Some(pane) = self.browser_pane.take()
            && tmux::pane_exists(&pane)
        {
            let _ = tmux::kill_pane(&pane);
        }
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
    /// if it is still visible. The attention panel keeps its own cursor the
    /// same way, inside `rebuild_attention`: the duplicated rows have
    /// independent cursor memory, so merely browsing Attention must not move
    /// the canonical list's selection on refresh.
    fn rebuild_keeping_selection(&mut self) {
        let keep = self.main_selected_conv_id();
        self.rebuild_items();
        // Follow the conversation, not the row number. A conversation that is
        // gone leaves the cursor on whatever row now sits at this position
        // (clamped by the rebuild) — never on an unrelated conversation
        // dragged in by a shifted index.
        if let Some(pos) = keep.and_then(|id| item_pos(&self.items, &id)) {
            self.selected = pos;
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        // One row per menu entry plus the rule above them, plus the usage
        // readout (its own divider + row) when there is anything to show.
        // The readout follows the *selected conversation*: its provider's
        // plan usage and accent — not the active provider's — so cursoring
        // across a mixed list swaps the whole gauge with it.
        // With nothing selected it falls back to the active provider.
        let pid = self
            .selected_conv_id()
            .and_then(|id| self.state.conversations.iter().find(|c| c.id == id))
            .map(|c| c.provider.clone())
            .unwrap_or_else(|| self.state.active_provider.clone());
        let usage = self.usage.entries(&pid).filter(|e| !e.is_empty());
        let readout = usage.is_some().then_some(provider::accent(&pid));
        let menu_h = self.menu_entries().len() as u16 + 1 + 2 * readout.is_some() as u16;
        // Attention gets enough room to show every row when practical, but at
        // most half of the content region so the canonical project list never
        // disappears. Its own ListState scrolls any overflow.
        let content_h = f.area().height.saturating_sub(menu_h + 1);
        let attention_h = attention_panel_height(self.attention.len(), content_h);
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(attention_h),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(menu_h),
            ])
            .split(f.area());
        self.draw_attention(f, outer[0]);
        self.draw_list(f, outer[1]);
        self.draw_footer(f, outer[2]);
        self.draw_menu(f, outer[3], usage.as_deref(), readout);
        self.draw_provider_picker(f);
    }

    /// The bottom-menu rows, top to bottom: (action, marker, label, key hint).
    /// The current window and folded-row count live on the History row; the
    /// provider row always names the active agent.
    fn menu_entries(&self) -> Vec<(MenuAction, &'static str, String, &'static str)> {
        let provider = provider::by_id(&self.state.active_provider).display_name();
        let history = if self.hidden > 0 {
            format!(
                "History · {} ({} hidden)",
                self.history_window.label(),
                self.hidden
            )
        } else {
            format!("History · {}", self.history_window.label())
        };
        // The provider row sits last so it lands right above the usage
        // readout at the sidebar's bottom edge — agent and its gauge together.
        vec![
            (MenuAction::New, "+", "New conversation".to_string(), "N"),
            (MenuAction::CycleHistory, "◷", history, "a"),
            (MenuAction::Shortcuts, "?", "Shortcuts".to_string(), "?"),
            (MenuAction::SwitchProvider, "⇄", provider.to_string(), "s"),
        ]
    }

    /// The bottom menu: a dim rule, then one quiet row per action — marker and
    /// label left, key hint right-aligned — echoing the conversation rows
    /// instead of shouting like a button bar. j past the last conversation
    /// walks the cursor in; the cursor row carries the same gray highlight as
    /// the list. The provider row is tinted with the active agent's accent.
    fn draw_menu(
        &mut self,
        f: &mut Frame,
        area: Rect,
        usage: Option<&[usage::Entry]>,
        readout: Option<Color>,
    ) {
        let width = area.width as usize;
        let dim = Style::default().fg(Color::DarkGray);
        let mut lines = vec![divider(width)];
        self.menu_hitboxes.clear();
        for (i, (action, marker, label, hint)) in self.menu_entries().into_iter().enumerate() {
            let selected = self.menu_sel == Some(i);
            let fg = match action {
                MenuAction::SwitchProvider => provider::accent(&self.state.active_provider),
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
            let hit_row = area.y + lines.len() as u16;
            lines.push(Line::from(vec![
                Span::styled(text, row),
                Span::styled(" ".repeat(pad), row),
                Span::styled(format!("{hint} "), hint_style),
            ]));
            self.menu_hitboxes
                .push((hit_row, area.x..area.x + area.width, action));
        }
        // The readout closes the sidebar as its very last row, set off from
        // the buttons by its own rule and tinted with the selected
        // conversation's provider accent so it reads as *that* agent's gauge.
        // Display only — no hitbox, no cursor stop.
        if let Some(accent) = readout {
            lines.push(Line::from(Span::styled("─".repeat(width), dim)));
            lines.push(usage_line(
                usage.unwrap_or_default(),
                Style::default().fg(accent),
            ));
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
            MenuAction::CycleHistory => self.cycle_history(),
            MenuAction::SwitchProvider => self.open_provider_picker(),
            MenuAction::Shortcuts => self.show_shortcuts(),
        }
    }

    /// Show the keyboard cheat-sheet in a centered `tmux display-popup`,
    /// reachable by `?` or the menu's `?` button. Blocks until the popup is
    /// dismissed, like the directory picker (D22); a missing/old tmux just
    /// surfaces an error in the footer.
    fn show_shortcuts(&mut self) {
        let exe = crate::self_exe();
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
    /// dim. Status colors per D6: Running yellow ●, Question/Unseen blue ●,
    /// Idle gray ●, Dead hollow ○. The viewed conversation carries a blue row
    /// background (`VIEWED_BG`) rather than a recolored dot, so its dot keeps
    /// showing status like any other row.
    fn render_conv(
        &self,
        id: &str,
        width: usize,
        now: u64,
        multi_provider: bool,
        selected: bool,
        show_project: bool,
    ) -> ListItem<'static> {
        // A row can outlive its conversation by at most the frame between a
        // removal and the next rebuild. Blank beats bringing the TUI down.
        let Some(conv) = self.state.conversation(id) else {
            return ListItem::new("");
        };
        let status = status_of(&self.statuses, id);
        let (dot, dot_color) = conversation_dot(status, conv.pinned);
        let meta = self.metas.meta(&conv.id);
        let mut title = meta
            .and_then(|m| m.display_title())
            .unwrap_or("(untitled)")
            .to_string();
        if show_project {
            let cwd = conv.cwd.to_string_lossy();
            // Every conversation's project is in the map; the basename is a
            // fallback for the frame between a spawn and the next rebuild.
            let project = self
                .project_labels
                .get(cwd.as_ref())
                .cloned()
                .unwrap_or_else(|| repo::label_for(&cwd, &[]));
            title = format!("{project} · {title}");
        }
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
        let title_style = if selected {
            base.add_modifier(Modifier::BOLD)
        } else if status == Status::Dead {
            Style::default().fg(Color::Gray)
        } else {
            base
        };
        let dim = Style::default().fg(Color::DarkGray);
        // The selected row uses DarkGray as its hover background, so the
        // normally dim time needs a lighter foreground to stay readable.
        let time_style = if selected {
            Style::default().fg(Color::Gray)
        } else {
            dim
        };
        let time_w = time.chars().count();
        let gap = if time_w > 0 { 1 } else { 0 };
        // The marker takes its columns from the title, not from the time.
        let mark = match conv.pane_id.as_deref() {
            Some(pane) if self.browser_panes.contains(pane) => BROWSER_MARK,
            _ => "",
        };
        let mark_w = mark.chars().count();
        let t = truncate(&title, width.saturating_sub(3 + mark_w + time_w + gap));
        let pad = width.saturating_sub(3 + t.chars().count() + mark_w + time_w);
        let item = ListItem::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(dot, Style::default().fg(dot_color)),
            Span::raw(" "),
            Span::styled(t, title_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(mark, Style::default().fg(Color::White)),
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
        let focused = self.attention_sel.is_none() && self.menu_sel.is_none();
        let items: Vec<ListItem> = (0..self.items.len())
            .map(|idx| match &self.items[idx] {
                Item::Header(name) => self.render_header(name, width),
                Item::Conv(id) => self.render_conv(
                    id,
                    width,
                    now,
                    multi,
                    focused && idx == self.selected,
                    false,
                ),
            })
            .collect();

        self.list_area = area;
        // While the cursor is in another panel the list drops its highlight,
        // so exactly one row on screen ever reads as selected.
        set_list_highlight(&mut self.list_state, focused.then_some(self.selected));
        keep_first_conversation_context_visible(
            &self.items,
            self.selected,
            area.height,
            &mut self.list_state,
        );
        let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));
        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    /// Status-driven panel at the top. Rows deliberately echo the normal
    /// conversation grammar, adding the project name because this list is
    /// flat rather than grouped. A plain bottom rule separates it from the
    /// canonical list, matching the rule above the settings menu.
    fn draw_attention(&mut self, f: &mut Frame, area: Rect) {
        if area.height == 0 || self.attention.is_empty() {
            self.attention_area = Rect::default();
            return;
        }
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        let width = area.width as usize;
        let now = state::unix_now();
        let multi = self.uses_multiple_providers();
        let items: Vec<ListItem> = self
            .attention
            .iter()
            .enumerate()
            .map(|(pos, id)| {
                self.render_conv(id, width, now, multi, self.attention_sel == Some(pos), true)
            })
            .collect();
        self.attention_area = rows[0];
        set_list_highlight(&mut self.attention_state, self.attention_sel);
        let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));
        f.render_stateful_widget(list, rows[0], &mut self.attention_state);
        f.render_widget(Paragraph::new(divider(width)), rows[1]);
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

fn divider(width: usize) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width),
        Style::default().fg(Color::DarkGray),
    ))
}

/// The plan-usage readout: `5h 21% · wk 24% · fable 41%`, one line
/// uniformly in the provider's accent — a single tone, since mixing dim labels
/// with brighter percents made the numbers jump out as clutter. Only a limit
/// about to bite gets a different color: its whole `label percent%` segment
/// turns yellow from 70% and red from 90%.
fn usage_line(entries: &[usage::Entry], base: Style) -> Line<'static> {
    let mut spans = vec![Span::styled(" ", base)];
    for e in entries {
        if spans.len() > 1 {
            spans.push(Span::styled(" · ", base));
        }
        let style = match e.percent {
            90.. => Style::default().fg(Color::Red),
            70.. => Style::default().fg(Color::Yellow),
            _ => base,
        };
        spans.push(Span::styled(format!("{} {}%", e.label, e.percent), style));
    }
    Line::from(spans)
}

/// Make ratatui treat every cell as changed on the next draw without emitting
/// a terminal clear. The NUL sentinel cannot be produced by ratatui's text
/// rendering (control characters are filtered), so even intended blank cells
/// differ and overwrite any glyph that leaked in from an adjacent tmux pane.
fn force_full_redraw<B: Backend>(terminal: &mut Terminal<B>) {
    terminal.swap_buffers();
    for cell in &mut terminal.current_buffer_mut().content {
        cell.set_symbol("\0");
    }
    terminal.swap_buffers();
}

fn contains(area: Rect, col: u16, row: u16) -> bool {
    col >= area.x
        && col < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn conversation_is_empty(
    conversation: Option<&state::Conversation>,
    meta: Option<&crate::discovery::Meta>,
) -> bool {
    conversation.is_some_and(|conversation| {
        !conversation.content_seen && meta.is_none_or(|meta| !meta.has_content)
    })
}

/// Whether a browser the agent has open should turn the view on: only on the
/// transition from absent to present, recorded in `seen`. Level-triggering it
/// instead would reopen a view the user closed by hand while the browser was
/// still up, one tick later, every tick.
///
/// `seen` is only ever updated for the conversation in view, so one switched
/// away from stays "seen" and coming back to it does not re-fire.
/// The agent panes of `conversations` that tmux still knows about, with the pid
/// of the process each one started — the roots `browser::panes_with_browser`
/// searches. A conversation whose pane is gone is left out: it is Dead, and its
/// stale pid could name some unrelated process by now.
fn live_pane_pids(
    conversations: &[state::Conversation],
    panes: &HashMap<String, tmux::Pane>,
) -> HashMap<String, u32> {
    conversations
        .iter()
        .filter_map(|c| {
            let pane_id = c.pane_id.clone()?;
            let pid = panes.get(&pane_id)?.pid;
            Some((pane_id, pid))
        })
        .collect()
}

fn browser_appeared(seen: &mut HashSet<String>, id: &str, present: bool) -> bool {
    if !present {
        seen.remove(id);
        return false;
    }
    seen.insert(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        HistoryWindow, IDLE_POLL, Item, PINNED_DOT, Panel, RenderKind, RenderSchedule,
        RepaintSchedule, Statuses, adjacent_panel, attention_ids, attention_panel_height,
        browser_appeared, conversation_dot, conversation_is_empty, force_full_redraw, item_pos,
        keep_first_conversation_context_visible, project_is_listed, set_list_highlight,
        within_window,
    };
    use crate::discovery::Meta;
    use crate::state::Conversation;
    use crate::status::Status;
    use ratatui::Terminal;
    use ratatui::backend::{Backend, TestBackend, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Size};
    use ratatui::style::Color;
    use ratatui::text::Line;
    use ratatui::widgets::{List, ListItem, ListState, Paragraph};
    use std::collections::HashSet;
    use std::io;
    use std::time::{Duration, Instant};

    #[test]
    fn history_window_cycles_through_every_supported_view() {
        let mut window = HistoryWindow::Active;
        let mut labels = Vec::new();
        for _ in 0..6 {
            labels.push(window.label());
            window = window.next();
        }

        assert_eq!(labels, ["active", "3h", "1D", "3D", "1W", "all time"]);
        assert_eq!(window, HistoryWindow::Active);
    }

    #[test]
    fn history_window_only_hides_dead_conversations_past_its_cutoff() {
        let three_hours = HistoryWindow::ThreeHours;
        assert!(!three_hours.hides(Status::Dead, 3 * 3600));
        assert!(three_hours.hides(Status::Dead, 3 * 3600 + 1));
        assert!(!three_hours.hides(Status::Idle, 3 * 3600 + 1));
        assert!(!HistoryWindow::AllTime.hides(Status::Dead, u64::MAX));
        assert!(HistoryWindow::Active.hides(Status::Dead, 0));
        assert!(!HistoryWindow::Active.hides(Status::Idle, u64::MAX));
        assert!(HistoryWindow::Active.is_uncapped());
    }

    /// The rule that keeps a corc start off hundreds of megabytes of
    /// transcript: a conversation whose metadata goes unread must be one the
    /// list would hide anyway, whatever the history window. Live panes are
    /// always read, however old the conversation is.
    #[test]
    fn a_conversation_left_unread_is_one_the_window_hides_anyway() {
        // Past every age below, so `now - age` stays a real timestamp.
        let now = 1_000_000_000u64;
        let mut window = HistoryWindow::Active;
        for _ in 0..6 {
            for age in [0, 1, 3600, 3601, 24 * 3600, 7 * 24 * 3600, 30 * 24 * 3600] {
                let read = within_window(window.cutoff_secs(), false, now - age, now);
                assert!(
                    read || window.hides(Status::Dead, age),
                    "{:?} skips a {age}s old conversation it would still show",
                    window
                );
                assert!(
                    within_window(window.cutoff_secs(), true, now - age, now),
                    "{:?} skips a live conversation",
                    window
                );
            }
            window = window.next();
        }
        assert_eq!(window, HistoryWindow::Active, "every window covered");
    }

    /// A project counts as listed while the expanded list holds it, or while the
    /// machine-local list names it *and* it is still on disk. Nothing names a
    /// deleted checkout back into the sidebar, and with no list to check against
    /// every project passes — a failed read can never empty the sidebar.
    #[test]
    fn stale_projects_are_the_ones_no_directory_list_names() {
        let listed: HashSet<String> = ["/home/h/projects/corc".to_string()].into();
        // A directory that really exists stands in for one just added through
        // the picker, which records it before the expanded list is rebuilt.
        let fresh = std::env::temp_dir().display().to_string();
        let local = [fresh.clone(), "/home/h/work/forgotten".to_string()];

        assert!(project_is_listed(
            Some(&listed),
            &local,
            "/home/h/projects/corc"
        ));
        assert!(project_is_listed(Some(&listed), &local, &fresh));
        // Named by the append-only local list, but long gone from disk.
        assert!(!project_is_listed(
            Some(&listed),
            &local,
            "/home/h/work/forgotten"
        ));
        assert!(!project_is_listed(
            Some(&listed),
            &local,
            "/home/h/projects/never-listed"
        ));
        assert!(project_is_listed(None, &local, "/home/h/work/forgotten"));
    }

    #[test]
    fn ctrl_panel_navigation_follows_attention_conversations_menu() {
        assert_eq!(
            adjacent_panel(Panel::Attention, 1, true, true),
            Panel::Conversations
        );
        assert_eq!(
            adjacent_panel(Panel::Conversations, 1, true, true),
            Panel::Menu
        );
        assert_eq!(
            adjacent_panel(Panel::Menu, -1, true, true),
            Panel::Conversations
        );
        assert_eq!(
            adjacent_panel(Panel::Conversations, -1, true, true),
            Panel::Attention
        );
    }

    #[test]
    fn ctrl_panel_navigation_skips_empty_panels_and_clamps() {
        assert_eq!(
            adjacent_panel(Panel::Conversations, -1, false, true),
            Panel::Conversations
        );
        assert_eq!(
            adjacent_panel(Panel::Conversations, 1, false, true),
            Panel::Menu
        );
        assert_eq!(
            adjacent_panel(Panel::Attention, 1, true, false),
            Panel::Menu
        );
        assert_eq!(adjacent_panel(Panel::Menu, 1, true, true), Panel::Menu);
    }

    #[test]
    fn attention_panel_scrolls_before_it_crowds_out_the_main_list() {
        assert_eq!(attention_panel_height(0, 20), 0);
        assert_eq!(attention_panel_height(2, 20), 3);
        assert_eq!(attention_panel_height(20, 20), 10);
        assert_eq!(attention_panel_height(2, 2), 0);
        assert_eq!(attention_panel_height(2, 3), 2);
    }

    fn conversation(content_seen: bool) -> Conversation {
        Conversation {
            id: "conversation".into(),
            cwd: "/tmp".into(),
            pane_id: None,
            last_viewed: 0,
            created_at: 0,
            provider: "claude".into(),
            turn_started_at: None,
            content_seen,
            pinned: false,
            browser: false,
            relocation_requested_at: None,
        }
    }

    #[test]
    fn attention_sorts_blue_then_yellow_then_pinned() {
        let mut running = conversation(true);
        running.id = "running".into();
        running.created_at = 20;

        let mut pinned = conversation(true);
        pinned.id = "pinned".into();
        pinned.created_at = 10;
        pinned.pinned = true;

        let mut unseen = conversation(true);
        unseen.id = "unseen".into();
        unseen.created_at = 30;

        let mut question = conversation(true);
        question.id = "question".into();
        question.created_at = 35;

        let mut idle = conversation(true);
        idle.id = "idle".into();
        idle.created_at = 40;

        let state = crate::state::State {
            projects: vec!["/tmp".into()],
            conversations: vec![running, pinned, unseen, question, idle],
            ..Default::default()
        };
        let statuses: Statuses = [
            ("running".to_string(), Status::Running),
            ("pinned".to_string(), Status::Dead),
            ("unseen".to_string(), Status::Unseen),
            ("question".to_string(), Status::Question),
            ("idle".to_string(), Status::Idle),
        ]
        .into();

        assert_eq!(
            attention_ids(&state, &statuses),
            ["question", "unseen", "running", "pinned"]
        );
    }

    /// The property that keeps the cursor sane when a conversation is deleted:
    /// a row is found by the conversation it holds, and a conversation that no
    /// longer has a row is simply a miss. It must never resolve to the row of
    /// whichever conversation moved into the deleted one's place — the old
    /// index-carrying rows did exactly that, flinging the cursor across the
    /// sidebar on every `x`.
    #[test]
    fn rows_are_found_by_conversation_not_by_position() {
        let items = vec![
            Item::Header("one".into()),
            Item::Conv("a".into()),
            Item::Conv("b".into()),
            Item::Header("two".into()),
            Item::Conv("c".into()),
        ];

        assert_eq!(item_pos(&items, "a"), Some(1));
        assert_eq!(item_pos(&items, "c"), Some(4));
        assert_eq!(item_pos(&items, "deleted"), None);
    }

    #[test]
    fn activity_dot_colors_take_precedence_over_pin_color() {
        assert_eq!(
            conversation_dot(Status::Running, true),
            ("●", Color::Yellow)
        );
        assert_eq!(conversation_dot(Status::Question, true), ("●", Color::Blue));
        assert_eq!(conversation_dot(Status::Unseen, true), ("●", Color::Blue));
        assert_eq!(conversation_dot(Status::Idle, true), ("●", PINNED_DOT));
        assert_eq!(conversation_dot(Status::Dead, true), ("○", PINNED_DOT));
    }

    #[test]
    fn untouched_conversation_is_empty_with_or_without_metadata() {
        let conversation = conversation(false);
        assert!(conversation_is_empty(Some(&conversation), None));
        assert!(conversation_is_empty(
            Some(&conversation),
            Some(&Meta::default())
        ));
    }

    #[test]
    fn current_content_prevents_empty_cleanup_before_it_is_persisted() {
        let conversation = conversation(false);
        let meta = Meta {
            has_content: true,
            ..Meta::default()
        };
        assert!(!conversation_is_empty(Some(&conversation), Some(&meta)));
    }

    #[test]
    fn previously_seen_content_survives_missing_or_empty_metadata() {
        let conversation = conversation(true);
        assert!(!conversation_is_empty(Some(&conversation), None));
        assert!(!conversation_is_empty(
            Some(&conversation),
            Some(&Meta::default())
        ));
    }

    #[test]
    fn repaint_schedule_cannot_be_accelerated_by_events() {
        let start = Instant::now();
        let mut schedule = RepaintSchedule::new(start, Duration::from_millis(100));

        assert!(schedule.take_due(start));
        for millis in 1..100 {
            assert!(!schedule.take_due(start + Duration::from_millis(millis)));
        }
        assert!(schedule.take_due(start + Duration::from_millis(100)));
        assert!(!schedule.take_due(start + Duration::from_millis(101)));
    }

    #[test]
    fn input_changes_draw_immediately_without_accelerating_full_repair() {
        let start = Instant::now();
        let mut schedule = RenderSchedule::new(start, Some(Duration::from_millis(100)));

        assert_eq!(schedule.take(start), Some(RenderKind::Full));
        schedule.mark_dirty();
        assert_eq!(
            schedule.take(start + Duration::from_millis(1)),
            Some(RenderKind::Diff)
        );
        schedule.mark_dirty();
        assert_eq!(
            schedule.take(start + Duration::from_millis(2)),
            Some(RenderKind::Diff)
        );
        assert_eq!(schedule.take(start + Duration::from_millis(3)), None);
        assert_eq!(
            schedule.take(start + Duration::from_millis(100)),
            Some(RenderKind::Full)
        );
    }

    #[test]
    fn without_repair_an_idle_sidebar_never_draws() {
        let start = Instant::now();
        let mut schedule = RenderSchedule::new(start, None);

        // The first frame still paints the sidebar; after that only input and
        // state refreshes do, so a still corc writes nothing to the terminal.
        assert_eq!(schedule.take(start), Some(RenderKind::Diff));
        assert_eq!(schedule.take(start + Duration::from_secs(10)), None);
        schedule.mark_dirty();
        assert_eq!(
            schedule.take(start + Duration::from_secs(10)),
            Some(RenderKind::Diff)
        );
        assert_eq!(schedule.wait(start + Duration::from_secs(10)), IDLE_POLL);
    }

    struct RecordingBackend {
        inner: TestBackend,
        draw_counts: Vec<usize>,
        clear_calls: usize,
    }

    impl RecordingBackend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                inner: TestBackend::new(width, height),
                draw_counts: Vec::new(),
                clear_calls: 0,
            }
        }
    }

    impl Backend for RecordingBackend {
        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            let updates: Vec<_> = content.collect();
            self.draw_counts.push(updates.len());
            self.inner.draw(updates.into_iter())
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> io::Result<()> {
            self.clear_calls += 1;
            self.inner.clear()
        }

        fn size(&self) -> io::Result<Size> {
            self.inner.size()
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    #[test]
    fn full_redraw_updates_every_cell_without_clearing_the_screen() {
        let mut terminal = Terminal::new(RecordingBackend::new(4, 3)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("same"), frame.area()))
            .unwrap();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("same"), frame.area()))
            .unwrap();
        assert_eq!(terminal.backend().draw_counts.last(), Some(&0));

        force_full_redraw(&mut terminal);
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("same"), frame.area()))
            .unwrap();

        assert_eq!(terminal.backend().draw_counts.last(), Some(&12));
        assert_eq!(terminal.backend().clear_calls, 0);
    }

    #[test]
    fn first_conversation_keeps_project_header_and_spacer_visible() {
        let items = vec![
            Item::Header("one".into()),
            Item::Conv("a".into()),
            Item::Conv("b".into()),
            Item::Header("two".into()),
            Item::Conv("c".into()),
        ];
        // Reproduce a list that previously scrolled with the selected first
        // conversation as its first visible item, clipping both header rows.
        let mut state = ListState::default().with_offset(4).with_selected(Some(4));

        keep_first_conversation_context_visible(&items, 4, 3, &mut state);

        let rendered = vec![
            ListItem::new(vec![Line::raw(""), Line::raw("one")]),
            ListItem::new("conversation 0"),
            ListItem::new("conversation 1"),
            ListItem::new(vec![Line::raw(""), Line::raw("two")]),
            ListItem::new("conversation 2"),
        ];
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_stateful_widget(List::new(rendered), frame.area(), &mut state)
            })
            .unwrap();

        assert_eq!(state.offset(), 3);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(0, 1)].symbol(), "t");
        assert_eq!(buffer[(0, 2)].symbol(), "c");
    }

    #[test]
    fn entering_menu_hides_list_highlight_without_resetting_scroll() {
        let mut state = ListState::default().with_offset(8).with_selected(Some(10));

        set_list_highlight(&mut state, None);

        assert_eq!(state.selected(), None);
        assert_eq!(state.offset(), 8);
    }

    /// The whole life of an auto-opened browser view, in the order a user meets
    /// it: the agent opens a browser, the user closes the view, comes back to
    /// the conversation, and the agent later opens a second browser.
    #[test]
    fn the_view_opens_when_a_browser_appears_and_not_again_until_the_next_one() {
        let mut seen = HashSet::new();

        // The agent's first browser tool call: the view opens.
        assert!(browser_appeared(&mut seen, "a", true));
        // Every tick after that, the same browser is still there and must not
        // reopen a view the user has since closed by hand.
        assert!(!browser_appeared(&mut seen, "a", true));
        assert!(!browser_appeared(&mut seen, "a", true));

        // Switching to another conversation leaves "a" alone — only the viewed
        // conversation is ever checked — so coming back does not re-fire.
        assert!(browser_appeared(&mut seen, "b", true));
        assert!(!browser_appeared(&mut seen, "a", true));

        // The browser goes away, and the next one the agent opens is a fresh
        // reason to show the view.
        assert!(!browser_appeared(&mut seen, "a", false));
        assert!(browser_appeared(&mut seen, "a", true));
    }
}
