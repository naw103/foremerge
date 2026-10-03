#!/usr/bin/env node
// Builds the npm packages for one Foremerge release into npm/dist/.
//
//   node npm/scripts/build.mjs [--version 0.5.0] [--assets DIR] [--out DIR]
//                              [--allow-untagged]
//
// Each platform package carries the binaries from that release's GitHub
// archive, unmodified. Archives are read from --assets when given, otherwise
// downloaded from the release, and every archive must match the SHA-256 the
// release published beside it before anything is unpacked. The launcher
// package is copied from npm/foremerge with its version pins checked.
//
// The launcher comes from this checkout, so the checkout must be the release:
// HEAD must be the commit `v<version>` names and the tree must be clean, or a
// launcher changed after the tag would ship under a version already released.
// --allow-untagged skips that for tests and local experiments, and marks every
// package it writes `private`, which npm refuses to publish.
//
// Nothing is published. The script prints the publish commands, platform
// packages first, because the launcher's optional dependencies must already
// exist on the registry when someone installs it.

import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const npmRoot = path.resolve(here, '..');
const repoRoot = path.resolve(npmRoot, '..');
const launcherSource = path.join(npmRoot, 'foremerge');

const TARGETS = [
  { pkg: 'foremerge-darwin-arm64', target: 'aarch64-apple-darwin', os: 'darwin', cpu: 'arm64', label: 'macOS on Apple silicon' },
  { pkg: 'foremerge-darwin-x64', target: 'x86_64-apple-darwin', os: 'darwin', cpu: 'x64', label: 'macOS on Intel' },
  { pkg: 'foremerge-linux-arm64', target: 'aarch64-unknown-linux-gnu', os: 'linux', cpu: 'arm64', libc: 'glibc', label: 'Linux arm64 (glibc)' },
  { pkg: 'foremerge-linux-x64', target: 'x86_64-unknown-linux-gnu', os: 'linux', cpu: 'x64', libc: 'glibc', label: 'Linux x64 (glibc)' },
  { pkg: 'foremerge-win32-x64', target: 'x86_64-pc-windows-msvc', os: 'win32', cpu: 'x64', label: 'Windows x64' },
];
const PROGRAMS = ['foremerge', 'fmg'];

// Written into every output directory. A directory is only ever emptied when
// it holds this file, so --out cannot erase anything this script did not
// write, whatever path it is given.
const OUTPUT_MARKER = '.foremerge-npm-build';

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    if (flag === '--allow-untagged') {
      args.allowUntagged = true;
    } else if (flag === '--version' || flag === '--assets' || flag === '--out') {
      const value = argv[i + 1];
      if (!value) fail(`${flag} needs a value`);
      args[flag.slice(2)] = value;
      i += 1;
    } else {
      fail(`unknown argument ${flag}`);
    }
  }
  return args;
}

function fail(message) {
  console.error(`build: ${message}`);
  process.exit(1);
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function writeJson(file, value) {
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

function git(...gitArgs) {
  return execFileSync('git', ['-C', repoRoot, ...gitArgs], { encoding: 'utf8' }).trim();
}

function requireReleaseCheckout(tag) {
  let tagged;
  try {
    tagged = git('rev-parse', '--verify', '--quiet', `${tag}^{commit}`);
  } catch {
    fail(`no ${tag} tag in this checkout; build from the release tag, or pass --allow-untagged for a local build that cannot be published`);
  }
  const head = git('rev-parse', 'HEAD');
  if (head !== tagged) {
    fail(`HEAD is ${head.slice(0, 12)}, but ${tag} is ${tagged.slice(0, 12)}; run \`git switch --detach ${tag}\` first, so the launcher is the one that release tagged`);
  }
  const dirty = git('status', '--porcelain');
  if (dirty) fail(`the checkout has uncommitted changes, which would ship in the launcher:\n${dirty}`);
}

function isWithin(child, parent) {
  const relative = path.relative(parent, child);
  return relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative));
}

