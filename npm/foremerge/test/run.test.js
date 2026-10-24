'use strict';

const assert = require('node:assert/strict');
const path = require('node:path');
const test = require('node:test');

const { PLATFORM_PACKAGES, binaryPath, packageFor, pinnedVersion } = require('../lib/run.js');
const manifest = require('../package.json');

test('every platform package is pinned to the launcher version', () => {
  assert.deepEqual(
    Object.keys(manifest.optionalDependencies).sort(),
    Object.values(PLATFORM_PACKAGES).sort(),
  );
  for (const version of Object.values(manifest.optionalDependencies)) {
    assert.equal(version, manifest.version);
  }
});

test('each supported platform maps to its package', () => {
  assert.equal(packageFor('darwin', 'arm64', false), 'foremerge-darwin-arm64');
  assert.equal(packageFor('darwin', 'x64', false), 'foremerge-darwin-x64');
  assert.equal(packageFor('linux', 'arm64', false), 'foremerge-linux-arm64');
  assert.equal(packageFor('linux', 'x64', false), 'foremerge-linux-x64');
  assert.equal(packageFor('win32', 'x64', false), 'foremerge-win32-x64');
});

test('musl Linux is refused with other ways to install', () => {
  assert.throws(() => packageFor('linux', 'x64', true), /musl[\s\S]*install\.sh/);
});

test('an unsupported platform is refused with other ways to install', () => {
  assert.throws(() => packageFor('win32', 'arm64', false), /win32-arm64[\s\S]*cargo install/);
  assert.throws(() => packageFor('freebsd', 'x64', false), /freebsd-x64/);
});

test('the binary is found inside the resolved platform package', () => {
  const resolve = (request) => {
    assert.equal(request, 'foremerge-linux-x64/package.json');
    return path.join('/nm', 'foremerge-linux-x64', 'package.json');
  };
  assert.equal(
    binaryPath('fmg', { platform: 'linux', arch: 'x64', musl: false, resolve }),
    path.join('/nm', 'foremerge-linux-x64', 'bin', 'fmg'),
  );
});

test('Windows binaries carry the .exe suffix', () => {
  const resolve = () => path.join('/nm', 'foremerge-win32-x64', 'package.json');
  assert.equal(
    binaryPath('foremerge', { platform: 'win32', arch: 'x64', musl: false, resolve }),
    path.join('/nm', 'foremerge-win32-x64', 'bin', 'foremerge.exe'),
  );
});

test('a missing platform package is named with its pinned version and every likely cause', () => {
  const resolve = () => {
    throw new Error('MODULE_NOT_FOUND');
  };
  let message;
  assert.throws(
    () => binaryPath('foremerge', { platform: 'darwin', arch: 'arm64', musl: false, resolve }),
    (error) => {
      message = error.message;
      return true;
    },
  );
  const pinned = `foremerge-darwin-arm64@${manifest.optionalDependencies['foremerge-darwin-arm64']}`;
  assert.ok(message.startsWith(`${pinned}, the package that carries`), message);
  assert.match(message, /Likely causes:/);
  assert.match(message, /optional dependencies were omitted/);
  assert.match(message, new RegExp(`${pinned.replace(/\./g, '\\.')} is not published on npm`));
  assert.match(message, /unsupported on this system/);
  assert.match(message, /releases/);
  assert.match(message, /install\.sh/);
  assert.match(message, /cargo install/);
  // The flag is one possible cause, not the stated one.
  assert.doesNotMatch(message, /npm skips it when optional dependencies are disabled/);
});

test('a missing platform package uses the version the launcher manifest pins', () => {
  const resolve = () => {
    throw new Error('MODULE_NOT_FOUND');
  };
  const launcher = { optionalDependencies: { 'foremerge-win32-x64': '9.8.7' } };
  assert.throws(
    () => binaryPath('foremerge', { platform: 'win32', arch: 'x64', musl: false, resolve, launcher }),
    /foremerge-win32-x64@9\.8\.7, the package[\s\S]*foremerge-win32-x64@9\.8\.7 is not published on npm/,
  );
});

test('the pinned version is read from the launcher manifest', () => {
  for (const name of Object.values(PLATFORM_PACKAGES)) {
    assert.equal(pinnedVersion(name), manifest.version);
  }
  assert.equal(pinnedVersion('foremerge-darwin-arm64', {}), undefined);
});

test('an unsupported platform is refused before any package lookup', () => {
  const resolve = () => assert.fail('an unsupported platform must not look up a package');
  assert.throws(
    () => binaryPath('foremerge', { platform: 'win32', arch: 'arm64', musl: false, resolve }),
    /does not publish an npm build for win32-arm64/,
  );
  assert.throws(
    () => binaryPath('foremerge', { platform: 'linux', arch: 'x64', musl: true, resolve }),
    /musl/,
  );
});
