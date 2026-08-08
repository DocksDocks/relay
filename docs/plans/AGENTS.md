# AGENTS.md — docs/plans/

Canonical plans are complete cold handoffs for work that benefits from durable coordination. The Markdown plan is the only tracked artifact;
rendered views are disposable. `active/` is multi-occupancy and `finished/` is the terminal archive.

Use direct implementation for one clear, reversible, low-risk local diff with one bounded acceptance path. Direct work creates no plan,
reviewer invocation, or automatic commit. Use a canonical plan for an explicit planning request, multi-commit or cross-repository work,
scheduling, cold handoff, an unresolved decision, a cross-subsystem or public-contract change, security-sensitive or destructive work,
or any requested external effect. Never create a placeholder plan merely to unlock review.

<constraint>
There are exactly three live owners. `plan-workspace` maintains this workspace. Main-context `plan-manager` owns goal classification,
drafting, bounded review and one repair, lifecycle, implementation/delegation, observed acceptance, archive, and guarded GitHub issue
publication. Internal `plan-reviewer` reads one immutable bundle and returns `PlanReviewV1` evidence only. Only the reviewer has
Claude/Codex wrappers; main invokes `plan-manager` directly.
</constraint>

<constraint>
Current plans contain exactly one unfenced current `Plan-run: <compact JCS PlanRunV1>` line. Prior terminal runs may appear only as
validated append-only `Plan-attempt-history: <compact JCS PlanAttemptHistoryV1>` records inside `## Review`; history is never current
authority. Schemas 1–6 are historical validation/quarantine formats only: preserve their bytes and behavior, but never emit one as
current authority. Unsettled legacy evidence never blocks an unrelated goal or authorizes dispatch or an external effect.
</constraint>

## Skill routing

| Request | Owner |
|---|---|
| Bootstrap, migrate, audit, or explicitly refresh `docs/plans/` | `plan-workspace` |
| Decide direct work versus a canonical plan; create, review, repair, execute, verify, block, schedule, finish, archive, list, show, or publish a plan | main-context `plan-manager` |
| Inspect one immutable draft-review bundle and return typed findings | internal `plan-reviewer` |

No creator, repairer, improver, or manager wrapper is live. A missing reviewer wrapper does not create another role: dispatch a fresh
read-only task with the same `PlanReviewV1` contract.

## Directory and frontmatter

```text
docs/plans/
├── AGENTS.md
├── CLAUDE.md      # exactly @AGENTS.md
├── active/        # every nonterminal plan; status is frontmatter
└── finished/      # terminal archive, unique date-prefixed filename
```
`docs/plans/QUEUE.md` is optional; when present it carries exactly one `Plan-queue: PlanQueueV1` marker and one `Stage | Goal ID | Plan | Depends on | Why` table. Goal ID is the row identity, and current paths are resolved by scanning `active/` and `finished/` records.
A row is eligible only when every explicit direct and transitive dependency is finished; stages give deterministic priority among otherwise eligible rows. The queue is a discovery and prioritization view only, never lifecycle, review, mutation, scheduling, or external-effect authority.
A workspace without the queue stays valid.

Every current plan starts with a closed frontmatter map. Project-specific fields may extend this shape only when the nested contract names them.

```yaml
---
title: Short imperative title, ≤70 chars
goal: One observable sentence, ≤200 chars
plan_hash_mode: status-excluded-v1
status: drafting | planned | scheduled | ongoing | blocked | finished
created: "2026-07-24T12:00:00+00:00"
updated: "2026-07-24T12:00:00+00:00"
started_at: null
finished_at: null
assignee: null
tags: []
affected_paths: []
related_plans: []
---
```

`blocked` adds `blocked_reason` and `blocked_since`. `scheduled` adds `trigger: date | manual-approval`, and a date trigger adds
`scheduled_date` plus `auto_execute`. Set `started_at` once on first `ongoing`; set `finished_at` only when archiving. All timestamps are quoted ISO 8601 with an offset.

The non-authoritative `## Proposed repair` section is excluded from `plan_sha256`; it is installed only by the transition that blocks a
run and is never added to an already-blocked run, because `blocked` → `blocked` rejects any byte change.

## Cold-handoff body

