# OpenCode session formats

OpenCode v2 is **event-sourced**. The canonical record is the `event` table in
the v2 SQLite store; every other form (live API, plain JSON export, Markdown
export) is a projection of that log. `tapes` reduces them to one normalized model, so
nothing downstream needs to know which projection it came from.

```
                 ┌─────────────────────────────┐
                 │ opencode-next.db (event     │  ← canonical, WAL, safe to read
                 │  table, ordered by seq)     │    concurrently with `mode=ro`
                 └──────────────┬──────────────┘
                                │
        ┌───────────────────────┼─────────────────────────┐
        │                       │                         │
  live API (SSE)          TUI export action          TUI export action
  /api/experimental/      "with debug" toggle ON     "with debug" toggle OFF
  session/{id}/log        → {info, events}           → {info, messages}
  + /api/session/{id}/                                + Markdown export (human-
    message (paginated)                                readable, lossy)
```

## 1. Plain JSON export (`info` + `messages`)

Produced by the TUI export action without debug. Shape:

```jsonc
{
  "info": {
    "id": "ses_…", "projectID": "…", "agent": "build",
    "model": {"id": "kimi-k3", "providerID": "opencode-go", "variant": "max"},
    "cost": 11.69,
    "tokens": {"input": 0, "output": 0, "reasoning": 0,
               "cache": {"read": 0, "write": 0}},
    "time": {"created": 1784590971789, "updated": 1784590971789},
    "title": "…", "location": {"directory": "/abs/cwd"}
  },
  "messages": [
    // user:      {id, type:"user", time:{created}, text, files[], agents[]}
    // assistant: {id, type:"assistant", time:{created, completed}, agent, model,
    //             content[], snapshot, finish, cost, tokens}
  ]
}
```

Assistant `content[]` block types:

| `type` | payload | notes |
|---|---|---|
| `reasoning` | `text`, `state`, `time{created,completed}` | full thinking, verbatim |
| `text` | `text` | visible prose |
| `tool` | `id`, `name`, `executed`, `state{status,input,content[],structured,error,result}`, `time{created,ran,completed}` | see quirks |

Quirks a reader must handle:

- **`finish`** is one of `"stop"`, `"tool-calls"`, or absent (`None`). A
  message with `finish` absent and zero content blocks is a step that never
  produced output (killed or retried away).
- **Tool failure payload is split**: `state.error` is often vacuous
  (`{"type":"unknown","message":""}`) while the real message lives in
  `state.result.value` (e.g. "File changed after permission approval…").
  Always check both (`tool_error_text`).
- **`snapshot`** on assistant messages: `{start, end, files[]}` — v2 snapshots
  are git-tree hashes of the project; `files` lists repo-relative paths that
  changed between the two. This is edit evidence independent of tool calls.
- **`info.time.updated` can equal `created`** — never trust it for
  last-activity; compute from message/event timestamps.
- Interrupt/retry events are **not** in this form — terminal-state detection
  here is heuristic (empty tail → `mid_generation_interrupted`).

## 2. Debug JSON export (`info` + `events`)

Same export with the debug toggle: the raw durable event log.

```jsonc
{"info": {…same…},
 "events": [
   {"id": "evt_…", "created": 1784590971803,
    "type": "session.input.admitted",           // version suffix stripped
    "durable": {"aggregateID": "ses_…", "seq": 1, "version": 1},
    "data": {…type-specific…}}
 ]}
```

Event types (observed, v0.0.0-next-15914/15919):

| event | carries | digest use |
|---|---|---|
| `session.input.admitted` | `inputID`, `input{type,data{text,files,agents},delivery}` | user prompts |
| `session.step.started` | `assistantMessageID`, `agent`, `model`, `snapshot` (start hash) | assistant msg open |
| `session.step.ended` | `finish`, `cost`, `tokens`, `snapshot` (end hash), `files[]` | assistant msg close |
| `session.reasoning.started/.ended` | `ordinal`, ended has full `text` | reasoning blocks |
| `session.text.started/.ended` | `ordinal`, ended has full `text` | prose blocks |
| `session.tool.input.started` | `callID`, `name` | tool block open |
| `session.tool.input.ended` | `text` (JSON string of input) | input fallback |
| `session.tool.called` | `input` (parsed), `executed` | tool input |
| `session.tool.progress` | partial `content[]` | progress tail for in-flight calls |
| `session.tool.success` | `content[]`, `structured{exit,truncated}` | tool output |
| `session.tool.failed` | `error`, `result{type,value}` | tool errors (see quirk above) |
| `session.retry.scheduled` | `attempt`, `at`, `error{type,message}` | **quota/rate-limit kills** (`provider.rate-limit`, HTTP 429) |
| `session.execution.started/.succeeded` | — | turn boundaries |
| `session.execution.interrupted` | `reason` (`"user"`) | **user aborts** — the sharpest terminal signal |
| `session.usage.recorded` | `source` (e.g. `title`), `cost`, `tokens` | side-usage (title gen) |
| `session.renamed` | `title` | title history |
| `session.instructions.updated` | config hashes | ignored |
| `client.connection`, `log.synced` | — | non-durable, skipped |

