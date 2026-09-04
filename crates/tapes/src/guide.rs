//! The bare-`tapes` guide: what a session needs before its first command.
//!
//! `--help` is the reference — every command, every flag. This is the
//! orientation: what tapes owns, the order the commands are meant to be used
//! in, and the judgment that decides what to do with what comes back.
//! Anything belonging to one command's contract stays in that command's
//! `--help`.

pub const GUIDE: &str = r#"tapes — read and export coding-agent sessions across harnesses.

A session is a recording, and each harness keeps its own in a private store:
claude and codex as JSONL files, opencode behind its stable database or beta
API, pi as an append-only tree. tapes finds one, normalizes it into a single model, and
hands it back. It never writes to a harness store, so no command here can
disturb a live session.

START WITHOUT AN ID
  tapes show --latest --tail 40  The newest session of this project.
  tapes show --latest --exclude <your own id>
  tapes export --latest

  --latest takes the most recent session in scope, so nothing has to read a
  table of ids and choose between them. The scope is the
  project holding the current directory: every worktree of its repository, or
  the directory's subtree when it is not in one. --project <path> asks about
  another project and --global drops the scope entirely.

  "Most recent" is settled by recorded activity across the newest few sessions
  each store offers — but which ones those are is the store's own answer: file
  time for the file-backed harnesses, one page of the API for opencode.
  Establishing it independently would mean reading every session, so a
  transcript whose file time was disturbed by a restore, or an opencode
  session past that page, can fall outside the window. When it matters
  exactly, list and name the id.

  Asking from inside a live session usually returns that session — it is the
  newest one there. Nothing in a store distinguishes the session asking from
  the session that just died, so pass --exclude <id> for any session you
  already hold. A harness that tells an agent its own session id makes this
  exact; without one, read the first turns and check whose they are.

FIND IT
  tapes list                     Every available harness, newest first.
  tapes list --here              This project, across every worktree.
  tapes list --harness codex     One backend.
  tapes list --search <text>     Recent content, across matching sessions.

  --limit bounds each harness and defaults to 20. The scope applies first, so
  a scoped listing cannot be emptied by a bound spent on other projects. An
  empty scoped list means the search found nothing — unless it says it stopped
  early, which is a different fact and is reported when it happens.

  --model <substring> matches case-insensitively against the full model
  identity shown in MODEL: `id (variant)` when a variant exists. A session
  without a model never matches. --directory <substring> matches
  case-insensitively against the recorded directory path; a session without a
  directory never matches. Both filters are applied before the per-harness
  limit and compose with --harness and the scope flags.

  --since and --until filter `last_activity_at`, the newest recorded activity,
  with the half-open rule `since <= last_activity_at < until`, before the
  per-harness limit. --sort newest|oldest orders by that clock and decides
  which sessions the limit keeps: the newest of each harness, or the oldest,
  which inspects every candidate the scan reaches as --search does. Ties use
  session id ascending, then harness ascending.

  --search <text> matches case-insensitively against the last 32 normalized
  turns in each candidate session. The fixed tail keeps a listing bounded, so
  a non-match means only that those recent turns did not contain the text. It
  is applied before the per-harness limit. A session whose bounded read fails
  is omitted from sessions and named in unsearched instead of being treated as
  a non-match; a preflight notice is written to stderr before scanning, and a
  stopped candidate scan is still reported as scan_truncated.
  If the local OpenCode2 API server cannot start or list candidates, search
  falls back to CLI API GETs and records the failed stage and diagnostic in
  unsearched.

  File-backed harnesses may show a bounded first-meaningful-user-turn hint
  prefixed with ~ when a harness recorded no title. JSON keeps that hint as
  derived_title and leaves the recorded title absent; derived_title_truncated
  says whether the hint was shortened. OpenCode's API-backed
  listing does not fetch messages merely to invent titles, so title-less OpenCode
  title metadata stays absent in list, show, and export. Human timestamps use whole
  RFC 3339 seconds with Z; JSON keeps recorded precision. show compares store
  activity with the newest rendered turn at that same whole-second precision.
  Whenever cost or tokens are present, JSON also carries accounting stating
  whether the figures are a recorded total or a sum of requests and whether
  they cover the session or only the bounded read window.

  A session is named by its full id or an unambiguous prefix; an ambiguous
  prefix lists its candidates and fails rather than guessing. A harness whose
  binary or store is absent reports itself unavailable and never fails a
  listing. If one stored session row cannot be read, listing keeps every other
  session and reports that one as unreadable, with its id and the diagnostic —
  a separate fact from an unavailable harness, since the store itself was fine.
  A content search that cannot read one candidate's bounded tail reports that
  session separately in unsearched. A Codex row without model metadata can
  mean its model-bearing turn_context was before the bounded file tail; JSON
  leaves model absent rather than inventing one.
  Show and export still fail when the session they were given cannot be
  resolved.

  A session recorded in a directory that no longer exists cannot be placed in
  any project, so a scoped search will not find it. Its id still resolves.

  When the optional harness-status command answers, list puts matching
  sessions' working or idle state in its separate LIVE column, and the JSON
  form carries the same `live` field. A missing, failed, malformed, oversized,
  or status snapshot slower than 250 ms leaves that field out and does not
  change retrieval.
  An unrecognized state for one thread is ignored while recognized states for
  other threads remain usable. show applies the same join to its header.

