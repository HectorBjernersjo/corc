# corc

A tmux-native hub for agent CLIs: one TUI that owns, monitors, and switches
between your Claude Code, Codex, Cursor CLI, and OpenCode conversations.

corc keeps every agent pane in a single hidden tmux session and shows a
sidebar of conversations grouped by project. Selecting one swaps its live pane
into view; conversations you've spawned stay listed and resumable even after
they exit or you reboot.

## Requirements

- **tmux 3.3+** — corc is built on tmux, uses popup flags added in 3.3, and
  must run inside it.
- **At least one agent CLI** on your `PATH`:
  - **Claude Code** (`claude`) — corc spawns `claude --session-id <uuid>` /
    `claude --resume <uuid>`.
  - **Cursor CLI** (`cursor-agent`, optional) — corc mints a chat with
    `cursor-agent create-chat` and attaches with `cursor-agent --resume <id>`.
  - **Codex CLI** (`codex`, optional) — corc spawns plain `codex` and resumes
    with `codex resume <uuid>`. Codex only reveals its session id once the
    first message is sent, so a brand-new conversation shows as untitled
    until then.
  - **OpenCode** (`opencode`, optional) — corc spawns plain `opencode` and
    resumes with `opencode --session <id>`. OpenCode creates its session when
    the first prompt is sent, after which corc adopts its `ses_...` id.
  - Switch which one new conversations use with `s` (see below).
- **git** / **jj** (both optional) — only used to expand a repo into its other
  checkouts (git worktrees, jj workspaces) for the directory picker. Project
  labels and session names detect a checkout straight from the filesystem, so
  they work without either binary.

## Install

### One-line installer (Linux / macOS)

```sh
curl -fsSL https://github.com/HectorBjernersjo/corc/releases/latest/download/install.sh | sh
```

This downloads the right prebuilt binary for your platform into
`$HOME/.local/bin` (override with `CORC_INSTALL_DIR`). Pin a version with
`CORC_VERSION=v0.1.0`. Make sure the install dir is on your `PATH` — the
installer warns you if it isn't.

corc is a tmux tool, so only Linux and macOS (x86_64 and aarch64) are built.

### From source

```sh
git clone https://github.com/HectorBjernersjo/corc
cd corc
cargo install --path .
```

## tmux setup — do you need to change anything?

**Not strictly.** Launch corc from any terminal with:

```sh
corc
```

That creates the visible `_corc` session, starts the TUI in it, and attaches
your terminal (or switches your client if you're already in tmux). `corc open`
is the explicit form of the same command and doubles as a toggle when invoked
from the `_corc` session.

**Recommended:** bind it to a key so you can jump to corc from anywhere. Add
this to your `~/.tmux.conf` (or `~/.config/tmux/tmux.conf`):

```tmux
bind -n C-q run-shell "corc open"
```

Now `Ctrl+q` from any session brings you into corc, creating and starting it on
first use. Reload with `tmux source-file ~/.tmux.conf`.

> If `run-shell` can't find `corc` (its `PATH` may not include
> `~/.local/bin` or `~/.cargo/bin`), use the absolute path:
> `bind -n C-q run-shell "/home/you/.local/bin/corc open"`.

### Optional: the directory picker

The `N` key opens a picker to start a new conversation in a directory. It reads
`~/.config/corc/directories.txt` (one directory path per line, `~` expanded,
`#` comments ignored), merges it with machine-local directories stored in corc's
state, and expands each repo into its other checkouts — git worktrees and jj
workspaces alike. A secondary checkout is labelled `{repo}/{checkout}`, which is
also the name of the tmux session it gets.

A line ending in `/*` is a **scan root**: every checkout up to three levels
below it is listed on its own, most recently modified first, without being
named in the file. Containers are looked through, checkouts are never descended
into, and dotted directories and symlinks are skipped:

```
~/projects/*
~/work/*
~/some/one-off-repo
```

That covers new worktrees and workspaces the moment they exist, and costs no
`git`/`jj` process at all — a checkout is recognised straight from disk.

The picker's “+ add directory…” row switches to path completion and can save
a new machine-local directory in `~/.local/state/corc/state.json`. Use
`directories.txt` for a hand-curated list you want to sync between machines.

## Usage

Launch with `corc` (or `Ctrl+q` if you bound it). Inside the TUI:

| Key | Action |
|---|---|
| `j`/`k`, arrows, `g`/`G` | move selection |
| `Ctrl+j`/`Ctrl+k` | next / previous panel |
| `}`/`{`, `Ctrl+d`/`Ctrl+u` | next / previous project |
| `Enter` / click | view the conversation (resumes it if dead) |
| `n` | new conversation in the selected conversation's directory |
| `N` | directory picker → new conversation in a listed directory |
| `p` | pin / unpin the selected conversation at the top |
| `s` | switch which agent new conversations use |
| `x` | kill a live conversation / remove a dead one (confirms if running) |
| `b` | browser view on/off for the selected conversation |
| `Ctrl+b` | the same toggle, from anywhere inside corc (agent pane included) |
| `V`, then `K`/`J` | move mode: reorder projects |
| `Alt+1`–`Alt+9` | jump to window N of the project's normal tmux session |
| `a` | cycle visible history: `active` / `3h` / `1D` / `3D` / `1W` / `all time` |
| `/` | filter the list |

