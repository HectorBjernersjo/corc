# The browser view attaches to Playwright's browser instead of owning it

corc can show, live, whatever page the agent is driving through Playwright, in
a pane beside the agent — opened by the agent reaching for a browser, or by `b`.
It does this by attaching to the Chromium
Playwright already launched — discovered by walking down from the agent pane's
pid — and streaming CDP screencast frames into the pane with the kitty
graphics protocol. corc never launches, wraps or proxies anything.

Three constraints shaped this, all verified against a real setup rather than
assumed:

- Playwright MCP starts Chromium **lazily**, on the agent's first browser tool
  call. An idle conversation costs one Node process, not a browser.
- Playwright talks to Chromium over `--remote-debugging-pipe` (fd 3/4), so
  there is no endpoint another process can join.
- Chromium started with `--remote-debugging-port=0` picks a free port and
  writes it to `DevToolsActivePort` in its user data directory — which is on
  its command line.

So the whole integration is one flag in the user's Playwright MCP config
(`--config ~/.config/corc/playwright.json`, which `corc doctor` writes and
tells them to wire up). Everything else corc discovers.

## Considered Options

- **A corc MCP server wrapping Playwright**, so the agent explicitly asks for
  a browser. Rejected: it exists to solve a problem that isn't there — the
  browser is already lazy — while putting corc in the request path for every
  browser action, where a corc bug breaks the agent's browser. Re-exposing
  ~25 tool schemas is also a maintenance treadmill against
  `@playwright/mcp@latest`. A transparent JSON-RPC proxy avoids the schema
  duplication but then buys only "know when the browser woke up", which the
  process walk gives for free.
- **corc allocates a port per conversation** and writes a per-conversation
  config. Rejected once `--remote-debugging-port=0` proved to work: the kernel
  assigns, so there is nothing to allocate, no collisions to avoid, and one
  static config file serves every conversation.
- **`corc browser` writing the flag straight into `state.json`.** Rejected:
  the TUI owns that file and rewrites it wholesale from its in-memory copy, so
  a second process editing a conversation's flag there is overwritten on the
  next save — and a save lands whenever a turn starts or ends, which is exactly
  when the command gets run. The `directories` union merge does not generalise
  to a boolean, where "memory says false, disk says true" cannot be told from
  "the user just turned it off". The CLI therefore appends a request to a
  mailbox file next to the state, and the TUI applies and clears it on its next
  refresh: the flag still lives with `pinned` in `state.json`, and the TUI is
  still its only writer.
- **Decoding frames in corc.** Rejected: CDP emits PNG frames already
  base64-encoded, Chromium does the scaling, and the kitty protocol accepts
  exactly that encoding. corc forwards the string untouched and needs no image
  library.
- **Direct kitty placement** (what `kitten icat` does by default). Rejected:
  a direct placement lands at the terminal's *physical* cursor, which tmux only
  parks inside a pane while that pane is active — a browser view the user is
  not focused on would paint over whatever pane is. It is also unclipped, so
  an oversized frame bleeds across pane borders. Unicode placeholders (`U=1`)
  express the image as ordinary text cells, which tmux lays out, clips and
  repaints like any other text. Ghostty 1.3.1 draws them correctly — the one
  thing about this design that could only be settled by looking.

## Consequences

- The browser view is **read-only and best-effort**. If the user never wires
  up the config, `b` explains what to add and does nothing else; corc works
  exactly as before.
- Frames only arrive when the page repaints, because that is when Chromium
  emits them. A static page costs nothing, and the pane keeps showing the last
  frame — the placeholder cells are tmux's text, so they survive redraws.
- The view is **per conversation**: `browser` is a persisted flag on the
  conversation, like `pinned`, and the pane is derived from it — it exists
  while the conversation carrying it is the one in view. Switching
  conversations therefore opens or closes the pane rather than repointing it,
  and the choice survives a corc restart. Closing the pane by hand clears the
  flag, as does a failure to open it, so a broken setup is reported once
  instead of retried every tick.
- The pane process itself still resolves "which conversation is in view" on its
  own, every tick, rather than being told at spawn. That keeps `corc __browser`
  independent of the sidebar's bookkeeping, and it is what makes a switch
  between two conversations that both want the view cost nothing.
- The toggle is reachable from inside the agent pane as `corc browser [on|off]`
  — in Claude Code, `!corc browser` — resolving which conversation it belongs
  to from `TMUX_PANE`, which the agent's own tool calls inherit.
