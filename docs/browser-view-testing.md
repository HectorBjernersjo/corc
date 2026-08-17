# Testing the browser view

The browser view (`b` in the sidebar) mirrors, live, whatever page the agent is
driving through Playwright, in a pane beside it. Design and rejected
alternatives are in [ADR-0002](adr/0002-browser-view.md); this document is only
about verifying it works.

## What is proven

**The whole path has been run against a real browser in ghostty 1.3.1 / tmux
3.6b, image included.** Unicode placeholders (`U=1`) render correctly there, so
the fallback described at the bottom is for other terminals, not for this one.

**wezterm has been proven not to work**, so do not spend time on it: it
implements the kitty image protocol but ignores `U=1`. On 20260815-143815
(Windows, WSL2) a virtual placement is drawn as an ordinary placement at the
cursor, and the placeholder cells are then rendered as literal glyphs — three
rows of image followed by three rows of junk. That holds for the one-shot
`a=T,U=1,c=,r=` corc sends *and* for the two-command `a=t` then `a=p,U=1` the
kitty docs spell out, and with the column diacritic on every cell rather than
only the first of a row. Direct placements draw fine, which is not what corc
uses.

**rio 0.5.25 works on Windows**, and is the only Windows-native terminal found
that does. Verified end to end — rio → ConPTY → WSL → tmux passthrough → corc's
exact bytes — with two conditions:

- `conpty.dll` and `OpenConsole.exe` must sit next to `rio.exe`. rio loads them
  from its own directory the way wezterm does; without them it uses the system
  ConPTY, which answers `\x1b[c` with conhost's `?61;…` instead of rio's
  `?62;4;6;22;52c` and drops the image escapes entirely. This is *not* a matter
  of being up to date: Windows 11 25H2 (build 26200.9106) behaves that way.
  With them in place rio answers `_Gi=99;OK` to a transmit and `_Gi=31;OK` to a
  query.
- Only the one-command placement works. `a=T,q=2,f=100,t=d,i=…,U=1,c=,r=` — what
  `transmit` sends — draws correctly; the `a=t` then `a=p,U=1` spelling from the
  kitty docs draws nothing in rio. Do not "clean up" `transmit` into two
  commands.

Two dead ends met on the way there, worth knowing so they are not re-derived:

- **Older wezterm on WSL draws nothing at all**, for an unrelated reason: the
  ConPTY it bundles (20240203 ships a 2023 build) swallows the APC sequences and
  answers `\x1b[c` with conhost's own `?61;…` instead of wezterm's `?65;4;6;…`.
  Nightly bundles conpty 1.22.250204002 and the protocol works end to end
  through WSL — `a=q` answers `_Gi=31;OK`. So "no image in wezterm" says nothing
  about placeholders until the ConPTY generation is ruled out.
- **`wezterm imgcat` panics** with a divide-by-zero on 20240203 in a pty that
  reports no pixel dimensions, which is every pty behind ConPTY. Fixed in
  nightly. It is not a usable oracle on old builds.

The probe scripts live in `~/test/kitty-graphics-probe/` and are throwaway; the
technique is the part worth keeping — query the terminal instead of reading
pixels, and use XTVERSION as the control that proves the read-back channel
works, so silence on a graphics query means the protocol and not the plumbing.

`cargo test -- --ignored` runs
`browser::tests::a_real_playwright_browser_is_discovered_and_streamed`, which
starts a real Playwright MCP server, makes real tool calls, and drives corc's
own code end to end:

- Chromium does not exist before the first browser tool call (lazy launch).
- corc finds it by walking down from the agent pane's pid, reads
  `--user-data-dir` off its command line and the port out of
  `DevToolsActivePort`.
- corc's hand-rolled WebSocket completes a CDP handshake and pulls a real
  screencast frame (asserted to be a PNG).
- The frame comes back wrapped for tmux passthrough with `U=1,c=…,r=…`.
- A second navigation moves the header's url. This one is worth keeping honest:
  a screencast frame carries only geometry, so the url rides on `Page`'s
  navigation events, and a stale url still looks like a url.

It needs `npx`, Chromium and network, which is why it is `#[ignore]`d. Run it
after touching `browser.rs`, `ws.rs` or `kitty.rs`.

So if the pane misbehaves, discovery and streaming are the least likely
suspects. Diagnose from the bottom of the stack up.