The `active` history view shows every conversation with a live tmux pane and
no dead conversations. The age windows add recent dead conversations; live
ones always remain visible. History starts at `1W` each time corc launches.

Pinned conversations (pink-purple while idle or dead), running conversations
(yellow), active questions (blue), and unseen conversations (blue) are collected in an
unlabelled panel at the top, with pinned rows first. Pins remain there across restarts and regardless of the
history window. `j`/`k` cross into adjacent panels at their boundaries, while
`Ctrl+j`/`Ctrl+k` jump directly between panels. Selecting a top-panel row opens
it and moves the selection to its normal project-grouped row in the sidebar.

On Linux this also works with stock `vim-tmux-navigator` configuration. corc
identifies its sidebar process as `corc/view`, consumes `Ctrl+j`/`Ctrl+k` while
there is another internal panel in that direction, and hands `Ctrl+h/j/k/l`
back to tmux at an outer edge.

Each conversation remembers which agent spawned it, so `Enter` resumes a dead
one with the same CLI. The `s` picker only changes the agent used for
conversations you start afterwards; it's persisted, so the choice survives
restarts. Provider metadata is read from each CLI's local, read-only history:
Claude and Codex JSONL transcripts, Cursor's chat stores, and OpenCode's SQLite
database. This drives titles and the same Running, Unseen, Idle, and Dead
states across providers, plus a blue Question state where the provider exposes
structured interactive questions. An untouched Codex or OpenCode conversation stays
`(untitled)` until the CLI creates its real session on the first prompt.

### Browser view

`b` opens a pane beside the agent showing, live, whatever page it is driving
through Playwright. corc attaches to the browser Playwright already launched —
it never starts one — so an agent that has not opened a browser simply says so.

The view belongs to the conversation, not to the layout: `b` turns it on for
the conversation under the cursor, the setting is remembered across restarts,
and the pane appears whenever you view that conversation and disappears when
you leave it. Closing the pane yourself turns the setting off.

You do not have to go to the sidebar to toggle it: **`Ctrl+b` works anywhere
inside corc**, the agent pane included. corc binds the key at runtime, scoped
to its own session — in every other session the key passes straight through as
before — and unbinds it again on exit, so your tmux config is never touched.
tmux resolves the prefix before the root table, so if your prefix *is* `C-b`
the key stays your prefix and the toggle is simply unavailable; `corc doctor`
tells you.

The same toggle is a command, which is what the key runs:

```
corc browser          # toggle; !corc browser types it at Claude Code
corc browser on|off   # the explicit forms
```

Run inside an agent pane it applies to that conversation, anywhere else to the
one you are viewing.

It needs three things, all checked by `corc doctor`:

1. A terminal that draws kitty graphics: ghostty, kitty, or wezterm.
2. `set -g allow-passthrough on` in your tmux config.
3. Playwright launching Chromium with a debugging port. `corc doctor` writes
   `~/.config/corc/playwright.json` for you; add it to the Playwright MCP
   server's arguments and restart the agent:

   ```
   --config ~/.config/corc/playwright.json
   ```

   The file only adds `--remote-debugging-port=0`, letting the kernel pick a
   free port that corc then finds on its own. Nothing else about your
   Playwright setup changes.

Note that Playwright refuses to open a second browser against a profile already
in use, so only one conversation at a time can have one. The lock is held by
whichever Chromium is still alive, so a browser left behind by a conversation
you have moved on from will block the next one too — close it rather than
starting over. To run two at once, pass `--isolated` as well, which keeps the
profile in memory and so drops any logins you rely on persisting. That tradeoff
is yours to make; corc does not make it for you.

There is no quit key — corc is meant to live in its own tmux session. To stop
it, kill that session yourself (e.g. `tmux kill-session -t _corc`). `Ctrl+C`
still exits if you need a hard escape hatch.

### Other commands

- `corc` — create or enter the corc session.
- `corc open` — the explicit form of `corc` (bind this to a key).
- `corc list` — print every conversation corc owns, grouped by project.
- `corc browser [on|off]` — toggle the browser view for the conversation the
  command runs in; meant for `!corc browser` from inside the agent.
- `corc doctor` — check tmux compatibility, agent binaries, `PATH`, state file
  permissions, and the browser view's prerequisites.
- `corc --help` — show command-line help.

## How it works

corc keeps all agent panes in a hidden tmux session (`_corc-sessions`), one
window per conversation. The TUI lives in its own visible session (`_corc`) and
swaps the selected conversation's pane into a content pane next to the sidebar —
nothing is destroyed when you switch between conversations. State (which
conversations exist, their directories, last-viewed times) is persisted to
`~/.local/state/corc/state.json`, which is what keeps conversations listable and
resumable across restarts.
