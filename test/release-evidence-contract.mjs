#!/usr/bin/env node

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { deflateRawSync } from 'node:zlib';
import { parse as parseYaml } from 'yaml';
import { canonicalPlanView } from '../../../plugins/docks/skills/productivity/plan-reviewer/scripts/review-policy.mjs';
import { gitRaw } from '../../../scripts/lib/session-relay-release-core.mjs';
import { runFixture } from '../../../scripts/lib/session-relay-release-fixture.mjs';
import {
  bindCompletion,
  checkPrepared,
  prepareCiCall,
  runPrepareCi,
  validateProducerPreflightReceipt,
  validateProof,
  validateSourceCiReceipt,
  validateSourcePreparationProof,
  validateTddRedReceipt,
  verifyEmbedded,
  verifySourceCi,
} from '../../../scripts/lib/session-relay-release-preparation.mjs';
import { verifyPreflight } from '../../../scripts/verify-session-relay-preflight.mjs';

const REPOSITORY_ID = 'DocksDocks/docks';
const COMMIT = '1234567890abcdef1234567890abcdef12345678';
const WORKFLOW_BLOB = 'abcdef1234567890abcdef1234567890abcdef12';
const RUN_ID = 712345;
const RUN_ATTEMPT = 2;
const REPOSITORY_DATABASE_ID = 98765;
const WORKFLOW_ID = 4567;
const TARGETS = [
  ['x86_64-unknown-linux-musl', 'Linux', 'X64'],
  ['aarch64-unknown-linux-musl', 'Linux', 'ARM64'],
  ['x86_64-apple-darwin', 'macOS', 'X64'],
  ['aarch64-apple-darwin', 'macOS', 'ARM64'],
];

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

function testRawGitBytes(temp) {
  const repo = path.join(temp, 'raw-git');
  fs.mkdirSync(repo);
  for (const args of [
    ['init', '--quiet'],
    ['config', 'user.name', 'Session Relay Test'],
    ['config', 'user.email', 'relay-test@example.invalid'],
  ]) {
    const result = spawnSync('git', args, { cwd: repo, encoding: 'utf8', shell: false });
    assert.equal(result.status, 0, result.stderr);
  }
  fs.writeFileSync(path.join(repo, 'payload.txt'), 'terminal newline\n');
  const commit = spawnSync('git', ['add', 'payload.txt'], { cwd: repo, encoding: 'utf8', shell: false });
  assert.equal(commit.status, 0, commit.stderr);
  const committed = spawnSync('git', ['commit', '--quiet', '-m', 'fixture'], {
    cwd: repo,
    encoding: 'utf8',
    shell: false,
  });
  assert.equal(committed.status, 0, committed.stderr);
  assert.deepEqual(gitRaw(['-C', repo, 'show', 'HEAD:payload.txt']), Buffer.from('terminal newline\n'));
}

function jcs(value) {
  if (value === null || typeof value === 'boolean' || typeof value === 'string' || typeof value === 'number')
    return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(jcs).join(',')}]`;
  return `{${Object.keys(value)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${jcs(value[key])}`)
    .join(',')}}`;
}

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function zip(entries) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const entry of entries) {
    const name = Buffer.from(entry.name);
    const bytes = Buffer.from(entry.bytes);
    const compressed = entry.deflate ? deflateRawSync(bytes) : bytes;
    const method = entry.deflate ? 8 : 0;
    const declaredSize = entry.declaredSize ?? bytes.length;
    const crc = crc32(bytes);
    const flags = 0x800 | (entry.dataDescriptor ? 0x0008 : 0);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(flags, 6);
    local.writeUInt16LE(method, 8);
    if (!entry.dataDescriptor) {
      local.writeUInt32LE(crc, 14);
      local.writeUInt32LE(compressed.length, 18);
      local.writeUInt32LE(declaredSize, 22);
    }
    local.writeUInt16LE(name.length, 26);
    const descriptor = entry.dataDescriptor ? Buffer.alloc(16) : Buffer.alloc(0);
    if (entry.dataDescriptor) {
      descriptor.writeUInt32LE(0x08074b50, 0);
      descriptor.writeUInt32LE(crc, 4);
      descriptor.writeUInt32LE(compressed.length, 8);
      descriptor.writeUInt32LE(declaredSize, 12);
    }
    locals.push(local, name, compressed, descriptor);

    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(0x031e, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(flags, 8);
    central.writeUInt16LE(method, 10);
    central.writeUInt32LE(crc, 16);
    central.writeUInt32LE(compressed.length, 20);
    central.writeUInt32LE(declaredSize, 24);
    central.writeUInt16LE(name.length, 28);
    central.writeUInt32LE(entry.mode === 'symlink' ? 0xa1ff0000 : 0x81a40000, 38);
    central.writeUInt32LE(offset, 42);
    centrals.push(central, name);
    offset += local.length + name.length + compressed.length + descriptor.length;
  }
  const centralOffset = offset;
  const centralBytes = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(centralBytes.length, 12);
  end.writeUInt32LE(centralOffset, 16);
  return Buffer.concat([...locals, centralBytes, end]);
}

function binaryFor(target) {
  if (target.endsWith('linux-musl')) {
    const bytes = Buffer.alloc(32);
    bytes.set([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1]);
    bytes.writeUInt16LE(target.startsWith('x86_64') ? 62 : 183, 18);
    return bytes;
  }
  const bytes = Buffer.alloc(16);
  bytes.writeUInt32LE(0xfeedfacf, 0);
  bytes.writeUInt32LE(target.startsWith('x86_64') ? 0x01000007 : 0x0100000c, 4);
  return bytes;
}

function nativeProducerJobs() {
  const descriptors = [
    ['x86_64-unknown-linux-musl', 'ubuntu-24.04', 'Linux'],
    ['aarch64-unknown-linux-musl', 'ubuntu-24.04-arm', 'Linux'],
    ['x86_64-apple-darwin', 'macos-15-intel', 'macOS'],
    ['aarch64-apple-darwin', 'macos-15', 'macOS'],
  ];
  const stepNames = [
    'build locked native release',
    'prove Linux managed-workspace custody',
    'prove macOS managed-workspace admission STOP',
    'smoke explicit fresh Linux workspace binary',
    'attest native release binary',
    'upload stable executable and canonical attestation',
  ];
  return descriptors.map(([target, runner, runnerOs], jobIndex) => ({
    id: 7000 + jobIndex,
    run_id: RUN_ID,
    run_attempt: RUN_ATTEMPT,
    head_sha: COMMIT,
    name: `build ${target} on ${runner}`,
    status: 'completed',
    conclusion: 'success',
    runner_id: 7100 + jobIndex,
    runner_name: `GitHub Actions ${jobIndex + 1}`,
    runner_group_id: 0,
    runner_group_name: 'GitHub Actions',
    labels: [runner],
    steps: stepNames.map((name, stepIndex) => ({
      name,
      number: stepIndex + 1,
      status: 'completed',
      conclusion:
        name === 'prove Linux managed-workspace custody'
          ? runnerOs === 'Linux'
            ? 'success'
            : 'skipped'
          : name === 'prove macOS managed-workspace admission STOP'
            ? runnerOs === 'macOS'
              ? 'success'
              : 'skipped'
            : name === 'smoke explicit fresh Linux workspace binary'
              ? runnerOs === 'Linux'
                ? 'success'
                : 'skipped'
              : 'success',
    })),
  }));
}

function artifactFixture({ mutateArchive } = {}) {
  const archives = new Map();
  const artifacts = [];
  const binaryDigests = new Map();
  let databaseId = 8000;
  for (const [target, runnerOs, runnerArch] of TARGETS) {
    const assetName = `session-relay-${target}`;
    const artifactName = `session-relay-binary-${target}`;
    const binary = binaryFor(target);
    const digest = sha256(binary);
    binaryDigests.set(assetName, digest);
    const attestation = Buffer.from(
      jcs({
        asset_name: assetName,
        inputs: { expected_commit: COMMIT, expected_tag: '', mode: 'validate-only' },
        runner_arch: runnerArch,
        runner_os: runnerOs,
        schema: 'SessionRelayBinaryAttestationV1',
        sha256: digest,
        source_commit: COMMIT,
        target,
        version_stdout: 'session-relay 0.12.0',
        workflow_run_attempt: RUN_ATTEMPT,
        workflow_run_id: RUN_ID,
      }),
    );
    const archive = zip([
      { name: assetName, bytes: binary, deflate: true, dataDescriptor: true },
      { name: `attestation-${target}.json`, bytes: attestation, deflate: true, dataDescriptor: true },
    ]);
    archives.set(databaseId, archive);
    artifacts.push(artifactRecord(databaseId, artifactName, archive));
    databaseId += 1;
  }
  const manifest = Buffer.from(
    [...binaryDigests]
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([name, digest]) => `${digest}  ${name}\n`)
      .join(''),
  );
  const checksumArchive = zip([{ name: 'SHA256SUMS', bytes: manifest, deflate: true, dataDescriptor: true }]);
  archives.set(databaseId, checksumArchive);
  artifacts.push(artifactRecord(databaseId, 'session-relay-checksums', checksumArchive));
  if (mutateArchive) mutateArchive({ archives, artifacts });
  return { archives, artifacts, jobs: nativeProducerJobs() };
}

