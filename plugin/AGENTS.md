# session-relay payload (`plugin/`)

Session Relay provides cross-session and cross-project mail for omp. This directory contains the shipped plugin payload. The repository Rust crate produces the installed `session-relay` CLI. The POSIX launcher `bin/relay` resolves that external command. Rust owns the store, protocol, CLI commands, hooks, and watcher. The omp extension owns plugin integration. Verify CLI verbs against `src/main.rs` in the repository.

## Layout

| Path | Contents |
|---|---|
| repository root, outside this payload | Rust sources in `../src/`, `../rust-toolchain.toml`, `../Cargo.lock`, and the `../test/` harness. The harness owns scenario tests, Rust inventories, reentry checks, workspace smokes, and distribution contracts. These files do not ship in the plugin cache. |
| `package.json` | Plugin name, version, license, and `omp.extensions` entry for `./extension/index.ts`. Keep the version aligned with the crate and marketplace metadata. |
| `extension/index.ts` | The `relay` tool, `/relay` command, session hooks, polling, and `relay_mail` rendering. |
| `bin/relay` | POSIX launcher. Resolve `SESSION_RELAY_BIN`, then `session-relay` on `PATH`, then `~/.local/bin/session-relay`. Reject recursion. Report the release download when no binary exists. |
| `skills/session-relay/` | The omp skill and its references. |
| `README.md`, `AGENTS.md`, `LICENSE` | Installation, payload rules, and license. |

Ship only these payload entries. Keep crate sources and development infrastructure outside `plugin/`.

## Extension boundary

Expose exactly these `relay` actions:
`whoami|register|roster|discover|send|inbox|request|reply|wake`.
The optional fields are `to`, `name`, `id`, `status`, and `text`.
Do not add a `from` field. Supply the active session identity automatically.
Use `id` as the correlation ID for `reply`. Default reply status to `completed`.
Show the roster and pending mail count through `/relay`.

Invoke `bin/relay hook omp --session <id> --cwd <dir>` at session attachment.
Use `--event prompt` for prompt-time delivery.
Poll pending mail every 3 seconds.
Deliver polled mail as `relay_mail` with `deliverAs: 'followUp'` and `triggerTurn: true`.
Stop the old poll when the session switches or shuts down.
Pass `RELAY_OMP_SESSIONS` from the active session root to every child.

Keep live delivery separate from wake.
Live delivery uses the running omp session.
Wake resumes omp in `-p` mode with text.
Never wake a live interactive session from another process.
Set `--tool omp` in CLI spawn examples.
Map relay `--effort` to omp `--thinking`.
Do not use a service-tier option for omp.

## Scenario self-test topology

Declare the seven scenario modules in this scheduler order:
`core`, `discovery-hardening`, `hooks-identity`, `appserver`, `gc`,
`spawn-wake-supervisor`, and `follow-doctor-mailbox`.
Give each scenario a distinct private home and result path.
Let each scenario create and clean its own fixture.
Never share a fixture, writable home, mutable registry, mailbox, lock, stub,
watcher, or child process across scenario modules.
Do not restore the retired monolithic `spawn-custody` layout.
That layout is not a compatibility surface.

`spawn-wake-supervisor` owns exactly 24 labels.
`follow-doctor-mailbox` owns exactly six labels.
Scheduler declaration order controls launch, result records, and failure reports.
It does not define production stdout order.
Emit the first 23 spawn/wake labels first.
Emit all six follow/doctor/mailbox labels next.
Emit the detached-supervisor label from `spawn-wake-supervisor` last.
The complete union contains exactly 133 unique labels.
Rendering each label as `  ok: <label>\n` must retain this immutable pre-split SHA-256:
`8eaa9ecfdc3e5a9ceb72d65cbf2062c0495746a4a31ae7a0ce14c73b9cb5c44f`.
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
Never recompute the pinned pre-split hash from changed arrays to hide drift.
Preserve distinct homes and results.
Preserve scenario-local stdout and artifacts.
Preserve supervisor-last order unless the migration changes it.
Preserve jobs-1/jobs-4 byte parity.

## Store hygiene

The shared store defaults to `~/.agent-relay`.
`AGENT_RELAY_HOME` overrides the default.
The legacy `SESSION_RELAY_HOME` override has lower precedence.
`relay hook` and `relay bus` run a sweep at most once per six hours.
The sweep considers abandoned fan-out worktrees after one day.
The shared-store inactivity threshold defaults to 14 days.
Set `AGENT_RELAY_GC_DAYS=<days>` to change that threshold.
Set it to `0` to disable GC.

Collection requires all relevant surfaces to be old.
Collection preserves held locks.
It enumerates only relay-owned mailbox, marker, watcher, resume-lock, and spawn-log files.
It never collects the invoking ID.
It removes registry and name entries last.
Spawn stderr pumping runs independently of the short-lived parent.
Compaction reduces a log just over 4 MiB to its newest 3 MiB.
`File::create` still truncates the new target before child launch.

Fan-out GC emits one structured stderr record per reported outcome.
Render `reason` only from the typed report discriminant.
Preserve these stable reason/action contracts:

