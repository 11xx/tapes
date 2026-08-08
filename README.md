# tapes

Read, list, and export coding-agent sessions across harnesses.

A session is a recording. `tapes` finds it, normalizes it, and hands it back
as something you can query — whether it lives in a JSONL file on disk or
behind an HTTP API.

```
tapes                           # the workflow guide
tapes list                      # every harness, newest first
tapes list --harness opencode --here
tapes show ses_07e16cc8 --tail 20
tapes show ses_07e16cc8 --json
tapes export ses_07e16cc8 --bundle /tmp/
```

## Orientation

`tapes` with no arguments prints the workflow guide: what the tool owns, the
order the commands are meant to be used in, when a dead session is worth
rescuing at all, how to ingest a bundle progressively, how to read why a
session ended, and what the export contains that must not leave `/tmp`. It is
the whole briefing an agent needs before its first command, so no separate
document has to be loaded first; `tapes --help` remains the per-command
reference.

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

`list` merges sessions from every available harness and sorts them by last
activity. `--limit` bounds each harness before merging, `--here` keeps sessions
whose recorded directory is the current directory, and `--harness` selects one
backend. Human output ends with an availability note when a backend cannot be
read. JSON output is a `tapes-list/1` object containing `sessions` and
`unavailable`.

`show` accepts a full session ID or an unambiguous prefix. It searches every
available backend, rejects ambiguous prefixes with the matching candidates,
and prints normalized turns in chronological order. `--tail` bounds the turns
returned. JSON output uses the `tapes-session/1` transcript schema.

`export` writes a three-file bundle sharing one timestamped prefix, into
`--bundle <dir>` or `/tmp`:

- `.context.md` — exact operator turns and assistant-visible text. Read first.
- `.json` — the canonical `tapes-session/1` object plus turns, cost, tokens,
  and the session directory's git head and branch when they resolve. Query
  selectively with `jq`.
- `.trace.md` — complete reasoning and tool chronology, for grepping. Each
  tool turn is headed by its tool name, with the harness's raw envelope kept
  beneath it.

Read context first, query the JSON narrowly, and reach for the trace only
when free-text search is genuinely easier. Never ingest a whole bundle
because it exists.

Stdout is a manifest of exactly those three paths and their sizes; nothing
else goes there. Each file is written under a temporary name and renamed, so
a bundle never looks complete while it is half written.

## Integrating

`tapes` is meant to be the one reader of transcript stores on a machine. If you
are writing a tool, a skill, or an agent instruction that needs a session, plug
into it here rather than reaching into a store yourself.

**Discovery.** The binary is on `PATH`. There is no config file, no daemon, and
no environment to prepare — `tapes list --here --limit 5` works from any
directory and answers "what ran here recently". Harness stores are located by
the backends, so a caller never needs a path. An agent that has never used the
tool runs `tapes` bare and gets the same briefing this section describes,
which is why an instruction file can point at the command instead of
restating it.

**The cheap probe first.** `tapes show <id> --tail 40` answers "is there
anything here worth having?" without exporting. Reach for `export` only after
that says yes; a bundle costs context, and the tail usually settles it.

**Contracts you can build on.** `tapes-list/1` and `tapes-session/1` are
versioned JSON; a breaking shape change bumps the version. `export` prints
exactly three paths and their sizes on stdout, in reading order, and writes each
file under a temporary name before renaming — so a bundle is never observed
half-written. A harness whose binary or store is absent reports itself
unavailable and never fails the command, which means a caller can run against
any subset of harnesses without branching on what is installed.

**Two signals worth reading rather than ignoring.** A `truncated` transcript is
a window, not the whole session. Transcript `notes` carry what the normalized
model has no field for — abandoned branches, subagent transcripts, skipped
unparseable lines, page boundaries. Either one means a caller's picture is
partial, and a tool that reports a conclusion without checking them will state
more than it knows.

**Do not write a second parser.** Four separate extractors preceded this tool
and drifted apart; that drift is the reason it exists. If `tapes` cannot express
something you need, add it here — a caller that hand-parses a store is a fifth
extractor with the same future.

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
