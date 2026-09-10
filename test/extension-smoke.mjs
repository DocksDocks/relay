#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import ext from '../plugin/extension/index.ts';

const registrations = [
  'session_start',
  'before_agent_start',
  'agent_end',
  'session_branch',
  'session_tree',
  'session_switch',
  'session_shutdown',
];
const largeMail = 'x'.repeat(600000);
const text = (content) => (typeof content === 'string' ? content : content.map((block) => block.text).join(''));
const hash = (value) => createHash('sha256').update(value).digest('hex');

// Model the runtime's recursive per-string persistence limit, not a total entry limit.
function persisted(value) {
  return JSON.parse(
    JSON.stringify(value, (_key, item) =>
      typeof item === 'string' && item.length > 500000 ? item.slice(0, 500000) : item,
    ),
  );
}

function runtime() {
  const handlers = new Map();
  const tools = new Map();
  const commands = new Map();
  const renderers = new Map();
  const branches = new Map([
    ['old', []],
    ['new', []],
  ]);
  const disk = new Map();
  const mailboxes = new Map([
    ['old', []],
    ['new', []],
  ]);
  const holds = new Map();
  const timers = new Map();
  const calls = [];
  const order = [];
  const doorbells = [];
  const queuedPrompts = [];
  const messages = [];
  const notifications = [];
  const scripts = [];
  let started = false;
  let sessionId = 'old';
  let idle = true;
  let now = 100000;
  let serial = 0;
  let flushEffect;
  let appendEffect;
  let failAck = false;
  let failRollback = false;
  const branch = () => branches.get(sessionId);
  const chunks = () => branch().filter((entry) => entry.type === 'custom' && entry.customType === 'session-relay.mail');
  const outcome = (stdout = '', code = 0, stderr = '') => ({ stdout, code, stderr, killed: false });
  const manager = {
    getSessionId: () => sessionId,
    getSessionFile: () => `/sessions/project/${sessionId}.jsonl`,
    getSessionDir: () => '/sessions/project',
    getBranch: branch,
    ensureOnDisk: async () => {},
    async flush() {
      order.push('flush');
      if (flushEffect) {
        const effect = flushEffect;
        flushEffect = undefined;
        effect();
      }
      disk.set(sessionId, persisted(branch()));
    },
  };
  const ctx = {
    cwd: '/project',
    sessionManager: manager,
    hasUI: true,
    isIdle: () => idle,
    ui: { notify: (...args) => notifications.push(args) },
    setInterval(callback, interval) {
      assert.equal(interval, 3000);
      const timer = ++serial;
      timers.set(timer, callback);
      return timer;
    },
    clearTimer: (timer) => timers.delete(timer),
  };
  const pi = {
    on: (name, handler) => handlers.set(name, handler),
    registerTool: (tool) => tools.set(tool.name, tool),
    registerCommand: (name, command) => commands.set(name, command),
    registerMessageRenderer: (name, renderer) => renderers.set(name, renderer),
    typebox: { Type: new Proxy({}, { get: () => () => ({}) }) },
    pi: {
      Text: class {
        constructor(content) {
          this.content = content;
        }
      },
    },
    appendEntry(customType, data) {
      order.push('append');
      if (appendEffect) appendEffect();
      branch().push({ type: 'custom', customType, data: persisted(data), id: `entry-${++serial}` });
    },
    sendUserMessage(content, options) {
      assert.equal(idle, true, 'streaming mail must never become a steer');
      assert.equal(options?.deliverAs, undefined);
      assert.match(content, /^\[relay\] \d+ new message\(s\)$/);
      doorbells.push(content);
      // Real sendUserMessage is fire-and-forget; scenarios explicitly run or drop its prompt.
      queuedPrompts.push(content);
    },
    sendMessage: (...args) => messages.push(args),
    async exec(_program, args) {
      if (!started) throw new Error('extension factory must not spawn a process');
      const argv = args.slice(2);
      calls.push(argv);
      order.push(argv[0]);
      let result;
      if (argv[0] === 'hook' || argv[0] === 'inbox') {
        assert.ok(argv.includes('--hold'), `${argv[0]} must hold its drain`);
        const id = argv[0] === 'hook' ? argv[argv.indexOf('--session') + 1] : argv.at(-1);
        const rows = mailboxes.get(id) || [];
        if (rows.length === 0) {
          result = outcome(
            argv[0] === 'hook' ? '' : JSON.stringify({ token: null, expires_at: null, count: 0, messages: [] }),
          );
        } else {
          const token = `00000000-0000-4000-8000-${(++serial).toString(16).padStart(12, '0')}`;
          holds.set(token, { id, rows });
          mailboxes.set(id, []);
          result = outcome(
            argv[0] === 'hook'
              ? `${token}\n${rows.map((row) => row.body).join('\n')}`
              : JSON.stringify({ token, expires_at: 9999999999, count: rows.length, messages: rows }),
          );
        }
      } else if (argv[0] === 'ack') {
        assert.ok(order.lastIndexOf('flush') > order.lastIndexOf('append'), 'ack follows persistence');
        if (failAck) result = outcome('', 1, 'scripted ack failure');
        else {
          assert.ok(holds.delete(argv[1]), 'ack must address a live hold');
          result = outcome();
        }
      } else if (argv[0] === 'rollback') {
        const hold = holds.get(argv[1]);
        assert.ok(hold, 'rollback must address a live hold');
        if (failRollback) result = outcome('', 1, 'scripted rollback failure');
        else {
          mailboxes.set(hold.id, [...hold.rows, ...mailboxes.get(hold.id)]);
          holds.delete(argv[1]);
          result = outcome();
        }
      } else if (argv[0] === 'peek') {
        result = outcome(JSON.stringify({ count: (mailboxes.get(argv[1]) || []).length }));
      } else if (argv[0] === 'list') result = outcome('roster');
      else throw new Error(`unexpected exec: ${argv.join(' ')}`);
      const index = scripts.findIndex((script) => script.match(argv));
      if (index !== -1) scripts.splice(index, 1)[0].effect(argv, result);
      return result;
    },
  };
  ext(pi);
  assert.deepEqual([...handlers.keys()], registrations);
  assert.deepEqual([...tools.keys()], ['relay']);
  assert.deepEqual([...commands.keys()], ['relay']);
  assert.deepEqual([...renderers.keys()], ['relay_mail']);
  assert.equal(calls.length, 0, 'extension factory must not execute a process');
  const emit = (name, event = {}) => handlers.get(name)(event, ctx);
  return {
    calls,
    order,
    doorbells,
    queuedPrompts,
    messages,
    notifications,
    holds,
    mailboxes,
    scripts,
    branch,
    chunks,
    emit,
    disk,
    set id(value) {
      sessionId = value;
    },
    set idle(value) {
      idle = value;
    },
    set flushEffect(value) {
      flushEffect = value;
    },
    set appendEffect(value) {
      appendEffect = value;
    },
    set failAck(value) {
      failAck = value;
    },
    set failRollback(value) {
      failRollback = value;
    },
    clock: () => now,
    add(body, id = sessionId) {
      mailboxes.get(id).push({ id: `mail-${++serial}`, from: 'sender', to: id, body, ts: '2026-09-08T00:00:00Z' });
    },
    async start() {
      started = true;
      await emit('session_start');
    },
    async stop() {
      await emit('session_shutdown');
      assert.equal(timers.size, 0);
    },
    async tick() {
      now += 3000;
      for (const callback of [...timers.values()]) await callback();
    },
    async prompt({ persist = true, queued = false } = {}) {
      if (queued) assert.ok(queuedPrompts.shift(), 'automatic prompt needs a doorbell');
      const result = await emit('before_agent_start', { prompt: 'runtime prompt' });
      if (persist && result?.message) {
        branch().push(persisted({ type: 'custom_message', ...result.message }));
        disk.set(sessionId, persisted(branch()));
      }
      return result?.message;
    },
    inbox: () => tools.get('relay').execute('call-1', { action: 'inbox' }, undefined, undefined, ctx),
    command: () => commands.get('relay').handler('', ctx),
    persistTool(result) {
      branch().push(persisted({ type: 'message', message: { role: 'toolResult', toolName: 'relay', ...result } }));
      disk.set(sessionId, persisted(branch()));
    },
    resume() {
      branches.set(sessionId, persisted(disk.get(sessionId)));
    },
    render: (message) => renderers.get('relay_mail')(message).content,
  };
}

