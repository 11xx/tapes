# Session model

Every harness backend translates its records into the same `tapes-core`
types. Consumers can therefore list sessions and read transcripts without
knowing which harness stored them.

## Types

`Session` identifies the native session or conversation, records optional first
and latest activity timestamps, and may carry a model, recorded title, derived
title, working directory, present live state, cost, token counts, and their
accounting metadata. Missing timestamps remain absent; filesystem times are not
substituted, and a session without activity cannot be chosen by `--latest`.

`LiveState` is either `working` or `idle`. It is an optional present-tense
annotation joined by session id from the `harness-status` command; recording
backends leave it absent. The authority's `working` state maps to `working`,
while `completed`, `idle`, and `attention` map to `idle`. Unknown states are
ignored per thread so known entries remain usable. If the authority is
unavailable, malformed, oversized, or slower than its 250 ms wall-clock
deadline, live state remains absent. The field is added to `list` and `show`
output only and is not written to export bundles. Human list output places it
in a separate `LIVE` column after the exact `ID` column.

`title` is the harness-recorded value and remains absent when the harness did
not provide one. `derived_title` is a bounded hint made from the first
meaningful user turn for human discovery; it is separate so consumers can tell
recorded metadata from a display aid. Known instruction envelopes are removed
and whitespace is collapsed before the hint is capped at 96 Unicode
characters, using an ellipsis when it is shortened. Human renderers prefix
this hint with `~`; JSON retains the recorded title, derived hint, and
truncation marker separately. The Claude and Codex readers take the first user
turn from the file's opening, which is read even when the transcript is larger
than the bounded tail, so the hint names the session's actual first turn; the
pi reader withholds the hint past that bound, since the opening cannot prove
which root its active path descends from; and any reader that cannot see the
first user turn leaves the hint absent rather than labeling a later turn as the
first. API-backed metadata readers also leave it absent
when deriving it would require an extra message request; this keeps listing
bounded and keeps show/export metadata consistent with listing metadata.
`derived_title_truncated` is present with the derived hint and is `true` when
the hint was shortened, or `false` when the present hint is complete. For
Codex, an absent `model` can mean that the model-bearing `turn_context` was
before the bounded 4 MiB file-tail read; the reader preserves that absence
rather than inventing a model.

`source` describes where the normalized session came from. Its `kind` is
`installed-recording` or `supplied-export`; `origin` names the platform,
`recorded_harness` is present only when that platform supplied one, and
`representation`, `producer`, `producer_authority`, `scope`, and `location`
retain the known representation, provenance authority, and opaque
source/container coordinate. `producer` is omitted when auto-detection cannot
establish a producer for a structurally overlapping representation. An
explicitly named input format marks its producer with
`producer_authority: "declared"`; it is not an observation from the bytes.
`id` is the native
conversation or session identity; it is not a path, archive member, or global
identity outside the descriptor's scope. Supplied sources also carry an opaque
`occurrence` coordinate, so repeated native IDs remain separate observations.
OpenAI and ChatGPT Exporter conversation representations use the OpenAI
platform origin; their representation and producer fields remain separate.

`Session.metadata` retains provider-specific conversation-level source facts
that do not identify the model or prove an outcome. Perplexity supplies
collection, mode, engine, status, and label values from the conversation object
when recorded. Entry-level values stay on `Turn.metadata`. The session
metadata is carried by the brief, ending, lineage, stats, and usage identity
projections; absent values remain omitted.

Caller-supplied inputs use `--input PATH` one or more times with optional
`--input-format auto|openai|chatgpt-exporter|perplexity` and `--source-scope`.
The reader
accepts files, extracted directories, and ZIP members without extracting or
opening referenced artifacts. It uses per-invocation scan, decoded, record,
resident, member, and serialized-output bounds; `--after-occurrence` continues
a collection only when the ordered supplied-input observation still matches.
The coordinate therefore remains valid across files in one collection, while
any changed file or ZIP member refuses continuation. A partial scan reports
gaps and refuses to pretend an unreached record is absent. Supplied input is a
separate source collection and never falls back to installed harness stores.

Perplexity conversations retain collection, mode, engine, status, and label
metadata from the conversation object when those fields are recorded. Entry
queries and answers are separate content records with the entry reference;
entry engine, status, and label remain on both projected turns with native
field pointers. An entry timestamp belongs to the query record, while a
response does not receive a copied timestamp.
Null and non-string fields remain explicit unknown coverage rather than empty
text or invented values.

`started_at` and `last_activity_at` come from two bounded windows on a
file-backed session. File-backed readers open the first 64 KiB, where every
harness writes its session header, and the last 4 MiB, where the newest
records are; neither window grows with the file. `started_at` is the earliest
timestamp across both, so a session larger than the tail still reports the
start its header recorded rather than the first record the tail happened to
retain. The head probe grows to 1 MiB when the first line alone is longer than
64 KiB. A first line longer than that leaves the opening empty; the start then
falls back to the tail's earliest record and the session carries
`start_uncertain: true`, meaning the session began at or before `started_at`.
The flag is omitted when the start is the recorded one. `last_activity_at` is the newest timestamp in the tail, which is the
end of the file. The session id and directory are likewise read from the
opening first. A header without a timestamp falls back to the tail's earliest,
and a file with no timestamp anywhere in either window remains a session with
both timestamps absent. Missing activity is ordered after timestamped activity
and cannot satisfy a date predicate. `--latest` refuses when a candidate lacks
the activity evidence needed to prove the winner; filesystem modification time
is never substituted.

`Model` contains the model identifier and an optional variant. The variant
also carries an effort level when the harness records one. Its identity is the
identifier alone, or `id (variant)` when a variant exists; `list --model` uses
that full identity for case-insensitive substring matching and does not match
sessions whose model is absent. `list --directory` likewise performs a
case-insensitive substring match against the recorded path and does not match
sessions whose directory is absent. Both filters are applied before the
per-harness listing bound. `Cost` contains a single USD value. `Tokens` can
independently record input, output, reasoning, cache-read, and cache-write
counts. Each counter is what the harness recorded and is absent where it
recorded nothing. `Accounting` is present exactly when `cost` or `tokens` is
present and serializes as an object with two independent axes:

- `basis` is `recorded-total` when the harness supplied a cumulative
  session-level record, or `summed-requests` when `tapes` added per-request
  usage records.
- `coverage` is `session` when the figures cover the whole recording, or
  `read-window` when a bounded source read may have left older requests
  unread.

OpenCode supplies recorded totals from its session row or API response, and
Codex supplies the cumulative totals of the newest `token_count` event in the
bounded read. Both are `recorded-total`/`session` because the harness records
running session totals. Claude uses the newest `cost-state` record in the
bounded read as a `recorded-total`/`session`; without one, it sums each
per-request `message.usage` once by `requestId`, using `summed-requests` with
coverage determined by the file-tail bound. pi sums `message.usage` from the
active conversation path, excluding abandoned branches, with the same
coverage rule. Cost, provider quota, and recorded tokens are distinct facts;
no counter or cost is derived from another counter.

