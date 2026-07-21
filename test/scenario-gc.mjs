#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createFixture, createScenarioCheck, runScenarioCli } from './selftest-fixture.mjs';

export const EXPECTED_LABELS = [
  'GC refuses a symlinked surface without touching its target or permissions',
  'GC ignores unreadable aged foreign files and still collects eligible relay state',
  'GC fails closed on a fresh unreadable marker for an otherwise-aged session',
  'GC cannot follow a mailbox-directory symlink to delete a victim file',
  'GC removes exactly aged registered/orphan surfaces and preserves young state',
  'GC preserves an aged session while its watcher lock is held',
  'a live per-log pump protects only its candidate while GC collects an unrelated aged log',
  'spawn-log pump lock follows a provisional log rename to the born session name',
  'GC never removes the invoking session even when all its surfaces are aged',
  'AGENT_RELAY_GC_DAYS=0 disables GC without writing a stamp',
  'fresh gc-stamp throttles an immediate second sweep',
];

export async function run({ bin, home, emit }) {
  const fixture = createFixture({ bin, home });
  const labels = [];
  const check = createScenarioCheck({ emit, labels });
  const { bin: BIN, home: HOME, envFor, trackChild } = fixture;
  const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
  const waitFor = (predicate, label, timeoutMs = 5000) => {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (predicate()) return;
      sleep(25);
    }
    assert.fail(`timed out waiting for ${label}`);
  };

  try {
    // --- store GC: each case owns an AGENT_RELAY_HOME sandbox. Normal structure
    // is seeded through the binary; only lastSeen and mtimes are aged directly. ---
    const gcOld = new Date(Date.now() - 20 * 86400_000);
    const gcEnv = (home, extra = {}) =>
      envFor({
        AGENT_RELAY_HOME: home,
        AGENT_RELAY_GC_DAYS: '14',
        RELAY_NO_WATCH: '1',
        ...extra,
      });
    const gcRun = (home, args, opts = {}) =>
      spawnSync(BIN, args, {
        encoding: 'utf8',
        input: opts.input,
        cwd: opts.cwd,
        env: gcEnv(home, opts.env),
      });
    const gcMarkerName = (dir) => path.resolve(dir).replace(/[^a-zA-Z0-9]/g, '-');
    const gcSurfaces = (home, id, dir) => [
      path.join(home, 'mailbox', `${id}.jsonl`),
      path.join(home, 'markers', gcMarkerName(dir)),
      path.join(home, 'watchers', `${id}.lock`),
      path.join(home, 'watchers', `${id}.progress`),
      path.join(home, 'locks', `resume-${id}.lock`),
      path.join(home, 'spawn-logs', `${id}.stderr`),
    ];
    const seedGcSession = (home, name, id) => {
      const dir = path.join(home, `project-${name}`);
      fs.mkdirSync(dir, { recursive: true });
      const event = JSON.stringify({ session_id: id, cwd: dir, hook_event_name: 'SessionStart', source: 'startup' });
      assert.equal(gcRun(home, ['hook'], { input: event }).status, 0);
      assert.equal(gcRun(home, ['register', name, '--id', id, '--dir', dir]).status, 0);
      fs.mkdirSync(path.join(home, 'spawn-logs'), { recursive: true });
      fs.writeFileSync(path.join(home, 'mailbox', `${id}.jsonl`), '{}\n');
      fs.writeFileSync(path.join(home, 'watchers', `${id}.lock`), '{}');
      fs.writeFileSync(path.join(home, 'watchers', `${id}.progress`), '0\n');
      fs.writeFileSync(path.join(home, 'locks', `resume-${id}.lock`), '{}');
      fs.writeFileSync(path.join(home, 'spawn-logs', `${id}.stderr`), 'log\n');
      return { id, dir, paths: gcSurfaces(home, id, dir) };
    };
    const ageGcSession = (home, session) => {
      const registryPath = path.join(home, 'registry.json');
      const registry = JSON.parse(fs.readFileSync(registryPath, 'utf8'));
      registry.agents[session.id].lastSeen = gcOld.toISOString();
      fs.writeFileSync(registryPath, JSON.stringify(registry, null, 2));
      for (const file of session.paths) fs.utimesSync(file, gcOld, gcOld);
    };
    const runGcBus = (home, dir, env = {}) => {
      const r = gcRun(home, ['bus'], { input: '', env: { RELAY_PROJECT_DIR: dir, ...env } });
      assert.equal(r.status, 0, `GC bus exited ${r.status}: ${r.stderr}`);
      return r;
    };
    const makeGcSurfaceDirs = (home, omit = []) => {
      for (const directory of ['mailbox', 'markers', 'watchers', 'locks', 'spawn-logs']) {
        if (!omit.includes(directory)) fs.mkdirSync(path.join(home, directory), { recursive: true });
      }
    };

    check('GC refuses a symlinked surface without touching its target or permissions', () => {
      const home = fs.mkdtempSync(path.join(HOME, 'gc-symlink-external-'));
      const external = fs.mkdtempSync(path.join(HOME, 'gc-external-'));
      fs.chmodSync(external, 0o755);
      const victim = path.join(external, 'foreign.lock');
      fs.writeFileSync(victim, 'foreign\n');
      makeGcSurfaceDirs(home, ['watchers']);
      fs.symlinkSync(external, path.join(home, 'watchers'));
      const modeBefore = fs.statSync(external).mode & 0o777;

      const r = runGcBus(home, home);

      assert.match(r.stderr, /refusing GC: relay surface directory watchers/);
      assert.equal(fs.existsSync(victim), true, 'external target survives the full bus entry path');
      assert.equal(fs.statSync(external).mode & 0o777, modeBefore, 'external mode is unchanged');
      assert.equal(fs.existsSync(path.join(home, '.lock')), false, 'GC refused before creating its lock');
    });

    check('GC ignores unreadable aged foreign files and still collects eligible relay state', () => {
      const home = fs.mkdtempSync(path.join(HOME, 'gc-foreign-'));
      const aged = seedGcSession(home, 'aged', '43434343-4343-4343-8343-434343434343');
      ageGcSession(home, aged);
      const foreign = [
        path.join(home, 'mailbox', 'notes.jsonl'),
        path.join(home, 'markers', 'not-a-relay-marker'),
        path.join(home, 'watchers', 'notes.lock'),
        path.join(home, 'watchers', 'notes.progress'),
        path.join(home, 'locks', 'resume-notes.lock'),
        path.join(home, 'spawn-logs', 'notes.stderr'),
      ];
      for (const file of foreign) {
        fs.writeFileSync(file, 'not-a-uuid\n');
        fs.utimesSync(file, gcOld, gcOld);
        fs.chmodSync(file, 0o000);
      }
      fs.rmSync(path.join(home, 'gc-stamp'), { force: true });

      runGcBus(home, home);

      assert.ok(
        aged.paths.every((file) => !fs.existsSync(file)),
        'eligible relay surfaces were collected',
      );
      assert.ok(
        foreign.every((file) => fs.existsSync(file)),
        'unreadable foreign files survive in all surfaces',
      );
      assert.equal(fs.existsSync(path.join(home, 'gc-stamp')), true, 'completed sweep writes its stamp');
    });

    check('GC fails closed on a fresh unreadable marker for an otherwise-aged session', () => {
      const home = fs.mkdtempSync(path.join(HOME, 'gc-fresh-marker-'));
      const held = seedGcSession(home, 'held', '47474747-4747-4747-8747-474747474747');
      const invoker = seedGcSession(home, 'invoker', '48484848-4848-4848-8848-484848484848');
      ageGcSession(home, held);
      const marker = path.join(home, 'markers', gcMarkerName(held.dir));
      const fresh = new Date();
      fs.utimesSync(marker, fresh, fresh);
      fs.chmodSync(marker, 0o000);
      fs.rmSync(path.join(home, 'gc-stamp'), { force: true });

      runGcBus(home, invoker.dir);

      assert.ok(
        held.paths.every((file) => fs.existsSync(file)),
        'fresh unknown marker preserves the full session',
      );
      const registry = JSON.parse(fs.readFileSync(path.join(home, 'registry.json'), 'utf8'));
      assert.ok(registry.agents[held.id], 'fresh unknown marker preserves the registry entry');
    });

    check('GC cannot follow a mailbox-directory symlink to delete a victim file', () => {
      const home = fs.mkdtempSync(path.join(HOME, 'gc-victim-'));
      const future = path.join(home, 'future');
      fs.mkdirSync(future);
      makeGcSurfaceDirs(home, ['mailbox']);
      fs.symlinkSync(future, path.join(home, 'mailbox'));
      const victim = path.join(future, '42424242-4242-4242-8242-424242424242.jsonl');
      fs.writeFileSync(victim, 'victim\n');
      fs.utimesSync(victim, gcOld, gcOld);

      const r = runGcBus(home, home);

      assert.match(r.stderr, /refusing GC: relay surface directory mailbox/);
      assert.equal(fs.existsSync(victim), true, 'victim survives the path-check/use layout');
    });

    check('GC removes exactly aged registered/orphan surfaces and preserves young state', () => {
      const home = fs.mkdtempSync(path.join(HOME, 'gc-exact-'));
      const aged = seedGcSession(home, 'aged', '30303030-3030-4030-8030-303030303030');
      const young = seedGcSession(home, 'young', '31313131-3131-4131-8131-313131313131');
      const invoker = seedGcSession(home, 'invoker', '32323232-3232-4232-8232-323232323232');
      ageGcSession(home, aged);
      const orphanId = '33333333-3434-4333-8333-333333333333';
      const orphan = [
        path.join(home, 'mailbox', `${orphanId}.jsonl`),
        path.join(home, 'spawn-logs', `${orphanId}.stderr`),
      ];
      for (const file of orphan) {
        fs.writeFileSync(file, 'orphan\n');
        fs.utimesSync(file, gcOld, gcOld);
      }
      fs.rmSync(path.join(home, 'gc-stamp'), { force: true });
      runGcBus(home, invoker.dir);

      assert.ok(
        aged.paths.every((file) => !fs.existsSync(file)),
        'all aged session surfaces removed',
      );
      assert.ok(
        orphan.every((file) => !fs.existsSync(file)),
        'aged orphan surfaces removed',
      );
      assert.ok(
        young.paths.every((file) => fs.existsSync(file)),
        'young surfaces preserved',
      );
      const registry = JSON.parse(fs.readFileSync(path.join(home, 'registry.json'), 'utf8'));
      assert.equal(registry.agents[aged.id], undefined);
      assert.equal(registry.names.aged, undefined);
      assert.ok(registry.agents[young.id]);
    });

    check('GC preserves an aged session while its watcher lock is held', () => {
      const home = fs.mkdtempSync(path.join(HOME, 'gc-held-'));
      const held = seedGcSession(home, 'held', '34343434-3434-4434-8434-343434343434');
      const invoker = seedGcSession(home, 'invoker', '35353535-3535-4535-8535-353535353535');
      const watcher = trackChild(
        spawn(BIN, ['watch', '--follow', held.id], {
          env: gcEnv(home),
          detached: true,
          stdio: ['ignore', 'ignore', 'ignore'],
        }),
        { processGroup: true },
      );
      try {
        const lock = path.join(home, 'watchers', `${held.id}.lock`);
        waitFor(() => {
          try {
            return JSON.parse(fs.readFileSync(lock, 'utf8')).pid === watcher.pid;
          } catch {
            return false;
          }
        }, 'held GC watcher lock');
        watcher.kill('SIGSTOP');
        sleep(50);
        ageGcSession(home, held);
        fs.rmSync(path.join(home, 'gc-stamp'), { force: true });
        runGcBus(home, invoker.dir);
        assert.ok(
          held.paths.every((file) => fs.existsSync(file)),
          'held-lock session survives intact',
        );
      } finally {
        watcher.kill('SIGKILL');
      }
    });

    check('a live per-log pump protects only its candidate while GC collects an unrelated aged log', () => {
      const home = fs.mkdtempSync(path.join(HOME, 'gc-spawn-pump-'));
      const held = seedGcSession(home, 'held', '44444444-4444-4444-8444-444444444444');
      const collected = seedGcSession(home, 'collected', '46464646-4646-4646-8646-464646464646');
      const invoker = seedGcSession(home, 'invoker', '45454545-4545-4545-8545-454545454545');
      ageGcSession(home, held);
      ageGcSession(home, collected);
      const heldLog = path.join(home, 'spawn-logs', `${held.id}.stderr`);
      fs.rmSync(heldLog);
      fs.rmSync(path.join(home, 'gc-stamp'), { force: true });
      const pump = trackChild(
        spawn(BIN, ['__spawn-log-writer', held.id], {
          env: gcEnv(home),
          detached: true,
          stdio: ['pipe', 'ignore', 'ignore'],
        }),
        { processGroup: true },
      );
      try {
        waitFor(() => pump.exitCode === null && fs.existsSync(heldLog), 'per-log pump liveness lock');
        fs.utimesSync(heldLog, gcOld, gcOld);
        runGcBus(home, invoker.dir);
        assert.ok(
          held.paths.every((file) => fs.existsSync(file)),
          'pump-held candidate survives intact',
        );
        assert.ok(
          collected.paths.every((file) => !fs.existsSync(file)),
          'unrelated aged candidate is collected',
        );
      } finally {
        pump.stdin.end();
        pump.kill('SIGKILL');
      }
    });

    check('spawn-log pump lock follows a provisional log rename to the born session name', () => {
      const home = fs.mkdtempSync(path.join(HOME, 'gc-spawn-rename-'));
      const born = seedGcSession(home, 'born', '49494949-4949-4949-8949-494949494949');
      const invoker = seedGcSession(home, 'invoker', '51515151-5151-4151-8151-515151515151');
      const provisionalId = '52525252-5252-4252-8252-525252525252';
      const bornLog = path.join(home, 'spawn-logs', `${born.id}.stderr`);
      const provisionalLog = path.join(home, 'spawn-logs', `${provisionalId}.stderr`);
      ageGcSession(home, born);
      fs.rmSync(bornLog);
      fs.rmSync(path.join(home, 'gc-stamp'), { force: true });
      const pump = trackChild(
        spawn(BIN, ['__spawn-log-writer', provisionalId], {
          env: gcEnv(home),
          detached: true,
          stdio: ['pipe', 'ignore', 'ignore'],
        }),
        { processGroup: true },
      );
      try {
        waitFor(() => pump.exitCode === null && fs.existsSync(provisionalLog), 'provisional per-log pump lock');
        fs.renameSync(provisionalLog, bornLog);
        fs.utimesSync(bornLog, gcOld, gcOld);
        runGcBus(home, invoker.dir);
        assert.ok(
          born.paths.every((file) => fs.existsSync(file)),
          'renamed pump-held candidate survives intact',
        );
      } finally {
        pump.stdin.end();
        pump.kill('SIGKILL');
      }
    });

    check('GC never removes the invoking session even when all its surfaces are aged', () => {
      const home = fs.mkdtempSync(path.join(HOME, 'gc-self-'));
      const self = seedGcSession(home, 'self', '36363636-3636-4636-8636-363636363636');
      ageGcSession(home, self);
      fs.rmSync(path.join(home, 'gc-stamp'), { force: true });
      runGcBus(home, self.dir);
      assert.ok(
        self.paths.every((file) => fs.existsSync(file)),
        'invoker survives intact',
      );
      const registry = JSON.parse(fs.readFileSync(path.join(home, 'registry.json'), 'utf8'));
      assert.ok(registry.agents[self.id]);
    });

    check('AGENT_RELAY_GC_DAYS=0 disables GC without writing a stamp', () => {
      const home = fs.mkdtempSync(path.join(HOME, 'gc-disabled-'));
      const aged = seedGcSession(home, 'aged', '37373737-3737-4737-8737-373737373737');
      const invoker = seedGcSession(home, 'invoker', '38383838-3838-4838-8838-383838383838');
      ageGcSession(home, aged);
      fs.rmSync(path.join(home, 'gc-stamp'), { force: true });
      runGcBus(home, invoker.dir, { AGENT_RELAY_GC_DAYS: '0' });
      assert.ok(
        aged.paths.every((file) => fs.existsSync(file)),
        'disabled GC preserves aged state',
      );
      assert.equal(fs.existsSync(path.join(home, 'gc-stamp')), false);
    });

    check('fresh gc-stamp throttles an immediate second sweep', () => {
      const home = fs.mkdtempSync(path.join(HOME, 'gc-throttle-'));
      const first = seedGcSession(home, 'first', '39393939-3939-4939-8939-393939393939');
      const invoker = seedGcSession(home, 'invoker', '40404040-4040-4040-8040-404040404040');
      ageGcSession(home, first);
      fs.rmSync(path.join(home, 'gc-stamp'), { force: true });
      runGcBus(home, invoker.dir);
      assert.ok(
        first.paths.every((file) => !fs.existsSync(file)),
        'first sweep ran',
      );

      const second = seedGcSession(home, 'second', '41414141-4141-4141-8141-414141414141');
      ageGcSession(home, second);
      runGcBus(home, invoker.dir);
      assert.ok(
        second.paths.every((file) => fs.existsSync(file)),
        'second sweep was throttled',
      );
    });
    assert.deepEqual(labels, EXPECTED_LABELS);
    return { count: labels.length, labels };
  } finally {
    await fixture.cleanup();
  }
}

const isMain = process.argv[1] !== undefined && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) await runScenarioCli({ scenario: 'gc', run });
