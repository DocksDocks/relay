#!/usr/bin/env node
// Contract for the release-instance separation.
//
// The Session Relay release lane must carry protocol logic only. Every value that
// identifies one particular release attempt - run ids, commits, receipt digests, plan
// paths - belongs in a per-version instance file. The live release version comes from
// exact manifest/catalog agreement and selects that instance without scanning for a maximum.
//
// Cases:
//   (default)         scan the lane and require zero identity literals
//   --case modules    the same scan, reported and asserted per module
//   --case validator  the instance validator rejects four malformed shapes distinctly
//   --case coverage   every literal in the FROZEN pre-migration inventory maps to
//                     exactly one schema field
//   --freeze          write the frozen inventory from today's scan
//
// The inventory is frozen deliberately. Migration deletes the very literals coverage
// has to reason about, so a coverage case that re-scanned the live lane would pass on
// an empty set once the work was done. `--case coverage` therefore asserts the frozen
// census still matches the counts the plan recorded before it maps anything, and
// `--freeze` refuses to overwrite a census that already matches the plan.
//
// The escaped-pin check keys on the module and the occurrence count, never on a line
// number: migration deletes constants above the pin, so its line moves while the
// property being asserted - exactly one, in promotion, not naming the current version -
// does not.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import url from 'node:url';
import { byName, CLAUDE_MARKETPLACE, claudeManifest, codexManifest } from '../../../scripts/lib/plugins.mjs';
import {
  INSTANCE_DIR,
  loadReleaseInstance,
  PLUGIN,
  VERSION,
} from '../../../scripts/lib/session-relay-release-core.mjs';
import { validateReleaseInstance } from '../../../scripts/lib/session-relay-release-instances/schema.mjs';

const HERE = path.dirname(url.fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(HERE, '../../..');
const LANE_DIR = path.join(REPO_ROOT, 'scripts/lib');
const INVENTORY = path.join(HERE, 'fixtures/release-identity-inventory.json');
const CURRENT_PLUGIN = byName(PLUGIN);
assert.notEqual(CURRENT_PLUGIN, null, 'Session Relay plugin descriptor is missing');
const currentClaudeManifest = JSON.parse(fs.readFileSync(path.join(REPO_ROOT, claudeManifest(CURRENT_PLUGIN)), 'utf8'));
const currentCodexManifest = JSON.parse(fs.readFileSync(path.join(REPO_ROOT, codexManifest(CURRENT_PLUGIN)), 'utf8'));
const currentMarketplace = JSON.parse(fs.readFileSync(path.join(REPO_ROOT, CLAUDE_MARKETPLACE), 'utf8'));
const currentMarketplaceEntries = currentMarketplace.plugins.filter(({ name }) => name === PLUGIN);
assert.equal(currentMarketplaceEntries.length, 1, 'Claude marketplace must contain exactly one Session Relay entry');
assert.deepEqual(
  [
    { name: currentClaudeManifest.name, version: currentClaudeManifest.version },
    { name: currentCodexManifest.name, version: currentCodexManifest.version },
    { name: currentMarketplaceEntries[0].name, version: currentMarketplaceEntries[0].version },
  ],
  [
    { name: PLUGIN, version: VERSION },
    { name: PLUGIN, version: VERSION },
    { name: PLUGIN, version: VERSION },
  ],
  'live Session Relay version is not derived from exact Claude, Codex, and marketplace agreement',
);

// The seven modules that make up the lane.
const MODULES = [
  'session-relay-release.mjs',
  'session-relay-release-core.mjs',
  'session-relay-release-cli.mjs',
  'session-relay-release-preparation.mjs',
  'session-relay-release-promotion.mjs',
  'session-relay-release-publication.mjs',
  'session-relay-release-fixture.mjs',
];

// The four quoted-literal identity classes, stated once in the plan's Context table.
const CLASSES = Object.freeze({
  uuid: /'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}'/g,
  commit40: /'[0-9a-f]{40}'/g,
  digest64: /'[0-9a-f]{64}'/g,
  planpath: /'docs\/plans\/(?:active|finished)\/[^']*'/g,
});

// A backslash-escaped version inside a regex literal, which a quoted-literal scan cannot
// see. Exactly one is permitted lane-wide: the historical 0.13 pin.
const ESCAPED_IDENT = /0\\\.[0-9]+\\\.[0-9]+/g;
const PERMITTED_ESCAPED_PIN = 'session-relay-release-promotion.mjs';

