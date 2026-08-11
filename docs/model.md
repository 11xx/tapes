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
while `completed`, `idle`, and `attention` map to `idle`. If the authority is
unavailable or its snapshot is unusable, live state remains absent. The field
is added to `list` and `show` output only and is not written to export bundles.

`title` is the harness-recorded value and remains absent when the harness did
not provide one. `derived_title` is a bounded hint made from the first
meaningful user turn for human discovery; it is separate so consumers can tell
recorded metadata from a display aid. Known instruction envelopes are removed
and whitespace is collapsed before the hint is capped at 96 Unicode
characters, using an ellipsis when it is shortened. Human renderers prefix
this hint with `~`; JSON retains the two fields separately. A bounded reader
that cannot see the first user turn leaves the hint absent rather than labeling
a later turn as the first. API-backed metadata readers also leave it absent
when deriving it would require an extra message request; this keeps listing
bounded and keeps show/export metadata consistent with listing metadata.

`Model` contains the model identifier and an optional variant. The variant
also carries an effort level when the harness records one. `Cost` contains a
single USD value. `Tokens` can independently record input, output, reasoning,
cache-read, and cache-write counts.

`Turn` contains a role, text, and optional UTC timestamp. Roles are `user`,
`assistant`, `tool`, and `reasoning`.

`Transcript` contains a session, its turns, a `truncated` flag, and optional
reader-facing notes. The flag is true when a read bound was reached and the
turns are only a window into the transcript. Notes preserve harness-specific
facts that do not fit the normalized fields, such as abandoned pi branches or
the number of malformed lines skipped while reading. They are prose rather
than a structured API and are omitted from JSON when empty. Both signals reach
a human reader too: `show` closes a truncated render with a note saying so,
beside whatever notes the read produced.

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
turn produced a useful hint. JSON timestamps retain their recorded precision;
human list, show, and Markdown renderings use RFC 3339 whole seconds with a
`Z` suffix.
