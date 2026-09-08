# PLAN.md - plan record standard

The plan record is a GitHub issue. Its body carries the v3 byte contract and the
human-authored plan, review records live in issue comments, and GitHub fields
carry the machine state GitHub already owns. No plan markdown is tracked in the
repository.

Use direct implementation for one clear, reversible, low-risk local diff with one
bounded acceptance path; it creates no plan issue, reviewer, or automatic
commit. Use a canonical plan for explicit planning, multi-commit or
cross-repository work, cold handoff, an unresolved decision, a cross-subsystem or
public-contract change, security-sensitive or destructive work, or any
non-`local` effect.

## Skill routing

| Request | Owner |
|---|---|
| Maintain, bootstrap, migrate, audit, or explicitly refresh the workspace | `plan-workspace` |
| Run the six phases and archive the plan | main-context `plan-manager` |
| Return a readable pre-implementation verdict | internal `plan-reviewer` |

The two read-only wrappers are `plan-reviewer` and `code-reviewer`. Main context invokes `plan-manager` directly. A missing wrapper never creates another role. Dispatch one fresh read-only subagent with the same three-kind contract.

## Where the record lives

```text
GitHub issue #<number>   the plan body, labels, assignee, and state
GitHub issue comments    trusted plan-review and code-review records
GitHub timestamps        the record's creation and last-update times
docs/PLAN.md             this standard
docs/PLAN-QUEUE.md       optional discovery and priority view
docs/plans/finished/     frozen pre-GitHub history, not lifecycle input
```

The issue number is the plan identity. There is no slug, no plan path, and no
tracked plan file. `docs/plans/finished/` holds records written before the
lifecycle moved to issues. Humans may read it as history, but it is never a
source of truth. No lifecycle command or workspace migration operation opens,
inventories, parses, classifies, lists, or migrates it.

This backend is a deliberate trade. An issue body has no reviewable diff, no
`git blame`, no CODEOWNERS, and no presubmit validation, which is why large
open-source projects keep long design records in tracked files. The plan record
accepts that loss because the reviewed artifact here is the pull request diff,
and the plan issue is the tracker that points at it.

## Labels

The exact closed lifecycle label set is created idempotently with `gh label create --force`: `plan`, `plan:drafting`, `plan:planned`, `plan:ongoing`, and `plan:blocked`. The four open-work statuses are exactly `drafting`, `planned`, `ongoing`, and `blocked`; `finished` is not a writable status. The retired names `plan:finished` and `plan-scheduled` are deleted and are not created, parsed, or applied.

Every plan issue carries `plan`. Every open plan normally carries exactly one
phase label. GitHub enforces no exclusion between labels, so every status write
removes all phase labels present except the one it applies. An open issue with
no phase label derives `unlabelled` rather than guessing a status.

Phase labels describe open work only. Every read of a closed issue ignores all phase labels and derives completion from `stateReason`. `plan.mjs status` refuses a closed issue with a message containing `is closed; status applies to open plans`. `plan.mjs archive` and `plan.mjs retire` strip every phase label that a closing merge or earlier edit left behind.

A `plan`-labelled issue is a record, not an invitation. Another agent that finds
one does not start implementing it; only the manager run that owns the plan moves
it.

Topic labels such as `security`, `auth`, or `cookies` are project-owned: name
them on `plan.mjs labels --extra` to create them and on `plan.mjs new --label`
to attach them. The `plan` namespace is reserved: both commands reject `plan`
and every value beginning `plan:` so a caller cannot forge lifecycle state.

## Body contract

A v3 issue body starts with the exact line `<!-- plan-contract: v3 -->`, followed
by one blank line. The marker travels with the bytes whose format it identifies.
It is not a label: a triage-capable actor can delete a label independently, and
a body must not become unreadable because its separately stored version was
removed. Labels carry lifecycle classification, not body-format identity.

After the marker, the body contains exactly these eight `##` sections, once each and in this order: `## Goal`, `## Research`, `## Steps`, `## Acceptance`, `## Do not touch`, `## Open questions`, `## Review`, `## Verification Results`.

A v3 body has no frontmatter and contains no `---` fence anywhere. These keys are
retired and never live fields in a v3 record: `plan_contract`, `title`, `goal`,
`status`, `created`, `updated`, `assignee`, and `blocked_reason`.

