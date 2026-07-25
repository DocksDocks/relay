#!/usr/bin/env node

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  canonicalPlanView,
  canonicalVerificationResults,
  createAffectedPathManifest,
  jcs,
  parsePlan,
  validatePlanRun,
} from '../../../plugins/docks/skills/productivity/plan-manager/scripts/plan-run.mjs';
import {
  bindCompletion,
  validateSourcePreparationProof,
  validateTddRedReceipt,
} from '../../../scripts/lib/session-relay-release-preparation.mjs';

const SELF_FILE = fileURLToPath(import.meta.url);
const HERE = path.dirname(SELF_FILE);
const REPO = fs.realpathSync.native(path.resolve(HERE, '../../..'));
const REPOSITORY_ID = 'DocksDocks/docks';
const RELEASE_VERSION = '0.14.0';
const GOAL_ID = '8b89aabf-7336-4352-bc11-225bab67f9aa';
const RUN_ID = '88732ba0-ef06-411b-a31c-93705ccefb27';
const STALE_RUN_ID = 'a69dcd97-d1bd-46fc-9b6b-70e349e353fc';
const SOURCE_BASE = '494881a0d973863d1ac8e233734c827eb6913ce8';
const PLAN_PATH = 'docs/plans/active/session-relay-correlated-results-release-remediation-v4.md';
const BLOCKED_PLAN_PATH = 'docs/plans/active/session-relay-correlated-results-release-completion.md';
const SELF_PATH = 'plugins/session-relay/test/remediation-contract.mjs';
const PRODUCER_PATH = 'scripts/capture-tdd-red.mjs';
const IMPLEMENTATION_FIXTURE_PATH = 'scripts/lib/session-relay-release-preparation.mjs';
const OUT_OF_SCOPE_PATH = 'remediation-contract-out-of-scope.txt';
const EXPECTED_AFFECTED_PATHS = [
  'plugins/session-relay/README.md',
  'plugins/session-relay/rust/src/cli.rs',
  'plugins/session-relay/rust/src/protocol.rs',
  'plugins/session-relay/rust/src/spawn.rs',
  'plugins/session-relay/rust/tests/protocol.rs',
  'plugins/session-relay/test/companion-distribution-contract.mjs',
  'plugins/session-relay/test/distribution-contract.mjs',
  'plugins/session-relay/test/fanout-smoke.mjs',
  'plugins/session-relay/test/fixtures/rust-test-inventory.json',
  'plugins/session-relay/test/release-evidence-contract.mjs',
  'plugins/session-relay/test/release-promotion-contract.mjs',
  'plugins/session-relay/test/release-publication-contract.mjs',
  SELF_PATH,
  IMPLEMENTATION_FIXTURE_PATH,
  'scripts/lib/session-relay-release-promotion.mjs',
  'scripts/lib/session-relay-release-publication.mjs',
];
const COMPLETION_DIFF_ARGS = (implementationCommit) => [
  'diff',
  '--binary',
  '--full-index',
  '--find-renames',
  '--no-ext-diff',
  '--no-textconv',
  '--no-color',
  SOURCE_BASE,
  implementationCommit,
  '--',
];

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

function logicalPath(root, relative) {
  return path.join(root, ...relative.split('/'));
}