// The pre-migration census recorded in the plan. STOP condition 5: if a scan disagrees
// with these, re-measure - never edit the census to match the scan.
const CENSUS = Object.freeze({ uuid: 6, commit40: 6, digest64: 15, planpath: 10 });

const EXPECTED_SINGLE = 30;
const EXPECTED_MULTI_ROLE = Object.freeze([
  '064e08a437e587d6f5600788754a8af2cc3d5800adfdcb520b7399c7162ed3bb => legacy_0_13.pinned_completion.settledStateSha256 + legacy_0_13.pinned_completion_state.state_sha256',
  '5275399617cf4812a55523aa30606e8a1aad34bf0d36bdf1ee40de3f2f5ebbbf => legacy_0_13.pinned_completion.planInputSha256 + legacy_0_13.pinned_completion_state.current_input_sha256 + legacy_0_13.pinned_completion_state.initial_input_sha256',
  '88732ba0-ef06-411b-a31c-93705ccefb27 => current_attempt.docks_run_id + retained_promotion.docks_run_id',
  'cd8ec18d-fed0-4063-b49d-812d0f5bda05 => legacy_0_13.pinned_completion.seriesId + legacy_0_13.pinned_completion_state.series_id',
  'docs/plans/active/session-relay-correlated-results-release-remediation-v4.md => current_attempt.docks_plan_path + retained_promotion.docks_plan_path',
  'docs/plans/active/session-relay-linux-workspace-publication.md => authorized_base.authorized_base_to_promoted_paths + authorized_base.shipped_to_promoted_paths',
  'docs/plans/active/session-relay-linux-workspace-recertification.md => current_attempt.release_plan_path + legacy_0_13.pinned_completion_state.plan_path',
]);

function scanLane() {
  const perModule = [];
  const dedup = { uuid: new Set(), commit40: new Set(), digest64: new Set(), planpath: new Set() };
  const escaped = [];

  for (const name of MODULES) {
    const file = path.join(LANE_DIR, name);
    assert.equal(fs.existsSync(file), true, `lane module is missing: ${name}`);
    const text = fs.readFileSync(file, 'utf8');
    const row = { module: name, total: 0 };
    for (const [cls, pattern] of Object.entries(CLASSES)) {
      const hits = text.match(pattern) ?? [];
      row[cls] = hits.length;
      row.total += hits.length;
      for (const hit of hits) dedup[cls].add(hit);
    }
    perModule.push(row);
    for (const hit of text.match(ESCAPED_IDENT) ?? []) escaped.push({ module: name, literal: hit });
  }

  return { perModule, dedup, escaped };
}

function reportCounts({ perModule, dedup, escaped }) {
  for (const row of perModule) {
    console.log(
      `  ${row.module.padEnd(40)} uuid ${row.uuid}  commit40 ${row.commit40}  digest64 ${row.digest64}  planpath ${row.planpath}  total ${row.total}`,
    );
  }
  console.log(
    `  deduplicated: ${dedup.uuid.size} uuid, ${dedup.commit40.size} commit40, ${dedup.digest64.size} digest64, ${dedup.planpath.size} planpath`,
  );
  console.log(`  escapedident: ${escaped.length} occurrence(s)`);
}

function assertEscapedPin(escaped) {
  assert.equal(
    escaped.length,
    1,
    `exactly one escaped version pattern is permitted lane-wide, found ${escaped.length}`,
  );
  assert.equal(
    escaped[0].module,
    PERMITTED_ESCAPED_PIN,
    `the permitted escaped pin lives in ${PERMITTED_ESCAPED_PIN}, found it in ${escaped[0].module}`,
  );
  const currentEscaped = VERSION.split('.').join('\\.');
  assert.equal(
    escaped[0].literal.includes(currentEscaped),
    false,
    `an escaped pattern names the current version ${VERSION}, which hides identity from the quoted-literal scan`,
  );
}

function caseScan({ perModule = false } = {}) {
  const scan = scanLane();
  reportCounts(scan);

  if (perModule) {
    for (const row of scan.perModule) {
      for (const cls of Object.keys(CLASSES)) {
        assert.equal(row[cls], 0, `${row.module} still holds ${row[cls]} ${cls} literal(s)`);
      }
    }
  } else {
    for (const cls of Object.keys(CLASSES)) {
      assert.equal(scan.dedup[cls].size, 0, `the lane still holds ${scan.dedup[cls].size} distinct ${cls} literal(s)`);
    }
  }

  assertEscapedPin(scan.escaped);
  console.log(
    perModule
      ? 'release instance contract: per-module scan clean across 7 modules'
      : 'release instance contract: lane scan clean across 7 modules',
  );
}

