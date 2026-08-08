//! The bare-`tapes` guide: what a session needs before its first command.
//!
//! `--help` is the reference — every command, every flag. This is the
//! orientation: what tapes owns, the order the commands are meant to be used
//! in, and the judgment that decides what to do with what comes back.
//! Anything belonging to one command's contract stays in that command's
//! `--help`.

pub const GUIDE: &str = r#"tapes — read and export coding-agent sessions across harnesses.

A session is a recording, and each harness keeps its own in a private store:
claude and codex as JSONL files, opencode behind an HTTP API, pi as an
append-only tree. tapes finds one, normalizes it into a single model, and
hands it back. It never writes to a harness store, so no command here can
disturb a live session.

FIND IT
  tapes list                     Every available harness, newest first.
  tapes list --here --limit 5    What ran in this directory recently.
  tapes list --harness codex     One backend.

  A session is named by its full id or an unambiguous prefix; an ambiguous
  prefix lists its candidates and fails rather than guessing. A harness whose
  binary or store is absent reports itself unavailable and never fails the
  command, so a caller never has to branch on what is installed.

PROBE BEFORE EXPORTING
  tapes show <id> --tail 40      A window, costing no bundle.
  tapes show <id> --json         The whole transcript as tapes-session/1.

  Retrieval is cheap; ingestion is not. The tail usually settles whether a
  dead session holds anything worth having. Rescue one that holds something
  expensive and not otherwise recorded: a design settled after real argument,
  a diagnosis that took many probes, a half-applied migration, the exact state
  of a long edit sequence. Start fresh when the session never reached a
  decision worth keeping, when its approach was already failing, or when
  redoing the task is faster than reconstructing it.

EXPORT, THEN INGEST PROGRESSIVELY
  tapes export <id> [--bundle <dir>]

  Three files share one timestamped prefix, and stdout is exactly their paths
  and sizes, in the order they are meant to be read:

  1. .context.md, whole. Operator turns and assistant-visible text — the
     session's argument, and the small part of it.
  2. .json, narrowly, with jq. The canonical object plus turns, cost, tokens,
     and the session directory's git head and branch when they resolve. Query
     it for facts; do not print it.
  3. .trace.md, only when free-text search is genuinely easier than JSON.
     Complete reasoning and tool chronology, each tool turn headed by its
     tool name.

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
  reasoning is frequently encrypted, so a reasoning turn without text is not
  an absence of reasoning. Read both signals and state what is missing rather
  than writing over the gap.

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
