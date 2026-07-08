# corc

A tmux-native hub for Claude Code: one TUI that owns, monitors, and switches
between all your Claude Code conversations, so you never have to interact with
Claude Code outside of it.

corc keeps every Claude pane in a single hidden tmux session and shows a
sidebar of conversations grouped by project. Selecting one swaps its live pane
into view; conversations you've spawned stay listed and resumable even after
they exit or you reboot.

## Requirements

- **tmux** — corc is built on tmux and must run inside it.
- **Claude Code CLI** (`claude`) on your `PATH` — corc spawns conversations
  with `claude --session-id <uuid>` / `claude --resume <uuid>`.
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
`~/.config/tmux/directories.txt` (one directory path per line) and expands each
git repo's worktrees. Create that file to populate the picker; without it, the
picker just has nothing to list. The `p` key adds any directory on disk to that
file (with `Tab` completion, prefilled from `$HOME`) and immediately starts a
conversation there — so you rarely need to edit the file by hand.

## Usage

Launch with `corc open` (or `Ctrl+q` if you bound it). Inside the TUI:

| Key | Action |
|---|---|
| `j`/`k`, arrows, `g`/`G` | move selection |
| `Enter` / click | view the conversation (resumes it if dead) |
| `n` | new conversation in the selected conversation's directory |
| `N` | directory picker → new conversation in a listed directory |
| `p` | add a directory to `directories.txt` (Tab-complete) → new conversation there |
| `x` | kill a live conversation / remove a dead one (confirms if running) |
| `V`, then `K`/`J` | move mode: reorder projects |
| `1`–`9` | jump to window N of the project's normal tmux session |
| `a` | also show dead conversations older than a week |
| `/` | filter the list |

There is no quit key — corc is meant to live in its own tmux session. To stop
it, kill that session yourself (e.g. `tmux kill-session -t _corc`). `Ctrl+C`
still exits if you need a hard escape hatch.

### Other commands

- `corc` — the TUI (must be run inside tmux; normally launched via `corc open`).
- `corc open` — create/enter the corc session (bind this to a key).
- `corc list` — print every conversation corc owns, grouped by project.

## How it works

corc keeps all Claude panes in a hidden tmux session (`_corc-sessions`), one
window per conversation. The TUI lives in its own visible session (`_corc`) and
swaps the selected conversation's pane into a content pane next to the sidebar —
nothing is destroyed when you switch between conversations. State (which
conversations exist, their directories, last-viewed times) is persisted to
`~/.local/state/corc/state.json`, which is what keeps conversations listable and
resumable across restarts.