PROBE BEFORE EXPORTING
  tapes show <id> --tail 40      A window, costing no bundle.
  tapes show <id> --json         The same turns as tapes-session/1.
  tapes events <id> --json       Typed tool calls, results, and pairs.
  tapes usage <id> --json        Tokens, cost, and turn counts.

  show returns the last 100 turns unless --tail says otherwise, and marks the
  result truncated whenever it dropped any. It is the probe, not the archive;
  export is what takes every turn the reader could reach. JSON says why under
  `truncation`: a `window` names how many turns were returned and how many
  earlier ones the --tail bound omitted, which a larger --tail or export
  recovers; `source` lists bounds the reader itself reached (a file tail, a
  store page, cut turn text), which no request through tapes reaches past.
  Human output says the same in its closing notes, recommending only the
  recovery that works.

  events projects harness-neutral tool records as tapes-events/1. Pairing is
  exact within the bounded read; an incomplete call or result says whether its
  counterpart was not reached or not recorded. Event ordinals are the same
  turn coordinates show prints. With no --tail, every event the bounded reader
  reaches is returned; --name and --call-id filter only after pairing.

  usage answers where a session's quota went as tapes-usage/1: its recorded
  tokens, cost, and accounting, and its turns counted by role. Read accounting
  before adding anything up — basis says whether a figure is a recorded total
  or a sum of per-request records, and coverage says how much of the session it
  covers, which is the same coverage the turn counts carry. Cost is only what
  the harness recorded, and quota is a separate fact about the account rather
  than about this session. Facts beyond the counters appear only where a
  harness records them: codex reports its context window and quota windows,
  claude reports wall-clock durations and a per-model split, pi and opencode
  report neither.

  Every turn carries an `ordinal`, its zero-based place in the session's
  normalized sequence. On a file-backed session (claude, codex, pi) it is kept
  under any window: `--tail 1` returns the turn with the last ordinal, and the
  window names the range it holds. On an OpenCode API session the read is
  paged, so a window that stopped fetching (`omitted_exact: false`) numbers
  from the oldest turn it fetched and a wider request renumbers; there the
  durable coordinate is `native_id`, which OpenCode always records. With the
  harness, the session id, and `session.store` (where tapes read it from,
  opaque) that is what to write down when filing something a session
  produced. Human output prints the ordinal in each turn heading.

  When a backend can verify a non-turn record after the newest rendered turn,
  show names that trailing record's kind and timestamp. JSON carries the
  optional `trailing_record` object; source timestamps that are absent stay
  absent rather than being inferred.

  Live state is a present-tense annotation, not part of an export bundle.
  Export reads only the recording, so a status authority that is unavailable
  or changes cannot alter the rescue files.

  Retrieval is cheap; ingestion is not. The tail usually settles whether a
  dead session holds anything worth having. Rescue one that holds something
  expensive and not otherwise recorded: a design settled after real argument,
  a diagnosis that took many probes, a half-applied migration, the exact state
  of a long edit sequence. Start fresh when the session never reached a
  decision worth keeping, when its approach was already failing, or when
  redoing the task is faster than reconstructing it.

