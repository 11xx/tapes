# pi session format

What `tapes`' pi backend reads. The format is undocumented by its harness and
drifts; check a real session before trusting any row here.

## File location

```
~/.pi/agent/sessions/<project-slug>/<timestamp>_<session-uuid>.jsonl
```

`PI_CODING_AGENT_SESSION_DIR` overrides the whole path;
`PI_CODING_AGENT_DIR` overrides its parent, with `sessions` appended. The
session id is the part of the filename after the underscore — note that the
separator is `_`, where codex uses `-`. A resolver that hardcodes one
harness's separator silently loses the other.

pi's own CLI offers an interactive picker and no non-interactive read, so file
discovery is the only retrieval path.

## Entries are a tree, not a list

Every line after the header is an entry with an `id` and a `parentId`. The
file is append-only, and a rewind or fork writes new entries whose `parentId`
points back above the abandoned ones rather than truncating the file.

The **active conversation** is therefore the leaf-to-root path: start at the
last entry and follow `parentId` until it is null. Entries off that path are
abandoned branches. Reading the file as a flat list silently interleaves work
the user rewound past with work they kept.

`tapes` digests the active path and reports the abandoned remainder as a
transcript note, because a count of discarded entries is a fact a rescuer
needs and the normalized model has no field for it.

## Line types

| `type` | Carries |
|---|---|
| `session` | the header: `id`, `timestamp`, `cwd` — written once, on the first line, and never repeated |
| `model_change` | `provider`, `modelId` |
| `thinking_level_change` | `thinkingLevel`, which the model carries as its variant |
| `message` | the conversation, under `message.role` |

Because the header is never repeated, the reader takes it from the file's
first 64 KiB rather than from the bounded tail (4 MiB by default): the session id, the
recorded start timestamp, and the working directory come from that opening
whatever the file's size. The tail supplies the active path, the turns, the
last activity, and the final model and thinking level. The first-turn hint is
not read from the opening: pi rewinds by appending a new
branch, so the first user message in the file may sit on a root the active path
never reaches, and past the bound the reader cannot tell. A transcript larger
than the tail therefore leaves `derived_title` absent. A turn's `native_id` is
its entry's `id`. The normalized source descriptor marks this as an installed
pi recording, and each retained turn carries an absolute file `record_ref`
when the JSONL read supplies a span. Missing entry timestamps remain absent;
filesystem times do not fill them.

The header also carries `parentSession` on a session started from another
one: the id of the session it came from, in pi's own terms. It is the only
relationship pi records, and it sits on the session that has it, so the
lineage view reports that reference — resolved when the store holds a session
under that id — and no children. A reference the store cannot resolve is kept
rather than dropped.

`model_change` and `thinking_level_change` are timestamped state entries rather
than turns. When either is the final entry on the active path after the newest
rendered turn, the backend reports its kind and timestamp as
`trailing_record`. The `session` header is not a trailing record.

## Message content

`message.role` is `user`, `assistant`, or `toolResult` — the tool result is a
role, not a content block, which is where pi differs most from the others. The
`user` role holds the operator's own messages and nothing else: pi keeps its
commands and its state changes in entries of their own, so a session ending on
a user message ends on an unanswered request.

Message content parts remain ordered. Text and thinking are readable parts;
tool calls and results retain bounded payload descriptors; unrecognized parts
remain qualified unknown evidence. References are never dereferenced.

Assistant content blocks:

| block `type` | Normalized as |
|---|---|
| `text` | assistant turn |
| `thinking` | reasoning turn |
| `toolCall` | tool turn |

`toolResult` entries become tool turns whole. Tool blocks and results have no
plain-text field, so `tapes` keeps the JSON as the turn's text and lets the
trace file head it with the tool name.

The tool JSON also supplies a typed event:

| event field | pi source |
|---|---|
| `kind` | `tool-call` for a `toolCall` block; `tool-result` for a `toolResult` role |
| `subtype` | `toolCall` or `toolResult` |
| `name` | call `name` or result `toolName` |
| `call_id` | call `id` or result `toolCallId` |
| `status` | `error` only when result `isError` is true |
| `arguments` | call `arguments` |
| `output` | result `content` |
| event timestamp | the containing entry's top-level `timestamp` |

String values remain strings while objects and content-block arrays are
serialized as compact JSON for bounded metadata. Calls and results occupy
separate entries, so `completed_ts` is absent. Only events on the active path
enter the normalized transcript.

Literal command or argv fields in a pi tool call can carry the same bounded
nested invocation evidence as other backends. The declaration is not a
separate executed call, and a result does not inherit the wrapper's duration.
Explicit artifact references remain unopened descriptors matched only within
the bounded active-path read.

## Assistant usage

Assistant `message` entries carry per-request `usage` when the provider reports
it:

```json
{
  "input": 9177,
  "output": 11399,
  "cacheRead": 704,
  "cacheWrite": 0,
  "reasoning": 7922,
  "totalTokens": 21280,
  "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}
}
```

The normalized counters sum `input`, `output`, `reasoning`, `cacheRead`, and
`cacheWrite` from assistant messages on the active leaf-to-root path. Entries
on abandoned branches are not counted. `cost.total` is summed only when every
counted usage entry carries a numeric `cost.total`; otherwise normalized cost
is absent. A zero is retained when pi recorded zero, while an omitted field
remains absent. `totalTokens` has no normalized field.

## What pi does not record

No session title is recorded. Cost is absent when no counted usage entry
provides a complete `cost.total` value; it is not estimated from token counts.
