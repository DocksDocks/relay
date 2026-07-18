#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

import { parse as parseYaml } from 'yaml';

import {
  ASSETS,
  PRERELEASE_BODY,
  REPOSITORY_ID,
  STABLE_BODY,
  TAG,
  VERSION,
  canonicalize,
  SessionRelayReleaseError,
  sha256,
} from '../../../scripts/lib/session-relay-release-core.mjs';
import {
  finalizeReviewed,
  publishReviewed,
} from '../../../scripts/lib/session-relay-release-publication.mjs';

const SOURCE = '1'.repeat(40);
const WORKFLOW_PATH = '.github/workflows/build-binaries.yml';
const POLL_LIMIT = 12;
const acceptPromotionReceipt = (receipt) => receipt;
let checks = 0;

function writeCanonical(directory, name, value) {
  const file = path.join(directory, name);
  const bytes = Buffer.from(canonicalize(value));
  fs.writeFileSync(file, bytes, { mode: 0o600, flag: 'wx' });
  return { file, digest: sha256(bytes), value };
}

function proof(directory) {
  const hash = '4'.repeat(64);
  const candidate = {
    schema: 1,
    type: 'SourcePreparationCandidateV1',
    repository_id: REPOSITORY_ID,
    version: VERSION,
    source_commit: SOURCE,
    execution_base_commit: 'b'.repeat(40),
    plan: {
      path: 'docs/plans/active/session-relay-prebuilt-cli-distribution.md',
      source_blob_sha256: hash,
    },
    docks_red: {
      sha256: hash,
      pre_production_commit: 'c'.repeat(40),
      test_blobs: [],
    },
    companion: {
      repository_id: 'DocksDocks/public',
      validation_ref: 'refs/heads/preflight/session-relay-test',
      commit: 'a'.repeat(40),
      plan_path: 'docs/plans/active/session-relay-cli-installation.md',
      input_sha256: hash,
      execution_base_commit: 'd'.repeat(40),
      review_receipt_sha256: hash,
      red_receipt_sha256: hash,
      status: 'blocked',
      blocked_reason: 'Awaiting the four independently hashed `session-relay--v0.12.0` production asset digests.',
    },
    preflight: {
      sha256: hash,
      workflow_file: '.github/workflows/build-binaries.yml',
      workflow_blob_id: 'e'.repeat(40),
      run_database_id: 101,
      run_attempt: 1,
    },
    source_ci: {
      sha256: hash,
      workflow_file: '.github/workflows/ci.yml',
      workflow_blob_id: 'f'.repeat(40),
      run_database_id: 102,
      run_attempt: 1,
    },
    checks: Array.from({ length: 6 }, (_, index) => ({
      id: `A${index + 1}`,
      steps: [{ argv: ['node', `A${index + 1}`], exit_code: 0, stdout_sha256: hash, stderr_sha256: hash }],
      exit_code: 0,
      stdout_sha256: hash,
      stderr_sha256: hash,
    })),
    created_at: '2026-07-17T00:00:00.000Z',
  };
  return writeCanonical(directory, 'proof.json', {
    schema: 1,
    type: 'SourcePreparationProofV1',
    repository_id: REPOSITORY_ID,
    version: VERSION,
    source_commit: SOURCE,
    tag_commit: SOURCE,
    evidence_commit: '6'.repeat(40),
    shipped_commit: '2'.repeat(40),
    promoted_commit: '2'.repeat(40),
    candidate,
    candidate_sha256: sha256(Buffer.from(canonicalize(candidate))),
    plans: {
      source_path: 'docs/plans/active/session-relay-prebuilt-cli-distribution.md',
      source_sha256: '7'.repeat(64),
      evidence_path: 'docs/plans/active/session-relay-prebuilt-cli-distribution.md',
      evidence_sha256: '8'.repeat(64),
      finished_path: 'docs/plans/finished/2026-07-17-session-relay-prebuilt-cli-distribution.md',
      finished_sha256: '9'.repeat(64),
    },
    completion_review_sha256: '5'.repeat(64),
    source_ancestry: {
      source_commit: SOURCE,
      evidence_commit: '6'.repeat(40),
      shipped_commit: '2'.repeat(40),
      verified: true,
    },
    non_plan_tree_equivalence: {
      source_commit: SOURCE,
      shipped_commit: '2'.repeat(40),
      excluded_paths: [
        'docs/plans/active/session-relay-prebuilt-cli-distribution.md',
        'docs/plans/finished/2026-07-17-session-relay-prebuilt-cli-distribution.md',
      ],
      verified: true,
    },
    public_repository_id: 'DocksDocks/public',
    public_reviewed_commit: 'a'.repeat(40),
    review_status: 'passed',
    bound_at: '2026-07-17T00:00:00.000Z',
  });
}

function optionsFor(directory, sourceProof, outputName = 'publication.json') {
  return new Map([
    ['source-proof', sourceProof.file],
    ['source-proof-sha256', sourceProof.digest],
    ['receipt-out', path.join(directory, outputName)],
  ]);
}

function run(id = 701, event = 'push') {
  return {
    id,
    run_attempt: 3,
    head_sha: SOURCE,
    head_branch: TAG,
    path: WORKFLOW_PATH,
    event,
    status: 'completed',
    conclusion: 'success',
    run_started_at: '2026-07-17T11:59:00.000Z',
    updated_at: '2026-07-17T12:01:00.000Z',
  };
}

