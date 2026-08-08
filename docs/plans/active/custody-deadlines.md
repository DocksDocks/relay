---
title: Stop slow-but-alive peers becoming retained custody faults
goal: Widen or bound the five custody deadlines that turn a merely slow local peer into a retained fault, and prove each change with a row that is red before it.
plan_hash_mode: status-excluded-v1
status: blocked
created: "2026-08-08T04:09:34+00:00"
updated: "2026-08-08T13:54:24.415+00:00"
started_at: "2026-08-08T05:36:24.808+00:00"
finished_at: null
blocked_reason: "Completion review invocation 2 of 2 returned two further findings, so the permit budget is spent and this run is terminal. Every finding across both rounds was reproduced and FIXED, and the fixed code shipped: the implementation is 30c1a6d404d14e3416f6a234c3921f6cc5882aae. The run is blocked on ledger budget, not on defect. Round 1: F1, raising GRACEFUL_STOP_DEADLINE to 5 s inverted an accidental invariant, because the supervisor acknowledges Quiesce only after graceful_stop_and_wait_empty returns while the client waited 2250 ms - repaired with STOP_EXCHANGE_DEADLINE derived from STOP_AND_EMPTY_BUDGET. F2, widening RUNTIME_COMMAND_IO_DEADLINE to 10 s made the guardian loop block uninterruptibly so a stalled client could fence a healthy peer - repaired by reading and writing in HEARTBEAT_INTERVAL slices that drive the heartbeat between them. Round 2: F3, the same inversion one layer out, because the client abandoned a stop after 10 s while the guardian may validly take 17.25 s - repaired with RUNTIME_STOP_CLIENT_IO_DEADLINE, derived from STOP_EXCHANGE_DEADLINE. F4, the crate-map taxonomy still claimed CONTROL_EXCHANGE_DEADLINE covered all nine reply sites and omitted the new stop constants - corrected to seven sites plus classified rows for STOP_EXCHANGE_DEADLINE, STOP_AND_EMPTY_BUDGET and RUNTIME_STOP_CLIENT_IO_DEADLINE. Acceptance stands at 30 of 31 rows. A6 alone fails, expecting 10 and observing 11, because the F1 repair added the derivation line that IS the fix; no single expected value could be correct both before and after it. A7, A8 and A26 independently prove what A6 was written to prove. A successor run needs only exact current-user replacement authority and two edits: correct A6 to 11, and add rows pinning STOP_EXCHANGE_DEADLINE and RUNTIME_STOP_CLIENT_IO_DEADLINE. No decision is outstanding and no defect is known. Full evidence is in Verification Results."
blocked_since: "2026-08-08T13:54:24.416+00:00"
assignee: null
tags: [custody, deadlines, reliability, linux]
affected_paths:
  - docs/crate-map.md
  - src/channel.rs
  - src/tests/workspace_lease_process.rs
  - src/workspace.rs
  - src/workspace/custody.rs
  - src/workspace/platform/linux.rs
  - src/workspace/repository_gate.rs
  - test/fixtures/reentry-inventory.json
  - test/fixtures/rust-test-inventory.json
  - test/rust-test-inventory.mjs
related_plans: []
---

## Goal

A workspace peer that is alive but descheduled must not be judged dead. Five wall-clock deadlines in
the custody path expire on load alone, and four of the five escalate into a retained custody fault
that only an operator can clear by running `relay workspace recover`.

Every bound this plan touches is **widened or given an explicit total**, never deleted. A deleted
ceiling on a synchronous path is a worse failure than a premature error, because it hangs and leaks
whatever child the expiry was reaping.

This plan carries one further obligation the goal's four previous attempts failed: every step owns at
least one acceptance row that is **red before the step and green after it**. A row that is green on
both sides is labelled as a regression guard and never counted as that step's proof.

## Context & rationale

This goal was first drafted in `DocksDocks/docks` as
`docs/plans/finished/2026-08-07-session-relay-custody-deadlines.md` and reached a terminal
`review_failed` after four runs. That record cannot move between repositories, because run identity
is `repository_id + plan_path + run_id`; it directed this goal to be re-drafted here under the same
`goal_id`. The crate has since been extracted into this repository and made Linux-only, so every path
and every line number in that record is stale. Each number below was re-derived from this tree.

Three of the archived plan's nine steps are absent here, and none of them is a silent drop. Its first
step asked for a runner that executes the crate's library unit tests: that runner now exists as the
`unit` case of `test/rust-test-inventory.mjs`, with a measured floor at
`test/rust-test-inventory.mjs:37`. Its last step asked for a correction to a doubly stale inventory
row, whose values were already corrected during the extraction, so `docs/crate-map.md:135` now records
the observed counts. Its second step asked the docks gate to fail on a check that did not execute;
that gate no longer exists in this repository, and the equivalent hole in `scripts/gate.mjs` is a
declared non-goal for the reason recorded under Environment.

Severity comes from where expiry lands. `src/supervisor.rs:863` `retain_runtime_fault` is divergent
(`-> !` at `:867`) and is reached from **fourteen** sites in the release protocol that begins at
`src/supervisor.rs:692` — `:699`, `:714`, `:730`, `:738`, `:742`, `:747`, `:761`, `:769`, `:775`,
`:780`, `:790`, `:799`, `:804`, `:814` — with the `quiesce_failed` literal at `:727`. So one 750 ms
miss on a custody acknowledgement becomes a fault requiring operator recovery.

The crate already documents this exact defect class. `src/tests/workspace_lease_process.rs:1700-1712`
records an earlier instance where one 500 ms `Instant` was spent on two waits, handing the empty proof
a remainder that reached zero under contention and turning a healthy shutdown into `quiesce_failed`.
That comment is the precedent this plan generalises: a deadline that bounds *a local peer we ourselves
spawned* is a liveness guess, and every such guess is wrong under contention.

The five rows in scope, all reachable from ordinary `relay workspace` verbs:

| Row | Site | Value | Why it is a false negative |
|---|---|---|---|
| 1 | `src/workspace.rs:5368` | 3 s, inline | Broker readiness. There is **no named constant**: the budget is the literal `Duration::from_secs(3)` and the diagnostic hardcodes the words `within three seconds` at `:5417`. The parent already distinguishes "exited" from "slow" via `child.try_wait()` at `:5373` and `:5403`, so this fires **only** when the child is confirmed alive. |
| 2 | `src/workspace/custody.rs:18` | 750 ms | `HEARTBEAT_FENCE_AFTER` is borrowed as a one-shot reply timeout at **nine** sites in eight functions: `command` `:505`, `finish_bootstrap` `:727`, `worker_prepared` `:754`, `activate` `:798`, `accept_bootstrap_fault` `:840` and `:851`, `confirm_empty` `:915`, `next_admitted` `:1047`, `wait_ack` `:1079`. It is not a per-receive budget. `heartbeat()` (`:527-555`) never reads it: it receives on `HEARTBEAT_INTERVAL` (250 ms, `:17`) at `:536` and fences after three consecutive misses at `:549-550`, so 750 ms is the **implicit aggregate** of that three-strike check. The nine sites spend a whole liveness aggregate on one reply. |
| 3 | `src/workspace/repository_gate.rs:15` | 3 s | `GATE_TIMEOUT` at its sole use `:150` is a fixed budget for lock contention regardless of how many writers are queued, so it degrades with concurrency by construction. It is also the **only** bound on the `flock(LOCK_EX\|LOCK_NB)` loop that starts at `:151` and calls `flock` at `:154`. |
| 4 | `src/workspace/platform/linux.rs:61` | 500 ms | `GRACEFUL_STOP_DEADLINE` bounds a possibly-descheduled root leaving after SIGTERM. Sole use `:776`, inside `graceful_stop_and_wait_empty` (`:770-778`). |
| 5 | `src/workspace.rs:41` | 2000 ms | `RUNTIME_EXCHANGE_DEADLINE` bounds BOTH client socket options as a one-shot pair with no retry: `set_write_timeout` (`SO_SNDTIMEO`) at `:6788` and `set_read_timeout` (`SO_RCVTIMEO`) at `:6791`, with a single `read_to_end` at `:6802`. It is **genuinely fused**: also reused at `:4682` as a post-SIGKILL-fence pidfd wait, which is a real verdict and out of scope, and interpolated into the operator diagnostic at `:6810`. |