function artifactRecord(id, name, archive) {
  return {
    id,
    name,
    expired: false,
    size_in_bytes: archive.length,
    digest: `sha256:${sha256(archive)}`,
    workflow_run: {
      id: RUN_ID,
      repository_id: REPOSITORY_DATABASE_ID,
      head_repository_id: REPOSITORY_DATABASE_ID,
      head_branch: `preflight/session-relay-0.12.0-${COMMIT.slice(0, 12)}`,
      head_sha: COMMIT,
    },
  };
}

function preflightAdapter(fixture) {
  return {
    apiJson(endpoint) {
      if (endpoint.endsWith(`/actions/runs/${RUN_ID}`))
        return {
          id: RUN_ID,
          event: 'workflow_dispatch',
          status: 'completed',
          conclusion: 'success',
          head_sha: COMMIT,
          head_branch: `preflight/session-relay-0.12.0-${COMMIT.slice(0, 12)}`,
          path: '.github/workflows/build-binaries.yml',
          run_attempt: RUN_ATTEMPT,
          workflow_id: WORKFLOW_ID,
          repository: { full_name: REPOSITORY_ID, id: REPOSITORY_DATABASE_ID },
          head_repository: { full_name: REPOSITORY_ID, id: REPOSITORY_DATABASE_ID },
        };
      if (endpoint.endsWith(`/actions/workflows/${WORKFLOW_ID}`))
        return { id: WORKFLOW_ID, path: '.github/workflows/build-binaries.yml', state: 'active' };
      if (endpoint.includes('/contents/.github/workflows/build-binaries.yml?ref='))
        return { type: 'file', path: '.github/workflows/build-binaries.yml', sha: WORKFLOW_BLOB };
      if (endpoint.endsWith(`/actions/runs/${RUN_ID}/jobs?filter=latest&per_page=100`))
        return { total_count: fixture.jobs.length, jobs: fixture.jobs };
      if (endpoint.endsWith(`/actions/runs/${RUN_ID}/artifacts?per_page=100`))
        return { total_count: fixture.artifacts.length, artifacts: fixture.artifacts };
      assert.fail(`unexpected API endpoint ${endpoint}`);
    },
    downloadArtifact({ artifactId, destination }) {
      const bytes = fixture.archives.get(artifactId);
      assert.ok(bytes, `download requested verified artifact database ID ${artifactId}`);
      fs.writeFileSync(destination, bytes, { flag: 'wx', mode: 0o600 });
    },
    now() {
      return '2026-07-17T18:00:00.000Z';
    },
  };
}

function runPreflight(temp, fixture = artifactFixture()) {
  const workspace = path.join(temp, `artifacts-${fs.readdirSync(temp).length}`);
  fs.mkdirSync(workspace, { mode: 0o700 });
  const receiptOut = path.join(temp, `preflight-${fs.readdirSync(temp).length}.json`);
  const result = verifyPreflight(
    { runId: RUN_ID, expectedCommit: COMMIT, artifacts: workspace, receiptOut },
    preflightAdapter(fixture),
  );
  return { ...result, workspace, receiptOut };
}

function expectReject(label, fn, pattern) {
  assert.throws(fn, pattern, label);
}

function extractDocksRedReceipt() {
  const active = path.resolve('docs/plans/active/session-relay-prebuilt-cli-distribution.md');
  const finished = fs
    .readdirSync(path.resolve('docs/plans/finished'))
    .filter((name) => /^\d{4}-\d{2}-\d{2}-session-relay-prebuilt-cli-distribution\.md$/.test(name))
    .sort()
    .reverse()
    .map((name) => path.resolve('docs/plans/finished', name));
  const candidates = fs.existsSync(active) ? [active, ...finished] : finished;
  for (const candidate of candidates) {
    const plan = fs.readFileSync(candidate, 'utf8');
    const match = plan.match(/^- Docks TDD-red receipt JCS bytes: (\{.*\})$/m);
    if (match) return JSON.parse(match[1]);
  }
  assert.fail('a tracked source-preparation plan carries the real capture-helper receipt');
}

function testClosedValidators(preflight) {
  const docksRed = extractDocksRedReceipt();
  validateTddRedReceipt(docksRed, { repositoryId: REPOSITORY_ID });
  expectReject(
    'TDD-red unknown property',
    () => validateTddRedReceipt({ ...docksRed, injected: true }, { repositoryId: REPOSITORY_ID }),
    /unknown|missing/i,
  );
  expectReject(
    'TDD-red must prove red',
    () => validateTddRedReceipt({ ...docksRed, exit_code: 0 }, { repositoryId: REPOSITORY_ID }),
    /nonzero|exit/i,
  );
  expectReject(
    'TDD-red repository substitution',
    () => validateTddRedReceipt(docksRed, { repositoryId: 'DocksDocks/public' }),
    /repository/i,
  );

  validateProducerPreflightReceipt(preflight.receipt, { sourceCommit: COMMIT });
  expectReject(
    'preflight source substitution',
    () =>
      validateProducerPreflightReceipt(
        { ...preflight.receipt, source_commit: '0'.repeat(40) },
        { sourceCommit: COMMIT },
      ),
    /source|commit/i,
  );
  expectReject(
    'preflight ref substitution',
    () =>
      validateProducerPreflightReceipt(
        { ...preflight.receipt, validation_ref: 'refs/heads/preflight/substituted' },
        { sourceCommit: COMMIT },
      ),
    /ref|workflow|identity/i,
  );
  expectReject(
    'preflight unknown property',
    () => validateProducerPreflightReceipt({ ...preflight.receipt, injected: true }, { sourceCommit: COMMIT }),
    /unknown|missing/i,
  );
}

function authoritativeCiWorkflow() {
  return fs.readFileSync(path.resolve('.github/workflows/ci.yml'), 'utf8');
}

function sourceCiFixture(
  temp,
  { receiptName = 'source-ci.json', workflowBytes = Buffer.from(authoritativeCiWorkflow()) } = {},
) {
  const receiptOut = path.join(temp, receiptName);
  const requiredSteps = [
    'setup Node 24',
    'enable corepack',
    'install pnpm dependencies (--frozen-lockfile; yaml + lockfile-pinned claude-code)',
    'materialize claude-code binary (allowBuilds denies it by default)',
    'add node_modules/.bin to PATH (so ci.mjs finds the pinned claude)',
    'provision Rust 1.85.0 with musl for the session-relay host leg',
    'run the authoritative gate (scripts/ci.mjs)',
  ];
  const jobs = [
    {
      id: 9000,
      run_id: 991,
      run_attempt: 1,
      head_sha: COMMIT,
      name: 'validation-shards',
      status: 'completed',
      conclusion: 'skipped',
      started_at: '2026-07-17T18:00:00Z',
      completed_at: '2026-07-17T18:00:00Z',
      steps: [],
    },
    {
      id: 9001,
      run_id: 991,
      run_attempt: 1,
      head_sha: COMMIT,
      name: 'validate (scripts/ci.mjs)',
      status: 'completed',
      conclusion: 'success',
      started_at: '2026-07-17T18:00:00Z',
      completed_at: '2026-07-17T18:10:00Z',
      steps: requiredSteps.map((name, index) => ({
        name,
        number: [3, 5, 9, 11, 12, 13, 14][index],
        status: 'completed',
        conclusion: 'success',
      })),
    },
  ];
  const logEndpoints = [];
  const adapter = {
    apiJson(endpoint) {
      if (endpoint.endsWith('/actions/runs/991'))
        return {
          id: 991,
          run_attempt: 1,
          event: 'workflow_dispatch',
          status: 'completed',
          conclusion: 'success',
          head_sha: COMMIT,
          head_branch: `preflight/session-relay-0.12.0-${COMMIT.slice(0, 12)}`,
          path: '.github/workflows/ci.yml',
          run_started_at: '2026-07-17T18:00:00Z',
          updated_at: '2026-07-17T18:10:00Z',
          repository: { full_name: REPOSITORY_ID },
          head_repository: { full_name: REPOSITORY_ID },
        };
      if (endpoint.includes('/contents/.github/workflows/ci.yml?ref='))
        return {
          type: 'file',
          path: '.github/workflows/ci.yml',
          sha: sha256GitBlob(workflowBytes),
          encoding: 'base64',
          content: workflowBytes.toString('base64'),
        };
      if (endpoint.endsWith('/actions/runs/991/jobs?per_page=100')) return { total_count: jobs.length, jobs };
      assert.fail(`unexpected source-CI endpoint ${endpoint}`);
    },
    apiBytes(endpoint) {
      logEndpoints.push(endpoint);
      assert.equal(endpoint, `repos/${REPOSITORY_ID}/actions/jobs/9001/logs`);
      return Buffer.from('authoritative job log\n');
    },
    now() {
      return '2026-07-17T18:11:00.000Z';
    },
  };
  const result = verifySourceCi(
    new Map([
      ['run-id', '991'],
      ['expected-commit', COMMIT],
      ['receipt-out', receiptOut],
    ]),
    adapter,
  );
  return { result, adapter, receiptOut, jobs, logEndpoints };
}

function sha256GitBlob(bytes) {
  return createHash('sha1')
    .update(Buffer.from(`blob ${bytes.length}\0`))
    .update(bytes)
    .digest('hex');
}

