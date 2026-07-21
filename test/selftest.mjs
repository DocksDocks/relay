#!/usr/bin/env node
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { EXPECTED_LABELS as APP_SERVER_LABELS } from './scenario-appserver.mjs';
import { EXPECTED_LABELS as CORE_LABELS } from './scenario-core.mjs';
import { EXPECTED_LABELS as DISCOVERY_HARDENING_LABELS } from './scenario-discovery-hardening.mjs';
import { EXPECTED_LABELS as FOLLOW_DOCTOR_MAILBOX_LABELS } from './scenario-follow-doctor-mailbox.mjs';
import { EXPECTED_LABELS as GC_LABELS } from './scenario-gc.mjs';
import { EXPECTED_LABELS as HOOKS_IDENTITY_LABELS } from './scenario-hooks-identity.mjs';
import { EXPECTED_LABELS as SPAWN_WAKE_SUPERVISOR_LABELS } from './scenario-spawn-wake-supervisor.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PLUGIN = path.resolve(HERE, '..');
const MAX_CAPTURE_BYTES = 16 * 1024 * 1024;
const MAX_RESULT_BYTES = 1024 * 1024;
const TERMINATE_GRACE_MS = 300;
const TERMINATE_KILL_MS = 1000;
const RESULT_CLOCK_TOLERANCE_MS = 1000;
const RESULT_KEYS = ['count', 'labels', 'scenario', 'schema', 'status'];

export const PRE_SPLIT_STDOUT_SHA256 = '8eaa9ecfdc3e5a9ceb72d65cbf2062c0495746a4a31ae7a0ce14c73b9cb5c44f';

function scenario(name, expectedLabels) {
  return Object.freeze({
    name,
    modulePath: path.join(HERE, `scenario-${name}.mjs`),
    expectedLabels: Object.freeze([...expectedLabels]),
  });
}

export const SCENARIOS = Object.freeze([
  scenario('core', CORE_LABELS),
  scenario('discovery-hardening', DISCOVERY_HARDENING_LABELS),
  scenario('hooks-identity', HOOKS_IDENTITY_LABELS),
  scenario('appserver', APP_SERVER_LABELS),
  scenario('gc', GC_LABELS),
  scenario('spawn-wake-supervisor', SPAWN_WAKE_SUPERVISOR_LABELS),
  scenario('follow-doctor-mailbox', FOLLOW_DOCTOR_MAILBOX_LABELS),
]);

export const PRODUCTION_OUTPUT_LABELS = Object.freeze([
  ...CORE_LABELS,
  ...DISCOVERY_HARDENING_LABELS,
  ...HOOKS_IDENTITY_LABELS,
  ...APP_SERVER_LABELS,
  ...GC_LABELS,
  ...SPAWN_WAKE_SUPERVISOR_LABELS.slice(0, -1),
  ...FOLLOW_DOCTOR_MAILBOX_LABELS,
  SPAWN_WAKE_SUPERVISOR_LABELS.at(-1),
]);

function parallelismCap(availableParallelism) {
  if (!Number.isSafeInteger(availableParallelism) || availableParallelism < 1) {
    throw new TypeError('available parallelism must be a positive integer');
  }
  return Math.min(4, availableParallelism);
}

export function parseScenarioJobs(value, availableParallelism = os.availableParallelism()) {
  const maximum = parallelismCap(availableParallelism);
  if (value === undefined) return maximum;
  if (typeof value !== 'string' || !/^[1-9]\d*$/.test(value)) {
    throw new Error(`SESSION_RELAY_TEST_JOBS must be a strict integer in 1..${maximum}`);
  }
  const jobs = Number(value);
  if (!Number.isSafeInteger(jobs) || jobs < 1 || jobs > maximum) {
    throw new Error(`SESSION_RELAY_TEST_JOBS must be a strict integer in 1..${maximum}`);
  }
  return jobs;
}

function validateScenarioDefinition(value, index) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError(`scenario ${index} must be an object`);
  }
  if (typeof value.name !== 'string' || !/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(value.name)) {
    throw new TypeError(`scenario ${index} has an invalid name`);
  }
  if (typeof value.modulePath !== 'string' || !path.isAbsolute(value.modulePath)) {
    throw new TypeError(`scenario ${value.name} modulePath must be absolute`);
  }
  if (
    !Array.isArray(value.expectedLabels) ||
    value.expectedLabels.length === 0 ||
    value.expectedLabels.some((label) => typeof label !== 'string' || label.length === 0)
  ) {
    throw new TypeError(`scenario ${value.name} expectedLabels must be nonempty strings`);
  }
}

