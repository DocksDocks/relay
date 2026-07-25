#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createFixture, createScenarioCheck, runScenarioCli } from './selftest-fixture.mjs';

const testUuid = (value) => `00000000-0000-4000-8000-${value.toString(16).padStart(12, '0')}`;

const messageV2 = ({
  body,
  correlationId,
  fromSessionId,
  id,
  kind,
  replyTo = null,
  resultSha256 = null,
  terminalStatus = null,
  toSessionId,
}) => ({
  body,
  correlation_id: correlationId,
  created_at: '2026-07-25T12:34:56.789Z',
  from_session_id: fromSessionId,
  id,
  kind,
  reply_to: replyTo,
  result_sha256: resultSha256,
  schema: 2,
  terminal_status: terminalStatus,
  to_session_id: toSessionId,
});

const sha256 = (value) => createHash('sha256').update(JSON.stringify(value)).digest('hex');

const claimStatusV1 = ({ origin, reply = null, request, requesterId, responderId }) => ({
  correlation_id: request.correlation_id,
  created_at: request.created_at,
  origin,
  reply,
  reply_delivery: reply === null ? null : 'enqueued',
  reply_sha256: reply === null ? null : sha256(reply),
  request,
  request_delivery: origin === 'fanout' ? 'not_applicable' : 'enqueued',
  request_sha256: sha256(request),
  requester_session_id: requesterId,
  responder_session_id: responderId,
  schema: 1,
  state: reply === null ? 'Open' : 'ReplyEnqueued',
  updated_at: '2026-07-25T12:35:00.001Z',
});

const followedDeliveryRows = ({ recipientId, seed, senderId }) => {
  const id = (offset) => testUuid(seed + offset);
  const reservationId = id(9);
  const workerId = id(10);
  const generation = id(11);
  const collectCommand = `relay collect ${senderId} --from ${recipientId}`;
  const request = messageV2({
    body: 'followed typed request',
    correlationId: id(2),
    fromSessionId: senderId,
    id: id(1),
    kind: 'request',
    toSessionId: recipientId,
  });
  const terminalRequest = messageV2({
    body: 'terminal reply authority request',
    correlationId: id(5),
    fromSessionId: recipientId,
    id: id(4),
    kind: 'request',
    toSessionId: senderId,
  });
  const terminalReply = messageV2({
    body: 'followed typed terminal reply',
    correlationId: id(5),
    fromSessionId: senderId,
    id: id(3),
    kind: 'terminal_reply',
    replyTo: id(4),
    terminalStatus: 'failed',
    toSessionId: recipientId,
  });
  const workerRequest = messageV2({
    body: 'worker result authority request',
    correlationId: id(8),
    fromSessionId: recipientId,
    id: id(7),
    kind: 'request',
    toSessionId: senderId,
  });
  const workerResult = messageV2({
    body:
      `worker_result reservation_id=${reservationId} worker_id=${workerId} generation=${generation} ` +
      `runtime_session_id=${senderId} parent_session_id=${recipientId}; collect: ${collectCommand}`,
    correlationId: id(8),
    fromSessionId: senderId,
    id: id(6),
    kind: 'worker_result',
    replyTo: id(7),
    resultSha256: 'cd'.repeat(32),
    terminalStatus: 'completed',
    toSessionId: recipientId,
  });
  const legacy = {
    body: 'legacy follow row',
    from: senderId,
    fromName: 'legacy-follow-sender',
    id: id(12),
    to: recipientId,
    ts: '2026-07-25T12:34:57.000Z',
  };
  const claims = [
    claimStatusV1({
      origin: 'message',
      request,
      requesterId: senderId,
      responderId: recipientId,
    }),
    claimStatusV1({
      origin: 'message',
      reply: terminalReply,
      request: terminalRequest,
      requesterId: recipientId,
      responderId: senderId,
    }),
    claimStatusV1({
      origin: 'fanout',
      reply: workerResult,
      request: workerRequest,
      requesterId: recipientId,
      responderId: senderId,
    }),
  ];
  return { claims, rows: [request, terminalReply, workerResult, legacy] };
};

const seedClaimFixtures = (home, claims) => {
  const protocolRoot = path.join(home, 'protocol-v1');
  fs.mkdirSync(protocolRoot, { recursive: true, mode: 0o700 });
  for (const claim of claims) {
    const directory = path.join(protocolRoot, claim.state === 'Open' ? 'open' : 'terminal');
    fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
    fs.writeFileSync(path.join(directory, `${claim.correlation_id}.json`), `${JSON.stringify(claim)}\n`, {
      mode: 0o600,
    });
  }
};