function caseFreeze() {
  const scan = scanLane();
  reportCounts(scan);
  for (const [cls, want] of Object.entries(CENSUS)) {
    assert.equal(
      scan.dedup[cls].size,
      want,
      `census disagreement: the plan records ${want} ${cls}, the scan found ${scan.dedup[cls].size}. Re-measure; do not edit the census to match a scan.`,
    );
  }
  const payload = {
    census: Object.fromEntries(Object.entries(scan.dedup).map(([cls, set]) => [cls, set.size])),
    literals: Object.fromEntries(Object.entries(scan.dedup).map(([cls, set]) => [cls, [...set].sort()])),
    per_module: scan.perModule,
    frozen_at_version: VERSION,
  };
  fs.writeFileSync(INVENTORY, `${JSON.stringify(payload, null, 2)}\n`);
  console.log(`  frozen inventory written: ${path.relative(REPO_ROOT, INVENTORY)}`);
}
function caseValidator() {
  const rejections = [
    ['a missing key', { fixture: {} }],
    ['an unknown key', { fixture: { plan_path: 'docs/plans/active/example.md', extra: 1 } }],
    [
      'a malformed run id',
      {
        current_attempt: {
          goal_id: 'not-a-uuid',
          docks_run_id: '88732ba0-ef06-411b-a31c-93705ccefb27',
          docks_plan_path: 'docs/plans/active/example.md',
          docks_source_base: 'a'.repeat(40),
          public_run_id: '88732ba0-ef06-411b-a31c-93705ccefb27',
          release_plan_path: 'docs/plans/active/example.md',
        },
      },
    ],
    [
      'a non-40-hex commit',
      {
        authorized_base: {
          current_main_base: 'abc123',
          shipped_to_promoted_paths: ['a'],
          authorized_base_to_promoted_paths: ['b'],
        },
      },
    ],
  ];

  const messages = [];
  for (const [label, instance] of rejections) {
    let message = null;
    try {
      validateReleaseInstance(instance, { source: 'probe' });
    } catch (error) {
      message = error.message;
    }
    assert.notEqual(message, null, `the validator accepted ${label}`);
    console.log(`  rejects ${label.padEnd(21)} ${message}`);
    messages.push(message);
  }
  assert.equal(new Set(messages).size, messages.length, 'two rejection messages are identical');

  // The loader caches a parsed instance. A required-group check that rode along with the
  // parse would be skipped on every later call, making it order-dependent: a caller
  // needing `current_attempt` would pass because an earlier caller loaded the same
  // version without requiring it. Prove the requirement is re-checked against the cache.
  loadReleaseInstance('0.13.0');
  assert.throws(
    () => loadReleaseInstance('0.13.0', { require: ['current_attempt'] }),
    /missing required field group current_attempt/,
    'the loader cache bypasses the required-group check',
  );

  console.log(`release instance contract: validator rejected ${messages.length} shapes distinctly`);
}

// Where each identity literal lives in the instance files. An array entry is recorded as
// a reference rather than a declaration: a list repeating a path that a scalar field
// already declares is citing that path, not defining a second home for it.
function instanceHomes() {
  const homes = new Map();
  const add = (literal, field, kind) => {
    if (!homes.has(literal)) homes.set(literal, { scalar: new Set(), reference: new Set() });
    homes.get(literal)[kind].add(field);
  };
  const walk = (value, trail, inArray) => {
    if (typeof value === 'string') {
      add(value, trail, inArray ? 'reference' : 'scalar');
      return;
    }
    if (Array.isArray(value)) {
      for (const entry of value) walk(entry, trail, true);
      return;
    }
    if (value && typeof value === 'object') {
      for (const [key, inner] of Object.entries(value)) walk(inner, trail ? `${trail}.${key}` : key, inArray);
    }
    return undefined;
  };
  for (const name of fs.readdirSync(INSTANCE_DIR).filter((n) => n.endsWith('.json'))) {
    walk(JSON.parse(fs.readFileSync(path.join(INSTANCE_DIR, name), 'utf8')), '', false);
  }
  return homes;
}