async function scenario(name, run) {
  const r = runtime();
  const originalNow = Date.now;
  Date.now = r.clock;
  try {
    await run(r);
  } catch (error) {
    throw new Error(`${name}: ${error.message}`, { cause: error });
  } finally {
    await r.stop();
    Date.now = originalNow;
  }
}

await scenario('drift-rollback', async (r) => {
  r.add('old-session secret');
  r.scripts.push({
    match: (argv) => argv[0] === 'hook',
    effect: () => {
      r.id = 'new';
    },
  });
  await r.start();
  assert.equal(r.chunks().length, 0);
  assert.equal(r.doorbells.length, 0);
  assert.equal(r.messages.length, 0);
  assert.equal(r.calls.filter((argv) => argv[0] === 'rollback').length, 1);
  assert.equal(r.calls.filter((argv) => argv[0] === 'ack').length, 0);
  assert.equal(await r.prompt(), undefined);
  r.id = 'old';
  await r.emit('session_switch');
  assert.equal(text((await r.prompt({ queued: true })).content), 'old-session secret');
  assert.equal(r.mailboxes.get('old').length, 0);
});

await scenario('drift-rollback nonzero exit', async (r) => {
  r.add('held until expiry');
  r.failRollback = true;
  r.scripts.push({
    match: (argv) => argv[0] === 'hook',
    effect: () => {
      r.id = 'new';
    },
  });
  await r.start();
  assert.equal(r.calls.filter((argv) => argv[0] === 'rollback').length, 1);
  assert.equal(r.calls.filter((argv) => argv[0] === 'ack').length, 0);
  assert.equal(r.holds.size, 1);
  const hold = [...r.holds.values()][0];
  assert.equal(hold.id, 'old');
  assert.equal(hold.rows[0].body, 'held until expiry');
  assert.deepEqual(r.mailboxes.get('old'), []);
  assert.equal(r.chunks().length, 0);
  assert.equal(r.messages.length, 0);
  assert.equal(r.doorbells.length, 0);
  assert.ok(r.notifications.some(([message, level]) => level === 'warning' && /rollback failed:/.test(message)));
  assert.equal(await r.prompt(), undefined);
  assert.ok(r.calls.some((argv) => argv[0] === 'hook' && argv[argv.indexOf('--session') + 1] === 'new'));
  r.id = 'old';
  assert.equal(r.chunks().length, 0, 'failed rollback cannot persist old-session mail');
  r.id = 'new';
});