### Readiness as an event rather than a clock {mechanism}

Row 1 replaces one literal with one named, overridable constant rather than deleting a bound.
`broker_readiness_pair` (`src/workspace.rs:85`) builds a `UnixStream::pair()` and then deliberately
sets the reader non-blocking at `:91`; that flag is the sole reason the loop at `:5369-5412` busy-polls
and needs a budget at all. Polling the fd for `POLLIN|POLLHUP` returns the instant the broker writes,
and `POLLHUP` already carries the `Closed` verdict. `custody::wait_readable` is the in-crate model.

What the event wait must NOT do is drop the ceiling. `src/workspace.rs:5413-5419` is the only path
that bounds a broker which stays alive and never publishes readiness: it kills the child, reaps it,
takes its stderr and returns a diagnostic. `POLLIN`, `POLLHUP` and the periodic `child.try_wait()` are
each never satisfied in that case. The wait is inline in `start_git_broker` (`:5270`), called
synchronously from the workspace-start path, and the repository gate is dropped one line before that
call, so **nothing outside imposes any deadline**. Two sibling arms perform the same cleanup and must
also survive: `Closed` at `:5385-5393` and the error arm at `:5395-5400`.

The override mechanism is decided here rather than left open. `env_ms` already exists at
`src/channel.rs:33-40` with signature `fn env_ms(name: &str, default: u64) -> Duration`, but it is
**private to that module** and today serves exactly two constants (`RELAY_CHANNEL_REGISTER_TIMEOUT_MS`
and `RELAY_CHANNEL_POLL_MS`, wired at `src/channel.rs:210-214`). This plan widens its visibility to
`pub(crate)` and reuses it. It does not introduce a second override helper, because two spellings of
one mechanism drift.

The same reasoning applies to row 3, and it is the sharper trap. `src/watch.rs` holds a capped backoff
that caps only the *interval* between attempts and imposes **no total ceiling** — it retries for the
life of the watch loop. Copying it verbatim into `repository_gate` would delete the only bound on a
synchronous gate acquisition. Row 3 therefore adds an explicit total budget alongside the per-attempt
one.

Row 5 is the one row whose fix is not obvious, because retrying `quiesce`, `terminate`, `close_lease`
or `closed_committed` is only safe if the runtime protocol tolerates a duplicate request.
step:runtime_exchange_retry settles that from the protocol itself before changing anything, and its
failure action is to introduce a separate client bound rather than guess. Note the responder has its
own, separate budget: `RUNTIME_COMMAND_IO_DEADLINE` (200 ms, `src/workspace.rs:39`, used at `:6949`
and `:6952`) bounds the server end of the same socket, so widening only the client would leave a
descheduled custodian that cannot *write* within 200 ms still failing. step:runtime_exchange_retry
therefore widens both ends, and A31 is the row that proves the responder moved with the client.

### What can and cannot be injected {seams}

The regression test in step:load_regression uses only mechanisms that exist today. The crate's
test-only seams are `INTEGRATION_FAULT_POINTS` (`src/workspace.rs:2240-2247`),
`SESSION_RELAY_TEST_CLEANUP_FAULT` (`:3101-3102`), and the `RELAY_TEST_SUPERVISOR_*` /
`RELAY_TEST_CONTROL_READY_LATCH*` / `RELAY_TEST_WATCHDOG_*` family in `src/supervisor.rs`. Every
workspace seam injects an **error**, not a delay, and every delay seam sits on supervisor startup or
cancellation rather than on custody.

Two of the three delays this plan needs are therefore reachable without adding a production seam:

- **A held repository gate.** `repository_gate` takes a real `flock` on a real file, so a test holds it
  by opening that file and taking `LOCK_EX` itself. No crate change is required.
- **A root that leaves slowly after SIGTERM.** A `#!/bin/sh` fixture with `trap 'sleep <d>; exit 0' TERM`
  exits `<d>` after the signal, which is what discriminates a stop budget. The crate's existing
  fixtures at `src/tests/lifecycle_supervisor.rs:486`, `:571` and `:701` use `trap '' TERM HUP INT`
  instead, which ignores the signal forever; that shape proves a kill escalation, not a budget, and
  must not be reused here.

The third, a late custodian acknowledgement, has **no seam**: the custodian acknowledgements in
`src/supervisor.rs` are unconditional. step:load_regression therefore does not attempt it, and
creating that seam would mean editing `src/supervisor.rs`, which this plan does not declare.

## Environment & how-to-run

Linux with cgroup v2 delegation and a systemd user session; Rust toolchain per `rust-toolchain.toml`.
All commands run from the repository root.

Three integration cases — `workspace_identity`, `workspace_lease_process`, and
`workspace_coordination_process` — require an owned cgroup. The harness provisions one per case
(`test/rust-test-inventory.mjs:184-193`); a bare `cargo test` for those targets fails on a missing
`SESSION_RELAY_TEST_CGROUP_ROOT`, which is a pre-existing environment requirement and not a defect
introduced here.

The harness does NOT fail loudly when it cannot obtain that delegation. Off CI it prints
`SKIP rust_test_inventory case=<name> reason=...` and exits 0 (`test/rust-test-inventory.mjs:214-220`);
it hard-fails only when `GITHUB_ACTIONS` is `true` (`:207-212`). `scripts/gate.mjs` never inspects
child output — it fails only on a non-zero child status (`scripts/gate.mjs:331`) — so off CI the gate
can exit 0 with those three cases unexecuted. **That hole is a declared non-goal of this plan**, for two
reasons: no row here can be satisfied by a skip, because every test-bound row counts a `PASS` line that
the skip path never prints, and A23 proves this host has delegation. Fixing the gate is a separate
defect with a separate blast radius — it changes how every check in the repository reports — and
bundling it here would put `scripts/gate.mjs` in `affected_paths` for a change the custody deadlines do
not need. Confirm delegation before trusting any custody result:

```bash
systemd-run --user --scope -p Delegate=yes --collect --quiet -- true
```

This repository's gate has no memo or cache short-circuit, so no row needs to disable one.

```bash
node test/rust-test-inventory.mjs --case unit
node test/rust-test-inventory.mjs --case workspace_lease_process
node test/reentry-inventory.mjs
node scripts/gate.mjs
```