function validateScenarioCatalog(scenarios) {
  if (!Array.isArray(scenarios) || scenarios.length === 0) throw new TypeError('scenarios must be a nonempty array');
  const names = new Set();
  const labels = new Set();
  for (const [index, definition] of scenarios.entries()) {
    validateScenarioDefinition(definition, index);
    if (names.has(definition.name)) throw new Error(`duplicate scenario: ${definition.name}`);
    names.add(definition.name);
    for (const label of definition.expectedLabels) {
      if (labels.has(label)) throw new Error(`duplicate scenario label: ${label}`);
      labels.add(label);
    }
  }
}

function validateOutputLabels(outputLabels, scenarios) {
  if (
    !Array.isArray(outputLabels) ||
    outputLabels.length === 0 ||
    outputLabels.some((label) => typeof label !== 'string' || label.length === 0)
  ) {
    throw new TypeError('outputLabels must be a nonempty array of nonempty strings');
  }
  if (new Set(outputLabels).size !== outputLabels.length) {
    throw new Error('outputLabels must not contain duplicate labels');
  }
  const catalogLabels = scenarios.flatMap(({ expectedLabels }) => expectedLabels);
  const catalogSet = new Set(catalogLabels);
  const outputSet = new Set(outputLabels);
  const missing = catalogLabels.filter((label) => !outputSet.has(label));
  const extra = outputLabels.filter((label) => !catalogSet.has(label));
  if (missing.length > 0 || extra.length > 0) {
    throw new Error(
      `outputLabels must contain the exact scenario label union (missing ${missing.length}, extra ${extra.length})`,
    );
  }
  return [...outputLabels];
}

export function validateScenarioResult(value, definition) {
  validateScenarioDefinition(definition, 0);
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError(`scenario ${definition.name} result must be an object`);
  }
  const keys = Object.keys(value).sort();
  if (keys.length !== RESULT_KEYS.length || keys.some((key, index) => key !== RESULT_KEYS[index])) {
    throw new Error(`scenario ${definition.name} result has an invalid schema surface`);
  }
  if (value.schema !== 1) throw new Error(`scenario ${definition.name} result schema must be 1`);
  if (value.scenario !== definition.name) {
    throw new Error(`scenario result name mismatch: expected ${definition.name}, received ${String(value.scenario)}`);
  }
  if (value.status !== 'passed') throw new Error(`scenario ${definition.name} result status must be passed`);
  if (!Number.isSafeInteger(value.count) || value.count < 0) {
    throw new Error(`scenario ${definition.name} result count must be a nonnegative integer`);
  }
  if (!Array.isArray(value.labels) || value.labels.some((label) => typeof label !== 'string' || label.length === 0)) {
    throw new Error(`scenario ${definition.name} result labels must be nonempty strings`);
  }
  if (new Set(value.labels).size !== value.labels.length) {
    throw new Error(`scenario ${definition.name} result contains duplicate labels`);
  }
  if (value.count !== value.labels.length) {
    throw new Error(`scenario ${definition.name} result count does not match labels`);
  }
  if (value.labels.length !== definition.expectedLabels.length) {
    throw new Error(`scenario ${definition.name} result label count is stale`);
  }
  for (const [index, expected] of definition.expectedLabels.entries()) {
    if (value.labels[index] !== expected) {
      throw new Error(`scenario ${definition.name} result label ${index} mismatch`);
    }
  }
  return value;
}

function requireTestBinary(configuredBin) {
  if (configuredBin === undefined) {
    throw new Error('SESSION_RELAY_TEST_BIN is required; set it to a freshly built host executable');
  }
  if (typeof configuredBin !== 'string' || configuredBin.trim() === '') {
    throw new Error('SESSION_RELAY_TEST_BIN must be nonempty');
  }
  if (!path.isAbsolute(configuredBin)) throw new Error('SESSION_RELAY_TEST_BIN must be an absolute path');
  let bin;
  try {
    bin = fs.realpathSync(configuredBin);
    if (!fs.statSync(bin).isFile()) throw new Error('not a file');
    fs.accessSync(bin, fs.constants.X_OK);
  } catch {
    throw new Error('SESSION_RELAY_TEST_BIN must resolve to an executable file');
  }
  const launcher = fs.realpathSync(path.join(PLUGIN, 'bin', 'relay'));
  if (bin === launcher) throw new Error('SESSION_RELAY_TEST_BIN must not resolve to the plugin launcher');
  return bin;
}