function runAssetRecords() {
  return [...ASSETS].sort().map((name, index) => {
    const bytes = Buffer.from(`same-run:${name}`);
    return { name, size: bytes.length, digest: sha256(bytes), path: `/bound-run/${index}-${name}` };
  });
}
function runAttestationRecords(workflowRun, assets, mode = workflowRun.event === 'push' ? '' : 'publish-existing-tag') {
  const runners = {
    'aarch64-apple-darwin': ['ARM64', 'macOS'],
    'aarch64-unknown-linux-musl': ['ARM64', 'Linux'],
    'x86_64-apple-darwin': ['X64', 'macOS'],
    'x86_64-unknown-linux-musl': ['X64', 'Linux'],
  };
  return assets.filter(({ name }) => name !== 'SHA256SUMS').map((asset) => {
    const target = asset.name.replace(/^session-relay-/, '');
    return {
      asset_name: asset.name,
      inputs: {
        expected_commit: workflowRun.event === 'push' ? '' : SOURCE,
        expected_tag: workflowRun.event === 'push' ? '' : TAG,
        mode,
      },
      runner_arch: runners[target][0],
      runner_os: runners[target][1],
      schema: 'SessionRelayBinaryAttestationV1',
      sha256: asset.digest,
      source_commit: SOURCE,
      target,
      version_stdout: `session-relay ${VERSION}`,
      workflow_run_attempt: workflowRun.run_attempt,
      workflow_run_id: workflowRun.id,
    };
  });
}


function releaseAssets(runAssets, { missing = [], digestConflict = null, duplicate = null } = {}) {
  const assets = runAssets
    .filter(({ name }) => !missing.includes(name))
    .map(({ name, size, digest }, index) => ({
      id: 900 + index,
      name,
      size,
      digest: `sha256:${name === digestConflict ? 'f'.repeat(64) : digest}`,
      created_at: '2026-07-17T12:00:00.000Z',
      updated_at: '2026-07-17T12:00:30.000Z',
      uploader: { id: 41898282, login: 'github-actions[bot]', type: 'Bot' },
    }));
  if (duplicate) assets.push({ ...assets.find(({ name }) => name === duplicate), id: 1999 });
  return assets;
}

function prerelease(runAssets, overrides = {}) {
  return {
    id: 501,
    tag_name: TAG,
    target_commitish: SOURCE,
    prerelease: true,
    draft: false,
    body: PRERELEASE_BODY,
    created_at: '2026-07-17T12:00:00.000Z',
    published_at: '2026-07-17T12:00:30.000Z',
    author: { id: 41898282, login: 'github-actions[bot]', type: 'Bot' },
    assets: releaseAssets(runAssets),
    ...overrides,
  };
}

function fakeAdapter({
  tagCommit = SOURCE,
  runs = [run()],
  release = prerelease(runAssetRecords()),
  runAssets = runAssetRecords(),
  attestations = runAttestationRecords(runs[0] ?? run(), runAssets),
  liveReleaseAssets = runAssets,
  checksumEntries = runAssets
    .filter(({ name }) => name !== 'SHA256SUMS')
    .map(({ name, digest }) => ({ name, digest })),
  publisher = { id: 41898282, login: 'github-actions[bot]', type: 'Bot' },
  releaseDriftAt = null,
} = {}) {
  const state = {
    tagCommit,
    runs: [...runs],
    release: release ? structuredClone(release) : null,
    runAssets: structuredClone(runAssets),
    attestations: structuredClone(attestations),
    liveReleaseAssets: structuredClone(liveReleaseAssets),
    checksumEntries: structuredClone(checksumEntries),
    publisher: structuredClone(publisher),
    releaseDriftAt,
    pushed: [],
    dispatched: [],
    watched: [],
    downloaded: [],
    releaseDownloads: 0,
    releaseQueries: 0,
    releaseDownloadRequests: [],
    uploaded: [],
    created: 0,
    edited: 0,
    sleeps: 0,
    cleaned: 0,
  };
  const adapter = {
    getTagCommit() { return state.tagCommit; },
    pushTag(commit) { state.pushed.push(commit); state.tagCommit = commit; },
    getRelease() {
      state.releaseQueries += 1;
      if (!state.release) return null;
      const value = structuredClone(state.release);
      if (state.releaseDriftAt === state.releaseQueries) value.assets[0].id += 10_000;
      return value;
    },
    getPublisherIdentity() { return structuredClone(state.publisher); },
    listRuns() { return structuredClone(state.runs); },
    dispatchRecovery(commit) { state.dispatched.push(commit); },
    watchRun(id) { state.watched.push(id); },
    getRun(id) { return structuredClone(state.runs.find((candidate) => candidate.id === id)); },
    downloadRunAssets(id) {
      state.downloaded.push(id);
      return { directory: `/bound-run/${id}`, assets: structuredClone(state.runAssets), attestations: structuredClone(state.attestations) };
    },
    downloadReleaseAssets(releaseId, assets) {
      state.releaseDownloads += 1;
      state.releaseDownloadRequests.push({ releaseId, assetIds: assets.map(({ id }) => id) });
      return {
        directory: '/live-release',
        assets: structuredClone(state.liveReleaseAssets),
        checksumEntries: structuredClone(state.checksumEntries),
      };
    },
    createPrerelease(bundle) {
      state.created += 1;
      assert.deepEqual(bundle.assets.map(({ name }) => name).sort(), [...ASSETS].sort());
      state.release = prerelease(state.runAssets);
    },
    uploadReleaseAsset(asset) {
      state.uploaded.push(asset.name);
      state.release.assets.push({
        id: 1500 + state.uploaded.length,
        name: asset.name,
        size: asset.size,
        digest: `sha256:${asset.digest}`,
        created_at: '2026-07-17T12:00:00.000Z',
        updated_at: '2026-07-17T12:00:30.000Z',
        uploader: { id: 41898282, login: 'github-actions[bot]', type: 'Bot' },
      });
    },
    editStable() { state.edited += 1; state.release.prerelease = false; state.release.body = STABLE_BODY; },
    sleep() { state.sleeps += 1; },
    now() { return '2026-07-17T12:00:00.000Z'; },
    cleanupRunAssets() { state.cleaned += 1; },
  };
  return { adapter, state };
}

