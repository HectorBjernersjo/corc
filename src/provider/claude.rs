//! Claude Code. corc generates the session uuid itself and passes it to
//! `--session-id` (new) / `--resume` (revive), along with a `--settings` file
//! that makes Claude report what it is doing through corc's hooks
//! (`crate::hooks`, ADR-0004).
//!
//! Metadata comes from three places, in descending order of how much corc
//! trusts them. The hook log is the record of what the agent did and is the
//! only source for turn state, timing and open questions. The jsonl transcript
//! is read for titles and for a `/cd` no hook has followed yet, because a
//! generated `ai-title`, a `/rename` and the `relocated` record exist nowhere
//! else. And Claude's own live session registry closes a turn the user
//! interrupted with Esc, which fires no hook at all.

use super::Provider;
use crate::discovery;
use crate::discovery::{Known, Meta, MetaSource, Store, TurnState};
use crate::{hooks, state, usage};
use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

pub struct Claude;

impl Provider for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn binary(&self) -> &'static str {
        "claude"
    }

    fn new_session_id(&self, _dir: &Path) -> Result<String> {
        state::new_uuid()
    }

    /// `--settings` is what installs corc's hooks for this pane only. Claude
    /// merges it over the user's own settings, so their hooks keep running and
    /// nothing under ~/.claude is written to. A settings file corc cannot
    /// write is not worth refusing to spawn over: the conversation runs, and
    /// the sidebar falls back to knowing only whether the pane is alive.
    fn spawn_args(&self, id: &str, resume: bool) -> Vec<String> {
        spawn_args(id, resume, hooks::settings_file().ok().as_deref())
    }

    /// `/cd` (v2.1.169+) relocates the session: transcript, `--resume` lookup
    /// and CLAUDE.md all follow the new directory. It is user-only inside the
    /// agent, which is exactly why corc types it (ADR-0003).
    fn cd_command(&self, dir: &Path) -> Option<String> {
        Some(format!("/cd {}", dir.display()))
    }

    fn meta_source(&self) -> Result<Box<dyn MetaSource>> {
        Ok(Box::new(ClaudeSource {
            hooks: hooks::store()?,
            titles: Store::titles()?,
            merged: HashMap::new(),
        }))
    }

    /// Plan usage from the OAuth usage endpoint, with the token Claude Code
    /// keeps in `~/.claude/.credentials.json`. corc only reads the token;
    /// refresh is Claude Code's job — an expired one just makes the fetch
    /// 401 and the menu keeps the previous snapshot.
    fn fetch_usage(&self) -> Option<Vec<usage::Entry>> {
        let token = access_token()?;
        let body = usage::curl_get(
            USAGE_URL,
            &[
                format!("Authorization: Bearer {token}"),
                "anthropic-beta: oauth-2025-04-20".to_string(),
            ],
        )?;
        let resp: UsageResponse = serde_json::from_slice(&body).ok()?;
        let entries: Vec<usage::Entry> = resp.limits.iter().filter_map(usage_entry).collect();
        (!entries.is_empty()).then_some(entries)
    }
}

/// The argv Claude is spawned with. Split out from the trait method so the
/// settings file is an argument rather than something read from the
/// environment mid-call.
fn spawn_args(id: &str, resume: bool, settings: Option<&Path>) -> Vec<String> {
    let flag = if resume { "--resume" } else { "--session-id" };
    let mut args = vec![flag.to_string(), id.to_string()];
    if let Some(settings) = settings {
        args.push("--settings".to_string());
        args.push(settings.to_string_lossy().into_owned());
    }
    args
}

/// Claude's metadata reader: the hook log for what the agent is doing, the
/// transcript for what it is called, and the live session registry for the one
/// transition neither of them reports.
struct ClaudeSource {
    hooks: Store,
    titles: Store,
    /// The two stores folded together, rebuilt per refresh. `MetaSource` hands
    /// out a borrowed `Meta`, so the merge has to live somewhere.
    merged: HashMap<String, Meta>,
}