Every canonical plan contains `## Goal`, `## Context & rationale`, `## Environment & how-to-run`, `## Steps`, `## Acceptance criteria`,
`## Out of scope / do-NOT-touch`, `## STOP conditions`, `## Open questions`, `## Review`, and manager-written `## Verification Results`.
Use a specific `N/A — <reason>` only when a section truly does not apply.

The legacy Steps schema omits `Id`; the new Steps schema adds it immediately after `#`.

The legacy Steps table is exact:

| # | Task | Files | Depends | Effect | Status | Done when / failure action |
|---:|---|---|---|---|---|---|
| 1 | concrete action | exact paths | — | `local` | `planned` | observable proof or STOP |

The new Steps table is exact:

| # | Id | Task | Files | Depends | Effect | Status | Done when / failure action |
|---:|---|---|---|---|---|---|---|
| 1 | concrete_action | concrete action | exact paths | — | `local` | `planned` | observable proof or STOP |

`Id` is immediately after `#` and must match `[a-z][a-z0-9_]{0,63}`. A missing `Id` is advisory only for the frozen grandfather set;
every new plan requires the `Id` column and one valid, unique id per Steps row. The exemption has exactly two routes: the frozen set,
exactly `docs/plans/active/plan-lifecycle-plugin-extraction.md` and `docs/plans/active/step-ids-and-class-budget.md`, and every
`docs/plans/finished/` path by prefix. An archived plan carries no frozen entry: keeping its old active path would exempt a new plan that
reused the filename, silently skipping the Id requirement. Within `Done when / failure action`, step citations are accepted only as
`step:<id>` and must resolve to a declared id; valid-looking numeric `step N` citations are rejected. `#` and `Depends` keep their numeric
display-number semantics.

`Effect` is exactly `local | probe | production_access | publish | push | release | deploy`. Status is exactly
`planned | in-flight | done | blocked | skipped`. Every row names exact paths and an observable done condition. Acceptance uses ordered
unique ids in an `ID | Command | Expected` table. Plans must not contain `TBD`, `TODO`, vague follow-ups, or undefined forward references.

### Status-excluded Steps hashing

New and successor plans opt in with frontmatter `plan_hash_mode: status-excluded-v1`; unmarked plans use byte-identical legacy hashing.
For marked all-`planned` bootstrap plans, validation accepts either the legacy full-body digest or the normalized digest. The first legal
status progress transaction atomically installs the normalized digest.

Normalization applies only to the exact `Status` cells of a valid unfenced `## Steps` table; every other cell and byte remains bound.
A status progress transaction allows only legal row-state changes plus the lifecycle `updated` timestamp and an optional bootstrap
`plan_sha256` change. `done` and `skipped` are terminal; blocked and finished PlanRun bytes stay immutable.

Plan text must be portable: a cold reader may hold this repository at a different path. `repository_id` is a portable repository identifier
such as `DocksDocks/docks`, never a local filesystem path. Cite repository-relative paths only; acceptance rows run from the repository
root and carry no `cd <absolute path>` prefix. A cross-repository reference names the other repository's id, not a local checkout.
Recorded evidence is exempt and frozen: never rewrite a `cwd` or path already captured inside a receipt.

## Release pre-completion guards

A release plan that will mutate an external boundary places every available live read-only final-boundary check before completion-review
reservation, using the exact canonical identities and data spellings consumed by the later mutation. Available means the repository
already provides a read-only command or adapter path that exercises the boundary without the pending mutation; never invent a check or
network call. If an available check requires probe authority and exact live `ExternalAuthorityV1` is absent, block before completion
review rather than review an unexercised release assumption.

Every closed object that affected code validates or emits has an explicit preserve-or-change disposition. A preserved shape has an
exact-key compatibility fixture. An intentional shape change is in scope and includes migration, versioning, and historical-reader acceptance.

When present, roles include release source, plan source, execution parent, implementation commit, and tag commit. A release identity
matrix names each role, producer, consumer, and required equality, distinction, or ancestry relation. Reject a contradictory or unstated
relation and any later successor whose current-run fixtures remain pinned to its predecessor.

Existing `PlanRunV1`, review-result, affected-path manifest, `ExternalAuthorityV1`, and release-receipt shapes remain byte-compatible;
these guards add no field, state, result, or authority.

## Current record