// Empty the output directory, or create it, refusing anything that could hold
// files this script did not write: the checkout, a directory containing it,
// the launcher source, or an existing directory without the marker.
function prepareOutput(dist) {
  if (isWithin(repoRoot, dist)) fail(`--out ${dist} contains the checkout; choose a new or empty directory`);
  if (isWithin(dist, launcherSource)) fail(`--out ${dist} is inside npm/foremerge, which the launcher is copied from`);
  let entries = null;
  try {
    const stat = fs.lstatSync(dist);
    if (!stat.isDirectory()) fail(`--out ${dist} exists and is not a directory`);
    entries = fs.readdirSync(dist);
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
  }
  if (entries && entries.length && !entries.includes(OUTPUT_MARKER)) {
    fail(`--out ${dist} already holds files this script did not write; choose a new or empty directory, or remove it yourself`);
  }
  fs.rmSync(dist, { recursive: true, force: true });
  fs.mkdirSync(dist, { recursive: true });
  fs.writeFileSync(path.join(dist, OUTPUT_MARKER), 'Written by npm/scripts/build.mjs, which replaces this directory on every run.\n');
}

function cargoVersion() {
  const manifest = fs.readFileSync(path.join(repoRoot, 'Cargo.toml'), 'utf8');
  const match = manifest.match(/^\[package\][^[]*?^version\s*=\s*"([^"]+)"/m);
  if (!match) fail('could not read the package version from Cargo.toml');
  return match[1];
}

async function fetchAsset(url, destination) {
  const response = await fetch(url, { redirect: 'follow' });
  if (!response.ok) fail(`GET ${url} returned ${response.status}`);
  fs.writeFileSync(destination, Buffer.from(await response.arrayBuffer()));
}

function sha256(file) {
  return createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

// The release's .sha256 files are `<hex>  <archive name>`.
function publishedDigest(file, archiveName) {
  const [digest, name] = fs.readFileSync(file, 'utf8').trim().split(/\s+/);
  if (!/^[0-9a-f]{64}$/.test(digest || '')) fail(`${file} does not hold a SHA-256 digest`);
  if (name && name !== archiveName) fail(`${file} names ${name}, expected ${archiveName}`);
  return digest;
}

// Windows ships bsdtar as System32\tar.exe, which reads zip as well as gzip.
// Named by path, because Git for Windows puts a GNU tar on PATH that does not.
export function tarCommand() {
  return process.platform === 'win32'
    ? path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'tar.exe')
    : 'tar';
}

function unpack(archive, destination) {
  fs.mkdirSync(destination, { recursive: true });
  if (archive.endsWith('.zip') && process.platform !== 'win32') {
    execFileSync('unzip', ['-q', '-o', archive, '-d', destination]);
  } else {
    execFileSync(tarCommand(), ['-xf', archive, '-C', destination]);
  }
}