function expectedScenarioStdout(definition) {
  return definition.expectedLabels.map((label) => `  ok: ${label}\n`).join('');
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

async function waitForPids(pids, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let remaining = livePids(pids);
  while (remaining.length > 0 && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 20));
    remaining = livePids(remaining);
  }
  return remaining;
}

async function terminateProcessTree(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const pids = processTreePids(child.pid);
  if (Number.isInteger(child.pid)) signalPids([-child.pid], 'SIGTERM');
  signalPids(pids, 'SIGTERM');
  const remaining = await waitForPids(pids, TERMINATE_GRACE_MS);
  if (remaining.length > 0) {
    if (Number.isInteger(child.pid)) signalPids([-child.pid], 'SIGKILL');
    signalPids(remaining, 'SIGKILL');
    await waitForPids(remaining, TERMINATE_KILL_MS);
  }
  await waitForClose(child, TERMINATE_KILL_MS);
}

function boundedUtf8Prefix(bytes, maxBytes) {
  const decoded = bytes.subarray(0, maxBytes).toString('utf8');
  if (Buffer.byteLength(decoded) <= maxBytes) return decoded;

  let retainedBytes = 0;
  let retainedCodeUnits = 0;
  for (const character of decoded) {
    const characterBytes = Buffer.byteLength(character);
    if (retainedBytes + characterBytes > maxBytes) break;
    retainedBytes += characterBytes;
    retainedCodeUnits += character.length;
  }
  return decoded.slice(0, retainedCodeUnits);
}

function createCapture(maxOutputBytes, onOverflow) {
  const chunks = { stdout: [], stderr: [] };
  const capturedBytes = { stdout: 0, stderr: 0 };
  let observedBytes = 0;
  let overflow = false;

  const append = (stream, chunk) => {
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    observedBytes += bytes.length;

    const remaining = maxOutputBytes - capturedBytes[stream];
    if (remaining > 0) {
      const retained = bytes.length <= remaining ? bytes : bytes.subarray(0, remaining);
      chunks[stream].push(retained);
      capturedBytes[stream] += retained.length;
    }

    if (observedBytes > maxOutputBytes && !overflow) {
      overflow = true;
      onOverflow();
    }
  };
  return {
    append,
    get overflow() {
      return overflow;
    },
    snapshot() {
      const stdout = boundedUtf8Prefix(Buffer.concat(chunks.stdout), maxOutputBytes);
      const stderrLimit = maxOutputBytes - Buffer.byteLength(stdout);
      return {
        stdout,
        stderr: stderrLimit > 0 ? boundedUtf8Prefix(Buffer.concat(chunks.stderr), stderrLimit) : '',
      };
    },
  };
}

