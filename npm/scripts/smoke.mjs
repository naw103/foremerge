#!/usr/bin/env node
// Installs the npm packages the way a user would and checks they work.
//
//   node npm/scripts/smoke.mjs --binaries target/debug
//
// Builds release-shaped assets with this machine's real binaries from
// --binaries, packs the launcher and this platform's package, installs both
// globally into a scratch prefix, and then, with only that prefix and Node on
// PATH and a scratch HOME:
//
//   - runs `foremerge --version` and `fmg --version` through the launcher;
//   - completes an MCP handshake from a directory that is not a repository;
//   - runs `doctor` in a scratch repository and requires no warning about the
//     npm launcher, which runs this installation rather than being another.
//
// Every directory it touches is created here, so it never opens a real
// repository's ledger.

import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { nativeTarget, writeAssets } from './fixtures.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const launcherManifest = JSON.parse(fs.readFileSync(path.join(here, '..', 'foremerge', 'package.json'), 'utf8'));
const version = launcherManifest.version;

function fail(message) {
  console.error(`smoke: ${message}`);
  process.exit(1);
}

const flag = process.argv.indexOf('--binaries');
if (flag === -1 || !process.argv[flag + 1]) fail('pass --binaries DIR holding foremerge and fmg');
const binaries = path.resolve(process.argv[flag + 1]);
const target = nativeTarget();
if (!target) fail(`no smoke test for ${process.platform}-${process.arch}`);
const windows = process.platform === 'win32';

// On Windows `npm` and the commands npm installs are .cmd shims, which Node
// will only start through a shell. Every path here is a temp path with no
// spaces, so the shell sees the arguments unchanged.
const shell = windows;
function npm(args, options = {}) {
  return execFileSync('npm', args, { encoding: 'utf8', shell, ...options });
}

const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'foremerge-npm-smoke-')));
const assets = path.join(root, 'assets');
const out = path.join(root, 'out');
const tarballs = path.join(root, 'tarballs');
const prefix = path.join(root, 'prefix');
const home = path.join(root, 'home');
for (const dir of [tarballs, prefix, home]) fs.mkdirSync(dir, { recursive: true });

writeAssets(assets, version, { native: binaries });
execFileSync(process.execPath, [path.join(here, 'build.mjs'), '--allow-untagged', '--assets', assets, '--out', out], { stdio: 'inherit' });

const platformPackage = Object.keys(launcherManifest.optionalDependencies).find(
  (name) => name === `foremerge-${target.platform}-${target.arch}`,
);
const packed = [platformPackage, 'foremerge'].map((name) => {
  // An untagged build is private so it can never be published. npm still
  // packs and installs it, which is all this needs.
  const [info] = JSON.parse(npm(['pack', '--json', '--pack-destination', tarballs, path.join(out, name)]));
  return path.join(tarballs, info.filename);
});
npm(['install', '--global', '--prefix', prefix, '--no-audit', '--no-fund', ...packed], { stdio: 'inherit' });

// npm puts global commands in <prefix>/bin on Unix and in <prefix> itself on
// Windows. Only that, Node, and the system's own tools are on PATH, so the
// only Foremerge to find is the one just installed.
const commands = windows ? prefix : path.join(prefix, 'bin');
const systemPath = windows ? process.env.PATH.split(path.delimiter) : ['/usr/bin', '/bin'];
const env = {
  ...process.env,
  HOME: home,
  USERPROFILE: home,
  PATH: [commands, path.dirname(process.execPath), ...systemPath].join(path.delimiter),
};
delete env.CARGO_HOME;
delete env.Path;

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: 'utf8', env, shell, ...options });
  if (result.error) fail(`${command} ${args.join(' ')}: ${result.error.message}`);
  return result;
}

for (const program of ['foremerge', 'fmg']) {
  const result = run(program, ['--version']);
  if (result.status !== 0 || !result.stdout.includes(version)) {
    fail(`${program} --version printed ${JSON.stringify(result.stdout)} (status ${result.status}): ${result.stderr}`);
  }
  console.log(`smoke: ${program} --version -> ${result.stdout.trim()}`);
}

// The MCP handshake a registry checker performs: no repository, stdio only.
const outside = path.join(root, 'outside');
fs.mkdirSync(outside);
const requests = [
  { jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'smoke', version: '1' } } },
  { jsonrpc: '2.0', method: 'notifications/initialized' },
  { jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} },
];
const mcp = run('foremerge', ['mcp'], { cwd: outside, input: requests.map((r) => JSON.stringify(r)).join('\n') + '\n' });
if (mcp.status !== 0) fail(`foremerge mcp exited ${mcp.status}: ${mcp.stderr}`);
const answers = mcp.stdout.trim().split('\n').map((line) => JSON.parse(line));
const initialized = answers.find((a) => a.id === 1);
const listed = answers.find((a) => a.id === 2);
if (initialized?.result?.serverInfo?.name !== 'foremerge') fail(`no initialize answer: ${mcp.stdout}`);
if (!(listed?.result?.tools?.length > 0)) fail(`no tools/list answer: ${mcp.stdout}`);
if (fs.readdirSync(outside).length !== 0) fail('the MCP server wrote into the directory it was started in');
console.log(`smoke: MCP handshake outside a repository -> ${listed.result.tools.length} tools`);

// doctor reads the installations on PATH. The npm launcher is there; it must
// not be reported as a second installation.
const repo = path.join(root, 'repo');
fs.mkdirSync(repo);
run('git', ['init', '--quiet', repo]);
const doctor = run('foremerge', ['--json', 'doctor', '--client', 'claude'], { cwd: repo });
const report = JSON.parse(doctor.stdout);
const installations = report.data.installations;
const launcher = installations.find((i) => i.npm_launcher);
if (!launcher) fail(`doctor did not recognise the npm launcher: ${JSON.stringify(installations)}`);
const warnings = (report.data.warnings || []).filter((w) => w.includes('Another foremerge is installed'));
if (warnings.length) fail(`doctor warned about this installation's own launcher: ${warnings.join('\n')}`);
console.log(`smoke: doctor lists ${installations.length} paths for one installation, no warnings`);

fs.rmSync(root, { recursive: true, force: true });
console.log('smoke: ok');
