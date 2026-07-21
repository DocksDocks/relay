import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PLUGIN = path.resolve(HERE, '..');
const SCRUBBED_ENV = [
  'AGENT_RELAY_HOME',
  'AGENT_RELAY_GC_DAYS',
  'RELAY_CLAUDE_PROJECTS',
  'RELAY_CODEX_SESSIONS',
  'CLAUDE_CONFIG_DIR',
  'CLAUDE_PROJECT_DIR',
  'CLAUDE_CODE_SESSION_ID',
  'CODEX_HOME',
  'RELAY_NO_WATCH',
  'RELAY_APP_SERVER',
  'RELAY_CHANNEL_POLL_MS',
  'RELAY_CHANNEL_REGISTER_TIMEOUT_MS',
  'RELAY_TURN_SETTLE_MS',
  'RELAY_TURN_WAIT_MS',
  'RELAY_SPAWN_CMD_CLAUDE',
  'RELAY_SPAWN_CMD_CODEX',
  'RELAY_WAKE_CMD_CLAUDE',
  'RELAY_WAKE_CMD_CODEX',
  'RELAY_SPAWN_TOOL',
  'STUB_RELAY_BIN',
  'STUB_TOOL',
  'STUB_RECORD',
  'STUB_DELAY_MS',
  'STUB_EXIT',
  'STUB_SKIP_HOOK',
  'STUB_STDERR_BYTES',
  'WAKE_STUB_FILE',
  'WAKE_STUB_STDOUT',
  'WAKE_STUB_STDERR',
  'WAKE_STUB_STATUS',
  'WAKE_STUB_DELAY_MS',
  'WAKE_STUB_RECORD',
  'ATTACH_STUB_OUTPUT',
  'ATTACH_STUB_INTERACTIVE',
];

const CLOSE_GRACE_MS = 300;
const CLOSE_KILL_MS = 1000;

function failTestBin(reason) {
  const error = new Error(`SESSION_RELAY_TEST_BIN ${reason}`);
  error.code = 'SESSION_RELAY_TEST_BIN';
  throw error;
}

function validateBinary(configuredBin) {
  if (configuredBin === undefined) failTestBin('is required; set it to a freshly built host executable');
  if (typeof configuredBin !== 'string' || configuredBin.trim() === '') failTestBin('must be nonempty');
  if (!path.isAbsolute(configuredBin)) failTestBin('must be an absolute path');

  let bin;
  try {
    bin = fs.realpathSync(configuredBin);
    if (!fs.statSync(bin).isFile()) failTestBin('must resolve to a regular file');
    fs.accessSync(bin, fs.constants.X_OK);
  } catch (error) {
    if (error?.code === 'SESSION_RELAY_TEST_BIN') throw error;
    failTestBin('must resolve to an executable file');
  }

  const launcher = fs.realpathSync(path.join(PLUGIN, 'bin', 'relay'));
  if (bin === launcher) failTestBin('must not resolve to the plugin launcher');
  return bin;
}

function waitForClose(child, timeoutMs) {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve(true);
  return new Promise((resolve) => {
    let timer;
    const finish = (closed) => {
      clearTimeout(timer);
      child.off('close', onClose);
      child.off('error', onError);
      resolve(closed);
    };
    const onClose = () => finish(true);
    const onError = () => finish(true);
    child.once('close', onClose);
    child.once('error', onError);
    timer = setTimeout(() => finish(false), timeoutMs);
    timer.unref?.();
  });
}

function processTreePids(rootPid) {
  if (!Number.isInteger(rootPid)) return [];
  const result = spawnSync('ps', ['-eo', 'pid=,ppid='], { encoding: 'utf8' });
  if (result.status !== 0) return [rootPid];
  const children = new Map();
  for (const line of result.stdout.split('\n')) {
    const [pidText, parentText] = line.trim().split(/\s+/);
    const pid = Number(pidText);
    const parent = Number(parentText);
    if (!Number.isInteger(pid) || !Number.isInteger(parent)) continue;
    const siblings = children.get(parent) ?? [];
    siblings.push(pid);
    children.set(parent, siblings);
  }
  const pids = [];
  const visit = (pid) => {
    for (const childPid of children.get(pid) ?? []) visit(childPid);
    pids.push(pid);
  };
  visit(rootPid);
  return pids;
}

function signalPids(pids, signal) {
  for (const pid of pids) {
    try {
      process.kill(pid, signal);
    } catch (error) {
      if (error?.code !== 'ESRCH') throw error;
    }
  }
}

function livePids(pids) {
  return pids.filter((pid) => {
    try {
      process.kill(pid, 0);
      return true;
    } catch (error) {
      if (error?.code === 'ESRCH') return false;
      throw error;
    }
  });
}