The reducer reconstructs the projection from these (user msgs from
`input.admitted`, assistant msgs per `step.started`, blocks in seq order) and
keeps retries/interrupts/usage as `events` extras. Tool blocks left in
`pending`/`running` at EOF are the in-flight signal.

## 3. Markdown export (`.md`) — v1 and v2 share the format

Human-readable export; the format did not change between v1 and v2 (same
markers in both binaries). Shape:

```md
# <title>

**Session ID:** ses_…
**Created:** 7/20/2026, 8:42:51 PM
**Updated:** …

---

## User
<verbatim>

---

## Assistant
_Thinking:_
<reasoning verbatim>

**Tool: shell**

**Input:**
```json
{"command": "…"}
```
<raw tool output, unfenced>
```

Losses and hazards (surfaced as `source_notes` in the digest):

- **Tool error output is dropped** — a failed tool renders with empty output.
- Edit results render as **diff lines** (`+`/`-`/context), not full content.
- No cost/tokens, no finish reasons, no per-message timestamps.
- Content may contain literal `## Heading` lines — split **only** on exact
  `^## User$` / `^## Assistant$` lines.
- Clean completion cannot be distinguished from an interrupted tail; the
  detector returns `unknown` with an explanation (or
  `mid_generation_interrupted` when the final `## Assistant` section is
  empty).

## 4. Live resolution: v2 SQLite store

Default path `$XDG_DATA_HOME/opencode/opencode-next.db`
(usually `~/.local/share/opencode/opencode-next.db`; override with
`OPENCODE_DB` or `--db`). Opened `mode=ro`; WAL makes concurrent reads safe
while the server writes — reading a session *while it runs* works and yields
`mid_tool_interrupted` when caught between call and result.

Relevant schema (drizzle migrations may drift this; a reader should degrade
with a clear error):

```sql
session(id, slug, project_id, parent_id, fork_session_id, directory, title,
        version, agent, model /* JSON text */, cost,
        tokens_input, tokens_output, tokens_reasoning,
        tokens_cache_read, tokens_cache_write,
        time_created, time_updated, time_compacting, …)
event(id, aggregate_id, seq, created, type /* versioned: 'session.tool.called.1' */,
      data /* JSON text = export event's data */)
event_sequence(aggregate_id, seq, owner_id)
```

Notes:

- `event.type` carries a schema-version suffix (`.1`, `.2`) — strip it.
- seq 0 is `session.created.1` (carries the initial `info`); the TUI debug
  export starts at seq 1. The reducer tolerates both.
- `session.model` is a JSON string; `time_compacting` (when set) is the
  compaction signal for the projection-less path.
- The current `opencode` executable stores v1 sessions in `opencode.db`; the
  CLI database path below reads its `session`, `message`, and `part` tables.
  The separate v2 store is used by `opencode2`.

## 5. Current CLI database path

Current `opencode` releases expose the v1 session projection through the
read-only `opencode db --format tsv` command. `tapes` uses SELECTs against the
`session`, `message`, and `part` tables for the stable executable, and the API
path for `opencode2`. When both commands are installed, both stores are read
and their sessions are merged under the `opencode` harness.
If both projections contain the same session id, the stable executable's
projection is retained rather than reporting the shared record as ambiguous.

## 6. Live HTTP API (legacy reference)

The `opencode2 serve` background service exposes:

- `GET /api/session` — list sessions (`{"data":[…]}`)
- `GET /api/session/{id}` — session info (`{"data":{…}}`)
- `GET /api/session/{id}/message?limit&order&cursor` — paginated projection
  (default 50, desc; cursor-based)
- `GET /api/experimental/session/{id}/log` — the full event log as **SSE**
  (`data: <json>` lines, terminates with a `log.synced` control event)

Auth: HTTP basic, credentials from `opencode2 pair` (URL/username/password).
Known hazard: `opencode2 api …` truncates large payloads on stdout (~196 KB),
so scripted consumers should call the HTTP endpoints directly with `curl`.
The `opencode2` compatibility path calls the API and never opens the SQLite
file. The stable executable uses its own read-only database query command;
when both commands are installed, each path remains available so sessions
from either store can be resolved.

The session-list and session-info responses are the normalized metadata source
for `tapes`. A title-less OpenCode session therefore keeps both `title` and
`derived_title` absent: deriving a first-user-turn hint would require an extra
message request per row, which would make listing unbounded and unexpectedly
expensive. Message reads preserve that same metadata rather than inventing a
title after listing.

## Resume / fork primitives

- Resume in the TUI: `opencode2 --session ses_<id>` (global store; cwd not required)
- Fork via API: `opencode2 api POST /api/session/{id}/fork`
- v1 binary (`opencode`): separate store, `opencode export <id>` writes the
  plain JSON to stdout — but it only sees the v1 DB.