function testSourceCi(temp) {
  const fixture = sourceCiFixture(temp);
  validateSourceCiReceipt(fixture.result.receipt, { sourceCommit: COMMIT });
  assert.equal(fixture.result.receipt.jobs_sha256, sha256(Buffer.from(jcs(fixture.jobs))));
  assert.equal(
    fixture.result.receipt.logs_sha256,
    sha256(
      Buffer.from(
        jcs([
          {
            job_database_id: 9001,
            sha256: sha256(Buffer.from('authoritative job log\n')),
          },
        ]),
      ),
    ),
  );
  assert.deepEqual(fixture.logEndpoints, [`repos/${REPOSITORY_ID}/actions/jobs/9001/logs`]);
  const multipleSkippedShardJobs = structuredClone(fixture.jobs);
  const additionalSkippedShardJob = { ...structuredClone(multipleSkippedShardJobs[0]), id: 9002 };
  additionalSkippedShardJob.steps = [{ name: 'not started', number: 1, status: 'queued', conclusion: null }];
  multipleSkippedShardJobs.splice(1, 0, additionalSkippedShardJob);
  const multipleSkippedShardAdapter = {
    ...fixture.adapter,
    apiJson(endpoint) {
      const value = fixture.adapter.apiJson(endpoint);
      return endpoint.endsWith('/jobs?per_page=100')
        ? { total_count: multipleSkippedShardJobs.length, jobs: multipleSkippedShardJobs }
        : value;
    },
  };
  const multipleSkippedShardResult = verifySourceCi(
    new Map([
      ['run-id', '991'],
      ['expected-commit', COMMIT],
      ['receipt-out', path.join(temp, 'source-ci-multiple-skipped-shards.json')],
    ]),
    multipleSkippedShardAdapter,
  );
  assert.equal(multipleSkippedShardResult.receipt.jobs_sha256, sha256(Buffer.from(jcs(multipleSkippedShardJobs))));
  assert.equal(multipleSkippedShardResult.receipt.logs_sha256, fixture.result.receipt.logs_sha256);
  assert.deepEqual(fixture.logEndpoints, [
    `repos/${REPOSITORY_ID}/actions/jobs/9001/logs`,
    `repos/${REPOSITORY_ID}/actions/jobs/9001/logs`,
  ]);
  let jobsAdversary = 0;
  const expectJobsReject = (label, mutate, pattern) => {
    const jobs = structuredClone(fixture.jobs);
    mutate(jobs);
    const adapter = {
      ...fixture.adapter,
      apiJson(endpoint) {
        const value = fixture.adapter.apiJson(endpoint);
        return endpoint.endsWith('/jobs?per_page=100') ? { total_count: jobs.length, jobs } : value;
      },
    };
    expectReject(
      label,
      () =>
        verifySourceCi(
          new Map([
            ['run-id', '991'],
            ['expected-commit', COMMIT],
            ['receipt-out', path.join(temp, `source-ci-jobs-adversary-${++jobsAdversary}.json`)],
          ]),
          adapter,
        ),
      pattern,
    );
  };
  const changedCommit = { ...fixture.result.receipt, source_commit: 'f'.repeat(40) };
  expectReject(
    'source-CI commit substitution',
    () => validateSourceCiReceipt(changedCommit, { sourceCommit: COMMIT }),
    /source|commit/i,
  );
  const changedBlob = structuredClone(fixture.result.receipt);
  changedBlob.workflow.file_blob_id = '0'.repeat(40);
  expectReject(
    'source-CI workflow blob substitution',
    () => validateSourceCiReceipt(changedBlob, { sourceCommit: COMMIT }),
    /workflow|blob/i,
  );

  expectJobsReject(
    'failed required CI step',
    (jobs) => {
      jobs
        .find(({ name }) => name === 'validate (scripts/ci.mjs)')
        .steps.find(({ name }) => name.startsWith('run the authoritative')).conclusion = 'failure';
    },
    /step|conclusion|success/i,
  );
  expectJobsReject(
    'successful validation shard row',
    (jobs) => {
      jobs[0].conclusion = 'success';
    },
    /validation-shards|skipped|non-authoritative/i,
  );
  expectJobsReject(
    'failed validation shard row',
    (jobs) => {
      jobs[0].conclusion = 'failure';
    },
    /validation-shards|skipped|non-authoritative/i,
  );
  expectJobsReject(
    'in-progress validation shard row',
    (jobs) => {
      jobs[0].status = 'in_progress';
      jobs[0].conclusion = null;
    },
    /validation-shards|skipped|non-authoritative/i,
  );
  expectJobsReject(
    'renamed skipped validation shard row',
    (jobs) => {
      jobs[0].name = 'validation shard (core)';
    },
    /validation-shards|skipped|non-authoritative/i,
  );
  expectJobsReject(
    'skipped validation shard with completed step evidence',
    (jobs) => {
      jobs[0].steps.push({ name: 'unexpected evidence', number: 1, status: 'completed', conclusion: 'success' });
    },
    /validation-shards|steps|evidence|non-authoritative/i,
  );
  expectJobsReject(
    'missing skipped validation shard row',
    (jobs) => {
      jobs.splice(0, 1);
    },
    /validation-shards|authoritative job/i,
  );
  expectJobsReject(
    'extra authoritative CI job row',
    (jobs) => {
      jobs.push({ ...structuredClone(jobs[1]), id: 9002 });
    },
    /exactly one authoritative job/i,
  );
  expectJobsReject(
    'unrecognized extra CI job row',
    (jobs) => {
      jobs.push({ ...structuredClone(jobs[0]), id: 9002, name: 'unrecognized-job' });
    },
    /validation-shards|skipped|non-authoritative/i,
  );
  const noOpWorkflow = authoritativeCiWorkflow().replace(
    '            node scripts/ci.mjs\n',
    "            printf 'node scripts/ci.mjs\\n'\n",
  );
  expectReject(
    'source-CI no-op command with matching marker text',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'noop-source-ci.json',
        workflowBytes: Buffer.from(noOpWorkflow),
      }),
    /workflow|authoritative|command|definition/i,
  );
  const checkoutOverride = authoritativeCiWorkflow().replace(`ref: \${{ github.sha }}`, 'ref: refs/heads/substituted');
  expectReject(
    'source-CI checkout override',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'checkout-source-ci.json',
        workflowBytes: Buffer.from(checkoutOverride),
      }),
    /checkout|workflow|definition|ref/i,
  );
  const credentialOverride = authoritativeCiWorkflow().replace(
    'persist-credentials: false',
    'persist-credentials: true',
  );
  expectReject(
    'source-CI credential persistence override',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'credentials-source-ci.json',
        workflowBytes: Buffer.from(credentialOverride),
      }),
    /checkout|workflow|definition|credential/i,
  );
  const unguardedValidateCheckout = authoritativeCiWorkflow().replace(
    `      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd # v6.0.2
        if: github.event_name != 'pull_request'
        with:`,
    `      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd # v6.0.2
        with:`,
  );
  expectReject(
    'source-CI validate checkout without pull-request guard',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'unguarded-checkout-source-ci.json',
        workflowBytes: Buffer.from(unguardedValidateCheckout),
      }),
    /workflow|validate|definition|pull.request|condition/i,
  );
  const unguardedInstall = authoritativeCiWorkflow().replace(
    `      - name: "install pnpm dependencies (--frozen-lockfile; yaml + lockfile-pinned claude-code)"
        if: github.event_name != 'pull_request'
        run: pnpm install --frozen-lockfile`,
    `      - name: "install pnpm dependencies (--frozen-lockfile; yaml + lockfile-pinned claude-code)"
        run: pnpm install --frozen-lockfile`,
  );
  expectReject(
    'source-CI validate install without pull-request guard',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'unguarded-install-source-ci.json',
        workflowBytes: Buffer.from(unguardedInstall),
      }),
    /workflow|validate|definition|pull.request|condition/i,
  );
  const relaxedCargoCondition = authoritativeCiWorkflow().replace(
    `      - name: "cache Cargo dependencies and target outputs"
        if: github.event_name != 'pull_request' && (github.event_name != 'push' || steps.target.outputs.needs_rust == 'true')`,
    `      - name: "cache Cargo dependencies and target outputs"
        if: github.event_name != 'push' || steps.target.outputs.needs_rust == 'true'`,
  );
  expectReject(
    'source-CI validate Cargo cache with relaxed pull-request guard',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'relaxed-cargo-condition-source-ci.json',
        workflowBytes: Buffer.from(relaxedCargoCondition),
      }),
    /workflow|validate|definition|pull.request|condition/i,
  );
  const pullRequestAuthoritativeGate = authoritativeCiWorkflow().replace(
    `      - name: "run the authoritative gate (scripts/ci.mjs)"
        if: github.event_name != 'pull_request'`,
    `      - name: "run the authoritative gate (scripts/ci.mjs)"
        if: always()`,
  );
  expectReject(
    'source-CI authoritative gate allowed on pull request',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'pull-request-authoritative-gate-source-ci.json',
        workflowBytes: Buffer.from(pullRequestAuthoritativeGate),
      }),
    /workflow|validate|authoritative|definition|pull.request|condition/i,
  );

  const cacheOverride = authoritativeCiWorkflow().replace(
    'run: pnpm config set store-dir "$HOME/.pnpm-store"',
    'run: printf "skip deterministic store\\n"',
  );
  expectReject(
    'source-CI non-required job step override',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'cache-source-ci.json',
        workflowBytes: Buffer.from(cacheOverride),
      }),
    /workflow|job|definition|store/i,
  );
  const mutationLaneOverride = authoritativeCiWorkflow().replace(
    'lane: [core, relay]',
    'lane: [core, relay, mutations]',
  );
  expectReject(
    'source-CI mutations validation lane restored',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'mutations-lane-source-ci.json',
        workflowBytes: Buffer.from(mutationLaneOverride),
      }),
    /workflow|validation-shards|definition|matrix/i,
  );
  const conditionalShardInstall = authoritativeCiWorkflow().replace(
    `      - name: "install pnpm dependencies (--frozen-lockfile; yaml + lockfile-pinned claude-code)"
        run: pnpm install --frozen-lockfile`,
    `      - name: "install pnpm dependencies (--frozen-lockfile; yaml + lockfile-pinned claude-code)"
        if: matrix.lane != 'mutations'
        run: pnpm install --frozen-lockfile`,
  );
  expectReject(
    'source-CI validation dependency install made lane-conditional',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'conditional-shard-install-source-ci.json',
        workflowBytes: Buffer.from(conditionalShardInstall),
      }),
    /workflow|validation-shards|definition|dependenc|condition|install/i,
  );
  const coreRustCacheOverride = authoritativeCiWorkflow().replace(
    `      - name: "cache Cargo dependencies and target outputs"
        if: matrix.lane == 'relay'`,
    `      - name: "cache Cargo dependencies and target outputs"
        if: matrix.lane == 'core'`,
  );
  expectReject(
    'source-CI validation Cargo cache moved from Relay',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'core-rust-cache-source-ci.json',
        workflowBytes: Buffer.from(coreRustCacheOverride),
      }),
    /workflow|validation-shards|definition|Cargo|Rust|relay|condition/i,
  );
  const shardConditionOverride = authoritativeCiWorkflow().replace(
    "if: github.event_name == 'pull_request'",
    "if: github.event_name != 'push'",
  );
  expectReject(
    'source-CI validation shard event override',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'shard-event-source-ci.json',
        workflowBytes: Buffer.from(shardConditionOverride),
      }),
    /workflow|validation-shards|definition|event/i,
  );
  const shardCommandOverride = authoritativeCiWorkflow().replace(
    `run: node scripts/ci.mjs --lane "\${{ matrix.lane }}"`,
    'run: node scripts/ci.mjs',
  );
  expectReject(
    'source-CI validation shard command override',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'shard-command-source-ci.json',
        workflowBytes: Buffer.from(shardCommandOverride),
      }),
    /workflow|validation-shards|definition|command/i,
  );
  const joinDependencyOverride = authoritativeCiWorkflow().replace(
    'needs: validation-shards',
    'needs: substituted-job',
  );
  expectReject(
    'source-CI validate join dependency override',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'join-dependency-source-ci.json',
        workflowBytes: Buffer.from(joinDependencyOverride),
      }),
    /workflow|validate|definition|needs/i,
  );
  const joinAssertionOverride = authoritativeCiWorkflow().replace(
    'echo "validation shards result: $VALIDATION_SHARDS_RESULT" >&2',
    'echo "validation shards ignored" >&2',
  );
  expectReject(
    'source-CI validation shard assertion override',
    () =>
      sourceCiFixture(temp, {
        receiptName: 'join-assertion-source-ci.json',
        workflowBytes: Buffer.from(joinAssertionOverride),
      }),
    /workflow|validate|definition|assert/i,
  );

  let runReads = 0;
  const racingAdapter = {
    ...fixture.adapter,
    apiJson(endpoint) {
      const value = fixture.adapter.apiJson(endpoint);
      if (endpoint.endsWith('/actions/runs/991') && ++runReads === 2) return { ...value, run_attempt: 2 };
      return value;
    },
  };
  expectReject(
    'source-CI run attempt race',
    () =>
      verifySourceCi(
        new Map([
          ['run-id', '991'],
          ['expected-commit', COMMIT],
          ['receipt-out', path.join(temp, 'racing-source-ci.json')],
        ]),
        racingAdapter,
      ),
    /attempt|changed|race|identity/i,
  );
  return fixture;
}