async function waitForTreeExit(pids, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let remaining = livePids(pids);
  while (remaining.length > 0 && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 20));
    remaining = livePids(remaining);
  }
  return remaining;
}

export function createFixture({ bin: configuredBin, home }) {
  const bin = validateBinary(configuredBin);
  if (typeof home !== 'string' || !path.isAbsolute(home)) throw new Error('fixture home must be an absolute path');
  if (fs.existsSync(home)) throw new Error('fixture home must not already exist');

  const cargoManifest = fs.readFileSync(path.join(PLUGIN, 'rust', 'Cargo.toml'), 'utf8');
  const packageSection = cargoManifest.split(/^\[package\]\s*$/m)[1]?.split(/^\[/m)[0];
  const cargoVersion = packageSection?.match(/^version\s*=\s*"([^"]+)"\s*$/m)?.[1];
  assert.ok(cargoVersion, 'Cargo package version is present');

  fs.mkdirSync(home, { recursive: false, mode: 0o700 });

  const tracked = new Map();
  let passed = 0;
  let cleanupPromise;

  function envFor(extra = {}) {
    const env = { ...process.env };
    for (const key of SCRUBBED_ENV) delete env[key];
    return { ...env, ...extra, SESSION_RELAY_HOME: home };
  }

  const relay = (args, opts = {}) =>
    spawnSync(bin, args, { encoding: 'utf8', input: opts.input, cwd: opts.cwd, env: envFor(opts.env) });
  const relayBytes = (args, opts = {}) =>
    spawnSync(bin, args, { input: opts.input, cwd: opts.cwd, env: envFor(opts.env) });
  const relayJSON = (args, opts = {}) => {
    const result = relay(args, opts);
    if (result.status !== 0) throw new Error(`relay ${args[0]} exited ${result.status}: ${result.stderr}`);
    return JSON.parse(result.stdout);
  };
  const runHook = (event) => relay(['hook'], { input: JSON.stringify(event) });
  const hookArgs = (args, event, env) => relay(['hook', ...args], { input: JSON.stringify(event), env });
  const peek = (who) => relayJSON(['peek', who]);

  function runBus(projectDir, requests, extraEnv = {}) {
    const input = `${requests.map((request) => JSON.stringify(request)).join('\n')}\n`;
    const result = spawnSync(bin, ['bus'], {
      input,
      encoding: 'utf8',
      env: envFor({ RELAY_PROJECT_DIR: projectDir, ...extraEnv }),
    });
    if (result.status !== 0 && result.status !== null) throw new Error(`bus exited ${result.status}: ${result.stderr}`);
    const byId = new Map();
    for (const line of (result.stdout || '').split('\n').filter(Boolean)) {
      const message = JSON.parse(line);
      if (message.id !== undefined) byId.set(message.id, message);
    }
    return byId;
  }

  const toolJSON = (response) => JSON.parse(response.result.content[0].text);

  function configValues(args) {
    const values = [];
    for (let index = 1; index < args.length; index += 1) {
      if (args[index - 1] === '-c') values.push(args[index]);
    }
    return values;
  }

  function check(label, fn) {
    fn();
    passed += 1;
    console.log(`  ok: ${label}`);
  }

  function trackChild(child, { processGroup = false } = {}) {
    if (!child || typeof child.once !== 'function' || typeof child.kill !== 'function')
      throw new TypeError('trackChild requires a ChildProcess');
    if (cleanupPromise) {
      try {
        child.kill('SIGKILL');
      } catch {}
      throw new Error('cannot track a child after fixture cleanup has started');
    }
    if (child.exitCode !== null || child.signalCode !== null) return child;
    const record = { child, processGroup };
    tracked.set(child, record);
    const forget = () => tracked.delete(child);
    child.once('close', forget);
    return child;
  }

  async function terminate(record) {
    const { child, processGroup } = record;
    if (child.exitCode !== null || child.signalCode !== null) return;
    const pids = processTreePids(child.pid);
    if (processGroup && Number.isInteger(child.pid)) signalPids([-child.pid], 'SIGTERM');
    else signalPids(pids, 'SIGTERM');
    let remaining = await waitForTreeExit(pids, CLOSE_GRACE_MS);
    if (remaining.length > 0) {
      if (processGroup && Number.isInteger(child.pid)) signalPids([-child.pid], 'SIGKILL');
      signalPids(remaining, 'SIGKILL');
      remaining = await waitForTreeExit(remaining, CLOSE_KILL_MS);
    }
    const closed = await waitForClose(child, CLOSE_KILL_MS);
    const survivors = livePids(remaining);
    if (survivors.length > 0) throw new Error(`tracked child process tree survived SIGKILL: ${survivors.join(', ')}`);
    if (!closed) throw new Error(`tracked child did not close after SIGKILL: ${String(child.pid ?? '<unknown>')}`);
  }

  function cleanup() {
    if (cleanupPromise) return cleanupPromise;
    cleanupPromise = (async () => {
      const records = [...tracked.values()];
      const settlements = await Promise.allSettled(records.map(terminate));
      tracked.clear();
      const failures = settlements
        .filter((settlement) => settlement.status === 'rejected')
        .map((settlement) => settlement.reason);
      try {
        fs.rmSync(home, { recursive: true, force: true });
      } catch (error) {
        failures.push(error);
      }
      if (failures.length === 1) throw failures[0];
      if (failures.length > 1) {
        const firstFailure = failures[0];
        const messages = failures.map((failure) => (failure instanceof Error ? failure.message : String(failure)));
        throw new AggregateError(failures, messages.join('\n'), { cause: firstFailure });
      }
    })();
    return cleanupPromise;
  }

  return {
    bin,
    home,
    cargoVersion,
    envFor,
    relay,
    relayBytes,
    relayJSON,
    runHook,
    hookArgs,
    runBus,
    toolJSON,
    configValues,
    peek,
    check,
    get passed() {
      return passed;
    },
    trackChild,
    cleanup,
  };
}

