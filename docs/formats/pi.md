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
first 64 KiB rather than from the bounded 4 MiB tail: the session id, the
recorded start timestamp, the working directory, and the first user message all
come from that opening whatever the file's size. The tail supplies the active
path, the turns, the last activity, and the final model and thinking level.
Branch structure beyond the opening is unknown to the opening itself, so its
messages are read in file order when the tail cannot see them.

`model_change` and `thinking_level_change` are timestamped state entries rather
than turns. When either is the final entry on the active path after the newest
rendered turn, the backend reports its kind and timestamp as
`trailing_record`. The `session` header is not a trailing record.

## Message content

`message.role` is `user`, `assistant`, or `toolResult` — the tool result is a
role, not a content block, which is where pi differs most from the others.

Assistant content blocks:

| block `type` | Normalized as |
|---|---|
| `text` | assistant turn |
| `thinking` | reasoning turn |
| `toolCall` | tool turn |

`toolResult` entries become tool turns whole. Tool blocks and results have no
plain-text field, so `tapes` keeps the JSON as the turn's text and lets the
trace file head it with the tool name.

## What pi does not record

No session title and no cost. Both are omitted from the normalized session
rather than defaulted — a consumer must be able to tell "no title" from
"empty title".