`## Goal` still contains exactly one `Mode: plan-and-implement` or `Mode: plan-only` line. Mode stays in the body because GitHub has no field that owns this plan-specific choice. Once an open plan leaves `drafting`, `## Research` must no longer carry the template placeholder `_Not researched yet._`.
`## Review` contains exactly `_Review records are stored in issue comments._`;
review reports are not appended to the body.

A blocked plan carries its reason as the first content line of `## Open questions`.
Spell it `Blocked: <one-line text>`. Only a blocked plan may open that section
with `Blocked:`. No other body field stores the reason.

The body contains no absolute machine path. A plan is a cold handoff, and a path
from one machine is not portable. A plan body contains no U+2014 em dash character anywhere.

Every plan delivers a durable solution: fix the root cause and complete the cutover in one pass. Temporary fixes, stopgaps, workarounds, and solutions that schedule future maintenance are prohibited unless the user explicitly requested a temporary fix, and the plan records that request in `## Goal` or `## Open questions`. Reviewers treat an unrequested temporary fix as a finding: `goal_fit` in plan review, `Spec` in code review.

Contract classification is byte-driven and deliberately does not guess:

| Body evidence | Classification | Handling |
|---|---|---|
| First line is exactly `<!-- plan-contract: v3 -->`, followed by one blank line | record | Parsed as the current contract |
| Anything else | unreadable | Refused; no parser is attempted |

## GitHub-owned fields

The issue title owns the plan title. The one `plan:<phase>` label on an open
issue owns its status. The issue assignee owns the single-writer owner.
`createdAt` and `updatedAt` own the timestamps. The issue `state` together with
`stateReason` owns completion. The body does not duplicate any of those values.

One writer owns a plan issue at a time. `plan.mjs new` assigns the creating
login, and `plan.mjs claim <issue>` claims an unassigned plan or stays idempotent
for its current owner. Every mutating command refuses a plan assigned to another
login. Read-only commands never check ownership. Taking a plan from another
owner is a deliberate manual GitHub action; no lifecycle command transfers
ownership and there is no override flag.

## Plan tables

The Steps table uses this exact header:

```text
| # | Id | Task | Files | Depends | Effect | Status | Done when |
|---:|---|---|---|---|---|---|---|
```

`#` is the positive display number. `Id` matches `[a-z][a-z0-9_]{0,63}` and is unique. `Task`, `Files`, and `Done when` are non-empty. `Depends` is `-` or a comma-separated list of lower display numbers from the same table. `Effect` is exactly one of `local`, `probe`, `production_access`, `publish`, `push`, `release`, or `deploy`. `Status` is exactly one of `planned`, `in-flight`, `done`, `blocked`, or `skipped`; `done` and `skipped` are terminal. `Done when` names one observable proof and carries no "or STOP" clause. Step citations use `step:<id>` and resolve to a declared id.

Every Steps row must be terminal before the closing pull request merges. Once
that merge closes the issue as completed, the derived state is `finished` and
step mutation is limited to terminal repair of a closed-but-unarchived record.
Post-merge work belongs to a named follow-up plan.

No Steps `Files` cell names the plan's own issue reference. Writing lifecycle
state into the record is the CLI's job, not an implementation step.

When a step cannot complete, first run `plan.mjs step <issue> <step-id> blocked`, then record the reason with `plan.mjs status <issue> blocked --reason <text>`; the latter writes `Blocked: <one-line text>` as the first content line of `## Open questions` and applies `plan:blocked`.

The Acceptance table uses this exact header:

```text
| ID | Command | Expected |
|---|---|---|
```

Acceptance IDs are unique. `Command` and `Expected` are non-empty.

## Derived state and transitions

State is derived by this closed truth table:

| Issue state | Phase label / state reason | Derived status |
|---|---|---|
| `OPEN` | exactly one phase label | that phase: `drafting`, `planned`, `ongoing`, or `blocked` |
| `OPEN` | no phase label | `unlabelled` |
| `CLOSED` | `COMPLETED` | `finished` |
| `CLOSED` | `NOT_PLANNED` | `retired` |
| `CLOSED` | `DUPLICATE` | `duplicate` |

A closed issue's phase labels are absent for derivation even when GitHub still
returns them. Reopening returns the issue to `OPEN`; its status is again derived
only from the phase labels then present.

Phase lives in the labels, so a body alone carries no phase. `check <issue>`
enforces every rule. `check --file <path>` enforces only the rules that read the
body: it skips the phase-label rule, both `Blocked:` rules, and the filled-Research
rule instead of assuming a phase.

The legal open-status transitions are:

