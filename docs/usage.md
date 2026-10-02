# Using corc

## Install details

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

## Keys

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

There is no quit key — corc is meant to live in its own tmux session. To stop
it, kill that session yourself (e.g. `tmux kill-session -t _corc`). `Ctrl+C`
still exits if you need a hard escape hatch.

## Other commands

- `corc` — create or enter the corc session.
- `corc open` — the explicit form of `corc` (bind this to a key).
- `corc cd DIR` — move the conversation the command runs in to DIR.
- `corc open DIR` — go to DIR's project session, creating it when missing.
  From a terminal it attaches; inside tmux it switches your client; run by a
  program with no terminal (a GUI keybinding) it moves the tmux client you
  last typed in.
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
