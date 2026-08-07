#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const plugin = path.resolve(here, '..');
const rust = path.join(plugin, 'rust');
const fixturePath = path.join(here, 'fixtures', 'rust-test-inventory.json');
const fixture = JSON.parse(fs.readFileSync(fixturePath, 'utf8'));
const workspaceTargets = [
  'workspace_identity',
  'workspace_lease_process',
  'workspace_coordination_process',
  'workspace_resources',
];
const featureTargets = ['protocol', 'fanout', 'fanout_reap', 'lifecycle_supervisor'];
const runnableTargets = [...featureTargets, ...workspaceTargets];
const omittedTargets = {
  bus_smoke: {
    owner: 'plugins/session-relay/test/rust-test-inventory.mjs',
    reason: 'Not selected by the orchestrated source-check target set; review for explicit gate inclusion.',
    expires: '2027-08-05',
  },
  lifecycle_admission: {
    owner: 'plugins/session-relay/test/rust-test-inventory.mjs',
    reason: 'Not selected by the orchestrated source-check target set; review for explicit gate inclusion.',
    expires: '2027-08-05',
  },
  lifecycle_managed: {
    owner: 'plugins/session-relay/test/rust-test-inventory.mjs',
    reason: 'Not selected by the orchestrated source-check target set; review for explicit gate inclusion.',
    expires: '2027-08-05',
  },
  lifecycle_release: {
    owner: 'plugins/session-relay/test/rust-test-inventory.mjs',
    reason: 'Not selected by the orchestrated source-check target set; review for explicit gate inclusion.',
    expires: '2027-08-05',
  },
  lock_race: {
    owner: 'plugins/session-relay/test/rust-test-inventory.mjs',
    reason: 'Not selected by the orchestrated source-check target set; review for explicit gate inclusion.',
    expires: '2027-08-05',
  },
};
const discoveredTargets = fs
  .readdirSync(path.join(rust, 'tests'), { withFileTypes: true })
  .filter((entry) => entry.isFile() && entry.name.endsWith('.rs'))
  .map((entry) => entry.name.slice(0, -3))
  .sort();
const discoveredOmittedTargets = discoveredTargets.filter((target) => !runnableTargets.includes(target));
const acceptanceOwners = {
  A01: 'workspace_lease_process::two_writers_same_worktree_exactly_one_lease',
  A02: 'workspace_lease_process::separate_worktrees_both_hold_leases',
  A03: 'workspace_lease_process::read_only_spawn_coexists_with_writer',
  A04: 'workspace_lease_process::crashed_writer_recovers_only_after_empty_proof',
  A05: 'workspace_identity::symlink_relative_case_aliases_share_one_identity',
  A06: 'workspace_coordination_process::unexpected_branch_switch_is_refused',
  A07: 'workspace_identity::unexpected_head_or_base_drift_is_refused',
  A08: 'workspace_identity::unowned_dirty_path_blocks_handback',
  A09: 'workspace_lease_process::worker_merge_rebase_reset_and_force_push_are_refused',
  A10: 'workspace_coordination_process::overlapping_path_claims_are_atomic_and_refused',
  A11: 'workspace_coordination_process::coordinator_integrates_commits_serially',
  A12: 'workspace_coordination_process::conflicting_commits_settle_once_needs_user_action',
  A13: 'scripts/tests/plan-orchestration.mjs --case historical',
  A14: 'workspace_resources::all_six_resource_kinds_are_isolated_and_receipted',
  A15: 'workspace_coordination_process::cleanup_refuses_dirty_or_unretained_work',
  A16: 'workspace_identity::integration_checkout_refuses_supported_writer',
  A17: 'workspace_identity::failed_preflight_or_lease_changes_no_source_bytes',
  A18: 'workspace_identity::preserve_commit_mode_uses_temp_index_ref_and_no_source_mutation',
  A19: 'workspace_identity::preserve_artifact_mode_round_trips_binary_and_untracked_pax',
  A20: 'workspace_coordination_process::applied_wip_is_first_produced_and_integrated_commit',
  A21: 'workspace_coordination_process::workspace_and_legacy_fanout_share_repository_gate',
  A22: 'workspace_lease_process::linux_cgroup_pidfd_guardian_kills_hostile_descendants',
  A23: 'workspace_lease_process::macos_process_group_recursive_guardian_kills_hostile_descendants',
  A24: 'workspace_identity::sha1_and_sha256_object_formats_validate_reported_oid_width',
  A25: 'workspace_coordination_process::coordinator_bootstrap_worker_scope_and_replay_are_closed',
  A26: 'workspace_coordination_process::recovery_matrix_has_no_unproven_progress',
  A27: 'test/selftest.mjs::fresh-binary-jobs-parity',
  A28: 'test/workspace-smoke.mjs::single-session-compat',
  A29: 'test/workspace-smoke.mjs::docs-contract',
};
const pendingApiGaps = {};

