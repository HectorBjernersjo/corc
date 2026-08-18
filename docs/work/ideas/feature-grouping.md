# Feature grouping: label conversations by feature, not directory

## Problem

Today a sidebar section *is* a cwd. That couples three things that don't
belong together: feature ↔ checkout ↔ conversation. In practice (HRM) features
get split, merged, and stacked on unmerged features, and the checkout is just
an execution slot — but the sidebar can only show directories. With a pool of
generic jj workspaces (`ws1`–`ws4`), every section would be named `wsN`, the
user has to pick a free slot manually, and nothing shows what a conversation
is actually about.

## Idea

Make **feature** an optional per-conversation label and a second grouping
dimension, with cwd as the fallback. The agent labels its own conversation.

1. **Model**: `Conversation.feature: Option<String>` (`#[serde(default)]`,
   same compat pattern as `pinned`/`browser`). Grouping key becomes
   `feature.unwrap_or(cwd)` — unlabeled conversations and all other projects
   behave exactly as today. cwd stays mandatory regardless: it's the
   transcript locator (`discovery::locate_jsonl`), spawn dir, and real-session
   key. Feature is a label on top, never a replacement — a conversation stays
   pinned to its checkout for life (resume is cwd-bound), the label just makes
   that invisible.
2. **Agent sets the label**: pass `CORC_CONV_ID=<uuid>` into the agent pane
   (`-e`, same precedent as the browser profile in `tmux::spawn_conversation`)
   and add `corc set-feature <id> <name>`. TUI and external command must not
   both write state.json — set-feature drops a small file the TUI folds in on
   refresh (cf. `merge_disk_directories`). Then a repo CLAUDE.md/skill line —
   "when you pick up a feature, run set-feature" — gives: user says "fix X on
   pr-1", agent does `jj new` on that stack *and* moves the conversation to
   the right section. Splitting a feature mid-conversation = relabel.
3. **Slot allocation**: `App::new_conversation_in` is the single spawn funnel.
   For a pooled project, replace the directory pick with a feature pick
   (reuse `widget::run_filter_picker`, "pick existing or type new") and let
   corc choose a free slot — a pool dir with no live conversation. Collision
   watching becomes corc's job. Spawning unlabeled and letting the agent
   label it once told what to do also works; both paths coexist since the
   field is optional.

## Known seams (from code exploration, 2026-08-18)

- Grouping is read in few places: the cwd string-equality filter in
  `App::rebuild_items` (ui.rs), `attention_ids` project_rank, `jump_project`,
  `move_project`, `State::add_conversation`/`prune_empty_projects`.
- Header text: `repo::labels`/`label_for` are path-shaped (grow-leftward
  uniqueness); a feature label is human-chosen and unique, so it bypasses
  them — group yields its own display name, path label is the fallback branch.
- Staleness: `project_is_listed` ("is this path still listed?") is
  meaningless for a feature group; needs its own liveness rule (e.g. stale
  when no conversation in the history window). `MAX_PER_PROJECT` then caps a
  feature, not a directory.
- Touches PLAN.md D8 ("the path is the identity; the name is only a label"):
  feature becomes a second identity beside the path, not instead of it.

## Rejected: deriving the feature from jj

Could infer the label from which bookmark/stack the slot's `@` sits on.
Worse: costs jj processes on refresh (against the no-VCS-in-draw-loop
discipline in repo.rs), flickers as the agent hops between changes, and a
conversation can be "about pr-3" before touching the repo. Explicit label set
by the agent is simpler and means what it says.

## Per-project models this enables (context, not corc's concern)

- **HRM**: fungible slot pool — repointing a slot is an incremental build, so
  any feature can run in any free slot.
- **kubernetes-infra**: slots carry sticky runtime state (cluster, db,
  migrations), so they're *not* fungible — there the model is environments
  with affinity: each slot declares what's deployed (`.env-state`), the
  repo-side skill places work where switching is cheapest, and a scripted
  "reset env to revision X" (seed + forward migrations from zero) is the
  lever that restores near-fungibility. corc needs nothing extra for this;
  the feature label + cwd fallback is agnostic to which model a project runs.
