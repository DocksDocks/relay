import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';

const MANIFEST = 'plugins/session-relay/.claude-plugin/plugin.json';
const SEMVER = /^\d+\.\d+\.\d+$/;

// The shipped manifest is the source of truth for which version reaches a
// consumer, so the release contract suites derive the current release identity
// from it rather than restating a literal in each file. That turns "the suite
// pins a version" into the stronger claim it was reaching for: the lane's
// receipts must carry the version that actually ships. `ci.mjs` already holds
// the two plugin manifests and the marketplace entry in lockstep, and
// `distribution-contract.mjs` keeps the deliberate literal pins on
// `Cargo.lock` and the core `VERSION` declaration so a bump stays loud.
export function resolveShippedRelayVersion(repoRoot) {
  const manifest = JSON.parse(fs.readFileSync(path.join(repoRoot, MANIFEST), 'utf8'));
  assert.match(manifest.version ?? '', SEMVER, `${MANIFEST} must declare a semver version`);
  return { version: manifest.version, tag: `session-relay--v${manifest.version}` };
}
