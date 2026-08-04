import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { dispatchSessionRelayRelease } from '../../../scripts/lib/session-relay-release-cli.mjs';
import {
  LOCK_REF as CURRENT_LOCK_REF,
  TRANSACTION_REF as CURRENT_TRANSACTION_REF,
  canonicalize,
  loadReleaseInstance,
  PRERELEASE_BODY,
  REPO,
  SessionRelayReleaseError,
  STABLE_BODY,
  TAG,
  VERSION,
} from '../../../scripts/lib/session-relay-release-core.mjs';
import { releaseTagCommit } from '../../../scripts/lib/session-relay-release-instances/schema.mjs';
import { validateSourcePreparationProof } from '../../../scripts/lib/session-relay-release-preparation.mjs';
import * as releasePromotion from '../../../scripts/lib/session-relay-release-promotion.mjs';
import {
  fetchPromotionAuthoritativeRef,
  PROMOTION_ADAPTER_KEYS,
  promoteReviewed,
  readPromotionJournalFromRepository,
  validatePromotionJournal,
  validatePromotionReceipt,
  validatePromotionReceiptForFinalization,
} from '../../../scripts/lib/session-relay-release-promotion.mjs';
import {
  canonicalPlanView,
  canonicalVerificationResults,
} from '../../plan-lifecycle/skills/productivity/plan-manager/scripts/plan-run.mjs';
import { resolveHistoricalPublicationPlanPath } from './historical-plan-path.mjs';
import { resolveShippedRelayVersion } from './version.mjs';

// Synthetic commit-shaped values isolate the legacy promotion state machine from
// repository history. They are positive fixture topology, never release-instance
// identity and never deliberately wrong comparison values.
const OLD_MAIN = '1'.repeat(40);
const TAG_COMMIT = '2'.repeat(40);
const SHIPPED_COMMIT = 'd'.repeat(40);
const PROMOTED_COMMIT = '3'.repeat(40);
const LOCK_COMMIT = '4'.repeat(40);
const RESTORE_COMMIT = '5'.repeat(40);
const REAPPLY_COMMIT = '6'.repeat(40);
const REPAIR_IMPLEMENTATION_COMMIT = 'e'.repeat(40);
// Immutable public commit used by the legacy public-release fixture; unlike a
// rejection mutation, it is positive fixture identity and is never compared as a
// deliberately wrong value.
const PUBLIC_REVIEWED_COMMIT = loadReleaseInstance('0.13.0', {
  require: ['legacy_0_13'],
}).legacy_0_13.companion_base_commit;
const PUBLIC_RELEASE_COMMIT = '7'.repeat(40);
const PUBLIC_PLAN_COMMIT = '8'.repeat(40);
const DIGEST = (letter) => letter.repeat(64);
const BLOB = (letter) => letter.repeat(40);

function differentHex(value) {
  return value.slice(0, -1) + (value.endsWith('f') ? '0' : 'f');
}
const hash = (value) =>
  createHash('sha256')
    .update(Buffer.isBuffer(value) || typeof value === 'string' ? value : canonicalize(value))
    .digest('hex');
const HOST_ASSET_DIGEST = '5'.repeat(64);
const RELEASE_VERSION = '0.13.0';
const RELEASE_TAG = 'session-relay--v0.13.0';
const LEGACY_RELEASE_INSTANCE = loadReleaseInstance(RELEASE_VERSION, { require: ['legacy_0_13'] });
const LEGACY_PRERELEASE_BODY =
  'Session Relay 0.13.0 is staged for compatibility validation. Do not install it directly or advertise installation instructions. Wait for the stable release.';
const LOCK_REF = 'refs/heads/locks/session-relay-0.13.0';
const TRANSACTION_REF = 'refs/heads/transactions/session-relay-0.13.0';
const PUBLIC_VERSION = '0.10.2';
const PUBLIC_TAG = 'cli-v0.10.2';
const DOCKS_PLAN_PATH = LEGACY_RELEASE_INSTANCE.legacy_0_13.pinned_completion_state.plan_path;
const DOCKS_FINISHED_PLAN_PATH = LEGACY_RELEASE_INSTANCE.legacy_0_13.pinned_completion.finishedPlanPath;
const PUBLIC_PLAN_PATH = LEGACY_RELEASE_INSTANCE.legacy_0_13.public_plan_path;
const PUBLIC_FINISHED_PLAN_PATH = 'docs/plans/finished/2026-07-23-session-relay-cli-0.13.0-production-release.md';
// Retained 0.13-generation Relay asset lists: the legacy promotion machine
// fixtures below stay four-target (including x86_64-apple-darwin) because the
// frozen 0.13 receipts they replay shipped four binaries.
const ORDINARY_ASSET_NAMES = Object.freeze([
  'session-relay-aarch64-apple-darwin',
  'session-relay-aarch64-unknown-linux-musl',
  'session-relay-x86_64-apple-darwin',
  'session-relay-x86_64-unknown-linux-musl',
]);
const PUBLICATION_ASSET_NAMES = Object.freeze(['SHA256SUMS', ...ORDINARY_ASSET_NAMES]);
// Current 0.16-generation Relay asset lists: exactly three targets.
const CURRENT_ORDINARY_ASSET_NAMES = Object.freeze([
  'session-relay-aarch64-apple-darwin',
  'session-relay-aarch64-unknown-linux-musl',
  'session-relay-x86_64-unknown-linux-musl',
]);
const CURRENT_PUBLICATION_ASSET_NAMES = Object.freeze(['SHA256SUMS', ...CURRENT_ORDINARY_ASSET_NAMES]);
const CURRENT_PUBLIC_ASSET_TARGETS = Object.freeze([
  'x86_64-unknown-linux-musl',
  'aarch64-unknown-linux-musl',
  'aarch64-apple-darwin',
]);
const { version: CURRENT_RELEASE_VERSION, tag: CURRENT_RELEASE_TAG } = resolveShippedRelayVersion(REPO);
const CURRENT_RELEASE_INSTANCE = loadReleaseInstance(CURRENT_RELEASE_VERSION, { require: ['planrun_attempt'] });
const RETAINED_V2_RELEASE_VERSION = '0.14.0';
const RETAINED_V2_RELEASE_TAG = `session-relay--v${RETAINED_V2_RELEASE_VERSION}`;
const RETAINED_V2_INSTANCE = loadReleaseInstance(RETAINED_V2_RELEASE_VERSION, {
  require: ['current_attempt', 'public_child'],
});
const RETAINED_V2_PUBLIC_PLAN_PATH =
  `docs/plans/active/session-relay-${RETAINED_V2_RELEASE_VERSION}-` +
  `docks-kit-${RETAINED_V2_INSTANCE.public_child.version}-release.md`;
const PLANRUN_RELEASE_TAG_COMMIT = releaseTagCommit(CURRENT_RELEASE_INSTANCE);
const NOT_PLANRUN_RELEASE_TAG_COMMIT =
  PLANRUN_RELEASE_TAG_COMMIT === null ? null : differentHex(PLANRUN_RELEASE_TAG_COMMIT);
// The validated instance value is the only branch selector: null exercises exact
// unborn-tag refusals; a real commit exercises the preserved happy-path contracts.
const CURRENT_PUBLIC_VERSION = CURRENT_RELEASE_INSTANCE.public_child.version;
const CURRENT_PUBLIC_TAG = CURRENT_RELEASE_INSTANCE.public_child.tag;
const CURRENT_GOAL_ID = CURRENT_RELEASE_INSTANCE.current_attempt.goal_id;
const CURRENT_DOCKS_RUN_ID = CURRENT_RELEASE_INSTANCE.current_attempt.docks_run_id;
const CURRENT_DOCKS_PLAN_PATH = CURRENT_RELEASE_INSTANCE.current_attempt.docks_plan_path;
const PLANRUN_DOCKS_REPOSITORY_ID = CURRENT_RELEASE_INSTANCE.planrun_attempt.docks_repository_id;
const PLANRUN_DOCKS_RUN_ID = CURRENT_RELEASE_INSTANCE.planrun_attempt.docks_run_id;
const PLANRUN_DOCKS_PLAN_PATH = CURRENT_RELEASE_INSTANCE.planrun_attempt.docks_plan_path;
const PLANRUN_DOCKS_SOURCE_BASE = CURRENT_RELEASE_INSTANCE.planrun_attempt.docks_source_base;
// The instance source is the immutable release source. A current same-path
// successor has a later PlanRun source and must preserve both identities.
const POST_TAG_PLANRUN_SOURCE_BASE = differentHex(PLANRUN_DOCKS_SOURCE_BASE);
const NOT_PLANRUN_DOCKS_SOURCE_BASE = `${differentHex(PLANRUN_DOCKS_SOURCE_BASE.slice(0, -1))}${PLANRUN_DOCKS_SOURCE_BASE.at(-1)}`;
// Sourced from the instance so the manifest census cannot drift from what the
// release actually declares. localeCompare-ascending, which the validator enforces.
const PLANRUN_DOCKS_AFFECTED_PATHS = Object.freeze([
  '.claude-plugin/marketplace.json',
  '.github/AGENTS.md',
  '.github/workflows/build-binaries.yml',
  'plugins/session-relay/.claude-plugin/plugin.json',
  'plugins/session-relay/.codex-plugin/plugin.json',
  'plugins/session-relay/AGENTS.md',
  'plugins/session-relay/rust/Cargo.lock',
  'plugins/session-relay/rust/Cargo.toml',
  'plugins/session-relay/rust/src/supervisor.rs',
  'plugins/session-relay/rust/tests/lifecycle_supervisor.rs',
  'plugins/session-relay/test/companion-distribution-contract.mjs',
  'plugins/session-relay/test/distribution-contract.mjs',
  'plugins/session-relay/test/fixtures/release-identity-inventory.json',
  'plugins/session-relay/test/fixtures/rust-test-inventory.json',
  'plugins/session-relay/test/release-evidence-contract.mjs',
  'plugins/session-relay/test/release-instance-contract.mjs',
  'plugins/session-relay/test/release-promotion-contract.mjs',
  'plugins/session-relay/test/release-publication-contract.mjs',
  'scripts/AGENTS.md',
  'scripts/lib/plugins.mjs',
  'scripts/lib/rust-bin.mjs',
  'scripts/lib/session-relay-release-core.mjs',
  'scripts/lib/session-relay-release-instances/0.16.0.json',
  'scripts/lib/session-relay-release-preparation.mjs',
  'scripts/lib/session-relay-release-promotion.mjs',
  'scripts/lib/session-relay-release-publication.mjs',
  'scripts/tests/ci-plugin-targeting.mjs',
  'scripts/verify-session-relay-preflight.mjs',
]);
const CURRENT_PUBLIC_RUN_ID = CURRENT_RELEASE_INSTANCE.current_attempt.public_run_id;
// The child's ACTIVE plan, which preparation.mjs:84 derives the same way. Its
// archived counterpart is a separate constant: one artifact, two lifecycle paths.
const CURRENT_PUBLIC_PLAN_PATH = `docs/plans/active/session-relay-${CURRENT_RELEASE_VERSION}-docks-kit-${CURRENT_PUBLIC_VERSION}-release.md`;
const CURRENT_PUBLIC_FINISHED_PLAN_PATH = `docs/plans/finished/2026-08-04-session-relay-${CURRENT_RELEASE_VERSION}-docks-kit-${CURRENT_PUBLIC_VERSION}-release.md`;
// The immutable 0.13 publication identities remain historical inputs to every
// promotion generation, independently of the current retained-attempt shape.
const HISTORICAL_RELEASE_PLAN_PATH = resolveHistoricalPublicationPlanPath(REPO);
const HISTORICAL_PUBLICATION_SHA256 = '31d096d31702b66d7e97085a82d8b7da1b75155f828b1d2382a0ac8427ba7ea2';
const HISTORICAL_PUBLIC_REQUEST_SHA256 = '7cf02781a2ed3c75423321492fb2cd4c4944f6da6d6d41290e26a5f3ca0cf902';
const RETAINED_PROMOTION_SHA256 = '7ffaa7967d9ca8cc7c53c3ca22efe932d3028ad3caf210cec8157aec7bbd1670';
const RETAINED_PROMOTION_SOURCE_PROOF_SHA256 = hash({
  kind: 'retained-source-proof',
  release_tag_commit: PLANRUN_RELEASE_TAG_COMMIT,
  version: CURRENT_RELEASE_VERSION,
});
const NOT_CURRENT_DOCKS_RUN_ID = differentHex(CURRENT_DOCKS_RUN_ID);
const NOT_PLANRUN_DOCKS_RUN_ID = differentHex(PLANRUN_DOCKS_RUN_ID);
const NOT_CURRENT_PUBLIC_RUN_ID = differentHex(CURRENT_PUBLIC_RUN_ID);
// A retained promotion describes a PRIOR ATTEMPT AT THE CURRENT RELEASE, retained
// across a re-attempt - not a previous release. The validators compare its version,
// tag, goal and run against the current module identity, so its child half must be
// the current child too.
// The synthetic prior-attempt digests bind the current release identity instead of
// borrowing historical receipt identities from a different release.
const retainedFixtureDigest = (kind) =>
  hash({ kind, release_tag_commit: PLANRUN_RELEASE_TAG_COMMIT, version: CURRENT_RELEASE_VERSION });
const RETAINED_PROMOTION_COMPLETION_REVIEW_SHA256 = retainedFixtureDigest('completion-review');
const RETAINED_PROMOTION_PUBLICATION_SHA256 = retainedFixtureDigest('publication');
const RETAINED_PROMOTION_PUBLIC_RELEASE_SHA256 = retainedFixtureDigest('public-release');
// 0.15.0 is a first attempt, so it declares no retained promotion and the module
// default refuses one. The rebind suite therefore injects a retained promotion
// through the validators' defaulted `expected` seam. It describes a PRIOR ATTEMPT AT
// THIS RELEASE, not a prior release: `validateCurrentPromotionReceiptCore` compares
// version, tag, goal and run against the current module identity, so a 0.14.0-shaped
// receipt can never satisfy a 0.15.0 module. Defined below the fixture because it
// digests it.

function retainedPromotionV3() {
  const publication = currentBoundaryPublicationValue();
  const releaseIdentity = {
    assets: structuredClone(publication.assets),
    release_database_id: publication.release_database_id,
    tag_commit: PLANRUN_RELEASE_TAG_COMMIT,
    workflow_run_attempt: publication.workflow.attempt,
    workflow_run_id: publication.workflow.run_id,
  };
  return {
    schema: 3,
    type: 'PromotionReceiptV3',
    repository_id: 'DocksDocks/docks',
    version: CURRENT_RELEASE_VERSION,
    tag: CURRENT_RELEASE_TAG,
    source_proof_sha256: RETAINED_PROMOTION_SOURCE_PROOF_SHA256,
    reviewed_source_commit: PLANRUN_RELEASE_TAG_COMMIT,
    reviewed_source_ancestry: true,
    docks_plan: {
      repository_id: 'DocksDocks/docks',
      goal_id: CURRENT_GOAL_ID,
      run_id: CURRENT_DOCKS_RUN_ID,
      plan_path: CURRENT_DOCKS_PLAN_PATH,
      implementation_commit: PLANRUN_RELEASE_TAG_COMMIT,
      completion_review_sha256: RETAINED_PROMOTION_COMPLETION_REVIEW_SHA256,
      status: 'ongoing',
    },
    publication_receipt_sha256: RETAINED_PROMOTION_PUBLICATION_SHA256,
    public_release_receipt_sha256: RETAINED_PROMOTION_PUBLIC_RELEASE_SHA256,
    public_child: {
      repository_id: 'DocksDocks/public',
      goal_id: CURRENT_GOAL_ID,
      run_id: CURRENT_PUBLIC_RUN_ID,
      version: CURRENT_PUBLIC_VERSION,
      tag: CURRENT_PUBLIC_TAG,
      npm_package: 'docks-kit',
      npm_version: CURRENT_PUBLIC_VERSION,
      plan_path: CURRENT_PUBLIC_FINISHED_PLAN_PATH,
      status: 'finished',
      planrun_verified: true,
      finished_at: '2026-07-26T01:36:05.859Z',
    },
    staged_release: { ...structuredClone(releaseIdentity), prerelease: true },
    stable_release: { ...structuredClone(releaseIdentity), prerelease: false },
    byte_identical_promotion: true,
    historical_receipts: {
      version: RELEASE_VERSION,
      tag: RELEASE_TAG,
      publication_sha256: HISTORICAL_PUBLICATION_SHA256,
      public_request_sha256: HISTORICAL_PUBLIC_REQUEST_SHA256,
    },
    outcome: 'success',
    completed_at: '2026-07-26T04:45:55.405Z',
  };
}

// The digest is taken from the fixture rather than pinned, because 0.15.0 has no
// recorded prior promotion to pin against. That equality is fixture bookkeeping, not
// a security claim - the security claims are the rejection cases in the rebind suite,
// which mutate this receipt 22 ways and require every one to be refused.
const RETAINED_PROMOTION_EXPECTATION = Object.freeze({
  docks_run_id: CURRENT_DOCKS_RUN_ID,
  docks_plan_path: CURRENT_DOCKS_PLAN_PATH,
  promotion_sha256: hash(retainedPromotionV3()),
  completion_review_sha256: RETAINED_PROMOTION_COMPLETION_REVIEW_SHA256,
  publication_sha256: RETAINED_PROMOTION_PUBLICATION_SHA256,
  public_release_sha256: RETAINED_PROMOTION_PUBLIC_RELEASE_SHA256,
  source_proof_sha256: RETAINED_PROMOTION_SOURCE_PROOF_SHA256,
  release_tag_commit: PLANRUN_RELEASE_TAG_COMMIT,
});

