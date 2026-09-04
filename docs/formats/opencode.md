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

A message of `type: "user"` carries the operator's own text and nothing else;
everything the harness contributes is a part of an assistant message. A session
ending on a user message therefore ends on an unanswered request.

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

A `tool` part supplies one tool turn and one typed call event. Completed and
failed states also project a result event without creating a second turn:

| event field | API part source | stable-database projection source |
|---|---|---|
| `kind` | `tool-call`; `completed` and `error` also project `tool-result` | same |
| `subtype` | `tool` | `tool` |
| `name` | part `name` | part `tool` |
| `call_id` | part `callID` when present, else the part `id` | part `callID` when present, else the part `id` |
| `status` | `state.status` | `state.status` |
| `arguments` | `state.input` | bounded `state.input` projection |
| `output` | `state.content`, then `state.output` or `state.error`; error states prefer `state.error` | `state.output`, or `state.error` for an error state |
| call timestamp | `time.created` | `time.start` |
| `completed_ts` | `time.completed` | `time.end` |

The result projection uses `completed_ts` as its event timestamp and shares the
call's turn ordinal and pairing key. A `pending` or `running` state has no
result projection. Structured payloads are serialized as compact JSON before
their bounded metadata is built.

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
  The separate v2 store is used by `opencode2`. Its `event` table records the
  event stream and `session_message.data` holds the materialized message shape
  exposed by the API, but the installed v2 CLI has no read-only database
  command.

## 5. Current database path

For the default stable executable, `tapes` opens `opencode.db` through
`sqlite3 -readonly` when that command is available. It otherwise uses
`opencode db --format tsv`. Both routes issue SELECTs against the `session`,
`message`, and `part` tables. OpenCode2 uses its API as the authoritative read
path. When both commands are installed, both stores are read and their
sessions are merged under the `opencode` harness.
If both projections contain the same session id, the stable executable's
projection is retained rather than reporting the shared record as ambiguous.
Search applies that rule to candidates before accepting matches, so a later
v2 projection cannot turn a stable projection's non-match into a match.
The database command emits a `row` header followed by one JSON object per
session row. Listing parses those rows independently: a row that cannot be
parsed is reported in the listing's `unreadable` field with its session id and
parse diagnostic, while an exact lookup of that id preserves the parse error.

The `message.data` column is the complete JSON message record and is not a
bounded transcript field. In a verified v1 store, no stored `data` value
was exactly 65,536 bytes, while 47 `message.data` values exceeded 65,536 bytes
and one exceeded 8 MiB. Two session-level raw message projections exceeded 8
MiB after their rows were combined. The 65,536-byte EOF boundary therefore
belongs to the OpenCode/Bun SQLite result-string path, not to a SQLite column
width; the shorter EOF diagnostic at column 129 is the same raw-message
projection failure class. The 8 MiB error is `tapes`' child-command response
cap. A reader must project only the message fields it consumes (`role` and
`time`) and keep the bounded part projection; it must not transfer raw
`message.data`.

The database transcript read is bounded in three places, and each bound it
reaches is reported under the transcript's `truncation.source`: the newest
1,000 messages (`record-page` of `messages`), the newest 5,000 parts
(`record-page` of `parts`), and text cut in the projection itself, at 4,000
characters for text and reasoning parts and 2,000 for tool input, output, and
error (`turn-text`, one entry per bound with the count of parts it cut).

The API transcript read pages `GET /api/session/{id}/message` newest first
rather than fetching the whole projection: the first page asks for
`limit=N&order=desc`, each later page passes `limit=N&cursor=<next>` and no
`order`, which the endpoint refuses to combine with a cursor. `N` is the
requested tail clamped between 8 and 50 messages. Pages are fetched until the
requested number of turns is in hand, the store is exhausted, or 1,000
messages have been read. The endpoint answers every page with a `cursor.next`,
including the page after its last message, so cursor presence means nothing;
a page shorter than its limit is the sign of exhaustion. A page the 8 MiB
transport bound cannot carry is retried one message at a time, and the page
size doubles back up while pages fit, since every failed attempt costs a full
bound of transfer. A single message larger than
the bound is the store's own limit: the read stops before it, hands over the
newer messages it has, and says so in a transcript note; only when that
message is the newest one, with nothing readable in front of it, does the read
fail. Why the read stopped decides what the transcript reports. Stopping
because the requested window was full is a window with `omitted_exact:
false`: only the fetched messages are counted, a wider `--tail` or `export`
fetches older ones, and ordinals count from the oldest fetched turn, so a
reference into an OpenCode API session should carry the `native_id`. Stopping
at the 1,000-message ceiling, which is exact whatever page size the read had
grown back to, or before a message the transport cannot carry, is a
`record-page` of `messages` naming how many were fetched. A page shorter than
its limit is exhaustion and reports nothing. `export` reads with an unbounded
window and pages to the same ceiling, so a session whose whole projection is
larger than the transport bound still exports. A content search that reached
fewer than the 32 turns it searches before a ceiling reports the session as
unsearched rather than as a non-match.