The closed record shapes — `ReviewPhaseV1`, `PlanRunV1`, `PlanAttemptHistoryV1` and `PlanRunReplacementAuthorityV1` — are defined once in
the `plan-manager` skill's `references/planrunv1-schema.md`. That file is the single source of truth for field names, types and enum
members; this node states the rules that govern them and never restates the shapes, because two spellings of one schema drift.

Compact JCS is byte-authoritative. `repository_id + plan_path + run_id` is the run identity. Cross-repository goals use one child run per
repository joined by `goal_id`; never record an unqualified commit as cross-repository identity. `requested_effects` is unique and
canonical-ordered, always beginning with `local`. It records intended scope, never authority.

A terminal `blocked` run is immutable. Exact current-user authorization may replace it only for the same
`goal_id + repository_id + plan_path`; it binds the predecessor identity and digest of the exact successor PlanRun. The transaction
appends the predecessor record and bytes/authorization digests, then installs the fresh `run_id` and review baselines in the same file.
History is append-only; ordinary transitions cannot alter it. A finished plan file never reopens.

`plan_sha256` covers the canonical plan after excluding only lifecycle status and timestamps, the `Plan-run` line, `## Review`,
manager-written `## Verification Results`, and the non-authoritative `## Proposed repair`. Goal, scope, paths, steps, effects, safety,
acceptance, and open decisions remain bound. `source_base` plus `source_sha256` binds a canonical sorted existence, kind, mode, and content
manifest for every affected path at review time, including dirty/untracked bytes and tombstones. `acceptance.source_sha256` binds the
final affected-path manifest; `verification_sha256` binds canonical Verification Results bytes. Never list the plan record in
`affected_paths`; acceptance writes to it and breaks that bind.
Minting or changing an acceptance requires live manifest proof and the caller passes it; carrying one forward unchanged, or reading an immutable terminal
predecessor, does not. A live-worktree proof is discharged at the instant it is written and is not re-provable once HEAD moves, so it is never a durable
invariant.

A scope omission found before acceptance — most often a path missing from `affected_paths` — is amended in place: one `ongoing -> ongoing` transition may
change `plan_sha256`, `source_base`, and `source_sha256` and no other field, only while neither review phase is `reserved` or `transport_retried`,
`completion_review.state` is not `passed`, and `acceptance` is null. After acceptance its scope is settled and only a replacement may change it.

## Closed phase table and transitions

| Phase state | Draft invocations | Completion invocations | Input | Result | Extra rule |
|---|---:|---:|---|---|---|
| `not_required` | 0 | forbidden | null | null | draft only, local risk only: the settled self-check gate |
| `not_started` | 0 | 0 | null | null | draft and completion baseline at every risk |
| `reserved` | 1–2 | 1–2 | hash | null | live initial or repair launch |
| `transport_retried` | 1–2 | 1–2 | hash | null | live launch after one transport failure |
| `retryable` | 0–1 | 0–1 | hash | failure hash | first transport failure; reservation refunded |
| `repairing` | 1 | 1 | hash | reviewer-result hash | accepted repair verdict only |
| `passed` | 1–2 | 1–2 | hash | reviewer-result hash | validated matching output |
| `degraded` | 1–2 | forbidden | hash | failure-set hash | draft only, local risk only |
| `blocked` | 1–2 | 1–2 | hash | evidence/result hash | terminal for this run |
| `cancelled` | 1–2 | 1–2 | hash | cancellation hash | terminal for this run |

Legal phase transitions are only `not_started → reserved`, plus local-draft `not_started → not_required`; `reserved → passed | repairing |
blocked | cancelled | retryable`; `retryable → transport_retried | blocked | cancelled`; `transport_retried → passed | repairing | blocked |
cancelled | degraded`; and `repairing → reserved | blocked | cancelled`. A transport-only failure refunds its reservation and allows one fresh
`transport_retried` dispatch without changing substantive bindings; a second transport failure degrades only local draft work at local risk and
otherwise blocks. One retry, never two. Terminal states never reset.

Before spawning, transactionally increment the invocation count and persist `reserved`, or `transport_retried` after a transport failure, with the
exact input digest. A verdict spends the reserved substantive permit. An arriving result may mutate only the matching phase while it remains
`reserved` or `transport_retried` with the same run id, invocation, and input hash; stale results are discarded. Cold entry into either live state
changes it to `blocked` with dangling-launch evidence and never redispatches.

