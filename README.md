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

## Install

Build from a local checkout with a Rust toolchain and Cargo on a Unix-like host:

```sh
cargo install --path crates/tapes --locked
```

For a published install, use `cargo install tapes-cli --locked`; add
`--features zip` to either command when ZIP input support is wanted.

Ensure Cargo's binary directory is on `PATH`, then run `tapes` for the workflow
guide. The published package is `tapes-cli` and installs the `tapes` binary.
ZIP input support is default-off; install with `--features zip` when archive
inputs or ZIP evidence are needed. Without that feature, plain JSON files and
directories remain available and a ZIP input reports the enabling command.
File-backed harnesses need only their recording stores. OpenCode reads its
selected native store through bounded read-only metadata transports; absent
harnesses remain optional. Development checks
are `cargo build --all-targets`, `cargo test`, and
`cargo clippy --all-targets -- -D warnings && cargo fmt --check`.

## Versioning

Released versions are the calendar date of publication, written `YYYY.M.D`.
The command, the binary, and the repository are `tapes`; the published package
name differs, because the bare name on crates.io belongs to an unrelated
project.

## Native discovery library

The workspace includes `tapes-discovery`, a Rust library that resolves
canonical identities in native Claude, Codex, Pi, and OpenCode stores. It
returns native identity and store metadata; transcript normalization remains
in `tapes-core`, and a resolved recording does not establish that a harness
is live or that a consumer can deliver an action to it.

A consumer declares the library by version:

```toml
tapes-discovery = "2026.9.25"
```

and, until the crate is published, point that requirement at a local checkout
from the machine's Cargo configuration rather than from the consuming
project's manifest, so no machine path enters the consumer's history:

```toml
# ~/.cargo/config.toml — paths resolve from the directory holding .cargo
[patch.crates-io]
tapes-discovery = { path = "code/tapes/crates/tapes-discovery" }
```

A consumer built without that override fails to resolve the crate, which is
the intended state until the tapes crates are published.

Cargo does not pin a path dependency's source revision. Record the exact Tapes
checkout revision beside the consuming project's dependency change and rerun
the independent consumer check there. Portable dependency distribution is
separate release work; the local path does not select a registry package or
remote branch.

Run `scripts/check-discovery-consumer` from the Tapes checkout with Python
3.11+ and Cargo available to build an independent consumer, inspect the crate
archive, and verify its normal dependency graph.

## Fixture demo

The demo uses only committed Codex fixtures. It creates an isolated temporary
home and empty executable search path, then removes the temporary files. It
requires Python 3 and a built binary:

```sh
cargo build -p tapes
python3 scripts/fixture-demo
```

Pass a binary path as the script's first argument to use another build.
Temporary path prefixes are displayed as `<fixture-store>`. Example output:

```text
$ tapes list --harness codex --limit 2
ID	LIVE	HARNESS	MODEL	TITLE	DIRECTORY	LAST ACTIVITY
10000000-0000-0000-0000-000000000002		codex	gpt-fixture (medium)	~Read valid lines.	/fixtures/project	2026-01-01T11:00:03Z
00000000-0000-0000-0000-000000000001		codex	gpt-fixture (high)	~Inspect the fixture.	/fixtures/project	2026-01-01T10:00:07Z
$ tapes show 00000000-0000-0000-0000-000000000001 --tail 1
[tool #5 2026-01-01T10:00:06Z]
{"call_id":"call-pending","id":"ctc_fixture_pending","input":"check fixture","name":"fixture_pending","status":"completed","type":"custom_tool_call"}
Note: The store records activity at 2026-01-01T10:00:07Z, after the newest turn rendered here (2026-01-01T10:00:06Z). The newest trailing record is `event_msg` at 2026-01-01T10:00:07Z; `show` does not render it as a turn.
Note: Showing the last 1 of 6 turns; 5 earlier turns fall outside the 1-turn window. Use --tail 6 to see them, or `tapes export` for every turn the reader can reach.
```

## License

