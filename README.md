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
  - **OpenCode** (`opencode`, optional) uses `opencode --standalone` and
    resumes with `opencode --standalone --session <id>`. OpenCode creates its
    session when the first prompt is sent, after which corc adopts its `ses_...` id.
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
restarts. Claude Code reports what it is doing through hooks: corc passes
`--settings` on the spawn line, so Claude runs `corc __hook` when a turn starts,
when a tool finishes, when it asks you something and when it stops. Nothing is
written to your `~/.claude` and your own hooks keep running. The other CLIs are
read from their local, read-only history instead: Codex's JSONL rollouts,
Cursor's chat stores, and OpenCode's SQLite database. This drives titles and
the same Running, Unseen, Idle, and Dead states across providers, plus a blue
Question state where the provider exposes structured interactive questions. An untouched Codex or OpenCode conversation stays
`(untitled)` until the CLI creates its real session on the first prompt.

### Resuming inside an agent

Running `/resume` in a corc-managed Claude Code or OpenCode pane updates corc
to the selected conversation, including its title and project directory.
Conversations started outside corc are added when you resume them here. The
previous conversation stays in history and can be reopened with Enter.

If the selected conversation already has a live corc pane, the pane where you
ran `/resume` takes over and corc closes the older pane. Pins and browser-view
preferences stay with the conversation.

Claude reports the switch through its session-start hook. OpenCode gets a
small CLI plugin through its pane environment; corc keeps the generated plugin
in its own state directory. Restart existing agent panes after upgrading corc
to enable this reporting.

### Browser view

`b` opens a pane beside the agent showing, live, whatever page it is driving
through Playwright. corc attaches to the browser Playwright already launched —
it never starts one — so an agent that has not opened a browser simply says so.

Mostly you do not press anything: **the view opens itself within a second of the
agent opening a browser**, for the conversation you are viewing. Closing it
still means closed — corc opens the view when a browser *appears*, not for as
long as one is there — so the next browser the agent opens brings it back, and
the one it already has does not.

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

1. A terminal that draws kitty graphics *with unicode placeholders*: ghostty,
   kitty or rio. Not wezterm — it draws kitty images but ignores `U=1`, leaving
   the placeholder cells on screen as literal glyphs.

   On Windows, rio is the one that works, with one extra step: copy `conpty.dll`
   and `OpenConsole.exe` next to `rio.exe`. Without them rio uses the system
   ConPTY, which swallows the image escapes on the way out of WSL and leaves the
   pane blank — verified on Windows 11 25H2 (26200), so being up to date is not
   enough. Both files ship with Windows Terminal and with wezterm.
2. `set -g allow-passthrough on` in your tmux config.
3. Playwright launching Chromium with a debugging port. For Claude Code corc
   arranges this itself: every pane it spawns gets `--mcp-config` pointing at a
   Playwright MCP server corc generates, and that server loads
   `~/.config/corc/playwright.json`. Claude merges corc's server over your own,
   so a `playwright` server you had wired up in `~/.claude.json` is replaced in
   corc's panes rather than run beside it. You can delete it.

   The config file is written once and is then yours. It starts out headless
   with `--remote-debugging-port=0`, letting the kernel pick a free port that
   corc then finds on its own. It is also where an `executablePath` or a
   `channel` goes under `launchOptions` if Playwright should not use the Chrome
   it finds by itself.

   OpenCode also gets the Playwright server automatically, through a config
   overlay in its pane environment. corc runs it with `--standalone` so its
   private server and browser stay under the pane's process tree. This uses
   Playwright tools, not OpenCode's desktop-only browser tools. Restart existing
   OpenCode panes to pick up the integration.

   Codex and Cursor still need a Playwright server of their own, with
   `--config ~/.config/corc/playwright.json` in its arguments.

Every conversation gets its own browser, and its own profile to go with it, in
`~/.cache/corc/browsers/<conversation>`. That is not cosmetic: Chromium locks a
profile while it lives, and Playwright's own choice of profile is keyed by
working directory — so without this, two conversations in one repo would fight
over one browser and the second to open would simply fail. corc sets
`PLAYWRIGHT_MCP_USER_DATA_DIR` on the agent's pane, which every process below it
inherits.

Logins therefore persist per conversation, resumes included, and the profiles of
conversations you have removed are deleted the next time corc starts.
When a provisional session id becomes a real id, corc keeps the original profile
directory and records its name for subsequent resumes.

There is no quit key — corc is meant to live in its own tmux session. To stop
it, kill that session yourself (e.g. `tmux kill-session -t _corc`). `Ctrl+C`
still exits if you need a hard escape hatch.

### Other commands

- `corc` — create or enter the corc session.
- `corc open` — the explicit form of `corc` (bind this to a key).
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
