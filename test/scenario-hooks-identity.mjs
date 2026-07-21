#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createFixture, createScenarioCheck, runScenarioCli } from './selftest-fixture.mjs';

const SCENARIO = 'hooks-identity';
export const EXPECTED_LABELS = [
  'prompt-event hook drains pending mail as UserPromptSubmit context',
  'prompt-event hook with an empty inbox emits nothing (zero per-turn overhead)',
  'claude SessionStart with an empty inbox still nudges a Monitor watch on this mailbox',
  'codex SessionStart with an empty inbox emits only the identity line (no Monitor to arm)',
  'rapid duplicate Codex SessionStart suppresses only repeated identity context',
  'Codex SessionStart debounce never suppresses mail or a different start source',
  'RELAY_NO_WATCH=1 suppresses the nudge but keeps the identity line',
  'two sessions register against one shared dir (marker ends on the later one)',
  "SessionStart identity line names each session's OWN id, not the marker owner's",
  'bus send WITHOUT from in a shared dir is attributed to the marker owner (the gap from is for)',
  'bus send with from:"alice" overrides the marker attribution',
  'bus inbox WITHOUT id still drains the marker owner (fallback intact)',
  'bus send with an unknown from is isError and enqueues nothing',
  'bus inbox with id:"alice" drains alice even while the marker points at bob',
  'bus inbox with an unknown id is isError',
  'CLI send --from stamps the sender; the drained mail trailer names the RECIPIENT own id',
  'CLI send with an unknown --from dies without queueing',
];

