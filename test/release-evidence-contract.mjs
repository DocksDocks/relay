#!/usr/bin/env node

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { deflateRawSync } from 'node:zlib';
import { parse as parseYaml } from 'yaml';

import {
  bindCompletion,
  checkPrepared,
  validateProducerPreflightReceipt,
  validateProof,
  validateSourceCiReceipt,
  validateSourcePreparationProof,
  validateTddRedReceipt,
  verifyEmbedded,
  verifySourceCi,
} from '../../../scripts/lib/session-relay-release-preparation.mjs';
import { canonicalPlanView } from '../../../plugins/docks/skills/productivity/plan-review/scripts/review-policy.mjs';
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

function jcs(value) {
  if (value === null || typeof value === 'boolean' || typeof value === 'string' || typeof value === 'number') return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(jcs).join(',')}]`;
  return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${jcs(value[key])}`).join(',')}}`;
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
    const attestation = Buffer.from(jcs({
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
    }));
    const archive = zip([
      { name: assetName, bytes: binary, deflate: true, dataDescriptor: true },
      { name: `attestation-${target}.json`, bytes: attestation, deflate: true, dataDescriptor: true },
    ]);
    archives.set(databaseId, archive);
    artifacts.push(artifactRecord(databaseId, artifactName, archive));
    databaseId += 1;
  }
  const manifest = Buffer.from([...binaryDigests]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([name, digest]) => `${digest}  ${name}\n`).join(''));
  const checksumArchive = zip([{ name: 'SHA256SUMS', bytes: manifest, deflate: true, dataDescriptor: true }]);
  archives.set(databaseId, checksumArchive);
  artifacts.push(artifactRecord(databaseId, 'session-relay-checksums', checksumArchive));
  if (mutateArchive) mutateArchive({ archives, artifacts });
  return { archives, artifacts };
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
      if (endpoint.endsWith(`/actions/runs/${RUN_ID}`)) return {
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
      if (endpoint.endsWith(`/actions/workflows/${WORKFLOW_ID}`)) return { id: WORKFLOW_ID, path: '.github/workflows/build-binaries.yml', state: 'active' };
      if (endpoint.includes('/contents/.github/workflows/build-binaries.yml?ref=')) return { type: 'file', path: '.github/workflows/build-binaries.yml', sha: WORKFLOW_BLOB };
      if (endpoint.endsWith(`/actions/runs/${RUN_ID}/artifacts?per_page=100`)) return { total_count: fixture.artifacts.length, artifacts: fixture.artifacts };
      assert.fail(`unexpected API endpoint ${endpoint}`);
    },
    downloadArtifact({ artifactId, destination }) {
      const bytes = fixture.archives.get(artifactId);
      assert.ok(bytes, `download requested verified artifact database ID ${artifactId}`);
      fs.writeFileSync(destination, bytes, { flag: 'wx', mode: 0o600 });
    },
    now() { return '2026-07-17T18:00:00.000Z'; },
  };
}

function runPreflight(temp, fixture = artifactFixture()) {
  const workspace = path.join(temp, `artifacts-${fs.readdirSync(temp).length}`);
  fs.mkdirSync(workspace, { mode: 0o700 });
  const receiptOut = path.join(temp, `preflight-${fs.readdirSync(temp).length}.json`);
  const result = verifyPreflight({ runId: RUN_ID, expectedCommit: COMMIT, artifacts: workspace, receiptOut }, preflightAdapter(fixture));
  return { ...result, workspace, receiptOut };
}

function expectReject(label, fn, pattern) {
  assert.throws(fn, pattern, label);
}

function extractDocksRedReceipt() {
  const plan = fs.readFileSync(path.resolve('docs/plans/active/session-relay-prebuilt-cli-distribution.md'), 'utf8');
  const match = plan.match(/^- Docks TDD-red receipt JCS bytes: (\{.*\})$/m);
  assert.ok(match, 'active plan carries the real capture-helper receipt');
  return JSON.parse(match[1]);
}

function testClosedValidators(preflight) {
  const docksRed = extractDocksRedReceipt();
  validateTddRedReceipt(docksRed, { repositoryId: REPOSITORY_ID });
  expectReject('TDD-red unknown property', () => validateTddRedReceipt({ ...docksRed, injected: true }, { repositoryId: REPOSITORY_ID }), /unknown|missing/i);
  expectReject('TDD-red must prove red', () => validateTddRedReceipt({ ...docksRed, exit_code: 0 }, { repositoryId: REPOSITORY_ID }), /nonzero|exit/i);
  expectReject('TDD-red repository substitution', () => validateTddRedReceipt(docksRed, { repositoryId: 'DocksDocks/public' }), /repository/i);

  validateProducerPreflightReceipt(preflight.receipt, { sourceCommit: COMMIT });
  expectReject('preflight source substitution', () => validateProducerPreflightReceipt({ ...preflight.receipt, source_commit: '0'.repeat(40) }, { sourceCommit: COMMIT }), /source|commit/i);
  expectReject('preflight ref substitution', () => validateProducerPreflightReceipt({ ...preflight.receipt, validation_ref: 'refs/heads/preflight/substituted' }, { sourceCommit: COMMIT }), /ref|workflow|identity/i);
  expectReject('preflight unknown property', () => validateProducerPreflightReceipt({ ...preflight.receipt, injected: true }, { sourceCommit: COMMIT }), /unknown|missing/i);
}

function authoritativeCiWorkflow() {
  return fs.readFileSync(path.resolve('.github/workflows/ci.yml'), 'utf8');
}

function sourceCiFixture(temp, { receiptName = 'source-ci.json', workflowBytes = Buffer.from(authoritativeCiWorkflow()) } = {}) {
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
  const jobs = [{
    id: 9001,
    run_id: 991,
    run_attempt: 1,
    head_sha: COMMIT,
    name: 'validate (scripts/ci.mjs)',
    status: 'completed',
    conclusion: 'success',
    started_at: '2026-07-17T18:00:00Z',
    completed_at: '2026-07-17T18:10:00Z',
    steps: requiredSteps.map((name, index) => ({ name, number: [3, 5, 9, 11, 12, 13, 14][index], status: 'completed', conclusion: 'success' })),
  }];
  const adapter = {
    apiJson(endpoint) {
      if (endpoint.endsWith('/actions/runs/991')) return {
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
      if (endpoint.includes('/contents/.github/workflows/ci.yml?ref=')) return {
        type: 'file', path: '.github/workflows/ci.yml', sha: sha256GitBlob(workflowBytes),
        encoding: 'base64', content: workflowBytes.toString('base64'),
      };
      if (endpoint.endsWith('/actions/runs/991/jobs?per_page=100')) return { total_count: 1, jobs };
      assert.fail(`unexpected source-CI endpoint ${endpoint}`);
    },
    apiBytes(endpoint) {
      assert.equal(endpoint, `repos/${REPOSITORY_ID}/actions/jobs/9001/logs`);
      return Buffer.from('authoritative job log\n');
    },
    now() { return '2026-07-17T18:11:00.000Z'; },
  };
  const result = verifySourceCi(new Map([
    ['run-id', '991'], ['expected-commit', COMMIT], ['receipt-out', receiptOut],
  ]), adapter);
  return { result, adapter, receiptOut, jobs };
}

function sha256GitBlob(bytes) {
  return createHash('sha1').update(Buffer.from(`blob ${bytes.length}\0`)).update(bytes).digest('hex');
}

function testSourceCi(temp) {
  const fixture = sourceCiFixture(temp);
  validateSourceCiReceipt(fixture.result.receipt, { sourceCommit: COMMIT });
  const changedCommit = { ...fixture.result.receipt, source_commit: 'f'.repeat(40) };
  expectReject('source-CI commit substitution', () => validateSourceCiReceipt(changedCommit, { sourceCommit: COMMIT }), /source|commit/i);
  const changedBlob = structuredClone(fixture.result.receipt);
  changedBlob.workflow.file_blob_id = '0'.repeat(40);
  expectReject('source-CI workflow blob substitution', () => validateSourceCiReceipt(changedBlob, { sourceCommit: COMMIT }), /workflow|blob/i);

  const badJobs = structuredClone(fixture.jobs);
  badJobs[0].steps.find(({ name }) => name.startsWith('run the authoritative')).conclusion = 'failure';
  const badAdapter = { ...fixture.adapter, apiJson(endpoint) {
    const value = fixture.adapter.apiJson(endpoint);
    return endpoint.endsWith('/jobs?per_page=100') ? { total_count: 1, jobs: badJobs } : value;
  } };
  expectReject('failed required CI step', () => verifySourceCi(new Map([
    ['run-id', '991'], ['expected-commit', COMMIT], ['receipt-out', path.join(temp, 'bad-source-ci.json')],
  ]), badAdapter), /step|conclusion|success/i);
  const noOpWorkflow = authoritativeCiWorkflow()
    .replace("            node scripts/ci.mjs\n", "            printf 'node scripts/ci.mjs\\n'\n");
  expectReject('source-CI no-op command with matching marker text', () => sourceCiFixture(temp, {
    receiptName: 'noop-source-ci.json',
    workflowBytes: Buffer.from(noOpWorkflow),
  }), /workflow|authoritative|command|definition/i);
  const checkoutOverride = authoritativeCiWorkflow().replace('ref: ${{ github.sha }}', 'ref: refs/heads/substituted');
  expectReject('source-CI checkout override', () => sourceCiFixture(temp, {
    receiptName: 'checkout-source-ci.json',
    workflowBytes: Buffer.from(checkoutOverride),
  }), /checkout|workflow|definition|ref/i);
  const credentialOverride = authoritativeCiWorkflow().replace('persist-credentials: false', 'persist-credentials: true');
  expectReject('source-CI credential persistence override', () => sourceCiFixture(temp, {
    receiptName: 'credentials-source-ci.json',
    workflowBytes: Buffer.from(credentialOverride),
  }), /checkout|workflow|definition|credential/i);
  const cacheOverride = authoritativeCiWorkflow().replace(
    'run: pnpm config set store-dir "$HOME/.pnpm-store"',
    'run: printf "skip deterministic store\\n"',
  );
  expectReject('source-CI non-required job step override', () => sourceCiFixture(temp, {
    receiptName: 'cache-source-ci.json',
    workflowBytes: Buffer.from(cacheOverride),
  }), /workflow|job|definition|store/i);

  let runReads = 0;
  const racingAdapter = { ...fixture.adapter, apiJson(endpoint) {
    const value = fixture.adapter.apiJson(endpoint);
    if (endpoint.endsWith('/actions/runs/991') && ++runReads === 2) return { ...value, run_attempt: 2 };
    return value;
  } };
  expectReject('source-CI run attempt race', () => verifySourceCi(new Map([
    ['run-id', '991'], ['expected-commit', COMMIT], ['receipt-out', path.join(temp, 'racing-source-ci.json')],
  ]), racingAdapter), /attempt|changed|race|identity/i);
  return fixture;
}

function testArtifactAdversaries(temp) {
  const digestFixture = artifactFixture();
  digestFixture.artifacts[0].digest = `sha256:${'0'.repeat(64)}`;
  expectReject('API digest mismatch', () => runPreflight(temp, digestFixture), /digest/i);

  const traversal = artifactFixture({ mutateArchive({ archives, artifacts }) {
    const id = artifacts[0].id;
    const bytes = zip([{ name: '../substitute', bytes: Buffer.from('owned?') }]);
    archives.set(id, bytes);
    Object.assign(artifacts[0], artifactRecord(id, artifacts[0].name, bytes));
  } });
  expectReject('archive traversal', () => runPreflight(temp, traversal), /archive|path|traversal/i);

  const duplicate = artifactFixture({ mutateArchive({ archives, artifacts }) {
    const id = artifacts[0].id;
    const bytes = zip([{ name: 'same', bytes: Buffer.from('a') }, { name: 'same', bytes: Buffer.from('b') }]);
    archives.set(id, bytes);
    Object.assign(artifacts[0], artifactRecord(id, artifacts[0].name, bytes));
  } });
  expectReject('duplicate archive path', () => runPreflight(temp, duplicate), /duplicate/i);

  const symlink = artifactFixture({ mutateArchive({ archives, artifacts }) {
    const id = artifacts[0].id;
    const bytes = zip([{ name: 'link', bytes: Buffer.from('/tmp/attacker'), mode: 'symlink' }]);
    archives.set(id, bytes);
    Object.assign(artifacts[0], artifactRecord(id, artifacts[0].name, bytes));
  } });
  expectReject('symlink archive member', () => runPreflight(temp, symlink), /symlink/i);
  const expansion = artifactFixture({ mutateArchive({ archives, artifacts }) {
    const id = artifacts[0].id;
    const bytes = zip([{ name: 'expansion', bytes: Buffer.alloc(2 * 1024 * 1024, 0x41), deflate: true, declaredSize: 1 }]);
    archives.set(id, bytes);
    Object.assign(artifacts[0], artifactRecord(id, artifacts[0].name, bytes));
  } });
  expectReject('archive expansion beyond declared size', () => runPreflight(temp, expansion), /could not be inflated/i);

  const fixture = artifactFixture();
  const workspace = path.join(temp, 'substituted-workspace');
  fs.mkdirSync(workspace, { mode: 0o700 });
  fs.writeFileSync(path.join(workspace, 'attacker-controlled'), 'substitute');
  expectReject('pre-populated artifact substitution workspace', () => verifyPreflight({
    runId: RUN_ID,
    expectedCommit: COMMIT,
    artifacts: workspace,
    receiptOut: path.join(temp, 'substitution.json'),
  }, preflightAdapter(fixture)), /empty|workspace|owned/i);
}

function writeReceipt(file, value) {
  const bytes = Buffer.from(jcs(value));
  fs.writeFileSync(file, bytes, { flag: 'wx', mode: 0o600 });
  return sha256(bytes);
}

function capturePublicRed(temp) {
  const head = spawnSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).stdout.trim();
  const receiptOut = path.join(temp, 'public-red.json');
  const result = spawnSync('node', [
    'scripts/capture-tdd-red.mjs',
    '--repo', process.cwd(),
    '--repository-id', 'DocksDocks/public',
    '--pre-production-commit', head,
    '--test', 'plugins/session-relay/test/selftest.mjs',
    '--receipt-out', receiptOut,
    '--', 'node', '-e', 'process.exit(7)',
  ], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
  return { value: JSON.parse(fs.readFileSync(receiptOut, 'utf8')), digest: result.stdout.trim(), path: receiptOut };
}

function preparationAdapter({ plan, sourcePlan, docksRed, publicRed, publicPlan, sourceCi, unrelated = false, unrelatedExecutionBase = false, publicTamper = null, status = '' }) {
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
        if (unrelatedExecutionBase && args[2] === '4567890abcdef1234567890abcdef12345678901' && args[3] === COMMIT) throw new Error('not an ancestor');
        return '';
      }
      if (joined === `rev-parse ${COMMIT}:.github/workflows/build-binaries.yml`) return WORKFLOW_BLOB;
      if (joined === `rev-parse ${COMMIT}:.github/workflows/ci.yml`) return sourceCi.workflow.file_blob_id;
      if (joined === `show ${COMMIT}:docs/plans/active/session-relay-prebuilt-cli-distribution.md`) return Buffer.from(sourcePlan);
      for (const receipt of [docksRed, publicRed]) {
        for (const test of receipt.test_paths) {
          if (joined === `rev-parse ${receipt.pre_production_commit}:${test.path}` || joined === `rev-parse ${COMMIT}:${test.path}`) return test.blob_id;
        }
        if (joined === `rev-parse ${receipt.pre_production_commit}:${receipt.producer.path}` || joined === `rev-parse ${COMMIT}:${receipt.producer.path}`) return receipt.producer.blob_id;
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
          '--public-remote', 'https://github.com/DocksDocks/public.git',
          '--public-ref', 'refs/heads/preflight/session-relay-cli-0.9.0',
          '--public-commit', '34567890abcdef1234567890abcdef1234567890',
          '--detached-clone',
        ]);
      }
      const stdout = executable === 'git' && args[0] === 'ls-files'
        ? Buffer.from('plugins/session-relay/bin/relay\n')
        : Buffer.from(`${argv.join(' ')}: ok\n`);
      return { status: 0, stdout, stderr: Buffer.alloc(0) };
    },
    now() { return '2026-07-17T18:20:00.000Z'; },
    readFile() { return Buffer.from(plan); },
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
    'blocked_reason: \"Awaiting the four independently hashed `session-relay--v0.12.0` production asset digests.\"',
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
    ['docks-red', docksRedPath], ['docks-red-sha256', docksRedDigest],
    ['public-red', publicRed.path], ['public-red-sha256', publicRed.digest],
    ['preflight', preflight.receiptOut], ['preflight-sha256', sha256(fs.readFileSync(preflight.receiptOut))],
    ['source-ci', sourceCi.receiptOut], ['source-ci-sha256', sha256(fs.readFileSync(sourceCi.receiptOut))],
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
  assert.deepEqual(candidate.receipt.checks.map(({ id }) => id), ['A1', 'A2', 'A3', 'A4', 'A5', 'A6']);
  assert.equal(candidate.receipt.checks[0].steps[0].argv[1], 'plugins/session-relay/test/companion-distribution-contract.mjs');
  assert.deepEqual(candidate.receipt.checks[1].steps[0].argv, [
    'cargo', '+1.85.0', 'build', '--manifest-path', 'plugins/session-relay/rust/Cargo.toml', '--release', '--locked',
  ]);
  const dirtyPlanOptions = new Map(options);
  dirtyPlanOptions.set('receipt-out', path.join(temp, 'candidate-dirty-plan.json'));
  const dirtyPlan = checkPrepared(dirtyPlanOptions, preparationAdapter({
    plan: sourcePlan,
    sourcePlan,
    docksRed,
    publicRed: publicRed.value,
    publicPlan,
    sourceCi: sourceCi.result.receipt,
    status: '1 .M N... 100644 100644 100644 abcdef1 abcdef1 docs/plans/active/session-relay-prebuilt-cli-distribution.md\0',
  }));
  assert.equal(dirtyPlan.receipt.source_commit, COMMIT);
  const misleadingPlanOptions = new Map(options);
  misleadingPlanOptions.set('receipt-out', path.join(temp, 'candidate-misleading-plan.json'));
  expectReject('active-plan suffix substitution', () => checkPrepared(misleadingPlanOptions, preparationAdapter({
    plan: sourcePlan,
    sourcePlan,
    docksRed,
    publicRed: publicRed.value,
    publicPlan,
    sourceCi: sourceCi.result.receipt,
    status: '1 M. N... 100644 100644 100644 abcdef1 abcdef1 x docs/plans/active/session-relay-prebuilt-cli-distribution.md\0',
  })), /working tree is not clean/i);

  for (const publicTamper of ['ref', 'blob']) {
    const tamperedOptions = new Map(options);
    tamperedOptions.set('receipt-out', path.join(temp, `candidate-public-${publicTamper}.json`));
    expectReject(`companion public ${publicTamper} substitution`, () => checkPrepared(tamperedOptions, preparationAdapter({
      plan: sourcePlan,
      sourcePlan,
      docksRed,
      publicRed: publicRed.value,
      publicPlan,
      sourceCi: sourceCi.result.receipt,
      publicTamper,
    })), /companion|public|ref|blob/i);
  }

  const plan = `${sourcePlan}`
    + `- Producer preflight receipt JCS bytes: ${fs.readFileSync(preflight.receiptOut, 'utf8')}\n`
    + `- Producer preflight receipt SHA-256: ${sha256(fs.readFileSync(preflight.receiptOut))}\n`
    + `- Source CI receipt JCS bytes: ${fs.readFileSync(sourceCi.receiptOut, 'utf8')}\n`
    + `- Source CI receipt SHA-256: ${sha256(fs.readFileSync(sourceCi.receiptOut))}\n`
    + `- Source preparation candidate JCS bytes: ${fs.readFileSync(candidateOut, 'utf8')}\n`
    + `- Source preparation candidate SHA-256: ${sha256(fs.readFileSync(candidateOut))}\n`;
  const verifyOptions = new Map([['plan', 'docs/plans/active/session-relay-prebuilt-cli-distribution.md']]);
  const verified = verifyEmbedded(verifyOptions, preparationAdapter({
    plan,
    sourcePlan,
    docksRed,
    publicRed: publicRed.value,
    publicPlan,
    sourceCi: sourceCi.result.receipt,
  }));
  assert.equal(verified.state.source_commit, COMMIT);
  const mismatchedCandidate = structuredClone(candidate.receipt);
  mismatchedCandidate.preflight.run_database_id += 1;
  const originalCandidateBytes = fs.readFileSync(candidateOut, 'utf8');
  const mismatchedCandidateBytes = jcs(mismatchedCandidate);
  const mismatchedPlan = plan
    .replace(`- Source preparation candidate JCS bytes: ${originalCandidateBytes}`, `- Source preparation candidate JCS bytes: ${mismatchedCandidateBytes}`)
    .replace(`- Source preparation candidate SHA-256: ${sha256(Buffer.from(originalCandidateBytes))}`, `- Source preparation candidate SHA-256: ${sha256(Buffer.from(mismatchedCandidateBytes))}`);
  expectReject('embedded candidate copied summary substitution', () => verifyEmbedded(verifyOptions, preparationAdapter({
    plan: mismatchedPlan,
    sourcePlan,
    docksRed,
    publicRed: publicRed.value,
    publicPlan,
    sourceCi: sourceCi.result.receipt,
  })), /candidate|preflight|summary|run/i);

  expectReject('unrelated evidence commit', () => verifyEmbedded(verifyOptions, preparationAdapter({
    plan,
    sourcePlan,
    docksRed,
    publicRed: publicRed.value,
    publicPlan,
    sourceCi: sourceCi.result.receipt,
    unrelated: true,
  })), /ancestor|ancestry|evidence/i);
  expectReject('unrelated embedded execution base', () => verifyEmbedded(verifyOptions, preparationAdapter({
    plan,
    sourcePlan,
    docksRed,
    publicRed: publicRed.value,
    publicPlan,
    sourceCi: sourceCi.result.receipt,
    unrelatedExecutionBase: true,
  })), /execution-base|ancestor|ancestry|source/i);
  return { candidate, candidateOut, plan, sourcePlan };
}

function replaceReceiptIdentity(value, replacements) {
  if (typeof value === 'string') return replacements.get(value) ?? value;
  if (Array.isArray(value)) return value.map((item) => replaceReceiptIdentity(item, replacements));
  if (value !== null && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, replaceReceiptIdentity(item, replacements)]));
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
  const evidencePlan = preparation.plan
    .replace('status: ongoing', 'status: in_review')
    .replace('review_status: null', 'review_status: null');
  const inputSha256 = sha256(canonicalPlanView(Buffer.from(evidencePlan)));
  const template = completionTemplate();
  const completion = replaceReceiptIdentity(template, new Map([
    [template.reviewed_head, evidenceCommit],
    [template.plan_input_sha256, inputSha256],
  ]));
  const finishedBody = evidencePlan
    .replace('status: in_review', 'status: finished')
    .replace('review_status: null', 'review_status: passed')
    + `\n## Review\n\nCompletion-review-receipt: ${jcs(completion)}\n`;
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
    shippedBody = finishedBody,
    shippedPlanMatches = true,
    dirty = false,
    finishedDate = '2026-07-17',
  } = {}) => ({
    repoRoot: root,
    git(args) {
      const joined = args.join(' ');
      if (joined === 'rev-parse HEAD^{commit}') return shippedCommit;
      if (joined === `show ${evidenceCommit}:docs/plans/active/session-relay-prebuilt-cli-distribution.md`) return Buffer.from(evidencePlan);
      if (joined === `show ${COMMIT}:docs/plans/active/session-relay-prebuilt-cli-distribution.md`) return Buffer.from(preparation.sourcePlan);
      if (joined === `show ${shippedCommit}:docs/plans/finished/${finishedDate}-session-relay-prebuilt-cli-distribution.md`) {
        return Buffer.from(shippedPlanMatches ? shippedBody : `${shippedBody}\nsubstituted\n`);
      }
      if (args[0] === 'status') return dirty ? ' M docs/plans/finished/substituted.md' : '';
      if (args[0] === 'merge-base' && args[1] === '--is-ancestor') {
        if (!ancestry) throw new Error('unrelated');
        return '';
      }
      if (args[0] === 'diff' && args[1] === '--name-only') {
        if (args[2] === COMMIT && args[3] === evidenceCommit) return sourceEvidenceClean ? 'docs/plans/active/session-relay-prebuilt-cli-distribution.md' : 'docs/plans/active/session-relay-prebuilt-cli-distribution.md\nscripts/transient.mjs';
        if (args[2] === evidenceCommit && args[3] === shippedCommit) return evidenceShippedClean ? `docs/plans/active/session-relay-prebuilt-cli-distribution.md\ndocs/plans/finished/${finishedDate}-session-relay-prebuilt-cli-distribution.md` : `docs/plans/active/session-relay-prebuilt-cli-distribution.md\ndocs/plans/finished/${finishedDate}-session-relay-prebuilt-cli-distribution.md\nscripts/transient.mjs`;
        return equivalent ? '' : 'scripts/substituted.mjs';
      }
      assert.fail(`unexpected completion git call: ${joined}`);
    },
    inspectPublic() { assert.fail('completion binding must not inspect public state'); },
    run() { assert.fail('completion binding must not execute commands'); },
    now() { return '2026-07-17T18:30:00.000Z'; },
    readFile(file) { return fs.readFileSync(file); },
  });

  const finished = makePlan('2026-07-17');
  const proofOut = path.join(temp, 'source-proof.json');
  const result = bindCompletion(new Map([
    ['finished-plan', finished],
    ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
    ['receipt-out', proofOut],
  ]), adapter());
  validateSourcePreparationProof(result.receipt);
  const loaded = validateProof(new Map([
    ['source-proof', proofOut],
    ['source-proof-sha256', sha256(fs.readFileSync(proofOut))],
  ]));
  assert.equal(loaded.value.evidence_commit, evidenceCommit);
  assert.equal(loaded.value.promoted_commit, shippedCommit);
  assert.deepEqual(result.receipt.candidate, preparation.candidate.receipt);
  const tamperedProof = structuredClone(result.receipt);
  tamperedProof.candidate.preflight.run_database_id += 1;
  expectReject('source proof nested candidate substitution', () => validateSourcePreparationProof(tamperedProof), /candidate|preflight|proof|source/i);

  const tampered = structuredClone(completion);
  tampered.injected = true;
  const tamperedPlan = finishedBody.replace(jcs(completion), jcs(tampered));
  expectReject('tampered completion receipt', () => bindCompletion(new Map([
    ['finished-plan', makePlan('2026-07-18', tamperedPlan)],
    ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
    ['receipt-out', path.join(temp, 'tampered-proof.json')],
  ]), adapter({ finishedDate: '2026-07-18', shippedBody: tamperedPlan })), /completion|unknown|missing/i);
  expectReject('broken completion ancestry', () => bindCompletion(new Map([
    ['finished-plan', makePlan('2026-07-19')],
    ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
    ['receipt-out', path.join(temp, 'ancestry-proof.json')],
  ]), adapter({ finishedDate: '2026-07-19', ancestry: false })), /ancestor|ancestry/i);
  expectReject('non-plan tree substitution', () => bindCompletion(new Map([
    ['finished-plan', makePlan('2026-07-20')],
    ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
    ['receipt-out', path.join(temp, 'tree-proof.json')],
  ]), adapter({ finishedDate: '2026-07-20', equivalent: false })), /outside|tree|equivalence/i);
  expectReject('shipped plan blob substitution', () => bindCompletion(new Map([
    ['finished-plan', makePlan('2026-07-21')],
    ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
    ['receipt-out', path.join(temp, 'shipped-blob-proof.json')],
  ]), adapter({ finishedDate: '2026-07-21', shippedPlanMatches: false })), /shipped|finished|blob|plan/i);
  expectReject('dirty shipped archive checkout', () => bindCompletion(new Map([
    ['finished-plan', makePlan('2026-07-22')],
    ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
    ['receipt-out', path.join(temp, 'dirty-proof.json')],
  ]), adapter({ finishedDate: '2026-07-22', dirty: true })), /clean|dirty|working tree|archive/i);
  expectReject('transient source-to-evidence non-plan change', () => bindCompletion(new Map([
    ['finished-plan', makePlan('2026-07-23')],
    ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
    ['receipt-out', path.join(temp, 'source-evidence-proof.json')],
  ]), adapter({ finishedDate: '2026-07-23', sourceEvidenceClean: false })), /source|evidence|plan-only|outside/i);
  expectReject('transient evidence-to-shipped non-plan change', () => bindCompletion(new Map([
    ['finished-plan', makePlan('2026-07-24')],
    ['embedded-candidate-sha256', sha256(fs.readFileSync(preparation.candidateOut))],
    ['receipt-out', path.join(temp, 'evidence-shipped-proof.json')],
  ]), adapter({ finishedDate: '2026-07-24', evidenceShippedClean: false })), /evidence|shipped|lifecycle|outside/i);
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
  const status = spawnSync('git', ['status', '--porcelain=v1', '--untracked-files=all'], { cwd: repo, encoding: 'utf8', shell: false });
  assert.equal(status.status, 0, status.stderr);
  const before = new Map(tracked.map((relative) => [relative, fs.readFileSync(path.join(repo, relative))]));
  const realDry = spawnSync(process.execPath, [
    'scripts/release.mjs', '--prepare', '--plugin', 'session-relay', '--dry-run', '0.12.0',
  ], { cwd: repo, encoding: 'utf8', shell: false });
  assert.equal(realDry.status, 0, `real prepare dry-run failed: ${realDry.stderr}`);
  const after = spawnSync('git', ['status', '--porcelain=v1', '--untracked-files=all'], { cwd: repo, encoding: 'utf8', shell: false });
  assert.equal(after.status, 0, after.stderr);
  assert.equal(after.stdout, status.stdout, 'real prepare dry-run mutated the working tree');
  for (const [relative, bytes] of before) assert.deepEqual(fs.readFileSync(path.join(repo, relative)), bytes);
}

function main() {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-release-evidence-'));
  fs.chmodSync(temp, 0o700);
  try {
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
    testLiveSourceCiAndPrepareDryRun();
    console.log('release evidence contract: ok');
  } finally {
    fs.rmSync(temp, { recursive: true, force: true });
  }
}

main();
