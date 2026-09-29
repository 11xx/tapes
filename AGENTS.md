# tapes

Read, list, and export coding-agent sessions across harnesses. One
normalized session model; a backend per harness. Non-goals: no daemon, no
server, no session *writing* — `tapes` never mutates a harness's store.

## Invariants

- **Absence is a fact, not an error.** A harness whose binary or store is
  missing reports itself unavailable and exits 0; `list` must work with any
  subset of harnesses installed. Asking for a specific session is the one
  place absence is an error, because the caller named something that is not
  there. A field a harness cannot supply is omitted, never invented or
  defaulted.
- **Reads are bounded.** Transcripts reach hundreds of megabytes. Every
  reader caps what it pulls into memory and every command caps what it
  returns. A tool that hangs on a large session is worse than no tool.
- **The normalized model is the contract.** Backends translate into it;
  nothing downstream should need to know which harness a session came from.
  Harness-specific truth that does not fit the model — pi's abandoned
  branches, opencode's event sourcing — is reported explicitly rather than
  dropped.
- **Never mutate a harness store.** Opening a session store is read-only,
  always. OpenCode database queries are SELECTs.
- The three-file bundle contract — `.context.md`, `.json`, `.trace.md` — is
  settled. Extend it; do not redesign it.

## Working here

- The CLI is the only workflow surface a caller needs: bare `tapes` teaches
  the retrieval order and the judgment around it, and `--help` carries each
  command's contract. Keep both accurate when behavior changes; there is no
  external skill to fall back on. After integrating a CLI change, refresh the
  installed binary: `cargo install --path crates/tapes --locked`.
- The repo dogfoods arc: run non-trivial changes through
  `begin` → implement → `snapshot` → `review` → `verify --all` → `integrate`
  with `ARC_HARNESS`/`ARC_SESSION` set. Gates live in `.arc/gates.toml`:
  build, test, and lint (clippy `-D warnings` + `fmt --check`) must pass.
- Record behavior changes on their arc change via `arc changelog`. Never
  hand-edit `CHANGELOG.md`; the `[Unreleased]` block is generated.
- `export GIT_EDITOR=true GIT_SEQUENCE_EDITOR=true` before any git command.
  An interactive editor blocks an unattended run.
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

## Releasing

- **A version is the calendar date of its publication**, written `YYYY.M.D`
  as the three numeric fields Cargo's semver parser accepts and nothing more:
  no leading zero on the month or the day, and no fourth field, prerelease, or
  build metadata. One release is cut per date; a second waits for the next date
  rather than qualifying a version. The manifest, `tapes --version`, and the
  changelog's top released heading carry the same string, and the release head
  is tagged `v<version>`. A cut version is published later, unchanged, by the
  operator.
- The packages are `agent-tapes`, `agent-tapes-core`, and
  `agent-tapes-discovery`, distributed by Git; the command is `tapes`.
- Every package is `publish = false`, inherited from the workspace, so
  publishing takes a deliberate manifest edit. Workspace path dependencies
  keep a version requirement, which publishing needs.
- `cargo publish` is the operator's act and is never run from a session; a
  session may run `cargo publish --dry-run`.

## Harness formats

`docs/formats/` documents what each harness's store actually contains. The
formats are undocumented by their harnesses and drift, so treat those files as
a record of what was last verified, not as a spec — check a real transcript
before relying on any claim in them, and update them in the change that
learns something new.
