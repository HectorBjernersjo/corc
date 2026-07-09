//! Standalone, centered pickers meant to run inside a `tmux display-popup`
//! (D22, D23). Each takes over the popup's terminal, runs its own event loop,
//! and returns the user's choice — it never touches corc's state. The running
//! TUI reads the returned value and acts on it, staying the sole writer of
//! `state.json`. The same code renders the sessionizer (`corc projects`) and
//! the directory pickers (`corc pick-dir`, `corc add-dir`).

use crate::picker::{complete_dirs, expand_tilde, matches_words};
use crate::{display_dir, truncate};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
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

/// Draw the shared picker chrome: a bordered block filling `area`, titled,
/// with the `▸ input` line on the *bottom* and the results above it, reversed
/// and bottom-anchored — the best match (index 0) sits just above the input
/// and the list grows upward.
fn draw(f: &mut Frame, title: &str, input: &str, rows: &[String], selected: usize) {
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
        .map(|r| ListItem::new(truncate(r, width)))
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

/// A centered fuzzy picker over `items` with the same word-substring matching
/// as the sidebar `/` filter. Returns the chosen value, or None on Esc / empty.
pub fn run_filter_picker(title: &str, items: Vec<Choice>) -> Result<Option<String>> {
    with_terminal(|term| filter_loop(term, title, &items))
}

fn filter_loop(term: &mut Term, title: &str, items: &[Choice]) -> Result<Option<String>> {
    let mut input = String::new();
    let mut selected = 0usize;
    loop {
        let filtered: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| matches_words(&input, &c.label))
            .map(|(i, _)| i)
            .collect();
        selected = selected.min(filtered.len().saturating_sub(1));
        let rows: Vec<String> = filtered.iter().map(|&i| items[i].label.clone()).collect();
        term.draw(|f| draw(f, title, &input, &rows, selected))?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Enter => {
                return Ok(filtered.get(selected).map(|&i| items[i].value.clone()));
            }
            KeyCode::Backspace => {
                input.pop();
                selected = 0;
            }
            // Reversed layout: the best match is at the bottom, so Up walks up
            // through the results (higher index) and Down walks back toward it.
            KeyCode::Up => selected = (selected + 1).min(filtered.len().saturating_sub(1)),
            KeyCode::Down => selected = selected.saturating_sub(1),
            KeyCode::Char(c) => {
                input.push(c);
                selected = 0;
            }
            _ => {}
        }
    }
}

/// A centered path prompt: type a filesystem path (prefilled with `$HOME/`),
/// Tab/▲▼ to complete against real subdirectories. Returns the chosen
/// directory, or None on Esc. Mirrors the old inline `p` overlay (D14).
pub fn run_path_prompt(title: &str) -> Result<Option<PathBuf>> {
    with_terminal(|term| path_loop(term, title))
}

fn path_loop(term: &mut Term, title: &str) -> Result<Option<PathBuf>> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut input = if home.is_empty() {
        String::new()
    } else {
        format!("{home}/")
    };
    let mut completions = complete_dirs(&input);
    let mut selected = 0usize;
    loop {
        if !completions.is_empty() {
            selected = selected.min(completions.len() - 1);
        }
        let rows: Vec<String> = completions
            .iter()
            .map(|d| display_dir(&d.to_string_lossy()))
            .collect();
        term.draw(|f| draw(f, title, &display_dir(&input), &rows, selected))?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(None),
            // Reversed layout: best completion sits at the bottom (see `draw`).
            KeyCode::Up => {
                if !completions.is_empty() {
                    selected = (selected + 1).min(completions.len() - 1);
                }
            }
            KeyCode::Down => selected = selected.saturating_sub(1),
            KeyCode::Tab => {
                if let Some(dir) = completions.get(selected) {
                    input = format!("{}/", dir.to_string_lossy());
                    completions = complete_dirs(&input);
                    selected = 0;
                }
            }
            KeyCode::Backspace => {
                input.pop();
                completions = complete_dirs(&input);
                selected = 0;
            }
            KeyCode::Char(c) => {
                input.push(c);
                completions = complete_dirs(&input);
                selected = 0;
            }
            KeyCode::Enter => {
                // Prefer the exact typed path if it is a directory; otherwise
                // take the highlighted completion.
                let typed = PathBuf::from(expand_tilde(&input));
                let chosen = if typed.is_dir() {
                    Some(typed)
                } else {
                    completions.get(selected).cloned()
                };
                if chosen.is_some() {
                    return Ok(chosen);
                }
            }
            _ => {}
        }
    }
}