```text
drafting  -> planned | ongoing | blocked
planned   -> drafting | ongoing | blocked
ongoing   -> blocked
blocked   -> drafting | planned | ongoing
```

The `planned -> drafting` transition returns a plan to drafting after
substantive review repair. Completion and retirement are issue closure results,
not status transitions. `plan.mjs archive` does not close the issue;
`plan.mjs retire` closes it as not planned.

## Lifecycle commands

`plan.mjs` is plugin payload, not project payload. It ships inside the installed
`plan-lifecycle` plugin at
`skills/productivity/plan-manager/scripts/plan.mjs`. A project never vendors,
copies, or re-creates it, and an unresolvable tool means the plugin is not
installed. Never report it as a file missing from the repository. Resolve it
from the loaded `plan-manager` skill directory, or from the runtime plugin
cache. Run it with the repository root as the working directory, because it
resolves the target repository from that checkout's GitHub remote.

| Command | Semantics |
|---|---|
| `plan.mjs labels [--extra <name>]…` | Create or update the exact five-label lifecycle set with `gh label create --force`, plus any extra topic labels named on the command line. |
| `plan.mjs new --title <t> --goal <g> [--mode plan-and-implement\|plan-only] [--label <name>]…` | Create a v3 issue whose body starts with the marker, with labels `plan` and `plan:drafting`, and assign the creating login. |
| `plan.mjs claim <issue>` | Take single-writer ownership of an existing plan: assign the acting login, stay idempotent when it already owns the plan, and refuse when another login does. |
| `plan.mjs show <issue> [--body]` | Print the header strip, then `reviews: plan=<pass\|repair\|blocked\|none> code=<pass\|fixes-required\|blocked\|none>`. With `--body`, print only the body to stdout and send both metadata lines to stderr, header first. |
| `plan.mjs export <issue>` | Write the issue body verbatim to `plan-<issue>.md` inside the scratch directory `git rev-parse --git-path docks-review` resolves, creating it mode 0700 when missing, and print the absolute path. |
| `plan.mjs edit <issue> --file <path>` | Validate the file as the plan record and enforce the irreversible execution-state freeze: existing step state is immutable after work starts, and new rows are append-only `planned` work on open plans. Refuse on any failed check, instruct re-export, replace the issue body, and print the header strip and changed lines. |
| `plan.mjs check <issue \| --file <path>>` | Validate a v3 record and print the pass result. |
| `plan.mjs status <issue> <status> [--reason <text>]` | Validate and apply one open-status transition, then replace all phase labels with the target phase label. Refuse closed issues. |
| `plan.mjs step <issue> <step-id> <status>` | Rewrite one Steps `Status` cell after checking dependencies; require an open `ongoing` plan, or a `finished` plan when the target status is terminal (`done` or `skipped`) for repair. |
| `plan.mjs list [--status <s>]` | Print `<status>\t#<issue>\t<title>` for every issue labelled `plan`, deriving `unlabelled`, `finished`, `retired`, and `duplicate` rather than reading them from the body. |
| `plan.mjs next` | Print startable open plans, using the queue when it is present and valid. |
| `plan.mjs archive <issue>` | Verify terminal steps, the latest trusted code-review result (with legacy body fallback only when no trusted comment record exists), completed closure, and an eligible merged closing pull request; strip stale phase labels and write no status. |
| `plan.mjs retire <issue> --reason <text>` | Close the issue as not planned and strip all phase labels; completion derives as `retired`. |

Legal step transitions are `planned → in-flight | done | blocked | skipped`, `in-flight → done | blocked | skipped`, and `blocked → in-flight | done | skipped`.

## Archive verification

`plan.mjs archive` is a verifier, not a writer of lifecycle state. It requires
all Steps rows to be terminal (`done` or `skipped`), the latest trusted
well-formed code-review comment to carry `Code-review: pass`, and an issue
already closed as completed by an eligible merged pull request. It accepts an
exact legacy body line `Code-review: pass` only when no trusted well-formed
code-review comment exists. It writes no status. On success it removes any
stale phase label and prints `plan #<n> finished (closed by <url>)`. A pass may
carry advisory `MEDIUM` and `LOW` finding lines; an unfixed `CRITICAL` or `HIGH`
keeps a plan from archiving.

The verifier reads the issue's `closedByPullRequestsReferences` with
`excludeUserLinked: true`. It accepts only keyword-linked merged pull requests.
A manually linked pull request never proves a landing.

