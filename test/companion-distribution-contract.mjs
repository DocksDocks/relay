#!/usr/bin/env node

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { resolveReleasePlanPath } from './historical-plan-path.mjs';

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
// This contract revalidates the published public main after the child release
// finishes, binding the generation that the active Docks parent now consumes.
const CURRENT_PUBLIC_RELAY_VERSION = '0.15.0';
const CURRENT_PUBLIC_RELAY_TAG = `session-relay--v${CURRENT_PUBLIC_RELAY_VERSION}`;
const CURRENT_PUBLIC_VERSION = '0.13.0';
const CURRENT_PUBLIC_TAG = `cli-v${CURRENT_PUBLIC_VERSION}`;
const CURRENT_PUBLIC_PLAN = 'docs/plans/active/session-relay-0.15.0-docks-kit-0.13.0-release.md';
const CURRENT_PUBLIC_RUN_ID = 'ad7f3b75-dfff-4bcd-8d1f-c8c11555b119';
const CURRENT_PUBLIC_EXECUTION_PARENT = '3e4eddec347e51189f1a13b3a48c0ca737520d94';
const CURRENT_PUBLIC_IMPLEMENTATION_COMMIT = '7ea0611958b85cd98123a8131189ddf950ce6fb9';
// The Docks release plan for this generation. It starts under `docs/plans/active/` and moves to a
// dated path under `docs/plans/finished/` when the release finishes; `currentDocksPlanFile()`
// accepts exactly one of the two.
const CURRENT_DOCKS_PLAN = 'docs/plans/active/session-relay-0.15.0-release.md';
const CURRENT_DOCKS_RUN_ID = '887eaf82-ffac-4d78-9368-b62cb64dda19';
const CURRENT_GOAL_ID = '258b44c2-c3b2-4902-862c-7461724ca078';
const HISTORICAL_PUBLIC_PLAN_SHA256 = 'e0b1d183122def14a3f4bd6f05605c6aa7de3fb2dccf4330e8956acc3e0db9ff';
const HISTORICAL_ASSET_DIGESTS = Object.freeze({
  'x86_64-unknown-linux-musl': 'f8c6374c2c704f48135cd646028fbd9e53fd43f9800b4a255fa36a0818744b7b',
  'aarch64-unknown-linux-musl': '6ebc6d9a38a8c3d1f191647d3ab679d56b69cffba36c3bc3c8eb99b0e163852e',
  'x86_64-apple-darwin': '06c046182922c6897e81278fecd7280008fa8040a489910993283017101f1be3',
  'aarch64-apple-darwin': '0686e68e3a88dd0dee647fc18211e941dd0d8012818d0bcfb79fac142b5baf21',
});
const CURRENT_ASSET_DIGESTS = Object.freeze({
  'x86_64-unknown-linux-musl': '875ca460a21d4f205833db5629bcf249413da77e444f4927107a44e63b71acab',
  'aarch64-unknown-linux-musl': 'ee52d7757a22febe3fcb4e00dbb81ec1fb1a1d5769c5eeda903f11a765029a06',
  'x86_64-apple-darwin': '8f4b11be831d5fc232965264c354f202c67c2260f383fba3e8c811eb6ea8ca39',
  'aarch64-apple-darwin': '24ef2cc98a4034391fef60bc3c13a672511b024f0d6493395bb61562936ac5c7',
});
const FROZEN_TESTS = ['cli/test/unit/pluginRefresh.test.ts', 'cli/test/unit/sessionRelayCli.test.ts'];
const FROZEN_COMMAND = [
  'bun',
  'run',
  'test:unit',
  '--',
  'cli/test/unit/sessionRelayCli.test.ts',
  'cli/test/unit/pluginRefresh.test.ts',
];
function recordedCompanionArgs() {
  const plan = fs.readFileSync(path.join(REPO, DOCKS_PLAN), 'utf8');
  const field = (name) => plan.match(new RegExp(`^- ${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}: (.+)$`, 'm'))?.[1];
  assert.equal(field('Companion repository ID'), 'DocksDocks/public');
  const ref = field('Companion validation ref');
  const commit = field('Companion implementation commit');
  assert.ok(ref, 'recorded companion validation ref is absent');
  assert.ok(commit, 'recorded companion implementation commit is absent');
  return ['--public-remote', PUBLIC_REMOTE, '--public-ref', ref, '--public-commit', commit, '--detached-clone'];
}