EXPORT, THEN INGEST PROGRESSIVELY
  tapes export <id|--latest> [--bundle <dir>]

  Three files share one timestamped prefix, and stdout is exactly their paths
  and sizes, in the order they are meant to be read:

  1. .context.md, whole. Operator turns and assistant-visible text — the
     session's argument, and the small part of it. The harness's own commands,
     notices, and attached context stay out of it and remain in the trace.
  2. .json, narrowly, with jq. The canonical object plus turns, cost, tokens,
     and the session directory's git head and branch when they resolve. Query
     it for facts; do not print it.
  3. .trace.md, only when free-text search is genuinely easier than JSON.
     Every reasoning and tool turn the transcript carries, in order. A tool
     turn is headed by the tool's name where its envelope carries one, by
     `result` for a bare result, and by `unnamed` otherwise.

  A bundle holds what the transcript held, and reads are capped, so a large
  session exports as a window and says so.

  Never ingest a whole bundle merely because it exists. A large trace read in
  full buys little over the context file and costs the budget the actual work
  needs.

READ THE ENDING
  tapes reports what a transcript contains and never classifies why it ended.
  That inference is yours, and it decides the shape of the handoff.

  Last turn is an operator turn  The ask was never processed; answer it first.
  Last turn is control or notice  The harness recorded its own message after
                                 the last exchange. The ending is decided by
                                 the turn before it.
  A tool call, no result         The call may have left side effects. Name the
                                 command and whether its result was ever seen.
  Results, then no text          Results landed but were never narrated. The
                                 successor must re-read them; no summary of
                                 them was ever written.
  A summary, then a detail jump  A compaction. The working detail is gone, so
                                 the handoff is what restores it.
  An error or limit message      A crash or a quota cut. The cut is arbitrary
                                 and often mid-tool, so say what was in flight
                                 and whether it landed.
  A closing summary, nothing open  Finished. A post-mortem, not a rescue.

  Every turn carries a `kind`, filled only from fields the harness itself
  wrote: `operator` for a message addressed to the agent, `control` for a
  harness command such as `/exit`, `notice` for a message the harness injected,
  `ambient` for context it attached, and the role's own name for assistant,
  reasoning, and tool turns. A user turn the harness left no evidence for is
  `unknown`, which is an answer rather than a gap — read its text before
  deciding what it was.

  A session the user deliberately aborted is a frozen handoff whatever its
  last turn looks like: report what was abandoned, and do not resume it on
  your own initiative. When two readings fit, take the more cautious one —
  telling a successor that a tool call may have left side effects costs a
  sentence, and not telling it costs a duplicated migration.

SIGNALS THAT THE PICTURE IS PARTIAL
  A truncated transcript is a window, not the session, and `truncation` says
  which kind: a turn window you can widen, or a source bound you cannot.
  Transcript notes carry
  what the normalized model has no field for: pi's abandoned branches, where a
  large remainder means the user changed direction; a claude session's
  subagent transcripts, which live in separate files and whose endings are not
  the parent's; lines that would not parse; an opencode page boundary. Codex
  reasoning is frequently encrypted and arrives as `[encrypted reasoning]`, so
  that placeholder marks reasoning you cannot read rather than reasoning that
  did not happen. Read both signals and state what is missing rather than
  writing over the gap.

CARRY IT FORWARD
  Bundles are working material, not artifacts — leave them in /tmp. What
  belongs in the project journal is the distillate: the decision, the
  diagnosis, the next action, the exact branch and head, written with
  `arc journal note <topic> --kind handoff`. Never copy a bundle, a raw
  transcript, or a trace into a journal entry, a commit message, or a pull
  request.

EXPORTED BUNDLES ARE UNREDACTED
  tapes redacts nothing, and a transcript holds whatever was pasted or printed
  into it: tokens captured in a tool result, credentials echoed by a command,
  personal data the user shared. Keep bundles in /tmp, attach them nowhere,
  and quote a conclusion rather than the raw material. Nothing upstream
  filters this.

ONE READER PER MACHINE
  Do not hand-parse a transcript store. Four separate extractors preceded this
  tool and drifted apart; if tapes cannot express what you need, add it here.

  tapes <command> --help for a command's full contract.
"#;

pub fn print() {
    print!("{GUIDE}");
}