Either proof suffices, and the verifier tries them in one order. It first looks
for an eligible keyword closer. Only when that connection holds none does it
examine the latest closure. A pull-request closer is itself verified as a
candidate closing pull request. A commit closer supplies its
`associatedPullRequests`. Any other latest closer supplies no fallback proof.
An ineligible keyword reference, such as one still open, therefore never hides
a valid closure proof. The verifier accepts only merged pull requests whose
base matches that repository's default branch.

Earlier closure events do not count. An issue closed by a commit, reopened, then
closed by hand has no commit proof. A commit pushed straight to the default
branch has no associated merged pull request. Archive refuses both cases.
It never performs the merge or closes the issue.

## Writing the record

A plan-issue write is a read-modify-write, and the GitHub API offers no
precondition for it. Every mutating command re-reads the issue body immediately
before the edit, refuses when it differs from the body it read, and re-reads
after the edit to confirm the pushed bytes.

Ownership narrows the writer set to one; compare-before-write narrows the
remaining window but does not close it, because the read and the edit are
separate API calls. A conflict is not an error to retry blindly: re-read the
record, re-apply the intent, and run `plan.mjs check <issue>` before continuing.

`step` requires an open `ongoing` plan, except that a `finished` plan accepts a
step mutation when the target status is terminal (`done` or `skipped`). This
repairs a closed-but-unarchived record without creating a new closure event.
Never reopen the issue for this repair: `archive` trusts only the latest
closure, so reopening would discard the eligible closure proof.

An export copy is a snapshot of one body revision, not a live view. The `step`
and `edit` commands rewrite body bytes. A status change also rewrites body bytes
when it adds or clears the blocked reason. These body writes supersede every
existing export.

`plan.mjs edit` requires the export digest in `<file>.origin` for every body
edit. It refuses a missing digest, an unreadable digest, or a digest for a
superseded body. These refusals prevent stale or unverified copies from
replacing recorded state.

After validation, `edit` refreshes the digest before it writes the remote body.
A local digest failure fails closed before the remote write and requires one
re-export. The `claim`, `archive`, and `retire` commands do not rewrite body
bytes. A successful phase-only status change writes labels only and leaves the
body and digest valid. The guard compares body bytes, not the issue timestamp.

Once work starts (irreversibly: when the current phase is neither `drafting` nor
`planned`, or label events show that `plan:ongoing` was ever applied), `edit`
preserves every existing Steps row's Status, Effect, Depends, display number,
and presence byte-for-byte; new rows must be appended after every existing row,
must start `planned`, and are refused on closed plans; after that boundary, only
`plan.mjs step` writes step state.
Before this boundary, a `drafting` or `planned` plan with no historical
`plan:ongoing` event may edit Steps freely. On a closed plan, post-merge step
mutation remains limited to terminal repair; new work requires a follow-up plan.

Re-export immediately before every body edit. Edit the export. Run
`plan.mjs check <issue>`. Delete the export and its `.origin` sidecar. Never
carry an edit across an intervening body write.

## Reading the record

Render a plan body verbatim only when the user names that plan and asks to see
it. After a write, report the one-line header strip and the changed lines only;
a write never re-renders the body.

A read-only reviewer never fetches the record. Before dispatch the manager runs
`plan.mjs export <issue>` and passes the issue number together with the printed
absolute path; a reviewer opens exactly that path and never a hardcoded one,
because the scratch directory is `.git/docks-review/` in a plain clone and a
worktree-private directory in a linked worktree. The export is scratch: never
tracked and never the record. The manager may edit its own export as the staging
file for a body write; a reviewer never edits it. Every dispatch re-exports
first, so a reviewer always reads the current record rather than a half-staged
edit.

The header strip is `#<issue> · <status> · <title> · <url>`. `show` prints
`reviews: plan=<pass|repair|blocked|none> code=<pass|fixes-required|blocked|none>`
on the next line. With `show --body`, the record alone goes to stdout and both
metadata lines go to stderr, header first.

## Review records - one issue comment per reviewer report

`## Review` is a static pointer, not a review log:

```markdown
_Review records are stored in issue comments._
```

Before every dispatch, the manager runs `plan.mjs export <issue>` and passes the
printed absolute path; the reviewer reads the export path the manager supplies.
For code review, the manager also supplies a fresh complete-candidate diff.

The reviewer returns exactly one markdown block. The manager posts that whole
block as one issue comment without editing it. The two exact shapes are:

