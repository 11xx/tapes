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
| `turn_context` | `payload.model`, `payload.effort`, `payload.cwd`, and sometimes `payload.turn_id` |
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
gives its turn that id as `native_id`; one without leaves the field absent. A
payload `turn_id` is retained separately as `request_turn_id` and is never
invented from a neighboring record.

A read counts each record that produced no turn under `unmapped` in its read
evidence, by `type`, then `payload.type`, then an item's `type`, joined with
`/`. Declined, because they hold no text a turn would carry or restate a record
already read: `session_meta`, `turn_context`, `token_usage_record`,
`event_msg/token_count`, `event_msg/task_started`, `event_msg/task_complete`,
`event_msg/turn_aborted`, `event_msg/thread_settings_applied`, the
`event_msg/item_completed` items `Reasoning`, `AgentMessage`, `UserMessage`,
`ContextCompaction`, and `Plan`, and any tool item or web search that mirrors
a record read before it. Every other type is unrecognized; in a 905-rollout
store those were `world_state`, `compacted`,
`inter_agent_communication_metadata`, `event_msg/thread_goal_updated`,
`response_item/agent_message` (a message between agents, its payload
encrypted), and `response_item/tool_search_call` and `tool_search_output`.

The normalized transcript also retains a bounded `read` descriptor. It records
the source length, the configured tail bound (4 MiB unless `--read-bytes` sets
another), the physical head, tail, and alignment ranges, each decoded
record's absolute byte span, separate spans for records decoded only from the
opening, and gaps for the discarded partial prefix or malformed records. When the tail
begins after byte zero, the reader reads one preceding byte as alignment
evidence. A preceding newline proves that the tail begins at a record boundary,
so the first tail record is retained; otherwise the bytes through the first
newline are a `discarded-partial-record` gap. The alignment byte is physical
I/O, not normalized coverage and does not widen the configured tail bound. The
head and tail are read through one open descriptor and the descriptor is
checked again before the result is returned; a mutation aborts the read.
Physical overlap with the head does not erase a malformed gap; a partial gap is
removed only when a successful decode of that same record covers it.

A Codex history page reads its byte range and at most one preceding alignment
byte, so its `read` names only `alignment` and `tail` ranges and no context
records. A user-role message's kind comes from its own record alone, so a page
gives it the kind a whole read does. The session a page reports, with its id,
working directory, and derived title, comes from session lookup's ordinary
bounded read of the opening and tail. Transcript pages carry the `transcript`
projection option; metadata pages use the same envelope with `models-only`.

The normalized reader retains only a bounded tail (4 MiB by default) for transcript reads,
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

The normalized turn retains the content array in native order. Text blocks are
`text` parts; verified image, audio, and file carriers become reference-only
parts with their URI/path/digest fields; structured carriers and unknown kinds
become bounded key/type descriptors. A malformed or oversized part remains
coverage evidence rather than disappearing. `encrypted_content` produces a
qualified reasoning placeholder and is not treated as readable narration.

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

A user `response_item` is what the model reads, and Codex writes no field on it
naming who it came from. The same role carries the operator's requests, the
context the harness attaches, and messages the harness raises on its own; the
elements the harness wraps its own text in are what separate them. That holds
for every entry point `payload.source` in the header names: `cli`, `vscode`,
`exec` (`codex exec`, whose caller supplies one prompt), and the object-valued
source of a spawned agent. The rollouts checked carry `session_meta.cli_version`
0.133.0 through 0.154.0.

| user message | kind |
|---|---|
| every `input_text` block is attached context | `ambient` |
| its blocks add only a message the harness raised | `notice` |
| anything else, including a request typed beside attached context or a pasted image | `operator` |

Attached context blocks, whether they arrive as a record of their own or beside
the request, begin with `<environment_context>`, `<recommended_plugins>`,
`<in-app-browser-context>`, `<INSTRUCTIONS>`, `# AGENTS.md instructions`
(sometimes followed by `for <directory>`), `# Files mentioned by the user:`, or
`<skill>`. A `<skill>` record carries a skill's instructions and
follows the message that invoked the skill.

