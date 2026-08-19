# Relocation types `/cd` into the pane; state follows the transcript

A conversation can move to another directory — the agent prepares a new
workspace and asks to live there (`corc cd <dir>`), or the user types `/cd`
into the agent themselves. Either way corc's involvement has two halves that
are deliberately decoupled:

- **Delivery**: `corc cd <dir>` appends to a mailbox (same single-writer
  handover as the browser-view mailbox), and the TUI types the provider's
  relocation command — Claude Code's `/cd <dir>` — into the conversation's
  pane with `send-keys -l`. Strictly the calling pane's conversation, no
  viewed-conversation fallback: relocating the wrong conversation is a real
  move, not a toggled view. Input typed into a busy agent queues and executes
  when the turn ends (documented Claude Code behavior), so delivery never
  waits for idle.
- **Bookkeeping**: `Conversation.cwd` is never updated optimistically. Every
  transcript record stamps the *shell* cwd it was written under — which
  follows every Bash `cd` the agent makes into subdirectories, so a record
  cwd on its own says where the shell stood, not where the session lives.
  What `/cd` uniquely moves is the transcript file itself, so a record cwd
  counts only when the file's location vouches for it: its mangled form must
  name the directory the file sits in. `discovery` folds the latest
  *confirmed* cwd into `Meta.cwd` and the refresh re-homes the row when it
  differs. A `/cd` the user typed by hand is followed too; a `/cd` that
  never ran (declined trust prompt, typo) changes nothing anywhere; and a
  `Conversation.cwd` that somehow drifted wrong self-repairs, because the
  confirmed cwd keeps naming the real home whatever state says.
  (The first version trusted record cwds unconditionally and scattered
  conversations into the subdirectories their agents had `cd`:d into.)

Verified against Claude Code v2.1.235: `/cd` (v2.1.169+) relocates the
session's transcript into the new directory's project storage and loads its
CLAUDE.md; `--resume` finds a moved session from any directory (v2.1.223+);
`/cd` is user-only inside the agent — the model cannot invoke it, which is
why corc does the typing. The transcript-file move itself needs nothing from
corc: `discovery::Store` already forgets a vanished path and re-locates by
uuid scan, then re-parses and reports the new cwd.

## Considered options

- **Transcript surgery** (kill pane, move the jsonl, respawn with
  `--resume`): rejected — depends on undocumented storage layout, loses the
  live process and its prompt cache, and `/cd` does all of it supported.
- **Optimistic cwd update when the request is sent**: rejected — the typed
  `/cd` can be declined by the trust prompt or fail, leaving state pointing
  at a directory the session never moved to. Following the transcript makes
  the manual-`/cd` case work for free.
- **The agent typing into its own pane** (`tmux send-keys` from a tool
  call): rejected — self-steering send-keys is exactly what permission
  classifiers flag, and it spreads the hack across every agent instead of
  keeping it in the tool that already owns panes.
