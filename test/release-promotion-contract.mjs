import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import * as releasePromotion from '../../../scripts/lib/session-relay-release-promotion.mjs';
import {
  PROMOTION_ADAPTER_KEYS,
  fetchPromotionAuthoritativeRef,
  promoteReviewed,
  readPromotionJournalFromRepository,
  validatePromotionJournal,
  validatePromotionReceipt,
  validatePromotionReceiptForFinalization,
} from '../../../scripts/lib/session-relay-release-promotion.mjs';
import { canonicalize, LOCK_REF, PRERELEASE_BODY, SessionRelayReleaseError, TRANSACTION_REF } from '../../../scripts/lib/session-relay-release-core.mjs';
import { validateSourcePreparationProof } from '../../../scripts/lib/session-relay-release-preparation.mjs';
import { dispatchSessionRelayRelease } from '../../../scripts/lib/session-relay-release-cli.mjs';
const OLD_MAIN = '1'.repeat(40);
const TAG_COMMIT = '2'.repeat(40);
const PROMOTED_COMMIT = '3'.repeat(40);
const LOCK_COMMIT = '4'.repeat(40);
const RESTORE_COMMIT = '5'.repeat(40);
const REAPPLY_COMMIT = '6'.repeat(40);
const REPAIR_IMPLEMENTATION_COMMIT = 'e'.repeat(40);
const PUBLIC_REVIEWED_COMMIT = 'c3b542220d5a24a98ca05383bbe28afc2319b7e2';
const PUBLIC_RELEASE_COMMIT = '7'.repeat(40);
const PUBLIC_PLAN_COMMIT = '8'.repeat(40);
const DIGEST = (letter) => letter.repeat(64);
const BLOB = (letter) => letter.repeat(40);
const hash = (value) => createHash('sha256').update(Buffer.isBuffer(value) || typeof value === 'string' ? value : canonicalize(value)).digest('hex');
const HOST_ASSET_DIGEST = '5'.repeat(64);

const candidate = {
  schema: 1,
  type: 'SourcePreparationCandidateV1',
  repository_id: 'DocksDocks/docks',
  version: '0.12.0',
  source_commit: TAG_COMMIT,
  execution_base_commit: OLD_MAIN,
  plan: {
    path: 'docs/plans/active/session-relay-prebuilt-cli-distribution.md',
    source_blob_sha256: DIGEST('1'),
  },
  docks_red: {
    sha256: DIGEST('2'),
    pre_production_commit: OLD_MAIN,
    test_blobs: [{ path: 'plugins/session-relay/test/release-promotion-contract.mjs', blob_id: BLOB('a') }],
  },
  companion: {
    repository_id: 'DocksDocks/public',
    validation_ref: 'refs/heads/preflight/session-relay-0.12.0',
    commit: PUBLIC_REVIEWED_COMMIT,
    plan_path: 'docs/plans/active/session-relay-cli-installation.md',
    input_sha256: DIGEST('3'),
    execution_base_commit: BLOB('b'),
    review_receipt_sha256: DIGEST('4'),
    red_receipt_sha256: DIGEST('5'),
    status: 'blocked',
    blocked_reason: 'Awaiting the four independently hashed `session-relay--v0.12.0` production asset digests.',
  },
  preflight: {
    sha256: DIGEST('6'),
    workflow_file: '.github/workflows/build-binaries.yml',
    workflow_blob_id: BLOB('c'),
    run_database_id: 61,
    run_attempt: 1,
  },
  source_ci: {
    sha256: DIGEST('7'),
    workflow_file: '.github/workflows/ci.yml',
    workflow_blob_id: BLOB('d'),
    run_database_id: 71,
    run_attempt: 1,
  },
  checks: Array.from({ length: 6 }, (_, index) => ({
    id: `A${index + 1}`,
    steps: [{
      argv: ['node', `check-${index + 1}.mjs`],
      exit_code: 0,
      stdout_sha256: DIGEST('8'),
      stderr_sha256: DIGEST('9'),
    }],
    exit_code: 0,
    stdout_sha256: DIGEST('a'),
    stderr_sha256: DIGEST('b'),
  })),
  created_at: '2026-07-17T19:00:00.000Z',
};
const proofValue = {
  schema: 1,
  type: 'SourcePreparationProofV1',
  repository_id: 'DocksDocks/docks',
  version: '0.12.0',
  source_commit: TAG_COMMIT,
  tag_commit: TAG_COMMIT,
  evidence_commit: '8'.repeat(40),
  shipped_commit: PROMOTED_COMMIT,
  promoted_commit: PROMOTED_COMMIT,
  candidate,
  candidate_sha256: hash(candidate),
  plans: {
    source_path: 'docs/plans/active/session-relay-prebuilt-cli-distribution.md',
    source_sha256: DIGEST('c'),
    evidence_path: 'docs/plans/active/session-relay-prebuilt-cli-distribution.md',
    evidence_sha256: DIGEST('d'),
    finished_path: 'docs/plans/finished/2026-07-17-session-relay-prebuilt-cli-distribution.md',
    finished_sha256: DIGEST('e'),
  },
  completion_review_sha256: DIGEST('f'),
  source_ancestry: { source_commit: TAG_COMMIT, evidence_commit: '8'.repeat(40), shipped_commit: PROMOTED_COMMIT, verified: true },
  non_plan_tree_equivalence: {
    source_commit: TAG_COMMIT,
    shipped_commit: PROMOTED_COMMIT,
    excluded_paths: [
      'docs/plans/active/session-relay-prebuilt-cli-distribution.md',
      'docs/plans/finished/2026-07-17-session-relay-prebuilt-cli-distribution.md',
    ],
    verified: true,
  },
  public_repository_id: 'DocksDocks/public',
  public_reviewed_commit: PUBLIC_REVIEWED_COMMIT,
  review_status: 'passed',
  bound_at: '2026-07-17T19:30:00.000Z',
};
validateSourcePreparationProof(proofValue);
const proof = { digest: hash(proofValue), value: proofValue };

const assets = [
  'SHA256SUMS',
  'session-relay-aarch64-apple-darwin',
  'session-relay-aarch64-unknown-linux-musl',
  'session-relay-x86_64-apple-darwin',
  'session-relay-x86_64-unknown-linux-musl',
].map((name, index) => ({ name, database_id: 100 + index, size: 1000 + index, digest: String(index + 1).repeat(64) }));

const publication = {
  digest: DIGEST('b'),
  value: {
    schema: 1,
    type: 'SessionRelayPublicationReceiptV1',
    repository_id: 'DocksDocks/docks',
    version: '0.12.0',
    source_proof_sha256: proof.digest,
    tag: 'session-relay--v0.12.0',
    tag_commit: TAG_COMMIT,
    workflow: {
      file: '.github/workflows/build-binaries.yml',
      workflow_sha: TAG_COMMIT,
      run_id: 81,
      attempt: 1,
      head_sha: TAG_COMMIT,
      path: '.github/workflows/build-binaries.yml',
      event: 'push',
      inputs: { expected_commit: '', expected_tag: '', mode: '' },
      conclusion: 'success',
    },
    release_database_id: 91,
    release_state: 'prerelease',
    body_sha256: hash(PRERELEASE_BODY),
    assets,
    transition: 'tag_and_release_created',
    created_at: '2026-07-17T20:00:00.000Z',
  },
};
const PUBLIC_ASSET_TARGETS = [
  'x86_64-unknown-linux-musl',
  'aarch64-unknown-linux-musl',
  'x86_64-apple-darwin',
  'aarch64-apple-darwin',
];
const PUBLIC_RELEASE_ASSET_NAMES = [
  'SHA256SUMS',
  'docks-kit-darwin-arm64',
  'docks-kit-darwin-x64',
  'docks-kit-linux-arm64',
  'docks-kit-linux-x64',
  'docks-kit-windows-x64.exe',
];
const publicationAssetPins = Object.fromEntries(PUBLIC_ASSET_TARGETS.map((target) => [
  target,
  assets.find(({ name }) => name === `session-relay-${target}`).digest,
]));
const publicRequestValue = {
  schema: 1,
  type: 'PublicReleaseRequestV1',
  repository_id: 'DocksDocks/public',
  tag: 'cli-v0.9.0',
  version: '0.9.0',
  companion_base_commit: PUBLIC_REVIEWED_COMMIT,
  session_relay: {
    repository_id: 'DocksDocks/docks',
    tag: 'session-relay--v0.12.0',
    version: '0.12.0',
    tag_commit: TAG_COMMIT,
    publication_receipt_sha256: publication.digest,
  },
  assets: publicationAssetPins,
  created_at: '2026-07-17T20:30:00.000Z',
};
const publicReleaseValue = {
  schema: 1,
  type: 'PublicReleaseReceiptV1',
  request_sha256: hash(publicRequestValue),
  repository_id: 'DocksDocks/public',
  tag: 'cli-v0.9.0',
  version: '0.9.0',
  release_commit: PUBLIC_RELEASE_COMMIT,
  companion_base_commit: PUBLIC_REVIEWED_COMMIT,
  ancestry_verified: true,
  workflow: {
    file: '.github/workflows/release-cli.yml',
    run_database_id: 801,
    run_attempt: 1,
    conclusion: 'success',
  },
  release: {
    database_id: 901,
    assets: PUBLIC_RELEASE_ASSET_NAMES.map((name, index) => ({
      name,
      size: 2000 + index,
      digest: String(index + 8).slice(-1).repeat(64),
    })),
    checksums_sha256: DIGEST('8'),
  },
  npm: { state: 'published' },
  pinned_assets: publicationAssetPins,
  public_plan: {
    commit: PUBLIC_PLAN_COMMIT,
    path: 'docs/plans/finished/2026-07-18-session-relay-cli-production-release.md',
    completion_receipt_sha256: DIGEST('c'),
  },
  created_at: '2026-07-17T21:00:00.000Z',
};
const publicRelease = { digest: hash(publicReleaseValue), value: publicReleaseValue };


