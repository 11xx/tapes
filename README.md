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
tapes events ses_07e16cc8 --name wait_agent --json
tapes usage ses_07e16cc8 --json
tapes stats ses_07e16cc8 --json

tapes brief ses_07e16cc8
tapes endings --here --since 2026-01-01 --json
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
`--since <TIMESTAMP>` and `--until <TIMESTAMP>` filter the newest recorded
activity, `last_activity_at`, with the half-open rule
`since <= last_activity_at < until`; each accepts an RFC 3339 timestamp with an offset or a bare
`YYYY-MM-DD` date interpreted as midnight UTC. The activity window is applied
before the per-harness bound. `--sort newest|oldest` orders by
`last_activity_at` and decides which sessions the bound keeps: `newest` keeps
the newest matching sessions of each harness, `oldest` the oldest, which
inspects every candidate the scan reaches, as `--search` does. Equal timestamps
are ordered by session id, then harness.
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
If the local v2 API server cannot start or list candidates, the search falls
back to `opencode2 api --standalone get` listing and per-session reads, and
records the failed stage and diagnostic in `unsearched`.
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
JSON always includes `sort`; it includes `activity` only when `--since` or
`--until` is present.

`show` accepts a full session ID or an unambiguous prefix. It searches every
available backend, rejects ambiguous prefixes with the matching candidates,
and prints normalized turns in chronological order. Every turn carries a
`kind` saying what the harness recorded it as — `operator`, `assistant`,
`reasoning`, `tool`, `control`, `ambient`, `notice`, or `unknown` — filled
only from fields the harness itself wrote, so a `/exit` command or an injected
notice is not read as an unanswered prompt and a record with no such field
stays `unknown` (see `docs/model.md`). Human output heads a user turn holding
anything but an operator's message `user/<kind>`. Every turn in `show` also carries its
zero-based `ordinal` in the session's normalized sequence, which a file-backed
session keeps under any window while a paged OpenCode API read renumbers when
a wider window fetches further back (see `docs/model.md`), and a `native_id`
where the harness records one; the session carries `store`, the opaque
coordinate it was read from. Human timestamps are RFC
3339 whole seconds with `Z`; JSON preserves the recorded timestamp precision.
`--tail` bounds the turns returned and defaults to the last 100; a transcript
that dropped any is marked `truncated`, and JSON says why under `truncation`:
a `window` names the turns returned and the earlier turns the bound omitted,
which a larger `--tail` or `export` recovers, while `source` lists bounds the
reader itself reached (a file tail, a store page, cut turn text). Wider
`show`/`export` turn windows retain those bounds; explicit `page` reads can
reach older Claude and Codex file history. Human output closes with one note per cause
and recommends only the recovery that works. When a backend verifies a final
non-turn record, human output names its kind and timestamp and JSON carries an
optional `trailing_record` object; unavailable source timestamps remain absent.
The activity note compares the store's last activity and the newest rendered
turn at whole-second precision, matching the timestamps shown to the reader.
JSON output uses the `tapes-session/1` transcript schema, with the optional
`live` annotation when the authority answers. The human header marks the same
state.

`show` and `export` also take `--latest` in place of an ID, which resolves the
most recent session in scope. `--exclude <id>` is repeatable and passes over
sessions the caller already holds — including its own, which is otherwise the
newest one there. `--project <path>` scopes elsewhere and `--global` drops the
scope.

`events` projects tool calls and results into the harness-neutral
`tapes-events/1` schema. Each record keeps the turn ordinal and native id,
bounded argument or output metadata, and an exact call/result pair when both
halves occur in the bounded read. Unpaired calls report `no-result-in-read`;
an unpaired result reports `call-before-read-bound` when a file-tail or
record-page bound can hide its call, and `call-not-recorded` when the read
reached the recording's start. `--tail` uses the same turn-ordinal window as
`show`, while `--name` and `--call-id` apply after pairing. With no `--tail`,
the command returns every event the bounded reader reaches so counts describe
the read rather than an implicit display window.

