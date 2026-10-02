# Browser view

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