The workspace packages declare `Unlicense`. The complete
[Unlicense text](LICENSE) accompanies the source.

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
| claude | `<CLAUDE_CONFIG_DIR>/projects/<slug>/<session>.jsonl` or `~/.claude/projects/<slug>/<session>.jsonl` | file discovery |
| codex | `$CODEX_HOME/sessions/<y>/<m>/<d>/rollout-*.jsonl` | file discovery |
| opencode | Stable SQLite or beta API store | `opencode db --format tsv` and `opencode2 api` |
| pi | `~/.pi/agent/sessions`, append-only tree | file discovery |

Claude uses the exact value of `CLAUDE_CONFIG_DIR` joined with `projects` when
the variable is set. An empty value selects `projects` under the current
working directory, and a relative value is resolved from there. When the
variable is unset, the fallback is `HOME/.claude/projects`; tapes scans only
the selected root.

A harness whose binary or store is absent reports itself unavailable; it
never fails a listing. Listing works with any subset installed.

## Supplied exports

Use `--input PATH` one or more times to read an explicit file, extracted
directory, or ZIP archive. `--input-format auto` detects the supported
OpenAI, ChatGPT Exporter, and Perplexity shapes; `--input-format openai`,
`--input-format chatgpt-exporter`, and `--input-format perplexity` require the
named representation and never
fall back to another parser. `--source-scope <label>` records a caller-declared
source namespace.

For example, a Perplexity export can be surveyed with
`tapes list --input ./fixtures/perplexity-export.zip --input-format perplexity --json`.

Supplied inputs are isolated from installed stores. `list` enumerates each
occurrence, while single-session views accept an ID, exact title, or the
opaque `--occurrence` coordinate emitted by `list`; repeated native IDs remain
ambiguous until one occurrence is named. `list` orders supplied rows by
`--sort` before `--limit`, and `--after-occurrence` continues after the named
row in that order, across files and ZIP members, only when the complete source
observation is unchanged. A named input path that does not exist is an error,
and a conversation without a native ID is reported as a gap rather than given
an invented one. The reader never
extracts ZIPs or opens referenced files and reports structural gaps instead of
claiming an unreached record is absent.

Mapping-based inputs retain a bounded graph alongside the selected canonical
turn path. Recognized associated report members retain their bounded bodies and
citation spans as artifact evidence; a report whose backing conversation is
unreached remains listed as an artifact with its association unresolved.

An OpenAI native directory or ZIP with a root `export_manifest.json` reads only
the declared conversation shards, library metadata, and manifest-declared
library `.dat` members. Nested site/account/settings JSON is ignored. Missing,
corrupt, or size-mismatched declared members are explicit input gaps; an exact
occurrence can return known evidence with those gaps, while title selection
refuses incomplete discovery and ID selection refuses any gap that could cover
an unread ID. A record without a native ID was parsed in full, so its gap does
not block ID selection. Records parsed from a ZIP member that
fails decompression or checksum verification are withheld, not listed.

The reader applies bounded per-invocation scan, decoded, record, resident, and
output budgets. Defaults are 512 MiB for source and decoded bytes, 8 MiB per
record, 512 MiB aggregate resident data, 10,000 members, and 16 MiB serialized
output; the byte flags may raise those values only to finite ceilings of 8 GiB,
64 MiB, 8 GiB, and 64 MiB respectively. A result limit never acts as a parser
budget. Every byte flag takes a count or a size such as `512MiB`, `8m`, or
`64KB`: a bare or `iB` unit is binary and a `B` unit is decimal, whatever the
case.
History-page and child-qualified reads remain installed-recording operations;
they refuse a supplied export explicitly.

## Output