function options(output = '/receipts/promotion.json', extra = {}) {
  return new Map(Object.entries({
    publication: '/receipts/publication.json',
    'publication-sha256': publication.digest,
    'public-release': '/receipts/public-release.json',
    'public-release-sha256': publicRelease.digest,
    'source-proof': '/receipts/proof.json',
    'source-proof-sha256': proof.digest,
    'docks-kit-release': 'cli-v0.9.0',
    'expected-origin-main': OLD_MAIN,
    'receipt-out': output,
    ...extra,
  }));
}

function smoke(kind) {
  return {
    kind,
    isolation_root_sha256: hash({ kind, source: kind === 'exact_source' ? TAG_COMMIT : 'origin/main', sync: ['sync'] }),
    sync_argv: ['sync'],
    stdout_sha256: DIGEST('f'),
    stderr_sha256: DIGEST('0'),
    ordering_log_sha256: DIGEST('1'),
    installed_binary_sha256: HOST_ASSET_DIGEST,
    session_relay_asset_name: 'session-relay-x86_64-unknown-linux-musl',
    installed_version: 'session-relay 0.12.0',
    launcher_sha256: DIGEST('a'),
    launcher_version: 'session-relay 0.12.0',
    docks_kit_target_commit: PUBLIC_RELEASE_COMMIT,
    docks_kit_asset: { name: 'docks-kit-linux-x64', database_id: 501, size: 2000, digest: DIGEST('9') },
    docks_kit_release_database_id: 601,
  };
}

function makeAdapter({ killAfter = null, killMutation = null, rejectMutation = null, outageAfterAppend = null, liveResults = [true], prepushThrows = false, prepushMutator = null, liveThrows = false, liveMutator = null, restoreDrift = false, reapplyDrift = false, ancestry = true, publicAncestry = true, initialMain = OLD_MAIN, adoptionRerunParity = null } = {}) {
  const state = {
    refs: new Map([['refs/heads/main', initialMain]]),
    journal: [],
    outputs: new Map(),
    calls: [],
    counts: { lock: 0, prepush: 0, main: 0, live: 0, restore: 0, reapply: 0, append: 0 },
    killed: false,
    killAfter,
    killMutation,
    rejectMutation,
    outageAfterAppend,
    probeOutage: false,
    liveResults: [...liveResults],
    prepushThrows,
    releaseAssets: structuredClone(assets),
    restoreDrift,
    prepushMutator,
    liveMutator,
    reapplyDrift,
    restoreFailure: false,
    currentBlobs: { '.claude-plugin/marketplace.json': BLOB('3'), 'plugins/session-relay/bin/relay': BLOB('4') },
    restoreFailureMovesMain: false,
    reapplyFailure: false,
    releaseAbsent: false,
    releaseStable: false,
    tagAbsent: false,
  };
  const promotedBlobs = {
    '.claude-plugin/marketplace.json': BLOB('5'),
    'plugins/session-relay/bin/relay': BLOB('6'),
    'plugins/session-relay/new-in-0.12': BLOB('7'),
  };
  const beforeRaw = { ...state.currentBlobs };
  const paths = [...new Set([...Object.keys(beforeRaw), ...Object.keys(promotedBlobs)])].sort();
  const fill = (map) => Object.fromEntries(paths.map((item) => [item, map[item] ?? null]));
  const beforeBlobs = fill(beforeRaw);
  const promotedFilled = fill(promotedBlobs);
  const interrupt = (point) => {
    if (!state.killed && (state.killAfter === point || state.killMutation === point)) {
      state.killed = true;
      const error = new Error(`interrupted after ${point}`);
      error.code = 'SIMULATED_CRASH';
      throw error;
    }
  };
  const adapter = {
    now: () => '2026-07-17T21:00:00.000Z',
    nonce: () => '0123456789abcdef0123456789abcdef',
    loadProof: () => proof,
    loadPublication: () => publication,
    loadPublicRelease: () => publicRelease,
    loadRetryReceipt: (_opts) => {
      const bytes = state.outputs.get('/receipts/failure.json');
      assert.ok(bytes, 'retry input must be a receipt emitted by the state machine');
      const value = JSON.parse(bytes);
      return { value, bytes: Buffer.from(bytes), digest: hash(bytes) };
    },
    remoteRef: (ref) => {
      if (state.probeOutage) {
        state.probeOutage = false;
        throw new SessionRelayReleaseError('simulated next-probe outage', 'failure');
      }
      return state.refs.get(ref) ?? null;
    },
    readJournal: (_ref, tip) => {
      assert.equal(tip, state.refs.get(TRANSACTION_REF));
      return state.journal.map((item) => structuredClone(item));
    },
    isAncestor: (ancestorCommit, descendantCommit) => {
      assert.ok([TAG_COMMIT, PROMOTED_COMMIT].includes(ancestorCommit), 'reviewed promotion commit must be the ancestor');
      assert.equal(descendantCommit, OLD_MAIN, 'expected origin/main must be the descendant');
      return ancestry;
    },
    isPublicAncestor: (ancestorCommit, descendantCommit) => {
      assert.equal(ancestorCommit, PUBLIC_REVIEWED_COMMIT);
      assert.equal(descendantCommit, PUBLIC_RELEASE_COMMIT);
      return publicAncestry;
    },
    validatePrepushRepair: ({ baseCommit, repairCommit }) => {
      assert.equal(baseCommit, OLD_MAIN);
      assert.equal(repairCommit, REPAIR_IMPLEMENTATION_COMMIT);
      return {
        base_commit: OLD_MAIN,
        commit: REPAIR_IMPLEMENTATION_COMMIT,
        paths: [
          'plugins/session-relay/test/release-promotion-contract.mjs',
          'scripts/lib/session-relay-release-cli.mjs',
          'scripts/lib/session-relay-release-promotion.mjs',
        ],
        full_ci_exit: 0,
        full_ci_stdout_sha256: DIGEST('d'),
        full_ci_stderr_sha256: DIGEST('e'),
      };
    },
    createLockCommit: ({ nonce }) => {
      assert.equal(nonce, '0123456789abcdef0123456789abcdef');
      return LOCK_COMMIT;
    },
    appendJournal: ({ ref, entry, prior }) => {
      state.calls.push({ operation: 'append', ref, prior, phase: entry.phase });
      if (state.rejectMutation === 'append') throw new Error('journal CAS rejected');
      assert.equal(ref, TRANSACTION_REF);
      assert.equal(state.refs.get(ref) ?? null, prior, 'journal append must use the authoritative prior tip as its CAS lease');
      const commit = hash({ parent: prior, entry }).slice(0, 40);
      state.journal.push({ commit, parent: prior, entry: structuredClone(entry) });
      state.refs.set(ref, commit);
      state.counts.append += 1;
      if (state.outageAfterAppend === entry.phase) state.probeOutage = true;
      interrupt(entry.phase);
      return commit;
    },
    createLock: ({ ref, commit, prior }) => {
      state.calls.push({ operation: 'lock', ref, commit, prior });
      if (state.rejectMutation === 'lock') throw new Error('lock CAS rejected');
      assert.equal(ref, LOCK_REF);
      assert.equal(prior, null, 'lock creation must carry an absence lease');
      if (state.refs.has(ref)) throw new Error('lock CAS conflict');
      state.refs.set(ref, commit);
      state.counts.lock += 1;
      interrupt('lock_mutation');
    },
    pushMain: ({ commit, expected }) => {
      state.calls.push({ operation: 'main', commit, expected });
      if (state.rejectMutation === 'main') throw new Error('main CAS rejected');
      assert.equal(state.refs.get('refs/heads/main'), expected, 'main update must be leased to the expected head');
      state.refs.set('refs/heads/main', commit);
      state.currentBlobs = { ...promotedBlobs };
      state.counts.main += 1;
      interrupt('main_mutation');
    },
    releaseState: () => ({
      repository_id: 'DocksDocks/docks',
      tag: 'session-relay--v0.12.0',
      commit: state.tagAbsent ? null : TAG_COMMIT,
      release_database_id: state.releaseAbsent ? null : 91,
      prerelease: state.releaseAbsent ? null : !state.releaseStable,
      assets: state.releaseAbsent ? [] : structuredClone(state.releaseAssets),
    }),
    compatibilityState: () => {
      return {
        paths,
        before: structuredClone(beforeBlobs),
        promoted: structuredClone(promotedFilled),
        current: fill(state.currentBlobs),
        unexpected_paths: Object.keys(state.currentBlobs).filter((item) => !paths.includes(item)).sort(),
      };
    },
    validateCommitTransition: ({ kind, commit, parent }) => ({
      valid: kind === 'restore'
        ? commit === RESTORE_COMMIT && [PROMOTED_COMMIT, REAPPLY_COMMIT].includes(parent)
        : commit === REAPPLY_COMMIT && parent === RESTORE_COMMIT,
      live_smoke: kind === 'restore' ? smoke('live') : null,
      recorded_parity: kind === 'restore'
        ? { manifest_catalog: true, full_ci_exit: 0, full_ci_stdout_sha256: DIGEST('7'), full_ci_stderr_sha256: DIGEST('8') }
        : null,
      parity: kind === 'restore'
        ? adoptionRerunParity ?? { manifest_catalog: true, full_ci_exit: 0, full_ci_stdout_sha256: DIGEST('7'), full_ci_stderr_sha256: DIGEST('8') }
        : null,
      verification_ok: true,
    }),
    runPrepushSmoke: ({ docksKitRepository, docksKitRelease, sourceCommit, publicReleaseCommit }) => {
      if (state.prepushThrows) throw new Error('docks-kit identity failed');
      state.calls.push({ operation: 'prepush', docksKitRepository, docksKitRelease, sourceCommit, publicReleaseCommit });
      assert.equal(docksKitRepository, 'DocksDocks/public');
      assert.equal(docksKitRelease, 'cli-v0.9.0');
      assert.equal(sourceCommit, TAG_COMMIT);
      assert.equal(publicReleaseCommit, PUBLIC_RELEASE_COMMIT);
      state.counts.prepush += 1;
      const evidence = smoke('exact_source');
      state.prepushMutator?.(evidence);
      return { ok: true, evidence };
    },
    runLiveSmoke: ({ publicReleaseCommit }) => {
      assert.equal(publicReleaseCommit, PUBLIC_RELEASE_COMMIT);
      if (liveThrows) throw new Error('live launcher scan failed');
      state.counts.live += 1;
      const ok = state.liveResults.length > 1 ? state.liveResults.shift() : state.liveResults[0];
      const evidence = smoke('live');
      state.liveMutator?.(evidence);
      return { ok, definitive: true, evidence, error: ok ? null : 'live sync failed' };
    },
    restoreCompatibility: ({ expected, promoted }) => {
      state.calls.push({ operation: 'restore', expected, promoted });
      if (state.restoreDrift) {
        state.refs.set('refs/heads/main', '9'.repeat(40));
        state.currentBlobs = { ...beforeRaw };
        return { ok: false, definitive: false, error: 'origin/main drifted during restore' };
      }
      if (state.restoreFailure) {
        if (state.restoreFailureMovesMain) {
          state.refs.set('refs/heads/main', '9'.repeat(40));
          state.currentBlobs = { ...beforeRaw };
        }
        return {
          ok: false,
          definitive: true,
          commit: 'a'.repeat(40),
          error: 'restore CI failed',
          parity: { manifest_catalog: false, full_ci_exit: 1, full_ci_stdout_sha256: DIGEST('c'), full_ci_stderr_sha256: DIGEST('d') },
        };
      }
      assert.equal(state.refs.get('refs/heads/main'), promoted);
      state.refs.set('refs/heads/main', RESTORE_COMMIT);
      state.currentBlobs = { ...beforeRaw };
      state.counts.restore += 1;
      interrupt('restore_mutation');
      return {
        ok: true,
        commit: RESTORE_COMMIT,
        blobs: { restored: structuredClone(beforeRaw) },
        parity: { manifest_catalog: true, full_ci_exit: 0, full_ci_stdout_sha256: DIGEST('7'), full_ci_stderr_sha256: DIGEST('8') },
      };
    },
    reapplyCompatibility: ({ expected }) => {
      state.calls.push({ operation: 'reapply', expected });
      assert.equal(state.refs.get('refs/heads/main'), expected);
      if (state.reapplyDrift) {
        state.refs.set('refs/heads/main', '9'.repeat(40));
        state.currentBlobs = { ...beforeRaw };
        return { ok: false, definitive: false, error: 'origin/main drifted during reapply' };
      }
      if (state.reapplyFailure) return { ok: false, definitive: true, error: 'reapply CI failed' };
      state.refs.set('refs/heads/main', REAPPLY_COMMIT);
      state.currentBlobs = { ...promotedBlobs };
      state.counts.reapply += 1;
      interrupt('reapply_mutation');
      return { ok: true, commit: REAPPLY_COMMIT, blobs: { reapplied: structuredClone(promotedBlobs) } };
    },
    assertReceiptOutputAvailable: ({ path }) => {
      if (state.outputs.has(path)) throw new Error(`receipt output already exists: ${path}`);
    },
    writeReceipt: ({ path, receipt, allowExisting }) => {
      const bytes = canonicalize(receipt);
      const existing = state.outputs.get(path);
      if (existing !== undefined) {
        if (!allowExisting || existing !== bytes) throw new Error('receipt output conflict');
      } else {
        state.outputs.set(path, bytes);
      }
      return { bytes: Buffer.from(bytes), digest: hash(bytes), path };
    },
  };
  assert.deepEqual(Object.keys(adapter).sort(), [...PROMOTION_ADAPTER_KEYS].sort());
  return { adapter, state };
}

