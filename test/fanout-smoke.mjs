#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const plugin = path.resolve(here, '..');
const bin = path.join(plugin, 'rust', 'target', 'debug', 'relay');
assert.ok(fs.existsSync(bin), `missing development relay binary: ${bin}`);

const home = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-fanout-'));
const repo = path.join(home, 'repo');
const invoker = '71111111-1111-4111-8111-111111111111';
const triggers = [];
const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
const envFor = (extra = {}) => ({
  ...process.env,
  AGENT_RELAY_HOME: home,
  SESSION_RELAY_HOME: '',
  RELAY_SPAWN_CMD_CLAUDE: path.join(home, 'fanout-child.mjs'),
  STUB_RELAY_BIN: bin,
  ...extra,
});
const run = (command, args, options = {}) => {
  const result = spawnSync(command, args, {
    cwd: options.cwd,
    input: options.input,
    encoding: 'utf8',
    env: envFor(options.env),
  });
  return result;
};
const git = (args, cwd = repo) => {
  const result = run('git', args, { cwd });
  assert.equal(result.status, 0, `git ${args.join(' ')} failed:\n${result.stderr}`);
  return result.stdout.trim();
};
const relay = (args, options = {}) => run(bin, args, options);
const waitFor = (description, predicate, timeoutMs = 10_000) => {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    sleep(50);
  }
  assert.fail(`timed out waiting for ${description}`);
};
const lifecycleState = () => JSON.parse(fs.readFileSync(path.join(home, 'lifecycle-v1.json'), 'utf8')).state;
const workerState = (sessionId) =>
  Object.values(lifecycleState().managed_workers ?? {}).find((worker) => worker.runtime_session_id === sessionId)
    ?.state;
const fanoutRecords = () =>
  Object.values(JSON.parse(fs.readFileSync(path.join(home, 'fanout-v1.json'), 'utf8')).records);
const spawnId = (result) => {
  assert.equal(result.status, 0, `fanout spawn failed:\n${result.stdout}\n${result.stderr}`);
  const id = /spawned (?:[^\s]+ \()?([0-9a-f-]{36})\)? in /.exec(result.stdout)?.[1];
  assert.ok(id, `missing spawned session id: ${result.stdout}`);
  return id;
};
const triggerPath = (name) => {
  const trigger = path.join(home, `${name}.trigger`);
  triggers.push(trigger);
  return trigger;
};

fs.mkdirSync(repo);
git(['init', '-q']);
git(['config', 'user.email', 'relay@example.test']);
git(['config', 'user.name', 'Relay Test']);
fs.writeFileSync(path.join(repo, 'base.txt'), 'base\n');
git(['add', 'base.txt']);
git(['commit', '-qm', 'base']);

const hook = relay(['hook'], {
  input: JSON.stringify({
    session_id: invoker,
    cwd: repo,
    hook_event_name: 'SessionStart',
    source: 'startup',
  }),
});
assert.equal(hook.status, 0, `register invoker hook failed: ${hook.stderr}`);

const stub = path.join(home, 'fanout-child.mjs');
fs.writeFileSync(
  stub,
  `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
const sessionIndex = process.argv.indexOf('--session-id');
const sessionId = process.argv[sessionIndex + 1];
const event = JSON.stringify({ session_id: sessionId, cwd: process.cwd(), hook_event_name: 'SessionStart', source: 'startup' });
const hook = spawnSync(process.env.STUB_RELAY_BIN, ['hook'], { input: event, encoding: 'utf8', env: process.env });
if (hook.status !== 0) process.exit(20);
while (!fs.existsSync(process.env.STUB_HANDBACK_TRIGGER)) sleep(25);
const output = path.join(process.cwd(), process.env.STUB_OUTPUT_FILE);
fs.writeFileSync(output, process.env.STUB_OUTPUT_FILE + '\\n');
for (const args of [['add', process.env.STUB_OUTPUT_FILE], ['commit', '-qm', 'fanout ' + process.env.STUB_OUTPUT_FILE]]) {
  const git = spawnSync('git', args, { cwd: process.cwd(), encoding: 'utf8' });
  if (git.status !== 0) process.exit(21);
}
const handback = spawnSync(process.env.STUB_RELAY_BIN, ['handback', '--from', sessionId, '--status', 'completed', '--note', 'ready'], { encoding: 'utf8', env: process.env });
if (handback.status !== 0) process.exit(22);
while (true) sleep(1000);
`,
  { mode: 0o755 },
);