`usage` answers where one session's quota went as `tapes-usage/1`: the
session's recorded `tokens`, `cost`, and `accounting`, and `turns` counted by
role. The accounting `basis` and `coverage` decide whether figures may be
summed — a recorded total and a sum of per-request records are both safe to
add, while coverage says how much of each session a figure covers, and `turns`
carries the same coverage for the read behind it. Cost is only what the harness
recorded; a provider quota is a separate fact about the account. Facts beyond
the normalized counters appear only where a harness records them: Codex adds
`context_window` and `rate_limits`, Claude adds `durations_ms` and `by_model`
when the recording holds a `cost-state`, and pi and OpenCode add neither.
Human output prints one line per recorded fact and closes with the same
truncation notes `show` prints.

Given a scope or a listing filter instead of a session, `usage` answers the
whole selection as `tapes-usage-summary/1`: the same flags `list` and `export`
take, grouped by `--by harness,model,variant,directory` and defaulting to
harness and model. It sums the counters the listing already carries, so no
transcript is read. Each sum covers the sessions that recorded that counter
and `counted` says how many those were — a counter nine of twelve sessions
recorded is not a figure about twelve — while `coverage` counts the sessions
behind a sum by their accounting, including those whose harness recorded
nothing to sum. Cost is summed only where a harness recorded one, never
inferred from tokens. Human output is one row per group, a totals row, and
the listing's own diagnostics.

`endings` answers what each session of a selection ends on as
`tapes-endings/1`, so deciding which endings deserve reading costs one bounded
read each rather than a transcript apiece. It takes the same selection flags
`list` and `export` do, applied before any transcript is opened, plus `--tail`
for how many of each session's newest turns are read (12 by default) and
`--text` for the operator and assistant text of those turns, cut at 400
characters. `facts` names what the read establishes from the normalized turn
kinds and typed tool events alone — an unanswered request, a harness command
or notice recorded last, a call the read never saw a result for, results no
turn narrates, a closing assistant turn — and `incomplete` names what it left
unestablished, which qualifies each of them. `lineage` counts the relatives a
store records without reading any of them, and `source` is the text-free
coordinate to write down when filing a follow-up. Nothing is classified: the
report infers no reason for an ending and labels no session complete. A
session whose read fails is named in `unread` with its diagnostic and does not
stop the run. Human output is one line per session, the text tail indented
beneath it when asked for, and the listing's own diagnostics.

`stats` counts what one recording holds as `tapes-stats/1`: `turns` by the
`kind` the harness recorded them as, `tools` — calls, results, complete pairs,
unpaired events by the boundary that left them unpaired, errors, and a
`by_name` row per tool with its paired durations — the recorded clock in
`durations_ms`, the session's own counters with the share of
`input + cache_read + cache_write` each cache counter accounts for, and the
children its store names. It reads the same typed events
`events` returns and the same reference read `lineage` answers with, so no
transcript is parsed twice and no child is opened. Every total says what it
covers: `coverage.turns` is `read-window` when a source bound withheld turns,
a duration comes only from a pair the read holds both halves of, a cache ratio
divides recorded token counts rather than cost and is present only when every
counter in its denominator is (whether a harness's `input` already includes
its cache reads is that harness's convention, so the ratio compares recordings
of one harness), and `warnings` names the limits of the read behind the
figures. Every figure is a count of records the harness wrote; nothing is
judged, ranked, or explained. Human output prints one line per group and no
line for a group the recording holds nothing for.

`brief` answers what a continuation of one session needs from its recording as
`tapes-brief/1`, for the case where resuming the session itself has gone too
expensive: where it stopped, the directory it worked in and the commit that
directory sits on, the tool calls the read never saw a result for, the children
whose outcome its store does not record, and the last `--tail` operator and
assistant turns (12 by default) cut at 600 characters. It reads the recording
alone — no journal, no project tool — so it is one half of a continuation and
the project's own record of the work is the other; the caller joins them.
`working_set.directory_exists` states whether the recorded directory is still
there, and uncommitted state is left to the caller, being present-tense rather
than recorded. A call or child it lists is a handle, not a verdict: the
recording says only that nothing answered it. Human output is the same content
in one screen, in that reading order.