The messages the harness raises each fill a record of their own:

| element | written when |
|---|---|
| `<turn_aborted>` | the operator interrupted the previous turn; an `event_msg` of `payload.type == "turn_aborted"` follows |
| `<subagent_notification>` | a spawned agent reports its status to the parent |
| `<codex_internal_context source="goal">` | the harness prompts the agent to keep working toward the thread's goal |

Rollouts also mirror a request as an `event_msg` whose `payload.type` is
`item_completed` and whose `payload.item.type` is `UserMessage`, with the text
in `payload.item.content[]`. Spawned-agent rollouts from 0.147.0 carry requests
with no mirror, so the mirror does not decide a message's kind. No rollout
checked carries an `event_msg` of `payload.type == "user_message"`.

## Token accounting

Modern rollouts carry a top-level `token_usage_record` with `ordinal`,
`payload.response_id`, and `payload.usage`. The response identity and native
ordinal are retained as recorded; neither is inferred from a turn or from the
position of a neighboring record. The normalized usage view groups valid,
deduplicated observations by the model and effort context actually reached.
Its basis is `usage-record`, and a late modern record selects that basis for
the read without adding legacy token-event observations to it.

Older rollouts carry an `event_msg` with `payload.type == "token_count"` after
a model response. Its `info.total_token_usage` is the newest recorded session
total and `info.last_token_usage` is that event's response, each with
`input_tokens`, `cached_input_tokens`, `cache_write_input_tokens`,
`output_tokens`, `reasoning_output_tokens`, and `total_tokens`. `info` is null
on an event that carries no usage; `rate_limits` rides alongside and is not
accounting. Codex reports no cost.

The normalized session `tokens` remain the newest `total_token_usage` observed
in the read: `input` from `input_tokens`, `cache_read` from
`cached_input_tokens`, `cache_write` from `cache_write_input_tokens`, `output`
from `output_tokens`, and `reasoning` from `reasoning_output_tokens`;
`total_tokens` has no normalized field. A counter the event did not write stays
absent, a counter written as zero is zero, and an event without usage is
passed over for an older one. A window with no such event reports no tokens
rather than zero. Totals are recorded values, not a promise of global
monotonicity: an observed decrease is retained numerically and marks
`accounting.coverage` as `since-reset` with reset evidence in attribution.

When no valid modern record is reached, the reader uses the explicitly named
`token-event-advance` heuristic. A differing cumulative total may count its
actual `last_token_usage`; an unchanged total, a leading bounded observation,
missing counters, and quota-only records remain visibly uncounted. This is an
observation heuristic, not a verified request identity. The usage view keeps
raw rows, request counts, attribution coverage, and unattributed counters
separate. Unknown counters are summed only from values the source actually
wrote; no request usage is inferred by subtracting from a session total.

`tapes usage SESSION --series[=N]` opts into a recent bounded suffix of these
raw accounting observations (200 rows by default, 1 through 10,000 allowed).
Rows carry their source byte span and revision, optional native ordinal and
response identity, model context, classification, counted status, and the
counters present on their own source record. Quota stays on the token-count
row that carried it and is never copied onto a modern request row. `--full`
scans the pinned recording but keeps only the requested suffix. The series has
an 8 MiB retained serialized-row budget and reports row-cap, byte-budget,
oversized-row, and source-gap omissions independently. Ordinary usage, list,
and show paths retain no observation row collection.

The same event carries two facts that are not accounting.
`info.model_context_window` is how many tokens the session's model holds at
once. `rate_limits` sits beside `info` and describes the account's provider
quota rather than this session: `limit_id`, `plan_type`, optional `credits`
(`balance`, `has_credits`, and `unlimited`), reached-limit fields, and
`primary` and `secondary` windows, each with `used_percent`, `window_minutes`,
and a `resets_at` in epoch seconds. The normalized reader preserves the native
JSON type of balances and percentages, including false and string zero. A
quota refresh is written as a `token_count`
event with `info: null`, so the newest event carrying each fact answers for
it independently. The usage view reports them as `context_window` and
`rate_limits`, never folded into the session's counters.

