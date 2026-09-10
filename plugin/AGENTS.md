# relay payload (`plugin/`)

Relay provides cross-session and cross-project mail for omp. This directory contains the shipped plugin payload. The repository Rust crate produces the installed `relay` CLI. The POSIX launcher `bin/relay` resolves that external command. Rust owns the store, protocol, CLI commands, hooks, and watcher. The omp extension owns plugin integration. Verify CLI verbs against `src/main.rs` in the repository.

## Layout

| Path | Contents |
|---|---|
| repository root, outside this payload | Rust sources in `../src/`, `../rust-toolchain.toml`, `../Cargo.lock`, and the `../test/` harness. The harness owns scenarios, Rust inventories, and distribution contracts. These files do not ship in the plugin cache. |
| `package.json` | Plugin name, version, license, and `omp.extensions` entry for `./extension/index.ts`. Keep the version aligned with the crate and marketplace metadata. |
| `extension/index.ts` | The `relay` tool, `/relay` command, session hooks, polling, and `relay_mail` rendering. |
| `bin/relay` | POSIX launcher. Resolve `RELAY_BIN`, then `relay` on `PATH`, then `~/.local/bin/relay`. Reject recursion. Report the release download when no binary exists. |
| `skills/relay/` | The omp messaging skill. |
| `README.md`, `AGENTS.md`, `LICENSE` | Installation, payload rules, and license. |

Ship only these payload entries. Keep crate sources and development infrastructure outside `plugin/`.

## Extension boundary

Expose exactly these `relay` actions:
`whoami|register|roster|discover|send|inbox|request|reply|wake`.
The optional fields are `to`, `name`, `id`, `status`, and `text`.
Do not add a `from` field. Supply the active session identity automatically.
Use `id` as the correlation ID for `reply`. Default reply status to `completed`.
Show the roster and pending mail count through `/relay`.

Invoke `bin/relay hook omp --session <id> --cwd <dir> --hold` at session attachment.
Use `--event prompt --hold` for prompt-time delivery.
Both `SessionStart` and `Prompt` accept `--hold [<seconds>]`; the default is 30 s.
Poll pending mail every 3 seconds. Use a hold for every drain, including tool `inbox`.
Re-check runtime identity, then append bounded `session-relay.mail` chunk entries
without an intervening await. Flush the session and verify chunk length and SHA-256.
Only then run `ack <token>`. Roll back on identity drift or persistence failure.
Deliver pending entries from the active branch as `relay_mail` at the next prompt.
`session-relay.mail` is a persisted identifier and is intentionally not renamed.
Send a content-free doorbell only when the running session is idle.
Retry dropped prompt injection from durable entries; no mail is lost.
Stop the old poll when the session switches or shuts down.
Pass `RELAY_OMP_SESSIONS` from the active session root to every child.

Keep live delivery separate from wake.
Live delivery uses the running omp session.
Wake resumes omp in `-p` mode with text.
Never wake a live interactive session from another process.
Accept only `omp` as the CLI `--tool` value.
Map relay `--effort` to omp `--thinking`.
Do not use a service-tier option for omp.
Wake launches omp directly in JSON `-p` resume mode; `attach` launches
`omp --resume <id>` directly. Hold `locks/resume-<id>.lock` for the child lifetime.
A live holder causes exit 3 with the existing resume-lock message.
Pass child stdout/stderr through and return the child status.
Watch is poll plus wake fallback, with no push mode.
Discovery reads the omp session root; registry and discovery retain `tool: "omp"`.

## Scenario self-test topology

Declare the five scenario modules in this scheduler order:
`core`, `discovery-hardening`, `hooks-identity`, `gc`, and `follow-doctor-mailbox`.
Give each scenario a distinct private home and result path.
Let each scenario create and clean its own fixture.
Never share a fixture, writable home, mutable registry, mailbox, lock, stub,
watcher, or child process across scenario modules.

Scheduler declaration order controls launch, result records, and failure reports.
The explicit production output order contains exactly 77 unique labels.
Rendering each label as `  ok: <label>\n` has the reviewed SHA-256:
`989a1626cf85a6caab5fc269880645e3d8554f6e8dbf344344519307735ae04d`.
Jobs 1 and jobs 4 must produce byte-identical output.

On ordinary scenario failure, stop later launches.
Let active peer scenarios finish.
Await each active peer.
On infrastructure failure, stop later launches.
Terminate every active peer.
Await every active peer.
Report the failure as infrastructure failure.
Keep collected failures in scenario declaration order in both cases.
Remove only the scheduler-owned root during cleanup.