function testArtifactAdversaries(temp) {
  const digestFixture = artifactFixture();
  digestFixture.artifacts[0].digest = `sha256:${'0'.repeat(64)}`;
  expectReject('API digest mismatch', () => runPreflight(temp, digestFixture), /digest/i);

  const traversal = artifactFixture({
    mutateArchive({ archives, artifacts }) {
      const id = artifacts[0].id;
      const bytes = zip([{ name: '../substitute', bytes: Buffer.from('owned?') }]);
      archives.set(id, bytes);
      Object.assign(artifacts[0], artifactRecord(id, artifacts[0].name, bytes));
    },
  });
  expectReject('archive traversal', () => runPreflight(temp, traversal), /archive|path|traversal/i);

  const duplicate = artifactFixture({
    mutateArchive({ archives, artifacts }) {
      const id = artifacts[0].id;
      const bytes = zip([
        { name: 'same', bytes: Buffer.from('a') },
        { name: 'same', bytes: Buffer.from('b') },
      ]);
      archives.set(id, bytes);
      Object.assign(artifacts[0], artifactRecord(id, artifacts[0].name, bytes));
    },
  });
  expectReject('duplicate archive path', () => runPreflight(temp, duplicate), /duplicate/i);

  const symlink = artifactFixture({
    mutateArchive({ archives, artifacts }) {
      const id = artifacts[0].id;
      const bytes = zip([{ name: 'link', bytes: Buffer.from('/tmp/attacker'), mode: 'symlink' }]);
      archives.set(id, bytes);
      Object.assign(artifacts[0], artifactRecord(id, artifacts[0].name, bytes));
    },
  });
  expectReject('symlink archive member', () => runPreflight(temp, symlink), /symlink/i);
  const expansion = artifactFixture({
    mutateArchive({ archives, artifacts }) {
      const id = artifacts[0].id;
      const bytes = zip([
        { name: 'expansion', bytes: Buffer.alloc(2 * 1024 * 1024, 0x41), deflate: true, declaredSize: 1 },
      ]);
      archives.set(id, bytes);
      Object.assign(artifacts[0], artifactRecord(id, artifacts[0].name, bytes));
    },
  });
  expectReject('archive expansion beyond declared size', () => runPreflight(temp, expansion), /could not be inflated/i);

  const fixture = artifactFixture();
  const workspace = path.join(temp, 'substituted-workspace');
  fs.mkdirSync(workspace, { mode: 0o700 });
  fs.writeFileSync(path.join(workspace, 'attacker-controlled'), 'substitute');
  expectReject(
    'pre-populated artifact substitution workspace',
    () =>
      verifyPreflight(
        {
          runId: RUN_ID,
          expectedCommit: COMMIT,
          artifacts: workspace,
          receiptOut: path.join(temp, 'substitution.json'),
        },
        preflightAdapter(fixture),
      ),
    /empty|workspace|owned/i,
  );
}

function testNativeProducerWorkflow() {
  const workflow = parseYaml(fs.readFileSync('.github/workflows/build-binaries.yml', 'utf8'));
  const build = workflow.jobs.build;
  assert.equal(build.needs, 'identity');
  assert.equal(build['runs-on'], '${{ matrix.runner }}');
  assert.deepEqual(
    build.strategy.matrix.include.map(({ runner, runner_os, runner_arch, target, asset }) => ({
      runner,
      runner_os,
      runner_arch,
      target,
      asset,
    })),
    [
      {
        runner: 'ubuntu-24.04',
        runner_os: 'Linux',
        runner_arch: 'X64',
        target: 'x86_64-unknown-linux-musl',
        asset: 'session-relay-x86_64-unknown-linux-musl',
      },
      {
        runner: 'ubuntu-24.04-arm',
        runner_os: 'Linux',
        runner_arch: 'ARM64',
        target: 'aarch64-unknown-linux-musl',
        asset: 'session-relay-aarch64-unknown-linux-musl',
      },
      {
        runner: 'macos-15-intel',
        runner_os: 'macOS',
        runner_arch: 'X64',
        target: 'x86_64-apple-darwin',
        asset: 'session-relay-x86_64-apple-darwin',
      },
      {
        runner: 'macos-15',
        runner_os: 'macOS',
        runner_arch: 'ARM64',
        target: 'aarch64-apple-darwin',
        asset: 'session-relay-aarch64-apple-darwin',
      },
    ],
  );
  const names = build.steps.map((step) => step.name ?? step.uses);
  const required = [
    'build locked native release',
    'prove Linux managed-workspace custody',
    'prove macOS managed-workspace admission STOP',
    'smoke explicit fresh Linux workspace binary',
    'attest native release binary',
    'upload stable executable and canonical attestation',
  ];
  assert.deepEqual(
    required.map((name) => names.indexOf(name)),
    [...required.keys()].map((offset) => names.indexOf(required[0]) + offset),
    'native producer evidence steps must be present, contiguous, and ordered',
  );
  const byName = new Map(build.steps.map((step) => [step.name, step]));
  assert.equal(byName.get('prove Linux managed-workspace custody').if, "runner.os == 'Linux'");
  assert.match(
    byName.get('prove Linux managed-workspace custody').run,
    /SESSION_RELAY_TEST_CGROUP_ROOT=[\s\S]*workspace_lease_process[\s\S]*linux_cgroup_pidfd_guardian_kills_hostile_descendants[\s\S]*--exact --nocapture/,
  );
  assert.equal(byName.get('prove macOS managed-workspace admission STOP').if, "runner.os == 'macOS'");
  assert.match(
    byName.get('prove macOS managed-workspace admission STOP').run,
    /stat -f %T[\s\S]*workspace_lease_process[\s\S]*macos_process_group_recursive_guardian_kills_hostile_descendants[\s\S]*--exact --nocapture/,
  );
  assert.equal(byName.get('smoke explicit fresh Linux workspace binary').if, "runner.os == 'Linux'");
  assert.match(
    byName.get('smoke explicit fresh Linux workspace binary').run,
    /--case single-session-compat --bin "\$bin"[\s\S]*--case docs-contract --bin "\$bin"/,
  );
  assert.deepEqual(workflow.jobs.aggregate.needs, ['identity', 'build']);
  assert.deepEqual(workflow.jobs.publish.needs, ['identity', 'aggregate']);
}