Slowness is reproduced by INJECTING a deterministic delay at the mechanism under test — a held
repository gate, a worker root that ignores SIGTERM — never by loading the host. Do not run CPU
spinners on this repository. Synthetic host load makes the result depend on the runner's spare
capacity, and on a small box it starves the very processes under test.

**Observe each acceptance row on an otherwise idle host.** This is measured, not cautionary. While
establishing the baselines below, `node scripts/gate.mjs` exited 0 twice when it ran alone, and exited
1 once when the same command ran immediately after `--case unit` and `--case workspace_lease_process`
in the same shell. Nothing in the tree differed between those runs. That failure is itself an instance
of the defect class this plan fixes, so it is evidence rather than noise — but it also means a row
observed while another suite is still settling proves nothing. Let the host settle between rows.

**The reentry census is a derived artefact of three of these steps.**
`test/fixtures/reentry-inventory.json` freezes every syscall and signal site in the crate, so
step:broker_event_wait adding a `poll()` and step:gate_backoff restructuring the `flock` loop both move
it. Regenerate it with `node test/reentry-inventory.mjs --generate` and never hand-edit it; A24 is the
row that catches a forgotten regeneration.

## Steps

| # | Id | Task | Files | Depends | Effect | Status | Done when / failure action |
|---:|---|---|---|---|---|---|---|
| 1 | broker_event_wait | Replace the inline 3 s busy-poll with an event wait, without removing the ceiling. Drop `set_nonblocking(true)` at `src/workspace.rs:91`, wait via `poll()` for `POLLIN\|POLLHUP`, and keep a coarse periodic `child.try_wait()` so a broker that dies without closing its fd is still detected. Introduce exactly one named wall-clock ceiling, `BROKER_READINESS_DEADLINE`, raised well above any observed publish latency and overridable through `env_ms`, which this step widens from private to `pub(crate)` in `src/channel.rs`. Make the expiry diagnostic interpolate the constant instead of hardcoding `within three seconds`. Preserve all three cleanup arms in effect — `Closed` `:5385-5393`, error `:5395-5400`, and expiry `:5413-5419` — each of which kills the child, reaps it, takes its stderr and returns a distinguishable diagnostic. Add the discriminating unit test `broker_readiness_waits_on_event_and_still_expires` to `workspace::tests`, proving BOTH halves of this step: that the wait returns as soon as readiness is published rather than spinning to the ceiling, and that a broker which stays alive and never publishes is still killed, reaped, and reported through the expiry arm. A25 executes exactly that test, so this step cannot be satisfied by editing source text alone. | `src/workspace.rs`, `src/channel.rs`, `test/fixtures/reentry-inventory.json` | — | `local` | `planned` | A1, A2, A3, A4 and A25 all hold, and A5 and A24 still pass. A25 is the behavioural row; the rest are source guards. Failure action: if `poll()` cannot observe the pair without the non-blocking flag, keep the flag, raise only the constant, and record why in the step's notes rather than deleting the ceiling. |
| 2 | custody_forgiveness | Stop the nine one-shot reply receives from borrowing the heartbeat liveness aggregate. Add `CONTROL_EXCHANGE_DEADLINE = 3 * HEARTBEAT_FENCE_AFTER` (2250 ms) and use it at all nine sites: `command` `:505`, `finish_bootstrap` `:727`, `worker_prepared` `:754`, `activate` `:798`, `accept_bootstrap_fault` `:840` and `:851`, `confirm_empty` `:915`, `next_admitted` `:1047`, `wait_ack` `:1079`. Leave `ControlEndpoint::heartbeat` (`:527-555`) byte-identical: it receives on `HEARTBEAT_INTERVAL` and counts three strikes, and that counter is the liveness oracle. Express the new constant in terms of the fence so the relationship stays visible, which also leaves `HEARTBEAT_FENCE_AFTER` referenced exactly twice — its definition and that derivation. Add the unit test `control_reply_after_old_fence_is_accepted` to `workspace::custody::tests`, proving a reply arriving after the old 750 ms fence but inside the new 2250 ms deadline is accepted, and that a reply arriving after the new deadline still fails. A26 executes exactly that test. | `src/workspace/custody.rs`, `test/fixtures/reentry-inventory.json`, `test/rust-test-inventory.mjs` | — | `local` | `planned` | A6, A7 and A26 hold, and A8 and A9 still return 1. A26 is the behavioural row. Failure action: if any of the nine sites turns out to bound a fencing decision rather than a reply, leave that site on the fence, record which, and reduce A6's expected count to match in the same change. |
| 3 | gate_backoff | Replace the fixed single-attempt wait with capped exponential backoff between attempts AND an explicit total ceiling. `GATE_TIMEOUT` becomes the budget for one acquisition attempt; add `GATE_TOTAL_BUDGET` bounding the whole loop. The `src/watch.rs` model caps only the inter-attempt interval and never terminates, so copying it alone would delete the only bound on the `flock` loop at `src/workspace/repository_gate.rs:154` — that is forbidden. Preserve the `no mutation performed` guarantee on final expiry, and interpolate the constant into the expiry diagnostic rather than hardcoding a duration in prose. Add the unit test `gate_acquires_under_contention_and_expires_without_mutation` to `workspace::repository_gate::tests`, which holds the gate file from the test process, proves a second acquisition still succeeds once the holder releases inside the total budget, and proves that final expiry returns the no-mutation error rather than hanging. A27 executes exactly that test. | `src/workspace/repository_gate.rs`, `test/fixtures/reentry-inventory.json` | — | `local` | `planned` | A10, A11 and A27 hold, and A24 still passes. A27 is the behavioural row. Failure action: if a total ceiling cannot be expressed without restructuring the caller, keep one bound and raise it, and STOP rather than leaving the loop unbounded. |
| 4 | graceful_stop_budget | Raise the shipped `GRACEFUL_STOP_DEADLINE` default (`src/workspace/platform/linux.rs:61`, sole use `:776`) to a value justified by a measurement recorded in this step: sample the SIGTERM-to-exit interval for a runnable child enough times to see the tail, and set the default well above the observed maximum. The injectable variant already exists at `:794-799`, so no threading work is needed; the two callers that supply the default are `src/supervisor.rs:719` and `:928`. Measure on an unloaded host and label the value provisional; do NOT create synthetic CPU load to measure, since starving the box distorts the very interval being sampled. Do not alter the stop/empty budget split, and do not touch `EMPTY_DEADLINE` (`:59`), which serves three call sites. Record the sampled maximum as a named constant beside the deadline and add the unit test `graceful_stop_budget_exceeds_observed_sigterm_tail` to `workspace::platform::linux::tests`, asserting the shipped default is strictly greater than that recorded tail and strictly less than `EMPTY_DEADLINE`. That encodes this step's STOP condition as an executable assertion rather than prose. A28 executes exactly that test. | `src/workspace/platform/linux.rs`, `test/fixtures/reentry-inventory.json` | — | `local` | `planned` | A12 returns 0, A28 returns 1, and A13 still returns 1. A28 is the behavioural row. Failure action: if the sampled tail exceeds the empty budget, STOP — that would invert the split this plan promises not to touch. |
| 5 | runtime_exchange_retry | Determine from the custody runtime protocol whether `quiesce`, `terminate`, `close_lease` and `closed_committed` tolerate a duplicate request. If they do, add one bounded retry. If any does not, introduce a separate exchange-only bound and apply it to BOTH client socket options — `set_write_timeout` at `src/workspace.rs:6788` and `set_read_timeout` at `:6791` — so send and receive stay symmetric. Widening the read alone would leave the send budget at 2 s, which is the same asymmetry this plan flags for the responder-side constant. `RUNTIME_EXCHANGE_DEADLINE` is fused: its reuse at `:4682` is a post-SIGKILL-fence pidfd verdict and keeps its current value either way. Widen the responder budget in the same change: `RUNTIME_COMMAND_IO_DEADLINE` (200 ms, `src/workspace.rs:39`, used at `:6949` and `:6952`) bounds the server end of the same socket, so a client-only fix leaves a descheduled custodian that cannot write within 200 ms still failing. It is the same defect class as the client bound and not a fencing decision: fencing is decided by the heartbeat three-strike counter and by pidfd verdicts, never by this socket option. Give both ends the same budget so send and receive stay symmetric on both sides of the exchange. Whichever route is taken, add a unit test named exactly `runtime_exchange_bounds_send_and_receive_symmetrically` covering BOTH the client and responder bounds, raise the unit floor at `test/rust-test-inventory.mjs:37` to the new measured count, and record the idempotency finding in `docs/crate-map.md`. | `src/workspace.rs`, `docs/crate-map.md`, `test/rust-test-inventory.mjs` | — | `local` | `planned` | A14, A15, A16 and A17 hold, and A18 and A19 still return 1. Failure action: if the protocol is silent on duplicates, take the separate-bound route — never guess idempotency. |
| 6 | load_regression | Add one regression test named exactly `slow_peers_do_not_become_retained_custody_faults` driving a full workspace release with a deterministic delay INJECTED at each mechanism reachable from this target: a repository gate held by the test process through its own `flock`, and a worker root that HANDLES SIGTERM and then exits after a deliberate delay. The delay must be strictly greater than the old 500 ms bound and strictly less than the value step:graceful_stop_budget ships, so reverting that step makes this test fail. Express it from the shipped constant rather than a second literal. Do NOT reuse the `trap '' TERM HUP INT` pattern at `src/tests/lifecycle_supervisor.rs:486`: a root that ignores SIGTERM permanently cannot exit inside ANY finite stop budget, so it expires identically before and after the change and discriminates nothing. Measured: `trap 'sleep 1; exit 0' TERM` yields a 1056 ms SIGTERM-to-exit interval, which is the shape this stimulus needs. Assert no retained custody fault is produced. Do NOT attempt to inject a late custodian acknowledgement: no such seam exists, the acknowledgements in `src/supervisor.rs` are unconditional, and creating one would mean editing a file this plan does not declare. Regenerate the frozen inventory with `node test/rust-test-inventory.mjs --generate`; never hand-edit it. | `src/tests/workspace_lease_process.rs`, `test/fixtures/rust-test-inventory.json` | 1, 2, 3, 4, 5 | `local` | `planned` | A20 returns 1 and A21 returns 1. Failure action: this test injects exactly two stimuli, so its rollback set is exactly step:gate_backoff and step:graceful_stop_budget — reverting either must make it fail. It does NOT include step:load_regression itself, and it does NOT include step:broker_event_wait, step:custody_forgiveness or step:runtime_exchange_retry, which inject nothing here and are discriminated instead by A25, A26 and A14 respectively. If reverting either step in the rollback set leaves this test green, the test is not discriminating — fix the test, and STOP if it cannot be made to fail. |
| 7 | deadline_taxonomy | Record the deadline taxonomy in the crate map so a future author can tell a liveness guess from a safety bound, naming every constant this plan changed with its bucket and citing the safety bounds left untouched. Extend the existing section 6 (`docs/crate-map.md:145`) rather than adding a competing section. | `docs/crate-map.md` | 1, 2, 3, 4, 5, 6 | `local` | `planned` | A22 returns 1 and the map lists every changed constant with its bucket. Failure action: if a constant resists classification, record it as unclassified with the reason rather than forcing a bucket. |