A session also carries `usage_detail`: usage facts a harness records that the
normalized model has no field for, filled by the backend holding them. Like a
turn's tool event it is skipped by serialization, so the usage view is where
it reaches a consumer; bounded read and terminal evidence remain on the
transcript contract.

`Turn` contains a role, a `kind`, text, an optional UTC timestamp, an
`ordinal`, optional `native_id`, an optional `request_turn_id`, optional
entry-scoped metadata, and an optional `record_ref`. Roles are `user`, `assistant`, `tool`,
and `reasoning`.
A tool turn also carries one typed `ToolEvent` inside the process for the
`events` projection. The field is skipped by serialization, so
the session wire object and export bundles retain the tool's harness envelope only in
`text`.
In ordinary transcript views, the ordinal is the turn's zero-based position in the normalized turn
sequence, counted from the first turn the reader reaches. For a file-backed
session the reader's reach is the file's last `--read-bytes` (4 MiB by
default) whatever the window, so
the ordinal does not change with `--tail`: `show --tail 1` returns the turn
whose ordinal is the sequence's last, the window under `truncation` names the
ordinals it holds, and a consumer holding a session id and an ordinal re-finds
the turn with `show` alone. On a recording past that bound the sequence starts
at the bound, which is what the `file-tail` source entry says, and it moves as
the file grows. For a paged store read, the OpenCode API, the reach depends on
the window: a read that stopped with the window full reports
`omitted_exact: false`, its ordinals count from the oldest turn it fetched,
and a wider request renumbers. Such a reference should carry the `native_id`,
which OpenCode always records. `native_id` is
the harness's own id for the record the turn came from, when the harness
records one: Claude's message `uuid`, pi's entry `id`, OpenCode's message
`id`, and Codex's `payload.id` where a response item carries one. Several
turns share it when one record yields a message, its reasoning, and its tool
calls.

`Turn.metadata` is an `EntryMetadata` object when a supplied provider records
metadata for the native entry. It carries optional `engine`, `status`, and
`label` values plus `source_fields`, a map from those normalized names to the
native JSON pointers that supplied them. Empty strings remain values; null and
absent fields are omitted. Conversation metadata remains on `Session.metadata`.

`record_ref.part_index` is the zero-based position of the normalized turn
within its source record. A `ContentPart.record_ref` repeats that parent
coordinate and adds the optional `content_part_index`, the zero-based position
of the part within the turn. A source-native `pointer` remains the pointer to
the source record; the content coordinate is never packed into it or used to
invent a second native path.

`parts` is the ordered content inventory for a turn. Text and recorded
transcription parts carry readable bodies; media and file parts carry only a
verified `ArtifactReference`; structured and tool parts carry bounded shape
descriptors; unknown parts retain their native kind and bounded key/type
shape. `coverage` distinguishes retained body, reference-only, unsupported,
read-bound, and unknown representation and reports retained and omitted part
counts. No referenced path is opened and no media/base64 body is retained as
a descriptor.

`kind` is always present and says what the record behind a turn is, which the
role alone cannot: a harness records its own commands, the context it attaches,
and the messages it injects in the same user envelope an operator's prompt
arrives in. Its values are `operator`, `assistant`, `reasoning`, `tool`,
`control`, `ambient`, `notice`, and `unknown`. An `assistant`, `reasoning`, or
`tool` turn always carries the kind of its own role, so only a user turn takes
any other value:

| kind | what the turn holds |
|---|---|
| `operator` | Content a person, or the caller driving the harness, addressed to the agent. A record carrying ambient context beside a request is one. |
| `control` | A harness command or control message recorded in a user envelope, such as Claude's `/exit` and its local-command output. |
| `ambient` | Context the harness attached on its own, with no request in it. |
| `notice` | A message the harness injected on the system's behalf, such as a task notification. |
| `unknown` | A user-envelope turn the harness recorded no evidence for. |

Every value rests on what the harness itself wrote — a field beside the text,
or an element the harness wraps its own text in — never on what the text seems
to ask, so `unknown` is the answer for a record whose harness version wrote
neither:

| harness | evidence |
|---|---|
| Claude | `origin.kind` and `promptSource` name the sender; `isMeta` marks text the harness attached; on a record carrying none of the three, content that is exactly a `<command-name>` envelope or a `<local-command-stdout>` element is the harness's own command. |
| Codex | No field names the sender, so the elements the harness wraps its own text in are the separation, whatever entry point started the session. A user message whose every block is attached context, such as `<environment_context>`, an `<INSTRUCTIONS>` block, or a `<skill>`, is `ambient`; one that adds only a message the harness raised, `<turn_aborted>`, `<subagent_notification>`, or `<codex_internal_context>`, is `notice`; any other message is `operator`. A Codex user turn is never `unknown`, and its kind comes from its own record, so every read gives it the same one. |
| pi, OpenCode | Neither records anything but the operator's messages in its user role. |

## Tool event layer

`ToolEvent` is the harness-neutral representation attached while a backend is
already parsing a tool turn. `kind` is `tool-call` or `tool-result`; `subtype`
keeps the harness's record kind; and optional `name`, `call_id`, and `status`
keep only facts the record supplies. Call arguments and result output use a
`Bounded` value with the payload's Unicode character count and its first 200
characters. A string is measured as written, while an object or array is first
serialized as compact JSON. The preview always ends on a character boundary.

`invocations` are bounded declarations nested inside the outer recorded tool
event. Structured runtime argv is marked `structured-runtime`; literal shell
and JavaScript forms are marked `static-declaration`. Each declaration keeps
its program, subcommand, bounded arguments, source field/span, coverage, and
optional intent. Variables, interpolation, heredocs, loops, and conditional
execution remain unsupported or conditional; declarations never inherit a
wrapper's duration or success. A wrapper result does not populate a nested
declaration's `witnessed_result`; that field requires a separate native result
for the same structured operation. `events --program` selects these exact
program names while `--name` continues to select the outer recorded tool.

`artifact_references` are explicit structured descriptors, never path-like
text guesses. A paired result can attach an
`artifact_consumptions` entry with `matching-consumption-observed` only when
the qualified reference matches. Otherwise the entry says
`no-matching-consumption-observed-in-read`; neither status claims byte delivery
or perception, and no referenced object is opened.

The same `ArtifactReference` type can appear in a transcript's top-level
`artifacts` collection. Supplied report readers retain a bounded `body` when
the report carries readable text, a `body_availability` value when the body is
absent or unsupported, and `citations` with recorded URI, title, kind, and span
fields. Citation kind, URI, and title strings use `BoundedText`: each field is
limited to 4 KiB of UTF-8 bytes and all citation descriptors share a 16 KiB
cumulative byte bound.
A truncated URI is explicitly incomplete and must not be treated as a valid
reference. A grouped citation keeps its span and nests bounded `sources`; both
group and source bounds report omitted members. `backing` and
`originating_conversation` record association claims — the session backing
the artifact and the conversation it was requested in, which a source can
name separately; neither creates a turn when the outer conversation is
absent.