function listTests(target) {
  const run = spawnSync('cargo', ['test', '--locked', '--test', target, '--', '--list'], {
    cwd: rust,
    encoding: 'utf8',
  });
  assert.equal(run.status, 0, `${target}: cargo test --list failed\n${run.stdout}\n${run.stderr}`);
  return run.stdout
    .split('\n')
    .filter((line) => line.endsWith(': test'))
    .map((line) => line.slice(0, -6))
    .sort();
}

if (process.argv.includes('--generate')) {
  const cases = Object.fromEntries(runnableTargets.map((target) => [target, { tests: listTests(target) }]));
  for (const [target, entry] of Object.entries(cases))
    assert.ok(entry.tests.length > 0, `${target}: generated test set is empty`);
  fs.writeFileSync(
    fixturePath,
    `${JSON.stringify(
      {
        schema_version: 5,
        acceptance_owners: acceptanceOwners,
        pending_api_gaps: pendingApiGaps,
        omitted_targets: omittedTargets,
        cases,
      },
      null,
      2,
    )}\n`,
  );
  console.log(`PASS rust_test_inventory generated=${runnableTargets.length}`);
  process.exit(0);
}

assert.deepEqual(Object.keys(fixture).sort(), [
  'acceptance_owners',
  'cases',
  'omitted_targets',
  'pending_api_gaps',
  'schema_version',
]);
assert.equal(fixture.schema_version, 5);
assert.deepEqual(fixture.acceptance_owners, acceptanceOwners, 'A01-A29 owner matrix drifted');
assert.deepEqual(fixture.pending_api_gaps, pendingApiGaps, 'declared production API gaps drifted');
assert.deepEqual(fixture.omitted_targets, omittedTargets, 'omitted Rust target ownership drifted');
assert.deepEqual(
  Object.keys(fixture.omitted_targets).sort(),
  discoveredOmittedTargets,
  'every unselected Rust target must have an explicit owner, reason, and expiry',
);
assert.deepEqual(Object.keys(fixture.cases).sort(), [...runnableTargets].sort(), 'Rust targets drifted');
assert.deepEqual(
  Object.keys(fixture.acceptance_owners),
  Array.from({ length: 29 }, (_, index) => `A${String(index + 1).padStart(2, '0')}`),
);
for (const [acceptance, owner] of Object.entries(fixture.acceptance_owners)) {
  if (!owner.startsWith('workspace_')) continue;
  const [target, test] = owner.split('::');
  assert.ok(workspaceTargets.includes(target), `${acceptance}: invalid Rust target owner`);
  const present = fixture.cases[target].tests.includes(test);
  assert.equal(
    present,
    !Object.hasOwn(fixture.pending_api_gaps, acceptance),
    `${acceptance}: executable owner/pending API classification differs`,
  );
}
const attachmentOwners = Object.entries(fixture.acceptance_owners).filter(
  ([acceptance]) => Number(acceptance.slice(1)) <= 17,
);
assert.equal(attachmentOwners.length, 17);
assert.equal(new Set(attachmentOwners.map(([, owner]) => owner)).size, 17, 'A01-A17 owners must be one-to-one');
for (const target of runnableTargets) {
  const tests = fixture.cases[target].tests;
  assert.ok(tests.length > 0, `${target}: frozen test set is empty`);
  assert.equal(new Set(tests).size, tests.length, `${target}: duplicate test`);
  assert.deepEqual(tests, [...tests].sort(), `${target}: fixture tests must be sorted`);
}

