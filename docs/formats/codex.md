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
| `session_meta` | `payload.id` (session UUID), `payload.cwd` |
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
reports its kind and top-level timestamp as `trailing_record`.

The normalized reader retains only a bounded 4 MiB tail for transcript reads.
On a rollout whose model-bearing `turn_context` falls before that tail, the
normalized `model` is absent even though the full file records the model; JSON
preserves that absence rather than inventing a value.

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

Some Codex invocations inject a leading user record containing a heading such
as `# AGENTS.md instructions` (optionally followed by a directory), an
`<INSTRUCTIONS>` block, and a `<recommended_plugins>` block before the human
request. The normalized
derived title ignores that wrapper and chooses the first later user turn with
meaningful content; an instruction-only record does not become the title.

Reasoning payloads are frequently encrypted, carrying a signature rather than
readable text. That is absence, not failure — a session can legitimately yield
reasoning turns with placeholder content.

## Token accounting

`payload.type == "token_count"` carries
`info.total_token_usage.{input_tokens, cached_input_tokens,
cache_write_input_tokens, output_tokens, reasoning_output_tokens,
total_tokens}`. Codex reports no cost.

## Lineage note

The Python extractor this backend replaced opened with a docstring claiming it
parsed OpenCode. The logic was codex-specific throughout and parsed live
rollouts correctly; the docstring was stale from a copied file. Nothing in this
document is inherited from that claim — every row above was checked against a
real rollout.