function caseCoverage() {
  assert.equal(fs.existsSync(INVENTORY), true, `the frozen inventory is missing: ${INVENTORY}`);
  const inventory = JSON.parse(fs.readFileSync(INVENTORY, 'utf8'));

  // Anti-vacuity, and the reason the inventory is frozen at all: migration deletes these
  // literals from the lane, so coverage over a live re-scan would map an empty set and
  // pass having proved nothing.
  const total = Object.values(inventory.literals).reduce((sum, list) => sum + list.length, 0);
  assert.ok(total > 0, 'the frozen inventory is empty, so coverage would be vacuous');
  for (const [cls, want] of Object.entries(CENSUS)) {
    assert.equal(
      inventory.census[cls],
      want,
      `the frozen inventory records ${inventory.census[cls]} ${cls}, the plan census says ${want}`,
    );
  }

  const homes = instanceHomes();
  const unmapped = [];
  const multiRole = [];
  let single = 0;

  for (const list of Object.values(inventory.literals)) {
    for (const quoted of list) {
      const literal = quoted.slice(1, -1);
      const home = homes.get(literal);
      if (home === undefined) {
        unmapped.push(literal);
        continue;
      }
      const declaring = home.scalar.size > 0 ? [...home.scalar] : [...home.reference];
      if (declaring.length === 1) single += 1;
      else multiRole.push({ literal, fields: declaring });
    }
  }

  assert.equal(unmapped.length, 0, `identity literals map to no schema field: ${unmapped.join(', ')}`);

  // Zero-unmapped alone would let a literal scatter across new fields unnoticed, so the
  // multiplicity split is pinned too. A9 expects each literal to map to exactly one
  // schema field; 30 of 37 do. The other 7 are one value filling two or three distinct
  // declared roles - the same run id serving as both the current attempt and the retained
  // promotion, and digests the legacy evidence records each restate. That is the lane's
  // own shape rather than schema ambiguity, and it is pinned here so a genuinely new
  // ambiguity fails instead of being absorbed into a growing report.
  const signature = multiRole.map(({ literal, fields }) => `${literal} => ${[...fields].sort().join(' + ')}`).sort();
  assert.equal(single, EXPECTED_SINGLE, `expected ${EXPECTED_SINGLE} single-home literals, found ${single}`);
  assert.deepEqual(signature, EXPECTED_MULTI_ROLE, 'the multi-role identity set changed');

  console.log(`  literals in the frozen inventory : ${total}`);
  console.log(`  mapped to exactly one field      : ${single}`);
  console.log(`  one value, several roles         : ${multiRole.length}`);
  for (const { literal, fields } of multiRole) {
    console.log(`    ${literal.slice(0, 42).padEnd(44)} ${fields.join(' + ')}`);
  }
  console.log('release instance contract: every scanned identity literal maps to a declared field');
}
// Intentional pinned oracle: do not "unify" these literals into instance-derived
// expectations. The 0.15.0 local-form value is an anomaly retained because its
// immutable plan lineage cannot be migrated.
assert.deepEqual(
  Object.fromEntries(
    ['0.14.0', '0.15.0'].map((version) => {
      const instance = loadReleaseInstance(version, { require: ['current_attempt', 'planrun_attempt'] });
      return [
        version,
        {
          current_attempt: instance.current_attempt.docks_repository_id,
          planrun_attempt: instance.planrun_attempt.docks_repository_id,
        },
      ];
    }),
  ),
  {
    '0.14.0': { current_attempt: 'DocksDocks/docks', planrun_attempt: 'DocksDocks/docks' },
    '0.15.0': {
      current_attempt: 'docks:/home/vagrant/projects/docks',
      planrun_attempt: 'docks:/home/vagrant/projects/docks',
    },
  },
  'release-instance repository-id lineage oracle changed',
);

const args = process.argv.slice(2);
const caseIndex = args.indexOf('--case');
const requested = caseIndex >= 0 ? args[caseIndex + 1] : null;

if (args.includes('--freeze')) {
  caseFreeze();
} else if (requested === null) {
  caseScan();
} else if (requested === 'modules') {
  caseScan({ perModule: true });
} else if (requested === 'validator') {
  caseValidator();
} else if (requested === 'coverage') {
  caseCoverage();
} else {
  throw new Error(`unknown case: ${requested}`);
}