const candidate = {
  schema: 1,
  type: 'SourcePreparationCandidateV1',
  repository_id: 'DocksDocks/docks',
  version: RELEASE_VERSION,
  source_commit: TAG_COMMIT,
  execution_base_commit: OLD_MAIN,
  plan: {
    path: DOCKS_PLAN_PATH,
    source_blob_sha256: DIGEST('1'),
  },
  docks_red: {
    sha256: DIGEST('2'),
    pre_production_commit: OLD_MAIN,
    test_blobs: [{ path: 'plugins/session-relay/test/release-promotion-contract.mjs', blob_id: BLOB('a') }],
  },
  companion: {
    repository_id: 'DocksDocks/public',
    validation_ref: `refs/heads/preflight/session-relay-cli-0.13.0-${PUBLIC_REVIEWED_COMMIT.slice(0, 12)}`,
    commit: PUBLIC_REVIEWED_COMMIT,
    plan_path: PUBLIC_PLAN_PATH,
    input_sha256: DIGEST('3'),
    execution_base_commit: BLOB('b'),
    review_receipt_sha256: DIGEST('4'),
    red_receipt_sha256: DIGEST('5'),
    status: 'blocked',
    blocked_reason: 'Awaiting the four independently hashed `session-relay--v0.13.0` production asset digests.',
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
  checks: [
    ['A1', [['node', 'plugins/session-relay/test/release-evidence-contract.mjs']]],
    ['A2', [['node', 'plugins/session-relay/test/release-publication-contract.mjs']]],
    ['A3', [['node', 'plugins/session-relay/test/release-promotion-contract.mjs']]],
    ['A4', [['node', 'plugins/session-relay/test/distribution-contract.mjs']]],
    [
      'A5',
      [
        [
          'node',
          'plugins/session-relay/test/companion-distribution-contract.mjs',
          '--public-remote',
          'https://github.com/DocksDocks/public.git',
          '--public-ref',
          `refs/heads/preflight/session-relay-cli-0.13.0-${PUBLIC_REVIEWED_COMMIT.slice(0, 12)}`,
          '--public-commit',
          PUBLIC_REVIEWED_COMMIT,
          '--detached-clone',
        ],
      ],
    ],
    [
      'A6',
      [
        [
          'cargo',
          '+1.85.0',
          'build',
          '--manifest-path',
          'plugins/session-relay/rust/Cargo.toml',
          '--release',
          '--locked',
        ],
        ['sh', '-c', 'test "$(plugins/session-relay/rust/target/release/relay --version)" = "session-relay 0.13.0"'],
      ],
    ],
  ].map(([id, commands]) => ({
    id,
    steps: commands.map((argv) => ({
      argv,
      exit_code: 0,
      stdout_sha256: DIGEST('8'),
      stderr_sha256: DIGEST('9'),
    })),
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
  version: RELEASE_VERSION,
  source_commit: TAG_COMMIT,
  tag_commit: TAG_COMMIT,
  evidence_commit: '8'.repeat(40),
  shipped_commit: SHIPPED_COMMIT,
  promoted_commit: PROMOTED_COMMIT,
  candidate,
  candidate_sha256: hash(candidate),
  plans: {
    source_path: DOCKS_PLAN_PATH,
    source_sha256: DIGEST('c'),
    evidence_path: DOCKS_PLAN_PATH,
    evidence_sha256: DIGEST('d'),
    finished_path: DOCKS_FINISHED_PLAN_PATH,
    finished_sha256: DIGEST('e'),
  },
  completion_review_sha256: DIGEST('f'),
  source_ancestry: {
    source_commit: TAG_COMMIT,
    evidence_commit: '8'.repeat(40),
    shipped_commit: SHIPPED_COMMIT,
    verified: true,
  },
  non_plan_tree_equivalence: {
    source_commit: TAG_COMMIT,
    shipped_commit: SHIPPED_COMMIT,
    excluded_paths: [DOCKS_PLAN_PATH, DOCKS_FINISHED_PLAN_PATH],
    verified: true,
  },
  public_repository_id: 'DocksDocks/public',
  public_reviewed_commit: PUBLIC_REVIEWED_COMMIT,
  review_status: 'passed',
  bound_at: '2026-07-17T19:30:00.000Z',
};
assert.equal(VERSION, CURRENT_RELEASE_VERSION, `Session Relay production version must be ${CURRENT_RELEASE_VERSION}`);
assert.equal(TAG, CURRENT_RELEASE_TAG, `Session Relay production tag must be ${CURRENT_RELEASE_TAG}`);
assert.match(
  candidate.companion.validation_ref,
  /^refs\/heads\/preflight\/session-relay-cli-0\.13\.0-[0-9a-f]{12}$/,
  'public validation ref must use the immutable 0.13.0 preflight grammar',
);
assert.equal(
  candidate.companion.validation_ref.slice(-12),
  candidate.companion.commit.slice(0, 12),
  'public validation ref suffix must bind the first 12 hex of the immutable public commit',
);
validateSourcePreparationProof(proofValue);
const proof = { digest: hash(proofValue), value: proofValue };

const assets = PUBLICATION_ASSET_NAMES.map((name, index) => ({
  name,
  database_id: 100 + index,
  size: 1000 + index,
  digest: String(index + 1).repeat(64),
}));

const publication = {
  digest: DIGEST('b'),
  value: {
    schema: 1,
    type: 'SessionRelayPublicationReceiptV1',
    repository_id: 'DocksDocks/docks',
    version: RELEASE_VERSION,
    source_proof_sha256: proof.digest,
    tag: RELEASE_TAG,
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
    body_sha256: hash(LEGACY_PRERELEASE_BODY),
    assets,
    transition: 'tag_and_release_created',
    created_at: '2026-07-17T20:00:00.000Z',
  },
};
// Retained 0.13-generation pin targets for the legacy machine fixtures above.
const PUBLIC_ASSET_TARGETS = [
  'x86_64-unknown-linux-musl',
  'aarch64-unknown-linux-musl',
  'x86_64-apple-darwin',
  'aarch64-apple-darwin',
];
assert.deepEqual(
  assets.map(({ name }) => name),
  PUBLICATION_ASSET_NAMES,
  'promotion fixture must retain SHA256SUMS and exactly four ordinary native assets',
);
assert.deepEqual(
  PUBLIC_ASSET_TARGETS.map((target) => `session-relay-${target}`).sort(),
  [...ORDINARY_ASSET_NAMES].sort(),
  'promotion must retain exact digest pins for both Linux and both Darwin targets',
);
const PUBLIC_RELEASE_ASSET_NAMES = [
  'SHA256SUMS',
  'docks-kit-darwin-arm64',
  'docks-kit-darwin-x64',
  'docks-kit-linux-arm64',
  'docks-kit-linux-x64',
];
const publicationAssetPins = Object.fromEntries(
  PUBLIC_ASSET_TARGETS.map((target) => [target, assets.find(({ name }) => name === `session-relay-${target}`).digest]),
);
const publicRequestValue = {
  schema: 1,
  type: 'PublicReleaseRequestV1',
  repository_id: 'DocksDocks/public',
  tag: PUBLIC_TAG,
  version: PUBLIC_VERSION,
  companion_base_commit: PUBLIC_REVIEWED_COMMIT,
  session_relay: {
    repository_id: 'DocksDocks/docks',
    tag: RELEASE_TAG,
    version: RELEASE_VERSION,
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
  tag: PUBLIC_TAG,
  version: PUBLIC_VERSION,
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
      digest: String(index + 8)
        .slice(-1)
        .repeat(64),
    })),
    checksums_sha256: DIGEST('8'),
  },
  npm: { state: 'published' },
  pinned_assets: publicationAssetPins,
  public_plan: {
    commit: PUBLIC_PLAN_COMMIT,
    path: PUBLIC_FINISHED_PLAN_PATH,
    completion_receipt_sha256: DIGEST('c'),
  },
  created_at: '2026-07-17T21:00:00.000Z',
};
const publicRelease = { digest: hash(publicReleaseValue), value: publicReleaseValue };

function options(output = '/receipts/promotion.json', extra = {}) {
  return new Map(
    Object.entries({
      publication: '/receipts/publication.json',
      'publication-sha256': publication.digest,
      'public-release': '/receipts/public-release.json',
      'public-release-sha256': publicRelease.digest,
      'source-proof': '/receipts/proof.json',
      'source-proof-sha256': proof.digest,
      'docks-kit-release': PUBLIC_TAG,
      'expected-origin-main': OLD_MAIN,
      'receipt-out': output,
      ...extra,
    }),
  );
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
    installed_version: `session-relay ${RELEASE_VERSION}`,
    launcher_sha256: DIGEST('a'),
    launcher_version: `session-relay ${RELEASE_VERSION}`,
    docks_kit_target_commit: PUBLIC_RELEASE_COMMIT,
    docks_kit_asset: { name: 'docks-kit-linux-x64', database_id: 501, size: 2000, digest: DIGEST('9') },
    docks_kit_release_database_id: 601,
  };
}

function makeAdapter({
  killAfter = null,
  killMutation = null,
  rejectMutation = null,
  outageAfterAppend = null,
  liveResults = [true],
  prepushThrows = false,
  prepushMutator = null,
  liveThrows = false,
  liveMutator = null,
  restoreDrift = false,
  reapplyDrift = false,
  ancestry = true,
  publicAncestry = true,
  initialMain = OLD_MAIN,
  adoptionRerunParity = null,
} = {}) {
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
    'plugins/session-relay/new-in-0.13': BLOB('7'),
  };
  if (initialMain === PROMOTED_COMMIT) state.currentBlobs = { ...promotedBlobs };
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
      assert.ok(
        [TAG_COMMIT, PROMOTED_COMMIT].includes(ancestorCommit),
        'reviewed promotion commit must be the ancestor',
      );
      assert.equal(descendantCommit, initialMain, 'expected origin/main must match the fixture head');
      return ancestry;
    },
    isPublicAncestor: (ancestorCommit, descendantCommit) => {
      assert.equal(ancestorCommit, PUBLIC_REVIEWED_COMMIT);
      assert.equal(descendantCommit, PUBLIC_RELEASE_COMMIT);
      return publicAncestry;
    },
    validatePrepushRepair: ({ baseCommit, repairCommit }) => {
      assert.equal(baseCommit, initialMain);
      assert.equal(repairCommit, REPAIR_IMPLEMENTATION_COMMIT);
      return {
        base_commit: initialMain,
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
      assert.equal(
        state.refs.get(ref) ?? null,
        prior,
        'journal append must use the authoritative prior tip as its CAS lease',
      );
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
      assert.equal(commit, PROMOTED_COMMIT, 'main update must consume the reviewed promoted implementation');
      assert.notEqual(commit, SHIPPED_COMMIT, 'main update must not roll back to the shipped archive');
      state.refs.set('refs/heads/main', commit);
      state.currentBlobs = { ...promotedBlobs };
      state.counts.main += 1;
      interrupt('main_mutation');
    },
    releaseState: () => ({
      repository_id: 'DocksDocks/docks',
      tag: RELEASE_TAG,
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
        unexpected_paths: Object.keys(state.currentBlobs)
          .filter((item) => !paths.includes(item))
          .sort(),
      };
    },
    validateCommitTransition: ({ kind, commit, parent }) => ({
      valid:
        kind === 'restore'
          ? commit === RESTORE_COMMIT && [PROMOTED_COMMIT, REAPPLY_COMMIT].includes(parent)
          : commit === REAPPLY_COMMIT && parent === RESTORE_COMMIT,
      live_smoke: kind === 'restore' ? smoke('live') : null,
      recorded_parity:
        kind === 'restore'
          ? {
              manifest_catalog: true,
              full_ci_exit: 0,
              full_ci_stdout_sha256: DIGEST('7'),
              full_ci_stderr_sha256: DIGEST('8'),
            }
          : null,
      parity:
        kind === 'restore'
          ? (adoptionRerunParity ?? {
              manifest_catalog: true,
              full_ci_exit: 0,
              full_ci_stdout_sha256: DIGEST('7'),
              full_ci_stderr_sha256: DIGEST('8'),
            })
          : null,
      verification_ok: true,
    }),
    runPrepushSmoke: ({ docksKitRepository, docksKitRelease, sourceCommit, publicReleaseCommit }) => {
      if (state.prepushThrows) throw new Error('docks-kit identity failed');
      state.calls.push({
        operation: 'prepush',
        docksKitRepository,
        docksKitRelease,
        sourceCommit,
        publicReleaseCommit,
      });
      assert.equal(docksKitRepository, 'DocksDocks/public');
      assert.equal(docksKitRelease, PUBLIC_TAG);
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
          parity: {
            manifest_catalog: false,
            full_ci_exit: 1,
            full_ci_stdout_sha256: DIGEST('c'),
            full_ci_stderr_sha256: DIGEST('d'),
          },
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
        parity: {
          manifest_catalog: true,
          full_ci_exit: 0,
          full_ci_stdout_sha256: DIGEST('7'),
          full_ci_stderr_sha256: DIGEST('8'),
        },
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
  return promoteReviewed(
    options(output, resume ? { 'transaction-ref': TRANSACTION_REF, ...extra } : extra),
    resume,
    adapter,
  );
}

function phases(state) {
  return state.journal.map(({ entry }) => `${entry.attempt}:${entry.sequence}:${entry.phase}`);
}

function expectInterrupted(fn) {
  assert.throws(fn, /interrupted after/);
}

function currentReleaseChainV2() {
  const relayImplementationCommit = 'a'.repeat(40);
  const relayTagCommit = PLANRUN_RELEASE_TAG_COMMIT ?? relayImplementationCommit;
  const publicExecutionParent = 'b'.repeat(40);
  const publicRedCommit = 'c'.repeat(40);
  const publicImplementationCommit = 'd'.repeat(40);
  const publicReleaseCommit = 'e'.repeat(40);
  const publicArchiveCommit = 'f'.repeat(40);
  const relayAssets = CURRENT_PUBLICATION_ASSET_NAMES.map((name, index) => ({
    name,
    database_id: 2_000 + index,
    size: 20_000 + index,
    digest: String(index + 1)
      .repeat(64)
      .slice(0, 64),
  }));
  const relayPins = Object.fromEntries(
    CURRENT_PUBLIC_ASSET_TARGETS.map((target) => [
      target,
      relayAssets.find(({ name }) => name === `session-relay-${target}`).digest,
    ]),
  );
  const publicationValue = {
    schema: 2,
    type: 'SessionRelayPublicationReceiptV2',
    repository_id: 'DocksDocks/docks',
    version: CURRENT_RELEASE_VERSION,
    tag: CURRENT_RELEASE_TAG,
    tag_commit: relayTagCommit,
    release_state: 'prerelease',
    release_database_id: 2_100,
    workflow: { run_id: 2_101, attempt: 1 },
    assets: relayAssets,
    digest_evidence: {
      artifact_sha256: structuredClone(relayPins),
      release_download_sha256: structuredClone(relayPins),
      checksum_rows: structuredClone(relayPins),
    },
    created_at: '2026-07-25T15:00:00.000Z',
  };
  const publication = { digest: hash(publicationValue), value: publicationValue };
  const publicReleaseAssets = PUBLIC_RELEASE_ASSET_NAMES.map((name, index) => ({
    name,
    size: 30_000 + index,
    digest: String(index + 7)
      .slice(-1)
      .repeat(64),
  }));
  const publicRedReceipt = {
    schema: 1,
    type: 'TddRedReceiptV1',
    repository_id: 'DocksDocks/public',
    pre_production_commit: publicRedCommit,
    test_paths: [
      {
        path: 'cli/test/unit/sessionRelayCli.test.ts',
        blob_id: '1'.repeat(40),
      },
      {
        path: 'cli/test/unit/toolchain.test.ts',
        blob_id: '2'.repeat(40),
      },
    ],
    command: {
      cwd: '/home/vagrant/projects/public',
      argv: [
        'bun',
        'run',
        'test:unit',
        '--',
        'cli/test/unit/sessionRelayCli.test.ts',
        'cli/test/unit/toolchain.test.ts',
      ],
    },
    exit_code: 1,
    stdout_sha256: DIGEST('8'),
    stderr_sha256: DIGEST('9'),
    captured_at: '2026-07-25T15:30:00.000Z',
    producer: {
      path: 'scripts/capture-tdd-red.mjs',
      blob_id: '3'.repeat(40),
      version: '1',
    },
  };
  const publicRedReceiptSha256 = hash(publicRedReceipt);
  const publicReleaseValue = {
    schema: 2,
    type: 'PublicReleaseReceiptV2',
    request_sha256: DIGEST('1'),
    repository_id: 'DocksDocks/public',
    tag: CURRENT_PUBLIC_TAG,
    version: CURRENT_PUBLIC_VERSION,
    release_commit: publicReleaseCommit,
    companion_base_commit: publicExecutionParent,
    ancestry: {
      execution_parent: publicExecutionParent,
      red_pre_production_commit: publicRedCommit,
      implementation_commit: publicImplementationCommit,
      reviewed_commit: publicImplementationCommit,
      release_commit: publicReleaseCommit,
      archive_commit: publicArchiveCommit,
      red_captured_at: '2026-07-25T15:30:00.000Z',
      implementation_reviewed_at: '2026-07-25T16:30:00.000Z',
      red_to_implementation: true,
      execution_parent_to_implementation: true,
      implementation_to_release: true,
      release_to_archive: true,
    },
    workflow: {
      file: '.github/workflows/release-cli.yml',
      run_database_id: 3_001,
      run_attempt: 1,
      conclusion: 'success',
    },
    release: {
      database_id: 3_002,
      assets: publicReleaseAssets,
      checksums_sha256: publicReleaseAssets[0].digest,
    },
    npm: { package: 'docks-kit', state: 'published', version: CURRENT_PUBLIC_VERSION },
    pinned_assets: structuredClone(relayPins),
    public_plan: {
      schema: 1,
      repository_id: 'DocksDocks/public',
      goal_id: CURRENT_GOAL_ID,
      run_id: CURRENT_PUBLIC_RUN_ID,
      path: CURRENT_PUBLIC_FINISHED_PLAN_PATH,
      status: 'finished',
      implementation_commit: publicImplementationCommit,
      release_commit: publicReleaseCommit,
      archive_commit: publicArchiveCommit,
      red_receipt_sha256: publicRedReceiptSha256,
      red_pre_production_commit: publicRedCommit,
      red_evidence: {
        schema: 1,
        type: 'PublicRedFirstEvidenceV1',
        receipt_sha256: publicRedReceiptSha256,
        expected_failure_signature: 'Relay 0.14.0 and docks-kit 0.12.0 bindings are absent',
        ordered_before_implementation: true,
        receipt: publicRedReceipt,
      },
      completion_review_sha256: DIGEST('3'),
      acceptance_sha256: DIGEST('4'),
      verification_sha256: DIGEST('5'),
      remote_read_back: true,
      finished_at: '2026-07-25T17:30:00.000Z',
    },
    created_at: '2026-07-25T17:00:00.000Z',
  };
  const publicRelease = { digest: hash(publicReleaseValue), value: publicReleaseValue };
  const stagedAssets = relayAssets.map(({ name, database_id: databaseId, size, digest }) => ({
    name,
    database_id: databaseId,
    size,
    digest,
  }));
  const promotionValue = {
    schema: 2,
    type: 'PromotionReceiptV2',
    repository_id: 'DocksDocks/docks',
    version: CURRENT_RELEASE_VERSION,
    tag: CURRENT_RELEASE_TAG,
    source_proof_sha256: DIGEST('6'),
    reviewed_source_commit: relayImplementationCommit,
    reviewed_source_ancestry: true,
    docks_plan: {
      repository_id: 'DocksDocks/docks',
      goal_id: CURRENT_GOAL_ID,
      run_id: CURRENT_DOCKS_RUN_ID,
      plan_path: CURRENT_DOCKS_PLAN_PATH,
      implementation_commit: relayImplementationCommit,
      completion_review_sha256: DIGEST('7'),
      status: 'ongoing',
    },
    publication_receipt_sha256: publication.digest,
    public_release_receipt_sha256: publicRelease.digest,
    public_child: {
      repository_id: 'DocksDocks/public',
      goal_id: CURRENT_GOAL_ID,
      run_id: CURRENT_PUBLIC_RUN_ID,
      version: CURRENT_PUBLIC_VERSION,
      tag: CURRENT_PUBLIC_TAG,
      npm_package: 'docks-kit',
      npm_version: CURRENT_PUBLIC_VERSION,
      plan_path: CURRENT_PUBLIC_FINISHED_PLAN_PATH,
      status: 'finished',
      red_first_verified: true,
      finished_at: '2026-07-25T17:30:00.000Z',
    },
    staged_release: {
      release_database_id: publicationValue.release_database_id,
      tag_commit: publicationValue.tag_commit,
      workflow_run_id: publicationValue.workflow.run_id,
      workflow_run_attempt: publicationValue.workflow.attempt,
      prerelease: true,
      assets: stagedAssets,
    },
    stable_release: {
      release_database_id: publicationValue.release_database_id,
      tag_commit: publicationValue.tag_commit,
      workflow_run_id: publicationValue.workflow.run_id,
      workflow_run_attempt: publicationValue.workflow.attempt,
      prerelease: false,
      assets: structuredClone(stagedAssets),
    },
    byte_identical_promotion: true,
    historical_receipts: {
      version: RELEASE_VERSION,
      tag: RELEASE_TAG,
      publication_sha256: HISTORICAL_PUBLICATION_SHA256,
      public_request_sha256: HISTORICAL_PUBLIC_REQUEST_SHA256,
    },
    outcome: 'success',
    completed_at: '2026-07-25T18:00:00.000Z',
  };
  return {
    publication,
    publicRelease,
    promotion: { digest: hash(promotionValue), value: promotionValue },
  };
}

function assertCurrentReleasePromotionContract() {
  const historicalPlan = fs.readFileSync(HISTORICAL_RELEASE_PLAN_PATH, 'utf8');
  for (const digest of [HISTORICAL_PUBLICATION_SHA256, HISTORICAL_PUBLIC_REQUEST_SHA256]) {
    assert.match(historicalPlan, new RegExp(digest), 'historical Session Relay 0.13.0 receipt identity changed');
  }

  const chain = currentReleaseChainV2();
  const { publication, publicRelease, promotion } = chain;
  assert.equal(publication.value.version, CURRENT_RELEASE_VERSION);
  assert.equal(publication.value.tag, CURRENT_RELEASE_TAG);
  assert.equal(publication.value.release_state, 'prerelease');
  assert.deepEqual(
    publication.value.assets.map(({ name }) => name),
    CURRENT_PUBLICATION_ASSET_NAMES,
    'current Relay prerelease must contain exactly three native binaries plus SHA256SUMS',
  );
  assert.equal(
    publication.value.assets.some(({ name }) => /windows|win32|\.exe$/i.test(name)),
    false,
    'current Relay prerelease must not contain Windows',
  );
  assert.deepEqual(
    publication.value.digest_evidence.artifact_sha256,
    publication.value.digest_evidence.release_download_sha256,
    'independent Relay download hashes must match producer artifact hashes',
  );
  assert.deepEqual(
    publication.value.digest_evidence.artifact_sha256,
    publication.value.digest_evidence.checksum_rows,
    'independent Relay hashes must match SHA256SUMS',
  );
  assert.deepEqual(publicRelease.value.pinned_assets, publication.value.digest_evidence.artifact_sha256);
  assert.equal(publicRelease.value.version, CURRENT_PUBLIC_VERSION);
  assert.equal(publicRelease.value.tag, CURRENT_PUBLIC_TAG);
  assert.deepEqual(publicRelease.value.npm, {
    package: 'docks-kit',
    state: 'published',
    version: CURRENT_PUBLIC_VERSION,
  });
  assert.equal(promotion.value.public_child.npm_package, 'docks-kit');
  assert.equal(promotion.value.public_child.npm_version, CURRENT_PUBLIC_VERSION);
  assert.deepEqual(
    {
      repository_id: promotion.value.docks_plan.repository_id,
      goal_id: promotion.value.docks_plan.goal_id,
      run_id: promotion.value.docks_plan.run_id,
      plan_path: promotion.value.docks_plan.plan_path,
    },
    {
      repository_id: 'DocksDocks/docks',
      goal_id: CURRENT_GOAL_ID,
      run_id: CURRENT_DOCKS_RUN_ID,
      plan_path: CURRENT_DOCKS_PLAN_PATH,
    },
  );
  assert.equal(publicRelease.value.public_plan.status, 'finished');
  assert.equal(publicRelease.value.public_plan.remote_read_back, true);
  assert.equal(publicRelease.value.ancestry.red_to_implementation, true);
  assert.equal(publicRelease.value.ancestry.implementation_to_release, true);
  assert.equal(publicRelease.value.ancestry.release_to_archive, true);
  const redEvidence = publicRelease.value.public_plan.red_evidence;
  assert.ok(
    Date.parse(redEvidence.receipt.captured_at) < Date.parse(publicRelease.value.ancestry.implementation_reviewed_at),
    'public TDD red evidence must precede the reviewed implementation',
  );
  assert.equal(redEvidence.ordered_before_implementation, true);
  assert.equal(redEvidence.receipt.exit_code, 1);
  assert.equal(redEvidence.receipt.test_paths.length, 2);
  assert.ok(redEvidence.receipt.command.argv.length > 0);
  assert.equal(redEvidence.receipt.pre_production_commit, publicRelease.value.ancestry.red_pre_production_commit);
  assert.equal(redEvidence.receipt_sha256, publicRelease.value.public_plan.red_receipt_sha256);
  assert.ok(
    Date.parse(publicRelease.value.created_at) < Date.parse(publicRelease.value.public_plan.finished_at),
    'public release must precede the finished archived child receipt',
  );
  assert.ok(
    Date.parse(publication.value.created_at) < Date.parse(publicRelease.value.created_at),
    'Relay must remain prerelease until the public child release exists',
  );
  assert.ok(
    Date.parse(publicRelease.value.public_plan.finished_at) < Date.parse(promotion.value.completed_at),
    'public child must finish before stable Relay promotion',
  );
  assert.equal(promotion.value.public_child.red_first_verified, true);
  assert.equal(promotion.value.byte_identical_promotion, true);
  assert.equal(promotion.value.staged_release.prerelease, true);
  assert.equal(promotion.value.stable_release.prerelease, false);
  assert.equal(promotion.value.staged_release.release_database_id, promotion.value.stable_release.release_database_id);
  assert.equal(promotion.value.staged_release.tag_commit, promotion.value.stable_release.tag_commit);
  assert.equal(promotion.value.staged_release.workflow_run_id, promotion.value.stable_release.workflow_run_id);
  assert.deepEqual(
    promotion.value.staged_release.assets,
    promotion.value.stable_release.assets,
    'stable promotion must retain byte-identical prerelease assets',
  );

  releasePromotion.validatePublicReleaseReceipt(publicRelease, { publication });
  for (const [label, mutate, pattern] of [
    [
      'public red command evidence removed',
      (value) => {
        value.public_plan.red_evidence.receipt.command.argv = [];
      },
      /red|command|evidence/i,
    ],
    [
      'public red commit detached from ancestry',
      (value) => {
        value.public_plan.red_evidence.receipt.pre_production_commit = differentHex(
          value.public_plan.red_evidence.receipt.pre_production_commit,
        );
      },
      /red|commit|ancestry/i,
    ],
    [
      'public archive not remotely read back',
      (value) => {
        value.public_plan.remote_read_back = false;
      },
      /public|archive|read.back/i,
    ],
  ]) {
    const changed = structuredClone(publicRelease.value);
    mutate(changed);
    assert.throws(
      () => releasePromotion.validatePublicReleaseReceipt({ digest: hash(changed), value: changed }, { publication }),
      pattern,
      label,
    );
  }
  validatePromotionReceipt(promotion.value);

  for (const [label, mutate, pattern] of [
    [
      'unfinished public child',
      (value) => {
        value.public_child.status = 'ongoing';
      },
      /public|finished|status/i,
    ],
    [
      'missing public red-first proof',
      (value) => {
        value.public_child.red_first_verified = false;
      },
      /public|red|order/i,
    ],
    [
      'stable asset substitution',
      (value) => {
        value.stable_release.assets[1].digest = differentHex(value.stable_release.assets[1].digest);
      },
      /asset|byte|digest|promotion/i,
    ],
    [
      'reviewed source ancestry failure',
      (value) => {
        value.reviewed_source_ancestry = false;
      },
      /review|source|ancestry/i,
    ],
    [
      'historical 0.13 receipt rewrite',
      (value) => {
        value.historical_receipts.publication_sha256 = differentHex(value.historical_receipts.publication_sha256);
      },
      /historical|0\.13|receipt/i,
    ],
  ]) {
    const changed = structuredClone(promotion.value);
    mutate(changed);
    assert.throws(() => validatePromotionReceipt(changed), pattern, label);
  }
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
  'verify-public-release wrong five-asset set',
  'verify-public-release present Windows asset',
  'verify-public-release extra asset',
  'verify-public-release missing checksum row',
  'verify-public-release extra checksum row',
  'verify-public-release checksum name conflict',
  'verify-public-release checksum digest conflict',
  'verify-public-release digest-pin mismatch',
  'verify-public-release completion line hash mismatch',
  'verify-public-release non-passed review_status',
];
const missingBoundaryFixtures = boundaryFixtureNames.filter((name) =>
  name.startsWith('emit-')
    ? typeof releasePromotion.emitPublicRequest !== 'function'
    : typeof releasePromotion.verifyPublicRelease !== 'function',
);
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
  const requestResult = captureDigest(() =>
    releasePromotion.emitPublicRequest(
      new Map([
        ['publication', publicationFile.file],
        ['publication-sha256', publicationFile.digest],
        ['receipt-out', requestOutput],
      ]),
      { now: () => '2026-07-17T20:30:00.000Z' },
    ),
  );
  const requestBytes = fs.readFileSync(requestOutput);
  const request = {
    file: requestOutput,
    bytes: requestBytes,
    digest: hash(requestBytes),
    value: JSON.parse(requestBytes),
  };
  const expectedRequest = {
    ...publicRequestValue,
    session_relay: {
      ...publicRequestValue.session_relay,
      publication_receipt_sha256: publicationFile.digest,
    },
  };
  assert.deepEqual(request.value, expectedRequest, 'emit-public-request derivation');
  assert.deepEqual(Object.keys(request.value).sort(), [
    'assets',
    'companion_base_commit',
    'created_at',
    'repository_id',
    'schema',
    'session_relay',
    'tag',
    'type',
    'version',
  ]);
  assert.deepEqual(Object.keys(request.value.session_relay).sort(), [
    'publication_receipt_sha256',
    'repository_id',
    'tag',
    'tag_commit',
    'version',
  ]);
  assert.deepEqual(Object.keys(request.value.assets).sort(), [...PUBLIC_ASSET_TARGETS].sort());
  assert.equal(requestBytes.toString('utf8'), canonicalize(request.value));
  assert.throws(
    () =>
      releasePromotion.emitPublicRequest(
        new Map([
          ['publication', publicationFile.file],
          ['publication-sha256', publicationFile.digest],
          ['receipt-out', requestOutput],
        ]),
        { now: () => '2026-07-17T20:30:00.000Z' },
      ),
    /output already exists/i,
    'emit-public-request no-clobber emission',
  );
  assert.equal(requestResult.receipt.type, 'PublicReleaseRequestV1');
  return { publicationFile, request };
}

