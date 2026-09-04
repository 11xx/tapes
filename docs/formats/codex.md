# Codex rollout format

What `tapes`' codex backend reads. The format is undocumented by its harness
and drifts; check a real rollout before trusting any row here.

## File location

```
$CODEX_HOME/sessions/<yyyy>/<mm>/<dd>/rollout-<timestamp>-<session-uuid>.jsonl
```

`$CODEX_HOME` defaults to `~/.codex`. The date directories are the write date,
not the session's own timestamps, so discovery walks the tree rather than
computing a path. The session id is the trailing UUID of the filename,
separated from the timestamp by `-`.

There is no non-interactive listing or reading command; file discovery is the
only retrieval path.

## Line shape

Every line is `{"type": …, "timestamp": …, "payload": {…}}` with an RFC 3339
`timestamp`. The top-level types relevant to `tapes` are:

| `type` | Carries |
|---|---|
| `session_meta` | `payload.id` (session UUID), `payload.cwd`, `payload.source` |
| `turn_context` | `payload.model`, `payload.effort`, `payload.cwd` |
| `response_item` | the conversation itself, discriminated by `payload.type` |
| `event_msg` | harness lifecycle and accounting state, discriminated by `payload.type` |
| `world_state` | harness state outside the conversation |

`turn_context` repeats whenever the model or effort changes, so the last one
holds the session's final selection. `effort` is what the normalized model
carries as the model variant.

Real rollouts also contain timestamped non-turn records. The verified
top-level kinds are `session_meta`, `turn_context`, `event_msg`, and
`world_state`; `event_msg` carries a more specific `payload.type`, but the
normalized trailing-record kind stays the top-level `event_msg`. When one of
these kinds is the final record after the newest rendered turn, the backend
reports its kind and top-level timestamp as `trailing_record`. A
`response_item` whose payload carries an `id` (reasoning items do, as `rs_…`)
gives its turn that id as `native_id`; one without leaves the field absent.

The normalized reader retains only a bounded 4 MiB tail for transcript reads,
plus the first 64 KiB of the file. `session_meta` is the first line, so the
session id, the recorded start timestamp, the working directory, and the first
user turn come from that opening whatever the file's size; the tail supplies
the turns, the last activity, and the final `turn_context`. On a rollout whose
model-bearing `turn_context` falls before that tail, the normalized `model` is
absent even though the full file records the model; JSON preserves that absence
rather than inventing a value.

## `response_item` payloads

| `payload.type` | Normalized as |
|---|---|
| `message` | a user or assistant turn, per `payload.role` |
| `reasoning` | a reasoning turn |
| `function_call`, `custom_tool_call` | a tool turn |
| `function_call_output`, `custom_tool_call_output` | a tool turn |

Message text lives in `payload.content[]` blocks of type `input_text` (user)
or `output_text` (assistant). Tool payloads have no text field; `tapes` keeps
the whole payload as the turn's text so nothing is lost, and the trace file
heads each one with its `name` or marks it a result via `call_id`.

The same payload supplies the typed tool event:

| event field | Codex source |
|---|---|
| `kind` | `tool-call` for `function_call` and `custom_tool_call`; `tool-result` for their output types |
| `subtype` | `payload.type` |
| `name` | call `payload.name`; result records leave it absent |
| `call_id` | `payload.call_id` |
| `status` | `payload.status` on `custom_tool_call`; every other subtype leaves it absent |
| `arguments` | `function_call.arguments` or `custom_tool_call.input` |
| `output` | result `payload.output` |
| event timestamp | the record's top-level `timestamp` |

String arguments and output are measured as written; structured output is
serialized as compact JSON before its bounded metadata is built. Codex call
and result records are separate, so `completed_ts` is absent.

Some Codex invocations inject a leading user record containing a heading such
as `# AGENTS.md instructions` (optionally followed by a directory), an
`<INSTRUCTIONS>` block, and a `<recommended_plugins>` block before the human
request. Others put the same wrapper in the same `input_text` block as the
request. The normalized
derived title ignores that wrapper and chooses the first later user turn with
meaningful content; an instruction-only record does not become the title.

Reasoning payloads are frequently encrypted, carrying a signature rather than
readable text. That is absence, not failure — a session can legitimately yield
reasoning turns with placeholder content.

## The operator's own messages