function receiptAt(file) {
  const bytes = fs.readFileSync(file);
  assert.equal(bytes.toString(), canonicalize(JSON.parse(bytes)));
  return JSON.parse(bytes);
}

function expectConflict(action, pattern) {
  assert.throws(action, pattern);
  checks += 1;
}

function publish(directory, adapterState, configureOptions) {
  const sourceProof = proof(directory);
  const options = optionsFor(directory, sourceProof);
  configureOptions?.(options);
  const result = publishReviewed(options, adapterState.adapter);
  return { result, sourceProof, options, receipt: receiptAt(options.get('receipt-out')) };
}

function makePromotion(directory, sourceProof, publicationFile, outcome = 'success') {
  const publicationBytes = fs.readFileSync(publicationFile);
  return writeCanonical(directory, 'promotion.json', {
    schema: 1,
    type: 'PromotionReceiptV1',
    outcome,
    source_proof_sha256: sourceProof.digest,
    publication_receipt_sha256: sha256(publicationBytes),
  });
}

function finalizeOptions(directory, sourceProof, publicationFile, promotion, output = 'final.json') {
  const publicationBytes = fs.readFileSync(publicationFile);
  return new Map([
    ['source-proof', sourceProof.file],
    ['source-proof-sha256', sourceProof.digest],
    ['publication', publicationFile],
    ['publication-sha256', sha256(publicationBytes)],
    ['promotion', promotion.file],
    ['promotion-sha256', promotion.digest],
    ['receipt-out', path.join(directory, output)],
  ]);
}

function publicationWorkflowScript() {
  const workflow = parseYaml(fs.readFileSync(WORKFLOW_PATH, 'utf8'));
  assert.equal(workflow.permissions.contents, 'read');
  assert.equal(workflow.jobs.publish.permissions.contents, 'write');
  const step = workflow.jobs.publish.steps.find(({ env }) => env?.GH_TOKEN);
  assert.ok(step?.run, 'publish job must contain a GH_TOKEN-scoped publication step');
  return step.run;
}

function githubAsset(file, id) {
  const bytes = fs.readFileSync(file);
  return {
    id,
    name: path.basename(file),
    size: bytes.length,
    digest: `sha256:${sha256(bytes)}`,
  };
}

const FAKE_GH = String.raw`#!/usr/bin/env node
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const args = process.argv.slice(2);
const stateFile = process.env.FAKE_GH_STATE;
const state = JSON.parse(fs.readFileSync(stateFile, 'utf8'));
const save = () => fs.writeFileSync(stateFile, JSON.stringify(state));
const fail = (message, code = 1) => {
  save();
  process.stderr.write(message + '\n');
  process.exit(code);
};
const asset = (file) => {
  const bytes = fs.readFileSync(file);
  return {
    id: state.nextAssetId++,
    name: path.basename(file),
    size: bytes.length,
    digest: 'sha256:' + crypto.createHash('sha256').update(bytes).digest('hex'),
  };
};
if (args[0] === 'api') {
  state.queries += 1;
  if (state.queryFailure) {
    if (state.queryFailure.withResponse) {
      process.stdout.write('HTTP/2.0 ' + state.queryFailure.status + ' Error\r\ncontent-type: application/json\r\n\r\n{}\n');
    }
    fail(state.queryFailure.message);
  }
  if (!state.release) {
    process.stdout.write('HTTP/2.0 404 Not Found\r\ncontent-type: application/json\r\n\r\n{"message":"Not Found"}\n');
    fail('gh: Not Found (HTTP 404)');
  }
  process.stdout.write('HTTP/2.0 200 OK\r\ncontent-type: application/json\r\n\r\n' + JSON.stringify(state.release) + '\n');
  save();
  process.exit(0);
}
if (args[0] !== 'release') fail('unexpected gh command: ' + args.join(' '));
const operation = args[1];
const tag = args[2];
if (tag !== process.env.TAG) fail('unexpected release tag');
if (operation === 'create') {
  if (state.release) fail('release already exists');
  const optionStart = args.findIndex((value, index) => index >= 3 && value.startsWith('--'));
  const files = args.slice(3, optionStart < 0 ? args.length : optionStart);
  const option = (name) => {
    const index = args.indexOf(name);
    return index < 0 ? null : args[index + 1];
  };
  if (!args.includes('--prerelease') || !args.includes('--verify-tag')) fail('create flags are incomplete');
  state.release = {
    id: 9001,
    tag_name: tag,
    draft: false,
    prerelease: true,
    name: option('--title'),
    body: fs.readFileSync(option('--notes-file'), 'utf8'),
    assets: files.map(asset),
  };
  state.mutations.push({ operation, names: state.release.assets.map(({ name }) => name) });
  save();
  process.exit(0);
}
if (operation === 'upload') {
  const files = args.slice(3);
  if (state.raceOnUpload && files.length > 0) {
    const raced = asset(files[0]);
    raced.digest = 'sha256:' + 'f'.repeat(64);
    state.release.assets.push(raced);
  }
  for (const file of files) {
    if (state.release.assets.some(({ name }) => name === path.basename(file))) {
      fail('asset already exists');
    }
  }
  const uploaded = files.map(asset);
  state.release.assets.push(...uploaded);
  state.mutations.push({ operation, names: uploaded.map(({ name }) => name) });
  save();
  process.exit(0);
}
fail('unexpected release operation: ' + operation);
`;