Before reserving, preflight the exact reviewer route and a private file that will receive complete stdout. Preflighting the route means running it
once with a trivial prompt and requiring exit 0 plus a parseable JSON object, because a route that spawns can still answer `usage_limit_reached` or
reply off-contract; the probe runs below the dry-run exit and above the reserve, so a refusal costs no permit, and `--skip-route-probe` waives it
only for an already-proven route. Each invocation has a newly sealed bundle whose closed binding contains that invocation number. After reservation
read-back, derive the prompt only from that bundle and capture directly to the file. Never consume console rendering, clipped lines, transcript
fragments, or reconstructed JSON; do not request compact/single-line reviewer output. Parse the file, validate the closed object, then hash canonical
JCS. Review transport is a direct reviewer subprocess. Session Relay is never review evidence and never a required dependency.

Draft review has one initial review and, only after an accepted repair, one mandatory fresh verification, with a ceiling of two substantive
invocations. Completion review has an empty `accepted_classes` set, exactly one substantive invocation at local risk — spent on the
implementation commit and its exact diff, with no repair round — and exactly two at sensitive or external risk. A draft repair verdict is accepted at most once. Any further repair or new finding after the mandatory
verification terminal-blocks the run and requires a new user-authorized successor. `accepted_classes` remains valid on read for historical
records and is written by no current transition. Historical records are read-only inputs to the historical adapter and never current authority.

For draft review, pre-seal rebinding changes exactly the run's `plan_sha256`, `source_base`, and `source_sha256`; it leaves both review phases
untouched, so sealed `plan.md` retains the pre-reserve draft phase. Immediately before a permit is reserved, the driver re-verifies the bundle and its
digests through reviewer policy, then requires record `plan_sha256` to equal binding `plan_sha256`, record `source_sha256` to equal binding
`source_sha256`, and record `source_base` to equal manifest `source_base`. The binding has no `source_base` field. Any mismatch fails with `PREFLIGHT
FAILED - no permit reserved, no reviewer dispatched.` before the reserve transaction.

A transport retry preserves canonical plan/source and any completion implementation/acceptance bindings, but seals a fresh bundle with a different
input digest and persists `transport_retried`. It reuses the refunded substantive permit; reusing the failed dispatch's bundle or prompt is stale.

## Closed lifecycle and tuple matrix

Lifecycle transitions are only absent → `drafting`; `drafting` → `planned |
scheduled | ongoing | blocked`; `planned` ↔ `scheduled`; `planned | scheduled` →
`ongoing | blocked`; and `ongoing` → `finished | blocked`. `finished` is terminal.

| Frontmatter status | Draft phase | Completion phase | Implementation / acceptance | Blocker |
|---|---|---|---|---|
| `drafting` | `not_started | reserved | transport_retried | retryable | repairing | passed`, plus local-only `degraded | not_required` | `not_started` | both null | null |
| `planned` / `scheduled` | `passed`, or local-only `degraded | not_required` | `not_started` | both null | null |
| `ongoing` before completion | `passed`, plus local-only `degraded | not_required` | `not_started` | both null | null |
| `ongoing` during/after completion | `passed`, plus local-only `degraded | not_required` | `reserved | transport_retried | retryable | repairing | passed` | implementation required; acceptance required except that a sensitive/external replacement clears stale acceptance while `repairing`, then the next reservation rebinds it | null |
| `blocked` before start | baseline or terminal draft, plus local-only `not_required` | `not_started` | both null | required |
| `blocked` after start, before completion | `passed`, plus local-only `degraded | not_required` | `not_started` | both null | required |
| `blocked` during completion | `passed`, plus local-only `degraded | not_required` | `blocked | cancelled` | implementation and acceptance required | required |
| `blocked` after completion | `passed`, plus local-only `degraded | not_required` | `passed` | implementation and acceptance required | `missing_authority | concurrent_change` only |
| `finished` | `passed`, plus local-only `degraded | not_required` | `passed` | implementation and acceptance required | null |

At local risk the deterministic self-check gate is the draft gate and `draft_review` may be `not_required`; sensitive or external risk always
requires a passed substantive draft review, and no risk waives the completion review. The gate is
`plan-manager/scripts/lifecycle/plan-self-check.mjs`: it is free, repeatable at `drafting`, and settles the phase without an invocation, an
input digest, or a result digest. Settling it freezes the draft body exactly as a passed review does.

