#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const plugin = path.resolve(here, '..');
const repo = path.resolve(plugin, '..', '..');
const fixture = JSON.parse(fs.readFileSync(path.join(here, 'fixtures', 'reentry-inventory.json'), 'utf8'));
assert.equal(fixture.schema_version, 2);

const rustFiles = fs.readdirSync(path.join(plugin, 'rust', 'src'))
  .filter((name) => name.endsWith('.rs'))
  .map((name) => path.join(plugin, 'rust', 'src', name));
const sources = rustFiles.map((file) => ({
  file: path.relative(repo, file),
  text: fs.readFileSync(file, 'utf8'),
}));

function functionRanges(source) {
  const pattern = /\bfn\s+([a-zA-Z0-9_]+)\s*\(/g;
  const starts = [...source.text.matchAll(pattern)].map((match) => ({ name: match[1], start: match.index }));
  return starts.map((entry, index) => {
    const end = starts[index + 1]?.start ?? source.text.length;
    return { ...entry, end, text: source.text.slice(entry.start, end) };
  });
}

const rangesByFile = new Map(sources.map((source) => [source.file, functionRanges(source)]));

function containingFunction(source, offset) {
  return rangesByFile.get(source.file).find((range) => range.start <= offset && offset < range.end);
}

function sourceByName(file) {
  const source = sources.find((candidate) => candidate.file === file);
  assert.ok(source, `inventory source is missing: ${file}`);
  return source;
}

function functionByName(source, name) {
  const matches = rangesByFile.get(source.file).filter((range) => range.name === name);
  assert.equal(matches.length, 1, `${source.file}: expected exactly one function named ${name}`);
  return matches[0];
}

function compileFail() {
  const manifest = path.join(here, 'fixtures', 'lifecycle-capability-bypass', 'Cargo.toml');
  const target = fs.mkdtempSync(path.join(os.tmpdir(), 'relay-capability-compile-fail-'));
  try {
    for (const name of ['guardless', 'wrong-target', 'fence-reentry', 'reentry-fence']) {
      const run = spawnSync('cargo', ['check', '--manifest-path', manifest, '--bin', name], {
        cwd: path.join(plugin, 'rust'),
        encoding: 'utf8',
        env: { ...process.env, CARGO_TARGET_DIR: target },
      });
      assert.notEqual(run.status, 0, `${name} unexpectedly compiled`);
      assert.match(run.stderr, /drain_with_guard|drain_prior_operations|mismatched types|arguments?/, `${name} failed for an unrelated reason:\n${run.stderr}`);
      console.log(`PASS compile_fail bin=${name}`);
    }
  } finally {
    fs.rmSync(target, { recursive: true, force: true });
  }
}

if (process.argv.includes('--compile-fail')) {
  compileFail();
  process.exit(0);
}

const findings = [];
for (const rule of fixture.forbidden_calls) {
  const regex = new RegExp(rule.pattern, 'g');
  for (const source of sources) {
    for (const match of source.text.matchAll(regex)) {
      const line = source.text.slice(0, match.index).split('\n').length;
      findings.push(`${source.file}:${line}: ${match[0]} — ${rule.reason}`);
    }
  }
}
assert.deepEqual(findings, [], `unguarded lifecycle mutators remain:\n${findings.join('\n')}`);

const lifecycleSource = sourceByName('plugins/session-relay/rust/src/lifecycle.rs');
assert.doesNotMatch(
  lifecycleSource.text,
  /admit_operation_with_appserver/,
  'app-server selector must be materialized before ordinary sealed admission',
);
const cliRun = functionByName(sourceByName('plugins/session-relay/rust/src/cli.rs'), 'run');
assert.match(cliRun.text, /RELAY_APP_SERVER/, 'wake env fallback is missing');
assert.match(
  cliRun.text,
  /store::register\([\s\S]*OperationKind::WakeAppServer/,
  'wake fallback must materialize Entry authority before WakeAppServer admission',
);
const drain = functionByName(sourceByName('plugins/session-relay/rust/src/store.rs'), 'drain_with_guard');
assert.match(drain.text, /guard\.with_authorized/, 'mailbox validation and removal must share one store lock');
const rollback = functionByName(sourceByName('plugins/session-relay/rust/src/store.rs'), 'rollback');
assert.doesNotMatch(rollback.text, /recipient|session_id|target/, 'receipt rollback may not accept an independent target');
assert.match(rollback.text, /self\.raw[\s\S]*push_str\(&current\)/, 'receipt rollback must restore exact original lines before newer mail');
const appserverSource = sourceByName('plugins/session-relay/rust/src/appserver.rs');
const guardedRequest = functionByName(appserverSource, 'request_with_guard');
assert.match(guardedRequest.text, /Duration::from_secs\(RPC_TIMEOUT_SECS\)/, 'guarded RPC must preserve the normal timeout');
assert.match(guardedRequest.text, /recv_text_with_guard/, 'guarded RPC responses must poll lifecycle cancellation');
assert.match(guardedRequest.text, /BeforeSend[\s\S]*AfterSend/, 'guarded RPC must retain its sent boundary');
const guardedReceive = functionByName(appserverSource, 'recv_text_with_guard');
assert.match(guardedReceive.text, /authorize_use[\s\S]*parse_frame/, 'buffered frames must reauthorize before parsing');
const guardedConnect = functionByName(appserverSource, 'connect_with_guard');
assert.match(guardedConnect.text, /connect_checked/, 'connect and HTTP upgrade must use the guard-aware poller');

const threadState = functionByName(
  sourceByName('plugins/session-relay/rust/src/appserver.rs'),
  'thread_state',
);
assert.doesNotMatch(
  threadState.text,
  /"thread\/resume"/,
  'appserver::thread_state is classified ReadOnly but still mutates via thread/resume',
);
assert.match(threadState.text, /read_status/, 'appserver::thread_state must remain a real thread/read observation');

const processPatterns = [
  { name: 'create', regex: /\bCommand::new\(/g },
  { name: 'spawn', regex: /\.spawn\(\)/g },
  { name: 'signal', regex: /\.kill\(\)/g },
];
const processRows = [];
const processClasses = new Map(
  Object.entries(fixture.process_function_classes).flatMap(([kind, names]) =>
    names.map((name) => [name, kind]),
  ),
);
for (const source of sources) {
  for (const pattern of processPatterns) {
    for (const match of source.text.matchAll(pattern.regex)) {
      const owner = containingFunction(source, match.index);
      assert.ok(owner, `${source.file}: process ${pattern.name} is outside a function`);
      const classification = processClasses.get(owner.name);
      assert.ok(
        classification,
        `${source.file}:${source.text.slice(0, match.index).split('\n').length}: unclassified process ${pattern.name} in ${owner.name}`,
      );
      processRows.push({ source, match, owner, classification, operation: pattern.name });
    }
  }
}
assert.ok(processRows.length > 0, 'source-derived process inventory is empty');

for (const helper of fixture.guarded_helpers) {
  let calls = 0;
  const callPattern = new RegExp(`\\b${helper.name}\\(`, 'g');
  for (const source of sources) {
    for (const match of source.text.matchAll(callPattern)) {
      const owner = containingFunction(source, match.index);
      assert.ok(owner, `${source.file}: guarded helper ${helper.name} is outside a function`);
      if (owner.name === helper.name) continue;
      calls += 1;
      assert.ok(
        helper.allowed_callers.includes(owner.name),
        `${source.file}: guarded helper ${helper.name} called from unguarded ${owner.name}`,
      );
    }
  }
  assert.ok(calls > 0, `guarded helper is stale or unused: ${helper.name}`);
}

const appserverPatterns = [
  { name: 'resume', regex: /"thread\/resume"/g },
  { name: 'inject', regex: /"thread\/inject_items"/g },
  { name: 'start-turn', regex: /"turn\/start"/g },
  { name: 'start-thread', regex: /"thread\/start"/g },
];
const appserverRows = [];
const appserverClasses = new Map(
  Object.entries(fixture.appserver_function_classes).flatMap(([kind, names]) =>
    names.map((name) => [name, kind]),
  ),
);
for (const source of sources) {
  for (const pattern of appserverPatterns) {
    for (const match of source.text.matchAll(pattern.regex)) {
      const owner = containingFunction(source, match.index);
      assert.ok(owner, `${source.file}: app-server ${pattern.name} is outside a function`);
      const classification = appserverClasses.get(owner.name);
      assert.ok(
        classification,
        `${source.file}:${source.text.slice(0, match.index).split('\n').length}: unclassified app-server ${pattern.name} in ${owner.name}`,
      );
      appserverRows.push({ source, match, owner, classification, operation: pattern.name });
    }
  }
}
assert.ok(appserverRows.length > 0, 'source-derived app-server inventory is empty');

for (const birth of fixture.births) {
  const source = sourceByName(birth.file);
  const owner = functionByName(source, birth.function);
  assert.ok(owner.text.includes(birth.anchor), `${birth.id}: birth anchor is missing or stale`);
  for (const evidence of birth.evidence) {
    assert.ok(source.text.includes(evidence), `${birth.id}: creates-new evidence is missing: ${evidence}`);
  }
  const classified = [...processRows, ...appserverRows].filter(
    (row) => row.source.file === birth.file
      && row.owner.name === birth.function
      && row.classification === 'non_reentry_creation',
  );
  assert.ok(classified.length > 0, `${birth.id}: no source-derived NON-REENTRY CREATION row`);
  console.log(`PASS birth_inventory id=${birth.id} creates_new=1 rows=${classified.length}`);
}
for (const api of fixture.guarded_apis) {
  const count = sources.reduce((total, source) => total + source.text.split(api).length - 1, 0);
  assert.ok(count > 0, `guarded API is stale/missing: ${api}`);
}
for (const api of fixture.read_only_apis) {
  const count = sources.reduce((total, source) => total + source.text.split(api).length - 1, 0);
  assert.ok(count > 0, `read-only inventory entry is stale/missing: ${api}`);
}
const mutatorCount = fixture.guarded_apis.reduce(
  (total, api) => total + sources.reduce((sum, source) => sum + source.text.split(api).length - 1, 0),
  0,
);
assert.ok(mutatorCount > fixture.guarded_apis.length, 'source-derived mutator count is empty or fixture-only');
const sourceDerivedCount = mutatorCount + processRows.length + appserverRows.length;
console.log(
  `PASS reentry_inventory source_derived=${sourceDerivedCount} mutators=${mutatorCount} process=${processRows.length} appserver=${appserverRows.length} births=${fixture.births.length}`,
);
