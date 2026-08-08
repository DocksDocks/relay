#!/usr/bin/env node
// gate.mjs — the whole gate for this repository. Run it before pushing and before tagging.
// Eight phases run in a fixed order: manifests, skill, shell, rust, delegation, checks,
// selftest, javascript. The first failure names its phase and exits 1, so a red run always
// reports the earliest cause rather than a cascade.
// Usage: node scripts/gate.mjs
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const REPO = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..');
process.chdir(REPO);

const CASES = [
  'bus_smoke',
  'fanout',
  'fanout_reap',
  'lifecycle_admission',
  'lifecycle_managed',
  'lifecycle_release',
  'lifecycle_supervisor',
  'lock_race',
  'protocol',
  'workspace_coordination_process',
  'workspace_identity',
  'workspace_lease_process',
  'workspace_resources',
  'unit',
];

const PAYLOAD_ALLOWED_SKILLS = './skills/';
const SKILL_PATH = 'plugin/skills/productivity/session-relay/SKILL.md';
const SKILL_DESCRIPTION_LIMIT = 1024;
const SKILL_BODY_LINE_LIMIT = 500;

let activePhase = null;
// The delegation phase may hand back a closure that releases the cgroup leaf it created.
// It is consumed exactly once, by whichever of `fail()` or the normal end of the run gets
// there first, so a failing gate still gives the runner its cgroup subtree back.
let releaseDelegationLeaf = null;

const ok = (message) => console.log(`\x1b[1;32m  ✔\x1b[0m ${message}`);
const warn = (message) => console.log(`\x1b[1;33m  ⚠\x1b[0m ${message}`);
const section = (name) => {
  activePhase = name;
  console.log(`\n\x1b[1m▸ ${name}\x1b[0m`);
};

function takeDelegationRelease() {
  const release = releaseDelegationLeaf;
  releaseDelegationLeaf = null;
  return release === null ? null : release();
}

function fail(message) {
  console.log(`\x1b[1;31m  ✘\x1b[0m ${message}`);
  console.log(`\x1b[1;31mgate failed in phase: ${activePhase}\x1b[0m`);
  const releaseDetail = takeDelegationRelease();
  if (releaseDetail !== null) console.log(`\x1b[1;31m  ✘\x1b[0m ${releaseDetail}`);
  process.exit(1);
}

const run = (argv, options = {}) => spawnSync(argv[0], argv.slice(1), { cwd: REPO, ...options });

const failed = (result) => Boolean(result.error) || result.signal !== null || (result.status ?? 1) !== 0;

const detailOf = (result) =>
  result.error?.message || result.stderr?.toString().trim() || result.signal || `exit ${result.status}`;

function readJSON(file) {
  let text;
  try {
    text = fs.readFileSync(path.join(REPO, file), 'utf8');
  } catch (error) {
    fail(`${file} is unreadable: ${error.message}`);
  }
  try {
    return JSON.parse(text);
  } catch (error) {
    fail(`${file} is not valid JSON: ${error.message}`);
  }
}

// ── 1. manifests ────────────────────────────────────────────────────────────────────
section('manifests');
{
  readJSON('plugin/.claude-plugin/plugin.json');
  ok('plugin/.claude-plugin/plugin.json parses');

  const codexManifest = readJSON('plugin/.codex-plugin/plugin.json');
  const declaredSkills = codexManifest.skills;
  if (declaredSkills !== PAYLOAD_ALLOWED_SKILLS) {
    const found = JSON.stringify(declaredSkills);
    fail(`plugin/.codex-plugin/plugin.json skills must be '${PAYLOAD_ALLOWED_SKILLS}' (found ${found})`);
  }
  ok(`plugin/.codex-plugin/plugin.json parses and declares skills '${PAYLOAD_ALLOWED_SKILLS}'`);

  readJSON('plugin/hooks/codex-hooks.json');
  readJSON('plugin/.codex-plugin/bus.mcp.json');
  ok('plugin/hooks/codex-hooks.json and plugin/.codex-plugin/bus.mcp.json parse');

  const soleEntry = (catalog, file) => {
    const entries = catalog.plugins;
    if (!Array.isArray(entries) || entries.length !== 1) {
      fail(`${file} must list exactly one plugin (found ${Array.isArray(entries) ? entries.length : 'no array'})`);
    }
    if (entries[0].name !== 'session-relay') {
      fail(`${file} plugin name must be 'session-relay' (found ${JSON.stringify(entries[0].name)})`);
    }
    return entries[0];
  };

  const claudeEntry = soleEntry(readJSON('.claude-plugin/marketplace.json'), '.claude-plugin/marketplace.json');
  if (claudeEntry.source !== './plugin') {
    fail(`.claude-plugin/marketplace.json source must be './plugin' (found ${JSON.stringify(claudeEntry.source)})`);
  }
  ok(".claude-plugin/marketplace.json lists session-relay at source './plugin'");

  const codexEntry = soleEntry(readJSON('.agents/plugins/marketplace.json'), '.agents/plugins/marketplace.json');
  const codexSource = codexEntry.source;
  const codexSourceMatches =
    codexSource !== null &&
    typeof codexSource === 'object' &&
    !Array.isArray(codexSource) &&
    Object.keys(codexSource).length === 2 &&
    codexSource.source === 'local' &&
    codexSource.path === './plugin';
  if (!codexSourceMatches) {
    const found = JSON.stringify(codexSource);
    fail(`.agents/plugins/marketplace.json source must be {"source":"local","path":"./plugin"} (found ${found})`);
  }
  ok('.agents/plugins/marketplace.json lists session-relay at the local ./plugin source');
}

