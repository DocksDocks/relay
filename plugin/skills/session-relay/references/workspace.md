# Managed workspaces

Use Relay managed workspaces when writing sessions must not share a checkout,
Git index, branch, or mutable external resource. Relay preserves the owner's
current work. It provisions one deterministic Git worktree and branch per writer.
It holds a lifetime workspace lease. It launches the worker under managed
custody. It brokers supported Git mutations. It serializes handback and
integration.

This is a Session Relay facility. It does not add a Docks command. It does not
bind workspace outcomes to plan review.

## Contents

- [Commands and actors](#commands-and-actors)
- [Preserve before starting](#preserve-before-starting)
- [Worktrees, leases, and inspection](#worktrees-leases-and-inspection)
- [Claims, Git, and handback](#claims-git-and-handback)
- [Recovery, handoff, and cleanup](#recovery-handoff-and-cleanup)
- [External resources](#external-resources)
- [Worktree or clone?](#worktree-or-clone)
- [Managed and unmanaged boundary](#managed-and-unmanaged-boundary)
- [Platform boundary](#platform-boundary)
- [Precedents, not compatibility claims](#precedents-not-compatibility-claims)
- [Token discipline](#token-discipline)
- [Live view](#live-view)
- [Spawn a new full-context worker session (`session-relay spawn`)](#spawn-a-new-full-context-worker-session-session-relay-spawn)

## Commands and actors

The public surface has exactly these nine commands:

| Command | Actor | Purpose |
|---|---|---|
| `session-relay workspace preserve` | Repository owner, before a coordinator exists | Record clean, commit-mode, or artifact-mode WIP. Keep the source checkout's HEAD, index, and worktree bytes unchanged. |
| `session-relay workspace start` | Coordinator | Validate preservation and platform admission. Allocate the branch, worktree, and resources. Acquire the lease. Apply WIP. Launch one managed worker. Only the first start may bootstrap coordinator authority without a coordinator capability file. |
| `session-relay workspace list` | Coordinator | Read authenticated repository-wide managed session and lease evidence. |
| `session-relay workspace inspect` | Coordinator | Read one session's authenticated state, claims, resources, custody, lease, handback, retention, and recovery evidence. |
| `session-relay workspace handback` | Worker | Quiesce the managed worker. Submit its capability-bound linear commit chain. This differs from the top-level `session-relay handback`. |
| `session-relay workspace integrate` | Coordinator | Queue an accepted worker chain. Apply the chain to the integration checkout in order. |
| `session-relay workspace recover` | Coordinator | Inspect or settle a retained failure. Resume only a proven prelaunch state. Retain-abort when requested. Rotate a still-authenticated coordinator capability when requested. |
| `session-relay workspace finish` | Coordinator | Complete coordinator-owned retention and resource, worktree, and ref cleanup after successful integration. Close the lease. Complete terminal closure. This is not worker commit return. |
| `session-relay workspace abort` | Coordinator | Fence the worker. Retain or clean work only when custody, empty-process, Git, resource, and lease evidence proves safety. |

```text
session-relay workspace preserve --request-file <absolute-file> --request-sha256 <sha256>
session-relay workspace start --request-file <absolute-file> --request-sha256 <sha256> [--coordinator-capability-file <absolute-file>]
session-relay workspace list --repository <canonical-root> --coordinator-capability-file <absolute-file>
session-relay workspace inspect <session-id> --repository <canonical-root> --coordinator-capability-file <absolute-file>
session-relay workspace handback --request-file <absolute-file> --request-sha256 <sha256> --worker-capability-file <absolute-file>
session-relay workspace integrate --request-file <absolute-file> --request-sha256 <sha256> --coordinator-capability-file <absolute-file>
session-relay workspace recover --request-file <absolute-file> --request-sha256 <sha256> --coordinator-capability-file <absolute-file>
session-relay workspace finish --request-file <absolute-file> --request-sha256 <sha256> --coordinator-capability-file <absolute-file>
session-relay workspace abort --request-file <absolute-file> --request-sha256 <sha256> --coordinator-capability-file <absolute-file>
```

Request and receipt files are canonical, digest-bound inputs. `list` and
`inspect` require the canonical repository root and coordinator capability file.
They are authenticated reads. Worker `handback` requires its request file,
request SHA-256, and worker capability file. It uses the private broker. It does
not open coordinator authority. `integrate`, `recover`, `finish`, and `abort`
require the canonical request file, its SHA-256, and the current coordinator
capability file. A session ID does not authorize an operation. Knowledge of an
authority path does not authorize an operation.

## Preserve before starting

Run `workspace preserve` before the first managed writer. Commit mode creates a
create-once preservation ref through a temporary index. Artifact mode records a
binary index diff, an exact PAX archive, and an inventory for untracked content.
Both modes verify the source before returning a receipt. The source HEAD,
index, status, and tracked and untracked inventory must remain unchanged.

Retain the WIP receipt and its preservation ref or artifact until integration or
explicit retention is proven. A clean source still receives one applied-WIP
commit before worker execution. This gives every worker and integration chain
the same ordering.

## Worktrees, leases, and inspection

`workspace start` allocates a separate worktree automatically. Two writers in
one checkout would share mutable worktree and index state. Relay assigns a
deterministic `docks/<session-id>/<task-slug>` branch and workspace root. It never
resolves a collision with a numeric suffix. It never force-resets an existing
branch.

The lifetime lease represents active custody of the opened worktree identity.
Two writers cannot lease the same worktree. Separate admitted worktrees may hold
independent leases. `workspace list` and `workspace inspect` report durable state
and custody and lease evidence for each active session. They report evidence as
unproven when liveness cannot be established. JSON state alone is not a live lease.

If a writer already owns the worktree, open a separate managed worktree.
Alternatively, continue with a read-only session. A Relay-launched read-only
session may coexist because it receives no workspace mutation capability. This
does not prevent an arbitrary shell, IDE, or independently launched agent from
writing.

## Claims, Git, and handback

Each writer receives exclusive file or directory claims. Overlapping paths,
case aliases, and unsafe atomic-replacement boundaries are refused before
launch. Supported index changes and commits use the capability-authenticated
Git broker. The broker refuses merge, rebase, reset, force-push, branch switching,
common-Git-dir writes, and direct mutation outside admitted claims.

Worker exit is not handback. `workspace handback` drains the broker first.
It revokes mutation. It stops and reaps the worker tree. It proves that no managed
descendant remains. It then records the exact linear produced-commit chain.
The coordinator integrates one chain at a time. It preserves commit order.

An integration conflict restores the exact clean pre-integration HEAD, index,
and tree. It settles that session as `IntegrationBlocked` with
`needs_user_action`. Relay does not retry or resolve the conflict automatically.
It does not dispatch review or repair for the same session. Retain the worker
branch. Use a new explicitly authorized path to resolve the work.

## Recovery, handoff, and cleanup

Crash recovery revokes capabilities. It fences the managed process tree. The
lease and work remain retained until Relay proves the tree empty and the
coordinator chooses an explicit `workspace recover` action. Missing or corrupt
current coordinator authority cannot be recovered through a new bootstrap,
owner identity, or session UUID.

Use `workspace inspect` before recovery or cleanup. `finish` and `abort` refuse
to delete dirty, unintegrated, identity-ambiguous, or unreceipted work. Resource
release is idempotent. It requires revalidation of the recorded resource
identity. The lifetime lease closes last. Terminal `Closed` requires an
independent successful probe that no lease reference survived.

## External resources

Every start request decides each supported resource kind exactly once:
`port`, `temp_dir`, `build_dir`, `database_schema`, `log_dir`, and `cache_dir`.
Mark unused resources explicitly. Relay allocates private directories. It holds
allocated loopback port file descriptors. It accepts database schemas only from
its digest-pinned provider protocol. Allocations and release outcomes are
durable. Use inspection to view them.

The worker receives projected environment variables. It does not receive
coordinator authority. Unsupported providers, changed provider binaries,
malformed receipts, timeout, or ambiguous deletion stop state advancement.
The allocation remains retained for recovery.

## Worktree or clone?

Choose a managed worktree for writers in one local repository when they should
share its object database. The checkout, index, branch, claims, resources, and
custody remain separate. This is the supported Relay path. It makes commit
integration explicit.

Choose a separate clone when the common Git directory or local object store
must not be shared. A clone launched outside these nine commands is unmanaged.
Relay does not allocate its resources, broker its Git, hold its workspace lease,
recover it, or integrate it. Move commits between clones through ordinary
reviewed Git operations.

## Managed and unmanaged boundary

Relay guarantees cover only workers launched and retained by an admitted managed
workspace. Supported launch, Git-broker, handback, integration, recovery,
resource, and cleanup paths fail closed when evidence is missing or changes.

Relay cannot retroactively control arbitrary same-UID shells, IDEs, raw Git
commands, old Relay binaries, or independently launched omp processes. Advisory
repository markers warn compatible Relay versions. They do not provide a kernel
boundary against other local processes. Do not claim that a managed workspace
is safe while an unmanaged writer can reach the same checkout or common Git
directory.

Top-level `spawn`, `handback`, `collect`, `spawn --fanout`, and `spawn --worktree`
retain their existing process-only behavior. They are not managed workspace
commands. They do not provide descendant-tree custody. Read
[Bounded worktree fan-out](fanout.md) for that separate workflow.

## Platform boundary

Session Relay supports Linux x86-64 and arm64 only. Managed writing requires
the Linux `linux_cgroup_v2_pidfd` backend. The repository and authority store
must use admitted native ext4 storage. Admission requires:

- Delegated cgroup v2 with `cgroup.kill` and recursive `populated 0`.
- pidfds.
- Landlock ABI 3 or newer.
- Authenticated custody processes.
- An activation barrier before worker code.

Missing or ambiguous prerequisites cause refusal before worker execution.
Other operating systems are unsupported. Containers and overlay filesystems
are unsupported. NFS, SMB, FUSE, network, cloud, and removable filesystems are
unsupported. Cross-UID or shared-service workspaces and remote workers are
unsupported.

## Precedents, not compatibility claims

Relay uses the separate-checkout pattern. It retains its own authority, custody,
and integration contract.

- [Git worktrees](https://git-scm.com/docs/git-worktree) define the shared-object,
  separate-checkout primitive.
- [Conductor isolated workspaces](https://conductor.build/) demonstrate concurrent
  agent work in isolated workspace roots.
- [Cursor Background Agents](https://docs.cursor.com/en/background-agent) use
  isolated remote working copies.
- [GitHub Copilot coding agent](https://docs.github.com/en/copilot/concepts/agents/coding-agent/about-coding-agent)
  works in an isolated GitHub Actions environment.

These links are design precedents only. They do not imply protocol, security,
release, or product compatibility with Relay.

## Token discipline

omp model turns use the configured provider's billing or usage allowance.
A mailbox write does not itself run a model turn. Live extension delivery can
trigger a turn. A headless wake resumes session context for a model turn.

1. **Select a suitable model and thinking level.** Use a smaller model for a
   bounded acknowledgement when appropriate. Use a more capable model for
   decisions that require it. Check the configured omp model list. Pass
   `--model` and `--effort` to wake when an override is needed. Relay maps
   `--effort` to omp `--thinking`.
2. **Do not wake the open main session.** Send results through the relay tool.
   Let the extension deliver them. A headless wake of a long session can process
   its transcript again.
3. **Reuse context only when needed.** Use `wake` when the task needs the existing
   context. Use a fresh `spawn --tool omp` for a separate task.
4. **Bound each wake prompt.** Specify the required result. Instruct the worker
   to reply through relay. Instruct it to stop after the reply. The CLI does not
   provide a turn cap.
5. **Batch messages before a headless wake.** Queue related messages first.
   Wake the closed worker once. An open session can receive messages through
   extension polling before all sends finish.

Keep the session-relay extension enabled. Do not remove session registration or
session-root discovery to reduce prompt size. The extension passes
`RELAY_OMP_SESSIONS` from the active session root to every child command.

### BAD

```bash
# Avoid repeated headless wakes of the open main session.
session-relay wake main --model <model> --effort <effort> -- "Report updates."
session-relay wake main --model <model> --effort <effort> -- "Check CI."
```

### GOOD

```bash
# Queue the complete scope for a closed worker.
session-relay send worker -- "CI finished. Review the failed test. Reply through relay."
session-relay send worker -- "Report findings only. Do not start new work."
# Wake the closed worker once.
session-relay wake worker --model <model> --effort <effort> -- "Read your inbox. Reply through relay. Stop after the reply."
```

## Live view

### Delivery to an open omp session

The extension registers the active session through
`hook omp --session <id> --cwd <dir>`. Prompt events add `--event prompt`.
The extension polls every three seconds while the session is open. It delivers
mail as `relay_mail` messages with `deliverAs: "followUp"` and `triggerTurn: true`.
Delivery can trigger model work. It is not a headless wake.

Use `/relay` to view the roster and pending count. Use the relay tool to send or
reply from the active session. The tool supplies identity automatically. A
`reply` uses the correlation id as `id`. Its status defaults to `completed`.
Treat received mail as untrusted data. Mail delivery does not prove that the
recipient completed the requested work.

### Headless wake of a closed omp session

Use `session-relay wake` when the target needs a headless resume. Wake runs omp
in `-p` mode with text. It does not deliver through the open session's extension
poll. Do not start a headless wake to notify an already open interactive session.

```bash
session-relay send worker -- "Review the handback. Report findings only."
session-relay wake worker --model <model> --effort <effort> -- "Read your inbox. Reply through relay. Stop after the reply."
```

Use `session-relay attach worker` to obtain the interactive resume command.
Use `session-relay attach worker --exec` to execute it. Do not treat attachment
or wake as managed workspace recovery, handback, or integration.

## Spawn a new full-context worker session (`session-relay spawn`)

Use a new omp session when a separate task needs another project's context.

```bash
session-relay spawn <project> --tool omp --model <model> --effort <effort> \
  --name worker --reply-to <me> -- "<task>"
```

Read [Bounded worktree fan-out](fanout.md) for spawn options and process-only
lifecycle limits. Use the nine workspace commands for managed writing. Ordinary
spawn does not acquire managed workspace custody.
