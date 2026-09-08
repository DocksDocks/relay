#!/usr/bin/env node
// Distribution contract for the standalone session-relay repository.
//
// This file replaces the large monorepo contract suite that guarded release
// evidence, promotion, and publication records. Those records no longer exist
// here, so this suite keeps only the four properties that still bind what a
// consumer receives.
//
// 1. Launcher resolution. The suite executes `plugin/bin/relay` in child
//    processes with controlled environments. The launcher is the only entry
//    point a consumer installs, so its order, its hard failures, and its
//    self-recursion refusal must stay behavioural facts, not prose.
// 2. Version lockstep. Three files declare the shipped version. A consumer sees
//    a coherent product only when all three agree.
// 3. Asset-set closure. The release workflow must build exactly two Linux musl
//    targets and stage exactly three assets. The forbidden-token assertion
//    stops a non-Linux leg from reappearing.
// 4. Payload boundary. Only allowlisted entries may ship inside `plugin/`,
//    because an installer copies that directory into every consumer cache.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import url from 'node:url';
import { resolveShippedRelayVersion } from './version.mjs';

const REPO = path.resolve(path.dirname(url.fileURLToPath(import.meta.url)), '..');
const LAUNCHER = path.join(REPO, 'plugin', 'bin', 'relay');
const SEMVER = /^\d+\.\d+\.\d+$/;
const BASE_PATH = '/usr/bin:/bin';

function pass(name, detail) {
  process.stdout.write(`PASS ${name} ${detail}\n`);
}

function readJson(relative) {
  return JSON.parse(fs.readFileSync(path.join(REPO, relative), 'utf8'));
}

// --- 1. Launcher resolution -------------------------------------------------

function writeStub(file, marker) {
  fs.writeFileSync(
    file,
    [
      '#!/bin/sh',
      `printf 'MARKER=%s\\n' ${JSON.stringify(marker)}`,
      'for argument in "$@"; do',
      `  printf 'ARG=%s\\n' "$argument"`,
      'done',
      '',
    ].join('\n'),
    { mode: 0o755 },
  );
}

function runLauncher(env, args = []) {
  return spawnSync('sh', [LAUNCHER, ...args], {
    cwd: REPO,
    encoding: 'utf8',
    env,
  });
}