// ── 2. skill ────────────────────────────────────────────────────────────────────────
section('skill');
{
  // A minimal frontmatter reader, deliberately: the gate needs four scalar facts, and a YAML
  // dependency would be a runtime the release path does not otherwise carry. Top-level
  // `key: value` lines are read; indented continuation lines belong to a nested map and are
  // skipped, because no check below reads one.
  const readFrontmatter = (file) => {
    const text = fs.readFileSync(path.join(REPO, file), 'utf8');
    const lines = text.split('\n');
    if (lines[0].trim() !== '---') fail(`${file} does not open with a '---' frontmatter delimiter`);
    const closing = lines.findIndex((line, index) => index > 0 && line.trim() === '---');
    if (closing === -1) fail(`${file} frontmatter is never closed by a '---' delimiter`);
    const fields = new Map();
    for (const line of lines.slice(1, closing)) {
      if (line.trim() === '' || line.startsWith(' ') || line.startsWith('\t') || line.startsWith('#')) continue;
      const match = /^([A-Za-z_][A-Za-z0-9_-]*):[ \t]*(.*)$/.exec(line);
      if (match === null) fail(`${file} frontmatter line is not a 'key: value' pair: ${line}`);
      const [, key, rawValue] = match;
      const value = rawValue.trim();
      if (value.startsWith('"') && value.endsWith('"') && value.length >= 2) {
        try {
          fields.set(key, JSON.parse(value));
        } catch (error) {
          fail(`${file} frontmatter key '${key}' is not a decodable quoted string: ${error.message}`);
        }
      } else if (value.startsWith("'") && value.endsWith("'") && value.length >= 2) {
        fields.set(key, value.slice(1, -1).replaceAll("''", "'"));
      } else {
        fields.set(key, value);
      }
    }
    const body = lines.slice(closing + 1);
    while (body.length > 0 && body[body.length - 1] === '') body.pop();
    return { fields, bodyLines: body.length };
  };

  const { fields, bodyLines } = readFrontmatter(SKILL_PATH);
  ok(`${SKILL_PATH} frontmatter parses`);

  const name = fields.get('name');
  const directory = path.basename(path.dirname(SKILL_PATH));
  if (name !== 'session-relay') fail(`${SKILL_PATH} name must be 'session-relay' (found ${JSON.stringify(name)})`);
  if (name !== directory) fail(`${SKILL_PATH} name '${name}' does not equal its directory name '${directory}'`);
  ok(`skill name '${name}' equals its directory name`);

  const description = fields.get('description');
  if (typeof description !== 'string' || description.length === 0) {
    fail(`${SKILL_PATH} has no description`);
  }
  if (description.length > SKILL_DESCRIPTION_LIMIT) {
    fail(`${SKILL_PATH} description is ${description.length} characters, above the ${SKILL_DESCRIPTION_LIMIT} limit`);
  }
  ok(`skill description is ${description.length} characters (limit ${SKILL_DESCRIPTION_LIMIT})`);

  if (bodyLines > SKILL_BODY_LINE_LIMIT) {
    fail(`${SKILL_PATH} body is ${bodyLines} lines, above the ${SKILL_BODY_LINE_LIMIT} limit`);
  }
  ok(`skill body is ${bodyLines} lines (limit ${SKILL_BODY_LINE_LIMIT})`);
}

