#!/usr/bin/env node

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, '../../..');
const SHA = /^[0-9a-f]{64}$/;
const COMMIT = /^[0-9a-f]{40}$/;
const PUBLIC_REMOTE = 'https://github.com/DocksDocks/public.git';
const BLOCKED_REASON = 'Awaiting the four independently hashed `session-relay--v0.13.0` production asset digests.';

function parseCli(argv) {
  const result = {};
  const names = new Map([
    ['--public-remote', 'remote'],
    ['--public-ref', 'ref'],
    ['--public-commit', 'commit'],
  ]);
  for (let index = 0; index < argv.length; index += 1) {
    const option = argv[index];
    if (option === '--detached-clone') {
      assert.equal(result.detached, undefined, 'duplicate --detached-clone');
      result.detached = true;
      continue;
    }
    const name = names.get(option);
    assert.ok(name, `unknown option ${option}`);
    assert.equal(result[name], undefined, `duplicate ${option}`);
    index += 1;
    const value = argv[index];
    assert.ok(value && !value.startsWith('--'), `${option} requires a value`);
    result[name] = value;
  }
  assert.deepEqual(Object.keys(result).sort(), ['commit', 'detached', 'ref', 'remote']);
  assert.equal(result.detached, true);
  assert.equal(result.remote, PUBLIC_REMOTE);
  assert.match(result.ref, /^refs\/heads\/preflight\/session-relay-cli-0\.9\.0-[0-9a-f]{12}$/);
  assert.match(result.commit, COMMIT);
  return result;
}

function git(cwd, args, { ancestorMiss = false } = {}) {
  const result = spawnSync('git', args, { cwd, encoding: 'utf8', shell: false });
  if (ancestorMiss && result.status === 1) return null;
  assert.equal(result.status, 0, `git ${args.join(' ')}: ${result.stderr}`);
  return result.stdout.trim();
}
function run(cwd, executable, args) {
  const result = spawnSync(executable, args, {
    cwd,
    encoding: 'utf8',
    shell: false,
    env: { ...process.env, CI: '1' },
    maxBuffer: 64 * 1024 * 1024,
  });
  assert.equal(result.signal, null, `${executable} ${args.join(' ')} terminated by ${result.signal}`);
  assert.equal(result.status, 0, `${executable} ${args.join(' ')}: ${result.stderr || result.stdout}`);
}

