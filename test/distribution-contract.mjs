#!/usr/bin/env node

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parse as parseYaml } from 'yaml';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, '../../..');
const HELPER = path.join(REPO, 'scripts/capture-tdd-red.mjs');
const LAUNCHER = path.join(REPO, 'plugins/session-relay/bin/relay');
const RELEASE = path.join(REPO, 'scripts/release.mjs');
const WORKFLOW = path.join(REPO, '.github/workflows/build-binaries.yml');
const COMPANION = path.join(REPO, 'plugins/session-relay/test/companion-distribution-contract.mjs');
const DOCKS_PLAN = 'docs/plans/active/session-relay-linux-workspace-recertification.md';
const RELEASE_VERSION = '0.13.0';
const RELEASE_TAG = `session-relay--v${RELEASE_VERSION}`;
const PUBLIC_REF = /^refs\/heads\/preflight\/session-relay-cli-0\.13\.0-[0-9a-f]{12}$/;
const COMMIT = /^[0-9a-f]{40}$/;
const ASSETS = [
  'session-relay-aarch64-apple-darwin',
  'session-relay-aarch64-unknown-linux-musl',
  'session-relay-x86_64-apple-darwin',
  'session-relay-x86_64-unknown-linux-musl',
];
const sha256 = (value) => createHash('sha256').update(value).digest('hex');
const run = (command, args, options = {}) =>
  spawnSync(command, args, {
    cwd: options.cwd ?? REPO,
    env: options.env ?? process.env,
    encoding: options.encoding ?? 'utf8',
    input: options.input,
    shell: false,
    stdio: options.stdio,
  });
const git = (cwd, args) => {
  const result = run('git', args, { cwd });
  assert.equal(result.status, 0, `git ${args.join(' ')}: ${result.stderr}`);
  return result.stdout.trim();
};
const canonicalize = (value) => {
  if (value === null || typeof value !== 'object') return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalize).join(',')}]`;
  return `{${Object.keys(value)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${canonicalize(value[key])}`)
    .join(',')}}`;
};
const exactKeys = (object, expected, label) =>
  assert.deepEqual(Object.keys(object).sort(), [...expected].sort(), label);
let passed = 0;
const check = (label, fn) => {
  try {
    fn();
    passed += 1;
    process.stdout.write(`  ok: ${label}\n`);
  } catch (error) {
    error.message = `${label}: ${error.message}`;
    throw error;
  }
};

function parseCli(argv) {
  const result = {
    releaseFixtures: null,
    publicRemote: null,
    publicRef: null,
    publicCommit: null,
    detachedClone: false,
  };
  const singleton = new Set();
  for (let index = 0; index < argv.length; index += 1) {
    const option = argv[index];
    if (option === '--detached-clone') {
      assert.ok(!singleton.has(option), `duplicate ${option}`);
      singleton.add(option);
      result.detachedClone = true;
      continue;
    }
    const field = {
      '--release-fixtures': 'releaseFixtures',
      '--public-remote': 'publicRemote',
      '--public-ref': 'publicRef',
      '--public-commit': 'publicCommit',
    }[option];
    assert.ok(field, `unknown option ${option}`);
    assert.ok(!singleton.has(option), `duplicate ${option}`);
    singleton.add(option);
    assert.ok(argv[index + 1] && !argv[index + 1].startsWith('--'), `missing value for ${option}`);
    result[field] = argv[index + 1];
    index += 1;
  }
  const companionValues = [result.publicRemote, result.publicRef, result.publicCommit];
  if (companionValues.some(Boolean) || result.detachedClone) {
    assert.ok(
      companionValues.every(Boolean) && result.detachedClone,
      'public verification requires remote, ref, commit, and --detached-clone',
    );
    assert.match(result.publicCommit, COMMIT);
  }
  if (result.releaseFixtures) {
    assert.equal(path.isAbsolute(result.releaseFixtures), true, '--release-fixtures must be absolute');
    assert.equal(
      fs.realpathSync.native(result.releaseFixtures),
      result.releaseFixtures,
      '--release-fixtures must be canonical',
    );
  }
  return result;
}
const cli = parseCli(process.argv.slice(2));

function makeHelperFixture() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'capture-tdd-red-fixture-'));
  fs.mkdirSync(path.join(root, 'scripts'), { recursive: true });
  fs.mkdirSync(path.join(root, 'test'), { recursive: true });
  fs.copyFileSync(HELPER, path.join(root, 'scripts/capture-tdd-red.mjs'));
  fs.chmodSync(path.join(root, 'scripts/capture-tdd-red.mjs'), 0o755);
  fs.writeFileSync(path.join(root, 'test/a.mjs'), 'export const a = 1;\n');
  fs.writeFileSync(path.join(root, 'test/z.mjs'), 'export const z = 1;\n');
  fs.symlinkSync('a.mjs', path.join(root, 'test/link.mjs'));
  git(root, ['init', '-q']);
  git(root, ['config', 'user.email', 'fixture@example.invalid']);
  git(root, ['config', 'user.name', 'Fixture']);
  git(root, ['add', 'scripts/capture-tdd-red.mjs', 'test/a.mjs', 'test/z.mjs', 'test/link.mjs']);
  git(root, ['commit', '-qm', 'fixture']);
  return {
    root: fs.realpathSync.native(root),
    helper: path.join(root, 'scripts/capture-tdd-red.mjs'),
    commit: git(root, ['rev-parse', 'HEAD']),
  };
}