OpenCode records a call and its outcome in one `tool` part. Its turn carries a
call event with the state output, native status, and completion timestamp. The
event projection emits a result record from that same event when the state is
`completed` or `error`; both records use the part's turn ordinal and pairing
key. A `pending` or `running` part emits only the call.

Pairing scans a session's bounded read in turn order. A result pairs with the
most recent unpaired call carrying the same `call_id`. Both records name the
counterpart's ordinal and optional native id. A paired call carries
`duration_ms` only when both timestamps are present and the result is not
earlier than the call; results never carry a duration. Pairing happens before
an event window or filter, so the reference remains when a counterpart falls
outside the returned set.

An unpaired event carries one `incomplete` reason:

| value | meaning |
|---|---|
| `no-result-in-read` | The bounded read ended without reaching a result for this call. |
| `call-before-read-bound` | A `file-tail` or `record-page` source bound can hide the call for this result. |
| `call-not-recorded` | The read reached the recording's start and contains no call for this result. |

`tapes events` serializes the projection as `tapes-events/6`. The object holds
the same `Session` representation as `show`, the event records, complete and
incomplete pair counts, and the transcript's truncation and notes. A
`--tail N` window keeps events whose turn ordinals are in the final `N` turns;
the window metadata therefore uses the same turn coordinates as `show` rather
than counting event records. Name and call-id filters apply after pairing.
`pairs.complete` counts distinct complete pairs represented by at least one
returned event, while `pairs.incomplete` counts returned events carrying an
`incomplete` reason. Both counts describe the returned window, since the
window is applied before they are taken; the pairing itself is what the window
never changes. On the OpenCode API path the window also bounds the read, so
the events a wider `--tail` returns may pair with calls a narrower read never
fetched.

```json
{
  "schema": "tapes-events/6",
  "session": {
    "id": "session-1",
    "source": {
      "kind": "installed-recording",
      "origin": "codex",
      "recorded_harness": "codex",
      "representation": "codex-recording",
      "producer": "codex",
      "location": { "locator": "/store/rollout.jsonl" }
    },
    "started_at": "2023-11-14T22:13:20Z",
    "last_activity_at": "2023-11-14T22:15:00Z"
  },
  "events": [
    {
      "ordinal": 12,
      "kind": "tool-call",
      "subtype": "function_call",
      "name": "exec",
      "call_id": "call_1",
      "arguments": { "chars": 14, "preview": "{\"cmd\":\"true\"}" },
      "pair": { "ordinal": 13 },
      "duration_ms": 4312
    }
  ],
  "pairs": { "complete": 1, "incomplete": 0 },
  "truncated": false
}
```

## Usage view

`tapes usage` answers where one session's quota went and serializes as a
`tapes-usage/5` object. `tokens`, `cost`, and `accounting` are the session's
own fields, repeated unchanged: a `recorded-total` is cumulative and a
`summed-requests` figure is a sum of per-request records, so either may be
added across sessions, and `coverage` is the difference a consumer must
respect. Cost is only what a harness recorded, and a provider quota is a
separate fact about the account rather than about this session.

`turns` counts the normalized turns the read reached, by role — `user`,
`assistant`, `tool`, `reasoning` — plus their `total`. Its `coverage` is
`read-window` when a source bound withheld whole turns, and `session`
otherwise; the read uses the same unbounded turn window `export` does, so the
counts are the bounded read's rather than a display window's.

`usage --full` answers from the whole recording streamed. `turns` counts every
turn with coverage `session`; `tokens`, `cost`, `accounting`, `context_window`,
`rate_limits`, and `session.model` are folded from every record the turn read
reached rather than from the tail, so a summed figure's coverage is `session`;
and `read` is the whole-recording evidence `show --full --json` carries. No
`text_tail` is reported, and `truncation` holds only the `turn-text` bounds an
OpenCode database projection cut.

The remaining objects are present exactly when the harness recorded them:

| member | source |
|---|---|
| `context_window` | Codex `info.model_context_window` |
| `rate_limits` | Codex `rate_limits`: optional `primary` and `secondary` windows with native `used_percent`, `window_minutes`, and an RFC 3339 `resets_at`, the account `plan`, native `credits`, reached-limit flags, and the observation timestamp |
| `durations_ms` | Claude `cost-state` wall clock: `api`, `api_without_retries`, `tool`, `total` |
| `by_model` | Claude `cost-state` `modelUsage`, one entry per model with its `tokens` and `cost`, ordered by model id |

pi and OpenCode record none of them, and each stays absent rather than empty
or null.

```json
{
  "schema": "tapes-usage/5",
  "session": {
    "id": "session-1",
    "source": {
      "kind": "installed-recording",
      "origin": "codex",
      "recorded_harness": "codex",
      "representation": "codex-recording",
      "producer": "codex",
      "location": { "locator": "/store/rollout.jsonl" }
    },
    "model": { "id": "gpt-5.6-sol", "variant": "high" },
    "started_at": "2023-11-14T22:13:20Z",
    "last_activity_at": "2023-11-14T22:15:00Z"
  },
  "accounting": { "basis": "recorded-total", "coverage": "session" },
  "tokens": { "input": 1200, "output": 300, "cache_read": 1000 },
  "turns": {
    "user": 1,
    "assistant": 1,
    "tool": 3,
    "reasoning": 1,
    "total": 6,
    "coverage": "session"
  },
  "context_window": 828400,
  "rate_limits": {
    "primary": {
      "used_percent": 1.0,
      "window_minutes": 300,
      "resets_at": "2026-01-01T14:00:00Z"
    },
    "plan": "plus",
    "credits": { "balance": "0", "has_credits": false },
    "spend_control_reached": false,
    "observed_at": "2026-01-01T10:00:06.700Z"
  },
  "truncated": false
}
```

The view also carries the read's `truncated` flag, its `truncation` record,
and its `notes`, with the same meaning they have on a transcript.

## Lineage view

`tapes lineage` answers which sessions a recording names as its relatives and
serializes as a `tapes-lineage/2` object. A relationship exists only where a
record states it: a child's header naming a parent, a parent's spawn or
completion event, a transcript file under the parent's own directory, or a
session row's parent column. Directory proximity, a shared title, and a
timestamp coincidence create nothing. A reference the store cannot resolve is
kept with `resolved: false`, because a relative whose recording is gone is a
fact rather than a gap.

The view refers to a child and never absorbs one: resolving a reference reads
the child's header or row and never its turns, and a child's transcript is
read under its own session ID, or with `child PARENT --reference CHILD` for a Claude subagent.

`lineage --full` reads every record of the recording instead of its bounded
tail, so every reference the recording holds is named and `truncated` is
false. A store probe, such as the Codex search for child rollout headers,
keeps its own bound and says so in `notes`. OpenCode's relationships are read
from session rows, which no transcript bound reaches.

