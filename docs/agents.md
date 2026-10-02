# Agent CLIs

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

## How conversations are tracked

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

## Resuming inside an agent

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
