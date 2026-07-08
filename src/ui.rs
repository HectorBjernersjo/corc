use crate::discovery::{self, Store};
use crate::status::{Annotated, Status};
use crate::{age, display_dir, procs, tmux};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

enum Item {
    Header(String),
    Conv(usize),
}

/// A conversation pane currently joined in next to the sidebar.
struct Embedded {
    conv_id: String,
    pane_id: String,
    home: tmux::PaneHome,
}

struct App {
    embedded: Option<Embedded>,
    store: Store,
    rows: Vec<Annotated>,
    items: Vec<Item>,
    selected: usize,
    show_all: bool,
    filter: String,
    filter_input: bool,
    status_msg: Option<String>,
    preview: HashMap<String, (SystemTime, Vec<(&'static str, String)>)>,
    last_refresh: Instant,
}

pub fn run() -> Result<()> {
    let mut app = App {
        embedded: None,
        store: Store::new()?,
        rows: Vec::new(),
        items: Vec::new(),
        selected: 0,
        show_all: false,
        filter: String::new(),
        filter_input: false,
        status_msg: None,
        preview: HashMap::new(),
        last_refresh: Instant::now(),
    };
    app.refresh()?;

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?;

    let result = app.event_loop(&mut terminal);

    // Send any embedded pane back home before leaving.
    if let Some(e) = app.embedded.take() {
        let _ = tmux::unembed(&e.pane_id, &e.home);
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

impl App {
    fn event_loop(
        &mut self,
        terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        loop {
            terminal.draw(|f| self.draw(f))?;

            if event::poll(Duration::from_millis(200))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if self.handle_key(key.code, key.modifiers)? {
                        return Ok(());
                    }
                }
            }
            if self.last_refresh.elapsed() >= Duration::from_secs(1) {
                self.refresh()?;
            }
        }
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<bool> {
        if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            return Ok(true);
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
            return Ok(false);
        }
        match code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('j') | KeyCode::Down => self.select_next(1),
            KeyCode::Char('k') | KeyCode::Up => self.select_next(-1),
            KeyCode::Char('g') | KeyCode::Home => self.select_edge(true),
            KeyCode::Char('G') | KeyCode::End => self.select_edge(false),
            KeyCode::Char('a') => {
                self.show_all = !self.show_all;
                self.rebuild_items();
            }
            KeyCode::Char('/') => self.filter_input = true,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.rebuild_items();
            }
            KeyCode::Enter => self.embed_selected(),
            KeyCode::Char('o') => self.goto_selected(),
            KeyCode::Char('n') => self.new_conversation(),
            KeyCode::Char('r') => self.refresh()?,
            _ => {}
        }
        Ok(false)
    }

    fn refresh(&mut self) -> Result<()> {
        self.last_refresh = Instant::now();
        // The embedded claude may have exited (its pane closes with it).
        if let Some(e) = &self.embedded {
            if !tmux::pane_exists(&e.pane_id) {
                self.embedded = None;
            }
        }
        let keep = self.selected_row().map(|r| r.conv.session_id.clone());
        self.store.refresh()?;
        let convs = self.store.conversations();
        let procs = procs::scan();
        let panes = tmux::list_panes();
        self.rows = crate::status::annotate(&convs, &procs, &panes);
        self.rebuild_items();
        if let Some(id) = keep {
            if let Some(pos) = self.items.iter().position(
                |i| matches!(i, Item::Conv(r) if self.rows[*r].conv.session_id == id),
            ) {
                self.selected = pos;
            }
        }
        Ok(())
    }

    fn rebuild_items(&mut self) {
        let week = Duration::from_secs(7 * 24 * 3600);
        let now = SystemTime::now();
        let filter = self.filter.to_lowercase();

        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            let recent = now
                .duration_since(row.conv.mtime)
                .map(|a| a < week)
                .unwrap_or(true);
            if !self.show_all && !recent && row.status == Status::Idle {
                continue;
            }
            let project = display_dir(&row.conv.project_dir());
            if !filter.is_empty() {
                let hay = format!(
                    "{} {} {}",
                    project.to_lowercase(),
                    row.conv.display_title().to_lowercase(),
                    row.conv.git_branch.as_deref().unwrap_or("").to_lowercase()
                );
                if !filter.split_whitespace().all(|w| hay.contains(w)) {
                    continue;
                }
            }
            match groups.iter_mut().find(|(name, _)| *name == project) {
                Some((_, list)) => list.push(i),
                None => groups.push((project, vec![i])),
            }
        }