function helperArgs(fixture, receipt, command, extra = []) {
  return [
    '--repo',
    fixture.root,
    '--repository-id',
    'Fixture/example',
    '--pre-production-commit',
    fixture.commit,
    '--test',
    'test/z.mjs',
    '--test',
    'test/a.mjs',
    '--receipt-out',
    receipt,
    ...extra,
    '--',
    ...command,
  ];
}

function testCaptureHelper() {
  const fixture = makeHelperFixture();
  try {
    const receiptPath = path.join(fixture.root, 'red.json');
    const exactTokens = ['white space', '$(touch NEVER)', ';', '*', '--', '--flag=value', 'line\nbreak'];
    const source =
      'process.stdout.write(JSON.stringify(process.argv.slice(1)));process.stderr.write("frozen stderr");process.exit(17)';
    const result = run(
      process.execPath,
      [fixture.helper, ...helperArgs(fixture, receiptPath, [process.execPath, '-e', source, ...exactTokens])],
      { cwd: '/' },
    );
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /^[0-9a-f]{64}\n$/);
    assert.equal(result.stderr, '');
    assert.equal(fs.statSync(receiptPath).mode & 0o777, 0o600);
    const bytes = fs.readFileSync(receiptPath);
    assert.equal(result.stdout.trim(), sha256(bytes));
    const receipt = JSON.parse(bytes);
    assert.equal(bytes.toString(), canonicalize(receipt), 'receipt must be RFC 8785 canonical UTF-8 without a newline');
    exactKeys(
      receipt,
      [
        'schema',
        'type',
        'repository_id',
        'pre_production_commit',
        'test_paths',
        'command',
        'exit_code',
        'stdout_sha256',
        'stderr_sha256',
        'captured_at',
        'producer',
      ],
      'receipt schema is closed',
    );
    exactKeys(receipt.command, ['cwd', 'argv'], 'command schema is closed');
    exactKeys(receipt.producer, ['path', 'blob_id', 'version'], 'producer schema is closed');
    for (const bound of receipt.test_paths) exactKeys(bound, ['path', 'blob_id'], 'test binding schema is closed');
    assert.equal(receipt.schema, 1);
    assert.equal(receipt.type, 'TddRedReceiptV1');
    assert.equal(receipt.repository_id, 'Fixture/example');
    assert.equal(receipt.pre_production_commit, fixture.commit);
    assert.deepEqual(
      receipt.test_paths.map(({ path: testPath }) => testPath),
      ['test/a.mjs', 'test/z.mjs'],
    );
    assert.deepEqual(
      receipt.test_paths.map(({ blob_id: blob }) => blob),
      [
        git(fixture.root, ['rev-parse', `${fixture.commit}:test/a.mjs`]),
        git(fixture.root, ['rev-parse', `${fixture.commit}:test/z.mjs`]),
      ],
    );
    assert.deepEqual(receipt.command, { cwd: fixture.root, argv: [process.execPath, '-e', source, ...exactTokens] });
    assert.equal(receipt.exit_code, 17);
    assert.equal(receipt.stdout_sha256, sha256(JSON.stringify(exactTokens)));
    assert.equal(receipt.stderr_sha256, sha256('frozen stderr'));
    assert.match(receipt.captured_at, /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/);
    assert.deepEqual(receipt.producer, {
      path: 'scripts/capture-tdd-red.mjs',
      blob_id: git(fixture.root, ['rev-parse', `${fixture.commit}:scripts/capture-tdd-red.mjs`]),
      version: '1',
    });

    const verbosePath = path.join(fixture.root, 'verbose-red.json');
    const verboseSource = 'require("node:fs").writeSync(1,Buffer.alloc(2*1024*1024,0x61));process.exit(23)';
    const verboseRun = run(process.execPath, [
      fixture.helper,
      ...helperArgs(fixture, verbosePath, [process.execPath, '-e', verboseSource]),
    ]);
    assert.equal(verboseRun.status, 0, verboseRun.stderr);
    const verboseReceipt = JSON.parse(fs.readFileSync(verbosePath));
    assert.equal(verboseReceipt.exit_code, 23);
    assert.equal(verboseReceipt.stdout_sha256, sha256(Buffer.alloc(2 * 1024 * 1024, 0x61)));
    assert.equal(verboseRun.stdout.trim(), sha256(fs.readFileSync(verbosePath)));

    const failureCases = [
      ['unknown option', ['--wat', 'x']],
      ['duplicate singleton', ['--repo', fixture.root]],
      ['missing singleton value', ['--repository-id']],
    ];
    for (const [name, extra] of failureCases) {
      const output = path.join(fixture.root, `failure-${name.replaceAll(' ', '-')}.json`);
      const failed = run(process.execPath, [
        fixture.helper,
        ...helperArgs(fixture, output, [process.execPath, '-e', 'process.exit(1)'], extra),
      ]);
      assert.notEqual(failed.status, 0, name);
      assert.equal(fs.existsSync(output), false, `${name} must not create a receipt`);
    }

    const mutate = (args, name) => {
      const output = path.join(fixture.root, `reject-${name}.json`);
      const failed = run(process.execPath, [fixture.helper, ...args(output)]);
      assert.notEqual(failed.status, 0, `${name} unexpectedly succeeded`);
      assert.equal(fs.existsSync(output), false, `${name} created a receipt`);
    };
    const base = (output) => helperArgs(fixture, output, [process.execPath, '-e', 'process.exit(9)']);
    mutate(
      (output) => base(output).filter((token, i, all) => !(token === '--' && i === all.lastIndexOf('--'))),
      'missing-separator',
    );
    mutate((output) => [...base(output).slice(0, -3), '--'], 'empty-command');
    mutate(
      (output) => base(output).map((token) => (token === 'Fixture/example' ? 'not-an-id' : token)),
      'repository-id',
    );
    mutate(
      (output) => base(output).map((token) => (token === fixture.commit ? fixture.commit.toUpperCase() : token)),
      'uppercase-commit',
    );
    const treeObject = git(fixture.root, ['rev-parse', 'HEAD^{tree}']);
    mutate(
      (output) => base(output).map((token) => (token === fixture.commit ? treeObject : token)),
      'non-commit-object',
    );
    mutate(
      (output) => base(output).map((token) => (token === 'test/z.mjs' ? './test/z.mjs' : token)),
      'noncanonical-test',
    );
    mutate((output) => base(output).map((token) => (token === 'test/z.mjs' ? 'test/a.mjs' : token)), 'duplicate-test');
    mutate(
      (output) => base(output).map((token) => (token === fixture.root ? `${fixture.root}/` : token)),
      'noncanonical-repo',
    );

    fs.writeFileSync(path.join(fixture.root, 'test/untracked.mjs'), 'x\n');
    mutate(
      (output) => base(output).map((token) => (token === 'test/z.mjs' ? 'test/untracked.mjs' : token)),
      'untracked-test',
    );
    mutate(
      (output) => base(output).map((token) => (token === 'test/z.mjs' ? 'test/link.mjs' : token)),
      'nonregular-test',
    );
    fs.writeFileSync(path.join(fixture.root, 'test/a.mjs'), 'dirty\n');
    mutate((output) => base(output), 'dirty-test');
    fs.writeFileSync(path.join(fixture.root, 'test/a.mjs'), 'export const a = 1;\n');

    const existing = path.join(fixture.root, 'existing.json');
    fs.writeFileSync(existing, 'sentinel');
    const existingRun = run(process.execPath, [fixture.helper, ...base(existing)]);
    assert.notEqual(existingRun.status, 0);
    assert.equal(fs.readFileSync(existing, 'utf8'), 'sentinel');

    mutate((output) => helperArgs(fixture, output, [process.execPath, '-e', 'process.exit(0)']), 'zero-exit');
    mutate(
      (output) => helperArgs(fixture, output, [process.execPath, '-e', 'process.kill(process.pid,"SIGTERM")']),
      'signal-exit',
    );
    mutate((output) => helperArgs(fixture, output, [path.join(fixture.root, 'missing-command')]), 'missing-executable');

    fs.appendFileSync(fixture.helper, '\n');
    mutate((output) => base(output), 'dirty-producer');
  } finally {
    fs.rmSync(fixture.root, { recursive: true, force: true });
  }
}

