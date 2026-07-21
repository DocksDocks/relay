#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createFixture, createScenarioCheck, runScenarioCli } from './selftest-fixture.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));

export const EXPECTED_LABELS = [
  '--version prints the exact Cargo package version',
  'hook seeds marker + registration for both sessions (exit 0)',
  'register CLI names both sessions',
  'initialize negotiates protocol + serverInfo',
  'tools/list returns the 6 bus tools',
  'whoami resolves this session from the cwd marker',
  'roster lists both registered sessions',
  'send to agent-B reports ok + correct recipient dir',
  "message landed in agent-B's mailbox tagged with the sender (peek is read-only)",
  'hook exits 0',
  'hook injects pending mail as SessionStart additionalContext',
  'hook drained the inbox (no redelivery)',
  'send CLI queues to an explicit --id target',
  'inbox() returns then clears pending messages',
  'non-attach verbs still treat --exec as a value flag',
  'send to an unknown recipient returns isError',
  'registry carries a tool field (codex tagged; default claude)',
  'AGENT_RELAY_HOME takes precedence over SESSION_RELAY_HOME',
  'wake dispatches the codex doorbell for a codex target',
  'wake dispatches the claude doorbell for a claude target',
  'wake --dry maps --model/--effort for codex and claude targets',
  'wake --service-tier is Codex-only and emits exact Fast overrides',
  'attach always runs as a guarded spawn+wait and prints no copyable command',
  'legacy attach --exec is accepted but still uses guarded spawn+wait',
  'guarded attach inherits stdin/stdout/stderr while the relay parent waits',
  'attach strictly rejects extra operands, unknown flags, and exec after --',
  'attach refuses a missing stored dir before guarded spawn',
  'attach rejects an unresolved non-UUID id',
  'attach fails closed when the resume lock cannot be probed',
  'wake prints claude usage to stderr and keeps fixture stdout byte-identical',
  'wake prints codex usage to stderr and keeps fixture stdout byte-identical',
  'wake preserves no-trailing-newline stdout while still reporting usage',
  'wake omits usage on garbage stdout and preserves the child exit code',
];

