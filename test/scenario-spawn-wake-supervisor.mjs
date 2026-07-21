#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createFixture, createScenarioCheck, runScenarioCli } from './selftest-fixture.mjs';

export const EXPECTED_LABELS = [
  'spawn --dry falls back to claude when codex is absent, and keeps the reply-loop prompt',
  'spawn --dry maps --model/--effort for claude and codex children',
  'spawn --service-tier is Codex-only and keeps Fast role overrides exact',
  'spawn --dry defaults to codex when its CLI is available',
  'spawn --dry honors RELAY_SPAWN_TOOL and rejects invalid values',
  'spawn births a claude child via the pre-mint path and registers its name',
  'spawn births a classic codex child through an exact managed hook claim',
  'app-server spawn returns before turn completion while its detached pump later accepts bus elicitation',
  'app-server Fast spawn fails closed when the effective tier is missing or mismatched',
  'app-server spawn surfaces thread/start failure synchronously without a detached pump',
  'app-server spawn surfaces initial turn/start failure synchronously without detaching',
  'app-server spawn --watch blocks until the detached pump reports turn completion',
  'app-server pump failure after turn/start retains fail-closed lifecycle authority',
  'app-server spawn pump exits and closes its connection at the spawn timeout',
  'app-server timeout retains FencingUnconfirmed when exact interrupt cannot be proven',
  'spawn timeout names the child stderr log when no birth arrives',
  'spawn caps a live child stderr log near 4 MiB and keeps its newest tail',
  'spawn --watch waits for a successful first turn and reports completion',
  'spawn --watch mirrors a failed child exit',
  'spawn detects a pre-registration child failure without burning the birth timeout',
  'spawn without --watch still returns immediately after registration',
  'wake refuses a concurrent relay-launched resume and proceeds after its lock releases',
  'watch retries a refused wake after the resume lock releases and delivers queued mail',
  'detached lifecycle supervisor preserves PTY and flood-disconnect custody',
];

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PLUGIN = path.resolve(HERE, '..');

