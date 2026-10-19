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

PICKING UP A SESSION'S WORK
  Short and cold: `tapes brief <id|--latest>` (CONTINUE A COLD SESSION below).
  Long, or with relatives, executors or delegated runs: `agent-delegate
  pickup --latest --here --harness <h> --backend <scout backend>`, which
  freezes it, fans cheap scouts over it and verifies their facts; bare
  `agent-delegate` (PICK UP A SESSION) carries the procedure.

SELECT A MATERIALIZED NATIVE STORE
  tapes list --native-store claude --native-root archive/claude --limit 5
  tapes show --native-store codex --native-root archive/codex --latest --tail 10

  --native-store <claude|codex|pi> requires --native-root <PATH>. PATH is the
  harness configuration root: Claude reads PATH/projects, Codex and Pi read
  PATH/sessions. Every session-reading command accepts the pair, including
  capture, child, full reads, history paging, aggregates, and export. The pair
  alone selects a collection for usage, stats, and export. Only this
  store participates; missing or unreadable roots never fall back to installed
  stores. Harness configuration environment is neither read nor changed.
  --input and --remote conflict with the pair. --harness may name the selected
  harness and refuses another. OpenCode uses an API or database, not a root,
  so --native-store opencode refuses.
  --native-file <PATH> selects the exact recording at the locator returned by
  list, alongside --native-store and --native-root. Only that file is inspected;
  a missing, unreadable or identity-mismatched file never selects another source.
  Repeated directory recording IDs choose the newest modification time, with the
  lexically first locator breaking ties, for both listing and exact lookup.
  Codex resumed filenames retain their canonical ID before the occurrence suffix.
  Discovery and read bounds remain explicit; a locator does not pin growing bytes.
  IDs and prefixes resolve only in the selected store. `self` uses the caller's
  exported identity but must resolve within that store. --latest and --title
  default to the selected store across all recorded projects; --here,
  --project, and --global give an explicit scope. A removed recorded working
  directory does not prevent direct ID or global-store reads.
  source.kind is explicit-native-store, source.location.container names PATH,
  and source.location.locator names the exact recording. Recording reads accept
  only regular opened files, without waiting for FIFO writers. Identity-bearing
  records must match the selected native session (the parent for Claude children,
  whose records must also name no agent but the child). Bounded reads and listings
  check the opening and retained tail; full reads check every record.
  Directory and raw-text prefilters validate the probed native identity before
  excluding a candidate; uncertain probes defer to normalization and its diagnostics.
  This is a selected
  occurrence, with no live annotation or claim of immutable bytes. capture
  reports caller-selected-native-store: the caller selected the native source,
  and tapes has not pinned its bytes.

START WITHOUT AN ID
  tapes show --latest --tail 40  The newest session of this project.
  tapes show --latest --exclude self
  tapes export --latest

  --latest takes the most recent session in scope, so nothing has to read a
  table of ids and choose between them. The scope is the
  project holding the current directory: every worktree of its repository, or
  the directory's subtree when it is not in one. --project <path> asks about
  another project and --global drops the scope entirely. If the bounded scan
  cannot read candidate rows or stores, the chosen readable session remains
  useful but every --latest view names the unreadable evidence and warns that
  newer activity may be hidden; a missing harness alone is not a warning.
  A candidate with no recorded activity timestamp makes --latest refuse
  rather than guess: file modification time is never substituted.

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
  already hold. self stands for your own session wherever an id does, read
  from the variable your harness exports (CLAUDE_SESSION_ID or
  CLAUDE_CODE_SESSION_ID, CODEX_THREAD_ID, OPENCODE_SESSION, PI_SESSION_ID);
  Pi adds PI_SESSION_FILE as the active recording and prefers it for its own
  id, while keeping configured session directories available for other ids;
  stderr says what it became, and it refuses when none is set or the store
  does not hold the id. Without one, read the first turns and check whose
  they are.

FIND IT
  tapes list                     Every available harness, newest first.
  tapes list --here              This project, across every worktree.
  tapes list --harness codex     One backend.
  tapes list --launch headless  Sessions with a recorded headless marker.
  tapes list --search <text>     Recent content, across matching sessions.

  --limit bounds each harness and defaults to 20. The scope applies first, so
  a scoped listing cannot be emptied by a bound spent on other projects. An
  empty scoped list means the search found nothing — unless it says it stopped
  early, which is a different fact and is reported when it happens. A session
  recorded in a directory that no longer exists (a removed worktree) cannot be
  placed in a project: the scoped list reports those directories as Not
  placed, and `tapes list --global --directory <substring>` reaches them.

  --model <substring> matches case-insensitively against the full model
  identity shown in MODEL: `id (variant)` when a variant exists. A session
  without a model never matches. --directory <substring> matches
  case-insensitively against the recorded directory path; a session without a
  directory never matches. --launch interactive|headless uses recognized
  launch evidence: Claude `cli` and `claude-desktop` entrypoints are
  interactive, `sdk-cli` is headless, and a recording with both is
  interactive; Codex `cli` and `vscode` sources are interactive, while `exec`
  and a `subagent` source are headless. Pi records no launch mode itself:
  `tapes install pi` adds an extension that writes a `tapes.launch` entry,
  naming the session it was written for, each time pi opens a session. `tui`
  is interactive, `print` and `json` are headless, `rpc` says neither, any
  `tui` entry makes the session interactive, and entries a fork copied from
  its parent are ignored. `tapes install opencode` adds a plugin under
  `$OPENCODE_CONFIG_DIR/plugins`, or `$XDG_CONFIG_HOME/opencode/plugins`
  (default `~/.config/opencode/plugins`). Restart OpenCode to load it.
  Its session metadata `tapes.launch` contains `session` and `modes`: `tui`
  is interactive, `run` is headless, and TUI evidence wins. A marker naming
  another session is ignored. Creation and the first prompt in a resumed
  session record the process mode. The plugin recognizes the process's
  `/cli/tui/worker` entrypoint path and `run` command argument, verified
  against OpenCode 1.18.33. A layout tapes does not recognize leaves the
  session without a marker; attached servers record nothing.
  Pi and OpenCode sessions without their integration carry no marker.
  Install and uninstall refuse foreign files, symlinks, and directories;
  `tapes uninstall opencode` preserves recorded metadata. For Claude, Pi,
  and OpenCode, a read that stops short of a recording keeps interactive evidence it reached but never
  concludes headless;
  `show --full` settles Claude and Pi; OpenCode streaming exports carry its
  complete session metadata. OpenCode listings read the complete launch marker
  independently of transcript windows. Codex's source is its opening record,
  so a bounded read settles it. Unknown or absent markers never match. These filters apply
  before the per-harness limit and compose with --harness and the scope flags.

  --since and --until filter `last_activity_at`, the newest recorded activity,
  with the half-open rule `since <= last_activity_at < until`, before the
  per-harness limit. --sort newest|oldest orders by that clock and decides
  which sessions the limit keeps: the newest of each harness, or the oldest,
  which inspects every candidate the scan reaches as --search does. Ties use
  session id ascending, then harness ascending.

  --search <text> matches case-insensitively against normalized turns in each
  bounded read. --search-turns <N> selects the last N turns (default 32);
  --search-turns all searches every retained turn. Byte and record bounds still
  apply. Search precedes the per-harness limit. Partial reads are searched:
  hits remain matches without a coverage diagnostic. partial_search names
  nonmatching sessions whose requested window was withheld by a source bound. A miss
  describes only the searched window. Read failures are named in unsearched.
  Human search diagnostics group by harness and reason with counts; JSON
  preserves per-session diagnostics. OpenCode API refusals name the store once
  with their reason. A preflight notice goes to stderr, and a stopped candidate
  scan is reported as scan_truncated. If the local OpenCode2 API server cannot
  start or list candidates, search falls back to CLI API GETs and records the
  failed stage and diagnostic in unsearched.

  --search <text> --full streams each selected recording through its whole
  reader, with memory following one record. Metadata, scope, harness, launch,
  and activity filters apply before reading. --limit bounds sessions read
  globally; full_search.selection_truncated reports candidates withheld or
  an incomplete candidate scan. --search-matches <N> caps matching turn
  coordinates per session (default 100); matches_truncated reports overflow.
  full_search.coverage includes nonmatches and states whole_recording_read,
  which requires no gaps, skipped records, trailing record, or source bounds.
  Ordinals are zero-based across the whole normalized recording. OpenCode uses
  its database or API full reader. Stores without a whole reader are named
  once in unsearched. --full conflicts with --search-turns.

  File-backed harnesses may show a bounded first-meaningful-user-turn hint
  prefixed with ~ when a harness recorded no title, cut at 96 characters. JSON
  keeps that hint as derived_title and leaves the recorded title absent;
  derived_title_truncated says whether the hint was shortened. OpenCode's API-backed
  listing does not fetch messages merely to invent titles, so title-less OpenCode
  title metadata stays absent in list, show, and export. Human timestamps use whole
  RFC 3339 seconds with Z; JSON keeps recorded precision. show compares store
  activity with the newest rendered turn at that same whole-second precision.
  Whenever cost or tokens are present, or the read counted requests that
  recorded no amount, JSON also carries accounting stating whether the
  figures are a recorded total or a sum of requests and whether they cover
  the session or only the bounded read window. A cost carries
  kind: harness-calculated-estimate and lists pricing_basis under missing:
  the harness priced it, and no recording says on which price table.
  Where a recording keeps more than one conversation path, its selection says
  which records the figures were summed over: pi's counters are
  `active-branch`, the last
  entry's ancestry, and cover the session only when the read linked that
  branch from its last entry to its root.

  JSON omits a fact the harness did not record rather than writing it as
  null, an empty string, or an invented default. `launch` appears only when
  the reader recognizes the harness's recorded marker; it is never inferred
  from `promptSource`, the command line, or the session's content. A session
  whose opening record the reader could not reach carries
  start_uncertain: true: it began at or before its started_at.

  A full session id uses direct native lookup. A recording file named for the id
  whose header records another session fails the lookup, naming the path and
  both ids, rather than reporting the session absent; a correct file for the id
  in the same store wins even after an intervening read failure. Every bounded
  exact filename candidate is inspected until a correct match is found.
  The filename/header rule also applies to Pi's explicit recording file;
  a correct directory recording takes precedence over its mismatch.
  An identity mismatch remains a failure
  when candidate enumeration finds only prefix matches. A prefix is accepted only when
  bounded candidate coverage proves it unique; an ambiguous prefix lists its
  candidates. A file-backed harness matches an id or prefix against the
  identities its walk reads from filenames and recording headers and reads
  only the matching recordings whole, so a recording another session is
  still writing never decides a miss; a candidate whose header it could not
  read keeps the miss from reading as absence. A whole read whose recording
  changes under it starts again, up to three reads. --harness <h> restricts ID and prefix lookup to that harness's
  stores. Lookup diagnostics retain the failure causes alongside incomplete
  and completely searched harness stores, distinguishing store generations
  when a harness has several. Incomplete coverage asks for the full id or
  --harness <h>; a single observed prefix
  match cannot prove uniqueness across incomplete stores. A harness whose
  binary or store is absent reports itself unavailable and never fails a
  listing. If one stored session row cannot be read, listing keeps every other
  session and reports that one as unreadable, with its id and the diagnostic —
  a separate fact from an unavailable harness, since the store itself was fine.
  OpenCode keeps two stores, and the first answers for a session id both hold;
  a session it could not read fails the read naming both stores rather than
  answering from the other one.
  A content search that cannot read one candidate's bounded tail reports that
  session separately in unsearched. A Codex row without model metadata can
  mean its model-bearing turn_context was before the bounded file tail; JSON
  leaves model absent rather than inventing one.
  Show and export still fail when the session they were given cannot be
  resolved.

  Each harness's store is found where the harness itself writes it, and the
  harness's own variable moves it: claude under $CLAUDE_CONFIG_DIR/projects
  (~/.claude/projects), codex under $CODEX_HOME/sessions (~/.codex/sessions),
  pi under $PI_CODING_AGENT_SESSION_DIR or $PI_CODING_AGENT_DIR/sessions
  (~/.pi/agent/sessions), and opencode in opencode.db and, when opencode2 is
  installed, opencode-next.db under $XDG_DATA_HOME/opencode
  (~/.local/share/opencode). A path is used as written, never expanded or
  canonicalized, so an empty or relative one resolves from the current
  directory.

  A session recorded in a directory that no longer exists cannot be placed in
  any project, so a scoped search will not find it. Its id still resolves.

  When the optional agent-status command answers, list puts matching
  sessions' working or idle state in its separate LIVE column, and the JSON
  form carries the same `live` field. A missing, failed, malformed, oversized,
  or status snapshot slower than 250 ms leaves that field out and does not
  change retrieval.
  An unrecognized state for one thread is ignored while recognized states for
  other threads remain usable. show applies the same join to its header.