## Environment

| Thing | State |
|---|---|
| `~/.config/corc/playwright.json` | written; adds only `--remote-debugging-port=0` |
| `~/.claude.json` | `--config …/corc/playwright.json` added to the playwright MCP args |
| Backup of the above | `~/.claude.json.corc-backup-20260816-174844` |
| `~/.cargo/bin/corc` | built from this tree with `cargo install --path .` |
| Terminal / tmux | ghostty 1.3.1, tmux 3.6b, `allow-passthrough on` |

`corc doctor` re-checks all of this and should print three green
`browser view:` lines. If any is missing, fix that before testing anything else.

## The test

1. **Close any Chromium left over from an earlier agent.** One live browser
   holds Playwright's shared profile and blocks every other conversation,
   new ones included — see the first failure mode below. Check with:

   ```sh
   pgrep -af 'ms-playwright-mcp' | grep -v 'type=' | head
   ```

2. Restart corc so it runs the new binary:
   `tmux kill-session -t _corc && corc`
3. Start a **new** conversation (`n`). This matters — an agent that was already
   running started its MCP server with the old arguments and will never expose
   a port. To reuse an existing conversation instead, press `x` then `Enter` on
   the dead row, which respawns it with `--resume`.
4. Ask the agent to navigate somewhere with Playwright, and press nothing.

Expected: within a second of the agent's tool call a pane appears to the right
of the agent, without `b`. Its top row is a dim URL, and the rest is the page,
updating as the agent clicks around. The URL should follow the agent within half
a second of each navigation, fragment included.

5. **Closing it means closed.** Press `b` (or `Ctrl+b`) while the browser is
   still open: the pane goes away and *stays* away — if it reappears a second
   later, `auto_open_browser` is level-triggering instead of firing on the
   browser appearing. Ask the agent to close the browser and open a new one:
   the pane comes back on its own.
6. **The view belongs to the conversation.** Switch to another conversation:
   the pane goes away. Switch back: it returns. Press `b` on a conversation
   that is not in view and nothing happens on screen until you open it.
7. **Toggling without the sidebar.** With focus in the agent pane, press
   `Ctrl+b` — the pane closes within a second; press it again and it comes
   back. `!corc browser` typed at the agent does the same thing. Both write a
   request to `~/.local/state/corc/browser-requests`, which the TUI drains on
   its next refresh; if the file is left sitting there, the TUI is not running
   or is not reaching `apply_requests`.

   Check the key changed nothing else. While corc runs, `tmux list-keys -T
   root C-b` shows one conditional binding whose else-branch is `send-keys
   C-b`, so `Ctrl+b` in any other session behaves as it always did; after
   quitting corc the listing is empty again. Your own root bindings must keep
   working *inside* the corc session — if you have vim-tmux-navigator or
   anything else on `bind -n C-h`, press it in the agent pane and in the
   sidebar. (`tmux show -t _corc key-table` must say `root`: pointing the
   session at its own key table replaces the root table instead of layering
   over it, which is exactly how those bindings once went dead.) If `Ctrl+b`
   does nothing at all, `tmux show -gv prefix` is probably `C-b` — tmux
   resolves the prefix before the root table, and `corc doctor` says so.
8. **It survives a restart.** `tmux kill-session -t _corc && corc`, then view
   the conversation again: the pane comes back on its own.

### Driving it without an agent

The pane discovers Chromium by walking down from the conversation's pane pid,
so any browser started from inside that pane will do — useful for testing the
rendering and streaming half on its own, without waiting on an agent or
fighting the profile lock:

```sh
chromium --headless=new --remote-debugging-port=0 \
  --user-data-dir=/tmp/corc-browser-test https://example.com
```

It has to stay a *descendant* of the pane: `setsid`, `disown` or anything else
that reparents it to pid 1 puts it outside the walk. Navigate it over CDP with
the port from `/tmp/corc-browser-test/DevToolsActivePort`.

## Failure modes, in the order worth checking

