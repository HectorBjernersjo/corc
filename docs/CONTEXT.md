# corc

A tmux-native hub for Claude Code: one TUI that owns, monitors and switches
between all Claude Code conversations, so the user never interacts with
Claude Code outside of it.

## Language

**Conversation**:
A Claude Code session — its jsonl transcript plus, when live, its process and pane.
_Avoid_: chat, task

**Hidden session** (`_corc-sessions`):
The single global tmux session where corc keeps every Claude pane it owns; filtered out of the user's session picker (`new.sh`).
_Avoid_: background session, corc server

**Sidebar**:
The pane running the corc TUI — a narrow, fixed-width list of conversations grouped by project.

**Project**:
A directory a conversation runs in, shown by basename only (or `{repo}/{worktree}` for git worktrees, detected via the `.git` file's `gitdir:` pointer).
_Avoid_: folder path, cwd (in UI contexts)

**Move mode**:
Sidebar mode (entered with `V`) where `K`/`J` move the selected project up/down; the order is persisted in the state file.

**Digit jump**:
Pressing `1`–`9` switches the client to window N of the selected project's **Real session**, creating the session (with its `.tmux.sh` hook) and the window if missing. Window 1 is the editor window: created running nvim, and an idle shell there gets `nvim` typed into it — but a busy foreground process is never disturbed.

**Directory picker**:
The `n` overlay: a ratatui-native filter over `directories.txt` expanded with git worktrees (same source and expansion as `new.sh`). Selecting a directory spawns a fresh Claude in a hidden-session window and swaps it in immediately; Esc cancels.

**corc session**:
The visible tmux session named `_corc` where the TUI itself lives (underscore-prefixed so it never clashes with a project session named after a directory). `Ctrl+q` (root-table tmux binding → `corc open`) creates it and starts the TUI if needed, then switches the client there. On quit corc swaps the viewed pane home and removes the content pane it created.

**Real session**:
The user's normal tmux session for a project (created by `new.sh`, named after the directory) — where nvim etc. live, as opposed to the hidden session.

**Content pane**:
The pane next to the sidebar where the selected conversation's Claude pane is swapped in (see ADR-0001); holds a placeholder when nothing is selected.

**State file**:
corc's persistent record (`~/.local/state/corc/state.json`) of every conversation it has spawned (id, cwd) plus per-conversation last-viewed times; what makes dead conversations listable and resumable across tmux/reboots.

### Conversation states

**Running** (yellow ●):
A live pane whose turn is in flight. Shows elapsed time since the turn started (`4m`, `1h12m`).

**Unseen** (blue ●):
A live pane whose turn completed after the user last viewed it. Shows how long the completed turn ran.

**Idle** (gray ●):
A live pane, turn complete, viewed since completion. The conversation in the content pane counts as continuously viewed — it goes straight to Idle, never Unseen. Shows nothing under 1h, then coarse age (`5h`).

**Dead** (hollow ○):
A conversation with no pane; resumable from the state file via `claude --resume <id>`. Shows coarse age (`5h`, `3d`), never finer than hours.

Within a project: live conversations above dead ones, most recently active first — but rows only re-sort on a state change, never while the user is looking at an unchanged list. Seconds are never shown anywhere.

_Known limitation_: a Claude blocked on a permission prompt is mid-turn in the jsonl, so it shows as **Running**; distinguishing it needs a Notification hook (deliberately out of scope for now).

### Lifecycle

- Claude exits (or crashes) → corc kills the now-shell-only parked window and marks the conversation **Dead** in the state file: still listed, hollow, resumable.
- `x` on a live conversation kills its Claude and window (`y/n` confirm if **Running**); `x` on a **Dead** one removes it from the state file and the list. The jsonl under `~/.claude` is never touched.
- **Dead** conversations older than a week are hidden by default; the `a` toggle shows everything.

## Relationships

- The **Hidden session** holds one tmux window per live **Conversation**.
- corc spawns every Claude with `claude --session-id <uuid>`, so the pane ↔ conversation mapping is exact bookkeeping, never cwd-based guessing.
- A **Project** has at most one **Real session** and any number of **Conversations**.
- A **Conversation** exists for corc only if corc spawned it. Pre-existing `~/.claude/projects` history and manually started `claude` processes are invisible — there is no process scanning and no adoption.
- **Projects** keep a fixed, user-managed order: a new project is appended when its first conversation is spawned, and the user rearranges via **Move mode**. The order never changes on its own.

## Example dialogue

> **Dev:** "The user pressed Enter on a conversation with no live pane — do I search for a matching claude process?"
> **Domain expert:** "No. If corc didn't spawn it, there is no pane. Spawn `claude --resume <id> --session-id` in a new hidden-session window and record the pane id."

## Flagged ambiguities

- "session" was overloaded (tmux session vs Claude Code session) — resolved: **Conversation** means the Claude Code session; "session" alone always means a tmux session.
