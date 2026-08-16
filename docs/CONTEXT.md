# corc

A tmux-native hub for agent CLIs: one TUI that owns, monitors and switches
between conversations created with Claude Code, Codex, Cursor CLI, and
OpenCode.

## Language

**Conversation**:
A provider-owned agent session — its persisted history plus, when live, its
process and pane. Every conversation remembers which provider created it.
_Avoid_: chat, task

**Hidden session** (`_corc-sessions`):
The single global tmux session where corc keeps every agent pane it owns;
filtered out of the user's session picker (`new.sh`).
_Avoid_: background session, corc server

**Sidebar**:
The pane running the corc TUI — a narrow, fixed-width list of conversations grouped by project.

**Status panel**:
The status-driven panel at the top of the Sidebar containing every **Running**,
**Question**, and **Unseen** conversation. It duplicates those rows without
changing the project-grouped conversation list; activating one moves the
selection to its canonical Sidebar row and views it. `j`/`k` cross panel
boundaries and `Ctrl+j`/`Ctrl+k` jump directly between panels.

**Project**:
A directory a conversation runs in, shown by basename only (or `{repo}/{checkout}` for a secondary **Checkout**).
_Avoid_: folder path, cwd (in UI contexts)

**Checkout**:
A working directory of a repo: the main one, a git worktree, or a jj workspace.
corc treats the last two the same — a secondary checkout is detected without
running the VCS (`.git` being a *file* with a `gitdir:` pointer, or `.jj/repo`
being a *file* pointing at the main workspace's repo), and it decides both the
project label and the **Real session** name. Siblings are enumerated with
`git worktree list --porcelain` and `jj workspace list --ignore-working-copy`,
but only for a directory that carries that VCS's marker in itself or an
ancestor, and only once per repo per listing (`repo::Expansions`) — process
startup, `jj`'s in particular, is what a directory listing actually costs.
_Avoid_: worktree (as the general term)

**Scan root**:
A directory-list entry ending in `/*` (e.g. `~/projects/*`), standing for every
**Checkout** up to three levels below it, most recently modified first.
Containers are looked through, checkouts are never descended into, and dotted
directories and symlinks are skipped. A scan root needs no VCS expansion — the
walk finds worktrees and workspaces itself — so it spawns no `git`/`jj`.

**Move mode**:
Sidebar mode (entered with `V`) where `K`/`J` move the selected project up/down; the order is persisted in the state file.

**Digit jump**:
Pressing `1`–`9` switches the client to window N of the selected project's **Real session**, creating the session (with its `.tmux.sh` hook) and the window if missing. Window 1 is the editor window: created running nvim, and an idle shell there gets `nvim` typed into it — but a busy foreground process is never disturbed.

**Directory picker**:
The `N` overlay: a ratatui-native filter over `directories.txt` expanded with
each entry's sibling **Checkout**s, plus every checkout under each **Scan
root**. Selecting a directory
spawns a fresh conversation with the active provider in a hidden-session
window and swaps it in immediately; Esc cancels.

**corc session**:
The visible tmux session named `_corc` where the TUI itself lives (underscore-prefixed so it never clashes with a project session named after a directory). `corc` creates it and starts the TUI through the private `corc __tui` entry point if needed, then attaches or switches the client there. `Ctrl+q` uses the explicit `corc open` form from a root-table tmux binding. On quit corc swaps the viewed pane home and removes the content pane it created.

**Real session**:
The user's normal tmux session for a project (created by `new.sh`, named after the directory — `{repo}/{checkout}` for a secondary **Checkout**, so same-named worktrees of different repos never share one) — where nvim etc. live, as opposed to the hidden session.

**Content pane**:
The pane next to the sidebar where the selected conversation's agent pane is
swapped in (see ADR-0001); holds a placeholder when nothing is selected.

**Browser view**:
The optional pane beside the **Content pane** (`b`) mirroring, live, the page
the agent is driving through Playwright. corc attaches to the browser
Playwright already launched rather than owning it, and streams CDP screencast
frames as kitty graphics (see ADR-0002). It belongs to one **Conversation**: a
persisted per-conversation flag decides whether the pane opens while that
conversation is the one in view. Three ways to set it, all the same flag: `b`
in the sidebar, `Ctrl+b` anywhere in the **corc session**, or `corc browser`
from inside the agent pane.
_Avoid_: preview pane, screenshot pane

**State file**:
corc's persistent record (`~/.local/state/corc/state.json`) of every conversation it has spawned (id, cwd), per-conversation last-viewed times, user-controlled pins, whether the **Browser view** is on, and sticky proof once real content has been observed; what makes dead conversations listable, pinnable at the top, and resumable across tmux/reboots without mistaking temporary provider-metadata loss for an empty conversation.

### Conversation states

**Running** (yellow ●):
A live pane whose agent is working. Claude's animated tmux pane title is the
live runtime signal; an unrecognized/disabled title falls back to an in-flight
turn in the provider history. Shows elapsed time since the turn started in one
largest unit (`4m`, `1h`).

**Question** (blue ●):
A live conversation with an active provider question waiting for the user.
For Claude Code this is an unanswered `AskUserQuestion` tool call in the
transcript. It stays blue even while viewed and shows how long the question
has been waiting.

**Unseen** (blue ●):
A live pane whose turn completed after the user last viewed it. Shows how long the completed turn ran.

**Idle** (gray ●):
A live pane whose agent is at its prompt, or whose turn is complete and viewed
since completion. The conversation in the content pane counts as continuously
viewed — it goes straight to Idle, never Unseen. Shows coarse age (`<1m`, `5h`).

**Dead** (hollow ○):
A conversation with no pane; resumable from the state file via the same
provider that created it. Shows coarse age (`5h`, `3d`).

Within a project: live conversations above dead ones, most recently active first — but rows only re-sort on a state change, never while the user is looking at an unchanged list. Seconds are never shown anywhere.

_Known limitation_: a provider blocked on an interactive permission prompt
can still look mid-turn in its persisted history and therefore show as
**Running**.

### Lifecycle

- An agent exits (or crashes) → corc kills the now-shell-only parked window
  and marks the conversation **Dead** in the state file: still listed,
  hollow, resumable.
- `x` on a live conversation kills its agent and window (`y/n` confirm if
  **Running**); `x` on a **Dead** one removes it from the state file and the
  list. Provider history is never modified or deleted.
- **Dead** conversations outside the selected history window are hidden. The
  `a` control cycles through **active**, **3h**, **1D**, **3D**, **1W**, and
  **all time**; **1W** is the default. **Active** shows every conversation
  backed by a live tmux pane and no dead conversations. Live conversations
  remain visible at every setting.

## Relationships

- The **Hidden session** holds one tmux window per live **Conversation**.
- Pane ↔ conversation mapping is corc bookkeeping. Claude receives a
  corc-minted id, Cursor pre-creates one, and Codex/OpenCode start with a
  provisional id that is migrated once the provider persists its real id.
- A **Project** has at most one **Real session** and any number of **Conversations**.
- A **Conversation** exists for corc only if corc spawned it. Pre-existing
  provider history and manually started agent processes are invisible; the
  narrow pending-id resolution window is only used for a pane corc just
  spawned.
- **Projects** keep a fixed, user-managed order: a new project is appended when its first conversation is spawned, and the user rearranges via **Move mode**. The order never changes on its own.

## Example dialogue

> **Dev:** "The user pressed Enter on a conversation with no live pane — do I
> search for a matching agent process?"
> **Domain expert:** "No. Resume the recorded conversation with its provider
> in a new hidden-session window and record the new pane id."

## Flagged ambiguities

- "session" is overloaded (tmux session vs provider session) — resolved:
  **Conversation** means the provider-owned agent session; "session" alone
  always means a tmux session.
