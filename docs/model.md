# Session model

Every harness backend translates its records into the same `tapes-core`
types. Consumers can therefore list sessions and read transcripts without
knowing which harness stored them.

## Types

`Session` identifies the harness and session, records its first and latest
activity timestamps, and may carry a model, recorded title, derived title,
working directory, present live state, cost, token counts, and their accounting
metadata. Both timestamps are required UTC values. A record that cannot supply
timestamps is not a session and is omitted from listings.

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
and a file with no timestamp anywhere in either window is not a session.

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
turn's tool event it is skipped by serialization, so `tapes-session/1` and
export bundles are unaffected and the usage view is where it reaches a
consumer.

`Turn` contains a role, a `kind`, text, an optional UTC timestamp, an
`ordinal`, and an optional `native_id`. Roles are `user`, `assistant`, `tool`,
and `reasoning`.
A tool turn also carries one typed `ToolEvent` inside the process for the
`events` projection. The field is skipped by serialization, so
`tapes-session/1` and export bundles retain the tool's harness envelope only in
`text`.
The ordinal is the turn's zero-based position in the session's normalized turn
sequence, counted from the first turn the reader reaches. For a file-backed
session the reader's reach is the file's last 4 MiB whatever the window, so
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

Every value rests on a field the harness itself wrote; nothing is inferred
from the text, so `unknown` is the answer for a record whose harness version
wrote no such field:

| harness | evidence |
|---|---|
| Claude | `origin.kind` and `promptSource` name the sender; `isMeta` marks text the harness attached; on a record carrying none of the three, content that is exactly a `<command-name>` envelope or a `<local-command-stdout>` element is the harness's own command. |
| Codex | A `user_message` event carries the text of each message the operator sent, so a user message the event vouches for is theirs; a message holding only the blocks the harness wraps around a message is attached context. On an `exec` session, whose header names that source and which records no such event, the wrapper blocks are the only separation. |
| pi, OpenCode | Neither records anything but the operator's messages in its user role. |

## Tool event layer

`ToolEvent` is the harness-neutral representation attached while a backend is
already parsing a tool turn. `kind` is `tool-call` or `tool-result`; `subtype`
keeps the harness's record kind; and optional `name`, `call_id`, and `status`
keep only facts the record supplies. Call arguments and result output use a
`Bounded` value with the payload's Unicode character count and its first 200
characters. A string is measured as written, while an object or array is first
serialized as compact JSON. The preview always ends on a character boundary.

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

`tapes events` serializes the projection as `tapes-events/1`. The object holds
the same `Session` representation as `show`, the event records, complete and
incomplete pair counts, and the transcript's truncation and notes. A
`--tail N` window keeps events whose turn ordinals are in the final `N` turns;
the window metadata therefore uses the same turn coordinates as `show` rather
than counting event records. Name and call-id filters apply after pairing.
`pairs.complete` counts distinct complete pairs represented by at least one
returned event, while `pairs.incomplete` counts returned events carrying an
`incomplete` reason. Both describe the read: a file-backed or database read
pairs the whole bounded read before the window applies, while the OpenCode API
read fetches only the pages the window needs, so its counts belong to that
window's read and a wider `--tail` can pair more.

