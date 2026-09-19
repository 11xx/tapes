# OpenAI and supplied conversation exports

`--input` reads caller-supplied exports through the same normalized session,
turn, content, source, and read-evidence model as installed recordings. The
input path is an observation boundary, not a harness store: it is never used
to discover another source and it is never modified.

## Supported containers

The reader accepts a single JSON or JSONL file, an extracted directory, and a
ZIP archive. ZIP members are read directly; no member is extracted. Absolute
names, parent traversal, duplicate normalized names, and symlink members are
rejected before a selected body is read. A native container with a root
`export_manifest.json` selects only the declared `conversations.json` shards,
`library_files.json`, and manifest-declared `.dat` library members. Nested
`sites/export_manifest.json`, account/settings JSON, HTML, workbooks, and
unselected JSON are not conversation inputs. A directory without the native
manifest and an explicitly supplied file keep structural source-shape
recognition, including recognized JSON `.dat` reports in a directory or ZIP.

The native manifest's version, logical member names, duplicate normalized paths,
and `export_files.size_bytes` values are validated before selected bodies are
read. Missing, malformed, corrupt, or size-mismatched selected members remain
explicit coverage gaps; they never become an empty successful collection or an
assertion that an unreached ID is absent. A ZIP member whose decompression or
checksum verification fails contributes no records: bytes already parsed from
it are not the archived bytes, so its records are withheld with a diagnostic.
A member cut short by an input budget keeps the records it reached, with gaps.

The official OpenAI export stores conversation shards as top-level arrays.
Each conversation has a native `conversation_id`, a `mapping` graph, and a
`current_node`. ChatGPT Exporter raw inputs use a native `id` with the same
mapping shape, while its convenience inputs may be a conversation object with
`messages` or `entries`, an array of such objects, or one conversation per
explicit file/member. Auto detection uses these structural distinctions for
the representation but does not infer producer provenance from overlapping
mapping fields; ambiguous auto reads omit `source.producer`. A filename does
not choose a producer. A declared `--input-format` records its producer as a
caller declaration and, when it does not recognize a record, reports the
bounded diagnostic without trying another adapter. A conversation record
without its native `conversation_id` or `id` is a `missing-native-id` gap with
a diagnostic; the reader never invents an identity for it. The record was
parsed in full and holds no ID, so the gap does not block ID selection; it
still blocks title selection, because the record may carry the title.

Both OpenAI and ChatGPT Exporter mapping representations use `source.origin`
with value `"openai"`; `source.representation` identifies the detected or declared
representation, and `source.producer` is the acquisition label only when an
explicit input format supplies it or the source establishes it independently.

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
values. `part_index` identifies the normalized turn within its source record;
content-part references repeat it and add `content_part_index` for the part's
position within that turn. The native JSON pointer remains unchanged.

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

The official export names a message's representation in
`message.content.content_type` and an object part's kind in its own
`content_type`, where other producers use `type`. Voice conversations are
`multimodal_text` messages whose `audio_transcription` parts carry the spoken
text and whose `image_asset_pointer`, `audio_asset_pointer`, and
`real_time_user_audio_video_asset_pointer` parts become media references
identified by `asset_pointer` and `size_bytes`; the real-time part's reference
is its nested `audio_asset_pointer`. Reasoning arrives under the assistant
author as `thoughts` messages, whose `thoughts[]` entries carry `summary` and
`content`, and `reasoning_recap` messages with one `content` string. Both
become `reasoning` turns with those strings as text parts; a reasoning record
with no such string keeps an unknown part naming its content type.

OpenAI library/report `.dat` members are decoded only when they contain the
recognized widget-state report shape. A report reference retains bounded
identity, origin, backing conversation, authorship, completion state, source
member, byte size, citation count, a bounded body when present, and native
citation spans. Citation kind, URI, and title strings use the shared bounded
text representation: each field is limited to 4 KiB of UTF-8 bytes and all
citation descriptors share a 16 KiB cumulative byte bound. A truncated URI is incomplete,
not a valid altered reference. Grouped citation spans retain bounded nested
source URLs and titles, with omission counts at each bound. Structural
traversal bounds are exposed separately when an omitted count cannot be
established.

