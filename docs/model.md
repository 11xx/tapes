# Session model

Every harness backend translates its records into the same `tapes-core`
types. Consumers can therefore list sessions and read transcripts without
knowing which harness stored them.

## Types

`Session` identifies the harness and session, records its first and latest
activity timestamps, and may carry a model, recorded title, derived title,
working directory, present live state, cost, and token counts. Both timestamps
are required UTC values. A record that cannot supply timestamps is not a
session and is omitted from listings.

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
truncation marker separately. File-backed readers take the first user turn
from the file's opening, which is read even when the transcript is larger than
the bounded tail, so the hint names the session's actual first turn; a reader
that cannot see the first user turn leaves the hint absent rather than labeling
a later turn as the first. API-backed metadata readers also leave it absent
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
recorded nothing: OpenCode supplies all five from its session row, Codex
supplies the cumulative totals of the newest `token_count` event in the
bounded read (so a session's counters are its running total, not the
window's), and Claude and pi record no session-level counters. Cost, provider
quota, and recorded tokens are distinct facts; no counter is derived from
another.

`Turn` contains a role, text, an optional UTC timestamp, an `ordinal`, and an
optional `native_id`. Roles are `user`, `assistant`, `tool`, and `reasoning`.
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

`Session.store` is where `tapes` read the session from, as an opaque string: a
recording file's path for the file-backed harnesses, the database file for
OpenCode's stable store, and the program and endpoint for the OpenCode API.
Together with the harness and the native session id it is the coordinate a
consumer writes down; none of the three carries transcript text.

`TrailingRecord` identifies a verified final record after the newest rendered
turn when that record does not become a turn. It carries the source `kind` and
an optional UTC `timestamp`; the timestamp stays absent when the source record
does not provide one. `Transcript` omits `trailing_record` when no verified
trailing record is available.

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
{ "role": "user", "text": "…", "ts": "2023-11-14T22:13:20Z", "ordinal": 47, "native_id": "msg_1" }
```

An optional field means that the source harness does not record that fact.
Absent values are omitted from JSON rather than emitted as `null`, empty
strings, empty note arrays, or invented defaults. This distinction applies to
session metadata, model variants, every token counter, turn timestamps, and
transcript notes. `derived_title` is present only when the bounded first user
turn produced a useful hint. `trailing_record` and its `timestamp` are omitted
when their source facts are unavailable. JSON timestamps retain their recorded
precision; human list, show, and Markdown renderings use RFC 3339 whole seconds
with a `Z` suffix.

A serialized list is a `tapes-list/1` object with `sessions`, `unavailable`,
`unreadable`, `unsearched`, `scanned`, and `scan_truncated`. `unsearched` is
populated only for a requested content search when a candidate's bounded read
fails or a search stage falls back; its entries name the session or search
stage and diagnostic. It is distinct from
`unreadable`, which describes a session that could not be normalized at all.
`list --search` inspects the last 32 normalized turns per candidate before the
per-harness result limit.