try {
  const rootTrigger = triggerPath('root');
  const rootId = spawnId(
    relay(['spawn', repo, '--fanout', '--from', invoker, '--tool', 'claude', '--timeout', '5', '--', 'root task'], {
      env: { STUB_HANDBACK_TRIGGER: rootTrigger, STUB_OUTPUT_FILE: 'root.txt' },
    }),
  );
  const rootRecord = fanoutRecords().find((record) => record.runtime_session_id === rootId);
  assert.equal(rootRecord?.depth, '0');
  assert.equal(rootRecord?.state, 'Running');

  const leaf1Trigger = triggerPath('leaf1');
  const leaf1Id = spawnId(
    relay(
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
        'leaf one',
      ],
      { env: { STUB_HANDBACK_TRIGGER: leaf1Trigger, STUB_OUTPUT_FILE: 'leaf-one.txt' } },
    ),
  );
  const leaf2Trigger = triggerPath('leaf2');
  const leaf2Id = spawnId(
    relay(
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
        'leaf two',
      ],
      { env: { STUB_HANDBACK_TRIGGER: leaf2Trigger, STUB_OUTPUT_FILE: 'leaf-two.txt' } },
    ),
  );

  const worktreesBeforeThird = fs.readdirSync(path.join(home, 'worktrees')).sort();
  const branchesBeforeThird = git(['branch', '--list', 'relay/fanout-*'], rootRecord.worktree)
    .split('\n')
    .filter(Boolean);
  const third = relay(
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
      'third leaf',
    ],
    { env: { STUB_HANDBACK_TRIGGER: triggerPath('leaf3'), STUB_OUTPUT_FILE: 'leaf-three.txt' } },
  );
  assert.notEqual(third.status, 0, 'third live leaf must be refused');
  assert.match(third.stderr, /fanout cap reached \(2 active descendants\)/);
  assert.deepEqual(fs.readdirSync(path.join(home, 'worktrees')).sort(), worktreesBeforeThird);
  assert.deepEqual(
    git(['branch', '--list', 'relay/fanout-*'], rootRecord.worktree).split('\n').filter(Boolean),
    branchesBeforeThird,
    'cap refusal happens before branch creation',
  );

  fs.writeFileSync(leaf1Trigger, 'go');
  fs.writeFileSync(leaf2Trigger, 'go');
  waitFor('first leaf exact reap', () => workerState(leaf1Id) === 'TerminalReleasable');
  waitFor('second leaf exact reap', () => workerState(leaf2Id) === 'TerminalReleasable');
  for (const leafId of [leaf1Id, leaf2Id]) {
    const collected = relay(['collect', leafId, '--from', rootId]);
    assert.equal(collected.status, 0, `leaf collect failed: ${collected.stderr}`);
  }
  assert.equal(fs.readFileSync(path.join(rootRecord.worktree, 'leaf-one.txt'), 'utf8'), 'leaf-one.txt\n');
  assert.equal(fs.readFileSync(path.join(rootRecord.worktree, 'leaf-two.txt'), 'utf8'), 'leaf-two.txt\n');

  fs.writeFileSync(rootTrigger, 'go');
  waitFor('root exact reap', () => workerState(rootId) === 'TerminalReleasable');
  const collectedRoot = relay(['collect', rootId, '--from', invoker]);
  assert.equal(collectedRoot.status, 0, `root collect failed: ${collectedRoot.stderr}`);
  for (const file of ['leaf-one.txt', 'leaf-two.txt', 'root.txt']) {
    assert.equal(fs.readFileSync(path.join(repo, file), 'utf8'), `${file}\n`);
  }
  assert.ok(fanoutRecords().every((record) => record.state === 'Collected'));
  console.log('fanout smoke: PASS');
} finally {
  for (const trigger of triggers) fs.writeFileSync(trigger, 'cleanup');
  sleep(300);
  fs.rmSync(home, { recursive: true, force: true });
}
