#!/usr/bin/env node
// Real-runtime feasibility spike for relay worker lifecycle primitives.
//
// This harness is intentionally not part of the ordinary self-test. It runs
// authenticated Claude and Codex CLIs, compiles host-native syscall probes,
// and exercises delegated cgroup/namespace facilities. Run it only on the
// real host with the explicit gate documented in the active plan.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..', '..', '..');
const SCRIPT_REL = 'plugins/session-relay/test/feasibility-probe.mjs';
const SCHEMA_REL = 'plugins/session-relay/test/fixtures/lifecycle-capability-schema.json';
const PLUGIN = path.resolve(HERE, '..');
const SCHEMA_PATH = path.join(ROOT, SCHEMA_REL);
const PROTOCOL_DEADLINE_MS = 20_000;
const GENESIS = '0'.repeat(64);
const INLINE_STREAM_LIMIT = 64 * 1024;
const RUNTIMES = ['claude', 'codex'];
const ORIGINAL_ENV = { ...process.env };
const ORIGINAL_HOME = ORIGINAL_ENV.HOME || os.homedir();
const ORIGINAL_CLAUDE_CONFIG = ORIGINAL_ENV.CLAUDE_CONFIG_DIR || path.join(ORIGINAL_HOME, '.claude');
const ORIGINAL_CODEX_HOME = ORIGINAL_ENV.CODEX_HOME || path.join(ORIGINAL_HOME, '.codex');

if (
  process.env.RELAY_REAL_RUNTIME_TEST !== '1' ||
  process.argv.length !== 3 ||
  process.argv[2] !== '--verify-current'
) {
  console.error(
    'usage: RELAY_REAL_RUNTIME_TEST=1 node plugins/session-relay/test/feasibility-probe.mjs --verify-current',
  );
  process.exit(2);
}