function canonicalize(value) {
  if (value === null || typeof value !== 'object') return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalize).join(',')}]`;
  return `{${Object.keys(value)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${canonicalize(value[key])}`)
    .join(',')}}`;
}

const sha256 = (value) => createHash('sha256').update(value).digest('hex');
const exactKeys = (object, expected, label) =>
  assert.deepEqual(Object.keys(object).sort(), [...expected].sort(), label);

function verify() {
  const cli = parseCli(process.argv.slice(2));
  const directory = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-public-contract-')));
  const identity = fs.statSync(directory);
  try {
    git(directory, ['init', '--quiet']);
    git(directory, ['fetch', '--quiet', '--no-tags', cli.remote, cli.ref]);
    assert.equal(git(directory, ['rev-parse', 'FETCH_HEAD^{commit}']), cli.commit);
    git(directory, ['checkout', '--detach', '--quiet', cli.commit]);
    assert.equal(git(directory, ['rev-parse', 'HEAD']), cli.commit);
    assert.equal(git(directory, ['status', '--porcelain=v1']), '');

    const planPath = [
      'docs/plans/active/session-relay-cli-installation.md',
      'docs/plans/finished/session-relay-cli-installation.md',
    ]
      .map((relative) => path.join(directory, relative))
      .find(fs.existsSync);
    assert.ok(planPath, 'companion plan is absent');
    const plan = fs.readFileSync(planPath, 'utf8');
    const executionBase = plan.match(/^execution_base_commit:\s*([0-9a-f]{40})$/m)?.[1];
    assert.ok(executionBase, 'companion execution base is absent');
    const reviewBytes = plan.match(/^Review-receipt:\s*(\{.*\})$/m)?.[1];
    assert.ok(reviewBytes, 'companion review receipt is absent');
    const review = JSON.parse(reviewBytes);
    assert.equal(review.schema, 1);
    assert.equal(review.phase, 'draft');
    assert.equal(review.outcome, 'single');
    assert.equal(review.reviewer?.verdict, 'ready');
    assert.match(plan, /^review_status:\s*ready$/m);
    assert.match(plan, /^status:\s*blocked$/m);
    assert.match(
      plan,
      new RegExp(`^- .*blocked reason: ${BLOCKED_REASON.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}$`, 'mi'),
    );

    const receiptBytes = plan.match(/^- (?:Public|Companion) TDD-red receipt JCS bytes: (\{.*\})$/m)?.[1];
    const receiptHash = plan.match(/^- (?:Public|Companion) TDD-red receipt SHA-256: ([0-9a-f]{64})$/m)?.[1];
    assert.ok(receiptBytes && receiptHash, 'companion TDD-red receipt is absent');
    assert.equal(canonicalize(JSON.parse(receiptBytes)), receiptBytes);
    assert.equal(sha256(receiptBytes), receiptHash);
    const receipt = JSON.parse(receiptBytes);
    assert.equal(receipt.type, 'TddRedReceiptV1');
    assert.equal(receipt.repository_id, 'DocksDocks/public');
    assert.ok(receipt.test_paths.length > 0);
    for (const test of receipt.test_paths) {
      assert.equal(git(directory, ['rev-parse', `${receipt.pre_production_commit}:${test.path}`]), test.blob_id);
    }
    assert.notEqual(
      git(directory, ['merge-base', '--is-ancestor', receipt.pre_production_commit, cli.commit], {
        ancestorMiss: true,
      }),
      null,
    );
    assert.deepEqual(receipt.test_paths.map(({ path: testPath }) => testPath).sort(), [
      'cli/test/unit/pluginRefresh.test.ts',
      'cli/test/unit/sessionRelayCli.test.ts',
    ]);
    assert.deepEqual(receipt.command.argv, [
      'bun',
      'run',
      'test:unit',
      '--',
      'cli/test/unit/sessionRelayCli.test.ts',
      'cli/test/unit/pluginRefresh.test.ts',
    ]);
    for (const test of receipt.test_paths) {
      assert.equal(
        git(directory, ['rev-parse', `${cli.commit}:${test.path}`]),
        test.blob_id,
        `${test.path} changed after the frozen TDD-red capture`,
      );
    }
    assert.equal(fs.statSync(path.join(directory, 'bun.lock')).isFile(), true, 'companion lockfile is absent');
    run(directory, 'bun', ['install', '--frozen-lockfile']);
    run(directory, receipt.command.argv[0], receipt.command.argv.slice(1));
    assert.equal(git(directory, ['rev-parse', 'HEAD']), cli.commit);
    assert.equal(
      git(directory, ['status', '--porcelain=v1']),
      '',
      'companion tests or setup changed the reviewed checkout',
    );

    const docksPlan = fs.readFileSync(
      path.join(REPO, 'docs/plans/active/session-relay-prebuilt-cli-distribution.md'),
      'utf8',
    );
    const docksField = (name) =>
      docksPlan.match(new RegExp(`^- ${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}: (.+)$`, 'm'))?.[1];
    assert.equal(docksField('Companion repository ID'), 'DocksDocks/public');
    assert.equal(docksField('Companion validation ref'), cli.ref);
    assert.equal(docksField('Companion implementation commit'), cli.commit);
    assert.equal(docksField('Companion plan input SHA-256'), review.input_sha256);
    assert.equal(docksField('Companion execution base commit'), executionBase);
    assert.equal(docksField('Companion review receipt SHA-256'), sha256(reviewBytes));
    assert.equal(docksField('Companion TDD-red receipt JCS bytes'), receiptBytes);
    assert.equal(docksField('Companion TDD-red receipt SHA-256'), receiptHash);
    assert.equal(docksField('Companion status'), 'blocked');
    assert.equal(docksField('Companion blocked reason'), BLOCKED_REASON);

    const toolchain = JSON.parse(fs.readFileSync(path.join(directory, 'SoT/toolchain.json'), 'utf8'));
    const relay = toolchain['session-relay'] ?? toolchain.tools?.['session-relay'];
    exactKeys(
      relay,
      ['kind', 'policy', 'verified', 'repository', 'tag', 'plugin_id', 'plugin_version', 'install_path', 'assets'],
      'companion Session Relay manifest is closed',
    );
    assert.equal(relay.kind, 'managed-release');
    assert.equal(relay.policy, 'exact');
    assert.equal(relay.repository, 'DocksDocks/docks');
    assert.equal(relay.tag, 'session-relay--v0.13.0');
    assert.equal(relay.verified, '0.13.0');
    assert.equal(relay.plugin_id, 'session-relay@docks');
    assert.equal(relay.plugin_version, '0.13.0');
    assert.equal(relay.install_path, '~/.local/bin/session-relay');
    exactKeys(
      relay.assets,
      ['x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl', 'x86_64-apple-darwin', 'aarch64-apple-darwin'],
      'companion asset pins are closed',
    );
    for (const digest of Object.values(relay.assets)) assert.match(digest, SHA);
  } finally {
    const current = fs.statSync(directory);
    assert.equal(
      `${current.dev}:${current.ino}`,
      `${identity.dev}:${identity.ino}`,
      'companion clone identity changed before cleanup',
    );
    fs.rmSync(directory, { recursive: true, force: true });
    assert.equal(fs.existsSync(directory), false);
  }
}

verify();
process.stdout.write('PASS: companion detached distribution contract\n');
