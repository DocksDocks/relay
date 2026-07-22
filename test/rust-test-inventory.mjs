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
  A13: 'scripts/tests/plan-review-policy-regressions.mjs --orchestration-oracle',
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
  const cases = Object.fromEntries(workspaceTargets.map((target) => [target, { tests: listTests(target) }]));
  for (const [target, entry] of Object.entries(cases))
    assert.ok(entry.tests.length > 0, `${target}: generated test set is empty`);
  fs.writeFileSync(
    fixturePath,
    `${JSON.stringify({ schema_version: 3, acceptance_owners: acceptanceOwners, pending_api_gaps: pendingApiGaps, cases }, null, 2)}\n`,
  );
  console.log(`PASS rust_test_inventory generated=${workspaceTargets.length}`);
  process.exit(0);
}

assert.deepEqual(Object.keys(fixture).sort(), ['acceptance_owners', 'cases', 'pending_api_gaps', 'schema_version']);
assert.equal(fixture.schema_version, 3);
assert.deepEqual(fixture.acceptance_owners, acceptanceOwners, 'A01-A29 owner matrix drifted');
assert.deepEqual(fixture.pending_api_gaps, pendingApiGaps, 'declared production API gaps drifted');
assert.deepEqual(Object.keys(fixture.cases).sort(), [...workspaceTargets].sort(), 'workspace Rust targets drifted');
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
for (const target of workspaceTargets) {
  const tests = fixture.cases[target].tests;
  assert.ok(tests.length > 0, `${target}: frozen test set is empty`);
  assert.equal(new Set(tests).size, tests.length, `${target}: duplicate test`);
  assert.deepEqual(tests, [...tests].sort(), `${target}: fixture tests must be sorted`);
}

const caseIndex = process.argv.indexOf('--case');
assert.ok(caseIndex >= 0 && process.argv[caseIndex + 1], 'usage: node rust-test-inventory.mjs --case <name>');
const name = process.argv[caseIndex + 1];
assert.ok(workspaceTargets.includes(name), `unknown rust test inventory case: ${name}`);
let testEnv = process.env;
let delegatedCgroupRoot = null;
let originalCgroupProcs = null;
if (
  ['workspace_identity', 'workspace_lease_process', 'workspace_coordination_process'].includes(name) &&
  process.platform === 'linux'
) {
  const provided = process.env.SESSION_RELAY_TEST_CGROUP_ROOT;
  if (provided) {
    assert.ok(path.isAbsolute(provided), 'SESSION_RELAY_TEST_CGROUP_ROOT must be absolute');
  } else {
    assert.equal(typeof process.getuid, 'function', 'Linux cgroup delegation requires a native uid');
    delegatedCgroupRoot = `/sys/fs/cgroup/session-relay-test-${process.getuid()}-${process.pid}-${Date.now()}`;
    const create = spawnSync('sudo', ['-n', 'mkdir', delegatedCgroupRoot], { encoding: 'utf8' });
    assert.equal(create.status, 0, `create test cgroup delegation failed:\n${create.stderr}`);
    const own = spawnSync(
      'sudo',
      [
        '-n',
        'chown',
        `${process.getuid()}:${process.getgid()}`,
        delegatedCgroupRoot,
        path.join(delegatedCgroupRoot, 'cgroup.kill'),
        path.join(delegatedCgroupRoot, 'cgroup.procs'),
        path.join(delegatedCgroupRoot, 'cgroup.threads'),
        path.join(delegatedCgroupRoot, 'cgroup.subtree_control'),
      ],
      { encoding: 'utf8' },
    );
    assert.equal(own.status, 0, `own test cgroup delegation failed:\n${own.stderr}`);
    testEnv = { ...process.env, SESSION_RELAY_TEST_CGROUP_ROOT: delegatedCgroupRoot };
    const unified = fs
      .readFileSync('/proc/self/cgroup', 'utf8')
      .split('\n')
      .find((line) => line.startsWith('0::'));
    assert.ok(unified, 'Linux cgroup v2 membership is unavailable');
    originalCgroupProcs = path.join('/sys/fs/cgroup', unified.slice(3).replace(/^\/+/, ''), 'cgroup.procs');
    const enter = spawnSync('sudo', ['-n', 'tee', path.join(delegatedCgroupRoot, 'cgroup.procs')], {
      encoding: 'utf8',
      input: `${process.pid}\n`,
    });
    assert.equal(enter.status, 0, `enter test runner cgroup failed:\n${enter.stderr}`);
  }
}
const cleanupDelegation = () => {
  if (!delegatedCgroupRoot) return;
  if (originalCgroupProcs) {
    const restore = spawnSync('sudo', ['-n', 'tee', originalCgroupProcs], {
      encoding: 'utf8',
      input: `${process.pid}\n`,
    });
    assert.equal(restore.status, 0, `restore test runner cgroup failed:\n${restore.stderr}`);
    originalCgroupProcs = null;
  }
  const removeChildCgroups = (root) => {
    for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const child = path.join(root, entry.name);
      removeChildCgroups(child);
      fs.rmdirSync(child);
    }
  };
  try {
    fs.writeFileSync(path.join(delegatedCgroupRoot, 'cgroup.kill'), '1\n');
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
  }
  removeChildCgroups(delegatedCgroupRoot);
  const cleanup = spawnSync('sudo', ['-n', 'rmdir', delegatedCgroupRoot], { encoding: 'utf8' });
  if (cleanup.status !== 0) {
    process.stderr.write(`cleanup test cgroup delegation failed:\n${cleanup.stderr}`);
    process.exitCode = 1;
  }
  delegatedCgroupRoot = null;
};
process.on('exit', cleanupDelegation);

const actual = listTests(name);
assert.deepEqual(actual, fixture.cases[name].tests, `${name}: executable test inventory drifted`);
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
cleanupDelegation();