export async function run({ bin, home, emit }) {
  const fixture = createFixture({ bin, home });
  const labels = [];
  const check = createScenarioCheck({ emit, labels });
  const { relay, runHook, hookArgs, runBus, peek } = fixture;

  try {
    const idB = '22222222-2222-2222-2222-222222222222';
    const dirB = path.join(home, 'proj-b');
    fs.mkdirSync(dirB, { recursive: true });
    assert.equal(runHook({ session_id: idB, cwd: dirB, source: 'startup' }).status, 0);
    assert.equal(relay(['register', 'agent-B', '--id', idB, '--dir', dirB]).status, 0);

    const idP = '44444444-4444-4444-4444-444444444444';
    const dirP = path.join(home, 'proj-p');
    fs.mkdirSync(dirP, { recursive: true });
    check('prompt-event hook drains pending mail as UserPromptSubmit context', () => {
      assert.equal(hookArgs([], { session_id: idP, cwd: dirP, source: 'startup' }).status, 0);
      assert.equal(relay(['send', '--id', idP, '--', 'push me']).status, 0);
      const result = hookArgs(['--event', 'prompt'], {
        session_id: idP,
        cwd: dirP,
        hook_event_name: 'UserPromptSubmit',
      });
      const output = JSON.parse(result.stdout);
      assert.equal(output.hookSpecificOutput.hookEventName, 'UserPromptSubmit');
      assert.ok(output.hookSpecificOutput.additionalContext.includes('push me'));
      assert.equal(peek(idP).count, 0);
    });
    check('prompt-event hook with an empty inbox emits nothing (zero per-turn overhead)', () => {
      const result = hookArgs(['--event', 'prompt'], { session_id: idP, cwd: dirP });
      assert.equal(result.status, 0);
      assert.equal(result.stdout, '');
    });
    check('claude SessionStart with an empty inbox still nudges a Monitor watch on this mailbox', () => {
      const result = hookArgs([], { session_id: idP, cwd: dirP, source: 'resume' });
      const context = JSON.parse(result.stdout).hookSpecificOutput.additionalContext;
      assert.ok(/monitor/i.test(context), 'nudge names the Monitor tool');
      assert.ok(
        context.includes(`watch --follow ${idP}`),
        'nudge carries the unified watcher command for this session',
      );
      assert.ok(context.includes(`bus id is ${idP}`), 'identity line rides along');
    });
    check('codex SessionStart with an empty inbox emits only the identity line (no Monitor to arm)', () => {
      const result = hookArgs(['codex'], { session_id: idP, cwd: dirP, source: 'startup' });
      assert.equal(result.status, 0);
      const context = JSON.parse(result.stdout).hookSpecificOutput.additionalContext;
      assert.ok(context.includes(`bus id is ${idP}`), 'identity line present');
      assert.ok(!context.includes('session-relay-mail') && !/monitor/i.test(context), 'nothing but identity');
    });

    const idCodexDebounce = '77777777-7777-4777-8777-777777777777';
    const dirCodexDebounce = path.join(home, 'proj-codex-debounce');
    fs.mkdirSync(dirCodexDebounce, { recursive: true });
    check('rapid duplicate Codex SessionStart suppresses only repeated identity context', () => {
      const event = { session_id: idCodexDebounce, cwd: dirCodexDebounce, source: 'resume' };
      const first = hookArgs(['codex'], event);
      const duplicate = hookArgs(['codex'], event);
      assert.equal(first.status, 0);
      assert.ok(JSON.parse(first.stdout).hookSpecificOutput.additionalContext.includes(`bus id is ${idCodexDebounce}`));
      assert.equal(duplicate.status, 0);
      assert.equal(duplicate.stdout, '');
    });
    check('Codex SessionStart debounce never suppresses mail or a different start source', () => {
      assert.equal(relay(['send', '--id', idCodexDebounce, '--', 'debounce mail']).status, 0);
      const withMail = hookArgs(['codex'], {
        session_id: idCodexDebounce,
        cwd: dirCodexDebounce,
        source: 'resume',
      });
      const mailContext = JSON.parse(withMail.stdout).hookSpecificOutput.additionalContext;
      assert.ok(mailContext.includes('debounce mail'));
      assert.ok(mailContext.includes(`bus id is ${idCodexDebounce}`));
      assert.equal(peek(idCodexDebounce).count, 0);
      const differentSource = hookArgs(['codex'], {
        session_id: idCodexDebounce,
        cwd: dirCodexDebounce,
        source: 'compact',
      });
      assert.ok(
        JSON.parse(differentSource.stdout).hookSpecificOutput.additionalContext.includes(
          `bus id is ${idCodexDebounce}`,
        ),
      );
    });
    check('RELAY_NO_WATCH=1 suppresses the nudge but keeps the identity line', () => {
      const result = hookArgs([], { session_id: idP, cwd: dirP, source: 'startup' }, { RELAY_NO_WATCH: '1' });
      assert.equal(result.status, 0);
      const context = JSON.parse(result.stdout).hookSpecificOutput.additionalContext;
      assert.ok(!context.includes(`watch --follow ${idP}`), 'no Monitor nudge');
      assert.ok(context.includes(`bus id is ${idP}`), 'identity survives RELAY_NO_WATCH');
    });

    const dirShared = path.join(home, 'proj-shared');
    fs.mkdirSync(dirShared, { recursive: true });
    const idAlice = '88888888-8888-8888-8888-888888888888';
    const idBob = '99999999-9999-9999-9999-999999999999';
    check('two sessions register against one shared dir (marker ends on the later one)', () => {
      assert.equal(runHook({ session_id: idAlice, cwd: dirShared, source: 'startup' }).status, 0);
      assert.equal(relay(['register', 'alice', '--id', idAlice, '--dir', dirShared]).status, 0);
      assert.equal(runHook({ session_id: idBob, cwd: dirShared, source: 'startup' }).status, 0);
      assert.equal(relay(['register', 'bob', '--id', idBob, '--dir', dirShared]).status, 0);
    });
    check("SessionStart identity line names each session's OWN id, not the marker owner's", () => {
      const alice = runHook({ session_id: idAlice, cwd: dirShared, source: 'resume' });
      assert.ok(JSON.parse(alice.stdout).hookSpecificOutput.additionalContext.includes(`bus id is ${idAlice}`));
      const bob = runHook({ session_id: idBob, cwd: dirShared, source: 'resume' });
      assert.ok(JSON.parse(bob.stdout).hookSpecificOutput.additionalContext.includes(`bus id is ${idBob}`));
    });
    const initialize = { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} };
    const busShared = (name, args) =>
      runBus(dirShared, [
        initialize,
        { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name, arguments: args } },
      ]).get(2);
    check('bus send WITHOUT from in a shared dir is attributed to the marker owner (the gap from is for)', () => {
      busShared('send', { to: 'agent-B', body: 'anon hello' });
      assert.equal(peek('agent-B').messages.at(-1).fromName, 'bob');
      relay(['inbox', 'agent-B']);
    });
    check('bus send with from:"alice" overrides the marker attribution', () => {
      const result = busShared('send', { to: 'bob', from: 'alice', body: 'from alice' });
      assert.equal(JSON.parse(result.result.content[0].text).ok, true);
      const mail = peek('bob');
      assert.equal(mail.messages[0].from, idAlice);
      assert.equal(mail.messages[0].fromName, 'alice');
    });
    check('bus inbox WITHOUT id still drains the marker owner (fallback intact)', () => {
      const box = JSON.parse(busShared('inbox', {}).result.content[0].text);
      assert.equal(box.count, 1);
      assert.equal(box.messages[0].body, 'from alice');
      assert.equal(peek('bob').count, 0);
    });
    check('bus send with an unknown from is isError and enqueues nothing', () => {
      const result = busShared('send', { to: 'bob', from: 'ghost', body: 'x' });
      assert.equal(result.result.isError, true);
      assert.equal(peek('bob').count, 0);
    });
    check('bus inbox with id:"alice" drains alice even while the marker points at bob', () => {
      assert.equal(relay(['send', '--id', idAlice, '--', 'for alice']).status, 0);
      const box = JSON.parse(busShared('inbox', { id: 'alice' }).result.content[0].text);
      assert.equal(box.count, 1);
      assert.equal(box.messages[0].body, 'for alice');
      assert.equal(peek('alice').count, 0);
    });
    check('bus inbox with an unknown id is isError', () => {
      assert.equal(busShared('inbox', { id: 'ghost' }).result.isError, true);
    });
    check('CLI send --from stamps the sender; the drained mail trailer names the RECIPIENT own id', () => {
      assert.equal(relay(['send', '--id', idBob, '--from', 'alice', '--', 'cli hello']).status, 0);
      const mail = peek('bob');
      assert.equal(mail.messages[0].from, idAlice);
      assert.equal(mail.messages[0].fromName, 'alice');
      const result = runHook({ session_id: idBob, cwd: dirShared, source: 'resume' });
      const context = JSON.parse(result.stdout).hookSpecificOutput.additionalContext;
      assert.ok(context.includes('cli hello'), 'mail delivered');
      assert.ok(context.includes(`from:"${idBob}"`), 'reply trailer carries the recipient id, not the marker owner');
    });
    check('CLI send with an unknown --from dies without queueing', () => {
      const result = relay(['send', '--id', idBob, '--from', 'ghost', '--', 'x']);
      assert.notEqual(result.status, 0);
      assert.equal(peek('bob').count, 0);
    });

    assert.deepEqual(labels, EXPECTED_LABELS);
    return { count: labels.length, labels };
  } finally {
    await fixture.cleanup();
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await runScenarioCli({ scenario: SCENARIO, run });
}