```markdown
### Plan review - <YYYY-MM-DD>
Plan-review: pass|repair|blocked
- [goal_fit] `## Steps` row 4 - the step removes the validator without replacing it - add the replacement before removal
```

```markdown
### Code review round <n> - <YYYY-MM-DD>
Code-review: pass|fixes-required|blocked
- HIGH · Security · plugins/x/y.mjs:41 - user input reaches a shell command unquoted - pass an argument array
```

A well-formed record occupies the whole comment. It has the matching heading,
then exactly one verdict line, then zero or more nonblank finding lines. Extra
prose, multiple records, a missing heading, or an invalid verdict makes the
comment ineligible. `Plan-review:` is exactly `pass`, `repair`, or `blocked`. Plan-review findings use
`- [goal_fit|research_gap|security_risk] <locator> - <defect> - <fix>`.
`Code-review:` is exactly `pass`, `fixes-required`, or `blocked`. Code-review
findings use `- <CRITICAL|HIGH|MEDIUM|LOW> · <Bug|Security|Performance|Maintainability|Spec> · <locator> - <defect> - <fix>`.

A record is trusted only when the issue has exactly one assignee and the
comment's author login equals that assignee. For each review kind independently,
the latest trusted well-formed comment wins, ordered by `createdAt` with API
order as the tie-break. Foreign-authored, malformed, and superseded comments
never establish current review state. A legacy verdict in the body is consulted
for one review kind only when there is no trusted well-formed comment record of
that kind.

A code-review `pass` means no `CRITICAL` or `HIGH` finding stands unfixed; it
carries only advisory `MEDIUM` and `LOW` lines, or none. After a pass, record
each advisory as follow-up work and do not change reviewed bytes; advisory
findings never trigger another review. `fixes-required` names at least one
evidenced `CRITICAL` or `HIGH` defect. A `blocked` verdict has at least one
finding line.

A plan-review finding is exactly one of `goal_fit`, `research_gap`, or
`security_risk`; nothing else is a finding. A sufficient plan passes.

Both review phases run at most five rounds. Each round uses a fresh plan export;
each code-review round also uses a fresh complete-candidate diff. On rounds 1
through 4, a `repair` or `fixes-required` verdict requires every reproduced or
named finding to be fixed, followed by a fresh export or diff and a fresh
review. A repair that changes no relevant bytes is no progress. A finding
repeated in the next round survived its fix. Either condition stops the loop,
as does `repair` or `fixes-required` in round 5; there is no sixth-round repair.

A plan-review `blocked` verdict routes its user-only decision through
`## Open questions` and `ask`; the verdict alone is not a lifecycle block. A
technical code-review `blocked` verdict stops immediately. After implementation
has started, a technical block or any terminal repair failure requires the
manager to commit and normally push all current work to the verified linked
branch before recording the blocker, setting the plan `blocked`, and stopping.

## Phases

1. **Decide.** Phase 1 asks exactly one question with exactly three options, in this order and wording: `Plan and implement now`, `Plan only, stop at planned`, `Implement directly` - and skips the question only when the request already settles the mode.
2. **Draft.** Create the plan issue, write the goal and research hypothesis, and keep provisional Steps and Acceptance tables while status remains `drafting`.
3. **Research.** Verify repository facts and external claims, record their sources, choose the durable fix, bind the exact files, complete Acceptance, pass `plan.mjs check`, and set the plan `planned`.
4. **Plan review.** Run up to five rounds from fresh exports. Post each reviewer block as one issue comment. Fix reproduced findings and dispatch a fresh review; stop on pass, no progress, a finding surviving its fix, or `repair` in round five. Route every `blocked` user-only decision through `## Open questions` and `ask`, including in round five. A plan-only run stops at `planned` only after plan review passes.
5. **Implement.** Set the plan `ongoing`, verify and check out its GitHub-linked branch before changing implementation bytes, move each step through its legal states, and record real Acceptance output in `## Verification Results` before the closing merge.
6. **Code review.** Run up to five rounds from fresh complete-candidate diffs and fresh plan exports. Post each reviewer block as one issue comment. Fix every critical and high finding and dispatch a fresh review; stop on pass, no progress, a finding surviving its fix, a technical block, or `fixes-required` in round five. Before recording a technical block or terminal repair failure, commit and normally push all current work to the linked plan branch. Every step must be terminal and code review must pass before the closing merge; archive verifies those facts afterward.

Build the review diff from the complete candidate pull request, not only the
dirty worktree. Resolve and fetch the repository default branch, then compute
`<merge-base>` with `git merge-base <default-remote-ref> HEAD`. Cover one net
tracked candidate with `git diff <merge-base> -- <changed paths>`. Add one
`git diff --no-index /dev/null <path>` hunk for each untracked path.
`git status --porcelain` still names dirty paths. Name every changed path that
no Steps `Files` cell mentions in the review request.

