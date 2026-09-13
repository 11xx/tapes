# Claude Code JSONL schema (relevant fields)

What `tapes`' claude backend reads, and why. The format is undocumented by its
harness and drifts; check a real transcript before trusting any row here.

## File location

```
~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl
```

`<encoded-cwd>` is the absolute path with each `/` replaced by `-`, with a
leading `-`. A `.` is replaced the same way, and other punctuation is assumed
to be — the encoding has only been observed, never documented by the harness:

| Cwd | Encoded |
|---|---|
| `/work/project` | `-work-project` |
| `/home/user` | `-home-user` |
| `/home/user/.config/emacs` | `-home-user--config-emacs` |
| `/` | `-` |

The encoding is not injective: a literal `-` in a directory name and a `/`
both encode to `-`, so a name cannot be decoded back to a path. It also
records the spelling the session used, so the same directory reached through
a symlink produces a different name. `tapes` therefore never treats these
names as an index of which project a session belongs to — the `cwd` field
inside the transcript is the only authoritative answer.

`$CLAUDE_HOME` (default `~/.claude`) and `$PROJECTS_DIR` are overridable for testing.

## Top-level fields on every message

| Field | Type | Notes |
|---|---|---|
| `type` | string | `user`, `assistant`, `system`, `file-history-snapshot`, `attachment`, `last-prompt`, `ai-title`, `mode`, `permission-mode`, `atis-latch`, `queue-operation`, `cost-state` |
| `timestamp` | ISO 8601 | UTC; missing on metadata messages |
| `sessionId` | UUID | Same on every message in a thread |
| `cwd` | string | Captured working directory; same on every message |
| `gitBranch` | string | Branch at the time of the message |
| `parentUuid` | UUID | Parent in the message tree (for subagent messages) |
| `isSidechain` | bool | true for subagent / fork messages |
| `uuid` | UUID | Unique per message |
| `message` | object | The actual content (shape depends on `type`) |
| `origin`, `promptSource`, `isMeta` | object, string, bool | On a `user` record, what the record is; see below |

The reader keeps two bounded windows on a transcript: the first 64 KiB and the
last 4 MiB. `sessionId`, `cwd`, the recorded start timestamp, and the first
user turn come from the opening, so a transcript larger than the tail still
reports the start and first prompt its opening recorded. The tail supplies the
turns, the last activity, the `aiTitle`, and the final model. A turn's
`native_id` is the `uuid` of the line it came from; the text, thinking, and
tool blocks of one assistant message share it.

The normalized source descriptor identifies this as an installed Claude
recording and keeps the file path as an opaque location. The optional
`started_at` and `last_activity_at` fields remain absent when no reached record
supplies valid timestamps; filesystem times are not substituted. Every decoded
turn carries an absolute file `record_ref` span and the read object records the
source revision, physical ranges, and gaps. The reference is local to that
source observation and is not a portable content digest.

## `type: "user"` — the user role

`message.content` is *either* a string (real prompt) *or* an array (tool result envelope).

### Real prompt (string content)

```json
{
  "type": "user",
  "origin": { "kind": "human" },
  "promptSource": "typed",
  "message": {
    "role": "user",
    "content": "what's the weather?"
  }
}
```

The user role is the envelope for everything the harness has to put in front
of the model, so three top-level fields say what a record actually is:

| Field | Values seen | Means |
|---|---|---|
| `origin.kind` | `human`, `task-notification`, `auto-continuation` | Who sent the message. Every prompt a person typed carries `human`. |
| `promptSource` | `typed`, `system` | How the prompt reached the harness. A typed prompt carries `typed` alongside `origin.kind: human`; a message the harness raised itself carries `system`. |
| `isMeta` | `true` | The harness attached this text itself, such as a hook notice or the caveat that precedes a local command's output. |

A record carrying none of the three is the harness's own local-command
envelope when its string content is exactly one of these and nothing else:

```json
{ "message": { "role": "user", "content": "<command-name>/exit</command-name>\n<command-message>exit</command-message>\n<command-args></command-args>" } }
{ "message": { "role": "user", "content": "<local-command-stdout>(no content)</local-command-stdout>" } }
```

