'use strict';

// Runs the Foremerge binary that npm installed for this platform. The binary
// comes from one of the platform packages listed as optionalDependencies,
// each holding the unmodified release build, so nothing is downloaded or
// compiled here and the Rust program stays the product.

const { spawn } = require('node:child_process');
const path = require('node:path');

const PLATFORM_PACKAGES = {
  'darwin-arm64': 'foremerge-darwin-arm64',
  'darwin-x64': 'foremerge-darwin-x64',
  'linux-arm64': 'foremerge-linux-arm64',
  'linux-x64': 'foremerge-linux-x64',
  'win32-x64': 'foremerge-win32-x64',
};

const OTHER_INSTALLS =
  'Install Foremerge another way instead:\n' +
  '  curl -fsSL https://foremerge.com/install.sh | sh\n' +
  '  cargo install --locked foremerge\n' +
  'or download a release from https://github.com/naw103/foremerge/releases';

// The Linux builds link against glibc. Node reports the runtime glibc version
// only when it is running on glibc, so its absence means musl (Alpine).
function isMusl() {
  if (process.platform !== 'linux') return false;
  try {
    const header = process.report.getReport().header;
    return !header.glibcVersionRuntime;
  } catch {
    return false;
  }
}

function packageFor(platform, arch, musl) {
  if (platform === 'linux' && musl) {
    throw new Error(
      'Foremerge does not publish a musl (Alpine) Linux build to npm.\n' + OTHER_INSTALLS,
    );
  }
  const name = PLATFORM_PACKAGES[`${platform}-${arch}`];
  if (!name) {
    throw new Error(
      `Foremerge does not publish an npm build for ${platform}-${arch}.\n` + OTHER_INSTALLS,
    );
  }
  return name;
}

function binaryPath(program, options = {}) {
  const platform = options.platform || process.platform;
  const arch = options.arch || process.arch;
  const musl = options.musl === undefined ? isMusl() : options.musl;
  const resolve = options.resolve || require.resolve;
  const name = packageFor(platform, arch, musl);
  let manifest;
  try {
    manifest = resolve(`${name}/package.json`);
  } catch {
    throw new Error(
      `The ${name} package that carries the Foremerge binary is not installed.\n` +
        'npm skips it when optional dependencies are disabled (--omit=optional or\n' +
        '--no-optional); reinstall without that flag.\n' +
        OTHER_INSTALLS,
    );
  }
  const executable = platform === 'win32' ? `${program}.exe` : program;
  return path.join(path.dirname(manifest), 'bin', executable);
}

function run(program) {
  let binary;
  try {
    binary = binaryPath(program);
  } catch (error) {
    process.stderr.write(`foremerge: ${error.message}\n`);
    process.exit(1);
  }

  // Inherited stdio hands the child this process's descriptors, so an MCP
  // client talking over stdin and stdout reaches the binary directly.
  const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit' });

  // A client that stops the server signals this process, not the binary, so
  // pass the signal on rather than leaving the binary running.
  const forwarded = ['SIGINT', 'SIGTERM', 'SIGHUP'];
  const forward = (signal) => {
    if (child.exitCode === null && child.signalCode === null) child.kill(signal);
  };
  for (const signal of forwarded) process.on(signal, forward);

  child.on('error', (error) => {
    process.stderr.write(`foremerge: could not start ${binary}: ${error.message}\n`);
    process.exit(1);
  });

  child.on('exit', (code, signal) => {
    for (const s of forwarded) process.removeListener(s, forward);
    if (signal) {
      // Exit the way the binary did, so callers see the same signal.
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code === null ? 1 : code);
  });
}

module.exports = { PLATFORM_PACKAGES, binaryPath, isMusl, packageFor, run };
