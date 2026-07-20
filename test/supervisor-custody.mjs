#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

if (process.argv[2] !== '--matrix') {
  console.error('usage: node supervisor-custody.mjs --matrix');
  process.exit(2);
}

const ROOT = path.resolve(import.meta.dirname, '..', '..', '..');
const BIN = process.env.RELAY_BIN
  ? path.resolve(process.env.RELAY_BIN)
  : path.join(ROOT, 'plugins/session-relay/rust/target/debug/relay');
assert.ok(fs.existsSync(BIN), `missing test binary: ${BIN}`);

function fresh(tag) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), `relay-supervisor-${tag}-`));
  for (const dir of ['mailbox', 'markers', 'watchers', 'locks']) {
    fs.mkdirSync(path.join(home, dir), { recursive: true, mode: 0o700 });
  }
  return home;
}

function seed(home, session, cwd) {
  fs.mkdirSync(cwd, { recursive: true });
  fs.writeFileSync(
    path.join(home, 'registry.json'),
    JSON.stringify(
      {
        agents: {
          [session]: {
            id: session,
            dir: cwd,
            name: null,
            tool: 'claude',
            lastSeen: new Date().toISOString(),
            server: null,
            spawned_via: null,
          },
        },
        names: {},
      },
      null,
      2,
    ),
  );
}

function writeExecutable(file, body) {
  fs.writeFileSync(file, body, { mode: 0o755 });
  fs.chmodSync(file, 0o755);
}

function lifecycle(home) {
  const authority = JSON.parse(fs.readFileSync(path.join(home, 'lifecycle-v1.json'), 'utf8'));
  assert.equal(authority.schema_version, '1');
  return authority.state;
}

function operationRows(home) {
  const state = lifecycle(home);
  return [...Object.values(state.active_operations ?? {}), ...Object.values(state.operation_tombstones ?? {})];
}

async function waitFor(label, predicate, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = predicate();
    if (value) return value;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error(`timeout waiting for ${label}`);
}