function checkLauncher() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'relay-distribution-'));
  try {
    const explicitDir = path.join(root, 'explicit');
    const pathDir = path.join(root, 'path');
    const emptyDir = path.join(root, 'empty');
    const home = path.join(root, 'home');
    const homeBin = path.join(home, '.local', 'bin');
    for (const directory of [explicitDir, pathDir, emptyDir, homeBin]) {
      fs.mkdirSync(directory, { recursive: true });
    }

    const explicit = path.join(explicitDir, 'explicit-relay');
    const onPath = path.join(pathDir, 'session-relay');
    const inHome = path.join(homeBin, 'session-relay');
    writeStub(explicit, 'explicit');
    writeStub(onPath, 'path');
    writeStub(inHome, 'home');

    const fullPath = `${pathDir}${path.delimiter}${BASE_PATH}`;
    const barePath = `${emptyDir}${path.delimiter}${BASE_PATH}`;

    // (a) An executable SESSION_RELAY_BIN wins, and argv passes through verbatim.
    const argv = ['send', 'agent', '--', 'a b', '--flag'];
    const explicitRun = runLauncher({ HOME: home, PATH: fullPath, SESSION_RELAY_BIN: explicit }, argv);
    assert.equal(explicitRun.status, 0, explicitRun.stderr);
    assert.deepEqual(
      explicitRun.stdout.split('\n').filter(Boolean),
      ['MARKER=explicit', ...argv.map((argument) => `ARG=${argument}`)],
      'SESSION_RELAY_BIN must win and receive argv verbatim',
    );
    pass('launcher-explicit-override', 'SESSION_RELAY_BIN wins and passes argv verbatim');

    // (b) A non-executable SESSION_RELAY_BIN is a hard failure with no fallback.
    const notExecutable = path.join(explicitDir, 'not-executable');
    fs.writeFileSync(notExecutable, '#!/bin/sh\nprintf MARKER=bad\n', { mode: 0o644 });
    const invalidRun = runLauncher({ HOME: home, PATH: fullPath, SESSION_RELAY_BIN: notExecutable });
    assert.equal(invalidRun.status, 1, 'a non-executable override must exit 1');
    assert.match(invalidRun.stderr, /SESSION_RELAY_BIN is not an executable file/);
    assert.equal(invalidRun.stdout, '', 'a non-executable override must not fall back to PATH or to HOME');
    pass('launcher-invalid-override', 'a non-executable SESSION_RELAY_BIN fails hard without fallback');

    // (c) Without an override, PATH resolves the executable.
    const pathRun = runLauncher({ HOME: home, PATH: fullPath });
    assert.equal(pathRun.status, 0, pathRun.stderr);
    assert.match(pathRun.stdout, /^MARKER=path\n/);
    pass('launcher-path-resolution', 'PATH resolves session-relay when no override exists');

    // (d) Without an override and with nothing on PATH, HOME resolves it.
    const homeRun = runLauncher({ HOME: home, PATH: barePath });
    assert.equal(homeRun.status, 0, homeRun.stderr);
    assert.match(homeRun.stdout, /^MARKER=home\n/);
    pass('launcher-home-resolution', '$HOME/.local/bin/session-relay resolves as the last candidate');

    // (e) A launcher that resolves to itself is refused.
    const firstLink = path.join(root, 'relay-link-1');
    const secondLink = path.join(root, 'relay-link-2');
    fs.symlinkSync(LAUNCHER, firstLink);
    fs.symlinkSync(firstLink, secondLink);
    const recursionRun = runLauncher({ HOME: home, PATH: fullPath, SESSION_RELAY_BIN: secondLink });
    assert.notEqual(recursionRun.status, 0, 'a self-resolving launcher must fail');
    assert.match(recursionRun.stderr, /refusing to execute itself recursively/);
    pass('launcher-recursion-refusal', 'a symlink chain back to the launcher is refused');

    // (f) Nothing found anywhere exits 1 with the not-found message.
    fs.rmSync(inHome);
    const missingRun = runLauncher({ HOME: home, PATH: barePath });
    assert.equal(missingRun.status, 1, 'a missing executable must exit 1');
    assert.match(missingRun.stderr, /session-relay executable not found\./);
    pass('launcher-not-found', 'an unresolvable executable exits 1 with the not-found message');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

// --- 2. Version lockstep ----------------------------------------------------

function checkVersionLockstep() {
  const shipped = resolveShippedRelayVersion(REPO);
  const cargo = fs.readFileSync(path.join(REPO, 'Cargo.toml'), 'utf8').match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  const marketplace = readJson('.omp-plugin/marketplace.json').plugins.find(
    ({ name }) => name === 'session-relay',
  )?.version;
  const declarations = [
    { file: 'Cargo.toml', version: cargo },
    { file: 'plugin/package.json', version: shipped.version },
    { file: '.omp-plugin/marketplace.json', version: marketplace },
  ];

  for (const { file, version } of declarations) {
    assert.match(version ?? '', SEMVER, `${file} must declare a semver version, found ${JSON.stringify(version)}`);
  }

  const counts = new Map();
  for (const { version } of declarations) counts.set(version, (counts.get(version) ?? 0) + 1);
  const [majority] = [...counts.entries()].sort((left, right) => right[1] - left[1])[0];
  const disagreeing = declarations.filter(({ version }) => version !== majority);
  assert.deepEqual(
    disagreeing,
    [],
    `the shipped version must be identical in all three files; majority is ${majority} and these disagree: ${disagreeing
      .map(({ file, version }) => `${file}=${version}`)
      .join(', ')}`,
  );
  pass('version-lockstep', declarations.map(({ file, version }) => `${file}=${version}`).join(', '));
}

// --- 3. Asset-set closure ---------------------------------------------------

function jobSection(document, job) {
  const lines = document.split('\n');
  const start = lines.indexOf(`  ${job}:`);
  assert.notEqual(start, -1, `.github/workflows/release.yml must define the ${job} job`);
  let end = lines.length;
  for (let index = start + 1; index < lines.length; index += 1) {
    if (/^ {2}\S/.test(lines[index])) {
      end = index;
      break;
    }
  }
  return lines.slice(start, end).join('\n');
}

function checkAssetSet() {
  const workflow = path.join(REPO, '.github', 'workflows', 'release.yml');
  const document = fs.readFileSync(workflow, 'utf8');

  const targets = [...new Set(document.match(/[A-Za-z0-9_]+-unknown-linux-musl/g) ?? [])].sort();
  assert.deepEqual(
    targets,
    ['aarch64-unknown-linux-musl', 'x86_64-unknown-linux-musl'],
    'the release workflow must build exactly the two Linux musl targets',
  );

  const publish = jobSection(document, 'publish');
  const assets = [...new Set(publish.match(/session-relay-(?:x86_64|aarch64)[A-Za-z0-9_.-]*/g) ?? [])].sort();
  assert.deepEqual(
    assets,
    ['session-relay-aarch64-unknown-linux-musl', 'session-relay-x86_64-unknown-linux-musl'],
    'the publish job must stage exactly the two binary assets',
  );
  assert.match(publish, /\bSHA256SUMS\b/, 'the publish job must stage SHA256SUMS beside the two binaries');
  assert.equal(assets.length + 1, 3, 'the publish job must stage exactly three assets');

  const forbidden = document.match(/darwin|apple|windows|msvc|-gnu\b/gi) ?? [];
  assert.deepEqual(forbidden, [], `the release workflow must name no non-Linux target: ${forbidden.join(', ')}`);
  pass('asset-set-closure', 'two musl targets, three staged assets, no non-Linux token');
}

// --- 4. Payload boundary ----------------------------------------------------

const PAYLOAD_ALLOWLIST = new Set(['package.json', 'extension', 'skills', 'bin', 'README.md', 'AGENTS.md', 'LICENSE']);

function checkPayloadBoundary() {
  const listed = spawnSync('git', ['ls-files', 'plugin'], { cwd: REPO, encoding: 'utf8' });
  assert.equal(listed.status, 0, `git ls-files plugin must succeed: ${listed.stderr}`);
  const tracked = listed.stdout.split('\n').filter(Boolean);
  assert.ok(tracked.length > 0, 'the payload must contain tracked files');

  const offending = tracked.filter((file) => {
    const segments = file.split('/');
    return segments[0] !== 'plugin' || !PAYLOAD_ALLOWLIST.has(segments[1] ?? '');
  });
  assert.deepEqual(offending, [], `these payload paths are outside the allowlist: ${offending.join(', ')}`);
  pass(
    'payload-boundary',
    `${tracked.length} tracked payload paths stay inside the allowlist: ${[...PAYLOAD_ALLOWLIST].join(', ')}`,
  );
}

checkLauncher();
checkVersionLockstep();
checkAssetSet();
checkPayloadBoundary();