## Acceptance criteria

| ID | Step | Command | Expected | Baseline now | Colour |
|---|---|---|---|---|---|
| A1 | 1 | `grep -c 'set_nonblocking' src/workspace.rs` | `2` | `3` | RED |
| A2 | 1 | `grep -cE '^const BROKER_READINESS_DEADLINE' src/workspace.rs` | `1` | `0` | RED |
| A3 | 1 | `grep -c 'within three seconds' src/workspace.rs` | `0` | `1` | RED |
| A4 | 1 | `grep -c 'pub(crate) fn env_ms' src/channel.rs` | `1` | `0` | RED |
| A5 | 1 | `grep -c 'fn broker_readiness_pair' src/workspace.rs` | `1` | `1` | GUARD |
| A6 | 2 | `grep -c 'CONTROL_EXCHANGE_DEADLINE' src/workspace/custody.rs` | `10` | `0` | RED |
| A7 | 2 | `grep -c 'HEARTBEAT_FENCE_AFTER' src/workspace/custody.rs` | `2` | `10` | RED |
| A8 | 2 | `awk '/fn heartbeat/,/^    }/' src/workspace/custody.rs \| grep -c 'HEARTBEAT_INTERVAL'` | `1` | `1` | GUARD |
| A9 | 2 | `grep -cF 'custody control deadline elapsed after {} ms' src/workspace/custody.rs` | `1` | `1` | GUARD |
| A10 | 3 | `grep -cE '^pub const GATE_TOTAL_BUDGET' src/workspace/repository_gate.rs` | `1` | `0` | RED |
| A11 | 3 | `grep -c 'no mutation performed' src/workspace/repository_gate.rs` | `1` | `1` | GUARD |
| A12 | 4 | `grep -cF 'Duration::from_millis(500)' src/workspace/platform/linux.rs` | `0` | `1` | RED |
| A13 | 4 | `grep -cE '^const EMPTY_DEADLINE: Duration = Duration::from_secs\(10\);' src/workspace/platform/linux.rs` | `1` | `1` | GUARD |
| A14 | 5 | `grep -c 'fn runtime_exchange_bounds_send_and_receive_symmetrically' src/workspace.rs` | `1` | `0` | RED |
| A15 | 5 | `grep -c 'UNIT_TEST_FLOOR = 137' test/rust-test-inventory.mjs` | `0` | `1` | RED |
| A16 | 5 | `node test/rust-test-inventory.mjs --case unit \| grep -c '^PASS rust_test_inventory case=unit'` | `1` | `1` | GUARD |
| A17 | 5 | `grep -c 'idempotent' docs/crate-map.md` | `1` | `0` | RED |
| A18 | 5 | `grep -cF 'RUNTIME_EXCHANGE_DEADLINE: Duration = Duration::from_secs(2);' src/workspace.rs` | `1` | `1` | GUARD |
| A19 | 5 | `grep -cF 'let deadline = Instant::now() + RUNTIME_EXCHANGE_DEADLINE;' src/workspace.rs` | `1` | `1` | GUARD |
| A20 | 6 | `grep -c 'fn slow_peers_do_not_become_retained_custody_faults' src/tests/workspace_lease_process.rs` | `1` | `0` | RED |
| A21 | 6 | `node test/rust-test-inventory.mjs --case workspace_lease_process \| grep -c '^PASS rust_test_inventory case=workspace_lease_process'` | `1` | `1` | GUARD |
| A22 | 7 | `grep -c 'liveness guess' docs/crate-map.md` | `1` | `0` | RED |
| A23 | 6 | `systemd-run --user --scope -p Delegate=yes --collect --quiet -- true` | `0` | `0` | GUARD |
| A24 | 1 | `node test/reentry-inventory.mjs >/dev/null 2>&1; echo $?` | `0` | `0` | GUARD |
| A25 | 1 | `cargo test --locked --lib -- --exact workspace::tests::broker_readiness_waits_on_event_and_still_expires \| grep -c 'test result: ok. 1 passed'` | `1` | `0` | RED |
| A26 | 2 | `cargo test --locked --lib -- --exact workspace::custody::tests::control_reply_after_old_fence_is_accepted \| grep -c 'test result: ok. 1 passed'` | `1` | `0` | RED |
| A27 | 3 | `cargo test --locked --lib -- --exact workspace::repository_gate::tests::gate_acquires_under_contention_and_expires_without_mutation \| grep -c 'test result: ok. 1 passed'` | `1` | `0` | RED |
| A28 | 4 | `cargo test --locked --lib -- --exact workspace::platform::linux::tests::graceful_stop_budget_exceeds_observed_sigterm_tail \| grep -c 'test result: ok. 1 passed'` | `1` | `0` | RED |
| A29 | 5 | `cargo test --locked --lib -- --exact workspace::tests::runtime_exchange_bounds_send_and_receive_symmetrically \| grep -c 'test result: ok. 1 passed'` | `1` | `0` | RED |
| A30 | 6 | `grep -cF '\"slow_peers_do_not_become_retained_custody_faults\"' test/fixtures/rust-test-inventory.json` | `1` | `0` | RED |
| A31 | 5 | `grep -cF 'RUNTIME_COMMAND_IO_DEADLINE: Duration = Duration::from_millis(200);' src/workspace.rs` | `0` | `1` | RED |

