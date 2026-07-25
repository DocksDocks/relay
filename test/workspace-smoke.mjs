#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const plugin = path.resolve(here, '..');
const caseIndex = process.argv.indexOf('--case');
const binIndex = process.argv.indexOf('--bin');
assert.ok(
  caseIndex >= 0 && process.argv[caseIndex + 1],
  'usage: workspace-smoke.mjs --case <case> --bin <absolute-binary>',
);
assert.ok(
  binIndex >= 0 && process.argv[binIndex + 1],
  'usage: workspace-smoke.mjs --case <case> --bin <absolute-binary>',
);
const requested = process.argv[caseIndex + 1];
const bin = process.argv[binIndex + 1];
assert.ok(['single-session-compat', 'docs-contract'].includes(requested), `unknown workspace smoke case: ${requested}`);
assert.ok(path.isAbsolute(bin), '--bin must be an absolute path');
assert.equal(fs.realpathSync(bin), bin, '--bin must name the canonical fresh binary directly');
assert.notEqual(bin, path.join(plugin, 'bin', 'relay'), '--bin may not be the compatibility launcher');
assert.ok(fs.statSync(bin).isFile(), `--bin is not a regular file: ${bin}`);
fs.accessSync(bin, fs.constants.X_OK);

const run = (args, options = {}) =>
  spawnSync(bin, args, {
    cwd: options.cwd ?? plugin,
    input: options.input,
    encoding: 'utf8',
    env: options.env ?? process.env,
  });

