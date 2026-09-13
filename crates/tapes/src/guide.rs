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
  tapes show <id> --json         The same turns as tapes-session/3, with bounded read and terminal evidence.
  tapes events <id> --json       Typed tool calls, results, pairs, and read boundaries.
  tapes usage <id> --json        Tokens, cost, quota observations, and turn counts.
  tapes stats <id> --json        The same recording, counted with its read evidence.
  tapes lineage <id> --json      The sessions this one names as relatives.
  tapes brief <id> --json        What a continuation of it needs.
  tapes usage --here --json      The same counters summed over a project.
  tapes endings --here --json    What each session of a project ends on.

  show returns the last 100 turns unless --tail says otherwise, and marks the
  result truncated whenever it dropped any. It is the probe, not the archive;
  export is what takes every turn the reader could reach. JSON says why under
  `truncation`: a `window` names how many turns were returned and how many
  earlier ones the --tail bound omitted, which a larger --tail or export
  recovers; `source` lists bounds the reader itself reached (a file tail, a
  store page, cut turn text). Wider turn windows retain source bounds; use
  explicit page reads to reach older Claude or Codex file history. `read`
  records the source length, configured bound, physical head/tail ranges,
  decoded record spans, and gaps; alignment bytes are not normalized
  coverage. A terminal observation records only native stop fields reached by
  the read, and never says that the session is stopped now. An empty text tail
  carries the reason it has no operator or assistant text.
  Human output says the same in its closing notes, recommending only the
  recovery that works.

  events projects harness-neutral tool records as tapes-events/2. Pairing is
  exact within the bounded read; an incomplete call or result says whether its
  counterpart was not reached or not recorded. Event ordinals are the same
  turn coordinates show prints. With no --tail, every event the bounded reader
  reaches is returned; --name and --call-id filter only after pairing.

  usage answers where a session's quota went as tapes-usage/2: its recorded
  tokens, cost, and accounting, and its turns counted by role. Read accounting
  before adding anything up — basis says whether a figure is a recorded total
  or a sum of per-request records, and coverage says how much of the session it
  covers, which is the same coverage the turn counts carry. Cost is only what
  the harness recorded, and quota is a separate fact about the account rather
  than about this session. Facts beyond the counters appear only where a
  harness records them: codex reports its context window and quota windows,
  claude reports wall-clock durations and a per-model split, pi and opencode
  report neither. Codex quota observations preserve credits balances and
  reached-limit flags exactly as recorded, including false and string zero;
  absent fields remain unknown.

  stats counts what one recording holds as tapes-stats/2: turns by kind, tool
  calls by name with their paired durations and error counts, unpaired calls
  by the boundary that left them unpaired, the recorded clock, the session's
  token counters with the share of input plus cache read plus cache write its
  cache accounts for, and the children its store names. Every figure is a
  count of records the harness wrote, and every total says what it covers:
  turn coverage is read-window when a source bound withheld turns, a duration
  comes from a pair the read holds both halves of, and a cache ratio divides
  recorded token counts rather than cost, within one harness's own convention
  for what its input counter already includes. warnings names the limits of
  the read behind the figures. What a count means for the work is the reader's
  inference: nothing here calls a call wasteful, explains a latency, or says
  why a session ended.

  lineage answers which sessions a recording names as relatives, as
  tapes-lineage/1: the session it was spawned or forked from, and the children
  its own store records, each with the role, model, spawn and completion
  stamps, and outcome its harness wrote. A relationship exists only where a
  record states it — a child header naming a parent, a spawn or completion
  event, a transcript file under the parent's directory, a session row's
  parent column. Nothing is inferred from directories, titles, or times, and a
  reference the store cannot resolve is kept with resolved: false, because a
  child whose recording is gone is exactly what a reader is looking for.
  A parent refers to a child and never absorbs it. Use `show` with an ordinary
  child session ID, or `child PARENT --reference CHILD` for a Claude subagent.
  Claude child references do not become ordinary session IDs.
  What each harness records differs: claude names its subagent transcripts and
  the Agent calls that spawned them, codex joins spawn and wait calls to the
  rollout headers naming this session as their parent, opencode reads the
  parent column on either projection, and pi carries a parent reference on the
  child alone.

  Given a scope or a listing filter instead of a session, usage answers that
  whole selection as tapes-usage-summary/2, using the flags list and export
  take and grouping by --by (harness and model unless told otherwise). It sums
  the counters the listing already carries, so nothing is re-read. Read
  counted before a sum: it says how many of a group's sessions recorded that
  counter, and a counter none recorded is absent rather than zero. coverage
  counts the sessions behind a sum by their accounting, including those whose
  harness recorded nothing to sum, and cost is summed only where a harness
  recorded one.

  Every turn carries an `ordinal`, its zero-based place in the session's
  normalized sequence. On a file-backed session (claude, codex, pi) it is kept
  under any window: `--tail 1` returns the turn with the last ordinal, and the
  window names the range it holds. On an OpenCode API session the read is
  paged, so a window that stopped fetching (`omitted_exact: false`) numbers
  from the oldest turn it fetched and a wider request renumbers; there the
  durable coordinate is `native_id`, which OpenCode always records. With the
  source descriptor, the session id, and `source.location` (where tapes read
  it from, opaque) that is what to write down when filing something a session
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
  tapes export --global --since 2026-01-01 --until 2026-01-08

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

  Given the listing flags instead of an id — the scope flags, --harness,
  --model, --directory, --since, --until, --sort, --limit, --search — export
  takes the set list would return, in the same order, and writes one bundle
  per session. Bundles are never joined: each session keeps its own bounded
  three files, and manifest.json beside them is the only file spanning the
  set. It records the selection, every bundle's paths, the sessions whose
  store could not be read, and the listing's own diagnostics, so the set can
  be audited against the store. A session that fails costs its own bundle and
  nothing else.

  Never ingest a whole bundle merely because it exists. A large trace read in
  full buys little over the context file and costs the budget the actual work
  needs.