const sha256 = (value) => crypto.createHash('sha256').update(value).digest('hex');
const isoNow = () => new Date().toISOString();
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const canonical = (value) => {
  if (value === null || typeof value !== 'object') return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  return `{${Object.keys(value)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${canonical(value[key])}`)
    .join(',')}}`;
};
const exactKeys = (value, expected, label) => {
  assert.equal(
    value !== null && typeof value === 'object' && !Array.isArray(value),
    true,
    `${label} must be an object`,
  );
  assert.deepEqual(Object.keys(value).sort(), [...expected].sort(), `${label} has unknown or missing fields`);
};
const safeId = (id) => id.replace(/[^a-zA-Z0-9._-]/g, '_');
const buffer = (value) => (Buffer.isBuffer(value) ? value : Buffer.from(value ?? '', 'utf8'));
const jsonBuffer = (value) => Buffer.from(`${canonical(value)}\n`, 'utf8');
const firstLine = (value) => buffer(value).toString('utf8').trim().split(/\r?\n/, 1)[0] || null;
const exists = (file) => {
  try {
    fs.accessSync(file);
    return true;
  } catch {
    return false;
  }
};
const mkdir0700 = (dir) => fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
const quoteShell = (value) => `'${String(value).replaceAll("'", "'\\''")}'`;

const artifactRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'relay-lifecycle-feasibility-'));
fs.chmodSync(artifactRoot, 0o700);
const dirs = Object.fromEntries(
  Object.entries({
    home: path.join(artifactRoot, 'home'),
    codexHome: path.join(artifactRoot, 'codex-home'),
    claudeConfig: path.join(artifactRoot, 'home', '.claude'),
    pluginConfig: path.join(artifactRoot, 'plugin-config'),
    relayStore: path.join(artifactRoot, 'relay-store'),
    cwd: path.join(artifactRoot, 'cwd'),
    sentinels: path.join(artifactRoot, 'sentinels'),
    raw: path.join(artifactRoot, 'raw'),
    helpers: path.join(artifactRoot, 'helpers'),
    appserverSchema: path.join(artifactRoot, 'appserver-schema'),
    temp: path.join(artifactRoot, 'tmp'),
  }).map(([key, value]) => {
    mkdir0700(value);
    return [key, value];
  }),
);

const platform = { os: process.platform, arch: process.arch, release: os.release() };
const schemaBytes = fs.readFileSync(SCHEMA_PATH);
const schema = JSON.parse(schemaBytes);
assert.equal(schema.$id, 'https://docks.dev/session-relay/lifecycle-capability-evidence-v1.schema.json');
assert.equal(schema.properties.schema_version.const, '1');

function isolatedEnv(extra = {}) {
  const env = {};
  for (const key of [
    'PATH',
    'SHELL',
    'LANG',
    'LC_ALL',
    'LC_CTYPE',
    'TERM',
    'HTTP_PROXY',
    'HTTPS_PROXY',
    'ALL_PROXY',
    'NO_PROXY',
    'http_proxy',
    'https_proxy',
    'all_proxy',
    'no_proxy',
    'SSL_CERT_FILE',
    'SSL_CERT_DIR',
    'NODE_EXTRA_CA_CERTS',
    'CODEX_CA_CERTIFICATE',
    'ANTHROPIC_BASE_URL',
  ]) {
    if (typeof ORIGINAL_ENV[key] === 'string') env[key] = ORIGINAL_ENV[key];
  }
  return {
    ...env,
    HOME: dirs.home,
    CODEX_HOME: dirs.codexHome,
    CLAUDE_CONFIG_DIR: dirs.claudeConfig,
    CLAUDE_PROJECT_DIR: dirs.cwd,
    AGENT_RELAY_HOME: dirs.relayStore,
    SESSION_RELAY_HOME: dirs.relayStore,
    XDG_CONFIG_HOME: path.join(dirs.home, '.config'),
    XDG_CACHE_HOME: path.join(dirs.home, '.cache'),
    XDG_DATA_HOME: path.join(dirs.home, '.local', 'share'),
    TMPDIR: dirs.temp,
    ...extra,
  };
}

async function runProcess(argv, { cwd = dirs.cwd, env = isolatedEnv(), timeoutMs = 120_000, input = null } = {}) {
  const startedAt = isoNow();
  const started = process.hrtime.bigint();
  return await new Promise((resolve) => {
    let child;
    try {
      child = spawn(argv[0], argv.slice(1), { cwd, env, stdio: ['pipe', 'pipe', 'pipe'] });
    } catch (error) {
      const finishedAt = isoNow();
      resolve({
        startedAt,
        finishedAt,
        durationMs: 0,
        status: null,
        signal: null,
        timedOut: false,
        stdout: Buffer.alloc(0),
        stderr: Buffer.from(String(error), 'utf8'),
      });
      return;
    }
    const stdout = [];
    const stderr = [];
    let timedOut = false;
    child.stdout.on('data', (chunk) => stdout.push(Buffer.from(chunk)));
    child.stderr.on('data', (chunk) => stderr.push(Buffer.from(chunk)));
    child.stdin.on('error', (error) => stderr.push(Buffer.from(`stdin error: ${error}\n`, 'utf8')));
    child.on('error', (error) => stderr.push(Buffer.from(`spawn error: ${error}\n`, 'utf8')));
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill('SIGKILL');
    }, timeoutMs);
    child.on('close', (status, signal) => {
      clearTimeout(timer);
      const durationMs = Number((process.hrtime.bigint() - started) / 1_000_000n);
      resolve({
        startedAt,
        finishedAt: isoNow(),
        durationMs,
        status,
        signal,
        timedOut,
        stdout: Buffer.concat(stdout),
        stderr: Buffer.concat(stderr),
      });
    });
    if (input !== null) child.stdin.end(input);
    else child.stdin.end();
  });
}

function syntheticResult(raw, { status = 0, stderr = '', durationMs = 0 } = {}) {
  const now = isoNow();
  return {
    startedAt: now,
    finishedAt: now,
    durationMs,
    status,
    signal: null,
    timedOut: false,
    stdout: buffer(raw),
    stderr: buffer(stderr),
  };
}

const parsers = new Map();
parsers.set('command-version', (record, stdout, stderr) => {
  const version = firstLine(stdout);
  const ok = record.exit_status === 0 && !record.timed_out && Boolean(version);
  return {
    availability: ok ? 'available' : 'unavailable',
    reason: ok ? 'runtime version recorded' : `version command failed: ${firstLine(stderr) || 'no version output'}`,
    facts: { version },
  };
});
parsers.set('command-available', (record, stdout, stderr) => {
  const ok = record.exit_status === 0 && !record.timed_out;
  return {
    availability: ok ? 'available' : 'unavailable',
    reason: ok ? 'command completed' : `command unavailable: ${firstLine(stderr) || `status=${record.exit_status}`}`,
    facts: { stdout_sha256: sha256(stdout), stderr_sha256: sha256(stderr) },
  };
});
parsers.set('auth-preflight', (record, stdout, stderr) => {
  let envelope;
  try {
    envelope = JSON.parse(stdout.toString('utf8'));
  } catch {
    envelope = {};
  }
  const runtimeOut = Buffer.from(envelope.runtime_stdout_base64 || '', 'base64')
    .toString('utf8')
    .trim();
  const ok = record.exit_status === 0 && !record.timed_out && runtimeOut === 'RELAY_AUTH_OK';
  return {
    availability: ok ? 'available' : 'unavailable',
    reason: ok
      ? 'isolated authenticated preflight returned exact marker'
      : envelope.unavailable_reason ||
        firstLine(Buffer.from(envelope.runtime_stderr_base64 || '', 'base64')) ||
        firstLine(stderr) ||
        'authentication preflight did not return the exact marker',
    facts: { marker_exact: runtimeOut === 'RELAY_AUTH_OK', auth_source: envelope.auth_source || 'unavailable' },
  };
});
parsers.set('hook-contract', (record, stdout) => {
  let envelope;
  try {
    envelope = JSON.parse(stdout.toString('utf8'));
  } catch {
    envelope = {};
  }
  const rule = JSON.parse(record.parser.rule);
  const runtimeOut = Buffer.from(envelope.runtime_stdout_base64 || '', 'base64').toString('utf8');
  const hookLog = Buffer.from(envelope.hook_log_base64 || '', 'base64').toString('utf8');
  const logRows = hookLog
    .split(/\r?\n/)
    .filter(Boolean)
    .flatMap((line) => {
      try {
        return [JSON.parse(line)];
      } catch {
        return [];
      }
    });
  const targetRows = logRows.filter((row) => row.event === rule.event && row.phase === 'start');
  const loadedExact = targetRows.some(
    (row) => row.hook_path === envelope.hook_path && row.hook_sha256 === envelope.hook_sha256,
  );
  const markerPresent = runtimeOut.includes(rule.marker);
  const expectedMarker = rule.mode !== 'block';
  const statusMatches = rule.mode === 'block' ? record.exit_status !== null : record.exit_status === 0;
  const ok = statusMatches && !record.timed_out && loadedExact && markerPresent === expectedMarker;
  return {
    availability: ok ? 'available' : 'unavailable',
    reason: ok
      ? `${rule.event} ${rule.mode} behavior matched raw runtime and hook evidence`
      : envelope.unavailable_reason || 'hook behavior, load identity, or runtime marker did not match',
    facts: {
      event: rule.event,
      mode: rule.mode,
      hook_loaded: loadedExact,
      hook_path: envelope.hook_path || null,
      hook_sha256: envelope.hook_sha256 || null,
      marker_present: markerPresent,
      expected_marker: expectedMarker,
      hook_invocations: targetRows.length,
    },
  };
});
parsers.set('attach-timing', (record, stdout) => {
  let envelope;
  try {
    envelope = JSON.parse(stdout.toString('utf8'));
  } catch {
    envelope = {};
  }
  const runtimeOut = Buffer.from(envelope.runtime_stdout_base64 || '', 'base64').toString('utf8');
  const hookLog = Buffer.from(envelope.hook_log_base64 || '', 'base64').toString('utf8');
  const marker = envelope.marker || '';
  const logRows = hookLog
    .split(/\r?\n/)
    .filter(Boolean)
    .flatMap((line) => {
      try {
        return [JSON.parse(line)];
      } catch {
        return [];
      }
    });
  const end = logRows.find((row) => row.event === 'SessionStart' && row.mode === 'delegate' && row.phase === 'end');
  const attachMs =
    end && Number.isFinite(envelope.launched_at_ms) ? end.observed_at_ms - envelope.launched_at_ms : null;
  const ok =
    record.exit_status === 0 &&
    !record.timed_out &&
    runtimeOut.includes(marker) &&
    Number.isInteger(attachMs) &&
    attachMs >= 0 &&
    envelope.lock_contention === true;
  return {
    availability: ok ? 'available' : 'unavailable',
    reason: ok
      ? 'cold SessionStart attach completed with real relay hook and sequential store-lock contention'
      : envelope.unavailable_reason || 'timing sample lacked marker, delegated hook end timestamp, or lock contention',
    facts: {
      sample: envelope.sample ?? null,
      measured_ms: attachMs,
      process_duration_ms: record.duration_ms,
      lock_contention: envelope.lock_contention === true,
      marker_present: runtimeOut.includes(marker),
    },
  };
});
parsers.set('json-facts', function parseJsonFacts(...args) {
  const stdout = args[1];
  let raw;
  try {
    raw = JSON.parse(stdout.toString('utf8'));
  } catch {
    raw = { availability: 'unavailable', reason: 'raw JSON parse failed', facts: {} };
  }
  const availability = ['unavailable', 'observation_only', 'not_applicable'].includes(raw.availability)
    ? raw.availability
    : 'unavailable';
  return {
    availability,
    reason: String(raw.reason || 'raw probe supplied no reason'),
    facts: raw.facts && typeof raw.facts === 'object' && !Array.isArray(raw.facts) ? raw.facts : {},
  };
});
parsers.set('cgroup-v2', (_record, stdout) => {
  let raw;
  try {
    raw = JSON.parse(stdout.toString('utf8'));
  } catch {
    raw = { facts: {} };
  }
  const facts = raw.facts || {};
  const ok =
    facts.linux === true &&
    facts.delegation_create === true &&
    facts.delegation_move === true &&
    facts.cgroup_kill_present === true &&
    facts.cgroup_freeze_present === true &&
    facts.freeze_observed === true &&
    facts.kill_observed === true &&
    facts.populated_zero === true &&
    Array.isArray(facts.events) &&
    facts.events.some((event) => event.op === 'move_child') &&
    facts.events.some((event) => event.op === 'freeze') &&
    facts.events.some((event) => event.op === 'kill');
  return {
    availability: ok ? 'available' : 'unavailable',
    reason: ok
      ? 'delegated cgroup v2 leaf passed create/move/freeze/kill/populated-zero probes'
      : String(raw.reason || 'one or more delegated cgroup v2 operations is unavailable'),
    facts,
  };
});
parsers.set('namespace-isolation', (record, stdout) => {
  let raw;
  try {
    raw = JSON.parse(stdout.toString('utf8'));
  } catch {
    raw = { facts: {} };
  }
  const facts = raw.facts || {};
  let helper = {};
  try {
    helper = JSON.parse(Buffer.from(facts.helper_stdout_base64 || '', 'base64').toString('utf8'));
  } catch {}
  const ok =
    record.exit_status === 0 &&
    helper.user_namespace === true &&
    helper.mount_pid_cgroup_namespaces === true &&
    helper.namespace_pid_is_one === true &&
    helper.inherited_proc_cgroup_mounts === helper.detached_proc_cgroup_mounts &&
    helper.fresh_proc === true &&
    helper.fresh_cgroup2 === true &&
    helper.cgroup2_read_only === true &&
    helper.proc_mount_count === 1 &&
    helper.cgroup2_mount_count === 1 &&
    helper.proc_pid_fd_authority_denied === true;
  return {
    availability: ok ? 'available' : 'unavailable',
    reason: ok
      ? 'unprivileged user plus mount/PID/cgroup namespaces, detach-all, fresh proc/cgroup views, and proc-pid-fd denial succeeded'
      : String(raw.reason || 'namespace isolation prerequisite failed'),
    facts,
  };
});
parsers.set('seccomp-description', (record, stdout) => {
  let raw;
  try {
    raw = JSON.parse(stdout.toString('utf8'));
  } catch {
    raw = { facts: {} };
  }
  const facts = raw.facts || {};
  let description = {};
  try {
    description = JSON.parse(Buffer.from(facts.raw_stdout_base64 || '', 'base64').toString('utf8'));
  } catch {}
  const expectedArch =
    process.arch === 'x64' ? 'AUDIT_ARCH_X86_64' : process.arch === 'arm64' ? 'AUDIT_ARCH_AARCH64' : 'unsupported';
  const ok =
    record.exit_status === 0 &&
    description.audit_arch === expectedArch &&
    (process.arch !== 'x64' || description.x32_rejection === true) &&
    Number.isInteger(description.instructions) &&
    description.instructions > 0;
  return {
    availability: ok ? 'available' : 'unavailable',
    reason: ok
      ? 'compiled filter reports the exact native audit architecture and x32 branch'
      : 'filter description lacks the required architecture/x32 structure',
    facts,
  };
});
parsers.set('x32-probe', (record) => {
  const ok = process.arch === 'x64' ? record.signal === 'SIGSYS' && !record.timed_out : true;
  return {
    availability: process.arch === 'x64' ? (ok ? 'available' : 'unavailable') : 'not_applicable',
    reason:
      process.arch === 'x64'
        ? ok
          ? 'x32 syscall-number probe was killed by the architecture gate'
          : 'x32 syscall-number probe was not killed with SIGSYS'
        : 'x32 ABI exists only on x86_64',
    facts: { arch: process.arch, status: record.exit_status, signal: record.signal, timed_out: record.timed_out },
  };
});
parsers.set('native-pidfd', (record, stdout, stderr) => {
  let raw = {};
  try {
    raw = JSON.parse(stdout.toString('utf8'));
  } catch {}
  const ok = record.exit_status === 0 && raw.pidfd_open === true && raw.proc_pid_fd === true;
  return {
    availability: ok ? 'available' : 'unavailable',
    reason: ok
      ? 'native pidfd_open pinned the current process and proc-pid-fd is present'
      : firstLine(stderr) || 'native pidfd/proc-pid-fd probe is unavailable',
    facts: {
      pidfd_open: raw.pidfd_open ?? false,
      pidfd_errno: raw.pidfd_errno ?? null,
      proc_pid_fd: raw.proc_pid_fd ?? false,
    },
  };
});
parsers.set('seccomp-filter', (record, stdout) => {
  const rule = JSON.parse(record.parser.rule);
  const actual = sha256(stdout);
  const ok = record.exit_status === 0 && !record.timed_out && stdout.length > 0 && actual === rule.expected_bpf_sha256;
  return {
    availability: ok ? 'available' : 'unavailable',
    reason: ok ? 'compiled helper emitted the exact candidate BPF bytes' : 'candidate BPF dump failed or changed',
    facts: {
      bpf_sha256: actual,
      bpf_bytes: stdout.length,
      policy_sha256: rule.policy_sha256,
      helper_sha256: rule.helper_sha256,
      audit_arch: rule.audit_arch,
      x32_rejection: rule.x32_rejection,
      clone3_errno: 'ENOSYS',
    },
  };
});
parsers.set('seccomp-raw', (record, stdout, stderr) => {
  let raw;
  try {
    raw = JSON.parse(stdout.toString('utf8'));
  } catch {
    raw = {};
  }
  const ok =
    record.exit_status === 0 &&
    !record.timed_out &&
    raw.clone3_rc === -1 &&
    raw.clone3_errno === 38 &&
    raw.clone3_child_created === false &&
    raw.legacy_namespace_clone_denied === true;
  return {
    availability: ok ? 'available' : 'unavailable',
    reason: ok
      ? 'raw clone3 returned ENOSYS without a child and legacy namespace clone was denied'
      : firstLine(stderr) || 'raw namespace denial evidence did not match',
    facts: {
      clone3_rc: raw.clone3_rc ?? null,
      clone3_errno: raw.clone3_errno ?? null,
      clone3_child_created: raw.clone3_child_created ?? null,
      legacy_namespace_clone_denied: raw.legacy_namespace_clone_denied ?? null,
      legacy_clone_errno: raw.legacy_clone_errno ?? null,
    },
  };
});
parsers.set('runtime-spawn', (record, stdout) => {
  let envelope;
  try {
    envelope = JSON.parse(stdout.toString('utf8'));
  } catch {
    envelope = {};
  }
  const runtimeOut = Buffer.from(envelope.runtime_stdout_base64 || '', 'base64').toString('utf8');
  const sentinel = Buffer.from(envelope.sentinel_base64 || '', 'base64').toString('utf8');
  const lines = sentinel.split(/\r?\n/).filter(Boolean);
  const ok =
    record.exit_status === 0 &&
    !record.timed_out &&
    runtimeOut.includes('RELAY_STRONG_SPAWN_OK') &&
    lines.length === 2 &&
    lines[0] === 'child-complete' &&
    lines[1] === 'wait-complete' &&
    envelope.child_reaped === true;
  return {
    availability: ok ? 'available' : 'unavailable',
    reason: ok
      ? 'real runtime created an ordinary tool child, observed descendant completion, and waited under the candidate filter'
      : envelope.unavailable_reason || 'no safe legacy-clone fallback or missing child/wait sentinel',
    facts: {
      child_complete: lines[0] === 'child-complete',
      wait_complete: lines[1] === 'wait-complete',
      ordered: lines.join(',') === 'child-complete,wait-complete',
      child_reaped: envelope.child_reaped === true,
      filter_sha256: envelope.filter_sha256 || null,
      policy_sha256: envelope.policy_sha256 || null,
      clone3_errno: 'ENOSYS',
    },
  };
});
parsers.set('appserver-protocol', (record, stdout) => {
  let raw;
  try {
    raw = JSON.parse(stdout.toString('utf8'));
  } catch {
    raw = {};
  }
  const schemaFiles = [];
  for (const entry of raw.schema_manifest || []) {
    const absolute = path.resolve(artifactRoot, entry.artifact || '');
    assert.equal(
      absolute.startsWith(`${path.resolve(dirs.appserverSchema)}${path.sep}`),
      true,
      'app-server schema artifact stays inside the isolated schema directory',
    );
    const bytes = fs.readFileSync(absolute);
    assert.equal(bytes.length, entry.bytes, 'app-server schema byte count');
    assert.equal(sha256(bytes), entry.sha256, 'app-server schema artifact hash');
    schemaFiles.push(absolute);
  }
  assert.equal(
    sha256(Buffer.from(canonical(raw.schema_manifest || []))),
    raw.schema_bundle_sha256,
    'app-server schema bundle hash',
  );
  const classified = classifyProtocol(schemaFiles);
  const contract = classified.contract;
  const graceful = new Set(contract.graceful_stop);
  if (
    (raw.responses || []).some(
      (response) =>
        graceful.has(response.method) &&
        response.reason === 'graceful_exit' &&
        response.method_succeeded &&
        response.exited_before_signal &&
        response.reaped,
    ) &&
    contract.shutdown_ack.length > 0
  )
    contract.reap.push('supervisor_child_reaped_after_graceful_shutdown_ack');
  assert.deepEqual(raw.methods || [], classified.methods, 'app-server method inventory is source-derived');
  assert.deepEqual(raw.contract || {}, contract, 'app-server capability contract is source-derived');
  const complete = [
    'mutation_barrier',
    'graceful_stop',
    'mutation_reject',
    'durable_flush',
    'accepted_watermark',
    'flushed_watermark',
    'shutdown_ack',
    'storage_sync',
    'reap',
  ].every((key) => Array.isArray(contract[key]) && contract[key].length > 0);
  return {
    availability: complete ? 'available' : 'unavailable',
    reason: complete
      ? 'schema and raw responses expose the complete stop/reject/flush/watermark/reap contract'
      : raw.reason || 'protocol_tree=unavailable: no complete durable graceful-flush contract',
    facts: {
      protocol_tree: complete ? 'available' : 'unavailable',
      shared_protocol: 'observation_only',
      deadline_ms: PROTOCOL_DEADLINE_MS,
      measured_ms: raw.measured_ms ?? record.duration_ms,
      schema_bundle_sha256: raw.schema_bundle_sha256 || null,
      methods: raw.methods || [],
      contract,
      responses: raw.responses || [],
      process_reaped: raw.process_reaped === true,
    },
  };
});

const records = [];
function streamDescriptor(id, streamName, bytes) {
  const data = buffer(bytes);
  const artifact = path.posix.join('raw', `${safeId(id)}.${streamName}`);
  const absolute = path.join(artifactRoot, artifact);
  fs.writeFileSync(absolute, data, { flag: 'wx', mode: 0o400 });
  return {
    artifact,
    sha256: sha256(data),
    bytes: data.length,
    base64: data.length <= INLINE_STREAM_LIMIT ? data.toString('base64') : null,
  };
}

function addRecord({ id, category, runtime, runtimeVersion = null, argv, result, parserId, parserRule }) {
  assert.equal(
    records.some((record) => record.id === id),
    false,
    `duplicate record id ${id}`,
  );
  const record = {
    id,
    category,
    runtime,
    runtime_version: runtimeVersion,
    platform,
    argv,
    started_at: result.startedAt,
    finished_at: result.finishedAt,
    duration_ms: Math.max(0, Math.trunc(result.durationMs)),
    exit_status: result.status,
    signal: result.signal,
    timed_out: Boolean(result.timedOut),
    stdout: streamDescriptor(id, 'stdout', result.stdout),
    stderr: streamDescriptor(id, 'stderr', result.stderr),
    parser: { id: parserId, version: '1', rule: parserRule },
    derived: null,
    previous_hash: records.at(-1)?.record_hash || GENESIS,
    record_hash: null,
  };
  const parser = parsers.get(parserId);
  assert.ok(parser, `unknown parser ${parserId}`);
  record.derived = parser(record, buffer(result.stdout), buffer(result.stderr));
  const withoutHashes = { ...record };
  delete withoutHashes.previous_hash;
  delete withoutHashes.record_hash;
  record.record_hash = sha256(`${record.previous_hash}\n${canonical(withoutHashes)}`);
  records.push(record);
  return record;
}

function addUnavailable({ id, category, runtime, runtimeVersion = null, argv, reason, facts = {} }) {
  return addRecord({
    id,
    category,
    runtime,
    runtimeVersion,
    argv,
    result: syntheticResult(jsonBuffer({ availability: 'unavailable', reason, facts })),
    parserId: 'json-facts',
    parserRule: 'Use raw availability/reason/facts; unavailability is a valid fail-closed observation.',
  });
}

function readRecordStream(record, name) {
  const descriptor = record[name];
  const bytes = fs.readFileSync(path.join(artifactRoot, descriptor.artifact));
  assert.equal(bytes.length, descriptor.bytes, `${record.id} ${name} byte count`);
  assert.equal(sha256(bytes), descriptor.sha256, `${record.id} ${name} hash`);
  if (descriptor.base64 !== null)
    assert.equal(Buffer.from(descriptor.base64, 'base64').equals(bytes), true, `${record.id} ${name} base64`);
  return bytes;
}

const HOOK_SOURCE = String.raw`#!/usr/bin/env node
import crypto from 'node:crypto';
import fs from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const ownPath = fileURLToPath(import.meta.url);
const ownSha = crypto.createHash('sha256').update(fs.readFileSync(ownPath)).digest('hex');
const input = fs.readFileSync(0);
const observedAtMs = Date.now();
let event = 'unknown';
try { event = JSON.parse(input.toString('utf8')).hook_event_name || 'unknown'; } catch {}
const target = process.env.RELAY_PROBE_TARGET_EVENT || event;
const mode = event === target ? (process.env.RELAY_PROBE_HOOK_MODE || 'allow') : 'allow';
const log = process.env.RELAY_PROBE_HOOK_LOG;
const row = { phase: 'start', event, mode, observed_at_ms: observedAtMs, hook_path: ownPath, hook_sha256: ownSha, input_base64: input.toString('base64') };
if (log) fs.appendFileSync(log, JSON.stringify(row) + '\n');
const logEnd = () => { if (log) fs.appendFileSync(log, JSON.stringify({ phase: 'end', event, mode, observed_at_ms: Date.now(), hook_path: ownPath, hook_sha256: ownSha }) + '\n'); };
if (mode === 'timeout') {
  await new Promise((resolve) => setTimeout(resolve, 2500));
  logEnd();
  process.stdout.write('{}\n');
} else if (mode === 'block') {
  logEnd();
  if (event === 'UserPromptSubmit') process.stdout.write(JSON.stringify({ decision: 'block', reason: 'RELAY_PROBE_BLOCK' }) + '\n');
  else process.stdout.write(JSON.stringify({ continue: false, stopReason: 'RELAY_PROBE_STOP' }) + '\n');
} else if (mode === 'delegate') {
  const relay = process.env.RELAY_PROBE_RELAY_BIN;
  const args = ['hook'];
  if (process.env.RELAY_PROBE_RUNTIME === 'codex') args.push('codex');
  if (event === 'UserPromptSubmit') args.push('--event', 'prompt');
  const child = spawnSync(relay, args, { input, env: process.env });
  logEnd();
  if (child.stdout) process.stdout.write(child.stdout);
  if (child.stderr) process.stderr.write(child.stderr);
  process.exit(child.status === null ? 1 : child.status);
} else {
  logEnd();
  process.stdout.write('{}\n');
}
`;

const NAMESPACE_SOURCE = String.raw`#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static int write_text(const char *path, const char *text) {
  int fd = open(path, O_WRONLY | O_CLOEXEC);
  if (fd < 0) return -1;
  size_t len = strlen(text);
  ssize_t n = write(fd, text, len);
  int saved = n == (ssize_t)len ? 0 : (errno == 0 ? EIO : errno);
  close(fd);
  errno = saved;
  return n == (ssize_t)len ? 0 : -1;
}
static int mount_counts(int *proc_count, int *cgroup_count) {
  FILE *f = fopen("/proc/self/mountinfo", "r");
  if (!f) return -1;
  char *line = NULL;
  size_t cap = 0;
  *proc_count = 0;
  *cgroup_count = 0;
  while (getline(&line, &cap, f) >= 0) {
    if (strstr(line, " - proc ")) (*proc_count)++;
    if (strstr(line, " - cgroup2 ")) (*cgroup_count)++;
  }
  free(line);
  fclose(f);
  return 0;
}
static int fail_reason(const char *stage, int error_number, const char *reason,
                       int user_namespace, int other_namespaces) {
  printf("{\"user_namespace\":%s,\"mount_pid_cgroup_namespaces\":%s,\"failure_stage\":\"%s\",\"failure_errno\":%d,\"failure_reason\":\"%s\"}\n",
         user_namespace ? "true" : "false",
         other_namespaces ? "true" : "false",
         stage,
         error_number,
         reason);
  return 2;
}
static int fail_errno(const char *stage, int user_namespace, int other_namespaces) {
  int saved = errno;
  return fail_reason(stage, saved, strerror(saved), user_namespace, other_namespaces);
}
static int make_dir(const char *path, mode_t mode) {
  if (mkdir(path, mode) == 0 || errno == EEXIST) return 0;
  return -1;
}
int main(int argc, char **argv) {
  (void)argc;
  (void)argv;
  uid_t uid = getuid();
  gid_t gid = getgid();
  char map[128];
  if (unshare(CLONE_NEWUSER) < 0) return fail_errno("unshare_user", 0, 0);
  if (write_text("/proc/self/setgroups", "deny\n") < 0 && errno != ENOENT) return fail_errno("setgroups_deny", 1, 0);
  snprintf(map, sizeof(map), "0 %u 1\n", (unsigned)uid);
  if (write_text("/proc/self/uid_map", map) < 0) return fail_errno("uid_map", 1, 0);
  snprintf(map, sizeof(map), "0 %u 1\n", (unsigned)gid);
  if (write_text("/proc/self/gid_map", map) < 0) return fail_errno("gid_map", 1, 0);
  if (unshare(CLONE_NEWNS) < 0) return fail_errno("unshare_mount", 1, 0);
  if (unshare(CLONE_NEWPID) < 0) return fail_errno("unshare_pid", 1, 0);
  if (unshare(CLONE_NEWCGROUP) < 0) return fail_errno("unshare_cgroup", 1, 0);

  pid_t child = fork();
  if (child < 0) return fail_errno("fork_pid_namespace_init", 1, 1);
  if (child > 0) {
    int status = 0;
    if (waitpid(child, &status, 0) < 0) return fail_errno("wait_pid_namespace_init", 1, 1);
    if (WIFEXITED(status)) return WEXITSTATUS(status);
    return fail_reason("pid_namespace_init_signal", 0, "namespace init terminated by signal", 1, 1);
  }

  if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) < 0) return fail_errno("make_mounts_private", 1, 1);
  int inherited_proc = -1;
  int inherited_cgroup = -1;
  if (mount_counts(&inherited_proc, &inherited_cgroup) < 0) return fail_errno("count_inherited_mounts", 1, 1);

  const char *tmp = getenv("TMPDIR");
  if (!tmp || !*tmp) return fail_reason("isolated_tmpdir", 0, "TMPDIR is unavailable", 1, 1);
  char new_root[4096];
  int root_len = snprintf(new_root, sizeof(new_root), "%s/namespace-root-%ld", tmp, (long)getpid());
  if (root_len < 0 || (size_t)root_len >= sizeof(new_root)) return fail_reason("isolated_root_path", ENAMETOOLONG, "isolated root path is too long", 1, 1);
  if (make_dir(new_root, 0700) < 0) return fail_errno("create_isolated_root", 1, 1);
  if (mount("tmpfs", new_root, "tmpfs", MS_NOSUID | MS_NODEV, "mode=0755") < 0) return fail_errno("mount_isolated_root", 1, 1);

  char old_root[4096];
  char proc_path[4096];
  char sys_path[4096];
  char sys_fs_path[4096];
  char cgroup_path[4096];
  if (snprintf(old_root, sizeof(old_root), "%s/oldroot", new_root) >= (int)sizeof(old_root) ||
      snprintf(proc_path, sizeof(proc_path), "%s/proc", new_root) >= (int)sizeof(proc_path) ||
      snprintf(sys_path, sizeof(sys_path), "%s/sys", new_root) >= (int)sizeof(sys_path) ||
      snprintf(sys_fs_path, sizeof(sys_fs_path), "%s/sys/fs", new_root) >= (int)sizeof(sys_fs_path) ||
      snprintf(cgroup_path, sizeof(cgroup_path), "%s/sys/fs/cgroup", new_root) >= (int)sizeof(cgroup_path)) {
    return fail_reason("isolated_root_children", ENAMETOOLONG, "isolated root child path is too long", 1, 1);
  }
  if (make_dir(old_root, 0700) < 0 || make_dir(proc_path, 0555) < 0 ||
      make_dir(sys_path, 0555) < 0 || make_dir(sys_fs_path, 0555) < 0 ||
      make_dir(cgroup_path, 0555) < 0) return fail_errno("create_isolated_root_children", 1, 1);
  if (mount("proc", proc_path, "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) < 0) return fail_errno("mount_fresh_proc", 1, 1);
  if (mount("none", cgroup_path, "cgroup2", MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) < 0) return fail_errno("mount_fresh_cgroup2_read_only", 1, 1);
  if (chdir(new_root) < 0) return fail_errno("chdir_isolated_root", 1, 1);
  if (syscall(SYS_pivot_root, ".", "oldroot") < 0) return fail_errno("pivot_isolated_root", 1, 1);
  if (chdir("/") < 0) return fail_errno("chdir_new_root", 1, 1);
  if (umount2("/oldroot", MNT_DETACH) < 0) return fail_errno("detach_inherited_root", 1, 1);
  if (rmdir("/oldroot") < 0) return fail_errno("remove_old_root", 1, 1);

  int inherited_mounts = inherited_proc + inherited_cgroup;
  int detached = inherited_mounts;
  int proc_count = -1;
  int cgroup_count = -1;
  if (mount_counts(&proc_count, &cgroup_count) < 0) return fail_errno("count_fresh_mounts", 1, 1);
  if (proc_count != 1 || cgroup_count != 1) return fail_reason("fresh_mount_count", 0, "fresh proc/cgroup2 mount count is not one", 1, 1);
  struct statvfs cgroup_vfs;
  if (statvfs("/sys/fs/cgroup", &cgroup_vfs) < 0) return fail_errno("stat_fresh_cgroup2", 1, 1);
  if ((cgroup_vfs.f_flag & ST_RDONLY) == 0) return fail_reason("fresh_cgroup2_read_only", 0, "fresh cgroup2 view is writable", 1, 1);
  const char *host_pid = getenv("RELAY_PROBE_HOST_PID");
  char host_fd[128];
  snprintf(host_fd, sizeof(host_fd), "/proc/%s/fd", host_pid ? host_pid : "0");
  struct stat st;
  int host_proc_hidden = stat(host_fd, &st) < 0 && errno == ENOENT;
  if (!host_proc_hidden) return fail_reason("hide_host_proc_pid_fd", errno, "host proc-pid-fd remains visible", 1, 1);
  if (getpid() != 1) return fail_reason("namespace_pid_one", 0, "namespace init PID is not one", 1, 1);
  printf("{\"user_namespace\":true,\"mount_pid_cgroup_namespaces\":true,\"namespace_pid_is_one\":%s,\"inherited_proc_cgroup_mounts\":%d,\"detached_proc_cgroup_mounts\":%d,\"fresh_proc\":%s,\"fresh_cgroup2\":%s,\"cgroup2_read_only\":%s,\"proc_mount_count\":%d,\"cgroup2_mount_count\":%d,\"proc_pid_fd_authority_denied\":%s}\n",
    "true", inherited_mounts, detached, "true", "true", "true", proc_count, cgroup_count, "true");
  return 0;
}
`;

const SECCOMP_POLICY = {
  version: 'CooperativeWorkerV1-candidate-1',
  default_action: 'allow',
  architecture_mismatch: 'kill_process',
  x86_x32: 'kill_process',
  clone3: { action: 'errno', errno: 'ENOSYS' },
  legacy_clone_namespace_flags: {
    action: 'errno',
    errno: 'EPERM',
    flags: [
      'CLONE_NEWCGROUP',
      'CLONE_NEWIPC',
      'CLONE_NEWNET',
      'CLONE_NEWNS',
      'CLONE_NEWPID',
      'CLONE_NEWUSER',
      'CLONE_NEWUTS',
    ],
  },
  errno_eperm_syscalls: [
    'mount',
    'umount2',
    'fsopen',
    'fsmount',
    'open_tree',
    'move_mount',
    'mount_setattr',
    'fsconfig',
    'fspick',
    'pivot_root',
    'chroot',
    'setns',
    'unshare',
    'ptrace',
    'process_vm_writev',
    'pidfd_getfd',
  ],
};

const SECCOMP_SOURCE = String.raw`#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <linux/sched.h>
#include <stddef.h>
#include <stdint.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#if defined(__x86_64__)
#define EXPECTED_ARCH AUDIT_ARCH_X86_64
#define EXPECTED_ARCH_NAME "AUDIT_ARCH_X86_64"
#define X32_BIT 0x40000000U
#elif defined(__aarch64__)
#define EXPECTED_ARCH AUDIT_ARCH_AARCH64
#define EXPECTED_ARCH_NAME "AUDIT_ARCH_AARCH64"
#else
#error unsupported architecture
#endif

static struct sock_filter code[256]; static unsigned short used = 0;
static void stmt(unsigned short c, unsigned int k) { code[used++] = (struct sock_filter){ c, 0, 0, k }; }
static void jump(unsigned short c, unsigned int k, unsigned char jt, unsigned char jf) { code[used++] = (struct sock_filter){ c, jt, jf, k }; }
static void deny_syscall(int nr) { jump(BPF_JMP | BPF_JEQ | BPF_K, (unsigned)nr, 0, 1); stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (EPERM & SECCOMP_RET_DATA)); }
static void build(void) {
  used = 0;
  stmt(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch));
  jump(BPF_JMP | BPF_JEQ | BPF_K, EXPECTED_ARCH, 1, 0); stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS);
  stmt(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr));
