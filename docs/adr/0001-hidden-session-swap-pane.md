# Claude panes live in one hidden session and are viewed via swap-pane

orcim owns every Claude Code process it shows. All Claude panes are parked in a
single hidden tmux session (`_orcim`, one window per conversation, spawned with
`claude --session-id <uuid>` so pane ↔ conversation mapping is exact
bookkeeping). Viewing a conversation swaps its pane with the content pane next
to the sidebar using `swap-pane` — never `join-pane`.

## Considered Options

- **Separate tmux server** (`tmux -L orcim`) would hide the sessions perfectly,
  but panes cannot be moved between servers, which kills the embed mechanism.
  Hence: same server, hidden by the `_orcim` naming convention and a filter in
  `new.sh`.
- **`join-pane` + restore** (the original design): moving the only pane out of
  a window destroys the window, and sometimes the session, so unembedding
  needed fragile home-tracking and window/session recreation.
- **Nested attach** (`TMUX= tmux attach` in the content pane): prefix keys,
  status lines and resizing all misbehave in nested tmux.

## Consequences

- `swap-pane` never destroys anything — both windows always keep one pane — so
  there is no unembed logic and the sidebar layout is set once and never
  disturbed.
- If orcim crashes mid-view, the Claude pane survives in orcim's old window;
  on startup orcim reconciles by swapping stray Claude panes back into
  `_orcim`.
- `new.sh` must be patched to filter out the `_orcim` session.