The `Step` column carries each step's display number, matching the `#` column of Steps. The stable
identifiers are step:broker_event_wait `1`, step:custody_forgiveness `2`, step:gate_backoff `3`,
step:graceful_stop_budget `4`, step:runtime_exchange_retry `5`, step:load_regression `6`, and
step:deadline_taxonomy `7`.

A Command cell writes a shell pipeline separator as `\|`, which is Markdown escaping for a table cell
and not part of the command: a bare `|` would end the cell in any GitHub-flavoured renderer. Read every
`\|` as a single `|` when running the row. The digest binds the authored cell, so the escape is inside
the plan hash by construction; the row is executable after that one substitution.

**The Baseline column is measured, not asserted.** Every value in it was produced by running that
exact command against this tree before drafting, so `RED` means the row provably fails today and
`GUARD` means it already passes and therefore proves no step. Twenty rows are RED — A1, A2, A3, A4, A6, A7, A10, A12, A14, A15, A17, A20, A22, A25, A26, A27, A28, A29, A30 and A31 — and 11 are GUARD.

Every step therefore owns at least one RED row, and every step that changes behaviour owns exactly one
row that EXECUTES a named test rather than reading source text:

| Step | Red source rows | Red behavioural row | Named test it executes |
|---|---|---|---|
| step:broker_event_wait | A1 A2 A3 A4 | A25 | `broker_readiness_waits_on_event_and_still_expires` |
| step:custody_forgiveness | A6 A7 | A26 | `control_reply_after_old_fence_is_accepted` |
| step:gate_backoff | A10 | A27 | `gate_acquires_under_contention_and_expires_without_mutation` |
| step:graceful_stop_budget | A12 | A28 | `graceful_stop_budget_exceeds_observed_sigterm_tail` |
| step:runtime_exchange_retry | A14 A15 A17 A31 | A29 | `runtime_exchange_bounds_send_and_receive_symmetrically` |
| step:load_regression | A20 | A30 + A21 | `slow_peers_do_not_become_retained_custody_faults` |
| step:deadline_taxonomy | A22 | none — documentation | none |

step:load_regression is the one step whose named test cannot be selected by a bare `cargo test`,
because its target needs a delegated cgroup that only the harness provisions. Its pair is equivalent
to per-test execution by the harness's own contract: `test/rust-test-inventory.mjs:198` requires the
live `cargo test --list` names to equal the frozen fixture exactly, and `:230-232` require the
executed count to equal that list with zero ignored and zero filtered. So A30, which asserts the name
is in the frozen list, plus A21, which is emitted only after those four assertions hold, cannot both
be green unless that exact test ran.

This is the property the archived run failed on: its step for row 5 had no red row at all, so an
implementation writing only prose satisfied it. The behavioural column is what closes the weaker
version of the same hole, where a step owns red rows that all read source text and can therefore be
satisfied without the described behaviour ever running. `step:deadline_taxonomy` is the sole
exception and is honest about it: it writes documentation, so its correctness is a reviewer judgement
and no command can stand in for one.

One discriminatory test per mechanism, rather than one test for all five: a single regression test
cannot discriminate a mechanism it does not stimulate, and no seam exists to delay a custodian
acknowledgement or a runtime exchange from inside an integration target.

What each row cannot prove:

- **A5, A8, A9, A11, A13, A16, A18, A19, A21, A23 and A24 are green before and after.** They are
  regression guards, not discriminators. Two of them do real work despite proving no step: A24 fails
  if a step moves a syscall site and leaves the frozen reentry census stale, and A23 states the cgroup
  delegation precondition without which A21 could pass by skipping.
- **A14 is a source row; A25 through A28 are its behavioural counterparts for the other four steps.**
  A29 does the same for step:runtime_exchange_retry, and A30 plus A21 do it for the one target a bare
  `cargo test` cannot select. Each runs exactly one test by its full module path, so a test that is
  written but never registered,
  or registered but filtered out, reports `0`. Measured today: the same command shape returns `1`
  against an existing test and `0` against an absent one, which is what makes these rows red now.
- **A16, A21, A24, A25, A26, A27, A28 and A29 compile or run test binaries, so they are load sensitive.** Environment requires
  them on a settled host; a failure must be re-observed alone before it is read as a regression.
- **A1 alone is not sufficient for step:broker_event_wait.** It counts a token file-wide, so it
  proves the flag was dropped somewhere; A5 stops it being satisfied by deleting the function, and A2
  plus A3 prove the named ceiling replaced the literal rather than removing the bound.
- **A6 and A7 are complementary.** A6 pins one definition plus nine uses; A7 pins that
  `HEARTBEAT_FENCE_AFTER` survives exactly twice, its definition and the derivation, which is what
  keeps the three-strike relationship visible. A8 proves `heartbeat()` still receives on the interval.
- **A9 guards a fused literal, not prose.** `heartbeat()` classifies a missed deadline by string
  prefix at `src/workspace/custody.rs:548`, so this row is what keeps the liveness oracle intact.
- **A14, A15 and A16 together make step:runtime_exchange_retry falsifiable under both routes.** The
  step chooses between a bounded retry and a separate symmetric bound, so no single source grep covers
  both; a named unit test does. A15 forces the floor to move, and A16 executes the suite, so a test
  that exists but does not run cannot satisfy the step.
- **A18 and A19 are anti-widening guards.** A18 pins the definition and A19 pins the post-fence use at
  `src/workspace.rs:4682`, so step:runtime_exchange_retry cannot widen the fenced verdict by swapping
  the identifier.