Draft baseline is `not_started` and so is the completion baseline, at every risk. A pre-dispatch `user_decision` or
`missing_authority` blocker may reopen its existing run when new user input answers it; consumed permits never reset. Every other blocked/cancelled
run is terminal and immutable. For the same domain goal, explicit current-user `PlanRunReplacementAuthorityV1` may append that terminal run to
history and start fresh review budgets under a new `run_id` at the same `plan_path`. Replacement is never automatic and never reuses predecessor
permits, bundles, prompts, output, or hashes. Unrelated goals use new files; never mint `v2`/`vN` paths to bypass a terminal run or permit budget.

The same-file replacement transaction resolves an explicit repository root and the current file's normalized repository-relative path after
validating the current record. It rejects unless that file path equals the current run's `plan_path`, before any write, and never rewrites the
target to make it match.

## Main-context orchestration

1. Classify the goal. Direct local work stays untracked. Otherwise resolve one
   stable canonical path: create it only if absent; for an explicitly continued
   same-domain terminal goal, use the guarded same-file replacement transaction.
2. Research repository facts and bind plan/source manifests, then run the
   deterministic self-check until it passes. At local risk that gate closes the
   draft phase: settle `draft_review` to `not_required`, spending no permit. At
   sensitive or external risk, also preflight reviewer availability and private
   full-output capture, seal the invocation bundle, reserve its digest, read back,
   derive the prompt from that exact bundle, then launch one fresh reviewer and
   capture its complete stdout to the file.
3. For a substantive draft review: on `pass`, continue. On a repository-grounded
   `repair`, patch only the exact accepted blocking set, then dispatch the
   mandatory changed-input verification.
   Any further repair or new finding terminal-blocks this run and requires a
   user-authorized successor. On a real missing decision/authority, block with
   evidence. A first transport-only failure refunds its reservation; seal a fresh
   bundle with a different digest, persist `transport_retried`, read back, and
   dispatch once more without changing substantive bindings. Never reuse a bundle
   or prompt. A second transport failure may degrade only reversible local draft
   work at local risk; sensitive, destructive, public-contract, security, or
   external work blocks.

4. A plan-only request writes `planned` or `scheduled` and makes one owned-path
   checkpoint commit/read-back. A canonical implementation writes `ongoing`,
   captures `execution_parent`, and makes one reviewed start checkpoint.
5. Implement or delegate local steps, run their requested smoke/acceptance paths,
   and write canonical Verification Results. Diagnose ordinary verification
   failures inside the implementation loop; repeated same-signature failure with
   no relevant-byte progress blocks this run and never reopens its draft review.
   A plan never invents its own verification gate: `scripts/plans/no-bespoke-gates.mjs` fails when a plan-named export that validates its own versioned artifact clears one member of a set at a time, never requires that set to be non-empty, and answers to at most one shipped caller.
6. Every canonical implementation commits its implementation checkpoint and binds
   that exact commit and its diff, then reserves one completion permit, minting
   acceptance atomically with the reservation, and runs a fresh code-review agent
   returning `CompletionReviewV1`. Only a matching pass may write `finished`, move
   once to a unique archive path, and create the archive checkpoint, which commits
   the plan record and its archive move and nothing else. The archive is a new
   commit, never an amend of the reviewed one: `--amend` mints a fresh SHA, so the
   recorded `implementation_commit` would name an unreachable object and the
   reviewed diff could never be re-derived.
7. Ordinary local work spends that single completion permit and has no repair
   round: its one substantive verdict settles the run. Sensitive, destructive,
   public-contract, security, or external work additionally completes every
   required available live read-only final-boundary check before reserving, and
   one accepted blocker fix replaces/amends the still-unpublished implementation
   checkpoint, reruns invalidated checks, and consumes invocation 2 on the
   replacement SHA.

No numeric score, finding quota, fallback provider/model, resumed reviewer,
draft invocation beyond the initial review and mandatory post-repair
verification, completion invocation beyond its risk ceiling, completion-plan recursion,
automatic push, or per-round state/request/receipt commit exists.

## Transactions and checkpoint commits

Every plan mutation acquires an atomic exclusive lock keyed by repository and
normalized plan path; verifies exact bytes and run preimage; reduces one closed
transition; writes and fsyncs a sibling; atomically renames; reads back; then
releases. A checkpoint additionally acquires the repository lock, verifies
expected HEAD, index, and owned-path preimage, commits only owned paths, and
reads the commit back before release. Any mismatch fails before write, dispatch,
or external action and records `concurrent_change` when the tuple permits.