function run(adapter, output = '/receipts/promotion.json', extra = {}, resume = false) {
  return promoteReviewed(options(output, resume ? { 'transaction-ref': TRANSACTION_REF, ...extra } : extra), resume, adapter);
}

function phases(state) {
  return state.journal.map(({ entry }) => `${entry.attempt}:${entry.sequence}:${entry.phase}`);
}

function expectInterrupted(fn) {
  assert.throws(fn, /interrupted after/);
}

function runGit(cwd, args, expectOk = true) {
  const result = spawnSync('git', args, { cwd, encoding: 'utf8', shell: false });
  if (expectOk) assert.equal(result.status, 0, result.stderr);
  return result;
}

const boundaryFixtureNames = [
  'emit-public-request derivation',
  'verify-public-release success',
  'verify-public-release tag commit mismatch',
  'verify-public-release broken companion ancestry',
  'verify-public-release broken plan ancestry',
  'verify-public-release missing successful workflow run',
  'verify-public-release duplicate successful workflow run',
  'verify-public-release wrong six-asset set',
  'verify-public-release checksum conflict',
  'verify-public-release digest-pin mismatch',
  'verify-public-release completion line hash mismatch',
  'verify-public-release non-passed review_status',
];
const missingBoundaryFixtures = boundaryFixtureNames.filter((name) => (
  name.startsWith('emit-')
    ? typeof releasePromotion.emitPublicRequest !== 'function'
    : typeof releasePromotion.verifyPublicRelease !== 'function'
));
assert.deepEqual(missingBoundaryFixtures, [], 'public release boundary fixture exports are not implemented');

function writeBoundaryValue(directory, name, value) {
  const file = path.join(directory, name);
  const bytes = Buffer.from(canonicalize(value));
  fs.writeFileSync(file, bytes, { mode: 0o600, flag: 'wx' });
  return { file, bytes, digest: hash(bytes), value };
}

function captureDigest(action) {
  const original = process.stdout.write;
  let output = '';
  process.stdout.write = (chunk) => {
    output += Buffer.isBuffer(chunk) ? chunk.toString('utf8') : String(chunk);
    return true;
  };
  try {
    const value = action();
    assert.match(output, /^[0-9a-f]{64}\n$/);
    return value;
  } finally {
    process.stdout.write = original;
  }
}

function publicBoundaryFixture(directory) {
  const publicationFile = writeBoundaryValue(directory, 'publication.json', publication.value);
  const requestOutput = path.join(directory, 'request.json');
  const requestResult = captureDigest(() => releasePromotion.emitPublicRequest(new Map([
    ['publication', publicationFile.file],
    ['publication-sha256', publicationFile.digest],
    ['receipt-out', requestOutput],
  ]), { now: () => '2026-07-17T20:30:00.000Z' }));
  const requestBytes = fs.readFileSync(requestOutput);
  const request = { file: requestOutput, bytes: requestBytes, digest: hash(requestBytes), value: JSON.parse(requestBytes) };
  const expectedRequest = {
    ...publicRequestValue,
    session_relay: {
      ...publicRequestValue.session_relay,
      publication_receipt_sha256: publicationFile.digest,
    },
  };
  assert.deepEqual(request.value, expectedRequest, 'emit-public-request derivation');
  assert.deepEqual(Object.keys(request.value).sort(), [
    'assets', 'companion_base_commit', 'created_at', 'repository_id', 'schema',
    'session_relay', 'tag', 'type', 'version',
  ]);
  assert.deepEqual(Object.keys(request.value.session_relay).sort(), [
    'publication_receipt_sha256', 'repository_id', 'tag', 'tag_commit', 'version',
  ]);
  assert.deepEqual(Object.keys(request.value.assets).sort(), [...PUBLIC_ASSET_TARGETS].sort());
  assert.equal(requestBytes.toString('utf8'), canonicalize(request.value));
  assert.throws(
    () => releasePromotion.emitPublicRequest(new Map([
      ['publication', publicationFile.file],
      ['publication-sha256', publicationFile.digest],
      ['receipt-out', requestOutput],
    ]), { now: () => '2026-07-17T20:30:00.000Z' }),
    /output already exists/i,
    'emit-public-request no-clobber emission',
  );
  assert.equal(requestResult.receipt.type, 'PublicReleaseRequestV1');
  return { publicationFile, request };
}

function makePublicReleaseAdapter(request, {
  tagCommit = PUBLIC_RELEASE_COMMIT,
  releaseAncestry = true,
  planAncestry = true,
  runs,
  removeAsset = null,
  checksumConflict = false,
  pinnedAssets = request.value.assets,
  reviewStatus = 'passed',
  completionReceipt = canonicalize({ schema: 5, phase: 'completion', outcome: 'passed' }),
} = {}) {
  const binaryNames = PUBLIC_RELEASE_ASSET_NAMES.filter((name) => name !== 'SHA256SUMS');
  const binaries = new Map(binaryNames.map((name) => [name, Buffer.from(`public-release:${name}`)]));
  let checksumBytes = Buffer.from(`${binaryNames.map((name) => `${hash(binaries.get(name))}  ${name}`).join('\n')}\n`);
  if (checksumConflict) checksumBytes = Buffer.from(`${DIGEST('f')}  ${binaryNames[0]}\n`);
  const bytesByName = new Map([...binaries, ['SHA256SUMS', checksumBytes]]);
  let releaseAssets = PUBLIC_RELEASE_ASSET_NAMES.map((name, index) => {
    const bytes = bytesByName.get(name);
    return { id: 1000 + index, name, size: bytes.length, digest: `sha256:${hash(bytes)}` };
  });
  if (removeAsset !== null) releaseAssets = releaseAssets.filter(({ name }) => name !== removeAsset);
  const release = {
    id: 901,
    tag_name: 'cli-v0.9.0',
    draft: false,
    prerelease: false,
    assets: releaseAssets,
  };
  const successfulRun = {
    id: 801,
    run_attempt: 1,
    head_sha: PUBLIC_RELEASE_COMMIT,
    head_branch: 'cli-v0.9.0',
    path: '.github/workflows/release-cli.yml',
    event: 'push',
    status: 'completed',
    conclusion: 'success',
  };
  const selectedRuns = runs ?? [successfulRun];
  const finishedPlan = Buffer.from([
    '---',
    'status: finished',
    `review_status: ${reviewStatus}`,
    '---',
    '',
    '# Public release',
    '',
    `Completion-review-receipt: ${completionReceipt}`,
    '',
  ].join('\n'));
  const adapter = {
    now: () => '2026-07-17T21:00:00.000Z',
    getTagCommit: () => tagCommit,
    isAncestor: (ancestor, descendant) => {
      if (ancestor === PUBLIC_REVIEWED_COMMIT && descendant === PUBLIC_RELEASE_COMMIT) return releaseAncestry;
      if (ancestor === PUBLIC_RELEASE_COMMIT && descendant === PUBLIC_PLAN_COMMIT) return planAncestry;
      assert.fail(`unexpected public ancestry check: ${ancestor}...${descendant}`);
    },
    getFinishedPlan: (commit) => {
      assert.equal(commit, PUBLIC_PLAN_COMMIT, 'finished plan must be read from the public plan commit');
      return finishedPlan;
    },
    listWorkflowRuns: () => structuredClone(selectedRuns),
    getRelease: () => structuredClone(release),
    downloadReleaseAsset: (id) => {
      const asset = releaseAssets.find((candidate) => candidate.id === id);
      assert.ok(asset, `unexpected asset download ${id}`);
      return Buffer.from(bytesByName.get(asset.name));
    },
    getPinnedAssets: () => structuredClone(pinnedAssets),
    getNpmState: () => 'published',
  };
  return { adapter, completionDigest: hash(completionReceipt), successfulRun };
}