function run(command, args, { cwd, encoding = 'utf8', env = process.env } = {}) {
  const result = spawnSync(command, args, {
    cwd,
    encoding,
    env,
    maxBuffer: Infinity,
    shell: false,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  assert.ifError(result.error);
  assert.equal(result.signal, null, `${command} ${args.join(' ')} terminated by ${result.signal}`);
  assert.equal(
    result.status,
    0,
    `${command} ${args.join(' ')} failed: ${Buffer.isBuffer(result.stderr) ? result.stderr.toString('utf8') : result.stderr}`,
  );
  return result.stdout;
}

function gitBytes(repo, args) {
  const result = spawnSync('git', args, {
    cwd: repo,
    encoding: null,
    maxBuffer: Infinity,
    shell: false,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  if (result.error) throw result.error;
  if (result.signal !== null || result.status !== 0) {
    throw new Error(`git ${args.join(' ')} failed: ${(result.stderr ?? Buffer.alloc(0)).toString('utf8').trim()}`);
  }
  return result.stdout ?? Buffer.alloc(0);
}

function gitText(repo, args) {
  return gitBytes(repo, args).toString('utf8').trim();
}

function commit(repo, message, at) {
  run('git', ['commit', '--quiet', '-m', message], {
    cwd: repo,
    env: {
      ...process.env,
      GIT_AUTHOR_DATE: at,
      GIT_COMMITTER_DATE: at,
    },
  });
  return gitText(repo, ['rev-parse', 'HEAD^{commit}']);
}

function planRunFrom(bytes) {
  const plan = bytes.toString('utf8');
  const matches = [...plan.matchAll(/^Plan-run: (\{.*\})$/gm)];
  assert.equal(matches.length, 1, 'the real active plan must carry exactly one PlanRunV1 record');
  return JSON.parse(matches[0][1]);
}

function sectionMachineRecords(plan, label) {
  return plan.split('\n').filter((line) => line.startsWith(`${label}: `));
}

function replaceSection(plan, title, bodyLines) {
  const lines = plan.split('\n');
  const heading = `## ${title}`;
  const start = lines.indexOf(heading);
  assert.notEqual(start, -1, `fixture template is missing ${heading}`);
  let end = lines.length;
  for (let index = start + 1; index < lines.length; index += 1) {
    if (/^#{1,2} /.test(lines[index])) {
      end = index;
      break;
    }
  }
  return [...lines.slice(0, start + 1), '', ...bodyLines, '', ...lines.slice(end)].join('\n').replace(/\n*$/, '\n');
}

function replacePlanRun(plan, value) {
  const matches = [...plan.matchAll(/^Plan-run: .*$/gm)];
  assert.equal(matches.length, 1, 'fixture plan must carry exactly one Plan-run line');
  return plan.replace(/^Plan-run: .*$/m, `Plan-run: ${jcs(value)}`);
}

function receiptOutput(fixture, name) {
  return path.join(fixture.receipts, `${name}.json`);
}

function makeAdapter(repo) {
  return {
    repoRoot: repo,
    git: (args) => gitBytes(repo, args),
    inspectPublic() {
      assert.fail('Docks source-proof binding must not inspect or mutate the public repository');
    },
    run() {
      assert.fail('Docks source-proof binding must not execute acceptance commands');
    },
    now() {
      return '2026-07-25T23:30:00.000Z';
    },
    readFile(file) {
      return fs.readFileSync(file);
    },
  };
}

function captureCanonicalRed(root, repo, redCommit) {
  const receiptOut = path.join(root, 'canonical-tdd-red.json');
  const failingProgram = "process.stderr.write('expected remediation binder fixture failure\\n'); process.exit(17);";
  const argv = [
    logicalPath(repo, PRODUCER_PATH),
    '--repo',
    repo,
    '--repository-id',
    REPOSITORY_ID,
    '--pre-production-commit',
    redCommit,
    '--test',
    SELF_PATH,
    '--receipt-out',
    receiptOut,
    '--',
    process.execPath,
    '-e',
    failingProgram,
  ];
  const result = run(process.execPath, argv, { cwd: repo });
  const bytes = fs.readFileSync(receiptOut);
  const value = JSON.parse(bytes.toString('utf8'));

  assert.equal(String(result).trim(), sha256(bytes), 'capture helper stdout must be the exact receipt digest');
  validateTddRedReceipt(value, { repositoryId: REPOSITORY_ID });
  assert.equal(value.pre_production_commit, redCommit);
  assert.equal(value.exit_code, 17);
  assert.deepEqual(
    value.test_paths.map(({ path: testPath }) => testPath),
    [SELF_PATH],
  );
  assert.deepEqual(value.command, {
    cwd: repo,
    argv: [process.execPath, '-e', failingProgram],
  });
  return { bytes, value };
}

function completionDiff(repo, implementationCommit) {
  return gitBytes(repo, COMPLETION_DIFF_ARGS(implementationCommit));
}

function changedPaths(repo, implementationCommit) {
  const output = gitText(repo, ['diff', '--name-only', '--no-renames', SOURCE_BASE, implementationCommit, '--']);
  return output === '' ? [] : output.split('\n');
}

function createFixture(root, templateBytes, templateRun) {
  const clonePath = path.join(root, 'repo');
  run('git', ['clone', '--quiet', '--no-hardlinks', REPO, clonePath], { cwd: root });
  const repo = fs.realpathSync.native(clonePath);
  for (const args of [
    ['config', 'user.name', 'Session Relay Remediation Contract'],
    ['config', 'user.email', 'relay-remediation@example.invalid'],
    ['config', 'commit.gpgsign', 'false'],
    ['checkout', '--quiet', '--detach', SOURCE_BASE],
  ]) {
    run('git', args, { cwd: repo });
  }

  const sourceManifest = createAffectedPathManifest({
    repo,
    paths: EXPECTED_AFFECTED_PATHS,
    sourceBase: SOURCE_BASE,
  });
  assert.equal(
    sourceManifest.source_sha256,
    templateRun.source_sha256,
    'fixture source checkout must reproduce the reviewed active-plan affected-path manifest',
  );

  const copiedTest = logicalPath(repo, SELF_PATH);
  fs.mkdirSync(path.dirname(copiedTest), { recursive: true, mode: 0o700 });
  fs.copyFileSync(logicalPath(REPO, SELF_PATH), copiedTest);
  run('git', ['add', '--', SELF_PATH], { cwd: repo });
  const redCommit = commit(repo, 'test: add remediation red contract', '2026-07-25T22:00:00.000Z');
  const red = captureCanonicalRed(root, repo, redCommit);

  fs.appendFileSync(
    logicalPath(repo, IMPLEMENTATION_FIXTURE_PATH),
    '\n// Isolated remediation-contract implementation fixture.\n',
  );
  run('git', ['add', '--', IMPLEMENTATION_FIXTURE_PATH], { cwd: repo });
  const implementationCommit = commit(repo, 'fix: isolated remediation fixture', '2026-07-25T22:05:00.000Z');
  const acceptanceManifest = createAffectedPathManifest({
    repo,
    paths: EXPECTED_AFFECTED_PATHS,
    sourceBase: implementationCommit,
  });
  const diffBytes = completionDiff(repo, implementationCommit);
  const diffSha256 = sha256(diffBytes);
  assert.deepEqual(
    changedPaths(repo, implementationCommit),
    [SELF_PATH, IMPLEMENTATION_FIXTURE_PATH].sort(),
    'positive fixture must change only reviewed affected paths',
  );

  const receipts = path.join(root, 'receipts');
  fs.mkdirSync(receipts, { mode: 0o700 });
  return {
    acceptanceManifest,
    adapter: makeAdapter(repo),
    diffBytes,
    diffSha256,
    implementationCommit,
    planPath: logicalPath(repo, PLAN_PATH),
    receipts,
    red,
    redCommit,
    repo,
    template: templateBytes.toString('utf8'),
    templateRun,
  };
}

function renderBoundPlan(
  fixture,
  {
    acceptanceManifest = fixture.acceptanceManifest,
    diffSha256 = fixture.diffSha256,
    implementationCommit = fixture.implementationCommit,
    includeRed = true,
    redValue = fixture.red.value,
    runId = RUN_ID,
  } = {},
) {
  const completionReview = {
    schema: 1,
    run_id: runId,
    invocation: 1,
    implementation_commit: implementationCommit,
    diff_sha256: diffSha256,
    verdict: 'pass',
    findings: [],
  };
  const completionReviewBytes = Buffer.from(jcs(completionReview));
  const completionInputSha256 = sha256(
    Buffer.from(
      jcs({
        schema: 1,
        type: 'RemediationCompletionBundleFixtureV1',
        run_id: runId,
        implementation_commit: implementationCommit,
        acceptance_source_sha256: acceptanceManifest.source_sha256,
      }),
    ),
  );
  assert.notEqual(
    completionInputSha256,
    diffSha256,
    'PlanRun completion input is a sealed bundle digest, not the reviewed Git diff digest',
  );

  const draftRecords = sectionMachineRecords(fixture.template, 'Review-result');
  assert.equal(draftRecords.length, 2, 'the real active v4 plan must retain both bound draft review records');
  let plan = replaceSection(fixture.template, 'Review', [
    ...draftRecords,
    '',
    `Completion-review-result: ${completionReviewBytes.toString('utf8')}`,
  ]);
  const verification = [
    ...(includeRed ? [`TDD-red-evidence: ${jcs(redValue)}`] : []),
    '- Isolated canonical release-binder fixture completed.',
  ];
  plan = replaceSection(plan, 'Verification Results', verification);
  const verificationSha256 = sha256(Buffer.from(canonicalVerificationResults(Buffer.from(plan))));
  const runValue = {
    ...fixture.templateRun,
    execution_parent: SOURCE_BASE,
    implementation_commit: implementationCommit,
    completion_review: {
      state: 'passed',
      invocations: 1,
      input_sha256: completionInputSha256,
      result_sha256: sha256(completionReviewBytes),
    },
    acceptance: {
      source_sha256: acceptanceManifest.source_sha256,
      verification_sha256: verificationSha256,
    },
    blocker: null,
    goal_id: GOAL_ID,
    run_id: runId,
    plan_path: PLAN_PATH,
    source_base: SOURCE_BASE,
  };
  plan = replacePlanRun(plan, runValue);
  const bytes = Buffer.from(plan);
  assert.equal(
    sha256(Buffer.from(canonicalPlanView(bytes))),
    fixture.templateRun.plan_sha256,
    'lifecycle evidence must leave the reviewed canonical plan identity unchanged',
  );
  validatePlanRun(bytes, {
    acceptanceManifest,
    acceptanceManifestExpectation: {
      repo: fixture.repo,
      paths: EXPECTED_AFFECTED_PATHS,
      sourceBase: implementationCommit,
    },
    goalId: GOAL_ID,
    planPath: PLAN_PATH,
    repositoryId: REPOSITORY_ID,
    runId,
  });
  return { acceptanceManifest, bytes, completionReview, run: runValue };
}

function invokeBind(fixture, boundPlan, name, planPath = fixture.planPath) {
  if (planPath === fixture.planPath) fs.writeFileSync(planPath, boundPlan.bytes);
  const out = receiptOutput(fixture, name);
  const result = bindCompletion(
    new Map([
      ['finished-plan', planPath],
      ['version', RELEASE_VERSION],
      ['receipt-out', out],
    ]),
    fixture.adapter,
  );
  return { out, result };
}

function expectBindReject(fixture, boundPlan, name, pattern, planPath = fixture.planPath) {
  const out = receiptOutput(fixture, name);
  assert.throws(() => invokeBind(fixture, boundPlan, name, planPath), pattern, name);
  assert.equal(fs.existsSync(out), false, `${name} must fail before writing a source-proof receipt`);
}

function withRun(boundPlan, mutate) {
  const runValue = structuredClone(boundPlan.run);
  mutate(runValue);
  return {
    ...boundPlan,
    bytes: Buffer.from(replacePlanRun(boundPlan.bytes.toString('utf8'), runValue)),
    run: runValue,
  };
}

function addOutOfScopeDrift(fixture) {
  fs.writeFileSync(logicalPath(fixture.repo, OUT_OF_SCOPE_PATH), 'unreviewed release input\n');
  run('git', ['add', '--', OUT_OF_SCOPE_PATH], { cwd: fixture.repo });
  const implementationCommit = commit(
    fixture.repo,
    'fix: include out-of-scope drift fixture',
    '2026-07-25T22:10:00.000Z',
  );
  const acceptanceManifest = createAffectedPathManifest({
    repo: fixture.repo,
    paths: EXPECTED_AFFECTED_PATHS,
    sourceBase: implementationCommit,
  });
  const diffSha256 = sha256(completionDiff(fixture.repo, implementationCommit));
  assert.ok(
    changedPaths(fixture.repo, implementationCommit).includes(OUT_OF_SCOPE_PATH),
    'drift fixture must really change a path outside frontmatter affected_paths',
  );
  return renderBoundPlan(fixture, { acceptanceManifest, diffSha256, implementationCommit });
}

function runBinderContract() {
  const templateBytes = fs.readFileSync(logicalPath(REPO, PLAN_PATH));
  const parsed = parsePlan(templateBytes);
  const templateRun = planRunFrom(templateBytes);
  assert.equal(parsed.frontmatter.status, 'ongoing', 'the real remediation v4 PlanRun must be active and ongoing');
  assert.deepEqual(parsed.frontmatter.affected_paths, EXPECTED_AFFECTED_PATHS);
  assert.equal(templateRun.repository_id, REPOSITORY_ID);
  assert.equal(templateRun.goal_id, GOAL_ID);
  assert.equal(templateRun.run_id, RUN_ID);
  assert.equal(templateRun.plan_path, PLAN_PATH);
  assert.equal(templateRun.source_base, SOURCE_BASE);
  assert.equal(templateRun.execution_parent, SOURCE_BASE);
  validatePlanRun(templateBytes, {
    goalId: GOAL_ID,
    planPath: PLAN_PATH,
    repositoryId: REPOSITORY_ID,
    runId: RUN_ID,
  });

  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-remediation-contract-'));
  fs.chmodSync(root, 0o700);
  try {
    const fixture = createFixture(root, templateBytes, templateRun);
    const positive = renderBoundPlan(fixture);

    // This is the intentional red edge: the current binder still hard-codes the blocked predecessor path/run.
    const bound = invokeBind(fixture, positive, 'fresh-v4-plan');
    assert.deepEqual(bound.result.receipt.tdd_red, fixture.red.value);
    assert.equal(bound.result.receipt.goal_id, GOAL_ID);
    assert.equal(bound.result.receipt.run_id, RUN_ID);
    assert.equal(bound.result.receipt.plan_run.plan_path, PLAN_PATH);
    assert.equal(bound.result.receipt.plan_run.status, 'ongoing');
    assert.equal(bound.result.receipt.implementation_commit, fixture.implementationCommit);
    assert.deepEqual(JSON.parse(fs.readFileSync(bound.out, 'utf8')), bound.result.receipt);
    validateSourcePreparationProof(bound.result.receipt);

    const missingRed = renderBoundPlan(fixture, { includeRed: false });
    expectBindReject(fixture, missingRed, 'missing-canonical-red', /TDD-red|red evidence|Verification Results/i);

    const fabricatedRed = structuredClone(fixture.red.value);
    fabricatedRed.producer.blob_id = gitText(fixture.repo, [
      'rev-parse',
      `${fixture.redCommit}:${IMPLEMENTATION_FIXTURE_PATH}`,
    ]);
    const fabricatedPlan = renderBoundPlan(fixture, { redValue: fabricatedRed });
    expectBindReject(fixture, fabricatedPlan, 'fabricated-red-producer', /TDD-red|producer|blob|identity|fabricated/i);

    expectBindReject(
      fixture,
      positive,
      'blocked-predecessor-plan',
      /blocked|PlanRun|identity|current|finished-plan/i,
      logicalPath(fixture.repo, BLOCKED_PLAN_PATH),
    );

    const staleIdentity = renderBoundPlan(fixture, { runId: STALE_RUN_ID });
    expectBindReject(fixture, staleIdentity, 'stale-plan-run-identity', /PlanRun|run|identity|stale/i);

    const verificationText = positive.bytes.toString('utf8');
    assert.ok(verificationText.includes('- Isolated canonical release-binder fixture completed.'));
    const tamperedVerification = {
      ...positive,
      bytes: Buffer.from(
        verificationText.replace(
          '- Isolated canonical release-binder fixture completed.',
          '- Isolated canonical release-binder fixture completed.\n- Injected unbound verification result.',
        ),
      ),
    };
    expectBindReject(
      fixture,
      tamperedVerification,
      'tampered-verification-results',
      /acceptance|verification_sha256|Verification Results/i,
    );

    const tamperedAcceptance = withRun(positive, (runValue) => {
      runValue.acceptance.source_sha256 = '0'.repeat(64);
    });
    expectBindReject(fixture, tamperedAcceptance, 'tampered-acceptance-manifest', /acceptance|source_sha256|manifest/i);

    const mismatchedReview = renderBoundPlan(fixture, { diffSha256: '0'.repeat(64) });
    expectBindReject(
      fixture,
      mismatchedReview,
      'completion-review-diff-mismatch',
      /CompletionReviewV1|review|diff|implementation/i,
    );

    const drifted = addOutOfScopeDrift(fixture);
    expectBindReject(fixture, drifted, 'affected-path-drift', /affected|path|scope|drift|manifest|diff/i);

    process.stdout.write('session relay remediation contract: ok\n');
  } finally {
    fs.rmSync(root, { force: true, recursive: true });
  }
}

function forwardChildOutput(stream, bytes) {
  if (!Buffer.isBuffer(bytes) || bytes.length === 0) return;
  stream.write(bytes);
  if (bytes.at(-1) !== 0x0a) stream.write('\n');
}

function runAggregateContract() {
  const rust = logicalPath(REPO, 'plugins/session-relay/rust');
  const checks = [
    {
      label: 'typed drain preflights an unclaimed suffix',
      command: 'cargo',
      args: [
        'test',
        '--locked',
        '--test',
        'protocol',
        'typed_drain_preflights_unclaimed_suffix_before_consuming_valid_prefix',
      ],
      cwd: rust,
    },
    {
      label: 'renderable drain preflights a mismatched suffix',
      command: 'cargo',
      args: [
        'test',
        '--locked',
        '--test',
        'protocol',
        'renderable_drain_preflights_mismatched_suffix_before_consuming_valid_prefix',
      ],
      cwd: rust,
    },
    {
      label: 'renderable drain rejects a noncanonical typed row',
      command: 'cargo',
      args: [
        'test',
        '--locked',
        '--test',
        'protocol',
        'renderable_drain_rejects_noncanonical_typed_row_before_consuming_claim',
      ],
      cwd: rust,
    },
    {
      label: 'spawn task option boundary',
      command: process.execPath,
      args: [logicalPath(REPO, 'plugins/session-relay/test/fanout-smoke.mjs')],
      cwd: REPO,
    },
    {
      label: 'release evidence binder',
      command: process.execPath,
      args: [SELF_FILE, '--binder-only'],
      cwd: REPO,
    },
  ];
  const results = checks.map((check) => ({
    check,
    result: spawnSync(check.command, check.args, {
      cwd: check.cwd,
      encoding: null,
      env: process.env,
      maxBuffer: Infinity,
      shell: false,
      stdio: ['ignore', 'pipe', 'pipe'],
    }),
  }));

  let failures = 0;
  for (const { check, result } of results) {
    const passed = result.error === undefined && result.signal === null && result.status === 0;
    if (!passed) failures += 1;
    process.stdout.write(
      `remediation-contract: ${check.label}: ${passed ? 'passed' : `failed (${result.signal ?? result.status ?? 'spawn'})`}\n`,
    );
    forwardChildOutput(process.stdout, result.stdout);
    forwardChildOutput(process.stderr, result.stderr);
    if (result.error) process.stderr.write(`remediation-contract: ${check.label}: ${result.error.message}\n`);
  }
  process.stdout.write(`remediation-contract: ${results.length - failures}/${results.length} focused checks passed\n`);
  if (failures !== 0) process.exitCode = 1;
}

const args = process.argv.slice(2);
if (args.length === 0) runAggregateContract();
else if (args.length === 1 && args[0] === '--binder-only') runBinderContract();
else {
  process.stderr.write('usage: node plugins/session-relay/test/remediation-contract.mjs [--binder-only]\n');
  process.exitCode = 2;
}