After pull-request creation, record `headRefOid` and compare the changed paths
and hunks from `gh pr diff` with the reviewed net candidate. Any mismatch
invalidates the pass and blocks merge.


A step whose `Effect` is not `local` requires an in-session `ask` confirmation
immediately before it runs; when `ask` is unavailable the step is set `blocked`
and `Blocked: <unconfirmed effect>` is recorded first in `## Open questions`.

Routine plan issue publication is authorized by the settled mode and needs no
repeated repository picker. Routine linked-branch creation, commits, and normal
pushes are authorized when a settled `plan-and-implement` run enters phase 5.
Immediately after setting the plan `ongoing`, resolve the target repository's
`nameWithOwner` and `defaultBranchRef.name`.

Before any branch checkout, and specifically before any `gh issue develop
--checkout`, require `git status --porcelain` to be empty. If it is dirty, never
stash, move, or commit the ambient work. Set the plan `blocked` and name the
dirty paths, or continue only in an authorized clean worktree.

Pass `--repo <nameWithOwner>` to every `gh issue develop` call. First run
`gh issue develop <issue> --repo <nameWithOwner> --list`. If it reports a
linked branch, verify that branch belongs to the resolved repository, fetch it,
and check it out. Otherwise run `gh issue develop <issue> --repo
<nameWithOwner> --base <default-branch> --checkout`. After either path, verify
that the checked-out branch is the issue's linked branch.

After any list, create, fetch, or checkout failure, re-run the repository-scoped
`--list`. If it reports a linked branch, verify that branch belongs to the
resolved repository, fetch it, and check it out. If recovery cannot verify and
check out a linked branch, record the blocker, set the plan `blocked`, and stop.
There is no local or unlinked fallback, and implementation never starts on an
unverified branch.

## Landing

Work lands through a pull request whose body carries `Closes #<issue>` and whose
base is the repository default branch.

After `Code-review: pass`, commit and push any remaining reviewed bytes, then
create or update one pull request carrying `Closes #<issue>` and targeting the
repository default branch. This landing work needs no additional prompt.

Never treat an empty first checks result as success. Retry
`gh pr checks --json name,bucket` at most 12 times with a 10-second delay until
checks appear. If required checks exist, run
`gh pr checks --watch --required`; if CI checks exist but none are required,
run `gh pr checks --watch` to wait for all reported CI. Any failed check blocks
merge. If no checks appear, continue only when repository inspection confirms
that no pull-request CI is configured; otherwise leave the pull request open
with a named no-checks blocker and do not show the merge prompt.

When the checks policy passes and GitHub reports the pull request mergeable,
ask immediately with exactly two options: `Merge now` or
`Leave pull request open`. Merge only on that fresh answer. If the user
declines, or `ask` is unavailable, leave the pull request and the issue open
and report the pull request URL. Never auto-merge, force-push, bypass branch
protection, or merge on a stale or assumed answer.

Immediately before merge, re-read `headRefOid` and `gh pr diff`. If the head SHA
or diff changed, block merge. Invoke `gh pr merge` with
`--match-head-commit <reviewed-head-sha>` and the repository's configured merge
strategy only after the fresh `Merge now` answer.

Only the pull request that lands the completed work carries `Closes #<issue>`.
A partial pull request carries plain `Refs #<issue>`. `archive` verifies the
merged result rather than causing it. A plan that never lands is retired, not
archived.

## Portability

Cite repository-relative paths only; acceptance rows run from the repository root and carry no `cd <absolute path>` prefix. A cross-repository reference names the other repository by its portable identifier, such as `DocksDocks/docks`, never a local checkout path. A cross-repository closing keyword is written `OWNER/REPO#<issue>`.

## Queue

`docs/PLAN-QUEUE.md` is optional and classification-neutral. Its table is `| Stage | Plan | Depends on | Why |`, with `Plan` holding the issue number. An empty `Depends on` cell is `-`. A row is eligible only when its full direct and transitive dependency closure is finished. Stages give deterministic priority. The queue is a discovery and prioritization view only and grants no lifecycle, review, mutation, or external-effect authority. A workspace without it stays valid.

A `Plan` cell that is not a positive issue number names a frozen pre-GitHub
record. Such a row, and any row depending on it, is skipped rather than treated
as a malformed queue.