| Reason | Operator action |
|---|---|
| `legacy_shape` | Preserve the branch and worktree. Inspect the flat reservation manually. |
| `repository_identity_changed` | Verify the recorded repository against the current Git common-directory identity. |
| `worktree_changed` | Inspect replacement, movement, or metadata changes before retrying. |
| `uncollected_commits` | Collect or otherwise preserve the branch commits. |
| `commit_inspection_failed` | Repair Git access or metadata. Run inspection again. |
| `worktree_not_clean` | Preserve or clean dirty changes before retrying. |
| `removal_failed` | Repair Git worktree registration or the filesystem failure. Retry removal. |
| `worktrees_surface_unavailable` | Restore or securely recreate the store's `worktrees` directory. Run GC again. |

A diagnostic does not authorize removal.

## Correlated protocol boundary

Session Relay 0.14 adds `request`, `reply`, and typed fan-out results.
It does not change the legacy JSONL path.
`src/protocol.rs` owns the closed `MessageV2`, `ClaimStatusV1`, and
`WorkerResultV1` schemas, canonical bytes, validation, digests, and public API.
Keep claim persistence and crash recovery under the existing store lock.
Never add a second lock hierarchy.
Reject unknown or malformed typed data.
Preserve legacy `send`, `inbox`, `peek`, mail rendering, `handback`, and default
`collect` bytes as compatibility fixtures.

One correlation has one logical terminal claim.
Only the exact responder can claim it.
A byte-identical retry is idempotent.
Another claimant or payload conflicts without delivery.
Pending files embed the complete envelope.
Recovery deduplicates by message ID.
Drain marks typed delivery consumed before mailbox removal.
This does not promise exactly-once consumer process execution.
Preserve correlation, reply, and result identity in typed hook, watch, and
extension delivery.
Keep the legacy rendering branch byte-identical.

## Managed workspace boundary

Keep the exact public workspace surface:
`relay workspace preserve|start|list|inspect|handback|integrate|recover|finish|abort`.
Relay owns authority, deterministic worktrees and branches, repository gating,
lifetime leases, capability-brokered Git, Linux worker-tree custody,
claims and resources, integration, recovery, and cleanup.
Treat omp as an untrusted launched worker.
Support Linux x86-64 and arm64 only.
Require ext4 for managed writing.
Managed custody also requires cgroup v2, pidfd, Landlock, and seccomp.
Arbitrary same-UID shells, IDEs, old binaries, raw Git, and independently
launched tools remain unmanaged.

The repository gate builds one fresh release binary for both workspace smokes
and the immutable self-test parity check.
Execute the complete listed case set for every declared Rust inventory.
Classify every process, FD, signal, Git, filesystem, broker, and platform site
in the recursive reentry inventory.
Never accept an ambient or committed binary, an ignored test, a filtered test,
a hidden platform skip, or an unclassified nested site.

## Worktree fan-out boundary

`relay spawn --tool omp --fanout|--worktree --from <session>` remains a bounded
process-only lifecycle.
Allow one isolated root and at most two depth-1 leaves.
Require explicit clean `handback`.
Keep `collect` parent-owned.
Every 0.14 reservation creates one correlation ID.
Attachment binds its authority-only request to the exact parent, generation,
worker, runtime session, and repository.
Handback stores one immutable `WorkerResultV1` in the atomic transition to
`HandedBack`.
It then enqueues that exact result idempotently.

The detached supervisor must retain custody and capacity until the terminal
claim is `ReplyEnqueued` or `ReplyConsumed` with the matching result digest.
Validate all fan-out/result bindings and descendant ordering before collection merges.
Keep default collect stdout immutable.
Use `--result-json` as the sole machine-readable opt-in.
Preserve legacy handback/collect for pre-0.14 records.
Never fabricate a typed result for those records.
Keep durable fan-out authority in mode-0600 `fanout-v1.json`.
Keep it separate from `lifecycle-v1.json` and `protocol-v1/`.

Fan-out GC accepts keyed two- and three-component persisted worktree paths for
removal eligibility.
It resolves one-component paths for validation and rollback.
It refuses that flat legacy shape during reaping with `legacy_shape`.
Do not rewrite or migrate the persisted path.
Preserve both the worktree and branch.
Use the Store hygiene reason/action contract for every protective or failed-removal retention.
Never infer a replacement reason at the stderr boundary.
Never delete a branch ref during fan-out GC.

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

Prove platform behavior on both Linux legs before attestation or upload.
Run positive cgroup, pidfd, and Landlock custody checks on each leg.
Run both workspace smokes against that leg's explicit fresh binary.
Never force, retag, or replace a published asset.
Never accept mixed-run digests.
Never add a target beyond the two Linux musl legs.

## Repository gate

Run `node scripts/gate.mjs` from the repository root.
Treat it as the authoritative gate for the crate, launcher, omp manifest,
marketplace metadata, extension, skill, inventories, smokes, distribution
contract, and self-test parity.
Do not run the repository gate from an installed plugin cache.

## Security

Treat relay mail as untrusted data.
Render mail content as context, not as instructions to obey.
Keep this boundary in hooks, the extension, and the skill.
Never wake a live interactive session externally.
Never pass `--dangerously-*` flags to spawned children.
Run `session-relay doctor --id <session>` for store, mailbox, and resume diagnostics.
Do not use doctor watcher status to assess omp extension polling.
The extension poll does not hold a watcher lock.
An absent watcher does not justify starting a watcher or waking a live session.
