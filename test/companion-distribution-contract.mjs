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
const PUBLIC_PLAN = 'docs/plans/active/session-relay-cli-0.13.0-release-preparation.md';
const DOCKS_PLAN = 'docs/plans/finished/2026-07-23-session-relay-linux-workspace-recertification.md';
const PUBLIC_VERSION = '0.13.0';
const PRODUCTION_VERSION = '0.12.0';
const BLOCKED_REASON = 'Awaiting the four independently hashed `session-relay--v0.13.0` production asset digests.';
const FROZEN_TESTS = ['cli/test/unit/pluginRefresh.test.ts', 'cli/test/unit/sessionRelayCli.test.ts'];
const FROZEN_COMMAND = [
  'bun',
  'run',
  'test:unit',
  '--',
  'cli/test/unit/sessionRelayCli.test.ts',
  'cli/test/unit/pluginRefresh.test.ts',
];

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
  assert.match(result.ref, /^refs\/heads\/preflight\/session-relay-cli-0\.13\.0-[0-9a-f]{12}$/);
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
  assert.equal(
    cli.ref,
    `refs/heads/preflight/session-relay-cli-${PUBLIC_VERSION}-${cli.commit.slice(0, 12)}`,
    'public validation ref must be derived from the exact blocked commit',
  );
  const directory = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-public-contract-')));
  const identity = fs.statSync(directory);
  try {
    git(directory, ['init', '--quiet']);
    assert.equal(
      git(directory, ['ls-remote', '--heads', cli.remote, cli.ref]),
      `${cli.commit}\t${cli.ref}`,
      'remote validation ref does not resolve exactly to the recorded blocked commit',
    );
    git(directory, ['fetch', '--quiet', '--no-tags', cli.remote, cli.ref]);
    assert.equal(git(directory, ['rev-parse', 'FETCH_HEAD^{commit}']), cli.commit);
    git(directory, ['checkout', '--detach', '--quiet', cli.commit]);
    assert.equal(git(directory, ['rev-parse', 'HEAD']), cli.commit);
    assert.equal(git(directory, ['status', '--porcelain=v1']), '');

    const planPath = path.join(directory, PUBLIC_PLAN);
    assert.equal(fs.statSync(planPath).isFile(), true, 'exact active public source-preparation plan is absent');
    const plan = fs.readFileSync(planPath, 'utf8');
    const executionBase = plan.match(/^execution_base_commit:\s*([0-9a-f]{40})$/m)?.[1];
    assert.ok(executionBase, 'public execution base is absent');
    const sourceCommit = plan.match(/^- Source commit:\s*([0-9a-f]{40})$/m)?.[1];
    assert.ok(sourceCommit, 'public source commit is absent');

    const reviewLine = plan.match(/^Review-receipt:\s*(\{.*\})$/m)?.[0];
    const reviewBytes = plan.match(/^Review-receipt:\s*(\{.*\})$/m)?.[1];
    assert.ok(reviewLine && reviewBytes, 'public draft review receipt is absent');
    const review = JSON.parse(reviewBytes);
    assert.equal(canonicalize(review), reviewBytes, 'public review receipt is not canonical payload JSON');
    assert.equal(review.schema, 6);
    assert.equal(review.phase, 'draft');
    assert.equal(review.outcome, 'passed');
    assert.equal(review.pre_execution_eligible, true);
    assert.equal(review.request?.schema, 6);
    assert.equal(review.request?.phase, 'draft');
    assert.equal(review.reviewer?.raw?.reviewer_output?.schema, 6);
    assert.equal(review.reviewer?.raw?.reviewer_output?.verdict, 'pass');
    assert.match(review.reviewed_commit, COMMIT);
    assert.notEqual(review.reviewed_commit, executionBase, 'reviewed commit must strictly precede execution base');
    assert.equal(git(directory, ['merge-base', '--is-ancestor', review.reviewed_commit, executionBase]), '');

    assert.match(plan, /^status:\s*blocked$/m);
    assert.match(
      plan,
      new RegExp(`^blocked_reason:\\s*"${BLOCKED_REASON.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}"$`, 'm'),
    );
    assert.match(plan, new RegExp(`^- Blocked reason: ${BLOCKED_REASON.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}$`, 'm'));

    const receiptBytes = plan.match(/^Public TDD-red receipt JCS bytes: (\{.*\})$/m)?.[1];
    const receiptHash = plan.match(/^Public TDD-red receipt SHA-256: ([0-9a-f]{64})$/m)?.[1];
    assert.ok(receiptBytes && receiptHash, 'public TDD-red receipt is absent');
    assert.equal(canonicalize(JSON.parse(receiptBytes)), receiptBytes);
    assert.equal(sha256(receiptBytes), receiptHash);
    const receipt = JSON.parse(receiptBytes);
    exactKeys(
      receipt,
      [
        'schema',
        'type',
        'repository_id',
        'pre_production_commit',
        'test_paths',
        'command',
        'exit_code',
        'stdout_sha256',
        'stderr_sha256',
        'captured_at',
        'producer',
      ],
      'public TDD-red receipt is closed',
    );
    assert.equal(receipt.schema, 1);
    assert.equal(receipt.type, 'TddRedReceiptV1');
    assert.equal(receipt.repository_id, 'DocksDocks/public');
    assert.match(receipt.pre_production_commit, COMMIT);
    assert.notEqual(receipt.pre_production_commit, sourceCommit);
    assert.equal(git(directory, ['merge-base', '--is-ancestor', receipt.pre_production_commit, sourceCommit]), '');
    assert.deepEqual(receipt.test_paths.map(({ path: testPath }) => testPath).sort(), FROZEN_TESTS);
    assert.deepEqual(receipt.command.argv, FROZEN_COMMAND);
    for (const test of receipt.test_paths) {
      assert.equal(git(directory, ['rev-parse', `${receipt.pre_production_commit}:${test.path}`]), test.blob_id);
      assert.equal(
        git(directory, ['rev-parse', `${sourceCommit}:${test.path}`]),
        test.blob_id,
        `${test.path} changed between frozen red capture and source readiness`,
      );
      assert.equal(
        git(directory, ['rev-parse', `${cli.commit}:${test.path}`]),
        test.blob_id,
        `${test.path} changed in the blocked public commit`,
      );
    }

    assert.notEqual(sourceCommit, cli.commit, 'blocked commit must be distinct from source commit');
    assert.equal(git(directory, ['merge-base', '--is-ancestor', sourceCommit, cli.commit]), '');
    assert.equal(
      git(directory, ['diff', '--name-only', `${sourceCommit}..${cli.commit}`]),
      PUBLIC_PLAN,
      'source-to-public delta must contain only the blocked public plan',
    );

    const docksPlan = fs.readFileSync(path.join(REPO, DOCKS_PLAN), 'utf8');
    const docksField = (name) =>
      docksPlan.match(new RegExp(`^- ${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}: (.+)$`, 'm'))?.[1];
    assert.equal(docksField('Companion repository ID'), 'DocksDocks/public');
    assert.equal(docksField('Companion plan'), `\`/home/vagrant/projects/public/${PUBLIC_PLAN}\``);
    assert.equal(docksField('Companion validation ref'), cli.ref);
    assert.equal(docksField('Companion implementation commit'), cli.commit);
    assert.equal(docksField('Companion plan input SHA-256'), review.input_sha256);
    assert.equal(docksField('Companion execution base commit'), executionBase);
    assert.equal(docksField('Companion review receipt SHA-256'), sha256(reviewBytes));
    assert.notEqual(docksField('Companion review receipt SHA-256'), sha256(reviewLine));
    assert.equal(docksField('Companion TDD-red receipt JCS bytes'), receiptBytes);
    assert.equal(docksField('Companion TDD-red receipt SHA-256'), receiptHash);
    assert.equal(docksField('Companion status'), 'blocked');
    assert.equal(docksField('Companion blocked reason'), BLOCKED_REASON);

    for (const protectedPath of ['SoT/toolchain.json', 'cli/src/generated/sotPayload.ts']) {
      const baseBlob = git(directory, ['rev-parse', `${executionBase}:${protectedPath}`]);
      assert.equal(git(directory, ['rev-parse', `${sourceCommit}:${protectedPath}`]), baseBlob);
      assert.equal(git(directory, ['rev-parse', `${cli.commit}:${protectedPath}`]), baseBlob);
    }
    const toolchainBytes = fs.readFileSync(path.join(directory, 'SoT/toolchain.json'), 'utf8');
    assert.doesNotMatch(toolchainBytes, /0\.13\.0/, 'future version leaked into production toolchain authority');
    const toolchain = JSON.parse(toolchainBytes);
    const relay = toolchain.tools?.['session-relay'];
    exactKeys(
      relay,
      ['kind', 'policy', 'verified', 'repository', 'tag', 'plugin_id', 'plugin_version', 'install_path', 'assets'],
      'production Session Relay manifest is closed',
    );
    assert.equal(relay.kind, 'managed-release');
    assert.equal(relay.policy, 'exact');
    assert.equal(relay.repository, 'DocksDocks/docks');
    assert.equal(relay.tag, `session-relay--v${PRODUCTION_VERSION}`);
    assert.equal(relay.verified, PRODUCTION_VERSION);
    assert.equal(relay.plugin_id, 'session-relay@docks');
    assert.equal(relay.plugin_version, PRODUCTION_VERSION);
    assert.equal(relay.install_path, '~/.local/bin/session-relay');
    exactKeys(
      relay.assets,
      ['x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl', 'x86_64-apple-darwin', 'aarch64-apple-darwin'],
      'production asset pins are closed',
    );
    for (const digest of Object.values(relay.assets)) assert.match(digest, SHA);
    const baseToolchain = JSON.parse(git(directory, ['show', `${executionBase}:SoT/toolchain.json`]));
    assert.deepEqual(relay.assets, baseToolchain.tools['session-relay'].assets, 'production digests changed');
    assert.doesNotMatch(
      fs.readFileSync(path.join(directory, 'cli/src/generated/sotPayload.ts'), 'utf8'),
      /0\.13\.0/,
      'future version leaked into generated production payload',
    );

    const fixtureSource = fs.readFileSync(path.join(directory, 'cli/test/unit/sessionRelayCli.test.ts'), 'utf8');
    const pluginRefreshSource = fs.readFileSync(path.join(directory, 'cli/test/unit/pluginRefresh.test.ts'), 'utf8');
    const installerSource = fs.readFileSync(path.join(directory, 'cli/src/engine-native/sessionRelayCli.ts'), 'utf8');
    assert.match(fixtureSource, /const VERSION = "0\.13\.0"/);
    assert.match(fixtureSource, /ASSET_DIGEST = createHash\("sha256"\)\.update\(ASSET_BYTES\)\.digest\("hex"\)/);
    assert.doesNotMatch(fixtureSource, /["'][0-9a-f]{64}["']/, 'fixture copied a literal digest');
    assert.match(pluginRefreshSource, /toolchain\.tools\["session-relay"\]\.verified/);
    assert.doesNotMatch(pluginRefreshSource, /0\.13\.0/, 'future fixture version leaked into production refresh test');
    assert.doesNotMatch(installerSource, /0\.(?:12|13)\.0/, 'installer source retained a version authority');
    assert.match(installerSource, /const version = value\["verified"\]/);
    assert.match(installerSource, /exactVersion\(ops, stable, manifest\.verified\)/);
    assert.match(installerSource, /exactVersion\(ops, stage, manifest\.verified\)/);

    assert.equal(fs.statSync(path.join(directory, 'bun.lock')).isFile(), true, 'public lockfile is absent');
    run(directory, 'bun', ['install', '--frozen-lockfile']);
    run(directory, FROZEN_COMMAND[0], FROZEN_COMMAND.slice(1));
    assert.equal(git(directory, ['rev-parse', 'HEAD']), cli.commit);
    assert.equal(
      git(directory, ['status', '--porcelain=v1']),
      '',
      'public tests or setup changed the reviewed checkout',
    );
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
