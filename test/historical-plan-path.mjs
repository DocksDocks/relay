import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';

const PLAN_NAME = 'session-relay-linux-workspace-publication.md';
const ACTIVE_PLAN = `docs/plans/active/${PLAN_NAME}`;
const FINISHED_PLANS = 'docs/plans/finished';
const FINISHED_PLAN_PATTERN = /^\d{4}-\d{2}-\d{2}-session-relay-linux-workspace-publication\.md$/;

export function resolveHistoricalPublicationPlanPath(repoRoot) {
  // The plan was retired under the quarantine retirement path, so both locations are legitimate.
  if (fs.existsSync(path.join(repoRoot, ACTIVE_PLAN))) return ACTIVE_PLAN;

  const matches = fs
    .readdirSync(path.join(repoRoot, FINISHED_PLANS))
    .filter((name) => FINISHED_PLAN_PATTERN.test(name));
  assert.equal(matches.length, 1, 'exactly one finished publication plan is available as the review template');

  return `${FINISHED_PLANS}/${matches[0]}`;
}

// A release plan lives under `docs/plans/active/` while its run is `ongoing` and moves to a dated
// path under `docs/plans/finished/` when the run finishes. A contract that reads only the active
// path passes for the whole life of the release and then fails the moment the release succeeds,
// which is exactly what happened to the 0.15.0 chain. Both locations are legitimate; exactly one
// must exist.
export function resolveReleasePlanPath(repoRoot, version) {
  const activePlan = `docs/plans/active/session-relay-${version}-release.md`;
  if (fs.existsSync(path.join(repoRoot, activePlan))) return activePlan;

  const escaped = version.replaceAll('.', '\\.');
  const finishedPattern = new RegExp(`^\\d{4}-\\d{2}-\\d{2}-session-relay-${escaped}-release\\.md$`);
  const finishedDirectory = path.join(repoRoot, FINISHED_PLANS);
  const matches = fs.existsSync(finishedDirectory)
    ? fs.readdirSync(finishedDirectory).filter((name) => finishedPattern.test(name))
    : [];
  assert.equal(matches.length, 1, `exactly one Session Relay ${version} release plan must exist`);

  return `${FINISHED_PLANS}/${matches[0]}`;
}