await scenario('commit and large-mail-replay', async (r) => {
  r.add(largeMail);
  await r.start();
  assert.equal(r.chunks().length, 10);
  const entries = r.chunks();
  const token = entries[0].data.token;
  assert.deepEqual(
    entries.map((entry) => entry.data.chunk),
    Array.from({ length: 10 }, (_, i) => i),
  );
  for (const { data } of entries) {
    assert.ok(data.content.length <= 65536);
    assert.equal(data.chunks, 10);
    assert.equal(data.rendered_len, largeMail.length);
    assert.equal(data.rendered_sha256, hash(largeMail));
  }
  assert.equal(entries.map((entry) => entry.data.content).join(''), largeMail);
  assert.equal(r.holds.size, 0);
  assert.equal(r.mailboxes.get('old').length, 0);
  assert.equal(r.calls.filter((argv) => argv[0] === 'ack').length, 1);
  assert.equal(r.doorbells.length, 1);
  const message = await r.prompt({ queued: true });
  assert.equal(message.customType, 'relay_mail');
  assert.deepEqual(message.details.tokens, [token]);
  assert.equal(message.content.length, 10);
  assert.ok(message.content.every((block) => block.type === 'text' && block.text.length <= 65536));
  assert.equal(text(message.content), largeMail);
  assert.equal(r.render(message), largeMail);
  assert.equal(await r.prompt(), undefined);
  await r.emit('session_shutdown');
  r.resume();
  await r.start();
  const replay = r.branch().find((entry) => entry.type === 'custom_message');
  assert.equal(text(replay.content), largeMail);
  assert.equal(await r.prompt(), undefined);
  assert.equal(r.doorbells.length, 1);
});

