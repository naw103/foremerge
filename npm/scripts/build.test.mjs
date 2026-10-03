// Tests for npm/scripts/build.mjs against release-shaped fixtures.
//
//   node --test npm/scripts/build.test.mjs
//
// Needs `git`, `tar`, and `zip`, so CI runs it on Linux and macOS.

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { TARGETS, writeAssets } from './fixtures.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const build = path.join(here, 'build.mjs');
const launcher = JSON.parse(fs.readFileSync(path.join(here, '..', 'foremerge', 'package.json'), 'utf8'));
const version = launcher.version;

const PACKAGES = {
  'aarch64-apple-darwin': { name: 'foremerge-darwin-arm64', os: 'darwin', cpu: 'arm64' },
  'x86_64-apple-darwin': { name: 'foremerge-darwin-x64', os: 'darwin', cpu: 'x64' },
  'aarch64-unknown-linux-gnu': { name: 'foremerge-linux-arm64', os: 'linux', cpu: 'arm64', libc: 'glibc' },
  'x86_64-unknown-linux-gnu': { name: 'foremerge-linux-x64', os: 'linux', cpu: 'x64', libc: 'glibc' },
  'x86_64-pc-windows-msvc': { name: 'foremerge-win32-x64', os: 'win32', cpu: 'x64' },
};

function scratch() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'foremerge-npm-build-'));
}

function runBuild(args) {
  return spawnSync(process.execPath, [build, ...args], { encoding: 'utf8' });
}

function sha256(file) {
  return createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

test('a fixture release builds six private packages with the release binaries', () => {
  const root = scratch();
  const assets = path.join(root, 'assets');
  const out = path.join(root, 'out');
  writeAssets(assets, version);
  const result = runBuild(['--allow-untagged', '--assets', assets, '--out', out]);
  assert.equal(result.status, 0, result.stderr);

  for (const { triple, platform } of TARGETS) {
    const expected = PACKAGES[triple];
    const dir = path.join(out, expected.name);
    const manifest = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8'));
    assert.equal(manifest.name, expected.name);
    assert.equal(manifest.version, version);
    assert.deepEqual(manifest.os, [expected.os]);
    assert.deepEqual(manifest.cpu, [expected.cpu]);
    assert.deepEqual(manifest.libc, expected.libc ? [expected.libc] : undefined);
    assert.deepEqual(manifest.files, ['bin']);
    assert.equal(manifest.private, true, 'an untagged build must not be publishable');
    assert.equal(manifest.bin, undefined, 'only the launcher puts commands on PATH');
    assert.equal(manifest.scripts, undefined, 'platform packages run nothing on install');

    const suffix = platform === 'win32' ? '.exe' : '';
    for (const program of ['foremerge', 'fmg']) {
      const binary = path.join(dir, 'bin', `${program}${suffix}`);
      assert.equal(fs.statSync(binary).mode & 0o777, 0o755, binary);
      assert.match(fs.readFileSync(binary, 'utf8'), new RegExp(`^#!/bin/sh\\necho "${program} ${triple}`));
    }

    // What npm would publish: the binaries and the package's own files only.
    const [packed] = JSON.parse(execFileSync('npm', ['pack', '--dry-run', '--json', dir], { encoding: 'utf8' }));
    assert.deepEqual(
      packed.files.map((file) => file.path).sort(),
      ['LICENSE', 'README.md', `bin/fmg${suffix}`, `bin/foremerge${suffix}`, 'package.json'],
    );
  }

  const built = JSON.parse(fs.readFileSync(path.join(out, 'foremerge', 'package.json'), 'utf8'));
  assert.equal(built.private, true);
  assert.deepEqual(built.optionalDependencies, launcher.optionalDependencies);
  const [packed] = JSON.parse(execFileSync('npm', ['pack', '--dry-run', '--json', path.join(out, 'foremerge')], { encoding: 'utf8' }));
  assert.deepEqual(
    packed.files.map((file) => file.path).sort(),
    ['LICENSE', 'README.md', 'bin/fmg.js', 'bin/foremerge.js', 'lib/run.js', 'package.json'],
    'tests and other checkout files stay out of the published launcher',
  );
  // A published launcher must keep its scripts free of install hooks.
  assert.deepEqual(Object.keys(built.scripts || {}).filter((name) => /install/.test(name)), []);
});

test('an archive that does not match its published digest is refused', () => {
  const root = scratch();
  const assets = path.join(root, 'assets');
  writeAssets(assets, version);
  const archive = path.join(assets, `foremerge-v${version}-x86_64-unknown-linux-gnu.tar.gz`);
  fs.appendFileSync(archive, 'tampered');
  const result = runBuild(['--allow-untagged', '--assets', assets, '--out', path.join(root, 'out')]);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /x86_64-unknown-linux-gnu\.tar\.gz: SHA-256 [0-9a-f]{64} does not match/);
  assert.ok(!fs.existsSync(path.join(root, 'out', 'foremerge-linux-x64', 'bin')), 'nothing from it is unpacked');
});

