# Session Relay Rust crate map

Session Relay is a local durable message bus for omp sessions. A session ID
outlives its process; registration, queued mail, holds, and correlated claims
persist in the shared store. The shipped omp extension connects a running
session to that store through the CLI.

## Module map

| Module | Responsibility |
|---|---|
| `src/store.rs` | Relay home, omp registry and names, mailboxes, markers, store and resume locks, durable holds, ack/rollback, expiry recovery, and inactive-state collection. |
| `src/protocol.rs` | Closed request and terminal-reply envelopes, correlated claims, validation, canonical digests, crash recovery, and typed delivery state. |
| `src/jcs.rs` | Public canonical JSON values, parsing, serialization, secure file reads, lowercase UUID-v4 record-id and SHA-256 digest primitives. |
| `src/bus.rs` | MCP stdio transport and messaging tool dispatch. The omp extension uses its own tool interface instead. |
| `src/discover.rs` | Read-only discovery under the omp session root; `RELAY_OMP_SESSIONS` selects the root. Recency does not prove process liveness. |
| `src/hook.rs` | omp registration and prompt hooks; render held or drained mail as untrusted context. |
| `src/watch.rs` | Mailbox polling and wake fallback, without push delivery. |
| `src/gc.rs` | Opportunistic collection (from hook and bus activity) of inactive relay-owned state. |
| `src/cli.rs` | Argument parsing, registry and mail commands, request/reply, holds, direct omp wake/attach, and doctor diagnostics. |
| `src/sha256.rs` | SHA-256 primitives used for durable content integrity. |
| `src/main.rs` | Binary dispatch for public commands and selftest helpers. |
| `src/lib.rs` | Public library module declarations. |

## Delivery and locking

- `send` appends ordinary mail; `request` persists a correlated request before
  delivery. `reply` permits only the exact responder's single logical terminal
  claim. A byte-identical retry is idempotent; a competing payload conflicts.
- Store and protocol operations share the store lock. Recovery reuses canonical
  envelopes and deduplicates by message ID.
- A plain drain consumes typed delivery before mailbox removal. A held drain
  defers consumption until ack. Rollback and expiry restore eligibility before
  later mail. Request-delivery updates preserve an already-advanced reply state.
- The extension holds mail, re-checks identity, appends bounded
  `session-relay.mail` chunks, flushes and verifies their length and SHA-256,
  then acks. Drift or persistence failure rolls back. Pending active-branch
  entries become `relay_mail` at the next prompt; dropped injections remain pending.
- Wake runs `omp -p --resume <id> --mode json [options] -- <message>` directly;
  `attach` runs `omp --resume <id>`. Both hold the store's
  `locks/resume-<id>.lock` while the child runs. A live holder causes exit 3.
  Child stdout/stderr pass through and Relay returns the child status.
- A resume lock does not exclude independently launched omp processes. Never
  wake a live interactive session externally; use its extension for live mail.
- Doctor's watcher-lock check does not measure the extension's polling timer.

## Test topology

`Cargo.toml` disables automatic test discovery and declares exactly five
integration targets:

| Target | Source | Observable boundary |
|---|---|---|
| `bus_smoke` | `src/tests/bus_smoke.rs` | MCP messaging and error responses. |
| `protocol` | `src/tests/protocol.rs` | Canonical request/reply validation, claims, recovery, and delivery. |
| `lock_race` | `src/tests/lock_race.rs` | Store serialization under concurrent operations. |
| `holds` | `src/tests/holds.rs` | Held and ordinary drains, ack, rollback, and expiry. |
| `watch` | `src/tests/watch.rs` | Registration and watch target validation: no launcher receives a non-UUID session id. |

Inline unit tests run through `--lib`. `test/rust-test-inventory.mjs` compares
live names against `test/fixtures/rust-test-inventory.json` and executes each
complete inventory with no ignored or filtered cases. The Node selftest uses
five isolated scenario modules: `core`, `discovery-hardening`, `hooks-identity`,
`gc`, and `follow-doctor-mailbox`. Jobs 1 and 4 must produce byte-identical output.

## Distribution

Releases publish Linux x86-64 and arm64 musl binaries plus `SHA256SUMS`.
The `plugin/` payload contains the extension, skill, launcher, and payload docs;
it does not contain crate sources, build output, or the development harness.