export async function run({ bin, home, emit }) {
  const fixture = createFixture({ bin, home });
  const labels = [];
  let check;
  let waitForLifecycleCustody;
  let scenarioResult;
  let hasPrimaryError = false;
  let primaryError;
  const {
    home: HOME,
    cargoVersion: CARGO_VERSION,
    relay,
    relayBytes,
    relayJSON,
    runHook,
    runBus,
    toolJSON,
    configValues,
    peek,
  } = fixture;

  try {
    check = createScenarioCheck({ emit, labels });
    const lifecycleState = () => JSON.parse(fs.readFileSync(path.join(HOME, 'lifecycle-v1.json'), 'utf8')).state;
    const lifecycleWait = new Int32Array(new SharedArrayBuffer(4));
    waitForLifecycleCustody = () => {
      const deadline = Date.now() + 5_000;
      while (true) {
        const state = lifecycleState();
        const supervisors = Object.keys(state.lifecycle_supervisors ?? {});
        const watchdogs = Object.keys(state.lifecycle_watchdogs ?? {});
        if (supervisors.length === 0 && watchdogs.length === 0) return;
        assert.ok(Date.now() < deadline, 'detached lifecycle custody exits before scenario cleanup');
        Atomics.wait(lifecycleWait, 0, 0, 10);
      }
    };
    const dirA = path.join(HOME, 'proj-a');
    const dirB = path.join(HOME, 'proj-b');
    fs.mkdirSync(dirA, { recursive: true });
    fs.mkdirSync(dirB, { recursive: true });
    const idA = '11111111-1111-1111-1111-111111111111';
    const idB = '22222222-2222-2222-2222-222222222222';

    check('--version prints the exact Cargo package version', () => {
      const result = relay(['--version']);
      assert.equal(result.status, 0, `relay --version exited ${result.status}: ${result.stderr}`);
      assert.equal(result.stdout, `session-relay ${CARGO_VERSION}\n`);
      assert.equal(result.stderr, '');
    });

    check('hook seeds marker + registration for both sessions (exit 0)', () => {
      assert.equal(
        runHook({ session_id: idA, cwd: dirA, hook_event_name: 'SessionStart', source: 'startup' }).status,
        0,
      );
      assert.equal(
        runHook({ session_id: idB, cwd: dirB, hook_event_name: 'SessionStart', source: 'startup' }).status,
        0,
      );
    });
    check('register CLI names both sessions', () => {
      assert.equal(relay(['register', 'agent-A', '--id', idA, '--dir', dirA]).status, 0);
      assert.equal(relay(['register', 'agent-B', '--id', idB, '--dir', dirB]).status, 0);
    });

    const reqs = [
      {
        jsonrpc: '2.0',
        id: 1,
        method: 'initialize',
        params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'selftest', version: '1' } },
      },
      { jsonrpc: '2.0', method: 'notifications/initialized' },
      { jsonrpc: '2.0', id: 2, method: 'tools/list' },
      { jsonrpc: '2.0', id: 3, method: 'tools/call', params: { name: 'whoami', arguments: {} } },
      { jsonrpc: '2.0', id: 4, method: 'tools/call', params: { name: 'roster', arguments: {} } },
      {
        jsonrpc: '2.0',
        id: 5,
        method: 'tools/call',
        params: { name: 'send', arguments: { to: 'agent-B', body: 'hello from A' } },
      },
    ];
    const res = runBus(dirA, reqs);

    check('initialize negotiates protocol + serverInfo', () => {
      assert.equal(res.get(1).result.protocolVersion, '2025-06-18');
      assert.equal(res.get(1).result.serverInfo.name, 'session-relay-bus');
      assert.ok(res.get(1).result.capabilities.tools);
    });
    check('tools/list returns the 6 bus tools', () => {
      const names = res
        .get(2)
        .result.tools.map((tool) => tool.name)
        .sort();
      assert.deepEqual(names, ['discover', 'inbox', 'register', 'roster', 'send', 'whoami']);
    });
    check('whoami resolves this session from the cwd marker', () => {
      const me = toolJSON(res.get(3));
      assert.equal(me.registered, true);
      assert.equal(me.id, idA);
      assert.equal(me.name, 'agent-A');
    });
    check('roster lists both registered sessions', () => {
      const { agents } = toolJSON(res.get(4));
      assert.deepEqual(agents.map((agent) => agent.name).sort(), ['agent-A', 'agent-B']);
    });
    check('send to agent-B reports ok + correct recipient dir', () => {
      const result = toolJSON(res.get(5));
      assert.equal(result.ok, true);
      assert.equal(result.delivered_to, 'agent-B');
      assert.equal(result.recipient_dir, dirB);
    });
    check("message landed in agent-B's mailbox tagged with the sender (peek is read-only)", () => {
      const mail = peek('agent-B');
      assert.equal(mail.count, 1);
      assert.equal(mail.messages[0].body, 'hello from A');
      assert.equal(mail.messages[0].fromName, 'agent-A');
      assert.equal(peek('agent-B').count, 1);
    });

    const hookRun = runHook({ session_id: idB, cwd: dirB, hook_event_name: 'SessionStart', source: 'resume' });
    check('hook exits 0', () => assert.equal(hookRun.status, 0));
    check('hook injects pending mail as SessionStart additionalContext', () => {
      const out = JSON.parse(hookRun.stdout);
      assert.equal(out.hookSpecificOutput.hookEventName, 'SessionStart');
      assert.ok(out.hookSpecificOutput.additionalContext.includes('hello from A'));
    });
    check('hook drained the inbox (no redelivery)', () => assert.equal(peek('agent-B').count, 0));

    check('send CLI queues to an explicit --id target', () => {
      assert.equal(relay(['send', '--id', idB, '--', 'second message']).status, 0);
    });
    const res2 = runBus(dirB, [
      { jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18' } },
      { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'inbox', arguments: {} } },
    ]);
    check('inbox() returns then clears pending messages', () => {
      const box = toolJSON(res2.get(2));
      assert.equal(box.count, 1);
      assert.equal(box.messages[0].body, 'second message');
      assert.equal(peek('agent-B').count, 0);
    });
    check('non-attach verbs still treat --exec as a value flag', () => {
      const result = relay(['send', 'agent-B', '--exec', 'must-not-send']);
      assert.equal(result.status, 1);
      assert.match(result.stderr, /usage: relay send/);
      assert.equal(peek('agent-B').count, 0);
    });

    const res3 = runBus(dirA, [
      { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
      { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'send', arguments: { to: 'ghost', body: 'x' } } },
    ]);
    check('send to an unknown recipient returns isError', () => {
      assert.equal(res3.get(2).result.isError, true);
    });

    const dirC = path.join(HOME, 'proj-c');
    const idC = '33333333-3333-3333-3333-333333333333';
    fs.mkdirSync(dirC, { recursive: true });
    relay(['register', 'codex-C', '--id', idC, '--dir', dirC, '--tool', 'codex']);
    check('registry carries a tool field (codex tagged; default claude)', () => {
      const { agents } = toolJSON(
        runBus(dirA, [
          { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
          { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'roster', arguments: {} } },
        ]).get(2),
      );
      const byName = Object.fromEntries(agents.map((agent) => [agent.name, agent.tool]));
      assert.equal(byName['codex-C'], 'codex');
      assert.equal(byName['agent-A'], 'claude');
    });
    check('AGENT_RELAY_HOME takes precedence over SESSION_RELAY_HOME', () => {
      const alternateHome = path.join(HOME, 'alt-home');
      const precedenceId = '77777777-7777-7777-7777-777777777777';
      assert.equal(
        relay(['register', 'prec', '--id', precedenceId, '--dir', dirA], {
          env: { AGENT_RELAY_HOME: alternateHome },
        }).status,
        0,
      );
      const alternateRegistry = JSON.parse(fs.readFileSync(path.join(alternateHome, 'registry.json'), 'utf8'));
      assert.ok(alternateRegistry.agents[precedenceId], 'registered into the AGENT_RELAY_HOME store');
      const registry = JSON.parse(fs.readFileSync(path.join(HOME, 'registry.json'), 'utf8'));
      assert.ok(!registry.agents[precedenceId], 'legacy-alias store untouched');
    });
    const relayDry = (who) => relayJSON(['wake', who, '--dry']);
    check('wake dispatches the codex doorbell for a codex target', () => {
      const dryRun = relayDry('codex-C');
      assert.equal(dryRun.tool, 'codex');
      assert.equal(dryRun.cmd, 'codex');
      assert.deepEqual(dryRun.args.slice(0, 3), ['exec', 'resume', idC]);
      assert.deepEqual(configValues(dryRun.args), ['service_tier="default"']);
      assert.equal(dryRun.cwd, dirC);
    });
    check('wake dispatches the claude doorbell for a claude target', () => {
      const dryRun = relayDry('agent-A');
      assert.equal(dryRun.tool, 'claude');
      assert.equal(dryRun.cmd, 'claude');
      assert.ok(dryRun.args.includes('--resume') && dryRun.args.includes(idA));
    });
    check('wake --dry maps --model/--effort for codex and claude targets', () => {
      const codex = relayJSON(['wake', 'codex-C', '--model', 'gpt-5.6-sol', '--effort', 'xhigh', '--dry']);
      assert.deepEqual(codex.args.slice(0, 7), [
        'exec',
        'resume',
        idC,
        '-m',
        'gpt-5.6-sol',
        '-c',
        'model_reasoning_effort=xhigh',
      ]);
      assert.deepEqual(configValues(codex.args), ['model_reasoning_effort=xhigh', 'service_tier="default"']);
      assert.ok(codex.args.indexOf('--') > 6, 'codex model flags stay before the prompt fence');

      const claude = relayJSON(['wake', 'agent-A', '--model', 'opus', '--effort', 'max', '--dry']);
      const resume = claude.args.indexOf('--resume');
      assert.deepEqual(claude.args.slice(resume, resume + 7), [
        '--resume',
        idA,
        '--model',
        'opus',
        '--effort',
        'max',
        '--output-format',
      ]);
      assert.ok(claude.args.indexOf('--') > resume + 6, 'claude model flags stay before the prompt fence');
    });
    check('wake --service-tier is Codex-only and emits exact Fast overrides', () => {
      const codex = relayJSON(['wake', 'codex-C', '--service-tier', 'fast', '--dry']);
      assert.deepEqual(configValues(codex.args), ['features.fast_mode=true', 'service_tier="fast"']);
      const duplicate = relay(['wake', 'codex-C', '--service-tier', 'fast', '--service-tier', 'default', '--dry']);
      assert.notEqual(duplicate.status, 0);
      assert.match(duplicate.stderr, /duplicate.*service-tier|service-tier.*duplicate/i);
      const invalid = relay(['wake', 'codex-C', '--service-tier', 'turbo', '--dry']);
      assert.notEqual(invalid.status, 0);
      assert.match(invalid.stderr, /service-tier.*default\|fast/i);
      const claude = relay(['wake', 'agent-A', '--service-tier', 'fast', '--dry']);
      assert.notEqual(claude.status, 0);
      assert.match(claude.stderr, /service-tier.*Codex-only/i);
    });

    const attachStubDir = path.join(HOME, 'attach-stubs');
    fs.mkdirSync(attachStubDir);
    const attachStub = `#!/usr/bin/env node
const fs = require('node:fs');
const interactive = process.env.ATTACH_STUB_INTERACTIVE === '1';
const stdin = interactive ? fs.readFileSync(0, 'utf8') : '';
if (interactive) {
  process.stdout.write('attach-stdout');
  process.stderr.write('attach-stderr');
}
fs.writeFileSync(process.env.ATTACH_STUB_OUTPUT, JSON.stringify({ argv: process.argv.slice(2), cwd: process.cwd(), ...(interactive ? { stdin } : {}) }));
`;
    for (const tool of ['codex', 'claude']) {
      fs.writeFileSync(path.join(attachStubDir, tool), attachStub, { mode: 0o755 });
    }
    const attachPath = `${attachStubDir}${path.delimiter}${process.env.PATH}`;

    check('attach always runs as a guarded spawn+wait and prints no copyable command', () => {
      const codexRecord = path.join(HOME, 'attach-codex.json');
      const codex = relay(['attach', 'codex-C'], {
        env: { PATH: attachPath, ATTACH_STUB_OUTPUT: codexRecord },
      });
      assert.equal(codex.status, 0, codex.stderr);
      assert.deepEqual(JSON.parse(fs.readFileSync(codexRecord, 'utf8')), {
        argv: ['resume', idC, '-C', dirC, '-c', 'service_tier="default"'],
        cwd: process.cwd(),
      });
      assert.match(codex.stderr, /WARNING: split-brain risk/);
      assert.doesNotMatch(codex.stdout, /command:/);

      const byIdRecord = path.join(HOME, 'attach-codex-id.json');
      const byId = relay(['attach', idC], {
        env: { PATH: attachPath, ATTACH_STUB_OUTPUT: byIdRecord },
      });
      assert.equal(byId.status, 0, byId.stderr);
      assert.deepEqual(JSON.parse(fs.readFileSync(byIdRecord, 'utf8')).argv, [
        'resume',
        idC,
        '-C',
        dirC,
        '-c',
        'service_tier="default"',
      ]);

      const claudeRecord = path.join(HOME, 'attach-claude.json');
      const claude = relay(['attach', 'agent-A'], {
        env: { PATH: attachPath, ATTACH_STUB_OUTPUT: claudeRecord },
      });
      assert.equal(claude.status, 0, claude.stderr);
      assert.deepEqual(JSON.parse(fs.readFileSync(claudeRecord, 'utf8')), {
        argv: ['--resume', idA],
        cwd: dirA,
      });
      assert.match(claude.stderr, /WARNING: split-brain risk/);
    });

    check('legacy attach --exec is accepted but still uses guarded spawn+wait', () => {
      for (const args of [
        ['attach', 'codex-C', '--exec'],
        ['attach', '--exec', 'codex-C'],
      ]) {
        const record = path.join(HOME, `attach-legacy-${args[1] === '--exec' ? 'before' : 'after'}.json`);
        const result = relay(args, {
          env: { PATH: attachPath, ATTACH_STUB_OUTPUT: record },
        });
        assert.equal(result.status, 0, result.stderr);
        assert.deepEqual(JSON.parse(fs.readFileSync(record, 'utf8')).argv, [
          'resume',
          idC,
          '-C',
          dirC,
          '-c',
          'service_tier="default"',
        ]);
        assert.match(result.stderr, /--exec is deprecated/);
        assert.doesNotMatch(result.stdout, /command:/);
      }
    });

    check('guarded attach inherits stdin/stdout/stderr while the relay parent waits', () => {
      const record = path.join(HOME, 'attach-interactive.json');
      const result = relay(['attach', 'agent-A'], {
        input: 'interactive-input',
        env: {
          PATH: attachPath,
          ATTACH_STUB_OUTPUT: record,
          ATTACH_STUB_INTERACTIVE: '1',
        },
      });
      assert.equal(result.status, 0, result.stderr);
      assert.equal(JSON.parse(fs.readFileSync(record, 'utf8')).stdin, 'interactive-input');
      assert.match(result.stdout, /attach-stdout/);
      assert.match(result.stderr, /attach-stderr/);
      assert.deepEqual(
        lifecycleState().active_operations ?? {},
        {},
        'attach releases its durable operation id before exit',
      );
    });

    check('attach strictly rejects extra operands, unknown flags, and exec after --', () => {
      const record = path.join(HOME, 'attach-strict.json');
      for (const args of [
        ['attach', 'codex-C', 'extra'],
        ['attach', 'codex-C', '--bogus'],
        ['attach', 'codex-C', '--', '--exec'],
      ]) {
        const result = relay(args, { env: { PATH: attachPath, ATTACH_STUB_OUTPUT: record } });
        assert.equal(result.status, 2, `${args.join(' ')} exited ${result.status}: ${result.stderr}`);
        assert.match(result.stderr, /usage: relay attach <nameOrId> \[--exec\]/);
      }
      assert.equal(fs.existsSync(record), false, '--exec after -- never replaced the process');
    });

    check('attach refuses a missing stored dir before guarded spawn', () => {
      const missingId = '53535353-5353-4353-8353-535353535353';
      const missingDir = path.join(HOME, 'missing-attach-dir');
      assert.equal(
        relay(['register', 'missing-attach', '--id', missingId, '--dir', missingDir, '--tool', 'codex']).status,
        0,
      );
      for (const args of [
        ['attach', 'missing-attach'],
        ['attach', 'missing-attach', '--exec'],
      ]) {
        const record = path.join(HOME, `attach-missing-${args.length}.json`);
        const result = relay(args, { env: { PATH: attachPath, ATTACH_STUB_OUTPUT: record } });
        assert.equal(result.status, 1);
        assert.match(result.stderr, /stored dir does not exist/);
        assert.equal(fs.existsSync(record), false);
      }
    });

    check('attach rejects an unresolved non-UUID id', () => {
      const result = relay(['attach', 'not-a-session-id']);
      assert.equal(result.status, 1);
      assert.match(result.stderr, /session UUID/);
    });

    check('attach fails closed when the resume lock cannot be probed', () => {
      const unknownId = '54545454-5454-4454-8454-545454545454';
      assert.equal(relay(['register', 'unknown-attach-lock', '--id', unknownId, '--dir', dirA]).status, 0);
      const lock = path.join(HOME, 'locks', `resume-${unknownId}.lock`);
      fs.writeFileSync(lock, '{}', { mode: 0o000 });
      try {
        const result = relay(['attach', 'unknown-attach-lock']);
        assert.equal(result.status, 4);
        assert.match(result.stderr, /attach refused: cannot verify resume lock state/);
        assert.match(result.stderr, /relay doctor --id/);
        assert.match(result.stderr, /WARNING: split-brain risk/);
      } finally {
        fs.chmodSync(lock, 0o600);
      }
    });

    const wakeStub = path.join(HOME, 'fake-wake');
    fs.writeFileSync(
      wakeStub,
      `#!/usr/bin/env node
const fs = require('node:fs');
if (process.env.WAKE_STUB_RECORD) fs.writeFileSync(process.env.WAKE_STUB_RECORD, JSON.stringify(process.argv.slice(2)));
const file = process.env.WAKE_STUB_FILE;
if (file) process.stdout.write(fs.readFileSync(file));
else process.stdout.write(process.env.WAKE_STUB_STDOUT || '');
if (process.env.WAKE_STUB_STDERR) process.stderr.write(process.env.WAKE_STUB_STDERR);
const delay = Number(process.env.WAKE_STUB_DELAY_MS || 0);
if (delay > 0) Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, delay);
process.exit(Number(process.env.WAKE_STUB_STATUS || 0));
`,
      { mode: 0o755 },
    );
    const fixtureDir = path.join(HERE, 'fixtures');
    const claudeUsageFixture = path.join(fixtureDir, 'wake-usage-claude.json');
    const codexUsageFixture = path.join(fixtureDir, 'wake-usage-codex.jsonl');
    const noNewlineFixture = path.join(HOME, 'wake-no-newline.json');
    fs.writeFileSync(
      noNewlineFixture,
      '{"type":"result","total_cost_usd":1.25,"usage":{"input_tokens":7,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":3}}',
    );

    check('wake prints claude usage to stderr and keeps fixture stdout byte-identical', () => {
      const expected = fs.readFileSync(claudeUsageFixture);
      const result = relayBytes(['wake', 'agent-A', '--model', 'opus', '--effort', 'max'], {
        env: { RELAY_WAKE_CMD_CLAUDE: wakeStub, WAKE_STUB_FILE: claudeUsageFixture },
      });
      assert.equal(result.status, 0, `wake exited ${result.status}: ${result.stderr}`);
      assert.deepEqual(result.stdout, expected);
      assert.match(
        result.stderr.toString('utf8'),
        /\[relay wake\] claude: 45682 in \(45603 cached\) \/ 4 out, \$0\.0142089/,
      );
    });
    check('wake prints codex usage to stderr and keeps fixture stdout byte-identical', () => {
      const expected = fs.readFileSync(codexUsageFixture);
      const result = relayBytes(['wake', 'codex-C', '--model', 'gpt-5.6-sol', '--effort', 'xhigh'], {
        env: { RELAY_WAKE_CMD_CODEX: wakeStub, WAKE_STUB_FILE: codexUsageFixture },
      });
      assert.equal(result.status, 0, `wake exited ${result.status}: ${result.stderr}`);
      assert.deepEqual(result.stdout, expected);
      assert.match(result.stderr.toString('utf8'), /\[relay wake\] codex: 47400 in \(12032 cached\) \/ 10 out/);
    });
    check('wake preserves no-trailing-newline stdout while still reporting usage', () => {
      const expected = fs.readFileSync(noNewlineFixture);
      assert.notEqual(expected.at(-1), 0x0a, 'fixture intentionally has no trailing newline');
      const result = relayBytes(['wake', 'agent-A', '--model', 'opus', '--effort', 'max'], {
        env: { RELAY_WAKE_CMD_CLAUDE: wakeStub, WAKE_STUB_FILE: noNewlineFixture },
      });
      assert.equal(result.status, 0, `wake exited ${result.status}: ${result.stderr}`);
      assert.deepEqual(result.stdout, expected);
      assert.match(result.stderr.toString('utf8'), /\[relay wake\] claude: 7 in \/ 3 out, \$1\.25/);
    });
    check('wake omits usage on garbage stdout and preserves the child exit code', () => {
      const result = relayBytes(['wake', 'agent-A', '--model', 'opus', '--effort', 'max'], {
        env: { RELAY_WAKE_CMD_CLAUDE: wakeStub, WAKE_STUB_STDOUT: 'not json', WAKE_STUB_STATUS: '7' },
      });
      assert.equal(result.status, 7);
      assert.deepEqual(result.stdout, Buffer.from('not json'));
      assert.doesNotMatch(result.stderr.toString('utf8'), /\[relay wake\]/);
    });

    assert.deepEqual(labels, EXPECTED_LABELS);
    scenarioResult = { count: labels.length, labels };
  } catch (error) {
    hasPrimaryError = true;
    primaryError = error;
  }

  let hasDrainError = false;
  let drainError;
  try {
    waitForLifecycleCustody?.();
  } catch (error) {
    hasDrainError = true;
    drainError = error;
  }

  let hasCleanupError = false;
  let cleanupError;
  try {
    await fixture.cleanup();
  } catch (error) {
    hasCleanupError = true;
    cleanupError = error;
  }

  const failures = [];
  if (hasPrimaryError) failures.push(primaryError);
  if (hasDrainError) failures.push(drainError);
  if (hasCleanupError) failures.push(cleanupError);
  if (failures.length === 1) throw failures[0];
  if (failures.length > 1) {
    const firstFailure = failures[0];
    const messages = failures.map((failure) => (failure instanceof Error ? failure.message : String(failure)));
    throw new AggregateError(failures, messages.join('\n'), { cause: firstFailure });
  }
  return scenarioResult;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await runScenarioCli({ scenario: 'core', run });
}