test('a digest file naming a different archive is refused', () => {
  const root = scratch();
  const assets = path.join(root, 'assets');
  writeAssets(assets, version);
  const digestFile = path.join(assets, `foremerge-v${version}-aarch64-apple-darwin.tar.gz.sha256`);
  const archive = path.join(assets, `foremerge-v${version}-aarch64-apple-darwin.tar.gz`);
  fs.writeFileSync(digestFile, `${sha256(archive)}  something-else.tar.gz\n`);
  const result = runBuild(['--allow-untagged', '--assets', assets, '--out', path.join(root, 'out')]);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /names something-else\.tar\.gz/);
});

test('an archive missing one of the two programs is refused', () => {
  const root = scratch();
  const assets = path.join(root, 'assets');
  writeAssets(assets, version);
  const name = `foremerge-v${version}-x86_64-apple-darwin.tar.gz`;
  const stage = fs.mkdtempSync(path.join(root, 'stage-'));
  fs.writeFileSync(path.join(stage, 'foremerge'), '#!/bin/sh\n');
  execFileSync('tar', ['-C', stage, '-czf', path.join(assets, name), 'foremerge']);
  fs.writeFileSync(path.join(assets, `${name}.sha256`), `${sha256(path.join(assets, name))}  ${name}\n`);
  const result = runBuild(['--allow-untagged', '--assets', assets, '--out', path.join(root, 'out')]);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /has no fmg/);
});

test('without --allow-untagged the build must run from the release tag', () => {
  const root = scratch();
  const result = runBuild(['--version', '999.0.0', '--out', path.join(root, 'out')]);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /no v999\.0\.0 tag/);
  assert.ok(!fs.existsSync(path.join(root, 'out')), 'nothing is written');
});

test('a version the launcher does not pin is refused', () => {
  const root = scratch();
  const assets = path.join(root, 'assets');
  writeAssets(assets, '999.0.0');
  const result = runBuild(['--allow-untagged', '--version', '999.0.0', '--assets', assets, '--out', path.join(root, 'out')]);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, new RegExp(`npm/foremerge/package.json is ${version.replace(/\./g, '\\.')}, building 999\\.0\\.0`));
});

test('--out refuses a directory this script did not write, and leaves it alone', () => {
  const root = scratch();
  const assets = path.join(root, 'assets');
  writeAssets(assets, version);
  const precious = path.join(root, 'precious');
  fs.mkdirSync(precious);
  fs.writeFileSync(path.join(precious, 'sentinel'), 'keep me');
  const result = runBuild(['--allow-untagged', '--assets', assets, '--out', precious]);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /already holds files this script did not write/);
  assert.equal(fs.readFileSync(path.join(precious, 'sentinel'), 'utf8'), 'keep me');
  assert.deepEqual(fs.readdirSync(precious), ['sentinel']);
});

