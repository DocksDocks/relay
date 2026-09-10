# Plans

A plan records a goal, research, ordered work, and verification evidence.
The canonical record is a GitHub issue. Reviews live in its comments. GitHub
owns its title, assignee, labels, timestamps, and completion state.
Do not track plan bodies as repository files.

Read `skills/productivity/plan-manager/references/plan-contract.md` inside the
installed `plan-lifecycle` plugin for the v4 contract and helper commands.
Resolve this reference from the installed plugin, not a marketplace checkout.

Use `plan-workspace` to maintain workspace routing, main-context `plan-manager`
to run the lifecycle, and internal `plan-reviewer` for plan review.
The two read-only wrappers are `plan-reviewer` and `code-reviewer`.
If the plugin is missing, report that prerequisite. Do not invent a local copy.

`docs/plans/finished/` is frozen pre-GitHub history, not lifecycle input.
An optional `docs/PLAN-QUEUE.md` is a human note only. No helper reads it.