// ── 3. shell ────────────────────────────────────────────────────────────────────────
section('shell');
{
  const launcher = 'plugin/bin/relay';
  const shellcheck = run(['shellcheck', '-S', 'warning', launcher], { encoding: 'utf8' });
  if (shellcheck.error) warn('shellcheck not installed — skipped locally (CI enforces)');
  else if (failed(shellcheck)) fail(`shellcheck warnings (run: shellcheck -S warning ${launcher})`);
  else ok(`shellcheck -S warning clean (${launcher})`);
}

// ── 4. rust ─────────────────────────────────────────────────────────────────────────
section('rust');
const privateBinaryDirs = new Set();
process.on('exit', () => {
  // The private copy is scratch. Without this sweep every run would strand a
  // `.gate-binary-*` directory inside target/release/ forever.
  for (const created of privateBinaryDirs) {
    try {
      fs.rmSync(created, { force: true, recursive: true });
    } catch {
      // Best effort: a swept-away or read-only scratch directory must never fail the gate.
    }
  }
});

const RUST_BINARY = (() => {
  const cargo = process.env.CARGO ?? 'cargo';
  const cargoRun = (args) => run([cargo, ...args], { encoding: 'utf8', stdio: 'inherit' });

  const formatted = cargoRun(['fmt', '--check']);
  if (formatted.error) fail(`cargo not found — the Rust source build is required (${detailOf(formatted)})`);
  if (failed(formatted)) fail('cargo fmt --check failed (run: cargo fmt)');
  ok('cargo fmt --check clean');

  if (failed(cargoRun(['clippy', '--release', '--all-targets', '--locked', '--', '-D', 'warnings']))) {
    fail('cargo clippy failed (run: cargo clippy --release --all-targets --locked -- -D warnings)');
  }
  ok('cargo clippy --release --all-targets --locked -D warnings clean');

  if (failed(cargoRun(['build', '--release', '--locked']))) {
    fail('release build failed (run: cargo build --release --locked)');
  }

  // CARGO_TARGET_DIR is relative to cargo's working directory, which is the repository root here.
  const targetDir = process.env.CARGO_TARGET_DIR;
  const built =
    typeof targetDir === 'string' && targetDir.length > 0
      ? path.resolve(REPO, targetDir, 'release', 'relay')
      : path.resolve(REPO, 'target', 'release', 'relay');
  try {
    if (!fs.statSync(built).isFile()) throw new Error('not a regular file');
    fs.accessSync(built, fs.constants.X_OK);
  } catch {
    fail(`release build did not produce the executable ${built}`);
  }

  // Privatize the artifact: every later phase reads this copy, never target/release/relay
  // itself. A concurrent rebuild, or a test that writes through its own binary path, would
  // otherwise swap the bytes out from under validation and the gate would grade a different
  // executable than the one it built.
  const privateDir = fs.mkdtempSync(path.join(path.dirname(built), '.gate-binary-'));
  privateBinaryDirs.add(privateDir);
  const privateBinary = path.join(privateDir, path.basename(built));
  fs.copyFileSync(built, privateBinary, fs.constants.COPYFILE_EXCL);
  fs.chmodSync(privateBinary, 0o755);
  ok(`source-built host executable ready --release --locked: ${built} → private ${privateBinary}`);
  return privateBinary;
})();

// ── 5. delegation ───────────────────────────────────────────────────────────────────
section('delegation');
{
  const configured = process.env.SESSION_RELAY_TEST_CGROUP_ROOT;
  if (configured) {
    let canonical;
    try {
      canonical = fs.realpathSync(configured);
      const stat = fs.statSync(canonical);
      if (!stat.isDirectory() || stat.uid !== process.getuid()) throw new Error('not an owned directory');
    } catch (error) {
      fail(`cgroup delegation is invalid: ${error.message}`);
    }
    if (canonical !== path.resolve(configured)) fail('cgroup delegation must be a canonical path');
    process.env.SESSION_RELAY_TEST_CGROUP_ROOT = canonical;
    ok(`cgroup delegation honoured from the environment: ${canonical}`);
  } else if (process.env.GITHUB_ACTIONS !== 'true') {
    // Off CI the variable stays unset, so test/rust-test-inventory.mjs finds its own
    // `systemd-run --user` scope instead.
    ok('no cgroup delegation prepared off CI; the inventory finds its own systemd-run scope');
  } else {
    const uid = process.getuid();
    const gid = process.getgid();
    const leaf =
      `/sys/fs/cgroup/session-relay-test-${uid}-` +
      `${process.env.GITHUB_RUN_ID ?? 'run'}-${process.env.GITHUB_RUN_ATTEMPT ?? 'attempt'}-${process.pid}`;
    const sudo = (args) => run(['sudo', '-n', ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });

    const created = sudo(['mkdir', '-p', leaf]);
    if (failed(created)) fail(`cgroup delegation could not be created: ${detailOf(created)}`);
    const owned = sudo(['chown', `${uid}:${gid}`, leaf]);
    if (failed(owned)) {
      sudo(['rmdir', leaf]);
      fail(`cgroup delegation could not be delegated: ${detailOf(owned)}`);
    }
    process.env.SESSION_RELAY_TEST_CGROUP_ROOT = leaf;
    releaseDelegationLeaf = () => {
      delete process.env.SESSION_RELAY_TEST_CGROUP_ROOT;
      const removed = sudo(['rmdir', leaf]);
      // A leaf that will not close is a leaked cgroup on a shared runner: the gate fails.
      return failed(removed) ? `cgroup delegation did not cleanly close: ${detailOf(removed)}` : null;
    };
    ok(`cgroup delegation prepared for CI: ${leaf}`);
  }
}