READ A SUPPLIED EXPORT
  tapes list --input <path> --json
  tapes show <id> --input <path> --input-format openai --json
  tapes list --input <path> --input-format perplexity --json
  tapes show --occurrence <coordinate> --input <path> --json
  tapes export <id> --input <path> --bundle <dir>
  tapes export <id> --input <path> --bundle <dir> --evidence

  `--input` reads files, extracted directories, and, with the optional `zip`
  feature, ZIP archives. Input, evidence and artifact reads require regular
  opened files and never wait for FIFO writers. Supplied exports form a separate
  source collection. It never falls back to installed harness stores. `auto`
  detects OpenAI, ChatGPT Exporter, and Perplexity shapes; a declared format never hands a
  failed record to another parser. Auto-detected overlapping representations
  leave producer provenance absent, while an explicit format marks it declared.
  `list` emits an opaque occurrence coordinate
  for every supplied record. Use `--occurrence` when a native ID repeats, and
  `--after-occurrence` to continue a collection only when the ordered supplied
  input observation is unchanged; the cursor can cross files and ZIP members.
  Rows are ordered by --sort before --limit, and a continuation resumes in that
  same order. A named input path that does not exist is an error, and a
  conversation without a native ID is a gap rather than an invented identity.
  Exact titles are allowed; `--latest` is not, because supplied
  exports do not establish a global newest session. A build without `zip`
  keeps plain JSON files and directories available and reports how to enable
  the feature when a ZIP path is supplied.

  A native OpenAI container with a root `export_manifest.json` selects only the
  declared conversation shards, library metadata, and declared library `.dat`
  members. Nested site/account/settings JSON is not scanned. Missing,
  corrupt, or mismatched members remain explicit gaps. Title selection refuses
  incomplete discovery and ID selection refuses any gap that could cover an
  unread ID, which a record without a native ID cannot; an exact `--occurrence`
  can return the known projection with its gaps. Records parsed from a ZIP member that fails
  decompression or checksum verification are withheld rather than listed.

  An occurrence and a supplied record_ref.domain identify one observation of
  the input, keyed by the path as spelled and the file's identity, so they are
  cursors and never a conversation's or record's persistent identity. What
  survives the input moving is its bytes: read.record_sha256 holds the
  SHA-256 of the original bytes in each span in read.records, in order,
  including standalone objects and records inside collections. Whitespace and
  object key order are part of those source bytes.

  Source, member, pointer, and byte-span evidence stays attached to the
  normalized result. Scan, decoded, record, member, resident, and
  serialized-output budgets are finite and configurable with `--scan-bytes`,
  `--decoded-bytes`, `--record-bytes`, `--resident-bytes`, and `--output-bytes`.
  Every byte flag takes a count or a size: a bare or `iB` unit is binary
  (`64k` and `64KiB` are 65536 bytes) and a `B` unit is decimal (`64KB` is 64000).
  Oversized records and malformed
  members are reported as gaps or diagnostics while synchronization remains
  possible. A partial scan never becomes a not-found claim. Referenced files,
  media, and ZIP members are not opened or extracted, except that
  export --evidence copies the conversation record's span and each associated
  report's member verbatim into <bundle>.evidence/, named by SHA-256, with a
  manifest of where every byte came from; unsupported history-page and
  child-qualified reads refuse a supplied export explicitly.
  Mapping inputs retain bounded graph nodes and edges beside the selected path;
  associated reports retain bounded bodies and citation spans as artifacts,
  including when their backing conversation is unresolved.

READ A RECORDING ON ANOTHER MACHINE
  tapes list --remote <ssh-destination>
  tapes show --latest --remote <ssh-destination> --tail 40
  tapes export <id> --remote <ssh-destination> --bundle <remote-dir>

  --remote asks the named host's own tapes over ssh and answers from that
  replica; nothing is installed there and no store is copied. The replica
  resolves its own scope paths and stores, and a session's live state is
  whatever that machine reported: this machine's agent-status is never
  consulted for a replica, so live is unknown unless the replica answered.
  A replica that cannot answer is unavailable with the cause and the
  destination — never an empty listing, and never a completed session.
  Reads stay bounded on both sides: the replica is asked for the bounded
  answer the arguments name, and this machine accepts at most 64 MiB over at
  most 60 seconds. TAPES_SSH names the program that carries the query;
  TAPES_REMOTE_MAX_BYTES and TAPES_REMOTE_DEADLINE_MS raise or lower the two
  local bounds. The default ssh runs with BatchMode=yes, so a query never
  prompts: an unknown host key or a missing key fails at once instead of
  waiting out the deadline. A program named through TAPES_SSH is the
  operator's own and receives no added options, so naming ssh there keeps
  interactive prompts.

  export --remote runs the replica's own export, which writes the bundle on
  that machine through the reader that holds the recording. Every path it
  prints is destination:path — the coordinate scp and rsync accept — and
  --bundle names a directory on the replica. Read across machines with show
  --remote; export --remote keeps the rescue beside the recording, and the
  path it prints is how to fetch it.

