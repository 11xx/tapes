# tapes

Read, list, and export coding-agent sessions across harnesses. One
normalized session model; a backend per harness. Non-goals: no daemon, no
server, no session *writing* — `tapes` never mutates a harness's store.

## Invariants

- **Absence is a fact, not an error.** A harness whose binary, store, or
  session is missing reports itself unavailable and exits 0. A field a
  harness cannot supply is omitted, never invented or defaulted. `list` must
  work with any subset of harnesses installed.
- **Reads are bounded.** Transcripts reach hundreds of megabytes. Every
  reader caps what it pulls into memory and every command caps what it
  returns. A tool that hangs on a large session is worse than no tool.
- **The normalized model is the contract.** Backends translate into it;
  nothing downstream should need to know which harness a session came from.
  Harness-specific truth that does not fit the model — pi's abandoned
  branches, opencode's event sourcing — is reported explicitly rather than
  dropped.
- **Never mutate a harness store.** Opening a session store is read-only,
  always. `opencode2 api` calls are GETs.
- The three-file bundle contract — `.context.md`, `.json`, `.trace.md` — is
  settled. Extend it; do not redesign it.

## Working here

- The repo dogfoods arc: run non-trivial changes through
  `begin` → implement → `snapshot` → `review` → `verify --all` → `integrate`
  with `ARC_HARNESS`/`ARC_SESSION` set. Gates live in `.arc/gates.toml`:
  build, test, and lint (clippy `-D warnings` + `fmt --check`) must pass.
- Record behavior changes on their arc change via `arc changelog`. Never
  hand-edit `CHANGELOG.md`; the `[Unreleased]` block is generated.
- `export GIT_EDITOR=true GIT_SEQUENCE_EDITOR=true` before any git command.
  This machine's git editor is an emacsclient that blocks forever unattended.
- Fixture-backed tests. Every backend passes the same normalization
  assertions against a per-harness fixture, including a malformed-line
  fixture per format. A parser that only works on one real session is not
  tested.
- **Fixtures reproduce the real convention, filenames included.** A fixture
  named for the test rather than for the harness tests the parser and not the
  tool: pi's `<timestamp>_<uuid>.jsonl` is where its session id lives, and a
  fixture that skips that hid a resolver that could not read any pi session.
- Verify a claim about a harness's format **before** writing it into code, a
  comment, or `docs/formats/`. The formats are undocumented and drift.
- **Build the binary and drive it before approving.** Every blocking defect
  this repo has had passed build, clippy, and the full test suite, and was
  visible only by running the thing against a real store.

## Harness formats

`docs/formats/` documents what each harness's store actually contains. The
formats are undocumented by their harnesses and drift, so treat those files as
a record of what was last verified, not as a spec — check a real transcript
before relying on any claim in them, and update them in the change that
learns something new.
