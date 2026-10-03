#!/usr/bin/env node
// Builds the npm packages for one Foremerge release into npm/dist/.
//
//   node npm/scripts/build.mjs [--version 0.5.0] [--assets DIR]
//
// Each platform package carries the binaries from that release's GitHub
// archive, unmodified. Archives are read from --assets when given, otherwise
// downloaded from the release, and every archive must match the SHA-256 the
// release published beside it before anything is unpacked. The launcher
// package is copied from npm/foremerge with its version pins checked.
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
const dist = path.join(npmRoot, 'dist');

const TARGETS = [
  { pkg: 'foremerge-darwin-arm64', target: 'aarch64-apple-darwin', os: 'darwin', cpu: 'arm64', label: 'macOS on Apple silicon' },
  { pkg: 'foremerge-darwin-x64', target: 'x86_64-apple-darwin', os: 'darwin', cpu: 'x64', label: 'macOS on Intel' },
  { pkg: 'foremerge-linux-arm64', target: 'aarch64-unknown-linux-gnu', os: 'linux', cpu: 'arm64', libc: 'glibc', label: 'Linux arm64 (glibc)' },
  { pkg: 'foremerge-linux-x64', target: 'x86_64-unknown-linux-gnu', os: 'linux', cpu: 'x64', libc: 'glibc', label: 'Linux x64 (glibc)' },
  { pkg: 'foremerge-win32-x64', target: 'x86_64-pc-windows-msvc', os: 'win32', cpu: 'x64', label: 'Windows x64' },
];
const PROGRAMS = ['foremerge', 'fmg'];

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    if (flag === '--version' || flag === '--assets') {
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

function unpack(archive, destination) {
  fs.mkdirSync(destination, { recursive: true });
  if (archive.endsWith('.zip')) {
    execFileSync('unzip', ['-q', '-o', archive, '-d', destination]);
  } else {
    execFileSync('tar', ['-xzf', archive, '-C', destination]);
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

  fs.rmSync(dist, { recursive: true, force: true });
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
  console.log(`build: foremerge@${version} launcher`);

  fs.rmSync(work, { recursive: true, force: true });

  console.log('\nPublish platform packages first, then the launcher:');
  for (const t of TARGETS) console.log(`  npm publish --access public ${path.relative(process.cwd(), path.join(dist, t.pkg))}`);
  console.log(`  npm publish --access public ${path.relative(process.cwd(), launcherOut)}`);
}

await main();