`lineage` carries an optional `parent`, an always-present `children` array,
and an optional `forked_from`. `ParentRef` is the parent's `native_id` in the
harness's own terms, whether a session with that id is in the store
(`resolved`), and the record that named it (`source`). Each `ChildRef`
carries:

| member | meaning |
|---|---|
| `reference` | how the parent's store names the child: its session id where the records carry one, otherwise the harness's own name for it, such as a Codex agent path |
| `session_id` | the child's session id, where the store holds a session under it; absent for a Claude subagent transcript, which is not addressable as a session |
| `harness` | the harness both sessions belong to |
| `role` | the role the parent asked for, in the harness's vocabulary: a Claude `agentType`, a Codex nickname or task name, an OpenCode agent |
| `model` | the model the records name for the child |
| `group` | the namespace a harness organizes children under, such as a Codex agent path's parent |
| `spawned_at`, `completed_at` | the spawn call's and the completion record's timestamps |
| `disposition` | the outcome the harness recorded, in its own words: a Claude `toolUseResult.status`, a Codex `agent_status` |
| `resolved` | whether the child's own recording is in the store |
| `source` | where the reference was read from: a `record` with its `native_id`, a `file` with its `path`, or a `session` with its `id` |

Every optional member is absent where the harness recorded nothing, and
children are ordered by the moment they were spawned, with a child whose spawn
was never recorded after them.

What each harness records:

| harness | recorded |
|---|---|
| Claude | subagent transcripts under `<session>/subagents/`, their meta records, and the `Agent` calls and `toolUseResult` records in the parent |
| Codex | `spawn_agent` and agent-status outputs in the parent, joined by agent path to the rollout headers whose `parent_thread_id` is this session; the header's own `parent_thread_id` and `forked_from_id` |
| OpenCode | the parent column on a session's row or object, in either direction |
| pi | the header's `parentSession`, on the child alone |

```json
{
  "schema": "tapes-lineage/2",
  "session": {
    "id": "session-1",
    "harness": "codex",
    "started_at": "2023-11-14T22:13:20Z",
    "last_activity_at": "2023-11-14T22:15:00Z"
  },
  "lineage": {
    "children": [
      {
        "reference": "/root/backend_workhorse",
        "session_id": "session-2",
        "harness": "codex",
        "role": "backend_workhorse",
        "group": "/root",
        "spawned_at": "2023-11-14T22:13:30Z",
        "completed_at": "2023-11-14T22:14:40Z",
        "disposition": "completed",
        "resolved": true,
        "source": [{ "kind": "record", "native_id": "call_1" }]
      }
    ]
  },
  "truncated": false
}
```

The view also carries the read's `truncated` flag, its `truncation` record,
and its `notes`, with the same meaning they have on a transcript: a bounded
read can leave a spawn record unread, and a store larger than the read's own
probe says so in a note.

`Session.source.location` is where `tapes` read the session from, as an opaque
locator and optional container member. It is separate from the native session
id and from the source revision in `read`; none of those fields carries
transcript text or proves that a relocated source is identical.

`TrailingRecord` identifies a verified final record after the newest rendered
turn when that record does not become a turn. It carries the source `kind` and
an optional UTC `timestamp`; the timestamp stays absent when the source record
does not provide one. `Transcript` omits `trailing_record` when no verified
trailing record is available. A message-only projection that does not expose a
record kind after its newest message, such as OpenCode's paged read, leaves the
field absent.

`Transcript` contains a session, its turns, a `truncated` flag, a `truncation`
record, an optional `trailing_record`, and optional reader-facing notes. The
flag is true when anything was omitted and the turns are only a window into the
transcript; `truncation` says what and why, and is omitted from JSON when
nothing was. Its `window`, present when the requested turn window dropped
turns, carries `returned`, `omitted`, `omitted_from` (always `head`, since a
window keeps the newest turns), the `bound` in force, and `omitted_exact`,
which is omitted when true; a larger `--tail` or `export` recovers what a
window omitted. `omitted_exact: false` marks a read that stopped fetching once
the window was full, as the paged OpenCode API read does: `omitted` then
counts only what was fetched, a wider request fetches older turns, and
ordinals count from the oldest turn that read reached rather than from the
session's start, so `native_id` is the stable reference for such a store. Its
`source` lists bounds the reader itself reached, each tagged by `kind`:
`file-tail` with the `bytes` read from the end of a recording file,
`record-page` with the newest `records` a paged store read fetched before its
ceiling (a message cap, or a message the transport cannot carry) and what they
are (`of`), and `turn-text` with how many `turns` carry text cut at `chars`
characters. How much lies beyond a source bound is unknown. `export` reads
with an unbounded turn window and still reports the same `source` entries.
Explicit `page` reads can reach older Claude and Codex file history, with
separate byte ranges and page-local ordinals. A content search whose bounded read reached
fewer turns than it was asked to search behind a source bound reports the
session as unsearched rather than as a non-match.
Notes preserve harness-specific facts that do not fit the normalized fields,
such as abandoned pi branches or the number of malformed lines skipped while
reading. They are prose rather than a structured API and are omitted from JSON
when empty. Both signals reach a human reader too: `show` closes a truncated
render with a note saying so, beside whatever notes the read produced. When a
trailing record is available, `show` names its kind and timestamp when one was
recorded.

## Stats view

`tapes stats` counts what one session's recording holds and serializes as a
`tapes-stats/5` object. Every figure is a count of records the harness wrote:
nothing here labels a call useful, attributes a reason to a latency, classifies
why a session ended, or recommends anything.

`session` is the identity the usage view carries. `coverage` says what the
figures are figures about: `turns` is `read-window` when a source bound
withheld whole turns and `session` otherwise, `pairs` is `complete-only`
because every duration comes from a call and result the read holds both halves
of, and `truncation` is the read's own record, absent when the read reached
everything.

`turns` counts the normalized turns the read reached by the `kind` the harness
recorded them as — `operator`, `assistant`, `tool`, `reasoning`, `control`,
`ambient`, `notice`, `unknown` — plus their `total`.

`tools` counts the same typed events `tapes events` returns. `calls` and
`results` count event records, `paired` counts the distinct complete pairs
among them, and `incomplete` splits the unpaired events by the boundary that
left them unpaired, in the keys the event layer names. `errors` counts calls
whose recorded outcome is an error, once per call however many halves of a
pair the harness wrote that status on. `by_name` holds one row per tool, calls
descending and then by name; a row's `duration_ms` covers the complete pairs
that carried both timestamps and `count` says how many those were, so a tool
whose pairs carried no timestamps has no `duration_ms` at all. A record whose
tool name neither it nor its counterpart carries is in the totals and in no
row, because the read holds no name to key one by.

`durations_ms` is present when at least one turn the read reached carried a
timestamp, and `count_with_timestamps` says how many did. `recorded_span` runs
from the first timestamped turn to the last and needs two of them;
`between_turns_max` is the longest interval between consecutive timestamped
turns; `in_tool` sums the complete pairs' durations. Each is absent when the
read holds nothing to measure it from.