await scenario('dropped-injection', async (r) => {
  r.add('retry me');
  await r.start();
  const dropped = await r.prompt({ persist: false, queued: true });
  assert.equal(text(dropped.content), 'retry me');
  await r.emit('agent_end', { stopReason: 'stop' });
  assert.equal(r.doorbells.length, 2);
  const delivered = await r.prompt({ queued: true });
  assert.deepEqual(delivered.details.tokens, dropped.details.tokens);
  assert.equal(text(delivered.content), 'retry me');
  await r.emit('agent_end', { stopReason: 'stop' });
  assert.equal(r.doorbells.length, 2);
  assert.equal(await r.prompt(), undefined);
});

await scenario('command-drift', async (r) => {
  await r.start();
  r.scripts.push({
    match: (argv) => argv[0] === 'list',
    effect: () => {
      r.id = 'new';
    },
  });
  await r.command();
  assert.deepEqual(r.notifications, []);
  assert.deepEqual(r.messages, []);
});

await scenario('ack-failure', async (r) => {
  r.add('durable despite ack failure');
  r.failAck = true;
  await r.start();
  assert.equal(r.chunks().length, 1);
  assert.equal(r.holds.size, 1);
  assert.ok(r.notifications.some(([message, level]) => level === 'warning' && /ack failed:/.test(message)));
  assert.equal(text((await r.prompt()).content), 'durable despite ack failure');
  assert.equal(await r.prompt(), undefined);
});

await scenario('streaming-doorbell', async (r) => {
  await r.start();
  r.idle = false;
  r.add('stream mail');
  await r.tick();
  assert.equal(r.chunks().length, 1);
  assert.equal(r.holds.size, 0);
  assert.equal(r.doorbells.length, 0);
  await r.emit('agent_end', { stopReason: 'stop' });
  assert.equal(r.doorbells.length, 0);
  r.idle = true;
  await r.tick();
  assert.equal(r.mailboxes.get('old').length, 0);
  assert.equal(r.doorbells.length, 1);
  assert.equal(text((await r.prompt({ queued: true })).content), 'stream mail');
  await r.tick();
  assert.equal(r.doorbells.length, 1);
});

await scenario('continuation-settle', async (r) => {
  r.add('continuation mail');
  await r.start();
  await r.emit('agent_end', { willContinue: true, stopReason: 'stop' });
  assert.equal(r.doorbells.length, 1);
  await r.tick();
  assert.equal(r.doorbells.length, 1);
  await r.emit('agent_end', { stopReason: 'stop' });
  assert.equal(r.doorbells.length, 2);
  assert.equal(text((await r.prompt()).content), 'continuation mail');
});

await scenario('chunk-mismatch', async (r) => {
  r.add(largeMail);
  r.flushEffect = () => {
    r.branch().pop();
  };
  await r.start();
  assert.equal(r.chunks().length, 9);
  assert.equal(r.calls.filter((argv) => argv[0] === 'rollback').length, 1);
  assert.equal(r.calls.filter((argv) => argv[0] === 'ack').length, 0);
  assert.equal(r.mailboxes.get('old')[0].body, largeMail);
  assert.equal(r.doorbells.length, 0);
  // Isolate the incomplete token from the next (legitimate) redrain of restored mail.
  r.mailboxes.set('old', []);
  assert.equal(await r.prompt(), undefined);
});

await scenario('doorbell-expiry', async (r) => {
  r.add('lost wake');
  await r.start();
  r.queuedPrompts.length = 0;
  await r.tick();
  assert.equal(r.doorbells.length, 1);
  await r.tick();
  assert.equal(r.doorbells.length, 1, 'exactly 6000ms does not expire');
  await r.tick();
  assert.equal(r.doorbells.length, 2);
  await r.tick();
  assert.equal(r.doorbells.length, 2, 'new latch prevents a poll loop');
  assert.equal(text((await r.prompt({ queued: true })).content), 'lost wake');
});