function testNativeProducerEvidence(temp) {
  let adversary = 0;
  const expectJobsReject = (label, mutate, pattern) => {
    const fixture = artifactFixture();
    mutate(fixture.jobs);
    expectReject(label, () => runPreflight(temp, fixture), pattern);
    adversary += 1;
  };

  expectJobsReject('missing native job', (jobs) => jobs.pop(), /native job|exactly once/i);
  expectJobsReject(
    'cross-built native job',
    (jobs) => {
      jobs[0].labels = ['ubuntu-24.04-arm'];
    },
    /did not run|runner/i,
  );
  expectJobsReject(
    'skipped Linux custody',
    (jobs) => {
      jobs[0].steps.find(({ name }) => name === 'prove Linux managed-workspace custody').conclusion = 'skipped';
    },
    /custody|conclusion/i,
  );
  expectJobsReject(
    'failed macOS negative admission',
    (jobs) => {
      jobs[2].steps.find(({ name }) => name === 'prove macOS managed-workspace admission STOP').conclusion = 'failure';
    },
    /admission STOP|conclusion/i,
  );
  expectJobsReject(
    'macOS support substitution',
    (jobs) => {
      jobs[2].steps.find(({ name }) => name === 'prove macOS managed-workspace admission STOP').name =
        'prove macOS managed-workspace custody';
    },
    /admission STOP|exactly once/i,
  );
  expectJobsReject(
    'self-hosted runner substitution',
    (jobs) => {
      jobs[0].runner_group_id = 5;
      jobs[0].runner_group_name = 'default';
    },
    /runner_group/i,
  );
  expectJobsReject(
    'skipped fresh-binary smoke',
    (jobs) => {
      jobs[1].steps.find(({ name }) => name === 'smoke explicit fresh Linux workspace binary').conclusion = 'skipped';
    },
    /smoke|conclusion/i,
  );
  expectJobsReject(
    'reordered attestation before evidence',
    (jobs) => {
      const steps = jobs[0].steps;
      const attestation = steps.splice(
        steps.findIndex(({ name }) => name === 'attest native release binary'),
        1,
      )[0];
      steps.splice(1, 0, attestation);
    },
    /reordered/i,
  );
  assert.equal(adversary, 8);
}

function writeReceipt(file, value) {
  const bytes = Buffer.from(jcs(value));
  fs.writeFileSync(file, bytes, { flag: 'wx', mode: 0o600 });
  return sha256(bytes);
}

function capturePublicRed(temp) {
  const repo = path.join(temp, 'public-red-repo');
  const testPath = 'plugins/session-relay/test/selftest.mjs';
  const producerPath = 'scripts/capture-tdd-red.mjs';
  for (const relativePath of [testPath, producerPath]) {
    const fixturePath = path.join(repo, ...relativePath.split('/'));
    fs.mkdirSync(path.dirname(fixturePath), { recursive: true });
    fs.copyFileSync(path.resolve(relativePath), fixturePath);
  }
  for (const args of [
    ['init', '--quiet'],
    ['config', 'user.name', 'Session Relay Test'],
    ['config', 'user.email', 'relay-test@example.invalid'],
    ['add', testPath, producerPath],
    ['commit', '--quiet', '-m', 'fixture'],
  ]) {
    const git = spawnSync('git', args, { cwd: repo, encoding: 'utf8', shell: false });
    assert.equal(git.status, 0, git.stderr);
  }
  const head = spawnSync('git', ['rev-parse', 'HEAD'], { cwd: repo, encoding: 'utf8', shell: false }).stdout.trim();
  const receiptOut = path.join(temp, 'public-red.json');
  const result = spawnSync(
    'node',
    [
      path.join(repo, ...producerPath.split('/')),
      '--repo',
      repo,
      '--repository-id',
      'DocksDocks/public',
      '--pre-production-commit',
      head,
      '--test',
      testPath,
      '--receipt-out',
      receiptOut,
      '--',
      'node',
      '-e',
      'process.exit(7)',
    ],
    { encoding: 'utf8' },
  );
  assert.equal(result.status, 0, result.stderr);
  return { value: JSON.parse(fs.readFileSync(receiptOut, 'utf8')), digest: result.stdout.trim(), path: receiptOut };
}

function preparationAdapter({
  plan,
  sourcePlan,
  docksRed,
  publicRed,
  publicPlan,
  sourceCi,
  unrelated = false,
  unrelatedExecutionBase = false,
  publicTamper = null,
  status = '',
}) {
  const evidenceCommit = '234567890abcdef1234567890abcdef123456789';
  return {
    repoRoot: process.cwd(),
    git(args) {
      const joined = args.join(' ');
      if (joined === `rev-parse ${COMMIT}^{commit}`) return COMMIT;
      if (joined === 'rev-parse HEAD^{commit}') return evidenceCommit;
      if (args[0] === 'diff' && args[1] === '--name-only') return '';
      if (args[0] === 'status') return status;
      if (args[0] === 'merge-base' && args[1] === '--is-ancestor') {
        if (unrelated && args[2] === COMMIT && args[3] === evidenceCommit) throw new Error('not an ancestor');
        if (unrelatedExecutionBase && args[2] === '4567890abcdef1234567890abcdef12345678901' && args[3] === COMMIT)
          throw new Error('not an ancestor');
        return '';
      }
      if (joined === `rev-parse ${COMMIT}:.github/workflows/build-binaries.yml`) return WORKFLOW_BLOB;
      if (joined === `rev-parse ${COMMIT}:.github/workflows/ci.yml`) return sourceCi.workflow.file_blob_id;
      if (joined === `show ${COMMIT}:docs/plans/active/session-relay-prebuilt-cli-distribution.md`)
        return Buffer.from(sourcePlan);
      for (const receipt of [docksRed, publicRed]) {
        for (const test of receipt.test_paths) {
          if (
            joined === `rev-parse ${receipt.pre_production_commit}:${test.path}` ||
            joined === `rev-parse ${COMMIT}:${test.path}`
          )
            return test.blob_id;
        }
        if (
          joined === `rev-parse ${receipt.pre_production_commit}:${receipt.producer.path}` ||
          joined === `rev-parse ${COMMIT}:${receipt.producer.path}`
        )
          return receipt.producer.blob_id;
      }
      assert.fail(`unexpected git adapter call: ${joined}`);
    },
    inspectPublic(input) {
      assert.equal(input.remote, 'https://github.com/DocksDocks/public.git');
      assert.equal(input.ref, 'refs/heads/preflight/session-relay-cli-0.9.0');
      assert.equal(input.commit, '34567890abcdef1234567890abcdef1234567890');
      assert.equal(input.planPath, 'docs/plans/active/session-relay-cli-installation.md');
      assert.equal(input.redCommit, publicRed.pre_production_commit);
      const testBlobs = publicRed.test_paths.map((item, index) => ({
        path: item.path,
        red_blob_id: item.blob_id,
        implementation_blob_id: publicTamper === 'blob' && index === 0 ? '0'.repeat(40) : item.blob_id,
      }));
      return {
        resolved_commit: publicTamper === 'ref' ? '0'.repeat(40) : input.commit,
        plan_bytes: Buffer.from(publicPlan),
        red_is_ancestor: true,
        test_blobs: testBlobs,
      };
    },
    run(executable, args) {
      const argv = [executable, ...args];
      if (executable === 'node' && args[0] === 'plugins/session-relay/test/companion-distribution-contract.mjs') {
        assert.deepEqual(args, [
          'plugins/session-relay/test/companion-distribution-contract.mjs',
          '--public-remote',
          'https://github.com/DocksDocks/public.git',
          '--public-ref',
          'refs/heads/preflight/session-relay-cli-0.9.0',
          '--public-commit',
          '34567890abcdef1234567890abcdef1234567890',
          '--detached-clone',
        ]);
      }
      const stdout =
        executable === 'git' && args[0] === 'ls-files'
          ? Buffer.from('plugins/session-relay/bin/relay\n')
          : Buffer.from(`${argv.join(' ')}: ok\n`);
      return { status: 0, stdout, stderr: Buffer.alloc(0) };
    },
    now() {
      return '2026-07-17T18:20:00.000Z';
    },
    readFile() {
      return Buffer.from(plan);
    },
  };
}

