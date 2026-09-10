#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, '..');
const fixturePath = path.join(here, 'fixtures', 'rust-test-inventory.json');
const fixture = JSON.parse(fs.readFileSync(fixturePath, 'utf8'));
const runnableTargets = Object.keys(fixture.cases).sort();
// Every integration target is gated. The mechanism stays so that a future omission must be
// declared with an owner, a reason, and an expiry instead of disappearing silently.
const omittedTargets = {};
// Library tests are listed independently of the frozen integration inventory.
const UNIT_CASE = 'unit';
const SUMMARY_PATTERN = /test result: ok\. (\d+) passed; 0 failed; (\d+) ignored; 0 measured; (\d+) filtered out/;
const discoveredTargets = fs
  .readdirSync(path.join(repoRoot, 'src', 'tests'), { withFileTypes: true })
  .filter((entry) => entry.isFile() && entry.name.endsWith('.rs'))
  .map((entry) => entry.name.slice(0, -3))
  .sort();
const discoveredOmittedTargets = discoveredTargets.filter((target) => !runnableTargets.includes(target));

function listTests(target) {
  const targetArgs = target === UNIT_CASE ? ['--lib'] : ['--test', target];
  const run = spawnSync('cargo', ['test', '--locked', ...targetArgs, '--', '--list'], {
    cwd: repoRoot,
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
  const cases = Object.fromEntries(discoveredTargets.map((target) => [target, { tests: listTests(target) }]));
  for (const [target, entry] of Object.entries(cases))
    assert.ok(entry.tests.length > 0, `${target}: generated test set is empty`);
  fs.writeFileSync(
    fixturePath,
    `${JSON.stringify(
      {
        schema_version: 1,
        omitted_targets: omittedTargets,
        cases,
      },
      null,
      2,
    )}\n`,
  );
  console.log(`PASS rust_test_inventory generated=${discoveredTargets.length}`);
  process.exit(0);
}

assert.deepEqual(Object.keys(fixture).sort(), ['cases', 'omitted_targets', 'schema_version']);
assert.equal(fixture.schema_version, 1);
assert.deepEqual(fixture.omitted_targets, omittedTargets, 'omitted Rust target ownership drifted');
assert.deepEqual(
  Object.keys(fixture.omitted_targets).sort(),
  discoveredOmittedTargets,
  'every unselected Rust target must have an explicit owner, reason, and expiry',
);
assert.deepEqual(runnableTargets, discoveredTargets, 'Rust targets drifted');
for (const target of runnableTargets) {
  const tests = fixture.cases[target].tests;
  assert.ok(tests.length > 0, `${target}: frozen test set is empty`);
  assert.equal(new Set(tests).size, tests.length, `${target}: duplicate test`);
  assert.deepEqual(tests, [...tests].sort(), `${target}: fixture tests must be sorted`);
}

const caseIndex = process.argv.indexOf('--case');
assert.ok(caseIndex >= 0 && process.argv[caseIndex + 1], 'usage: node rust-test-inventory.mjs --case <name>');
const name = process.argv[caseIndex + 1];
assert.ok(runnableTargets.includes(name) || name === UNIT_CASE, `unknown rust test inventory case: ${name}`);
// Widen the Rust poll-wait deadlines for this whole case.
process.env.RELAY_TEST_TIME_FACTOR ||= '4';

if (name === UNIT_CASE) {
  const actual = listTests(UNIT_CASE);
  assert.ok(actual.length > 0, `${UNIT_CASE}: executable test set is empty`);
  const executed = spawnSync('cargo', ['test', '--locked', '--lib', '--', '--nocapture', '--test-threads=1'], {
    cwd: repoRoot,
    encoding: 'utf8',
    env: process.env,
  });
  assert.equal(executed.status, 0, `${executed.stdout}\n${executed.stderr}`);
  const unitSummary = `${executed.stdout}\n${executed.stderr}`.match(SUMMARY_PATTERN);
  assert.ok(unitSummary, `${UNIT_CASE}: missing executable test summary`);
  const passed = Number(unitSummary[1]);
  assert.equal(passed, actual.length, `${UNIT_CASE}: listed/executed test count differs`);
  assert.equal(Number(unitSummary[2]), 0, `${UNIT_CASE}: ignored library tests`);
  assert.equal(Number(unitSummary[3]), 0, `${UNIT_CASE}: filtered library tests`);
  console.log(`PASS rust_test_inventory case=${UNIT_CASE} tests=${passed}`);
  process.exit(0);
}

const actual = listTests(name);
assert.deepEqual(actual, fixture.cases[name].tests, `${name}: executable test inventory drifted`);
const executed = spawnSync('cargo', ['test', '--locked', '--test', name, '--', '--nocapture', '--test-threads=1'], {
  cwd: repoRoot,
  encoding: 'utf8',
  env: process.env,
});
assert.equal(executed.status, 0, `${executed.stdout}\n${executed.stderr}`);
const summary = `${executed.stdout}\n${executed.stderr}`.match(SUMMARY_PATTERN);
assert.ok(summary, `${name}: missing executable test summary`);
assert.equal(Number(summary[1]), actual.length, `${name}: listed/executed test count differs`);
assert.equal(Number(summary[2]), 0, `${name}: ignored required tests`);
assert.equal(Number(summary[3]), 0, `${name}: filtered required tests`);
console.log(`PASS rust_test_inventory case=${name} tests=${actual.length} executed=${summary[1]}`);