**The agent's browser tool call fails with `Browser is already in use for
…/mcp-chrome-…, use --isolated`.**
Another Chromium holds the shared user data directory. It does not have to
belong to a *running* conversation — a browser outliving its agent keeps the
lock, and then even a brand-new, correctly configured conversation cannot
launch one. Find the squatter and what it belongs to:

```sh
pgrep -af 'ms-playwright-mcp' | grep -v 'type=' | head -1
ps -o ppid= -p <that pid>     # walk up: chromium → MCP server → agent
```

A leftover with `--remote-debugging-pipe` and no `--remote-debugging-port` also
tells you its agent predates the config edit. Closing it frees the profile.

`--isolated` in the MCP args is the real fix for running two at once, and corc
deliberately does **not** add it for the user: it keeps the profile in memory
and would silently drop their persisted logins. Mention it, do not add it.

**Pane says `no browser — the agent has not opened one`.**
The agent has not made a browser tool call yet, or its MCP server predates the
config edit. Confirm a Chromium with a port exists at all:

```sh
pgrep -af 'remote-debugging-port=0' | grep -v type= | head -1
```

Then check corc can reach it — take the `--user-data-dir` from that line:

```sh
head -1 <user-data-dir>/DevToolsActivePort            # the port
curl -s http://127.0.0.1:<port>/json/list | head -c 300
```

If the port answers but corc does not see it, the process walk is the suspect:
`cdp_port()` starts from `#{pane_pid}` of the conversation's pane, so the
browser must be a descendant of that pane.

**Header shows a URL the agent has already left.**
The header is fed by `Page.frameNavigated` and `Page.navigatedWithinDocument`,
which is why `Page.enable` is called before the cast starts. It is not fed by
the frames: `Page.ScreencastFrameMetadata` carries only `offsetTop`,
`pageScaleFactor`, `deviceWidth`, `deviceHeight`, `scrollOffset*` and
`timestamp` — there is no url in it, in any Chromium version. If a url only
ever changes when the pane reconnects, something has gone back to reading it
from the frame.

**Image freezes.**
Usually not a bug: Chromium only emits a frame when the page repaints. Navigate
or scroll and see whether it updates. If it never recovers, the cast died — the
pane reconnects on its own within a second, so watch the header.

**Image spills over the sidebar or the agent pane.**
Should be impossible with placeholders, since tmux clips them. If it happens,
something is emitting a direct placement.

**Header is right but the area below is blank, or fills with small dotted
marks.** The terminal is not drawing the image — either it ignores `U=1`, or it
renders the placeholder cells as literal text. Ghostty and kitty both handle
them; on anything else, confirm with kitty's own implementation:

```sh
kitten icat --unicode-placeholder <some.png>   # placeholders — what corc uses
kitten icat <some.png>                         # direct placement — the fallback
```

If the first draws nothing and the second draws the image, that terminal needs
direct placement: in `kitty.rs`, drop `U=1` from the `transmit` control string,
stop calling `paint_grid`, and position the cursor at the pane origin before
each frame. The cost is real and is why placeholders were chosen — a direct
placement lands at the terminal's *physical* cursor, so the image only lands
correctly while the browser pane is the active one, and it is not clipped to
the pane. Note that limitation in the ADR if you make the switch.

## Ground rules

- `~/.claude.json` is the user's file. It has been edited once, with a backup.
  Do not rewrite it further without asking.
- corc must keep working when none of this is set up: `b` reports what is
  missing and changes nothing else — the flag is cleared again rather than
  left on to fail on every refresh. Do not let a browser-view failure become a
  corc failure.
- `.playwright-mcp/` is gitignored — test runs drop snapshot files there.

## Where the code is

| File | Role |
|---|---|
| `src/browser.rs` | process-tree discovery, CDP, the pane's event loop, config, `corc browser` and its request mailbox |
| `src/ws.rs` | minimal RFC 6455 client (no dependencies) |
| `src/kitty.rs` | graphics protocol, tmux passthrough, placeholder grid |
| `src/base64.rs` | encoder, for the WebSocket handshake key only |
| `src/tmux.rs` | `split_browser_pane`, `pane_pid`, `pane_info`, `passthrough_enabled`, `install_browser_binding` |
| `src/ui.rs` | the `b` key, `toggle_selected_browser`, `auto_open_browser`, `browser_appeared`, `sync_browser_pane` |
| `src/doctor.rs` | `check_browser_view` |

corc never decodes an image: CDP hands over base64 PNG, Chromium does the
scaling, and the kitty protocol accepts that encoding as-is. If you find
yourself reaching for an image crate, something has gone wrong.