function testPreparationHandlers(temp, preflight, sourceCi) {
  const docksRed = extractDocksRedReceipt();
  const docksRedPath = path.join(temp, 'docks-red.json');
  const docksRedDigest = writeReceipt(docksRedPath, docksRed);
  const publicRed = capturePublicRed(temp);
  const companionReview = {
    input_sha256: '1'.repeat(64),
    outcome: 'single',
    phase: 'draft',
    reviewed_commit: '6'.repeat(40),
    reviewer: { company: 'openai', mode: 'fresh_subagent', verdict: 'ready' },
    schema: 1,
  };
  const companionReviewDigest = sha256(Buffer.from(jcs(companionReview)));
  const publicPlan = [
    '---',
    'status: blocked',
    'review_status: ready',
    `execution_base_commit: ${'5'.repeat(40)}`,
    'blocked_reason: "Awaiting the four independently hashed `session-relay--v0.12.0` production asset digests."',
    '---',
    '',
    `Review-receipt: ${jcs(companionReview)}`,
    '',
    '## Notes',
    '',
    `- Companion TDD-red receipt JCS bytes: ${jcs(publicRed.value)}`,
    `- Companion TDD-red receipt SHA-256: ${publicRed.digest}`,
    '- Status: blocked',
    '- Blocked reason: Awaiting the four independently hashed `session-relay--v0.12.0` production asset digests.',
    '',
  ].join('\n');
  const base = '4567890abcdef1234567890abcdef12345678901';
  const sourcePlan = [
    '---',
    `execution_base_commit: ${base}`,
    'status: ongoing',
    'review_status: null',
    '---',
    '',
    '## Notes',
    '',
    '- Companion repository ID: DocksDocks/public',
    '- Companion plan: `/home/vagrant/projects/public/docs/plans/active/session-relay-cli-installation.md`.',
    '- Companion validation ref: refs/heads/preflight/session-relay-cli-0.9.0',
    '- Companion implementation commit: 34567890abcdef1234567890abcdef1234567890',
    `- Companion plan input SHA-256: ${'1'.repeat(64)}`,
    `- Companion execution base commit: ${'5'.repeat(40)}`,
    `- Companion review receipt SHA-256: ${companionReviewDigest}`,
    `- Companion TDD-red receipt JCS bytes: ${jcs(publicRed.value)}`,
    `- Companion TDD-red receipt SHA-256: ${publicRed.digest}`,
    '- Companion status: blocked',
    '- Companion blocked reason: Awaiting the four independently hashed `session-relay--v0.12.0` production asset digests.',
    `- Docks TDD-red receipt JCS bytes: ${jcs(docksRed)}`,
    `- Docks TDD-red receipt SHA-256: ${docksRedDigest}`,
    `- TAG_COMMIT / SOURCE_COMMIT: ${COMMIT}`,
    '',
  ].join('\n');
  const candidateOut = path.join(temp, 'candidate.json');
  const options = new Map([
    ['source-commit', COMMIT],
    ['docks-red', docksRedPath],
    ['docks-red-sha256', docksRedDigest],
    ['public-red', publicRed.path],
    ['public-red-sha256', publicRed.digest],
    ['preflight', preflight.receiptOut],
    ['preflight-sha256', sha256(fs.readFileSync(preflight.receiptOut))],
    ['source-ci', sourceCi.receiptOut],
    ['source-ci-sha256', sha256(fs.readFileSync(sourceCi.receiptOut))],
    ['receipt-out', candidateOut],
  ]);
  const adapter = preparationAdapter({
    plan: sourcePlan,
    sourcePlan,
    docksRed,
    publicRed: publicRed.value,
    publicPlan,
    sourceCi: sourceCi.result.receipt,
  });
  const candidate = checkPrepared(options, adapter);
  assert.equal(candidate.receipt.source_commit, COMMIT);
  assert.deepEqual(
    candidate.receipt.checks.map(({ id }) => id),
    ['A1', 'A2', 'A3', 'A4', 'A5', 'A6'],
  );
  assert.equal(
    candidate.receipt.checks[0].steps[0].argv[1],
    'plugins/session-relay/test/companion-distribution-contract.mjs',
  );
  assert.deepEqual(candidate.receipt.checks[1].steps[0].argv, [
    'cargo',
    '+1.85.0',
    'build',
    '--manifest-path',
    'plugins/session-relay/rust/Cargo.toml',
    '--release',
    '--locked',
  ]);
  const dirtyPlanOptions = new Map(options);
  dirtyPlanOptions.set('receipt-out', path.join(temp, 'candidate-dirty-plan.json'));
  const dirtyPlan = checkPrepared(
    dirtyPlanOptions,
    preparationAdapter({
      plan: sourcePlan,
      sourcePlan,
      docksRed,
      publicRed: publicRed.value,
      publicPlan,
      sourceCi: sourceCi.result.receipt,
      status:
        '1 .M N... 100644 100644 100644 abcdef1 abcdef1 docs/plans/active/session-relay-prebuilt-cli-distribution.md\0',
    }),
  );
  assert.equal(dirtyPlan.receipt.source_commit, COMMIT);
  const misleadingPlanOptions = new Map(options);
  misleadingPlanOptions.set('receipt-out', path.join(temp, 'candidate-misleading-plan.json'));
  expectReject(
    'active-plan suffix substitution',
    () =>
      checkPrepared(
        misleadingPlanOptions,
        preparationAdapter({
          plan: sourcePlan,
          sourcePlan,
          docksRed,
          publicRed: publicRed.value,
          publicPlan,
          sourceCi: sourceCi.result.receipt,
          status:
            '1 M. N... 100644 100644 100644 abcdef1 abcdef1 x docs/plans/active/session-relay-prebuilt-cli-distribution.md\0',
        }),
      ),
    /working tree is not clean/i,
  );

  for (const publicTamper of ['ref', 'blob']) {
    const tamperedOptions = new Map(options);
    tamperedOptions.set('receipt-out', path.join(temp, `candidate-public-${publicTamper}.json`));
    expectReject(
      `companion public ${publicTamper} substitution`,
      () =>
        checkPrepared(
          tamperedOptions,
          preparationAdapter({
            plan: sourcePlan,
            sourcePlan,
            docksRed,
            publicRed: publicRed.value,
            publicPlan,
            sourceCi: sourceCi.result.receipt,
            publicTamper,
          }),
        ),
      /companion|public|ref|blob/i,
    );
  }

  const plan =
    `${sourcePlan}` +
    `- Producer preflight receipt JCS bytes: ${fs.readFileSync(preflight.receiptOut, 'utf8')}\n` +
    `- Producer preflight receipt SHA-256: ${sha256(fs.readFileSync(preflight.receiptOut))}\n` +
    `- Source CI receipt JCS bytes: ${fs.readFileSync(sourceCi.receiptOut, 'utf8')}\n` +
    `- Source CI receipt SHA-256: ${sha256(fs.readFileSync(sourceCi.receiptOut))}\n` +
    `- Source preparation candidate JCS bytes: ${fs.readFileSync(candidateOut, 'utf8')}\n` +
    `- Source preparation candidate SHA-256: ${sha256(fs.readFileSync(candidateOut))}\n`;
  const verifyOptions = new Map([['plan', 'docs/plans/active/session-relay-prebuilt-cli-distribution.md']]);
  const verified = verifyEmbedded(
    verifyOptions,
    preparationAdapter({
      plan,
      sourcePlan,
      docksRed,
      publicRed: publicRed.value,
      publicPlan,
      sourceCi: sourceCi.result.receipt,
    }),
  );
  assert.equal(verified.state.source_commit, COMMIT);
  const mismatchedCandidate = structuredClone(candidate.receipt);
  mismatchedCandidate.preflight.run_database_id += 1;
  const originalCandidateBytes = fs.readFileSync(candidateOut, 'utf8');
  const mismatchedCandidateBytes = jcs(mismatchedCandidate);
  const mismatchedPlan = plan
    .replace(
      `- Source preparation candidate JCS bytes: ${originalCandidateBytes}`,
      `- Source preparation candidate JCS bytes: ${mismatchedCandidateBytes}`,
    )
    .replace(
      `- Source preparation candidate SHA-256: ${sha256(Buffer.from(originalCandidateBytes))}`,
      `- Source preparation candidate SHA-256: ${sha256(Buffer.from(mismatchedCandidateBytes))}`,
    );
  expectReject(
    'embedded candidate copied summary substitution',
    () =>
      verifyEmbedded(
        verifyOptions,
        preparationAdapter({
          plan: mismatchedPlan,
          sourcePlan,
          docksRed,
          publicRed: publicRed.value,
          publicPlan,
          sourceCi: sourceCi.result.receipt,
        }),
      ),
    /candidate|preflight|summary|run/i,
  );

  expectReject(
    'unrelated evidence commit',
    () =>
      verifyEmbedded(
        verifyOptions,
        preparationAdapter({
          plan,
          sourcePlan,
          docksRed,
          publicRed: publicRed.value,
          publicPlan,
          sourceCi: sourceCi.result.receipt,
          unrelated: true,
        }),
      ),
    /ancestor|ancestry|evidence/i,
  );
  expectReject(
    'unrelated embedded execution base',
    () =>
      verifyEmbedded(
        verifyOptions,
        preparationAdapter({
          plan,
          sourcePlan,
          docksRed,
          publicRed: publicRed.value,
          publicPlan,
          sourceCi: sourceCi.result.receipt,
          unrelatedExecutionBase: true,
        }),
      ),
    /execution-base|ancestor|ancestry|source/i,
  );
  return { candidate, candidateOut, plan, sourcePlan };
}

function replaceReceiptIdentity(value, replacements) {
  if (typeof value === 'string') return replacements.get(value) ?? value;
  if (Array.isArray(value)) return value.map((item) => replaceReceiptIdentity(item, replacements));
  if (value !== null && typeof value === 'object') {
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [key, replaceReceiptIdentity(item, replacements)]),
    );
  }
  return value;
}

function completionTemplate() {
  const plan = fs.readFileSync('docs/plans/finished/2026-07-17-completion-reuse-terminal-lf.md', 'utf8');
  const match = plan.match(/^Completion-review-receipt: (\{.*\})$/m);
  assert.ok(match, 'a real plan-manager completion receipt is available as the producer template');
  return JSON.parse(match[1]);
}

