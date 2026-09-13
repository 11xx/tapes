# OpenAI and supplied conversation exports

`--input` reads caller-supplied exports through the same normalized session,
turn, content, source, and read-evidence model as installed recordings. The
input path is an observation boundary, not a harness store: it is never used
to discover another source and it is never modified.

## Supported containers

The reader accepts a single JSON or JSONL file, a directory containing JSON
members, and a ZIP archive. ZIP members are read directly; no member is
extracted. Absolute names, parent traversal, duplicate normalized names, and
symlink members are rejected. Only JSON, JSONL, and recognized JSON `.dat`
report members are inspected, so HTML, workbooks, media, and account assets
are not parsed as conversations.

The official OpenAI export stores conversation shards as top-level arrays.
Each conversation has a native `conversation_id`, a `mapping` graph, and a
`current_node`. ChatGPT Exporter raw inputs use a native `id` with the same
mapping shape, while its convenience inputs may be a conversation object with
`messages` or `entries`, an array of such objects, or one conversation per
explicit file/member. Auto detection uses these structural distinctions; a
filename does not choose a producer. A declared `--input-format` that does not
recognize a record reports the bounded diagnostic and does not try another
adapter.

## Mapping graph

The canonical transcript follows `current_node` through `parent` links and
reverses that path into chronological order. The `graph` projection retains
bounded nodes and parent-child edges, and `selected_path` names the branch
projected into the transcript. A missing current node produces a session with
no canonical turns while retaining the graph and an explicit note. Cycles,
dangling parents, graph bounds, unknown roles, and unrecognized content shapes
are reported as evidence gaps or notes; traversal never follows an unbounded
recursive graph. Native node and message IDs, JSON pointers, and absolute
record spans remain on normalized turns and graph messages as `record_ref`
values.

The normalized roles are `user`, `assistant`, `system`, `developer`, `tool`,
and `reasoning` where the source supplies them. `channel`, `recipient`,
request-turn IDs, timestamps, model metadata, and recorded directory values
remain absent when the export does not provide them. Filesystem timestamps are
not substituted.

## Content and associated reports

Message content keeps ordered text, transcription, media/file references,
structured descriptors, tool payloads, and unknown parts. Bodies are retained
only for recognized text-bearing parts; references identify a source object
without opening it. Shape descriptors keep keys and value types while
excluding body values.

OpenAI library/report `.dat` members are decoded only when they contain the
recognized widget-state report shape. A report reference retains bounded
identity, origin, backing conversation, authorship, completion state, source
member, byte size, citation count, a bounded body when present, and native
citation spans. When a report names a uniquely reached conversation it is
attached as a structured artifact part of an existing turn and also remains
in the artifact collection. A report without a reached outer conversation
remains an explicit artifact with its unresolved association; it never becomes
a fabricated message or turn.

## Bounds and continuation

Each invocation has independent limits for source bytes, decoded bytes, one
record, members, structural depth, and serialized output. The defaults are
512 MiB source and decoded bytes, 8 MiB per record, 10,000 members, depth 128,
and 16 MiB output. The byte flags can raise the bounded values only to their
documented finite ceilings. Oversized records are skipped structurally when
possible, with a `read.gaps` entry and a diagnostic. If synchronization or a
budget stops the scan, the result is partial and an unreached occurrence is
not reported as absent.

`list` emits an opaque `occurrence` coordinate for every supplied record. The
coordinate binds the ordered set of supplied files and ZIP members, not just
the source that produced one row, so continuation can cross input files while
changed input refuses the cursor.
Single-session commands accept that coordinate through `--occurrence` when a
native ID or title is duplicated. `--after-occurrence` resumes collection
listing only when the ordered supplied-input observation matches the
observation that emitted the coordinate; changed input refuses continuation.

The normal output contracts remain versioned (`tapes-list/3`,
`tapes-session/5`, `tapes-events/4`, `tapes-brief/3`, `tapes-endings/3`,
`tapes-stats/3`, `tapes-usage/3`, and `tapes-export-manifest/3`). History-page
and child-qualified operations are installed-recording capabilities and
refuse supplied exports rather than interpreting an export as JSONL history.