`usage` repeats the session's own `tokens`, `cost`, and `accounting`, read
exactly as the usage view states them, and adds `cache_read_ratio` and
`cache_write_ratio`: the share of `input + cache_read + cache_write` that each
cache counter accounts for. A ratio is a ratio of recorded token counts and
never a share of cost, and it is present only when every counter in its
denominator is. Whether a harness's `input` already includes what it read from
the cache is that harness's own convention, so a ratio compares recordings of
one harness rather than of two. The whole object is absent for a session whose harness
recorded no counters.

`lineage` counts the children the store records for this session, from the
same reference read `tapes lineage` answers with and without opening a child:
`children`, how many `resolved` to a recording in the store, and
`by_disposition`, keyed by the outcomes the harness wrote. A child whose
outcome it did not write is in `children` and in no disposition. The object is
absent when the store records no child.

`warnings` names the limits of the read the figures came from, in a fixed
order:

| value | meaning |
|---|---|
| `read-window` | a source bound withheld whole turns, so the counts are the read's rather than the session's |
| `tail-window` | a turn window dropped turns the read had produced |
| `kind-unknown` | a user-envelope turn carries no evidence of what it is |
| `incomplete-pairs` | a call or result the read holds has no counterpart in it |
| `no-timestamps` | a turn the read reached carries no timestamp, so the clock covers fewer turns than the counts do |

```json
{
  "schema": "tapes-stats/5",
  "session": {
    "id": "session-1",
    "harness": "codex",
    "model": { "id": "gpt-5.6-sol", "variant": "high" },
    "started_at": "2023-11-14T22:13:20Z",
    "last_activity_at": "2023-11-14T22:15:00Z"
  },
  "coverage": { "turns": "session", "pairs": "complete-only" },
  "turns": {
    "operator": 1,
    "assistant": 1,
    "tool": 4,
    "reasoning": 1,
    "control": 0,
    "ambient": 0,
    "notice": 0,
    "unknown": 0,
    "total": 7
  },
  "tools": {
    "calls": 2,
    "results": 2,
    "paired": 2,
    "incomplete": {
      "no-result-in-read": 0,
      "call-before-read-bound": 0,
      "call-not-recorded": 0
    },
    "by_name": [
      {
        "name": "exec",
        "calls": 2,
        "paired": 2,
        "errors": 1,
        "duration_ms": { "total": 5312, "max": 4312, "count": 2 }
      }
    ],
    "errors": 1
  },
  "durations_ms": {
    "recorded_span": 100000,
    "in_tool": 5312,
    "between_turns_max": 40000,
    "count_with_timestamps": 7
  },
  "usage": {
    "tokens": { "input": 2500, "output": 400, "cache_read": 1000, "cache_write": 500 },
    "accounting": { "basis": "recorded-total", "coverage": "session" },
    "cache_read_ratio": 0.25,
    "cache_write_ratio": 0.125
  },
  "lineage": { "children": 1, "resolved": 1, "by_disposition": { "completed": 1 } },
  "warnings": []
}
```

## Usage summary

`tapes usage` over a selection sums what that set of sessions spent and
serializes as a `tapes-usage-summary/3` object. The counters come from the
listing, so the summed set is exactly the set `list` returns for the same
flags and no transcript is read.

`selection` restates the query that chose the set, with the members and
meanings the export manifest's `selection` carries. The listing's
`unavailable`, `unreadable`, `unsearched`, `scanned`, and `scan_truncated`
are carried verbatim, so a total can be audited against the store it came
from.

`groups` holds one entry per distinct combination of the requested
dimensions: `harness`, `model` (the model id), `variant` (what qualifies that
id, such as a reasoning effort), and `directory`. A group's `key` carries
only the requested dimensions, and a dimension the sessions did not record is
absent from the key rather than empty. Groups are ordered by their key values
ascending, in the order the dimensions were requested; an absent value sorts
first. `totals` has a group's shape without its key and covers every selected
session.

Within a compatible group or partition, `tokens.<counter>` is the sum over the sessions
that recorded that counter and `counted.<counter>` is how many those were, so
a total over twelve sessions of which nine recorded reasoning tokens is not
read as twelve. A counter no session recorded is absent rather than zero, and
`tokens` itself is absent when no counter was recorded at all. `cost.usd`
sums only recorded costs, with `counted.cost` behind it; nothing is inferred
from tokens, and a group with no recorded cost omits `cost`.

`coverage` counts the sessions behind a sum by their accounting:
`recorded_total` for a cumulative figure, `summed_session` and
`summed_read_window` for a sum of per-request records by how much of its
session it covers, and `no_accounting` for a session whose harness recorded
no counters at all. A provider quota is not part of this view: it is an
account-wide fact, not a sum over sessions.

```json
{
  "schema": "tapes-usage-summary/3",
  "selection": {
    "scope": "global",
    "sort": "newest",
    "limit": 20
  },
  "groups": [
    {
      "key": {
        "harness": "codex",
        "model": "fixture-model"
      },
      "sessions": 2,
      "tokens": {
        "input": 30
      },
      "coverage": {
        "recorded_total": 2,
        "summed_session": 0,
        "summed_read_window": 0,
        "no_accounting": 0
      },
      "counted": {
        "input": 2,
        "output": 0,
        "reasoning": 0,
        "cache_read": 0,
        "cache_write": 0,
        "cost": 0
      }
    }
  ],
  "totals": {
    "sessions": 2,
    "tokens": {
      "input": 30
    },
    "coverage": {
      "recorded_total": 2,
      "summed_session": 0,
      "summed_read_window": 0,
      "no_accounting": 0
    },
    "counted": {
      "input": 2,
      "output": 0,
      "reasoning": 0,
      "cache_read": 0,
      "cache_write": 0,
      "cost": 0
    }
  },
  "partitions": [
    {
      "harness": "codex",
      "accounting": {
        "basis": "recorded-total",
        "coverage": "session"
      },
      "sessions": 2,
      "tokens": {
        "input": 30
      },
      "coverage": {
        "recorded_total": 2,
        "summed_session": 0,
        "summed_read_window": 0,
        "no_accounting": 0
      },
      "counted": {
        "input": 2,
        "output": 0,
        "reasoning": 0,
        "cache_read": 0,
        "cache_write": 0,
        "cost": 0
      }
    }
  ],
  "unavailable": [],
  "unreadable": [],
  "unsearched": [],
  "scanned": 2,
  "scan_truncated": false
}
```

### Accounting partitions in usage summary version 2

`partitions` groups sessions by harness and the optional accounting basis and
coverage. Each row carries the ordinary tally and counter-contribution counts.
A total or requested group containing counters from multiple such domains sets
`mixed_accounting: true` and omits `tokens` and `cost`. Sessions with no counters
retain their counts without making otherwise compatible sums incomparable.
Version 1 summed across domains; version 2 retains comparable partition sums
and suppresses mixed sums. Unknown accounting is its own domain.