function platformReadme(t, version) {
  return `# ${t.pkg}

The Foremerge ${version} binaries for ${t.label}: \`foremerge\` and \`fmg\`,
byte-for-byte the \`${t.target}\` archive from the
[GitHub release](https://github.com/naw103/foremerge/releases/tag/v${version}).

Do not install this package directly. Install
[\`foremerge\`](https://www.npmjs.com/package/foremerge), which selects the
right platform package for you.
`;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const launcher = readJson(path.join(launcherSource, 'package.json'));
  const version = args.version || launcher.version;
  const tag = `v${version}`;

  if (args.allowUntagged) {
    console.warn('build: --allow-untagged: not bound to a release tag; every package is marked private and cannot be published');
  } else {
    requireReleaseCheckout(tag);
  }
  if (launcher.version !== version) {
    fail(`npm/foremerge/package.json is ${launcher.version}, building ${version}`);
  }
  const crate = cargoVersion();
  if (crate !== version) {
    console.warn(`build: note: Cargo.toml is ${crate}; packaging the published ${tag} release`);
  }
  const pins = launcher.optionalDependencies || {};
  for (const t of TARGETS) {
    if (pins[t.pkg] !== version) fail(`npm/foremerge pins ${t.pkg}@${pins[t.pkg]}, expected ${version}`);
  }
  const extra = Object.keys(pins).filter((name) => !TARGETS.some((t) => t.pkg === name));
  if (extra.length) fail(`npm/foremerge pins unknown packages: ${extra.join(', ')}`);

  const dist = path.resolve(args.out || path.join(npmRoot, 'dist'));
  prepareOutput(dist);
  const work = path.join(dist, '.work');
  fs.mkdirSync(work, { recursive: true });
  const license = path.join(repoRoot, 'LICENSE');

  for (const t of TARGETS) {
    const ext = t.os === 'win32' ? 'zip' : 'tar.gz';
    const archiveName = `foremerge-${tag}-${t.target}.${ext}`;
    const archive = path.join(work, archiveName);
    const digestFile = `${archive}.sha256`;
    if (args.assets) {
      fs.copyFileSync(path.join(args.assets, archiveName), archive);
      fs.copyFileSync(path.join(args.assets, `${archiveName}.sha256`), digestFile);
    } else {
      const base = `https://github.com/naw103/foremerge/releases/download/${tag}`;
      await fetchAsset(`${base}/${archiveName}`, archive);
      await fetchAsset(`${base}/${archiveName}.sha256`, digestFile);
    }

    const expected = publishedDigest(digestFile, archiveName);
    const actual = sha256(archive);
    if (actual !== expected) fail(`${archiveName}: SHA-256 ${actual} does not match the release's ${expected}`);

    const unpacked = path.join(work, t.target);
    unpack(archive, unpacked);

    const out = path.join(dist, t.pkg);
    const bin = path.join(out, 'bin');
    fs.mkdirSync(bin, { recursive: true });
    for (const program of PROGRAMS) {
      const executable = t.os === 'win32' ? `${program}.exe` : program;
      const source = path.join(unpacked, executable);
      if (!fs.existsSync(source)) fail(`${archiveName} has no ${executable}`);
      fs.copyFileSync(source, path.join(bin, executable));
      fs.chmodSync(path.join(bin, executable), 0o755);
    }

    const manifest = {
      name: t.pkg,
      version,
      description: `The Foremerge binary for ${t.label}. Install the foremerge package instead.`,
      license: launcher.license,
      homepage: launcher.homepage,
      repository: { ...launcher.repository, directory: 'npm' },
      os: [t.os],
      cpu: [t.cpu],
      ...(t.libc ? { libc: [t.libc] } : {}),
      files: ['bin'],
      preferUnplugged: true,
      ...(args.allowUntagged ? { private: true } : {}),
    };
    writeJson(path.join(out, 'package.json'), manifest);
    fs.writeFileSync(path.join(out, 'README.md'), platformReadme(t, version));
    fs.copyFileSync(license, path.join(out, 'LICENSE'));
    console.log(`build: ${t.pkg}@${version} from ${archiveName} (sha256 ${actual})`);
  }

  const launcherOut = path.join(dist, 'foremerge');
  fs.cpSync(launcherSource, launcherOut, {
    recursive: true,
    filter: (source) => !source.includes(`${path.sep}node_modules`),
  });
  fs.copyFileSync(license, path.join(launcherOut, 'LICENSE'));
  if (args.allowUntagged) {
    const manifestPath = path.join(launcherOut, 'package.json');
    writeJson(manifestPath, { ...readJson(manifestPath), private: true });
  }
  console.log(`build: foremerge@${version} launcher`);

  fs.rmSync(work, { recursive: true, force: true });

  if (args.allowUntagged) return;
  console.log('\nPublish platform packages first, then the launcher:');
  for (const t of TARGETS) console.log(`  npm publish --access public ${path.relative(process.cwd(), path.join(dist, t.pkg))}`);
  console.log(`  npm publish --access public ${path.relative(process.cwd(), launcherOut)}`);
}

// Run only when executed, not when fixtures.mjs imports tarCommand. Both
// sides are real paths, so a symlinked invocation still runs.
const invoked = process.argv[1] ? fs.realpathSync(process.argv[1]) : '';
if (invoked === fs.realpathSync(fileURLToPath(import.meta.url))) {
  await main();
}