function completePublicCompletionReceipt() {
  const producerPlan = fs.readFileSync('docs/plans/finished/2026-07-17-completion-reuse-terminal-lf.md', 'utf8');
  const match = producerPlan.match(/^Completion-review-receipt: (\{.*\})$/m);
  assert.ok(match, 'a complete schema-5 plan-manager completion receipt is available as the producer fixture');
  const receipt = JSON.parse(match[1]);
  const reviewedHead = receipt.reviewed_head;
  return JSON.parse(JSON.stringify(receipt).replaceAll(reviewedHead, PUBLIC_RELEASE_COMMIT));
}

function completionReceiptForReviewedHead(reviewedHead) {
  const receipt = completePublicCompletionReceipt();
  return JSON.parse(JSON.stringify(receipt).replaceAll(PUBLIC_RELEASE_COMMIT, reviewedHead));
}

function changedCompletionReceipt(change) {
  const receipt = completePublicCompletionReceipt();
  change(receipt);
  return receipt;
}

function makePublicReleaseAdapter(
  request,
  {
    tagCommit = PUBLIC_RELEASE_COMMIT,
    releaseAncestry = true,
    planAncestry = true,
    runs,
    removeAsset = null,
    extraAsset = null,
    checksumMutation = null,
    pinnedAssets = request.value.assets,
    reviewStatus = 'passed',
    completionReceipt = completePublicCompletionReceipt(),
  } = {},
) {
  const completionReceiptText = canonicalize(completionReceipt);
  const binaryNames = PUBLIC_RELEASE_ASSET_NAMES.filter((name) => name !== 'SHA256SUMS');
  const binaries = new Map(binaryNames.map((name) => [name, Buffer.from(`public-release:${name}`)]));
  const checksumRows = binaryNames.map((name) => `${hash(binaries.get(name))}  ${name}`);
  if (checksumMutation === 'missing') checksumRows.pop();
  if (checksumMutation === 'extra') checksumRows.push(`${DIGEST('e')}  docks-kit-extra-x64`);
  if (checksumMutation === 'name') checksumRows[0] = `${hash(binaries.get(binaryNames[0]))}  docks-kit-renamed`;
  if (checksumMutation === 'digest') checksumRows[0] = `${DIGEST('f')}  ${binaryNames[0]}`;
  const checksumBytes = Buffer.from(`${checksumRows.join('\n')}\n`);
  const bytesByName = new Map([...binaries, ['SHA256SUMS', checksumBytes]]);
  let releaseAssets = PUBLIC_RELEASE_ASSET_NAMES.map((name, index) => {
    const bytes = bytesByName.get(name);
    return { id: 1000 + index, name, size: bytes.length, digest: `sha256:${hash(bytes)}` };
  });
  if (extraAsset !== null) {
    const bytes = Buffer.from(`public-release:${extraAsset}`);
    bytesByName.set(extraAsset, bytes);
    releaseAssets.push({
      id: 1000 + releaseAssets.length,
      name: extraAsset,
      size: bytes.length,
      digest: `sha256:${hash(bytes)}`,
    });
  }
  if (removeAsset !== null) releaseAssets = releaseAssets.filter(({ name }) => name !== removeAsset);
  const release = {
    id: 901,
    tag_name: PUBLIC_TAG,
    draft: false,
    prerelease: false,
    assets: releaseAssets,
  };
  const successfulRun = {
    id: 801,
    run_attempt: 1,
    head_sha: PUBLIC_RELEASE_COMMIT,
    head_branch: PUBLIC_TAG,
    path: '.github/workflows/release-cli.yml',
    event: 'push',
    status: 'completed',
    conclusion: 'success',
  };
  const selectedRuns = runs ?? [successfulRun];
  const finishedPlan = Buffer.from(
    [
      '---',
      'status: finished',
      `review_status: ${reviewStatus}`,
      '---',
      '',
      '# Public release',
      '',
      `Completion-review-receipt: ${completionReceiptText}`,
      '',
    ].join('\n'),
  );
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
    // The legacy V2 finished plan carries no `Plan-run:` line, so the PlanRun branch that consults
    // the released-content pin must never be reached from here.
    expectedImplementationContent: () =>
      assert.fail('legacy public release path must not consult the released-content pin'),
  };
  return { adapter, completionDigest: hash(completionReceiptText), successfulRun };
}

