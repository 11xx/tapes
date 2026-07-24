# tapes

Read, list, and export coding-agent sessions across harnesses.

A session is a recording. `tapes` finds it, normalizes it, and hands it back
as something you can query — whether it lives in a JSONL file on disk or
behind an HTTP API.

```
tapes list                      # every harness, newest first
tapes list --harness opencode --here
tapes show ses_07e16cc8 --tail 20
tapes export ses_07e16cc8 --bundle /tmp/
```

## Why this exists

An agent session dies mid-task — quota, a crash, a context wall — and the
work survives only in a transcript nobody can query. Recovering it meant
hand-exporting hundreds of kilobytes to `/tmp` and grepping. `tapes` makes
that a command.

## Supported harnesses

| harness | store | retrieval |
| :-- | :-- | :-- |
| claude | `~/.claude/projects/<slug>/<session>.jsonl` | file discovery |
| codex | `$CODEX_HOME/sessions/<y>/<m>/<d>/rollout-*.jsonl` | file discovery |
| opencode | SQLite behind an HTTP API | `opencode2 api --standalone` |
| pi | `~/.pi/agent/sessions`, append-only tree | file discovery |

A harness whose binary or store is absent reports itself unavailable; it
never fails the command. Listing works with any subset installed.

## Output

`export` writes a three-file bundle with a shared prefix:

- `.context.md` — exact operator turns and assistant-visible text. Read first.
- `.json` — canonical normalized messages, state, evidence, cost, git.
  Query selectively with `jq`.
- `.trace.md` — complete reasoning and tool chronology, for grepping.

Read context first, query the JSON narrowly, and reach for the trace only
when free-text search is genuinely easier. Never ingest a whole bundle
because it exists.

## Redaction

`tapes` prints what a transcript contains and performs no redaction.
Transcripts hold whatever was pasted into them. Treat exported bundles as
sensitive.

## Name

"handoff" was taken twice — an AirPods audio daemon on AUR and a Rust
agent-context tool — and it names the wrong half of the job. This tool's
primary direction is *pull*: retrieve and reconstruct a session that already
ended. Handing context forward is one thing you might do with what it
returns.