export const EXPECTED_LABELS = [
  'follow watcher provides live/dead/never/unknown status and tail -n0 -F semantics',
  'follow detects consumed-prefix rewrites that preserve the prior 64-byte suffix',
  'follow watcher exits and releases its lock when the stdout consumer closes',
  'watch rejects an invalid --tool before writing lock metadata',
  'doctor verifies a live receive path, reports dead re-arm, and honors --id in a shared dir',
  'doctor gives an actionable command for an unknown named identity',
];

export async function run({ bin, home, emit }) {
  const fixture = createFixture({ bin, home });
  const labels = [];
  const check = createScenarioCheck({ emit, labels });
  const { bin: BIN, home: HOME, envFor, relay, relayJSON, runHook, runBus, toolJSON, trackChild } = fixture;

  try {
    const dirA = path.join(HOME, 'proj-a');
    const idA = '11111111-1111-1111-1111-111111111111';
    fs.mkdirSync(dirA, { recursive: true });
    assert.equal(runHook({ session_id: idA, cwd: dirA, hook_event_name: 'SessionStart', source: 'startup' }).status, 0);
    assert.equal(relay(['register', 'agent-A', '--id', idA, '--dir', dirA]).status, 0);

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
    const readProgress = (id) => {
      const progressPath = path.join(HOME, 'watchers', `${id}.progress`);
      let text;
      try {
        text = fs.readFileSync(progressPath, 'utf8');
      } catch (error) {
        if (error?.code === 'ENOENT') return undefined;
        throw error;
      }
      assert.match(text, /^(?:0|[1-9]\d*)\n$/, `${id} progress must contain one canonical integer`);
      const value = Number(text.slice(0, -1));
      assert.ok(Number.isSafeInteger(value), `${id} progress must be a safe integer`);
      return value;
    };
    const waitForProgressAfter = (id, previous, label) => {
      let observed;
      waitFor(() => {
        observed = readProgress(id);
        return observed !== undefined && observed > previous;
      }, label);
      return observed;
    };
    const waitForObservedMutation = (id, previous, label) => {
      const first = waitForProgressAfter(id, previous, `${label} first completed cycle`);
      return waitForProgressAfter(id, first, `${label} second completed cycle`);
    };
    const busSendResult = (to, body) =>
      toolJSON(
        runBus(dirA, [
          { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
          { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'send', arguments: { to, body } } },
        ]).get(2),
      );

    check('follow watcher provides live/dead/never/unknown status and tail -n0 -F semantics', () => {
      const id = '12121212-1212-4212-8212-121212121212';
      const dir = path.join(HOME, 'proj-follow');
      fs.mkdirSync(dir, { recursive: true });
      assert.equal(relay(['register', 'follow-target', '--id', id, '--dir', dir]).status, 0);
      assert.equal(relay(['send', 'follow-target', '--', 'preexisting-skip']).status, 0);

      const followed = spawnToFiles(['watch', '--follow', id], {}, 'follow-watch');
      const lock = path.join(HOME, 'watchers', `${id}.lock`);
      waitFor(() => {
        try {
          return JSON.parse(fs.readFileSync(lock, 'utf8')).pid === followed.child.pid;
        } catch {
          return false;
        }
      }, 'the follow watcher lock');
      waitForProgressAfter(id, -1, 'the first completed follow cycle');

      const live = busSendResult('follow-target', 'after-start');
      assert.equal(live.recipient_watch, 'live');
      waitFor(() => fs.readFileSync(followed.stdoutPath, 'utf8').includes('after-start'), 'the after-start delivery');
      let output = fs.readFileSync(followed.stdoutPath, 'utf8');
      assert.ok(!output.includes('preexisting-skip'), 'startup content was skipped');
      assert.ok(output.includes('after-start'), 'an appended complete line was emitted');
      assert.equal(relay(['send', 'follow-target', '--', 'cli-stable']).stdout, 'queued -> follow-target\n');
      waitFor(
        () => fs.readFileSync(followed.stdoutPath, 'utf8').includes('cli-stable'),
        'the pre-truncation follow offset',
      );

      const mailbox = path.join(HOME, 'mailbox', `${id}.jsonl`);
      const inodeBefore = fs.statSync(mailbox).ino;
      const regrown = `${JSON.stringify({ body: `same-inode-truncate-${'x'.repeat(4096)}` })}\n`;
      fs.writeFileSync(mailbox, regrown);
      assert.equal(fs.statSync(mailbox).ino, inodeBefore, 'truncate/regrow fixture preserves the inode');
      waitFor(
        () => fs.readFileSync(followed.stdoutPath, 'utf8').includes(regrown),
        'the complete line after same-inode truncate/regrow',
      );

      let previousProgress = readProgress(id);
      relayJSON(['inbox', 'follow-target']); // remove the current inode through the binary
      waitForObservedMutation(id, previousProgress, 'the deleted mailbox observation');
      previousProgress = readProgress(id);
      fs.appendFileSync(mailbox, '{"partial":'); // raw fixture: store API only emits complete lines
      waitForObservedMutation(id, previousProgress, 'the partial record observation');
      output = fs.readFileSync(followed.stdoutPath, 'utf8');
      assert.ok(!output.includes('{"partial":'), 'partial content stayed buffered');
      fs.appendFileSync(mailbox, '"done"}\n');
      waitFor(
        () => fs.readFileSync(followed.stdoutPath, 'utf8').includes('{"partial":"done"}\n'),
        'the completed partial record',
      );
      output = fs.readFileSync(followed.stdoutPath, 'utf8');
      assert.ok(output.includes('{"partial":"done"}\n'), 'partial content flushed once newline arrived');

      previousProgress = readProgress(id);
      relayJSON(['inbox', 'follow-target']);
      waitForObservedMutation(id, previousProgress, 'the second deleted mailbox observation');
      busSendResult('follow-target', 'after-recreate');
      waitFor(
        () => fs.readFileSync(followed.stdoutPath, 'utf8').includes('after-recreate'),
        'the after-recreate delivery',
      );
      output = fs.readFileSync(followed.stdoutPath, 'utf8');
      assert.ok(output.includes('after-recreate'), 'follow resumed after mailbox delete/recreate');

      followed.child.kill('SIGKILL');
      sleep(100);
      assert.equal(busSendResult('follow-target', 'after-kill').recipient_watch, 'dead');

      const neverId = '13131313-1313-4313-8313-131313131313';
      assert.equal(relay(['register', 'never-watch', '--id', neverId, '--dir', dir]).status, 0);
      assert.equal(busSendResult('never-watch', 'never').recipient_watch, 'never');

      const unknownId = '14141414-1414-4414-8414-141414141414';
      assert.equal(relay(['register', 'unknown-watch', '--id', unknownId, '--dir', dir]).status, 0);
      const unknownLock = path.join(HOME, 'watchers', `${unknownId}.lock`);
      fs.writeFileSync(unknownLock, '{}', { mode: 0o000 });
      try {
        assert.equal(busSendResult('unknown-watch', 'unknown').recipient_watch, 'unknown');
      } finally {
        fs.chmodSync(unknownLock, 0o600);
      }
      const recipientId = testUuid(0x701);
      const senderId = testUuid(0x702);
      const typedDir = path.join(HOME, 'proj-follow-typed');
      fs.mkdirSync(typedDir, { recursive: true });
      assert.equal(relay(['register', 'follow-typed-target', '--id', recipientId, '--dir', typedDir]).status, 0);
      assert.equal(relay(['register', 'follow-typed-sender', '--id', senderId, '--dir', typedDir]).status, 0);

      const typedMailbox = path.join(HOME, 'mailbox', `${recipientId}.jsonl`);
      fs.writeFileSync(typedMailbox, '', { mode: 0o600 });
      const typedFollowed = spawnToFiles(['watch', '--follow', recipientId], {}, 'follow-typed-watch');
      const typedLock = path.join(HOME, 'watchers', `${recipientId}.lock`);
      waitFor(() => {
        try {
          return JSON.parse(fs.readFileSync(typedLock, 'utf8')).pid === typedFollowed.child.pid;
        } catch {
          return false;
        }
      }, 'the typed follow watcher lock');
      waitForProgressAfter(recipientId, -1, 'the typed follow watcher first cycle');
      assert.equal(fs.readFileSync(typedFollowed.stdoutPath, 'utf8'), '', 'an empty mailbox emits no follow bytes');

      const { claims, rows } = followedDeliveryRows({ recipientId, senderId, seed: 0x710 });
      seedClaimFixtures(HOME, claims);
      const payload = `${rows.map((row) => JSON.stringify(row)).join('\n')}\n`;
      fs.appendFileSync(typedMailbox, payload);
      waitFor(
        () => fs.readFileSync(typedFollowed.stdoutPath, 'utf8') === payload,
        'the canonical typed and legacy follow rows',
      );
      const typedOutput = fs.readFileSync(typedFollowed.stdoutPath, 'utf8');
      assert.equal(typedOutput, payload, 'follow does not rewrite canonical MessageV2 or legacy JSONL bytes');
      assert.deepEqual(
        typedOutput
          .trimEnd()
          .split('\n')
          .map((line) => JSON.parse(line)),
        rows,
        'every correlation, reply, status, digest, collection, and legacy field survives follow',
      );
      typedFollowed.child.kill('SIGKILL');
      sleep(100);
    });

    check('follow detects consumed-prefix rewrites that preserve the prior 64-byte suffix', () => {
      const id = '20202020-2020-4020-8020-202020202020';
      const dir = path.join(HOME, 'proj-follow-preserved-suffix');
      fs.mkdirSync(dir, { recursive: true });
      assert.equal(relay(['register', 'preserved-suffix', '--id', id, '--dir', dir]).status, 0);

      const watched = spawnToFiles(['watch', '--follow', id], {}, 'preserved-suffix-watch');
      const lock = path.join(HOME, 'watchers', `${id}.lock`);
      waitFor(() => {
        try {
          return JSON.parse(fs.readFileSync(lock, 'utf8')).pid === watched.child.pid;
        } catch {
          return false;
        }
      }, 'the preserved-suffix watcher lock');
      waitForProgressAfter(id, -1, 'the first completed preserved-suffix cycle');

      const mailbox = path.join(HOME, 'mailbox', `${id}.jsonl`);
      const sharedSuffix = `${'s'.repeat(61)}"}\n`;
      const line = (marker) => `{"body":"${marker}${'p'.repeat(256)}${sharedSuffix}`;
      const original = line('old-prefix');
      const equalReplacement = line('new-prefix');
      assert.equal(original.length, equalReplacement.length);
      assert.equal(original.slice(-64), equalReplacement.slice(-64));

      try {
        fs.appendFileSync(mailbox, original);
        waitFor(
          () => fs.readFileSync(watched.stdoutPath, 'utf8').includes(original),
          'the original preserved-suffix line',
        );
        const inode = fs.statSync(mailbox).ino;

        fs.writeFileSync(mailbox, equalReplacement);
        assert.equal(fs.statSync(mailbox).ino, inode, 'equal-length rewrite preserves the inode');
        waitFor(
          () => fs.readFileSync(watched.stdoutPath, 'utf8').includes(equalReplacement),
          'the equal-length replacement with an unchanged 64-byte suffix',
        );

        const longerReplacement = line('alt-prefix');
        const appended = '{"extra":"after-longer-rewrite"}\n';
        assert.equal(equalReplacement.slice(-64), longerReplacement.slice(-64));
        fs.writeFileSync(mailbox, `${longerReplacement}${appended}`);
        assert.equal(fs.statSync(mailbox).ino, inode, 'longer rewrite preserves the inode');
        waitFor(
          () => fs.readFileSync(watched.stdoutPath, 'utf8').includes(longerReplacement),
          'the longer replacement with an unchanged consumed suffix',
        );
        waitFor(
          () => fs.readFileSync(watched.stdoutPath, 'utf8').includes(appended),
          'the append after the longer replacement',
        );
      } finally {
        watched.child.kill('SIGKILL');
      }
    });

    check('follow watcher exits and releases its lock when the stdout consumer closes', () => {
      const id = '18181818-1818-4818-8818-181818181818';
      const dir = path.join(HOME, 'proj-follow-closed-stdout');
      fs.mkdirSync(dir, { recursive: true });
      assert.equal(relay(['register', 'closed-stdout', '--id', id, '--dir', dir]).status, 0);

      const watched = trackChild(
        spawn(BIN, ['watch', '--follow', id], {
          detached: true,
          env: envFor(),
          stdio: ['ignore', 'pipe', 'ignore'],
        }),
        { processGroup: true },
      );
      const lock = path.join(HOME, 'watchers', `${id}.lock`);
      waitFor(() => {
        try {
          return JSON.parse(fs.readFileSync(lock, 'utf8')).pid === watched.pid;
        } catch {
          return false;
        }
      }, 'the closed-stdout watcher lock');
      waitForProgressAfter(id, -1, 'the first completed closed-stdout cycle');
      watched.stdout.destroy();
      try {
        assert.equal(relay(['send', 'closed-stdout', '--', 'break the closed pipe']).status, 0);
        waitFor(
          () => busSendResult('closed-stdout', 'probe output-failure liveness').recipient_watch === 'dead',
          'the watcher lock to release after its stdout closes',
        );
      } finally {
        watched.kill('SIGKILL');
      }
    });

    check('watch rejects an invalid --tool before writing lock metadata', () => {
      const id = '19191919-1919-4919-8919-191919191919';
      const r = spawnSync(BIN, ['watch', '--follow', id, '--tool', 'bogus'], {
        encoding: 'utf8',
        env: envFor(),
        timeout: 1000,
      });
      assert.equal(r.status, 1, `invalid tool should exit 1, got status=${r.status} signal=${r.signal}`);
      assert.match(r.stderr, /--tool must be claude\|codex/);
      assert.equal(fs.existsSync(path.join(HOME, 'watchers', `${id}.lock`)), false);
    });

    check('doctor verifies a live receive path, reports dead re-arm, and honors --id in a shared dir', () => {
      const shared = path.join(HOME, 'proj-doctor-shared');
      fs.mkdirSync(shared, { recursive: true });
      const doctorA = '15151515-1515-4515-8515-151515151515';
      const doctorB = '16161616-1616-4616-8616-161616161616';
      runHook({ session_id: doctorA, cwd: shared, source: 'startup' });
      relay(['register', 'doctor-a', '--id', doctorA, '--dir', shared]);
      runHook({ session_id: doctorB, cwd: shared, source: 'startup' });
      relay(['register', 'doctor-b', '--id', doctorB, '--dir', shared]);

      const watched = spawnToFiles(['watch', '--follow', doctorA], {}, 'doctor-watch');
      const lock = path.join(HOME, 'watchers', `${doctorA}.lock`);
      waitFor(() => {
        try {
          return JSON.parse(fs.readFileSync(lock, 'utf8')).pid === watched.child.pid;
        } catch {
          return false;
        }
      }, 'the doctor watcher lock');
      waitForProgressAfter(doctorA, -1, 'the first completed doctor follow cycle');

      const healthy = relay(['doctor', '--id', 'doctor-a']);
      assert.equal(healthy.status, 0, `healthy doctor exited ${healthy.status}: ${healthy.stdout}\n${healthy.stderr}`);
      assert.ok(
        healthy.stdout
          .trim()
          .split('\n')
          .every((line) => line.startsWith('PASS ')),
        'healthy explicit-id doctor is all PASS',
      );

      const hookRun = runHook({ session_id: doctorA, cwd: shared, source: 'resume' });
      const context = JSON.parse(hookRun.stdout).hookSpecificOutput.additionalContext;
      const rearm = /run `([^`]+)` as a persistent watch/.exec(context)?.[1];
      assert.ok(rearm, 'hook nudge exposes the re-arm command');
      watched.child.kill('SIGKILL');
      sleep(100);

      const dead = relay(['doctor', '--id', 'doctor-a']);
      assert.equal(dead.status, 1);
      assert.match(dead.stdout, /FAIL watcher: dead/);
      assert.ok(dead.stdout.includes(`fix: ${rearm}`), 'doctor and hook share the exact re-arm command');

      const eachA = relay(['doctor', '--id', doctorA]);
      const eachB = relay(['doctor', '--id', 'doctor-b']);
      assert.ok(eachA.stdout.includes(`PASS identity: ${doctorA}`));
      assert.ok(eachB.stdout.includes(`PASS identity: ${doctorB}`));
      runHook({ session_id: doctorB, cwd: shared, source: 'resume' });
      const fallback = relay(['doctor'], { cwd: shared });
      assert.match(fallback.stdout, new RegExp(`single-session-only fallback resolved ${doctorB}`));
    });

    check('doctor gives an actionable command for an unknown named identity', () => {
      const r = relay(['doctor', '--id', 'not-registered']);
      assert.equal(r.status, 1);
      assert.match(r.stdout, /FAIL identity: unknown session not-registered — fix: relay list/);
    });
    assert.deepEqual(labels, EXPECTED_LABELS);
    return { count: labels.length, labels };
  } finally {
    await fixture.cleanup();
  }
}

const isMain = process.argv[1] !== undefined && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) await runScenarioCli({ scenario: 'follow-doctor-mailbox', run });
