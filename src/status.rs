//! Conversation status, derived from exact bookkeeping: pane liveness from
//! the state file + tmux, turn state and timing from the jsonl, `last_viewed`
//! from the state file. No guessing (D1).
//!
//! The four states and their time columns (PLAN.md D6):
//!
//! | State   | Condition                                | Time column            |
//! |---------|------------------------------------------|------------------------|
//! | Running | pane alive, turn in flight               | elapsed since turn start |
//! | Unseen  | pane alive, turn completed after viewing | completed turn's duration |
//! | Idle    | pane alive, turn complete, viewed since  | empty < 1h, else coarse |
//! | Dead    | no pane                                  | coarse age, hours+     |
//!
//! Every time column is a single largest unit — `9s`, `4m`, `2h`, `3d`, `5w`
//! — so the column stays narrow. Idle/Dead are never finer than hours.

use crate::discovery::{Meta, TurnState};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Pane alive, turn in flight.
    Running,
    /// Pane alive, turn completed after the user last viewed it.
    Unseen,
    /// Pane alive, turn complete, viewed since completion.
    Idle,
    /// No pane; resumable from the state file.
    Dead,
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Unseen => "unseen",
            Status::Idle => "idle",
            Status::Dead => "dead",
        }
    }
}

/// Derive the state per the D6 table. `is_viewed` marks the conversation
/// currently in the content pane: it counts as continuously viewed, so it
/// goes straight to Idle and never turns Unseen.
pub fn derive(pane_alive: bool, meta: Option<&Meta>, last_viewed: u64, is_viewed: bool) -> Status {
    if !pane_alive {
        return Status::Dead;
    }
    match meta.map(|m| m.turn_state).unwrap_or(TurnState::Unknown) {
        TurnState::Mid => Status::Running,
        TurnState::Complete if !is_viewed => match meta.and_then(|m| m.turn_completed_at) {
            Some(completed) if completed > last_viewed => Status::Unseen,
            _ => Status::Idle,
        },
        // Complete-and-viewed, or a fresh pane with no turn yet.
        TurnState::Complete | TurnState::Unknown => Status::Idle,
    }
}

/// The per-state time column (D6/D7). `now` and `created_at` in unix seconds.
pub fn time_column(status: Status, meta: Option<&Meta>, created_at: u64, now: u64) -> String {
    match status {
        Status::Running => meta
            .and_then(|m| m.turn_started_at)
            .map(|start| fmt_duration(now.saturating_sub(start)))
            .unwrap_or_default(),
        Status::Unseen => meta
            .and_then(|m| m.turn_started_at.zip(m.turn_completed_at))
            .map(|(start, done)| fmt_duration(done.saturating_sub(start)))
            .unwrap_or_default(),
        Status::Idle => coarse_age(now.saturating_sub(last_activity(meta, created_at))),
        Status::Dead => coarse_age(now.saturating_sub(last_activity(meta, created_at))),
    }
}

/// When the conversation last did anything, in unix seconds: the jsonl mtime
/// when known, else the spawn time.
pub fn last_activity(meta: Option<&Meta>, created_at: u64) -> u64 {
    meta.map(|m| m.mtime)
        .filter(|t| *t != SystemTime::UNIX_EPOCH)
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(created_at)
}

/// Largest-unit duration: `9s`, `4m`, `2h`, `3d`, `5w`. Always exactly one
/// unit, so the time column stays narrow.
fn fmt_duration(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    match secs {
        s if s < MIN => format!("{s}s"),
        s if s < HOUR => format!("{}m", s / MIN),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < WEEK => format!("{}d", s / DAY),
        s => format!("{}w", s / WEEK),
    }
}

/// Coarse age for Idle/Dead: single unit, never finer than hours, empty
/// under an hour.
fn coarse_age(secs: u64) -> String {
    if secs < 3600 {
        String::new()
    } else {
        fmt_duration(secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(state: TurnState, started: Option<u64>, completed: Option<u64>) -> Meta {
        Meta {
            turn_state: state,
            turn_started_at: started,
            turn_completed_at: completed,
            mtime: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000),
            ..Meta::default()
        }
    }

    /// The D6 table.
    #[test]
    fn derive_states() {
        let running = meta(TurnState::Mid, Some(100), None);
        let done = meta(TurnState::Complete, Some(100), Some(500));

        // No pane ⇒ Dead, whatever the jsonl says.
        assert_eq!(derive(false, Some(&running), 0, false), Status::Dead);
        // Turn in flight ⇒ Running, even while viewed.
        assert_eq!(derive(true, Some(&running), 0, false), Status::Running);
        assert_eq!(derive(true, Some(&running), 0, true), Status::Running);
        // Completed after last_viewed ⇒ Unseen…
        assert_eq!(derive(true, Some(&done), 200, false), Status::Unseen);
        // …but the viewed conversation counts as continuously viewed.
        assert_eq!(derive(true, Some(&done), 200, true), Status::Idle);
        // Viewed since completion ⇒ Idle.
        assert_eq!(derive(true, Some(&done), 600, false), Status::Idle);
        // Fresh pane, no transcript yet ⇒ Idle.
        assert_eq!(derive(true, None, 0, false), Status::Idle);
    }

    /// Time column per state; seconds never appear.
    #[test]
    fn time_columns() {
        let now = 1_000_000;
        let running = meta(TurnState::Mid, Some(now - 4 * 60 - 30), None);
        assert_eq!(time_column(Status::Running, Some(&running), 0, now), "4m");

        // Under a minute shows seconds; only ever one unit past that.
        let fresh = meta(TurnState::Mid, Some(now - 9), None);
        assert_eq!(time_column(Status::Running, Some(&fresh), 0, now), "9s");
        let long = meta(TurnState::Mid, Some(now - 3600 - 12 * 60), None);
        assert_eq!(time_column(Status::Running, Some(&long), 0, now), "1h");

        let done = meta(TurnState::Complete, Some(1000), Some(1000 + 25 * 60));
        assert_eq!(time_column(Status::Unseen, Some(&done), 0, now), "25m");

        // Idle: empty under an hour of age, then coarse.
        let idle = |mtime: u64| Meta {
            mtime: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(mtime),
            ..Meta::default()
        };
        assert_eq!(time_column(Status::Idle, Some(&idle(now - 300)), 0, now), "");
        assert_eq!(
            time_column(Status::Idle, Some(&idle(now - 5 * 3600)), 0, now),
            "5h"
        );

        // Dead: coarse age, days past 24h, never finer than hours.
        assert_eq!(
            time_column(Status::Dead, Some(&idle(now - 5 * 3600 - 59 * 60)), 0, now),
            "5h"
        );
        assert_eq!(
            time_column(Status::Dead, Some(&idle(now - 3 * 86_400)), 0, now),
            "3d"
        );
        // Past a week, collapse to weeks — still a single unit.
        assert_eq!(
            time_column(Status::Dead, Some(&idle(now - 8 * 86_400)), 0, now),
            "1w"
        );
        // No jsonl yet: age falls back to created_at.
        assert_eq!(time_column(Status::Dead, None, now - 7200, now), "2h");
    }
}