OpenCode transcript reads render messages as turns and do not read a record kind
that could follow the newest message. `Transcript.trailing_record` is therefore
always absent for this backend.

A turn's `native_id` is the part's `id` where the read carries one, and the
message `id` (`msg_…`) otherwise; the database projection carries message ids
only. `session.store` is the `opencode.db` path for the stable store and the
program and `/api/session/<id>` endpoint for the API.

For content search, the database path first runs a read-only SQL prefilter over
the listed session ids. It searches JSON-decoded part values (and preserves
invalid JSON rows as candidates) and returns ids only. The bounded normalized
transcript read remains the authority, so the prefilter may retain a
non-matching session but may not reject a possible match. Candidate ids are
queried in batches of 256; batching is an output-size bound, not a result
limit, and the batches are unioned before confirmation.

OpenCode2's session-list `search` parameter is a title filter, not a content
filter. Its message endpoint can page one session at a time with `limit` and
`before`, but it has no global transcript-search operation. The API-backed
reader first uses an explicit read-only `sqlite3` prefilter over
`session_message.data` when the v2 session table covers every
listed session. Otherwise it starts one local server for enumeration and uses
bounded HTTP GET reads for each candidate against that server.
This path starts `opencode2 serve` on a temporary port bound to `127.0.0.1`
with `OPENCODE_SERVER_PASSWORD` removed from the server process environment,
so the server accepts unauthenticated requests for the duration of the scan.
The server process is killed when the search finishes.
The v2 prefilter returns only session ids, retains invalid or larger messages
as uncertain candidates, and checks the `session` table coverage before using
an empty result as a definitive non-match. Its 64 KiB per-message and 8 MiB
per-session uncertainty bounds reduce work without hiding a read failure.
Without full database coverage, a no-match search necessarily costs one
confirmation read per candidate after metadata filters; the shared server
removes repeated process startup but cannot change that v2 API contract. If a
SQL prefilter fails, `tapes` keeps the safe per-session fallback and adds the
prefilter diagnostic to `unsearched`; an unsupported prefilter is distinct and
falls back without that error note. If the local API server cannot start or
list candidates, `tapes` falls back to `opencode2 api --standalone get` listing
and per-session reads and records the failed stage and diagnostic in
`unsearched`.

## 6. Live HTTP API (legacy reference)

The `opencode2 serve` background service exposes:

- `GET /api/session` — list sessions (`{"data":[…]}`)
- `GET /api/session/{id}` — session info (`{"data":{…}}`)
- `GET /api/session/{id}/message?limit&order&cursor` — paginated projection
  (default 50, desc; cursor-based; a request combining `cursor` with `order`
  is answered with an `InvalidCursorError` body and exit 0, so a missing
  `data` array is an error)
- `GET /api/experimental/session/{id}/log` — the full event log as **SSE**
  (`data: <json>` lines, terminates with a `log.synced` control event)

Auth: HTTP basic, credentials from `opencode2 pair` (URL/username/password).
Known hazard: `opencode2 api …` truncates large payloads on stdout (~196 KB),
so scripted consumers should call the HTTP endpoints directly with `curl`.
The `opencode2` compatibility path uses the API for authoritative reads and
may query `opencode-next.db` through `sqlite3 -readonly` only as a candidate
prefilter. The stable executable uses `sqlite3 -readonly` when available and
falls back to its database query command;
when both commands are installed, each path remains available so sessions
from either store can be resolved.

The session-list and session-info responses are the normalized metadata source
for `tapes`. A title-less OpenCode session therefore keeps both `title` and
`derived_title` absent in an ordinary listing: deriving a first-user-turn hint
would require an extra message request per row, which would make listing
unbounded and unexpectedly expensive. An explicit content search may read the
bounded message tail, but that read preserves the same metadata rather than
inventing a title after listing.

## Resume / fork primitives

- Resume in the TUI: `opencode2 --session ses_<id>` (global store; cwd not required)
- Fork via API: `opencode2 api POST /api/session/{id}/fork`
- v1 binary (`opencode`): separate store, `opencode export <id>` writes the
  plain JSON to stdout — but it only sees the v1 DB.