impl MetaSource for ClaudeSource {
    fn refresh(&mut self, known: &[Known]) -> Result<()> {
        self.hooks.refresh(known)?;
        self.titles.refresh(known)?;
        let live = live_statuses();
        self.merged = known
            .iter()
            .filter_map(|k| {
                let hooks = self.hooks.meta(&k.id);
                let titles = self.titles.meta(&k.id);
                // A conversation corc spawned before it installed hooks, or one
                // started outside corc, has no log — it still has a transcript
                // worth a title and a live status worth reading. Nothing known
                // from any source is what means "nothing known".
                let mut meta = hooks.or(titles).cloned().or_else(|| {
                    live.contains_key(&k.id).then(Meta::default)
                })?;
                if let Some(titles) = titles {
                    meta.custom_title = titles.custom_title.clone();
                    meta.title = titles.title.clone();
                    meta.has_content |= titles.has_content;
                }
                // `/cd` fires no hook, so the hook log keeps naming the old
                // directory until the next prompt. The transcript's own
                // `relocated` record says where it went, and the file having
                // actually moved there is what makes it count — a stale one
                // (moved on, or written by a `/cd` that never completed)
                // mangles to a directory the file no longer sits in.
                // Only the hook log's cwd is taken as reported; the hook
                // process vouched for it at write time (`hooks::session_dir`).
                let transcript = self.titles.path(&k.id);
                let relocated = titles
                    .and_then(|t| t.cwd.as_deref())
                    .and_then(|cwd| home_of(transcript, cwd));
                let reported = hooks.and_then(|h| h.cwd.clone());
                meta.cwd = relocated
                    .or(reported)
                    .or_else(|| home_of(transcript, &k.cwd));
                if let Some((status, at)) = live.get(&k.id) {
                    apply_live_status(&mut meta, status, *at);
                }
                Some((k.id.clone(), meta))
            })
            .collect();
        Ok(())
    }

    fn meta(&self, id: &str) -> Option<&Meta> {
        self.merged.get(id)
    }

    fn save_cache(&mut self) {
        self.hooks.save_cache();
        self.titles.save_cache();
    }
}

/// Where a conversation lives, worked out from where its transcript sits.
///
/// Claude keeps transcripts under a directory named by the mangled session cwd,
/// and the mangling is lossy, so corc cannot read the home off the path — but
/// it can check a candidate against it. `recorded` is what state.json believes
/// (or what a `relocated` record claims), which is right almost always and one
/// directory too deep when something has pushed a subdirectory in there
/// (`corc cd` into a path that does not exist, or the shell-cwd bug this rule
/// was restored for). Walking up its ancestors finds the real home in one
/// step, and finding nothing leaves state alone rather than guessing.
///
/// This is the repair half of ADR-0003: whatever goes wrong upstream, a
/// conversation's recorded directory converges back on the one Claude files its
/// transcript under.
fn home_of(transcript: Option<&Path>, recorded: &Path) -> Option<PathBuf> {
    let project = transcript?.parent()?.file_name()?.to_str()?;
    recorded
        .ancestors()
        .find(|dir| discovery::mangled(&dir.to_string_lossy()) == project)
        .map(Path::to_path_buf)
}

