#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const plugin = path.resolve(here, '..');
const repo = path.resolve(plugin, '..', '..');
const fixturePath = path.join(here, 'fixtures', 'reentry-inventory.json');
const fixture = JSON.parse(fs.readFileSync(fixturePath, 'utf8'));
const expectedKeys = [
  'appserver_function_classes',
  'births',
  'compile_fail_bins',
  'forbidden_calls',
  'guarded_apis',
  'guarded_helpers',
  'operation_sites',
  'process_function_classes',
  'read_only_apis',
  'schema_version',
];

function rustSources(root) {
  return fs.readdirSync(root, { withFileTypes: true }).flatMap((entry) => {
    const child = path.join(root, entry.name);
    if (entry.isDirectory()) return rustSources(child);
    return entry.isFile() && entry.name.endsWith('.rs') ? [child] : [];
  });
}

const sources = rustSources(path.join(plugin, 'rust', 'src'))
  .sort()
  .map((file) => ({ file: path.relative(repo, file), text: fs.readFileSync(file, 'utf8') }));

function functionEnd(text, body) {
  let depth = 0;
  let blockCommentDepth = 0;
  for (let index = body; index < text.length; index += 1) {
    if (blockCommentDepth > 0) {
      if (text.startsWith('/*', index)) {
        blockCommentDepth += 1;
        index += 1;
      } else if (text.startsWith('*/', index)) {
        blockCommentDepth -= 1;
        index += 1;
      }
      continue;
    }
    if (text.startsWith('//', index)) {
      index = text.indexOf('\n', index + 2);
      if (index < 0) return text.length;
      continue;
    }
    if (text.startsWith('/*', index)) {
      blockCommentDepth = 1;
      index += 1;
      continue;
    }
    const raw = text.slice(index).match(/^(?:b?r)(#+)?"/);
    if (raw) {
      const terminator = `"${raw[1] ?? ''}`;
      index = text.indexOf(terminator, index + raw[0].length);
      assert.ok(index >= 0, 'unterminated raw Rust string');
      index += terminator.length - 1;
      continue;
    }
    if (text[index] === '"') {
      for (index += 1; index < text.length; index += 1) {
        if (text[index] === '\\') index += 1;
        else if (text[index] === '"') break;
      }
      continue;
    }
    const character = text.slice(index).match(/^'(?:\\.|[^\\'])'/);
    if (character) {
      index += character[0].length - 1;
      continue;
    }
    if (text[index] === '{') depth += 1;
    else if (text[index] === '}' && --depth === 0) return index + 1;
  }
  assert.fail('function has unbalanced braces');
}

function functionRanges(source) {
  return [...source.text.matchAll(/\bfn\s+([a-zA-Z0-9_]+)\s*(?:<[^>{}]*>)?\s*\(/g)].map((match) => {
    const body = source.text.indexOf('{', match.index + match[0].length);
    assert.ok(body >= 0, `${source.file}: function ${match[1]} has no body`);
    const end = functionEnd(source.text, body);
    return { name: match[1], start: match.index, end, text: source.text.slice(match.index, end) };
  });
}

const rangesByFile = new Map(sources.map((source) => [source.file, functionRanges(source)]));
const containingFunction = (source, offset) =>
  rangesByFile.get(source.file).findLast((range) => range.start <= offset && offset < range.end);

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
  const binDir = path.join(here, 'fixtures', 'lifecycle-capability-bypass', 'src', 'bin');
  const actualBins = fs
    .readdirSync(binDir)
    .filter((name) => name.endsWith('.rs'))
    .map((name) => name.slice(0, -3))
    .sort();
  assert.deepEqual(actualBins, Object.keys(fixture.compile_fail_bins).sort(), 'compile-fail bin set drifted');
  const target = fs.mkdtempSync(path.join(os.tmpdir(), 'relay-capability-compile-fail-'));
  try {
    for (const name of actualBins) {
      const run = spawnSync('cargo', ['check', '--locked', '--manifest-path', manifest, '--bin', name], {
        cwd: path.join(plugin, 'rust'),
        encoding: 'utf8',
        env: { ...process.env, CARGO_TARGET_DIR: target },
      });
      assert.notEqual(run.status, 0, `${name} unexpectedly compiled`);
      assert.ok(
        run.stderr.includes(fixture.compile_fail_bins[name]),
        `${name} failed with the wrong signature:\n${run.stderr}`,
      );
      console.log(`PASS compile_fail bin=${name}`);
    }
  } finally {
    fs.rmSync(target, { recursive: true, force: true });
  }
}

if (process.argv.includes('--compile-fail')) {
  assert.equal(fixture.schema_version, 4);
  compileFail();
  process.exit(0);
}

const operationPatterns = [
  { category: 'git', operation: 'direct_git_command', regex: /\bCommand::new\s*\(\s*"git"\s*\)/g },
  { category: 'git', operation: 'git_api', regex: /\b(?:run_git(?:_text|_bytes)?|git_env|git_command)\s*\(/g },
  { category: 'fd_transfer', operation: 'sendmsg', regex: /\b(?:libc::)?sendmsg\s*\(/g },
  { category: 'fd_transfer', operation: 'recvmsg', regex: /\b(?:libc::)?recvmsg\s*\(/g },
  { category: 'fd_transfer', operation: 'socketpair', regex: /\b(?:libc::)?socketpair\s*\(/g },
  { category: 'signal', operation: 'pidfd_send_signal', regex: /\b(?:libc::)?pidfd_send_signal\s*\(/g },
  { category: 'signal', operation: 'kill_and_wait_empty', regex: /\bkill_and_wait_empty\s*\(/g },
  { category: 'signal', operation: 'libc_kill', regex: /\blibc::kill\s*\(/g },
  { category: 'signal', operation: 'child_kill', regex: /\.kill\s*\(\s*\)/g },
  { category: 'filesystem_probe', operation: 'statx', regex: /\b(?:libc::)?statx\s*\(/g },
  { category: 'filesystem_probe', operation: 'openat2', regex: /\b(?:libc::)?openat2\s*\(/g },
  { category: 'broker', operation: 'listener_bind', regex: /\bUnixListener::bind\s*\(/g },
  { category: 'broker', operation: 'stream_connect', regex: /\bUnixStream::connect\s*\(/g },
  { category: 'process_birth', operation: 'command', regex: /\bCommand::new\s*\(/g },
  { category: 'process_birth', operation: 'spawn', regex: /\.spawn\s*\(\s*\)/g },
  { category: 'platform', operation: 'libc', regex: /\blibc::([a-zA-Z0-9_]+)\s*\(/g },
];

function runtimeText(source) {
  const tests = source.text.indexOf('#[cfg(test)]');
  return tests < 0 ? source.text : source.text.slice(0, tests);
}

function operationSites() {
  const rows = [];
  const occupied = new Set();
  const ordinals = new Map();
  for (const source of sources) {
    for (const pattern of operationPatterns) {
      for (const match of runtimeText(source).matchAll(pattern.regex)) {
        const key = `${source.file}:${match.index}`;
        if (occupied.has(key)) continue;
        const prefix = source.text.slice(Math.max(0, match.index - 8), match.index);
        if (/\bfn\s+$/.test(prefix)) continue;
        const owner = containingFunction(source, match.index);
        assert.ok(owner, `${source.file}: ${pattern.category} ${pattern.operation} is outside a function`);
        occupied.add(key);
        const operation = pattern.operation === 'libc' ? `libc_${match[1]}` : pattern.operation;
        const ordinalKey = `${source.file}::${owner.name}::${pattern.category}::${operation}`;
        const ordinal = (ordinals.get(ordinalKey) ?? 0) + 1;
        ordinals.set(ordinalKey, ordinal);
        rows.push({
          id: `${pattern.category}:${source.file}:${owner.name}:${operation}:${ordinal}`,
          file: source.file,
          function: owner.name,
          category: pattern.category,
          operation,
          anchor: match[0],
        });
      }
    }
  }
  return rows.sort((left, right) => left.id.localeCompare(right.id));
}

const actualSites = operationSites();
if (process.argv.includes('--generate')) {
  const generated = { ...fixture, schema_version: 4, operation_sites: actualSites };
  fs.writeFileSync(fixturePath, `${JSON.stringify(generated, null, 2)}\n`);
  console.log(`PASS reentry_inventory generated=${actualSites.length}`);
  process.exit(0);
}

assert.deepEqual(Object.keys(fixture).sort(), expectedKeys, 'schema-4 reentry fixture keys drifted');
assert.equal(fixture.schema_version, 4);
assert.ok(actualSites.length > 0, 'source-derived recursive operation inventory is empty');
assert.deepEqual(fixture.operation_sites, actualSites, 'recursive process/Git/platform operation inventory drifted');
assert.ok(
  actualSites.some((site) => site.category === 'process_birth'),
  'process birth inventory is empty',
);
assert.ok(
  actualSites.some((site) => site.category === 'git'),
  'Git site inventory is empty',
);
for (const category of ['broker', 'fd_transfer', 'filesystem_probe', 'platform', 'signal']) {
  assert.ok(
    actualSites.some((site) => site.category === category),
    `${category} operation inventory is empty`,
  );
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
  'app-server selector must be materialized before sealed admission',
);
const cliRun = functionByName(sourceByName('plugins/session-relay/rust/src/cli.rs'), 'run');
assert.match(cliRun.text, /RELAY_APP_SERVER/, 'wake env fallback is missing');
assert.match(
  cliRun.text,
  /store::register\([\s\S]*OperationKind::WakeAppServer/,
  'wake fallback must materialize Entry authority',
);
const drain = functionByName(lifecycleSource, 'drain_with_guard');
assert.match(drain.text, /guard\.with_authorized/, 'mailbox validation and removal must share one store lock');
const rollback = functionByName(sourceByName('plugins/session-relay/rust/src/store.rs'), 'rollback');
assert.doesNotMatch(
  rollback.text,
  /recipient|session_id|target/,
  'receipt rollback may not accept an independent target',
);
assert.match(
  rollback.text,
  /self\.raw[\s\S]*push_str\(&current\)/,
  'receipt rollback must restore exact original lines',
);
const appserverSource = sourceByName('plugins/session-relay/rust/src/appserver.rs');
const guardedRequest = functionByName(appserverSource, 'request_with_guard');
assert.match(guardedRequest.text, /Duration::from_secs\(RPC_TIMEOUT_SECS\)/, 'guarded RPC must preserve timeout');
assert.match(guardedRequest.text, /recv_text_with_guard/, 'guarded RPC must poll lifecycle cancellation');
assert.match(guardedRequest.text, /BeforeSend[\s\S]*AfterSend/, 'guarded RPC must retain its sent boundary');
assert.match(
  functionByName(appserverSource, 'recv_text_with_guard').text,
  /authorize_use[\s\S]*parse_frame/,
  'buffered frames must reauthorize before parsing',
);
assert.match(
  functionByName(appserverSource, 'connect_with_guard').text,
  /connect_checked/,
  'connect and HTTP upgrade must use guard-aware polling',
);
const threadState = functionByName(appserverSource, 'thread_state');
assert.doesNotMatch(threadState.text, /"thread\/resume"/, 'read-only thread_state still mutates');
assert.match(threadState.text, /read_status/, 'thread_state must remain a real observation');

const processClasses = new Map(
  Object.entries(fixture.process_function_classes).flatMap(([kind, owners]) => owners.map((owner) => [owner, kind])),
);
for (const site of actualSites.filter((site) => ['process_birth', 'signal'].includes(site.category))) {
  const owner = `${site.file}::${site.function}`;
  assert.ok(
    processClasses.has(owner) || processClasses.has(site.function),
    `${site.id}: process/signal owner is unclassified`,
  );
}
const appserverClasses = new Map(
  Object.entries(fixture.appserver_function_classes).flatMap(([kind, owners]) => owners.map((owner) => [owner, kind])),
);
for (const operation of ['"thread/resume"', '"thread/inject_items"', '"turn/start"', '"thread/start"']) {
  for (const source of sources) {
    const text = runtimeText(source);
    let offset = text.indexOf(operation);
    while (offset >= 0) {
      const owner = containingFunction(source, offset);
      assert.ok(owner, `${source.file}: app-server operation is outside a function`);
      assert.ok(appserverClasses.has(owner.name), `${source.file}::${owner.name}: app-server owner is unclassified`);
      offset = text.indexOf(operation, offset + operation.length);
    }
  }
}

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
        `${source.file}: ${helper.name} called from unguarded ${owner.name}`,
      );
    }
  }
  assert.ok(calls > 0, `guarded helper is stale or unused: ${helper.name}`);
}

for (const birth of fixture.births) {
  const source = sourceByName(birth.file);
  const owner = functionByName(source, birth.function);
  assert.ok(owner.text.includes(birth.anchor), `${birth.id}: birth anchor is missing or stale`);
  for (const evidence of birth.evidence)
    assert.ok(source.text.includes(evidence), `${birth.id}: creates-new evidence is missing: ${evidence}`);
  const localBirth = actualSites.some(
    (site) => site.file === birth.file && site.function === birth.function && site.category === 'process_birth',
  );
  const remoteBirth =
    owner.text.includes('"thread/start"') && appserverClasses.get(birth.function) === 'non_reentry_creation';
  assert.ok(localBirth || remoteBirth, `${birth.id}: no source-derived local or app-server birth`);
  console.log(`PASS birth_inventory id=${birth.id} creates_new=1`);
}
for (const api of [...fixture.guarded_apis, ...fixture.read_only_apis]) {
  assert.ok(
    sources.some((source) => source.text.includes(api)),
    `API inventory entry is stale/missing: ${api}`,
  );
}
console.log(`PASS reentry_inventory source_derived=${actualSites.length} births=${fixture.births.length}`);