PROBE BEFORE EXPORTING
  tapes show <id> --tail 40      A window, costing no bundle.
  tapes show <id> --turn-chars 2000  Cap each turn’s readable text, naming omitted characters.
  tapes show <id> --json         tapes-session/21 turns, typed context events, and bounded read, graph, artifact, terminal, and recorded launch evidence.
  tapes show <id> --exchange     Operator and assistant turns only, timestamps and ordinals kept; the name for --only operator,assistant.
  tapes show <id> --only operator --origin person  Operator turns with recorded person origin.
  tapes show <id> --omit tool    Every turn but tool calls and results; --only and --omit take kind labels, and export takes both.
  tapes show <id> --full         Every turn of a Claude, Codex, Pi, or OpenCode session, streamed past the read bounds; --json streams it too.
  tapes events <id> --json       Typed tool calls, results, pairs, and content parts.
  tapes capture <id> --json      The recording's store class and custody state.
  tapes usage <id> --json        Tokens, cost, quota observations, and turn counts.
  tapes usage <id> --from T --to T --json  The same answer for the requests in one span.
  tapes usage <id> --prices FILE --json  The same answer with a list-price estimate.
  tapes stats <id> --json        The same recording, counted with its read evidence.
  tapes lineage <id> --json      The sessions this one names as relatives.
  tapes brief <id> --json        What a continuation of it needs.
  tapes workset <id> --bundle DIR  The session and its relatives, read whole and cut into lanes.
  tapes locate --workset DIR --refs FILE --json  Whether a frozen workset still holds, and what its cited references are.
  tapes usage --here --json      The same counters summed over a project.
  tapes usage --global --window FROM..TO --by session,parent  What a window consumed, by session.
  tapes usage --global --quota-window current --harness codex  The same over the running quota window.
  tapes usage --global --allowance-rates --harness codex --since DATE --by model,variant  Allowance per lane.
  tapes usage --global --forecast MODEL#VARIANT=LANES --harness codex  Window percent and time left for a planned mix.
  tapes endings --here --json    What each session of a project ends on.

  show returns the last 100 turns unless --tail says otherwise, and marks the
  result truncated whenever it dropped any. It is the probe, not the archive;
  export is what takes every turn the reader could reach. JSON says why under
  `truncation`: a `window` names how many turns were returned and how many
  earlier ones the --tail bound omitted, which a larger --tail or export
  recovers; `source` lists bounds the reader itself reached (a file tail, a
  store page, cut turn text, gaps in a supplied input). Wider turn windows retain source bounds;
  `--read-bytes` widens the file tail (4MiB by default, at most 1GiB), `show
  --full` streams a whole session past it and past OpenCode's store page, and
  explicit page reads reach older Claude or Codex file history. `read`
  includes `source_prefix_digest` when a streaming full read or a pinned
  history walk establishes a prefix: algorithm `sha256`, lowercase hex `value`, and `span` from byte zero
  to `source_length` (exclusive). It hashes source bytes, including malformed
  records and newline-less tails. Identical copies share the digest even when
  their stat-based `source_revision` differs. Replays retain the pinned prefix
  identity after appends. Bounded reads, strict pages, and API reads omit it
  and read no extra bytes to establish it. Absence means no digest was established.
  `read` records the source length, configured bound, physical
  head/tail/alignment/verification ranges, decoded record spans, context-only record spans,
  and gaps;
  physical coverage does not erase malformed records, and a partial gap is
  discharged only by a successful decode of that same record. A preceding
  newline lets an exact tail boundary retain its first record; a mid-record
  boundary records the discarded partial prefix. Alignment bytes are not
  normalized coverage. `read.unmapped` counts, by native record type, the
  decoded records that became no turn: `declined` for types the reader leaves
  out on purpose, such as accounting, headers, and records restating another
  record's text, and `unrecognized` for every other type, whose content no
  view shows. An empty object means every decoded record became a turn; an
  absent one means that reader does not count. `read.reader` names the build
  that produced the read, as `tapes --version` does on its second line: a
  reader repair can change what the same source projects to under the same
  schema version, and this is what tells the two apart. A terminal observation records only native stop fields
  reached by the read, including bounded nested Codex error fields, and never
  says that the session is stopped now or that the account is currently
  available. A later recorded token total remains independent. An empty text
  tail carries the reason it has no operator or assistant text.
  Human output says the same in its closing notes, recommending only the
  recovery that works.

  Claude, Pi and Codex report `session.model_observation`: `mixed` means
  more than one model/effort pair was observed, `distinct_observed` counts
  those pairs, and `attribution_uncertain` marks gaps, partial reads or
  exhausted evidence budgets. `head_read: false` means the selection read
  did not reach the recording head; the opening metadata probe does not
  count. A single observed pair in that tail is not a whole-session claim.
  `session.model_selections` holds consecutive selection spans in recording
  order, each with `model`, `first` and `last` record coordinates (`timestamp`
  and `native_id` when recorded). A selection that returns after a switch
  has another span. Endpoints prove the selection at observed records;
  they do not prove it between records. At most the newest 1024 spans and
  32 distinct keys are retained; exhaustion marks attribution uncertain
  and withholds the count. Claude and Pi endpoints are assistant records;
  Codex endpoints are turn-context records. Claude and Pi assistant turns
  carry their own `model` with effort in `variant`; Pi uses the active
  branch's preceding `thinking_level_change`, leaving effort absent when
  that setting was not reached. The session model remains the newest
  selection. Library consumers use `Session::newest_model_selection()` to
  obtain a `ModelSelectionSpan` and its `last: ModelSelectionRecord` without
  decoding the native records.

  show --turn-chars N caps each turn's text at N Unicode characters after
  selecting turns. Capped JSON turns carry text_truncated: true and text_chars
  with the original length; human turns end with the omitted character count.
  Without the flag text is unchanged. Zero keeps no text. This presentation
  bound applies to full, supplied-input, and remote reads; native content
  parts and other metadata retain their own bounds.

  timeline <id> or timeline --here projects each turn as ordinal, kind and ts
  under tapes-timeline/1, without text or payloads. Ordinals use show's
  zero-based coordinate. Missing timestamps are omitted (a dash in human output).
  Selection, scope and read bounds follow stats; --full streams every selected
  recording. JSON groups records by session with truncation and read notes;
  human output is one line per turn. A selected session that cannot be read
  fails the command.

  events <id> projects harness-neutral tool records as tapes-events/14.
  --tool (alias --name) matches the recorded tool name exactly. --command
  matches a case-insensitive substring of a shell call's recorded command
  field (command or cmd), or the serialized input for other tools. Matching
  uses complete retained input, including text beyond arguments.preview;
  paired results and recorded exit status remain attached to selected calls,
  including results whose recordings omit the tool name.
  events --here or --global accepts the session filters and limits of stats.
  Selection JSON uses tapes-events-selection/2: each sessions row contains
  session, count of matching calls, partial, truncation, and a nested events
  object. --count omits that object, including for one named session. Zero
  matches still produce a row. Partial counts describe only the read window;
  --full streams each selected recording. An unreadable selected recording
  fails the command. Collection discovery diagnostics accompany the rows.
  Each record
  names its source turn with turn_ordinal, the same coordinate show prints;
  ordinal retains that turn coordinate, including for call/result pairs. Pairing is
  exact within the bounded read; an incomplete call or result says whether its
  counterpart was not reached or not recorded. `exit` is an integer process
  status on completed Codex CommandExecution events when exit_code is recorded.
  Claude Bash and Pi bash results omit it because their recordings supply no
  structured process status. Output text never supplies an exit status. Event ordinals are the same
  turn coordinates show prints. event_id stays stable across read windows;
  native_id is the harness record id where recorded, or a coordinate-derived
  record-sha256 id for a tool turn without one. A recognized file read has
  its recorded path, recorded line range or explicit whole-file scope, outcome
  when known, and sha256 of returned UTF-8 text. content_sha256 names the
  returned file bytes for Claude text reads only when structured file content
  or every decimal-number/tab line establishes them exactly. Structured token
  truncation and any unmatched line leave it absent. Partial reads digest the
  returned range and carry its line coordinates. Unknown formats omit
  content_sha256. Missing scope is unknown.
  capture reports unpinned for installed recordings; exporting does not
  pin native bytes. With no --tail, every event the bounded reader
  reaches is returned; --name and --call-id filter only after pairing.
  --full-arguments returns each tool call's complete recorded argument text in
  place of the 200-character prefix, under the same members and schema
  version. `--program`
  filters exact nested literal or structured-runtime declarations while
  `--name` continues to mean the recorded outer tool. JavaScript declarations
  require a complete direct literal `cmd`/`command` property; supported string
  escapes are decoded and dynamic, nested, incomplete, arrow, short-circuit,
  ternary, or unsupported forms remain qualified evidence. Shell assignments
  and reserved/control forms are also unsupported. Structured argv with
  non-string elements or too many arguments is reported unsupported rather
  than filtered or truncated.
  A shortened first or second invocation token is unsupported rather than an
  exact-looking program or subcommand name; later arguments retain their
  bounded truncation facts.
  Wrapper results never witness an individual nested declaration or give it
  timing. Explicit artifact references and within-read consumption
  observations are descriptors; tapes never opens the named object. Codex
  lifecycle mirrors for messages, reasoning, user messages, and compaction
  are not projected as tools; only the verified `CommandExecution` and
  `FileChange` variants enter this event layer. events --full streams a whole
  Claude, Codex, or Pi recording twice, pairing past the read bound and
  writing each event as it pairs, with the same filters and counts.

  usage answers where a session's quota went as tapes-usage/16: its recorded
  launch mode when known, tokens, cost, accounting, turns, and any by-model attribution the source can
  support — Claude's recorded cost-state or, for a request sum, the model
  each request names; Codex's request observations; or pi's per-message
  model each qualified by the thinking level in effect. Read
  accounting before adding anything up — basis says
  whether a figure is a recorded total or an observed request sum, and
  coverage says how much of the session it covers. An observed cumulative
  counter restart is `since-reset`; the newest recorded total remains visible
  without being rebuilt from request rows. Codex attribution counts every
  usage record, compactions included, and `attribution.outside_total` counts
  the requests the cumulative total never moved for (a remote compaction, or
  a request no token count followed), with `outside_total_tokens` their sum.
  A compaction is the usage record a `compacted` record names by its
  `compaction_response_id`, and it is outside the total however the next
  total moves:
  the model shares less those are the recorded total's movement, so a share
  above the recorded total says what the total left out. Codex quota
  observations preserve credits balances and reached-limit flags exactly as recorded, including false
  and string zero; absent fields remain unknown. `usage --full` streams the
  whole recording and folds its counters. Turns and complete request reads
  cover the session; recorded totals keep their epoch coverage. Pi's active
  accounting stays `read-window` when an unreadable record cuts its branch
  or follows its last entry.
  `usage <id> --from <RFC3339> --to <RFC3339>` answers one span of one
  session, the half-open `[from, to)` with either bound open, and
  `--requests FIRST..LAST` the session's counted requests 1-based and
  inclusive in recording order. Both stream the whole recording and place
  each counted request by its own timestamp: a Claude request id by its first
  record, a Codex token count that moves the cumulative total or a usage
  record the total never moved for (a remote compaction, or a request no token
  count followed), a pi consumption event counted once, an OpenCode assistant message by its
  creation time. The answer keeps the whole-session shape and vocabulary and
  adds `span`: the requested bounds, the `first` and `last` request placed
  (ordinal, timestamp, and the harness's own request id where it records
  one), how many `requests` it placed, and the session's `session_requests`.
  Turn counts describe the whole read. Spans conserve: requests no span
  holds form the remainder, and spans that share no request plus the
  remainder add up to the whole session per counter and disjoint category
  within one accounting basis, so the span before an instant and the span
  from it add up to `usage --full`. A request in two spans counts in each and
  is `overlapped`. A Codex span sums its requests, `summed-requests`: a token
  count's share is its move of the cumulative total and a usage record the
  total never moved for counts its own usage, so the whole the spans add up
  to is the thread's total, as `thread_token_usage` records it. A span is
  `since-reset` when the counter restarted inside it, as a resumed `codex
  exec` process starts its total again. `usage --full` keeps the recorded
  total, which omits the requests `attribution.outside_total` names and,
  after a restart, covers only the requests since it. A Claude cost-state is the running total of one run and an OpenCode session total
  counts requests no message records, so no span divides either: their
  spans, and the whole they add up to, sum requests, and the recorded total is
  withheld with a note. A pi span keeps the active-branch counters and the
  retained-branch `consumption` apart, each conserving. A request with no
  usable timestamp is held by the remainder and qualifies every span it may
  belong to `unestablished-scope`; an unreadable record, a failed attempt or
  unrecognized usage the read cannot place for or against a span is counted
  under `unplaced` and qualifies it `unreadable-record`, `stopped-attempt` or
  `unrecognized-usage`, so a span the read could not cover is never complete.
  The library answers several spans in one pass, as tapes-usage-spans/5: a
  figure per span, the `remainder`, and the `whole` in the spans' basis. A
  whole-recording read asked for its timeline also carries, without another
  pass, the request timeline the spans count (every counted request with its
  ordinal, timestamp or none, and id) and the working stretches `stats
  --idle` cuts from the same turns, each holding the timed requests that
  fall in it. A read not asked keeps neither. One that counts more than
  100,000 requests or hands over more than 1,000,000 timed turns withholds
  both whole, with its reason, rather than keep part of them.
  Money beyond what a harness wrote comes from a price table the caller
  supplies; tapes holds no prices. Every command that reports a session's
  cost prices it the same way and puts `list_price_estimate` beside it:
  `usage`, `list --json` and `show --json` (on each session), `stats` and
  `brief` (in `usage`), and the per-session figures of a summary's overlap
  groups. Text adds the estimate, labelled as one, only where it prints a
  cost: `usage`, the `stats` usage block, and a summary's session figures.
  A replica's answer under `--remote` is not priced. `--prices FILE` reads
  a table, else the
  file `TAPES_PRICES` names, else `$XDG_CONFIG_HOME/tapes/prices.toml`
  (`~/.config/tapes/prices.toml` when that is unset). A table named by the
  flag or the variable that is missing or invalid refuses the command; an
  invalid default table is reported once on stderr and the estimate is
  absent, as it is with no table. Install a table by writing it to the
  default path or pointing `TAPES_PRICES` at it. The format is TOML,
  tapes-price-table/1:

      schema = "tapes-price-table/1"
      currency = "USD"
      [[models]]
      id = "model-id-as-recorded"     # the id a harness records
      aliases = ["other-recorded-id"] # optional
      effective = 2026-10-01          # the date the prices were read
      source = "where they were read"
      per_million = { uncached_input = 3.0, cache_read = 0.3,
                      cache_write_5m = 3.75, cache_write_1h = 6.0, output = 15.0 }

  Prices are per million tokens of each disjoint category; reasoning is a
  share of output and priced as output. A category a row leaves out has no
  price. Unknown keys, negative or non-finite prices, a non-date
  `effective`, and an id or alias named twice refuse, naming the entry.
  tapes-price-table/2 takes the same rows, and a row may add terms priced
  by the request:

      schema = "tapes-price-table/2"
      [[models]]
      ...                              # as in /1
      effective_from = 2027-01-01      # optional: a date (midnight UTC) or
      effective_until = 2027-07-01     # an instant with an offset; until is
                                       # exclusive
      [models.long_context]
      above_prompt_tokens = 272000     # or from_prompt_tokens: at or above
      prompt_tokens = ["uncached_input", "cache_read", "cache_write"]
      billing = ["api"]                # optional; "api", "subscription"
      multipliers = { uncached_input = 2.0, cache_read = 2.0, output = 1.5 }
                                       # or per_million = { ... }
      [[models.time_of_day]]
      from = "16:30"                   # UTC, inclusive
      until = "00:30"                  # UTC, exclusive; past midnight when
      per_million = { ... }            # not after `from`
      days = ["mon", "tue", "wed", "thu", "fri"]
                                       # optional: the UTC days it opens on,
                                       # mon..sun; every day when left out

  A long-context term charges the whole request, every category, once the
  request's prompt — the sum of the categories `prompt_tokens` names —
  passes the threshold (`above_prompt_tokens`, more than; or
  `from_prompt_tokens`, at least): its `per_million` replaces the request's
  rates, or its `multipliers` scale them, a category it leaves out keeping
  its rate. It applies to the billing it names, per-token API billing
  unless it says otherwise. A time-of-day window's rates replace the row's
  for a request made inside it on a UTC day of the week its `days` names,
  or on any day when it names none; a window past midnight belongs to the
  day it opens on, so Friday's 22:00 to 02:00 holds early Saturday. Public
  holidays are not expressed: a window holds on a holiday as on any other
  day of its weekday. A row is in effect from `effective_from`
  until `effective_until`; one id may name several rows whose windows do
  not meet, so a dated price change is two rows. A /1 table refuses these
  keys, and prices exactly as it always has. A figure is priced by these
  terms request by request where its reader kept its requests — a Claude,
  Codex or pi session, span or window share, a pi retained-branch
  `consumption`, each event at its own time on whichever branch holds it,
  and an OpenCode span or window share, up to 100000 per read — and they
  sum to no more than the figure; what they leave of it, a kept request
  whose recording holds no time for it, or a figure whose requests are not
  known one by one (a recorded total such as a Claude cost-state or an
  OpenCode session row), is priced between every rate it could pay, as
  `at_least` and `at_most`, its categories qualified
  request-terms-unknown, and never reads complete. A request no row of its
  model is in effect for is unpriced, its categories outside-validity;
  usage of no known time that
  the rows in effect leave part of the figure's span uncovered for is
  priced by the rows that cover it, its categories outside-validity too.
  Each recording's billing comes
  from what it records, named with its evidence in the estimate's
  `billing` wherever a used row has a long-context term: a Codex
  `rate_limits` window reporting a used percent is a ChatGPT plan's quota,
  a subscription; a Codex `model_provider` other than `openai` bills per
  token; a Claude `api-request` record carrying the `oauth-2025-04-20`
  beta, or a `quotaLimits` refusal naming a plan quota, is a subscription,
  and one sent without that beta bills per token; a pi recording whose
  every token-carrying event is a request on the `openai-codex-responses`
  API, which takes only a ChatGPT account's sign-in, is a subscription; any
  other pi recording, a compaction or tool result naming no API included,
  and every OpenCode recording name no billing. A span or window share is
  billed as its whole recording is, whatever records it holds, and names
  no billing where the whole recording names none. A
  subscription pays no long-context surcharge; billing the recording
  establishes neither way is
  priced between base and surcharged rates, its categories
  billing-unestablished.
  Each figure — a session, each model of `by_model`, a pi `consumption`, a
  span, a summary group, total or partition — gains `list_price_estimate`
  beside the harness's own `cost`, never in its place: `kind:
  list-price-estimate`, `currency`, an `amount`, the table `schema`, each
  row it used in `models` (`id`, `effective`, `source`), `billing` where
  a used row has a long-context term, and
  `measurement_completeness` over uncached_input, cache_read, cache_write and
  output. A cache write that recorded no TTL is priced between the 5-minute
  and 1-hour prices, as `at_least` and `at_most` in place of `amount`, and
  its category says ttl-unrecorded. Each served model's share is priced by
  its own row: a figure whose records name one model is that model's share
  whole; one that names several, or whose uncertain attribution leaves part
  of it out of its model shares (a Codex request made before any model was
  in effect), is priced share by share from the split its harness recorded,
  and what the shares leave uncovered is incomplete-attribution; with
  neither, nothing is priced. A model the table
  lacks qualifies its share unlisted-model and a missing category price
  missing-price. Every reason a category's counters carry (missing-counter,
  stopped-attempt, unrecognized-usage, read-window, ...) carries into the
  estimate, whose amount is then a known subtotal. A model share carries
  each reason its figure carries that describes the read rather than one
  model's events (unreadable-record, unrecognized-usage, read-window,
  since-reset, unestablished-scope, incomplete-attribution), so a
  share-by-share estimate carries it too, and a share prices its cache
  writes at the TTLs its own requests recorded wherever those writes sum
  exactly to the share's. Pi consumption is split by
  the model each event names. On the active branch alone it is priced as
  the session figure is, share by share where every event names a model;
  with events on other branches it is priced by that split, and usage no
  model names is incomplete-attribution. The estimate is
  absent, never zero, when nothing could be priced. A summary adds estimates
  where its disjoint categories add and otherwise withholds them, naming why
  in `list_price_withheld`: mixed-accounting, identity-unestablished, or
  exact-union for related sessions added by their events' union. Text output
  prints `list-price estimate <amount> <currency> (from the price table:
  ...)` with a completeness line and a `list-price billing:` line per
  billing it names, and a summary adds a LIST-PRICE ESTIMATE column when a
  table is given.
  `usage --window FROM..TO` (RFC 3339, either end open) answers the
  consumption inside a window over the sessions the scope and filters select.
  --since and --until select sessions by their newest activity; --window cuts
  consumption at its edges. Every selected session with activity inside the
  window contributes the span of its own requests inside it, read whole as
  `usage --from/--to` reads one session, so a session that straddles an edge
  contributes what it consumed inside and nothing else; a Claude parent's
  subagent recordings, which no listing returns, contribute their own spans as
  `<parent>::<agent>`, and a parent whose own records end before the window is
  reached through subagent recordings written since it opened, and since
  --since where that is later; under --since it is kept only when it or one of
  its subagents has recorded activity since it. Every other filter applies to
  the parent as to a listed session, --search over the parent's own turns
  included, but the parent is reached beyond --limit. The shares are
  summed under the summary's partitions, withholding and overlap rules, so the
  window total is the sum of its shares within one accounting basis, and a
  request a span cannot place qualifies its share as it qualifies a span.
  Related sessions are never added by an exact union, since event keys cover
  whole recordings. `window` names the edges,
  how they were established (`requested`, `recorded`, `inferred`), the
  selected sessions that placed nothing inside it, and each session's share,
  largest first: its requests inside the window out of its own, its launch,
  the `parent` its recording names and how (`parent_evidence`), the `root`
  that chain of parents ends at, the harness holding it (`root_harness`),
  and whether the store holds it, and its disjoint categories. --limit still
  bounds the sessions each harness lists. `--by session`, `launch` and
  `parent` group any summary by session, by recorded launch, and under the
  root of each session's named parents. A parent group's key names that root
  by `parent` and `parent_harness`: an id names a session only within one
  harness, so roots sharing an id in different harnesses stay apart, and a
  table writes such an id as `<harness>:<id>`.
  A recording names its parent in one of four ways, and a share's
  `parent_evidence` says which: `subagent` (a Claude subagent's recording,
  kept under its parent's), `spawn` (a Codex child's `parent_thread_id`),
  `fork-origin` (a declared fork origin), or `declared-launch-parent` (a
  marker the tool that launched the session wrote into it). A fork copies its
  origin's first prompt, so a fork origin outranks a marker the copy carries.
  The launch marker is the contract a launcher writes to. The first line of
  the prompt a launched session starts with is exactly
      tapes-parent: <harness>:<session-id>
  with nothing before it: `tapes-parent:`, one space, the launching session's
  harness (`claude`, `codex`, `pi` or `opencode`), a colon, and that
  session's native id, 1 to 128 ASCII letters, digits, `-`, `_` or `.`. The
  line ends at a line feed, or at the end of the prompt; the rest of the
  prompt follows it. tapes reads the marker from the session's first user
  message that is not wholly context the harness attached (Codex records its
  AGENTS.md and environment blocks first), within the recording's first MiB
  in Claude, Codex and pi recordings, and from the first 256 characters of
  the first user message's first text part in OpenCode: one query per page of
  sessions, or one request per session through the API. Only an answer that
  uses a parent reads it, and only such an answer looks up a parent outside
  the selection: `--by parent` (in a summary or allowance rates), a window
  read, whose shares carry `parent` and `root`, and one session's `usage`,
  whose notes report a marker that names no parent. Listings, every other
  grouping of a summary or of allowance rates, and forecasts read no marker
  and look up no parent. A marker is read only for a session its store names
  no parent or fork origin for, a parent outside the selection included. A
  parent is looked up by its id in the stores of the harness that holds it:
  the one a marker names, and the session's own for a subagent, a spawn or a
  fork origin. One those stores do not hold stays the named, unresolved root,
  as a missing fork origin does, even where another harness holds a session of
  that id. A first prompt opening with `tapes-parent:` in any other form, or
  naming the session itself by its harness and id, names no parent, and the
  session's usage notes say so. A launched session holds no copy of its parent, so the marker never
  makes the two an overlap group: their consumption adds. A derived title
  leaves the marker line out.
  `usage --quota-window current|<instant> --harness codex|claude [--quota
  primary|secondary]` answers as --window over the provider's quota window,
  from the recordings' own quota observations. A Codex `rate_limits`
  observation records its window's reset and length, so the window is
  `recorded`. Claude records a limit only on a request it refused
  (`quotaLimits`: `five_hour` or `seven_day` and the reset), so its length is
  the one the limit's name states and the window is `inferred`. `current`
  reads the newest observation alone, and once its window has reset takes the
  window after it for the same length, `inferred`; an instant reads the newest
  observation whose window holds it. Anything else refuses and the window is
  read with --window. `window.quota` names the observation and the session
  whose recording holds it.
  A window read also apportions allowance: the percent of a quota window
  sessions spent, an estimate from observations reported beside tokens and any
  price, never in their place. A pool is one harness's quota window for one
  account, limit and plan as the recordings name them (Codex `session_meta`
  `creator_account_id`, `rate_limits` `limit_id` and `plan_type`); where no
  record names an account, one harness's store on this machine is one pool and
  `window.allowance_assumption` says so. Every session of the pool's harness on
  this machine with activity in the window that spends from the pool is a
  member: its recording names the pool's account (or, like the pool, none),
  and where its recording's observations name quota limits, inside the
  window or before it, the pool's is among them. Members count selected or not, up to
  10,000 sessions whatever --limit says; a pool whose member listing reached
  that bound says `members_truncated`, its members' shares then holding what
  the sessions left out spent.
  Between two consecutive observations of a pool, ordered by observation time,
  the rise in `used_percent` is apportioned over each member's disjoint tokens
  (uncached input, cache read, cache write, output) inside that interval. A
  new `resets_at`, beyond a minute's jitter, starts a new chain, as a falling
  percent does where no reset is recorded; inside one period a lower percent
  is a snapshot older than one already seen (concurrent sessions report the
  same account) and is skipped, counted as `stale_observations`. Requests before
  a chain's first observation, after its last, or recorded without a time are
  `uncovered_requests`, not counted; a Codex total recorded without a time
  still advances the running total, so the next request's move is its own.
  Each member keeps one weight per interval it spent in, so apportioning holds
  memory in proportion to the requests read, never to members times
  intervals. Observed percents are whole numbers, read as the true percent
  rounded to the nearest whole, so each figure gives `estimate`, the rises
  apportioned as observed, with `at_least` and `at_most` that hold for every
  true percent the observations allow: a run of equal observations hides at
  most one unit of rise. `output_weighted` repeats the apportionment weighted
  by output alone, a sensitivity bound on the weighting. Each share, group
  and total carries `allowance`, one `kind: apportioned-allowance` entry per
  pool window (`primary` or `secondary`, its length, the plan) with its
  `coverage` (observations, intervals, uncovered requests);
  `window.allowance` gives each pool's `observed` rise and how it divides:
  `attributed` to the selection, `outside_selection` for the pool's other
  sessions, and `without_recorded_use` for rises in intervals where no member
  spent a token. The estimates of the three add up to the observed rise.
  Claude records no used percent (its quota observations exist only on
  refused requests), so a Claude share carries `allowance_absent` with that
  reason and no allowance.
  `usage --allowance-rates` turns the same apportionment into rates per lane:
  every completed session (last activity at least fifteen minutes before the
  read) with requests in the lookback (--window, else --since..--until) is a
  lane, its percent of the --quota window summed over that window's pools,
  grouped by --by (`model,variant` for cost per model and effort) and by the
  `plan`, `limit` and `window_minutes` of the pools it was measured in, each
  group naming them, since a percent is of one capacity: two plans or two
  limits whose windows share a length spend different allowances, and a
  plan change can give a window of the same kind another length. Each group
  gives lanes, percent per lane (median, 90th percentile by nearest rank,
  maximum) over the lanes' estimates and their bounds, tokens per one percent
  by disjoint category (the tokens of the lanes' requests an interval of their
  pools holds, over the percent apportioned on them), `uncovered_requests`
  (the lanes' requests no interval held, on neither side of the rate), the
  median minutes from a lane's first to its last
  request, and `measured_over`, the first and last request measured: rates
  drift with models and pricing, so a rate is read with its dates. A plan
  whose window lacks the --quota kind (a plan with one weekly window, say)
  leaves its lanes `excluded_without_allowance`. Percent per lane comes
  twice, each naming its `estimator`: `percent_per_lane` is
  `interval-apportioned`, the attribution a window read gives, and
  `smoothed_percent_per_lane` is `period-smoothed`: within each window
  period, the lane's share of the tokens spent in the period's intervals
  times the period's rise, bounded by the rounding of the observations that
  bound it. At whole-percent resolution most intervals rise by nothing and a
  step lands on whichever lanes spent in it, so the smoothed figure is the
  steadier lane cost. A session either estimator gives a percent is a lane
  of both, so both sum to the same group total. `tokens_per_lane`
  gives the lanes' tokens. Lanes come from up to 10000 sessions per harness,
  not --limit's default of 20; an explicit --limit keeps that many in --sort
  order, a parent reached only through its subagents taking a place after the
  listed sessions, and none when --since does not keep it. `lanes_read`
  names the limit, whether it is the caller's,
  the sessions the lookback may hold that it left out (`left_out_by_limit`),
  and `lane_bound_reached` when a listing stopped at its bound, so sessions
  past it went unread. The answer is tapes-allowance-rates/5.
  `usage --forecast MODEL[#VARIANT]=LANES,... --harness H [--concurrency N]
  [--quota primary|secondary]` sizes a planned mix against that window, as
  tapes-allowance-forecast/2. It reads the rates grouped by model and
  variant over --window, else --since..--until, else the last 14 days, and
  sizes each lane as its group's median lane tokens over the group's tokens
  per one percent, with the ratio's bounds, and carries the rates'
  `lanes_read`. The window is the one the pools'
  newest observation is of; its length is named, never assumed, and only
  lanes measured in pools of its plan, its limit and its length size the
  plan, since a percent is of one capacity and plans and limits differ in
  theirs as a plan's windows differ by kind and length. That observation gives what is
  left (100 less its used percent, bounded by its rounding, or the whole
  window once its reported reset has passed) and the newest period's reset,
  which moves, so it is read rather than assumed. The answer gives the mix's
  percent and whether it fits what is left (`yes`, `maybe`, `no`), the
  percent per minute with N lanes at once (each lane's percent over its
  group's median duration; all of them at once by default), the minutes
  until the window is spent at that pace, and whether that comes before the
  reset. A bound the measurements leave open is null. A model and variant
  with no measured lanes refuses, naming it; a Claude forecast refuses,
  since Claude records no used percent.
  A summary adds sessions only where no recording relates them. Two sessions
  are related when one declares the other as its fork origin or parent (Codex
  `forked_from_id`, Pi's parent header, OpenCode `fork_session_id` and
  `parent_id`), transitively, when both are forks of the same source (Codex
  `forked_from_id`, Pi's parent header, OpenCode `fork_session_id`), whether
  or not that source is selected, or when both recorded the same request id
  or message uuid, which is how a Claude fork shows: it keeps its origin's
  ids.
  Related sessions form an overlap group. A group of two whose consumption
  event keys establish their union adds by that exact union, its keys
  confined to each session's active branch like the counters beside it; a
  fork's verified copy of an entry its parent rewound past counts once; any other group
  withholds the summary's exact token, cost and disjoint totals, keeps each
  session's own figure in `session_figures`, and reports `identity_bounds`
  within a compatible accounting partition: at least the largest member of
  each group, at most the sum. A missing or partial counter leaves its upper
  bound unmeasured. `overlap.related` lists each group with its sessions, its
  `evidence` (declared-fork, fork-origin-unplaced, shared-declared-origin,
  shared-request-id, shared-message-uuid), whether its `union` is exact or withheld, and why.
  Unrelated sessions beside a withheld group stay summed in the bounds.
  `overlap.disjointness` states how the unrelated sessions and groups were
  added as disjoint, and is absent when every session is in one overlap
  group. It is `inferred-from-absent-copy-evidence`: the recordings hold no
  sign that they share events, which is evidence of absence and not proof.
  The id check reads the ids the usage read visits and keeps a bounded sample
  of a very long recording's, so a copy sharing only a few ids with it can go
  unseen. When any summed session was sampled the basis is
  `inferred-from-sampled-copy-evidence` and `overlap.sampled` names those
  sessions by id.
  Different bases have separate partitions, and `session_figures` preserves
  per-counter coverage without recording locators.

  tokens are the harness's own counters, and what its input and output
  include differs: Codex input counts cache reads, OpenCode output leaves
  reasoning out. Never add raw input across harnesses. disjoint restates the
  same counters as uncached input, cache read, cache write by TTL (5m, 1h, or
  ttl_unrecorded), and output, with reasoning a share of output, and names the
  native convention it mapped from. Its total is present only when every
  summed category is completely measured. `missing` names absent and
  incomplete categories, and unestablished TTLs of nonzero writes. A numeric
  category named in `missing` is a qualified known subtotal. Native measurement
  coverage follows each category's dependencies; incomplete counters cannot
  establish a subtraction such as Codex's uncached input. Summary missing counts
  include selected sessions with no token measurement, while `disjoint.sessions`
  counts sessions that recorded tokens.

  `measurement_coverage` counts, per native counter, the events a figure's
  sum covers by whether each recorded the counter. For a request sum
  (Claude and Pi sessions, a Codex or Pi per-model split, Codex's
  unattributed requests under `attribution.unattributed_coverage`) an event
  is a counted request, and a request that wrote no usage counts with every
  counter missing, even when no request wrote any counter and the figure has
  no sum; for a harness-written total (a Codex or OpenCode session, or a
  Claude cost-state, which counts the models it sums) the total is the
  event. A counter some events lack is a qualified known subtotal, never
  complete, and `measurement_incomplete` says how many recorded it. A
  summary's `measurement_coverage` adds the events of every session it sums,
  those that recorded no counter included, and is withheld wherever its
  tokens are.

  `measurement_completeness` answers, counter by counter, whether a figure is
  completely measured and why not. It names each incomplete counter, or
  disjoint category, with fixed reasons: missing-counter (some counted events
  did not record it, or the figure has none), missing-price, unpriced (a
  written zero beside usage), stopped-attempt (a request stopped on an error
  or an abort, or failed, so its consumption is partial or unknown, even with
  every counter populated), unreadable-record, unrecognized-usage (a record
  the reader has no reading for carries its harness's usage fields, so its
  consumption is in no counter), read-window, since-reset,
  unestablished-scope, incomplete-attribution (a model split whose read could
  not attribute every request), undetermined (the convention cannot
  derive the category), and, for list-price estimates, unlisted-model (the
  price table has no row for the model), ttl-unrecorded (cache writes with
  no TTL, priced between bounds), request-terms-unknown (a term priced by
  the request may apply to usage whose requests are not known one by one),
  billing-unestablished (a long-context surcharge for per-token billing, on
  a recording that establishes no billing) and outside-validity (no row of
  the model is in effect at the request's time). A figure that counted events and measured nothing
  qualifies every counter missing-counter and keeps every other reason that
  applies to it: its scope, pricing and coverage. A counter it does not name is complete; a present
  amount it names is a known subtotal. Usage, consumption, model splits,
  summary totals, groups and partitions, session figures, brief, stats, and
  session JSON all carry it, and every figure derived from a counter follows
  it: a disjoint category carries its native dependencies' reasons, the
  disjoint total and an identity upper bound need complete counters, and a
  stats cache ratio needs complete input and cache counters. A model split
  carries its own reasons plus those no single model can be spared: the
  read's coverage, failed requests, unrecognized usage, incomplete
  attribution. The unattributed share (`attribution.unattributed_completeness`)
  carries its own reasons, the read's coverage, failed requests and
  unrecognized usage, but not incomplete attribution. One model's
  stopped request leaves another model's figure whole, and one session's
  reasons leave another session's figure and group whole.
  `read.unmapped` counts records by type and says nothing of their usage, so
  an unrecognized type alone qualifies no figure: Claude's attachments are
  unrecognized and carry none. `unrecognized-usage` is raised, with the count
  in `unrecognized_usage`, only for a record no reading counts that carries
  its harness's usage fields: for Claude a `message.usage` counter object on a
  record that is not an assistant request; for Pi a `usage` object on an
  entry or its message holding a counter or total cost; for Codex a
  `payload.usage` counter object or a `payload.info` usage on a record other
  than a token record or a `token_count`; for OpenCode a `tokens` counter on a
  message outside the projected roles. It qualifies every native counter and
  cost of the figure. A Pi entry on an abandoned branch qualifies the
  consumption, not the active branch. An OpenCode database record that cannot
  be read is an unreadable record: a message row whose data does not decode,
  a message row without an id or an integer `time_created`, a part row whose
  data does not decode, a part row without an id or an integer
  `time_created`, or a part row that names no message. Its turns are absent,
  every other row's stay, and it qualifies every counter
  `unreadable-record`. A `show --full` read counts each such row in `skipped`
  and marks one `malformed-record` gap at each message position that holds
  any, so a message with two unreadable parts is two skipped records in one
  gap; a row with no place in the
  message order, or a part that names no message, takes a position after the
  last message.
  A harness can attach only some accounting bases: Claude summed-requests or
  recorded-total, Codex and OpenCode recorded-total, Pi summed-requests. Their
  span figures, and the windows built from them, sum requests.
  The text output names the same reasons, one line per incomplete figure:
  `completeness: input: missing-counter; output, cost: stopped-attempt` lists
  each distinct reason set once with the counters that share it, and `all
  counters` stands for every counter. The same line follows a model split
  (`model <name> completeness:`), consumption, and each session figure. A
  disjoint category that holds an amount reads `known subtotal; <reasons>`,
  and a withheld disjoint total prints `total withheld:` with the summed
  categories and reasons that withheld it. A summary row's counter and COST
  cells end with their reasons in brackets, and a withheld DISJOINT TOTAL
  cell reads `withheld [<categories: reasons>]`. Coverage lines are labelled
  `active` for a pi session's active branch and `session` otherwise.
  A Claude request sum keeps the TTL split. A cost-state total merges it,
  so its writes take the split its requests recorded, for the figure and for
  each model's share, only where that split sums exactly to the total's cache
  writes; otherwise they are ttl_unrecorded. requests
  tallies what the read's deduplicated Claude requests recorded beside their
  tokens — cache writes by TTL, service tier, speed, inference geo, web
  search and fetch counts — and counts under missing each request that did
  not record a dimension. It covers the requests read, which need not be a
  recorded total's scope. A cost is the harness's own estimate: it says
  kind harness-calculated-estimate, and pricing_basis is missing because no
  recording states its price table. A token count is never a cost.
  A written zero beside nonzero tokens is unpriced and its cost is absent.
  This pairing is enforced when session reports enter through JSON, remote
  list/show, and public usage projections or aggregation. Session JSON carries
  cost_coverage so forwarding a report preserves its written-zero evidence.
  cost_coverage names incomplete pricing and counts zero-written requests
  where request evidence exists, and zero-written cumulative observations
  otherwise. A mixed priced/unpriced sum is a known priced subtotal.
  Synthetic assistant records join no request tally; positively witnessed
  failures are counted separately and their consumption is unknown. A
  Claude assistant record with no request id that writes each of its four
  usage counters as zero, as a plugin writes when it mirrors work done
  elsewhere (a delegated run's `delegated-agents:run` rows), stands in for a
  request without being one: it counts as no request, and a recording made
  only of such records notes that it holds no consumption of its own, with
  no measured zeros. One that leaves a counter out is a request of unknown
  consumption, counted with that counter missing. A Claude
  read, whole or bounded, that skips an undecodable record qualifies every
  counter unreadable-record.

  A pi rewind appends a new branch and keeps the old one, and the abandoned
  requests were consumed all the same. pi usage therefore carries
  `consumption` beside the active-branch counters: tokens and cost summed over
  every retained branch, with `accounting.selection` `retained-branches`.
  Both figures count assistant requests, tool results with usage, compactions,
  branch summaries, and usage entries. They agree when all consumption is on
  the active branch. Missing usage leaves the affected event unmeasured.
  Each event counts once:
  an entry seen again, or another entry naming the same provider response, is
  a repeat and adds nothing, so a re-read or a copied entry consumes nothing
  new. `events` splits the counted events between the active branch and the
  others; `identity` says how many use an observed (provider, response)
  identifier within the read and how many use the recording's own entry.
  `provider_account_namespace` is `unestablished`: response identifiers
  alone do not establish cross-recording equality without an account
  namespace. `forked_from` is the parent session id derived from the
  header's recording filename, never its path. The first underscore separates
  the timestamp from the full id. Timestamp-prefixed basenames are recording
  references; other bare values, including `parent.jsonl`, are ids.
  Parent ids hold at most 128 ASCII bytes,
  begin and end with a letter or digit, and allow dots, hyphens, and underscores
  inside. A header whose parent id cannot be derived sets
  `fork_parent_unestablished` instead.
  Coverage is `session` for a read from the first byte and `read-window`
  for a bounded tail, independently of the active branch: a branch that
  restarted from a new root can lie wholly in the tail while what it replaced
  does not. Active counters and recorded model splits carry their own
  `measurement_coverage` and `measurement_incomplete` qualifications.
  `measurement_coverage` counts measured, missing, and unpriced events
  for each token dimension and cost. Missing dimensions and missing or
  unpriced costs leave qualified known subtotals in both figures. A harness-written cost of
  zero beside nonzero token usage, including a native total without individual
  dimensions, is unpriced; it is never a zero estimate. Consumption cost is
  absent when no counted event is priced; its measurement coverage and
  incomplete reason still report the unpriced events.
  Consumption cost uses the same estimate kind and missing pricing basis as
  every other cost. Its `cost_coverage` marks missing or unpriced event costs
  incomplete, counts every zero-written event as an observation, and counts
  assistant-message requests under `zero_written_requests`.
  `incomplete` names these quantities, unreadable records, and requests
  that stopped on an error or an abort, whose usage stopped with them.
  Quantities are complete only with `session` coverage and no
  `incomplete` reason. Cross-recording identity still needs an established
  account namespace. Nothing is counted for a branch the read did not reach.

  Plain Claude usage chooses a request sum when the newest cost-state is
  below it or omits its counters. The accounting note names that total and
  the fallback reason; recorded_total retains its counters and dollar facts
  under recorded-total/session accounting, separate from the request sum.
  Requests' dimensions retain compatible source evidence independently of the
  token witness. A sparse later record erases no known dimension. TTL splits
  apply only when they match the selected cache-write count; incompatible
  categorical values, TTL splits and tool observations are withheld and
  counted per request under conflicting. Tool counters select their own
  dominant observation without summing alternatives; only fresh evidence
  dominating all earlier observations can discharge a tool conflict.
  At most 16 TTL-count
  witnesses are held per request; excess observations join omitted_values.

  `usage SESSION --reconcile` is Claude-only. It reads the parent and every
  subagent recording whole and retains the newest cost-state's counters and
  dollar facts, including per-model estimates, under their own accounting.
  Request IDs deduplicate across a validated native family: children under
  the parent's subagents directory must positively witness its native session
  identity and name no agent but their own. Such children share the parent's
  request namespace, reported as validated-family under identity_coverage.
  Per-recording figures remain visible. Foreign, unvalidated or unenumerated
  children, and sources outside the family, withhold request-namespace and
  leave family counts, counters and differences absent. Failed assistant records leave consumption unmeasured
  whether usage is an object, absent or null; hook answers made no request.
  A later witness dominating every earlier counter discharges provisional
  request ambiguity. The newest cumulative observation, count and reset
  evidence are held incrementally with constant observation memory.

  Comparable counters keep a signed recorded-minus-identified difference:
  zero reconciles, a negative value conflicts, and a positive difference or
  an incomplete measurement is a discrepancy. No positive difference proves
  consumption beyond the requests: recorded-total membership is missing.
  Resets, unresolved witnesses, absent counters or an unmatched comparison
  frontier withhold differences. Parent file positions order parent requests;
  child requests are ordered by time against the total's own timestamp or,
  for a cost-state, which writes none, the newest timestamp of the parent
  records written before it. A child request without a timestamp, a total no
  timestamp orders (named in a note), or a child request after the total
  cannot contradict that total. Each source names the exact
  revision, length and SHA-256 read under the report's rules. A source that
  could not be used carries `error`, and an unenumerated family a note: the
  session id and one fixed reason (foreign-identity, no-identity,
  source-unreadable-or-changed, subagents-unenumerated), never error text,
  which can name a recording path.

  `usage SESSION --event-keys[=N]` exposes Pi's counted consumption identities
  under `consumption_event_keys`: 200 recent keys by default, 1..=10000 keys,
  and an 8 MiB budget over serialized keys. `omissions` counts the row cap,
  byte budget, oversized keys, unidentified events, private or invalid ids,
  and unavailable source keys. Measurements still cover the whole read;
  an omitted key never removes its known usage. Keys and measurement figures
  use the transcript's own source frontier, including
  entries appended after discovery. `coverage` and
  `unreadable_records` qualify the key read independently, and
  `unrecognized_usage` counts records in the read that carry usage no reading
  counts as consumption, which therefore have no key. A source that
  cannot be replayed sets `unavailable: source-unreadable-or-changed` and keeps
  the held measurements; unknown identity counts stay absent.
  Each key names `kind: provider-response` or `kind: recording-entry`.
  Empty provider, response, or entry ids establish no identity; an available
  recording coordinate identifies the event when no provider response does.
  Provider responses carry provider and response ids with
  `provider_account_namespace: unestablished`. Recording coordinates carry
  session and entry ids; matching ids alone do not establish equality across
  recordings. Identity fields contain ASCII letters, digits, dots, colons,
  hyphens, or underscores, beginning and ending with a letter or digit.
  Paths, whitespace, and diagnostics are omitted and counted.
  `copy_of` witnesses a matching entry in the named fork source, verified
  against its normalized consumption digest; it does not establish a provider
  account namespace. The digest includes entry type, timestamp, usage,
  recorded model, observed provider response, and stopped-request state.
  `copy_coverage` reports how much of that source was checked, and
  `copy_conflicts` counts contradictory witnesses. A malformed parent record
  qualifies its copy evidence while healthy matching entries remain available.
  Parent paths and raw read errors are never included in the keys.
  Each key carries `measurement`, the event's own amounts: its `disjoint`
  token categories (uncached input, cache read, cache write by TTL, output,
  with reasoning a share of output), its `cost` with kind
  `harness-calculated-estimate` and a missing pricing basis, `cost_coverage`
  where the price is absent or a written zero beside usage, and
  `measurement_completeness` naming each category or counter the event did
  not record, an unpriced zero, or a stopped request. An amount the entry did
  not record is absent, never zero. A measurement is omitted with its key, so
  `omissions` account for both, and it carries no recording locator. The
  digest covers the amounts, so a verified copy holds its source's
  measurement. `session_id`, `forked_from` and `basis` name the selection,
  and `selection: retained-branches` its branch domain: these keys cover
  every retained branch, as `consumption` does.
  `ConsumptionEventKeys::union` in tapes-core takes two selections' keys and
  measurements and computes their union exactly only when every key of both
  is present and measured, both reads cover their recordings with no
  unreadable record and no unrecognized usage, one names the other as its fork parent with
  `copy_coverage: session` and no conflicts, each copy is a key the parent
  holds with an equal digest, and no key shares a provider response with
  another recording's key without a verified copy. The union then counts each
  event once and sums categories; one an event did not record is a known
  subtotal named in `measurement_completeness`, and the total is withheld.
  Otherwise `withheld` names why (omitted-keys, incomplete-read,
  unmeasured-event, unrecognized-usage, mixed-basis, unestablished-lineage, unverified-copies,
  unestablished-identity, mixed-selection for keys of different branch
  domains), each selection keeps its own figure, and `bounds`
  give at-least and at-most per category and cost: at least the larger known
  amount, at most their sum only when every key of both is present, neither
  read holds unrecognized usage, and every event measured that category or
  price completely; a stopped
  request's partial price bounds only from below. Missing parents, bounded copy reads, conflicts, and unrelated
  recordings leave cross-recording equality unestablished.
  `--full --event-keys` uses the pinned recording and streams the parent copy
  check. This option answers one native recording and is separate from
  `--series`; supplied exports and selection summaries refuse it.

  `usage SESSION --series[=N]` opts into a recent bounded suffix of Codex raw
  accounting observations: 200 rows by default, 1 through 10,000 explicitly.
  Rows retain source coordinates, native ordinals when recorded, response IDs,
  model context, counters, quota-only events, repeats, and reset/advance
  classifications, and restate the row's own request counters as disjoint. They are observations, not guaranteed request identities;
  aggregate attribution covers the actual read even when the suffix evicts
  older rows. `--full --series` uses the pinned recording, while summaries,
  supplied inputs, and unsupported harnesses refuse the option.

  stats --idle 45m sets the gap above which timestamped records start another
  working stretch. Active milliseconds sum nonnegative gaps at or below the
  threshold; fewer than two timestamped records omit active, stretches and
  idle_threshold. These figures cover the same read window as recorded span.
  Selection totals sum active milliseconds and stretches across readable sessions.

  stats counts what one recording holds as tapes-stats/16: turns by kind
  beside the kinds the harness can record at all, so a zero for a kind it
  cannot record is not read as none observed, tool
  calls by name with their paired durations and error counts, unpaired calls
  by the boundary that left them unpaired, the recorded clock, the
  distribution of assistant-turn wall clock — one sample per source record
  carrying a model response, from the previous turn's source record — as
  count, median, p90, and max,
  the session's
  token counters with the share of input plus cache read plus cache write its
  cache accounts for, the model and effort split the source recorded, and the
  children its store names. Every figure is a
  count of records the harness wrote, and every total says what it covers:
  turn coverage is read-window when a source bound withheld turns, a duration
  comes from a pair the read holds both halves of, and a cache ratio divides
  recorded token counts rather than cost, within one harness's own convention
  for what its input counter already includes. warnings names the limits of
  the read behind the figures. What a count means for the work is the reader's
  inference: nothing here calls a call wasteful, explains a latency, or says
  why a session ended. stats --full streams the whole recording twice and
  counts every record, so turn coverage is session; a selection with --full
  streams each selected session. A selection also names, per harness, the
  kinds the harness can record that no session read held: across a wide
  selection that is the shape of a reader that stopped recognizing a kind.

  lineage answers which sessions a recording names as relatives, as
  tapes-lineage/4: the session it was spawned or forked from, and the children
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
  Claude lineage fails when subagent directory enumeration or a required
  child-ending read fails, naming the source and retaining the I/O error.
  An absent subagent directory establishes no stored child transcripts.
  Codex lineage notes retain candidate header probe failures with their source
  and I/O error; an unreadable header does not establish an absent relationship.
  These notes apply to bounded reads and lineage --full.
  What each harness records differs: claude names its subagent transcripts and
  the Agent calls that spawned them, codex joins spawn and wait calls to the
  rollout headers naming this session as their parent, opencode reads the
  parent column on either projection, and pi carries a parent reference on the
  child alone. lineage --full streams every record, so a child spawned before
  the tail window is still named.
  A Claude subagent run in the background answers its Agent call at once as
  async_launched; its end reaches the parent later as a task notification
  naming the call, written each time the subagent stops, on a queued-command
  attachment or a user record of task-notification origin. The latest one is
  its completion, its status the disposition, its record id in source. A
  shell task notifies the same way and names no child.
  A Claude subagent a plugin launched has no Agent call in the parent, so the
  parent states neither its spawn nor its end. Where the parent states no end, for that
  reason or because the notification lies outside the read, the completion is
  read from the subagent's own recording: the timestamp of its final record
  when its meta record says it finished, or, for a plugin subagent that takes
  no further input, when that final record is an assistant record that
  stopped on end_turn (known trailing housekeeping records, including
  cost-state and system turn_duration/stop_hook_summary, are skipped), with
  that record's id in source after the recording's file. Identity-bearing records in the bounded opening
  and tail must name the selected native parent, and a record that names an agent
  must name this child; absent or conflicting identity fails the ending read.
  Child endings inspect a 64 KiB tail and the bounded opening first,
  expanding to the configured read bound when no complete substantive tail
  record is available. Counting children reads transcript filenames only.
  Recording and child metadata reads accept only regular opened files and never
  wait for a FIFO writer. Without that ending there is no completion
  stamp, which means not seen to finish rather than running: a child killed
  mid-turn reads the same. A completion the parent's own records state is
  preferred, and the spawn stays unstamped, since the child's first record is
  its own first activity and not the parent's launch.

  Given a scope or a listing filter instead of a session, usage answers that
  whole selection as tapes-usage-summary/14, using the flags list and export
  take and grouping by --by (harness and model unless told otherwise). It sums
  the counters the listing already carries, so nothing is re-read. Read
  counted before a sum: it says how many of a group's sessions recorded that
  counter, and a counter none recorded is absent rather than zero. coverage
  counts the sessions behind a sum by their accounting, including those whose
  harness recorded nothing to sum, and cost is summed only where a harness
  recorded one. Native tokens and cost add only within one harness and
  accounting; a group spanning more sets mixed_accounting and omits them.
  disjoint sums each session's disjoint categories across harnesses, names
  every convention it mapped from, and counts under missing the sessions that
  could not supply a category; its total needs every session with tokens to
  supply all four. Sessions of different accounting basis or coverage add in
  neither form, and partitions keep each harness and accounting's own sums.

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
  A turn record_ref.part_index is its normalized position within the source
  record. A content part repeats that parent coordinate and adds
  content_part_index for its position within the turn; any native pointer
  remains the source pointer and is never repacked.

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
  tapes export <id> --full       Every turn of a Claude, Codex, or Pi recording, streamed into the bundle.
  tapes export --global --since 2026-01-01 --until 2026-01-08

  Three files share one timestamped prefix, and stdout is exactly their paths
  and sizes, in the order they are meant to be read:

  1. .context.md, whole. The turns show --exchange returns: operator turns and
     assistant-visible text — the session's argument, and the small part of
     it. The harness's own commands, notices, attached context, and user turns
     of unknown sender stay out of it and remain in the trace. --only and
     --omit and --origin narrow all three files.
  2. .json, narrowly, with jq. The canonical object plus turns, cost, tokens,
     and the session directory's git head and branch when they resolve. Query
     it for facts; do not print it.
  3. .trace.md, only when free-text search is genuinely easier than JSON.
     Every reasoning and tool turn the transcript carries, in order. A tool
     turn is headed by the tool's name where its envelope carries one, by
     `result` for a bare result, and by `unnamed` otherwise.

  A bundle holds what the transcript held, and reads are capped, so a large
  session exports as a window and says so. --full streams a whole Claude,
  Codex, or Pi recording into all three files instead; OpenCode refuses it.
  Probe with show first: a whole bundle of a large session is large.

  Given the listing flags instead of an id — the scope flags, --harness,
  --model, --launch, --directory, --since, --until, --sort, --limit, --search — export
  takes the set list would return, in the same order, and writes one bundle
  per session. Bundles are never joined: each session keeps its own three
  files, bounded unless --full, and manifest.json beside them is the only file spanning the
  set. It records the selection, every bundle's paths, the sessions whose
  store could not be read, and the listing's own diagnostics, including scoped
  `unplaced` directories, so the set can be audited against the bounded scan
  and store. Each invocation reserves a unique sibling
  prefix before writing, so concurrent exports do not replace one another; a
  session that fails costs its own bundle and nothing else.

  Never ingest a whole bundle merely because it exists. A large trace read in
  full buys little over the context file and costs the budget the actual work
  needs.

READ MANY ENDINGS
  tapes endings --here --since 2026-01-01 --json
  tapes endings --global --harness codex --limit 50 --text

  endings answers what each session of a selection ends on, as
  tapes-endings/10, including a session's recorded launch mode when known, so
  choosing which few endings deserve reading costs one
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
  A compaction notice            A recorded model-context reduction, not source
                                 deletion or completion. Use show --full or
                                 history pages to recover retained earlier
                                 messages; summaries remain ambient evidence.
  An error or limit message      A crash or a quota cut. The cut is arbitrary
                                 and often mid-tool, so say what was in flight
                                 and whether it landed.
  A closing summary, nothing open  Finished. A post-mortem, not a rescue.

  Claude attachment records carry context outside `message`: file attachments
  name paths in `displayPath` and `filename`, with text in
  `content.file.content` under `content.type = text`. Compact file references
  name paths without a body. Both carriers become ambient turns with file
  references under the system role; the path is `displayPath`, then the nested
  `filePath`, then `filename`. Text bodies retain at most 64Ki characters, with truncation
  explicit, and record inclusive line ranges and an optional total line count.
  Unsupported or malformed bodies remain reference-only. References are never
  opened. Instruction attachments become ambient turns with one file reference
  per recorded `files` entry, retaining `path` and optional `content`. At most
  64 files and 128 parts are retained per turn. Environment attachments become
  ambient notices carrying bounded JSON text from `snapshot` and `changes`:
  snapshot fields are workingDirectory, additionalWorkingDirectories, isGitRepo,
  isWorktree, osVersion, platform, scratchpadDirectory and shell; change fields
  are field, from, added and removed, with at most 128 changes. Other fields
  and unsupported shapes produce explicit coverage facts.
  Prompt snapshots are reference-only descriptors: system_prompt_chars counts
  characters across the recorded systemPrompt strings, tool_count counts tools,
  and system_turns retains the recorded systemTurns boolean. Missing values are
  omitted. System prompt bodies are excluded because repeating them would bury
  the conversation. Other attachment types remain unrecognized.
  Every retained attachment body has a 64Ki-character field budget. A cut body
  has ReadBound availability (`read-bound` in JSON) and an omitted_reason;
  collection and descriptor bounds are also explicit coverage facts. These
  content bounds apply independently of source-file and page read bounds.
  Repeated records remain separate turns even when their text matches.
  Claude's `api-request-blob` records are the exception: each restates one
  message of a request Claude sent, its system prompt included, under no
  timestamp or uuid, so it becomes no turn, names no spawn and no model, and
  counts as `declined` in `read.unmapped`.

  A Claude tool-result string with a complete `<persisted-output>` envelope
  shows its recorded preview and a reference-only file under `tool-results/`.
  The reference retains the rounded native `recorded_size` label, not an
  inferred exact byte count, and the originating `tool_call_id` when recorded.
  show --json and events --json include an opaque versioned `reference` token.
  tapes artifact <id> --reference <token> [--bytes 64KiB] [--json]
  reads only the named body beside that recording. The budget is 1 byte to
  4 MiB. tapes-artifact/1 reports bytes returned, truncated, SHA-256 over those
  bytes, encoding (utf-8 or lossless hex), and body. Tokens bind the native
  session ID, source descriptor and originating result; changed results and
  other sources refuse. The envelope must name the selected session directory.
  Session directory, tool-results and file symlinks refuse, as do non-regular
  files. Platforms without confined no-follow opens refuse retrieval.
  Ordinary transcript and event reads never open the body. Malformed envelopes
  remain ordinary tool payloads; path-like text
  outside a tool result does not establish a persisted-output reference.

  Every turn carries a `kind`, filled only from what the harness itself
  wrote, a field beside the text or an element it wraps its own text in:
  `operator` for a message addressed to the agent, `control` for a harness
  command such as `/exit`, `notice` for a message the harness injected,
  `ambient` for context it attached, and the role's own name for assistant,
  reasoning, and tool turns. A user turn the harness left no evidence for is
  `unknown`, which is an answer rather than a gap — read its text before
  deciding what it was.

  Operator turns carry an orthogonal `origin` only with positive sender
  evidence. Headless sessions assign `program` to every operator turn.
  Otherwise Claude assigns `harness` for `isMeta: true`, or `person` for
  `promptSource: typed`; `queued` and absent promptSource do not identify a
  person. Pi assigns `harness` to extension `custom_message` entries and
  `person` to user `message` entries only with interactive launch evidence.
  Codex records no sender: interactive operator turns have no origin.
  All other cases omit origin. No message text establishes origin. `show`
  and `export` accept --origin person|program|harness alongside --only;
  absent origins never match. The projection records the origin selector
  and counts rejected turns by kind. Origin does not identify a launcher or
  resolve agent-delegate
  handles; agent-delegate status names its session, and a pi handle is already
  the session id.

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
  large remainder means the user changed direction and `usage` reports what
  those branches consumed; a claude session's
  subagent transcripts, which live in separate files and whose endings are not
  the parent's; lines that would not parse; an opencode page boundary. A
  subagent count requires successful directory enumeration; failed enumeration
  produces a diagnostic note instead of a count. Codex
  reasoning is frequently encrypted and arrives as `[encrypted reasoning]`, so
  that placeholder marks reasoning you cannot read rather than reasoning that
  did not happen. Read both signals and state what is missing rather than
  writing over the gap.

CONTINUE A COLD SESSION
  tapes brief <id|--latest>
  tapes brief <id> --tail 20 --json

  A continuation has two halves. brief is the transcript's half, as
  tapes-brief/14: where the session stopped, the directory it worked in and the
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

  A brief's bounded tail cannot carry a long session, or one whose relatives,
  executors or delegated runs hold half the work. `agent-delegate pickup`
  does: it freezes the session with tapes workset, fans cheap read-only scouts
  over the lanes, and keeps only the facts tapes locate verifies, then names
  what the lead rereads before writing the continuation. Its guide (bare
  `agent-delegate`, PICK UP A SESSION) carries the procedure.

FREEZE A WORKING SET
  tapes workset <id> --bundle /tmp/<id>-workset
  tapes workset <id> --bundle DIR --lane-bytes 256KiB

  workset freezes a session and the relatives its own lineage resolves —
  its parent, the session it was forked from, its children; lineage is
  followed one level, never to a parent's parent or a child's children — reads
  each in full, and writes DIR/workset.json as tapes-workset/1 beside one
  lane file per lane, DIR/lanes/<lane-id>.md. A DIR that already holds either
  is refused, never overwritten: rerun against a fresh directory. Each source
  gets a short key (s0 is the root) and is cut into lanes: contiguous
  turn-ordinal ranges sized by the lane projection, the per-turn rendering a
  lane carries. The projection caps each turn's text at 16KiB for operator
  and assistant turns and 2KiB for every other kind, marking the bytes it
  left out; each lane and source counts them. A lane holds as many turns as
  fit in --lane-bytes (64KiB by default, 8KiB to 4MiB) and is never cut
  inside a source record: the turns one record yields (a message with
  several text blocks, say) stay in one lane, so file-byte ranges tile. A
  record, or a single turn, whose capped rendering exceeds the bound gets a
  lane of its own and is the one lane over it. A lane
  file names its lane, source and turn range, then a marked margin of
  read-only context — the last two operator turns earlier in the same
  source, each with its first assistant reply, or a line saying none
  precedes the lane — then a marked lane section holding exactly the lane's
  projected turns. A turn is cited `<key>#<ordinal>`, and only turns in a
  lane section are cited from that lane. Markers and turn headings are whole
  lines that transcript text cannot forge: a text line beginning with `## `,
  `=== `, `[… `, `tool: `, `artifact: ` or a backslash is rendered with one
  more backslash in front.

  The manifest pins each source's length, revision and file prefix digest
  where the store has a file, and for every source a content digest: SHA-256
  over each turn's whole rendering, in order, with the turn count it covers.
  Its coverage says what the read could not
  turn into turns: skipped records, bytes no turn covers, notices, references
  the store cannot resolve, and failed child enumeration. A relative that
  cannot be read is listed with its diagnostic; only an unreadable root
  fails. Lanes partition the turns exactly, so dividing the reading of a long
  session along them leaves no turn unread or read twice.

LOCATE WHAT A WORKSET CITES
  tapes locate --workset DIR
  tapes locate --workset DIR --refs facts.jsonl --text-bytes 64KiB --json

  locate checks a frozen workset against its sources and resolves the
  references cited from its lanes, as tapes-locate/1. Each source is replayed
  once, pinned to the length, revision and prefix digest the workset froze
  where it has a file, and its content digest is recomputed over the same
  first turns in that pass: `unchanged`; `grown` when only appended content
  follows frozen content that still matches, the normal case for a live
  session; `changed` when it is shorter, its frozen prefix or frozen content
  differs, or it has neither to compare; `unreadable`, with the diagnostic. Any `changed` source
  makes the workset `stale`: freeze a new one rather than trust its lanes.

  --refs takes JSON lines whose `ref` is `<key>#<ordinal>` and whose
  optional `lane` is the lane the citation came from; any other field is
  ignored, so a scout's fact file passes as it is. Each reference is
  `resolved` — kind, origin, timestamp, the lane holding it, and its record
  reference — or `unknown` (no such key, or past the frozen turn count even
  when the source grew), `outside-lane` (not in the cited lane; a margin
  turn never is), or `source-changed`. A blank or non-JSON line is
  `malformed`, named by its line number. --text-bytes returns a resolved
  turn's whole text, past the lane projection's cap, up to the bound and
  marked when cut. Outcomes are data: locate exits 0 whenever it reports,
  and 1 only when workset.json or the refs file cannot be read.

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
  Read a Claude child's own transcript, usage and ending as tapes-child/10,
  retaining its parent and reference. It is not an ordinary listed session; nested lineage
  remains uninspected and child activity is never added to parent totals.
  The child's id is qualified as PARENT::CHILD, and a child whose recorded
  native session ids do not name the parent, or name none, is refused.
  --full streams the child's whole recording: usage covers every turn, and the
  transcript and ending keep the newest --tail turns.
HISTORICAL READS
  tapes page SESSION --bytes 65536 --json
  tapes history-search SESSION --search TEXT --pages 8 --json
  tapes metadata SESSION --pages 8 --json
  Claude and Codex history is paged backward, with chronological turns within
  each page. Pass next_cursor back as --cursor. Strict cursors are the default
  and refuse any source revision change; malformed and oversized record gaps
  remain explicit. Budgets protect context:
  1 KiB–4 MiB per page, 1–32 pages per search, 100 excerpts/observations.
  To walk a growing recording, start page, history-search, or metadata with
  --pin. It hashes the fixed prefix in one streaming pass with bounded memory
  and refuses prefixes over 1 GiB. At most 256 SHA-256 chunks are anchored
  downward from the pinned end. Later pages verify the chunks covering their
  window and alignment byte, reading at most 3 × 4 MiB for verification.
  Appends past the pinned end are ignored. Replacement and truncation refuse
  immediately; a rewrite inside the prefix refuses when its chunk is reached,
  naming the byte span. Verified records, prefix digest and source revision
  retain the pinned generation across page, search and metadata continuations.
  Pinned turns use the launch mode observed when the walk began for origin
  attribution, even if a later append changes the session's launch evidence.
  The cursor carries the mode: continue with --cursor alone. --pin and --cursor
  conflict, and a strict walk must restart to acquire a pin.
  Page JSON uses tapes-page/12 and reports pin.chunk_bytes, chunks,
  pinned_bytes, verification_bytes and verified spans. read.ranges marks these
  spans as verification; source_prefix_digest identifies bytes 0..pinned_bytes.
  Use --bytes equal to pin.chunk_bytes to limit verification amplification.
  Chunks already passed are not rechecked, and mutations during the initial
  hashing pass are part of its observation. A pin does not preserve bytes
  through a rewrite. For durable bytes, capture a copy and read its native
  store with --native-store and --native-root; --pin also works on that copy.
  Search JSON uses tapes-history-search/11. Matches retain the turn's source
  reference, native ID and timestamp when recorded, and its kind. Page byte
  bounds and zero-based ordinals locate matches within a page; ordinals are
  not session indices. Human matches show kind and recorded timestamp.
  Metadata JSON uses tapes-metadata-history/11. Its pages extract model
  observations without decoding transcript turns; their read evidence uses
  the page schema with the `models-only` option. Older model observations never silently replace current session
  metadata.
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