`<command-name>` is accompanied by `<command-message>`, `<command-args>`, and
sometimes `<command-contents>`, separated by whitespace. A `<local-command-caveat>`
envelope rides on an `isMeta` record rather than on one of its own. The
envelopes are read only where the sender fields are absent, since a person can
type text that looks like one and the fields the harness wrote outrank the
text every time. A record with neither a sender field nor an envelope — a
transcript from a harness version that wrote none — says nothing about what it
is, and the normalized turn keeps that absence as `kind: unknown`.

### Tool result (array content)

```json
{
  "type": "user",
  "message": {
    "role": "user",
    "content": [
      {
        "type": "tool_result",
        "tool_use_id": "toolu_01ABC...",
        "content": "Total Test time (real) =   1.61 sec",
        "is_error": false
      }
    ]
  }
}
```

The `tool_use_id` references the assistant's `tool_use` block id. `is_error: true` is a hard error signal.

Tool blocks supply typed event fields as follows:

| event field | Claude source |
|---|---|
| `kind` | `tool-call` for `tool_use`; `tool-result` for `tool_result` |
| `subtype` | content block `type` |
| `name` | `tool_use.name`; result blocks leave it absent |
| `call_id` | `tool_use.id` or `tool_result.tool_use_id` |
| `status` | `error` only when `tool_result.is_error` is true |
| `arguments` | `tool_use.input` |
| `output` | `tool_result.content` |
| event timestamp | the containing record's top-level `timestamp` |

Object and array payloads are serialized as compact JSON for bounded argument
and output metadata. Separate records carry the two halves, so
`completed_ts` is absent.

## `type: "assistant"`

`message.content` is an array of typed blocks:

```json
{
  "type": "assistant",
  "message": {
    "role": "assistant",
    "stop_reason": "end_turn",     // or "tool_use", "stop_sequence", "max_tokens"
    "content": [
      { "type": "thinking", "thinking": "...", "signature": "..." },
      { "type": "text", "text": "..." },
      { "type": "tool_use", "id": "toolu_01ABC...", "name": "Bash", "input": { ... } }
    ]
  }
}
```

`stop_reason` is the spine of state detection:
- `end_turn` — assistant finished its turn (could be cleanly completed OR could be session limit)
- `tool_use` — assistant emitted a tool call; turn is still open
- `stop_sequence` — stopped by a stop sequence (often the session-limit message)
- `max_tokens` — truncated by the model's output limit

Each assistant record also carries per-request usage when the API reports it:

```json
{
  "requestId": "request-1",
  "message": {
    "role": "assistant",
    "usage": {
      "input_tokens": 34546,
      "output_tokens": 285431,
      "cache_read_input_tokens": 64355969,
      "cache_creation_input_tokens": 604968,
      "output_tokens_details": {"thinking_tokens": 102346}
    }
  }
}
```

One API request can produce several assistant records, one for each content
block, and those records repeat the same `requestId` and usage. A reader sums
one usage object per `requestId`; an assistant record without `requestId` is
counted once by itself. Synthetic records can carry zero usage, which remains
recorded as zero.

`tool_use.input` is whatever the assistant passed to the tool — for `Bash`, it's `{ "command": "..." }`; for `Read`, `{ "file_path": "..." }`; etc. The `id` matches `tool_result.tool_use_id` in the next user message.

## `type: "system"`

`message.subtype` discriminates:

| Subtype | Meaning | Useful fields |
|---|---|---|
| `turn_duration` | End-of-turn timing metadata | — |
| `compact_boundary` | `/compact` happened | `compactMetadata.trigger`, `compactMetadata.preTokens`, `compactMetadata.postTokens`, `compactMetadata.preservedMessages.allUuids` |
| `away_summary` | Snapshot written when the user was away (auto-detected) | `content` — a 1-2 sentence state description |
| `local_command` | A local command's stdout | `content` |

The `compact_boundary` event is critical for state detection: if it appears and a real user prompt follows it, the thread is `compacted_and_resumed`. If it appears and no real user prompt follows, it's `compacted_no_resume`.

## `type: "file-history-snapshot"`

