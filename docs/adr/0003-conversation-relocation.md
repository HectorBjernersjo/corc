# Relocation follows what each provider can move

A conversation can move to another directory — the agent prepares a new
workspace and asks to live there (`corc cd <dir>`), or the user types `/cd`
into the agent themselves. Either way corc's involvement has two halves that
depend on the provider:

- **Cursor bookkeeping**: the Cursor agent has already chosen the new
  directory when it calls `corc cd`. Cursor has no persistent `/cd` and does
  not report a changed conversation directory, so the TUI only re-homes the
  row in corc's state. The move is final. Nothing is typed back into the pane
  and no confirmation is expected from Cursor.
- **Claude delivery**: `corc cd <dir>` appends to a mailbox (same single-writer
  handover as the browser-view mailbox), and the TUI types the provider's
  `/cd <dir>` into the conversation's pane with `send-keys -l`. Strictly the
  calling pane's conversation, no
  viewed-conversation fallback: relocating the wrong conversation is a real
  move, not a toggled view. Input typed into a busy agent queues and executes
  when the turn ends (documented Claude Code behavior), so delivery never
  waits for idle.
- **Claude bookkeeping**: the agent has the last word on `Conversation.cwd`. Every
  Claude hook payload carries the session's own working directory (ADR-0004),
  which `discovery` folds into `Meta.cwd` so the next refresh re-homes the
  row. A `/cd` the user typed by hand is followed that way, and a
  `Conversation.cwd` that somehow drifted wrong self-repairs, because the
  reported cwd keeps naming the real home whatever state says.
  `/cd` itself fires no hook, though, so the log keeps naming the old directory
  until the next prompt — a conversation relocated and then left alone sat in
  its old project for as long as nobody typed into it. Claude writes a
  `relocated` record into the transcript as it moves the file, and that record
  is read alongside the titles: it counts on the same terms as a hook's cwd,
  only when the file actually sits in the directory it names.
  For `corc cd` the row moves the moment `/cd` is typed, before the agent has
  done anything. The queued `/cd` runs only when the turn ends, and an agent
  that asks to move early in a twenty-minute turn otherwise sits in the old
  project for all of it: correct, and useless to the person looking for it.
  The move is provisional (`relocation_requested_at`) until the agent's report
  can be about it: a report naming the target confirms it whenever it arrives,
  and a turn that started after the request means the `/cd` has had its
  chance, so a report still naming the old directory then (declined trust
  prompt, typo) puts the row back. Reports dated earlier — including the
  `Stop` of the very turn that made the request, which fires before the queued
  `/cd` runs — say nothing. The worst case is a refused `/cd` showing the row
  wrong for one turn.
  (Two earlier versions read this off the transcript instead. Records stamp the
  *shell* cwd, which follows every Bash `cd` the agent makes, so the first
  version scattered conversations into subdirectories their agents had `cd`:d
  into, and the second had to confirm each record cwd against the mangled name
  of the directory its file sat in. The hook payload just says it.)

Verified against Claude Code v2.1.235: `/cd` (v2.1.169+) relocates the
session's transcript into the new directory's project storage and loads its
CLAUDE.md; `--resume` finds a moved session from any directory (v2.1.223+);
`/cd` is user-only inside the agent — the model cannot invoke it, which is
why corc does the typing. The transcript-file move itself needs nothing from
corc: `discovery::Store` already forgets a vanished path and re-locates by
uuid scan, and re-parsing from the top is what turns up the `relocated`
record.

## Considered options

- **Prompting Cursor with the new directory**: rejected — Cursor itself calls
  `corc cd`, so it already knows what directory it chose. Sending the same
  instruction back would spend a turn and add no information.
- **Restarting Cursor with `--workspace`**: rejected — corc only needs to
  group the conversation under the directory where the agent says it is
  working. Restarting would discard the live process and prompt cache to make
  Cursor's own workspace metadata agree with bookkeeping that already works.
- **Transcript surgery** (kill pane, move the jsonl, respawn with
  `--resume`): rejected — depends on undocumented storage layout, loses the
  live process and its prompt cache, and `/cd` does all of it supported.
- **Waiting for the agent before moving the row**: this ADR's first version,
  rejected in practice — the typed `/cd` can be declined by the trust prompt
  or fail, which argued for never moving early, but the row then spent whole
  turns in the wrong project. Moving early and letting the agent's next
  report settle it keeps the same evidence with a bounded wrong-for-one-turn
  instead of an unbounded right-but-stale.
- **The agent typing into its own pane** (`tmux send-keys` from a tool
  call): rejected — self-steering send-keys is exactly what permission
  classifiers flag, and it spreads the hack across every agent instead of
  keeping it in the tool that already owns panes.
