# Managed workspaces

Use Relay managed workspaces when multiple writing sessions must not share a checkout, Git index, branch, or mutable external resource. Relay preserves the owner's current work, provisions one deterministic Git worktree and branch per writer, holds a lifetime workspace lease, launches the worker under managed custody, brokers supported Git mutations, and serializes handback and integration.

This is a Session Relay facility. It does not add a Docks command or bind workspace outcomes to plan review.

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

## Commands and actors

The public surface has exactly these nine commands:

| Command | Actor | Purpose |
|---|---|---|
| `session-relay workspace preserve` | repository owner, before a coordinator exists | Record clean, commit-mode, or artifact-mode WIP without changing the source checkout's HEAD, index, or worktree bytes. |
| `session-relay workspace start` | coordinator | Validate preservation and platform admission, allocate the branch/worktree and resources, acquire the lease, apply WIP, and launch one managed worker. Only the first start may bootstrap coordinator authority without a coordinator capability file. |
| `session-relay workspace list` | coordinator | Authenticated repository-wide read of managed sessions and lease evidence. |
| `session-relay workspace inspect` | coordinator | Authenticated read of one session's state, claims, resources, custody, lease, handback, retention, and recovery evidence. |
| `session-relay workspace handback` | worker | Quiesce the managed worker and submit its capability-bound, linear commit chain. This is distinct from the legacy top-level `session-relay handback`. |
| `session-relay workspace integrate` | coordinator | Queue and apply an accepted worker chain to the integration checkout in order. |
| `session-relay workspace recover` | coordinator | Inspect or settle a retained failure, resume only a proven prelaunch state, retain-abort, or rotate a still-authenticated coordinator capability. |
| `session-relay workspace finish` | coordinator | Complete coordinator-owned retention, resource/worktree/ref cleanup, lease close, and terminal closure after successful integration. It is not worker commit return. |
| `session-relay workspace abort` | coordinator | Fence the worker and retain or clean work only when custody, empty-process, Git, resource, and lease evidence makes that safe. |

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

Request and receipt files are canonical, digest-bound inputs. `list --repository <canonical-root> --coordinator-capability-file <absolute-file>` and `inspect <session-id> --repository <canonical-root> --coordinator-capability-file <absolute-file>` are authenticated reads. Worker `handback` requires its request file, request SHA-256, and worker capability file; it uses the private broker and does not open coordinator authority. `integrate`, `recover`, `finish`, and `abort` require the canonical request file, its SHA-256, and the current coordinator capability file. A session ID or knowledge of an authority path is not authorization.

## Preserve before starting

Run `workspace preserve` before the first managed writer. Commit mode creates a create-once preservation ref through a temporary index; artifact mode records a binary index diff plus an exact PAX archive and inventory for untracked content. Both modes verify that the source HEAD, index, status, and tracked/untracked inventory are unchanged before returning a receipt.

Retain the WIP receipt and its preservation ref or artifact until integration or explicit retention is proven. A clean source is still represented by one applied-WIP commit before worker execution, so every worker and integration chain has the same ordering.

## Worktrees, leases, and inspection

`workspace start` automatically allocates a separate worktree because two writers in one checkout would share mutable worktree and index state. Relay assigns a deterministic `docks/<session-id>/<task-slug>` branch and a deterministic workspace root; it never resolves a collision with a numeric suffix or force-resets an existing branch.

The lifetime lease represents active custody of that opened worktree identity. Two writers cannot lease the same worktree; separate admitted worktrees may hold independent leases. `workspace list` and `workspace inspect` report each active session's durable state plus custody/lease evidence. They report evidence as unproven when liveness cannot be established; JSON state alone is never treated as a live lease.

If a writer already owns the worktree, open a separate managed worktree or continue with a read-only session. A Relay-launched read-only session may coexist because it receives no workspace mutation capability. Read-only coexistence is not a promise that an arbitrary shell, IDE, or independently launched agent cannot write.

## Claims, Git, and handback

Each writer receives exclusive file or directory claims. Overlapping paths, case aliases, and unsafe atomic-replacement boundaries are refused before launch. Supported index changes and commits go through the capability-authenticated Git broker. Merge, rebase, reset, force-push, branch switching, common-Git-dir writes, and direct mutation outside admitted claims are refused.

Worker exit is not handback. `workspace handback` first drains the broker, revokes mutation, stops and reaps the worker tree, and proves no managed descendant remains. It then records the exact linear produced-commit chain. The coordinator integrates one chain at a time and preserves commit order.

An integration conflict restores the exact clean pre-integration HEAD/index/tree and settles that session as `IntegrationBlocked` with `needs_user_action`. Relay does not retry, auto-resolve, or dispatch review/repair for the same session. Retain the worker branch and use a new explicitly authorized path to resolve the work.

