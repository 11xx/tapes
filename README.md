# tapes

Read any coding agent's recorded sessions, and hand them to the next one.

When a coding agent's session dies mid-task, the work it did survives only in
the harness's own recording, in a format no other harness reads. `tapes` reads
the recordings of Claude Code, Codex, OpenCode and Pi through one model. It
tells you where a session stopped, its recorded directory and that directory's
current checkout commit, which tool calls never got a result, and what it cost.
It exports the session as a bundle a fresh session in any harness can read.

The same reader answers questions across sessions: which ones mention
something, what each ended on, where the tokens went. With `--input` it reads
ChatGPT and Perplexity exports, and with `--remote` it asks another machine's
`tapes` over ssh. To read one materialized native store, use
`--native-store claude --native-root archive/claude` (also `codex` or `pi`).
The root contains Claude's `projects` or Codex/Pi's `sessions`; selection never
falls back to installed stores. It only reads: no command writes to a harness's store, so it
is safe to point at a session that is still running.

Select recorded tool calls across sessions with `tapes events --here --tool
bash --command 'cargo test'`. Matching includes paired results and recorded
exit status. Add `--count` for per-session counts, or `--full` to stream whole
recordings. Selection JSON uses `tapes-events-selection/2` with per-session
counts and partial-read status; event objects use `tapes-events/14`.

## Install

`tapes` builds to a single binary, supported on Unix-like systems, and needs
Rust 1.97 or newer.

```sh
cargo install --git https://github.com/11xx/tapes agent-tapes --locked
```

Add `--tag vYYYY.M.D` to install a release, and `--features zip` to read ZIP
exports. From a checkout, run `cargo install --path crates/tapes --locked`.
The package is named `agent-tapes`; the installed command is `tapes`.

## Pick up a session that stopped

```sh
tapes show --latest --tail 40         # the newest session here, its last 40 turns
tapes brief --latest                  # where it stopped: directory, commit, unanswered calls
tapes export --latest --bundle out/   # a bounded bundle another session can read
```

`--latest` is the newest session in the project you are in, across every
worktree of its repository, so picking one up needs no session id. A bundle
carries whatever the transcript holds, including anything pasted into it;
`tapes` redacts nothing.

## Page a growing recording

```sh
tapes read <id> --items 40 --bytes 1MiB --kind user,assistant --json
tapes read <id> --before '<start>' --kind user,assistant --json
tapes read <id> --after '<end>' --kind user,assistant --json
```

`tapes-recording-page/1` carries flat typed items: `start`, `end`, `kind`,
`role`, complete normalized `text` for messages/reasoning, `ts`, native and
request IDs, recorded model/effort and origin, complete `tool` fields, native
`content`, and parent/message links. Store opaque gap positions verbatim across
processes. `--after` includes the item at the gap; `--before` excludes it. Keep
`end` to follow appends. Claude children use `parent::agent`, page separately
with `--reference`, and link their parent. An item's identity is its `start`;
`native_id` identifies its native record/entry and can be shared by many parts.
`message_id` groups parts of one native message: Claude assistant `message.id`
or user record UUID, Codex message `payload.id` (absent on reasoning/tools), Pi
entry ID, OpenCode message ID. `parent_id` links Claude/Pi tree entries; on
OpenCode it links the containing message to its parent message (usually the
user prompt). Codex omits it.

`tool` exposes `name`, `status`, `title`, `metadata`, `caller`, `arguments` and
`output` without harness-specific paths. Codex search arguments include the
search action/query; OpenCode title/metadata, Pi result details as metadata,
and Claude caller are retained. These payload fields share the native content
allocation in Rust. Status is `running`, `completed`, `failed`,
`never-finished` or `unknown`; native words remain in `content`. A call without
a recorded outcome is running; a result without an error is completed.
OpenCode text/reasoning also has item `status` when recorded. Missing fields
stay absent or null; complete native `content` is always present.

`--bytes` charges native `content` once plus serialized item metadata, measured
with a counting writer. Normalized text and tool argument/output projections
share that payload budget; their duplicate wire representations and the page
envelope are additional bytes. An oversized first item refuses without clipping
and supplies `needed`, `start` and `end`, so a caller can enlarge the budget or
skip it. `--read-bytes` bounds source scanning (default 8MiB, maximum 64MiB).
A filter fills the item budget until a source, output or scan bound stops it.
`stop` names the boundary; `start`/`end` retain progress. File reads add a bounded
identity header (at most 1MiB) and verify the source window once. Positions bind
the inode, the preceding newline and the digest of the preceding record's last
min(record length, 4KiB) bytes; internal gaps also bind their whole record.
Anchors consume scan budget. Incomplete final lines wait for a newline.
`stop: "pending"` means wait for a source change on both files and databases;
`pending_bytes` describes a file suffix and `pending` names a database part.
Positions do not pin untouched history.

Claude normalizes each record independently. Codex messages page by content
part; runtime tool mirrors remain separate occurrences labelled `runtime`.
Pi pages the physical tree log, including abandoned branches, with `parent_id`;
its active branch needs a whole-recording ancestry pass. OpenCode's stable
database pages parts by `time_created,id`. Only `text` parts are prose;
step/patch metadata is `unknown`. An unfinished assistant text/reasoning or
pending/running tool stops the stream only while its message is the newest
message and has neither completion time nor error. Tail pages end below that
frontier. Older unfinished parts are published as recorded with
`never-finished` status, so abandoned turns cannot hide resumed turns.
Positions bind published parts; ordinary completion can be followed once.
A changed published part refuses.