```json
{
  "type": "file-history-snapshot",
  "snapshot": {
    "messageId": "...",
    "timestamp": "...",
    "trackedFileBackups": {
      "src/ipc/IpcSocket.h": {
        "backupFileName": null,
        "version": 1,
        "backupTime": "..."
      }
    }
  }
}
```

Note: `trackedFileBackups` is a **dict keyed by file path**, not a list. The values are metadata. Most snapshots are empty dicts. The *union of keys across all snapshots* is the full set of files the transcript recorded as touched.

## `type: "cost-state"`

`cost-state` is session metadata with no top-level `timestamp`, and it produces
no conversation turn. Its `modelUsage` object is keyed by model and carries
cumulative `inputTokens`, `outputTokens`, `thinkingTokens`,
`cacheReadInputTokens`, `cacheCreationInputTokens`, and per-model `costUSD`.
`totalCostUSD` is the cumulative cost for the session at the time of the
record. Four wall-clock durations ride alongside it in milliseconds:
`totalAPIDuration`, `totalAPIDurationWithoutRetries`, `totalToolDuration`, and
`totalDuration`. The durations and the per-model split are what the usage view
reports as `durations_ms` and `by_model`; a transcript whose bounded read holds
no `cost-state` reports neither. When a bounded read contains more than one such record, the newest
record is the session's accounting source; otherwise assistant `message.usage`
records provide a per-request sum. Claude records no per-request cost in
`message.usage`.

## Metadata types

`mode`, `last-prompt`, `ai-title`, `queue-operation`, `attachment`, and
`cost-state` are UI or accounting metadata carrying no conversation content,
so they produce no turns. `cost-state` contributes session accounting but is
not a `trailing_record` kind. `ai-title` is the exception the backend does
read: it is the only harness-supplied session title of the four.

When the final records after the newest rendered turn are the verified
metadata kinds `last-prompt`, `ai-title`, `mode`, `permission-mode`, or
`atis-latch`, the backend reports that final kind as `trailing_record.kind`.
These records have no top-level `timestamp` in the observed store, so
`trailing_record.timestamp` remains absent rather than borrowing a timestamp
from a neighboring turn.

## Subagent transcripts

A session's subagent threads live one level deeper, and each carries the
**parent's** `sessionId`:

```
~/.claude/projects/<encoded-cwd>/<session-uuid>/subagents/agent-<id>.jsonl
```

Enumerating `<encoded-cwd>/*.jsonl` therefore lists sessions; recursing further
lists the same session many times over. `tapes` enumerates at session depth and
reports the subagent count as a transcript note.

Beside each transcript is `agent-<id>.meta.json`, describing the agent the
parent asked for:

```json
{"agentType": "Explore", "description": "…", "toolUseId": "toolu_01…", "spawnDepth": 1, "model": "sonnet"}
```

`toolUseId` is the `id` of the `Agent` tool use in the parent, which is where
the spawn itself is recorded:

```json
{"type": "tool_use", "id": "toolu_01…", "name": "Agent", "input": {"subagent_type": "Explore", "description": "…"}}
```

The matching tool-result record carries a top-level `toolUseResult`, an object
or the JSON text of one, reporting the agent's outcome: `status`, `agentId`,
`agentType`, `resolvedModel`, and, once it ends, `totalDurationMs`,
`totalTokens`, and `totalToolUseCount`. A record whose `status` is
`async_launched` reports a subagent that is still running and is not an
ending; any other status is one.

A `toolUseResult` accompanies every tool's result, so a result belongs to an
agent when it answers an `Agent` call or names an `agentId` itself; the second
is what recognizes an agent whose call is behind the bounded read.

The lineage view reads those three records and nothing else: the file stem's
agent id is the child's reference, the meta record and the call supply its
role and model, the call and result supply the spawn and ending timestamps,
and `status` is the disposition. A subagent transcript is not addressable as a
session of its own, so a child carries no session id. A call whose transcript
is absent from the store stays a child with `resolved: false`.

Subagent metadata files are capped at 64 KiB. Missing optional metadata leaves
its fields absent. Oversized, malformed or unreadable metadata leaves those
fields unavailable and adds a lineage diagnostic; the child transcript
reference remains available independently of the metadata.
