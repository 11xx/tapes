# tapes

Read any coding agent's recorded sessions, and hand them to the next one.

When a coding agent's session dies mid-task, the work it did survives only in
the harness's own recording, in a format no other harness reads. `tapes` reads
the recordings of Claude Code, Codex, OpenCode and Pi through one model. It
tells you where a session stopped, its recorded directory and that directory's
current checkout commit, which tool calls never got a result, and what it cost.
It exports the session as a bundle a fresh session in any harness can read.

The same reader answers questions across sessions: which ones mention
something, what each ended on, where the tokens went.

No command writes to a harness's session store, so `tapes` is safe to point at
a session that is still running.

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
tapes list --launch interactive           # sessions a person drove, not headless runs
tapes endings --here --since 2026-09-01   # what each session ended on
tapes usage --here --by model             # where the tokens and the cost went
tapes stats <id> --idle 45m               # active time, stretches, tools, cache share
tapes events --here --tool bash --command 'cargo test' --count   # how often each session ran a command
```

Without `--here`, a listing covers every session on the machine. Every command
that reads sessions, except `export`, takes `--json`, and each JSON answer
names its versioned schema.

Claude Code and Codex record how each session was launched. Pi and OpenCode
do once `tapes install pi` or `tapes install opencode` has added one file to
the harness's configuration.

## Follow a recording as it grows

```sh
tapes read <id> --kind user,assistant --json                     # the newest page of the conversation
tapes read <id> --before '<start>' --kind user,assistant --json  # the page before it
tapes read <id> --after '<end>' --kind user,assistant --json     # what was written since
```

A page is a list of typed items (messages, reasoning, tool calls and results)
between two positions, `start` and `end`. Pass `start` to `--before` to go
back and `end` to `--after` to read what was appended. Positions are opaque
strings that stay valid across processes. A read scans a bounded part of a
recording file, so the newest page of a very long session comes back as fast
as a short one's.

Rust programs get the same pages from `agent_tapes_core::recording`.

## Read from somewhere else

```sh
tapes list --input conversations.json     # a ChatGPT or Perplexity export
tapes list --remote build-box             # another machine's sessions, asked over ssh
tapes list --native-store claude --native-root archive/claude   # a copied Claude Code store
```

Each of these names the only source read: none falls back to the sessions
installed on this machine. `--native-store` also takes `codex` and `pi`.

## Where the rest is

`tapes` with no arguments prints the workflow guide, and `tapes <command>
--help` is each command's full contract. A tool that only needs to find
sessions can depend on the `agent-tapes-discovery` library instead of the CLI;
see [its README](crates/tapes-discovery/README.md).
