# agent-tapes-discovery

`agent-tapes-discovery` is a small library for finding canonical identities in
native Claude, Codex, Pi, and OpenCode stores. It returns an identity, harness,
store coordinate, native locator, and bounded session metadata. It does not
normalize transcript turns, read supplied exports, determine liveness, or
select a delivery adapter.

## Resolve a native session

```rust,no_run
use agent_tapes_discovery::Discovery;

let discovery = Discovery::from_env();
let session = discovery.resolve("session-id-or-prefix")?;
println!("{} {} {}", session.id(), session.harness(), session.store_coordinate());
# Ok::<(), Box<dyn std::error::Error>>(())
```

Use `Discovery::new` with explicit `NativeStore::{claude,codex,pi,pi_file,
pi_with_file,opencode_stable,opencode_v2}` values when the caller owns a
specific root or program. `NativeSession` is created only from those native
sources. A copied UUID or imported recording alone does not establish native
identity.

`Discovery::from_env` adds `PI_SESSION_FILE` alongside the configured Pi
directory. An exact identity in that live recording takes precedence over the
same identity in the directory; candidate pages include the identity once.
A live file named for the queried ID whose header names another ID reports an
identity mismatch, with a correct directory recording taking precedence.
Other Pi identities remain resolvable from `PI_CODING_AGENT_SESSION_DIR`,
then `PI_CODING_AGENT_DIR/sessions`, then the default Pi session directory.

The caller decides whether a resolved harness is eligible for its own action.
A recorded session may be old, closed, or otherwise unsuitable for delivery.

## Identity and bounds

Claude, Codex, and Pi identities come from a valid native opening record when
present. A recognized native filename is the fallback only when the opening
has no identity field. A malformed or contradictory identity is an error; a
UUID-shaped string is not enough to identify a harness. Core receives the
selected `NativeSession` and adds normalized transcript facts without applying
a second filename-to-ID rule.

File identity reads begin with 64 KiB and grow only to 1 MiB. The bytes read
may include message content because harnesses can place identity and message
fields in the same native record. Discovery inspects identity fields from
those bounded bytes; it does not return or retain transcript text. Native
traversal visits at most 100,000 entries at depth 64 and retains at most 16
MiB per store. Prefix resolution inspects at most 1,000 candidates per store.
A full ID matching a native-convention filename can be located beyond that
prefix page. A file named for the queried ID whose header names another ID
produces an identity-mismatch failure. Exact lookup inspects every bounded
filename candidate until it finds a correct recording, retaining mismatch and
read failures until then. A correct exact recording takes precedence;
without one, identity mismatches refuse prefix fallback. Incomplete candidate
coverage remains an error for prefix selection.

OpenCode metadata responses are capped at 8 MiB with a 30-second command
deadline and at most 1,000 candidate rows. Stable and v2 stores share the
`opencode` harness and stable takes precedence when both contain one ID. The
default v2 command is not run unless `opencode-next.db` is present; the stable
command is not run unless `opencode.db` is present. A corrupt or unreadable
present database remains a failure. Explicit program constructors remain
usable without an unrelated default data root. Each store captures its XDG
root when constructed, and `OpenCodeStore::configure_command` applies that
root to child processes that use the selected store.

## Errors

`DiscoveryError` distinguishes I/O failures, invalid native metadata,
exhausted bounds, timed-out metadata commands, and failed commands.
`ResolveError` distinguishes invalid queries, absence, incomplete coverage,
ambiguity, backend failures, and a failed higher-precedence shared store.
`CandidatePage` carries its records, scan counts, completeness, unreadable IDs,
and failures. `IdentitySource::locate_for_resolution` retains whether an exact
failure refuses prefix fallback; native identity mismatches carry that policy.

`NotFound`, `Incomplete`, and `BackendFailed` carry a separate `coverage`
vector of `LookupCoverage` records. Each record names the harness, an optional
store generation, and whether candidate enumeration was complete. Absent
stores are omitted unless inspecting them failed. A terminal exact-lookup
failure has an empty vector because candidate enumeration did not run.
`failures` and `diagnostics` contain only causes. Consumers can select causes
and inspect coverage directly; `Display` adds the store coverage and lookup
advice for people reading an incomplete search diagnostic.

## Dependency

The package carries the workspace's calendar version and has no default features. Its
normal dependencies are `libc` and `serde_json`; it has no dependency on
`agent-tapes-core`, `agent-tapes`, transcript, export, history, or usage code.

A consumer declares the library as a Git dependency pinned to a full
revision:

```toml
agent-tapes-discovery = { git = "https://github.com/11xx/tapes", rev = "<full revision>" }
```

A consumer that also uses `agent-tapes-core` must take both crates from the same Git
source and revision, so its dependency graph holds one copy of each.

From the Tapes checkout, run `scripts/check-discovery-consumer` with Python
3.11+ and Cargo available to build an external consumer, inspect the crate
archive, and check default and no-default-feature dependency graphs.