## Recovery, handoff, and cleanup

Crash recovery revokes capabilities and fences the managed process tree. The lease and work remain retained until Relay proves the tree empty and the coordinator chooses an explicit `workspace recover` action. Missing or corrupt current coordinator authority is not recoverable by a new bootstrap, owner identity, or session UUID.

Use `workspace inspect` before recovery or cleanup. `finish` and `abort` refuse to delete dirty, unintegrated, identity-ambiguous, or unreceipted work. Resources are released idempotently only after their recorded identity is revalidated. The lifetime lease closes last; terminal `Closed` follows an independent successful probe that no lease reference survived.

## External resources

Every start request decides each closed resource kind exactly once: `port`, `temp_dir`, `build_dir`, `database_schema`, `log_dir`, and `cache_dir`. Mark unused resources explicitly. Relay allocates private directories, holds allocated loopback port file descriptors, and accepts database schemas only from its digest-pinned provider protocol. Allocations and release outcomes are durable and visible through inspection.

The worker receives projected environment variables, not coordinator authority. Unsupported providers, changed provider binaries, malformed receipts, timeout, or ambiguous deletion stop state advancement and retain the allocation for recovery.

## Worktree or clone?

Choose a managed worktree when writers belong to one local repository and should share its object database while keeping checkout, index, branch, claims, resources, and custody separate. This is the supported Relay path and makes commit integration explicit.

Choose a separate clone when sharing the common Git directory or local object store is itself outside your trust or operational boundary. A clone launched outside these nine commands is unmanaged: Relay does not allocate its resources, broker its Git, hold its workspace lease, recover it, or integrate it. Move commits between clones using ordinary reviewed Git operations.

## Managed and unmanaged boundary

Relay's guarantees cover only workers launched and retained by an admitted managed workspace. Supported Relay launch, Git-broker, handback, integration, recovery, resource, and cleanup paths fail closed when their evidence is missing or drifts.

Relay cannot retroactively control arbitrary same-UID shells, IDEs, raw Git commands, old Relay binaries, or independently launched Claude, Codex, or OMP processes. Advisory repository markers warn compatible Relay versions; they are not a kernel boundary against other local processes. Do not claim a managed workspace is safe while an unmanaged writer can reach the same checkout or common Git directory.

Legacy top-level `spawn`, `handback`, `collect`, `spawn --fanout`, and `spawn --worktree` keep their existing process-only behavior. They do not become managed workspace commands and do not provide descendant-tree custody.

## Platform boundary

Managed writing is supported only by the Linux `linux_cgroup_v2_pidfd` backend on an admitted native ext4 repository and authority store. It requires delegated cgroup v2 with `cgroup.kill` and recursive `populated 0`, pidfds, Landlock ABI 3 or newer, authenticated custody processes, and an activation barrier before worker code. Missing or ambiguous prerequisites refuse before worker execution.

The macOS STOP is exact: managed writing is stopped, and the inactive `macos_pgroup_libproc` backend returns `process groups are escapable, kqueue is PID observation rather than durable containment, and no documented public primitive provides crash-durable descendant membership plus atomic kill/empty proof`. A macOS build, APFS check, process scan, or passing negative-admission test is not custody evidence and must not be attested or advertised as managed-workspace support.

This STOP does not remove ordinary macOS Relay support or its x86-64 and arm64 release artifacts. Each GitHub-hosted native macOS release leg must run the exact negative-admission test against its fresh binary before attestation and upload. That runner evidence is sufficient to prove fail-closed managed-workspace refusal for an ordinary macOS artifact; it does not claim macOS custody, a successful macOS managed workspace, or testing on a physical Mac.

Other operating systems; containers or overlay filesystems; NFS, SMB, FUSE, network, cloud, or removable filesystems; cross-UID/shared-service workspaces; and remote workers are unsupported.

## Precedents, not compatibility claims

Relay follows the established separate-checkout pattern while retaining its own stricter authority, custody, and integration contract:

- [Git worktrees](https://git-scm.com/docs/git-worktree) define the shared-object, separate-checkout primitive.
- [Conductor isolated workspaces](https://conductor.build/) demonstrate concurrent agent work in isolated workspace roots.
- [OpenAI Codex worktrees](https://developers.openai.com/codex/app/) document worktree-based parallel tasks.
- [Claude Code worktree workflows](https://code.claude.com/docs/en/common-workflows) document separate worktrees for parallel Claude sessions.
- [Cursor Background Agents](https://docs.cursor.com/en/background-agent) use isolated remote working copies.
- [GitHub Copilot coding agent](https://docs.github.com/en/copilot/concepts/agents/coding-agent/about-coding-agent) works in an isolated GitHub Actions environment.

These links are design precedents only. They do not imply protocol, security, release, or product compatibility with Relay.