function verifyPublicBoundary(directory, boundary, adapter, {
  output = 'public-release.json',
  completionDigest = adapter.completionDigest,
} = {}) {
  return captureDigest(() => releasePromotion.verifyPublicRelease(new Map([
    ['request', boundary.request.file],
    ['request-sha256', boundary.request.digest],
    ['publication', boundary.publicationFile.file],
    ['publication-sha256', boundary.publicationFile.digest],
    ['public-finished-plan', 'docs/plans/finished/2026-07-18-session-relay-cli-production-release.md'],
    ['public-release-commit', PUBLIC_RELEASE_COMMIT],
    ['public-plan-commit', PUBLIC_PLAN_COMMIT],
    ['public-completion-sha256', completionDigest],
    ['receipt-out', path.join(directory, output)],
  ]), adapter.adapter ?? adapter));
}

{
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-public-boundary-'));
  try {
    const boundary = publicBoundaryFixture(directory);
    const success = makePublicReleaseAdapter(boundary.request);
    const verified = verifyPublicBoundary(directory, boundary, success);
    assert.equal(verified.receipt.type, 'PublicReleaseReceiptV1', 'verify-public-release success');
    assert.equal(verified.receipt.release_commit, PUBLIC_RELEASE_COMMIT);
    assert.equal(verified.receipt.public_plan.commit, PUBLIC_PLAN_COMMIT);
    assert.equal(verified.receipt.companion_base_commit, PUBLIC_REVIEWED_COMMIT);
    assert.deepEqual(verified.receipt.pinned_assets, boundary.request.value.assets);
    assert.deepEqual(verified.receipt.release.assets.map(({ name }) => name), PUBLIC_RELEASE_ASSET_NAMES);
    assert.equal(fs.readFileSync(path.join(directory, 'public-release.json'), 'utf8'), canonicalize(verified.receipt));

    const cases = [
      ['tag commit mismatch', makePublicReleaseAdapter(boundary.request, { tagCommit: 'e'.repeat(40) }), {}, /tag.*commit|release commit/i],
      ['broken plan ancestry', makePublicReleaseAdapter(boundary.request, { planAncestry: false }), {}, /finished-plan.*ancestry|plan.*ancestry/i],
      ['broken companion ancestry', makePublicReleaseAdapter(boundary.request, { releaseAncestry: false }), {}, /ancestor|ancestry/i],
      ['missing successful workflow run', makePublicReleaseAdapter(boundary.request, { runs: [] }), {}, /workflow.*run|successful/i],
      ['duplicate successful workflow run', (() => {
        const value = makePublicReleaseAdapter(boundary.request);
        return makePublicReleaseAdapter(boundary.request, { runs: [value.successfulRun, { ...value.successfulRun, id: 802 }] });
      })(), {}, /workflow.*run|duplicate|exactly one/i],
      ['wrong six-asset set', makePublicReleaseAdapter(boundary.request, { removeAsset: 'docks-kit-windows-x64.exe' }), {}, /asset set|six|assets/i],
      ['checksum conflict', makePublicReleaseAdapter(boundary.request, { checksumConflict: true }), {}, /checksum|digest/i],
      ['digest-pin mismatch', makePublicReleaseAdapter(boundary.request, { pinnedAssets: { ...boundary.request.value.assets, 'x86_64-unknown-linux-musl': DIGEST('f') } }), {}, /pinned|digest/i],
      ['completion line hash mismatch', makePublicReleaseAdapter(boundary.request), { completionDigest: DIGEST('e') }, /completion.*hash|digest/i],
      ['non-passed review_status', makePublicReleaseAdapter(boundary.request, { reviewStatus: 'failed' }), {}, /review_status|passed/i],
    ];
    for (const [name, adapter, overrides, pattern] of cases) {
      assert.throws(
        () => verifyPublicBoundary(directory, boundary, adapter, { output: `${name.replaceAll(' ', '-')}.json`, ...overrides }),
        pattern,
        `verify-public-release ${name}`,
      );
    }
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

{
  const { adapter, state } = makeAdapter();
  const result = run(adapter);
  assert.equal(result.receipt.outcome, 'success');
  assert.equal(result.receipt.retryable, false);
  const wrongPublicTag = structuredClone(result.receipt);
  wrongPublicTag.public_tag_commit = 'e'.repeat(40);
  assert.throws(() => validatePromotionReceipt(wrongPublicTag), /public release|docks-kit/i);
  const wrongDocksTarget = structuredClone(result.receipt);
  wrongDocksTarget.docks_kit.target_commit = 'e'.repeat(40);
  assert.throws(() => validatePromotionReceipt(wrongDocksTarget), /public release|docks-kit/i);
  const duplicateExcludedPath = structuredClone(result.receipt);
  duplicateExcludedPath.non_plan_tree_equivalence.excluded_paths = ['z', 'z'];
  assert.throws(() => validatePromotionReceipt(duplicateExcludedPath), /tree equivalence/i);
  assert.equal(validatePromotionReceipt(result.receipt), result.receipt);
  assert.throws(() => validatePromotionReceipt({ ...result.receipt, unknown: true }), /schema|keys|unknown|field/i);
  assert.equal(result.receipt.public_repository_id, 'DocksDocks/public');
  assert.equal(result.receipt.public_reviewed_commit, PUBLIC_REVIEWED_COMMIT);
  assert.equal(result.receipt.public_release_commit, PUBLIC_RELEASE_COMMIT);
  assert.equal(result.receipt.public_release_receipt_sha256, publicRelease.digest);
  assert.equal(result.receipt.public_tag_commit, PUBLIC_RELEASE_COMMIT);
  assert.equal(result.receipt.docks_kit.target_commit, PUBLIC_RELEASE_COMMIT);
  const oldShape = structuredClone(result.receipt);
  delete oldShape.public_release_commit;
  delete oldShape.public_release_receipt_sha256;
  assert.throws(() => validatePromotionReceipt(oldShape), /unknown|missing|field|keys/i, 'old promotion receipt shape must be rejected');
  assert.throws(
    () => validatePromotionReceiptForFinalization(oldShape, { proof, publication }, adapter),
    /unknown|missing|field|keys/i,
    'finalization must reject the old promotion receipt shape',
  );
  assert.deepEqual(result.receipt.docks_kit.asset, smoke('exact_source').docks_kit_asset);
  assert.deepEqual(phases(state), ['0:0:initialized', '0:1:locked', '0:2:prepush_passed', '0:3:main_pushed', '0:4:live_passed', '0:5:terminal_success']);
  assert.deepEqual(state.counts, { lock: 1, prepush: 1, main: 1, live: 1, restore: 0, reapply: 0, append: 6 });
  assert.equal(state.calls.find(({ operation }) => operation === 'main').expected, OLD_MAIN);
  assert.equal(state.calls.find(({ operation }) => operation === 'lock').prior, null);
  assert.equal(validatePromotionReceiptForFinalization(result.receipt, { proof, publication }, adapter), result.receipt);
  assert.throws(
    () => validatePromotionReceiptForFinalization(result.receipt, { proof, publication }, { ...adapter, isAncestor: () => false }),
    /lineage|ancestor/i,
  );
  assert.throws(
    () => validatePromotionReceiptForFinalization({ ...result.receipt, created_at: '2026-07-17T21:00:00.001Z' }, { proof, publication }, adapter),
    /reconstruct|journal/i,
  );
  state.releaseStable = true;
  assert.equal(validatePromotionReceiptForFinalization(result.receipt, { proof, publication }, adapter), result.receipt, 'stable finalization resume must retain authoritative receipt validation');
  state.releaseStable = false;
  assert.ok(state.calls.filter(({ operation }) => operation === 'append').every((call, index, calls) => index === 0 ? call.prior === null : call.prior !== calls[index - 1].prior));
}

for (const phase of ['initialized', 'locked', 'prepush_passed', 'main_pushed', 'live_passed', 'terminal_success']) {
  const { adapter, state } = makeAdapter({ killAfter: phase });
  expectInterrupted(() => run(adapter));
  const before = { ...state.counts };
  const result = run(adapter, `/receipts/resume-${phase}.json`, {}, true);
  assert.equal(result.receipt.outcome, 'success', `resume from ${phase}`);
  assert.equal(state.counts.lock, 1, `lock not replayed from ${phase}`);
  assert.equal(state.counts.prepush, 1, `prepush not replayed from ${phase}`);
  assert.equal(state.counts.main, 1, `main not replayed from ${phase}`);
  assert.equal(state.counts.live, 1, `live not replayed from ${phase}`);
  if (phase === 'terminal_success') assert.equal(state.counts.append, before.append, 'terminal recovery must not append');
}

{
  const { adapter, state } = makeAdapter({ outageAfterAppend: 'initialized' });
  assert.throws(() => run(adapter, '/receipts/probe-outage.json'), (error) => error.outcome === 'failure' && /probe outage/.test(error.message));
  assert.equal(state.counts.append, 1, 'accepted journal mutation must remain durable across the next-probe outage');
  assert.equal(run(adapter, '/receipts/probe-outage-resume.json', {}, true).receipt.outcome, 'success');
  assert.equal(state.counts.append, 6, 'resume must not duplicate the accepted initialized mutation');
}

{
  const { adapter, state } = makeAdapter({ killMutation: 'main_mutation' });
  expectInterrupted(() => run(adapter));
  assert.deepEqual(phases(state).slice(-1), ['0:2:prepush_passed']);
  run(adapter, '/receipts/resumed-main.json', {}, true);
  assert.equal(state.counts.main, 1, 'reconciliation must not replay a completed main push');
}

{
  const legacyRetryArgv = ['sync', '--release-test-source', TAG_COMMIT];
  const { adapter, state } = makeAdapter({
    liveResults: [false, true],
    prepushMutator: (evidence) => { evidence.sync_argv = legacyRetryArgv; },
  });
  const failure = run(adapter, '/receipts/failure.json');
  assert.equal(failure.receipt.outcome, 'restored_failure');
  assert.equal(failure.receipt.retryable, true);
  assert.deepEqual(failure.receipt.exact_source_smoke.sync_argv, legacyRetryArgv);
  assert.equal(state.refs.get('refs/heads/main'), RESTORE_COMMIT);
  assert.deepEqual(phases(state).slice(-2), ['0:4:restore_pushed', '0:5:terminal_failure']);
  const retried = run(adapter, '/receipts/retry.json', {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': failure.state.receipt_sha256,
  });
  assert.equal(retried.receipt.outcome, 'success');
  assert.equal(retried.receipt.attempt, 1);
  assert.deepEqual(retried.receipt.exact_source_smoke.sync_argv, legacyRetryArgv, 'restored retry preserves historical exact-source evidence');
  assert.deepEqual(phases(state).slice(-4), ['1:0:initialized', '1:1:locked', '1:2:reapply_pushed', '1:3:terminal_success']);
  assert.deepEqual({ prepush: state.counts.prepush, main: state.counts.main, restore: state.counts.restore, reapply: state.counts.reapply }, { prepush: 1, main: 1, restore: 1, reapply: 1 });
  const legacyChain = adapter.readJournal(TRANSACTION_REF, state.refs.get(TRANSACTION_REF));
  for (const item of legacyChain) delete item.entry.immutable.prepush_repair;
  const attemptOneIndex = legacyChain.findIndex(({ entry }) => entry.attempt === 1);
  const legacyPrefix = legacyChain.slice(0, attemptOneIndex);
  const legacyPriorReceipt = structuredClone(failure.receipt);
  legacyPriorReceipt.journal_chain_sha256 = hash(canonicalize(legacyPrefix));
  const legacyPriorDigest = hash(canonicalize(legacyPriorReceipt));
  for (const item of legacyChain.slice(attemptOneIndex)) item.entry.immutable.prior_attempt_receipt_sha256 = legacyPriorDigest;
  legacyChain.at(-1).entry.receipt_projection.prior_attempt_receipt_sha256 = legacyPriorDigest;
  assert.equal(
    validatePromotionJournal(legacyChain, legacyChain.at(-1).commit).tip.entry.phase,
    'terminal_success',
    'old-format restored retry journals must keep the reapply transition map',
  );
  assert.throws(() => run(adapter, '/receipts/retry-again.json', {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': failure.state.receipt_sha256,
  }), /retry|terminal|attempt/i);
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'main_pushed' });
  expectInterrupted(() => run(adapter));
  state.refs.delete('refs/heads/main');
  const incident = run(adapter, '/receipts/deleted-main-main-pushed.json', {}, true);
  assert.equal(incident.receipt.outcome, 'manual_incident');
  assert.equal(incident.receipt.observed_origin_main, null);
}