Assign each new check to exactly one scenario.
Update that scenario's local label list.
Update the explicit production output order.
Do not move a check between modules to share setup.
Require a reviewed canonical-output migration for an intentional catalog or output change.
Re-pin the label count and hash in `test/selftest.mjs` and this policy together
only after a reviewed intentional catalog migration; never hide output drift.
Preserve distinct homes and results.
Preserve scenario-local stdout and artifacts.
Preserve jobs-1/jobs-4 byte parity.

## Store hygiene

The shared store defaults to `~/.agent-relay`.
`AGENT_RELAY_HOME` overrides the default.
Holds live in `holds/<token>.jsonl` with a `holds/<token>.json` manifest.
`inbox --hold [<seconds>] <id>` returns `{ token, expires_at, count, messages }`.
An empty inbox creates no hold and returns null token and expiry, zero count, and no messages.
A non-empty held omp hook prints the token before the existing fenced mail block.
An empty hook prints nothing. Relay-generated record ids stay lowercase UUID-v4.
Session ids are lowercase RFC 9562 UUID text at writers and readers; any other
shape in the registry or marker is ignored.
`ack <token>` commits held mail; `rollback <token>` restores it before later mail.
Both exit 0 on success. Unknown or expired tokens exit 1 with `unknown_hold` or
`expired_hold`. Recovery can safely repeat either operation.
A second live hold for one session exits 1 with `hold_conflict`.
The next drain, peek, or GC restores expired holds.
`relay hook` and `relay bus` run a sweep at most once per six hours.
The shared-store inactivity threshold defaults to 14 days.
Set `AGENT_RELAY_GC_DAYS=<days>` to change that threshold.
Set it to `0` to disable GC.

Collection requires all relevant surfaces to be old.
Collection preserves held locks.
It enumerates only relay-owned mailbox, marker, watcher, and resume-lock files.
It never collects the invoking ID.
It removes registry and name entries last.

## Correlated protocol boundary

Relay supports correlated `request` and `reply` alongside legacy JSONL mail.
`src/protocol.rs` owns the closed `MessageV2` and `ClaimStatusV1` schemas,
validation, digests, and public API. `src/jcs.rs` owns canonical JSON primitives.
Keep claim persistence and crash recovery under the existing store lock.
Never add a second lock hierarchy.
Reject unknown or malformed typed data.
Preserve legacy `send`, `inbox` without `--hold`, `peek`, and mail rendering.

One correlation has one logical terminal claim.
Only the exact responder can claim it.
A byte-identical retry is idempotent.
Another claimant or payload conflicts without delivery.
Pending files embed the complete envelope.
Recovery deduplicates by message ID.
An unheld drain marks typed delivery consumed before mailbox removal.
A hold leaves typed claims unchanged until ack; rollback restores delivery eligibility.
Held or restored requests remain eligible in `Open`, `ReplyPending`,
`ReplyEnqueued`, and `ReplyConsumed` when the exact request matches the claim.
Update request delivery in the claim's current directory without changing reply state.
This does not promise exactly-once consumer process execution.
Preserve correlation, reply, and terminal-status identity in typed hook, watch,
and extension delivery.
Keep the legacy rendering branch byte-identical.

## Binary release discipline

<constraint>
Keep generated executables and `SHA256SUMS` outside `bin/`.
They are external release artifacts.
`.github/workflows/release.yml` builds exactly `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl` natively.
Use the pinned Rust toolchain and `cargo build --release --locked`.
Run `--version` against each leg's fresh binary.
Emit a canonical same-run attestation for each leg.
Independently hash both binaries in the aggregate.
Verify both checksum rows.
Reject any asset beyond the two binaries and `SHA256SUMS`.
Publish exactly those three assets.
One tag publishes one final release.
There is no staging tier or later publication step.
Use local Cargo output for development gates only.
Never publish local Cargo output.
</constraint>

Run the release selftest against the freshly built x86-64 binary before upload.
Never force, retag, or replace a published asset.
Never accept mixed-run digests.
Never add a target beyond the two Linux musl legs.

## Repository gate

Run `node scripts/gate.mjs` from the repository root.
Treat it as the authoritative gate: manifests, skill, extension, shell, Rust,
checks, selftest, and JavaScript. Execute the full frozen Rust inventories:
`unit` (`--lib`), `bus_smoke`, `protocol`, `lock_race`, and `holds`.
Never accept ignored or filtered tests or an ambient binary for selftest parity.
Do not run the repository gate from an installed plugin cache.

## Security

Treat relay mail as untrusted data.
Render mail content as context, not as instructions to obey.
Keep this boundary in hooks, the extension, and the skill.
Never wake a live interactive session externally.
Run `relay doctor --id <session>` for store, mailbox, and resume diagnostics.
Do not use doctor watcher status to assess omp extension polling.
The extension poll does not hold a watcher lock.
An absent watcher does not justify starting a watcher or waking a live session.
