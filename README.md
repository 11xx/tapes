# tapes

Read, list, and export coding-agent sessions across harnesses.

A session is a recording. `tapes` finds it, normalizes it, and hands it back
as something you can query — whether it lives in a JSONL file on disk or
behind an HTTP API.

```
tapes                           # the workflow guide
tapes show --latest --tail 40   # this project's newest session, no id needed
tapes list                      # every harness, newest first
tapes list --harness opencode --here
tapes show ses_07e16cc8 --tail 20
tapes show ses_07e16cc8 --json
tapes export ses_07e16cc8 --bundle /tmp/
```

## Orientation

`tapes` with no arguments prints the workflow guide: what the tool owns, the
order the commands are meant to be used in, when a dead session is worth
rescuing at all, how to ingest a bundle progressively, how to read why a
session ended, and what the export contains that must not leave `/tmp`. It is
the whole briefing an agent needs before its first command, so no separate
document has to be loaded first; `tapes --help` remains the per-command
reference.

## Why this exists

An agent session dies mid-task — quota, a crash, a context wall — and the
work survives only in a transcript nobody can query. Recovering it meant
hand-exporting hundreds of kilobytes to `/tmp` and grepping. `tapes` makes
that a command.

## Supported harnesses

| harness | store | retrieval |
| :-- | :-- | :-- |
| claude | `~/.claude/projects/<slug>/<session>.jsonl` | file discovery |
| codex | `$CODEX_HOME/sessions/<y>/<m>/<d>/rollout-*.jsonl` | file discovery |
| opencode | SQLite behind an HTTP API | `opencode2 api --standalone` |
| pi | `~/.pi/agent/sessions`, append-only tree | file discovery |

A harness whose binary or store is absent reports itself unavailable; it
never fails a listing. Listing works with any subset installed.

## Output

`list` merges sessions from every available harness and sorts them by last
activity. `--limit` bounds each harness and defaults to 20, `--harness` selects
one backend, and `--here` restricts the listing to the project holding the
current directory. The scope applies before the bound, so a scoped listing
cannot be emptied by a limit spent on other projects. Human output ends with an
availability note when a backend cannot be read, and says so when a search
stopped early. File-backed harnesses may show a bounded first-meaningful-user-
turn hint prefixed with `~` when no recorded title exists; JSON keeps that hint
in `derived_title` and leaves the recorded `title` absent. OpenCode's
API-backed listing does not fetch messages to invent titles, so a title-less
OpenCode row keeps both title fields absent in list, show, and export metadata.
JSON output is a `tapes-list/1` object containing `sessions`, `unavailable`,
`scanned`, and `scan_truncated`. When the optional `harness-status` command
supplies a usable snapshot, matching sessions also carry `live: "working"` or
`live: "idle"`; an unavailable, malformed, oversized, or slow authority leaves
the field out. Unknown states are ignored per thread so recognized entries
remain usable. Human list output keeps the exact session id in its first
column and uses a separate `LIVE` column for that state.

`show` accepts a full session ID or an unambiguous prefix. It searches every
available backend, rejects ambiguous prefixes with the matching candidates,
and prints normalized turns in chronological order. Human timestamps are RFC
3339 whole seconds with `Z`; JSON preserves the recorded timestamp precision.
`--tail` bounds the turns returned and defaults to the last 100; a transcript
that dropped any is marked `truncated`. JSON output uses the
`tapes-session/1` transcript schema, with the optional `live` annotation when
the authority answers. The human header marks the same state.

`show` and `export` also take `--latest` in place of an ID, which resolves the
most recent session in scope. `--exclude <id>` is repeatable and passes over
sessions the caller already holds — including its own, which is otherwise the
newest one there. `--project <path>` scopes elsewhere and `--global` drops the
scope.

`export` writes a three-file bundle sharing one timestamped prefix, into
`--bundle <dir>` or `/tmp`:

- `.context.md` — exact operator turns and assistant-visible text. Read first.
- `.json` — the canonical `tapes-session/1` object plus turns, cost, tokens,
  and the session directory's git head and branch when they resolve. Query
  selectively with `jq`.
- `.trace.md` — every reasoning and tool turn the transcript carries, in
  order, for grepping. A tool turn is headed by the tool's name where its
  envelope carries one, by `result` for a bare result, and by `unnamed`
  otherwise, with the harness's raw envelope kept beneath it.

Export bundles contain the recording only; volatile `live` state is never
written to rescue files.
Markdown titles use the same `~` marker for a derived first-meaningful-user-
turn hint where the backend can derive one, and Markdown timestamps use RFC
3339 whole seconds with `Z`. The JSON member retains the full timestamp
precision.

Read context first, query the JSON narrowly, and reach for the trace only
when free-text search is genuinely easier. Never ingest a whole bundle
because it exists.

Stdout is a manifest of exactly those three paths and their sizes; nothing
else goes there. Each file is written under a temporary name and renamed, so
a bundle never looks complete while it is half written.

## Integrating

`tapes` is meant to be the one reader of transcript stores on a machine. If you
are writing a tool, a skill, or an agent instruction that needs a session, plug
into it here rather than reaching into a store yourself.

**Discovery costs no inference.** The binary is on `PATH`. There is no config
file, no daemon, and no environment to prepare — `tapes show --latest` works
from any directory in a project and needs no session ID at all, so an agent
never has to list sessions, read the table, and choose one. The project is the
repository holding the current directory, identified by its common Git
directory: every linked worktree of it counts, a nested repository does not,
and a directory outside a repository scopes to its own subtree. Harness stores
are located by the backends, so a caller never needs a path. An agent that has
never used the tool runs `tapes` bare and gets the same briefing this section
describes, which is why an instruction file can point at the command instead
of restating it.

**Exclude the session doing the asking.** An agent running `--latest` inside a
live session is usually the newest session in its own project. Nothing in a
store separates it from the session that just died, so a wrapper that knows its
own ID should pass `--exclude <id>`; `tapes` reports the latest and never
guesses which one the caller meant.

**The cheap probe first.** `tapes show --latest --tail 40` answers "is there
anything here worth having?" without exporting. Reach for `export` only after
that says yes; a bundle costs context, and the tail usually settles it.

**Contracts you can build on.** `tapes-list/1` and `tapes-session/1` are
versioned JSON; a breaking shape change bumps the version. `export` prints
exactly three paths and their sizes on stdout, in reading order, and writes each
file under a temporary name before renaming — so a bundle is never observed
half-written. A harness whose binary or store is absent reports itself
unavailable and never fails a listing, which means a caller can run against any
subset of harnesses without branching on what is installed; `show` and `export`
still fail when the session they were given cannot be resolved.

**Signals worth reading rather than ignoring.** A `truncated` transcript is a
window, not the whole session. Transcript `notes` carry what the normalized
model has no field for — abandoned branches, subagent transcripts, skipped
unparseable lines, page boundaries. A listing's `scan_truncated` says the
search stopped at its ceiling rather than exhausting the store, and `scanned`
says how far it got. Each one means a caller's picture is partial, and a tool
that reports a conclusion without checking them will state more than it knows.
One case has no signal at all: a session recorded in a directory that has since
been deleted belongs to no resolvable project, so a scoped search will not
return it, though its ID still resolves.

**Do not write a second parser.** Four separate extractors preceded this tool
and drifted apart; that drift is the reason it exists. If `tapes` cannot express
something you need, add it here — a caller that hand-parses a store is a fifth
extractor with the same future.

## Redaction

`tapes` prints what a transcript contains and performs no redaction.
Transcripts hold whatever was pasted into them. Treat exported bundles as
sensitive.

## Name

"handoff" was taken twice — an AirPods audio daemon on AUR and a Rust
agent-context tool — and it names the wrong half of the job. This tool's
primary direction is *pull*: retrieve and reconstruct a session that already
ended. Handing context forward is one thing you might do with what it
returns.