const usage = () => {
  const result = run([]);
  assert.notEqual(result.status, 0, 'empty invocation must print usage and refuse');
  return `${result.stdout}\n${result.stderr}`;
};
function explicitFreshBinaryCorrelatedResult() {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-workspace-correlated-'));
  const repo = path.join(home, 'repo');
  fs.mkdirSync(repo);
  const env = { ...process.env, AGENT_RELAY_HOME: home, SESSION_RELAY_HOME: '' };
  const parent = '73333333-3333-4333-8333-333333333333';
  const trigger = path.join(home, 'correlated.trigger');
  const promptFile = path.join(home, 'correlated.prompt');
  const receiptFile = path.join(home, 'correlated.receipt.json');
  const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
  const git = (args, gitCwd = repo) => {
    const result = spawnSync('git', args, { cwd: gitCwd, encoding: 'utf8', env });
    assert.equal(result.status, 0, `git ${args.join(' ')} failed:\n${result.stderr}`);
    return result.stdout.trim();
  };
  const waitFor = (description, predicate, timeoutMs = 10_000) => {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (predicate()) return;
      sleep(50);
    }
    assert.fail(`timed out waiting for ${description}`);
  };
  const fanoutRecords = () =>
    Object.values(JSON.parse(fs.readFileSync(path.join(home, 'fanout-v1.json'), 'utf8')).records);
  const workerState = (sessionId) => {
    const lifecycle = JSON.parse(fs.readFileSync(path.join(home, 'lifecycle-v1.json'), 'utf8')).state;
    return Object.values(lifecycle.managed_workers ?? {}).find((worker) => worker.runtime_session_id === sessionId)
      ?.state;
  };
  const canonicalJson = (value) => {
    if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`;
    if (value !== null && typeof value === 'object') {
      return `{${Object.keys(value)
        .sort()
        .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
        .join(',')}}`;
    }
    return JSON.stringify(value);
  };

  try {
    git(['init', '-q']);
    git(['config', 'user.email', 'relay@example.test']);
    git(['config', 'user.name', 'Relay Test']);
    fs.writeFileSync(path.join(repo, 'base.txt'), 'base\n');
    git(['add', 'base.txt']);
    git(['commit', '-qm', 'base']);
    const baseCommit = git(['rev-parse', 'HEAD']);

    const hook = run(['hook'], {
      cwd: repo,
      env,
      input: `${JSON.stringify({
        session_id: parent,
        cwd: repo,
        hook_event_name: 'SessionStart',
        source: 'startup',
      })}\n`,
    });
    assert.equal(hook.status, 0, `register correlated fanout parent failed: ${hook.stderr}`);

    const stub = path.join(home, 'correlated-worker.mjs');
    fs.writeFileSync(
      stub,
      `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
const sessionIndex = process.argv.indexOf('--session-id');
const sessionId = process.argv[sessionIndex + 1];
fs.writeFileSync(process.env.STUB_PROMPT_FILE, process.argv.at(-1));
const hook = spawnSync(process.env.STUB_RELAY_BIN, ['hook'], {
  input: JSON.stringify({
    session_id: sessionId,
    cwd: process.cwd(),
    hook_event_name: 'SessionStart',
    source: 'startup',
  }),
  encoding: 'utf8',
  env: process.env,
});
if (hook.status !== 0) process.exit(20);
while (!fs.existsSync(process.env.STUB_HANDBACK_TRIGGER)) sleep(25);
const output = path.join(process.cwd(), 'result.txt');
fs.writeFileSync(output, 'correlated result\\n');
for (const args of [
  ['add', 'result.txt'],
  ['commit', '-qm', 'correlated result'],
]) {
  const result = spawnSync('git', args, { cwd: process.cwd(), encoding: 'utf8' });
  if (result.status !== 0) process.exit(21);
}
const head = spawnSync('git', ['rev-parse', 'HEAD'], {
  cwd: process.cwd(),
  encoding: 'utf8',
}).stdout.trim();
const handback = spawnSync(
  process.env.STUB_RELAY_BIN,
  ['handback', '--from', sessionId, '--status', 'completed', '--note', 'ready'],
  { encoding: 'utf8', env: process.env },
);
fs.writeFileSync(
  process.env.STUB_RECEIPT_FILE,
  JSON.stringify({ status: handback.status, stdout: handback.stdout, stderr: handback.stderr, head }),
);
if (handback.status !== 0) process.exit(22);
while (true) sleep(1000);
`,
      { mode: 0o755 },
    );
    const correlatedEnv = {
      ...env,
      RELAY_SPAWN_CMD_CLAUDE: stub,
      STUB_RELAY_BIN: bin,
      STUB_HANDBACK_TRIGGER: trigger,
      STUB_PROMPT_FILE: promptFile,
      STUB_RECEIPT_FILE: receiptFile,
    };
    const spawn = run(
      ['spawn', repo, '--fanout', '--from', parent, '--tool', 'claude', '--timeout', '5', '--', 'correlated task'],
      { cwd: repo, env: correlatedEnv },
    );
    assert.equal(spawn.status, 0, `correlated spawn failed:\n${spawn.stdout}\n${spawn.stderr}`);
    const workerId = /spawned (?:[^\s]+ \()?([0-9a-f-]{36})\)? in /.exec(spawn.stdout)?.[1];
    assert.ok(workerId, `correlated spawn did not report its session: ${spawn.stdout}`);

    const running = fanoutRecords().find((record) => record.runtime_session_id === workerId);
    assert.ok(running, 'correlated spawn must persist its fanout record');
    assert.match(running.correlation_id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    assert.match(running.request_message_id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    const prompt = fs.readFileSync(promptFile, 'utf8');
    assert.match(prompt, new RegExp(`\\bcorrelation(?: ID)?: ${running.correlation_id}\\b`, 'i'));
    const finalCommand = `${bin} handback --from ${workerId} --status completed --note "<brief note>"`;
    assert.equal(
      prompt.split(finalCommand).length - 1,
      1,
      `fanout prompt must contain the exact final correlated handback/reply command once: ${finalCommand}`,
    );
    assert.match(prompt, /This must be your final action; do not write to the worktree afterward\./);
    assert.ok(prompt.trimEnd().endsWith('correlated task'), 'correlated prompt must retain the task payload');

    fs.writeFileSync(trigger, 'go');
    waitFor('correlated handback receipt', () => fs.existsSync(receiptFile));
    waitFor('correlated exact reap after result enqueue', () => workerState(workerId) === 'TerminalReleasable');
    const handback = JSON.parse(fs.readFileSync(receiptFile, 'utf8'));
    assert.deepEqual(handback, {
      status: 0,
      stdout: `handed back ${workerId} at ${handback.head}\n`,
      stderr: '',
      head: handback.head,
    });

    const handedBack = fanoutRecords().find((record) => record.runtime_session_id === workerId);
    assert.equal(handedBack.state, 'HandedBack');
    assert.equal(handedBack.handback_head, handback.head);
    assert.equal(handedBack.worker_result_sha256?.length, 64);
    const result = handedBack.worker_result;
    assert.deepEqual(Object.keys(result).sort(), [
      'base_commit',
      'changed_paths',
      'correlation_id',
      'created_at',
      'generation',
      'handback_commit',
      'object_format',
      'parent_session_id',
      'repo_common_dir',
      'repo_dev',
      'repo_ino',
      'reservation_id',
      'result_id',
      'root_reservation_id',
      'runtime_session_id',
      'schema',
      'status',
      'summary',
      'worker_id',
    ]);
    assert.equal(result.schema, 1);
    assert.match(result.result_id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    assert.equal(result.correlation_id, running.correlation_id);
    assert.equal(result.reservation_id, handedBack.reservation_id);
    assert.equal(result.root_reservation_id, handedBack.root_reservation_id);
    assert.equal(result.parent_session_id, parent);
    assert.equal(result.worker_id, handedBack.worker_id);
    assert.equal(result.generation, handedBack.generation);
    assert.equal(result.runtime_session_id, workerId);
    assert.equal(result.repo_common_dir, handedBack.repo_common_dir);
    assert.equal(result.repo_dev, handedBack.repo_dev);
    assert.equal(result.repo_ino, handedBack.repo_ino);
    assert.equal(result.object_format, handedBack.object_format);
    assert.equal(result.base_commit, baseCommit);
    assert.equal(result.handback_commit, handback.head);
    assert.equal(result.status, 'completed');
    assert.equal(result.summary, 'ready');
    assert.deepEqual(result.changed_paths, ['result.txt']);
    assert.match(result.created_at, /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/);
    const resultSha256 = createHash('sha256').update(canonicalJson(result)).digest('hex');
    assert.equal(handedBack.worker_result_sha256, resultSha256);

    const notificationResult = run(['inbox', parent], { cwd: repo, env: correlatedEnv });
    assert.equal(notificationResult.status, 0, notificationResult.stderr);
    const notificationPayload = JSON.parse(notificationResult.stdout);
    assert.equal(notificationPayload.count, 1);
    const notification = notificationPayload.messages[0];
    assert.deepEqual(Object.keys(notification).sort(), [
      'body',
      'correlation_id',
      'created_at',
      'from_session_id',
      'id',
      'kind',
      'reply_to',
      'result_sha256',
      'schema',
      'terminal_status',
      'to_session_id',
    ]);
    assert.equal(notification.schema, 2);
    assert.match(notification.id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    assert.match(notification.created_at, /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/);
    assert.equal(notification.from_session_id, workerId);
    assert.equal(notification.to_session_id, parent);
    assert.equal(notification.correlation_id, running.correlation_id);
    assert.equal(notification.kind, 'worker_result');
    assert.equal(notification.reply_to, running.request_message_id);
    assert.equal(notification.terminal_status, 'completed');
    assert.ok(Buffer.byteLength(notification.body, 'utf8') >= 1);
    assert.ok(Buffer.byteLength(notification.body, 'utf8') <= 4096);
    assert.ok(!notification.body.includes('\0'));
    assert.equal(notification.result_sha256, resultSha256);
    const notificationEmpty = run(['inbox', parent], { cwd: repo, env: correlatedEnv });
    assert.equal(notificationEmpty.status, 0, notificationEmpty.stderr);
    assert.deepEqual(JSON.parse(notificationEmpty.stdout), { count: 0, messages: [] });

    const collect = run(['collect', workerId, '--from', parent, '--result-json'], {
      cwd: repo,
      env: correlatedEnv,
    });
    assert.equal(collect.status, 0, collect.stderr);
    assert.equal(collect.stdout, `{"result":${canonicalJson(result)},"sha256":"${resultSha256}"}\n`);
    assert.deepEqual(JSON.parse(collect.stdout), { result, sha256: resultSha256 });
    assert.equal(fs.readFileSync(path.join(repo, 'result.txt'), 'utf8'), 'correlated result\n');
    assert.equal(git(['status', '--porcelain']), '');
  } finally {
    fs.writeFileSync(trigger, 'cleanup');
    sleep(300);
    fs.rmSync(home, { recursive: true, force: true });
  }
}

function singleSessionCompat() {
  const text = usage();
  for (const grammar of [
    'spawn <dir> [--fanout|--worktree --from <session>]',
    'handback --from <session> --status completed|failed',
    'collect <session> --from <parent>',
  ]) {
    assert.ok(text.includes(grammar), `legacy grammar disappeared: ${grammar}`);
  }
  assert.ok(text.includes('workspace preserve|start|list|inspect|handback|integrate|recover|finish|abort'));
  assert.ok(!text.includes('spawn --workspace'), 'forbidden spawn --workspace alias is advertised');
  assert.ok(!text.includes('docks session'), 'forbidden docks session command is advertised');
  explicitFreshBinaryCorrelatedResult();

  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-workspace-compat-'));
  const cwd = path.join(home, 'project');
  fs.mkdirSync(cwd);
  const env = { ...process.env, AGENT_RELAY_HOME: home, SESSION_RELAY_HOME: '' };
  const session = '71111111-1111-4111-8111-111111111111';
  const invoker = '72222222-2222-4222-8222-222222222222';
  const repo = path.join(home, 'repo');
  const triggers = [];
  const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
  const git = (args, gitCwd = repo) => {
    const result = spawnSync('git', args, { cwd: gitCwd, encoding: 'utf8', env });
    assert.equal(result.status, 0, `git ${args.join(' ')} failed:\n${result.stderr}`);
    return result.stdout.trim();
  };
  const waitFor = (description, predicate, timeoutMs = 10_000) => {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (predicate()) return;
      sleep(50);
    }
    assert.fail(`timed out waiting for ${description}`);
  };
  const fanoutRecords = () =>
    Object.values(JSON.parse(fs.readFileSync(path.join(home, 'fanout-v1.json'), 'utf8')).records);
  const downgradeFanoutRecordToLegacy = (reservationId) => {
    const file = path.join(home, 'fanout-v1.json');
    const state = JSON.parse(fs.readFileSync(file, 'utf8'));
    const record = state.records[reservationId];
    assert.ok(record, `cannot fixture missing fanout record ${reservationId}`);
    for (const field of ['correlation_id', 'request_message_id', 'worker_result', 'worker_result_sha256']) {
      assert.ok(
        Object.hasOwn(record, field),
        `fresh fanout record must persist ${field} before legacy fixture downgrade`,
      );
      delete record[field];
    }
    fs.writeFileSync(file, JSON.stringify(state));
    const legacy = JSON.parse(fs.readFileSync(file, 'utf8')).records[reservationId];
    for (const field of ['correlation_id', 'request_message_id', 'worker_result', 'worker_result_sha256']) {
      assert.ok(!Object.hasOwn(legacy, field), `old-state fixture unexpectedly contains ${field}`);
    }
  };
  const workerState = (sessionId) => {
    const lifecycle = JSON.parse(fs.readFileSync(path.join(home, 'lifecycle-v1.json'), 'utf8')).state;
    return Object.values(lifecycle.managed_workers ?? {}).find((worker) => worker.runtime_session_id === sessionId)
      ?.state;
  };
  const spawnedSession = (result) => {
    assert.equal(result.status, 0, `legacy spawn failed:\n${result.stdout}\n${result.stderr}`);
    const id = /spawned (?:[^\s]+ \()?([0-9a-f-]{36})\)? in /.exec(result.stdout)?.[1];
    assert.ok(id, `legacy spawn did not report its session: ${result.stdout}`);
    return id;
  };
  const trigger = (name) => {
    const file = path.join(home, `${name}.trigger`);
    triggers.push(file);
    return file;
  };
  const receipt = (name) => path.join(home, `${name}.receipt.json`);

  try {
    const bus = run(['bus'], {
      cwd,
      env,
      input:
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}\n' +
        '{"jsonrpc":"2.0","method":"notifications/initialized"}\n' +
        '{"jsonrpc":"2.0","id":2,"method":"ping"}\n',
    });
    assert.equal(bus.status, 0, bus.stderr);
    const frames = bus.stdout
      .trim()
      .split('\n')
      .map((line) => JSON.parse(line));
    assert.equal(frames.length, 2, 'notification must not receive a reply');
    assert.equal(frames[0].result.protocolVersion, '2025-06-18');
    assert.equal(frames[1].id, 2);

    const register = run(['register', 'compat', '--id', session, '--dir', cwd, '--tool', 'codex'], { cwd, env });
    assert.equal(register.status, 0, register.stderr);
    assert.match(register.stdout, new RegExp(`registered compat \\[codex\\] -> ${session}`));

    const send = run(['send', 'compat', '--from', session, '--', 'legacy message'], { cwd, env });
    assert.equal(send.status, 0, send.stderr);
    assert.equal(send.stdout, 'queued -> compat\n');

    const inbox = run(['inbox', 'compat'], { cwd, env });
    assert.equal(inbox.status, 0, inbox.stderr);
    const payload = JSON.parse(inbox.stdout);
    assert.equal(payload.count, 1);
    assert.equal(payload.messages[0].body, 'legacy message');
    assert.equal(payload.messages[0].from, session);

    const empty = run(['inbox', 'compat'], { cwd, env });
    assert.equal(empty.status, 0, empty.stderr);
    assert.deepEqual(JSON.parse(empty.stdout), { count: 0, messages: [] });

    const ordinarySpawn = run(
      ['spawn', cwd, '--tool', 'claude', '--reply-to', 'compat', '--dry', '--', 'compatibility task'],
      { cwd, env },
    );
    assert.equal(ordinarySpawn.status, 0, ordinarySpawn.stderr);
    const spawnPlan = JSON.parse(ordinarySpawn.stdout);
    assert.equal(spawnPlan.tool, 'claude');
    assert.equal(spawnPlan.cwd, cwd);
    assert.ok(spawnPlan.args.includes('--session-id'), 'ordinary spawn must retain pre-minted session identity');
    assert.match(spawnPlan.prompt, /compatibility task/, 'ordinary spawn must retain the task payload');

    const forbidden = run(['spawn', cwd, '--workspace', '--', 'must refuse'], { cwd, env });
    assert.notEqual(forbidden.status, 0, 'spawn --workspace must remain invalid');

    fs.mkdirSync(repo);
    git(['init', '-q']);
    git(['config', 'user.email', 'relay@example.test']);
    git(['config', 'user.name', 'Relay Test']);
    fs.writeFileSync(path.join(repo, 'base.txt'), 'base\n');
    git(['add', 'base.txt']);
    git(['commit', '-qm', 'base']);
    const baseHead = git(['rev-parse', 'HEAD']);

    const hook = run(['hook'], {
      cwd: repo,
      env,
      input: `${JSON.stringify({
        session_id: invoker,
        cwd: repo,
        hook_event_name: 'SessionStart',
        source: 'startup',
      })}\n`,
    });
    assert.equal(hook.status, 0, `register legacy fanout parent failed: ${hook.stderr}`);

    const stub = path.join(home, 'legacy-worker.mjs');
    fs.writeFileSync(
      stub,
      `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
const sessionIndex = process.argv.indexOf('--session-id');
const sessionId = process.argv[sessionIndex + 1];
const hook = spawnSync(process.env.STUB_RELAY_BIN, ['hook'], {
  input: JSON.stringify({
    session_id: sessionId,
    cwd: process.cwd(),
    hook_event_name: 'SessionStart',
    source: 'startup',
  }),
  encoding: 'utf8',
  env: process.env,
});
if (hook.status !== 0) process.exit(20);
while (!fs.existsSync(process.env.STUB_HANDBACK_TRIGGER)) sleep(25);
const output = path.join(process.cwd(), process.env.STUB_OUTPUT_FILE);
fs.writeFileSync(output, process.env.STUB_OUTPUT_FILE + '\\n');
for (const args of [
  ['add', process.env.STUB_OUTPUT_FILE],
  ['commit', '-qm', 'legacy ' + process.env.STUB_OUTPUT_FILE],
]) {
  const result = spawnSync('git', args, { cwd: process.cwd(), encoding: 'utf8' });
  if (result.status !== 0) process.exit(21);
}
const head = spawnSync('git', ['rev-parse', 'HEAD'], {
  cwd: process.cwd(),
  encoding: 'utf8',
}).stdout.trim();
const handback = spawnSync(
  process.env.STUB_RELAY_BIN,
  ['handback', '--from', sessionId, '--status', 'completed', '--note', 'ready'],
  { encoding: 'utf8', env: process.env },
);
fs.writeFileSync(
  process.env.STUB_RECEIPT_FILE,
  JSON.stringify({ status: handback.status, stdout: handback.stdout, stderr: handback.stderr, head }),
);
if (handback.status !== 0) process.exit(22);
while (true) sleep(1000);
`,
      { mode: 0o755 },
    );
    const legacyEnv = {
      ...env,
      RELAY_SPAWN_CMD_CLAUDE: stub,
      STUB_RELAY_BIN: bin,
    };

    const rootTrigger = trigger('root');
    const rootReceipt = receipt('root');
    const rootId = spawnedSession(
      run(['spawn', repo, '--fanout', '--from', invoker, '--tool', 'claude', '--timeout', '5', '--', 'root task'], {
        cwd: repo,
        env: {
          ...legacyEnv,
          STUB_HANDBACK_TRIGGER: rootTrigger,
          STUB_OUTPUT_FILE: 'root.txt',
          STUB_RECEIPT_FILE: rootReceipt,
        },
      }),
    );
    const rootRecord = fanoutRecords().find((record) => record.runtime_session_id === rootId);
    assert.equal(rootRecord?.parent_session_id, invoker);
    assert.equal(rootRecord?.root_reservation_id, rootRecord?.reservation_id);
    assert.equal(rootRecord?.depth, '0');
    assert.equal(rootRecord?.base_sha, baseHead);
    assert.equal(rootRecord?.state, 'Running');
    assert.ok(fs.existsSync(rootRecord.worktree), 'top-level fanout must prepare a real worktree');
    assert.equal(git(['status', '--porcelain'], rootRecord.worktree), '');

    const leafTrigger = trigger('leaf');
    const leafReceipt = receipt('leaf');
    const leafId = spawnedSession(
      run(
        [
          'spawn',
          rootRecord.worktree,
          '--worktree',
          '--from',
          rootId,
          '--tool',
          'claude',
          '--timeout',
          '5',
          '--',
          'leaf task',
        ],
        {
          cwd: rootRecord.worktree,
          env: {
            ...legacyEnv,
            STUB_HANDBACK_TRIGGER: leafTrigger,
            STUB_OUTPUT_FILE: 'leaf.txt',
            STUB_RECEIPT_FILE: leafReceipt,
          },
        },
      ),
    );
    const leafRecord = fanoutRecords().find((record) => record.runtime_session_id === leafId);
    assert.equal(leafRecord?.parent_session_id, rootId);
    assert.equal(leafRecord?.root_reservation_id, rootRecord.reservation_id);
    assert.equal(leafRecord?.depth, '1');
    assert.equal(leafRecord?.base_sha, baseHead);
    assert.equal(leafRecord?.state, 'Running');
    assert.ok(fs.existsSync(leafRecord.worktree), 'nested worktree spawn must prepare a real worktree');
    assert.equal(git(['status', '--porcelain'], leafRecord.worktree), '');

    fs.writeFileSync(leafTrigger, 'go');
    waitFor('leaf handback receipt', () => fs.existsSync(leafReceipt));
    waitFor('leaf exact reap', () => workerState(leafId) === 'TerminalReleasable');
    const leafHandback = JSON.parse(fs.readFileSync(leafReceipt, 'utf8'));
    assert.deepEqual(leafHandback, {
      status: 0,
      stdout: `handed back ${leafId} at ${leafHandback.head}\n`,
      stderr: '',
      head: leafHandback.head,
    });
    assert.match(leafHandback.head, /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/);
    const handedBackLeaf = fanoutRecords().find((record) => record.runtime_session_id === leafId);
    assert.equal(handedBackLeaf?.state, 'HandedBack');
    assert.equal(handedBackLeaf?.handback_head, leafHandback.head);
    assert.equal(handedBackLeaf?.handback_status, 'completed');
    assert.equal(handedBackLeaf?.handback_note, 'ready');

    const collectLeaf = run(['collect', leafId, '--from', rootId], { cwd: rootRecord.worktree, env: legacyEnv });
    assert.equal(collectLeaf.status, 0, collectLeaf.stderr);
    assert.equal(collectLeaf.stdout, `collected ${leafId} into ${rootId}\n`);
    assert.equal(fs.readFileSync(path.join(rootRecord.worktree, 'leaf.txt'), 'utf8'), 'leaf.txt\n');
    assert.deepEqual(git(['ls-files'], rootRecord.worktree).split('\n'), ['base.txt', 'leaf.txt']);
    assert.equal(git(['status', '--porcelain'], rootRecord.worktree), '');
    assert.ok(!fs.existsSync(leafRecord.worktree), 'collect must remove the nested worktree');

    fs.writeFileSync(rootTrigger, 'go');
    waitFor('root handback receipt', () => fs.existsSync(rootReceipt));
    waitFor('root exact reap', () => workerState(rootId) === 'TerminalReleasable');
    const rootHandback = JSON.parse(fs.readFileSync(rootReceipt, 'utf8'));
    assert.deepEqual(rootHandback, {
      status: 0,
      stdout: `handed back ${rootId} at ${rootHandback.head}\n`,
      stderr: '',
      head: rootHandback.head,
    });
    assert.match(rootHandback.head, /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/);
    const handedBackRoot = fanoutRecords().find((record) => record.runtime_session_id === rootId);
    assert.equal(handedBackRoot?.state, 'HandedBack');
    assert.equal(handedBackRoot?.handback_head, rootHandback.head);
    assert.equal(handedBackRoot?.handback_status, 'completed');
    assert.equal(handedBackRoot?.handback_note, 'ready');
    downgradeFanoutRecordToLegacy(handedBackRoot.reservation_id);

    const collectRoot = run(['collect', rootId, '--from', invoker], { cwd: repo, env: legacyEnv });
    assert.equal(collectRoot.status, 0, collectRoot.stderr);
    assert.equal(collectRoot.stdout, `collected ${rootId} into ${invoker}\n`);
    assert.deepEqual(git(['ls-files']).split('\n'), ['base.txt', 'leaf.txt', 'root.txt']);
    assert.equal(fs.readFileSync(path.join(repo, 'base.txt'), 'utf8'), 'base\n');
    assert.equal(fs.readFileSync(path.join(repo, 'leaf.txt'), 'utf8'), 'leaf.txt\n');
    assert.equal(fs.readFileSync(path.join(repo, 'root.txt'), 'utf8'), 'root.txt\n');
    assert.equal(git(['status', '--porcelain']), '');
    assert.ok(!fs.existsSync(rootRecord.worktree), 'collect must remove the top-level worktree');
    const records = fanoutRecords();
    assert.equal(records.length, 2);
    for (const record of records) {
      assert.equal(record.state, 'Collected');
      assert.equal(record.collection_phase, 'WorktreeRemoved');
      assert.equal(record.handback_status, 'completed');
      assert.equal(record.handback_note, 'ready');
      assert.equal(record.last_error, null);
    }
  } finally {
    for (const file of triggers) fs.writeFileSync(file, 'cleanup');
    sleep(300);
    fs.rmSync(home, { recursive: true, force: true });
  }
  console.log('PASS workspace_smoke case=single-session-compat');
}

function docsContract() {
  const text = usage();
  const commands = ['preserve', 'start', 'list', 'inspect', 'handback', 'integrate', 'recover', 'finish', 'abort'];
  for (const command of commands) {
    assert.ok(text.includes(command), `runtime usage omits workspace ${command}`);
  }

  const skill = fs.readFileSync(path.join(plugin, 'skills', 'productivity', 'session-relay', 'SKILL.md'), 'utf8');
  const reference = fs.readFileSync(
    path.join(plugin, 'skills', 'productivity', 'session-relay', 'references', 'workspace.md'),
    'utf8',
  );
  const docs = `${skill}\n${reference}`;
  for (const command of commands) {
    assert.match(docs, new RegExp(`session-relay workspace ${command}\\b`), `docs omit workspace ${command}`);
  }
  for (const actor of ['owner', 'coordinator', 'worker']) {
    assert.match(docs, new RegExp(`\\b${actor}\\b`, 'i'), `workspace docs omit the ${actor} actor`);
  }
  for (const resource of ['port', 'temp_dir', 'build_dir', 'database_schema', 'log_dir', 'cache_dir']) {
    assert.match(docs, new RegExp(`\\b${resource}\\b`), `workspace docs omit resource kind ${resource}`);
  }
  for (const topic of [
    /separate (?:checkout|worktree)/i,
    /automatic(?:ally)? allocat/i,
    /active session/i,
    /lease/i,
    /crash recovery/i,
    /commit integration/i,
    /read-only/i,
    /external resources/i,
    /clone.{0,40}worktree|worktree.{0,40}clone/is,
    /managed.{0,80}unmanaged/is,
    /Linux/,
    /macOS.{0,80}(?:STOP|refus|unsupported)/is,
    /linux_cgroup_v2_pidfd/,
    /macos_pgroup_libproc/,
  ]) {
    assert.match(docs, topic, `workspace documentation topic is missing: ${topic}`);
  }
  for (const precedent of [
    'https://conductor.build/',
    'https://developers.openai.com/codex/app/',
    'https://code.claude.com/docs/en/common-workflows',
    'https://docs.cursor.com/en/background-agent',
    'https://docs.github.com/en/copilot/concepts/agents/coding-agent/about-coding-agent',
    'https://git-scm.com/docs/git-worktree',
  ]) {
    assert.ok(docs.includes(precedent), `workspace documentation precedent is missing: ${precedent}`);
  }
  for (const forbidden of [
    'docks session',
    'spawn --workspace',
    'macOS managed writing is supported',
    'controls arbitrary unmanaged same-UID',
  ]) {
    assert.ok(!docs.includes(forbidden), `workspace documentation contains forbidden claim: ${forbidden}`);
  }
  console.log('PASS workspace_smoke case=docs-contract');
}

if (requested === 'single-session-compat') singleSessionCompat();
else docsContract();