function verifyPublicBoundary(
  directory,
  boundary,
  adapter,
  {
    output = 'public-release.json',
    completionDigest = adapter.completionDigest,
    finishedPlanPath = PUBLIC_FINISHED_PLAN_PATH,
  } = {},
) {
  return captureDigest(() =>
    releasePromotion.verifyPublicRelease(
      new Map([
        ['request', boundary.request.file],
        ['request-sha256', boundary.request.digest],
        ['publication', boundary.publicationFile.file],
        ['publication-sha256', boundary.publicationFile.digest],
        ['public-finished-plan', finishedPlanPath],
        ['public-release-commit', PUBLIC_RELEASE_COMMIT],
        ['public-plan-commit', PUBLIC_PLAN_COMMIT],
        ['public-completion-sha256', completionDigest],
        ['receipt-out', path.join(directory, output)],
      ]),
      adapter.adapter ?? adapter,
    ),
  );
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
    assert.deepEqual(
      verified.receipt.release.assets.map(({ name }) => name),
      PUBLIC_RELEASE_ASSET_NAMES,
    );
    assert.equal(fs.readFileSync(path.join(directory, 'public-release.json'), 'utf8'), canonicalize(verified.receipt));

    const cases = [
      [
        'tag commit mismatch',
        makePublicReleaseAdapter(boundary.request, {
          tagCommit: differentHex(boundary.request.value.session_relay.tag_commit),
        }),
        {},
        /tag.*commit|release commit/i,
      ],
      [
        'broken plan ancestry',
        makePublicReleaseAdapter(boundary.request, { planAncestry: false }),
        {},
        /finished-plan.*ancestry|plan.*ancestry/i,
      ],
      [
        'broken companion ancestry',
        makePublicReleaseAdapter(boundary.request, { releaseAncestry: false }),
        {},
        /ancestor|ancestry/i,
      ],
      [
        'missing successful workflow run',
        makePublicReleaseAdapter(boundary.request, { runs: [] }),
        {},
        /workflow.*run|successful/i,
      ],
      [
        'duplicate successful workflow run',
        (() => {
          const value = makePublicReleaseAdapter(boundary.request);
          return makePublicReleaseAdapter(boundary.request, {
            runs: [value.successfulRun, { ...value.successfulRun, id: 802 }],
          });
        })(),
        {},
        /workflow.*run|duplicate|exactly one/i,
      ],
      [
        'wrong five-asset set',
        makePublicReleaseAdapter(boundary.request, { removeAsset: 'docks-kit-linux-x64' }),
        {},
        /asset set|five|assets/i,
      ],
      [
        'present Windows asset',
        makePublicReleaseAdapter(boundary.request, { extraAsset: 'docks-kit-windows-x64.exe' }),
        {},
        /asset set|five|assets/i,
      ],
      [
        'extra asset',
        makePublicReleaseAdapter(boundary.request, { extraAsset: 'docks-kit-extra-x64' }),
        {},
        /asset set|five|assets/i,
      ],
      [
        'missing checksum row',
        makePublicReleaseAdapter(boundary.request, { checksumMutation: 'missing' }),
        {},
        /checksum|row/i,
      ],
      [
        'extra checksum row',
        makePublicReleaseAdapter(boundary.request, { checksumMutation: 'extra' }),
        {},
        /checksum|row/i,
      ],
      [
        'checksum name conflict',
        makePublicReleaseAdapter(boundary.request, { checksumMutation: 'name' }),
        {},
        /checksum|identity/i,
      ],
      [
        'checksum digest conflict',
        makePublicReleaseAdapter(boundary.request, { checksumMutation: 'digest' }),
        {},
        /checksum|digest/i,
      ],
      [
        'digest-pin mismatch',
        makePublicReleaseAdapter(boundary.request, {
          pinnedAssets: {
            ...boundary.request.value.assets,
            'x86_64-unknown-linux-musl': differentHex(boundary.request.value.assets['x86_64-unknown-linux-musl']),
          },
        }),
        {},
        /pinned|digest/i,
      ],
      [
        'completion line hash mismatch',
        makePublicReleaseAdapter(boundary.request),
        { completionDigest: DIGEST('e') },
        /completion.*hash|digest/i,
      ],
      [
        'non-passed review_status',
        makePublicReleaseAdapter(boundary.request, { reviewStatus: 'failed' }),
        {},
        /review_status|passed/i,
      ],
      [
        'completion receipt missing field',
        makePublicReleaseAdapter(boundary.request, {
          completionReceipt: changedCompletionReceipt((receipt) => {
            delete receipt.reviewed_at;
          }),
        }),
        {},
        /completion receipt|missing|keys/i,
      ],
      [
        'completion receipt unknown field',
        makePublicReleaseAdapter(boundary.request, {
          completionReceipt: changedCompletionReceipt((receipt) => {
            receipt.unknown_field = true;
          }),
        }),
        {},
        /completion receipt|unknown|keys/i,
      ],
      [
        'completion receipt wrong phase',
        makePublicReleaseAdapter(boundary.request, {
          completionReceipt: changedCompletionReceipt((receipt) => {
            receipt.phase = 'draft';
          }),
        }),
        {},
        /completion receipt|phase/i,
      ],
      [
        'completion receipt wrong outcome',
        makePublicReleaseAdapter(boundary.request, {
          completionReceipt: changedCompletionReceipt((receipt) => {
            receipt.outcome = 'not_ready';
          }),
        }),
        {},
        /completion receipt|outcome|series/i,
      ],
      [
        'completion receipt wrong reviewed_head',
        makePublicReleaseAdapter(boundary.request, {
          completionReceipt: completionReceiptForReviewedHead('d'.repeat(40)),
        }),
        {},
        /completion receipt|reviewed_head|stale/i,
      ],
      [
        'wrong finished-plan slug',
        makePublicReleaseAdapter(boundary.request),
        { finishedPlanPath: 'docs/plans/finished/2026-07-23-session-relay-cli-0.13.1-production-release.md' },
        /finished-plan|production-release|path/i,
      ],
      [
        'wrong finished-plan date',
        makePublicReleaseAdapter(boundary.request),
        { finishedPlanPath: 'docs/plans/finished/not-a-date-session-relay-cli-0.13.0-production-release.md' },
        /finished-plan|production-release|path/i,
      ],
    ];
    for (const [name, adapter, overrides, pattern] of cases) {
      assert.throws(
        () =>
          verifyPublicBoundary(directory, boundary, adapter, {
            output: `${name.replaceAll(' ', '-')}.json`,
            ...overrides,
          }),
        pattern,
        `verify-public-release ${name}`,
      );
    }
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

const CURRENT_PUBLIC_SOURCE_BASE = 'd'.repeat(40);
const CURRENT_PUBLIC_EXECUTION_PARENT = 'b'.repeat(40);
// A PlanRun run's implementation commit is deliberately later than the tag it publishes, so this
// fixture value must differ from the publication's tag commit.
const CURRENT_DOCKS_IMPLEMENTATION_FIXTURE_COMMIT = '9'.repeat(40);
const CURRENT_PUBLIC_RED_COMMIT = 'c'.repeat(40);
const CURRENT_PUBLIC_IMPLEMENTATION_COMMIT = 'd'.repeat(40);
const CURRENT_PUBLIC_RELEASE_COMMIT = 'e'.repeat(40);
const CURRENT_PUBLIC_ARCHIVE_COMMIT = 'f'.repeat(40);
const CURRENT_PUBLIC_RELEASED_AT = '2026-07-25T17:00:00.000Z';

function currentBoundaryPublicationValue() {
  const tagCommit = 'a'.repeat(40);
  const relayAssets = [...CURRENT_ORDINARY_ASSET_NAMES, 'SHA256SUMS'].sort().map((name, index) => ({
    name,
    database_id: 4_000 + index,
    size: 40_000 + index,
    digest: String(index + 1)
      .repeat(64)
      .slice(0, 64),
  }));
  const ordinaryDigests = Object.fromEntries(
    relayAssets.filter(({ name }) => name !== 'SHA256SUMS').map(({ name, digest }) => [name, digest]),
  );
  return {
    schema: 2,
    type: 'SessionRelayPublicationReceiptV2',
    repository_id: 'DocksDocks/docks',
    version: CURRENT_RELEASE_VERSION,
    source_proof_sha256: DIGEST('b'),
    source: {
      reviewed_commit: tagCommit,
      implementation_commit: tagCommit,
      reviewed_ancestry_verified: true,
    },
    tag: CURRENT_RELEASE_TAG,
    tag_commit: tagCommit,
    workflow: {
      file: '.github/workflows/build-binaries.yml',
      workflow_sha: tagCommit,
      run_id: 4_100,
      attempt: 1,
      head_sha: tagCommit,
      path: '.github/workflows/build-binaries.yml',
      event: 'push',
      inputs: { expected_commit: '', expected_tag: '', mode: '' },
      conclusion: 'success',
    },
    release_database_id: 4_200,
    release_state: 'prerelease',
    body_sha256: hash(PRERELEASE_BODY),
    assets: relayAssets,
    digest_evidence: {
      workflow_run_id: 4_100,
      workflow_run_attempt: 1,
      artifact_sha256: ordinaryDigests,
      release_download_sha256: structuredClone(ordinaryDigests),
      checksum_rows: structuredClone(ordinaryDigests),
    },
    public_companion: {
      repository_id: 'DocksDocks/public',
      version: CURRENT_PUBLIC_VERSION,
      tag: CURRENT_PUBLIC_TAG,
      package: 'docks-kit',
      npm_version: CURRENT_PUBLIC_VERSION,
    },
    historical_predecessor: {
      version: RELEASE_VERSION,
      tag: RELEASE_TAG,
      publication_receipt_sha256: HISTORICAL_PUBLICATION_SHA256,
    },
    transition: 'tag_and_release_created',
    created_at: '2026-07-25T15:00:00.000Z',
  };
}

function currentPublicBoundaryFixture(directory) {
  const publicationFile = writeBoundaryValue(directory, 'current-publication.json', currentBoundaryPublicationValue());
  const publicationAssets = new Map(publicationFile.value.assets.map((asset) => [asset.name, asset]));
  const pinnedAssets = Object.fromEntries(
    CURRENT_PUBLIC_ASSET_TARGETS.map((target) => [target, publicationAssets.get(`session-relay-${target}`).digest]),
  );
  const request = writeBoundaryValue(directory, 'current-public-request.json', {
    schema: 1,
    type: 'PublicReleaseRequestV1',
    repository_id: 'DocksDocks/public',
    tag: CURRENT_PUBLIC_TAG,
    version: CURRENT_PUBLIC_VERSION,
    companion_base_commit: CURRENT_PUBLIC_EXECUTION_PARENT,
    session_relay: {
      repository_id: 'DocksDocks/docks',
      tag: CURRENT_RELEASE_TAG,
      version: CURRENT_RELEASE_VERSION,
      tag_commit: publicationFile.value.tag_commit,
      publication_receipt_sha256: publicationFile.digest,
    },
    assets: pinnedAssets,
    created_at: '2026-07-25T15:15:00.000Z',
  });
  return { publicationFile, request };
}

function currentPublicReleaseEvidenceFixture(completionDigest) {
  const { ancestry, public_plan: publicPlan } = currentReleaseChainV2().publicRelease.value;
  return {
    schema: 1,
    type: 'PublicReleaseEvidenceV1',
    ancestry: structuredClone(ancestry),
    public_plan: {
      ...structuredClone(publicPlan),
      completion_review_sha256: completionDigest,
    },
  };
}
function currentPlanRunPublicFixture(timestamps = {}) {
  const activePlanPath = `docs/plans/active/session-relay-${CURRENT_RELEASE_VERSION}-docks-kit-${CURRENT_PUBLIC_VERSION}-release.md`;
  const createdAt = timestamps.created ?? '2026-07-25T12:00:00.000+00:00';
  const updatedAt = timestamps.updated ?? '2026-07-26T01:00:00.000+00:00';
  const startedAt = timestamps.started_at ?? '2026-07-25T13:00:00.000+00:00';
  const finishedAt = timestamps.finished_at ?? '2026-07-26T01:00:00.000+00:00';
  const fileBytes = new Map([
    ['SoT/toolchain.json', Buffer.from(`{"tools":{"session-relay":{"verified":"${CURRENT_RELEASE_VERSION}"}}}\n`)],
    ['package.json', Buffer.from(`{"name":"docks-kit","version":"${CURRENT_PUBLIC_VERSION}"}\n`)],
  ]);
  const affectedPaths = [...fileBytes.keys()].sort();
  const manifestPaths = affectedPaths.map((filePath) => ({
    path: filePath,
    state: 'file',
    kind: 'file',
    mode: 0o664,
    sha256: hash(fileBytes.get(filePath)),
  }));
  const sourceSha256 = hash({
    schema: 1,
    source_base: CURRENT_PUBLIC_IMPLEMENTATION_COMMIT,
    paths: manifestPaths,
  });
  const completionDigest = DIGEST('8');
  const run = {
    acceptance: {
      source_sha256: sourceSha256,
      verification_sha256: DIGEST('0'),
    },
    blocker: null,
    completion_review: {
      input_sha256: DIGEST('7'),
      invocations: 2,
      result_sha256: completionDigest,
      state: 'passed',
    },
    draft_review: {
      input_sha256: DIGEST('6'),
      invocations: 2,
      result_sha256: DIGEST('5'),
      state: 'passed',
    },
    execution_parent: CURRENT_PUBLIC_EXECUTION_PARENT,
    goal_id: CURRENT_GOAL_ID,
    implementation_commit: CURRENT_PUBLIC_IMPLEMENTATION_COMMIT,
    plan_path: activePlanPath,
    plan_sha256: DIGEST('0'),
    repository_id: 'DocksDocks/public',
    requested_effects: ['local', 'probe', 'publish', 'push', 'release'],
    risk: 'external',
    run_id: CURRENT_PUBLIC_RUN_ID,
    schema: 1,
    source_base: CURRENT_PUBLIC_SOURCE_BASE,
    source_sha256: DIGEST('4'),
  };
  const render = () =>
    Buffer.from(
      [
        '---',
        'title: Current public PlanRun fixture',
        'status: finished',
        `created: ${JSON.stringify(createdAt)}`,
        `updated: ${JSON.stringify(updatedAt)}`,
        `started_at: ${JSON.stringify(startedAt)}`,
        `finished_at: ${JSON.stringify(finishedAt)}`,
        'assignee: null',
        'tags: [session-relay, release]',
        'affected_paths:',
        ...affectedPaths.map((filePath) => `  - ${filePath}`),
        'related_plans: []',
        '---',
        '',
        '# Current public PlanRun fixture',
        '',
        `Plan-run: ${canonicalize(run)}`,
        '',
        '## Goal',
        '',
        'Release the current public companion.',
        '',
        '## Review',
        '',
        'Passed.',
        '',
        '## Verification Results',
        '',
        '- Current public PlanRun fixture verified.',
        '',
      ].join('\n'),
    );
  run.acceptance.verification_sha256 = hash(canonicalVerificationResults(render()));
  run.plan_sha256 = hash(canonicalPlanView(render()));
  return {
    completionDigest,
    fileBytes,
    planBytes: render(),
    run: structuredClone(run),
  };
}

function makeCurrentPublicReleaseAdapter(
  request,
  {
    ancestryFailure = null,
    contentPinOverride = null,
    evidenceCopies = 1,
    evidenceMutation = null,
    npmState = 'published',
    planRun = false,
    planRunTimestamps = {},
    publishedAt = CURRENT_PUBLIC_RELEASED_AT,
  } = {},
) {
  const planRunFixture = planRun ? currentPlanRunPublicFixture(planRunTimestamps) : null;
  const completionReceipt = completionReceiptForReviewedHead(CURRENT_PUBLIC_IMPLEMENTATION_COMMIT);
  const completionReceiptText = canonicalize(completionReceipt);
  const completionDigest = planRunFixture?.completionDigest ?? hash(completionReceiptText);
  const evidence = currentPublicReleaseEvidenceFixture(completionDigest);
  evidenceMutation?.(evidence);
  const evidenceText = canonicalize(evidence);

  const binaryNames = PUBLIC_RELEASE_ASSET_NAMES.filter((name) => name !== 'SHA256SUMS');
  const binaries = new Map(binaryNames.map((name) => [name, Buffer.from(`current-public-release:${name}`)]));
  const checksumBytes = Buffer.from(
    `${binaryNames.map((name) => `${hash(binaries.get(name))}  ${name}`).join('\n')}\n`,
  );
  const bytesByName = new Map([...binaries, ['SHA256SUMS', checksumBytes]]);
  const releaseAssets = PUBLIC_RELEASE_ASSET_NAMES.map((name, index) => {
    const bytes = bytesByName.get(name);
    return { id: 5_000 + index, name, size: bytes.length, digest: `sha256:${hash(bytes)}` };
  });
  const release = {
    id: 5_100,
    tag_name: CURRENT_PUBLIC_TAG,
    draft: false,
    prerelease: false,
    published_at: publishedAt,
    assets: releaseAssets,
  };
  const successfulRun = {
    id: 5_200,
    run_attempt: 1,
    head_sha: CURRENT_PUBLIC_RELEASE_COMMIT,
    head_branch: CURRENT_PUBLIC_TAG,
    path: '.github/workflows/release-cli.yml',
    event: 'push',
    status: 'completed',
    conclusion: 'success',
  };
  const finishedPlan =
    planRunFixture?.planBytes ??
    Buffer.from(
      [
        '---',
        'status: finished',
        'review_status: passed',
        '---',
        '',
        '# Public release',
        '',
        `Completion-review-receipt: ${completionReceiptText}`,
        ...Array.from({ length: evidenceCopies }, () => `Public-release-evidence: ${evidenceText}`),
        '',
      ].join('\n'),
    );
  const ancestryFlags = new Map([
    [`${CURRENT_PUBLIC_RED_COMMIT}...${CURRENT_PUBLIC_IMPLEMENTATION_COMMIT}`, 'red_to_implementation'],
    [`${CURRENT_PUBLIC_SOURCE_BASE}...${CURRENT_PUBLIC_IMPLEMENTATION_COMMIT}`, 'source_base_to_implementation'],
    [
      `${CURRENT_PUBLIC_EXECUTION_PARENT}...${CURRENT_PUBLIC_IMPLEMENTATION_COMMIT}`,
      'execution_parent_to_implementation',
    ],
    [`${CURRENT_PUBLIC_IMPLEMENTATION_COMMIT}...${CURRENT_PUBLIC_RELEASE_COMMIT}`, 'implementation_to_release'],
    [`${CURRENT_PUBLIC_RELEASE_COMMIT}...${CURRENT_PUBLIC_ARCHIVE_COMMIT}`, 'release_to_archive'],
  ]);
  const expectedContentDigest =
    contentPinOverride ??
    (planRunFixture === null
      ? DIGEST('c')
      : hash({
          schema: 1,
          source_base: CURRENT_PUBLIC_IMPLEMENTATION_COMMIT,
          paths: [...planRunFixture.fileBytes.keys()]
            .sort()
            .map((filePath) => ({ path: filePath, sha256: hash(planRunFixture.fileBytes.get(filePath)) })),
        }));
  const adapter = {
    now: () => '2026-07-25T18:00:00.000Z',
    getTagCommit: () => CURRENT_PUBLIC_RELEASE_COMMIT,
    isAncestor: (ancestor, descendant) => {
      const flag = ancestryFlags.get(`${ancestor}...${descendant}`);
      assert.ok(flag, `unexpected current public ancestry check: ${ancestor}...${descendant}`);
      return flag !== ancestryFailure;
    },
    getFinishedPlan: (commit, planPath) => {
      if (planRunFixture && commit === CURRENT_PUBLIC_IMPLEMENTATION_COMMIT) {
        const bytes = planRunFixture.fileBytes.get(planPath);
        assert.ok(bytes, `unexpected current public implementation path: ${planPath}`);
        return Buffer.from(bytes);
      }
      assert.equal(commit, CURRENT_PUBLIC_ARCHIVE_COMMIT, 'current finished plan must be read from archive commit');
      assert.equal(planPath, CURRENT_PUBLIC_FINISHED_PLAN_PATH, 'current finished plan path identity');
      return finishedPlan;
    },
    listWorkflowRuns: () => [structuredClone(successfulRun)],
    getRelease: () => structuredClone(release),
    downloadReleaseAsset: (id) => {
      const asset = releaseAssets.find((candidate) => candidate.id === id);
      assert.ok(asset, `unexpected current public asset download ${id}`);
      return Buffer.from(bytesByName.get(asset.name));
    },
    getPinnedAssets: (commit) => {
      assert.equal(commit, CURRENT_PUBLIC_RELEASE_COMMIT, 'current Relay pins must be read from release commit');
      return structuredClone(request.value.assets);
    },
    getNpmState: (run) => {
      assert.equal(run.id, successfulRun.id, 'npm docks-kit@0.12.0 must bind the exact successful workflow');
      return npmState;
    },
    // Snapshot at construction, exactly like the reviewed instance value it stands in for. A lazy
    // derivation would drift together with the implementation bytes and could never fail, which is
    // precisely the vacuous shape the refusal cases below exist to rule out.
    expectedImplementationContent: () => expectedContentDigest,
  };
  return { adapter, completionDigest, evidence, planRunFixture };
}

function verifyCurrentPublicBoundary(directory, boundary, observation, output) {
  return captureDigest(() =>
    releasePromotion.verifyPublicRelease(
      new Map([
        ['request', boundary.request.file],
        ['request-sha256', boundary.request.digest],
        ['publication', boundary.publicationFile.file],
        ['publication-sha256', boundary.publicationFile.digest],
        ['public-finished-plan', CURRENT_PUBLIC_FINISHED_PLAN_PATH],
        ['public-release-commit', CURRENT_PUBLIC_RELEASE_COMMIT],
        ['public-plan-commit', CURRENT_PUBLIC_ARCHIVE_COMMIT],
        ['public-completion-sha256', observation.completionDigest],
        ['receipt-out', path.join(directory, output)],
      ]),
      observation.adapter,
    ),
  );
}

{
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-current-public-boundary-'));
  try {
    const boundary = currentPublicBoundaryFixture(directory);

    // A PlanRun publication records an implementation commit LATER than the tag it publishes, so
    // `reviewed_commit === implementation_commit === tag_commit` can never hold for it. The
    // standalone validators carry no source proof and must classify the receipt from its own shape;
    // when that classification was missing, every PlanRun publication fell down the legacy branch
    // and `emit-public-request` refused a receipt its own `publish-reviewed` had just emitted.
    // Both directions are asserted so neither the classification nor the binding can be dropped.
    {
      const planRunValue = currentBoundaryPublicationValue();
      planRunValue.source = {
        ...planRunValue.source,
        implementation_commit: CURRENT_DOCKS_IMPLEMENTATION_FIXTURE_COMMIT,
      };
      const planRunPublication = writeBoundaryValue(directory, 'planrun-publication.json', planRunValue);
      const emitted = captureDigest(() =>
        releasePromotion.emitPublicRequest(
          new Map([
            ['publication', planRunPublication.file],
            ['publication-sha256', planRunPublication.digest],
            ['public-execution-parent', CURRENT_PUBLIC_EXECUTION_PARENT],
            ['receipt-out', path.join(directory, 'planrun-public-request.json')],
          ]),
        ),
      );
      assert.equal(
        emitted.receipt.session_relay.publication_receipt_sha256,
        planRunPublication.digest,
        'emit-public-request must accept a PlanRun publication whose implementation commit follows the tag',
      );

      const skewedValue = currentBoundaryPublicationValue();
      skewedValue.source = {
        ...skewedValue.source,
        reviewed_commit: differentHex(skewedValue.tag_commit),
        implementation_commit: CURRENT_DOCKS_IMPLEMENTATION_FIXTURE_COMMIT,
      };
      const skewed = writeBoundaryValue(directory, 'skewed-publication.json', skewedValue);
      assert.throws(
        () =>
          releasePromotion.emitPublicRequest(
            new Map([
              ['publication', skewed.file],
              ['publication-sha256', skewed.digest],
              ['public-execution-parent', CURRENT_PUBLIC_EXECUTION_PARENT],
              ['receipt-out', path.join(directory, 'skewed-public-request.json')],
            ]),
          ),
        /reviewed source commit ancestry or tag identity conflict/,
        'a PlanRun publication whose reviewed commit is not the tag commit must still be refused',
      );
    }
    const success = makeCurrentPublicReleaseAdapter(boundary.request);
    const verified = verifyCurrentPublicBoundary(directory, boundary, success, 'current-public-release.json');
    assert.equal(verified.receipt.schema, 2, 'current verifier must emit schema 2');
    assert.equal(verified.receipt.type, 'PublicReleaseReceiptV2', 'current verifier must emit PublicReleaseReceiptV2');
    assert.equal(verified.receipt.request_sha256, boundary.request.digest);
    assert.equal(verified.receipt.created_at, CURRENT_PUBLIC_RELEASED_AT);
    assert.deepEqual(verified.receipt.ancestry, success.evidence.ancestry);
    assert.deepEqual(verified.receipt.public_plan, success.evidence.public_plan);
    assert.deepEqual(verified.receipt.npm, {
      package: 'docks-kit',
      state: 'published',
      version: CURRENT_PUBLIC_VERSION,
    });
    assert.deepEqual(Object.keys(success.evidence).sort(), ['ancestry', 'public_plan', 'schema', 'type']);
    assert.deepEqual(Object.keys(success.evidence.ancestry).sort(), [
      'archive_commit',
      'execution_parent',
      'execution_parent_to_implementation',
      'implementation_commit',
      'implementation_reviewed_at',
      'implementation_to_release',
      'red_captured_at',
      'red_pre_production_commit',
      'red_to_implementation',
      'release_commit',
      'release_to_archive',
      'reviewed_commit',
    ]);
    assert.deepEqual(Object.keys(success.evidence.public_plan).sort(), [
      'acceptance_sha256',
      'archive_commit',
      'completion_review_sha256',
      'finished_at',
      'goal_id',
      'implementation_commit',
      'path',
      'red_evidence',
      'red_pre_production_commit',
      'red_receipt_sha256',
      'release_commit',
      'remote_read_back',
      'repository_id',
      'run_id',
      'schema',
      'status',
      'verification_sha256',
    ]);
    assert.equal(
      fs.readFileSync(path.join(directory, 'current-public-release.json'), 'utf8'),
      canonicalize(verified.receipt),
    );
    const planRunSuccess = makeCurrentPublicReleaseAdapter(boundary.request, { planRun: true });
    const planRunVerified = verifyCurrentPublicBoundary(
      directory,
      boundary,
      planRunSuccess,
      'current-public-planrun-release.json',
    );
    assert.equal(planRunVerified.receipt.schema, 3, 'PlanRun verifier must emit schema 3');
    assert.equal(
      planRunVerified.receipt.type,
      'PublicReleaseReceiptV3',
      'PlanRun verifier must emit PublicReleaseReceiptV3',
    );
    assert.deepEqual(planRunVerified.receipt.public_plan.plan_run, planRunSuccess.planRunFixture.run);
    assert.equal(
      planRunVerified.receipt.public_plan.plan_run.completion_review.result_sha256,
      planRunSuccess.completionDigest,
    );
    assert.equal(
      planRunVerified.receipt.public_plan.plan_run.acceptance.source_sha256,
      planRunSuccess.planRunFixture.run.acceptance.source_sha256,
    );
    assert.equal(
      planRunVerified.receipt.public_plan.finished_path,
      CURRENT_PUBLIC_FINISHED_PLAN_PATH,
      'PlanRun receipt must bind the observed finished archive path',
    );
    assert.throws(
      () =>
        verifyCurrentPublicBoundary(
          directory,
          boundary,
          makeCurrentPublicReleaseAdapter(boundary.request, {
            planRun: true,
            ancestryFailure: 'source_base_to_implementation',
          }),
          'unrelated-public-source-base.json',
        ),
      /source-base-to-implementation ancestry was not independently observed/i,
      'current verify-public-release must refuse an unrelated child PlanRun source base',
    );
    verifyCurrentPublicBoundary(
      directory,
      boundary,
      makeCurrentPublicReleaseAdapter(boundary.request, {
        planRun: true,
        planRunTimestamps: { finished_at: '2026-07-26T01:00:00.000Z' },
      }),
      'current-public-planrun-z-time.json',
    );
    for (const [name, finishedAt] of [
      ['negative-offset', '2026-07-26T01:00:00.000-03:00'],
      ['positive-offset', '2026-07-26T01:00:00.000+01:00'],
      ['missing-milliseconds', '2026-07-26T01:00:00+00:00'],
      ['impossible-date', '2026-02-30T01:00:00.000+00:00'],
      ['non-string', 42],
    ]) {
      assert.throws(
        () =>
          verifyCurrentPublicBoundary(
            directory,
            boundary,
            makeCurrentPublicReleaseAdapter(boundary.request, {
              planRun: true,
              planRunTimestamps: { finished_at: finishedAt },
            }),
            `invalid-current-public-planrun-${name}.json`,
          ),
        /exact RFC3339 UTC timestamp/,
        `current public PlanRun must reject ${name} timestamps`,
      );
    }

    // Non-vacuity for the released-content pin. The positive case above derives the expectation
    // from the fixture's own implementation bytes, so on its own it could pass while comparing
    // nothing. Feed a pin the observed bytes cannot produce and require refusal; then feed the
    // right pin with one implementation byte changed and require refusal again, so neither side of
    // the comparison can be dropped without a test going red.
    assert.throws(
      () =>
        verifyCurrentPublicBoundary(
          directory,
          boundary,
          makeCurrentPublicReleaseAdapter(boundary.request, {
            planRun: true,
            contentPinOverride: DIGEST('b'),
          }),
          'wrong-released-content-pin.json',
        ),
      /released affected-path content does not match the reviewed digest/,
      'current verify-public-release must refuse a released-content digest it did not observe',
    );
    const driftedContent = makeCurrentPublicReleaseAdapter(boundary.request, { planRun: true });
    const driftedPath = [...driftedContent.planRunFixture.fileBytes.keys()].sort()[0];
    driftedContent.planRunFixture.fileBytes.set(driftedPath, Buffer.from('drifted release content\n'));
    assert.throws(
      () => verifyCurrentPublicBoundary(directory, boundary, driftedContent, 'drifted-released-content.json'),
      /released affected-path content does not match the reviewed digest/,
      'current verify-public-release must refuse implementation bytes that drifted from the reviewed digest',
    );

    for (const [name, observation, pattern] of [
      [
        'wrong red-to-implementation order',
        makeCurrentPublicReleaseAdapter(boundary.request, { ancestryFailure: 'red_to_implementation' }),
        /ancestry|order|red-to-implementation/i,
      ],
      [
        'wrong completion hash',
        makeCurrentPublicReleaseAdapter(boundary.request, {
          evidenceMutation: (evidence) => {
            evidence.public_plan.completion_review_sha256 = differentHex(evidence.public_plan.completion_review_sha256);
          },
        }),
        /completion.*hash|digest/i,
      ],
      [
        'wrong public plan identity',
        makeCurrentPublicReleaseAdapter(boundary.request, {
          evidenceMutation: (evidence) => {
            evidence.public_plan.run_id = differentHex(evidence.public_plan.run_id);
          },
        }),
        /public|plan|identity|archive/i,
      ],
      [
        'duplicate public release evidence',
        makeCurrentPublicReleaseAdapter(boundary.request, { evidenceCopies: 2 }),
        /exactly one|Public-release-evidence/i,
      ],
      [
        'wrong public release evidence type',
        makeCurrentPublicReleaseAdapter(boundary.request, {
          evidenceMutation: (evidence) => {
            evidence.type = 'PublicReleaseEvidenceV2';
          },
        }),
        /evidence.*schema|type/i,
      ],
    ]) {
      assert.throws(
        () => verifyCurrentPublicBoundary(directory, boundary, observation, `${name.replaceAll(' ', '-')}.json`),
        pattern,
        `current verify-public-release ${name}`,
      );
    }
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

function currentPromotionProofV2() {
  const sourceCommit = RETAINED_V2_INSTANCE.current_attempt.docks_source_base;
  const redCommit = '2'.repeat(40);
  const implementationCommit = 'a'.repeat(40);
  const proofPath = 'plugins/session-relay/test/release-promotion-contract.mjs';
  const manifestPaths = [{ path: proofPath, state: 'missing', kind: null, mode: null, sha256: null }];
  const manifest = {
    schema: 1,
    source_base: implementationCommit,
    source_sha256: hash({ schema: 1, source_base: implementationCommit, paths: manifestPaths }),
    paths: manifestPaths,
  };
  return {
    schema: 2,
    type: 'SourcePreparationProofV2',
    repository_id: 'DocksDocks/docks',
    version: RETAINED_V2_RELEASE_VERSION,
    tag: RETAINED_V2_RELEASE_TAG,
    goal_id: RETAINED_V2_INSTANCE.current_attempt.goal_id,
    run_id: RETAINED_V2_INSTANCE.current_attempt.docks_run_id,
    source_commit: sourceCommit,
    implementation_commit: implementationCommit,
    tag_commit: implementationCommit,
    plan_run: {
      schema: 1,
      repository_id: 'DocksDocks/docks',
      goal_id: RETAINED_V2_INSTANCE.current_attempt.goal_id,
      run_id: RETAINED_V2_INSTANCE.current_attempt.docks_run_id,
      plan_path: RETAINED_V2_INSTANCE.current_attempt.docks_plan_path,
      source_base: sourceCommit,
      implementation_commit: implementationCommit,
      status: 'ongoing',
    },
    tdd_red: {
      schema: 1,
      type: 'TddRedReceiptV1',
      repository_id: 'DocksDocks/docks',
      pre_production_commit: redCommit,
      test_paths: [{ path: proofPath, blob_id: '3'.repeat(40) }],
      command: {
        cwd: '/home/vagrant/projects/docks',
        argv: ['node', proofPath],
      },
      exit_code: 17,
      stdout_sha256: DIGEST('8'),
      stderr_sha256: DIGEST('9'),
      captured_at: '2026-07-25T13:55:00.000Z',
      producer: {
        path: 'scripts/capture-tdd-red.mjs',
        blob_id: '4'.repeat(40),
        version: '1',
      },
    },
    completion_review: {
      schema: 1,
      type: 'CompletionReviewV1',
      reviewed_commit: implementationCommit,
      diff_sha256: DIGEST('6'),
      result_sha256: DIGEST('7'),
      verdict: 'pass',
    },
    acceptance: {
      manifest,
      verification_sha256: DIGEST('5'),
      changed_paths: [proofPath],
    },
    ancestry: {
      source_to_red: true,
      red_to_implementation: true,
      implementation_to_reviewed: true,
      reviewed_to_tag: true,
    },
    companion: {
      repository_id: 'DocksDocks/public',
      goal_id: RETAINED_V2_INSTANCE.current_attempt.goal_id,
      run_id: RETAINED_V2_INSTANCE.current_attempt.public_run_id,
      plan_path: RETAINED_V2_PUBLIC_PLAN_PATH,
      version: RETAINED_V2_INSTANCE.public_child.version,
      tag: RETAINED_V2_INSTANCE.public_child.tag,
      session_relay_version: RETAINED_V2_RELEASE_VERSION,
      session_relay_tag: RETAINED_V2_RELEASE_TAG,
      package: 'docks-kit',
      npm_version: RETAINED_V2_INSTANCE.public_child.version,
    },
    historical_receipts: {
      version: RELEASE_VERSION,
      tag: RELEASE_TAG,
      source_proof_v1: '419b23ccdcf0ca21672e81c05ae9d22c55bc67781839ffb6a29e7eecc2b59396',
      source_proof_v2: '87a6260ae20280712ebb2d76d39667b128c8f6cf687141ebd779d8eca16c2262',
      publication: HISTORICAL_PUBLICATION_SHA256,
      public_request: HISTORICAL_PUBLIC_REQUEST_SHA256,
    },
    created_at: '2026-07-25T14:00:00.000Z',
  };
}

function currentPromotionProofV3() {
  const proof = currentPromotionProofV2();
  proof.schema = 3;
  proof.type = 'SourcePreparationProofV3';
  proof.version = CURRENT_RELEASE_VERSION;
  proof.tag = CURRENT_RELEASE_TAG;
  proof.goal_id = CURRENT_GOAL_ID;
  proof.run_id = PLANRUN_DOCKS_RUN_ID;
  proof.source_commit = PLANRUN_DOCKS_SOURCE_BASE;
  proof.tag_commit = PLANRUN_RELEASE_TAG_COMMIT;
  proof.plan_run = {
    ...proof.plan_run,
    repository_id: PLANRUN_DOCKS_REPOSITORY_ID,
    goal_id: CURRENT_GOAL_ID,
    run_id: PLANRUN_DOCKS_RUN_ID,
    plan_path: PLANRUN_DOCKS_PLAN_PATH,
    source_base: POST_TAG_PLANRUN_SOURCE_BASE,
  };
  proof.companion = {
    ...proof.companion,
    goal_id: CURRENT_GOAL_ID,
    run_id: CURRENT_PUBLIC_RUN_ID,
    plan_path: CURRENT_PUBLIC_PLAN_PATH,
    version: CURRENT_PUBLIC_VERSION,
    tag: CURRENT_PUBLIC_TAG,
    session_relay_version: CURRENT_RELEASE_VERSION,
    session_relay_tag: CURRENT_RELEASE_TAG,
    npm_version: CURRENT_PUBLIC_VERSION,
  };
  const manifestPaths = PLANRUN_DOCKS_AFFECTED_PATHS.map((affectedPath) => ({
    path: affectedPath,
    state: 'missing',
    kind: null,
    mode: null,
    sha256: null,
  }));
  proof.acceptance.manifest = {
    schema: 1,
    source_base: proof.implementation_commit,
    source_sha256: hash({
      schema: 1,
      source_base: proof.implementation_commit,
      paths: manifestPaths,
    }),
    paths: manifestPaths,
  };
  proof.acceptance.changed_paths = [...PLANRUN_DOCKS_AFFECTED_PATHS];
  delete proof.tdd_red;
  proof.ancestry = {
    source_to_tag: true,
    tag_to_implementation: true,
    implementation_to_reviewed: true,
  };
  return proof;
}

function makeCurrentPromotionAdapter({
  byteDrift = false,
  planRun = true,
  sourcePlanRun = true,
  refConflict = null,
  stableInitially = false,
  planSourceAncestry = true,
} = {}) {
  const proofValue = sourcePlanRun ? currentPromotionProofV3() : currentPromotionProofV2();
  const proofEnvelope = { value: proofValue, digest: hash(proofValue) };
  const publicationValue = currentBoundaryPublicationValue();
  publicationValue.source_proof_sha256 = proofEnvelope.digest;
  publicationValue.source.reviewed_commit = proofValue.tag_commit;
  publicationValue.source.implementation_commit = proofValue.implementation_commit;
  publicationValue.tag_commit = proofValue.tag_commit;
  publicationValue.workflow.workflow_sha = proofValue.tag_commit;
  publicationValue.workflow.head_sha = proofValue.tag_commit;
  const publicationEnvelope = { value: publicationValue, digest: hash(publicationValue) };
  const publicReleaseValue = structuredClone(currentReleaseChainV2().publicRelease.value);
  if (planRun) {
    const publicFixture = currentPlanRunPublicFixture();
    publicReleaseValue.schema = 3;
    publicReleaseValue.type = 'PublicReleaseReceiptV3';
    publicReleaseValue.ancestry = {
      execution_parent: CURRENT_PUBLIC_EXECUTION_PARENT,
      implementation_commit: CURRENT_PUBLIC_IMPLEMENTATION_COMMIT,
      release_commit: CURRENT_PUBLIC_RELEASE_COMMIT,
      archive_commit: CURRENT_PUBLIC_ARCHIVE_COMMIT,
      execution_parent_to_implementation: true,
      implementation_to_release: true,
      release_to_archive: true,
    };
    publicReleaseValue.public_plan = {
      plan_run: publicFixture.run,
      active_path: publicFixture.run.plan_path,
      finished_path: CURRENT_PUBLIC_FINISHED_PLAN_PATH,
      release_commit: CURRENT_PUBLIC_RELEASE_COMMIT,
      archive_commit: CURRENT_PUBLIC_ARCHIVE_COMMIT,
      remote_read_back: true,
      finished_at: '2026-07-26T01:36:05.859Z',
    };
  }
  publicReleaseValue.pinned_assets = Object.fromEntries(
    CURRENT_PUBLIC_ASSET_TARGETS.map((target) => [
      target,
      publicationValue.assets.find(({ name }) => name === `session-relay-${target}`).digest,
    ]),
  );
  const publicReleaseEnvelope = { value: publicReleaseValue, digest: hash(publicReleaseValue) };
  const prereleaseBody = PRERELEASE_BODY;
  const stableBody = STABLE_BODY;
  const state = {
    main: proofValue.implementation_commit,
    promotions: 0,
    ancestryCalls: [],
    outputs: new Map(),
    release: {
      id: publicationValue.release_database_id,
      tag_name: CURRENT_RELEASE_TAG,
      draft: false,
      prerelease: !stableInitially,
      body: stableInitially ? stableBody : prereleaseBody,
      assets: publicationValue.assets.map(({ name, database_id: databaseId, size, digest }) => ({
        id: databaseId,
        name,
        size,
        digest: `sha256:${digest}`,
      })),
    },
  };
  const adapter = {
    now: () => (planRun || sourcePlanRun ? '2026-07-26T02:00:00.000Z' : '2026-07-25T18:00:00.000Z'),
    loadProof: () => proofEnvelope,
    loadPublication: () => publicationEnvelope,
    loadPublicRelease: () => publicReleaseEnvelope,
    remoteRef: (ref) => {
      if (ref === 'refs/heads/main') return state.main;
      if (refConflict !== null && [CURRENT_TRANSACTION_REF, CURRENT_LOCK_REF].includes(ref)) {
        return refConflict;
      }
      return null;
    },
    isAncestor: (ancestor, descendant) => {
      state.ancestryCalls.push([ancestor, descendant]);
      return planSourceAncestry || ancestor !== POST_TAG_PLANRUN_SOURCE_BASE;
    },
    isPublicAncestor: () => true,
    currentReleaseState: () => ({
      commit: proofValue.tag_commit,
      release: structuredClone(state.release),
    }),
    promoteStable: ({ tag, releaseDatabaseId, body }) => {
      assert.equal(tag, CURRENT_RELEASE_TAG);
      assert.equal(releaseDatabaseId, publicationValue.release_database_id);
      assert.equal(body, stableBody);
      state.promotions += 1;
      state.release.prerelease = false;
      state.release.body = stableBody;
      if (byteDrift) state.release.assets[0].digest = `sha256:${DIGEST('f')}`;
    },
    assertReceiptOutputAvailable: ({ path: output }) => {
      if (state.outputs.has(output)) throw new Error('receipt output conflict');
    },
    writeReceipt: ({ path: output, receipt, allowExisting }) => {
      assert.equal(allowExisting, false);
      const bytes = canonicalize(receipt);
      if (state.outputs.has(output)) throw new Error('receipt output conflict');
      state.outputs.set(output, bytes);
      return { digest: hash(bytes) };
    },
  };
  assert.deepEqual(
    Object.keys(adapter).sort(),
    [...releasePromotion.CURRENT_PROMOTION_ADAPTER_KEYS].sort(),
    'current promotion adapter surface changed',
  );
  return {
    adapter,
    options: new Map([
      ['docks-kit-release', CURRENT_PUBLIC_TAG],
      ['expected-origin-main', proofValue.implementation_commit],
      ['receipt-out', '/receipts/current-promotion.json'],
    ]),
    publicationEnvelope,
    publicReleaseEnvelope,
    state,
  };
}

function makePromotionEvidenceRebindAdapter({
  proofMutation = null,
  publicationMutation = null,
  publicReleaseMutation = null,
  retainedMutation = null,
  liveMutation = null,
  workflowMutation = null,
  custodyConflict = false,
  reconciledAt = '2026-07-26T06:00:00.000Z',
} = {}) {
  const retainedPromotion = retainedPromotionV3();
  retainedMutation?.(retainedPromotion);

  const proofValue = currentPromotionProofV3();
  proofMutation?.(proofValue);
  const proofEnvelope = { value: proofValue, digest: hash(proofValue) };

  const publicationValue = currentBoundaryPublicationValue();
  publicationValue.source_proof_sha256 = proofEnvelope.digest;
  publicationValue.source.reviewed_commit = proofValue.tag_commit;
  publicationValue.source.implementation_commit = proofValue.implementation_commit;
  publicationValue.tag_commit = proofValue.tag_commit;
  publicationValue.workflow.workflow_sha = proofValue.tag_commit;
  publicationValue.workflow.head_sha = proofValue.tag_commit;
  publicationValue.workflow.run_id = retainedPromotion.stable_release.workflow_run_id;
  publicationValue.workflow.attempt = retainedPromotion.stable_release.workflow_run_attempt;
  publicationValue.release_database_id = retainedPromotion.stable_release.release_database_id;
  publicationValue.assets = structuredClone(retainedPromotion.stable_release.assets);
  const ordinaryDigestPins = Object.fromEntries(
    publicationValue.assets.filter(({ name }) => name !== 'SHA256SUMS').map(({ name, digest }) => [name, digest]),
  );
  publicationValue.digest_evidence = {
    workflow_run_id: publicationValue.workflow.run_id,
    workflow_run_attempt: publicationValue.workflow.attempt,
    artifact_sha256: structuredClone(ordinaryDigestPins),
    release_download_sha256: structuredClone(ordinaryDigestPins),
    checksum_rows: structuredClone(ordinaryDigestPins),
  };
  publicationValue.transition = 'tag_and_release_created';
  publicationValue.created_at = '2026-07-25T14:00:00.000Z';
  publicationMutation?.(publicationValue);
  const publicationEnvelope = { value: publicationValue, digest: hash(publicationValue) };

  const publicFixture = currentPlanRunPublicFixture();
  const publicReleaseValue = structuredClone(currentReleaseChainV2().publicRelease.value);
  publicReleaseValue.schema = 3;
  publicReleaseValue.type = 'PublicReleaseReceiptV3';
  publicReleaseValue.ancestry = {
    execution_parent: CURRENT_PUBLIC_EXECUTION_PARENT,
    implementation_commit: CURRENT_PUBLIC_IMPLEMENTATION_COMMIT,
    release_commit: CURRENT_PUBLIC_RELEASE_COMMIT,
    archive_commit: CURRENT_PUBLIC_ARCHIVE_COMMIT,
    execution_parent_to_implementation: true,
    implementation_to_release: true,
    release_to_archive: true,
  };
  publicReleaseValue.public_plan = {
    plan_run: publicFixture.run,
    active_path: publicFixture.run.plan_path,
    finished_path: CURRENT_PUBLIC_FINISHED_PLAN_PATH,
    release_commit: CURRENT_PUBLIC_RELEASE_COMMIT,
    archive_commit: CURRENT_PUBLIC_ARCHIVE_COMMIT,
    remote_read_back: true,
    finished_at: retainedPromotion.public_child.finished_at,
  };
  publicReleaseValue.pinned_assets = Object.fromEntries(
    CURRENT_PUBLIC_ASSET_TARGETS.map((target) => [
      target,
      publicationValue.assets.find(({ name }) => name === `session-relay-${target}`).digest,
    ]),
  );
  publicReleaseMutation?.(publicReleaseValue);
  const publicReleaseEnvelope = { value: publicReleaseValue, digest: hash(publicReleaseValue) };

  const stableBody = STABLE_BODY;
  const state = {
    calls: [],
    outputs: new Map(),
    release: {
      id: publicationValue.release_database_id,
      tag_name: CURRENT_RELEASE_TAG,
      draft: false,
      prerelease: false,
      body: stableBody,
      created_at: publicationValue.created_at,
      published_at: '2026-07-25T15:00:00.000Z',
      assets: publicationValue.assets.map(({ name, database_id: databaseId, size, digest }) => ({
        id: databaseId,
        name,
        size,
        digest: `sha256:${digest}`,
      })),
    },
    workflow: {
      id: publicationValue.workflow.run_id,
      run_attempt: publicationValue.workflow.attempt,
      head_sha: publicationValue.workflow.head_sha,
      path: publicationValue.workflow.path,
      event: publicationValue.workflow.event,
      status: 'completed',
      conclusion: 'success',
      run_started_at: '2026-07-25T14:50:00.000Z',
      updated_at: '2026-07-25T15:00:00.000Z',
    },
  };
  workflowMutation?.(state.workflow);
  liveMutation?.(state);
  const output = '/receipts/promotion-evidence-rebind.json';
  if (custodyConflict) state.outputs.set(output, 'occupied');
  const recordCall = (operation) => state.calls.push(operation);
  const adapter = {
    now: () => {
      recordCall('now');
      return reconciledAt;
    },
    loadProof: () => {
      recordCall('loadProof');
      return proofEnvelope;
    },
    loadPublication: () => {
      recordCall('loadPublication');
      return publicationEnvelope;
    },
    loadPublicRelease: () => {
      recordCall('loadPublicRelease');
      return publicReleaseEnvelope;
    },
    loadRetainedPromotion: () => {
      recordCall('loadRetainedPromotion');
      return { value: retainedPromotion, digest: hash(retainedPromotion) };
    },
    isAncestor: () => {
      recordCall('isAncestor');
      return true;
    },
    isPublicAncestor: () => {
      recordCall('isPublicAncestor');
      return true;
    },
    currentReleaseState: () => {
      recordCall('currentReleaseState');
      return { commit: PLANRUN_RELEASE_TAG_COMMIT, release: structuredClone(state.release) };
    },
    getPublicationWorkflowRun: () => {
      recordCall('getPublicationWorkflowRun');
      return structuredClone(state.workflow);
    },
    assertReceiptOutputAvailable: ({ path: receiptOutput }) => {
      recordCall('assertReceiptOutputAvailable');
      if (state.outputs.has(receiptOutput)) throw new Error('receipt output conflict');
    },
    writeReceipt: ({ path: receiptOutput, receipt, allowExisting }) => {
      recordCall('writeReceipt');
      assert.equal(allowExisting, false);
      if (state.outputs.has(receiptOutput)) throw new Error('receipt output conflict');
      state.outputs.set(receiptOutput, canonicalize(receipt));
      return { digest: hash(receipt) };
    },
  };
  assert.deepEqual(
    Object.keys(adapter).sort(),
    [...releasePromotion.PROMOTION_EVIDENCE_REBIND_ADAPTER_KEYS].sort(),
    'promotion evidence rebind adapter surface changed',
  );
  return {
    adapter,
    options: new Map([['receipt-out', output]]),
    proofEnvelope,
    publicationEnvelope,
    publicReleaseEnvelope,
    retainedPromotion,
    state,
  };
}

let tagStateBranch;
if (PLANRUN_RELEASE_TAG_COMMIT !== null) {
  tagStateBranch = 'cut';
  {
    const evidence = makePromotionEvidenceRebindAdapter();
    const result = releasePromotion.rebindPromotionEvidence(
      evidence.options,
      evidence.adapter,
      RETAINED_PROMOTION_EXPECTATION,
    );
    const freshContext = {
      proof: evidence.proofEnvelope,
      publication: evidence.publicationEnvelope,
      publicRelease: evidence.publicReleaseEnvelope,
    };
    const validateEvidenceReceipt = (receipt) =>
      releasePromotion.validatePromotionEvidenceRebindReceipt(
        receipt,
        freshContext,
        evidence.adapter,
        RETAINED_PROMOTION_EXPECTATION,
      );
    assert.equal(result.receipt.schema, 4);
    assert.equal(result.receipt.type, 'PromotionEvidenceRebindReceiptV1');
    assert.deepEqual(
      { schema: result.receipt.schema, type: result.receipt.type },
      releasePromotion.PROMOTION_EVIDENCE_REBIND_RECEIPT_DESCRIPTOR,
    );
    assert.equal(result.receipt.docks_plan.run_id, PLANRUN_DOCKS_RUN_ID);
    // The retained promotion is a prior attempt at THIS release, so its plan run is
    // this run - that is exactly what validateCurrentPromotionReceiptCore enforces.
    assert.equal(result.receipt.retained_promotion.docks_plan.run_id, CURRENT_DOCKS_RUN_ID);
    assert.equal(result.receipt.retained_promotion_sha256, RETAINED_PROMOTION_EXPECTATION.promotion_sha256);
    assert.equal(result.receipt.chronology.publication_workflow_completed_at, '2026-07-25T15:00:00.000Z');
    assert.equal(result.receipt.chronology.public_child_finished_at, '2026-07-26T01:36:05.859Z');
    assert.equal(result.receipt.chronology.original_promotion_completed_at, '2026-07-26T04:45:55.405Z');
    assert.equal(result.receipt.reconciled_at, '2026-07-26T06:00:00.000Z');
    assert.throws(
      () => validatePromotionReceipt(result.receipt),
      /promotion receipt.*keys|unknown or missing fields/i,
      'schema4 evidence must not enter the ordinary promotion receipt validator',
    );
    assert.equal(validateEvidenceReceipt(result.receipt), result.receipt);
    assert.equal(
      validatePromotionReceiptForFinalization(
        result.receipt,
        freshContext,
        evidence.adapter,
        RETAINED_PROMOTION_EXPECTATION,
      ),
      result.receipt,
    );
    assert.equal(
      evidence.state.outputs.get('/receipts/promotion-evidence-rebind.json'),
      canonicalize(result.receipt),
      'promotion evidence rebind must write one canonical local receipt',
    );
    const allowedCalls = new Set(releasePromotion.PROMOTION_EVIDENCE_REBIND_ADAPTER_KEYS);
    assert.ok(
      evidence.state.calls.every((operation) => allowedCalls.has(operation)),
      'promotion evidence rebind must use only the closed read-only adapter surface',
    );
    assert.equal(
      evidence.state.calls.filter((operation) => operation === 'now').length,
      1,
      'rebind may call now only once for reconciled_at',
    );
    assert.ok(
      evidence.state.calls.indexOf('now') > evidence.state.calls.indexOf('currentReleaseState') &&
        evidence.state.calls.indexOf('now') > evidence.state.calls.indexOf('getPublicationWorkflowRun'),
      'rebind must observe immutable release/workflow event times before recording reconciled_at',
    );
    for (const forbidden of [
      'promoteStable',
      'editStable',
      'pushMain',
      'pushTag',
      'remoteRef',
      'dispatchWorkflow',
      'retry',
      'repair',
    ]) {
      assert.equal(evidence.state.calls.includes(forbidden), false, `promotion evidence rebind called ${forbidden}`);
    }

    const repeated = makePromotionEvidenceRebindAdapter();
    const repeatedResult = releasePromotion.rebindPromotionEvidence(
      repeated.options,
      repeated.adapter,
      RETAINED_PROMOTION_EXPECTATION,
    );
    assert.deepEqual(repeatedResult.receipt, result.receipt, 'promotion evidence rebind must be deterministic');

    for (const key of Object.keys(result.receipt)) {
      const missing = structuredClone(result.receipt);
      delete missing[key];
      assert.throws(
        () => validateEvidenceReceipt(missing),
        /keys|unknown|missing|schema|field/i,
        `schema4 must reject missing top-level key ${key}`,
      );
    }
    assert.throws(
      () => validateEvidenceReceipt({ ...result.receipt, unknown: true }),
      /keys|unknown|field/i,
      'schema4 must reject unknown top-level keys',
    );
    const nestedUnknown = structuredClone(result.receipt);
    nestedUnknown.live_stable_release.unknown = true;
    assert.throws(
      () => validateEvidenceReceipt(nestedUnknown),
      /keys|unknown|field/i,
      'schema4 must keep the live stable snapshot closed',
    );

    for (const [label, mutate] of [
      ['wrong-schema', (receipt) => (receipt.schema -= 1)],
      ['wrong-type', (receipt) => (receipt.type = `${receipt.type}Wrong`)],
      ['wrong-repository', (receipt) => (receipt.repository_id = `${receipt.repository_id}/wrong`)],
      ['wrong-version', (receipt) => (receipt.version = `${receipt.version}-wrong`)],
      ['wrong-tag', (receipt) => (receipt.tag = `${receipt.tag}-wrong`)],
      [
        'wrong-retained-digest',
        (receipt) => (receipt.retained_promotion_sha256 = differentHex(receipt.retained_promotion_sha256)),
      ],
      [
        'blocked-v9-as-retained',
        (receipt) => {
          receipt.retained_promotion.docks_plan.run_id = NOT_CURRENT_DOCKS_RUN_ID;
          receipt.retained_promotion.docks_plan.plan_path = PLANRUN_DOCKS_PLAN_PATH;
        },
      ],
      ['wrong-fresh-run', (receipt) => (receipt.docks_plan.run_id = NOT_PLANRUN_DOCKS_RUN_ID)],
      [
        'wrong-reviewed-source',
        (receipt) => (receipt.reviewed_source_commit = differentHex(receipt.reviewed_source_commit)),
      ],
      ['wrong-public-child', (receipt) => (receipt.public_child.run_id = NOT_CURRENT_PUBLIC_RUN_ID)],
      ['wrong-staged-workflow', (receipt) => (receipt.staged_release.workflow_run_id += 1)],
      ['wrong-stable-database-id', (receipt) => (receipt.stable_release.release_database_id += 1)],
      [
        'wrong-live-commit',
        (receipt) => (receipt.live_stable_release.commit = differentHex(receipt.live_stable_release.commit)),
      ],
      [
        'wrong-live-body',
        (receipt) => (receipt.live_stable_release.body_sha256 = differentHex(receipt.live_stable_release.body_sha256)),
      ],
      [
        'wrong-live-asset',
        (receipt) =>
          (receipt.live_stable_release.assets[0].digest = differentHex(receipt.live_stable_release.assets[0].digest)),
      ],
      ['wrong-byte-identity', (receipt) => (receipt.byte_identical_promotion = false)],
      ['wrong-outcome', (receipt) => (receipt.outcome = 'failure')],
      [
        'publication-after-child',
        (receipt) =>
          (receipt.chronology.publication_workflow_completed_at = receipt.chronology.public_child_finished_at),
      ],
      [
        'child-after-promotion',
        (receipt) => (receipt.chronology.public_child_finished_at = receipt.chronology.original_promotion_completed_at),
      ],
      ['promotion-after-reconciliation', (receipt) => (receipt.reconciled_at = '2026-07-26T04:00:00.000Z')],
      [
        'promotion-equals-reconciliation',
        (receipt) => (receipt.reconciled_at = receipt.chronology.original_promotion_completed_at),
      ],
      ['invalid-reconciliation-time', (receipt) => (receipt.reconciled_at = 'now')],
    ]) {
      const corrupted = structuredClone(result.receipt);
      mutate(corrupted);
      assert.throws(
        () => validateEvidenceReceipt(corrupted),
        /promotion evidence|retained|identity|digest|PlanRun|child|release|asset|byte|outcome|chronology|timestamp|keys/i,
        `schema4 must reject ${label}`,
      );
    }
  }

  {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-rebind-production-adapter-'));
    try {
      const evidence = makePromotionEvidenceRebindAdapter();
      const inputs = [
        ['source-proof', 'loadProof', writeBoundaryValue(directory, 'source-proof.json', evidence.proofEnvelope.value)],
        [
          'publication',
          'loadPublication',
          writeBoundaryValue(directory, 'publication.json', evidence.publicationEnvelope.value),
        ],
        [
          'public-release',
          'loadPublicRelease',
          writeBoundaryValue(directory, 'public-release.json', evidence.publicReleaseEnvelope.value),
        ],
        [
          'promotion',
          'loadRetainedPromotion',
          writeBoundaryValue(directory, 'retained-promotion.json', evidence.retainedPromotion),
        ],
      ];
      assert.deepEqual(
        Object.keys(releasePromotion.PROMOTION_EVIDENCE_REBIND_PRODUCTION_ADAPTER).sort(),
        [...releasePromotion.PROMOTION_EVIDENCE_REBIND_ADAPTER_KEYS].sort(),
        'production rebind adapter must expose only the read-only surface',
      );
      for (const [name, loader, file] of inputs) {
        const options = new Map([
          [name, file.file],
          [`${name}-sha256`, file.digest],
        ]);
        assert.equal(
          releasePromotion.PROMOTION_EVIDENCE_REBIND_PRODUCTION_ADAPTER[loader](options).digest,
          file.digest,
          `production rebind adapter must load the exact --${name}/--${name}-sha256 pair`,
        );
        options.set(`${name}-sha256`, differentHex(file.digest));
        assert.throws(
          () => releasePromotion.PROMOTION_EVIDENCE_REBIND_PRODUCTION_ADAPTER[loader](options),
          /digest mismatch/i,
          `production rebind adapter must reject a mismatched --${name}-sha256`,
        );
      }
      for (const [name, loader, value] of [
        ['source-proof', 'loadProof', evidence.proofEnvelope.value],
        ['publication', 'loadPublication', evidence.publicationEnvelope.value],
        ['public-release', 'loadPublicRelease', evidence.publicReleaseEnvelope.value],
        ['promotion', 'loadRetainedPromotion', evidence.retainedPromotion],
      ]) {
        const wrong = writeBoundaryValue(directory, `wrong-${name}.json`, {
          ...structuredClone(value),
          schema: value.schema - 1,
          type: `${value.type}Wrong`,
        });
        const options = new Map([
          [name, wrong.file],
          [`${name}-sha256`, wrong.digest],
        ]);
        assert.throws(
          () => releasePromotion.PROMOTION_EVIDENCE_REBIND_PRODUCTION_ADAPTER[loader](options),
          /wrong schema or type/i,
          `production rebind adapter must reject the wrong --${name} generation`,
        );
      }
    } finally {
      fs.rmSync(directory, { recursive: true, force: true });
    }
  }

  for (const [label, fixtureOptions, pattern] of [
    [
      'stale successor run',
      { proofMutation: (proof) => (proof.run_id = NOT_PLANRUN_DOCKS_RUN_ID) },
      /fresh successor|source proof|identity|PlanRun|run/i,
    ],
    [
      'wrong immutable release source',
      {
        proofMutation: (proof) => {
          proof.source_commit = NOT_PLANRUN_DOCKS_SOURCE_BASE;
        },
      },
      /fresh successor|source|PlanRun/i,
    ],
    [
      'wrong immutable tag',
      { proofMutation: (proof) => (proof.tag_commit = proof.implementation_commit) },
      /immutable.*release|tag|source proof/i,
    ],
    [
      'V3 publication reviewed implementation relabel',
      {
        publicationMutation: (publication) => {
          publication.source.reviewed_commit = publication.source.implementation_commit;
        },
      },
      /reviewed|tag|source|publication/i,
    ],
    [
      'publication now substituted for event time',
      { publicationMutation: (publication) => (publication.created_at = '2026-07-26T06:00:00.000Z') },
      /chronology|created_at|live|publication/i,
    ],
    [
      'publication workflow identity drift',
      {
        publicationMutation: (publication) => {
          publication.workflow.run_id += 1;
          publication.digest_evidence.workflow_run_id += 1;
        },
      },
      /workflow|stable|publication|receipt/i,
    ],
    [
      'wrong public child run',
      {
        publicReleaseMutation: (publicRelease) =>
          (publicRelease.public_plan.plan_run.run_id = NOT_CURRENT_PUBLIC_RUN_ID),
      },
      /PlanRun|public|run|receipt/i,
    ],
    [
      'retained receipt mutation',
      { retainedMutation: (promotion) => (promotion.docks_plan.run_id = NOT_CURRENT_DOCKS_RUN_ID) },
      /retained|exact|digest|SHA-256/i,
    ],
    [
      'stable tag drift',
      { liveMutation: (state) => (state.release.tag_name = `${state.release.tag_name}-wrong`) },
      /stable|tag|release/i,
    ],
    ['stable draft drift', { liveMutation: (state) => (state.release.draft = true) }, /stable|state|release|tag/i],
    [
      'stable prerelease drift',
      { liveMutation: (state) => (state.release.prerelease = true) },
      /stable|state|release|prerelease/i,
    ],
    [
      'stable created-at drift',
      { liveMutation: (state) => (state.release.created_at = '2026-07-25T14:00:00.001Z') },
      /created_at|live|publication|receipt/i,
    ],
    [
      'stable body drift',
      { liveMutation: (state) => (state.release.body = `${state.release.body}\ndrift`) },
      /stable|body|release/i,
    ],
    [
      'stable database identity drift',
      { liveMutation: (state) => (state.release.id += 1) },
      /stable|database|release/i,
    ],
    [
      'stable asset digest drift',
      { liveMutation: (state) => (state.release.assets[0].digest = differentHex(state.release.assets[0].digest)) },
      /stable|asset|digest|drift/i,
    ],
    [
      'stable release timestamp inversion',
      { liveMutation: (state) => (state.release.published_at = '2026-07-25T13:00:00.000Z') },
      /timestamp|inverted/i,
    ],
    [
      'publication workflow identity drift',
      { workflowMutation: (run) => (run.id += 1) },
      /workflow.*identity|terminal state/i,
    ],
    [
      'publication workflow no longer terminal',
      { workflowMutation: (run) => (run.status = 'in_progress') },
      /workflow.*identity|terminal state/i,
    ],
    [
      'publication event outside workflow window',
      { workflowMutation: (run) => (run.updated_at = '2026-07-25T14:59:59.999Z') },
      /workflow.*timestamp|publication event/i,
    ],
    [
      'publication workflow completion after public child',
      { workflowMutation: (run) => (run.updated_at = '2026-07-26T02:00:00.000Z') },
      /chronology|publication|public child/i,
    ],
    [
      'reconciliation before promotion',
      { reconciledAt: '2026-07-26T04:00:00.000Z' },
      /chronology|timestamp|reconciliation/i,
    ],
    [
      'reconciliation equals promotion',
      { reconciledAt: '2026-07-26T04:45:55.405Z' },
      /chronology|timestamp|reconciliation/i,
    ],
  ]) {
    const evidence = makePromotionEvidenceRebindAdapter(fixtureOptions);
    assert.throws(
      () =>
        releasePromotion.rebindPromotionEvidence(evidence.options, evidence.adapter, RETAINED_PROMOTION_EXPECTATION),
      pattern,
      `promotion evidence rebind must reject ${label}`,
    );
    assert.equal(evidence.state.outputs.size, 0, `${label} must not write a receipt`);
    assert.equal(evidence.state.calls.includes('writeReceipt'), false, `${label} must fail before receipt output`);
  }

  {
    const evidence = makePromotionEvidenceRebindAdapter();
    evidence.proofEnvelope.digest = differentHex(evidence.proofEnvelope.digest);
    assert.throws(
      () =>
        releasePromotion.rebindPromotionEvidence(evidence.options, evidence.adapter, RETAINED_PROMOTION_EXPECTATION),
      /source proof digest mismatch/i,
      'promotion evidence rebind must independently reject an envelope digest mismatch',
    );
    assert.equal(evidence.state.outputs.size, 0);
  }

  {
    const evidence = makePromotionEvidenceRebindAdapter({ custodyConflict: true });
    assert.throws(
      () =>
        releasePromotion.rebindPromotionEvidence(evidence.options, evidence.adapter, RETAINED_PROMOTION_EXPECTATION),
      /output conflict/i,
      'promotion evidence rebind must fail closed on occupied output custody',
    );
    assert.deepEqual(
      evidence.state.calls,
      ['assertReceiptOutputAvailable'],
      'occupied custody must fail before reading evidence, observing release/workflow state, or calling now',
    );
  }

  for (const forbidden of [
    'promoteStable',
    'editStable',
    'pushMain',
    'pushTag',
    'remoteRef',
    'dispatchWorkflow',
    'retry',
    'repair',
  ]) {
    const evidence = makePromotionEvidenceRebindAdapter();
    const adapter = { ...evidence.adapter, [forbidden]: () => assert.fail(`${forbidden} must not be callable`) };
    assert.throws(
      () => releasePromotion.rebindPromotionEvidence(evidence.options, adapter, RETAINED_PROMOTION_EXPECTATION),
      /adapter|unknown|keys|field/i,
      `promotion evidence rebind adapter must reject mutation surface ${forbidden}`,
    );
    assert.deepEqual(evidence.state.calls, []);
  }

  {
    const foreignLineage = retainedPromotionV3();
    foreignLineage.docks_plan.run_id = NOT_CURRENT_DOCKS_RUN_ID;
    assert.throws(
      () => validatePromotionReceipt(foreignLineage),
      /PlanRun|identity|current promotion/i,
      'ordinary promotion validation must reject a receipt from a different release lineage',
    );
    const blocked = retainedPromotionV3();
    blocked.docks_plan.run_id = NOT_PLANRUN_DOCKS_RUN_ID;
    blocked.docks_plan.plan_path = PLANRUN_DOCKS_PLAN_PATH;
    assert.throws(
      () => validatePromotionReceipt(blocked),
      /PlanRun|identity|current promotion/i,
      'ordinary promotion validation must not accept the blocked v9 identity',
    );
  }
} else {
  tagStateBranch = 'unborn';
  const evidence = makePromotionEvidenceRebindAdapter();
  assert.throws(
    () => releasePromotion.rebindPromotionEvidence(evidence.options, evidence.adapter, RETAINED_PROMOTION_EXPECTATION),
    /retained promotion receipt reviewed source commit must be a 40-character lowercase commit/i,
    'promotion evidence rebind must refuse every concrete retained receipt while the current release tag is uncut',
  );
  assert.equal(evidence.state.outputs.size, 0, 'unborn-tag refusal must not write a receipt');
}

{
  const retainedV2Proof = currentPromotionProofV2();
  assert.equal(validateSourcePreparationProof(retainedV2Proof), retainedV2Proof);
  assert.equal(retainedV2Proof.schema, 2);
  assert.equal(retainedV2Proof.type, 'SourcePreparationProofV2');
  assert.equal(retainedV2Proof.version, RETAINED_V2_RELEASE_VERSION);
  assert.equal(retainedV2Proof.run_id, RETAINED_V2_INSTANCE.current_attempt.docks_run_id);
  assert.equal(retainedV2Proof.plan_run.plan_path, RETAINED_V2_INSTANCE.current_attempt.docks_plan_path);
}
if (PLANRUN_RELEASE_TAG_COMMIT !== null) {
  {
    const current = makeCurrentPromotionAdapter({ planRun: true, sourcePlanRun: true });
    validateSourcePreparationProof(current.adapter.loadProof().value);
    const result = promoteReviewed(current.options, false, current.adapter);
    validatePromotionReceipt(result.receipt);
    validatePromotionReceiptForFinalization(result.receipt, {
      proof: current.adapter.loadProof(),
      publication: current.publicationEnvelope,
      publicRelease: current.publicReleaseEnvelope,
    });
    assert.equal(result.receipt.schema, 3);
    assert.equal(result.receipt.type, 'PromotionReceiptV3');
    assert.equal(result.receipt.docks_plan.run_id, PLANRUN_DOCKS_RUN_ID);
    assert.equal(result.receipt.docks_plan.plan_path, PLANRUN_DOCKS_PLAN_PATH);
    assert.equal(current.adapter.loadProof().value.tag_commit, PLANRUN_RELEASE_TAG_COMMIT);
    assert.notEqual(
      current.adapter.loadProof().value.plan_run.source_base,
      current.adapter.loadProof().value.source_commit,
    );
    assert.deepEqual(current.adapter.loadProof().value.ancestry, {
      source_to_tag: true,
      tag_to_implementation: true,
      implementation_to_reviewed: true,
    });
    assert.equal(result.receipt.public_child.planrun_verified, true);
    assert.equal(result.receipt.public_release_receipt_sha256, current.publicReleaseEnvelope.digest);
    assert.equal(result.receipt.publication_receipt_sha256, current.publicationEnvelope.digest);
    assert.equal(result.receipt.public_child.npm_version, CURRENT_PUBLIC_VERSION);
    assert.equal(result.receipt.staged_release.prerelease, true);
    assert.equal(result.receipt.stable_release.prerelease, false);
    assert.deepEqual(result.receipt.staged_release.assets, result.receipt.stable_release.assets);
    assert.equal(current.state.promotions, 1, 'PlanRun release must be promoted exactly once');
    assert.equal(
      current.state.outputs.get('/receipts/current-promotion.json'),
      canonicalize(result.receipt),
      'current PlanRun promotion receipt output must be canonical',
    );
    assert.deepEqual(current.state.ancestryCalls, [
      [PLANRUN_DOCKS_SOURCE_BASE, PLANRUN_RELEASE_TAG_COMMIT],
      [PLANRUN_RELEASE_TAG_COMMIT, current.adapter.loadProof().value.implementation_commit],
      [POST_TAG_PLANRUN_SOURCE_BASE, current.adapter.loadProof().value.implementation_commit],
    ]);
    const wrongTag = currentPromotionProofV3();
    wrongTag.tag_commit = wrongTag.implementation_commit;
    assert.throws(
      () => validateSourcePreparationProof(wrongTag),
      /immutable Session Relay release commit/i,
      'PlanRun source proof must not relabel the reviewed remediation commit as the release tag',
    );
    const backwardsAncestry = currentPromotionProofV3();
    backwardsAncestry.ancestry = {
      tag_to_source: true,
      source_to_implementation: true,
      implementation_to_reviewed: true,
    };
    assert.throws(
      () => validateSourcePreparationProof(backwardsAncestry),
      /ancestry|source_to_tag/i,
      'PlanRun source proof must bind source-to-tag-to-implementation ancestry',
    );
  }

  const unrelatedPlanSource = makeCurrentPromotionAdapter({
    planRun: true,
    sourcePlanRun: true,
    planSourceAncestry: false,
  });
  assert.throws(
    () => promoteReviewed(unrelatedPlanSource.options, false, unrelatedPlanSource.adapter),
    /plan-source-to-implementation ancestry was not independently observed/i,
    'promotion must independently refuse an unrelated successor PlanRun source',
  );

  {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-current-production-adapter-'));
    try {
      const current = makeCurrentPromotionAdapter();
      const production = releasePromotion.CURRENT_PRODUCTION_ADAPTER;
      assert.deepEqual(
        Object.keys(production).sort(),
        [...releasePromotion.CURRENT_PROMOTION_ADAPTER_KEYS].sort(),
        'current production promotion adapter surface changed',
      );

      const publicationFile = writeBoundaryValue(
        directory,
        'current-publication.json',
        current.publicationEnvelope.value,
      );
      const publication = production.loadPublication(
        new Map([
          ['publication', publicationFile.file],
          ['publication-sha256', current.publicationEnvelope.digest],
        ]),
      );
      assert.deepEqual(publication.value, current.publicationEnvelope.value);
      assert.equal(publication.digest, current.publicationEnvelope.digest);

      const publicReleaseFile = writeBoundaryValue(
        directory,
        'current-public-release.json',
        current.publicReleaseEnvelope.value,
      );
      const publicRelease = production.loadPublicRelease(
        new Map([
          ['public-release', publicReleaseFile.file],
          ['public-release-sha256', current.publicReleaseEnvelope.digest],
        ]),
      );
      assert.deepEqual(publicRelease.value, current.publicReleaseEnvelope.value);
      assert.equal(publicRelease.digest, current.publicReleaseEnvelope.digest);

      const legacyValue = structuredClone(current.publicationEnvelope.value);
      legacyValue.schema = 1;
      legacyValue.type = 'SessionRelayPublicationReceiptV1';
      const legacyFile = writeBoundaryValue(directory, 'legacy-publication.json', legacyValue);
      assert.throws(
        () =>
          production.loadPublication(
            new Map([
              ['publication', legacyFile.file],
              ['publication-sha256', legacyFile.digest],
            ]),
          ),
        /wrong schema or type/i,
        'current production adapter must accept only the exact schema-2 V2 publication descriptor',
      );
    } finally {
      fs.rmSync(directory, { recursive: true, force: true });
    }
  }

  {
    const current = makeCurrentPromotionAdapter({ stableInitially: true });
    assert.throws(
      () => promoteReviewed(current.options, false, current.adapter),
      /already stable|public child.*first|prerelease/i,
      'current promotion must reject a release made stable before this reviewed public-child boundary',
    );
    assert.equal(current.state.promotions, 0);
    assert.equal(current.state.outputs.size, 0);
  }

  {
    const current = makeCurrentPromotionAdapter({ byteDrift: true });
    assert.throws(
      () => promoteReviewed(current.options, false, current.adapter),
      /asset.*drift|asset.*IDs|digest/i,
      'current promotion must reject stable read-back byte drift',
    );
    assert.equal(current.state.promotions, 1);
    assert.equal(current.state.outputs.size, 0);
  }

  {
    const current = makeCurrentPromotionAdapter({ refConflict: NOT_PLANRUN_RELEASE_TAG_COMMIT });
    assert.throws(
      () => promoteReviewed(current.options, false, current.adapter),
      /authority ref|retries.*not permitted/i,
      'current promotion must reject conflicting current authority refs',
    );
    assert.equal(current.state.promotions, 0);
    assert.equal(current.state.outputs.size, 0);
  }

  {
    const current = makeCurrentPromotionAdapter();
    current.options.set('retry-failed', '/receipts/legacy-failure.json');
    current.options.set('retry-failed-sha256', DIGEST('0'));
    assert.throws(
      () => promoteReviewed(current.options, false, current.adapter),
      /single-shot|cannot.*retry/i,
      'current promotion must reject legacy retry machinery',
    );
    assert.equal(current.state.promotions, 0);
    assert.equal(current.state.outputs.size, 0);
  }
} else {
  const unbornProof = currentPromotionProofV3();
  assert.equal(unbornProof.tag_commit, null, 'the unborn PlanRun proof fixture must not invent a tag commit');
  assert.throws(
    () => validateSourcePreparationProof(unbornProof),
    /current source proof tag_commit must be 40 lowercase hexadecimal characters/i,
    'PlanRun promotion must refuse a source proof until the immutable release tag exists',
  );
}
assert.ok(
  tagStateBranch === 'unborn' || tagStateBranch === 'cut',
  'exactly one bound release-tag state branch must run',
);

for (const [label, mutate] of [
  ['missing-darwin', (records) => records.filter(({ name }) => name !== 'session-relay-aarch64-apple-darwin')],
  [
    'substituted-darwin',
    (records) =>
      records.map((record) =>
        record.name === 'session-relay-x86_64-apple-darwin'
          ? { ...record, name: 'session-relay-x86_64-apple-darwin-workspace-supported' }
          : record,
      ),
  ],
]) {
  const { adapter, state } = makeAdapter();
  state.releaseAssets = mutate(state.releaseAssets);
  assert.throws(
    () => run(adapter, `/receipts/${label}.json`),
    /authoritative release.*asset|wrong closed asset set|release assets changed/i,
    `promotion must reject ${label} ordinary asset state`,
  );
  assert.deepEqual(state.calls, [], `${label} rejection must precede release mutation`);
  assert.deepEqual(
    state.counts,
    { lock: 0, prepush: 0, main: 0, live: 0, restore: 0, reapply: 0, append: 0 },
    `${label} rejection must precede state-machine mutation`,
  );
}

{
  const { adapter, state } = makeAdapter();
  const result = run(adapter);
  assert.equal(result.receipt.outcome, 'success');
  assert.equal(result.receipt.retryable, false);
  assert.equal(result.receipt.promoted_commit, PROMOTED_COMMIT);
  assert.equal(state.refs.get('refs/heads/main'), PROMOTED_COMMIT);
  const wrongPublicTag = structuredClone(result.receipt);
  wrongPublicTag.public_tag_commit = differentHex(wrongPublicTag.public_tag_commit);
  assert.throws(() => validatePromotionReceipt(wrongPublicTag), /public release|docks-kit/i);
  const wrongDocksTarget = structuredClone(result.receipt);
  wrongDocksTarget.docks_kit.target_commit = differentHex(wrongDocksTarget.docks_kit.target_commit);
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
  assert.throws(
    () => validatePromotionReceipt(oldShape),
    /unknown|missing|field|keys/i,
    'old promotion receipt shape must be rejected',
  );
  assert.throws(
    () => validatePromotionReceiptForFinalization(oldShape, { proof, publication }, adapter),
    /unknown|missing|field|keys/i,
    'finalization must reject the old promotion receipt shape',
  );
  assert.deepEqual(result.receipt.docks_kit.asset, smoke('exact_source').docks_kit_asset);
  assert.deepEqual(phases(state), [
    '0:0:initialized',
    '0:1:locked',
    '0:2:prepush_passed',
    '0:3:main_pushed',
    '0:4:live_passed',
    '0:5:terminal_success',
  ]);
  assert.deepEqual(state.counts, { lock: 1, prepush: 1, main: 1, live: 1, restore: 0, reapply: 0, append: 6 });
  assert.equal(state.calls.find(({ operation }) => operation === 'main').expected, OLD_MAIN);
  assert.equal(state.calls.find(({ operation }) => operation === 'lock').prior, null);
  assert.equal(
    validatePromotionReceiptForFinalization(result.receipt, { proof, publication }, adapter),
    result.receipt,
  );
  assert.throws(
    () =>
      validatePromotionReceiptForFinalization(
        result.receipt,
        { proof, publication },
        { ...adapter, isAncestor: () => false },
      ),
    /lineage|ancestor/i,
  );
  assert.throws(
    () =>
      validatePromotionReceiptForFinalization(
        { ...result.receipt, created_at: '2026-07-17T21:00:00.001Z' },
        { proof, publication },
        adapter,
      ),
    /reconstruct|journal/i,
  );
  state.releaseStable = true;
  assert.equal(
    validatePromotionReceiptForFinalization(result.receipt, { proof, publication }, adapter),
    result.receipt,
    'stable finalization resume must retain authoritative receipt validation',
  );
  state.releaseStable = false;
  assert.ok(
    state.calls
      .filter(({ operation }) => operation === 'append')
      .every((call, index, calls) => (index === 0 ? call.prior === null : call.prior !== calls[index - 1].prior)),
  );
}

{
  const { adapter, state } = makeAdapter({ initialMain: PROMOTED_COMMIT });
  const result = run(adapter, '/receipts/already-promoted.json', {
    'expected-origin-main': PROMOTED_COMMIT,
  });
  assert.equal(result.receipt.outcome, 'success');
  assert.equal(state.refs.get('refs/heads/main'), PROMOTED_COMMIT);
  assert.equal(state.counts.main, 0, 'promotion must not replay an already-applied origin/main update');
  assert.deepEqual(phases(state), [
    '0:0:initialized',
    '0:1:locked',
    '0:2:prepush_passed',
    '0:3:main_pushed',
    '0:4:live_passed',
    '0:5:terminal_success',
  ]);
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
  if (phase === 'terminal_success')
    assert.equal(state.counts.append, before.append, 'terminal recovery must not append');
}

{
  const { adapter, state } = makeAdapter({ outageAfterAppend: 'initialized' });
  assert.throws(
    () => run(adapter, '/receipts/probe-outage.json'),
    (error) => error.outcome === 'failure' && /probe outage/.test(error.message),
  );
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
    prepushMutator: (evidence) => {
      evidence.sync_argv = legacyRetryArgv;
    },
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
  assert.deepEqual(
    retried.receipt.exact_source_smoke.sync_argv,
    legacyRetryArgv,
    'restored retry preserves historical exact-source evidence',
  );
  assert.deepEqual(phases(state).slice(-4), [
    '1:0:initialized',
    '1:1:locked',
    '1:2:reapply_pushed',
    '1:3:terminal_success',
  ]);
  assert.deepEqual(
    {
      prepush: state.counts.prepush,
      main: state.counts.main,
      restore: state.counts.restore,
      reapply: state.counts.reapply,
    },
    { prepush: 1, main: 1, restore: 1, reapply: 1 },
  );
  const legacyChain = adapter.readJournal(TRANSACTION_REF, state.refs.get(TRANSACTION_REF));
  for (const item of legacyChain) delete item.entry.immutable.prepush_repair;
  const attemptOneIndex = legacyChain.findIndex(({ entry }) => entry.attempt === 1);
  const legacyPrefix = legacyChain.slice(0, attemptOneIndex);
  const legacyPriorReceipt = structuredClone(failure.receipt);
  legacyPriorReceipt.journal_chain_sha256 = hash(canonicalize(legacyPrefix));
  const legacyPriorDigest = hash(canonicalize(legacyPriorReceipt));
  for (const item of legacyChain.slice(attemptOneIndex))
    item.entry.immutable.prior_attempt_receipt_sha256 = legacyPriorDigest;
  legacyChain.at(-1).entry.receipt_projection.prior_attempt_receipt_sha256 = legacyPriorDigest;
  assert.equal(
    validatePromotionJournal(legacyChain, legacyChain.at(-1).commit).tip.entry.phase,
    'terminal_success',
    'old-format restored retry journals must keep the reapply transition map',
  );
  assert.throws(
    () =>
      run(adapter, '/receipts/retry-again.json', {
        'retry-failed': '/receipts/failure.json',
        'retry-failed-sha256': failure.state.receipt_sha256,
      }),
    /retry|terminal|attempt/i,
  );
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
  expectInterrupted(() =>
    run(adapter, `/receipts/retry-delete-${phase}.json`, {
      'retry-failed': '/receipts/failure.json',
      'retry-failed-sha256': failure.state.receipt_sha256,
    }),
  );
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
  const { adapter } = makeAdapter({
    liveResults: [false],
    killMutation: 'restore_mutation',
    adoptionRerunParity: rerunParity,
  });
  expectInterrupted(() => run(adapter, '/receipts/nondeterministic-ci.json'));
  const recovered = run(adapter, '/receipts/nondeterministic-ci-recovered.json', {}, true);
  assert.deepEqual(
    recovered.receipt.post_restore,
    {
      manifest_catalog: true,
      full_ci_exit: 0,
      full_ci_stdout_sha256: DIGEST('7'),
      full_ci_stderr_sha256: DIGEST('8'),
    },
    'receipt must preserve commit-bound restore evidence rather than nondeterministic adoption-run hashes',
  );
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
  assert.equal(
    one.state.outputs.get('/receipts/one.json'),
    two.state.outputs.get('/receipts/two.json'),
    'terminal receipt bytes must be deterministic',
  );
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
  assert.equal(
    result.receipt.outcome,
    'success',
    'resume may rerun and journal prepush evidence before deriving main_pushed',
  );
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
  expectInterrupted(() =>
    run(adapter, '/receipts/retry-killed.json', {
      'retry-failed': '/receipts/failure.json',
      'retry-failed-sha256': failure.state.receipt_sha256,
    }),
  );
  const result = run(adapter, '/receipts/retry-recovered.json', {}, true);
  assert.equal(result.receipt.outcome, 'success');
  assert.equal(state.counts.reapply, 1, 'resume must not repeat a completed reapply');
}

{
  const { adapter, state } = makeAdapter({ liveResults: [false, true] });
  const failure = run(adapter, '/receipts/failure.json');
  state.killAfter = 'locked';
  expectInterrupted(() =>
    run(adapter, '/receipts/retry-killed.json', {
      'retry-failed': '/receipts/failure.json',
      'retry-failed-sha256': failure.state.receipt_sha256,
    }),
  );
  state.refs.set('refs/heads/main', '9'.repeat(40));
  state.currentBlobs = structuredClone(state.journal[0].entry.state.compatibility.promoted);
  const result = run(adapter, '/receipts/arbitrary-reapply-head.json', {}, true);
  assert.equal(result.receipt.outcome, 'manual_incident', 'same blobs cannot authenticate an arbitrary reapply head');
}

{
  const { adapter, state } = makeAdapter({ liveResults: [false, true] });
  const failure = run(adapter, '/receipts/failure.json');
  state.killAfter = 'terminal_success';
  expectInterrupted(() =>
    run(adapter, '/receipts/retry-killed.json', {
      'retry-failed': '/receipts/failure.json',
      'retry-failed-sha256': failure.state.receipt_sha256,
    }),
  );
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
  expectInterrupted(() =>
    run(adapter, '/receipts/retry-killed.json', {
      'retry-failed': '/receipts/failure.json',
      'retry-failed-sha256': failure.state.receipt_sha256,
    }),
  );
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
  assert.throws(
    () => run(adapter, `/receipts/tag-recreated-${releaseAbsent}.json`, {}, true),
    /snapshot.*changed|authority/i,
  );
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
  assert.throws(
    () => validatePromotionJournal(corruptObservedRelease, state.refs.get(TRANSACTION_REF)),
    /observed Release state/i,
  );
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
  state.prepushMutator = (evidence) => {
    evidence.unknown = true;
  };
  const before = { append: state.counts.append, tip: state.refs.get(TRANSACTION_REF) };
  assert.throws(() => run(adapter, '/receipts/invalid-generated-state.json', {}, true), /smoke|unknown|schema/i);
  assert.deepEqual(
    { append: state.counts.append, tip: state.refs.get(TRANSACTION_REF) },
    before,
    'invalid generated journal entry must be rejected before remote append',
  );
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'locked' });
  expectInterrupted(() => run(adapter));
  const valid = adapter.readJournal(TRANSACTION_REF, state.refs.get(TRANSACTION_REF));
  assert.equal(validatePromotionJournal(valid, state.refs.get(TRANSACTION_REF)).tip.entry.phase, 'locked');
  const corruptions = [
    (chain) => {
      chain[0].entry.unknown = true;
    },
    (chain) => {
      chain[1].entry.sequence = 8;
    },
    (chain) => {
      chain[1].entry.immutable.tag_commit = differentHex(chain[1].entry.immutable.tag_commit);
    },
    (chain) => {
      chain[1].entry.phase = 'main_pushed';
    },
    (chain) => {
      chain[1].parent = differentHex(chain[1].parent);
    },
  ];
  for (const corrupt of corruptions) {
    const chain = structuredClone(valid);
    corrupt(chain);
    assert.throws(
      () => validatePromotionJournal(chain, state.refs.get(TRANSACTION_REF)),
      /journal|schema|sequence|immutable|transition|parent/i,
    );
  }
  assert.throws(() => validatePromotionJournal(valid, '8'.repeat(40)), /authoritative.*tip/i);
}

{
  const { adapter, state } = makeAdapter();
  run(adapter, '/receipts/schema.json');
  const valid = adapter.readJournal(TRANSACTION_REF, state.refs.get(TRANSACTION_REF));
  const corruptions = [
    (projection) => {
      projection.source_ancestry.extra = true;
    },
    (projection) => {
      projection.non_plan_tree_equivalence = {};
    },
    (projection) => {
      const path = 'plugins/session-relay/bin/relay';
      projection.compatibility_blobs.before[path] = `${projection.compatibility_blobs.before[path]}0`;
    },
    (projection) => {
      projection.docks_kit.extra = true;
    },
    (projection) => {
      projection.session_relay_assets[0].extra = true;
    },
    (projection) => {
      projection.exact_source_smoke = {};
    },
    (projection) => {
      projection.live_smoke = {};
    },
    (projection) => {
      projection.created_at = '1';
    },
    (projection) => {
      projection.outcome = 'restored_failure';
      projection.retryable = true;
    },
    (projection) => {
      projection.public_reviewed_commit = null;
    },
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
  assert.throws(
    () => run(adapter, '/receipts/divergent-public-release.json'),
    /public.*ancestor|companion.*ancestry|lineage/i,
  );
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
  assert.deepEqual(
    result.receipt.exact_source_smoke.sync_argv,
    legacyArgv,
    'historical failed evidence retains the rejected sync argv',
  );
  assert.equal(state.counts.main, 0);
  assert.equal(state.journal.at(-1).entry.phase, 'terminal_failure');
  assert.equal(run(adapter, '/receipts/prepush-exception-resume.json', {}, true).receipt.outcome, 'failure');
  assert.throws(
    () =>
      run(adapter, '/receipts/generic-retry.json', {
        'retry-failed': '/receipts/failure.json',
        'retry-failed-sha256': result.state.receipt_sha256,
      }),
    /restored.failure|repair.prepush/i,
  );

  const appendCountBeforeAuthorityFailure = state.counts.append;
  state.releaseStable = true;
  assert.throws(
    () =>
      run(adapter, '/receipts/stable-release-repair.json', {
        'repair-prepush': true,
        'repair-implementation-commit': REPAIR_IMPLEMENTATION_COMMIT,
        'retry-failed': '/receipts/failure.json',
        'retry-failed-sha256': result.state.receipt_sha256,
      }),
    /release|prerelease/i,
  );
  assert.equal(
    state.counts.append,
    appendCountBeforeAuthorityFailure,
    'changed prerelease authority must be rejected before attempt 1',
  );
  state.releaseStable = false;
  const validateRepair = adapter.validatePrepushRepair;
  adapter.validatePrepushRepair = (input) => {
    const evidence = validateRepair(input);
    state.releaseStable = true;
    return evidence;
  };
  assert.throws(
    () =>
      run(adapter, '/receipts/raced-release-repair.json', {
        'repair-prepush': true,
        'repair-implementation-commit': REPAIR_IMPLEMENTATION_COMMIT,
        'retry-failed': '/receipts/failure.json',
        'retry-failed-sha256': result.state.receipt_sha256,
      }),
    /release|prerelease/i,
  );
  assert.equal(
    state.counts.append,
    appendCountBeforeAuthorityFailure,
    'authority drift during repair CI must be rejected before attempt 1',
  );
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
  assert.deepEqual(
    repaired.receipt.exact_source_smoke.sync_argv,
    ['sync'],
    'successful repair evidence uses the URL-rewrite sync binding',
  );
  assert.equal(state.refs.get('refs/heads/main'), PROMOTED_COMMIT);
  assert.equal(
    state.outputs.get('/receipts/failure.json'),
    priorBytes,
    'repair continuation must not rewrite failed evidence',
  );
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
  for (const item of invalidBase.filter(({ entry }) => entry.attempt === 1))
    item.entry.immutable.prepush_repair.base_commit = differentHex(item.entry.immutable.prepush_repair.base_commit);
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
  invalidEligibility.find(
    ({ entry }) => entry.attempt === 0 && entry.phase === 'terminal_failure',
  ).entry.state.failure = 'unrelated pre-push failure';
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
  assert.throws(
    () =>
      run(failedPrepush.adapter, '/receipts/invalid-repair.json', {
        'repair-prepush': true,
        'repair-implementation-commit': REPAIR_IMPLEMENTATION_COMMIT,
        'retry-failed': '/receipts/failure.json',
        'retry-failed-sha256': failed.state.receipt_sha256,
      }),
    /repair.*paths/i,
  );
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

  wrongDocksKit.live_smoke.docks_kit_asset.digest = differentHex(wrongDocksKit.live_smoke.docks_kit_asset.digest);
  assert.throws(() => validatePromotionReceipt(wrongDocksKit), /live.*docks-kit|smoke binding/i);
  const wrongLauncher = structuredClone(receipt);
  wrongLauncher.live_smoke.launcher_sha256 = differentHex(wrongLauncher.live_smoke.launcher_sha256);
  assert.throws(() => validatePromotionReceipt(wrongLauncher), /launcher.*identity|smoke binding/i);
}

{
  const { adapter, state } = makeAdapter({ killAfter: 'terminal_success' });
  expectInterrupted(() => run(adapter, '/receipts/release-assets.json'));
  state.releaseAssets[0].digest = differentHex(state.releaseAssets[0].digest);
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
  assert.throws(
    () => validatePromotionJournal(chain, state.refs.get(TRANSACTION_REF)),
    /retry.*restored|retryable|authorization/i,
  );
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
      '--plugin',
      'session-relay',
      '--source-proof',
      proofPath,
      '--source-proof-sha256',
      hash(proofBytes),
      '--publication',
      publicationPath,
      '--publication-sha256',
      hash(publicationBytes),
      '--public-release',
      publicReleasePath,
      '--public-release-sha256',
      hash(publicReleaseBytes),
      '--docks-kit-release',
      PUBLIC_TAG,
      '--expected-origin-main',
      PROMOTED_COMMIT,
      '--receipt-out',
      receiptPath,
      RELEASE_VERSION,
    ];
    await assert.rejects(
      dispatchSessionRelayRelease(['--promote-reviewed', ...common]),
      /output already exists/i,
      'non-resume modes must reject an existing receipt output before their handler',
    );
    await assert.rejects(
      dispatchSessionRelayRelease([
        '--resume-promotion',
        '--plugin',
        'session-relay',
        '--transaction-ref',
        TRANSACTION_REF,
        ...common.slice(2),
      ]),
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
    (evidence) => {
      evidence.launcher_sha256 = differentHex(evidence.launcher_sha256);
    },
    (evidence) => {
      evidence.docks_kit_asset.digest = differentHex(evidence.docks_kit_asset.digest);
    },
    (evidence) => {
      evidence.launcher_version = '';
    },
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
    assert.notEqual(
      runGit(cold, ['cat-file', '-e', tip], false).status,
      0,
      'cold fixture must start without authoritative objects',
    );
    const journal = readPromotionJournalFromRepository(TRANSACTION_REF, tip, cold);
    assert.equal(journal.at(-1).commit, tip);
    assert.deepEqual(journal.at(-1).entry, { kind: 'cold-journal' });
    assert.equal(runGit(cold, ['cat-file', '-e', tip]).status, 0, 'journal read must fetch before rev-list/show');
    assert.equal(fetchPromotionAuthoritativeRef('refs/heads/main', tip, 'refs/session-relay-release/main', cold), tip);
    runGit(seed, ['commit', '--allow-empty', '-m', canonicalize({ kind: 'cold-journal-2' })]);
    const movedTip = runGit(seed, ['rev-parse', 'HEAD']).stdout.trim();
    runGit(seed, ['push', 'origin', `HEAD:${TRANSACTION_REF}`]);
    const movedJournal = readPromotionJournalFromRepository(TRANSACTION_REF, tip, cold);
    assert.equal(
      movedJournal.at(-1).commit,
      movedTip,
      'a single ref movement must requery, refetch, and read the new exact tip',
    );
    assert.deepEqual(movedJournal.at(-1).entry, { kind: 'cold-journal-2' });
    runGit(seed, ['push', 'origin', `:${TRANSACTION_REF}`]);
    assert.throws(
      () => readPromotionJournalFromRepository(TRANSACTION_REF, tip, cold),
      (error) =>
        /deleted|fetch authoritative/.test(error.message) && ['failure', 'manual_incident'].includes(error.outcome),
    );
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

await assert.rejects(
  dispatchSessionRelayRelease(['--emit-public-request', '--plugin', 'session-relay', RELEASE_VERSION]),
  /missing required option: --publication/i,
  'emit-public-request CLI mode must be recognized',
);
await assert.rejects(
  dispatchSessionRelayRelease(['--verify-public-release', '--plugin', 'session-relay', RELEASE_VERSION]),
  /missing required option: --request/i,
  'verify-public-release CLI mode must be recognized',
);
await assert.rejects(
  dispatchSessionRelayRelease([
    '--promote-reviewed',
    '--plugin',
    'session-relay',
    RELEASE_VERSION,
    '--source-proof',
    '/proof.json',
    '--source-proof-sha256',
    DIGEST('1'),
    '--publication',
    '/publication.json',
    '--publication-sha256',
    DIGEST('2'),
    '--public-release',
    '/public-release.json',
    '--public-release-sha256',
    DIGEST('3'),
    '--docks-kit-release',
    PUBLIC_TAG,
    '--expected-origin-main',
    PROMOTED_COMMIT,
    '--receipt-out',
    '/promotion.json',
    '--repair-prepush',
  ]),
  /repair-prepush.*repair-implementation-commit.*together/i,
  'repair-prepush CLI mode must require its committed repair identity',
);

await assert.rejects(
  dispatchSessionRelayRelease(['--rebind-promotion-evidence', '--plugin', 'session-relay', CURRENT_RELEASE_VERSION]),
  /missing required option: --source-proof/i,
  'rebind-promotion-evidence CLI mode must be recognized',
);

const promotionEvidenceRebindArgv = [
  '--rebind-promotion-evidence',
  '--plugin',
  'session-relay',
  '--source-proof',
  '/proof.json',
  '--source-proof-sha256',
  DIGEST('1'),
  '--publication',
  '/publication.json',
  '--publication-sha256',
  DIGEST('2'),
  '--public-release',
  '/public-release.json',
  '--public-release-sha256',
  DIGEST('3'),
  '--promotion',
  '/promotion.json',
  '--promotion-sha256',
  RETAINED_PROMOTION_SHA256,
  '--receipt-out',
  '/promotion-evidence-rebind.json',
  CURRENT_RELEASE_VERSION,
];

for (const [pathName, digestName] of [
  ['source-proof', 'source-proof-sha256'],
  ['publication', 'publication-sha256'],
  ['public-release', 'public-release-sha256'],
  ['promotion', 'promotion-sha256'],
]) {
  const nonAdjacent = [...promotionEvidenceRebindArgv];
  const digestIndex = nonAdjacent.indexOf(`--${digestName}`);
  const digestPair = nonAdjacent.splice(digestIndex, 2);
  nonAdjacent.splice(nonAdjacent.length - 1, 0, ...digestPair);
  await assert.rejects(
    dispatchSessionRelayRelease(nonAdjacent),
    new RegExp(`${pathName}.*immediately followed|${pathName}.*adjacent`, 'i'),
    `rebind CLI must keep --${pathName}/--${digestName} adjacent`,
  );
}

await assert.rejects(
  dispatchSessionRelayRelease([
    ...promotionEvidenceRebindArgv.slice(0, -1),
    '--publication',
    '/duplicate.json',
    CURRENT_RELEASE_VERSION,
  ]),
  /duplicate option: --publication/i,
  'rebind CLI must reject duplicate options',
);
await assert.rejects(
  dispatchSessionRelayRelease([
    ...promotionEvidenceRebindArgv.slice(0, -1),
    '--repair-prepush',
    CURRENT_RELEASE_VERSION,
  ]),
  /unknown option.*repair-prepush/i,
  'rebind CLI must reject mutation options',
);
await assert.rejects(
  dispatchSessionRelayRelease([
    ...promotionEvidenceRebindArgv.slice(0, -1),
    '--version',
    CURRENT_RELEASE_VERSION,
    CURRENT_RELEASE_VERSION,
  ]),
  /unknown option.*--version/i,
  'rebind CLI must keep the version positional-only',
);
await assert.rejects(
  dispatchSessionRelayRelease([
    '--rebind-promotion-evidence',
    '--promote-reviewed',
    ...promotionEvidenceRebindArgv.slice(1),
  ]),
  /exactly one release mode/i,
  'rebind CLI must be exclusive from promotion mode',
);
await assert.rejects(
  dispatchSessionRelayRelease([...promotionEvidenceRebindArgv.slice(0, -1), RELEASE_VERSION]),
  new RegExp(`rebind-promotion-evidence.*only valid.*${CURRENT_RELEASE_VERSION.replaceAll('.', String.raw`\.`)}`, 'i'),
  'rebind CLI must reject the historical release generation',
);

{
  const priorFixture = process.env.SESSION_RELAY_RELEASE_FIXTURE;
  const priorReport = process.env.SESSION_RELAY_RELEASE_REPORT;
  try {
    process.env.SESSION_RELAY_RELEASE_FIXTURE = '/fixture-must-not-be-read.json';
    process.env.SESSION_RELAY_RELEASE_REPORT = '/report-must-not-be-written.json';
    await assert.rejects(
      dispatchSessionRelayRelease(promotionEvidenceRebindArgv),
      /unavailable in fixture mode|cannot be simulated/i,
      'rebind CLI fixture interception must fail closed instead of silently dropping the receipt',
    );
  } finally {
    if (priorFixture === undefined) delete process.env.SESSION_RELAY_RELEASE_FIXTURE;
    else process.env.SESSION_RELAY_RELEASE_FIXTURE = priorFixture;
    if (priorReport === undefined) delete process.env.SESSION_RELAY_RELEASE_REPORT;
    else process.env.SESSION_RELAY_RELEASE_REPORT = priorReport;
  }
}

{
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-rebind-cli-custody-'));
  const output = path.join(directory, 'promotion-evidence.json');
  try {
    fs.writeFileSync(output, '{}', { mode: 0o600 });
    const argv = [...promotionEvidenceRebindArgv];
    argv[argv.indexOf('/promotion-evidence-rebind.json')] = output;
    await assert.rejects(
      dispatchSessionRelayRelease(argv),
      /output already exists/i,
      'rebind CLI must reject occupied output custody before receipt reads',
    );
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

assertCurrentReleasePromotionContract();

process.stdout.write('release promotion contract: OK\n');