#if defined(__x86_64__)
  stmt(BPF_ALU | BPF_AND | BPF_K, X32_BIT); jump(BPF_JMP | BPF_JEQ | BPF_K, 0, 1, 0); stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS);
  stmt(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr));
#endif
#ifdef __NR_clone
  jump(BPF_JMP | BPF_JEQ | BPF_K, __NR_clone, 0, 5);
  stmt(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0]));
  stmt(BPF_ALU | BPF_AND | BPF_K, CLONE_NEWCGROUP | CLONE_NEWIPC | CLONE_NEWNET | CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWUSER | CLONE_NEWUTS);
  jump(BPF_JMP | BPF_JEQ | BPF_K, 0, 1, 0); stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (EPERM & SECCOMP_RET_DATA)); stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
  stmt(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr));
#endif
#ifdef __NR_clone3
  jump(BPF_JMP | BPF_JEQ | BPF_K, __NR_clone3, 0, 1); stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (ENOSYS & SECCOMP_RET_DATA));
#endif
#ifdef __NR_mount
  deny_syscall(__NR_mount);
#endif
#ifdef __NR_umount2
  deny_syscall(__NR_umount2);
#endif
#ifdef __NR_fsopen
  deny_syscall(__NR_fsopen);
#endif
#ifdef __NR_fsmount
  deny_syscall(__NR_fsmount);