const caseIndex = process.argv.indexOf('--case');
assert.ok(caseIndex >= 0 && process.argv[caseIndex + 1], 'usage: node rust-test-inventory.mjs --case <name>');
const name = process.argv[caseIndex + 1];
assert.ok(runnableTargets.includes(name), `unknown rust test inventory case: ${name}`);
// Widen the Rust poll-wait deadlines for this whole case. Mutating `process.env` rather than a
// derived object is deliberate: every downstream spawn either passes `process.env` directly or
// spreads it (including the `systemd-run` re-exec below, which builds an explicit `env`), so a
// value set here survives both branches and cannot be dropped by one of them.
process.env.SESSION_RELAY_TEST_TIME_FACTOR ||= '4';
let testEnv = process.env;
// Three cases exercise real cgroup-v2 custody, so they need a delegated subtree this runner owns.
// `unavailableDelegation` is non-null only when no such subtree exists; it is answered below,
// after the inventory drift check, so an unusable host still proves the cheap half.
let unavailableDelegation = null;
if (
  ['workspace_identity', 'workspace_lease_process', 'workspace_coordination_process'].includes(name) &&
  process.platform === 'linux'
) {
  // `systemd-run --user --scope -p Delegate=yes` hands us a cgroup we own with no privilege
  // escalation, and `--collect` makes systemd reap the scope and everything under it once the
  // last process exits. That is why this file no longer creates, moves into, or removes a
  // cgroup: the previous teardown wrote `cgroup.kill` and immediately `rmdir`-ed, but
  // `cgroup.kill` only *queues* SIGKILL, so the rmdir raced the exits and returned EBUSY,
  // reddening a run whose every PASS line had already printed.
  const scopeArgs = ['--user', '--scope', '-p', 'Delegate=yes', '--collect', '--quiet', '--'];
  const provided = process.env.SESSION_RELAY_TEST_CGROUP_ROOT;
  if (provided) {
    // An explicit override wins over everything: .github/workflows/ci.yml provisions the
    // delegation for the hosted runner and scripts/ci.mjs canonicalises it, so under CI this
    // branch is the one that runs and this change does not touch that path.
    assert.ok(path.isAbsolute(provided), 'SESSION_RELAY_TEST_CGROUP_ROOT must be absolute');
  } else if (process.env.SESSION_RELAY_TEST_CGROUP_SCOPE === '1') {
    // We are the re-exec, running inside the delegated scope. The scope's own cgroup is the
    // delegation: the runner is already in it, so the common ancestor of runner and leaf is a
    // cgroup we own and clone3(CLONE_INTO_CGROUP) passes its permission check with no
    // `cgroup.procs` relocation at all.
    const unified = fs
      .readFileSync('/proc/self/cgroup', 'utf8')
      .split('\n')
      .find((line) => line.startsWith('0::'));
    assert.ok(unified, 'Linux cgroup v2 membership is unavailable');
    const scope = path.join('/sys/fs/cgroup', unified.slice(3).replace(/^\/+/, ''));
    // Mirror what workspace/platform/linux.rs validate_delegation demands. A scope that was
    // created without a real delegation must name this harness, not surface as a custody error
    // several layers down in a Rust test.
    const scopeStat = fs.statSync(scope);
    assert.ok(
      scopeStat.isDirectory() && scopeStat.uid === process.getuid(),
      `delegated scope ${scope} is not a uid-owned directory`,
    );
    for (const required of ['cgroup.controllers', 'cgroup.subtree_control', 'cgroup.procs']) {
      assert.ok(fs.existsSync(path.join(scope, required)), `delegated scope ${scope} is missing ${required}`);
    }
    testEnv = { ...process.env, SESSION_RELAY_TEST_CGROUP_ROOT: scope };
  } else {
    // Probe with a throwaway scope carrying the same properties before committing: once the run
    // is nested, "systemd could not give us a scope" and "the case failed" become the same
    // non-zero exit, and an honest availability answer is the whole point of this branch.
    const probe = spawnSync('systemd-run', [...scopeArgs, 'true'], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    if (!probe.error && probe.signal === null && probe.status === 0) {
      const scoped = spawnSync('systemd-run', [...scopeArgs, process.execPath, ...process.argv.slice(1)], {
        stdio: 'inherit',
        env: { ...process.env, SESSION_RELAY_TEST_CGROUP_SCOPE: '1' },
      });
      assert.equal(scoped.error, undefined, `delegated scope could not run this case: ${scoped.error?.message}`);
      assert.equal(scoped.signal, null, `delegated case run was killed by ${scoped.signal}`);
      process.exit(scoped.status);
    }
    unavailableDelegation = probe.error?.message || probe.stderr?.trim() || probe.signal || `exit ${probe.status}`;
  }
}

const actual = listTests(name);
assert.deepEqual(actual, fixture.cases[name].tests, `${name}: executable test inventory drifted`);
if (unavailableDelegation) {
  // Loud under CI: the hosted runner is expected to supply SESSION_RELAY_TEST_CGROUP_ROOT, so
  // reaching here means the delegation the workflow promised is gone and these three cases would
  // otherwise become a silent hole in the gate.
  assert.notEqual(
    process.env.GITHUB_ACTIONS,
    'true',
    `${name}: cgroup delegation is unavailable under CI (${unavailableDelegation}); ` +
      'expected SESSION_RELAY_TEST_CGROUP_ROOT from the workflow or a usable systemd --user scope',
  );
  process.stderr.write(
    `\n${'='.repeat(78)}\n` +
      `NOT RUN: ${name} needs an owned cgroup-v2 delegation and this host has none.\n` +
      `  systemd-run --user --scope -p Delegate=yes failed: ${unavailableDelegation}\n` +
      '  Fix by starting a systemd user manager (loginctl enable-linger "$USER"), or point\n' +
      '  SESSION_RELAY_TEST_CGROUP_ROOT at a cgroup-v2 directory you own.\n' +
      `  The Rust custody tests for ${name} did NOT execute; this is not a pass.\n` +
      `${'='.repeat(78)}\n\n`,
  );
  // No PASS line: a case that did not execute must never print one.
  console.log(`SKIP rust_test_inventory case=${name} reason=cgroup-delegation-unavailable`);
  process.exit(0);
}
const executed = spawnSync('cargo', ['test', '--locked', '--test', name, '--', '--nocapture', '--test-threads=1'], {
  cwd: rust,
  encoding: 'utf8',
  env: testEnv,
});
assert.equal(executed.status, 0, `${executed.stdout}\n${executed.stderr}`);
const summary = `${executed.stdout}\n${executed.stderr}`.match(
  /test result: ok\. (\d+) passed; 0 failed; (\d+) ignored; 0 measured; (\d+) filtered out/,
);
assert.ok(summary, `${name}: missing executable test summary`);
assert.equal(Number(summary[1]), actual.length, `${name}: listed/executed test count differs`);
assert.equal(Number(summary[2]), 0, `${name}: ignored required tests`);
assert.equal(Number(summary[3]), 0, `${name}: filtered required tests`);
console.log(`PASS rust_test_inventory case=${name} tests=${actual.length} executed=${summary[1]}`);
for (const [acceptance, owner] of Object.entries(acceptanceOwners)) {
  if (owner.startsWith(`${name}::`)) {
    console.log(`PASS acceptance=${acceptance} owner=${owner}`);
  }
}
