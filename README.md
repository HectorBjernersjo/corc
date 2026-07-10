# corc

A tmux-native hub for Claude Code: one TUI that owns, monitors, and switches
between all your Claude Code conversations, so you never have to interact with
Claude Code outside of it.

corc keeps every Claude pane in a single hidden tmux session and shows a
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
  - Switch which one new conversations use with `s` (see below).
- **git** (optional) — only used to detect git worktrees for the project
  labels and the directory picker.

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

**Not strictly.** You can launch corc from any terminal with:

```sh
corc open
```

That creates the visible `_corc` session, starts the TUI in it, and attaches
your terminal (or switches your client if you're already in tmux). Running the
bare `corc` command only works from inside tmux, because the TUI takes over the
current pane — so `corc open` is the entry point you actually want.

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
`~/.config/corc/directories.txt` (one directory path per line), merges it with
machine-local directories stored in corc's state, and expands each git repo's
worktrees. For compatibility, the old `~/.config/tmux/directories.txt` path is
used when the corc-specific file does not exist. The `p` key adds a directory
to the machine-local list in `~/.local/state/corc/state.json` (with `Tab`
completion, prefilled from `$HOME`) and immediately starts a conversation
there. Use `directories.txt` for a hand-curated list you want to sync between
machines, and `p` for local additions.

## Usage

Launch with `corc open` (or `Ctrl+q` if you bound it). Inside the TUI:

| Key | Action |
|---|---|
| `j`/`k`, arrows, `g`/`G` | move selection |
| `Enter` / click | view the conversation (resumes it if dead) |
| `n` | new conversation in the selected conversation's directory |
| `N` | directory picker → new conversation in a listed directory |
| `p` | add a machine-local directory (Tab-complete) → new conversation there |
| `s` | switch which agent (Claude / Cursor) new conversations use |
| `x` | kill a live conversation / remove a dead one (confirms if running) |
| `V`, then `K`/`J` | move mode: reorder projects |
| `Alt+1`–`Alt+9` | jump to window N of the project's normal tmux session |
| `a` | also show dead conversations older than a week |
| `/` | filter the list |

Each conversation remembers which agent spawned it, so `Enter` resumes a dead
one with the same CLI. The `s` picker only changes the agent used for
conversations you start afterwards; it's persisted, so the choice survives
restarts. Cursor conversations show their chat title in the sidebar once the
first message has been sent (read from Cursor's local chat store); before that,
and if the title can't be read, they show `(untitled)`. Cursor's local message
store also drives the same Running, Unseen, Idle, and Dead states as Claude.

There is no quit key — corc is meant to live in its own tmux session. To stop
it, kill that session yourself (e.g. `tmux kill-session -t _corc`). `Ctrl+C`
still exits if you need a hard escape hatch.

### Other commands

- `corc` — the TUI (must be run inside tmux; normally launched via `corc open`).
- `corc open` — create/enter the corc session (bind this to a key).
- `corc list` — print every conversation corc owns, grouped by project.
- `corc doctor` — check tmux compatibility, agent binaries, `PATH`, and state
  file permissions.

## How it works

corc keeps all Claude panes in a hidden tmux session (`_corc-sessions`), one
window per conversation. The TUI lives in its own visible session (`_corc`) and
swaps the selected conversation's pane into a content pane next to the sidebar —
nothing is destroyed when you switch between conversations. State (which
conversations exist, their directories, last-viewed times) is persisted to
`~/.local/state/corc/state.json`, which is what keeps conversations listable and
resumable across restarts.