function runPublicationWorkflow(root, {
  existing = null,
  releaseOverrides = {},
  assetOverrides = (assets) => assets,
  queryFailure = null,
  raceOnUpload = false,
} = {}) {
  const directory = fs.mkdtempSync(path.join(root, 'workflow-'));
  const releaseDirectory = path.join(directory, 'session-relay-release');
  const binDirectory = path.join(directory, 'bin');
  fs.mkdirSync(releaseDirectory);
  fs.mkdirSync(binDirectory);
  const files = ASSETS.map((name, index) => {
    const file = path.join(releaseDirectory, name);
    fs.writeFileSync(file, Buffer.from(`${name}:${index}\n`));
    return file;
  });
  fs.writeFileSync(
    path.join(directory, 'session-relay-prerelease.md'),
    PRERELEASE_BODY,
  );
  const localAssets = files.map((file, index) => githubAsset(file, 100 + index));
  const present = existing === null
    ? null
    : localAssets.filter(({ name }) => existing.includes(name));
  const release = present === null
    ? null
    : {
        id: 88,
        tag_name: TAG,
        draft: false,
        prerelease: true,
        name: `Session Relay ${VERSION}`,
        body: PRERELEASE_BODY,
        assets: assetOverrides(structuredClone(present)),
        ...releaseOverrides,
      };
  const stateFile = path.join(directory, 'state.json');
  fs.writeFileSync(stateFile, JSON.stringify({
    release,
    mutations: [],
    queries: 0,
    nextAssetId: 1000,
    queryFailure,
    raceOnUpload,
  }));
  const gh = path.join(binDirectory, 'gh');
  fs.writeFileSync(gh, FAKE_GH, { mode: 0o755 });
  const script = publicationWorkflowScript();
  const result = spawnSync('bash', ['-c', script], {
    cwd: process.cwd(),
    encoding: 'utf8',
    env: {
      ...process.env,
      FAKE_GH_STATE: stateFile,
      GH_TOKEN: 'fake-token',
      GITHUB_REPOSITORY: REPOSITORY_ID,
      PATH: `${binDirectory}:${process.env.PATH}`,
      RUNNER_TEMP: directory,
      TAG,
      VERSION,
    },
  });
  return {
    result,
    state: JSON.parse(fs.readFileSync(stateFile, 'utf8')),
    localAssets,
    script,
  };
}