A checkpoint whose changed owned paths exceed `affected_paths` amends that set to
the union and proceeds inside the same transaction, so the record always lists
every path it commits; the amendment, and the checkpoint with it, is refused
while a review phase is live, after a passed completion review, or once an
acceptance is minted.

Ordinary lifecycle writes use `transactPlanRun`. Terminal same-path rollover uses only `replacePlanRunInPlace`, locking on the predecessor
identity/preimage and validating exact authority, successor, and append-only history before write.

A same-host dead-owner lock may be reclaimed only after matching owner PID, `run_id`, and unchanged preimages. A live, foreign, ambiguous, or changed
stale lock blocks. Never weaken a lock, reset the index, include unrelated changes, or infer that another session owns a change.

Checkpoint ceilings: direct local work 0 automatic commits and 0 reviewers; reviewed plan-only 1 commit; ordinary canonical implementation
3 commits (start, implementation, archive) with 0 draft reviewers and exactly 1 completion reviewer; sensitive/external work 3 commits
(start, implementation, archive) with up to 2 draft and up to 2 completion reviewers. A real terminal blocker may add one cold-handoff
blocker commit. No automatic push follows any checkpoint.

## Reviewer records

```text
ReviewerFindingClassV1 =
  "v1_missing_decision" |
  "v1_contract_contradiction" | "v1_evidence_mismatch" |
    "v1_unstable_step_reference" |
  "v1_unauthorized_effect" | "v1_missing_safety_boundary" |
    "v1_affected_paths_incomplete" |
  "v1_acceptance_command_not_runnable" |
    "v1_acceptance_output_mismatch" |
    "v1_acceptance_coverage_incomplete" | "v1_failure_action_missing"

PlanReviewV1 = {
  schema:1, run_id:uuid, invocation:1..2,

  plan_sha256:64hex, source_sha256:64hex,
  verdict:"pass"|"repair"|"blocked",
  findings:[{id,kind:"missing_decision"|"contradiction"|"unsafe_scope"|"missing_acceptance",class:ReviewerFindingClassV1,locator,defect,fix}]
}

CompletionReviewV1 = {
  schema:1, run_id:uuid, invocation:1|2,
  implementation_commit:40hex, diff_sha256:64hex,
  verdict:"pass"|"repair"|"blocked",
  findings:[{id,kind,locator,defect,fix}]
}

ReviewInvalidInputV1 = {
  schema:1,
  error:"invalid_input",
  reason:"bundle_unavailable"|"bundle_integrity_failed"|"bundle_binding_mismatch"
}
```

Every `PlanReviewV1` finding carries a required `class`.

The draft finding vocabulary is closed by kind: `missing_decision` permits only
`v1_missing_decision`; `contradiction` permits only
`v1_contract_contradiction`, `v1_evidence_mismatch`, or
`v1_unstable_step_reference`; `unsafe_scope` permits only
`v1_unauthorized_effect`, `v1_missing_safety_boundary`, or
`v1_affected_paths_incomplete`; and `missing_acceptance` permits only
`v1_acceptance_command_not_runnable`, `v1_acceptance_output_mismatch`,
`v1_acceptance_coverage_incomplete`, or `v1_failure_action_missing`. The
reviewer emits `class`; the manager validates the kind/class pair and never
derives a class from plan prose.

Historical readers continue to accept a sorted, unique `accepted_classes` field;
an absent historical field reads as empty. Current reducers never write it.
Only the initial draft review may return an accepted repair verdict. That repair
requires invocation 2 over newly sealed candidate bytes; a further repair or any
finding terminal-blocks this run and needs explicit successor authority.

The draft substantive ceiling is two. Completion always consumes exactly two
substantive invocations and keeps its accepted-class set empty.

The two verdict records are closed compact JCS objects capped at 32 KiB. `pass`
has no findings; other verdicts have at least one. Draft `repair` contains only
defects resolvable from already-grounded repository facts. Draft `blocked`
contains only a required user decision or missing safety authority. The manager
validates every binding and accepts only reproducible findings; reviewer prose
never mutates state.