`list` merges sessions from every available harness and sorts them by last
activity. `--limit` bounds each harness and defaults to 20, `--harness` selects
one backend, and `--here` restricts the listing to the project holding the
current directory. A session recorded in a directory that no longer exists,
such as a removed per-change worktree, cannot be proven to belong to the
project; the scoped listing leaves it out and says how many directories it
left out, and `--global --directory <substring>` reaches them. `--model <substring>` matches case-insensitively against the
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
projection cannot resurrect its non-match. A session the stable store could
not read is not answered from the v2 projection either: an exact read fails,
naming the store that failed and the store that also answers the id, and a
listing reports that session as unreadable.
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
`tapes-list/6` object containing `sessions`, `artifacts`, `unavailable`, `unreadable`,
`unsearched`, `scanned`, and `scan_truncated`; scoped views may also carry
structured `unplaced` exclusions. `unavailable` names harnesses
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
only from what the harness itself wrote, a field beside the text or an element
it wraps its own text in, so a `/exit` command or an injected notice is not
read as an unanswered prompt and a record carrying neither stays `unknown`
(see `docs/model.md`). Human output heads a user turn holding
anything but an operator's message `user/<kind>`. Every turn in `show` also carries its
zero-based `ordinal` in the session's normalized sequence, which a file-backed
session keeps under any window while a paged OpenCode API read renumbers when
a wider window fetches further back (see `docs/model.md`), and a `native_id`
where the harness records one; the session carries `source`, the opaque
coordinate it was read from. Human timestamps are RFC
3339 whole seconds with `Z`; JSON preserves the recorded timestamp precision.
For supplied inputs, `source.representation` records the detected or declared
shape. `source.producer` is omitted when auto-detection cannot establish who
produced an overlapping representation; an explicit input format carries
`producer_authority: "declared"`.
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
The optional `read` object records the source length, configured bound,
head/tail/alignment physical ranges, decoded record spans, context-only
record spans, and gaps; physical coverage does not erase malformed records, and
a partial gap is discharged only when a successful decode identifies that same
record. A preceding newline lets an exact tail boundary retain its first
record; a mid-record tail records the discarded partial prefix. Alignment bytes
are physical I/O, not normalized coverage. A `terminal` observation retains
native stop fields reached by the read, including bounded nested Codex error
fields, without claiming present-tense liveness or account availability;
`text_tail` explains an empty operator/assistant tail. Each turn also
keeps ordered native content `parts` and a `coverage` summary, so an image,
file reference, structured artifact, tool payload, or unknown part does not
disappear merely because it has no readable text. The turn's
`record_ref.part_index` identifies its normalized position within the source
record. A part reference repeats that parent coordinate and adds
`content_part_index` for its position within the turn; a source-native
`pointer` remains the source pointer at both levels.
JSON output uses the `tapes-session/12` transcript schema, with the optional
`live` annotation when the authority answers. The human header marks the same
state. Supplied mapping exports also carry bounded graph evidence with the
selected canonical path, retained nodes, and retained parent-child edges.
Artifact-native reports carry their bounded body and citation spans, including
when their outer conversation association remains unresolved.
Citation kind, URI, and title descriptors use 4 KiB per-field and 16 KiB
cumulative UTF-8 byte bounds; shortened values retain their original lengths
and explicit truncation facts.

`--only <KIND>` and `--omit <KIND>` on `show` or `export` keep turns by kind:
repeatable or comma-separated, with the kind labels `operator`, `assistant`,
`reasoning`, `tool`, `control`, `ambient`, `notice`, and `unknown`, and never
together. Kept turns carry their timestamps, native ids, and original
ordinals; every other turn is counted by kind under `projection`, and `--tail`
counts kept turns. `show --exchange` is the name for `--only
operator,assistant`: operator requests and assistant-visible text.

A file-backed transcript read takes the last 4 MiB of a recording.
`--read-bytes` on `show`, `export`, `events`, `lineage`, `stats`, `usage`,
`brief`, `endings`, and `child` sets that bound between 64 KiB and 1 GiB; the
window is held in memory whole, and `read.configured_bound` records the bound
in force.

