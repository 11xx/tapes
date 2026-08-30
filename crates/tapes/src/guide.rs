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

  File-backed harnesses may show a bounded first-meaningful-user-turn hint
  prefixed with ~ when a harness recorded no title. JSON keeps that hint as
  derived_title and leaves the recorded title absent. OpenCode's API-backed
  listing does not fetch messages to invent titles, so title-less OpenCode
  metadata stays absent in list, show, and export. Human timestamps use whole
  RFC 3339 seconds with Z; JSON keeps recorded precision.

  A session is named by its full id or an unambiguous prefix; an ambiguous
  prefix lists its candidates and fails rather than guessing. A harness whose
  binary or store is absent reports itself unavailable and never fails a
  listing. If one stored session row cannot be read, listing keeps every other
  session and reports that one as unreadable, with its id and the diagnostic —
  a separate fact from an unavailable harness, since the store itself was fine.
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

  show returns the last 100 turns unless --tail says otherwise, and marks the
  result truncated whenever it dropped any. It is the probe, not the archive;
  export is what takes every turn the reader could reach.

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
     session's argument, and the small part of it.
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

  Last turn is a user turn      The ask was never processed; answer it first.
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

  A session the user deliberately aborted is a frozen handoff whatever its
  last turn looks like: report what was abandoned, and do not resume it on
  your own initiative. When two readings fit, take the more cautious one —
  telling a successor that a tool call may have left side effects costs a
  sentence, and not telling it costs a duplicated migration.

SIGNALS THAT THE PICTURE IS PARTIAL
  A truncated transcript is a window, not the session. Transcript notes carry
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