- **A17 and A22 are single-line greps.** They record that a finding and a taxonomy were written, not
  that either is correct; the reviewer judges the prose.
- **A20 and A21 are the pair that makes step:load_regression real.** A20 proves the named test exists;
  A21 proves the target it lives in executed and passed. Neither proves the test is discriminating —
  that is the step's own failure action, whose rollback set is exactly step:gate_backoff and
  step:graceful_stop_budget — the only two mechanisms this test injects. The other three deadline steps
  are discriminated by their own executing rows, A25, A26 and A29, not by this test.

How each row is judged:

| Binding | Meaning | Rows |
|---|---|---|
| `exit` | the command's exit status is compared against the expected value | A23 |
| `match` | the command's output is compared against the expected value | A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11 A12 A13 A14 A15 A16 A17 A18 A19 A20 A21 A22 A24 A25 A26 A27 A28 A29 A30 A31 |

## Out of scope / do-NOT-touch

These expire on a real verdict, not on load, and must keep their current strictness. Each was
confirmed present in this tree while drafting:

- `MANAGED_ATTACH_DEADLINE_MS` (`src/lifecycle.rs:21`, 4360 ms, used at `src/lifecycle.rs:1203` and
  `src/supervisor.rs:1663`) and `MANAGED_CANCEL_GRACE_MS` (`src/lifecycle.rs:23`, 5000 ms, used at
  `src/lifecycle.rs:1204` and `src/spawn.rs:793`) — expiry yields `FencingUnconfirmed`; widening the
  window widens the interval in which two workers could both believe they hold a binding.
- `PROVIDER_TERMINATION_GRACE` (`src/workspace/resources.rs:27`, 100 ms, used at `:2670`) — a
  SIGTERM-to-SIGKILL escalation rung.
- `PROVIDER_TIMEOUT` (`src/workspace/resources.rs:26`, 5 s, used at `:2449`) — bounds a third-party
  executable. `src/tests/workspace_resources.rs:374-377` asserts the observed *elapsed* time inside a
  4 s-8 s window, so it constrains this constant only indirectly; widening it past 8 s would fail that
  assertion.
- `src/workspace.rs:4682` — asserts something about an already-fenced peer, so expiry means a real
  leak. A19 pins it to the shared constant so step:runtime_exchange_retry cannot widen it by swapping
  the identifier.
- `ControlEndpoint::heartbeat` in full (`src/workspace/custody.rs:527-555`), including its
  `HEARTBEAT_INTERVAL` receive at `:536` and the three-strike count at `:549-550`. That counter is the
  liveness oracle and the safety property. Note it never reads `HEARTBEAT_FENCE_AFTER`: the 750 ms
  fence is the implicit aggregate of three 250 ms strikes, so after step:custody_forgiveness the fence
  constant survives only as the derivation base of `CONTROL_EXCHANGE_DEADLINE`.
- The deadline error literal formatted at `src/workspace/custody.rs:1896-1897`, guarded by A9.
  `heartbeat()` classifies a miss by `error.starts_with` at `:548`, so rewording that text would make
  it fence on the first miss rather than the third.
- `EMPTY_DEADLINE` (`src/workspace/platform/linux.rs:59`, 10 s) — serves three call sites (`:760`,
  `:777`, `:2228`) and is not changed here. Guarded by A13.
- `RUNTIME_COMMAND_IO_DEADLINE` is deliberately NOT on this list. An earlier draft protected it while
  Context argued that leaving it at 200 ms keeps a descheduled custodian failing, which made the plan
  contradict its own goal. It is now in scope for step:runtime_exchange_retry, guarded by A31. It is
  safe to widen for the same reason the client bound is: it is a socket option on the responder's I/O,
  and no fencing decision reads it — the heartbeat three-strike counter and the pidfd verdicts do.
- `src/supervisor.rs` in full. step:load_regression's exclusion of a late-acknowledgement injection
  exists precisely so this file stays undeclared and untouched.
- The stop/empty budget split itself, and the RAII ownership of `fresh_home`.

**The cost this plan accepts, stated rather than hidden.** `command()` drains pending heartbeats
before it sends, and the blocking receive is `src/workspace/custody.rs:505`. There is no concurrent
heartbeat driver in production: every call site is synchronous, and the supervisor loop that calls
`heartbeat()` is the same single-threaded non-blocking accept loop, in its `WouldBlock` branch, so it
is not running while that receive blocks. So a peer that dies MID-EXCHANGE is detected after 2250 ms
rather than 750 ms: worst-case death detection during a control exchange triples. That is accepted
because the cost is delaying a fault by about 1.5 s while the benefit is not killing a live peer at
all, and today that false kill reaches the divergent `retain_runtime_fault` and leaves a retained
custody fault clearable only by an operator. The idle monitoring path is unaffected, because that is
where `heartbeat()` still runs on its 250 ms beat.

## STOP conditions

- Any change would widen a fencing window or defer a kill escalation.
- A custody runtime action turns out not to be idempotent and a retry was already added.
- The step:load_regression test cannot be made to fail by reverting step:gate_backoff or
  step:graceful_stop_budget, the only two mechanisms it injects.
- Any step whose only red rows read source text. Every step from step:broker_event_wait through
  step:load_regression owns exactly one row that executes a named test, and step:deadline_taxonomy is
  documentation whose correctness the reviewer judges rather than a command.
- Either frozen fixture would need hand-editing rather than regeneration through
  `node test/rust-test-inventory.mjs --generate` or `node test/reentry-inventory.mjs --generate`.
- Any acceptance row regresses on the measured baseline recorded above.
- A deadline is deleted rather than widened, leaving a wait with no ceiling. Widening a budget is in
  scope; removing the last bound on a synchronous call is not, because a hang is a worse failure than
  a premature error and it leaks whatever child the bound was reaping.
- A step would change the deadline error literal at `src/workspace/custody.rs:1896-1897` or the
  identifier at `src/workspace.rs:4682`. Both are load-bearing for code the plan promises not to touch.
- A step would edit `src/supervisor.rs` or `scripts/gate.mjs`, neither of which is in
  `affected_paths`.
- The measured SIGTERM-to-exit tail in step:graceful_stop_budget exceeds `EMPTY_DEADLINE`.

## Open questions

None blocking. Row 5's idempotency question is answered inside step:runtime_exchange_retry from the
protocol definition, and that step carries an explicit non-retry fallback. The override-helper
question is settled in Context: widen the existing `env_ms` to `pub(crate)` rather than add a second
mechanism.

## Review