export function createScenarioCheck({ emit, labels }) {
  if (typeof emit !== 'function') throw new TypeError('scenario emit must be a function');
  if (!Array.isArray(labels)) throw new TypeError('scenario labels must be an array');

  return (label, assertion) => {
    if (typeof label !== 'string' || label.length === 0) throw new TypeError('scenario check label must be nonempty');
    if (typeof assertion !== 'function') throw new TypeError('scenario check assertion must be a function');
    const result = assertion();
    if (result && typeof result.then === 'function')
      throw new TypeError('scenario check assertions must be synchronous');
    labels.push(label);
    emit(`  ok: ${label}`);
  };
}

function requiredAbsoluteEnv(name) {
  const value = process.env[name];
  if (value === undefined || value.trim() === '') throw new Error(`${name} is required`);
  if (!path.isAbsolute(value)) throw new Error(`${name} must be an absolute path`);
  return path.resolve(value);
}

function pathIsInside(parent, candidate) {
  const relative = path.relative(parent, candidate);
  return relative === '' || (!relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative));
}

function removeResultBestEffort(file) {
  try {
    fs.rmSync(file, { force: true });
  } catch {}
}

export async function runScenarioCli({ scenario, run }) {
  let resultPath;
  let temporaryResult;
  try {
    if (typeof scenario !== 'string' || scenario.length === 0) throw new TypeError('scenario must be nonempty');
    if (typeof run !== 'function') throw new TypeError('scenario run must be a function');

    const bin = requiredAbsoluteEnv('SESSION_RELAY_TEST_BIN');
    const home = requiredAbsoluteEnv('SESSION_RELAY_SCENARIO_HOME');
    resultPath = requiredAbsoluteEnv('SESSION_RELAY_SCENARIO_RESULT');
    if (pathIsInside(home, resultPath)) {
      throw new Error('SESSION_RELAY_SCENARIO_RESULT must be outside SESSION_RELAY_SCENARIO_HOME');
    }

    fs.rmSync(resultPath, { force: true });
    const outcome = await run({ bin, home, emit: console.log });
    if (!outcome || typeof outcome !== 'object') throw new TypeError('scenario run must return a result object');
    const { count, labels } = outcome;
    if (!Number.isSafeInteger(count) || count < 0) {
      throw new TypeError('scenario result count must be a nonnegative integer');
    }
    if (!Array.isArray(labels) || labels.some((label) => typeof label !== 'string' || label.length === 0)) {
      throw new TypeError('scenario result labels must be nonempty strings');
    }
    if (labels.length !== count) throw new Error('scenario result count must equal labels length');

    const payload = { schema: 1, scenario, status: 'passed', count, labels };
    temporaryResult = `${resultPath}.${process.pid}.${randomUUID()}.tmp`;
    fs.writeFileSync(temporaryResult, JSON.stringify(payload), { encoding: 'utf8', flag: 'wx', mode: 0o600 });
    fs.renameSync(temporaryResult, resultPath);
    temporaryResult = undefined;
    return payload;
  } catch (error) {
    if (temporaryResult !== undefined) removeResultBestEffort(temporaryResult);
    if (resultPath !== undefined) removeResultBestEffort(resultPath);
    console.error(error instanceof Error ? (error.stack ?? error.message) : String(error));
    process.exitCode = 1;
    return undefined;
  }
}
