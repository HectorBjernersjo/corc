//! Standalone, centered pickers meant to run inside a `tmux display-popup`
//! (D22, D23). Each takes over the popup's terminal, runs its own event loop,
//! and returns the user's choice — it never touches corc's state. The running
//! TUI reads the returned value and acts on it, staying the sole writer of
//! `state.json`. The same code renders the sessionizer (`corc projects`) and
//! the directory picker (`corc pick-dir`); both share one picker that also
//! completes — and creates — filesystem paths (see `run_filter_picker`).

use crate::display_dir;
use crate::picker::{complete_dirs, expand_tilde, fuzzy_match};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Padding, Paragraph};
use ratatui::{Frame, Terminal};
use std::io::Stdout;
use std::path::PathBuf;

type Term = Terminal<ratatui::backend::CrosstermBackend<Stdout>>;

/// An item in a filter picker: the label shown and filtered against, and the
/// string returned when it is chosen.
pub struct Choice {
    pub label: String,
    pub value: String,
}

impl Choice {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// Enter raw mode on the popup terminal, run `body`, and always restore the
/// terminal afterwards (even on error).
fn with_terminal<T>(body: impl FnOnce(&mut Term) -> Result<T>) -> Result<T> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?;
    let result = body(&mut terminal);
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

/// A picker row: the text to show, plus the char indices (into `text`) the
/// query fuzzily matched, for highlighting. `matched` is empty for rows with
/// nothing to highlight (the path prompt's completions, the add-directory row).
struct Row {
    text: String,
    matched: Vec<usize>,
}

impl Row {
    /// A row with no highlighted characters.
    fn plain(text: String) -> Self {
        Self {
            text,
            matched: Vec::new(),
        }
    }
}

/// Color for the fuzzy-matched characters, à la Telescope/snacks — bold so it
/// reads over both the plain background and the gray selection highlight.
const MATCH_FG: Color = Color::Yellow;

/// Render one row's text into a `Line`, truncating to `width` (with a trailing
/// `…`) and styling the fuzzy-matched characters in `MATCH_FG`. Consecutive
/// characters of the same style are coalesced into one span.
fn highlight_row(row: &Row, width: usize) -> Line<'static> {
    let chars: Vec<char> = row.text.chars().collect();
    let truncated = chars.len() > width;
    let visible = if truncated {
        width.saturating_sub(1)
    } else {
        chars.len()
    };
    let hl = Style::default().fg(MATCH_FG).add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let mut buf_hl = false;
    for (i, &c) in chars.iter().enumerate().take(visible) {
        let is_hl = row.matched.contains(&i);
        if is_hl != buf_hl && !buf.is_empty() {
            let style = if buf_hl { hl } else { Style::default() };
            spans.push(Span::styled(std::mem::take(&mut buf), style));
        }
        buf_hl = is_hl;
        buf.push(c);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, if buf_hl { hl } else { Style::default() }));
    }
    if truncated {
        spans.push(Span::raw("…"));
    }
    Line::from(spans)
}

/// Draw the shared picker chrome: a bordered block filling `area`, titled,
/// with the `▸ input` line on the *bottom* and the results above it, reversed
/// and bottom-anchored — the best match (index 0) sits just above the input
/// and the list grows upward.
fn draw(f: &mut Frame, title: &str, input: &str, rows: &[Row], selected: usize) {
    let area = f.area();
    // Breathing room between the border and the content: 1 column horizontally.
    // Vertical padding is 0 — a terminal cell is indivisible, so the smallest
    // step below one whole blank row is none at all.
    let block = Block::bordered()
        .title(format!(" {title} "))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let list_area = split[0];

    let width = list_area.width as usize;
    let n = rows.len();
    // Reverse so the best match renders last (at the bottom).
    let items: Vec<ListItem> = rows
        .iter()
        .rev()
        .map(|r| ListItem::new(highlight_row(r, width)))
        .collect();
    let sel_rev = n.checked_sub(1).map(|last| last - selected.min(last));
    // Bottom-anchor when the results don't fill the area; when they overflow,
    // ratatui scrolls to keep the (bottom) selection visible on its own.
    let height = (n as u16).min(list_area.height);
    let anchored = Rect {
        x: list_area.x,
        y: list_area.y + list_area.height - height,
        width: list_area.width,
        height,
    };
    let mut state = ListState::default();
    state.select(sel_rev);
    let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));
    f.render_stateful_widget(list, anchored, &mut state);

    f.render_widget(Paragraph::new(format!("▸ {input}▏")), split[1]);
}

/// What choosing a picker row does — the selectable rows, in display order.
/// The error row is display-only: the selection is clamped to this list's
/// length, so it can never land there.
enum Action {
    /// A listed item: return its value.
    Item(usize),
    /// The "+ add directory…" escape hatch: prefill `~/`, switching the same
    /// picker into path mode.
    AddDir,
    /// A filesystem completion: return it.
    Completion(PathBuf),
    /// The "+ create" row: make the directory, then return it.
    Create(PathBuf),
}

/// Label of the "add directory" row — a searchable candidate in the fuzzy
/// ranking, so it can be found by typing (e.g. "add") like any other row.
const ADD_DIR_LABEL: &str = "+ add directory…";

/// The typed path as a create target: expanded, absolute, and not already
/// existing (an existing file must not be offered — create_dir_all would
/// fail). None means the create row is not shown.
fn create_target(input: &str) -> Option<PathBuf> {
    let expanded = expand_tilde(input.trim_end());
    // components() drops a trailing `/`, so the recorded path matches how the
    // directory lists spell it.
    let path = PathBuf::from(&expanded).components().as_path().to_path_buf();
    (expanded.starts_with('/') && !path.exists()).then_some(path)
}