`export` writes a three-file bundle sharing one timestamped prefix, into
`--bundle <dir>` or `/tmp`:

- `.context.md` — exact operator turns and assistant-visible text, without the
  harness's own commands, notices, and attached context. Read first.
- `.json` — the canonical `tapes-session/1` object plus turns, cost, tokens,
  their `accounting` basis and coverage when present, any verified
  `trailing_record`, and the session directory's git head and branch when
  they resolve. Query selectively with `jq`.
- `.trace.md` — every turn the transcript carries, reasoning and tool included,
  in order and headed as `show` heads it, for grepping. A tool turn is headed
  by the tool's name where its
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

Given the listing flags in place of an id — `--here`, `--project <path>`,
`--global`, `--harness`, `--model`, `--directory`, `--since`, `--until`,
`--sort`, `--limit`, `--search` — `export` takes the set `list` would return,
in the same order, and writes one bundle per session. Bundles are never
joined: each session keeps its own bounded three files, and a
`tapes-export-manifest/1` `manifest.json` beside them is the only file that
spans the set. It records the selection, each session's bundle paths, the
sessions whose store could not be read under `failed`, and the listing's own
`unavailable`, `unreadable`, `unsearched`, `scanned`, and `scan_truncated`
diagnostics. Stdout adds each bundle's three lines in selection order, then
the manifest's own path and size. A session whose store vanishes between the
listing and the read costs its own bundle and nothing else; the command fails
only when every selected session failed, or when the listing itself did.

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
`tapes events --latest --json` answers tool-count, pairing, duration, and
incompleteness questions without parsing raw tool envelopes from turn text,
and `tapes usage --latest --json` answers token, cost, and turn-count
questions without summing a transcript by hand. `tapes stats --latest --json`
answers the retrospective counting questions — calls by tool and outcome, time
spent in tools, incomplete calls, turns by kind, cache shares, recorded
children — in one bounded pass, so none of them needs custom `jq` over an
export. `tapes lineage --latest
--json` answers which sessions a recording names as relatives — the one that
spawned it, and the children its store records with their roles and outcomes
— from the records themselves rather than from directory or timestamp
proximity. `tapes brief <id> --json` answers what picking one session's work
back up needs from its recording, so a continuation costs one bounded read
rather than a resent history. `tapes usage --here --since <date> --json` answers the same questions across a project's sessions at once, and `tapes endings --here --since
<date> --json` says what each of those sessions ends on, so a scan reads the
few endings that matter instead of every tail.

**Contracts you can build on.** `tapes-list/1`, `tapes-session/1`,
`tapes-events/1`, `tapes-usage/1`, `tapes-usage-summary/1`, `tapes-lineage/1`,
`tapes-endings/1`, `tapes-child/1`, `tapes-stats/1`, `tapes-stats-summary/1`, `tapes-brief/1`,
`tapes-page/1`, `tapes-history-search/1`, `tapes-metadata-history/1`,
and `tapes-export-manifest/1` are versioned
JSON; a breaking shape change bumps the version. A single-session `export`
prints
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

## Historical pages

`tapes page SESSION --bytes 65536 --json` reads backward from the end of a
Claude or Codex recording. Pass its `next_cursor` to `--cursor` to continue.
Turns are chronological within a page; ordinals are page-local. The source
byte range plus session ID and store identify the page. Byte budgets range
from 1 KiB to 4 MiB; the default is 64 KiB. Session metadata and the bounded
opening-header probe are read separately. A page also probes at most one
alignment byte, counted separately as `alignment_bytes`. Codex also reads up
to 64 KiB of newer context to corroborate user-message provenance across page
boundaries, counted as `context_bytes`; those records are not returned as turns.
A kind remains unknown when its corroborating evidence is outside these bounds. Ordinary show/export retain their
source bounds.

A cursor binds the recording's identity, size, modification time and change time. Changed
or replaced recordings refuse continuation; restart without a cursor. Malformed
records and skipped fragments of records larger than a page are counted.
Those gaps prevent a complete-history claim even when no cursor remains.
Unsupported harnesses report unsupported paging.

`tapes history-search SESSION --search TEXT --pages 8 --json` searches
normalized text in at most eight pages. `--bytes` sets the page budget and
`--cursor` resumes older history. Search is case-insensitive; output is capped
at 100 matching records and 600 characters per excerpt, with explicit output
truncation. Remaining history is not a proven miss.

`tapes metadata SESSION --pages 8 --bytes 1048576 --json` recovers recorded
model observations outside the ordinary source tail. It reports observations
in reverse record order, page coverage and gaps, and a continuation cursor.
An older observation is not asserted to be the current model, and ordinary
session metadata is not overwritten. At most 100 observations are returned.
History search and metadata accept 1–32 pages per call.
## Tool usage across sessions

`tapes stats --here --since 2026-01-01 --json` returns
`tapes-stats-summary/1`. Listing filters select the sessions; each costs one
bounded transcript read through its listed backend origin. The report includes
selected/read/failed counts, per-session coverage and tool statistics, and
aggregates by harness and tool name. Pair durations cover complete timestamped
pairs only. Read failures have unknown activity and contribute no counters.
These are recorded harness tool names, such as `bash`, not inferred shell
commands. Child recordings contribute only when independently selected.

## Recorded-title lookup

Single-session commands accept `--title "Exact recorded title"` instead of an
ID or `--latest`: `show`, `brief`, `usage`, `stats`, `lineage`, `events`, and
`export`. Matching is case-sensitive and uses the recorded title only, never
a derived display hint. Lookup defaults to the current project; `--project`,
`--global`, and `--harness` choose its scope.

Lookup parses at most 5,000 candidates per backend, independently of the
ordinary listing limit. Claude discovery also stops after 10,000 directory
entries; Codex and Pi require no title scan. Multiple matches refuse with candidate IDs and
origins. Incomplete scans, unreadable records, and incomplete title evidence
refuse even when one candidate was observed. Choose an explicit ID or narrow
the scope in that case. Missing harnesses are skipped. Codex and Pi recordings
currently supply derived display hints rather than recorded titles.

## Claude child recordings

`tapes child PARENT --reference CHILD --json` reads the child's own transcript,
usage and ending evidence. Find `CHILD` in the parent's `lineage` output. The
`tapes-child/1` report retains parent identity and child reference; its transcript
uses the qualified ID `PARENT::CHILD`. This ID does not become an ordinary
listed session: use the child command to read it. Missing references refuse.

`--tail` bounds rendered transcript turns (default 40). Usage and ending facts
cover the child's own bounded source read, with source coverage retained.
Nested child lineage is explicitly uninspected; no child activity is absorbed
into its parent. Other harnesses' ordinary child sessions remain addressable
by their recorded session IDs.
## Tool usage across sessions

`tapes stats --here --since 2026-01-01 --json` returns
`tapes-stats-summary/1`. Listing filters select the sessions; each costs one
bounded transcript read through its listed backend origin. The report includes
selected/read/failed counts, per-session coverage and tool statistics, and
aggregates by harness and tool name. Pair durations cover complete timestamped
pairs only. Read failures have unknown activity and contribute no counters.
These are recorded harness tool names, such as `bash`, not inferred shell
commands. Child recordings contribute only when independently selected.

## Recorded-title lookup

Single-session commands accept `--title "Exact recorded title"` instead of an
ID or `--latest`: `show`, `brief`, `usage`, `stats`, `lineage`, `events`, and
`export`. Matching is case-sensitive and uses the recorded title only, never
a derived display hint. Lookup defaults to the current project; `--project`,
`--global`, and `--harness` choose its scope.

Lookup parses at most 5,000 candidates per backend, independently of the
ordinary listing limit. Claude discovery also stops after 10,000 directory
entries; Codex and Pi require no title scan. Multiple matches refuse with candidate IDs and
origins. Incomplete scans, unreadable records, and incomplete title evidence
refuse even when one candidate was observed. Choose an explicit ID or narrow
the scope in that case. Missing harnesses are skipped. Codex and Pi recordings
currently supply derived display hints rather than recorded titles.