A user `response_item` is what the model reads, wrapper included, so it does
not by itself say which part somebody typed. The harness records that
separately: for each message the operator sends, an `event_msg` with
`payload.type == "user_message"` carries `payload.message`, the text as it was
sent, and follows the matching `response_item`.

```json
{"type": "event_msg", "payload": {"type": "user_message", "message": "Implement the readable title."}}
```

`payload.source` in the header names the entry point the session was started
from. An `exec` session — `codex exec`, whose caller supplies one prompt and
reads the result — records no `user_message` event at all. There, its user
messages are the wrapper followed by the caller's prompt, and the wrapper
blocks are what separates them.

The wrapper blocks, whether they arrive as a record of their own or beside the
request, begin with `<environment_context>`, `<recommended_plugins>`,
`<in-app-browser-context>`, `<INSTRUCTIONS>`, `# AGENTS.md instructions`, or
`# Files mentioned by the user:`.

## Token accounting

An `event_msg` with `payload.type == "token_count"` follows every model
response. Its `info.total_token_usage` is the session's running total and
`info.last_token_usage` is that one response, each with `input_tokens`,
`cached_input_tokens`, `cache_write_input_tokens`, `output_tokens`,
`reasoning_output_tokens`, and `total_tokens`. `info` is null on an event that
carries no usage yet; `rate_limits` rides alongside and is not accounting.
Codex reports no cost.

The normalized `tokens` are the running total from the newest `token_count`
event with usage in the bounded read: `input` from `input_tokens`,
`cache_read` from `cached_input_tokens`, `cache_write` from
`cache_write_input_tokens`, `output` from `output_tokens`, and `reasoning`
from `reasoning_output_tokens`; `total_tokens` has no normalized field. A
counter the event did not write stays absent, a counter written as zero is
zero, and an event without usage is passed over for an older one. A window
with no such event reports no tokens rather than zero. The totals are
cumulative across the session: in every rollout checked they never decrease,
across model and effort changes and across compaction, so the newest event
is the whole session's accounting so far, not the accounting of the window.
`last_token_usage` is per response and is not normalized.

The same event carries two facts that are not accounting.
`info.model_context_window` is how many tokens the session's model holds at
once. `rate_limits` sits beside `info` and describes the account's provider
quota rather than this session: `limit_id`, `plan_type`, and `primary` and
`secondary` windows, each with `used_percent`, `window_minutes`, and a
`resets_at` in epoch seconds. A quota refresh is written as a `token_count`
event with `info: null`, so the newest event carrying each fact answers for
it independently. The usage view reports them as `context_window` and
`rate_limits`, never folded into the session's counters.

## Spawned agents

A rollout can drive other agents, and each of them is an ordinary rollout in
the same store. The child's `session_meta` payload is what says so:

| field | Carries |
|---|---|
| `thread_source` | `subagent` on a spawned rollout |
| `parent_thread_id` | the parent rollout's session id |
| `agent_path` | the path the agent runs under, such as `/root/backend_workhorse` |
| `agent_nickname` | the name the parent asked for |
| `source.subagent.thread_spawn` | the same facts plus `depth` and `agent_role` |
| `multi_agent_version` | `v1` or `v2` |
| `forked_from_id` | on a forked thread, the thread it was forked from |

The parent records the other half. `spawn_agent` is a `function_call` whose
arguments carry `task_name`, `model`, `reasoning_effort`, and the message; its
`function_call_output` answers with `{"task_name": "/root/<name>"}`, the agent
path. `wait_agent` and its siblings answer with
`{"agents": [{"agent_name": "/root/<name>", "agent_status": …}]}`. A child's
thread id appears nowhere in the parent's payloads, so the join is the agent
path on both sides, and the parent id in the child's header.

The lineage view reads that pair: the spawn supplies the model and the moment
the agent started, an agent report supplies the status and, on `completed`,
the moment it ended, and the child's header supplies its session id and
nickname. A `task_name` no recording in the store answers to stays a child
with `resolved: false`, and a child whose spawn is behind the bounded read is
still named by the report that mentions it. Finding children costs one head
probe per recording, bounded at the newest 5,000; a store larger than that
says so in a note.

## Lineage note

The Python extractor this backend replaced opened with a docstring claiming it
parsed OpenCode. The logic was codex-specific throughout and parsed live
rollouts correctly; the docstring was stale from a copied file. Nothing in this
document is inherited from that claim — every row above was checked against a
real rollout.