/// A centered picker over `items` with two modes, decided by the input's
/// shape. Plain text fuzzy-filters the items (same word-substring matching as
/// the sidebar `/` filter). Input starting with `~` or `/` is a filesystem
/// path instead: the rows become real-directory completions (Tab drills in),
/// plus an explicit `+ create <path>` row when the path doesn't exist yet —
/// ranked last, so creating is always a visible, deliberate choice, never a
/// silent side effect of Enter. The "+ add directory…" row bridges the modes
/// for discoverability: choosing it just prefills `~/` in place (no second
/// screen). It joins the fuzzy ranking as a regular candidate — searchable by
/// typing ("add"), ranked by score, last on an empty query, and filtered out
/// like any row when the query excludes it (path mode covers the dead-end
/// case). Tab on an item whose value is a path drills into it the same way.
/// Returns the chosen value/path, or None on Esc.
pub fn run_filter_picker(title: &str, items: Vec<Choice>) -> Result<Option<String>> {
    with_terminal(|term| picker_loop(term, title, &items))
}

fn picker_loop(term: &mut Term, title: &str, items: &[Choice]) -> Result<Option<String>> {
    let mut input = String::new();
    let mut selected = 0usize;
    let mut error: Option<String> = None;
    loop {
        let path_mode = input.starts_with('~') || input.starts_with('/');
        let mut rows: Vec<Row> = Vec::new();
        let mut actions: Vec<Action> = Vec::new();
        if path_mode {
            for dir in complete_dirs(&input) {
                rows.push(Row::plain(display_dir(&dir.to_string_lossy())));
                actions.push(Action::Completion(dir));
            }
            if let Some(target) = create_target(&input) {
                rows.push(Row::plain(format!(
                    "+ create {}",
                    display_dir(&target.to_string_lossy())
                )));
                actions.push(Action::Create(target));
            }
        } else {
            // Fuzzy subsequence match, best score first (so it lands at the
            // bottom of the reversed list). sort_by is stable, so equal scores
            // — and the empty-query case — keep source order. Each hit carries
            // the matched char positions so the row can highlight them. The
            // "add directory" row is a regular, searchable candidate (`None`),
            // ranked by its score like any item and filtered out when the
            // query excludes it; appended last, so an empty query keeps it at
            // the bottom of the ranking.
            let mut scored: Vec<(Option<usize>, Vec<usize>, i32)> = items
                .iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    fuzzy_match(&input, &c.label).map(|m| (Some(i), m.indices, m.score))
                })
                .collect();
            if let Some(m) = fuzzy_match(&input, ADD_DIR_LABEL) {
                scored.push((None, m.indices, m.score));
            }
            scored.sort_by(|a, b| b.2.cmp(&a.2));
            for (item, matched, _) in scored {
                match item {
                    Some(i) => {
                        rows.push(Row {
                            text: items[i].label.clone(),
                            matched,
                        });
                        actions.push(Action::Item(i));
                    }
                    None => {
                        rows.push(Row {
                            text: ADD_DIR_LABEL.to_string(),
                            matched,
                        });
                        actions.push(Action::AddDir);
                    }
                }
            }
        }
        if let Some(e) = &error {
            rows.push(Row::plain(format!("error: {e}")));
        }
        selected = selected.min(actions.len().saturating_sub(1));
        term.draw(|f| draw(f, title, &display_dir(&input), &rows, selected))?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(None),
            // Reversed layout: the best match is at the bottom, so Up walks up
            // through the results (higher index) and Down walks back toward it.
            KeyCode::Up => selected = (selected + 1).min(actions.len().saturating_sub(1)),
            KeyCode::Down => selected = selected.saturating_sub(1),
            // Tab drills into the selected directory — a completion, or a
            // listed item whose value is a path (projects; session rows are
            // bare names and stay put).
            KeyCode::Tab => {
                let dest = match actions.get(selected) {
                    Some(Action::Completion(dir)) => Some(dir.to_string_lossy().into_owned()),
                    Some(&Action::Item(i)) if items[i].value.starts_with('/') => {
                        Some(items[i].value.clone())
                    }
                    _ => None,
                };
                if let Some(dest) = dest {
                    input = format!("{}/", display_dir(&dest));
                    selected = 0;
                    error = None;
                }
            }
            KeyCode::Backspace => {
                input.pop();
                selected = 0;
                error = None;
            }
            KeyCode::Char(c) => {
                input.push(c);
                selected = 0;
                error = None;
            }
            KeyCode::Enter => {
                // In path mode an exact existing directory wins; otherwise the
                // highlighted row decides.
                if path_mode {
                    // components() drops a trailing `/` (see create_target).
                    let typed = PathBuf::from(expand_tilde(&input))
                        .components()
                        .as_path()
                        .to_path_buf();
                    if typed.is_dir() {
                        return Ok(Some(typed.to_string_lossy().into_owned()));
                    }
                }
                match actions.get(selected) {
                    Some(&Action::Item(i)) => return Ok(Some(items[i].value.clone())),
                    Some(Action::AddDir) => {
                        input = "~/".to_string();
                        selected = 0;
                    }
                    Some(Action::Completion(dir)) => {
                        return Ok(Some(dir.to_string_lossy().into_owned()));
                    }
                    Some(Action::Create(target)) => match std::fs::create_dir_all(target) {
                        Ok(()) => return Ok(Some(target.to_string_lossy().into_owned())),
                        Err(e) => error = Some(e.to_string()),
                    },
                    None => {}
                }
            }
            _ => {}
        }
    }
}