try {
  // ── 6. checks ─────────────────────────────────────────────────────────────────────
  section('checks');
  {
    const childEnv = { ...process.env, SESSION_RELAY_TEST_BIN: RUST_BINARY };
    const invocations = [
      ...CASES.map((name) => ['test/rust-test-inventory.mjs', '--case', name]),
      ['test/reentry-inventory.mjs'],
      ['test/workspace-smoke.mjs', '--case', 'single-session-compat', '--bin', RUST_BINARY],
      ['test/workspace-smoke.mjs', '--case', 'docs-contract', '--bin', RUST_BINARY],
      ['test/distribution-contract.mjs'],
    ];
    for (const argv of invocations) {
      const label = argv.join(' ');
      const outcome = run(['node', ...argv], { env: childEnv, stdio: 'inherit' });
      if (failed(outcome)) fail(`check failed (run: node ${label})`);
      ok(`check passed (${label})`);
    }
  }

  // ── 7. selftest ───────────────────────────────────────────────────────────────────
  section('selftest');
  {
    const baseEnv = { ...process.env, SESSION_RELAY_TEST_BIN: RUST_BINARY };
    const selftest = (jobs) =>
      run(['node', 'test/selftest.mjs'], {
        encoding: 'utf8',
        env: { ...baseEnv, SESSION_RELAY_TEST_JOBS: jobs },
      });
    const jobsOne = selftest('1');
    const jobsFour = selftest('4');
    const crashed = [
      ['jobs-1', jobsOne],
      ['jobs-4', jobsFour],
    ].filter(([, outcome]) => failed(outcome));
    if (crashed.length > 0) {
      for (const [label, outcome] of crashed) {
        const detail = `${outcome.stdout ?? ''}${outcome.stderr ?? ''}`.trim();
        console.error(`${label} exited ${outcome.status ?? 'null'}${detail ? `:\n${detail}` : ' with no output'}`);
      }
      fail(
        `self-test failed (${crashed.map(([label]) => label).join(', ')}) ` +
          `(run twice with SESSION_RELAY_TEST_BIN=${RUST_BINARY} and SESSION_RELAY_TEST_JOBS=1|4)`,
      );
    }
    if (jobsOne.stdout !== jobsFour.stdout) {
      const left = (jobsOne.stdout ?? '').split('\n');
      const right = (jobsFour.stdout ?? '').split('\n');
      const firstDiff = left.findIndex((line, index) => line !== right[index]);
      const at = firstDiff === -1 ? Math.min(left.length, right.length) : firstDiff;
      console.error(
        `jobs-1 (${left.length} lines) vs jobs-4 (${right.length} lines) diverged at line ${at + 1}:\n` +
          `- ${left[at] ?? '<eof>'}\n+ ${right[at] ?? '<eof>'}`,
      );
      fail('self-test jobs-1/jobs-4 output drifted; the scenario set is not scheduling-independent');
    }
    ok('self-test passed with byte-identical jobs-1/jobs-4 output');
  }

  // ── 8. javascript ─────────────────────────────────────────────────────────────────
  section('javascript');
  {
    const biome = run(['pnpm', 'exec', 'biome', 'ci', 'scripts', 'test', 'package.json', 'biome.json'], {
      stdio: 'inherit',
    });
    if (failed(biome)) fail('biome ci failed (run: pnpm exec biome ci scripts test package.json biome.json)');
    ok('biome ci clean');
  }
} finally {
  const releaseDetail = takeDelegationRelease();
  if (releaseDetail !== null) {
    activePhase = 'delegation';
    fail(releaseDetail);
  }
}

console.log('\n\x1b[1;32mgate passed\x1b[0m');