`ReviewInvalidInputV1` is a closed failure result, never a review verdict.
Classify it before generic transport, parse/output, or verdict handling. The
manager consumes it only through `review_invalid_input` against the exact live
reservation's `run_id`, invocation, and `input_sha256`; it hashes the result and
terminal-blocks the current run as `review_failed`. It never retries or resets
that run. Later same-file replacement still requires exact current-user
authority and fresh bindings.

## Effects and live authority

Local planning, edits, verification, and lifecycle may continue without external
authority. Every non-local row requires a live value derived from the exact
current-user message still present in main context:

```text
ExternalAuthorityV1 = {
  scopes: ["probe"|"production_access"|"publish"|"push"|"release"|"deploy", ...],
  mode: "read"|"mutate",
  targets: [exact-target, ...],
  source_sha256: sha256(exact-current-user-message-bytes)
}
```

Scopes are unique and canonical-ordered. `probe` must be the sole scope and
`mode:"read"`; every other scope requires `mode:"mutate"`. Scope, mode, target,
and live source digest must match at the instant of action. Persisted plan intent,
an old prompt digest, a schedule, a passed review/test, or a receipt grants
nothing. Cold recovery requires a new explicit current-user instruction.

A named `release` authorizes only that repository's documented atomic release
recipe, including its necessary tag/push/artifact publication; it grants no
deployment or production access. Standalone `push`, `publish`, `deploy`, or
production mutation needs its own literal scope and target. A probe never grants
mutation. Without authority, skip and report the external row while continuing
safe local rows; block only when the missing effect is acceptance-critical.

## GitHub issue publication

`--issues` or `publish <slug> as an issue` is a `publish` effect. Require an
existing canonical plan plus exact live publish authority for the repository.
Before creating anything, require successful `gh auth status`, a GitHub remote,
and `gh repo view --json visibility`. For a public repository, warn that the
issue is public and require explicit confirmation when the plan names a
vulnerability, credential location, or other sensitive finding. A failed check,
missing authority, or declined confirmation creates no issue and writes nothing.

Create the issue with the canonical title/body, record the returned URL in
`## Notes` through the plan transaction, and read it back. Publication never
changes lifecycle status, dispatches review, or makes the issue authoritative.
Report success only after the owned Notes checkpoint succeeds.

## Legacy quarantine and views

List, show, and workspace audit scan frontmatter first; they do not validate every
active plan as a prerequisite. Classify legacy evidence only for the requested
target. A record-free plan or complete settled terminal schema-1–6 family may be
migrated target-locally during an explicitly requested local start. Active,
prepared, commitment, cancellation, crossed, malformed, or otherwise unsettled
evidence is `legacy-quarantined`: render it, but never dispatch, resume, abandon,
repair, consume, or rewrite it.
One exception: a quarantined plan whose goal is abandoned may be retired by
moving the file unchanged to `docs/plans/finished/<YYYY-MM-DD>-<slug>.md` and
appending a `## Retirement` section. Frontmatter status, every record line, and
the classification must be byte-identical before and after; flipping status to
`finished` is prohibited because it relabels an unsettled family
`settled-terminal` and unlocks migration.

An unrelated fresh local goal may create a new plan file. An explicitly
continued same-domain terminal goal replaces only the current run at its stable
path and preserves append-only attempt history. Legacy records provide no
authority; external recovery requires live `ExternalAuthorityV1`. Never edit a
historical finished plan.

## Audit checks

Before claiming success, verify the exact path, closed frontmatter, one valid
current Plan-run line, append-only attempt history, repository/path/run identity,
status tuple, plan/source hashes, transaction read-back, owned commit path set,
the draft limit of one initial review plus one mandatory post-repair
verification, exactly two completion permits, the one-retry-never-two transport
state, historical-record isolation, and observed acceptance bindings. Never claim a wrapper ran
merely because its file exists,
claim review passed from reservation, translate stale output into state, or
translate persisted intent into external authority.

The `plan-manager` and `plan-reviewer` skill bodies are asserted verbatim by
`scripts/tests/plan-skill-phases.mjs --case bounded-workflows`, which also pins
the stable-step and bounded-review contract independently in this file,
`plan-manager`, `plan-reviewer`, `plan-workspace`, the dispatch reference,
reviewer wrappers, and the generated workspace template. Its mutation probes
remove each exact normative clause and require the named assertion to fail.

