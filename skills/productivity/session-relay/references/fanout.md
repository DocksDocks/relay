# Bounded worktree fan-out

Fan-out is for one managed relay worker that needs isolated parallel work in the
same Git repository. It is deliberately fixed at one depth-0 root and at most
two live depth-1 leaves.

## Contents

- [Workflow](#workflow)
- [Guarantee boundary](#guarantee-boundary)
- [Spawn a new full-context worker session (`session-relay spawn`)](#spawn-a-new-full-context-worker-session-session-relay-spawn)

## Workflow

```bash
# The registered invoking session starts the isolated root.
relay spawn <repo> --fanout --from <invoker-session> \
  --tool <claude|codex> --model <model> --effort <effort> \
  [--service-tier default|fast for Codex] -- "<root task>"

# The root may start no more than two isolated leaves.
relay spawn <root-worktree> --worktree --from <root-session> \
  --tool <claude|codex> --model <model> --effort <effort> \
  [--service-tier default|fast for Codex] -- "<leaf task>"

# Each worker commits everything, verifies a clean worktree, then hands back.
relay handback --from <worker-session> --status completed --note "ready"

# Only the exact stored parent collects the committed handback.
relay collect <worker-session> --from <parent-session>
```

The spawned worker prompt already identifies the assigned worktree and makes
`handback` the final action. Do not create another branch inside that worktree,
and do not write to it after handback.

Codex fan-out always resolves an explicit role tier. Use `--service-tier fast`
only for a Fast role and `--service-tier default` otherwise; omission is
Standard, and Claude rejects the flag.

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

## Spawn a new full-context worker session (`session-relay spawn`)

A native subagent runs inside THIS session and project. When the work belongs in
ANOTHER project — with that project's CLAUDE.md/AGENTS.md, skills, and plugins —
birth a real, resumable session there instead:

```bash
session-relay spawn <dir> --tool claude|codex --model <model> --effort <effort> [--service-tier default|fast for Codex] --name worker1 [--reply-to <me>] [--watch] -- "<first task>"
```

- **Pick the tool from standing preference first.** If `RELAY_SPAWN_TOOL`, user
  config, or session memory names `claude` or `codex`, use that tool without asking.
  Ask via the native question UI only when no preference is discoverable; the bare
  CLI defaults to `codex` when the codex CLI is installed, else `claude` — a
  printed note names the choice either way.
- **Model/tier discipline:** pass `--model`/`--effort` every time. For Codex,
  pass `--service-tier fast` only for an explicitly Fast role; otherwise pass
  `--service-tier default` (omission has the same Standard meaning). The flag is
  rejected for Claude. Classic Standard launches append
  `-c service_tier="default"`; Fast appends both `-c features.fast_mode=true`
  and `-c service_tier="fast"` without modifying global config.
- **Managed birth:** before launching a classic Claude/Codex child, relay writes
  a pending worker and passes one exact claim token only to that child. Its
  SessionStart hook must bind the observed session id `Active` before spawn
  reports birth; a registration without that claim is killed and refused. With
  Codex `--server <socket>`, relay instead orders `pending → thread/start →
  atomic exact claim + discovery → guarded turn/start`. No `codex exec` process
  or hook runs on the app-server path, and first-turn bytes cannot precede
  `Active`.
- The first prompt carries a standing prefix: report results/questions to
  `--reply-to` (default: this session's bus name) via the absolute installed
  `session-relay` path — so the reply loop works even in a project where the plugin isn't
  installed. App-server spawn includes `--from <returned-id>` directly because
  there is no hook-provided identity line.
- **App-server turn pump:** after confirming `turn/start`, foreground spawn
  returns while a detached relay helper keeps the same connection alive for MCP
  elicitations. `--watch` waits for that helper instead. The helper accepts
  `bus` only because the relay registered the thread's origin before starting
  the turn; joined/foreign threads still decline all elicitations. The existing
  `--timeout` (30 seconds by default) is a hard pump cap. At timeout relay first
  publishes a lifecycle fence, then interrupts only the exact recorded
  `{threadId, turnId}` under the drained fence permit. Matching completion or an
  idle exact thread confirms `Fenced`; missing/mismatched evidence stays
  `FencingUnconfirmed` and refuses re-entry. The cancellation wait is capped at
  five seconds. A failed `turn/start` has no safe turn id, so it fences
  unconfirmed and emits no interrupt. A connection/pump failure after
  `turn/start` also fences unconfirmed because terminal state cannot be proven.
- **App-server tier boundary:** relay sends explicit `serviceTier:"default"` or
  `"fast"` on thread start/resume and every turn start. It verifies the effective
  tier reported by thread start/resume. Missing or mismatched Fast support fails
  closed; relay never downgrades to Standard or inherits a shared server's state.
- **Completion signal:** add `--watch` to keep the spawn caller attached to the
  direct child process until its first turn exits. The relay exit mirrors the
  child and stdout reports `first turn complete` or `first turn failed`; without
  the flag, registration-time return stays unchanged.
- **Permissions (symmetric):** default = Claude `--permission-mode auto` / Codex
  `--sandbox workspace-write`; `--read-only` opts down (plan / read-only);
  `--full-access` opts up (bypassPermissions / danger-full-access). Guardrail rules
  ride in every child's prompt regardless: separate git branch only, no
  live/production mutations, ask the parent before destructive ops.
- Continue the conversation with `session-relay send worker1` + `session-relay wake worker1` — the id is
  durable and resumable; the process being one-shot is expected.
- On birth timeout, the error names the child's stderr log
  (`~/.agent-relay/spawn-logs/<id>.stderr`) — read it before retrying. Each log
  keeps only its newest approximately 4 MiB, so copy it before another long run
  if the earliest output matters.
- **Billing:** every spawned child is a full agent session on your subscription
  (Claude OAuth / ChatGPT login) — heavier than a wake; spawn deliberately, never
  in loops.

For managed writing, use only the exact nine `session-relay workspace` commands after reading the Linux-only admission, exact macOS STOP, ordinary macOS release boundary, actors, recovery, integration, and unmanaged-process limits in [`references/workspace.md`](references/workspace.md); this is neither legacy fan-out nor Docks plan-review evidence.