`show --full` reads the whole recording instead of its tail, writing each turn
as it is read, so memory follows one record rather than the file. Every turn
is shown unless `--tail` keeps the newest, and `--exchange`, `--only`, and
`--omit` apply as they do to a bounded read. It reads installed Claude, Codex,
Pi, and OpenCode sessions, and `--read-bytes` is not available with it.
`--full --json` writes the same `tapes-session/12` object turn by turn: its
`read` evidence is one range from byte 0 to the source length with
`projection_options: ["full"]`, and each turn's `record_ref` carries its own
record span. An OpenCode session is paged from its store oldest first, every
message rather than the newest 1,000, so memory follows one page of messages;
its `read` range counts messages (`coordinate_domain: "opencode-message"`),
and its turns carry native ids rather than record spans. The stable database
still cuts part text at 4,000 characters and each tool payload field at 2,000,
reported under `truncation.source`, and a message larger than the 8 MiB API
transport bound refuses the read. A Pi recording is read twice: the first pass
keeps only each entry's id and parent, which its active branch needs from
elsewhere in the file, and the second pass, stopping at the length and naming
the source revision the first observed, writes the turns.
A record that is malformed or longer than 64 MiB is skipped and counted. A
recording replaced or shortened during the read refuses; one appended to is
read to the length it had when the read opened.

`show` and `export` also take `--latest` in place of an ID, which resolves the
most recent session in scope. `--exclude <id>` is repeatable and passes over
sessions the caller already holds — including its own, which is otherwise the
newest one there. If the bounded candidate scan leaves unreadable rows or
stores, the readable choice is returned with a warning naming them and saying
that newer activity may be hidden; an unavailable harness alone is not such a
warning.

`self` stands for the caller's own session wherever a session id is taken —
`tapes show self`, `--exclude self` — resolved from the first of
`CLAUDE_SESSION_ID`, `CLAUDE_CODE_SESSION_ID`, `CODEX_THREAD_ID`,
`OPENCODE_SESSION`, and `PI_SESSION_ID` that is set; the hand-set Claude
spelling comes before the one Claude Code exports. What it became is written
to stderr. It refuses when none is set (OpenCode v2 exports no session id),
when no installed store holds the id as a session of the harness that
exported it, and for a supplied input, which holds no session of the
caller's. `--project <path>` scopes elsewhere and `--global` drops the
scope.

`events` projects tool calls and results into the harness-neutral
`tapes-events/7` schema. Each record keeps the turn ordinal and native id,
bounded argument or output metadata, and an exact call/result pair when both
halves occur in the bounded read. `--full-arguments` returns each call's
complete recorded argument text in place of the 200-character prefix, under
the same members and the same schema version. A Codex completed runtime item that carries
both halves in one native record is projected as that pair with one shared
source reference. Unpaired calls report `no-result-in-read`;
an unpaired result reports `call-before-read-bound` when a file-tail or
record-page bound can hide its call, and `call-not-recorded` when the read
reached the recording's start. `--tail` uses the same turn-ordinal window as
`show`, while `--name` and `--call-id` apply after pairing. With no `--tail`,
the command returns every event the bounded reader reaches so counts describe
the read rather than an implicit display window.
`--program` selects exact nested program declarations while preserving the
recorded outer tool name and pairing. Literal shell and JavaScript declarations
are qualified with their source span and parser coverage; structured runtime
argv is preferred. JavaScript requires a complete direct literal command
property, and structured argv rejects non-string or over-bound arrays instead
of filtering or truncating them. A shortened first or second invocation token
is unsupported rather than an exact-looking program or subcommand name; later
arguments retain their bounded truncation facts. Unsupported dynamic, arrow, short-circuit,
ternary, shell-assignment, or shell-control syntax stays evidence of
uncertainty, not an executed child call; wrapper results never witness or time
an individual nested declaration. Explicit artifact references and within-read
consumption observations remain bounded descriptors and never open the named
object. Codex lifecycle mirrors for messages, reasoning, user messages, and
compaction are not projected as tools; only verified `CommandExecution` and
`FileChange` items enter this event layer.

`usage` answers where one session's quota went as `tapes-usage/6`: the
session's recorded `tokens`, cost, accounting, turn counts, Codex model
observation status, and (where the source supports it) a by-model request
split — Claude's `cost-state` `modelUsage`, Codex's request observations, or
pi's per-message model each qualified by the thinking level in effect. The
accounting `basis` and `coverage` decide whether
figures may be summed — a recorded total and a sum of per-request records are
both safe to add, while coverage says how much of each session a figure covers.
An observed counter restart is `since-reset` for the newest recorded total;
request attribution retains the coverage of the read that observed it and
keeps the reset count even when modern request records are selected. The
newest recorded total stays numeric and is not rebuilt by summing request rows.
Cost is only what the harness recorded; a provider quota is a separate fact
about the account.
Claude's recorded `cost-state` keeps its durations and model split unchanged.
Human output prints the attribution basis, uncertainty, and the same
truncation notes `show` prints.

