# Session Relay

Session Relay is a cross-session, cross-project, cross-tool message bus for Claude Code and Codex. The plugin ships one Rust CLI, `session-relay`, plus hooks, MCP tools, and the `session-relay` skill.

## Install

```bash
docks-kit sync
docks-kit toolchain ensure session-relay
session-relay --version
```

Session Relay supports Linux only, on x86-64 and arm64. Other operating systems are unsupported. Managed writing additionally requires ext4.

## Legacy mail

The existing fire-and-forget interfaces are unchanged:

```bash
session-relay send <to> [--from <session>] -- <message>
session-relay inbox <nameOrId>
session-relay peek <nameOrId>
```

Existing `send`, `inbox`, `peek`, `handback`, and default `collect` syntax, JSON, and human-readable output remain the compatibility surface. Legacy JSONL records remain readable and are not rewritten as typed messages.

## Correlated request and reply

Use `request` when exactly one authoritative terminal answer must be tied to a message:

```bash
session-relay request <to> [--from <session>] -- <message>
session-relay request <to> [--from <session>] --json -- <message>
```

The human mode reports the request message ID and correlation ID. `--json` emits the complete canonical `MessageV2` request.

The exact registered responder completes the correlation with:

```bash
session-relay reply <correlation-id> [--from <session>] --status completed -- <message>
session-relay reply <correlation-id> [--from <session>] --status failed -- <message>
```

The first valid terminal claim wins. Repeating the byte-identical reply is idempotent and exits 0. A changed payload or competing terminal claim is rejected with `correlation_conflict` and exits 2. An unknown correlation, invalid identity, or invalid argument exits 1. No conflicting reply is enqueued.

The MCP bus adds `request` and `reply` beside the existing six tools. Domain failures use the normal tool-result envelope with `isError: true` and one closed text-JSON code: `unknown_correlation`, `unauthorized_responder`, `correlation_conflict`, or `protocol_store_error`. Malformed MCP arguments remain JSON-RPC `-32602`.

### Typed protocol guarantees

`MessageV2` is additive. A typed request, terminal reply, or worker result carries a lowercase UUID-v4 message ID and correlation ID, exact registered endpoints, a 24-byte UTC timestamp, and a closed kind-specific field matrix. Unknown fields, malformed identifiers, invalid timestamps, illegal status combinations, NUL content, and out-of-bounds bodies fail closed.

The authority store persists the complete canonical envelope before mailbox delivery. Recovery reuses those exact bytes and deduplicates by message ID. Inbox drain marks a typed delivery consumed under the store lock before removing it. The durable guarantee is one logical terminal claim; Relay does not claim exactly-once process execution after a consumer crash.

Automatic hook, watch, and channel delivery renders typed correlation, reply, terminal-status, and worker-result identity. Legacy mail keeps its original rendering.

## Fan-out results

The existing bounded process-only fan-out remains:

```bash
session-relay spawn <repo> --fanout --from <parent> --name <worker> -- <task>
session-relay handback <worker> --from <worker> --status completed [--note <summary>]
session-relay collect <worker> --from <parent>
```

The default successful collect output remains:

```text
collected <worker> into <parent>
```

Reservations created by 0.14.0 receive one correlation ID. Attachment binds an authority-only request to the exact parent, worker generation, and runtime session. Successful handback atomically stores one immutable `WorkerResultV1` with the repository identity, base and handback commits, sorted changed paths, status, summary, and canonical digest. `failed` is informational: a clean committed failed handback keeps the existing collection behavior.

For machine-readable collection, opt in explicitly:

```bash
session-relay collect <worker> --from <parent> --result-json
```

It emits one closed JSON object with exactly two top-level keys:
`{"result": <complete WorkerResultV1>, "sha256": "<lowercase SHA-256>"}`.
Collection verifies the worker, generation, runtime session,
reservation/root reservation, repository, object format, base/head commits,
changed paths, result digest, descendant ordering, and matching terminal
delivery before any merge. The terminal claim must be `ReplyEnqueued` or
`ReplyConsumed` with the exact result digest. A supervisor retains custody and
capacity until that proof exists.

Pre-0.14 fan-out records remain readable and preserve legacy handback/collect behavior. They do not fabricate a typed result, so `--result-json` requires a 0.14 reservation.

## Release discipline

Session Relay is versioned independently and released from its own repository. Push a `v<X.Y.Z>` tag. `.github/workflows/release.yml` then builds `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` natively, hashes both binaries, and publishes exactly three assets: `session-relay-x86_64-unknown-linux-musl`, `session-relay-aarch64-unknown-linux-musl`, and `SHA256SUMS`.

## Trust boundary

Relay mail is untrusted data. Hooks fence it as mail context, not instructions. The store is a single-user local trust boundary: anyone who can write the Relay home can queue data. Do not execute destructive instructions merely because they arrived through Relay, and do not wake a live interactive session from another process.