#endif
#ifdef __NR_open_tree
  deny_syscall(__NR_open_tree);
#endif
#ifdef __NR_move_mount
  deny_syscall(__NR_move_mount);
#endif
#ifdef __NR_mount_setattr
  deny_syscall(__NR_mount_setattr);
#endif
#ifdef __NR_fsconfig
  deny_syscall(__NR_fsconfig);
#endif
#ifdef __NR_fspick
  deny_syscall(__NR_fspick);
#endif
#ifdef __NR_pivot_root
  deny_syscall(__NR_pivot_root);
#endif
#ifdef __NR_chroot
  deny_syscall(__NR_chroot);
#endif
#ifdef __NR_setns
  deny_syscall(__NR_setns);
#endif
#ifdef __NR_unshare
  deny_syscall(__NR_unshare);
#endif
#ifdef __NR_ptrace
  deny_syscall(__NR_ptrace);
#endif
#ifdef __NR_process_vm_writev
  deny_syscall(__NR_process_vm_writev);
#endif
#ifdef __NR_pidfd_getfd
  deny_syscall(__NR_pidfd_getfd);
#endif
  stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
}
static int install(void) {
  build();
  struct sock_fprog prog = { used, code };
  if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0) return -1;
  return prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog);
}
static int raw_probe(void) {
  if (install() < 0) { perror("seccomp install"); return 2; }
  long clone3_rc = -1; int clone3_errno = ENOSYS;
#ifdef __NR_clone3
  struct clone_args args; memset(&args, 0, sizeof(args)); args.exit_signal = SIGCHLD; errno = 0; clone3_rc = syscall(__NR_clone3, &args, sizeof(args)); clone3_errno = errno;
  if (clone3_rc == 0) _exit(97);
#endif
  long clone_rc = -1; int clone_errno = ENOSYS;
#ifdef __NR_clone
  errno = 0; clone_rc = syscall(__NR_clone, (unsigned long)(CLONE_NEWUSER | SIGCHLD), 0, 0, 0, 0); clone_errno = errno;
  if (clone_rc == 0) _exit(98);
  if (clone_rc > 0) {
    (void)waitpid((pid_t)clone_rc, NULL, 0);
  }
#endif
  printf("{\"clone3_rc\":%ld,\"clone3_errno\":%d,\"clone3_child_created\":%s,\"legacy_namespace_clone_denied\":%s,\"legacy_clone_errno\":%d}\n", clone3_rc, clone3_errno, clone3_rc > 0 ? "true" : "false", clone_rc == -1 && clone_errno == EPERM ? "true" : "false", clone_errno);
  return (clone3_rc == -1 && clone3_errno == ENOSYS && clone_rc == -1 && clone_errno == EPERM) ? 0 : 3;
}
static int x32_probe(void) {
#if defined(__x86_64__)
  if (install() < 0) { perror("seccomp install"); return 7; }
  (void)syscall((long)(X32_BIT | __NR_getpid));
  return 8;
#else
  return 0;
#endif
}
static int native_probe(void) {
  int pidfd = -1; int saved = ENOSYS;
#ifdef __NR_pidfd_open
  errno = 0; pidfd = (int)syscall(__NR_pidfd_open, getpid(), 0); saved = errno;
#endif
  printf("{\"pidfd_open\":%s,\"pidfd_errno\":%d,\"proc_pid_fd\":%s}\n", pidfd >= 0 ? "true" : "false", saved, access("/proc/self/fd", R_OK | X_OK) == 0 ? "true" : "false");
  if (pidfd >= 0) close(pidfd);
  return pidfd >= 0 ? 0 : 9;
}
int main(int argc, char **argv) {
  build();
  if (argc == 2 && !strcmp(argv[1], "--dump-bpf")) return fwrite(code, sizeof(code[0]), used, stdout) == used ? 0 : 4;
  if (argc == 2 && !strcmp(argv[1], "--describe")) { printf("{\"audit_arch\":\"%s\",\"x32_rejection\":%s,\"instructions\":%u}\n", EXPECTED_ARCH_NAME,
#if defined(__x86_64__)
  "true",
#else
  "\"not_applicable\"",
#endif
  used); return 0; }
  if (argc == 2 && !strcmp(argv[1], "--raw-probe")) return raw_probe();
  if (argc == 2 && !strcmp(argv[1], "--probe-x32")) return x32_probe();
  if (argc == 2 && !strcmp(argv[1], "--native-probe")) return native_probe();
  if (argc >= 3 && !strcmp(argv[1], "--exec")) { if (install() < 0) { perror("seccomp install"); return 5; } execvp(argv[2], &argv[2]); perror("seccomp exec"); return 6; }
  fputs("usage: seccomp-probe --dump-bpf|--describe|--raw-probe|--probe-x32|--native-probe|--exec command...\n", stderr); return 64;
}
`;

const helperPaths = {
  namespaceSource: path.join(dirs.helpers, 'namespace-probe.c'),
  namespace: path.join(dirs.helpers, 'namespace-probe'),
  seccompSource: path.join(dirs.helpers, 'seccomp-probe.c'),
  seccomp: path.join(dirs.helpers, 'seccomp-probe'),
};
const hookPath = path.join(dirs.pluginConfig, 'lifecycle-hook.mjs');
const claudeSettingsPath = path.join(dirs.claudeConfig, 'settings.json');
const codexHooksPath = path.join(dirs.codexHome, 'hooks.json');
const codexConfigPath = path.join(dirs.codexHome, 'config.toml');
const relayBin = path.join(PLUGIN, 'bin', 'relay');

fs.writeFileSync(hookPath, HOOK_SOURCE, { flag: 'wx', mode: 0o500 });
fs.writeFileSync(helperPaths.namespaceSource, NAMESPACE_SOURCE, { flag: 'wx', mode: 0o400 });
fs.writeFileSync(helperPaths.seccompSource, SECCOMP_SOURCE, { flag: 'wx', mode: 0o400 });

const hookCommand = `${quoteShell(process.execPath)} ${quoteShell(hookPath)}`;
const claudeSettings = {
  hooks: {
    SessionStart: [{ hooks: [{ type: 'command', command: process.execPath, args: [hookPath], timeout: 1 }] }],
    UserPromptSubmit: [{ hooks: [{ type: 'command', command: process.execPath, args: [hookPath], timeout: 1 }] }],
  },
};
const codexHooks = {
  hooks: {
    SessionStart: [{ hooks: [{ type: 'command', command: hookCommand, timeout: 1 }] }],
    UserPromptSubmit: [{ hooks: [{ type: 'command', command: hookCommand, timeout: 1 }] }],
  },
};
fs.writeFileSync(claudeSettingsPath, `${JSON.stringify(claudeSettings, null, 2)}\n`, { flag: 'wx', mode: 0o400 });
fs.writeFileSync(codexHooksPath, `${JSON.stringify(codexHooks, null, 2)}\n`, { flag: 'wx', mode: 0o400 });
fs.writeFileSync(codexConfigPath, '[features]\nhooks = true\n\n[analytics]\nenabled = false\n', {
  flag: 'wx',
  mode: 0o400,
});

async function compileHelper(id, category, source, output) {
  if (process.platform !== 'linux') {
    addUnavailable({
      id,
      category,
      runtime: 'host',
      argv: ['cc', source, '-o', output],
      reason: 'Linux-only helper is not applicable on this host',
      facts: { source_sha256: sha256(fs.readFileSync(source)) },
    });
    return false;
  }
  const argv = ['cc', '-O2', '-Wall', '-Wextra', '-Werror', source, '-o', output];
  const result = await runProcess(argv, { timeoutMs: 30_000 });
  const record = addRecord({
    id,
    category,
    runtime: 'host',
    argv,
    result,
    parserId: 'command-available',
    parserRule: 'Exit zero proves the host compiler accepted the committed helper source.',
  });
  if (record.derived.availability === 'available') fs.chmodSync(output, 0o500);
  return record.derived.availability === 'available';
}

const namespaceHelperAvailable = await compileHelper(
  'helper.namespace.compile',
  'namespace_isolation',
  helperPaths.namespaceSource,
  helperPaths.namespace,
);
const seccompHelperAvailable = await compileHelper(
  'helper.seccomp.compile',
  'seccomp_filter',
  helperPaths.seccompSource,
  helperPaths.seccomp,
);

const auth = {};
const forwardedCredentials = [];
function prepareAuth(runtime) {
  const envNames =
    runtime === 'claude'
      ? ['ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN', 'CLAUDE_CODE_OAUTH_TOKEN']
      : ['CODEX_API_KEY', 'CODEX_ACCESS_TOKEN'];
  const envName = envNames.find((name) => typeof ORIGINAL_ENV[name] === 'string' && ORIGINAL_ENV[name].length > 0);
  if (envName) return { kind: 'allowlisted_secret_environment', envName, env: { [envName]: ORIGINAL_ENV[envName] } };

  const source =
    runtime === 'claude'
      ? path.join(ORIGINAL_CLAUDE_CONFIG, '.credentials.json')
      : path.join(ORIGINAL_CODEX_HOME, 'auth.json');
  if (exists(source)) {
    const target =
      runtime === 'claude' ? path.join(dirs.claudeConfig, '.credentials.json') : path.join(dirs.codexHome, 'auth.json');
    try {
      fs.copyFileSync(source, target, fs.constants.COPYFILE_EXCL);
      fs.chmodSync(target, 0o600);
      forwardedCredentials.push(target);
      return { kind: 'isolated_0600_copy', env: {} };
    } catch (error) {
      try {
        fs.rmSync(target, { force: true });
      } catch {}
      return {
        kind: 'unavailable',
        env: {},
        reason: `credential copy into isolated runtime home failed: ${error.code || error.message || error}`,
      };
    }
  }
  if (process.platform === 'darwin') return { kind: 'os_keychain', env: {} };
  return {
    kind: 'unavailable',
    env: {},
    reason: 'no allowlisted secret environment variable or supported credential artifact is present',
  };
}
for (const runtime of RUNTIMES) auth[runtime] = prepareAuth(runtime);

function runtimeCommand(runtime, prompt, { tools = false, seccomp = false } = {}) {
  const logical =
    runtime === 'claude'
      ? [
          'claude',
          '-p',
          prompt,
          '--output-format',
          'text',
          '--permission-mode',
          'auto',
          '--no-session-persistence',
          '--setting-sources',
          'user',
          ...(tools ? ['--tools', 'Bash', '--allowedTools', 'Bash'] : ['--tools', '']),
        ]
      : [
          'codex',
          'exec',
          '--sandbox',
          tools ? 'workspace-write' : 'read-only',
          '--skip-git-repo-check',
          '--ephemeral',
          '--dangerously-bypass-hook-trust',
          '--color',
          'never',
          '-C',
          dirs.cwd,
          prompt,
        ];

  let actual = [...logical];
  if (seccomp) actual = [helperPaths.seccomp, '--exec', ...actual];
  const credential = auth[runtime];
  const extraEnv = { ...credential.env };
  return { logical, actual, env: isolatedEnv(extraEnv), authSource: credential.kind };
}

async function runRuntime(runtime, prompt, options = {}) {
  if (auth[runtime].kind === 'unavailable')
    return { unavailableReason: auth[runtime].reason, command: runtimeCommand(runtime, prompt, options) };
  const command = runtimeCommand(runtime, prompt, options);
  const result = await runProcess(command.actual, { env: command.env, timeoutMs: options.timeoutMs || 180_000 });
  return { result, command };
}

const versions = {};
for (const runtime of RUNTIMES) {
  const argv = [runtime, '--version'];
  const result = await runProcess(argv, { env: isolatedEnv(), timeoutMs: 10_000 });
  const record = addRecord({
    id: `runtime.${runtime}.version`,
    category: 'runtime_version',
    runtime,
    argv,
    result,
    parserId: 'command-version',
    parserRule: 'Exit zero and the first non-empty stdout line are the installed runtime version.',
  });
  versions[runtime] = record.derived.facts.version;
}

const authAvailable = {};
for (const runtime of RUNTIMES) {
  const run = await runRuntime(runtime, 'Print exactly RELAY_AUTH_OK');
  let result;
  if (run.unavailableReason) {
    result = syntheticResult(
      jsonBuffer({
        auth_source: 'unavailable',
        unavailable_reason: run.unavailableReason,
        runtime_stdout_base64: '',
        runtime_stderr_base64: '',
      }),
    );
  } else {
    result = {
      ...run.result,
      stdout: jsonBuffer({
        auth_source: run.command.authSource,
        runtime_stdout_base64: run.result.stdout.toString('base64'),
        runtime_stderr_base64: run.result.stderr.toString('base64'),
      }),
      stderr: Buffer.alloc(0),
    };
  }
  const record = addRecord({
    id: `runtime.${runtime}.auth`,
    category: 'auth_preflight',
    runtime,
    runtimeVersion: versions[runtime],
    argv: run.command.logical,
    result,
    parserId: 'auth-preflight',
    parserRule: 'The isolated CLI must exit zero and stdout, after trimming, must equal RELAY_AUTH_OK.',
  });
  authAvailable[runtime] = record.derived.availability === 'available';
}

async function hookContractRow(runtime, event, mode) {
  const marker = `RELAY_HOOK_${runtime.toUpperCase()}_${event.toUpperCase()}_${mode.toUpperCase()}_OK`;
  const log = path.join(dirs.raw, `hook-${runtime}-${event}-${mode}.jsonl`);
  const prompt = `Print exactly ${marker}`;
  let command = runtimeCommand(runtime, prompt);
  let runtimeResult;
  let unavailableReason = null;
  if (!authAvailable[runtime]) {
    unavailableReason = 'isolated runtime authentication preflight failed';
    runtimeResult = syntheticResult('', { status: 0 });
  } else {
    command = runtimeCommand(runtime, prompt);
    const env = {
      ...command.env,
      RELAY_PROBE_RUNTIME: runtime,
      RELAY_PROBE_TARGET_EVENT: event,
      RELAY_PROBE_HOOK_MODE: mode,
      RELAY_PROBE_HOOK_LOG: log,
    };
    const launchedAtMs = Date.now();
    env.RELAY_PROBE_LAUNCHED_AT_MS = String(launchedAtMs);
    runtimeResult = await runProcess(command.actual, { env, timeoutMs: 180_000 });
    runtimeResult.launchedAtMs = launchedAtMs;
  }
  const hookLog = exists(log) ? fs.readFileSync(log) : Buffer.alloc(0);
  const envelope = {
    runtime_stdout_base64: runtimeResult.stdout.toString('base64'),
    runtime_stderr_base64: runtimeResult.stderr.toString('base64'),
    hook_log_base64: hookLog.toString('base64'),
    hook_path: hookPath,
    hook_sha256: sha256(fs.readFileSync(hookPath)),
    config_path: runtime === 'claude' ? claudeSettingsPath : codexHooksPath,
    config_sha256: sha256(fs.readFileSync(runtime === 'claude' ? claudeSettingsPath : codexHooksPath)),
    unavailable_reason: unavailableReason,
  };
  const result = { ...runtimeResult, stdout: jsonBuffer(envelope), stderr: Buffer.alloc(0) };
  return addRecord({
    id: `hook.${runtime}.${event}.${mode}`,
    category: 'hook_contract',
    runtime,
    runtimeVersion: versions[runtime],
    argv: command.logical,
    result,
    parserId: 'hook-contract',
    parserRule: canonical({ event, mode, marker }),
  });
}

for (const runtime of RUNTIMES) {
  for (const event of ['SessionStart', 'UserPromptSubmit']) {
    for (const mode of ['allow', 'block', 'timeout']) await hookContractRow(runtime, event, mode);
  }
}

async function startStoreContention(sample) {
  const ready = path.join(dirs.sentinels, `lock-${sample}.ready`);
  const lock = path.join(dirs.relayStore, '.lock');
  const script = `printf locked > ${quoteShell(ready)}; sleep 0.25`;
  let child;
  try {
    child = spawn('flock', ['-x', lock, 'sh', '-c', script], { cwd: dirs.cwd, env: isolatedEnv(), stdio: 'ignore' });
  } catch {
    return { available: false, reason: 'flock command could not start', wait: Promise.resolve() };
  }
  const deadline = Date.now() + 3000;
  while (!exists(ready) && Date.now() < deadline && child.exitCode === null) await sleep(10);
  const available = exists(ready);
  return {
    available,
    reason: available ? null : 'flock did not acquire the isolated store lock',
    wait: new Promise((resolve) => child.once('close', resolve)),
  };
}

async function timingRow(runtime, sample) {
  const marker = `RELAY_ATTACH_TIMING_${runtime.toUpperCase()}_${sample}_OK`;
  const log = path.join(dirs.raw, `timing-${runtime}-${sample}.jsonl`);
  const command = runtimeCommand(runtime, `Print exactly ${marker}`);
  let runtimeResult;
  let unavailableReason = null;
  let lockContention = false;
  if (!authAvailable[runtime] || !exists(relayBin)) {
    unavailableReason = !authAvailable[runtime]
      ? 'isolated runtime authentication preflight failed'
      : 'committed relay launcher is unavailable';
    runtimeResult = syntheticResult('');
  } else {
    const contention = await startStoreContention(`${runtime}-${sample}`);
    lockContention = contention.available;
    if (!contention.available) unavailableReason = contention.reason;
    const env = {
      ...command.env,
      RELAY_PROBE_RUNTIME: runtime,
      RELAY_PROBE_TARGET_EVENT: 'SessionStart',
      RELAY_PROBE_HOOK_MODE: 'delegate',
      RELAY_PROBE_HOOK_LOG: log,
      RELAY_PROBE_RELAY_BIN: relayBin,
    };
    const launchedAtMs = Date.now();
    env.RELAY_PROBE_LAUNCHED_AT_MS = String(launchedAtMs);
    runtimeResult = await runProcess(command.actual, { env, timeoutMs: 180_000 });
    runtimeResult.launchedAtMs = launchedAtMs;
    await contention.wait;
  }
  const envelope = {
    sample,
    marker,
    launched_at_ms: runtimeResult.launchedAtMs ?? null,
    lock_contention: lockContention,
    unavailable_reason: unavailableReason,
    runtime_stdout_base64: runtimeResult.stdout.toString('base64'),
    runtime_stderr_base64: runtimeResult.stderr.toString('base64'),
    hook_log_base64: (exists(log) ? fs.readFileSync(log) : Buffer.alloc(0)).toString('base64'),
  };
  return addRecord({
    id: `timing.${runtime}.${sample}`,
    category: 'attach_timing',
    runtime,
    runtimeVersion: versions[runtime],
    argv: command.logical,
    result: { ...runtimeResult, stdout: jsonBuffer(envelope), stderr: Buffer.alloc(0) },
    parserId: 'attach-timing',
    parserRule:
      'Require the unique runtime marker, delegated real relay hook evidence, and acquired isolated store-lock contention.',
  });
}

for (const runtime of RUNTIMES) for (let sample = 1; sample <= 10; sample += 1) await timingRow(runtime, sample);

function filesRecursively(dir) {
  if (!exists(dir)) return [];
  const out = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...filesRecursively(full));
    else if (entry.isFile()) out.push(full);
  }
  return out.sort();
}

function collectSchemaStrings(value, out = []) {
  if (typeof value === 'string') out.push(value);
  else if (Array.isArray(value)) for (const child of value) collectSchemaStrings(child, out);
  else if (value && typeof value === 'object') {
    for (const [key, child] of Object.entries(value)) {
      out.push(key);
      collectSchemaStrings(child, out);
    }
  }
  return out;
}

function classifyProtocol(schemaFiles) {
  const strings = [];
  const methods = new Set();
  for (const file of schemaFiles) {
    let value;
    try {
      value = JSON.parse(fs.readFileSync(file, 'utf8'));
    } catch {
      continue;
    }
    for (const text of collectSchemaStrings(value)) {
      strings.push(text);
      if (/^[a-zA-Z][a-zA-Z0-9_.-]*\/[a-zA-Z0-9_./-]+$/.test(text)) methods.add(text);
    }
  }
  const uniqueStrings = [...new Set(strings)];
  const matching = (regex, source = uniqueStrings) => source.filter((text) => regex.test(text)).sort();
  const methodList = [...methods].sort();
  const contract = {
    mutation_barrier: matching(/barrier|quiesc|lineage.*fence|mutation.*fence/i, methodList),
    graceful_stop: matching(
      /graceful.*(?:stop|shutdown)|(?:server|app).*shutdown|shutdown.*(?:server|app)/i,
      methodList,
    ),
    mutation_reject: matching(/reject.*mutat|mutat.*reject|quiesc|barrier/i, methodList),
    durable_flush: matching(/flush|storage.*sync|sync.*storage/i, methodList),
    accepted_watermark: matching(/accepted.*watermark|watermark.*accepted/i),
    flushed_watermark: matching(/flushed.*watermark|watermark.*flushed/i),
    shutdown_ack: matching(/shutdown.*ack|ack.*shutdown|graceful.*ack/i),
    storage_sync: matching(/storage.*sync|sync.*evidence|durable.*sync/i),
    reap: [],
  };
  return { methods: methodList, contract };
}

async function appserverRequest(method, deadlineAt, expectGracefulExit = false) {
  const remaining = Math.max(1, deadlineAt - Date.now());
  const started = process.hrtime.bigint();
  return await new Promise((resolve) => {
    const argv = ['codex', 'app-server', '--stdio'];
    const child = spawn(argv[0], argv.slice(1), { cwd: dirs.cwd, env: isolatedEnv(), stdio: ['pipe', 'pipe', 'pipe'] });
    const lines = [];
    const stderr = [];
    let pending = '';
    let resolved = false;
    let finishReason = null;
    let methodSucceeded = false;
    let signaledByHarness = false;
    let gracefulTimer = null;
    const finish = (reason, signalProcess = true) => {
      if (resolved) return;
      resolved = true;
      finishReason = reason;
      clearTimeout(timer);
      if (gracefulTimer) clearTimeout(gracefulTimer);
      if (signalProcess && child.exitCode === null) {
        signaledByHarness = true;
        child.kill('SIGTERM');
        setTimeout(() => {
          if (child.exitCode === null) child.kill('SIGKILL');
        }, 500).unref();
      }
    };
    const timer = setTimeout(() => finish('deadline'), remaining);
    child.stderr.on('data', (chunk) => stderr.push(Buffer.from(chunk)));
    child.stdin.on('error', (error) => stderr.push(Buffer.from(`stdin error: ${error}\n`, 'utf8')));
    child.on('error', (error) => {
      stderr.push(Buffer.from(String(error)));
      clearTimeout(timer);
      finish('spawn_error');
    });
    child.on('close', (status, signal) => {
      if (!resolved) finish(expectGracefulExit && methodSucceeded ? 'graceful_exit' : 'unexpected_exit', false);
      resolve({
        method,
        reason: finishReason,
        status,
        signal,
        method_succeeded: methodSucceeded,
        exited_before_signal: !signaledByHarness,
        transcript: lines,
        stderr_base64: Buffer.concat(stderr).toString('base64'),
        measured_ms: Number((process.hrtime.bigint() - started) / 1_000_000n),
        reaped: true,
      });
    });
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', (chunk) => {
      pending += chunk;
      while (true) {
        const newline = pending.indexOf('\n');
        if (newline < 0) break;
        const line = pending.slice(0, newline);
        pending = pending.slice(newline + 1);
        if (!line.trim()) continue;
        let message;
        try {
          message = JSON.parse(line);
        } catch {
          message = { raw_base64: Buffer.from(line).toString('base64') };
        }
        lines.push({ direction: 'server_to_client', message });
        if (message.id === 0) {
          const initialized = { method: 'initialized', params: {} };
          child.stdin.write(`${JSON.stringify(initialized)}\n`);
          lines.push({ direction: 'client_to_server', message: initialized });
          if (method) {
            const request = { id: 1, method, params: {} };
            child.stdin.write(`${JSON.stringify(request)}\n`);
            lines.push({ direction: 'client_to_server', message: request });
          } else {
            finish('initialize_only');
          }
        } else if (message.id === 1) {
          methodSucceeded = message.error === undefined;
          if (expectGracefulExit && methodSucceeded) {
            const grace = Math.max(1, Math.min(1000, deadlineAt - Date.now()));
            gracefulTimer = setTimeout(() => finish('method_response_no_exit'), grace);
          } else finish('method_response');
        }
      }
    });
    const initialize = {
      id: 0,
      method: 'initialize',
      params: { clientInfo: { name: 'relay-feasibility-probe', title: 'relay feasibility probe', version: '1' } },
    };
    child.stdin.write(`${JSON.stringify(initialize)}\n`);
    lines.push({ direction: 'client_to_server', message: initialize });
  });
}

const appserverStarted = process.hrtime.bigint();
const schemaArgv = ['codex', 'app-server', 'generate-json-schema', '--experimental', '--out', dirs.appserverSchema];
const schemaResult = await runProcess(schemaArgv, { env: isolatedEnv(), timeoutMs: PROTOCOL_DEADLINE_MS });
const schemaFiles = filesRecursively(dirs.appserverSchema);
const schemaManifest = schemaFiles.map((file) => {
  const relative = path.relative(dirs.appserverSchema, file);
  return {
    path: relative,
    artifact: path.posix.join('appserver-schema', relative.split(path.sep).join('/')),
    bytes: fs.statSync(file).size,
    sha256: sha256(fs.readFileSync(file)),
  };
});
const schemaBundleSha = sha256(Buffer.from(canonical(schemaManifest)));
const classified = classifyProtocol(schemaFiles);
const gracefulMethods = new Set(classified.contract.graceful_stop);
const probeMethods = [...new Set(Object.values(classified.contract).flat())]
  .filter((method) => classified.methods.includes(method))
  .slice(0, 12);
const protocolDeadlineAt =
  Date.now() + Math.max(0, PROTOCOL_DEADLINE_MS - Number((process.hrtime.bigint() - appserverStarted) / 1_000_000n));
const responses = [];
if (schemaResult.status === 0 && Date.now() < protocolDeadlineAt) {
  responses.push(await appserverRequest(null, protocolDeadlineAt));
  for (const method of probeMethods) {
    if (Date.now() >= protocolDeadlineAt) break;
    responses.push(await appserverRequest(method, protocolDeadlineAt, gracefulMethods.has(method)));
  }
}
const gracefulReaped = responses.some(
  (response) =>
    gracefulMethods.has(response.method) &&
    response.reason === 'graceful_exit' &&
    response.method_succeeded &&
    response.exited_before_signal &&
    response.reaped,
);
if (gracefulReaped && classified.contract.shutdown_ack.length > 0)
  classified.contract.reap.push('supervisor_child_reaped_after_graceful_shutdown_ack');
const appserverMeasuredMs = Number((process.hrtime.bigint() - appserverStarted) / 1_000_000n);
const appserverRaw = {
  schema_argv: schemaArgv,
  schema_exit_status: schemaResult.status,
  schema_timed_out: schemaResult.timedOut,
  schema_stdout_base64: schemaResult.stdout.toString('base64'),
  schema_stderr_base64: schemaResult.stderr.toString('base64'),
  schema_manifest: schemaManifest,
  schema_bundle_sha256: schemaBundleSha,
  methods: classified.methods,
  contract: classified.contract,
  responses,
  measured_ms: appserverMeasuredMs,
  process_reaped: responses.every((response) => response.reaped),
  reason:
    schemaResult.status === 0
      ? 'protocol_tree=unavailable unless every graceful stop/reject/flush/watermark/ack/sync/reap element is present in raw schema and responses'
      : 'protocol_tree=unavailable: app-server schema export failed',
};
addRecord({
  id: 'appserver.protocol.contract',
  category: 'appserver_protocol',
  runtime: 'app-server',
  runtimeVersion: versions.codex,
  argv: schemaArgv,
  result: {
    ...schemaResult,
    durationMs: appserverMeasuredMs,
    stdout: jsonBuffer(appserverRaw),
    stderr: Buffer.alloc(0),
  },
  parserId: 'appserver-protocol',
  parserRule:
    'A protocol-tree tier exists only when raw schema/responses contain every graceful mutation barrier, stop, reject, durable flush, accepted/flushed watermark, shutdown ack, storage sync, and supervisor reap element within one 20s deadline.',
});

function decodeMountPath(value) {
  return value.replace(/\\040/g, ' ').replace(/\\011/g, '\t').replace(/\\012/g, '\n').replace(/\\134/g, '\\');
}

async function probeCgroupV2() {
  const facts = {
    linux: process.platform === 'linux',
    cgroup2_mount: null,
    current_cgroup: null,
    cgroup_kill_present: false,
    cgroup_freeze_present: false,
    delegation_create: false,
    delegation_move: false,
    freeze_observed: false,
    kill_observed: false,
    populated_zero: false,
    events: [],
  };
  if (process.platform !== 'linux') return { availability: 'unavailable', reason: 'cgroup v2 is Linux-only', facts };
  let leaf = null;
  let child = null;
  try {
    const mountinfo = fs.readFileSync('/proc/self/mountinfo', 'utf8');
    const cgroupLine = mountinfo.split('\n').find((line) => line.includes(' - cgroup2 '));
    const membership = fs
      .readFileSync('/proc/self/cgroup', 'utf8')
      .split('\n')
      .find((line) => line.startsWith('0::'));
    facts.events.push({
      op: 'read_mountinfo',
      sha256: sha256(Buffer.from(mountinfo)),
      cgroup2_row: cgroupLine || null,
    });
    facts.events.push({ op: 'read_self_cgroup', raw: membership || null });
    if (!cgroupLine || !membership)
      return { availability: 'unavailable', reason: 'unified cgroup v2 mount or membership row is absent', facts };
    const mountPoint = decodeMountPath(cgroupLine.split(' ')[4]);
    const relative = membership.slice(3);
    const current = path.join(mountPoint, relative.replace(/^\/+/, ''));
    facts.cgroup2_mount = mountPoint;
    facts.current_cgroup = relative;
    leaf = path.join(current, `relay-feasibility-${process.pid}-${crypto.randomBytes(4).toString('hex')}`);
    fs.mkdirSync(leaf, { mode: 0o700 });
    facts.delegation_create = true;
    facts.cgroup_kill_present = exists(path.join(leaf, 'cgroup.kill'));
    facts.cgroup_freeze_present = exists(path.join(leaf, 'cgroup.freeze'));
    facts.events.push({ op: 'create_leaf', ok: true, files: fs.readdirSync(leaf).sort() });
    child = spawn('/bin/sh', ['-c', 'trap "exit 0" TERM; while :; do sleep 1; done'], { stdio: 'ignore' });
    await sleep(50);
    fs.writeFileSync(path.join(leaf, 'cgroup.procs'), `${child.pid}\n`);
    const childMembership = fs.readFileSync(`/proc/${child.pid}/cgroup`, 'utf8');
    facts.delegation_move = childMembership.includes(path.basename(leaf));
    facts.events.push({ op: 'move_child', pid: child.pid, membership: childMembership.trim() });
    if (facts.cgroup_freeze_present) {
      fs.writeFileSync(path.join(leaf, 'cgroup.freeze'), '1\n');
      const freezeDeadline = Date.now() + 2000;
      while (Date.now() < freezeDeadline) {
        const events = fs.readFileSync(path.join(leaf, 'cgroup.events'), 'utf8');
        if (/^frozen 1$/m.test(events)) {
          facts.freeze_observed = true;
          facts.events.push({ op: 'freeze', events: events.trim() });
          break;
        }
        await sleep(10);
      }
      fs.writeFileSync(path.join(leaf, 'cgroup.freeze'), '0\n');
    }
    if (facts.cgroup_kill_present) {
      fs.writeFileSync(path.join(leaf, 'cgroup.kill'), '1\n');
      await new Promise((resolve) => child.once('close', resolve));
      facts.kill_observed = true;
      child = null;
      const events = fs.readFileSync(path.join(leaf, 'cgroup.events'), 'utf8');
      facts.populated_zero = /^populated 0$/m.test(events);
      facts.events.push({ op: 'kill', events: events.trim() });
    }
    const ok =
      facts.delegation_create &&
      facts.delegation_move &&
      facts.cgroup_kill_present &&
      facts.cgroup_freeze_present &&
      facts.freeze_observed &&
      facts.kill_observed &&
      facts.populated_zero;
    return {
      availability: ok ? 'available' : 'unavailable',
      reason: ok
        ? 'delegated cgroup v2 leaf passed create/move/freeze/kill/populated-zero probes'
        : 'one or more delegated cgroup v2 operations is unavailable',
      facts,
    };
  } catch (error) {
    facts.events.push({ op: 'error', code: error.code || null, message: String(error.message || error) });
    return {
      availability: 'unavailable',
      reason: `cgroup v2 delegation probe failed: ${error.code || error.message || error}`,
      facts,
    };
  } finally {
    if (child && child.exitCode === null) {
      try {
        child.kill('SIGKILL');
      } catch {}
      await new Promise((resolve) => child.once('close', resolve));
    }
    if (leaf) {
      try {
        fs.rmdirSync(leaf);
      } catch {}
    }
  }
}

const cgroupResult = await probeCgroupV2();
addRecord({
  id: 'host.cgroup-v2',
  category: 'cgroup_v2',
  runtime: 'host',
  argv: ['node:fs', '/proc/self/mountinfo', '/proc/self/cgroup', '<delegated-leaf>'],
  result: syntheticResult(jsonBuffer(cgroupResult)),
  parserId: 'cgroup-v2',
  parserRule:
    'Recompute availability from raw leaf creation, child migration, frozen=1, cgroup.kill, populated=0, and matching operation events; absent delegation is valid unavailability.',
});

if (!namespaceHelperAvailable) {
  addUnavailable({
    id: 'host.namespace-isolation',
    category: 'namespace_isolation',
    runtime: 'host',
    argv: [helperPaths.namespace],
    reason: 'namespace helper did not compile',
    facts: {},
  });
} else {
  const rawResult = await runProcess([helperPaths.namespace], {
    env: isolatedEnv({ RELAY_PROBE_HOST_PID: String(process.pid) }),
    timeoutMs: 15_000,
  });
  let facts = {};
  try {
    facts = JSON.parse(rawResult.stdout.toString('utf8'));
  } catch {
    facts = { helper_stdout_base64: rawResult.stdout.toString('base64') };
  }
  const ok =
    rawResult.status === 0 &&
    facts.user_namespace === true &&
    facts.mount_pid_cgroup_namespaces === true &&
    facts.namespace_pid_is_one === true &&
    facts.inherited_proc_cgroup_mounts === facts.detached_proc_cgroup_mounts &&
    facts.fresh_proc === true &&
    facts.fresh_cgroup2 === true &&
    facts.cgroup2_read_only === true &&
    facts.proc_mount_count === 1 &&
    facts.cgroup2_mount_count === 1 &&
    facts.proc_pid_fd_authority_denied === true;
  const failure = facts.failure_stage
    ? `namespace isolation failed at ${facts.failure_stage}: errno=${facts.failure_errno} (${facts.failure_reason || 'reason unavailable'})`
    : firstLine(rawResult.stderr) || 'namespace isolation prerequisite failed without structured helper output';
  const wrapped = {
    availability: ok ? 'available' : 'unavailable',
    reason: ok
      ? 'unprivileged user plus mount/PID/cgroup namespaces, detach-all, fresh proc, and proc-pid-fd denial succeeded'
      : failure,
    facts: {
      ...facts,
      helper_stdout_base64: rawResult.stdout.toString('base64'),
      helper_stderr_base64: rawResult.stderr.toString('base64'),
    },
  };
  addRecord({
    id: 'host.namespace-isolation',
    category: 'namespace_isolation',
    runtime: 'host',
    argv: [helperPaths.namespace],
    result: { ...rawResult, stdout: jsonBuffer(wrapped), stderr: Buffer.alloc(0) },
    parserId: 'namespace-isolation',
    parserRule:
      'Reparse the base64 raw helper stdout and require user/mount/PID/cgroup namespaces, PID 1, detach-all inherited proc/cgroup mounts, fresh proc, one read-only cgroup view, and hidden host proc-pid-fd.',
  });
}

const workflowBytes = fs.readFileSync(path.join(ROOT, '.github', 'workflows', 'build-binaries.yml'));
const workflowText = workflowBytes.toString('utf8');
const nativeFacts = {
  workflow_sha256: sha256(workflowBytes),
  runner_labels: [...workflowText.matchAll(/- runner:\s*([^\s]+)/g)].map((match) => match[1]),
  target_rows: [...workflowText.matchAll(/targets:\s*"([^"]+)"/g)].map((match) => match[1].split(/\s+/)),
  linux_proc_start_field_22: null,
  proc_pid_fd_present: exists('/proc/self/fd'),
  darwin_process_generation: process.platform === 'darwin' ? 'observation_only' : 'not_applicable',
};
if (process.platform === 'linux') {
  const stat = fs.readFileSync('/proc/self/stat', 'utf8');
  const close = stat.lastIndexOf(')');
  const tail = stat.slice(close + 2).split(' ');
  nativeFacts.linux_proc_start_field_22 = tail[19] || null;
  nativeFacts.proc_self_stat_sha256 = sha256(Buffer.from(stat));
}
addRecord({
  id: 'host.native-process-and-runners',
  category: 'native_process',
  runtime: 'host',
  argv: ['node:fs', '/proc/self/stat', '.github/workflows/build-binaries.yml'],
  result: syntheticResult(
    jsonBuffer({
      availability: 'observation_only',
      reason:
        'native process generation fields and current workflow runner labels are observations, never stable signal handles',
      facts: nativeFacts,
    }),
  ),
  parserId: 'json-facts',
  parserRule:
    'Record the native host process observation and exact workflow runner/target rows without promoting either to signal authority.',
});

if (seccompHelperAvailable) {
  const nativePidfdResult = await runProcess([helperPaths.seccomp, '--native-probe'], { timeoutMs: 10_000 });
  addRecord({
    id: 'host.native-pidfd',
    category: 'native_process',
    runtime: 'host',
    argv: [helperPaths.seccomp, '--native-probe'],
    result: nativePidfdResult,
    parserId: 'native-pidfd',
    parserRule:
      'Derive Linux stable-handle availability from an actual pidfd_open on the current process plus proc-pid-fd presence.',
  });
} else {
  addUnavailable({
    id: 'host.native-pidfd',
    category: 'native_process',
    runtime: 'host',
    argv: [helperPaths.seccomp, '--native-probe'],
    reason: process.platform === 'linux' ? 'native pidfd helper did not compile' : 'pidfd is Linux-only',
    facts: {},
  });
}

const policySha = sha256(Buffer.from(canonical(SECCOMP_POLICY)));
const seccompHelperSha = sha256(Buffer.from(SECCOMP_SOURCE));
let filterSha = null;
let filterAvailable = false;
let rawSeccompAvailable = false;
let x32Available = process.arch !== 'x64';

if (!seccompHelperAvailable) {
  addUnavailable({
    id: 'host.seccomp-filter',
    category: 'seccomp_filter',
    runtime: 'host',
    argv: [helperPaths.seccomp, '--dump-bpf'],
    reason: 'candidate seccomp helper did not compile',
    facts: { policy_sha256: policySha, helper_sha256: seccompHelperSha },
  });
  addUnavailable({
    id: 'host.seccomp-raw',
    category: 'seccomp_raw_probe',
    runtime: 'host',
    argv: [helperPaths.seccomp, '--raw-probe'],
    reason: 'candidate seccomp helper did not compile',
    facts: {},
  });
  addUnavailable({
    id: 'host.seccomp-x32',
    category: 'seccomp_raw_probe',
    runtime: 'host',
    argv: [helperPaths.seccomp, '--probe-x32'],
    reason: 'candidate seccomp helper did not compile',
    facts: {},
  });
} else {
  const describeResult = await runProcess([helperPaths.seccomp, '--describe'], { timeoutMs: 10_000 });
  let description = {};
  try {
    description = JSON.parse(describeResult.stdout.toString('utf8'));
  } catch {}
  addRecord({
    id: 'host.seccomp-description',
    category: 'seccomp_filter',
    runtime: 'host',
    argv: [helperPaths.seccomp, '--describe'],
    result: {
      ...describeResult,
      stdout: jsonBuffer({
        availability: describeResult.status === 0 ? 'available' : 'unavailable',
        reason:
          describeResult.status === 0
            ? 'compiled filter reports its native audit architecture and x32 branch'
            : 'filter description failed',
        facts: {
          ...description,
          policy_sha256: policySha,
          helper_sha256: seccompHelperSha,
          raw_stdout_base64: describeResult.stdout.toString('base64'),
        },
      }),
      stderr: Buffer.alloc(0),
    },
    parserId: 'seccomp-description',
    parserRule:
      'Reparse the raw helper stdout and require the current native AUDIT_ARCH, x32 kill branch on x86_64, and a nonempty BPF program.',
  });

  const dumpResult = await runProcess([helperPaths.seccomp, '--dump-bpf'], { timeoutMs: 10_000 });
  filterSha = sha256(dumpResult.stdout);
  const auditArch =
    process.arch === 'x64' ? 'AUDIT_ARCH_X86_64' : process.arch === 'arm64' ? 'AUDIT_ARCH_AARCH64' : 'unsupported';
  const filterRecord = addRecord({
    id: 'host.seccomp-filter',
    category: 'seccomp_filter',
    runtime: 'host',
    argv: [helperPaths.seccomp, '--dump-bpf'],
    result: dumpResult,
    parserId: 'seccomp-filter',
    parserRule: canonical({
      expected_bpf_sha256: filterSha,
      policy_sha256: policySha,
      helper_sha256: seccompHelperSha,
      audit_arch: auditArch,
      x32_rejection: process.arch === 'x64' ? 'kill_process' : 'not_applicable',
    }),
  });
  filterAvailable =
    filterRecord.derived.availability === 'available' &&
    description.audit_arch === auditArch &&
    (process.arch !== 'x64' || description.x32_rejection === true);

  const rawResult = await runProcess([helperPaths.seccomp, '--raw-probe'], { timeoutMs: 10_000 });
  const rawRecord = addRecord({
    id: 'host.seccomp-raw',
    category: 'seccomp_raw_probe',
    runtime: 'host',
    argv: [helperPaths.seccomp, '--raw-probe'],
    result: rawResult,
    parserId: 'seccomp-raw',
    parserRule:
      'Require raw clone3=-1/ENOSYS with no child and raw legacy clone namespace flags denied with EPERM under the exact candidate filter.',
  });
  rawSeccompAvailable = rawRecord.derived.availability === 'available';

  if (process.arch === 'x64') {
    const x32Result = await runProcess([helperPaths.seccomp, '--probe-x32'], { timeoutMs: 10_000 });
    x32Available = x32Result.signal === 'SIGSYS';
    addRecord({
      id: 'host.seccomp-x32',
      category: 'seccomp_raw_probe',
      runtime: 'host',
      argv: [helperPaths.seccomp, '--probe-x32'],
      result: x32Result,
      parserId: 'x32-probe',
      parserRule: 'On x86_64, derive availability only from the child terminating with SIGSYS before syscall dispatch.',
    });
  } else {
    addRecord({
      id: 'host.seccomp-x32',
      category: 'seccomp_raw_probe',
      runtime: 'host',
      argv: [helperPaths.seccomp, '--probe-x32'],
      result: syntheticResult(
        jsonBuffer({
          availability: 'not_applicable',
          reason: 'x32 ABI exists only on x86_64',
          facts: { arch: process.arch },
        }),
      ),
      parserId: 'json-facts',
      parserRule: 'x32 rejection is not applicable off x86_64.',
    });
  }
}

const sentinelScript = path.join(dirs.pluginConfig, 'ordinary-child-wait.sh');
fs.writeFileSync(
  sentinelScript,
  String.raw`#!/bin/sh
