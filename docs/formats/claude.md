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
| `type` | string | `user`, `assistant`, `system`, `file-history-snapshot`, `attachment`, `last-prompt`, `ai-title`, `mode`, `permission-mode`, `atis-latch`, `queue-operation` |
| `timestamp` | ISO 8601 | UTC; missing on metadata messages |
| `sessionId` | UUID | Same on every message in a thread |
| `cwd` | string | Captured working directory; same on every message |
| `gitBranch` | string | Branch at the time of the message |
| `parentUuid` | UUID | Parent in the message tree (for subagent messages) |
| `isSidechain` | bool | true for subagent / fork messages |
| `uuid` | UUID | Unique per message |
| `message` | object | The actual content (shape depends on `type`) |

The reader keeps two bounded windows on a transcript: the first 64 KiB and the
last 4 MiB. `sessionId`, `cwd`, the recorded start timestamp, and the first
user turn come from the opening, so a transcript larger than the tail still
reports the start and first prompt its opening recorded. The tail supplies the
turns, the last activity, the `aiTitle`, and the final model. A turn's
`native_id` is the `uuid` of the line it came from; the text, thinking, and
tool blocks of one assistant message share it.

## `type: "user"` — the user role

`message.content` is *either* a string (real prompt) *or* an array (tool result envelope).

### Real prompt (string content)

```json
{
  "type": "user",
  "message": {
    "role": "user",
    "content": "what's the weather?"
  }
}
```

Marked "real" only if the content is non-empty and contains no `<local-command-caveat>` or `<command-name>` markers. `/command` invocations (`/clear`, `/compact`, `/help`, …) come through as user messages with content like `<command-name>/clear</command-name>...` and should be filtered out of "real" prompts.

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

## Metadata types

`mode`, `last-prompt`, `ai-title`, `queue-operation`, `attachment` are UI and
metadata state carrying no conversation content, so they produce no turns.
`ai-title` is the exception the backend does read: it is the only harness-
supplied session title of the four.

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
