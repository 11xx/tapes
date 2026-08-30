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
truncation marker separately. A bounded reader
that cannot see the first user turn leaves the hint absent rather than labeling
a later turn as the first. API-backed metadata readers also leave it absent
when deriving it would require an extra message request; this keeps listing
bounded and keeps show/export metadata consistent with listing metadata.
`derived_title_truncated` is present with the derived hint and is `true` when
the hint was shortened, or `false` when the present hint is complete. For
Codex, an absent `model` can mean that the model-bearing `turn_context` was
before the bounded 4 MiB file-tail read; the reader preserves that absence
rather than inventing a model.

`Model` contains the model identifier and an optional variant. The variant
also carries an effort level when the harness records one. Its identity is the
identifier alone, or `id (variant)` when a variant exists; `list --model` uses
that full identity for case-insensitive substring matching and does not match
sessions whose model is absent. `list --directory` likewise performs a
case-insensitive substring match against the recorded path and does not match
sessions whose directory is absent. Both filters are applied before the
per-harness listing bound. `Cost` contains a single USD value. `Tokens` can
independently record input, output, reasoning, cache-read, and cache-write
counts.

`Turn` contains a role, text, and optional UTC timestamp. Roles are `user`,
`assistant`, `tool`, and `reasoning`.

`TrailingRecord` identifies a verified final record after the newest rendered
turn when that record does not become a turn. It carries the source `kind` and
an optional UTC `timestamp`; the timestamp stays absent when the source record
does not provide one. `Transcript` omits `trailing_record` when no verified
trailing record is available.

`Transcript` contains a session, its turns, a `truncated` flag, an optional
`trailing_record`, and optional reader-facing notes. The flag is true when a
read bound was reached and the turns are only a window into the transcript.
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
