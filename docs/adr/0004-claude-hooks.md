# Claude reports through hooks, not through its own screen

corc used to decide what a Claude conversation was doing by looking at it:
match the Braille spinner on the tmux pane title, capture the pane and match
the status line (`✽ Baking… (3m 18s`), match the AskUserQuestion dialog footer
(`Enter to select · … · Esc to cancel`). All three are strings in a terminal UI
with no compatibility promise, and one had already broken: the `✳` title glyph
stopped meaning idle in 2.1.2xx, which made every working conversation read as
idle until the check was removed.

Claude Code runs a command of the operator's choosing at fixed points in its
own loop. corc installs one per pane with `--settings` on the spawn line, so
the user's `~/.claude` is never written to and their own hooks keep running
alongside (settings merge). `corc __hook` is that command: one payload on stdin,
one line appended to `~/.local/state/corc/hooks/<session-id>.jsonl`.

| what corc needs | where it comes from now |
|---|---|
| turn started | `UserPromptSubmit` |
| turn still moving | `PostToolUse` |
| turn finished | `Stop` |
| question opened / answered | `PreToolUse` / `PostToolUse` on `AskUserQuestion` |
| working directory | the `cwd` on a payload, when `transcript_path` vouches for it |
| title stand-in | the `prompt` on `UserPromptSubmit` |

The log is append-only and every writer opens it `O_APPEND`, which is what
makes Claude's parallel tool calls safe to record without a lock. corc folds it
with the same incremental reader it already had for transcripts
(`discovery::Store`), so the whole feed cost one locate function and one apply
function.

Two things hooks do not report, and one that follows from that.

**Titles, and the one move hooks miss.** A generated `ai-title` and a
`/rename` exist nowhere but the transcript, so `Store::titles` still reads it —
for those three record types, plus the `relocated` record a `/cd` writes, and
nothing else. `/cd` fires no hook, so without that record a relocated
conversation stayed in its old project until the user's next prompt
(ADR-0003). This is the only remaining read of Claude's own files.

**Esc, and panes that predate the hooks.** Interrupting a turn fires nothing:
no `Stop`, no `Notification`, and nothing in the transcript until the next
prompt. Verified against 2.1.251 by driving a real pane, interrupting it and
watching the log stay silent for 200 seconds. And a pane that was already
running when corc installed its hooks reports nothing at all, forever — asking
someone to restart every working agent to get their sidebar back is not a
migration, it is a bill.

Claude keeps a file per running session under `~/.claude/sessions` saying what
it is doing, so corc reads that for both. The rule is self-limiting rather than
special-cased: a status only counts when it is newer than the conversation's
own last logged progress. A hooked conversation mid-turn logs a tool call every
few seconds, so the registry never gets a word in; a conversation with no log is
described by it entirely.

The status has at least three values. `busy` puts a turn in flight and dates it
from the moment it changed, which is exactly when the turn started. `idle`
closes one. `waiting` is Claude holding a question or a permission prompt in
front of the user and names no state on its own, so it changes nothing — the
hook log says which of the two it is when there is one. An open question is
never cleared here whatever the status says.

The first version of this read `!= "busy"` as at rest, which turned a live
question dialog grey. Caught by pointing corc at a pane that had one open.
`waiting` is also the signal that would give a permission prompt a state of its
own, which corc still shows as Running (PLAN.md D6).

The directory is undocumented, so every failure to read it is treated as no
information: a turn then ages out through the ordinary stalled-turn timeout,
which is what corc did before this ADR anyway.

**Conversations corc did not spawn.** A pane the user started themselves with
`claude --resume` carries no hooks. It still has a live status and a
transcript, so it reads correctly apart from turn durations; corc does not
adopt foreign history anyway (PLAN.md D1).

## What this replaced

`Provider::runtime_hint`, `Provider::content_hint`, `provider::pane_hint`,
`status::RuntimeHint` and `tmux::capture_pane` are all gone, along with the
transcript parsing for turn state, turn timing, questions, interrupts and the
working directory. No other provider had ever implemented a pane hint, so the
whole path went with Claude's.

`Meta.cwd` did not get simpler, and the first version of this ADR claimed it
had. A payload's `cwd` is the *shell's*, exactly like a transcript record's, and
it follows every `cd` the agent makes: sessions reported
`.../gbandit/main/platform/apps/auth-service/src` as their home and the sidebar
scattered them into directories nobody had moved them to. ADR-0003's rule still
stands, only its evidence is cheaper. Every payload carries `transcript_path`,
and the directory that file sits in is the mangled session cwd, so the hook
process confirms the two against each other and records no directory at all
while the agent is off wandering. No searching, and the check happens once at
write time instead of on every parse.

The repair half of ADR-0003 lives in `home_of`: whatever pushed a wrong
directory into state.json, walking up its ancestors until one mangles to the
name Claude files the transcript under converges the row back on its real
home.

## Considered options

**Keep reading the pane.** It works until Claude redraws something. The
question footer in particular is one sentence of UI copy standing between the
user and a conversation silently waiting for them.

**Write the hooks into `~/.claude/settings.json`.** Simpler to install, but
corc would own a file the user edits, with no way to tell their entries from
its own, and the hooks would fire for every Claude they run rather than the
panes corc spawned. `--settings` is per-invocation and merges, so it costs two
argv entries and nothing else.

**Hooks for everything, titles included.** The hook process could tail the
transcript for `ai-title` on every `Stop` and write the title into the log.
That moves the read rather than removing it, buys an offset file to keep the
tail cheap, and puts filesystem work in the user's agent loop. Reading the
transcript for three record types from corc's own process is less machinery.

**Drop the interrupt case.** Without a corrective an interrupted turn sits on
Running for the full hour of the stalled-turn timeout. For a hub whose whole
job is telling you which conversation needs you, an hour of a confident wrong
answer is worse than a documented read of an undocumented file.