Plan-run: {"acceptance":{"source_sha256":"17830fb4ab85cf1a1d99eed1c6ad2e344ed64fcba585d2e056b4de2ea0cc7777","verification_sha256":"1d96f2c9f10a3f92556d8dffd652c8b29451d8171e5eaba0cd4602afe6b63054"},"blocker":{"evidence_sha256":"e68498bb78a0c7d0e03768c3687c8c4664591ed48e169fb2aaba39a7a36bdffe","kind":"review_failed"},"completion_review":{"input_sha256":"44ac19d541440fbe093fe5119ae15ebb01ab34bebd26126cffcca9980653a742","invocations":2,"result_sha256":"e68498bb78a0c7d0e03768c3687c8c4664591ed48e169fb2aaba39a7a36bdffe","state":"blocked"},"draft_review":{"input_sha256":"72db9c0a61350454f2fb4447660ab3309c1757d8451259e7b0584333471ebb9f","invocations":2,"result_sha256":"bc173f923e22c93eb32a43b5530cc82786a8139779b1657d7d158099732b93f8","state":"passed"},"execution_parent":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","goal_id":"9373c033-3c34-4774-9cf3-f5240feb538e","implementation_commit":"3dccce650e150f8dc4a36002194717de2ba610ab","plan_path":"docs/plans/active/custody-deadlines.md","plan_sha256":"c324da9605047e5a9740dfe6c159cf97bcf242b61c3fa5f11992d37772090c26","repository_id":"DocksDocks/session-relay","requested_effects":["local"],"risk":"sensitive","run_id":"be734bc4-5f6a-4f72-986b-462133c9a321","schema":1,"source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","source_sha256":"63573983406a6944e4552897fef3b6c21904f65a2e3823128960e6a700828ed7"}

Plan-attempt-history: {"authorization_source_sha256":"ffa213dfa45e624bdf5e0a0341edc262d9d57d2a5611579248b44e07d1aa7ca6","plan_bytes_sha256":"9723d3db6039a02f28c3a78009c25e38b69d5daae58a4fda5042600693c360f5","replacement_run_id":"be734bc4-5f6a-4f72-986b-462133c9a321","run":{"acceptance":null,"blocker":{"evidence_sha256":"fea8d4f9a28d2be9a6b33af9d20a9dd3e69fb7f55f0ffa776d06be44fcee8cd0","kind":"review_failed"},"completion_review":{"input_sha256":null,"invocations":0,"result_sha256":null,"state":"not_started"},"draft_review":{"input_sha256":"5e6f18bd2681243253c6338ebb0413c46426308102e3d1165347f46c912dccd7","invocations":2,"result_sha256":"fea8d4f9a28d2be9a6b33af9d20a9dd3e69fb7f55f0ffa776d06be44fcee8cd0","state":"blocked"},"execution_parent":null,"goal_id":"9373c033-3c34-4774-9cf3-f5240feb538e","implementation_commit":null,"plan_path":"docs/plans/active/custody-deadlines.md","plan_sha256":"a251b84b7f7d10e6865dbaf55db6a35f656a8ebb0b04a9d887ab6ac5cac6aa2d","repository_id":"DocksDocks/session-relay","requested_effects":["local"],"risk":"sensitive","run_id":"e60fca50-7d9c-4b7f-ab80-3fa5929d3d54","schema":1,"source_base":"edf27a5314e2b53be74bc9cdefdf43a468cc36ba","source_sha256":"9126fc9354ce3a2cc12176d5e6019f4292a8132bbfbc38282ad9277f4dc974a4"},"schema":1,"status":"blocked","successor_run_sha256":"936541987ba40e05a6a1303607bf5256d8ee16679f7b5d58636113ff36fa4bfb"}

## Verification Results

30 of 31 acceptance rows pass at the implementation commit, observed on a settled host with GNU
grep 3.11 at `/bin/grep`. A6 is the single exception and is analysed below; it fails because the
completion review's F1 repair added one line, not because the implementation is wrong.

**Engine.** A4 exposed a measurement hazard: a `pi-uu-grep` shim earlier in `PATH` defaults to extended
regular expressions, so the pattern `pub(crate) fn env_ms` parses `(crate)` as a capture group and reports
`0` against correct source. Both engines were then re-run against the pre-implementation tree in a scratch
worktree: all 23 non-compiling rows returned identical values under both, so no recorded Baseline and no
`Falsifiability-proof` receipt is affected. Only the post-change reading of A4 diverges, because before the
fix the answer was `0` for both reasons at once.

| ID | Step | Baseline | Observed | Expected | Colour | Result |
|---|---|---|---|---|---|---|
| A1 | 1 | `3` | `2` | `2` | RED | PASS |
| A2 | 1 | `0` | `1` | `1` | RED | PASS |
| A3 | 1 | `1` | `0` | `0` | RED | PASS |
| A4 | 1 | `0` | `1` | `1` | RED | PASS |
| A5 | 1 | `1` | `1` | `1` | GUARD | PASS |
| A6 | 2 | `0` | `11` | `10` | RED | FAIL |
| A7 | 2 | `10` | `2` | `2` | RED | PASS |
| A8 | 2 | `1` | `1` | `1` | GUARD | PASS |
| A9 | 2 | `1` | `1` | `1` | GUARD | PASS |
| A10 | 3 | `0` | `1` | `1` | RED | PASS |
| A11 | 3 | `1` | `1` | `1` | GUARD | PASS |
| A12 | 4 | `1` | `0` | `0` | RED | PASS |
| A13 | 4 | `1` | `1` | `1` | GUARD | PASS |
| A14 | 5 | `0` | `1` | `1` | RED | PASS |
| A15 | 5 | `1` | `0` | `0` | RED | PASS |
| A16 | 5 | `1` | `1` | `1` | GUARD | PASS |
| A17 | 5 | `0` | `1` | `1` | RED | PASS |
| A18 | 5 | `1` | `1` | `1` | GUARD | PASS |
| A19 | 5 | `1` | `1` | `1` | GUARD | PASS |
| A20 | 6 | `0` | `1` | `1` | RED | PASS |
| A21 | 6 | `1` | `1` | `1` | GUARD | PASS |
| A22 | 7 | `0` | `1` | `1` | RED | PASS |
| A23 | 6 | `0` | `0` | `0` | GUARD | PASS |
| A24 | 1 | `0` | `0` | `0` | GUARD | PASS |
| A25 | 1 | `0` | `1` | `1` | RED | PASS |
| A26 | 2 | `0` | `1` | `1` | RED | PASS |
| A27 | 3 | `0` | `1` | `1` | RED | PASS |
| A28 | 4 | `0` | `1` | `1` | RED | PASS |
| A29 | 5 | `0` | `1` | `1` | RED | PASS |
| A30 | 6 | `0` | `1` | `1` | RED | PASS |
| A31 | 5 | `1` | `0` | `0` | RED | PASS |

### A6 is a stale instrument, not a defect

A6 counts `CONTROL_EXCHANGE_DEADLINE` in `src/workspace/custody.rs` and expects `10`: one definition plus
the nine reply sites that stopped borrowing the heartbeat fence. It observes `11`. The extra occurrence is
line 27:

```rust
pub const STOP_EXCHANGE_DEADLINE: Duration =
    crate::workspace::platform::linux::STOP_AND_EMPTY_BUDGET.saturating_add(CONTROL_EXCHANGE_DEADLINE);
```

That line IS the repair for completion finding F1. A6 was authored before F1 was known, so no value of it
could have been correct both before and after the repair. Three independent rows still prove what A6 was
written to prove: A7 pins `HEARTBEAT_FENCE_AFTER` at exactly two occurrences, its definition and its
derivation, so no reply site borrows the fence; A8 proves `heartbeat()` still receives on
`HEARTBEAT_INTERVAL`; and A26 executes the discriminating test that a reply after the old fence but inside
the new deadline is accepted. The expected value is corrected to `11` for any successor run, together with
a row pinning the new derivation.

### The completion review found two regressions this implementation introduced

Both reproduced against the diff and both are fixed:

- **F1, contradiction.** Raising `GRACEFUL_STOP_DEADLINE` to 5 s inverted an invariant that had been
  accidentally true. `src/supervisor.rs:717-737` acknowledges Quiesce only after
  `graceful_stop_and_wait_empty` returns, and the client waited under the generic 2250 ms deadline. Before
  this work `HEARTBEAT_FENCE_AFTER` was 750 ms against a 500 ms stop, so the reply always arrived; at 5 s a
  healthy root leaving between 2250 ms and 5 s produced exactly the retained fault this plan removes. Fixed
  by `STOP_EXCHANGE_DEADLINE`, derived from `STOP_AND_EMPTY_BUDGET` plus ordinary reply slack, and applied
  only to Quiesce and Terminate. The other seven receives keep 2250 ms deliberately: widening all of them
  would triple worst-case death detection on exchanges that await no long operation, beyond the cost this
  plan's Out-of-scope section accepts.
- **F2, contradiction.** Widening `RUNTIME_COMMAND_IO_DEADLINE` from 200 ms to 10 s made the guardian
  command loop block uninterruptibly, and it cannot drive controller heartbeats while blocked, so a client
  that connected and then stalled could silence a healthy guardian past the 2250 ms control deadline and
  have it fenced. Fixed by reading the request and writing the response in 250 ms slices derived from
  `HEARTBEAT_INTERVAL`, driving the heartbeat between slices, under an unchanged 10 s total. Expiry keeps
  its existing diagnostic and failure path.

Both fixes single-source their budgets rather than restating numbers: `STOP_AND_EMPTY_BUDGET` is exported
once from the module that owns `GRACEFUL_STOP_DEADLINE` and `EMPTY_DEADLINE`, so a reader changing either
cannot forget the downstream reply deadline measured against their sum. That also restored A13, which had
briefly failed when `EMPTY_DEADLINE` was made `pub(crate)`.

### step:load_regression discriminates, proved by reverting

- `GRACEFUL_STOP_DEADLINE` reverted to 500 ms and the binary rebuilt: the new test FAILED during quiesce,
  timing out waiting for `HandbackReady` (9 passed, 1 failed). Restored to 5 s.
- `GATE_TOTAL_BUDGET` removed, collapsing acquisition to the old 3 s effective total: the new test FAILED
  with `RepositoryGate contention exceeded 3000 ms; no mutation performed`. Restored to 30 s.

### Measurements the plan required

100 samples of SIGTERM-to-exit for a ready-handshaken child on an idle 5-CPU host (1-minute load average
1.04 to 1.66): min 324 us, median 559 us, p95 751 us, p99 882 us, max 894 us. Recorded as
`OBSERVED_SIGTERM_EXIT_TAIL = 893_576 ns` (provisional). A28 asserts the shipped 5 s default is strictly
greater than that tail and strictly less than `EMPTY_DEADLINE`.

The custody runtime protocol is NOT idempotent: `CustodyController` requires exact phases then advances
them, `close_lease` consumes the guardian lease with `take()`, `closed_committed` exits the responder loop,
and `WorkspaceRuntimeCommandV1` parses a `request_id` but keeps no replay cache. Evidence:
`src/workspace/custody.rs:894-1008` and `src/workspace.rs:7029-7192`. The step therefore took its declared
non-retry route.

### Changes outside the literal step text, both inside declared paths

- `GRACEFUL_STOP_DEADLINE` became `pub const` in `src/workspace/platform/linux.rs`, because an integration
  target links this crate externally and step:load_regression must derive its stimulus from the shipped
  budget rather than restate it. `GATE_TIMEOUT`, `GATE_TOTAL_BUDGET`, `HEARTBEAT_INTERVAL` and
  `HEARTBEAT_FENCE_AFTER` were already `pub` for that reason; the private constant was the outlier.
- `test/fixtures/reentry-inventory.json` gained one classification entry,
  `"src/workspace.rs::kill_reap_broker"` under `internal_helper`. `test/reentry-inventory.mjs:294-301`
  requires an author to classify every process or signal owner and `--generate` deliberately cannot infer
  it. The site moved rather than appeared: step:broker_event_wait extracted the kill-and-reap sequence out
  of `start_git_broker`, itself already `internal_helper`, collapsing three duplicate `child_kill` sites
  into one helper with semantics identical to `reap_unpublished_child`. The rest of the census delta is the
  implementation: a new `libc_poll` site in `wait_for_broker_readiness`, and `acquire_ranked_lock` renamed
  `acquire_ranked_lock_with_budgets`.

### Gate and load sensitivity

`node scripts/gate.mjs` passes on a settled host. Three failures preceded it, all real and all fixed rather
than suppressed: A24 failed until the census classification was declared, which is exactly the drift that
row exists to catch; `biome ci` failed because the census generator writes expanded JSON arrays the
repository formatter rewrites; and A13 failed until the budget was single-sourced.

Four load-induced failures were observed on unmodified code during this work, each passing 2 or 3 of 3 when
re-run alone on a settled host: a `workspace_identity` 15-second broker-close proof, the full gate when
chained after two integration suites, `workspace_resources` missing its 4 to 8 second elapsed window, and
A21's `workspace_lease_process` immediately after a gate run. They are evidence for this plan's thesis and
the reason Environment requires every row to be observed on a settled host.

### Completion review round 2, and the fixes it forced

Invocation 2 of 2 returned two more findings on the repaired diff. Both reproduced and both are fixed, so
the shipped code carries all four repairs; the run is terminal on permit budget, not on defect.

- **F3, contradiction.** The same inversion as F1, one layer further out. `RUNTIME_CLIENT_IO_DEADLINE`
  bounded the client at 10 s while `STOP_EXCHANGE_DEADLINE` legitimately allows the guardian 17.25 s to
  reach its Quiesce reply, with `confirm_empty` and persistence still to follow. A healthy slow stop
  therefore still returned the retained-custody diagnostic. Fixed by `RUNTIME_STOP_CLIENT_IO_DEADLINE =
  STOP_EXCHANGE_DEADLINE + RUNTIME_CLIENT_IO_DEADLINE`, selected per action so only Quiesce and Terminate
  pay it, with the operator diagnostic now interpolating the bound actually in force. The symmetry test
  covers both regimes and asserts the read-back is never SHORTER than the budget, because the kernel rounds
  `SO_SNDTIMEO` up to its own granularity - 27.25 s reads back as 27.252 s - and an equality assertion there
  would fail against correct code.
- **F4, contradiction.** The `docs/crate-map.md` taxonomy still said `CONTROL_EXCHANGE_DEADLINE` was the
  ceiling at all nine reply sites, which the F1 repair had made false, and omitted the new constants that
  step:deadline_taxonomy requires every changed constant to carry. Corrected to seven ordinary sites plus
  classified rows for `STOP_EXCHANGE_DEADLINE`, `STOP_AND_EMPTY_BUDGET` and
  `RUNTIME_STOP_CLIENT_IO_DEADLINE`.

The review process is what made this work correct. Four real defects were found across two rounds, every
one of them a case of a widened budget outliving something that waited on it - the exact failure mode this
plan was written to remove, reproduced by the plan's own repairs. That is the durable lesson for the crate,
and it is why the taxonomy now records derivations rather than numbers: each new bound is expressed as a sum
of the budgets it must outlive, so the next author cannot reintroduce the inversion by editing one side.
