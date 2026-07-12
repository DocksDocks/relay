#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const plugin = path.resolve(here, '..');
const rust = path.join(plugin, 'rust');
const fixture = JSON.parse(fs.readFileSync(path.join(here, 'fixtures', 'rust-test-inventory.json'), 'utf8'));
assert.deepEqual(Object.keys(fixture).sort(), ['cases', 'schema_version', 'startup_phases']);
assert.equal(fixture.schema_version, 1);

const index = process.argv.indexOf('--case');
assert.ok(index >= 0 && process.argv[index + 1], 'usage: node rust-test-inventory.mjs --case <name>');
const name = process.argv[index + 1];
assert.ok(Object.hasOwn(fixture.cases, name), `unknown rust test inventory case: ${name}`);
const entry = fixture.cases[name];
assert.deepEqual(Object.keys(entry).sort(), ['tests']);

const run = spawnSync('cargo', ['test', '--locked', '--test', name, '--', '--list'], {
  cwd: rust,
  encoding: 'utf8',
});
assert.equal(run.status, 0, run.stderr);
const actual = run.stdout.split('\n')
  .filter((line) => line.endsWith(': test'))
  .map((line) => line.slice(0, -6))
  .sort();
assert.deepEqual(actual, [...entry.tests].sort(), `${name}: executable test inventory drifted`);

if (name === 'lifecycle_supervisor') {
  const source = fs.readFileSync(path.join(rust, 'src', 'supervisor.rs'), 'utf8');
  for (const phase of fixture.startup_phases) {
    assert.match(source, new RegExp(`Self::${phase}\\b`), `startup phase is not source-derived: ${phase}`);
    const checkpoints = source.match(new RegExp(`SupervisorStartupPhase::${phase}\\b`, 'g')) ?? [];
    assert.ok(checkpoints.length >= 2, `startup phase lacks before/after checkpoints: ${phase}`);
  }
}
const executed = spawnSync('cargo', ['test', '--locked', '--test', name, '--', '--nocapture'], {
  cwd: rust,
  encoding: 'utf8',
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