- `Ctrl+b` runs that command from anywhere inside corc, bound **in the root
  table with an `if-shell -F` session condition**, the same shape
  `install_jump_bindings` uses: inside `_corc` the key toggles the view, and
  everywhere else the else-branch is `send-keys C-b`, so the key does what it
  did before. `restore_bindings` unbinds it on quit. The cost is that a user
  whose prefix is `C-b` never reaches it — the prefix wins, harmlessly — so
  `corc doctor` reports that rather than letting the key look broken, and that
  a pre-existing root `C-b` binding is overwritten until the config is
  reloaded.

  This first shipped as a **key table the corc session opted into**
  (`set-option -t _corc key-table corc`), on the belief that tmux resolves keys
  as prefix → the client's key table → root → the application. It does not:
  `key-table` sets the default table *instead of* root, so inside the corc
  session every `bind-key -n` went dead — the user's own root bindings
  (vim-tmux-navigator's `C-h` and friends) and corc's own `M-1`..`M-9` digit
  jump alike. Unbound keys still reached the agent, which is why the breakage
  read as "my tmux plugin stopped working" rather than as corc's doing.
  `install_browser_binding` unsets the option on startup so a corc session that
  outlives the upgrade recovers without being killed.
- Finding the browser is a full `/proc` walk, so in the pane it runs only while
  disconnected. Once the cast is up, the connection is the liveness signal.
  The TUI walks too, once per refresh, for the reason below.
- **The view opens itself when the agent opens a browser**, which is what the
  toggle was standing in for. `cdp_port` was already the "a browser exists now"
  signal; the only thing missing was someone asking while no pane was open, so
  `auto_open_browser` asks on the TUI's one-second refresh and sets the same
  flag `b` does — the pane stays derived from the flag, and nothing else moves.
  Three things this settles:
  - **Only the viewed conversation is checked.** The pane exists for that one
    alone, so a flag set on a background conversation buys nothing visible, and
    switching to a conversation whose browser is already up fires this on the
    next tick anyway. That also keeps the walk at one per second, root pid
    included, instead of one per live conversation.
  - **Edge-triggered, not level-triggered**: the flag is set when a browser
    *appears*, tracked in an in-memory `browser_seen` set. Level-triggering it
    would reopen a view the user had closed by hand a second earlier, every
    second, for as long as the browser lived — the toggle would look broken. The
    set is only updated for the viewed conversation, so switching away and back
    does not re-fire, and it is not persisted: a corc restart already re-derives
    the pane from the flag.
  - **A setup that cannot draw the view is skipped silently.** `sync_browser_pane`
    reports its failure and clears the flag, which is right for a keypress and
    wrong here — a wezterm user would get the same message once per browser the
    agent opens. Auto-open therefore checks passthrough itself and does nothing
    if it is off. Finding a CDP port already proves the Playwright config is
    wired, so there is nothing else to pre-check.
- **Only one conversation at a time can have a browser** unless Playwright runs
  with `--isolated`: it refuses to open a second browser against a user data
  directory already in use. The lock belongs to whichever Chromium is alive, so
  a browser left running by an old conversation blocks every other one,
  including brand-new conversations — the fix is to close it, not to start
  another. This is a pre-existing Playwright limitation, not one corc
  introduces, and corc deliberately does not add `--isolated` to the shipped
  config — that would silently stop the user's logins from persisting.
- Requires a terminal that draws kitty graphics (ghostty, kitty, rio) and
  `allow-passthrough` in tmux. `corc doctor` checks both. rio 0.5.25 is the
  Windows-native one, verified end to end through WSL and tmux — but only with
  `conpty.dll` and `OpenConsole.exe` beside `rio.exe`, since the system ConPTY
  swallows the APC sequences (Windows 11 25H2 included) and answers `\x1b[c`
  itself. **wezterm does not qualify**, despite implementing the kitty image
  protocol: it ignores `U=1`.
  Probed on 20260815-143815, a virtual placement is drawn as an ordinary
  placement at the cursor and the placeholder cells are then rendered as literal
  glyphs, whether asked for as one `a=T,U=1` command or as `a=t` followed by
  `a=p,U=1`. Direct placements draw correctly there — which is the thing this
  ADR rejected. It is called out because it is a terminal a user is likely to be
  sitting in, expecting this to work.
- Identifying the terminal takes two questions, not one: `#{client_termname}` is
  a plain `xterm-256color` under both wezterm and konsole, so corc also reads
  the XTVERSION answer tmux collected (`#{client_termtype}`, e.g.
  `WezTerm 20240203-…`) and matches either. That is what lets the "cannot draw
  images" message name the terminal the user recognises instead of
  `xterm-256color` — advice they cannot act on.
  `CORC_BROWSER_GRAPHICS=1` in the tmux *session* environment forces the view
  on for a terminal neither answer names; the browser pane inherits that
  environment, not the shell's.