function assertWorkflowFailure(run, message) {
  assert.notEqual(run.result.status, 0, message);
  assert.deepEqual(run.state.mutations, [], `${message}: mutation occurred before rejection`);
}

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-publication-contract-'));
try {
  {
    const absent = runPublicationWorkflow(root);
    assert.equal(absent.result.status, 0, absent.result.stderr);
    assert.deepEqual(absent.state.mutations, [
      { operation: 'create', names: ASSETS },
    ]);
    assert.ok(absent.state.queries >= 2, 'created release must be authoritatively re-queried');
    assert.deepEqual(absent.state.release.assets.map(({ name }) => name), ASSETS);

    const partial = runPublicationWorkflow(root, { existing: ASSETS.slice(0, 2) });
    assert.equal(partial.result.status, 0, partial.result.stderr);
    assert.deepEqual(partial.state.mutations, [
      { operation: 'upload', names: ASSETS.slice(2) },
    ]);
    assert.deepEqual(partial.state.release.assets.map(({ name }) => name), ASSETS);

    const complete = runPublicationWorkflow(root, { existing: ASSETS });
    assert.equal(complete.result.status, 0, complete.result.stderr);
    assert.deepEqual(complete.state.mutations, []);
    assert.ok(complete.state.queries >= 2, 'complete release must still receive a final authoritative query');

    assert.doesNotMatch(complete.script, /--clobber|release\s+delete|api\s+.*\s-X\s+DELETE/i);

    assertWorkflowFailure(runPublicationWorkflow(root, {
      existing: ASSETS,
      assetOverrides: (assets) => {
        assets[0].size += 1;
        return assets;
      },
    }), 'existing asset size conflict');
    assertWorkflowFailure(runPublicationWorkflow(root, {
      existing: ASSETS,
      assetOverrides: (assets) => {
        assets[0].digest = `sha256:${'f'.repeat(64)}`;
        return assets;
      },
    }), 'existing asset digest conflict');
    assertWorkflowFailure(runPublicationWorkflow(root, {
      existing: ASSETS,
      assetOverrides: (assets) => [...assets, {
        id: 999,
        name: 'unknown-asset',
        size: 1,
        digest: `sha256:${'0'.repeat(64)}`,
      }],
    }), 'unknown release asset');
    assertWorkflowFailure(runPublicationWorkflow(root, {
      existing: ASSETS,
      assetOverrides: (assets) => [...assets, structuredClone(assets[0])],
    }), 'duplicate release asset');

    for (const [field, value] of [
      ['tag_name', 'session-relay--v9.9.9'],
      ['draft', true],
      ['prerelease', false],
      ['name', 'drifted title'],
      ['body', 'drifted body'],
    ]) {
      assertWorkflowFailure(runPublicationWorkflow(root, {
        existing: ASSETS,
        releaseOverrides: { [field]: value },
      }), `release ${field} conflict`);
    }

    assertWorkflowFailure(runPublicationWorkflow(root, {
      queryFailure: { withResponse: true, status: 500, message: 'gh: server failure (HTTP 500)' },
    }), 'non-404 query failure');
    assertWorkflowFailure(runPublicationWorkflow(root, {
      queryFailure: { withResponse: false, status: 0, message: 'gh: authentication failed' },
    }), 'authentication failure without an HTTP response');
    assertWorkflowFailure(runPublicationWorkflow(root, {
      existing: ASSETS.slice(0, 2),
      raceOnUpload: true,
    }), 'upload race');
    checks += 1;
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'tag-no-run-'));
    const fake = fakeAdapter({ tagCommit: null, runs: [], release: null });
    const sourceProof = proof(directory);
    const options = optionsFor(directory, sourceProof);
    expectConflict(() => publishReviewed(options, fake.adapter), /bounded.*tag-push.*run/i);
    assert.deepEqual(fake.state.pushed, [SOURCE]);
    assert.equal(fake.state.dispatched.length, 0);
    assert.equal(fake.state.sleeps, POLL_LIMIT - 1);
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'missing-release-'));
    const fake = fakeAdapter({ release: null });
    const { receipt } = publish(directory, fake);
    assert.equal(fake.state.created, 1);
    assert.deepEqual(fake.state.downloaded, [701]);
    assert.equal(fake.state.cleaned, 1);
    assert.deepEqual(receipt.workflow, {
      attempt: 3,
      conclusion: 'success',
      event: 'push',
      file: WORKFLOW_PATH,
      head_sha: SOURCE,
      inputs: { expected_commit: '', expected_tag: '', mode: '' },
      path: WORKFLOW_PATH,
      run_id: 701,
      workflow_sha: SOURCE,
    });
    assert.deepEqual(receipt.assets.map(({ digest }) => digest), runAssetRecords().map(({ digest }) => digest));
    checks += 1;
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'partial-'));
    const assets = runAssetRecords();
    const missing = [assets[1].name, assets[4].name];
    const fake = fakeAdapter({ release: prerelease(assets, { assets: releaseAssets(assets, { missing }) }) });
    publish(directory, fake);
    assert.deepEqual(fake.state.uploaded.sort(), missing.sort());
    assert.equal(fake.state.created, 0);
    checks += 1;
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'partial-conflict-'));
    const assets = runAssetRecords();
    const fake = fakeAdapter({ release: prerelease(assets, { assets: releaseAssets(assets, { missing: [assets[1].name], digestConflict: assets[0].name }) }) });
    const sourceProof = proof(directory);
    expectConflict(() => publishReviewed(optionsFor(directory, sourceProof), fake.adapter), /asset.*digest.*conflict/i);
    assert.equal(fake.state.uploaded.length, 0);
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'failed-publish-partial-'));
    const assets = runAssetRecords();
    const failedRun = { ...run(), conclusion: 'failure' };
    const recoveryRun = run(704, 'workflow_dispatch');
    const missing = [assets[2].name];
    const fake = fakeAdapter({
      runs: [failedRun],
      release: prerelease(assets, { assets: releaseAssets(assets, { missing }) }),
      runAssets: assets,
      attestations: runAttestationRecords(recoveryRun, assets),
    });
    const original = fake.adapter.listRuns;
    fake.adapter.listRuns = () => {
      if (fake.state.dispatched.length > 0) fake.state.runs = [failedRun, recoveryRun];
      return original();
    };
    const { receipt } = publish(directory, fake);
    assert.deepEqual(fake.state.uploaded, missing);
    assert.deepEqual(fake.state.downloaded, [704]);
    assert.equal(receipt.workflow.run_id, 704);
    checks += 1;
  }

  for (const history of [
    [{ ...run(702, 'workflow_dispatch'), conclusion: 'failure' }],
    [{ ...run(701, 'push'), conclusion: 'failure' }, { ...run(702, 'workflow_dispatch'), conclusion: 'failure' }],
  ]) {
    const directory = fs.mkdtempSync(path.join(root, `failed-history-${history.length}-`));
    const assets = runAssetRecords();
    const recoveryRun = run(705, 'workflow_dispatch');
    const missing = [assets[3].name];
    const fake = fakeAdapter({
      runs: history,
      release: prerelease(assets, { assets: releaseAssets(assets, { missing }) }),
      runAssets: assets,
      attestations: runAttestationRecords(recoveryRun, assets),
    });
    const originalList = fake.adapter.listRuns;
    fake.adapter.listRuns = () => {
      if (fake.state.dispatched.length > 0) fake.state.runs = [...history, recoveryRun];
      return originalList();
    };
    const { receipt } = publish(directory, fake);
    assert.deepEqual(fake.state.dispatched, [SOURCE]);
    assert.deepEqual(fake.state.downloaded, [705]);
    assert.deepEqual(fake.state.uploaded, missing);
    assert.equal(receipt.workflow.run_id, 705);
    checks += 1;
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'validate-only-run-'));
    const assets = runAssetRecords();
    const dispatchedRun = run(703, 'workflow_dispatch');
    const fake = fakeAdapter({
      runs: [dispatchedRun],
      release: null,
      runAssets: assets,
      attestations: runAttestationRecords(dispatchedRun, assets, 'validate-only'),
    });
    const sourceProof = proof(directory);
    expectConflict(() => publishReviewed(optionsFor(directory, sourceProof), fake.adapter), /attestation.*input|publish-existing-tag/i);
    assert.equal(fake.state.created, 0);
  }
  {
    const directory = fs.mkdtempSync(path.join(root, 'validate-only-complete-release-'));
    const assets = runAssetRecords();
    const dispatchedRun = run(706, 'workflow_dispatch');
    const fake = fakeAdapter({
      runs: [dispatchedRun],
      release: prerelease(assets),
      runAssets: assets,
      attestations: runAttestationRecords(dispatchedRun, assets, 'validate-only'),
    });
    const sourceProof = proof(directory);
    expectConflict(() => publishReviewed(optionsFor(directory, sourceProof), fake.adapter), /attestation.*input|publish-existing-tag/i);
    assert.equal(fake.state.created, 0);
    assert.equal(fake.state.uploaded.length, 0);
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'existing-tag-dispatch-'));
    const assets = runAssetRecords();
    const recoveryRun = run(702, 'workflow_dispatch');
    const fake = fakeAdapter({
      runs: [],
      release: null,
      runAssets: assets,
      attestations: runAttestationRecords(recoveryRun, assets),
    });
    let lists = 0;
    const original = fake.adapter.listRuns;
    fake.adapter.listRuns = () => {
      lists += 1;
      if (lists === 1) return [];
      if (fake.state.dispatched.length > 0) fake.state.runs = [recoveryRun];
      return original();
    };
    const { receipt } = publish(directory, fake);
    assert.deepEqual(fake.state.dispatched, [SOURCE]);
    assert.equal(receipt.workflow.run_id, 702);
    assert.equal(receipt.workflow.event, 'workflow_dispatch');
    assert.deepEqual(receipt.workflow.inputs, {
      expected_commit: SOURCE,
      expected_tag: TAG,
      mode: 'publish-existing-tag',
    });
    checks += 1;
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'duplicate-runs-'));
    const fake = fakeAdapter({ runs: [run(701), run(702)], release: null });
    const sourceProof = proof(directory);
    expectConflict(() => publishReviewed(optionsFor(directory, sourceProof), fake.adapter), /multiple.*workflow runs|duplicate.*workflow runs/i);
    assert.equal(fake.state.created, 0);
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'successful-run-expired-complete-'));
    const fake = fakeAdapter();
    let downloadAttempts = 0;
    fake.adapter.downloadRunAssets = () => {
      downloadAttempts += 1;
      throw new SessionRelayReleaseError('Actions artifacts unavailable', 'failure');
    };
    const staged = publish(directory, fake);
    assert.equal(staged.receipt.workflow.run_id, 701);
    assert.equal(fake.state.dispatched.length, 0);

    const resumeOptions = optionsFor(directory, staged.sourceProof, 'successful-expired-resume.json');
    resumeOptions.set('resume-publication', staged.options.get('receipt-out'));
    resumeOptions.set('resume-publication-sha256', sha256(fs.readFileSync(staged.options.get('receipt-out'))));
    publishReviewed(resumeOptions, fake.adapter);

    const promotion = makePromotion(directory, staged.sourceProof, staged.options.get('receipt-out'));
    const finalOptions = finalizeOptions(directory, staged.sourceProof, staged.options.get('receipt-out'), promotion, 'successful-expired-final.json');
    finalizeReviewed(finalOptions, fake.adapter, acceptPromotionReceipt);
    assert.equal(fake.state.edited, 1);
    assert.equal(fake.state.downloaded.length, 0);
    assert.equal(downloadAttempts, 0);
    assert.equal(fake.state.releaseDownloads, 1);
    assert.deepEqual(fake.state.releaseDownloadRequests, [{
      releaseId: 501,
      assetIds: fake.state.release.assets.map(({ id }) => id),
    }]);
    checks += 1;
  }

  for (const conflict of ['timestamp', 'publisher', 'mutual-publisher', 'database-id', 'digest', 'manifest']) {
    const directory = fs.mkdtempSync(path.join(root, `live-release-${conflict}-conflict-`));
    const fake = fakeAdapter();
    if (conflict === 'timestamp') {
      fake.state.release.assets[0].updated_at = '2026-07-17T12:02:00.000Z';
    } else if (conflict === 'digest') {
      fake.state.liveReleaseAssets[0].digest = 'f'.repeat(64);
    } else if (conflict === 'publisher') {
      fake.state.release.assets[0].uploader = { id: 1, login: 'replacement-user' };
    } else if (conflict === 'mutual-publisher') {
      const replacement = { id: 1, login: 'replacement-user', type: 'User' };
      fake.state.release.author = replacement;
      for (const asset of fake.state.release.assets) asset.uploader = replacement;
    } else if (conflict === 'database-id') {
      fake.state.release.assets[1].id = fake.state.release.assets[0].id;
    } else {
      fake.state.checksumEntries[0].digest = 'f'.repeat(64);
    }
    const sourceProof = proof(directory);
    expectConflict(
      () => publishReviewed(optionsFor(directory, sourceProof), fake.adapter),
      conflict === 'timestamp' || conflict.includes('publisher')
        ? /timestamp|run window|publisher/i
        : conflict === 'database-id' ? /database identity|duplicate/i : /digest|checksum/i,
    );
    assert.equal(fake.state.dispatched.length, 0);
    assert.equal(fake.state.created, 0);
    assert.equal(fake.state.uploaded.length, 0);
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'live-release-requery-drift-'));
    const fake = fakeAdapter({ releaseDriftAt: 3 });
    const sourceProof = proof(directory);
    expectConflict(() => publishReviewed(optionsFor(directory, sourceProof), fake.adapter), /drift|changed/i);
    assert.equal(fake.state.dispatched.length, 0);
    assert.equal(fake.state.created, 0);
    assert.equal(fake.state.uploaded.length, 0);
  }

  for (const [label, release] of [
    ['absent', null],
    ['partial', prerelease(runAssetRecords(), { assets: releaseAssets(runAssetRecords(), { missing: [ASSETS[0]] }) })],
  ]) {
    const directory = fs.mkdtempSync(path.join(root, `successful-run-expired-${label}-`));
    const fake = fakeAdapter({ release });
    let downloadAttempts = 0;
    fake.adapter.downloadRunAssets = () => {
      downloadAttempts += 1;
      throw new SessionRelayReleaseError('Actions artifacts unavailable', 'failure');
    };
    const sourceProof = proof(directory);
    expectConflict(() => publishReviewed(optionsFor(directory, sourceProof), fake.adapter), /artifacts unavailable/i);
    assert.equal(fake.state.dispatched.length, 0);
    assert.equal(fake.state.created, 0);
    assert.equal(fake.state.uploaded.length, 0);
    assert.equal(downloadAttempts, 1);
  }


  {
    const directory = fs.mkdtempSync(path.join(root, 'mismatched-run-'));
    const bad = { ...run(), head_sha: '9'.repeat(40) };
    const fake = fakeAdapter({ runs: [bad], release: null });
    const sourceProof = proof(directory);
    expectConflict(() => publishReviewed(optionsFor(directory, sourceProof), fake.adapter), /workflow run.*identity.*conflict/i);
    assert.equal(fake.state.dispatched.length, 0);
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'resume-'));
    const fake = fakeAdapter();
    const first = publish(directory, fake);
    const secondOptions = optionsFor(directory, first.sourceProof, 'resumed.json');
    secondOptions.set('resume-publication', first.options.get('receipt-out'));
    secondOptions.set('resume-publication-sha256', sha256(fs.readFileSync(first.options.get('receipt-out'))));
    const before = { created: fake.state.created, uploaded: fake.state.uploaded.length };
    const second = publishReviewed(secondOptions, fake.adapter);
    const resumed = receiptAt(secondOptions.get('receipt-out'));
    assert.equal(second.receipt.transition, 'reconciled');
    assert.deepEqual(resumed.workflow, first.receipt.workflow);
    assert.deepEqual({ created: fake.state.created, uploaded: fake.state.uploaded.length }, before);
    checks += 1;
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'resume-partial-release-conflict-'));
    const fake = fakeAdapter();
    const first = publish(directory, fake);
    const resumeOptions = optionsFor(directory, first.sourceProof, 'partial-resume.json');
    resumeOptions.set('resume-publication', first.options.get('receipt-out'));
    resumeOptions.set('resume-publication-sha256', sha256(fs.readFileSync(first.options.get('receipt-out'))));
    fake.state.release.assets.pop();
    const before = { created: fake.state.created, uploaded: fake.state.uploaded.length };
    expectConflict(() => publishReviewed(resumeOptions, fake.adapter), /resume publication release asset|resume publication release identity/i);
    assert.deepEqual({ created: fake.state.created, uploaded: fake.state.uploaded.length }, before);
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'cryptographic-conflict-no-recovery-'));
    const fake = fakeAdapter({ release: null });
    fake.adapter.downloadRunAssets = () => {
      throw new SessionRelayReleaseError('same-run checksum digest conflict');
    };
    const sourceProof = proof(directory);
    expectConflict(() => publishReviewed(optionsFor(directory, sourceProof), fake.adapter), /checksum digest conflict/i);
    assert.equal(fake.state.dispatched.length, 0);
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'failed-recovery-resume-'));
    const assets = runAssetRecords();
    const failedRun = { ...run(), conclusion: 'failure' };
    const recoveryRun = run(704, 'workflow_dispatch');
    const fake = fakeAdapter({
      runs: [failedRun],
      release: prerelease(assets, { assets: releaseAssets(assets, { missing: [assets[2].name] }) }),
      runAssets: assets,
      attestations: runAttestationRecords(recoveryRun, assets),
    });
    const originalList = fake.adapter.listRuns;
    fake.adapter.listRuns = () => {
      if (fake.state.dispatched.length > 0) fake.state.runs = [failedRun, recoveryRun];
      return originalList();
    };
    const first = publish(directory, fake);
    const resumeOptions = optionsFor(directory, first.sourceProof, 'failed-recovery-resumed.json');
    resumeOptions.set('resume-publication', first.options.get('receipt-out'));
    resumeOptions.set('resume-publication-sha256', sha256(fs.readFileSync(first.options.get('receipt-out'))));
    const downloads = fake.state.downloaded.length;
    fake.adapter.downloadRunAssets = () => { throw new Error('Actions artifacts expired'); };
    publishReviewed(resumeOptions, fake.adapter);
    assert.equal(fake.state.downloaded.length, downloads);
    checks += 1;
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'complete-resume-expired-artifacts-'));
    const fake = fakeAdapter();
    const first = publish(directory, fake);
    const resumeOptions = optionsFor(directory, first.sourceProof, 'expired-resumed.json');
    resumeOptions.set('resume-publication', first.options.get('receipt-out'));
    resumeOptions.set('resume-publication-sha256', sha256(fs.readFileSync(first.options.get('receipt-out'))));
    const downloads = fake.state.downloaded.length;
    fake.adapter.downloadRunAssets = () => { throw new Error('Actions artifacts expired'); };
    publishReviewed(resumeOptions, fake.adapter);
    assert.equal(fake.state.downloaded.length, downloads);
    checks += 1;
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'finalize-'));
    const fake = fakeAdapter();
    const staged = publish(directory, fake);
    const promotion = makePromotion(directory, staged.sourceProof, staged.options.get('receipt-out'));
    const options = finalizeOptions(directory, staged.sourceProof, staged.options.get('receipt-out'), promotion);
    const injectedPromotionAdapter = Object.freeze({ authority: 'promotion-test-only' });
    const promotionValidator = (receipt, context, validatorAdapter) => {
      assert.strictEqual(validatorAdapter, injectedPromotionAdapter);
      assert.deepEqual(Object.keys(context).sort(), ['proof', 'publication']);
      assert.deepEqual(Object.keys(context.proof).sort(), ['bytes', 'digest', 'path', 'value']);
      assert.deepEqual(Object.keys(context.publication).sort(), ['bytes', 'digest', 'path', 'value']);
      assert.equal(context.proof.digest, staged.sourceProof.digest);
      assert.equal(context.publication.digest, sha256(fs.readFileSync(staged.options.get('receipt-out'))));
      assert.deepEqual(receipt, JSON.parse(fs.readFileSync(promotion.file, 'utf8')));
      return receipt;
    };
    finalizeReviewed(options, fake.adapter, promotionValidator, injectedPromotionAdapter);
    const finalReceipt = receiptAt(options.get('receipt-out'));
    assert.equal(fake.state.edited, 1);
    assert.equal(finalReceipt.release_state, 'stable');
    assert.equal(finalReceipt.body_sha256, sha256(Buffer.from(STABLE_BODY)));
    assert.deepEqual(finalReceipt.workflow, staged.receipt.workflow);
    checks += 1;

    const stableOptions = finalizeOptions(directory, staged.sourceProof, staged.options.get('receipt-out'), promotion, 'already-stable.json');
    finalizeReviewed(stableOptions, fake.adapter, acceptPromotionReceipt);
    assert.equal(fake.state.edited, 1);
    assert.equal(receiptAt(stableOptions.get('receipt-out')).transition, 'already_stable');
    checks += 1;
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'finalize-crash-after-edit-'));
    const fake = fakeAdapter();
    const staged = publish(directory, fake);
    const promotion = makePromotion(directory, staged.sourceProof, staged.options.get('receipt-out'));
    const crashedOptions = finalizeOptions(directory, staged.sourceProof, staged.options.get('receipt-out'), promotion, 'crashed-final.json');
    const mutateStable = fake.adapter.editStable;
    fake.adapter.editStable = () => {
      mutateStable();
      throw new Error('simulated process death after editStable');
    };
    assert.throws(
      () => finalizeReviewed(crashedOptions, fake.adapter, acceptPromotionReceipt),
      /simulated process death after editStable/,
      'finalization crash fixture must propagate termination after editStable',
    );
    assert.equal(fake.state.edited, 1);
    assert.equal(fake.state.release.prerelease, false);
    assert.equal(fs.existsSync(crashedOptions.get('receipt-out')), false, 'finalization crash fixture must not write a receipt');

    fake.adapter.editStable = () => { throw new Error('base recovery attempted a second editStable'); };
    const recoveryOptions = finalizeOptions(directory, staged.sourceProof, staged.options.get('receipt-out'), promotion, 'fresh-path-recovery.json');
    finalizeReviewed(recoveryOptions, fake.adapter, acceptPromotionReceipt);
    assert.equal(fake.state.edited, 1, 'fresh-path base recovery must perform exactly one total editStable');
    const recovered = receiptAt(recoveryOptions.get('receipt-out'));
    assert.equal(recovered.release_state, 'stable');
    assert.equal(recovered.transition, 'already_stable');
    assert.equal(fs.readFileSync(recoveryOptions.get('receipt-out'), 'utf8'), canonicalize(recovered));
    checks += 1;
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'finalize-expired-artifacts-'));
    const fake = fakeAdapter();
    const staged = publish(directory, fake);
    const promotion = makePromotion(directory, staged.sourceProof, staged.options.get('receipt-out'));
    fake.adapter.downloadRunAssets = () => { throw new Error('Actions artifacts expired'); };
    const options = finalizeOptions(directory, staged.sourceProof, staged.options.get('receipt-out'), promotion);
    finalizeReviewed(options, fake.adapter, acceptPromotionReceipt);
    assert.equal(fake.state.edited, 1);
    checks += 1;
  }

  for (const kind of ['missing', 'rejecting']) {
    const directory = fs.mkdtempSync(path.join(root, `promotion-validator-${kind}-`));
    const fake = fakeAdapter();
    const staged = publish(directory, fake);
    const promotion = makePromotion(directory, staged.sourceProof, staged.options.get('receipt-out'));
    const options = finalizeOptions(directory, staged.sourceProof, staged.options.get('receipt-out'), promotion);
    const releaseQueries = fake.state.releaseQueries;
    const validator = kind === 'missing' ? undefined : () => { throw new SessionRelayReleaseError('promotion receipt rejected'); };
    expectConflict(
      () => finalizeReviewed(options, fake.adapter, validator),
      kind === 'missing' ? /promotion validator.*function/i : /promotion receipt rejected/i,
    );
    assert.equal(fake.state.edited, 0);
    assert.equal(fake.state.downloaded.length, 0);
    assert.equal(fake.state.releaseQueries, releaseQueries);
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'finalize-body-conflict-'));
    const fake = fakeAdapter();
    const staged = publish(directory, fake);
    const promotion = makePromotion(directory, staged.sourceProof, staged.options.get('receipt-out'));
    fake.state.release.body = 'drifted staging body';
    const options = finalizeOptions(directory, staged.sourceProof, staged.options.get('receipt-out'), promotion);
    expectConflict(() => finalizeReviewed(options, fake.adapter, acceptPromotionReceipt), /prerelease body.*conflict/i);
    assert.equal(fake.state.edited, 0);
  }

  {
    const directory = fs.mkdtempSync(path.join(root, 'finalize-release-conflict-'));
    const fake = fakeAdapter();
    const staged = publish(directory, fake);
    const promotion = makePromotion(directory, staged.sourceProof, staged.options.get('receipt-out'));
    fake.state.release.id += 1;
    const options = finalizeOptions(directory, staged.sourceProof, staged.options.get('receipt-out'), promotion);
    expectConflict(() => finalizeReviewed(options, fake.adapter, acceptPromotionReceipt), /release database identity conflict/i);
    assert.equal(fake.state.edited, 0);
  }

  console.log(`release publication contract: ${checks} production-handler cases passed`);
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
