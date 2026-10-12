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
  IDs and prefixes resolve only in the selected store. `self` uses the caller's
  exported identity but must resolve within that store. --latest and --title
  default to the selected store across all recorded projects; --here,
  --project, and --global give an explicit scope. A removed recorded working
  directory does not prevent direct ID or global-store reads.
  source.kind is explicit-native-store, source.location.container names PATH,
  and source.location.locator names the exact recording. Recording reads accept
  only regular opened files, without waiting for FIFO writers. Identity-bearing
  records must match the selected native session (the parent for Claude children).
  Bounded reads check the opening and retained tail; full reads check every record.
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
  Whenever cost or tokens are present, JSON also carries accounting stating
  whether the figures are a recorded total or a sum of requests and whether
  they cover the session or only the bounded read window. A cost carries
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
  candidates. --harness <h> restricts ID and prefix lookup to that harness's
  stores. Incomplete coverage names incomplete and completely searched harness
  stores and asks for the full id or --harness <h>; a single observed prefix
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
  feature, ZIP archives as a separate
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
  SHA-256 of each span in read.records, in order.

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
  tapes show <id> --json         tapes-session/19 turns, typed context events, and bounded read, graph, artifact, terminal, and recorded launch evidence.
  tapes show <id> --exchange     Operator and assistant turns only, timestamps and ordinals kept; the name for --only operator,assistant.
  tapes show <id> --only operator --origin person  Operator turns with recorded person origin.
  tapes show <id> --omit tool    Every turn but tool calls and results; --only and --omit take kind labels, and export takes both.
  tapes show <id> --full         Every turn of a Claude, Codex, Pi, or OpenCode session, streamed past the read bounds; --json streams it too.
  tapes events <id> --json       Typed tool calls, results, pairs, and content parts.
  tapes capture <id> --json      The recording's store class and custody state.
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

  usage answers where a session's quota went as tapes-usage/11: its recorded
  launch mode when known, tokens, cost, accounting, turns, and any by-model attribution the source can
  support — Claude's recorded cost-state, Codex's request observations, or
  pi's per-message model each qualified by the thinking level in effect. Read
  accounting before adding anything up — basis says
  whether a figure is a recorded total or an observed request sum, and
  coverage says how much of the session it covers. An observed cumulative
  counter restart is `since-reset`; the newest recorded total remains visible
  without being rebuilt from request rows. Codex quota observations preserve
  credits balances and reached-limit flags exactly as recorded, including false
  and string zero; absent fields remain unknown. `usage --full` streams the
  whole recording and folds its counters. Turns and complete request reads
  cover the session; recorded totals keep their epoch coverage. Pi's active
  accounting stays `read-window` when an unreadable record cuts its branch
  or follows its last entry.
  Pi selections keep each session's figure. Event identity across sessions
  is unestablished, so a multi-session group withholds exact token and cost
  unions. Within a compatible accounting partition, `identity_bounds` reports
  at least the largest known figure and at most their sum; a missing or partial
  counter leaves its upper bound unmeasured. Different bases have separate
  partitions, and `session_figures` preserves per-counter coverage without
  recording locators.

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
  (Claude and Pi sessions, a Codex or Pi per-model split) an event is a
  counted request, and a request that wrote no usage counts with every
  counter missing; for a harness-written total (a Codex or OpenCode session,
  or a Claude cost-state, which counts the models it sums) the total is the
  event. A counter some events lack is a qualified known subtotal, never
  complete, and `measurement_incomplete` says how many recorded it. A
  summary's `measurement_coverage` adds its sessions' events and is withheld
  wherever its tokens are.

  `measurement_completeness` answers, counter by counter, whether a figure is
  completely measured and why not. It names each incomplete counter, or
  disjoint category, with fixed reasons: missing-counter (some counted events
  did not record it, or the figure has none), missing-price, unpriced (a
  written zero beside usage), stopped-attempt (a request stopped on an error
  or an abort, or failed, so its consumption is partial or unknown, even with
  every counter populated), unreadable-record, read-window, since-reset,
  unestablished-scope, incomplete-attribution (a model split whose read could
  not attribute every request), and undetermined (the convention cannot
  derive the category). A counter it does not name is complete; a present
  amount it names is a known subtotal. Usage, consumption, model splits,
  summary totals, groups and partitions, session figures, brief, stats, and
  session JSON all carry it, and every figure derived from a counter follows
  it: a disjoint category carries its native dependencies' reasons, the
  disjoint total and an identity upper bound need complete counters, and a
  stats cache ratio needs complete input and cache counters. A model split
  carries its own reasons plus those no single model can be spared: the
  read's coverage, failed requests, incomplete attribution. One model's
  stopped request leaves another model's figure whole, and one session's
  reasons leave another session's figure and group whole.
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
  A Claude request sum keeps the TTL split; a
  cost-state total merges it, so its writes are ttl_unrecorded. requests
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
  failures are counted separately and their consumption is unknown.

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
  identity. Such children share the parent's request namespace, reported as
  validated-family under identity_coverage. Per-recording figures remain
  visible. Foreign, unvalidated or unenumerated children, and sources outside
  the family, withhold request-namespace and leave family counts, counters
  and differences absent. Failed assistant records leave consumption unmeasured
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
  child frontiers require timestamps. A missing frontier or a child request
  after the total cannot contradict that total. Each source names the exact
  revision, length and SHA-256 read under the report's rules.

  `usage SESSION --event-keys[=N]` exposes Pi's counted consumption identities
  under `consumption_event_keys`: 200 recent keys by default, 1..=10000 keys,
  and an 8 MiB budget over serialized keys. `omissions` counts the row cap,
  byte budget, oversized keys, unidentified events, private or invalid ids,
  and unavailable source keys. Measurements still cover the whole read;
  an omitted key never removes its known usage. Keys and measurement figures
  use the transcript's own source frontier, including
  entries appended after discovery. `coverage` and
  `unreadable_records` qualify the key read independently. A source that
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
  Keys establish event membership only. They carry no per-event token or cost
  amounts, and their digests cannot recover those measurements. An exact token
  or cost union requires normalized per-event amounts with quantity kind,
  accounting basis, and per-counter coverage; the keys alone leave it
  unmeasured. Per-event measurement support is tracked by the tapes todo
  `consumption-key-amounts`.
  An exact parent/fork membership union requires complete key coverage and verified
  copy equality for shared entries. Missing parents, bounded copy reads,
  conflicts, and unrelated recordings leave cross-recording equality
  unestablished; each recording's own figures remain available.
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

  stats counts what one recording holds as tapes-stats/14: turns by kind
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
  A Claude subagent a plugin launched has no Agent call in the parent, so the
  parent states neither its spawn nor its end. When such a subagent takes no
  further input, its completion is read from its own recording: the timestamp
  of its final record, if that is an assistant record that stopped on end_turn
  (known trailing housekeeping records, including cost-state and system
  turn_duration/stop_hook_summary, are skipped), with that record's id in source
  after the recording's file. Identity-bearing records in the bounded opening
  and tail must name the selected native parent; absent or conflicting identity
  fails the ending read. Recording and child metadata reads accept only regular
  opened files and never wait for a FIFO writer. Without that ending there is no completion
  stamp, which means not seen to finish rather than running: a child killed
  mid-turn reads the same. A completion the parent's own records state is
  preferred, and the spawn stays unstamped, since the child's first record is
  its own first activity and not the parent's launch.

  Given a scope or a listing filter instead of a session, usage answers that
  whole selection as tapes-usage-summary/6, using the flags list and export
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
  tapes-brief/12: where the session stopped, the directory it worked in and the
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
