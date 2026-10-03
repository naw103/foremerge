// Release-shaped assets for testing the npm build without a GitHub release.
//
// Writes one archive per release target, named and laid out as release.yml
// does, each with a `<hex>  <name>` .sha256 beside it. By default the
// binaries are stand-in shell scripts; `native` replaces this machine's
// target with real binaries from a directory, such as target/debug.

import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

import { tarCommand } from './build.mjs';

export const TARGETS = [
  { triple: 'aarch64-apple-darwin', platform: 'darwin', arch: 'arm64' },
  { triple: 'x86_64-apple-darwin', platform: 'darwin', arch: 'x64' },
  { triple: 'aarch64-unknown-linux-gnu', platform: 'linux', arch: 'arm64' },
  { triple: 'x86_64-unknown-linux-gnu', platform: 'linux', arch: 'x64' },
  { triple: 'x86_64-pc-windows-msvc', platform: 'win32', arch: 'x64' },
];

export function nativeTarget() {
  return TARGETS.find((t) => t.platform === process.platform && t.arch === process.arch);
}

// A stand-in that reports which program it is and echoes its arguments, so
// a test can tell the launcher ran the right one with the right argv.
function standIn(program, triple) {
  return `#!/bin/sh\necho "${program} ${triple} $*"\n`;
}

export function writeAssets(directory, version, { native } = {}) {
  const tag = `v${version}`;
  fs.mkdirSync(directory, { recursive: true });
  const nativeTriple = native ? nativeTarget()?.triple : undefined;
  for (const { triple, platform } of TARGETS) {
    const stage = fs.mkdtempSync(path.join(directory, `.stage-${triple}-`));
    const suffix = platform === 'win32' ? '.exe' : '';
    for (const program of ['foremerge', 'fmg']) {
      const file = path.join(stage, `${program}${suffix}`);
      if (triple === nativeTriple) {
        fs.copyFileSync(path.join(native, `${program}${suffix}`), file);
      } else {
        fs.writeFileSync(file, standIn(program, triple));
      }
      fs.chmodSync(file, 0o755);
    }
    const name = `foremerge-${tag}-${triple}.${platform === 'win32' ? 'zip' : 'tar.gz'}`;
    const archive = path.join(directory, name);
    const members = ['foremerge', 'fmg'].map((program) => `${program}${suffix}`);
    if (platform === 'win32' && process.platform !== 'win32') {
      execFileSync('zip', ['-q', '-j', archive, ...members.map((m) => path.join(stage, m))]);
    } else if (platform === 'win32') {
      // bsdtar picks the archive format from the .zip suffix with -a.
      execFileSync(tarCommand(), ['-a', '-c', '-f', archive, '-C', stage, ...members]);
    } else {
      execFileSync(tarCommand(), ['-C', stage, '-czf', archive, ...members]);
    }
    const digest = createHash('sha256').update(fs.readFileSync(archive)).digest('hex');
    fs.writeFileSync(`${archive}.sha256`, `${digest}  ${name}\n`);
    fs.rmSync(stage, { recursive: true, force: true });
  }
}