## Terminal observations

Terminal lifecycle records are observed without turning them into present-tense
account state. The terminal record keeps the native `outcome`, `code`,
`message`, and duration fields when they are present. Observed
`task_complete` records can instead carry an `error` object whose
`codex_error_info` is the native error code and whose `message` is the native
error message. Those nested fields take precedence over conflicting flat
fields; a flat field is only a fallback when its nested counterpart is absent.
Messages use the same bounded text representation as other terminal messages.
The observation does not infer whether the account is currently available or
whether a later request can run. A later `token_count` event is read
independently for recorded session accounting.

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

The transcript reader takes the session header and the first user turn's
derived title from the bounded opening even when both are outside the source
tail. Opening turns are not added to the returned transcript.

## Runtime and nested command evidence

Codex reports runtime items as `event_msg` records whose `payload.type` is
`item_completed` (and, in the reader's fixtures, `item_started`); no rollout
in a 905-file store carried `item_started`. Code mode runs one outer `exec`
call whose script's operations are recorded only as items, so the tool-bearing
item types — `CommandExecution`, `FileChange`, `McpToolCall`, `ImageView`,
`Extension`, `WebSearch`, `CollabAgentToolCall`, and `SubAgentActivity` — are
tool turns of their own. An item whose `id` a `response_item` read earlier
carried as its `id` or `call_id` mirrors that record and is not a second turn:
in the store above that held for 2,632 of 7,622 `FileChange` items (the
`custom_tool_call_output` of an `apply_patch`), 5 of 839 `McpToolCall`, the
17 `clock.sleep` `Extension` items, and every `CollabAgentToolCall` and
`SubAgentActivity` (the `function_call` that spawned or waited on an agent).
A web search is recorded as a `WebSearch` item and as a
`response_item/web_search_call` carrying the same `action`, and sometimes the
same `ws_` id, in either order; the first of the two is the turn and the other
is its mirror. Two identical searches are two turns.

`AgentMessage`, `Reasoning`, `UserMessage`, `ContextCompaction`, and `Plan`
remain ordinary lifecycle evidence and do not become phantom tool turns; a
`Plan` item's text is restated by the assistant message that follows it,
wrapped in `<proposed_plan>`. A completed item with no `item_started` is one
native record carrying both halves of an operation. `events` projects a call
and result that share that record's source reference, retains the item's
argument carrier on the call, and counts the pair as complete; no native call
record is invented. An `item_started` reached in the same read keeps the
ordinary started/completed pairing. For `CommandExecution`, the native
`command` array is the argv carrier. Its elements must be strings and the
array must fit the supported argument bound; otherwise the invocation is
explicitly unsupported rather than silently filtered or truncated. The native
`parsed_cmd` array may contain structured objects and is not treated as argv.
The resulting invocation is marked `structured-runtime`; its outcome and
timing remain on the outer event pairing. A function-call argument carrying a
literal `cmd` or `argv` is retained as a static declaration.
If the bounded first or second token is shortened, the declaration is
unsupported rather than an apparently exact program or subcommand name; later
arguments retain their bounded text and truncation facts.
Static shell declarations treat spaces as word separators, newlines and
semicolons as command separators, and `#` as a comment only at a token
boundary. Single-quoted text and supported literal backslash escapes are
preserved; expansions, leading assignments, reserved words, control forms,
and unsupported escape forms remain qualified as unsupported.
Literal `tools.exec_command({cmd: ...})` forms inside a recorded orchestration
string are inspected by a bounded non-evaluating parser: the command property
must be a direct top-level property whose complete value is one literal string.
The parser decodes its supported JavaScript escapes and marks dynamic,
interpolated, incomplete, arrow-function, short-circuit, conditional, or
otherwise unsupported syntax as unsupported. Nested object properties do not
override the direct command property. Wrapper results never witness an
individual nested declaration or give it timing. Artifact
references in structured arguments or results are descriptors and are matched
only within the same qualified read.
