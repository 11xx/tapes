# tapes-discovery

`tapes-discovery` identifies recordings in native harness stores and resolves
exact IDs or prefixes. It reads bounded opening metadata and store indexes; it
does not read transcript bodies, interpret turns, import supplied recordings,
infer liveness, or decide whether a caller can deliver a notification.

File identity comes from a recognized native header when present, otherwise
from the filename convention for a file inside a configured native root.
Malformed or contradictory identity fields are errors. A UUID-shaped string
alone never identifies a session.

Each native session carries its harness, store coordinate, and storage kind.
Stable OpenCode and opencode2 share the `opencode` harness while retaining
distinct store kinds.

Store absence is an empty observation. Read errors, malformed metadata, and
exhausted bounds remain explicit. Prefix selection succeeds only when the
relevant candidate scan proves uniqueness. File traversal visits at most
100,000 entries and depth 64 per store, retains at most 16 MiB of candidate
paths and diagnostics, and reads at most 1 MiB from any file opening. OpenCode
metadata transport is limited to 8 MiB and 30 seconds.

The native registry is read-only and accepts only native store constructors.
Callers own any delivery policy and must treat discovered recordings as
recordings, not as evidence that a process is live or reachable.