## Endings report

`tapes endings` answers what each session of a selection ends on and
serializes as a `tapes-endings/6` object. The selection is stated in the terms
`list` uses, so the reported set is exactly the set `list` returns for the same
flags, and the scope and metadata filters apply before any transcript is
opened. Each selected session then costs one bounded transcript read of
`--tail` turns and one lineage read; a session whose read fails is named in
`unread` with its diagnostic and does not stop the run.

`selection` restates the query that chose the set, with the members and
meanings the export manifest's `selection` carries, and the listing's
`unavailable`, `unreadable`, `unsearched`, `scanned`, and `scan_truncated` are
carried verbatim.

Each ending carries the session identity, the coordinate to write down for it,
the turns the report names, the facts the read establishes, and what it left
unestablished. `source` is that coordinate: the source descriptor, the session
id, the last read turn's `ts` — or the session's `last_activity_at` where that
turn carries none — its `turn` ordinal, `native_id`, and `record_ref`, this schema, and
`coverage`, which is `read-window` when a source bound withheld turns,
`window` when only the turn window omitted any, and `session` otherwise. It
holds no transcript text.

`facts` names what the read establishes, each from the normalized kinds and
typed tool events of the turns that were read and never from their text:

| fact | established by |
|---|---|
| `operator-turn-after-assistant` | the newest `operator` turn is later than the newest `assistant` turn, or the read holds an operator turn and no assistant turn at all |
| `control-turn-last` | the last turn is `control`, a harness command such as `/exit`; what the recording ends on is decided by the turns before it |
| `notice-turn-last` | the last turn is `notice`, a message the harness injected; the turns before it decide the ending in the same way |
| `call-without-result` | a `tool-call` in the read carries `no-result-in-read` and no `tool-result` in the read follows it |
| `results-without-narration` | the newest turn is a paired `tool-result`, so results landed and no assistant turn narrates them |
| `assistant-close` | the newest turn that is neither `control` nor `notice` is an `assistant` turn |

More than one can hold at once, and they are listed in the order of the table.
The report classifies nothing beyond them: it infers no reason for an ending
and labels no session complete.

`incomplete` qualifies every fact beside it:

| value | meaning |
|---|---|
| `read-window` | a `file-tail` or `record-page` source bound withheld turns, so the recording continues beyond this read |
| `tail-window` | the `--tail` window omitted turns the read produced; a wider window recovers them |
| `kind-unknown` | a user turn in the read is `unknown`, so what it holds is not established |
| `no-timestamps` | a turn an ordering depended on carries no timestamp, so the order rests on the normalized sequence alone |

`last_turn` names the newest turn's `role`, `kind`, `ordinal`, and `ts`, and is
absent when the read reached no turn. `last_operator` and `last_assistant`
name where those turns sit. `lineage` is present when the session's store
records a relative: the `parent` reference the lineage view carries, the
`children` count, how many of them are unresolved, and
`children_by_disposition`, one count per outcome the harness recorded. No
child is read; its own ending is read under its own session ID or through the parent-qualified `child` command.

`tail` is present only when the bounded text tail was asked for. It holds at
most `--tail` entries, one per `operator` or `assistant` turn in the read, each
with the turn's `ordinal`, `kind`, `role`, `ts`, its `text` cut at 400
characters, and whether that cut happened. The harness's own commands,
notices, and attached context stay out of it. Each ending also carries the
read's `truncated` flag, its `truncation` record, its `notes`, and any
verified `trailing_record`, with the meanings they have on a transcript.

```json
{
  "schema": "tapes-endings/6",
  "selection": { "scope": "global", "sort": "newest", "limit": 20 },
  "endings": [
    {
      "session": {
        "id": "session-1",
        "harness": "codex",
        "model": { "id": "gpt-5.6-sol", "variant": "high" },
        "last_activity_at": "2026-01-01T10:00:07Z"
      },
      "source": {
        "harness": "codex",
        "session": "session-1",
        "ts": "2026-01-01T10:00:06Z",
        "turn": 41,
        "native_id": "msg_1",
        "schema": "tapes-endings/6",
        "coverage": "window"
      },
      "last_turn": {
        "role": "tool",
        "kind": "tool",
        "ordinal": 41,
        "ts": "2026-01-01T10:00:06Z"
      },
      "last_operator": { "ordinal": 30, "ts": "2026-01-01T10:00:02Z" },
      "last_assistant": { "ordinal": 36, "ts": "2026-01-01T10:00:04Z" },
      "facts": ["results-without-narration"],
      "incomplete": ["tail-window"],
      "lineage": { "children": 2, "children_unresolved": 0, "children_by_disposition": { "completed": 2 } },
      "truncated": true
    }
  ],
  "unread": [
    { "id": "session-2", "harness": "codex", "error": "the recording is gone" }
  ],
  "unavailable": [],
  "unreadable": [],
  "unsearched": [],
  "scanned": 2,
  "scan_truncated": false
}
```

## Continuation brief

`tapes brief` answers what a continuation of one session needs from its
recording and serializes as a `tapes-brief/6` object. The session is named by
id or reached with `--latest`, and costs one export-shaped transcript read and
one lineage read: pairing therefore sees every call and result the reader
reached, while `--tail` bounds the rendered exchange alone.

The brief reads the recording and nothing else. It opens no journal, asks no
project tool, judges no ending, and resumes nothing; joining it with whatever a
project records about the same work is the caller's.

`session` carries the identity the usage view carries — `id`, `source`,
`model`, optional `started_at`, optional `last_activity_at`, and `directory` — plus the
recorded `title` or the derived `derived_title` hint, and `live` when a status
authority answered. `source` is the coordinate to write down for this reading,
emitted as the endings report emits it, `schema` included.

`working_set` is where the session worked: the recorded `directory`, the
`git` head and branch read from that directory when it is a repository, and
`directory_exists`, which says whether that directory is a directory on this
machine and is `false` when the recording names none. Nothing here reports
uncommitted state; what is dirty now is present-tense and the caller reads it
itself.

`ending` is the endings report's record for the same read: `last_turn`,
`last_operator`, `last_assistant`, `facts`, `incomplete`, and any verified
`trailing_record`, each with the meaning it has there.

`in_flight` holds the handles a continuation reattaches to, each bounded to 20
entries with a note when the bound cut the list:

| member | holds |
|---|---|
| `calls_without_result` | every tool call the read paired no result to, newest first, with its `ordinal`, `name`, `call_id`, `ts`, and bounded `arguments` |
| `children` | every child the store records no outcome for — neither a `completed_at` nor a `disposition` — and every child whose own recording the store cannot resolve, in the order the store records them as spawned |

A call listed there may have left side effects, and a child listed there may
still be running: the recording says only that nothing answered it.