/// Fold Claude's own live status into a conversation's metadata.
///
/// This carries two cases the hook log cannot. Interrupting a turn with Esc is
/// silent — no `Stop`, no `Notification`, nothing in the transcript until the
/// next prompt — so without this a row sits on Running for the full
/// stalled-turn hour. And a conversation whose pane started before corc
/// installed its hooks, or outside corc entirely, reports nothing at all: the
/// registry is the only thing that knows it is working.
///
/// `at` is when Claude last changed what it says it is doing. It only counts
/// when it is newer than the conversation's own last sign of progress, which
/// makes the rule self-limiting: a hooked conversation mid-turn logs a tool
/// call every few seconds and the registry never gets a word in, while a
/// conversation with no log at all is described by it entirely.
fn apply_live_status(meta: &mut Meta, status: &str, at: u64) {
    if meta.turn_progress_at.is_some_and(|progress| progress > at) {
        return;
    }
    match status {
        // At its prompt with nothing pending, so any turn still open is over.
        // An open question survives untouched: only its own answer closes it,
        // and a conversation blocked on the user is the last one that should
        // quietly go grey.
        IDLE if meta.turn_state == TurnState::Mid => {
            meta.turn_state = TurnState::Complete;
            meta.turn_completed_at = Some(at);
        }
        // Working. For a conversation corc has hooks for this only ever
        // confirms what the log already said; for one it does not, this is the
        // whole signal, and the status change is when the turn began.
        BUSY => {
            if meta.turn_state != TurnState::Mid {
                meta.turn_state = TurnState::Mid;
                meta.turn_started_at = Some(at);
                meta.turn_completed_at = None;
            }
            meta.turn_progress_at = Some(at);
        }
        // `waiting` is Claude holding a question or a permission prompt in
        // front of the user. The hook log says which, when there is one; on
        // its own it is not enough to name the state, so the turn is left in
        // flight (PLAN.md D6).
        _ => {}
    }
}

/// Claude's session statuses, as observed in 2.1.251: working, at its prompt,
/// or holding something in front of the user. Anything else corc has not seen
/// falls through to changing nothing.
const BUSY: &str = "busy";
const IDLE: &str = "idle";

/// What Claude says each of its running sessions is doing, from the file it
/// keeps per session under `~/.claude/sessions`. Every failure is treated as no
/// information: the directory is undocumented, so a rename or a reshuffle of it
/// costs the cases in `apply_live_status` and nothing else.
///
/// A session can have more than one file — a resumed session leaves the old
/// pid's behind — so the newest stamp wins.
fn live_statuses() -> HashMap<String, (String, u64)> {
    let Ok(home) = std::env::var("HOME") else {
        return HashMap::new();
    };
    let Ok(entries) = std::fs::read_dir(PathBuf::from(home).join(".claude/sessions")) else {
        return HashMap::new();
    };
    let mut live: HashMap<String, (String, u64)> = HashMap::new();
    for entry in entries.flatten() {
        if !entry.path().extension().is_some_and(|x| x == "json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(session) = serde_json::from_str::<SessionEntry>(&raw) else {
            continue;
        };
        let at = session.status_updated_at / 1000;
        let slot = live.entry(session.session_id).or_insert((String::new(), 0));
        if at >= slot.1 {
            *slot = (session.status, at);
        }
    }
    live
}

/// The fields corc reads out of a live session file. Everything else in there
/// (pid, socket path, tmux pane, model) is Claude's business.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionEntry {
    session_id: String,
    status: String,
    /// Unix milliseconds.
    #[serde(default)]
    status_updated_at: u64,
}

#[derive(Deserialize)]
struct UsageResponse {
    limits: Vec<Limit>,
}

#[derive(Deserialize)]
struct Limit {
    kind: String,
    percent: f64,
    scope: Option<Scope>,
}

#[derive(Deserialize)]
struct Scope {
    model: Option<Model>,
}

#[derive(Deserialize)]
struct Model {
    display_name: Option<String>,
}

/// The OAuth access token Claude Code maintains.
fn access_token() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let raw = std::fs::read_to_string(format!("{home}/.claude/.credentials.json")).ok()?;
    let creds: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(
        creds
            .get("claudeAiOauth")?
            .get("accessToken")?
            .as_str()?
            .to_string(),
    )
}

