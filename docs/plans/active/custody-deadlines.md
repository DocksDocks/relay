---
title: Stop slow-but-alive peers becoming retained custody faults
goal: Widen or bound the five custody deadlines that turn a merely slow local peer into a retained fault, and prove each change with a row that is red before it.
plan_hash_mode: status-excluded-v1
status: ongoing
created: "2026-08-08T04:09:34+00:00"
updated: "2026-08-08T05:36:24.808+00:00"
started_at: "2026-08-08T05:36:24.808+00:00"
finished_at: null
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

Plan-run: {"acceptance":null,"blocker":null,"completion_review":{"input_sha256":null,"invocations":0,"result_sha256":null,"state":"not_started"},"draft_review":{"input_sha256":"72db9c0a61350454f2fb4447660ab3309c1757d8451259e7b0584333471ebb9f","invocations":2,"result_sha256":"bc173f923e22c93eb32a43b5530cc82786a8139779b1657d7d158099732b93f8","state":"passed"},"execution_parent":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","goal_id":"9373c033-3c34-4774-9cf3-f5240feb538e","implementation_commit":null,"plan_path":"docs/plans/active/custody-deadlines.md","plan_sha256":"c324da9605047e5a9740dfe6c159cf97bcf242b61c3fa5f11992d37772090c26","repository_id":"DocksDocks/session-relay","requested_effects":["local"],"risk":"sensitive","run_id":"be734bc4-5f6a-4f72-986b-462133c9a321","schema":1,"source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","source_sha256":"63573983406a6944e4552897fef3b6c21904f65a2e3823128960e6a700828ed7"}

Plan-attempt-history: {"authorization_source_sha256":"ffa213dfa45e624bdf5e0a0341edc262d9d57d2a5611579248b44e07d1aa7ca6","plan_bytes_sha256":"9723d3db6039a02f28c3a78009c25e38b69d5daae58a4fda5042600693c360f5","replacement_run_id":"be734bc4-5f6a-4f72-986b-462133c9a321","run":{"acceptance":null,"blocker":{"evidence_sha256":"fea8d4f9a28d2be9a6b33af9d20a9dd3e69fb7f55f0ffa776d06be44fcee8cd0","kind":"review_failed"},"completion_review":{"input_sha256":null,"invocations":0,"result_sha256":null,"state":"not_started"},"draft_review":{"input_sha256":"5e6f18bd2681243253c6338ebb0413c46426308102e3d1165347f46c912dccd7","invocations":2,"result_sha256":"fea8d4f9a28d2be9a6b33af9d20a9dd3e69fb7f55f0ffa776d06be44fcee8cd0","state":"blocked"},"execution_parent":null,"goal_id":"9373c033-3c34-4774-9cf3-f5240feb538e","implementation_commit":null,"plan_path":"docs/plans/active/custody-deadlines.md","plan_sha256":"a251b84b7f7d10e6865dbaf55db6a35f656a8ebb0b04a9d887ab6ac5cac6aa2d","repository_id":"DocksDocks/session-relay","requested_effects":["local"],"risk":"sensitive","run_id":"e60fca50-7d9c-4b7f-ab80-3fa5929d3d54","schema":1,"source_base":"edf27a5314e2b53be74bc9cdefdf43a468cc36ba","source_sha256":"9126fc9354ce3a2cc12176d5e6019f4292a8132bbfbc38282ad9277f4dc974a4"},"schema":1,"status":"blocked","successor_run_sha256":"936541987ba40e05a6a1303607bf5256d8ee16679f7b5d58636113ff36fa4bfb"}

## Verification Results

Baseline observations only. No step has been implemented, so no row is claimed green beyond the
Baseline column of the acceptance table. Each receipt below binds one row to the exact command and
expected bytes it was measured with, at this run's `source_base`.

Falsifiability-proof: {"binds":"match","command_sha256":"0d26f6bf08d5fb531a6b178a9031a297756752ac2052585d2d8a0270063c77a9","expected_sha256":"9ccf2b7fcd6612847a2df9e3690e651dba906662971ee31324d96814e779799e","observed":{"matcher":"count","result":"3"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A1","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"1"}
Falsifiability-proof: {"binds":"match","command_sha256":"d89d8020862463e5765dafd9533084b028de4f1f5e8dbf77ceb5e6008b2890d9","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A2","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"1"}
Falsifiability-proof: {"binds":"match","command_sha256":"4a9f36db1fd9e8c8ee156f2ef773f758aa4a6e1a9ca5174f3501b13b74c81dda","expected_sha256":"ccf7f2a69fbff091dadda5c9ae3fc6c30f79d762043cfe5c45f8902c12cebf13","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A3","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"1"}
Falsifiability-proof: {"binds":"match","command_sha256":"2d2f70d80fe7aab5e03f0d41832716e586d9f36545cf199295bdaa601786583a","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A4","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"1"}
Falsifiability-proof: {"binds":"match","command_sha256":"5a4ff9f6eb79fb6bc361f1d3cd0391ab0cd4ad700292c3fbb84275dc50b1015a","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A5","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"1"}
Falsifiability-proof: {"binds":"match","command_sha256":"d036da3aaf537aac47cfea67a36ba9d93f803d8e621766d9ef9c9e35af366d95","expected_sha256":"073f61bd6f91e69ac74db7e668ba7af02ab2f9de9251076f22658c3ec1855050","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A6","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"2"}
Falsifiability-proof: {"binds":"match","command_sha256":"15f213382d6312ad4a901734a6b0f803b1c887ac6e0256319434e3ddd33ad41d","expected_sha256":"9ccf2b7fcd6612847a2df9e3690e651dba906662971ee31324d96814e779799e","observed":{"matcher":"count","result":"10"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A7","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"2"}
Falsifiability-proof: {"binds":"match","command_sha256":"df066888d3f725358878d0a6917f6824d5acf58bbe42540275b5740e27782338","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A8","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"2"}
Falsifiability-proof: {"binds":"match","command_sha256":"1b4695b887e6b5da7adbab642fe45b643e118b56f5ce7202604c6f1adbc55ca8","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A9","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"2"}
Falsifiability-proof: {"binds":"match","command_sha256":"3763a2ea60616bbd337616eec4491bf9b804d6e7b7923b8c82e96788fdecf621","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A10","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"3"}
Falsifiability-proof: {"binds":"match","command_sha256":"277b939a453af9deafcbf4ef80159afa39ce819d1631bfc128f5bcbeaaf1f0b3","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A11","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"3"}
Falsifiability-proof: {"binds":"match","command_sha256":"56d5cd1d4cd3556c81ab9c4a75aefd650d42940119c2c20a471de531ac0f507b","expected_sha256":"ccf7f2a69fbff091dadda5c9ae3fc6c30f79d762043cfe5c45f8902c12cebf13","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A12","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"4"}
Falsifiability-proof: {"binds":"match","command_sha256":"a47e179722159551cacc7f47f6b3f75d0de904bb3c5498cd2aad2502dca26175","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A13","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"4"}
Falsifiability-proof: {"binds":"match","command_sha256":"9840519a159b3f4ba4f3d0a26a8c75f91782519f5dbfa9b412a9bd42702880d1","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A14","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"5"}
Falsifiability-proof: {"binds":"match","command_sha256":"c532e04d3cd9ca1ca02b1c55fcd335beaf4e2b7e0df137565a084a08806341a4","expected_sha256":"ccf7f2a69fbff091dadda5c9ae3fc6c30f79d762043cfe5c45f8902c12cebf13","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A15","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"5"}
Falsifiability-proof: {"binds":"match","command_sha256":"cdbffbd30db6029cd9d8dd2ff65024c56ddfc11e73c0a0180fe24171b4a1c238","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A16","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"5"}
Falsifiability-proof: {"binds":"match","command_sha256":"2c4e94199011d6d0bf7f35cc83f3e28c55a1f98e5abddaeabb6ce6d3c685924f","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A17","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"5"}
Falsifiability-proof: {"binds":"match","command_sha256":"98a07f38becd285d73334a54e77502879c7ce308e0130172f45c4c20a7965c04","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A18","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"5"}
Falsifiability-proof: {"binds":"match","command_sha256":"a5c021e0527cb0e0aa60eee09acc18e414486140c174a0df41c65286cd49369e","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A19","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"5"}
Falsifiability-proof: {"binds":"match","command_sha256":"2202cc375977716e1e0b5b2dc96ab58fe678af3acc2c524c8e9030b72cab8de5","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A20","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"6"}
Falsifiability-proof: {"binds":"match","command_sha256":"7c1831b5e8e763cd26a47cfcaa007a170d2fe810ec55d6f8362077c95f33c87e","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A21","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"6"}
Falsifiability-proof: {"binds":"match","command_sha256":"dd93a5cf6b3996e7a085909aecca5f58c2704128385c9ada9389cd9f7cb2642b","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A22","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"7"}
Falsifiability-proof: {"binds":"exit","command_sha256":"b591052847d0765bf0847e653382350f614d467a89d07976650f4fd0e076970f","expected_sha256":"ccf7f2a69fbff091dadda5c9ae3fc6c30f79d762043cfe5c45f8902c12cebf13","observed":{"matcher":"exit","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A23","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"6"}
Falsifiability-proof: {"binds":"match","command_sha256":"618448652b1e2e61a94e048d30d58fff0419b05203b410c0f8d0bcd0aca3febf","expected_sha256":"ccf7f2a69fbff091dadda5c9ae3fc6c30f79d762043cfe5c45f8902c12cebf13","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A24","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"1"}
Falsifiability-proof: {"binds":"match","command_sha256":"b2c114531a2963752eeb59d50e275b911064faf5a0c47135b5a95092d9a96c2a","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A25","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"1"}
Falsifiability-proof: {"binds":"match","command_sha256":"5efa7ff9a70e816780d40db6ff671303b9885dcfed7dbf96231e108c8cc5acad","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A26","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"2"}
Falsifiability-proof: {"binds":"match","command_sha256":"8cbfcb4f32afb6a6e4dc72807a3e192c86c3b0d50b964975060c227a1d016fbe","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A27","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"3"}
Falsifiability-proof: {"binds":"match","command_sha256":"9893dac0e47ee90f70525d6f0147c53ae3c9412f011355c4ef1c4e48867feeb4","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A28","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"4"}
Falsifiability-proof: {"binds":"match","command_sha256":"23b2edc5653bb4c890ac090f4edd5bcf494bc24ac78c069887a1efce33c60e5b","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A29","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"5"}
Falsifiability-proof: {"binds":"match","command_sha256":"a1035ae9d845c62c16e4df44bf9ad395885e5bdd56e2be875c3ab40e8bb58e5b","expected_sha256":"c538263d2b9ccb860eeefbce59dc553b3518b366bb51f3bff46ec46ada5f98f1","observed":{"matcher":"count","result":"0"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A30","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"6"}
Falsifiability-proof: {"binds":"match","command_sha256":"fb80cff63a39b0d779118e61c93479adb09bc16bf73bd4c218fafa5f71f8ab5c","expected_sha256":"ccf7f2a69fbff091dadda5c9ae3fc6c30f79d762043cfe5c45f8902c12cebf13","observed":{"matcher":"count","result":"1"},"probe":"Measured against this repository at source_base 7a8669ce2c1b36a371a1b232c45bdf16e47a45a6. The crate, harness and gate bytes at that commit are byte-identical to the v0.16.0 tag: every commit between them touches only docs/plans, AGENTS.md and .codex, none of which any row reads. Each command was executed verbatim from the repository root with the single documented substitution of an escaped table pipe, and its output compared under the row's binding. RED means the row provably fails on these bytes and GUARD means it already passes and therefore proves no step. The per-test rows use the shape 'cargo test --locked --lib -- --exact <path>' which was itself measured falsifiable: it returns 1 against an existing test and 0 against an absent one. A16, A21, A24, A25 through A29 compile or run test binaries and are load sensitive; the full gate exited 0 twice alone and 1 once when chained after two integration suites, and workspace_resources failed its 4s-8s elapsed window once under load then passed 2/2 alone, so each was re-observed on a settled host.","row_id":"A31","source_base":"7a8669ce2c1b36a371a1b232c45bdf16e47a45a6","step_id":"5"}