`tail` holds the newest `--tail` `operator` and `assistant` turns of the read,
oldest first, each with the turn's `ordinal`, `kind`, `role`, `ts`, its `text`
cut at 600 characters, and whether that cut happened. The harness's own
commands, notices, and attached context stay out of it, as do reasoning and
tool turns.

`usage` is present when the session recorded a token count or a cost, and
carries those counters with the `accounting` that says what they cover. The
brief also carries the read's `truncated` flag, its `truncation` record, and
its `notes`, with the meanings they have on a transcript.

```json
{
  "schema": "tapes-brief/6",
  "session": {
    "id": "session-1",
    "harness": "codex",
    "model": { "id": "gpt-5.6-sol", "variant": "high" },
    "started_at": "2026-01-01T10:00:00Z",
    "last_activity_at": "2026-01-01T10:00:06Z",
    "directory": "/projects/tapes"
  },
  "source": {
    "harness": "codex",
    "session": "session-1",
    "ts": "2026-01-01T10:00:06Z",
    "turn": 41,
    "schema": "tapes-endings/6",
    "coverage": "session"
  },
  "working_set": {
    "directory": "/projects/tapes",
    "git": { "head": "0f2c1d9", "branch": "topic" },
    "directory_exists": true
  },
  "ending": {
    "last_turn": { "role": "tool", "kind": "tool", "ordinal": 41, "ts": "2026-01-01T10:00:06Z" },
    "last_operator": { "ordinal": 30, "ts": "2026-01-01T10:00:02Z" },
    "last_assistant": { "ordinal": 36, "ts": "2026-01-01T10:00:04Z" },
    "facts": ["call-without-result"],
    "incomplete": []
  },
  "in_flight": {
    "calls_without_result": [
      {
        "ordinal": 41,
        "name": "apply_patch",
        "call_id": "call-1",
        "ts": "2026-01-01T10:00:06Z",
        "arguments": { "chars": 812, "preview": "*** Begin Patch" }
      }
    ],
    "children": [
      { "reference": "/root/worker", "role": "worker", "resolved": true, "spawned_at": "2026-01-01T10:00:05Z" }
    ]
  },
  "tail": [
    {
      "ordinal": 36,
      "kind": "assistant",
      "role": "assistant",
      "ts": "2026-01-01T10:00:04Z",
      "text": "Applying the patch.",
      "truncated": false
    }
  ],
  "usage": {
    "tokens": { "input": 1200, "output": 300 },
    "accounting": { "basis": "recorded-total", "coverage": "session" }
  },
  "truncated": false
}
```

## JSON contract

A serialized transcript is a `tapes-session/9` object:

```json
{
  "schema": "tapes-session/9",
  "session": {
    "id": "session-1",
    "source": {
      "kind": "installed-recording",
      "origin": "codex",
      "recorded_harness": "codex",
      "representation": "codex-recording"
    },
    "started_at": "2023-11-14T22:13:20Z",
    "last_activity_at": "2023-11-14T22:15:00Z"
  },
  "turns": [],
  "truncated": false
}
```

When a supplied mapping export carries branches, `graph` retains bounded
nodes and parent-child edges, names the `current_node`, and records the
`selected_path` projected into `turns`. A missing current node leaves the
graph available while the canonical turn projection remains empty. The
top-level `artifacts` collection retains artifact-native reports even when no
turn can carry them; a report reference may include a bounded `body` and
structured citation spans, and its optional `backing` remains the source's
association rather than a fabricated turn.

When the source reader supplies it, `read` records the source length and the
configured byte bound, each physical head/tail/context/alignment range, the
absolute spans of decoded records, separate spans for decoded records used
only as projection context, and explicit gaps for bytes outside the bound,
partial records, or malformed records. A range is an observation of
this read, not a content digest or a portable source identity. Physical
coverage does not erase a malformed record gap; a partial gap is discharged
only when a successful decode identifies that same record. A terminal
observation is independent of normalized turns: it preserves the native record
type, payload subtype, explicit turn identity, outcome/code/message, and
duration only where the reached record supplies them. Its presence never says
that the session is stopped now. `text_tail` reports the requested and
returned text entries and explains an empty tail as a zero request, no
operator/assistant text in the read, or an empty complete projection.

`show --full --json` streams a whole Claude, Codex, Pi, or OpenCode session,
and its `read` says so. For a recording file that is one `tail` range from
byte 0 to the length observed when the read opened, `configured_bound` equal to that length, `projection_options`
holding `full`, and gaps for malformed records or records longer than 64 MiB.
Record spans are not listed under `read.records`; each turn's `record_ref`
carries its own span and the source revision it was read at. An OpenCode
session is read whole from its store rather than a file, so its `read` counts
message rows: `coordinate_domain` is `opencode-message`, the range runs from 0
to the number of messages streamed, and its turns carry `native_id` without a
`record_ref`. No `text_tail` is reported, and `truncation` holds only a
`--tail` window and the `turn-text` bounds of part text the OpenCode database
projection cut.

`projection` is present when a caller kept turns by kind with `--only`,
`--omit`, or `show --exchange`. `kept` lists the kept kinds in the order
`operator`, `assistant`, `reasoning`, `tool`, `control`, `ambient`, `notice`,
`unknown`, and `omitted` counts every dropped turn by kind; `show --exchange`
writes `{"kept": ["operator", "assistant"], "omitted": {"reasoning": 1,
"tool": 3}}`. Kept turns keep their ordinals. A turn window under a projection
counts the kept turns. A turn absent from a projection says nothing about
whether the session recorded it.

A bundle's `.context.md` holds the exchange that `show --exchange` returns —
the `operator` and `assistant` turns — of those a selection keeps; its `.json`
and `.trace.md` hold every kept turn.

A transcript that omitted anything carries `truncated: true` and a
`truncation` object:

```json
{
  "truncated": true,
  "truncation": {
    "window": { "returned": 100, "omitted": 47, "omitted_from": "head", "bound": 100 },
    "source": [
      { "kind": "file-tail", "bytes": 4194304 },
      { "kind": "record-page", "records": 1000, "of": "messages" },
      { "kind": "turn-text", "turns": 3, "chars": 4000 },
      { "kind": "input-coverage", "gaps": 1 }
    ]
  }
}
```

The window names the ordinals it holds (`"ordinals": { "first": 47, "last":
146 }` for the case above), and every turn carries its own:

```json
{ "role": "user", "kind": "operator", "text": "…", "ts": "2023-11-14T22:13:20Z", "ordinal": 47, "native_id": "msg_1", "record_ref": { "domain": "file:/store/rollout.jsonl", "revision": "stat:…", "span": { "start": 120, "end": 260 }, "native_id": "msg_1", "part_index": 0 } }
```

`record_ref` is the shared source coordinate used by turns, tool events,
pair references, endings, and bundle traces. JSONL spans are absolute file
offsets and remain stable across overlapping page sizes while the source
revision is unchanged. A native identifier without a source domain is not a
portable identity; a display ordinal is never promoted to one. Git context in
an export is observed working-directory context and does not alter these
record references.