/// Map one API limit to a menu entry. The payload already orders them
/// session, weekly, scoped; unknown kinds are dropped so new server-side
/// limit types never render as garbage.
fn usage_entry(limit: &Limit) -> Option<usage::Entry> {
    let label = match limit.kind.as_str() {
        "session" => "5h".to_string(),
        "weekly_all" => "wk".to_string(),
        "weekly_scoped" => limit
            .scope
            .as_ref()
            .and_then(|s| s.model.as_ref())
            .and_then(|m| m.display_name.as_deref())
            .unwrap_or("model")
            .to_lowercase(),
        _ => return None,
    };
    Some(usage::Entry {
        label,
        percent: limit.percent.clamp(0.0, 100.0).round() as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        BUSY, Claude, ClaudeSource, HashMap, IDLE, Known, Meta, MetaSource, Path, PathBuf,
        SessionEntry, Store, TurnState, UsageResponse, apply_live_status, home_of, hooks,
        spawn_args, usage_entry,
    };
    use crate::discovery;
    use crate::provider::Provider;
    use crate::status::{self, Status};
    use serde_json::json;

    /// A conversation's home is worked out from where Claude files its
    /// transcript, so a recorded directory that has drifted one level too deep
    /// converges back instead of stranding the row in a subdirectory nobody
    /// moved it to (ADR-0003).
    #[test]
    fn a_drifted_directory_converges_on_the_transcripts_own_home() {
        let transcript = Path::new("/h/.claude/projects/-work-app/x.jsonl");

        // The ordinary case: what state.json says is already right.
        assert_eq!(
            home_of(Some(transcript), Path::new("/work/app")),
            Some(PathBuf::from("/work/app"))
        );
        // Drifted into a subdirectory: the real home is an ancestor.
        assert_eq!(
            home_of(Some(transcript), Path::new("/work/app/src/platform")),
            Some(PathBuf::from("/work/app"))
        );
        // Nothing on the path matches, so state is left alone rather than
        // moved somewhere invented.
        assert_eq!(home_of(Some(transcript), Path::new("/elsewhere")), None);
        assert_eq!(home_of(None, Path::new("/work/app")), None);
    }

    /// Claude's own live status, the only thing that knows about two cases the
    /// hook log cannot: a turn interrupted with Esc, and a pane that started
    /// before corc installed its hooks.
    #[test]
    fn the_live_status_covers_what_the_log_cannot() {
        let mid = || Meta {
            turn_state: TurnState::Mid,
            turn_started_at: Some(1000),
            turn_progress_at: Some(1200),
            ..Meta::default()
        };

        // Interrupted: Claude went quiet after the last logged tool call.
        let mut interrupted = mid();
        apply_live_status(&mut interrupted, IDLE, 1300);
        assert_eq!(interrupted.turn_state, TurnState::Complete);
        assert_eq!(interrupted.turn_completed_at, Some(1300));
        assert_eq!(
            status::derive(true, Some(&interrupted), 0, false, 1400, 0),
            Status::Unseen
        );

        // A status older than the last hook is stale, and stale never wins.
        let mut working = mid();
        apply_live_status(&mut working, IDLE, 1100);
        assert_eq!(working.turn_state, TurnState::Mid);

        // No log at all: the status is the whole story, and the moment it
        // turned busy is when the turn started.
        let mut unhooked = Meta::default();
        apply_live_status(&mut unhooked, BUSY, 1000);
        assert_eq!(unhooked.turn_state, TurnState::Mid);
        assert_eq!(
            status::derive(true, Some(&unhooked), 0, false, 1240, 0),
            Status::Running
        );
        assert_eq!(status::time_column(Status::Running, Some(&unhooked), 0, 1240), "4m");
        apply_live_status(&mut unhooked, IDLE, 1300);
        assert_eq!(unhooked.turn_state, TurnState::Complete);

        // Busy does not restart a turn already in flight, so the elapsed
        // clock never jumps back to zero mid-turn.
        let mut running = mid();
        apply_live_status(&mut running, BUSY, 1250);
        assert_eq!(running.turn_started_at, Some(1000));
        assert_eq!(running.turn_progress_at, Some(1250));

        // An open question is never cleared here. Claude reports `waiting`
        // rather than `idle` while it holds one, and `waiting` names no state
        // on its own — but a conversation blocked on the user is the worst
        // possible thing to silently turn grey, so it survives either way.
        let mut asking = Meta {
            active_question: true,
            question_asked_at: Some(1200),
            ..mid()
        };
        apply_live_status(&mut asking, "waiting", 1300);
        apply_live_status(&mut asking, IDLE, 1300);
        assert!(asking.active_question);
        assert_eq!(
            status::derive(true, Some(&asking), 0, false, 1400, 0),
            Status::Question
        );
    }

    /// The registry file, captured from a running Claude Code 2.1.251. corc
    /// reads three fields out of it and must not care about the rest.
    #[test]
    fn the_session_registry_parses_down_to_three_fields() {
        let entry: SessionEntry = serde_json::from_str(
            r#"{"pid":218600,"sessionId":"047fca63-e9ad-4980-9d79-bec44f2b32d7",
                "cwd":"/home/h/projects/corc","startedAt":1788000296082,"version":"2.1.251",
                "tmux":"_corc:@126.%239","name":"corc-36","nameSource":"derived",
                "messagingSocketPath":"/run/user/1000/cc-socks/218600.sock",
                "status":"busy","updatedAt":1788002714028,"statusUpdatedAt":1788002714028}"#,
        )
        .unwrap();
        assert_eq!(entry.session_id, "047fca63-e9ad-4980-9d79-bec44f2b32d7");
        assert_eq!(entry.status, BUSY);
        assert_eq!(entry.status_updated_at / 1000, 1788002714);
    }

    /// The whole reader, over real files: a conversation that reports through
    /// hooks, and one from before corc installed them. The merge is the only
    /// place the two sources meet, and getting it wrong silently blanks the
    /// sidebar for every conversation that predates this.
    #[test]
    fn hooks_and_transcripts_merge_into_one_conversation_each() {
        let root = std::env::temp_dir().join("corc-test-claude-source");
        let _ = std::fs::remove_dir_all(&root);
        let (logs, transcripts) = (root.join("hooks"), root.join("projects"));
        let cwd = "/work/app";
        let project = transcripts.join(discovery::mangled(cwd));
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::create_dir_all(&project).unwrap();

        // Hooked: a finished turn, plus a title only the transcript knows.
        std::fs::write(
            logs.join("hooked.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"e":"prompt","t":1000,"dir":cwd,"text":"fix the sidebar"}),
                json!({"e":"stop","t":1200,"dir":cwd}),
            ),
        )
        .unwrap();
        std::fs::write(
            project.join("hooked.jsonl"),
            format!("{}\n", json!({"type":"ai-title","aiTitle":"Sidebar rewrite"})),
        )
        .unwrap();
        // Spawned before hooks existed: a transcript and nothing else.
        std::fs::write(
            project.join("older.jsonl"),
            format!("{}\n", json!({"type":"custom-title","customTitle":"the old one"})),
        )
        .unwrap();

        let mut source = ClaudeSource {
            hooks: hooks::store_in(logs),
            titles: Store::titles_in(transcripts.clone()),
            merged: HashMap::new(),
        };
        source
            .refresh(&[Known::shown("hooked", cwd), Known::shown("older", cwd)])
            .unwrap();

        let hooked = source.meta("hooked").unwrap();
        assert_eq!(hooked.display_title(), Some("Sidebar rewrite"));
        assert_eq!(hooked.turn_state, TurnState::Complete);
        assert_eq!(hooked.turn_started_at, Some(1000));
        assert_eq!(hooked.turn_completed_at, Some(1200));
        assert_eq!(hooked.cwd.as_deref(), Some(Path::new(cwd)));
        assert!(hooked.has_content);
        assert_eq!(
            status::derive(true, Some(hooked), 0, false, 1300, 0),
            Status::Unseen
        );

        // The old conversation keeps its name and simply says nothing about
        // turns, which reads as Idle rather than as a blank row.
        let older = source.meta("older").unwrap();
        assert_eq!(older.display_title(), Some("the old one"));
        assert_eq!(older.turn_state, TurnState::Unknown);
        assert_eq!(
            status::derive(true, Some(older), 0, false, 1300, 0),
            Status::Idle
        );

        // A conversation with neither is not invented.
        source.refresh(&[Known::shown("neither", cwd)]).unwrap();
        assert!(source.meta("neither").is_none());

        // `/cd`: Claude writes a `relocated` record and moves the file, and
        // fires no hook — the log still ends on the old directory. The row
        // re-homes on the transcript's word, because the file really sits
        // where the record says.
        let next = "/work/next";
        let moved_to = transcripts.join(discovery::mangled(next));
        std::fs::create_dir_all(&moved_to).unwrap();
        let mut transcript = std::fs::read_to_string(project.join("hooked.jsonl")).unwrap();
        transcript.push_str(&format!(
            "{}\n",
            json!({"type":"relocated","relocatedCwd":next})
        ));
        std::fs::write(moved_to.join("hooked.jsonl"), transcript).unwrap();
        std::fs::remove_file(project.join("hooked.jsonl")).unwrap();
        // One refresh notices the file is gone, the next finds it again.
        source.refresh(&[Known::shown("hooked", cwd)]).unwrap();
        source.refresh(&[Known::shown("hooked", cwd)]).unwrap();
        let hooked = source.meta("hooked").unwrap();
        assert_eq!(hooked.cwd.as_deref(), Some(Path::new(next)));
        assert_eq!(hooked.display_title(), Some("Sidebar rewrite"));

        // A record the file's location does not back is just a claim: the
        // transcript never left /work/app, so neither does the row.
        std::fs::write(
            project.join("older.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"type":"custom-title","customTitle":"the old one"}),
                json!({"type":"relocated","relocatedCwd":"/work/elsewhere"}),
            ),
        )
        .unwrap();
        source.refresh(&[Known::shown("older", cwd)]).unwrap();
        assert_eq!(source.meta("older").unwrap().cwd.as_deref(), Some(Path::new(cwd)));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every spawn carries the hook settings, resume included — a revived
    /// conversation that reported nothing would read as permanently idle. A
    /// settings file corc could not write is not worth refusing to spawn over.
    #[test]
    fn every_spawn_installs_the_hooks() {
        let settings = Path::new("/state/corc/claude-hooks.json");
        assert_eq!(
            spawn_args("uuid", false, Some(settings)),
            ["--session-id", "uuid", "--settings", "/state/corc/claude-hooks.json"]
        );
        assert_eq!(
            spawn_args("uuid", true, Some(settings)),
            ["--resume", "uuid", "--settings", "/state/corc/claude-hooks.json"]
        );
        assert_eq!(spawn_args("uuid", true, None), ["--resume", "uuid"]);
    }

    #[test]
    fn cd_command_types_the_slash_command() {
        assert_eq!(
            Claude.cd_command(std::path::Path::new("/work/HRM/feature x")),
            Some("/cd /work/HRM/feature x".to_string())
        );
    }

    #[test]
    fn parses_the_three_usage_limits() {
        let json = r#"{"limits":[
            {"kind":"session","group":"session","percent":21,"severity":"normal","resets_at":"x","scope":null,"is_active":false},
            {"kind":"weekly_all","group":"weekly","percent":24,"severity":"normal","resets_at":"x","scope":null,"is_active":false},
            {"kind":"weekly_scoped","group":"weekly","percent":41,"severity":"normal","resets_at":"x",
             "scope":{"model":{"id":null,"display_name":"Fable"},"surface":null},"is_active":true},
            {"kind":"something_new","group":"?","percent":7,"severity":"normal","resets_at":"x","scope":null,"is_active":false}
        ]}"#;
        let resp: UsageResponse = serde_json::from_str(json).unwrap();
        let shown: Vec<(String, u8)> = resp
            .limits
            .iter()
            .filter_map(usage_entry)
            .map(|e| (e.label, e.percent))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("5h".to_string(), 21),
                ("wk".to_string(), 24),
                ("fable".to_string(), 41),
            ]
        );
    }
}