function parseCli(argv) {
  // Release contracts are invoked without arguments by scripts/ci.mjs. Keep the
  // explicit release-preparation mode, but derive the standalone identity from
  // the immutable Docks plan that recorded the companion tuple.
  const effectiveArgv = argv.length === 0 ? recordedCompanionArgs() : argv;
  const result = {};
  const names = new Map([
    ['--public-remote', 'remote'],
    ['--public-ref', 'ref'],
    ['--public-commit', 'commit'],
  ]);
  for (let index = 0; index < effectiveArgv.length; index += 1) {
    const option = effectiveArgv[index];
    if (option === '--detached-clone') {
      assert.equal(result.detached, undefined, 'duplicate --detached-clone');
      result.detached = true;
      continue;
    }
    const name = names.get(option);
    assert.ok(name, `unknown option ${option}`);
    assert.equal(result[name], undefined, `duplicate ${option}`);
    index += 1;
    const value = effectiveArgv[index];
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
function gitBytes(cwd, args) {
  const result = spawnSync('git', args, { cwd, shell: false });
  assert.equal(result.status, 0, `git ${args.join(' ')}: ${result.stderr.toString('utf8')}`);
  return result.stdout;
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
const escapeRegExp = (value) => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

function planRun(plan, label) {
  const raw = plan.match(/^Plan-run:\s*(\{.*\})$/m)?.[1];
  assert.ok(raw, `${label} PlanRunV1 is absent`);
  const value = JSON.parse(raw);
  assert.equal(canonicalize(value), raw, `${label} PlanRunV1 is not canonical JCS`);
  return value;
}

function currentPublicPlanFile(directory) {
  const active = path.join(directory, CURRENT_PUBLIC_PLAN);
  if (fs.existsSync(active)) return active;
  const finishedDirectory = path.join(directory, 'docs/plans/finished');
  const finishedPlanName = new RegExp(
    `^\\d{4}-\\d{2}-\\d{2}-session-relay-${escapeRegExp(CURRENT_PUBLIC_RELAY_VERSION)}-docks-kit-${escapeRegExp(CURRENT_PUBLIC_VERSION)}-release\\.md$`,
  );
  const matches = fs.existsSync(finishedDirectory)
    ? fs.readdirSync(finishedDirectory).filter((name) => finishedPlanName.test(name))
    : [];
  assert.equal(matches.length, 1, 'exactly one current finished public Session Relay child must exist');
  return path.join(finishedDirectory, matches[0]);
}

function verifyCurrentPublicMain(directory, cli) {
  git(directory, ['fetch', '--quiet', '--no-tags', cli.remote, 'refs/heads/main']);
  const currentCommit = git(directory, ['rev-parse', 'FETCH_HEAD^{commit}']);
  assert.match(currentCommit, COMMIT);
  assert.notEqual(currentCommit, cli.commit, 'current public main must be distinct from the immutable 0.13 preflight');
  git(directory, ['checkout', '--detach', '--quiet', currentCommit]);
  assert.equal(git(directory, ['rev-parse', 'HEAD']), currentCommit);
  assert.equal(git(directory, ['status', '--porcelain=v1']), '');

  const historicalPlanPath = path.join(directory, PUBLIC_PLAN);
  assert.equal(
    sha256(fs.readFileSync(historicalPlanPath)),
    HISTORICAL_PUBLIC_PLAN_SHA256,
    'legacy public 0.13 plan/receipt bytes changed',
  );

  const currentPlanPath = currentPublicPlanFile(directory);
  const currentPlan = fs.readFileSync(currentPlanPath, 'utf8');
  const currentRun = planRun(currentPlan, 'current public child');
  const docksRun = planRun(
    fs.readFileSync(path.join(REPO, resolveReleasePlanPath(REPO, CURRENT_PUBLIC_RELAY_VERSION)), 'utf8'),
    'current Docks child',
  );

  const toolchainBytes = fs.readFileSync(path.join(directory, 'SoT/toolchain.json'), 'utf8');
  const toolchain = JSON.parse(toolchainBytes);
  const relay = toolchain.tools?.['session-relay'];
  exactKeys(
    relay,
    ['kind', 'policy', 'verified', 'repository', 'tag', 'plugin_id', 'plugin_version', 'install_path', 'assets'],
    'current Session Relay manifest is closed',
  );
  assert.equal(
    relay.verified,
    CURRENT_PUBLIC_RELAY_VERSION,
    'current Relay verified version does not match the published generation',
  );
  assert.equal(
    relay.plugin_version,
    CURRENT_PUBLIC_RELAY_VERSION,
    'current Relay plugin version does not match the published generation',
  );
  assert.equal(relay.tag, CURRENT_PUBLIC_RELAY_TAG, 'current Relay tag does not match the published generation');
  assert.deepEqual(Object.keys(relay.assets).sort(), Object.keys(HISTORICAL_ASSET_DIGESTS).sort());
  assert.deepEqual(
    relay.assets,
    CURRENT_ASSET_DIGESTS,
    'current Relay asset digests do not match independently recorded publication evidence',
  );
  assert.equal(
    Object.keys(relay.assets).some((target) => /windows|win32|msvc/i.test(target)),
    false,
    'current public pins must not add Windows',
  );
  for (const [target, digest] of Object.entries(relay.assets)) {
    assert.match(digest, SHA, `${target} has no independent Relay digest`);
    assert.notEqual(digest, HISTORICAL_ASSET_DIGESTS[target], `${target} reused its immutable 0.13 digest`);
    assert.match(currentPlan, new RegExp(digest), `${target} digest is absent from public verification evidence`);
  }

  assert.equal(
    JSON.parse(fs.readFileSync(path.join(directory, 'package.json'), 'utf8')).version,
    CURRENT_PUBLIC_VERSION,
    'current public package version does not match the published generation',
  );
  const generated = fs.readFileSync(path.join(directory, 'cli/src/generated/sotPayload.ts'), 'utf8');
  assert.match(
    generated,
    new RegExp(`GENERATED_PACKAGE_VERSION\\s*=\\s*["']${escapeRegExp(CURRENT_PUBLIC_VERSION)}["']`),
    'generated payload package version does not match the current public version',
  );
  assert.match(
    generated,
    new RegExp(escapeRegExp(CURRENT_PUBLIC_RELAY_TAG)),
    'generated payload Relay tag does not match the current public Relay tag',
  );
  for (const digest of Object.values(relay.assets)) assert.match(generated, new RegExp(digest));

  assert.equal(currentRun.schema, 1);
  assert.equal(currentRun.repository_id, 'DocksDocks/public');
  assert.equal(currentRun.run_id, CURRENT_PUBLIC_RUN_ID);
  assert.equal(currentRun.goal_id, CURRENT_GOAL_ID);
  assert.equal(currentRun.goal_id, docksRun.goal_id);
  assert.equal(currentRun.risk, 'external');
  assert.deepEqual(currentRun.requested_effects, ['local', 'probe', 'publish', 'push', 'release']);
  assert.equal(currentRun.source_base, currentRun.execution_parent);
  assert.equal(currentRun.execution_parent, CURRENT_PUBLIC_EXECUTION_PARENT);
  assert.equal(currentRun.implementation_commit, CURRENT_PUBLIC_IMPLEMENTATION_COMMIT);
  assert.equal(currentRun.draft_review?.state, 'passed');
  assert.equal(currentRun.completion_review?.state, 'passed');
  exactKeys(currentRun.acceptance, ['source_sha256', 'verification_sha256'], 'current public acceptance is closed');
  assert.match(currentRun.acceptance.source_sha256, SHA, 'current public source acceptance is not bound');
  assert.match(currentRun.acceptance.verification_sha256, SHA, 'current public verification acceptance is not bound');
  assert.equal(docksRun.repository_id, `docks:${REPO}`);
  assert.equal(docksRun.run_id, CURRENT_DOCKS_RUN_ID);
  assert.equal(docksRun.plan_path, CURRENT_DOCKS_PLAN);
  assert.equal(docksRun.draft_review?.state, 'passed');
  // Archival moves the public file but deliberately does not rewrite PlanRun identity.
  // The Docks parent is still active, so its live PlanRun is read from its active path.
  assert.equal(currentRun.plan_path, CURRENT_PUBLIC_PLAN);
  assert.match(currentPlan, /^status:\s*finished$/m);
  assert.doesNotMatch(currentPlan, /^Not run\.$/m);
  // This child records no historical TDD-red receipt. Instead, its finished
  // PlanRun binds passed draft/completion review and acceptance hashes, while
  // its verification results record the generated, focused-unit, full-gate,
  // and both planted-mismatch prove-red outcomes over the implementation.
  assert.match(
    currentPlan,
    /^- bun run check:generated: exit 0\.$/m,
    'current public generated-freshness verification result is absent',
  );
  assert.match(
    currentPlan,
    /^- bun run test:unit -- cli\/test\/unit\/toolchain\.test\.ts cli\/test\/unit\/engine-di\.test\.ts: exit 0, 2 files and 18 tests passed\.$/m,
    'current public focused pin-unit verification result is absent',
  );
  assert.match(
    currentPlan,
    /^- bun run test:ci: exit 0; generated freshness, typecheck, 26 unit files \/ 192 tests, POSIX runtime smoke, 25 dry-run golden cases, and 71 mutation golden cases passed\.$/m,
    'current public full-gate verification result is absent',
  );
  assert.match(
    currentPlan,
    /^- bun cli\/test\/golden-dryrun\.ts --prove-red: expected exit 1 with prove-red OK: golden-dryrun detected 25 planted mismatch\(es\); intentionally exiting 1\.$/m,
    'current public dry-run planted-mismatch result is absent',
  );
  assert.match(
    currentPlan,
    /^- bun cli\/test\/golden-mutation\.ts --prove-red: expected exit 1 with prove-red OK: golden-mutation detected 68 planted mismatch\(es\); intentionally exiting 1\.$/m,
    'current public mutation planted-mismatch result is absent',
  );
  assert.equal(
    git(directory, ['merge-base', '--is-ancestor', currentRun.execution_parent, currentRun.implementation_commit]),
    '',
  );
  assert.equal(git(directory, ['merge-base', '--is-ancestor', currentRun.implementation_commit, currentCommit]), '');
  assert.match(currentPlan, new RegExp(escapeRegExp(CURRENT_PUBLIC_RELAY_TAG)));
  assert.match(currentPlan, new RegExp(escapeRegExp(CURRENT_PUBLIC_TAG)));
  assert.match(
    currentPlan,
    new RegExp(`docks-kit@${escapeRegExp(CURRENT_PUBLIC_VERSION)}`),
    'current public npm package evidence does not match the published version',
  );
  assert.equal(git(directory, ['status', '--porcelain=v1']), '');
}

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
    verifyCurrentPublicMain(directory, cli);
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