await scenario('aborted-doorbell', async (r) => {
  r.add('aborted wake');
  await r.start();
  r.queuedPrompts.length = 0;
  await r.emit('agent_end', { stopReason: 'aborted' });
  assert.equal(r.doorbells.length, 2);
  assert.equal(text((await r.prompt({ queued: true })).content), 'aborted wake');
});

await scenario('tree-navigation and branch-navigation', async (r) => {
  r.add('tree mail');
  await r.start();
  const original = await r.prompt({ queued: true });
  r.branch().splice(
    r.branch().findIndex((entry) => entry.type === 'custom_message'),
    1,
  );
  await r.emit('session_tree');
  assert.equal(r.doorbells.length, 2);
  const replay = await r.prompt({ queued: true });
  assert.equal(text(replay.content), 'tree mail');
  assert.deepEqual(replay.details.tokens, original.details.tokens);
  r.branch().splice(
    r.branch().findIndex((entry) => entry.type === 'custom_message'),
    1,
  );
  await r.emit('session_branch');
  assert.equal(r.doorbells.length, 3);
  assert.equal(text((await r.prompt({ queued: true })).content), 'tree mail');
});

await scenario('switch-before-doorbell', async (r) => {
  r.idle = false;
  r.add('old private mail');
  await r.start();
  assert.equal(r.chunks().length, 1);
  assert.equal(r.doorbells.length, 0);
  r.id = 'new';
  r.idle = true;
  await r.emit('session_switch');
  assert.equal(await r.prompt(), undefined);
  assert.equal(r.doorbells.length, 0);
  assert.equal(r.chunks().length, 0);
  r.id = 'old';
  await r.emit('session_switch');
  assert.equal(text((await r.prompt({ queued: true })).content), 'old private mail');
  // A queued generic wake may land after a switch, but cannot carry old mail with it.
  r.add('another private row');
  await r.tick();
  r.id = 'new';
  await r.emit('session_switch');
  assert.equal(await r.prompt({ queued: true }), undefined);
});

await scenario('tool-inbox hold and persisted proof', async (r) => {
  await r.start();
  r.add('tool mail');
  const result = await r.inbox();
  assert.notEqual(result.isError, true);
  const envelope = JSON.parse(text(result.content));
  assert.equal(envelope.messages[0].body, 'tool mail');
  assert.deepEqual(result.details.tokens, [envelope.token]);
  assert.equal(r.holds.size, 0);
  assert.ok(r.calls.some((argv) => argv[0] === 'inbox' && argv.includes('--hold')));
  const retry = await r.prompt({ persist: false });
  assert.deepEqual(retry.details.tokens, result.details.tokens, 'unpersisted tool result is not proof');
  r.persistTool(result);
  assert.equal(await r.prompt(), undefined);
  const empty = await r.inbox();
  assert.deepEqual(empty.details.tokens, []);
});

for (const failure of ['append', 'flush']) {
  await scenario(`${failure}-failure rollback`, async (r) => {
    r.add('recoverable mail');
    const fail = () => {
      throw new Error(`scripted ${failure} failure`);
    };
    if (failure === 'append') r.appendEffect = fail;
    else r.flushEffect = fail;
    await r.start();
    assert.equal(r.calls.filter((argv) => argv[0] === 'rollback').length, 1);
    assert.equal(r.calls.filter((argv) => argv[0] === 'ack').length, 0);
    assert.equal(r.mailboxes.get('old')[0].body, 'recoverable mail');
  });
}

console.log(
  'PASS extension_smoke events=session_start,before_agent_start,agent_end,session_branch,session_tree,session_switch,session_shutdown tools=relay commands=relay renderers=relay_mail cases=drift-rollback,commit,dropped-injection,command-drift,ack-failure,streaming-doorbell,continuation-settle,chunk-mismatch,doorbell-expiry,large-mail-replay,aborted-doorbell,tree-navigation,switch-before-doorbell',
);
