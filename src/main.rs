mod discovery;
mod procs;
mod status;
mod tmux;
mod ui;

use anyhow::Result;
use discovery::Store;
use status::{Annotated, Status};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => ui::run(),
        Some("list") => list(args.iter().any(|a| a == "--all")),
        Some("watch") => watch(),
        Some(other) => anyhow::bail!("unknown command: {other} (expected: list, watch)"),
    }
}

fn snapshot(store: &mut Store) -> Result<Vec<Annotated>> {
    store.refresh()?;
    let convs = store.conversations();
    let procs = procs::scan();
    let panes = tmux::list_panes();
    Ok(status::annotate(&convs, &procs, &panes))
}

fn list(all: bool) -> Result<()> {
    let mut store = Store::new()?;
    let rows = snapshot(&mut store)?;
    let week = Duration::from_secs(7 * 24 * 3600);
    let now = SystemTime::now();

    // rows are sorted most-recent-first, so projects appear in recency order.
    let mut groups: Vec<(String, Vec<&Annotated>)> = Vec::new();
    for row in &rows {
        let recent = now
            .duration_since(row.conv.mtime)
            .map(|age| age < week)
            .unwrap_or(true);
        if !all && !recent && row.status == Status::Idle {
            continue;
        }
        let project = display_dir(&row.conv.project_dir());
        match groups.iter_mut().find(|(name, _)| *name == project) {
            Some((_, list)) => list.push(row),
            None => groups.push((project, vec![row])),
        }
    }

    if groups.is_empty() {
        println!("no conversations found");
    }
    for (project, list) in &groups {
        println!("\n{project}");
        for row in list {
            let pane = row
                .pane
                .as_ref()
                .map(|p| p.target())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "  {} {:7}  {:>4}  {:12}  {}",
                status_icon(row.status),
                row.status.label(),
                age(now, row.conv.mtime),
                pane,
                truncate(row.conv.display_title(), 70),
            );
        }
    }
    Ok(())
}

fn watch() -> Result<()> {
    let mut store = Store::new()?;
    let mut last: HashMap<String, Status> = HashMap::new();
    println!("polling every 1s, printing status transitions (ctrl-c to stop)");
    loop {
        let rows = snapshot(&mut store)?;
        let now = SystemTime::now();
        for row in &rows {
            let id = row.conv.session_id.clone();
            let prev = last.insert(id, row.status);
            if prev.is_some_and(|p| p != row.status) {
                let prev = prev.unwrap();
                println!(
                    "{}  {}  {}  {} → {}",
                    hhmmss(now),
                    display_dir(&row.conv.project_dir()),
                    truncate(row.conv.display_title(), 50),
                    prev.label(),
                    row.status.label(),
                );
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

pub fn status_icon(status: Status) -> &'static str {
    match status {
        Status::Running => "\x1b[33m●\x1b[0m",
        Status::Waiting => "\x1b[32m●\x1b[0m",
        Status::Idle => "\x1b[90m○\x1b[0m",
    }
}

pub fn display_dir(dir: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) => dir.replacen(&home, "~", 1),
        Err(_) => dir.to_string(),
    }
}

pub fn age(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

pub fn hhmmss(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = secs % 86400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}