READ MANY ENDINGS
  tapes endings --here --since 2026-01-01 --json
  tapes endings --global --harness codex --limit 50 --text

  endings answers what each session of a selection ends on, as
  tapes-endings/2, so choosing which few endings deserve reading costs one
  bounded read each instead of a transcript apiece. The selection is the one
  list and export take, and the scope and filters apply before any transcript
  is opened. --tail sets how many of each session's newest turns are read (12
  by default) and --text adds the operator and assistant text of those turns,
  cut at 400 characters.

  It states facts and classifies nothing. Each rests on the normalized turn
  kinds and typed tool events of the turns that were read, never on their
  text: operator-turn-after-assistant for a request nothing answers,
  control-turn-last or notice-turn-last for a harness command or injected
  message recorded after the last exchange, call-without-result for a call the
  read never saw a result for, results-without-narration for results no turn
  narrates, assistant-close for a closing assistant turn. More than one can
  hold at once. What the ending means — and whether it is worth rescuing — is
  the reading below, and it is yours.

  incomplete says what the read did not establish, and it qualifies every fact
  beside it: read-window and tail-window for turns the read or the window did
  not reach, kind-unknown for a user turn the harness left no evidence for,
  no-timestamps for an order resting on the normalized sequence alone. lineage
  counts the relatives a store records without reading any of them.

  source is what to write down when filing a follow-up: the harness, the
  session id, the last read turn's ordinal and native id, and coverage saying
  how much of the session the read covered. It carries no transcript text, so
  it can be quoted anywhere the conclusion can. A session whose read fails is
  named in unread with its diagnostic and does not stop the report.

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
  which kind: a turn window you can widen, or a source bound retained by
  ordinary show/export reads. Explicit page reads have their own byte budget.
  Transcript notes carry
  what the normalized model has no field for: pi's abandoned branches, where a
  large remainder means the user changed direction; a claude session's
  subagent transcripts, which live in separate files and whose endings are not
  the parent's; lines that would not parse; an opencode page boundary. Codex
  reasoning is frequently encrypted and arrives as `[encrypted reasoning]`, so
  that placeholder marks reasoning you cannot read rather than reasoning that
  did not happen. Read both signals and state what is missing rather than
  writing over the gap.

CONTINUE A COLD SESSION
  tapes brief <id|--latest>
  tapes brief <id> --tail 20 --json

  A continuation has two halves. brief is the transcript's half, as
  tapes-brief/2: where the session stopped, the directory it worked in and the
  commit that directory sits on, the tool calls the read never saw a result
  for, the children whose outcome its store does not record, and the last few
  operator and assistant turns, each cut at 600 characters. --tail sets how
  many of those turns are rendered (12 by default), and everything else about
  the session — its harness, id, model, counters, and the coordinate to quote
  — comes with it.

  The other half is the project's own record of the work: `arc catchup` for
  what the change has reached, `arc resume` for the thread to pick up. The
  journal holds the decisions and the reasoning, which is why a continuation
  reads it; the transcript holds what nobody filed, which is why a
  continuation reads the brief. Read both and join them yourself: brief reads
  the recording alone, so it knows nothing the journal wrote and judges
  nothing it reads.

  It reports what is open; it does not resume anything. A call with no result
  may have left side effects, a child with no recorded outcome may still be
  running, and a working directory that is gone is stated as gone rather than
  left blank. What is uncommitted in that directory now is present-tense state
  to check for yourself — the brief reports the recording, not the machine.

  Resuming the session in place is cheaper than any of this while its cache is
  warm; a brief is what a cold session is worth reading through instead.

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

CHILD RECORDINGS
  tapes lineage PARENT --json
  tapes child PARENT --reference CHILD --tail 40 --json
  Read a Claude child's own transcript, usage and ending with its parent and
  reference retained. It is not an ordinary listed session; nested lineage
  remains uninspected and child activity is never added to parent totals.
HISTORICAL READS
  tapes page SESSION --bytes 65536 --json
  tapes history-search SESSION --search TEXT --pages 8 --json
  tapes metadata SESSION --pages 8 --json
  Claude and Codex history is paged backward, with chronological turns within
  each page. Pass next_cursor back as --cursor. Changed sources refuse;
  malformed and oversized record gaps remain explicit. Budgets protect context:
  1 KiB–4 MiB per page, 1–32 pages per search, 100 excerpts/observations.
  Metadata pages extract model observations without decoding transcript turns
  or reading operator-provenance context. Older model observations never
  silently replace current session metadata.
TOOL USAGE OVER A SELECTION
  tapes stats --here --since 2026-01-01 --json
  Counts recorded tools by harness and name, retaining each session's read
  coverage and failures. These are tool calls, not inferred shell commands.
  A single ID or --latest keeps the single-session stats view.
RECORDED TITLE
  tapes show --title "Exact recorded title" --harness claude
  The same selector works on brief, usage, stats, lineage, events and export.
  It defaults to this project; --global or --project changes scope. Derived
  display hints never match. An incomplete lookup reports observed candidates
  without claiming uniqueness; choose an explicit ID or narrower scope.

"#;

pub fn print() {
    print!("{GUIDE}");
}