An `input-coverage` source bound means the supplied reader reached one or more
structural, member, or aggregate gaps. Selected-member byte spans remain in
`read.gaps`; collection diagnostics in `notes` identify other member failures
without assigning their offsets to this source. An exact supplied occurrence can still return the known
projection, while ID and title lookup require complete-enough discovery.

An optional field means that the source harness does not record that fact.
Absent values are omitted from JSON rather than emitted as `null`, empty
strings, empty note arrays, or invented defaults. This distinction applies to
session metadata, model variants, every token counter, turn timestamps, and
transcript notes. `derived_title` is present only when the bounded first user
turn produced a useful hint. `trailing_record` and its `timestamp` are omitted
when their source facts are unavailable. JSON timestamps retain their recorded
precision; human list, show, and Markdown renderings use RFC 3339 whole seconds
with a `Z` suffix. The human `show` activity note compares the store's last
activity with the newest rendered turn after both timestamps are truncated to
whole seconds.

A serialized list is a `tapes-list/5` object with `sessions`, optional
artifact-native `artifacts`, `unavailable`,
`unreadable`, `unsearched`, `scanned`, and `scan_truncated`. `unsearched` names
bounded content-search failures and supplied-input structural diagnostics; its
entries identify the session, source member, or search stage without turning an
unreached record into a not-found claim. It also carries a failed OpenCode2
local API stage when CLI API reads continue. It is distinct from
`unreadable`, which describes a session that could not be normalized at all.
A scoped listing (`--here`, `--project`) excludes a candidate whose recorded
directory no longer exists, because nothing left on disk proves its
repository; when it excludes any, the optional `unplaced` object reports
`directories`, the count of distinct such directories among the inspected
candidates, and `examples`, the first eight in path order. `--global` with
`--directory <substring>` reaches those sessions.
`list --search` inspects the last 32 normalized turns per candidate before the
per-harness result limit. The object always includes `sort`, either `newest` or
`oldest`, and includes `activity` only when an activity bound was requested:
its optional `since` and `until` members are UTC timestamps. The activity
window matches `last_activity_at` with `since <= last_activity_at < until` and
is applied before the per-harness result limit. The sort is applied before
that limit too: `newest` keeps each harness's newest matching sessions and
`oldest` its oldest, inspecting every candidate the scan reaches. Sort ties use
session id ascending, then harness ascending.

```json
{
  "schema": "tapes-list/5",
  "sort": "newest",
  "activity": {
    "since": "2026-01-01T00:00:00Z",
    "until": "2026-01-02T00:00:00Z"
  },
  "sessions": []
}
```

An `export` over a selection writes a `tapes-export-manifest/5` object beside
the bundles it produced. `selection` restates the query that chose the set:
`scope` is `here`, `project`, or `global`, `project` names the path whose
project was selected for the first two, `sort` and `limit` are always present,
and `harness`, `model`, `directory`, `activity`, and `search` appear only when
they were requested. `sessions` is in selection order and names each bundle's
three files. Supplied-input artifact-native reports appear in the manifest's
optional `artifacts` collection. A selected session whose store could not be read appears in
`failed` with its diagnostic instead. The listing's own `unavailable`,
`unreadable`, `unsearched`, `scanned`, and `scan_truncated` are carried
verbatim, so the exported set can be audited against the store it came from.

```json
{
  "schema": "tapes-export-manifest/5",
  "selection": {
    "scope": "global",
    "activity": { "since": "2026-01-01T00:00:00Z" },
    "sort": "newest",
    "limit": 20
  },
  "sessions": [
    {
      "id": "session-1",
      "harness": "codex",
      "last_activity_at": "2026-01-01T10:00:00Z",
      "files": {
        "context": "/tmp/20260101T120000Z-codex-session-1.context.md",
        "json": "/tmp/20260101T120000Z-codex-session-1.json",
        "trace": "/tmp/20260101T120000Z-codex-session-1.trace.md"
      }
    }
  ],
  "failed": [
    { "id": "session-2", "harness": "codex", "error": "the recording is gone" }
  ],
  "unavailable": [],
  "unreadable": [],
  "unsearched": [],
  "scanned": 2,
  "scan_truncated": false
}
```

## Historical evidence

`tapes-page/4` carries a normalized session, chronological `turns` with page-local
ordinals, recorded `models`, source `start`/`end` byte offsets, `source_bytes`,
`bytes_read`, separately counted `alignment_bytes` and `context_bytes`, malformed
`skipped_records`, `skipped_fragment_bytes`, and an
`read` evidence with absolute record spans and separate context-only record
spans, and an
optional `next_cursor`. A missing cursor means the source beginning was reached,
not that malformed or oversized records were decoded. The cursor is opaque;
it binds the session and file snapshot and must be passed back unchanged.

`tapes-history-search/4` carries session identity, accumulated pages/bytes/gaps
and one `read` descriptor per page,
matching text excerpts identified by page start, end and ordinal, an output-truncation
flag, and a continuation cursor. `tapes-metadata-history/4` carries the same
coverage facts with up to 100 reverse-record-ordered model observations and
an observation-truncation flag. Neither schema infers facts outside its reads.
Metadata traversal does not normalize transcript turns or read their
provenance context; each metadata page labels its read as the `tapes-page/4`
envelope with the `models-only` projection option, and its `context_bytes` is
zero for Claude and Codex. Initial session resolution is separate from the
page-byte counters.
## Selection statistics: `tapes-stats-summary/3`

`selection` records the listing query. `selected` counts its sessions, `read`
counts successful transcript reads, and `failed` names each read failure with
ID, harness, source and diagnostic. `sessions` holds each read session's identity,
`coverage` and `tools` in the same shapes as `tapes-stats/5`. `by_harness` maps
harness names to accumulated tool counters, including tool-name rows and
complete-pair duration totals, maxima and contributing counts. Counters never
cross-pair records from different sessions. Listing diagnostics (`unavailable`,
`unreadable`, `unsearched`, `scanned`, `scan_truncated`) remain separate from
transcript failures and per-session read bounds. An all-failed selected set
still emits the report and exits unsuccessfully.

## Child-qualified read: `tapes-child/3`

`parent` is the selected parent session and `reference` is the exact child
reference. `transcript`, `usage`, and `ending` use their normalized shapes with
the qualified child ID `PARENT::CHILD`. Transcript window coverage is separate
from the source coverage underlying usage and ending facts. Every native Claude session ID observed in the bounded opening and tail must
name the parent; missing or conflicting identity evidence is refused. Source
bounds still limit which native records were validated.
Nested lineage is explicitly unavailable rather than an asserted empty set.

`child --full` streams the child's whole recording. Every record carrying a
native session ID must name the parent; `usage` counts every turn and folds its
counters from every record; `transcript` and `ending` hold only the newest
`--tail` turns, with `transcript.truncation.window` counting the rest and the
ending's coverage `window` when there are any; and `read` states
whole-recording coverage.
