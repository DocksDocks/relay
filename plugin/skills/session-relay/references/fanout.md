# Bounded worktree fan-out

Use fan-out when one managed relay worker needs isolated parallel work in the
same Git repository. Fan-out permits one depth-0 root and at most two live
depth-1 leaves.

## Contents

- [Workflow](#workflow)
- [Guarantee boundary](#guarantee-boundary)
- [Spawn a new full-context worker session (`session-relay spawn`)](#spawn-a-new-full-context-worker-session-session-relay-spawn)

## Workflow

```bash
# Start the isolated root from the registered invoking session.
session-relay spawn <repo> --fanout --from <invoker-session> \
  --tool omp --model <model> --effort <effort> -- "<root task>"

# Start at most two isolated leaves from the root.
session-relay spawn <root-worktree> --worktree --from <root-session> \
  --tool omp --model <model> --effort <effort> -- "<leaf task>"

# Commit all worker changes.
# Verify that the worktree is clean.
# Submit the handback as the final worker action.
session-relay handback --from <worker-session> --status completed --note "ready"

# Collect the committed handback as the exact stored parent.
session-relay collect <worker-session> --from <parent-session>
```

The plugin's `bin/relay` resolves the installed `session-relay` executable.
The worker prompt identifies the assigned worktree. It requires `handback` as
the final action. Do not create another branch inside that worktree. Do not
write to the worktree after handback.

Pass `--tool omp` in every spawn command. Pass the selected model through
`--model`. Pass the selected thinking level through `--effort`. Relay maps
`--effort` to omp `--thinking`.

Collect both leaves before the root submits its handback. Collection uses a
no-fast-forward merge. It removes only the registered worktree. It retains the
relay branch for manual audit. A merge conflict is aborted. The handback then
permits another collection attempt. Collection refuses a dirty parent or child.
It also refuses a changed child HEAD after handback or a collection lock held
by another collector for that reservation. A retry can finish when worktree
removal succeeded before its phase write.

## Guarantee boundary

<constraint>
A capacity slot is released only after the detached fan-out supervisor reaps
the exact CLI child process and lifecycle authority reaches
`TerminalReleasable`. A missing supervisor, uncertain drain, unclaimed birth,
or terminal-retained worker stays counted. Do not infer descendant-tree
quiescence from this process-only proof.
</constraint>

`fanout-v1.json` is separate from `lifecycle-v1.json`. Older relay processes do
not encounter new lifecycle keys. Registered authority determines parentage,
root id, and depth. Caller input cannot set these values. A third live leaf is
rejected before its branch or worktree is created. A root admits leaves only
while its exact managed worker and generation remain `Active`. Managed workers
cannot create another depth-0 root.

`FailedNoProcess` is the only non-counting pre-birth failure. It requires all
three conditions:

- Child `spawn()` returned no process.
- The exact pristine worktree still matches its base commit and repository identity.
- That worktree was removed.

Any ambiguity retains the reservation and its slot.

Fan-out does not provide cgroups, pidfds, descendant containment, automatic
recovery, lease stealing, branch deletion,
cross-repository collection, or depth greater than one. Historical recovery is
operator context. It is not a product guarantee.

## Spawn a new full-context worker session (`session-relay spawn`)

A native subagent runs inside the current session and project. Use a separate
omp session when work needs another project's `AGENTS.md`, skills, and plugins.

```bash
session-relay spawn <dir> --tool omp --model <model> --effort <effort> \
  --name worker1 [--reply-to <me>] [--watch] -- "<first task>"
```

- **Select omp explicitly.** Use `--tool omp`. Do not depend on CLI tool defaults.
- **Select the model and thinking level.** Pass `--model` and `--effort` on each
  spawn. Relay maps the effort value to omp `--thinking`.
- **Wait for managed birth.** Relay writes a pending worker before launch. It
  passes one exact claim token only to that child. The omp session hook must
  bind the observed session id as `Active` before spawn reports birth. A
  registration without that claim is killed and refused.
- **Preserve session discovery.** The extension passes `RELAY_OMP_SESSIONS` from
  the active session root to every child command. Do not remove this environment
  value from extension-launched commands.
- **Report to the parent.** The first prompt contains a standing instruction to
  report results and questions to `--reply-to`. The default is the invoking
  session's bus name. The prompt uses the absolute installed `session-relay`
  path. This permits replies from a project without the plugin.
- **Wait for the first turn when needed.** Add `--watch` to keep the spawn caller
  attached to the direct child process until its first turn exits. The relay
  exit mirrors the child exit. Standard output reports `first turn complete`
  or `first turn failed`. Without `--watch`, spawn returns at registration.
- **Choose permissions explicitly when needed.** The default maps to omp
  `--approval-mode write`. Relay `--read-only` maps to `--approval-mode always-ask`.
  Relay `--full-access` maps to `--approval-mode yolo`. Use `--full-access` only
  when authorized. These flags do not establish managed workspace custody.
  Every child prompt requires a separate Git branch. It prohibits live or
  production mutations. It requires parent approval before destructive operations.
- **Continue the registered session.** Send follow-up work with
  `session-relay send worker1 -- "<follow-up>"`. For a closed session, use
  `session-relay wake worker1 --model <model> --effort <effort> -- "<bounded task>"`.
  Wake resumes omp in `-p` mode with text. A one-shot process exit does not remove
  the resumable session.
- **Use live delivery for an open session.** The omp extension polls every three
  seconds. It delivers mail as `relay_mail` follow-up messages with `triggerTurn`.
  This delivery is not a headless wake. Use `/relay` to inspect the roster and
  pending count.
- **Inspect a birth timeout before retrying.** Read the stderr log named in the
  error: `~/.agent-relay/spawn-logs/<id>.stderr`. When a log exceeds 4 MiB, relay
  compacts it to the newest 3 MiB. Copy the log before another long run if early
  output matters.
- **Control token use.** A spawned worker is a full omp session. Model turns use
  the configured provider's billing or usage allowance. Spawn only for a defined
  task. Do not spawn workers in an unbounded loop.

Session Relay supports Linux x86-64 and arm64 only. For managed writing, read
[Managed workspaces](workspace.md). Use only its exact nine
`session-relay workspace` commands. Follow its admission, actor, recovery,
integration, and unmanaged-process limits. Managed workspaces are separate from
fan-out. Workspace outcomes are not Docks plan-review evidence.