check('capture helper is shell-free, canonical, blob-bound, atomic, and fail-closed', testCaptureHelper);

function makeExecutable(file, body) {
  fs.writeFileSync(file, `#!/usr/bin/env node\n${body}\n`, { mode: 0o755 });
}

function launcherRun(env, args = ['--version']) {
  return run(LAUNCHER, args, { env: { ...process.env, ...env }, cwd: REPO });
}

function testLauncher() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-launcher-'));
  try {
    const envDir = path.join(root, 'env');
    const pathDir = path.join(root, 'path');
    const home = path.join(root, 'home');
    fs.mkdirSync(envDir);
    fs.mkdirSync(pathDir);
    fs.mkdirSync(path.join(home, '.local/bin'), { recursive: true });
    const record = path.join(root, 'record.json');
    const stubBody = (name) =>
      `require('node:fs').writeFileSync(process.env.RELAY_RECORD, JSON.stringify({name:${JSON.stringify(name)},argv:process.argv.slice(2)}));process.stdout.write(${JSON.stringify(`${name} ${RELEASE_VERSION}\n`)});`;
    const explicit = path.join(envDir, 'explicit');
    makeExecutable(explicit, stubBody('explicit'));
    makeExecutable(path.join(pathDir, 'session-relay'), stubBody('path'));
    makeExecutable(path.join(home, '.local/bin/session-relay'), stubBody('home'));
    const common = {
      HOME: home,
      PATH: `${pathDir}${path.delimiter}${path.dirname(process.execPath)}${path.delimiter}/usr/bin:/bin`,
      RELAY_RECORD: record,
    };

    let result = launcherRun({ ...common, SESSION_RELAY_BIN: explicit }, ['send', 'agent', '--', 'a b', '--flag']);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stdout, `explicit ${RELEASE_VERSION}\n`);
    assert.deepEqual(JSON.parse(fs.readFileSync(record, 'utf8')), {
      name: 'explicit',
      argv: ['send', 'agent', '--', 'a b', '--flag'],
    });

    fs.rmSync(record, { force: true });
    result = launcherRun({ ...common, SESSION_RELAY_BIN: '' });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(JSON.parse(fs.readFileSync(record, 'utf8')).name, 'path');

    fs.rmSync(path.join(pathDir, 'session-relay'));
    fs.rmSync(record, { force: true });
    result = launcherRun({ ...common, SESSION_RELAY_BIN: '' });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(JSON.parse(fs.readFileSync(record, 'utf8')).name, 'home');

    result = launcherRun({ ...common, SESSION_RELAY_BIN: path.join(root, 'absent') });
    assert.notEqual(result.status, 0, 'an explicit invalid override must fail rather than silently fall through');
    assert.match(result.stderr, /SESSION_RELAY_BIN|not.*executable|not found/i);

    const recursePath = path.join(root, 'relay-link');
    fs.symlinkSync(LAUNCHER, recursePath);
    result = launcherRun({ ...common, SESSION_RELAY_BIN: recursePath });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /recurs|itself/i);

    fs.rmSync(path.join(home, '.local/bin/session-relay'));
    result = launcherRun({ ...common, PATH: pathDir, SESSION_RELAY_BIN: '' });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /docks-kit sync/);
    assert.match(result.stderr, /docks-kit toolchain ensure session-relay/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

check('launcher resolves explicit, PATH, and home executables without recursion or fallback', testLauncher);

check('only the launcher remains tracked in the plugin bin directory', () => {
  assert.deepEqual(git(REPO, ['ls-files', 'plugins/session-relay/bin']).split('\n').filter(Boolean), [
    'plugins/session-relay/bin/relay',
  ]);
});

check('Cargo and plugin manifests expose exact synchronized release version', () => {
  const cargo = fs
    .readFileSync(path.join(REPO, 'plugins/session-relay/rust/Cargo.toml'), 'utf8')
    .match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  const claude = JSON.parse(
    fs.readFileSync(path.join(REPO, 'plugins/session-relay/.claude-plugin/plugin.json')),
  ).version;
  const codex = JSON.parse(fs.readFileSync(path.join(REPO, 'plugins/session-relay/.codex-plugin/plugin.json'))).version;
  const market = JSON.parse(fs.readFileSync(path.join(REPO, '.claude-plugin/marketplace.json'))).plugins.find(
    ({ name }) => name === 'session-relay',
  ).version;
  assert.equal(cargo, RELEASE_VERSION);
  assert.equal(claude, RELEASE_VERSION);
  assert.equal(codex, RELEASE_VERSION);
  assert.equal(market, RELEASE_VERSION);
});

function assertNativeWorkflowBoundary(document) {
  const build = document.jobs.build;
  assert.equal(build.needs, 'identity');
  assert.equal(build['runs-on'], '${{ matrix.runner }}');
  exactKeys(build.strategy.matrix, ['include'], 'native matrix is closed');
  for (const leg of build.strategy.matrix.include)
    exactKeys(leg, ['runner', 'runner_os', 'runner_arch', 'target', 'asset'], 'native matrix leg is closed');
  assert.deepEqual(build.strategy.matrix.include, [
    {
      runner: 'ubuntu-24.04',
      runner_os: 'Linux',
      runner_arch: 'X64',
      target: 'x86_64-unknown-linux-musl',
      asset: 'session-relay-x86_64-unknown-linux-musl',
    },
    {
      runner: 'ubuntu-24.04-arm',
      runner_os: 'Linux',
      runner_arch: 'ARM64',
      target: 'aarch64-unknown-linux-musl',
      asset: 'session-relay-aarch64-unknown-linux-musl',
    },
    {
      runner: 'macos-15-intel',
      runner_os: 'macOS',
      runner_arch: 'X64',
      target: 'x86_64-apple-darwin',
      asset: 'session-relay-x86_64-apple-darwin',
    },
    {
      runner: 'macos-15',
      runner_os: 'macOS',
      runner_arch: 'ARM64',
      target: 'aarch64-apple-darwin',
      asset: 'session-relay-aarch64-apple-darwin',
    },
  ]);

  const names = build.steps.map((step) => step.name ?? step.uses);
  const evidenceSteps = [
    'build locked native release',
    'prove Linux managed-workspace custody',
    'prove macOS managed-workspace admission STOP',
    'smoke explicit fresh Linux workspace binary',
    'attest native release binary',
    'upload stable executable and canonical attestation',
  ];
  const firstEvidence = names.indexOf(evidenceSteps[0]);
  assert.notEqual(firstEvidence, -1, 'native evidence steps are absent');
  assert.deepEqual(
    evidenceSteps.map((name) => names.indexOf(name)),
    evidenceSteps.map((_, index) => firstEvidence + index),
    'native custody, refusal, smoke, and attestation steps must remain contiguous and ordered',
  );
  const byName = new Map(build.steps.map((step) => [step.name, step]));
  const linuxCustody = byName.get('prove Linux managed-workspace custody');
  assert.equal(linuxCustody.if, "runner.os == 'Linux'");
  assert.match(
    linuxCustody.run,
    /SESSION_RELAY_TEST_BIN="\$RUNNER_TEMP\/session-relay\/\$TARGET\/\$ASSET_NAME"[\s\S]*workspace_lease_process[\s\S]*linux_cgroup_pidfd_guardian_kills_hostile_descendants[\s\S]*--exact --nocapture/,
  );
  const linuxSmoke = byName.get('smoke explicit fresh Linux workspace binary');
  assert.equal(linuxSmoke.if, "runner.os == 'Linux'");
  assert.match(
    linuxSmoke.run,
    /bin="\$RUNNER_TEMP\/session-relay\/\$TARGET\/\$ASSET_NAME"[\s\S]*--case single-session-compat --bin "\$bin"[\s\S]*--case docs-contract --bin "\$bin"/,
  );
  const macosRefusal = byName.get('prove macOS managed-workspace admission STOP');
  assert.equal(macosRefusal.if, "runner.os == 'macOS'");
  assert.match(
    macosRefusal.run,
    /test "\$filesystem_type" = apfs[\s\S]*workspace_lease_process[\s\S]*macos_process_group_recursive_guardian_kills_hostile_descendants[\s\S]*--exact --nocapture/,
  );
  assert.doesNotMatch(macosRefusal.name, /custody|support/i);

  const upload = byName.get('upload stable executable and canonical attestation');
  assert.match(upload.with.path, /\$\{\{ matrix\.asset \}\}/);
  assert.match(upload.with.path, /attestation-\$\{\{ matrix\.target \}\}\.json/);
  assert.deepEqual(document.jobs.aggregate.needs, ['identity', 'build']);
  const aggregate = document.jobs.aggregate.steps.find(
    ({ name }) => name === 'validate closed attestations and generate SHA256SUMS',
  );
  assert.ok(aggregate, 'aggregate attestation closure is absent');
  assert.match(aggregate.run, /const attestationName = `attestation-\$\{target\}\.json`;/);
  assert.match(aggregate.run, /expectedFiles\.push\(assetName, attestationName\)/);
  for (const target of build.strategy.matrix.include.map(({ target }) => target)) {
    assert.match(
      aggregate.run,
      new RegExp(`['"]${target}['"]\\s*:`),
      `aggregate does not declare attestation row ${target}`,
    );
  }
  const checksumRows = aggregate.run
    .match(/sha256sum \\\n([\s\S]*?)> SHA256SUMS/)?.[1]
    ?.match(/session-relay-[a-z0-9_-]+/g);
  assert.deepEqual(checksumRows, ASSETS, 'checksum manifest must contain exactly the four ordinary native assets');
  assert.match(aggregate.run, /test "\$\(wc -l < SHA256SUMS\)" -eq 4/);
}

function workflowContract() {
  const document = parseYaml(fs.readFileSync(WORKFLOW, 'utf8'));
  exactKeys(document.on, ['push', 'workflow_dispatch'], 'workflow triggers are closed');
  assert.deepEqual(document.on.push.tags, ['session-relay--v*']);
  const inputs = document.on.workflow_dispatch.inputs;
  exactKeys(inputs, ['mode', 'expected_commit', 'expected_tag'], 'dispatch inputs are closed');
  assert.equal(inputs.mode.required, true);
  assert.equal(inputs.mode.type, 'choice');
  assert.deepEqual(inputs.mode.options, ['validate-only', 'publish-existing-tag']);
  assert.equal(inputs.expected_commit.required, true);
  assert.equal(inputs.expected_tag.required, false);
  assert.equal(inputs.expected_tag.default, '');
  assert.deepEqual(document.permissions, { contents: 'read' });

  const jobs = Object.values(document.jobs);
  const publishing = jobs.filter((job) => job.permissions?.contents === 'write');
  assert.equal(publishing.length, 1, 'only one publisher may receive contents: write');
  for (const job of jobs.filter((job) => !publishing.includes(job)))
    assert.notEqual(job.permissions?.contents, 'write');
  assertNativeWorkflowBoundary(document);

  const supportSubstitution = structuredClone(document);
  supportSubstitution.jobs.build.steps.find(
    ({ name }) => name === 'prove macOS managed-workspace admission STOP',
  ).name = 'prove macOS managed-workspace custody';
  assert.throws(
    () => assertNativeWorkflowBoundary(supportSubstitution),
    /contiguous|ordered|absent/,
    'macOS support substitution was accepted',
  );
  const missingDarwin = structuredClone(document);
  missingDarwin.jobs.build.strategy.matrix.include = missingDarwin.jobs.build.strategy.matrix.include.filter(
    ({ target }) => target !== 'aarch64-apple-darwin',
  );
  assert.throws(() => assertNativeWorkflowBoundary(missingDarwin), /deep-equal/, 'missing Darwin asset was accepted');
  const substitutedDarwin = structuredClone(document);
  substitutedDarwin.jobs.build.strategy.matrix.include[2].asset = 'session-relay-x86_64-unknown-linux-musl';
  assert.throws(
    () => assertNativeWorkflowBoundary(substitutedDarwin),
    /deep-equal/,
    'substituted Darwin asset was accepted',
  );

  for (const match of fs.readFileSync(WORKFLOW, 'utf8').matchAll(/uses:\s*([^\s#]+)/g)) {
    assert.match(match[1], /@[0-9a-f]{40}$/i, `action is not pinned by full commit: ${match[1]}`);
  }
}
check(
  'binary workflow publishes four native, attested, checksummed prerelease assets with least privilege',
  workflowContract,
);

const RECEIPT_TYPES = {
  'materialize-tdd-red': 'TddRedReceiptV1',
  'verify-source-ci': 'SourceCiReceiptV1',
  'check-prepared': 'SourcePreparationCandidateV1',
  'bind-completion': 'SourcePreparationProofV1',
  'publish-reviewed': 'SessionRelayPublicationReceiptV1',
  'promote-reviewed': 'PromotionReceiptV1',
  'resume-promotion': 'PromotionReceiptV1',
  'finalize-reviewed': 'SessionRelayPublicationReceiptV1',
};
const PUBLICATION_CASES = [
  ['tag-absent', 'prerelease'],
  ['tag-no-run', 'prerelease'],
  ['bound-run-no-release', 'prerelease'],
  ['partial-prerelease', 'prerelease'],
  ['complete-prerelease', 'prerelease'],
  ['premature-stable', 'conflict'],
  ['tag-conflict', 'conflict'],
  ['run-conflict', 'conflict'],
  ['release-conflict', 'conflict'],
  ['asset-conflict', 'conflict'],
  ['digest-conflict', 'conflict'],
];
const PROMOTION_CASES = [
  ['success', 'success'],
  ['expected-main-drift', 'manual_incident'],
  ['lock-contention', 'failure'],
  ['prepush-failure', 'failure'],
  ['postpush-live-failure', 'restored_failure'],
  ['restore-failure', 'failure'],
  ['unknown-authoritative-state', 'manual_incident'],
  ['transaction-gap', 'conflict'],
  ['terminal-rewrite', 'conflict'],
  ['closed-set-drift', 'conflict'],
  ['resume-after-initialized', 'success'],
  ['resume-after-locked', 'success'],
  ['resume-after-prepush', 'success'],
  ['resume-after-main-push', 'success'],
  ['recover-success-receipt', 'success'],
  ['recover-restored-failure-receipt', 'restored_failure'],
  ['recover-failure-receipt', 'failure'],
  ['recover-manual-incident-receipt', 'manual_incident'],
  ['retry-restored-failure', 'success'],
  ['retry-old-receipt', 'conflict'],
  ['retry-success', 'conflict'],
  ['retry-manual-incident', 'conflict'],
  ['retry-second-attempt-failure', 'conflict'],
];

function releaseFixtureRoot() {
  if (cli.releaseFixtures) return { root: cli.releaseFixtures, owned: false };
  return { root: fs.mkdtempSync(path.join(os.tmpdir(), 'session-relay-release-fixtures-')), owned: true };
}

function runReleaseFixture(root, mode, scenario, expectedOutcome, releaseArgs) {
  const directory = path.join(root, `${mode}-${scenario}`);
  fs.mkdirSync(directory, { recursive: true });
  const fixture = {
    schema: 1,
    type: 'SessionRelayReleaseFixtureV1',
    scenario,
    repository_id: 'DocksDocks/docks',
    source_commit: '1'.repeat(40),
    promoted_commit: '2'.repeat(40),
    expected_origin_main: '3'.repeat(40),
    tag: RELEASE_TAG,
    assets: [...ASSETS, 'SHA256SUMS'].map((name, index) => ({
      name,
      digest: String(index + 4)
        .repeat(64)
        .slice(0, 64),
      database_id: 100 + index,
    })),
    expected_outcome: expectedOutcome,
  };
  const fixturePath = path.join(directory, 'fixture.json');
  const reportPath = path.join(directory, 'report.json');
  fs.writeFileSync(fixturePath, `${JSON.stringify(fixture, null, 2)}\n`);
  const result = run(process.execPath, [RELEASE, ...releaseArgs], {
    env: {
      ...process.env,
      SESSION_RELAY_RELEASE_FIXTURE: fixturePath,
      SESSION_RELAY_RELEASE_REPORT: reportPath,
    },
  });
  const unsuccessful = ['conflict', 'failure', 'manual_incident', 'restored_failure'].includes(expectedOutcome);
  assert.equal(result.status, unsuccessful ? 1 : 0, `${mode}/${scenario}: ${result.stderr}`);
  assert.equal(fs.existsSync(reportPath), true, `${mode}/${scenario} did not emit a fixture report`);
  const report = JSON.parse(fs.readFileSync(reportPath));
  exactKeys(
    report,
    ['schema', 'type', 'scenario', 'outcome', 'calls', 'mutations', 'journal', 'receipt', 'state'],
    `${mode}/${scenario} report schema`,
  );
  assert.equal(report.schema, 1);
  assert.equal(report.type, 'SessionRelayReleaseFixtureReportV1');
  assert.equal(report.scenario, scenario);
  assert.equal(report.outcome, expectedOutcome);
  assert.ok(Array.isArray(report.calls));
  assert.ok(report.state && typeof report.state === 'object' && !Array.isArray(report.state));
  assert.ok(Array.isArray(report.mutations));
  if (report.receipt) {
    assert.equal(report.receipt.schema, 1);
    if (RECEIPT_TYPES[mode]) assert.equal(report.receipt.type, RECEIPT_TYPES[mode]);
    const receiptIndex = releaseArgs.indexOf('--receipt-out');
    if (receiptIndex >= 0) {
      const output = releaseArgs[receiptIndex + 1];
      assert.equal(fs.statSync(output).mode & 0o777, 0o600, `${mode}/${scenario} receipt mode`);
      const receiptBytes = fs.readFileSync(output);
      assert.equal(receiptBytes.toString(), canonicalize(report.receipt), `${mode}/${scenario} noncanonical receipt`);
      assert.equal(
        result.stdout.trim().split('\n').at(-1),
        sha256(receiptBytes),
        `${mode}/${scenario} receipt digest output`,
      );
    }
  }
  return report;
}

function releaseContracts() {
  const fixtureRoot = releaseFixtureRoot();
  const inPath = (name) => path.join(fixtureRoot.root, `${name}.json`);
  const outPath = (name) => path.join(fixtureRoot.root, `${name}.receipt.json`);
  const pair = (name, digest, flag = name) => [inPath(name), `--${flag}-sha256`, digest.repeat(64)];
  try {
    for (const [scenario, outcome] of PUBLICATION_CASES) {
      const args = [
        '--publish-reviewed',
        '--plugin',
        'session-relay',
        RELEASE_VERSION,
        '--source-proof',
        ...pair('source-proof', 'a'),
        '--receipt-out',
        outPath(`publication-${scenario}`),
      ];
      if (scenario === 'partial-prerelease' || scenario === 'complete-prerelease') {
        args.push('--resume-publication', ...pair('prior-publication', 'b', 'resume-publication'));
      }
      const report = runReleaseFixture(fixtureRoot.root, 'publish-reviewed', scenario, outcome, args);
      if (outcome === 'prerelease') {
        assert.deepEqual(report.receipt.assets.map(({ name }) => name).sort(), [...ASSETS, 'SHA256SUMS'].sort());
        assert.equal(report.receipt.release_state, 'prerelease');
        assert.doesNotMatch(report.state.release.body, /docks-kit sync|plugin install/i);
      }
    }

    for (const [scenario, outcome] of PROMOTION_CASES) {
      const resume = scenario.startsWith('resume') || scenario.startsWith('recover');
      const mode = resume ? 'resume-promotion' : 'promote-reviewed';
      const args = [
        `--${mode}`,
        '--plugin',
        'session-relay',
        RELEASE_VERSION,
        ...(resume ? ['--transaction-ref', `refs/heads/transactions/session-relay-${RELEASE_VERSION}`] : []),
        '--source-proof',
        ...pair('source-proof', 'a'),
        '--publication',
        ...pair('publication', 'b'),
        '--docks-kit-release',
        'cli-v0.9.0',
        '--expected-origin-main',
        '3'.repeat(40),
        '--receipt-out',
        outPath(`promotion-${scenario}`),
      ];
      if (scenario.startsWith('retry-')) args.push('--retry-failed', ...pair('failed-promotion', 'c', 'retry-failed'));
      const report = runReleaseFixture(fixtureRoot.root, mode, scenario, outcome, args);
      const keys = report.journal.map(({ attempt, sequence }) => `${attempt}:${sequence}`);
      assert.deepEqual(
        keys,
        [...keys].sort((a, b) => a.localeCompare(b, undefined, { numeric: true })),
        `${scenario} journal keys regress`,
      );
      assert.equal(new Set(keys).size, keys.length, `${scenario} journal key reused`);
      if (report.receipt) assert.equal(report.receipt.outcome, outcome);
    }

    for (const terminal of ['success', 'restored-failure', 'failure', 'manual-incident']) {
      const outcome = terminal === 'restored-failure' ? 'restored_failure' : terminal.replace('-', '_');
      const resumeArgs = (receipt) => [
        '--resume-promotion',
        '--plugin',
        'session-relay',
        RELEASE_VERSION,
        '--transaction-ref',
        `refs/heads/transactions/session-relay-${RELEASE_VERSION}`,
        '--source-proof',
        ...pair('source-proof', 'a'),
        '--publication',
        ...pair('publication', 'b'),
        '--docks-kit-release',
        'cli-v0.9.0',
        '--expected-origin-main',
        '3'.repeat(40),
        '--receipt-out',
        receipt,
      ];
      const uninterrupted = runReleaseFixture(
        fixtureRoot.root,
        'resume-promotion',
        `terminal-${terminal}-uninterrupted`,
        outcome,
        resumeArgs(outPath(`terminal-${terminal}-uninterrupted`)),
      );
      const recovered = runReleaseFixture(
        fixtureRoot.root,
        'resume-promotion',
        `terminal-${terminal}-recover`,
        outcome,
        resumeArgs(outPath(`terminal-${terminal}-recover`)),
      );
      assert.equal(
        canonicalize(recovered.receipt),
        canonicalize(uninterrupted.receipt),
        `${terminal} recovered receipt differs`,
      );
      assert.deepEqual(recovered.mutations, [], `${terminal} terminal recovery mutated state`);
    }

    for (const [scenario, outcome] of [
      ['finalize-prerelease', 'stable'],
      ['finalize-already-stable', 'stable'],
      ['finalize-resume', 'stable'],
      ['finalize-partial', 'conflict'],
      ['finalize-failed-promotion', 'conflict'],
    ]) {
      const args = [
        '--finalize-reviewed',
        '--plugin',
        'session-relay',
        RELEASE_VERSION,
        '--source-proof',
        ...pair('source-proof', 'a'),
        '--publication',
        ...pair('publication', 'b'),
        '--promotion',
        ...pair('promotion', 'c'),
        '--receipt-out',
        outPath(scenario),
      ];
      if (scenario === 'finalize-resume')
        args.push('--resume-finalization', ...pair('prior-final-publication', 'd', 'resume-finalization'));
      const report = runReleaseFixture(fixtureRoot.root, 'finalize-reviewed', scenario, outcome, args);
      if (outcome === 'stable') {
        assert.equal(report.receipt.release_state, 'stable');
        assert.equal(report.state.release.body.match(/```\n([^\n]+)\n```/)?.[1], 'docks-kit sync');
        assert.doesNotMatch(report.state.release.body, /plugin install session-relay/i);
      }
    }

    const preparation = [
      ['prepare', ['--prepare', '--plugin', 'session-relay', RELEASE_VERSION]],
      [
        'materialize-tdd-red',
        [
          '--materialize-tdd-red',
          '--plugin',
          'session-relay',
          RELEASE_VERSION,
          '--plan',
          DOCKS_PLAN,
          '--docks-red-out',
          outPath('docks-red'),
          '--public-red-out',
          outPath('public-red'),
        ],
      ],
      [
        'verify-embedded-preparation',
        ['--verify-embedded-preparation', '--plugin', 'session-relay', RELEASE_VERSION, '--plan', DOCKS_PLAN],
      ],
      [
        'verify-source-ci',
        [
          '--verify-source-ci',
          '--plugin',
          'session-relay',
          RELEASE_VERSION,
          '--run-id',
          '12345',
          '--expected-commit',
          '1'.repeat(40),
          '--receipt-out',
          outPath('source-ci'),
        ],
      ],
      [
        'check-prepared',
        [
          '--check-prepared',
          '--plugin',
          'session-relay',
          RELEASE_VERSION,
          '--source-commit',
          '1'.repeat(40),
          '--docks-red',
          ...pair('docks-red', 'a'),
          '--public-red',
          ...pair('public-red', 'b'),
          '--preflight',
          ...pair('preflight', 'c'),
          '--source-ci',
          ...pair('source-ci', 'd'),
          '--receipt-out',
          outPath('candidate'),
        ],
      ],
      [
        'bind-completion',
        [
          '--bind-completion',
          '--plugin',
          'session-relay',
          RELEASE_VERSION,
          '--finished-plan',
          inPath('finished-plan'),
          '--embedded-candidate-sha256',
          'e'.repeat(64),
          '--receipt-out',
          outPath('source-proof'),
        ],
      ],
    ];
    for (const [mode, args] of preparation) {
      runReleaseFixture(fixtureRoot.root, mode, 'valid', 'success', args);
    }

    for (const plugin of ['docks', 'effect-kit']) {
      for (const bump of ['patch', 'minor', 'major', '9.8.7']) {
        const report = runReleaseFixture(fixtureRoot.root, 'legacy-release', `${plugin}-${bump}`, 'success', [
          '--plugin',
          plugin,
          bump,
        ]);
        assert.equal(report.state.tag, `${plugin}--v${report.state.version}`);
        assert.ok(report.calls.some((call) => call.argv?.includes('scripts/ci.mjs')));
      }
    }

    const before = new Map([
      ['status', git(REPO, ['status', '--porcelain=v1', '--untracked-files=all'])],
      ...[
        '.claude-plugin/marketplace.json',
        'plugins/session-relay/.claude-plugin/plugin.json',
        'plugins/session-relay/.codex-plugin/plugin.json',
        'plugins/session-relay/rust/Cargo.toml',
        'plugins/session-relay/rust/Cargo.lock',
      ].map((relative) => [relative, fs.readFileSync(path.join(REPO, relative))]),
    ]);
    const dry = runReleaseFixture(fixtureRoot.root, 'prepare', 'dry-run', 'success', [
      '--prepare',
      '--plugin',
      'session-relay',
      RELEASE_VERSION,
      '--dry-run',
    ]);
    assert.equal(
      dry.calls.some((call) => /ci\.mjs/.test(JSON.stringify(call))),
      false,
      'dry-run executed CI',
    );
    assert.deepEqual(dry.mutations, []);
    assert.match(JSON.stringify(dry), /real release|changed tree|gate/i);
    assert.equal(git(REPO, ['status', '--porcelain=v1', '--untracked-files=all']), before.get('status'));
    for (const [relative, bytes] of before)
      if (relative !== 'status') assert.deepEqual(fs.readFileSync(path.join(REPO, relative)), bytes);

    const validPublish = [
      '--publish-reviewed',
      '--plugin',
      'session-relay',
      RELEASE_VERSION,
      '--source-proof',
      ...pair('source-proof', 'a'),
      '--receipt-out',
      outPath('grammar'),
    ];
    const malformedCases = new Map([
      ['unknown-argument', [...validPublish, '--unknown']],
      ['duplicate-argument', [...validPublish, '--plugin', 'session-relay']],
      ['missing-argument', ['--prepare', '--plugin']],
      [
        'orphan-receipt-digest',
        [
          '--publish-reviewed',
          '--plugin',
          'session-relay',
          RELEASE_VERSION,
          '--source-proof-sha256',
          'a'.repeat(64),
          '--receipt-out',
          outPath('orphan'),
        ],
      ],
      ['existing-output', validPublish],
      ['noncanonical-input', validPublish],
      ['wrong-input-digest', validPublish],
      ['receipt-unknown-field', validPublish],
      ['receipt-missing-field', validPublish],
    ]);
    for (const [scenario, args] of malformedCases) {
      if (scenario === 'existing-output') {
        fs.writeFileSync(outPath('grammar'), 'existing receipt must survive', { mode: 0o600 });
      }
      const report = runReleaseFixture(fixtureRoot.root, 'grammar', scenario, 'conflict', args);
      assert.deepEqual(report.mutations, [], `${scenario} mutated before rejection`);
      if (scenario === 'existing-output') {
        assert.equal(fs.readFileSync(outPath('grammar'), 'utf8'), 'existing receipt must survive');
        fs.rmSync(outPath('grammar'));
      }
    }
    const positionalRelay = runReleaseFixture(fixtureRoot.root, 'grammar', 'session-relay-positional', 'conflict', [
      '--plugin',
      'session-relay',
      RELEASE_VERSION,
    ]);
    assert.deepEqual(positionalRelay.mutations, []);
  } finally {
    if (fixtureRoot.owned) fs.rmSync(fixtureRoot.root, { recursive: true, force: true });
  }
}
check(
  'release prepare, evidence, publication, promotion, recovery, finalization, dry-run, and legacy grammar are fixture-complete',
  releaseContracts,
);

function verifyCompanion() {
  if (!cli.publicRemote) return;
  assert.equal(cli.publicRemote, 'https://github.com/DocksDocks/public.git');
  assert.match(cli.publicRef, PUBLIC_REF);
  assert.equal(
    cli.publicRef,
    `refs/heads/preflight/session-relay-cli-${RELEASE_VERSION}-${cli.publicCommit.slice(0, 12)}`,
    'companion validation ref must be derived from the exact public commit',
  );
  const result = run(process.execPath, [
    COMPANION,
    '--public-remote',
    cli.publicRemote,
    '--public-ref',
    cli.publicRef,
    '--public-commit',
    cli.publicCommit,
    '--detached-clone',
  ]);
  assert.equal(result.status, 0, result.stderr || result.stdout);
}
check('companion validation ref is a clean detached, receipt-bound installer contract', verifyCompanion);

process.stdout.write(`\nPASS: session-relay distribution contract — ${passed} checks\n`);