for (const phase of ['locked', 'reapply_pushed']) {
  const { adapter, state } = makeAdapter({ liveResults: [false] });
  const failure = run(adapter, '/receipts/failure.json');
  state.killAfter = phase;
  state.killed = false;
  expectInterrupted(() => run(adapter, `/receipts/retry-delete-${phase}.json`, {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': failure.state.receipt_sha256,
  }));
  state.refs.delete('refs/heads/main');
  const incident = run(adapter, `/receipts/retry-delete-${phase}-resume.json`, {}, true);
  assert.equal(incident.receipt.outcome, 'manual_incident', `deleted main recovery from retry ${phase}`);
  assert.equal(incident.receipt.observed_origin_main, null);
}

{
  const { adapter, state } = makeAdapter({ liveResults: [false], killMutation: 'restore_mutation' });
  expectInterrupted(() => run(adapter, '/receipts/failure.json'));
  run(adapter, '/receipts/recovered-failure.json', {}, true);
  assert.equal(state.counts.restore, 1, 'restore reconciliation must not duplicate the restore push');
  assert.equal(JSON.parse(state.outputs.get('/receipts/recovered-failure.json')).outcome, 'restored_failure');
}

{
  const rerunParity = {
    manifest_catalog: true,
    full_ci_exit: 0,
    full_ci_stdout_sha256: DIGEST('c'),
    full_ci_stderr_sha256: DIGEST('d'),
  };
  const { adapter } = makeAdapter({ liveResults: [false], killMutation: 'restore_mutation', adoptionRerunParity: rerunParity });
  expectInterrupted(() => run(adapter, '/receipts/nondeterministic-ci.json'));
  const recovered = run(adapter, '/receipts/nondeterministic-ci-recovered.json', {}, true);
  assert.deepEqual(recovered.receipt.post_restore, {
    manifest_catalog: true,
    full_ci_exit: 0,
    full_ci_stdout_sha256: DIGEST('7'),
    full_ci_stderr_sha256: DIGEST('8'),
  }, 'receipt must preserve commit-bound restore evidence rather than nondeterministic adoption-run hashes');
}

{
  const first = makeAdapter({ killAfter: 'terminal_success' });
  expectInterrupted(() => run(first.adapter, '/receipts/recovered.json'));
  const recovered = run(first.adapter, '/receipts/recovered.json', {}, true);
  const exact = first.state.outputs.get('/receipts/recovered.json');
  const before = { ...first.state.counts };
  const idempotent = run(first.adapter, '/receipts/recovered.json', {}, true);
  assert.equal(canonicalize(recovered.receipt), canonicalize(idempotent.receipt));
  assert.equal(first.state.outputs.get('/receipts/recovered.json'), exact);
  assert.deepEqual(first.state.counts, before, 'terminal idempotence must perform no mutation or smoke');
  first.state.outputs.set('/receipts/conflict.json', '{}');
  assert.throws(() => run(first.adapter, '/receipts/conflict.json', {}, true), /receipt output conflict/);
  assert.deepEqual(first.state.counts, before, 'conflicting terminal output must fail before mutation');
}

{
  const one = makeAdapter();
  const two = makeAdapter();
  run(one.adapter, '/receipts/one.json');
  run(two.adapter, '/receipts/two.json');
  assert.equal(one.state.outputs.get('/receipts/one.json'), two.state.outputs.get('/receipts/two.json'), 'terminal receipt bytes must be deterministic');
}

{
  const { adapter, state } = makeAdapter({ killMutation: 'lock_mutation' });
  expectInterrupted(() => run(adapter));
  const result = run(adapter, '/receipts/resumed-lock.json', {}, true);
  assert.equal(result.receipt.outcome, 'success');
  assert.equal(state.counts.lock, 1, 'resume must not repeat completed lock creation');
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'main_pushed' });
  expectInterrupted(() => run(adapter));
  state.refs.set('refs/heads/main', '9'.repeat(40));
  state.currentBlobs = structuredClone(state.journal[0].entry.state.compatibility.before);
  const result = run(adapter, '/receipts/arbitrary-restore-head.json', {}, true);
  assert.equal(result.receipt.outcome, 'manual_incident', 'same blobs cannot authenticate an arbitrary restore head');
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'locked' });
  expectInterrupted(() => run(adapter));
  state.refs.set('refs/heads/main', PROMOTED_COMMIT);
  state.currentBlobs = structuredClone(state.journal[0].entry.state.compatibility.promoted);
  const result = run(adapter, '/receipts/locked-promoted.json', {}, true);
  assert.equal(result.receipt.outcome, 'success', 'resume may rerun and journal prepush evidence before deriving main_pushed');
  assert.equal(state.counts.prepush, 1);
  assert.equal(state.counts.main, 0, 'already completed main push is not repeated');
}

{
  const { adapter, state } = makeAdapter({ rejectMutation: 'main' });
  const result = run(adapter, '/receipts/main-cas-rejected.json');
  assert.equal(result.receipt.outcome, 'failure');
  assert.equal(state.refs.get('refs/heads/main'), OLD_MAIN);
  assert.equal(phases(state).at(-1), '0:3:terminal_failure');
}

{
  const { adapter, state } = makeAdapter({ liveResults: [false, true] });
  const failure = run(adapter, '/receipts/failure.json');
  state.killMutation = 'reapply_mutation';
  expectInterrupted(() => run(adapter, '/receipts/retry-killed.json', {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': failure.state.receipt_sha256,
  }));
  const result = run(adapter, '/receipts/retry-recovered.json', {}, true);
  assert.equal(result.receipt.outcome, 'success');
  assert.equal(state.counts.reapply, 1, 'resume must not repeat a completed reapply');
}

{
  const { adapter, state } = makeAdapter({ liveResults: [false, true] });
  const failure = run(adapter, '/receipts/failure.json');
  state.killAfter = 'locked';
  expectInterrupted(() => run(adapter, '/receipts/retry-killed.json', {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': failure.state.receipt_sha256,
  }));
  state.refs.set('refs/heads/main', '9'.repeat(40));
  state.currentBlobs = structuredClone(state.journal[0].entry.state.compatibility.promoted);
  const result = run(adapter, '/receipts/arbitrary-reapply-head.json', {}, true);
  assert.equal(result.receipt.outcome, 'manual_incident', 'same blobs cannot authenticate an arbitrary reapply head');
}

