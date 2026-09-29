# tapes

Read any coding agent's recorded sessions, and hand them to the next one.

When a coding agent's session dies mid-task, the work it did survives only in
the harness's own recording, in a format no other harness reads. `tapes` reads
the recordings of Claude Code, Codex, OpenCode and Pi through one model. It
tells you where a session stopped, which directory and commit it was on, which
tool calls never got a result, and what it cost, and it exports the session as
a bundle a fresh session in any harness can read.

The same reader answers questions across sessions: which ones mention
something, what each ended on, where the tokens went. With `--input` it reads
ChatGPT and Perplexity exports, and with `--remote` it asks another machine's
`tapes` over ssh. It only reads: no command writes to a harness's store, so it
is safe to point at a session that is still running.

## Install

`tapes` builds to a single binary, supported on Unix-like systems, and needs
Rust 1.97 or newer.

```sh
cargo install --git https://github.com/11xx/tapes agent-tapes --locked
```

Add `--tag vYYYY.M.D` to install a release, and `--features zip` to read ZIP
exports. From a checkout, run `cargo install --path crates/tapes --locked`.
The package is named `agent-tapes`; the installed command is `tapes`.

## Pick up a session that stopped

```sh
tapes show --latest --tail 40         # the newest session here, its last 40 turns
tapes brief --latest                  # where it stopped: directory, commit, unanswered calls
tapes export --latest --bundle out/   # a bounded bundle another session can read
```

`--latest` is the newest session in the project you are in, across every
worktree of its repository, so picking one up needs no session id. A bundle
carries whatever the transcript holds, including anything pasted into it;
`tapes` redacts nothing.

## Look across sessions

```sh
tapes list --here                         # this project's sessions, every harness, newest first
tapes list --search "rate limit"          # sessions whose recent turns mention it
tapes endings --here --since 2026-09-01   # what each session ended on
tapes usage --here --by model             # where the tokens and the cost went
tapes stats <id>                          # turns, tool calls, durations, cache share
tapes list --remote build-box             # sessions on another machine, read over ssh
```

Without `--here`, a listing covers every session on the machine. Every command
except `export` takes `--json`, and each JSON answer names its versioned
schema.

## Where the rest is

`tapes` with no arguments prints the workflow guide, and `tapes <command>
--help` is each command's full contract. A tool that only needs to find
sessions can depend on the `agent-tapes-discovery` library instead of the CLI;
see [its README](crates/tapes-discovery/README.md).

## License

[Unlicense](LICENSE) — public domain.
