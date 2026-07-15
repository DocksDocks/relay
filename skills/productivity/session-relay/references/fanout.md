# Bounded worktree fan-out

Fan-out is for one managed relay worker that needs isolated parallel work in the
same Git repository. It is deliberately fixed at one depth-0 root and at most
two live depth-1 leaves.

## Workflow

```bash
# The registered invoking session starts the isolated root.
relay spawn <repo> --fanout --from <invoker-session> \
  --tool <claude|codex> --model <model> --effort <effort> -- "<root task>"

# The root may start no more than two isolated leaves.
relay spawn <root-worktree> --worktree --from <root-session> \
  --tool <claude|codex> --model <model> --effort <effort> -- "<leaf task>"

# Each worker commits everything, verifies a clean worktree, then hands back.
relay handback --from <worker-session> --status completed --note "ready"

# Only the exact stored parent collects the committed handback.
relay collect <worker-session> --from <parent-session>
```

The spawned worker prompt already identifies the assigned worktree and makes
`handback` the final action. Do not create another branch inside that worktree,
and do not write to it after handback.

Collect both leaves before the root hands back. Collection uses a no-fast-forward
merge, removes only the registered worktree, and retains the relay branch for
manual audit. A merge conflict is aborted and returned to a retryable handback;
a dirty parent or child is refused. Collection also refuses if the child's HEAD
changed after handback or another collector holds that reservation's collection
lock. A retry can finish when worktree removal succeeded before its phase write.

## Guarantee boundary

<constraint>
A capacity slot is released only after the detached fan-out supervisor reaps
the exact CLI child process and lifecycle authority reaches
`TerminalReleasable`. A missing supervisor, uncertain drain, unclaimed birth,
or terminal-retained worker stays counted. Do not infer descendant-tree
quiescence from this process-only proof.
</constraint>

`fanout-v1.json` is separate from `lifecycle-v1.json`; older relay processes do
not encounter new lifecycle keys. Parentage, root id, and depth are derived from
registered authority, never accepted from caller input. A third live leaf is
rejected before its branch or worktree is created. A root admits leaves only
while its exact managed worker and generation remain `Active`; managed workers
cannot create another depth-0 root.

`FailedNoProcess` is the only non-counting pre-birth failure. It is recorded only
when child `spawn()` returned no process, the exact pristine worktree still
matches its base commit and repository identity, and that worktree was removed.
Any ambiguity retains the reservation and its slot.

This first release does not provide cgroups, pidfds, descendant containment,
automatic recovery/GC, lease stealing, branch deletion, app-server fan-out,
cross-repository collection, or depth greater than one. Historical recovery is
operator context, not a product guarantee.