{
  const { adapter, state } = makeAdapter({ liveResults: [false, true] });
  const failure = run(adapter, '/receipts/failure.json');
  state.killAfter = 'terminal_success';
  expectInterrupted(() => run(adapter, '/receipts/retry-killed.json', {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': failure.state.receipt_sha256,
  }));
  const before = { ...state.counts };
  const recovered = run(adapter, '/receipts/retry-terminal.json', {}, true);
  assert.equal(recovered.receipt.outcome, 'success');
  assert.equal(recovered.receipt.public_reviewed_commit, PUBLIC_REVIEWED_COMMIT);
  run(adapter, '/receipts/retry-terminal.json', {}, true);
  assert.deepEqual(state.counts, before, 'retry terminal recovery is mutation-free and idempotent');
}

{
  const { adapter, state } = makeAdapter({ liveResults: [false, false] });
  const failure = run(adapter, '/receipts/failure.json');
  state.killAfter = 'terminal_failure';
  expectInterrupted(() => run(adapter, '/receipts/retry-killed.json', {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': failure.state.receipt_sha256,
  }));
  const recovered = run(adapter, '/receipts/retry-restored-terminal.json', {}, true);
  assert.equal(recovered.receipt.outcome, 'restored_failure');
  assert.equal(recovered.receipt.retryable, false);
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'initialized' });
  expectInterrupted(() => run(adapter));
  state.outputs.set('/receipts/nonterminal-existing.json', canonicalize({ occupied: true }));
  const before = { ...state.counts };
  assert.throws(() => run(adapter, '/receipts/nonterminal-existing.json', {}, true), /output already exists/i);
  assert.deepEqual(state.counts, before, 'nonterminal resume must reject an existing receipt before mutation');
}

for (const releaseAbsent of [false, true]) {
  const { adapter, state } = makeAdapter({ killAfter: 'initialized' });
  expectInterrupted(() => run(adapter));
  state.tagAbsent = true;
  state.releaseAbsent = releaseAbsent;
  const output = `/receipts/tag-deleted-${releaseAbsent ? 'release-absent' : 'release-present'}.json`;
  const incident = run(adapter, output, {}, true);
  assert.equal(incident.receipt.outcome, 'manual_incident');
  assert.equal(state.journal.at(-1).entry.state.observed_release.commit, null);
  assert.equal(run(adapter, output, {}, true).receipt.outcome, 'manual_incident');
  state.tagAbsent = false;
  assert.throws(() => run(adapter, `/receipts/tag-recreated-${releaseAbsent}.json`, {}, true), /snapshot.*changed|authority/i);
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'initialized' });
  expectInterrupted(() => run(adapter));
  state.refs.set(LOCK_REF, '9'.repeat(40));
  const incident = run(adapter, '/receipts/contended.json', {}, true);
  assert.equal(incident.receipt.outcome, 'manual_incident');
  assert.equal(run(adapter, '/receipts/contended.json', {}, true).receipt.outcome, 'manual_incident');
  const corruptObservedRelease = structuredClone(state.journal);
  corruptObservedRelease.at(-1).entry.state.observed_release.release_database_id = 0;
  assert.throws(() => validatePromotionJournal(corruptObservedRelease, state.refs.get(TRANSACTION_REF)), /observed Release state/i);
  state.refs.set(LOCK_REF, '8'.repeat(40));
  assert.throws(() => run(adapter, '/receipts/contended-drift.json', {}, true), /snapshot.*changed|authority/i);
  assert.equal(phases(state).at(-1), '0:1:manual_incident', 'lock contention must be journaled');
  assert.equal(state.counts.prepush, 0);
  assert.equal(state.counts.main, 0);

}

{
  const { adapter, state } = makeAdapter({ killAfter: 'main_pushed' });
  expectInterrupted(() => run(adapter));
  state.releaseAbsent = true;
  const incident = run(adapter, '/receipts/release-deleted.json', {}, true);
  assert.equal(incident.receipt.outcome, 'manual_incident');
  assert.equal(state.journal.at(-1).entry.state.observed_release.release_database_id, null);
  assert.equal(run(adapter, '/receipts/release-deleted.json', {}, true).receipt.outcome, 'manual_incident');
  state.releaseAbsent = false;
  assert.throws(() => run(adapter, '/receipts/release-recreated.json', {}, true), /snapshot.*changed|authority/i);
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'initialized' });
  expectInterrupted(() => run(adapter));
  state.releaseAssets[0].digest = null;
  const incident = run(adapter, '/receipts/release-null-digest.json', {}, true);
  assert.equal(incident.receipt.outcome, 'manual_incident');
  assert.equal(state.journal.at(-1).entry.state.observed_release.assets[0].digest, null);
  assert.equal(run(adapter, '/receipts/release-null-digest.json', {}, true).receipt.outcome, 'manual_incident');
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'initialized' });
  expectInterrupted(() => run(adapter));
  state.releaseAssets[0].digest = DIGEST('e');
  const incident = run(adapter, '/receipts/release-conflict.json', {}, true);
  assert.equal(incident.receipt.outcome, 'manual_incident');
  assert.equal(run(adapter, '/receipts/release-conflict.json', {}, true).receipt.outcome, 'manual_incident');
  state.releaseAssets[0].digest = DIGEST('f');
  assert.throws(() => run(adapter, '/receipts/release-conflict-drift.json', {}, true), /snapshot.*changed|authority/i);

}

{

  const { adapter, state } = makeAdapter({ killAfter: 'prepush_passed' });
  expectInterrupted(() => run(adapter));
  state.refs.set('refs/heads/main', '9'.repeat(40));
  const incident = run(adapter, '/receipts/incident.json', {}, true);
  assert.equal(incident.receipt.outcome, 'manual_incident');
  assert.equal(state.counts.main, 0);
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'prepush_passed' });
  expectInterrupted(() => run(adapter));
  state.refs.delete('refs/heads/main');
  const incident = run(adapter, '/receipts/main-deleted.json', {}, true);
  assert.equal(incident.receipt.outcome, 'manual_incident');
  assert.equal(incident.receipt.observed_origin_main, null);
  assert.equal(run(adapter, '/receipts/main-deleted.json', {}, true).receipt.observed_origin_main, null);
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'locked' });
  expectInterrupted(() => run(adapter));
  state.prepushMutator = (evidence) => { evidence.unknown = true; };
  const before = { append: state.counts.append, tip: state.refs.get(TRANSACTION_REF) };
  assert.throws(() => run(adapter, '/receipts/invalid-generated-state.json', {}, true), /smoke|unknown|schema/i);
  assert.deepEqual({ append: state.counts.append, tip: state.refs.get(TRANSACTION_REF) }, before, 'invalid generated journal entry must be rejected before remote append');
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'locked' });
  expectInterrupted(() => run(adapter));
  const valid = adapter.readJournal(TRANSACTION_REF, state.refs.get(TRANSACTION_REF));
  assert.equal(validatePromotionJournal(valid, state.refs.get(TRANSACTION_REF)).tip.entry.phase, 'locked');
  const corruptions = [
    (chain) => { chain[0].entry.unknown = true; },
    (chain) => { chain[1].entry.sequence = 8; },
    (chain) => { chain[1].entry.immutable.tag_commit = '8'.repeat(40); },
    (chain) => { chain[1].entry.phase = 'main_pushed'; },
    (chain) => { chain[1].parent = '8'.repeat(40); },
  ];
  for (const corrupt of corruptions) {
    const chain = structuredClone(valid);
    corrupt(chain);
    assert.throws(() => validatePromotionJournal(chain, state.refs.get(TRANSACTION_REF)), /journal|schema|sequence|immutable|transition|parent/i);
  }
  assert.throws(() => validatePromotionJournal(valid, '8'.repeat(40)), /authoritative.*tip/i);
}

{
  const { adapter, state } = makeAdapter();
  run(adapter, '/receipts/schema.json');
  const valid = adapter.readJournal(TRANSACTION_REF, state.refs.get(TRANSACTION_REF));
  const corruptions = [
    (projection) => { projection.source_ancestry.extra = true; },
    (projection) => { projection.non_plan_tree_equivalence = {}; },
    (projection) => { projection.compatibility_blobs.before['plugins/session-relay/bin/relay'] = DIGEST('9'); },
    (projection) => { projection.docks_kit.extra = true; },
    (projection) => { projection.session_relay_assets[0].extra = true; },
    (projection) => { projection.exact_source_smoke = {}; },
    (projection) => { projection.live_smoke = {}; },
    (projection) => { projection.created_at = '1'; },
    (projection) => { projection.outcome = 'restored_failure'; projection.retryable = true; },
    (projection) => { projection.public_reviewed_commit = null; },
  ];
  for (const corrupt of corruptions) {
    const chain = structuredClone(valid);
    corrupt(chain.at(-1).entry.receipt_projection);


    assert.throws(
      () => validatePromotionJournal(chain, state.refs.get(TRANSACTION_REF)),
      /receipt|projection|schema|unknown|missing|identity|timestamp|outcome|blob|smoke/i,
    );
  }
}
{
  const { adapter, state } = makeAdapter({ ancestry: false });
  assert.throws(() => run(adapter, '/receipts/divergent-main.json'), /ancestor|lineage/i);
  assert.deepEqual(state.counts, { lock: 0, prepush: 0, main: 0, live: 0, restore: 0, reapply: 0, append: 0 });
}

{
  const { adapter, state } = makeAdapter({ publicAncestry: false });
  assert.throws(() => run(adapter, '/receipts/divergent-public-release.json'), /public.*ancestor|companion.*ancestry|lineage/i);
  assert.deepEqual(state.counts, { lock: 0, prepush: 0, main: 0, live: 0, restore: 0, reapply: 0, append: 0 });
}

