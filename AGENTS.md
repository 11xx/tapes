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
- Verify a claim about a harness's format **before** writing it into a brief
  or a comment. The formats are undocumented and change; check the fixture.

## The chain

`docs/chain-plan.md` is the execution program. Per-slice briefs are in
`docs/briefs/`, each one ready to feed to `arc brief <change> --body-file`.