function testCompletionBinding(temp, preparation) {
  const evidenceCommit = '234567890abcdef1234567890abcdef123456789';
  const shippedCommit = '567890abcdef1234567890abcdef123456789012';
  const currentHead = '67890abcdef1234567890abcdef1234567890123';
  const evidencePlan = preparation.plan
    .replace('status: ongoing', 'status: in_review')
    .replace('review_status: null', 'review_status: null');
  const inputSha256 = sha256(canonicalPlanView(Buffer.from(evidencePlan)));
  const template = completionTemplate();
  const completion = replaceReceiptIdentity(
    template,
    new Map([
      [template.reviewed_head, evidenceCommit],
      [template.plan_input_sha256, inputSha256],
    ]),
  );
  const finishedBody =
    evidencePlan
      .replace('status: in_review', 'status: finished')
      .replace('review_status: null', 'review_status: passed') +
    `\n## Review\n\nCompletion-review-receipt: ${jcs(completion)}\n`;
  const root = path.join(temp, 'completion-root');
  const finishedDirectory = path.join(root, 'docs/plans/finished');
  fs.mkdirSync(finishedDirectory, { recursive: true, mode: 0o700 });
  const makePlan = (date, body = finishedBody) => {
    const file = path.join(finishedDirectory, `${date}-session-relay-prebuilt-cli-distribution.md`);
    fs.writeFileSync(file, body);
    return file;
  };
  const adapter = ({
    ancestry = true,
    equivalent = true,
    sourceEvidenceClean = true,
    evidenceShippedClean = true,
    evidenceBody = evidencePlan,
    shippedBody = finishedBody,
    shippedPlanMatches = true,
    dirty = false,
    finishedDate = '2026-07-17',
    sourceBody = preparation.sourcePlan,
  } = {}) => ({
    repoRoot: root,
    git(args) {
      const joined = args.join(' ');
      if (joined === 'rev-parse HEAD^{commit}') return currentHead;
      if (
        joined ===
        `log -n1 --format=%H -- docs/plans/finished/${finishedDate}-session-relay-prebuilt-cli-distribution.md`
      )
        return shippedCommit;
      if (joined === `show ${evidenceCommit}:docs/plans/active/session-relay-prebuilt-cli-distribution.md`)
        return Buffer.from(evidenceBody);
      if (joined === `show ${COMMIT}:docs/plans/active/session-relay-prebuilt-cli-distribution.md`)
        return Buffer.from(sourceBody);
      if (
        joined ===
        `show ${shippedCommit}:docs/plans/finished/${finishedDate}-session-relay-prebuilt-cli-distribution.md`
      ) {
        return Buffer.from(shippedPlanMatches ? shippedBody : `${shippedBody}\nsubstituted\n`);
      }
      if (args[0] === 'status') return dirty ? ' M docs/plans/finished/substituted.md' : '';
      if (args[0] === 'merge-base' && args[1] === '--is-ancestor') {
        if (!ancestry) throw new Error('unrelated');
        return '';
      }
      if (args[0] === 'diff' && args[1] === '--name-only') {
        if (args[2] === COMMIT && args[3] === evidenceCommit)
          return sourceEvidenceClean
            ? 'docs/plans/active/session-relay-prebuilt-cli-distribution.md'
            : 'docs/plans/active/session-relay-prebuilt-cli-distribution.md\nscripts/transient.mjs';
        if (args[2] === evidenceCommit && args[3] === shippedCommit)
          return evidenceShippedClean
            ? `docs/plans/active/session-relay-prebuilt-cli-distribution.md\ndocs/plans/finished/${finishedDate}-session-relay-prebuilt-cli-distribution.md`
            : `docs/plans/active/session-relay-prebuilt-cli-distribution.md\ndocs/plans/finished/${finishedDate}-session-relay-prebuilt-cli-distribution.md\nscripts/transient.mjs`;
        return equivalent ? '' : 'scripts/substituted.mjs';
      }
      assert.fail(`unexpected completion git call: ${joined}`);
    },
    inspectPublic() {
      assert.fail('completion binding must not inspect public state');
    },
    run() {
      assert.fail('completion binding must not execute commands');
    },
    now() {
      return '2026-07-17T18:30:00.000Z';
    },
    readFile(file) {
      return fs.readFileSync(file);
    },
  });

  const finished = makePlan('2026-07-17');
  const proofOut = path.join(temp, 'source-proof.json');
  const result = bindCompletion(
    new Map([
      ['finished-plan', finished],
      ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
      ['receipt-out', proofOut],
    ]),
    adapter(),
  );
  validateSourcePreparationProof(result.receipt);
  const loaded = validateProof(
    new Map([
      ['source-proof', proofOut],
      ['source-proof-sha256', sha256(fs.readFileSync(proofOut))],
    ]),
  );
  assert.equal(loaded.value.evidence_commit, evidenceCommit);
  assert.equal(loaded.value.promoted_commit, shippedCommit);
  assert.deepEqual(result.receipt.candidate, preparation.candidate.receipt);

  // Model a candidate sealed before the raw-byte binder repair: its recorded
  // source_blob_sha256 is the hash of the TRIMMED `git show` adapter output,
  // while the binder now reads the raw blob (with its trailing newline).
  const legacyCandidate = structuredClone(preparation.candidate.receipt);
  legacyCandidate.plan.source_blob_sha256 = sha256(Buffer.from(preparation.sourcePlan.trim(), 'utf8'));
  const legacyCandidateBytes = jcs(legacyCandidate);
  const originalCandidateBytes = fs.readFileSync(preparation.candidateOut, 'utf8');
  const legacyPlanText = preparation.plan
    .replace(
      `- Source preparation candidate JCS bytes: ${originalCandidateBytes}`,
      `- Source preparation candidate JCS bytes: ${legacyCandidateBytes}\n`,
    )
    .replace(
      `- Source preparation candidate SHA-256: ${sha256(Buffer.from(originalCandidateBytes))}`,
      `- Source preparation candidate SHA-256: ${sha256(Buffer.from(legacyCandidateBytes))}`,
    );
  const legacyEvidencePlan = legacyPlanText
    .replace('status: ongoing', 'status: in_review')
    .replace('review_status: null', 'review_status: null');
  const legacyInputSha256 = sha256(canonicalPlanView(Buffer.from(legacyEvidencePlan)));
  const legacyTemplate = completionTemplate();
  const legacyCompletion = replaceReceiptIdentity(
    legacyTemplate,
    new Map([
      [legacyTemplate.reviewed_head, evidenceCommit],
      [legacyTemplate.plan_input_sha256, legacyInputSha256],
    ]),
  );
  const legacyFinishedBody =
    legacyEvidencePlan
      .replace('status: in_review', 'status: finished')
      .replace('review_status: null', 'review_status: passed') +
    `\n## Review\n\nCompletion-review-receipt: ${jcs(legacyCompletion)}\n`;
  const legacyFinished = makePlan('2026-07-16', legacyFinishedBody);
  const legacyProofOut = path.join(temp, 'source-proof-legacy.json');
  const legacyResult = bindCompletion(
    new Map([
      ['finished-plan', legacyFinished],
      ['embedded-candidate-sha256', sha256(Buffer.from(legacyCandidateBytes))],
      ['receipt-out', legacyProofOut],
    ]),
    adapter({
      finishedDate: '2026-07-16',
      evidenceBody: legacyEvidencePlan,
      shippedBody: legacyFinishedBody,
      sourceBody: preparation.sourcePlan,
    }),
  );
  validateSourcePreparationProof(legacyResult.receipt);
  assert.equal(
    legacyResult.receipt.plans.source_sha256,
    sha256(Buffer.from(preparation.sourcePlan)),
    'legacy trimmed-candidate binding must record the raw source blob hash',
  );
  assert.equal(
    legacyResult.receipt.candidate.plan.source_blob_sha256,
    sha256(Buffer.from(preparation.sourcePlan.trim(), 'utf8')),
    'legacy binding preserves the sealed candidate hash form',
  );

  expectReject(
    'source blob content tamper',
    () =>
      bindCompletion(
        new Map([
          ['finished-plan', finished],
          ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
          ['receipt-out', path.join(temp, 'source-proof-tampered.json')],
        ]),
        adapter({ sourceBody: `${preparation.sourcePlan}\ntampered` }),
      ),
    /source plan blob/,
  );
  const waiver = {
    phase: 'completion',
    input_sha256: inputSha256,
    roles: ['primary'],
    actor: 'user',
    reason: 'User explicitly waived the bounded completion review.',
    at: '2026-07-17T18:29:00.000Z',
  };
  const waivedReviewer = {
    raw: {
      schema: 5,
      role: 'primary',
      request: completion.request,
      result: 'waived',
      attempts: [],
      selected: null,
      reviewer_output: null,
      findings_sha256: null,
      waiver,
      waiver_sha256: sha256(Buffer.from(jcs(waiver))),
      reason: null,
    },
    accepted_finding_ids: [],
    rejected: [],
  };
  const waivedRun = {
    ...completion.series.rounds[0],
    reviewer: waivedReviewer,
    reproduced: [],
    outcome: 'waived',
  };
  const waivedCompletion = {
    ...completion,
    reviewer: waivedReviewer,
    reproduced: [],
    outcome: 'waived',
    series: { ...completion.series, rounds: [waivedRun] },
  };
  const waivedEvidencePlan = evidencePlan.replace(
    'review_status: null',
    `review_waivers: ${jcs([waiver])}\nreview_status: null`,
  );
  const waivedFinishedBody =
    waivedEvidencePlan
      .replace('status: in_review', 'status: finished')
      .replace('review_status: null', 'review_status: passed') +
    `\n## Review\n\nCompletion-review-receipt: ${jcs(waivedCompletion)}\n`;
  const waivedFinished = makePlan('2026-07-30', waivedFinishedBody);
  const waivedResult = bindCompletion(
    new Map([
      ['finished-plan', waivedFinished],
      ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
      ['receipt-out', path.join(temp, 'waived-source-proof.json')],
    ]),
    adapter({
      evidenceBody: waivedEvidencePlan,
      finishedDate: '2026-07-30',
      shippedBody: waivedFinishedBody,
    }),
  );
  validateSourcePreparationProof(waivedResult.receipt);
  const tamperedProof = structuredClone(result.receipt);
  tamperedProof.candidate.preflight.run_database_id += 1;
  expectReject(
    'source proof nested candidate substitution',
    () => validateSourcePreparationProof(tamperedProof),
    /candidate|preflight|proof|source/i,
  );

  const tampered = structuredClone(completion);
  tampered.injected = true;
  const tamperedPlan = finishedBody.replace(jcs(completion), jcs(tampered));
  expectReject(
    'tampered completion receipt',
    () =>
      bindCompletion(
        new Map([
          ['finished-plan', makePlan('2026-07-18', tamperedPlan)],
          ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
          ['receipt-out', path.join(temp, 'tampered-proof.json')],
        ]),
        adapter({ finishedDate: '2026-07-18', shippedBody: tamperedPlan }),
      ),
    /completion|unknown|missing/i,
  );
  expectReject(
    'broken completion ancestry',
    () =>
      bindCompletion(
        new Map([
          ['finished-plan', makePlan('2026-07-19')],
          ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
          ['receipt-out', path.join(temp, 'ancestry-proof.json')],
        ]),
        adapter({ finishedDate: '2026-07-19', ancestry: false }),
      ),
    /ancestor|ancestry/i,
  );
  expectReject(
    'non-plan tree substitution',
    () =>
      bindCompletion(
        new Map([
          ['finished-plan', makePlan('2026-07-20')],
          ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
          ['receipt-out', path.join(temp, 'tree-proof.json')],
        ]),
        adapter({ finishedDate: '2026-07-20', equivalent: false }),
      ),
    /outside|tree|equivalence/i,
  );
  expectReject(
    'shipped plan blob substitution',
    () =>
      bindCompletion(
        new Map([
          ['finished-plan', makePlan('2026-07-21')],
          ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
          ['receipt-out', path.join(temp, 'shipped-blob-proof.json')],
        ]),
        adapter({ finishedDate: '2026-07-21', shippedPlanMatches: false }),
      ),
    /shipped|finished|blob|plan/i,
  );
  expectReject(
    'dirty shipped archive checkout',
    () =>
      bindCompletion(
        new Map([
          ['finished-plan', makePlan('2026-07-22')],
          ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
          ['receipt-out', path.join(temp, 'dirty-proof.json')],
        ]),
        adapter({ finishedDate: '2026-07-22', dirty: true }),
      ),
    /clean|dirty|working tree|archive/i,
  );
  expectReject(
    'transient source-to-evidence non-plan change',
    () =>
      bindCompletion(
        new Map([
          ['finished-plan', makePlan('2026-07-23')],
          ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
          ['receipt-out', path.join(temp, 'source-evidence-proof.json')],
        ]),
        adapter({ finishedDate: '2026-07-23', sourceEvidenceClean: false }),
      ),
    /source|evidence|plan-only|outside/i,
  );
  expectReject(
    'transient evidence-to-shipped non-plan change',
    () =>
      bindCompletion(
        new Map([
          ['finished-plan', makePlan('2026-07-24')],
          ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
          ['receipt-out', path.join(temp, 'evidence-shipped-proof.json')],
        ]),
        adapter({ finishedDate: '2026-07-24', evidenceShippedClean: false }),
      ),
    /evidence|shipped|lifecycle|outside/i,
  );
}

