import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';

const MANIFEST = 'plugin/package.json';
const SEMVER = /^\d+\.\d+\.\d+$/;

// The shipped manifest is the source of truth for which version reaches a
// consumer, so the contract suite derives the current release identity from it
// rather than restating a literal. That turns "the suite pins a version" into
// the stronger claim it was reaching for: the published assets must carry the
// version that actually ships. `test/distribution-contract.mjs` holds this
// manifest, `Cargo.toml`, and the marketplace catalog in three-way lockstep,
// so a partial bump stays loud.
export function resolveShippedRelayVersion(repoRoot) {
  const manifest = JSON.parse(fs.readFileSync(path.join(repoRoot, MANIFEST), 'utf8'));
  assert.match(manifest.version ?? '', SEMVER, `${MANIFEST} must declare a semver version`);
  return { version: manifest.version, tag: `v${manifest.version}` };
}
