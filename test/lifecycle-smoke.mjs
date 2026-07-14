#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const plugin = path.resolve(here, '..');
const requested = process.argv[process.argv.indexOf('--case') + 1];
assert.equal(requested, 'reentry-matrix', 'usage: lifecycle-smoke.mjs --case reentry-matrix');
const run = spawnSync(
  'cargo',
  ['test', '--locked', 'lifecycle_admission_', '--', '--nocapture'],
  { cwd: path.join(plugin, 'rust'), encoding: 'utf8', env: process.env },
);
assert.equal(run.status, 0, `reentry cargo matrix failed:\n${run.stdout}\n${run.stderr}`);
for (const behavior of [
  'lifecycle_admission_drain_receipt_rolls_back_exact_lines_before_new_mail',
  'lifecycle_admission_revalidates_registry_tool_cwd_and_server_authority',
  'lifecycle_admission_stale_unmanaged_guard_refuses_after_claiming',
  'lifecycle_admission_drain_timeout_exact_cas_persists_active_operations',
  'lifecycle_admission_unmanaged_attach_claiming_kills_waits_and_releases_guard',
]) {
  assert.match(run.stdout, new RegExp(`test ${behavior} \\.\\.\\. ok`), `${behavior} behavior row did not pass`);
}
const rows = [...run.stdout.matchAll(/PASS reentry kind=(\S+) lower=(\S+)/g)]
  .map((match) => ({ kind: match[1], lower: match[2] }));
const lifecycle = fs.readFileSync(path.join(plugin, 'rust', 'src', 'lifecycle.rs'), 'utf8');
const enumBody = /pub enum OperationKind \{([\s\S]*?)\n\}/.exec(lifecycle)?.[1];
assert.ok(enumBody, 'OperationKind enum source is missing');
const expected = enumBody.split('\n')
  .map((line) => /^\s*([A-Z][A-Za-z0-9]+),\s*$/.exec(line)?.[1])
  .filter(Boolean);
assert.deepEqual(rows.map((row) => row.kind).sort(), expected.sort(), 'matrix rows must match every source-derived OperationKind');
assert.equal(new Set(rows.map((row) => row.kind)).size, rows.length, 'OperationKind rows must be unique');
assert.equal(rows.filter((row) => row.lower === 'drain_with_guard').length, 8, 'all eight drain kinds must execute the lower mutator');
const selftest = spawnSync(process.execPath, [path.join(plugin, 'test', 'selftest.mjs')], {
  cwd: path.resolve(plugin, '..', '..'),
  encoding: 'utf8',
  env: process.env,
});
assert.equal(selftest.status, 0, `surface selftest failed:\n${selftest.stdout}\n${selftest.stderr}`);
for (const evidence of [
  'guarded attach inherits stdin/stdout/stderr while the relay parent waits',
  'wake prints claude usage to stderr and keeps fixture stdout byte-identical',
  'app-server spawn returns before turn completion while its detached pump later accepts bus elicitation',
  'watch retries only a pending acknowledgement after post-inject contention clears',
  'watch retries a refused wake after the resume lock releases and delivers queued mail',
]) {
  assert.ok(selftest.stdout.includes(`ok: ${evidence}`), `missing concrete surface evidence: ${evidence}`);
}
console.log(`PASS reentry_matrix rows=${rows.length}`);
