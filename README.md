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
| opencode | Stable SQLite or beta API store | `opencode db --format tsv` and `opencode2 api` |
| pi | `~/.pi/agent/sessions`, append-only tree | file discovery |

A harness whose binary or store is absent reports itself unavailable; it
never fails a listing. Listing works with any subset installed.

## Output

`list` merges sessions from every available harness and sorts them by last
activity. `--limit` bounds each harness and defaults to 20, `--harness` selects
one backend, and `--here` restricts the listing to the project holding the
current directory. `--model <substring>` matches case-insensitively against the
full model identity shown in the `MODEL` column (`id (variant)` when a variant
exists); a session without a model never matches. `--directory <substring>`
matches case-insensitively against the recorded directory path; a session
without a directory never matches. Both filters are applied by the library
before the per-harness bound, and compose with the harness and scope filters.
`--search <substring>` matches case-insensitively against the last 32 normalized
turns in each candidate session. The fixed tail keeps the read bounded, so a
non-match says only that those recent turns did not contain the text. Search is
applied before the per-harness bound. If a candidate's bounded read fails, it
is named in `unsearched` rather than silently treated as a non-match.
If a database prefilter fails, its diagnostic is also recorded in `unsearched`
while the safe unfiltered confirmation fallback runs; an unsupported prefilter
is not an error.
The database-backed OpenCode v1 path uses a read-only SQL prefilter that
returns candidate ids before transferring bounded transcript projections; raw
message bodies are not transferred during that prefilter or confirmation
read. OpenCode2 has no content-search endpoint: its session-list `search`
filter is title-only and its message endpoint is per-session. When the v2
database covers the API listing, `tapes` uses a read-only `sqlite3` prefilter
over its materialized messages; otherwise it uses one short-lived local API
server for enumeration and the GET confirmation reads. A no-match OpenCode2
search must still inspect every candidate that survives metadata filters when
no v2 database prefilter is available; server reuse removes per-session
process startup, not that necessary scan. When stable and v2 both expose an
id, search uses the stable projection just as resolution does; a later
projection cannot resurrect its non-match.
The command writes a short preflight notice to stderr before scanning; JSON
results remain on stdout.
The scope applies before the bound, so a scoped listing cannot be emptied by a
limit spent on other projects. Human output ends with an availability note when
a backend cannot be read, a separate line for each stored session row that
cannot be read, a separate line for each candidate a content search could not
read, and says so when a search stopped early. File-backed harnesses
may show a bounded first-meaningful-user-turn hint prefixed with `~` when no
recorded title exists; JSON keeps that hint in `derived_title`, leaves the
recorded `title` absent, and adds `derived_title_truncated: true` when the hint
was shortened (`false` means the present hint is complete). OpenCode's
API-backed listing does not fetch messages merely to invent titles, so a
title-less OpenCode row keeps title metadata absent in
list, show, and export. File-backed readers open a session's first 64 KiB as
well as its last 4 MiB, so `started_at`, the id, and the directory come from
the header even when the transcript is larger than the tail (Claude and Codex
also take the first-turn hint from there; pi withholds it, since its active
path is unknowable past the bound). A session whose header could not be read
carries `start_uncertain: true`, and `started_at` is then a floor. A Codex row without `model` may have its
model-bearing `turn_context` before the bounded 4 MiB file-tail read; the
absence is preserved rather than filled with a guess. JSON output is a
`tapes-list/1` object containing `sessions`, `unavailable`, `unreadable`,
`unsearched`, `scanned`, and `scan_truncated`. `unavailable` names harnesses
that could not be read at all;
`unreadable` names sessions a readable harness could not normalize, each with
its id and diagnostic. They stay apart because a corrupt row says nothing about
the store holding it. When the optional `harness-status` command
supplies a usable snapshot, matching sessions also carry `live: "working"` or
`live: "idle"`; an unavailable, malformed, oversized, or slow authority leaves
the field out. Unknown states are ignored per thread so recognized entries
remain usable. Human list output keeps the exact session id in its first
column and uses a separate `LIVE` column for that state.

`show` accepts a full session ID or an unambiguous prefix. It searches every
available backend, rejects ambiguous prefixes with the matching candidates,
and prints normalized turns in chronological order. Every turn carries its
zero-based `ordinal` in the session's normalized sequence, kept under any
window, and a `native_id` where the harness records one; the session carries
`store`, the opaque coordinate it was read from. Human timestamps are RFC
3339 whole seconds with `Z`; JSON preserves the recorded timestamp precision.
`--tail` bounds the turns returned and defaults to the last 100; a transcript
that dropped any is marked `truncated`, and JSON says why under `truncation`:
a `window` names the turns returned and the earlier turns the bound omitted,
which a larger `--tail` or `export` recovers, while `source` lists bounds the
reader itself reached (a file tail, a store page, cut turn text), which no
request through `tapes` passes. Human output closes with one note per cause
and recommends only the recovery that works. When a backend verifies a final
non-turn record, human output names its kind and timestamp and JSON carries an
optional `trailing_record` object; unavailable source timestamps remain absent.
JSON output uses the `tapes-session/1` transcript schema, with the optional
`live` annotation when the authority answers. The human header marks the same
state.

`show` and `export` also take `--latest` in place of an ID, which resolves the
most recent session in scope. `--exclude <id>` is repeatable and passes over
sessions the caller already holds — including its own, which is otherwise the
newest one there. `--project <path>` scopes elsewhere and `--global` drops the
scope.

`export` writes a three-file bundle sharing one timestamped prefix, into
`--bundle <dir>` or `/tmp`:

- `.context.md` — exact operator turns and assistant-visible text. Read first.
- `.json` — the canonical `tapes-session/1` object plus turns, cost, tokens,
  any verified `trailing_record`, and the session directory's git head and
  branch when they resolve. Query selectively with `jq`.
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
