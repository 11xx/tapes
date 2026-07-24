# Session model

Every harness backend translates its records into the same `tapes-core`
types. Consumers can therefore list sessions and read transcripts without
knowing which harness stored them.

## Types

`Session` identifies the harness and session, records its first and latest
activity timestamps, and may carry a model, title, working directory, cost,
and token counts. Both timestamps are required UTC values. A record that
cannot supply timestamps is not a session and is omitted from listings.

`Model` contains the model identifier and an optional variant. The variant
also carries an effort level when the harness records one. `Cost` contains a
single USD value. `Tokens` can independently record input, output, reasoning,
cache-read, and cache-write counts.

`Turn` contains a role, text, and optional UTC timestamp. Roles are `user`,
`assistant`, `tool`, and `reasoning`.

`Transcript` contains a session, its turns, and a `truncated` flag. The flag
is true when a read bound was reached and the turns are only a window into
the transcript.

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
strings, or invented defaults. This distinction applies to session metadata,
model variants, every token counter, and turn timestamps.