set -eu
out=$1
pid_out=$2
(
  sleep 0.05
  printf 'child-complete\n' >> "$out"
) &
child=$!
printf '%s\n' "$child" > "$pid_out"
wait "$child"
printf 'wait-complete\n' >> "$out"
`,
  { flag: 'wx', mode: 0o500 },
);

const spawnRecords = {};
async function runtimeSpawnRow(runtime) {
  const sentinel = path.join(dirs.sentinels, `${runtime}-ordinary-spawn.txt`);
  const pidFile = path.join(dirs.sentinels, `${runtime}-ordinary-spawn.pid`);
  const shellCommand = `${quoteShell(sentinelScript)} ${quoteShell(sentinel)} ${quoteShell(pidFile)}`;
  const prompt = `Use the shell tool to run exactly this command and wait for it to finish: ${shellCommand}. Do not simulate the command. After the tool exits, print exactly RELAY_STRONG_SPAWN_OK`;
  const command = runtimeCommand(runtime, prompt, { tools: true, seccomp: true });
  let runtimeResult;
  let unavailableReason = null;
  if (!authAvailable[runtime]) {
    unavailableReason = 'isolated runtime authentication preflight failed';
    runtimeResult = syntheticResult('');
  } else if (!filterAvailable || !rawSeccompAvailable || !x32Available) {
    unavailableReason = 'candidate seccomp filter or raw ABI prerequisite is unavailable';
    runtimeResult = syntheticResult('');
  } else {
    const env = {
      ...command.env,
      RELAY_PROBE_RUNTIME: runtime,
      RELAY_PROBE_TARGET_EVENT: 'none',
      RELAY_PROBE_HOOK_MODE: 'allow',
      RELAY_PROBE_HOOK_LOG: path.join(dirs.raw, `strong-spawn-${runtime}-hooks.jsonl`),
    };
    runtimeResult = await runProcess(command.actual, { env, timeoutMs: 240_000 });
  }
  const sentinelBytes = exists(sentinel) ? fs.readFileSync(sentinel) : Buffer.alloc(0);
  const childPid = exists(pidFile) ? Number(fs.readFileSync(pidFile, 'utf8').trim()) : null;
  let childReaped = false;
  let procAfter = null;
  if (Number.isInteger(childPid) && childPid > 1 && process.platform === 'linux') {
    try {
      procAfter = fs.readFileSync(`/proc/${childPid}/stat`, 'utf8');
    } catch (error) {
      procAfter = error.code || String(error);
    }
    childReaped = procAfter === 'ENOENT';
  } else if (Number.isInteger(childPid) && childPid > 1) {
    try {
      process.kill(childPid, 0);
      childReaped = false;
    } catch (error) {
      childReaped = error.code === 'ESRCH';
      procAfter = error.code || String(error);
    }
  }
  const envelope = {
    runtime_stdout_base64: runtimeResult.stdout.toString('base64'),
    runtime_stderr_base64: runtimeResult.stderr.toString('base64'),
    sentinel_base64: sentinelBytes.toString('base64'),
    child_pid: childPid,
    child_reaped: childReaped,
    proc_after: procAfter,
    filter_sha256: filterSha,
    policy_sha256: policySha,
    unavailable_reason: unavailableReason,
  };
  return addRecord({
    id: `spawn.${runtime}.candidate-filter`,
    category: 'runtime_spawn',
    runtime,
    runtimeVersion: versions[runtime],
    argv: command.logical,
    result: { ...runtimeResult, stdout: jsonBuffer(envelope), stderr: Buffer.alloc(0) },
    parserId: 'runtime-spawn',
    parserRule:
      'Require an external two-line child-complete then wait-complete sentinel, zero runtime exit, no surviving child, exact candidate-filter hash, and real CLI tool marker.',
  });
}
for (const runtime of RUNTIMES) spawnRecords[runtime] = await runtimeSpawnRow(runtime);

for (const credential of forwardedCredentials) {
  fs.rmSync(credential, { force: true });
  assert.equal(exists(credential), false, 'forwarded credential copy must be removed before evidence sealing');
}

function buildSummary() {
  const appserver = records.find((record) => record.id === 'appserver.protocol.contract');
  const cgroup = records.find((record) => record.id === 'host.cgroup-v2');
  const namespace = records.find((record) => record.id === 'host.namespace-isolation');
  const filter = records.find((record) => record.id === 'host.seccomp-filter');
  const raw = records.find((record) => record.id === 'host.seccomp-raw');
  const x32 = records.find((record) => record.id === 'host.seccomp-x32');
  const timing = records.filter(
    (record) => record.category === 'attach_timing' && record.derived.availability === 'available',
  );
  const maxObserved = timing.length > 0 ? Math.max(...timing.map((record) => record.derived.facts.measured_ms)) : null;
  const managedAttachDeadline = maxObserved === null ? null : maxObserved + Math.max(2000, Math.ceil(maxObserved / 2));
  const deadlineWithin = managedAttachDeadline !== null && managedAttachDeadline <= 20_000 && timing.length === 20;
  const common = [cgroup, namespace, filter, raw, x32].every(
    (record) => record?.derived.availability === 'available' || record?.derived.availability === 'not_applicable',
  );
  const runtimeSummary = {};
  for (const runtime of RUNTIMES) {
    const spawnRecord = records.find((record) => record.id === `spawn.${runtime}.candidate-filter`);
    const failed = [cgroup, namespace, filter, raw, x32, spawnRecord]
      .filter((record) => !record || !['available', 'not_applicable'].includes(record.derived.availability))
      .map((record) => record?.id || 'missing_record');
    const strong = common && spawnRecord?.derived.availability === 'available';
    runtimeSummary[runtime] = {
      version: versions[runtime],
      auth: authAvailable[runtime] ? 'available' : 'unavailable',
      hook_rows: records.filter((record) => record.category === 'hook_contract' && record.runtime === runtime).length,
      strong_cgroup: strong ? 'available' : 'unavailable',
      strong_cgroup_reason: strong
        ? 'all common strong-cgroup prerequisites and the real runtime child+wait probe passed'
        : `failed prerequisites: ${failed.join(',') || 'unknown'}`,
      filter_sha256: filterSha,
    };
  }
  return {
    shared_protocol: 'observation_only',
    protocol_tree: appserver?.derived.facts.protocol_tree === 'available' ? 'available' : 'unavailable',
    protocol_reason: appserver?.derived.reason || 'protocol evidence record missing',
    protocol_lineage_deadline_ms: PROTOCOL_DEADLINE_MS,
    worker_tree_threat_model: 'cooperative',
    managed_attach_deadline_ms: managedAttachDeadline,
    managed_attach_deadline_within_limit: deadlineWithin,
    runtimes: runtimeSummary,
  };
}

function validateShape(evidence) {
  const expectedTop = [
    'schema_version',
    'schema_sha256',
    'generated_at',
    'harness',
    'invocation',
    'isolation',
    'records',
    'hash_chain',
    'summary',
  ];
  assert.deepEqual([...schema.required].sort(), [...expectedTop].sort(), 'evidence schema required fields');
  assert.equal(schema.additionalProperties, false, 'evidence schema rejects unknown top-level fields');
  exactKeys(evidence, expectedTop, 'evidence');
  assert.equal(evidence.schema_version, '1');
  assert.equal(evidence.schema_sha256, sha256(schemaBytes), 'evidence schema hash');
  assert.doesNotThrow(() => new Date(evidence.generated_at).toISOString());
  exactKeys(evidence.harness, ['path', 'git_blob_oid', 'sha256'], 'harness');
  exactKeys(evidence.invocation, ['argv', 'real_runtime_test', 'verify_current'], 'invocation');
  exactKeys(
    evidence.isolation,
    [
      'artifact_root',
      'home',
      'codex_home',
      'claude_config_dir',
      'plugin_config',
      'relay_store',
      'cwd',
      'sentinels',
      'mode',
    ],
    'isolation',
  );
  exactKeys(evidence.hash_chain, ['algorithm', 'genesis', 'head', 'record_count'], 'hash_chain');
  exactKeys(
    evidence.summary,
    [
      'shared_protocol',
      'protocol_tree',
      'protocol_reason',
      'protocol_lineage_deadline_ms',
      'worker_tree_threat_model',
      'managed_attach_deadline_ms',
      'managed_attach_deadline_within_limit',
      'runtimes',
    ],
    'summary',
  );
  exactKeys(evidence.summary.runtimes, ['claude', 'codex'], 'summary.runtimes');
  for (const runtime of RUNTIMES)
    exactKeys(
      evidence.summary.runtimes[runtime],
      ['version', 'auth', 'hook_rows', 'strong_cgroup', 'strong_cgroup_reason', 'filter_sha256'],
      `summary.runtimes.${runtime}`,
    );
  assert.equal(Array.isArray(evidence.records), true);
  assert.ok(evidence.records.length > 0);
  assert.equal(
    evidence.records.filter((record) => record.category === 'hook_contract').length,
    12,
    'exactly 12 hook contract rows',
  );
  assert.equal(
    evidence.records.filter((record) => record.category === 'attach_timing').length,
    20,
    'exactly 10 timing rows per runtime',
  );
  const ids = new Set();
  const recordKeys = [
    'id',
    'category',
    'runtime',
    'runtime_version',
    'platform',
    'argv',
    'started_at',
    'finished_at',
    'duration_ms',
    'exit_status',
    'signal',
    'timed_out',
    'stdout',
    'stderr',
    'parser',
    'derived',
    'previous_hash',
    'record_hash',
  ];
  assert.deepEqual([...schema.$defs.record.required].sort(), [...recordKeys].sort(), 'record schema required fields');
  assert.equal(schema.$defs.record.additionalProperties, false, 'record schema rejects unknown fields');
  const categories = new Set(schema.$defs.record.properties.category.enum);
  const runtimeNames = new Set(schema.$defs.record.properties.runtime.enum);
  const availabilities = new Set(schema.$defs.derived.properties.availability.enum);
  let previous = GENESIS;
  for (const record of evidence.records) {
    exactKeys(record, recordKeys, `record ${record.id}`);
    assert.equal(ids.has(record.id), false, `duplicate evidence id ${record.id}`);
    ids.add(record.id);
    exactKeys(record.platform, ['os', 'arch', 'release'], `${record.id}.platform`);
    exactKeys(record.stdout, ['artifact', 'sha256', 'bytes', 'base64'], `${record.id}.stdout`);
    exactKeys(record.stderr, ['artifact', 'sha256', 'bytes', 'base64'], `${record.id}.stderr`);
    exactKeys(record.parser, ['id', 'version', 'rule'], `${record.id}.parser`);
    exactKeys(record.derived, ['availability', 'reason', 'facts'], `${record.id}.derived`);
    assert.equal(categories.has(record.category), true, `${record.id} category is schema-known`);
    assert.equal(runtimeNames.has(record.runtime), true, `${record.id} runtime is schema-known`);
    assert.equal(availabilities.has(record.derived.availability), true, `${record.id} availability is schema-known`);
    assert.equal(
      Array.isArray(record.argv) && record.argv.length > 0 && record.argv.every((arg) => typeof arg === 'string'),
      true,
      `${record.id} argv`,
    );
    assert.equal(Number.isInteger(record.duration_ms) && record.duration_ms >= 0, true, `${record.id} duration`);
    assert.match(record.stdout.sha256, /^[0-9a-f]{64}$/);
    assert.match(record.stderr.sha256, /^[0-9a-f]{64}$/);
    assert.match(record.record_hash, /^[0-9a-f]{64}$/);
    assert.match(record.previous_hash, /^[0-9a-f]{64}$/);
    assert.equal(record.previous_hash, previous, `${record.id} chain predecessor`);
    const stdout = readRecordStream(record, 'stdout');
    const stderr = readRecordStream(record, 'stderr');
    const parser = parsers.get(record.parser.id);
    assert.ok(parser, `${record.id} parser exists`);
    assert.deepEqual(parser(record, stdout, stderr), record.derived, `${record.id} derived evidence is parser-owned`);
    const withoutHashes = { ...record };
    delete withoutHashes.previous_hash;
    delete withoutHashes.record_hash;
    assert.equal(
      sha256(`${record.previous_hash}\n${canonical(withoutHashes)}`),
      record.record_hash,
      `${record.id} hash`,
    );
    previous = record.record_hash;
  }
  assert.equal(evidence.hash_chain.genesis, GENESIS);
  assert.equal(evidence.hash_chain.record_count, evidence.records.length);
  assert.equal(evidence.hash_chain.head, previous);
  assert.deepEqual(evidence.summary, buildSummary(), 'summary must be derived exclusively from validated records');
}

const localBlob = firstLine(
  spawnSync('git', ['hash-object', SCRIPT_REL], { cwd: ROOT, encoding: 'utf8' }).stdout || '',
);
const committedBlobRun = spawnSync('git', ['rev-parse', `HEAD:${SCRIPT_REL}`], { cwd: ROOT, encoding: 'utf8' });
assert.equal(committedBlobRun.status, 0, 'harness must be committed before the real-runtime gate runs');
const committedBlob = committedBlobRun.stdout.trim();
assert.equal(localBlob, committedBlob, 'working-tree harness bytes differ from the committed git blob');

const summary = buildSummary();
const evidence = {
  schema_version: '1',
  schema_sha256: sha256(schemaBytes),
  generated_at: isoNow(),
  harness: {
    path: SCRIPT_REL,
    git_blob_oid: committedBlob,
    sha256: sha256(fs.readFileSync(path.join(ROOT, SCRIPT_REL))),
  },
  invocation: {
    argv: [process.execPath, SCRIPT_REL, '--verify-current'],
    real_runtime_test: true,
    verify_current: true,
  },
  isolation: {
    artifact_root: artifactRoot,
    home: dirs.home,
    codex_home: dirs.codexHome,
    claude_config_dir: dirs.claudeConfig,
    plugin_config: dirs.pluginConfig,
    relay_store: dirs.relayStore,
    cwd: dirs.cwd,
    sentinels: dirs.sentinels,
    mode: '0700',
  },
  records,
  hash_chain: {
    algorithm: 'sha256(previous_hash + LF + canonical_record_without_hashes)',
    genesis: GENESIS,
    head: records.at(-1).record_hash,
    record_count: records.length,
  },
  summary,
};
validateShape(evidence);

const evidencePath = path.join(artifactRoot, 'lifecycle-capability-evidence.json');
const evidenceBytes = Buffer.from(`${JSON.stringify(evidence, null, 2)}\n`, 'utf8');
fs.writeFileSync(evidencePath, evidenceBytes, { flag: 'wx', mode: 0o400 });
const reread = JSON.parse(fs.readFileSync(evidencePath, 'utf8'));
validateShape(reread);
function sealArtifacts(dir) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isSymbolicLink()) continue;
    if (entry.isDirectory()) {
      sealArtifacts(full);
      fs.chmodSync(full, 0o500);
    } else if (entry.isFile()) fs.chmodSync(full, 0o400);
  }
}
sealArtifacts(artifactRoot);
fs.chmodSync(artifactRoot, 0o500);

console.log('shared_protocol=observation_only');
console.log('worker_tree_threat_model=cooperative');
console.log(`protocol_tree=${summary.protocol_tree} reason=${JSON.stringify(summary.protocol_reason)}`);
console.log(`protocol_lineage_deadline_ms=${PROTOCOL_DEADLINE_MS} measured_ms=${appserverMeasuredMs}`);
console.log(
  `managed_attach_deadline_ms=${summary.managed_attach_deadline_ms === null ? 'unavailable' : summary.managed_attach_deadline_ms} within_20s=${summary.managed_attach_deadline_within_limit}`,
);
for (const runtime of RUNTIMES) {
  const row = summary.runtimes[runtime];
  if (row.strong_cgroup === 'available')
    console.log(`PASS strong_cgroup_spawn runtime=${runtime} clone3_errno=ENOSYS filter_sha256=${row.filter_sha256}`);
  else console.log(`strong_cgroup=unavailable runtime=${runtime} reason=${JSON.stringify(row.strong_cgroup_reason)}`);
}
console.log(`evidence_artifact=${evidencePath}`);
console.log(`evidence_sha256=${sha256(evidenceBytes)}`);
console.log(`raw_hash_chain_head=${evidence.hash_chain.head}`);
console.log(`harness_git_blob=${committedBlob}`);