`library_files.json` associates a report with its member by `file_id`, which
the official export spells as the member stem (`file_<hex>` for
`file_<hex>.dat`). An entry's `origination_thread_id` (or
`initiating_conversation_id`) becomes the reference's
`originating_conversation`, and `backing_conversation_id` its `backing`; a
report's own widget-state `backing_conversation_id` names a backing session
the export may not contain, and stays `backing` beside the originating
conversation rather than replacing it. `origination_message_id` names the
message the report came from. When the backing conversation, originating
conversation, or originating message identifies exactly one reached
conversation, the report is attached as a structured artifact part of an
existing turn and also remains in the artifact collection. A report without a reached outer conversation
remains an explicit artifact with its unresolved association; it never becomes
a fabricated message or turn.

## Bounds and continuation

Each invocation has independent limits for compressed/source bytes, decoded
bytes, one record, aggregate resident normalized data, members, structural
depth, and serialized output. The defaults are 512 MiB source and decoded
bytes, 8 MiB per record, 512 MiB resident data, 10,000 members, depth 128, and
16 MiB output. The byte flags can raise the bounded values only to their
documented finite ceilings. Reader I/O and ZIP central-directory inspection
consume the source budget; uncompressed bytes consumed by scanners and report
readers consume the decoded budget, including skipped oversized records. A
record is retained only when its bounded bytes fit the remaining resident
budget. Oversized records are skipped structurally when possible, with a
`read.gaps` entry and a diagnostic. If synchronization or a budget stops the
scan, the result is partial and an unreached occurrence is not reported as
absent. Title selection refuses incomplete discovery, and ID selection refuses
every gap except `missing-native-id`; an exact
`--occurrence` may return the known evidence with its gaps. Its `read.gaps`
contains only that member's byte coordinates; collection-wide limitations
remain in the `input-coverage` bound and bounded diagnostic notes.

`list` emits an opaque `occurrence` coordinate for every supplied record. The
coordinate binds the ordered set of supplied files and ZIP members, not just
the source that produced one row, so continuation can cross input files while
changed input refuses the cursor.
Single-session commands accept that coordinate through `--occurrence` when a
native ID or title is duplicated. `list` orders supplied rows by `--sort`
(recorded activity, then native ID, then source order) before applying
`--limit`, and `--after-occurrence` resumes after the named row in that same
order, only when the ordered supplied-input observation matches the
observation that emitted the coordinate; changed input refuses continuation.
A named `--input` path that does not exist is an error, not an unavailable
store.

`occurrence` and `record_ref.domain` identify one observation of the input and
nothing more. `domain` embeds the input path as the command line spelled it,
so the same archive named relatively and absolutely yields two domains, and
`occurrence` hashes the path with the file's device, inode, size, and times.
Both are correct as cursors within the observation that emitted them and must
not be persisted as the identity of a conversation or a record. What does
survive the input moving is the record's bytes: `read.record_sha256` holds the
SHA-256 of each span in `read.records`, in the same order, computed over the
source bytes the span covers, so a consumer can bind a retained citation to
exact bytes without decoding the input again. `read.reader` names the build of
the reader, so a changed projection of unchanged bytes is attributable to the
reader rather than to the source.

`export --evidence` copies those bytes out of the input beside each supplied
conversation's bundle, into a `<bundle>.evidence/` directory. The
conversation's record is its span copied byte for byte out of its file or ZIP
member; each report the projection associates with the conversation is its
whole ZIP member. Every file is named `sha256-<hex>` with its source's
extension, so the projection's `read.record_sha256` names the record file
directly and every `record_ref.pointer` resolves inside it without the input.
A `tapes-evidence/1` `manifest.json` records the input file's byte length and
SHA-256; `reader` and the projection's `schema` and `projection_options`; per
file its `role` (`record` or `report`), `sha256`, `bytes`, the ZIP member's
`name`, `size`, and `crc32`, and a record's `span`; per association the report
member, which of the report's fields (`backing`, `originating_conversation`)
named the conversation, and whether it was copied; and `gaps` for anything not
copied, including the read's own gaps. Members are read to their end so the
archive's checksum is checked: a member that fails decompression or its
checksum is a gap, never a file. A record whose bytes no longer match the
digest the read recorded is refused as a changed input. A report is copied
only out of the ZIP that holds its conversation. The directory is written
under a `.partial` name and renamed into place whole, and the input is only
ever opened for reading.

The normal output contracts remain versioned (`tapes-list/5`,
`tapes-session/10`, `tapes-events/6`, `tapes-brief/6`, `tapes-endings/6`,
`tapes-stats/6`, `tapes-usage/5`, and `tapes-export-manifest/5`). History-page
and child-qualified operations are installed-recording capabilities and
refuse supplied exports rather than interpreting an export as JSONL history.