test('--out refuses the checkout and anything containing it', () => {
  const root = scratch();
  const assets = path.join(root, 'assets');
  writeAssets(assets, version);
  const checkout = path.resolve(here, '..', '..');
  for (const out of [checkout, path.dirname(checkout)]) {
    const result = runBuild(['--allow-untagged', '--assets', assets, '--out', out]);
    assert.notEqual(result.status, 0, out);
    assert.match(result.stderr, /contains the checkout/, out);
  }
  const inside = path.join(here, '..', 'foremerge', 'out');
  const result = runBuild(['--allow-untagged', '--assets', assets, '--out', inside]);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /inside npm\/foremerge/);
  assert.ok(!fs.existsSync(inside));
});

test('--out replaces its own earlier output', () => {
  const root = scratch();
  const assets = path.join(root, 'assets');
  const out = path.join(root, 'out');
  writeAssets(assets, version);
  assert.equal(runBuild(['--allow-untagged', '--assets', assets, '--out', out]).status, 0);
  fs.writeFileSync(path.join(out, 'stale'), 'from an earlier run');
  const again = runBuild(['--allow-untagged', '--assets', assets, '--out', out]);
  assert.equal(again.status, 0, again.stderr);
  assert.ok(!fs.existsSync(path.join(out, 'stale')));
});

// A throwaway repository holding just what the build reads, committed and
// tagged the way a release is, so release mode runs for real.
function taggedCheckout() {
  const root = scratch();
  const repo = path.join(root, 'repo');
  const source = path.resolve(here, '..', '..');
  fs.mkdirSync(path.join(repo, 'npm'), { recursive: true });
  fs.cpSync(path.join(source, 'npm', 'foremerge'), path.join(repo, 'npm', 'foremerge'), {
    recursive: true,
    filter: (file) => !file.includes(`${path.sep}node_modules`),
  });
  fs.cpSync(path.join(source, 'npm', 'scripts'), path.join(repo, 'npm', 'scripts'), { recursive: true });
  for (const file of ['Cargo.toml', 'LICENSE']) fs.copyFileSync(path.join(source, file), path.join(repo, file));
  fs.writeFileSync(path.join(repo, '.gitignore'), '/out/\n');
  const git = (...args) =>
    execFileSync('git', ['-C', repo, '-c', 'user.name=Foremerge Test', '-c', 'user.email=test@example.invalid', '-c', 'commit.gpgsign=false', '-c', 'tag.gpgsign=false', ...args], { encoding: 'utf8' });
  git('init', '--quiet');
  git('add', '.');
  git('commit', '--quiet', '-m', 'release');
  git('tag', '-a', `v${version}`, '-m', 'release');
  return { root, repo, git, build: path.join(repo, 'npm', 'scripts', 'build.mjs') };
}

test('release mode builds publishable packages from a clean checkout of the tag', () => {
  const { root, repo, build: tagged } = taggedCheckout();
  const assets = path.join(root, 'assets');
  writeAssets(assets, version);
  const result = spawnSync(process.execPath, [tagged, '--assets', assets, '--out', path.join(repo, 'out')], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Publish platform packages first/);
  for (const name of ['foremerge', ...Object.keys(launcher.optionalDependencies)]) {
    const manifest = JSON.parse(fs.readFileSync(path.join(repo, 'out', name, 'package.json'), 'utf8'));
    assert.equal(manifest.private, undefined, `${name} must be publishable from the tag`);
  }
});

test('release mode refuses uncommitted changes and commits after the tag', () => {
  const { root, repo, git, build: tagged } = taggedCheckout();
  const assets = path.join(root, 'assets');
  writeAssets(assets, version);
  const buildTagged = () =>
    spawnSync(process.execPath, [tagged, '--assets', assets, '--out', path.join(repo, 'out')], { encoding: 'utf8' });

  fs.appendFileSync(path.join(repo, 'npm', 'foremerge', 'lib', 'run.js'), '\n// edited after the tag\n');
  let result = buildTagged();
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /uncommitted changes/);

  git('commit', '--quiet', '-am', 'after the tag');
  result = buildTagged();
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, new RegExp(`HEAD is [0-9a-f]{12}, but v${version.replace(/\./g, '\\.')} is`));
});
