# Perplexity conversation exports

`--input-format perplexity` reads the official JSON member of a supplied
Perplexity export. The format is recognized by a top-level `conversations`
array or by a conversation object containing `context_uuid` and `entries`;
filenames and unrelated ZIP members do not choose the adapter. ZIP workbooks,
HTML, and other profile assets are not parsed.

## Conversation and entry records

Each conversation keeps `context_uuid` as its native session ID and
`context_title` as its recorded title. `created_at` and `updated_at` supply
optional session activity timestamps. `collection_uuid` and conversation-level
`mode`, `engine_mode`, `query_status`, and `label` are retained as session
metadata only when those fields are present on the conversation object. A mode
or research label is not a model, and a completed query status is not a claim
that every artifact body was captured. Entry-level `engine_mode`,
`query_status`, and `label` remain on both turns projected from that entry as
`turn.metadata`; each normalized field names its native `/entries/<index>/...`
source pointer. Entry values are not promoted to conversation facts.

Entries remain in source order. `entry_uuid` is the native entry identity and
is shared by its query and answer records. A query becomes an operator content
record under `entry.query`, and an answer an assistant record under
`entry.answer`. `entry.created_at` belongs to the query record.
The answer does not receive a copied or fabricated timestamp. Empty metadata
strings remain present, while null and absent metadata fields remain absent.
Empty strings are retained as empty text parts, nulls remain explicit unknown
coverage, and non-string values retain a bounded shape descriptor rather than
being coerced to text. Only a non-empty string is an operator request or an
assistant answer: an empty, null, or non-string field keeps its part and
coverage on a turn of kind `unknown`, so it never stands as a request or closes
an exchange.

Answers remain text evidence, including Markdown, Mermaid fences, numeric
markers, and progress prose. Numeric citation-like syntax does not create a
typed reference without a recorded target. A file-shaped reference is a
descriptor only; the reader never opens or downloads it. When the export has
no artifact inventory, artifact coverage remains unknown rather than proving
that no file existed.

Invalid non-null timestamps are omitted and named in notes. Missing labels,
statuses, modes, and model data remain absent. Repeated context or entry IDs
are separate supplied observations and use the shared opaque `occurrence`
coordinate; the reader never applies last-write-wins or merges by title/text.

## Bounds and views

The envelope array is iterated structurally, so a multi-megabyte export is not
deserialized as one root value. The shared scan, decoded, record, resident,
member, and output budgets apply to every member. An oversized answer or an
interrupted scan leaves a gap and cannot become a not-found claim. List, show, brief,
events, stats, usage, lineage, endings, and bundle export use the same
normalized records. History pages and child-qualified reads refuse supplied
exports because they require installed-recording capabilities.