An OpenCode native part is bounded by the smaller of `--read-bytes` and 4MiB;
the transport cap is 8MiB. Each page captures one batch and verifies it in one
query: at most three SQLite processes for a tail and four for a positioned
read, excluding discovery. The native part indexes are on `session_id` and
`message_id,id`; SQLite scans/sorts a session for ordered pages. Response bytes
are bounded, while database engine work scales with the session. The OpenCode
API and supplied JSON/ZIP exports have no supported recording positions.

In `agent-tapes-core`, `recording::Recording::resolve` uses discovery's bounded
identity openings. `from_native` reuses a discovery result; `from_session`
reuses a file session. `tail`, `before` and `forwards` take
`Limits { items, bytes, scan_bytes, kinds }`. Public `Refusal` values identify
oversized items, scan exhaustion (with `needed` when the sufficient budget is
known), changed content, replaced recordings, unsupported stores, invalid
input/positions and unavailable stores. A deleted retained file is replaced;
an internal truncated record is changed. Busy/unreadable stores are unavailable;
busy reads retain exit 75 for retry. CLI JSON errors use `tapes-recording-error/1` with the same
`refusal.kind` values and a nonzero exit. `Kind`, `Stop` and `Refusal` are
non-exhaustive. Rust JSON readers deserialize unfamiliar kinds/stops to
`Unknown`. JSON readers retain unknown kinds as non-prose. An unknown stop
still permits consuming the returned items and saving its boundaries: continue
in the requested direction if that boundary advanced; otherwise wait for a
source change. Only known `end` means caught up; never spin on an unchanged
boundary. An unknown refusal requires an explicit recovery policy. Readers
retain no global state and never
write to a recording or store.

## Look across sessions

```sh
tapes list --here                         # this project's sessions, every harness, newest first
tapes list --search "rate limit"          # sessions whose recent turns mention it
tapes list --search "rate limit" --search-turns all  # every turn in the bounded read
tapes show <id> --only operator --origin person # input with recorded person origin
tapes list --launch interactive           # sessions a person drove, not headless runs
tapes endings --here --since 2026-09-01   # what each session ended on
tapes usage --here --by model             # where the tokens and the cost went
tapes timeline <id> --json                # turn ordinals, kinds and timestamps
tapes stats <id> --idle 45m               # active time, stretches, tools, cache share
tapes list --remote build-box             # sessions on another machine, read over ssh
```

For a growing Claude or Codex recording, start `tapes page <id> --pin --bytes
1MiB --json`, then continue with its `next_cursor` as `--cursor` alone.
`history-search` and `metadata` also accept `--pin`. One bounded-memory pass
hashes a prefix up to 1 GiB; later pages verify their chunks and ignore appends.
Edits refuse when their chunk is reached; replacement and truncation refuse
immediately. The guide explains verification costs and the captured-copy
fallback for bytes that must survive a rewrite.

Without `--here`, a listing covers every session on the machine. Every command
that reads sessions, except `export`, takes `--json`, and each JSON answer
names its versioned schema. Claude Code and Codex record how each session was launched; Pi does
with `tapes install pi`, and OpenCode with `tapes install opencode`. These
integrations write one owned file into the harness configuration. OpenCode
records `tui` and `run` in session-bound metadata at creation or the first
prompt after resuming. TUI evidence wins; inherited fork metadata is ignored.
Unrecognized runtimes leave launch mode absent. `tapes uninstall <harness>`
removes only a tapes-managed regular file. Restart OpenCode after installation
or removal.

`list --search <text> --search-turns <N|all>` searches normalized turns in
bounded reads (default: 32 turns). `all` covers every retained turn within the
reader's byte and record bounds. Partial misses are reported in
`partial_search`; matches carry no search diagnostic. Unreadable searches appear
in `unsearched`. Human
search diagnostics group by harness and reason with counts. JSON retains
per-session details; OpenCode API refusals describe the store once.

Operator turns carry `origin` only with positive sender evidence. Headless
sessions assign `program`. Claude identifies `person` with `promptSource: typed`
and `harness` with `isMeta: true`; queued or unmarked input does not identify a
person. Pi identifies `person` from interactive user messages and `harness`
from custom messages. Codex records no sender for interactive input.
`show` and `export` accept `--origin person|program|harness`; absent origins
never match.

`list --search <text> --full` streams whole recordings after metadata and
scope filters. `--limit` caps sessions read globally; `full_search` reports
selection truncation and coverage, including nonmatches. `--search-matches`
caps matching turn coordinates per session (default 100), with a truncation
flag. Coordinates carry the whole-recording ordinal, kind, timestamp and
record reference. Whole coverage requires a read without gaps or bounds.

## Where the rest is

`tapes` with no arguments prints the workflow guide, and `tapes <command>
--help` is each command's full contract. A tool that only needs to find
sessions can depend on the `agent-tapes-discovery` library instead of the CLI;
see [its README](crates/tapes-discovery/README.md).

## License

[Unlicense](LICENSE) — public domain.

Persisted Claude tool output is read explicitly with
`tapes artifact <id> --reference <token> --bytes 64KiB --json`.
Use the opaque `reference` from `show --json` or `events --json` on the same
native source (including `--native-store claude --native-root <root>`).
The `tapes-artifact/1` response includes the bounded body, returned byte count,
truncation, SHA-256 of returned bytes, and `utf-8` or lossless `hex` encoding.
The maximum budget is 4 MiB. Retrieval refuses symlinks, non-regular files,
stale results, and references from another recording or source.