function launchScenarioProcess(spec) {
  let terminationPromise;
  let child;
  const capture = createCapture(spec.maxOutputBytes, () => {
    if (child) void terminate();
  });
  child = spawn(process.execPath, [spec.scenario.modulePath], {
    detached: true,
    env: {
      ...process.env,
      ...spec.scenario.env,
      SESSION_RELAY_TEST_BIN: spec.bin,
      SESSION_RELAY_SCENARIO_HOME: spec.home,
      SESSION_RELAY_SCENARIO_RESULT: spec.resultPath,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  child.stdout.on('data', (chunk) => capture.append('stdout', chunk));
  child.stderr.on('data', (chunk) => capture.append('stderr', chunk));
  let launchError;
  child.once('error', (error) => {
    launchError = error;
  });
  const completion = new Promise((resolve) => {
    child.once('close', (code, signal) => {
      const output = capture.snapshot();
      resolve({
        code,
        signal,
        ...output,
        overflow: capture.overflow,
        launchError,
      });
    });
  });
  function terminate() {
    terminationPromise ??= terminateProcessTree(child);
    return terminationPromise;
  }
  return { completion, terminate };
}

const scenarioFailureErrors = new WeakSet();

function scenarioFailure(definition, message, outcome = {}, infrastructure = false) {
  const error = new Error(`scenario ${definition.name} ${message}`);
  error.scenario = definition.name;
  error.stdout = typeof outcome.stdout === 'string' ? outcome.stdout : '';
  error.stderr = typeof outcome.stderr === 'string' ? outcome.stderr : '';
  error.infrastructure = infrastructure;
  scenarioFailureErrors.add(error);
  return error;
}

function scenarioSchedulerError(failures) {
  const orderedFailures = [...failures].sort(({ scenarioIndex: left }, { scenarioIndex: right }) => left - right);
  const message =
    orderedFailures.length === 1
      ? orderedFailures[0].message
      : `${orderedFailures.length} scenario failures: ${orderedFailures.map(({ message: detail }) => detail).join('; ')}`;
  const error = new AggregateError(orderedFailures, message);
  error.name = 'ScenarioSchedulerError';
  error.failures = orderedFailures;
  error.infrastructure = orderedFailures.some(({ infrastructure }) => infrastructure);
  if (orderedFailures.length === 1) {
    error.scenario = orderedFailures[0].scenario;
    error.stdout = orderedFailures[0].stdout;
    error.stderr = orderedFailures[0].stderr;
  }
  return error;
}

function normalizeOutcome(definition, outcome, maxOutputBytes) {
  if (!outcome || typeof outcome !== 'object') {
    throw scenarioFailure(definition, 'returned no process outcome', {}, true);
  }
  let stdout;
  let stderr;
  let code;
  let signal;
  let overflow;
  let launchError;
  try {
    stdout = outcome.stdout;
    stderr = outcome.stderr;
    code = outcome.code;
    signal = outcome.signal;
    overflow = outcome.overflow;
    launchError = outcome.launchError;
    if (typeof stdout !== 'string' || typeof stderr !== 'string') {
      throw scenarioFailure(definition, 'returned a malformed process outcome', {}, true);
    }
    if (
      (code !== null && !Number.isInteger(code)) ||
      (signal !== null && typeof signal !== 'string') ||
      (overflow !== undefined && typeof overflow !== 'boolean')
    ) {
      throw scenarioFailure(definition, 'returned a malformed process outcome', { stdout, stderr }, true);
    }
  } catch (error) {
    if (error?.scenario) throw error;
    throw scenarioFailure(
      definition,
      `failed to normalize its process outcome: ${error instanceof Error ? error.message : String(error)}`,
      {},
      true,
    );
  }
  const normalized = { code, signal, stdout, stderr, overflow, launchError };
  if (launchError) {
    const launchMessage = launchError instanceof Error ? launchError.message : String(launchError);
    throw scenarioFailure(definition, `failed to launch: ${launchMessage}`, normalized, true);
  }
  if ((signal === null && !Number.isInteger(code)) || (signal !== null && code !== null)) {
    throw scenarioFailure(definition, 'returned a malformed process outcome', normalized, true);
  }
  const combined = Buffer.byteLength(stdout) + Buffer.byteLength(stderr);
  if (overflow || combined > maxOutputBytes) {
    const boundedStdout = boundedUtf8Prefix(Buffer.from(stdout), maxOutputBytes);
    const stderrLimit = maxOutputBytes - Buffer.byteLength(boundedStdout);
    const bounded = {
      stdout: boundedStdout,
      stderr: stderrLimit > 0 ? boundedUtf8Prefix(Buffer.from(stderr), stderrLimit) : '',
    };
    throw scenarioFailure(definition, `exceeded the ${maxOutputBytes}-byte output limit`, bounded, true);
  }
  if (code !== 0 || signal !== null) {
    const termination = signal ? `signal ${signal}` : `status ${String(code)}`;
    throw scenarioFailure(definition, `exited with ${termination}`, normalized);
  }
  return normalized;
}

function readResultArtifact(spec) {
  let stat;
  try {
    stat = fs.lstatSync(spec.resultPath);
  } catch (error) {
    if (error?.code === 'ENOENT') throw new Error(`scenario ${spec.scenario.name} result artifact is missing`);
    throw error;
  }
  if (!stat.isFile() || stat.isSymbolicLink() || stat.nlink !== 1) {
    throw new Error(`scenario ${spec.scenario.name} result artifact must be one regular file`);
  }
  if ((stat.mode & 0o077) !== 0) {
    throw new Error(`scenario ${spec.scenario.name} result artifact permissions are not private`);
  }
  if (stat.size < 1 || stat.size > MAX_RESULT_BYTES) {
    throw new Error(`scenario ${spec.scenario.name} result artifact size is invalid`);
  }
  if (stat.mtimeMs < spec.launchedAtMs - RESULT_CLOCK_TOLERANCE_MS) {
    throw new Error(`scenario ${spec.scenario.name} result artifact is stale`);
  }
  let payload;
  try {
    payload = JSON.parse(fs.readFileSync(spec.resultPath, 'utf8'));
  } catch (error) {
    throw new Error(`scenario ${spec.scenario.name} result artifact is malformed JSON: ${error.message}`);
  }
  return validateScenarioResult(payload, spec.scenario);
}

function rejectSiblingResultArtifacts(spec) {
  const resultName = path.basename(spec.resultPath);
  const siblings = fs.readdirSync(path.dirname(spec.resultPath));
  if (siblings.some((entry) => entry.startsWith(`${resultName}.`))) {
    throw new Error(`scenario ${spec.scenario.name} left a duplicate or temporary result artifact`);
  }
}

function validateResultDirectory(resultDirectory, specs) {
  const expected = specs.map(({ resultPath }) => path.basename(resultPath)).sort();
  const actual = fs.readdirSync(resultDirectory).sort();
  if (actual.length !== expected.length || actual.some((entry, index) => entry !== expected[index])) {
    throw new Error('scenario result directory contains a missing, duplicate, or unexpected artifact');
  }
}

function assertScenarioSuccess(spec, rawOutcome) {
  const outcome = normalizeOutcome(spec.scenario, rawOutcome, spec.maxOutputBytes);
  let result;
  try {
    result = readResultArtifact(spec);
    rejectSiblingResultArtifacts(spec);
  } catch (error) {
    throw scenarioFailure(spec.scenario, `failed result validation: ${error.message}`, outcome, true);
  }
  const expectedStdout = expectedScenarioStdout(spec.scenario);
  if (outcome.stdout !== expectedStdout) {
    throw scenarioFailure(spec.scenario, 'stdout did not exactly match its declared labels', outcome);
  }
  if (outcome.stderr !== '') throw scenarioFailure(spec.scenario, 'wrote unexpected stderr', outcome);
  return {
    scenario: spec.scenario.name,
    count: result.count,
    labels: [...result.labels],
    stdout: outcome.stdout,
  };
}

export async function runScenarioScheduler({
  scenarios,
  jobs,
  bin: configuredBin,
  rootParent = os.tmpdir(),
  maxOutputBytes = MAX_CAPTURE_BYTES,
  launchScenario = launchScenarioProcess,
  onOwnedRoot,
  outputLabels = scenarios.flatMap(({ expectedLabels }) => expectedLabels),
}) {
  validateScenarioCatalog(scenarios);
  const orderedOutputLabels = validateOutputLabels(outputLabels, scenarios);
  if (!Number.isSafeInteger(jobs) || jobs < 1 || jobs > 4) throw new TypeError('jobs must be an integer in 1..4');
  if (!Number.isSafeInteger(maxOutputBytes) || maxOutputBytes < 1) {
    throw new TypeError('maxOutputBytes must be a positive integer');
  }
  if (typeof launchScenario !== 'function') throw new TypeError('launchScenario must be a function');
  const bin = requireTestBinary(configuredBin);
  const root = fs.mkdtempSync(path.join(path.resolve(rootParent), 'session-relay-selftest-'));
  fs.chmodSync(root, 0o700);
  onOwnedRoot?.(root);
  const homeDirectory = path.join(root, 'homes');
  const resultDirectory = path.join(root, 'results');
  fs.mkdirSync(homeDirectory, { mode: 0o700 });
  fs.mkdirSync(resultDirectory, { mode: 0o700 });
  const specs = scenarios.map((definition, index) => ({
    index,
    scenario: definition,
    bin,
    home: path.join(homeDirectory, `${String(index).padStart(2, '0')}-${definition.name}`),
    resultPath: path.join(resultDirectory, `${String(index).padStart(2, '0')}-${definition.name}.json`),
    launchedAtMs: 0,
    maxOutputBytes,
  }));
  const records = new Array(scenarios.length);
  const active = new Map();
  const failures = new Map();
  let nextIndex = 0;
  let schedulingStopped = false;
  let infrastructureTriggered = false;
  let terminationPromise;

  function recordFailure(index, error, infrastructure, prefix) {
    if (failures.has(index)) return failures.get(index);
    const spec = specs[index];
    const failure =
      scenarioFailureErrors.has(error) && error.scenario === spec.scenario.name
        ? error
        : scenarioFailure(
            spec.scenario,
            `${prefix}: ${error instanceof Error ? error.message : String(error)}`,
            error && typeof error === 'object' ? error : {},
            infrastructure,
          );
    failure.scenarioIndex = index;
    failure.infrastructure = infrastructure || failure.infrastructure === true;
    failures.set(index, failure);
    schedulingStopped = true;
    return failure;
  }

  function terminateActive() {
    terminationPromise ??= (async () => {
      const entries = [...active.entries()];
      const terminations = await Promise.allSettled(
        entries.map(([, { handle }]) => Promise.resolve().then(() => handle.terminate())),
      );
      for (const [position, termination] of terminations.entries()) {
        if (termination.status === 'rejected') {
          const [index] = entries[position];
          recordFailure(index, termination.reason, true, 'failed to terminate');
        }
      }
    })();
    return terminationPromise;
  }

  async function recordInfrastructureFailure(index, error, prefix) {
    recordFailure(index, error, true, prefix);
    infrastructureTriggered = true;
    await terminateActive();
  }

  async function worker() {
    while (!schedulingStopped) {
      const index = nextIndex;
      if (index >= specs.length) return;
      nextIndex += 1;
      const spec = specs[index];
      spec.launchedAtMs = Date.now();
      let hasOwnedArtifact;
      try {
        hasOwnedArtifact = fs.existsSync(spec.home) || fs.existsSync(spec.resultPath);
      } catch (error) {
        await recordInfrastructureFailure(index, error, 'failed to inspect owned artifacts');
        return;
      }
      if (hasOwnedArtifact) {
        await recordInfrastructureFailure(
          index,
          scenarioFailure(spec.scenario, 'received a pre-existing home or result artifact', {}, true),
          'failed',
        );
        return;
      }

      let launchedHandle;
      try {
        launchedHandle = launchScenario(spec);
      } catch (error) {
        await recordInfrastructureFailure(index, error, 'launcher failed');
        return;
      }

      let handle;
      try {
        if (!launchedHandle || typeof launchedHandle !== 'object' || typeof launchedHandle.terminate !== 'function') {
          throw new TypeError('launcher must return a terminable scenario handle');
        }
        const terminate = launchedHandle.terminate;
        handle = {
          completion: undefined,
          terminate: () => Reflect.apply(terminate, launchedHandle, []),
        };
        active.set(index, { handle, spec });
        handle.completion = launchedHandle.completion;
        if (!handle.completion || typeof handle.completion.then !== 'function') {
          throw new TypeError('launcher handle completion must be a promise');
        }
      } catch (error) {
        await recordInfrastructureFailure(index, error, 'launcher returned an invalid handle');
        active.delete(index);
        return;
      }

      try {
        let outcome;
        try {
          outcome = await handle.completion;
        } catch (error) {
          await recordInfrastructureFailure(index, error, 'completion promise rejected');
          return;
        }
        if (infrastructureTriggered) return;
        try {
          records[index] = assertScenarioSuccess(spec, outcome);
        } catch (error) {
          if (error?.infrastructure === true || !error?.scenario) {
            await recordInfrastructureFailure(index, error, 'infrastructure failure');
          } else {
            recordFailure(index, error, false, 'failed');
          }
          return;
        }
      } finally {
        active.delete(index);
      }
    }
  }

  try {
    await Promise.all(Array.from({ length: Math.min(jobs, scenarios.length) }, () => worker()));
    if (failures.size > 0) {
      throw scenarioSchedulerError([...failures.values()]);
    }
    if (records.some((record) => record === undefined))
      throw new Error('scenario scheduler completed with missing results');
    try {
      validateResultDirectory(resultDirectory, specs);
    } catch (error) {
      const failure = scenarioFailure(
        { name: 'result-directory' },
        `validation failed: ${error instanceof Error ? error.message : String(error)}`,
        {},
        true,
      );
      failure.scenarioIndex = specs.length;
      throw scenarioSchedulerError([failure]);
    }
    const scenarioLabels = records.flatMap(({ labels: recordLabels }) => recordLabels);
    if (new Set(scenarioLabels).size !== scenarioLabels.length) {
      throw new Error('scenario result union contains duplicate labels');
    }
    return {
      records,
      count: records.reduce((sum, record) => sum + record.count, 0),
      labels: orderedOutputLabels,
      stdout: orderedOutputLabels.map((label) => `  ok: ${label}\n`).join(''),
    };
  } finally {
    await terminateActive();
    fs.rmSync(root, { recursive: true, force: true });
  }
}

function validateProductionCatalog() {
  const expectedOrder = [
    'core',
    'discovery-hardening',
    'hooks-identity',
    'appserver',
    'gc',
    'spawn-wake-supervisor',
    'follow-doctor-mailbox',
  ];
  if (SCENARIOS.length !== expectedOrder.length) throw new Error('session-relay scenario catalog is incomplete');
  for (const [index, expected] of expectedOrder.entries()) {
    if (SCENARIOS[index].name !== expected) throw new Error('session-relay scenario catalog order changed');
  }
  if (SPAWN_WAKE_SUPERVISOR_LABELS.length !== 24 || FOLLOW_DOCTOR_MAILBOX_LABELS.length !== 6) {
    throw new Error('session-relay split scenario catalog must contain exactly 24 and 6 labels');
  }
  const labels = SCENARIOS.flatMap(({ expectedLabels }) => expectedLabels);
  if (labels.length !== 133 || new Set(labels).size !== 133) {
    throw new Error('session-relay scenario catalog must contain exactly 133 unique labels');
  }
  if (
    PRODUCTION_OUTPUT_LABELS.length !== 133 ||
    new Set(PRODUCTION_OUTPUT_LABELS).size !== 133 ||
    PRODUCTION_OUTPUT_LABELS.some((label) => !labels.includes(label))
  ) {
    throw new Error('session-relay production output must contain the exact 133-label scenario union');
  }
  const rendered = PRODUCTION_OUTPUT_LABELS.map((label) => `  ok: ${label}\n`).join('');
  if (createHash('sha256').update(rendered).digest('hex') !== PRE_SPLIT_STDOUT_SHA256) {
    throw new Error('session-relay production output changed from the immutable pre-split baseline');
  }
}

function writeFailure(error, stderr) {
  const failures = Array.isArray(error?.failures) && error.failures.length > 0 ? error.failures : [error];
  for (const failure of failures) {
    const scenarioName = failure?.scenario ? ` in ${failure.scenario}` : '';
    const category = failure?.infrastructure === true ? ' infrastructure failure' : ' failed';
    stderr.write(
      `session-relay self-test${category}${scenarioName}: ${failure instanceof Error ? failure.message : String(failure)}\n`,
    );
    if (failure?.stdout) {
      stderr.write(`--- retained stdout ---\n${failure.stdout}${failure.stdout.endsWith('\n') ? '' : '\n'}`);
    }
    if (failure?.stderr) {
      stderr.write(`--- retained stderr ---\n${failure.stderr}${failure.stderr.endsWith('\n') ? '' : '\n'}`);
    }
  }
}

export async function main({ env = process.env, stdout = process.stdout, stderr = process.stderr } = {}) {
  try {
    validateProductionCatalog();
    const jobs = parseScenarioJobs(env.SESSION_RELAY_TEST_JOBS);
    const result = await runScenarioScheduler({
      scenarios: SCENARIOS,
      jobs,
      bin: env.SESSION_RELAY_TEST_BIN,
      outputLabels: PRODUCTION_OUTPUT_LABELS,
    });
    if (result.count !== 133 || result.labels.length !== 133) {
      throw new Error('session-relay scenario aggregation did not produce exactly 133 checks');
    }
    stdout.write(result.stdout);
    const bin = fs.realpathSync(env.SESSION_RELAY_TEST_BIN);
    stdout.write(`\nPASS: session-relay self-test — 133 checks (binary: ${path.relative(PLUGIN, bin)})\n`);
    return 0;
  } catch (error) {
    writeFailure(error, stderr);
    return 1;
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exitCode = await main();
}