export async function run({ bin, home, emit }) {
  const fixture = createFixture({ bin, home });
  const labels = [];
  const check = createScenarioCheck({ emit, labels });
  const { bin: BIN, home: HOME, envFor, relay, relayJSON, runHook, configValues, peek, trackChild } = fixture;
  const lifecycleState = () => JSON.parse(fs.readFileSync(path.join(HOME, 'lifecycle-v1.json'), 'utf8')).state;

  try {
    const dirA = path.join(HOME, 'proj-a');
    const idA = '11111111-1111-1111-1111-111111111111';
    fs.mkdirSync(dirA, { recursive: true });
    assert.equal(runHook({ session_id: idA, cwd: dirA, hook_event_name: 'SessionStart', source: 'startup' }).status, 0);
    assert.equal(relay(['register', 'agent-A', '--id', idA, '--dir', dirA]).status, 0);

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

    const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
    const waitFor = (predicate, label, timeoutMs = 5000) => {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        if (predicate()) return;
        sleep(25);
      }
      assert.fail(`timed out waiting for ${label}`);
    };
    const spawnToFiles = (args, env, stem) => {
      const stdoutPath = path.join(HOME, `${stem}.stdout`);
      const stderrPath = path.join(HOME, `${stem}.stderr`);
      const stdout = fs.openSync(stdoutPath, 'w');
      const stderr = fs.openSync(stderrPath, 'w');
      const child = trackChild(
        spawn(BIN, args, {
          detached: true,
          env: envFor(env),
          stdio: ['ignore', stdout, stderr],
        }),
        { processGroup: true },
      );
      fs.closeSync(stdout);
      fs.closeSync(stderr);
      return { child, stdoutPath, stderrPath };
    };
    const startFakeAppServer = (stem, settings = ['idle']) => {
      const serverSock = path.join(HOME, `${stem}.sock`);
      const serverFrames = path.join(HOME, `${stem}.jsonl`);
      const controlFile = path.join(HOME, `${stem}-control.json`);
      fs.writeFileSync(serverFrames, '');
      const control = Array.isArray(settings) ? { statuses: settings } : settings;
      fs.writeFileSync(controlFile, JSON.stringify(control));
      const child = trackChild(
        spawn(process.execPath, [path.join(HERE, 'fake-app-server.mjs'), serverSock, serverFrames, controlFile], {
          detached: true,
          stdio: 'ignore',
        }),
        { processGroup: true },
      );
      waitFor(() => fs.existsSync(serverSock), `${stem} fake app-server socket`);
      return {
        child,
        sock: serverSock,
        frames: () =>
          fs
            .readFileSync(serverFrames, 'utf8')
            .split('\n')
            .filter(Boolean)
            .map((line) => JSON.parse(line)),
      };
    };

    const stub = path.join(HOME, 'fake-child');
    fs.writeFileSync(
      stub,
      `#!/usr/bin/env node
    const { spawnSync } = require('node:child_process');
    const fs = require('node:fs');
    if (process.env.STUB_RECORD) fs.writeFileSync(process.env.STUB_RECORD, JSON.stringify(process.argv.slice(2)));
    const i = process.argv.indexOf('--session-id');
    const id = i >= 0 ? process.argv[i + 1] : require('node:crypto').randomUUID();
    const evt = JSON.stringify({ session_id: id, cwd: process.cwd(), hook_event_name: 'SessionStart', source: 'startup' });
    const hookArgs = process.env.STUB_TOOL === 'codex' ? ['hook', 'codex'] : ['hook'];
    if (process.env.STUB_SKIP_HOOK !== '1') spawnSync(process.env.STUB_RELAY_BIN, hookArgs, { input: evt, env: process.env });
    let stderrBytes = Number(process.env.STUB_STDERR_BYTES || 0);
    const chunk = Buffer.alloc(64 * 1024, 'x');
    while (stderrBytes > 0) {
      const size = Math.min(stderrBytes, chunk.length);
      fs.writeSync(2, chunk.subarray(0, size));
      stderrBytes -= size;
    }
    if (process.env.STUB_STDERR_BYTES) fs.writeSync(2, Buffer.from('SPAWN_TAIL_MARKER'));
    const delay = Number(process.env.STUB_DELAY_MS || 0);
    if (delay > 0) Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, delay);
    process.exit(Number(process.env.STUB_EXIT || 0));
    `,
      { mode: 0o755 },
    );

    check('spawn --dry falls back to claude when codex is absent, and keeps the reply-loop prompt', () => {
      const dirS = path.join(HOME, 'proj-s0');
      fs.mkdirSync(dirS, { recursive: true });
      // PATH with no codex → availability probe fails → claude fallback note.
      const r = relay(['spawn', dirS, '--reply-to', 'agent-A', '--dry', '--', 'do X'], {
        env: { PATH: '/nonexistent' },
      });
      assert.equal(r.status, 0, `spawn --dry exited ${r.status}: ${r.stderr}`);
      assert.ok(
        /codex not found, defaulting to claude/i.test(r.stderr),
        'no --tool + no codex prints the claude fallback note',
      );
      assert.ok(/no --model given/i.test(r.stderr), 'no --model prints the model pin note');
      const d = JSON.parse(r.stdout);
      assert.equal(d.tool, 'claude');
      assert.ok(d.args.includes('-p') && d.args.includes('--session-id'), 'headless + pre-minted id');
      assert.deepEqual(d.args.slice(d.args.indexOf('--permission-mode'), d.args.indexOf('--permission-mode') + 2), [
        '--permission-mode',
        'auto',
      ]);
      assert.ok(!d.args.some((a) => a.includes('output-format')), 'detached child gets no output-format flag');
      const premint = d.args[d.args.indexOf('--session-id') + 1];
      assert.ok(
        d.prompt.includes(`send "agent-A" --from ${premint} -- `) && d.prompt.trimEnd().endsWith('do X'),
        'prompt carries the abs-relay reply command (with the pre-minted --from) and the task',
      );
      assert.ok(d.prompt.includes('separate git branch'), 'guardrail rules ride in the prompt');
      assert.equal(d.cwd, fs.realpathSync(dirS));
    });
    check('spawn --dry maps --model/--effort for claude and codex children', () => {
      const dirS = path.join(HOME, 'proj-s0-models');
      fs.mkdirSync(dirS, { recursive: true });

      const claude = relayJSON([
        'spawn',
        dirS,
        '--tool',
        'claude',
        '--model',
        'opus',
        '--effort',
        'max',
        '--reply-to',
        'agent-A',
        '--dry',
        '--',
        'do X',
      ]);
      const perm = claude.args.indexOf('--permission-mode');
      assert.deepEqual(claude.args.slice(perm, perm + 6), [
        '--permission-mode',
        'auto',
        '--model',
        'opus',
        '--effort',
        'max',
      ]);
      assert.ok(claude.args.indexOf('--') > perm + 5, 'claude model flags stay before the prompt fence');

      const codex = relayJSON([
        'spawn',
        dirS,
        '--tool',
        'codex',
        '--model',
        'gpt-5.6-sol',
        '--effort',
        'xhigh',
        '--reply-to',
        'agent-A',
        '--dry',
        '--',
        'do Y',
      ]);
      assert.deepEqual(codex.args.slice(0, 7), [
        'exec',
        '--sandbox',
        'workspace-write',
        '-m',
        'gpt-5.6-sol',
        '-c',
        'model_reasoning_effort=xhigh',
      ]);
      assert.deepEqual(configValues(codex.args), ['model_reasoning_effort=xhigh', 'service_tier="default"']);
      assert.ok(codex.args.indexOf('--') > 6, 'codex model flags stay before the prompt fence');
    });
    check('spawn --service-tier is Codex-only and keeps Fast role overrides exact', () => {
      const dirS = path.join(HOME, 'proj-s0-service-tier');
      fs.mkdirSync(dirS, { recursive: true });
      const fast = relayJSON([
        'spawn',
        dirS,
        '--tool',
        'codex',
        '--service-tier',
        'fast',
        '--reply-to',
        'agent-A',
        '--dry',
        '--',
        'fast task',
      ]);
      assert.deepEqual(configValues(fast.args), ['features.fast_mode=true', 'service_tier="fast"']);
      const claude = relay([
        'spawn',
        dirS,
        '--tool',
        'claude',
        '--service-tier',
        'fast',
        '--reply-to',
        'agent-A',
        '--dry',
        '--',
        'invalid',
      ]);
      assert.notEqual(claude.status, 0);
      assert.match(claude.stderr, /service-tier.*Codex-only/i);
      const duplicate = relay([
        'spawn',
        dirS,
        '--tool',
        'codex',
        '--service-tier',
        'fast',
        '--service-tier',
        'default',
        '--reply-to',
        'agent-A',
        '--dry',
        '--',
        'invalid',
      ]);
      assert.notEqual(duplicate.status, 0);
      assert.match(duplicate.stderr, /duplicate.*service-tier|service-tier.*duplicate/i);
    });
    check('spawn --dry defaults to codex when its CLI is available', () => {
      const dirS = path.join(HOME, 'proj-s0-codex-default');
      fs.mkdirSync(dirS, { recursive: true });
      // RELAY_SPAWN_CMD_CODEX pointing at an executable satisfies the probe.
      const r = relay(
        ['spawn', dirS, '--model', 'gpt-5.6-sol', '--effort', 'xhigh', '--reply-to', 'agent-A', '--dry', '--', 'do X'],
        { env: { RELAY_SPAWN_CMD_CODEX: stub } },
      );
      assert.equal(r.status, 0, `spawn --dry exited ${r.status}: ${r.stderr}`);
      assert.ok(
        /codex available, defaulting to codex/i.test(r.stderr),
        'no --tool + codex present prints the codex default note',
      );
      const d = JSON.parse(r.stdout);
      assert.equal(d.tool, 'codex');
      assert.equal(d.cmd, stub);
    });
    check('spawn --dry honors RELAY_SPAWN_TOOL and rejects invalid values', () => {
      const dirS = path.join(HOME, 'proj-s0-env');
      fs.mkdirSync(dirS, { recursive: true });

      const r = relay(
        ['spawn', dirS, '--model', 'gpt-5.6-sol', '--effort', 'xhigh', '--reply-to', 'agent-A', '--dry', '--', 'do X'],
        { env: { RELAY_SPAWN_TOOL: 'codex' } },
      );
      assert.equal(r.status, 0, `spawn env default exited ${r.status}: ${r.stderr}`);
      assert.ok(!/defaulting to/i.test(r.stderr), 'env default does not print any fallback note');
      assert.equal(JSON.parse(r.stdout).tool, 'codex');

      const bad = relay(['spawn', dirS, '--reply-to', 'agent-A', '--dry', '--', 'do X'], {
        env: { RELAY_SPAWN_TOOL: 'bogus' },
      });
      assert.notEqual(bad.status, 0, 'invalid RELAY_SPAWN_TOOL is rejected');
      assert.ok(/valid values: claude\|codex/i.test(bad.stderr), 'invalid env error names the valid values');
    });
    check('spawn births a claude child via the pre-mint path and registers its name', () => {
      const dirS = path.join(HOME, 'proj-s1');
      fs.mkdirSync(dirS, { recursive: true });
      const r = relay(
        [
          'spawn',
          dirS,
          '--tool',
          'claude',
          '--name',
          'w1',
          '--reply-to',
          'agent-A',
          '--timeout',
          '5',
          '--',
          'task one',
        ],
        { env: { RELAY_SPAWN_CMD_CLAUDE: stub, STUB_RELAY_BIN: BIN } },
      );
      assert.equal(r.status, 0, `spawn exited ${r.status}: ${r.stderr}`);
      assert.ok(/^spawned w1 \([0-9a-f-]{36}\)/.test(r.stdout), `birth line: ${r.stdout}`);
      const id = /^spawned w1 \(([0-9a-f-]{36})\)/.exec(r.stdout)?.[1];
      const lifecycle = lifecycleState();
      const managed = Object.values(lifecycle.managed_workers ?? {}).find((worker) => worker.runtime_session_id === id);
      assert.ok(managed, 'classic Claude birth claims its pre-created managed worker');
      assert.equal(managed.state, 'Active');
      assert.equal(lifecycle.session_bindings?.[id]?.state?.kind, 'Managed');
      assert.deepEqual(lifecycle.pending_managed ?? {}, {}, 'successful Claude claim consumes its pending token');
      assert.equal(relay(['send', 'w1', '--', 'hello worker']).status, 0);
      assert.equal(peek('w1').count, 1, 'named worker is a routable bus target');
    });
    check('spawn births a classic codex child through an exact managed hook claim', () => {
      const dirS = path.join(HOME, 'proj-s1-codex');
      fs.mkdirSync(dirS, { recursive: true });
      const r = relay(
        [
          'spawn',
          dirS,
          '--tool',
          'codex',
          '--name',
          'w1-codex',
          '--reply-to',
          'agent-A',
          '--timeout',
          '5',
          '--',
          'task one codex',
        ],
        {
          env: { RELAY_SPAWN_CMD_CODEX: stub, STUB_RELAY_BIN: BIN, STUB_TOOL: 'codex' },
        },
      );
      assert.equal(r.status, 0, `spawn exited ${r.status}: ${r.stderr}`);
      const id = /^spawned w1-codex \(([0-9a-f-]{36})\)/.exec(r.stdout)?.[1];
      assert.ok(id, `birth line: ${r.stdout}`);
      const lifecycle = lifecycleState();
      const managed = Object.values(lifecycle.managed_workers ?? {}).find((worker) => worker.runtime_session_id === id);
      assert.ok(managed, 'classic Codex birth claims its pre-created managed worker');
      assert.equal(managed.state, 'Active');
      assert.equal(managed.execution, 'ObservationOnly');
      assert.equal(lifecycle.session_bindings?.[id]?.state?.kind, 'Managed');
      assert.deepEqual(lifecycle.pending_managed ?? {}, {}, 'successful Codex claim consumes its pending token');
    });
    check(
      'app-server spawn returns before turn completion while its detached pump later accepts bus elicitation',
      () => {
        const dirS = path.join(HOME, 'proj-s2');
        const id = '71717171-7171-4171-8171-717171717171';
        const fake = startFakeAppServer('spawn-appserver-async', {
          threadId: id,
          elicitationDelayMs: 1500,
          completionDelayMs: 100,
        });
        const childRecord = path.join(HOME, 'spawn-appserver-child-record.json');
        fs.mkdirSync(dirS, { recursive: true });
        try {
          const started = Date.now();
          const r = relay(
            [
              'spawn',
              dirS,
              '--tool',
              'codex',
              '--model',
              'gpt-5.6-sol',
              '--effort',
              'xhigh',
              '--service-tier',
              'fast',
              '--name',
              'w2',
              '--server',
              fake.sock,
              '--reply-to',
              'agent-A',
              '--timeout',
              '5',
              '--',
              'task two',
            ],
            {
              env: { RELAY_SPAWN_CMD_CODEX: stub, STUB_RECORD: childRecord },
            },
          );
          const elapsedMs = Date.now() - started;
          assert.equal(r.status, 0, `spawn exited ${r.status}: ${r.stderr}`);
          assert.ok(elapsedMs < 1000, `foreground returned before delayed elicitation/completion (${elapsedMs}ms)`);
          assert.match(r.stdout, new RegExp(`^spawned w2 \\(${id}\\)`));
          assert.equal(fs.existsSync(childRecord), false, 'reachable app-server spawn never launches codex exec');

          const initial = fake.frames();
          const threadStart = initial.find((frame) => frame.method === 'thread/start');
          assert.equal(threadStart.params.cwd, fs.realpathSync(dirS));
          assert.equal(threadStart.params.model, 'gpt-5.6-sol');
          assert.equal(threadStart.params.sandbox, 'workspace-write');
          assert.equal(threadStart.params.approvalPolicy, 'never');
          assert.equal(threadStart.params.serviceTier, 'fast');
          const turnStart = initial.find((frame) => frame.method === 'turn/start');
          assert.equal(turnStart.params.threadId, id);
          assert.equal(turnStart.params.model, 'gpt-5.6-sol');
          assert.equal(turnStart.params.effort, 'xhigh');
          assert.equal(turnStart.params.serviceTier, 'fast');
          const prompt = turnStart.params.input[0].text;
          assert.match(prompt, /separate git branch/);
          assert.match(prompt, /Never modify live\/production systems/);
          assert.match(prompt, new RegExp(`send "agent-A" --from ${id} --`));
          assert.ok(prompt.trimEnd().endsWith('task two'));

          let registry = JSON.parse(fs.readFileSync(path.join(HOME, 'registry.json'), 'utf8'));
          assert.equal(registry.names.w2, id);
          assert.equal(registry.agents[id].server, fake.sock);
          assert.equal(
            registry.agents[id].spawned_via,
            'app-server',
            'origin is published with the managed claim before the first turn',
          );
          const lifecycle = lifecycleState();
          const managed = Object.values(lifecycle.managed_workers ?? {}).find(
            (worker) => worker.runtime_session_id === id,
          );
          assert.ok(managed, 'app-server thread birth claims a managed lifecycle worker');
          assert.equal(managed.state, 'Active');
          assert.equal(lifecycle.session_bindings?.[id]?.state?.kind, 'Managed');
          assert.deepEqual(
            lifecycle.pending_managed ?? {},
            {},
            'Codex claim consumes its pending token before turn/start',
          );

          waitFor(
            () => fake.frames().some((frame) => frame.id === 990 && frame.result?.action === 'accept'),
            'detached pump bus elicitation response after foreground exit',
            4000,
          );
          waitFor(
            () => fake.frames().some((frame) => frame.event === 'connection/closed'),
            'detached pump completion close',
          );

          assert.equal(
            relay(['hook', 'codex'], { input: JSON.stringify({ session_id: id, cwd: dirS, source: 'resume' }) }).status,
            0,
          );
          registry = JSON.parse(fs.readFileSync(path.join(HOME, 'registry.json'), 'utf8'));
          assert.equal(registry.agents[id].spawned_via, 'app-server', 'hook refresh preserves relay ownership');
        } finally {
          fake.child.kill('SIGKILL');
        }
      },
    );
    check('app-server Fast spawn fails closed when the effective tier is missing or mismatched', () => {
      for (const [stem, control, pattern] of [
        ['spawn-tier-mismatch', { reportedServiceTier: 'default' }, /requested fast but app-server reported default/i],
        ['spawn-tier-missing', { omitServiceTier: true }, /did not report an effective service tier/i],
      ]) {
        const dirS = path.join(HOME, `proj-${stem}`);
        const fake = startFakeAppServer(stem, control);
        fs.mkdirSync(dirS, { recursive: true });
        try {
          const r = relay([
            'spawn',
            dirS,
            '--tool',
            'codex',
            '--service-tier',
            'fast',
            '--name',
            stem,
            '--server',
            fake.sock,
            '--reply-to',
            'agent-A',
            '--timeout',
            '2',
            '--',
            'must not downgrade',
          ]);
          assert.notEqual(r.status, 0);
          assert.match(r.stderr, pattern);
          assert.equal(
            fake.frames().some((frame) => frame.method === 'turn/start'),
            false,
          );
        } finally {
          fake.child.kill('SIGKILL');
        }
      }
    });
    check('app-server spawn surfaces thread/start failure synchronously without a detached pump', () => {
      const dirS = path.join(HOME, 'proj-spawn-thread-start-fail');
      const fake = startFakeAppServer('spawn-thread-start-fail', { threadStartError: 'thread birth rejected' });
      fs.mkdirSync(dirS, { recursive: true });
      try {
        const r = relay([
          'spawn',
          dirS,
          '--tool',
          'codex',
          '--name',
          'thread-start-fail',
          '--server',
          fake.sock,
          '--reply-to',
          'agent-A',
          '--timeout',
          '2',
          '--',
          'never starts',
        ]);
        assert.notEqual(r.status, 0);
        assert.match(r.stderr, /thread birth rejected/);
        assert.equal(
          fake.frames().some((frame) => frame.method === 'turn/start'),
          false,
        );
        assert.equal(
          JSON.parse(fs.readFileSync(path.join(HOME, 'registry.json'), 'utf8')).names['thread-start-fail'],
          undefined,
        );
        waitFor(
          () => fake.frames().some((frame) => frame.event === 'connection/closed'),
          'failed thread/start connection close',
        );
      } finally {
        fake.child.kill('SIGKILL');
      }
    });
    check('app-server spawn surfaces initial turn/start failure synchronously without detaching', () => {
      const dirS = path.join(HOME, 'proj-spawn-turn-start-fail');
      const id = '72727272-7272-4272-8272-727272727272';
      const fake = startFakeAppServer('spawn-turn-start-fail', {
        threadId: id,
        turnStartError: 'initial turn rejected',
      });
      fs.mkdirSync(dirS, { recursive: true });
      try {
        const r = relay([
          'spawn',
          dirS,
          '--tool',
          'codex',
          '--name',
          'turn-start-fail',
          '--server',
          fake.sock,
          '--reply-to',
          'agent-A',
          '--timeout',
          '2',
          '--',
          'never runs',
        ]);
        assert.notEqual(r.status, 0);
        assert.match(r.stderr, /initial turn rejected/);
        assert.equal(fake.frames().filter((frame) => frame.method === 'turn/start').length, 1);
        assert.equal(
          fake.frames().some((frame) => frame.method === 'turn/interrupt'),
          false,
          'unknown turn identity is never interrupted',
        );
        assert.equal(
          fake.frames().some((frame) => frame.id === 990 && frame.result),
          false,
          'no detached elicitation pump exists',
        );
        const lifecycle = lifecycleState();
        const managed = Object.values(lifecycle.managed_workers ?? {}).find(
          (worker) => worker.runtime_session_id === id,
        );
        assert.equal(
          managed?.state,
          'FencingUnconfirmed',
          'rejected turn/start does not leave the managed worker Active',
        );
        assert.match(managed?.proof_gap ?? '', /turn.*identity|turn\/start/i);
        waitFor(
          () => fake.frames().some((frame) => frame.event === 'connection/closed'),
          'failed turn/start connection close',
        );
      } finally {
        fake.child.kill('SIGKILL');
      }
    });
    check('app-server spawn --watch blocks until the detached pump reports turn completion', () => {
      const dirS = path.join(HOME, 'proj-spawn-appserver-watch');
      const id = '73737373-7373-4373-8373-737373737373';
      const fake = startFakeAppServer('spawn-appserver-watch', { threadId: id, completionDelayMs: 600 });
      fs.mkdirSync(dirS, { recursive: true });
      try {
        const started = Date.now();
        const r = relay([
          'spawn',
          dirS,
          '--tool',
          'codex',
          '--name',
          'appserver-watch',
          '--server',
          fake.sock,
          '--reply-to',
          'agent-A',
          '--timeout',
          '3',
          '--watch',
          '--',
          'wait for me',
        ]);
        const elapsedMs = Date.now() - started;
        assert.equal(r.status, 0, `app-server --watch exited ${r.status}: ${r.stderr}`);
        assert.ok(elapsedMs >= 550, `--watch waited for delayed completion (${elapsedMs}ms)`);
        assert.match(r.stdout, /^spawned appserver-watch; first turn complete; /);
      } finally {
        fake.child.kill('SIGKILL');
      }
    });
    check('app-server pump failure after turn/start retains fail-closed lifecycle authority', () => {
      const dirS = path.join(HOME, 'proj-spawn-appserver-disconnect');
      const id = '73737373-7373-4373-8373-737373737374';
      const fake = startFakeAppServer('spawn-appserver-disconnect', {
        threadId: id,
        turnId: 'turn-disconnected',
        disconnectAfterTurnStart: true,
      });
      fs.mkdirSync(dirS, { recursive: true });
      try {
        const r = relay([
          'spawn',
          dirS,
          '--tool',
          'codex',
          '--name',
          'appserver-disconnect',
          '--server',
          fake.sock,
          '--reply-to',
          'agent-A',
          '--timeout',
          '3',
          '--watch',
          '--',
          'disconnect',
        ]);
        assert.notEqual(r.status, 0);
        assert.match(r.stdout, /first turn failed/i);
        const lifecycle = lifecycleState();
        const managed = Object.values(lifecycle.managed_workers ?? {}).find(
          (worker) => worker.runtime_session_id === id,
        );
        assert.equal(managed?.state, 'FencingUnconfirmed');
        assert.match(managed?.proof_gap ?? '', /connection|terminal state is unknown/i);
        assert.equal(
          Object.values(lifecycle.active_operations ?? {}).some((operation) => operation.runtime_session_id === id),
          false,
          'failed pump consumes its own active-operation guard',
        );
      } finally {
        fake.child.kill('SIGKILL');
      }
    });
    check('app-server spawn pump exits and closes its connection at the spawn timeout', () => {
      const dirS = path.join(HOME, 'proj-spawn-appserver-timeout');
      const id = '74747474-7474-4474-8474-747474747474';
      const exactTurnId = 'turn-timeout-exact';
      const fake = startFakeAppServer('spawn-appserver-timeout', {
        threadId: id,
        turnId: exactTurnId,
        neverComplete: true,
        interruptMismatchedCompletion: true,
        interruptCompletionDelayMs: 150,
      });
      fs.mkdirSync(dirS, { recursive: true });
      try {
        const started = Date.now();
        const r = relay([
          'spawn',
          dirS,
          '--tool',
          'codex',
          '--name',
          'appserver-timeout',
          '--server',
          fake.sock,
          '--reply-to',
          'agent-A',
          '--timeout',
          '1',
          '--watch',
          '--',
          'time out',
        ]);
        const elapsedMs = Date.now() - started;
        assert.notEqual(r.status, 0, 'timed-out pump is not a successful watched turn');
        assert.ok(elapsedMs >= 900 && elapsedMs < 3000, `pump honored the one-second cap (${elapsedMs}ms)`);
        assert.match(r.stdout, /first turn failed/);
        const frames = fake.frames();
        const turnStart = frames.find((frame) => frame.method === 'turn/start');
        const interrupt = frames.find((frame) => frame.method === 'turn/interrupt');
        assert.ok(frames.indexOf(interrupt) > frames.indexOf(turnStart), 'interrupt follows the exact started turn');
        assert.deepEqual(interrupt.params, { threadId: id, turnId: exactTurnId });
        const lifecycle = lifecycleState();
        const managed = Object.values(lifecycle.managed_workers ?? {}).find(
          (worker) => worker.runtime_session_id === id,
        );
        assert.equal(managed?.state, 'Fenced', 'matching completion confirms the published timeout fence');
        waitFor(
          () => fake.frames().some((frame) => frame.event === 'connection/closed'),
          'timed-out pump connection close',
        );
      } finally {
        fake.child.kill('SIGKILL');
      }
    });
    check('app-server timeout retains FencingUnconfirmed when exact interrupt cannot be proven', () => {
      const dirS = path.join(HOME, 'proj-spawn-appserver-unconfirmed');
      const id = '75757575-7575-4575-8575-757575757575';
      const exactTurnId = 'turn-unconfirmed-exact';
      const fake = startFakeAppServer('spawn-appserver-unconfirmed', {
        threadId: id,
        turnId: exactTurnId,
        neverComplete: true,
        interruptNeverComplete: true,
        statuses: ['active'],
      });
      fs.mkdirSync(dirS, { recursive: true });
      try {
        const r = relay([
          'spawn',
          dirS,
          '--tool',
          'codex',
          '--name',
          'appserver-unconfirmed',
          '--server',
          fake.sock,
          '--reply-to',
          'agent-A',
          '--timeout',
          '1',
          '--watch',
          '--',
          'stay uncertain',
        ]);
        assert.notEqual(r.status, 0);
        const interrupt = fake.frames().find((frame) => frame.method === 'turn/interrupt');
        assert.deepEqual(interrupt?.params, { threadId: id, turnId: exactTurnId });
        const lifecycle = lifecycleState();
        const managed = Object.values(lifecycle.managed_workers ?? {}).find(
          (worker) => worker.runtime_session_id === id,
        );
        assert.equal(managed?.state, 'FencingUnconfirmed');
        assert.match(managed?.proof_gap ?? '', /interrupt|idle|completion/i);
      } finally {
        fake.child.kill('SIGKILL');
      }
    });
    check('spawn timeout names the child stderr log when no birth arrives', () => {
      const dirS = path.join(HOME, 'proj-s3');
      fs.mkdirSync(dirS, { recursive: true });
      const noop = path.join(HOME, 'noop-child');
      fs.writeFileSync(noop, '#!/bin/sh\nexit 0\n', { mode: 0o755 });
      const r = relay(
        ['spawn', dirS, '--tool', 'claude', '--reply-to', 'agent-A', '--timeout', '1', '--', 'never registers'],
        { env: { RELAY_SPAWN_CMD_CLAUDE: noop } },
      );
      assert.notEqual(r.status, 0, 'no birth within timeout is a failure');
      assert.ok(/spawn-logs.*\.stderr/.test(r.stderr), 'timeout message names the stderr log path');
    });
    check('spawn caps a live child stderr log near 4 MiB and keeps its newest tail', () => {
      const dirS = path.join(HOME, 'proj-spawn-log-cap');
      fs.mkdirSync(dirS, { recursive: true });
      const logDir = path.join(HOME, 'spawn-logs');
      const before = new Set(fs.readdirSync(logDir));
      const r = relay(
        [
          'spawn',
          dirS,
          '--tool',
          'claude',
          '--name',
          'log-cap',
          '--reply-to',
          'agent-A',
          '--timeout',
          '5',
          '--watch',
          '--',
          'emit stderr',
        ],
        {
          env: { RELAY_SPAWN_CMD_CLAUDE: stub, STUB_RELAY_BIN: BIN, STUB_STDERR_BYTES: String(6 * 1024 * 1024) },
        },
      );
      assert.equal(r.status, 0, `spawn log-cap exited ${r.status}: ${r.stderr}`);
      const created = fs.readdirSync(logDir).filter((name) => !before.has(name));
      assert.equal(created.length, 1, `expected one new spawn log, got ${created.join(', ')}`);
      const log = path.join(logDir, created[0]);
      assert.ok(fs.statSync(log).size <= 4 * 1024 * 1024, `bounded size: ${fs.statSync(log).size}`);
      assert.ok(fs.readFileSync(log).subarray(-64).toString().includes('SPAWN_TAIL_MARKER'));
    });

    // --- reliability follow-up: child completion, lock liveness, wake refusal,
    // raw follow semantics, and doctor. Lock assertions always cross a process
    // boundary; same-process flock re-lock behavior differs across Unix families. ---
    check('spawn --watch waits for a successful first turn and reports completion', () => {
      const dir = path.join(HOME, 'proj-spawn-watch-ok');
      fs.mkdirSync(dir, { recursive: true });
      const r = relay(
        [
          'spawn',
          dir,
          '--tool',
          'claude',
          '--name',
          'watch-ok',
          '--reply-to',
          'agent-A',
          '--timeout',
          '5',
          '--watch',
          '--',
          'task',
        ],
        {
          env: { RELAY_SPAWN_CMD_CLAUDE: stub, STUB_RELAY_BIN: BIN, STUB_DELAY_MS: '300' },
        },
      );
      assert.equal(r.status, 0, `spawn --watch exited ${r.status}: ${r.stderr}`);
      assert.match(r.stdout, /^spawned watch-ok; first turn complete; /);
    });
    check('spawn --watch mirrors a failed child exit', () => {
      const dir = path.join(HOME, 'proj-spawn-watch-fail');
      fs.mkdirSync(dir, { recursive: true });
      const r = relay(
        [
          'spawn',
          dir,
          '--tool',
          'claude',
          '--name',
          'watch-fail',
          '--reply-to',
          'agent-A',
          '--timeout',
          '5',
          '--watch',
          '--',
          'task',
        ],
        {
          env: { RELAY_SPAWN_CMD_CLAUDE: stub, STUB_RELAY_BIN: BIN, STUB_DELAY_MS: '150', STUB_EXIT: '7' },
        },
      );
      assert.equal(r.status, 7, `spawn --watch should mirror exit 7: ${r.stderr}`);
      assert.match(r.stdout, /first turn failed \(exit 7\)/);
    });
    check('spawn detects a pre-registration child failure without burning the birth timeout', () => {
      const dir = path.join(HOME, 'proj-spawn-watch-prebirth');
      fs.mkdirSync(dir, { recursive: true });
      const started = Date.now();
      const r = relay(
        ['spawn', dir, '--tool', 'claude', '--reply-to', 'agent-A', '--timeout', '5', '--watch', '--', 'task'],
        {
          env: { RELAY_SPAWN_CMD_CLAUDE: stub, STUB_RELAY_BIN: BIN, STUB_SKIP_HOOK: '1', STUB_EXIT: '9' },
        },
      );
      assert.equal(r.status, 9);
      assert.ok(Date.now() - started < 2000, 'fast failure returned well under the 5s birth timeout');
      assert.match(r.stderr, /before birth registration/);
    });
    check('spawn without --watch still returns immediately after registration', () => {
      const dir = path.join(HOME, 'proj-spawn-no-watch');
      fs.mkdirSync(dir, { recursive: true });
      const started = Date.now();
      const r = relay(
        [
          'spawn',
          dir,
          '--tool',
          'claude',
          '--name',
          'no-watch',
          '--reply-to',
          'agent-A',
          '--timeout',
          '5',
          '--',
          'task',
        ],
        {
          env: { RELAY_SPAWN_CMD_CLAUDE: stub, STUB_RELAY_BIN: BIN, STUB_DELAY_MS: '1500' },
        },
      );
      assert.equal(r.status, 0, `spawn exited ${r.status}: ${r.stderr}`);
      assert.ok(Date.now() - started < 1000, 'non-watch spawn returned before the delayed child exited');
      assert.match(r.stdout, /^spawned no-watch \([0-9a-f-]{36}\) in /);
    });

    check('wake refuses a concurrent relay-launched resume and proceeds after its lock releases', () => {
      const active = spawnToFiles(
        ['wake', 'agent-A', '--model', 'opus', '--effort', 'max'],
        { RELAY_WAKE_CMD_CLAUDE: wakeStub, WAKE_STUB_DELAY_MS: '5000' },
        'active-wake',
      );
      const lock = path.join(HOME, 'locks', `resume-${idA}.lock`);
      waitFor(() => {
        try {
          return JSON.parse(fs.readFileSync(lock, 'utf8')).pid === active.child.pid;
        } catch {
          return false;
        }
      }, 'the first wake resume lock');

      const attachRefused = relay(['attach', 'agent-A']);
      assert.equal(attachRefused.status, 3);
      assert.match(attachRefused.stderr, /attach refused: relay wake is in flight/);
      assert.match(attachRefused.stderr, /WARNING: split-brain risk/);

      const refused = relay(['wake', 'agent-A', '--model', 'opus', '--effort', 'max'], {
        env: { RELAY_WAKE_CMD_CLAUDE: wakeStub },
      });
      assert.equal(refused.status, 3);
      assert.match(refused.stderr, /wake refused: resume already running/);

      active.child.kill('SIGKILL');
      waitFor(() => {
        try {
          const operations = Object.values(lifecycleState().active_operations ?? {});
          return operations.every((operation) => operation.runtime_session_id !== idA);
        } catch {
          return false;
        }
      }, 'the detached supervisor to reap the cancelled wake');
      const after = relay(['wake', 'agent-A', '--model', 'opus', '--effort', 'max'], {
        env: { RELAY_WAKE_CMD_CLAUDE: wakeStub },
      });
      assert.equal(after.status, 0, `wake after lock release exited ${after.status}: ${after.stderr}`);
    });

    check('watch retries a refused wake after the resume lock releases and delivers queued mail', () => {
      const id = '17171717-1717-4717-8717-171717171717';
      const dir = path.join(HOME, 'proj-wake-retry');
      fs.mkdirSync(dir, { recursive: true });
      assert.equal(relay(['register', 'wake-retry', '--id', id, '--dir', dir]).status, 0);

      const deliveryStub = path.join(HOME, 'fake-wake-delivery');
      fs.writeFileSync(
        deliveryStub,
        `#!/usr/bin/env node
    const { spawnSync } = require('node:child_process');
    const i = process.argv.indexOf('--resume');
    const id = process.argv[i + 1];
    const evt = JSON.stringify({ session_id: id, cwd: process.cwd(), hook_event_name: 'SessionStart', source: 'resume' });
    const hook = spawnSync(process.env.STUB_RELAY_BIN, ['hook'], { input: evt, env: process.env });
    process.exit(hook.status ?? 1);
    `,
        { mode: 0o755 },
      );

      const active = spawnToFiles(
        ['wake', 'wake-retry'],
        { RELAY_WAKE_CMD_CLAUDE: wakeStub, WAKE_STUB_DELAY_MS: '5000' },
        'retry-active-wake',
      );
      const resumeLock = path.join(HOME, 'locks', `resume-${id}.lock`);
      waitFor(() => {
        try {
          return JSON.parse(fs.readFileSync(resumeLock, 'utf8')).pid === active.child.pid;
        } catch {
          return false;
        }
      }, 'the wake-retry resume lock');

      assert.equal(relay(['send', 'wake-retry', '--', 'deliver after refusal']).status, 0);
      const watched = spawnToFiles(
        ['watch', 'wake-retry'],
        { RELAY_WAKE_CMD_CLAUDE: deliveryStub, STUB_RELAY_BIN: BIN },
        'retry-watch',
      );
      try {
        waitFor(() => fs.readFileSync(watched.stderrPath, 'utf8').includes('wake refused'), 'the initial wake refusal');
        assert.equal(peek('wake-retry').count, 1, 'refused wake leaves mail durable');
        active.child.kill('SIGKILL');
        waitFor(() => {
          try {
            const operations = Object.values(lifecycleState().active_operations ?? {});
            return operations.every((operation) => operation.runtime_session_id !== id);
          } catch {
            return false;
          }
        }, 'the wake-retry supervisor to reap the cancelled wake');
        waitFor(() => peek('wake-retry').count === 0, 'watch retry delivery after lock release', 8000);
      } finally {
        active.child.kill('SIGKILL');
        watched.child.kill('SIGKILL');
      }
    });

    check('detached lifecycle supervisor preserves PTY and flood-disconnect custody', () => {
      const result = spawnSync(process.execPath, [path.join(PLUGIN, 'test', 'supervisor-custody.mjs'), '--matrix'], {
        cwd: path.resolve(PLUGIN, '..', '..'),
        env: { ...process.env, RELAY_BIN: BIN },
        encoding: 'utf8',
        timeout: 20000,
      });
      assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
      assert.match(result.stdout, /SUPERVISOR_CUSTODY PASS/);
    });
    assert.deepEqual(labels, EXPECTED_LABELS);
    return { count: labels.length, labels };
  } finally {
    await fixture.cleanup();
  }
}

const isMain = process.argv[1] !== undefined && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) await runScenarioCli({ scenario: 'spawn-wake-supervisor', run });
