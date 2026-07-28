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