{
  const legacyArgv = ['sync', '--release-test-source', TAG_COMMIT];
  const { adapter, state } = makeAdapter({
    prepushMutator: (evidence) => {
      evidence.sync_argv = legacyArgv;
      evidence.installed_binary_sha256 = hash(Buffer.alloc(0));
      evidence.launcher_sha256 = hash(Buffer.alloc(0));
      evidence.installed_version = '';
      evidence.launcher_version = '';
    },
  });
  const successfulPrepush = adapter.runPrepushSmoke;
  adapter.runPrepushSmoke = (args) => ({
    ...successfulPrepush(args),
    ok: false,
    error: 'Unknown sync target(s): --release-test-source, /tmp/reviewed-docks',
  });
  const result = run(adapter, '/receipts/failure.json');
  const priorBytes = state.outputs.get('/receipts/failure.json');
  assert.equal(result.receipt.outcome, 'failure');
  assert.equal(result.receipt.retryable, false);
  assert.deepEqual(result.receipt.exact_source_smoke.sync_argv, legacyArgv, 'historical failed evidence retains the rejected sync argv');
  assert.equal(state.counts.main, 0);
  assert.equal(state.journal.at(-1).entry.phase, 'terminal_failure');
  assert.equal(run(adapter, '/receipts/prepush-exception-resume.json', {}, true).receipt.outcome, 'failure');
  assert.throws(() => run(adapter, '/receipts/generic-retry.json', {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': result.state.receipt_sha256,
  }), /restored.failure|repair.prepush/i);

  const appendCountBeforeAuthorityFailure = state.counts.append;
  state.releaseStable = true;
  assert.throws(() => run(adapter, '/receipts/stable-release-repair.json', {
    'repair-prepush': true,
    'repair-implementation-commit': REPAIR_IMPLEMENTATION_COMMIT,
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': result.state.receipt_sha256,
  }), /release|prerelease/i);
  assert.equal(state.counts.append, appendCountBeforeAuthorityFailure, 'changed prerelease authority must be rejected before attempt 1');
  state.releaseStable = false;
  const validateRepair = adapter.validatePrepushRepair;
  adapter.validatePrepushRepair = (input) => {
    const evidence = validateRepair(input);
    state.releaseStable = true;
    return evidence;
  };
  assert.throws(() => run(adapter, '/receipts/raced-release-repair.json', {
    'repair-prepush': true,
    'repair-implementation-commit': REPAIR_IMPLEMENTATION_COMMIT,
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': result.state.receipt_sha256,
  }), /release|prerelease/i);
  assert.equal(state.counts.append, appendCountBeforeAuthorityFailure, 'authority drift during repair CI must be rejected before attempt 1');
  state.releaseStable = false;
  adapter.validatePrepushRepair = validateRepair;
  adapter.runPrepushSmoke = successfulPrepush;
  state.prepushMutator = null;
  const repaired = run(adapter, '/receipts/repaired.json', {
    'repair-prepush': true,
    'repair-implementation-commit': REPAIR_IMPLEMENTATION_COMMIT,
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': result.state.receipt_sha256,
  });
  assert.equal(repaired.receipt.outcome, 'success');
  assert.equal(repaired.receipt.attempt, 1);
  assert.equal(repaired.receipt.retryable, false);
  assert.deepEqual(repaired.receipt.exact_source_smoke.sync_argv, ['sync'], 'successful repair evidence uses the URL-rewrite sync binding');
  assert.equal(state.refs.get('refs/heads/main'), PROMOTED_COMMIT);
  assert.equal(state.outputs.get('/receipts/failure.json'), priorBytes, 'repair continuation must not rewrite failed evidence');
  assert.deepEqual(phases(state).slice(-6), [
    '1:0:initialized',
    '1:1:locked',
    '1:2:prepush_passed',
    '1:3:main_pushed',
    '1:4:live_passed',
    '1:5:terminal_success',
  ]);
  const repair = state.journal.find(({ entry }) => entry.attempt === 1).entry.immutable.prepush_repair;
  assert.equal(repair.commit, REPAIR_IMPLEMENTATION_COMMIT);
  const forgedRepairSuccess = adapter.readJournal(TRANSACTION_REF, state.refs.get(TRANSACTION_REF));
  forgedRepairSuccess.at(-1).entry.receipt_projection.exact_source_smoke.sync_argv = legacyArgv;
  forgedRepairSuccess.at(-1).entry.state.prepush_smoke.sync_argv = legacyArgv;
  assert.throws(
    () => validatePromotionJournal(forgedRepairSuccess, forgedRepairSuccess.at(-1).commit),
    /terminal repair success.*URL-rewrite/i,
  );
  const invalidBase = adapter.readJournal(TRANSACTION_REF, state.refs.get(TRANSACTION_REF));
  for (const item of invalidBase.filter(({ entry }) => entry.attempt === 1)) item.entry.immutable.prepush_repair.base_commit = 'f'.repeat(40);
  assert.throws(
    () => validatePromotionJournal(invalidBase, invalidBase.at(-1).commit),
    /repair base.*expected origin\/main/i,
  );
  const invalidProjection = adapter.readJournal(TRANSACTION_REF, state.refs.get(TRANSACTION_REF));
  invalidProjection.at(-1).entry.receipt_projection.prior_attempt_receipt_sha256 = null;
  assert.throws(
    () => validatePromotionJournal(invalidProjection, invalidProjection.at(-1).commit),
    /projection is not derived/i,
  );
  const invalidEligibility = adapter.readJournal(TRANSACTION_REF, state.refs.get(TRANSACTION_REF));
  invalidEligibility.find(({ entry }) => entry.attempt === 0 && entry.phase === 'terminal_failure').entry.state.failure = 'unrelated pre-push failure';
  assert.throws(
    () => validatePromotionJournal(invalidEligibility, invalidEligibility.at(-1).commit),
    /repair authorization/i,
  );
  const invalidAttemptZero = adapter.readJournal(TRANSACTION_REF, state.refs.get(TRANSACTION_REF));
  invalidAttemptZero[0].entry.immutable.prepush_repair = structuredClone(repair);
  assert.throws(
    () => validatePromotionJournal(invalidAttemptZero, invalidAttemptZero.at(-1).commit),
    /attempt 0 cannot bind pre-push repair/i,
  );
}

{
  const legacyArgv = ['sync', '--release-test-source', TAG_COMMIT];
  const failedPrepush = makeAdapter({
    prepushMutator: (evidence) => {
      evidence.sync_argv = legacyArgv;
      evidence.installed_binary_sha256 = hash(Buffer.alloc(0));
      evidence.launcher_sha256 = hash(Buffer.alloc(0));
      evidence.installed_version = '';
      evidence.launcher_version = '';
    },
  });
  const successfulPrepush = failedPrepush.adapter.runPrepushSmoke;
  failedPrepush.adapter.runPrepushSmoke = (args) => ({
    ...successfulPrepush(args),
    ok: false,
    error: 'Unknown sync target(s): --release-test-source, /tmp/reviewed-docks',
  });
  const failed = run(failedPrepush.adapter, '/receipts/failure.json');
  failedPrepush.adapter.validatePrepushRepair = ({ baseCommit, repairCommit }) => ({
    base_commit: baseCommit,
    commit: repairCommit,
    paths: ['scripts/lib/session-relay-release-promotion.mjs'],
    full_ci_exit: 0,
    full_ci_stdout_sha256: DIGEST('d'),
    full_ci_stderr_sha256: DIGEST('e'),
  });
  assert.throws(() => run(failedPrepush.adapter, '/receipts/invalid-repair.json', {
    'repair-prepush': true,
    'repair-implementation-commit': REPAIR_IMPLEMENTATION_COMMIT,
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': failed.state.receipt_sha256,
  }), /repair.*paths/i);
  assert.equal(failedPrepush.state.journal.at(-1).entry.attempt, 0, 'invalid repair must not append attempt 1');
}

{
  const { adapter, state } = makeAdapter({ liveResults: [false] });
  const restoredFailure = run(adapter, '/receipts/failure.json');
  state.reapplyFailure = true;
  const terminal = run(adapter, '/receipts/reapply-definitive-failure.json', {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': restoredFailure.state.receipt_sha256,
  });
  assert.equal(terminal.receipt.outcome, 'failure');
  assert.equal(terminal.receipt.restore_commit, null);
  assert.equal(terminal.receipt.compatibility_blobs.restored, null);
  assert.equal(terminal.receipt.post_restore, null);
}

{
  const { adapter, state } = makeAdapter({ liveThrows: true });
  const result = run(adapter, '/receipts/live-exception.json');

  assert.equal(result.receipt.outcome, 'restored_failure');
  assert.equal(state.counts.restore, 1, 'live smoke exceptions must enter the restore path');
}

{
  const attempt0 = makeAdapter({ liveResults: [false] });
  attempt0.state.restoreFailure = true;
  attempt0.state.restoreFailureMovesMain = true;
  assert.equal(run(attempt0.adapter, '/receipts/restore-ci-race.json').receipt.outcome, 'manual_incident');

  const retry = makeAdapter({ liveResults: [false, false] });
  const failure = run(retry.adapter, '/receipts/failure.json');
  retry.state.restoreFailure = true;
  retry.state.restoreFailureMovesMain = true;
  const incident = run(retry.adapter, '/receipts/retry-restore-ci-race.json', {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': failure.state.receipt_sha256,
  });
  assert.equal(incident.receipt.outcome, 'manual_incident');
}

{
  const { adapter } = makeAdapter();
  const receipt = run(adapter, '/receipts/smoke-binding.json').receipt;
  const wrongDocksKit = structuredClone(receipt);

  wrongDocksKit.live_smoke.docks_kit_asset.digest = DIGEST('e');
  assert.throws(() => validatePromotionReceipt(wrongDocksKit), /live.*docks-kit|smoke binding/i);
  const wrongLauncher = structuredClone(receipt);
  wrongLauncher.live_smoke.launcher_sha256 = DIGEST('e');
  assert.throws(() => validatePromotionReceipt(wrongLauncher), /launcher.*identity|smoke binding/i);
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'terminal_success' });
  expectInterrupted(() => run(adapter, '/receipts/release-assets.json'));
  state.releaseAssets[0].digest = DIGEST('e');
  assert.throws(() => run(adapter, '/receipts/release-assets.json', {}, true), /release.*asset|asset.*conflict/i);
}