`usage SESSION --series[=N]` opts into raw Codex accounting observations: 200
rows by default, 1 through 10,000 when explicit. Rows are not guaranteed
request identities. They preserve native ordinals, response IDs, source
coordinates, model context, counters, quota-only events, repeats, resets, and
classification. `--full --series` scans the pinned recording while retaining
only the bounded recent suffix. The series reports rows observed, rows
returned, row-cap/byte-budget/oversized-row omissions, and unique source gaps;
gaps beyond the retained gap budget are counted once each. The
aggregate attribution still covers the whole actual read. It is unavailable
for supplied inputs, selection summaries, and unsupported harnesses.

`usage --full` streams one session's whole recording instead of its tail.
Turns are counted and inventoried as they arrive, never held, and tokens, cost,
context window, quota, and model are folded from every record the turn read
reached, so `turns.coverage` and `accounting.coverage` are `session` and `read`
carries `projection_options: ["full"]`. OpenCode keeps a session's counters on
its own row as a whole-session total. `lineage --full` likewise reads every
record, so a spawn or completion recorded before the tail window is named,
with memory following the references kept rather than the file; OpenCode
records relationships on session rows, which no transcript bound reaches.
Both refuse `--read-bytes` and supplied inputs, and `usage --full` refuses a
selection.

`events --full` and `stats --full` stream the whole recording twice: the first
read observes every tool record and counts the turns, and the second replays
it, pinned to the length and file revision the first observed, pairing each
record and writing or counting it as it is emitted, so memory follows the
calls awaiting a result rather than the file. `events --full` writes the same
`tapes-events/7` object event by event; `--tail`, `--name`, `--call-id`, and
`--program` select as they do over a bounded read, `--full-arguments` widens a
kept call's arguments the same way it does there, and `pairs` counts the
returned events. `stats --full` folds turns, tool calls, durations, and errors
record by record, takes the counters from every record and the children from
the read `lineage --full` takes, so `coverage.turns` is `session`; given a
selection it streams each selected session, and one that cannot be streamed
is named under `failed`. Both read installed Claude, Codex, and Pi sessions,
refuse `--read-bytes` and supplied inputs, and refuse an OpenCode session,
whose messages are updated in place so a second read may not repeat the
first.

Given a scope or a listing filter instead of a session, `usage` answers the
whole selection as `tapes-usage-summary/4`: the same flags `list` and `export`
take, grouped by `--by harness,model,variant,directory` and defaulting to
harness and model. It sums the counters the listing already carries, so no
transcript is read. Each sum covers the sessions that recorded that counter
and `counted` says how many those were — a counter nine of twelve sessions
recorded is not a figure about twelve — while `coverage` counts the sessions
behind a sum by their accounting, including those whose harness recorded
nothing to sum. Cost is summed only where a harness recorded one, never
inferred from tokens. Human output includes group and total rows, compatible accounting partitions,
a mixed-accounting explanation when sums are omitted, and listing diagnostics.

`endings` answers what each session of a selection ends on as
`tapes-endings/6`, so deciding which endings deserve reading costs one bounded
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

`stats` counts what one recording holds as `tapes-stats/9`: `turns` by the
`kind` the harness recorded them as, beside the kinds that harness can record
at all, `tools` — calls, results, complete pairs,
unpaired events by the boundary that left them unpaired, errors, and a
`by_name` row per tool with its paired durations — the recorded clock in
`durations_ms`, the distribution of assistant-turn wall clock in
`assistant_turns_ms` (one sample per source record carrying a model response —
its text, its reasoning, or a tool call — measured from the previous turn's
source record: count, median, p90, max), the session's
own counters
with the share of
`input + cache_read + cache_write` each cache counter accounts for, the model
and reasoning-level split its harness recorded, and the
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
`tapes-brief/7`, for the case where resuming the session itself has gone too
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