function processExists(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function processSessionId(pid) {
  const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
  return Number(stat.slice(stat.lastIndexOf(') ') + 2).split(' ')[3]);
}

function ptyMatrix() {
  const home = fresh('pty');
  const cwd = path.join(home, 'project');
  const session = '81111111-1111-4111-8111-111111111111';
  seed(home, session, cwd);
  const binDir = path.join(home, 'bin');
  fs.mkdirSync(binDir);
  writeExecutable(
    path.join(binDir, 'claude'),
    `#!/bin/sh
if test -t 0; then in_tty=yes; else in_tty=no; fi
if test -t 1; then out_tty=yes; else out_tty=no; fi
if test -t 2; then err_tty=yes; else err_tty=no; fi
printf 'tty:%s:%s:%s\n' "$in_tty" "$out_tty" "$err_tty"
size=$(stty size); printf 'size:%s\n' "$size"
pgid=$(ps -o pgid= -p $$ | tr -d ' ')
tpgid=$(ps -o tpgid= -p $$ | tr -d ' ')
printf 'foreground:%s\n' "$([ "$pgid" = "$tpgid" ] && printf yes || printf no)"
trap 'printf got-int\\n; exit 0' INT
while IFS= read -r line; do printf 'input:%s\n' "$line"; done
`,
  );
  const result = spawnSync(
    'sh',
    [
      '-c',
      `(printf 'exact-pty-input\\n'; sleep 1; printf '\\003') | script -qefc '${BIN} attach ${session}' /dev/null`,
    ],
    {
      env: {
        ...process.env,
        AGENT_RELAY_HOME: home,
        PATH: `${binDir}${path.delimiter}${process.env.PATH ?? ''}`,
      },
      encoding: 'utf8',
      timeout: 10000,
    },
  );
  assert.equal(result.error, undefined, String(result.error));
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  assert.match(result.stdout, /tty:yes:yes:yes/);
  assert.match(result.stdout, /size:(24 80|[1-9][0-9]* [1-9][0-9]*)/);
  assert.match(result.stdout, /foreground:yes/);
  assert.match(result.stdout, /input:exact-pty-input/);
  assert.match(result.stdout, /got-int/);
  fs.rmSync(home, { recursive: true, force: true });
}

async function floodDisconnectMatrix() {
  const home = fresh('flood');
  const cwd = path.join(home, 'project');
  const session = '91111111-1111-4111-8111-111111111111';
  seed(home, session, cwd);
  const binDir = path.join(home, 'bin');
  fs.mkdirSync(binDir);
  writeExecutable(
    path.join(binDir, 'claude'),
    `#!/bin/sh
trap '' TERM HUP INT
while :; do printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'; done
`,
  );
  const attach = spawn(BIN, ['attach', session], {
    env: {
      ...process.env,
      AGENT_RELAY_HOME: home,
      PATH: `${binDir}${path.delimiter}${process.env.PATH ?? ''}`,
    },
    stdio: ['ignore', 'pipe', 'ignore'],
  });
  const owned = await waitFor('owned flood child', () => {
    try {
      const operations = operationRows(home);
      return operations.find((operation) => operation.custody?.kind === 'ChildOwned');
    } catch {
      return undefined;
    }
  });
  const childPid = Number(owned.custody.process.pid);
  const live = lifecycle(home);
  const supervisors = Object.values(live.lifecycle_supervisors ?? {});
  const watchdogs = Object.values(live.lifecycle_watchdogs ?? {});
  assert.equal(supervisors.length, 1, 'one supervisor must own the operation');
  assert.equal(watchdogs.length, 1, 'one watchdog must own the supervisor');
  const supervisor = supervisors[0];
  const watchdog = watchdogs[0];
  assert.equal(supervisor.state, 'Ready');
  assert.equal(supervisor.control_epoch, watchdog.control_epoch);
  assert.match(supervisor.control_nonce_sha256, /^[0-9a-f]{64}$/);
  assert.equal(supervisor.control_nonce_sha256, watchdog.control_nonce_sha256);
  assert.ok(Number(supervisor.heartbeat_at_ms) > 0);
  assert.ok(Number(watchdog.heartbeat_at_ms) > 0);
  const supervisorPid = Number(supervisor.process.pid);
  const watchdogPid = Number(watchdog.process.pid);
  assert.equal(processSessionId(supervisorPid), supervisorPid, 'supervisor must lead an independent session');
  assert.equal(processSessionId(watchdogPid), watchdogPid, 'watchdog must lead an independent session');
  assert.notEqual(supervisorPid, watchdogPid);
  attach.kill('SIGKILL');
  await new Promise((resolve) => attach.once('exit', resolve));
  const terminal = await waitFor('cancelled flood reap', () => {
    const operations = operationRows(home);
    return operations.find(
      (operation) =>
        operation.cancelled === true && operation.terminal === true && operation.custody?.kind === 'ChildReaped',
    );
  });
  assert.equal(terminal.custody.process.pid, String(childPid));
  assert.equal(processExists(childPid), false, 'supervisor must reap the flooded child');
  const finalRegistry = lifecycle(home);
  const servicePids = [
    ...Object.values(finalRegistry.lifecycle_supervisors ?? {}).map((row) => Number(row.process.pid)),
    ...Object.values(finalRegistry.lifecycle_watchdogs ?? {}).map((row) => Number(row.process.pid)),
  ];
  await waitFor('detached custody services to exit', () => servicePids.every((pid) => !processExists(pid)));
  fs.rmSync(home, { recursive: true, force: true, maxRetries: 10, retryDelay: 50 });
}

const spawnSource = fs.readFileSync(path.join(ROOT, 'plugins/session-relay/rust/src/spawn.rs'), 'utf8');
assert.doesNotMatch(spawnSource, /run_child_with_guard_legacy/);
assert.match(spawnSource, /crate::supervisor::run_child_with_guard\(guard, spec\)/);

ptyMatrix();
await floodDisconnectMatrix();
console.log('SUPERVISOR_CUSTODY PASS pipe=rust closed=rust pty=real flood=bounded watchdog=rust stale=cas');
