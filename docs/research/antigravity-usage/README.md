# Antigravity native usage accounting: investigation and implementation handoff

**Status:** research and local diagnostic only; production implementation pending review.
**Inspected revision:** [c12fb01](https://github.com/11xx/tapes/commit/c12fb01a65bd725d21594c83553c7c8588c560fd), following [27560c9](https://github.com/11xx/tapes/commit/27560c96376875ed8c34b4de3cc9f27bae0ee7a0). Both introduced native T3-managed Antigravity ACP recovery. No installed ACP database or descriptors were available during this investigation. No real usage figures or transcript contents are included.

## Confirmed in tapes

- [backend/antigravity.rs](../../../crates/tapes-core/src/backend/antigravity.rs) initializes cost, tokens, accounting, usage_detail and list_price_estimate to None. Its full-stream fold updates timestamps/models/turns, not usage, and hardcodes unrecognized_usage to zero.
- [The JSONL fixture](../../../crates/tapes/tests/fixtures/antigravity/transcript_full.jsonl) contains two distinct MODEL / PLANNER_RESPONSE / DONE steps with numeric input_tokens, cache_read_tokens and output_tokens. It does not establish separate reasoning, cache-write, cost or response IDs. It also contains an intentionally malformed line. Field names alone do not prove incremental rather than cumulative semantics or category inclusion. A planner response may yield several normalized turns: count the native generation, not the turns.
- [Discovery](../../../crates/tapes-discovery/src/antigravity.rs) requires UUIDv4 database filename and a matching unique trajectory_meta.cascade_id, rejects duplicate identities and non-native directories. It does not yet inspect usage.
- [Detached children](../../../crates/tapes-core/src/antigravity_children.rs) corroborate child descriptor conversationId/spawnStepIndex, the child's launch inbox tool call, the parent DB's steps.idx/step_type=132/step_format=0 and Protobuf step_payload path 5→4 (tool call). This establishes lineage, not whether the parent contains any child usage.
- [T3](../../../crates/tapes-core/src/t3.rs) projects native sessions and provider-switch boundaries. statev2.sqlite is orchestration metadata, not an independent inference ledger; a projected turn or failed run is not itself a metered request.

## Source inventory and authority

T3CODE_HOME (default ~/.t3):
- userdata/statev2.sqlite
- userdata/providers/antigravity/<64-hex-profile>/antigravity-acp/conversations/<cascade UUID>.db
- same profile /brain/<cascade UUID>/.system_generated/logs/transcript_full.jsonl (canonical transcript)
- same logs/transcript.jsonl (alternative representation; existing tests exclude it)
- same .system_generated/subagents and messages (lineage only)

Native fixture schemas: trajectory_meta(trajectory_id, cascade_id, trajectory_type, source); steps(idx, step_type, step_format, step_payload BLOB). Actual installations can contain richer schema. Do not generalize to standalone IDE stores without a separate discovery/identity contract.

## External evidence: hypotheses to validate on installed ACP

- [ccusage parser, pinned revision d41bf3d](https://github.com/ccusage/ccusage/blob/d41bf3d48a911e9742793087142e323093ae6a4f/rust/adapters/antigravity/src/parser.rs) parses both generation and step metadata, retries and identity witnesses. It calculates cost separately. [Issue #1488](https://github.com/ccusage/ccusage/issues/1488) documents offline recovery and cache-read separation; it also notes gaps in real retry/child testing.
- [pew descriptor investigation](https://github.com/nocoo/pew/blob/main/docs/51-antigravity-support.md) documents field names recovered from an installed CLI, including explicit generation step references and proto3 implicit scalar presence. Its findings disagree with some alternative decoders. Validate against the managed ACP build, not just the standalone CLI.
- [txcript 0.14.4 format notes](https://docs.rs/crate/txcript/0.14.4/source/docs/formats/antigravity.md) describe the trajectory database and recovered descriptor structure.

Candidate wire paths (numbers denote fields, not array indices):
| Location | Candidate meaning |
|---|---|
| gen_metadata.data / 1 / 4 | ModelUsageStats on ChatModelMetadata |
| gen_metadata.data / 1 / 17 / 2 | Usage on repeated RetryInfo |
| gen_metadata.data / 1 / 19 (also 21 in ccusage) | response model string candidate |
| gen_metadata.data / 2 | Repeated step_indices, packed/unpacked; explicit join evidence |
| steps.metadata / 9 | ModelUsageStats |
| steps.metadata / 28 / 2 | Retry usage |
| steps.step_payload / 5 / 9 | Embedded step metadata usage candidate |

Candidate ModelUsageStats: 1 model enum, 2 uncached/non-cache-read input, 3 total output, 4 cache write, 5 cache read, 6 provider enum, 7 message identity, 9 thinking, 10 visible output, 11 response identity, 12 provider-assigned message identity. These mappings are external evidence, not established schema for this ACP build. Do not interpret field 1 as token count, infer model names from placeholder enums, or use equality of gen_metadata.idx and steps.idx as a join.

Provisional normalization: input excludes cache reads; output includes thinking. Verify cache-write inclusion separately before selecting InputConvention::ExcludesCache; pew explicitly cautions about write semantics. A completed decoded usage message with proven proto3 implicit scalar defaults may establish zeros for omitted scalar fields; a missing message, missing JSON key, corrupt message or unverified descriptor does not. Output partition consistency must hold (total output = visible + thinking when all are established). If source-dependent conventions differ, do not add their native counters under one convention. Preserve raw counters and withhold unsupported disjoint totals.

## Accounting contracts and integration

[model.rs](../../../crates/tapes-core/src/model.rs): Tokens is a harness-native partition with Option counters; TokenConvention::of returns None for Antigravity; DisjointTokens total exists only when all summed categories are established. [backend/mod.rs](../../../crates/tapes-core/src/backend/mod.rs): TokenTotals::add/finish_covered preserves missing vs zero and per-event coverage; accounting_for attaches basis and scope. [usage.rs](../../../crates/tapes-core/src/usage.rs) contains UsageDetail, model attribution, MeasurementCoverage, cost coverage, unrecognized usage and failures. [measurement.rs](../../../crates/tapes-core/src/measurement.rs) provides completeness reasons. [pricing.rs](../../../crates/tapes-core/src/pricing.rs) prices caller-supplied list-price tables. [family.rs](../../../crates/tapes-core/src/family.rs) and [overlap.rs](../../../crates/tapes-core/src/overlap.rs) govern cross-recording aggregation.

### Stage 1 — JSONL native counters

Gate implementation on local evidence that planner usage is per distinct inference event; if instead cumulative, implement the correct recorded-total/reset semantics, never sum snapshots. If per-request semantics are established:
- Use a shared native usage accumulator for bounded and full streaming paths, before expanding a step to turns. Reuse TokenTotals and accounting_for; declare SummedRequests in Backend::accounting_bases. Keep model-level attribution by recorded served model; do not price all requests at the last selected model.
- Use the retained tail only for bounded usage (ReadWindow if truncated); head and tail windows may overlap. Full read starts a fresh fold, not a sum with inherited bounded totals. Use the pinned stream evidence, preserving read gaps/skipped rows and time bounds.
- Recognize invalid/missing fields, unknown usage-bearing records, stopped attempts and duplicate/conflicting native identities. Keep unrecognized_usage in both UsageDetail and StreamedTranscript consistent. Apply UnreadableRecord, MissingCounter, StoppedAttempt, Undetermined and other existing completeness reasons as appropriate. Do not silently zero missing fields.
- Do not claim a global Antigravity TokenConvention until partition semantics are confirmed. Without it, keep native counters but no disjoint total or unsupported pricing.

### Stage 2 — richer SQLite ledger

Read bounded schema/version-specific metadata; decode only known fields (bounded varints, blobs, records, retries). Require matching cascade identity and safe native paths. Pin source revisions coherently with the JSONL replay. Use safe read-only snapshot semantics; immutable on a live WAL store can miss committed data, while ordinary read-only SQLite can update original SHM. Use a coherent offline snapshot or a carefully validated private copy including WAL when necessary; never alter original sidecars.

The unit is a distinct inference event; JSONL, steps.metadata, steps.step_payload and gen_metadata are alternative witnesses, not additive sources. Match verified generation/step references and provider response identity in an established namespace. Within recording, stable native event identity may provide fallback when validated; different records with equal amounts are not inherently duplicates. A generation can reference multiple steps, so do not count one generation multiple times. Preserve each separately established retry, including failed requests with known usage. Do not add retry counters unless evidence shows main usage excludes them. Unknown or conflicting witnesses must qualify or withhold measurements, rather than synthesize a per-field maximum. Repeated IDs without verified scope are overlap evidence, not proof of global identity.

Detached children: independent launched child records should own their inference events, not their parent's invoke_subagent tool or completion message. Do not assume lineage proves disjointness or that context copying is consumption copying. Inspect parent_references/forks and native response identities; expose overlap/copy evidence where established and use existing family withholding/union contracts. Avoid unsupported exact family totals. Treat usage observations, event keys, spans, timeline and window hooks as separate feature surfaces; do not claim support until their pinned-source and coverage contracts are implemented.

### Cost

Leave Session.cost absent until a native monetary field and its semantics are established. Subscription quotas, plan cost, equivalent API list price and actual billed dollars are distinct. Use caller-supplied pricing via existing list_price_estimate, by actual served model, request timestamp and confirmed token convention. Count output thinking only once; honor cache-write TTL and context/price validity. Unknown rates and counters imply incomplete estimates. No hardcoded model aliases or inferred quota costs.

## Diagnostic and review steps

The adjacent script is a bounded, standard-library-only exploration tool, not a production decoder. Its report is an inventory of shapes and equality counts, not a usage total. By default it inspects canonical JSONL only; SQLite schema and candidate metadata require an explicit quiescent flag. Stop T3 and ACP cleanly (or use a consistent offline snapshot), do not delete WAL/SHM/journal files, and run from the repository root:

```sh
python3 docs/research/antigravity-usage/antigravity-schema-diagnostic.py
python3 docs/research/antigravity-usage/antigravity-schema-diagnostic.py \
  --sqlite-quiescent --max-sessions 20 --db-rows 1000 --max-blob-kib 1024 \
  > /tmp/antigravity-schema-report.json
```

Optional --root overrides T3CODE_HOME; --session selects a native UUID locally; neither is emitted. The script avoids text, arguments, raw payloads, individual counts of tokens, model/response/session identifiers and paths in the report; schema identifiers are retained, so inspect output before sharing. SQLite opens mode=ro&immutable=1 only after explicit assertion and when no sidecars exist, discards observations on detected source changes, and skips sidecar-bearing stores. Stat checks are not a guarantee against concurrent writers; don't assert quiescence on a live store. Bounded samples may miss rare usage; missing fields in a sample are not proof they never occur. Candidate explicit step references and identity equality are hypotheses, not proof of one event or billing. Generation index and step index are distinct namespaces.

Local lead checklist:
1. Inspect the installed descriptors (including field types, proto syntax/presence, generation/step/retry mapping), and representative JSONL/DB samples, including cache-heavy, switched-model, retry, detached child and fork cases. Share only aggregate structure/relationships, never transcript contents.
2. Establish per-event vs cumulative semantics and input/cache/creation/total-output partition; reconcile exact generation/step identity and whether main usage includes retries. Check generation-only or internal requests not in JSONL.
3. Decide whether JSONL is sufficient for a first conservative patch or whether the DB must be primary from the start. Preserve bounded/pinned read contracts and explicit unknowns.
4. Add fixtures for one planner response expanding to multiple turns; repeated/changed IDs; missing versus explicit zero; model switches; bounded/full overlap; malformed and unrecognized records; mirrored sources; independently metered retries; child/fork overlap; absent pricing; inconsistent DB/log snapshots.
5. Run existing Antigravity CLI/discovery, accounting_bases, counter_coverage and measurement_completeness tests plus the new regressions. No fabricated real measurements.

## Validation and limits

The current diagnostic's explicit-generation-reference refinement was exercised with synthetic packed/unpacked references, invalid wire input and a database whose generation index deliberately differs from the referenced step index; redaction sentinels and unchanged source file hashes passed. The missing-store path was also smoke-tested. No installed ACP store was available, and no Rust changes are proposed or tested by this PR. Treat external wire mappings, model identities, counter completeness, actual charges and family-wide disjointness as unresolved until local review. Do not merge this exploratory artifact as production functionality without that review.