- `.context.md` — the exchange `show --exchange` returns: exact operator turns
  and assistant-visible text, without the harness's own commands, notices,
  attached context, or user turns of unknown sender. Read first.
- `.json` — the canonical `tapes-session/12` object plus turns, cost, tokens,
  their `accounting` basis and coverage when present, any verified
  `trailing_record`, retained graph/artifact evidence, and the session
  directory's git head and branch when they resolve. Query selectively with
  `jq`.
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
else goes there. Each invocation reserves a free timestamped prefix before it
writes, and each file is created under a temporary name and published without
replacing an existing path. A bundle never looks complete while it is half
written, concurrent exports keep their own sibling files, and a failed export
leaves only the invocation's files to clean up. The `.json` is one compact
object; `jq` reads it as easily as an indented one.

`export --full` bundles the whole recording instead of the bounded read, as
`show --full` reads it, so memory follows one record rather than the file. The
recording is streamed twice: the first read writes the JSON turns and observes
tool calls and results, and the second replays it, stopping at the length and
naming the source revision the first observed, to pair the events and write
the Markdown files. A recording appended to between the two reads is exported
as the first read saw it; one replaced between them refuses. The JSON turns
are the ones `show --full --json` writes, and `--only` and `--omit` narrow all
three files. It reads installed Claude, Codex, and Pi recordings; OpenCode
refuses it by name, since its message rows are updated in place and a second
read cannot promise the first read's turns. `--read-bytes` is not available
with it, and a supplied input refuses it.

`export --evidence`, for a supplied input, also copies the source bytes behind
each conversation into a `<bundle>.evidence/` directory beside its bundle: the
conversation record's own span and every associated report's whole member,
byte for byte, named by their SHA-256, with a `tapes-evidence/2`
manifest giving the input's length and digest, each file's member, size, CRC,
and span, and what could not be copied. A report is copied out of whichever
container holds its conversation, a supplied directory or an archive alike;
a record supplied as a file on its own sits beside no members, and the
manifest says so with `associations_resolvable` rather than reporting an
empty association list that could be mistaken for a conversation without
reports. A consumer that must cite exactly can keep the evidence and resolve
every citation without the original input.

Given the listing flags in place of an id — `--here`, `--project <path>`,
`--global`, `--harness`, `--model`, `--directory`, `--since`, `--until`,
`--sort`, `--limit`, `--search` — `export` takes the set `list` would return,
in the same order, and writes one bundle per session, each read whole under
`--full`. Bundles are never
joined: each session keeps its own three files, and a
`tapes-export-manifest/5` `manifest.json` beside them is the only file that
spans the set. It records the selection, each session's bundle paths, the
artifact-native reports discovered in supplied inputs, the
sessions whose store could not be read under `failed`, and the listing's own
`unavailable`, `unreadable`, `unsearched`, `scanned`, and `scan_truncated`
diagnostics, plus scoped `unplaced` exclusions when present. Stdout adds each
bundle's three lines in selection order, then
the manifest's own path and size. A session whose store vanishes between the
listing and the read costs its own bundle and nothing else; the command fails
only when every selected session failed, or when the listing itself did.

## Remote replicas

`list`, `show`, and `export` take `--remote <ssh-destination>`: the named
host's own `tapes` answers, and its sessions name that replica instead of
being read here. Nothing is installed on the replica, and none of its store
is copied. `ssh` carries the query and runs with `BatchMode=yes`, so a
query never prompts for a host key or a password; `TAPES_SSH` names a
different program (a wrapper, a fixed configuration, a jump-host helper),
which receives no added options,
`TAPES_REMOTE_MAX_BYTES` bounds an accepted answer (64 MiB by default), and
`TAPES_REMOTE_DEADLINE_MS` bounds the wait (60 s by default).

`list --remote host` and `show --remote host <id>` read the replica's JSON
and tag every session with `source.replica`. The replica's own scope paths,
stores, and liveness authority apply: `live` is what that machine reported,
this machine's `harness-status` is never consulted, and a session the replica
did not mark is `unknown` rather than idle. `export --remote host ...` runs
the replica's own `export`, so the bundles are written through the reader
that holds the recording, and every printed path is `host:path`, the
coordinate `scp` and `rsync` accept; `--bundle` names a directory on the
replica for that reason.