        self.items.clear();
        for (project, list) in groups {
            self.items.push(Item::Header(project));
            // Live conversations above idle history within each project.
            for &i in list.iter().filter(|&&i| self.rows[i].status != Status::Idle) {
                self.items.push(Item::Conv(i));
            }
            for &i in list.iter().filter(|&&i| self.rows[i].status == Status::Idle) {
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

    fn selected_row(&self) -> Option<&Annotated> {
        match self.items.get(self.selected)? {
            Item::Conv(i) => self.rows.get(*i),
            Item::Header(_) => None,
        }
    }

    /// Bring the selected conversation's pane in next to the sidebar and
    /// focus it, resuming it in a new pane first if it isn't live.
    fn embed_selected(&mut self) {
        let Some(row) = self.selected_row() else { return };
        let conv_id = row.conv.session_id.clone();
        let pane_id = row.pane.as_ref().map(|p| p.pane_id.clone());
        let cwd = row.conv.cwd.clone();
        let command = format!("claude --resume {conv_id}");
        self.status_msg = self
            .embed(conv_id, pane_id, cwd, &command)
            .err()
            .map(|e| e.to_string());
    }

    fn new_conversation(&mut self) {
        let Some(row) = self.selected_row() else { return };
        let cwd = row.conv.cwd.clone();
        // A fresh conversation has no jsonl yet; track it under a synthetic
        // id so a later Enter on another row swaps it out properly.
        let conv_id = format!("new-in-{}", row.conv.project_dir());
        self.status_msg = self
            .embed(conv_id, None, cwd, "claude")
            .err()
            .map(|e| e.to_string());
    }

    fn embed(
        &mut self,
        conv_id: String,
        pane_id: Option<String>,
        cwd: Option<std::path::PathBuf>,
        spawn_command: &str,
    ) -> Result<()> {
        let sidebar = std::env::var("TMUX_PANE")
            .map_err(|_| anyhow::anyhow!("not running inside tmux"))?;

        // Selecting the conversation that is already embedded just refocuses it.
        if let Some(e) = &self.embedded {
            if e.conv_id == conv_id {
                return tmux::select_pane(&e.pane_id);
            }
        }

        let pane_id = match pane_id {
            Some(id) => id,
            None => {
                let dir = cwd.ok_or_else(|| {
                    anyhow::anyhow!("conversation has no recorded directory")
                })?;
                tmux::spawn_window(&dir, spawn_command)?.0
            }
        };

        if let Some(prev) = self.embedded.take() {
            let _ = tmux::unembed(&prev.pane_id, &prev.home);
        }
        let home = tmux::pane_home(&pane_id)?;
        tmux::embed(&pane_id, &sidebar)?;
        self.embedded = Some(Embedded { conv_id, pane_id, home });
        Ok(())
    }

    /// Jump to the conversation where it lives instead of embedding it here.
    fn goto_selected(&mut self) {
        let Some(row) = self.selected_row() else { return };
        if let (Some(e), Some(pane)) = (&self.embedded, &row.pane) {
            if e.pane_id == pane.pane_id {
                // It's currently sitting next to the sidebar — send it home
                // first, then follow it.
                let e = self.embedded.take().unwrap();
                let result = tmux::unembed(&e.pane_id, &e.home)
                    .and_then(|_| tmux::focus_pane(&e.pane_id));
                self.status_msg = result.err().map(|err| err.to_string());
                return;
            }
        }
        let result = match &row.pane {
            Some(pane) => tmux::focus(pane),
            None => match &row.conv.cwd {
                Some(dir) => tmux::open_in_new_window(
                    dir,
                    &format!("claude --resume {}", row.conv.session_id),
                ),
                None => Err(anyhow::anyhow!("conversation has no recorded directory")),
            },
        };
        self.status_msg = result.err().map(|e| e.to_string());
    }

    fn draw(&mut self, f: &mut Frame) {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(f.area());

        // With a real claude pane joined in to the right, the whole TUI pane
        // is the sidebar — skip the jsonl preview.
        if self.embedded.is_some() {
            self.draw_list(f, outer[0]);
        } else {
            let main = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
                .split(outer[0]);
            self.draw_list(f, main[0]);
            self.draw_preview(f, main[1]);
        }
        self.draw_footer(f, outer[1]);
    }

    fn draw_list(&self, f: &mut Frame, area: Rect) {
        let now = SystemTime::now();
        let running = self.rows.iter().filter(|r| r.status == Status::Running).count();
        let waiting = self.rows.iter().filter(|r| r.status == Status::Waiting).count();

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
                    let row = &self.rows[*i];
                    let (dot, color) = match row.status {
                        Status::Running => ("●", Color::Yellow),
                        Status::Waiting => ("●", Color::Green),
                        Status::Idle => ("○", Color::DarkGray),
                    };
                    let title_style = if idx == self.selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else if row.status == Status::Idle {
                        Style::default().fg(Color::Gray)
                    } else {
                        Style::default()
                    };
                    let marker = if self
                        .embedded
                        .as_ref()
                        .is_some_and(|e| e.conv_id == row.conv.session_id)
                    {
                        "▸ "
                    } else {
                        "  "
                    };
                    let mut spans = vec![
                        Span::raw(marker),
                        Span::styled(dot, Style::default().fg(color)),
                        Span::raw(" "),
                        Span::styled(
                            format!("{:>4} ", age(now, row.conv.mtime)),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(row.conv.display_title().to_string(), title_style),
                    ];
                    if let Some(branch) = &row.conv.git_branch {
                        spans.push(Span::styled(
                            format!("  {branch}"),
                            Style::default().fg(Color::Magenta).add_modifier(Modifier::DIM),
                        ));
                    }
                    ListItem::new(Line::from(spans))
                }
            })
            .collect();

        let title = format!(
            " orcim — {running} running · {waiting} waiting{} ",
            if self.show_all { " · all" } else { "" }
        );
        let mut state = ListState::default().with_selected(Some(self.selected));
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().bg(Color::Rgb(45, 50, 68)));
        f.render_stateful_widget(list, area, &mut state);
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL);
        let Some(row) = self.selected_row() else {
            f.render_widget(
                Paragraph::new("no conversation selected").block(block),
                area,
            );
            return;
        };

        let id = row.conv.session_id.clone();
        let mtime = row.conv.mtime;
        let path = row.conv.jsonl_path.clone();
        let stale = self
            .preview
            .get(&id)
            .map(|(t, _)| *t != mtime)
            .unwrap_or(true);
        if stale {
            let msgs = discovery::tail_messages(&path, 12);
            self.preview.insert(id.clone(), (mtime, msgs));
        }
        let row = self.selected_row().unwrap();
        let msgs = &self.preview[&id].1;

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("status ", Style::default().fg(Color::DarkGray)),
            Span::raw(row.status.label()),
            Span::styled("   pane ", Style::default().fg(Color::DarkGray)),
            Span::raw(
                row.pane
                    .as_ref()
                    .map(|p| p.target())
                    .unwrap_or_else(|| "-".into()),
            ),
            Span::styled("   branch ", Style::default().fg(Color::DarkGray)),
            Span::raw(row.conv.git_branch.clone().unwrap_or_else(|| "-".into())),
        ]));
        lines.push(Line::from(""));
        for (role, text) in msgs {
            let style = match *role {
                "you" => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                _ => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            };
            lines.push(Line::from(Span::styled(format!("── {role}"), style)));
            for l in text.lines() {
                lines.push(Line::from(l.to_string()));
            }
            lines.push(Line::from(""));
        }
        if msgs.is_empty() {
            lines.push(Line::from(Span::styled(
                "(no text messages found in tail)",
                Style::default().fg(Color::DarkGray),
            )));
        }

        // Scroll so the newest messages are visible.
        let inner_w = area.width.saturating_sub(2).max(1) as usize;
        let inner_h = area.height.saturating_sub(2) as usize;
        let total: usize = lines
            .iter()
            .map(|l| {
                let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
                w.div_ceil(inner_w).max(1)
            })
            .sum();
        let scroll = total.saturating_sub(inner_h) as u16;

        let title = format!(" {} ", crate::truncate(row.conv.display_title(), 60));
        let para = Paragraph::new(lines)
            .block(block.title(title))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        f.render_widget(para, area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let text = if self.filter_input {
            format!("/{}▏  (enter: keep, esc: clear)", self.filter)
        } else if let Some(msg) = &self.status_msg {
            format!("error: {msg}")
        } else {
            let filter = if self.filter.is_empty() {
                String::new()
            } else {
                format!("  filter: {}  ", self.filter)
            };
            format!(
                "{filter}enter chat here · o goto · n new here · a all · / filter · q quit"
            )
        };
        let style = if self.status_msg.is_some() && !self.filter_input {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        f.render_widget(Paragraph::new(text).style(style).dim(), area);
    }
}