```json
{
  "schema": "tapes-events/1",
  "session": {
    "id": "session-1",
    "harness": "codex",
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
`tapes-usage/1` object. `tokens`, `cost`, and `accounting` are the session's
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

The remaining objects are present exactly when the harness recorded them:

| member | source |
|---|---|
| `context_window` | Codex `info.model_context_window` |
| `rate_limits` | Codex `rate_limits`: optional `primary` and `secondary` windows with `used_percent`, `window_minutes`, and an RFC 3339 `resets_at`, and the account `plan` |
| `durations_ms` | Claude `cost-state` wall clock: `api`, `api_without_retries`, `tool`, `total` |
| `by_model` | Claude `cost-state` `modelUsage`, one entry per model with its `tokens` and `cost`, ordered by model id |

pi and OpenCode record none of them, and each stays absent rather than empty
or null.

```json
{
  "schema": "tapes-usage/1",
  "session": {
    "id": "session-1",
    "harness": "codex",
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
    "plan": "plus"
  },
  "truncated": false
}
```

The view also carries the read's `truncated` flag, its `truncation` record,
and its `notes`, with the same meaning they have on a transcript.

## Lineage view

`tapes lineage` answers which sessions a recording names as its relatives and
serializes as a `tapes-lineage/1` object. A relationship exists only where a
record states it: a child's header naming a parent, a parent's spawn or
completion event, a transcript file under the parent's own directory, or a
session row's parent column. Directory proximity, a shared title, and a
timestamp coincidence create nothing. A reference the store cannot resolve is
kept with `resolved: false`, because a relative whose recording is gone is a
fact rather than a gap.

The view refers to a child and never absorbs one: resolving a reference reads
the child's header or row and never its turns, and a child's transcript is
read with `show` under its own id.

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
  "schema": "tapes-lineage/1",
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

`Session.store` is where `tapes` read the session from, as an opaque string: a
recording file's path for the file-backed harnesses, the database file for
OpenCode's stable store, and the program and endpoint for the OpenCode API.
Together with the harness and the native session id it is the coordinate a
consumer writes down; none of the three carries transcript text.

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
characters. How much lies beyond a source bound is unknown, and no request
through `tapes` passes it: `export` reads with an unbounded window and still
reports the same `source` entries. A content search whose bounded read reached
fewer turns than it was asked to search behind a source bound reports the
session as unsearched rather than as a non-match.
Notes preserve harness-specific facts that do not fit the normalized fields,
such as abandoned pi branches or the number of malformed lines skipped while
reading. They are prose rather than a structured API and are omitted from JSON
when empty. Both signals reach a human reader too: `show` closes a truncated
render with a note saying so, beside whatever notes the read produced. When a
trailing record is available, `show` names its kind and timestamp when one was
recorded.

## Usage summary

`tapes usage` over a selection sums what that set of sessions spent and
serializes as a `tapes-usage-summary/1` object. The counters come from the
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

Within a group or the totals, `tokens.<counter>` is the sum over the sessions
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
  "schema": "tapes-usage-summary/1",
  "selection": { "scope": "global", "sort": "newest", "limit": 20 },
  "groups": [
    {
      "key": { "harness": "codex", "model": "gpt-5.6-sol", "variant": "high" },
      "sessions": 12,
      "tokens": { "input": 120000, "output": 8000, "reasoning": 4000 },
      "cost": { "usd": 1.23 },
      "coverage": {
        "recorded_total": 10,
        "summed_session": 1,
        "summed_read_window": 1,
        "no_accounting": 0
      },
      "counted": {
        "input": 12,
        "output": 12,
        "reasoning": 9,
        "cache_read": 12,
        "cache_write": 12,
        "cost": 12
      }
    }
  ],
  "totals": {
    "sessions": 12,
    "tokens": { "input": 120000, "output": 8000, "reasoning": 4000 },
    "cost": { "usd": 1.23 },
    "coverage": {
      "recorded_total": 10,
      "summed_session": 1,
      "summed_read_window": 1,
      "no_accounting": 0
    },
    "counted": {
      "input": 12,
      "output": 12,
      "reasoning": 9,
      "cache_read": 12,
      "cache_write": 12,
      "cost": 12
    }
  },
  "unavailable": [],
  "unreadable": [],
  "unsearched": [],
  "scanned": 12,
  "scan_truncated": false
}
```

## JSON contract

A serialized transcript is a `tapes-session/1` object:

```json
{
  "schema": "tapes-session/1",
  "session": {
    "id": "session-1",
    "harness": "codex",
    "started_at": "2023-11-14T22:13:20Z",
    "last_activity_at": "2023-11-14T22:15:00Z"
  },
  "turns": [],
  "truncated": false
}
```

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
      { "kind": "turn-text", "turns": 3, "chars": 4000 }
    ]
  }
}
```

The window names the ordinals it holds (`"ordinals": { "first": 47, "last":
146 }` for the case above), and every turn carries its own:

```json
{ "role": "user", "kind": "operator", "text": "…", "ts": "2023-11-14T22:13:20Z", "ordinal": 47, "native_id": "msg_1" }
```

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

A serialized list is a `tapes-list/1` object with `sessions`, `unavailable`,
`unreadable`, `unsearched`, `scanned`, and `scan_truncated`. `unsearched` is
populated only for a requested content search when a candidate's bounded read
fails or a search stage falls back; its entries name the session or search
stage and diagnostic, including a failed OpenCode2 local API server start or
candidate listing when CLI API reads continue. It is distinct from
`unreadable`, which describes a session that could not be normalized at all.
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
  "schema": "tapes-list/1",
  "sort": "newest",
  "activity": {
    "since": "2026-01-01T00:00:00Z",
    "until": "2026-01-02T00:00:00Z"
  },
  "sessions": []
}
```

An `export` over a selection writes a `tapes-export-manifest/1` object beside
the bundles it produced. `selection` restates the query that chose the set:
`scope` is `here`, `project`, or `global`, `project` names the path whose
project was selected for the first two, `sort` and `limit` are always present,
and `harness`, `model`, `directory`, `activity`, and `search` appear only when
they were requested. `sessions` is in selection order and names each bundle's
three files. A selected session whose store could not be read appears in
`failed` with its diagnostic instead. The listing's own `unavailable`,
`unreadable`, `unsearched`, `scanned`, and `scan_truncated` are carried
verbatim, so the exported set can be audited against the store it came from.

```json
{
  "schema": "tapes-export-manifest/1",
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