Every failure to reach or run the replica — no `ssh`, no `tapes` there, a
schema this build does not read, an answer past the byte bound, no answer
within the deadline — is unavailable with its cause beside the destination:
`list` carries it in `unavailable` and exits unsuccessfully, and `show` and
`export` refuse without printing a session or a path. An empty listing is
therefore never a replica that holds nothing, and a refusal is never a
completed session.

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
store separates it from the session that just died, so a caller should pass
`--exclude self`, or `--exclude <id>` where its harness exports no id; `tapes`
reports the latest and never guesses which one the caller meant.

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

**A query may name another machine.** `--remote <ssh-destination>` on
`list`, `show`, and `export` asks that host's own `tapes` and returns its
answer tagged with the replica. It is a transport, not a sync: nothing is
installed there and no store is copied. A replica's sessions carry
`source.replica`, and their `live` field is only ever what the replica
reported — this machine's status authority never answers for another
machine's processes.

**Contracts you can build on.** `tapes-list/6`, `tapes-session/12`,
`tapes-events/7`, `tapes-usage/6`, `tapes-usage-summary/4`, `tapes-lineage/2`,
`tapes-endings/6`, `tapes-child/4`, `tapes-stats/9`, `tapes-stats-summary/5`, `tapes-brief/7`,
`tapes-page/6`, `tapes-history-search/6`, `tapes-metadata-history/6`,
and `tapes-export-manifest/5` are versioned
JSON; a breaking shape change bumps the version. A single-session `export`
prints
exactly three paths and their sizes on stdout, in reading order, reserves a
unique sibling prefix, and publishes each file without replacing an existing
path — so a bundle is never observed half-written or collides with another
invocation. A harness whose binary or store is absent reports itself
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
from 1 KiB to 4 MiB; the default is 64 KiB. Session lookup keeps its own
bounded metadata read. A page also probes at most one alignment byte, counted
separately as `alignment_bytes`, and decodes no record outside its byte range.
A Codex turn's kind comes from its own record, so a page gives it the kind a
whole read does.
Ordinary show/export retain their source bounds.

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
History search and metadata accept 1–32 pages per call. Metadata pages extract
model observations directly without normalizing transcript turns; their read
evidence uses the page schema with the `models-only` option. Session lookup retains its ordinary bounded
metadata read; page counters describe the subsequent history traversal.
## Tool usage across sessions

`tapes stats --here --since 2026-01-01 --json` returns
`tapes-stats-summary/5`. Listing filters select the sessions; each costs one
bounded transcript read through its listed backend origin, or two streamed
whole-recording reads with `--full`. The report includes
selected/read/failed counts, per-session coverage and tool statistics,
aggregates by harness and tool name, and per harness the kinds it can record
that no session read held. Pair durations cover complete timestamped
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
`tapes-child/4` report retains parent identity and child reference; its transcript
uses the qualified ID `PARENT::CHILD`. This ID does not become an ordinary
listed session: use the child command to read it. Missing references refuse.

`--tail` bounds rendered transcript turns (default 40). Usage and ending facts
cover the child's own bounded source read, with source coverage retained.
`child --full` streams the child's whole recording: usage counts every turn and
folds its counters from every record, every record naming a native session is
checked against the parent, and the transcript and ending keep only the newest
`--tail` turns, so an ending over a longer recording has `window` coverage.
Nested child lineage is explicitly uninspected; no child activity is absorbed
into its parent. Other harnesses' ordinary child sessions remain addressable
by their recorded session IDs.
## Compatible accounting totals

Usage summaries partition counters by harness, accounting basis and coverage.
A total or requested group spanning incompatible domains retains its session
and contributing-counter counts, sets `mixed_accounting: true`, and omits token
and cost sums. `partitions` retains each compatible sum, including explicitly
missing accounting. Recorded totals, full-session request sums and read-window
request sums are never presented as one comparable total. This contract is
`tapes-usage-summary/4`; single-session `tapes-usage/6` carries the same
accounting plus bounded read and terminal evidence.