{
  const { adapter, state } = makeAdapter({ liveResults: [false] });
  const failure = run(adapter, '/receipts/failure.json');
  run(adapter, '/receipts/retry-auth.json', {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': failure.state.receipt_sha256,
  });
  const chain = adapter.readJournal(TRANSACTION_REF, state.refs.get(TRANSACTION_REF));
  const prior = chain.find(({ entry }) => entry.attempt === 0 && entry.phase === 'terminal_failure');
  prior.entry.state.restore_commit = null;
  prior.entry.state.compatibility.restored = null;
  prior.entry.state.post_restore = null;
  prior.entry.receipt_projection.restore_commit = null;
  prior.entry.receipt_projection.compatibility_blobs.restored = null;
  prior.entry.receipt_projection.post_restore = null;
  prior.entry.receipt_projection.outcome = 'failure';
  prior.entry.receipt_projection.retryable = false;
  assert.throws(() => validatePromotionJournal(chain, state.refs.get(TRANSACTION_REF)), /retry.*restored|retryable|authorization/i);
}

{
  const { adapter } = makeAdapter({ liveResults: [false], restoreDrift: true });
  const incident = run(adapter, '/receipts/restore-drift.json');
  assert.equal(incident.receipt.outcome, 'manual_incident');
  assert.equal(incident.receipt.observed_origin_main, '9'.repeat(40));
}

{
  const { adapter, state } = makeAdapter({ liveResults: [false, true] });
  const failure = run(adapter, '/receipts/failure.json');
  state.reapplyDrift = true;
  const incident = run(adapter, '/receipts/reapply-drift.json', {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': failure.state.receipt_sha256,
  });
  assert.equal(incident.receipt.outcome, 'manual_incident');
  assert.equal(incident.receipt.observed_origin_main, '9'.repeat(40));
}

{
  const { adapter, state } = makeAdapter({ liveResults: [false, false] });
  const failure = run(adapter, '/receipts/failure.json');
  state.restoreFailure = true;
  const terminal = run(adapter, '/receipts/retry-restore-ci-failure.json', {
    'retry-failed': '/receipts/failure.json',
    'retry-failed-sha256': failure.state.receipt_sha256,
  });
  assert.equal(terminal.receipt.outcome, 'failure');
  assert.equal(terminal.receipt.retryable, false);
  assert.equal(terminal.receipt.observed_origin_main, REAPPLY_COMMIT);
}


{
  const { adapter } = makeAdapter();
  assert.throws(() => promoteReviewed(options(), false, { ...adapter, extra: () => {} }), /adapter.*unknown/i);
}

{
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-resume-cli-'));
  try {
    const proofPath = path.join(directory, 'proof.json');
    const publicationPath = path.join(directory, 'publication.json');
    const publicReleasePath = path.join(directory, 'public-release.json');
    const receiptPath = path.join(directory, 'promotion.json');
    const successful = run(makeAdapter().adapter, path.join(directory, 'fake-output.json')).receipt;
    const proofBytes = Buffer.from(`${canonicalize(proof.value)}\n`);
    const publicationBytes = Buffer.from(`${canonicalize(publication.value)}\n`);
    const publicReleaseBytes = Buffer.from(canonicalize(publicRelease.value));
    fs.writeFileSync(proofPath, proofBytes, { mode: 0o600 });
    fs.writeFileSync(publicationPath, publicationBytes, { mode: 0o600 });
    fs.writeFileSync(publicReleasePath, publicReleaseBytes, { mode: 0o600 });
    fs.writeFileSync(receiptPath, `${canonicalize(successful)}\n`, { mode: 0o600 });
    const common = [
      '--plugin', 'session-relay',
      '--source-proof', proofPath, '--source-proof-sha256', hash(proofBytes),
      '--publication', publicationPath, '--publication-sha256', hash(publicationBytes),
      '--public-release', publicReleasePath, '--public-release-sha256', hash(publicReleaseBytes),
      '--docks-kit-release', 'cli-v0.9.0',
      '--expected-origin-main', OLD_MAIN,
      '--receipt-out', receiptPath,
      '0.12.0',
    ];
    await assert.rejects(
      dispatchSessionRelayRelease(['--promote-reviewed', ...common]),
      /output already exists/i,
      'non-resume modes must reject an existing receipt output before their handler',
    );
    await assert.rejects(
      dispatchSessionRelayRelease(['--resume-promotion', '--plugin', 'session-relay', '--transaction-ref', TRANSACTION_REF, ...common.slice(2)]),
      (error) => {
        assert.doesNotMatch(error.message, /output already exists/i);
        return true;
      },
      'resume must pass an existing mode-0600 receipt output through to transaction recovery',
    );
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

{
  for (const liveMutator of [
    (evidence) => { evidence.launcher_sha256 = DIGEST('e'); },
    (evidence) => { evidence.docks_kit_asset.digest = DIGEST('e'); },
    (evidence) => { evidence.launcher_version = ''; },
  ]) {
    const { adapter } = makeAdapter({ liveMutator });
    assert.equal(run(adapter, '/receipts/live-binding-failure.json').receipt.outcome, 'restored_failure');
  }
  const failedPrepush = makeAdapter({
    prepushMutator: (evidence) => {
      evidence.installed_binary_sha256 = hash(Buffer.alloc(0));
      evidence.installed_version = '';
      evidence.launcher_version = '';
    },
  });
  const failed = run(failedPrepush.adapter, '/receipts/prepush-nonzero.json');
  assert.equal(failed.receipt.outcome, 'failure');
  assert.equal(failedPrepush.state.journal.at(-1).entry.phase, 'terminal_failure');
  const appendCount = failedPrepush.state.counts.append;
  assert.equal(run(failedPrepush.adapter, '/receipts/prepush-nonzero.json', {}, true).receipt.outcome, 'failure');
  assert.equal(failedPrepush.state.counts.append, appendCount, 'failed prepush terminal resume must not append');
}

{
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-promotion-cold-'));
  const origin = path.join(root, 'origin.git');
  const seed = path.join(root, 'seed');
  const cold = path.join(root, 'cold');
  try {
    fs.mkdirSync(seed);
    fs.mkdirSync(cold);
    runGit(root, ['init', '--bare', origin]);
    runGit(seed, ['init']);
    runGit(seed, ['config', 'user.name', 'Session Relay Test']);
    runGit(seed, ['config', 'user.email', 'session-relay@example.invalid']);
    runGit(seed, ['commit', '--allow-empty', '-m', canonicalize({ kind: 'cold-journal' })]);
    const tip = runGit(seed, ['rev-parse', 'HEAD']).stdout.trim();
    runGit(seed, ['remote', 'add', 'origin', origin]);
    runGit(seed, ['push', 'origin', `HEAD:${TRANSACTION_REF}`, 'HEAD:refs/heads/main']);
    runGit(cold, ['init']);
    runGit(cold, ['remote', 'add', 'origin', origin]);
    assert.notEqual(runGit(cold, ['cat-file', '-e', tip], false).status, 0, 'cold fixture must start without authoritative objects');
    const journal = readPromotionJournalFromRepository(TRANSACTION_REF, tip, cold);
    assert.equal(journal.at(-1).commit, tip);
    assert.deepEqual(journal.at(-1).entry, { kind: 'cold-journal' });
    assert.equal(runGit(cold, ['cat-file', '-e', tip]).status, 0, 'journal read must fetch before rev-list/show');
    assert.equal(fetchPromotionAuthoritativeRef('refs/heads/main', tip, 'refs/session-relay-release/main', cold), tip);
    runGit(seed, ['commit', '--allow-empty', '-m', canonicalize({ kind: 'cold-journal-2' })]);
    const movedTip = runGit(seed, ['rev-parse', 'HEAD']).stdout.trim();
    runGit(seed, ['push', 'origin', `HEAD:${TRANSACTION_REF}`]);
    const movedJournal = readPromotionJournalFromRepository(TRANSACTION_REF, tip, cold);
    assert.equal(movedJournal.at(-1).commit, movedTip, 'a single ref movement must requery, refetch, and read the new exact tip');
    assert.deepEqual(movedJournal.at(-1).entry, { kind: 'cold-journal-2' });
    runGit(seed, ['push', 'origin', `:${TRANSACTION_REF}`]);
    assert.throws(
      () => readPromotionJournalFromRepository(TRANSACTION_REF, tip, cold),
      (error) => /deleted|fetch authoritative/.test(error.message) && ['failure', 'manual_incident'].includes(error.outcome),
    );
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

await assert.rejects(
  dispatchSessionRelayRelease(['--emit-public-request', '--plugin', 'session-relay', '0.12.0']),
  /missing required option: --publication/i,
  'emit-public-request CLI mode must be recognized',
);
await assert.rejects(
  dispatchSessionRelayRelease(['--verify-public-release', '--plugin', 'session-relay', '0.12.0']),
  /missing required option: --request/i,
  'verify-public-release CLI mode must be recognized',
);
await assert.rejects(
  dispatchSessionRelayRelease([
    '--promote-reviewed',
    '--plugin', 'session-relay', '0.12.0',
    '--source-proof', '/proof.json', '--source-proof-sha256', DIGEST('1'),
    '--publication', '/publication.json', '--publication-sha256', DIGEST('2'),
    '--public-release', '/public-release.json', '--public-release-sha256', DIGEST('3'),
    '--docks-kit-release', 'cli-v0.9.0',
    '--expected-origin-main', OLD_MAIN,
    '--receipt-out', '/promotion.json',
    '--repair-prepush',
  ]),
  /repair-prepush.*repair-implementation-commit.*together/i,
  'repair-prepush CLI mode must require its committed repair identity',
);

process.stdout.write('release promotion contract: OK\n');