function testPrepareFixtureUsesFullCi(temp) {
  const fixturePath = path.join(temp, 'prepare-fixture.json');
  const reportPath = path.join(temp, 'prepare-report.json');
  fs.writeFileSync(
    fixturePath,
    JSON.stringify({
      schema: 1,
      type: 'SessionRelayReleaseFixtureV1',
      scenario: 'prepare-full-ci',
      repository_id: REPOSITORY_ID,
      source_commit: COMMIT,
      promoted_commit: '2'.repeat(40),
      expected_origin_main: '3'.repeat(40),
      tag: 'session-relay--v0.12.0',
      assets: [],
      expected_outcome: 'success',
    }),
  );
  const previousFixture = process.env.SESSION_RELAY_RELEASE_FIXTURE;
  const previousReport = process.env.SESSION_RELAY_RELEASE_REPORT;
  try {
    process.env.SESSION_RELAY_RELEASE_FIXTURE = fixturePath;
    process.env.SESSION_RELAY_RELEASE_REPORT = reportPath;
    assert.equal(
      runFixture(['--prepare', '--plugin', 'session-relay', '0.12.0'], { mode: 'prepare', options: new Map() }, null),
      true,
    );
  } finally {
    if (previousFixture === undefined) delete process.env.SESSION_RELAY_RELEASE_FIXTURE;
    else process.env.SESSION_RELAY_RELEASE_FIXTURE = previousFixture;
    if (previousReport === undefined) delete process.env.SESSION_RELAY_RELEASE_REPORT;
    else process.env.SESSION_RELAY_RELEASE_REPORT = previousReport;
  }
  const report = JSON.parse(fs.readFileSync(reportPath, 'utf8'));
  assert.deepEqual(
    report.calls,
    [{ argv: ['node', 'scripts/ci.mjs'] }],
    'Session Relay prepare must gate the exact changed tree with full CI',
  );
  assert.deepEqual(
    prepareCiCall(),
    { argv: ['node', 'scripts/ci.mjs'] },
    'production prepare journal must match the full CI command',
  );
  const observed = [];
  assert.deepEqual(
    runPrepareCi((commandName, args, options) => observed.push({ commandName, args, options })),
    { argv: ['node', 'scripts/ci.mjs'] },
  );
  assert.deepEqual(
    observed,
    [{ commandName: 'node', args: ['scripts/ci.mjs'], options: { inherit: true } }],
    'production prepare execution must equal its journal argv',
  );
}

function testLiveSourceCiAndPrepareDryRun() {
  const repo = process.cwd();
  const sourceCi = parseYaml(fs.readFileSync(path.join(repo, '.github/workflows/ci.yml'), 'utf8'));
  const sourceCiStepNames = JSON.stringify(sourceCi.jobs.validate.steps.map((step) => step.name ?? step.uses ?? ''));
  for (const required of ['24', '--frozen-lockfile', '1.85.0', 'musl', 'scripts/ci.mjs']) {
    assert.ok(sourceCiStepNames.includes(required), `source CI job metadata omits ${required}`);
  }

  const tracked = [
    '.claude-plugin/marketplace.json',
    'plugins/session-relay/.claude-plugin/plugin.json',
    'plugins/session-relay/.codex-plugin/plugin.json',
    'plugins/session-relay/rust/Cargo.toml',
    'plugins/session-relay/rust/Cargo.lock',
  ];
  const status = spawnSync('git', ['status', '--porcelain=v1', '--untracked-files=all'], {
    cwd: repo,
    encoding: 'utf8',
    shell: false,
  });
  assert.equal(status.status, 0, status.stderr);
  const before = new Map(tracked.map((relative) => [relative, fs.readFileSync(path.join(repo, relative))]));
  const realDry = spawnSync(
    process.execPath,
    ['scripts/release.mjs', '--prepare', '--plugin', 'session-relay', '--dry-run', '0.12.0'],
    { cwd: repo, encoding: 'utf8', shell: false },
  );
  assert.equal(realDry.status, 0, `real prepare dry-run failed: ${realDry.stderr}`);
  const after = spawnSync('git', ['status', '--porcelain=v1', '--untracked-files=all'], {
    cwd: repo,
    encoding: 'utf8',
    shell: false,
  });
  assert.equal(after.status, 0, after.stderr);
  assert.equal(after.stdout, status.stdout, 'real prepare dry-run mutated the working tree');
  for (const [relative, bytes] of before) assert.deepEqual(fs.readFileSync(path.join(repo, relative)), bytes);
}

function main() {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-release-evidence-'));
  fs.chmodSync(temp, 0o700);
  try {
    testRawGitBytes(temp);
    const preflight = runPreflight(temp);
    assert.equal(fs.statSync(preflight.receiptOut).mode & 0o777, 0o600);
    assert.equal(preflight.receipt.workflow.file_blob_id, WORKFLOW_BLOB);
    assert.equal(preflight.receipt.artifacts.length, 4);
    assert.ok(preflight.receipt.artifacts.every(({ archive_digest }) => /^sha256:[0-9a-f]{64}$/.test(archive_digest)));
    testClosedValidators(preflight);
    const sourceCi = testSourceCi(temp);
    const preparation = testPreparationHandlers(temp, preflight, sourceCi);
    testCompletionBinding(temp, preparation);
    testArtifactAdversaries(temp);
    testNativeProducerEvidence(temp);
    testNativeProducerWorkflow();
    testPrepareFixtureUsesFullCi(temp);
    testLiveSourceCiAndPrepareDryRun();
    console.log('release evidence contract: ok');
  } finally {
    fs.rmSync(temp, { recursive: true, force: true });
  }
}

main();
