#!/usr/bin/env node
import assert from 'node:assert/strict';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const plugin = path.resolve(here, '..');
const requested = process.argv[process.argv.indexOf('--case') + 1];
assert.equal(requested, 'reentry-matrix', 'usage: lifecycle-smoke.mjs --case reentry-matrix');
const run = spawnSync(
  'cargo',
  ['test', '--locked', 'lifecycle_admission_reentry_matrix', '--', '--nocapture'],
  { cwd: path.join(plugin, 'rust'), encoding: 'utf8', env: process.env },
);
assert.equal(run.status, 0, `reentry cargo matrix failed:\n${run.stdout}\n${run.stderr}`);
const rows = [...run.stdout.matchAll(/^PASS reentry kind=(\S+)$/gm)].map((match) => match[1]);
assert.equal(rows.length, 14, `expected 14 OperationKind rows, got ${rows.length}`);
assert.equal(new Set(rows).size, rows.length, 'OperationKind rows must be unique');
console.log(`PASS reentry_matrix rows=${rows.length}`);
