# Releasing Foremerge

This is the maintainer checklist for cutting a Foremerge release. It exists
because several steps are not enforced by CI and have been missed before: the
plugin manifest carries its own version, the changelog needs a compare link, and
the crates.io publish is a manual command that no workflow performs.

Only the maintainer releases. A tag push starts the release workflow, so treat
pushing a tag as the release itself rather than as preparation for one.

## Version policy

Foremerge is pre-1.0. Per `CHANGELOG.md`, a minor version may contain breaking
changes when the entry calls them out with a migration note. Use a patch version
for fixes and additions that keep the CLI, the JSON API, the MCP tool surface and
the ledger schema compatible. Use a minor version for a breaking change to any of
those, including a ledger migration that an older build will refuse.

A ledger schema bump deserves extra care. An older binary refuses a ledger a
newer build has migrated, so shipping a schema change locks anyone still running
the previous version out of their own coordination state until they upgrade. Say
so in the changelog entry.

## 1. Prepare the branch

Cut `release/<version>` from `main`, after every feature PR intended for the
release has already merged to `main`.

Do not assemble a release branch by cherry-picking from open pull requests. Doing
that produces rewritten copies of commits that still exist on the original
branches, those pull requests can never close as merged, and the release can
silently omit commits the original branch has gained since.

## 2. Verify from a clean checkout

```console
make verify
make msrv
make npm-test
```

`make verify` runs fmt, check, clippy, the full test suite, the `query-smoke`
harness (one 200-row run, not the full `query-benchmark`) and a warning-free doc
build. `make msrv` repeats clippy and the tests
on the pinned minimum supported Rust version, currently 1.85.0. Clippy's lint set
differs between the MSRV toolchain and stable, so a green `make verify` on a
newer toolchain does not predict a green CI run.

`make npm-test` runs the npm launcher's tests and the package builder's, which
need Node 18 or newer, `git`, `tar` and `zip`. CI's `npm` and `npm-smoke` jobs
cover the other platforms and an install of the packed tarballs.

All three must pass before the version bump, not after.

`make verify` only ever runs on the platform you are sitting at. macOS and
Windows coverage comes from CI's `platform` job, which runs on every pull
request, so it is the release pull request's CI run that clears those, not
anything you can run locally.

## 3. Write the changelog entry

In `CHANGELOG.md`:

- Rename `## [Unreleased]` to `## [<version>] - <YYYY-MM-DD>` and open a fresh
  empty `## [Unreleased]` above it.
- Update the compare links at the bottom of the file:

  ```
  [Unreleased]: https://github.com/naw103/foremerge/compare/v<version>...HEAD
  [<version>]: https://github.com/naw103/foremerge/compare/v<previous>...v<version>
  ```

Entries describe what changed for someone using Foremerge and why it matters.
Keep the Keep a Changelog headings (`Added`, `Changed`, `Fixed`, `Removed`).

## 4. Bump every file that carries the version

Seven files carry the version string, plus the lockfile. `CHANGELOG.md` is step
3 and is not repeated here. Missing any one of them ships assets that disagree
with each other.

| File | What it is |
| --- | --- |
| `Cargo.toml` | the crate version, and what the release workflow checks the tag against |
| `Cargo.lock` | regenerate with `cargo check` after the `Cargo.toml` edit |
| `README.md` | the status line near the top that names the current version |
| `docs/openapi.yaml` | `info.version`, the published API contract |
| `plugins/foremerge/.claude-plugin/plugin.json` | the Claude Code plugin manifest; the marketplace shows this version and nothing else in this list touches it |
| `server.json` | the MCP registry entry, in two places: `version` and `packages[0].version`. Both name the crate version, because the registry lists the crates.io package |
| `npm/foremerge/package.json` | the npm launcher, in six places: `version` and the five pins under `optionalDependencies`. The platform packages are generated from it in step 9 |
| `npm/foremerge/README.md` | the npm package page: the pinned `foremerge@<version>` in the install and `npx` examples |

`tests/skill_parity.rs` fails when the plugin manifest, either version in
`server.json`, or any version in the npm launcher or its README falls behind
the crate. That test is the backstop, not the
checklist.

Then sweep for anything the table does not know about. Run it before tagging,
so the last tag is the previous release:

```console
rg -F --hidden "$(git describe --tags --abbrev=0 | sed 's/^v//')" \
  --glob '!.git/**' --glob '!target/**' --glob '!Cargo.lock' --glob '!CHANGELOG.md'
```

`--hidden` matters: the plugin manifest lives under `.claude-plugin/`, which a
plain `rg` skips. A literal version in this command goes stale the release after
it is written, and then finds only history while every file that must move is
missed.

Read every hit before changing it. Some are the current version and must move;
some are statements of history and must not:

- `docs/roadmap.md`, the `## Shipped local proof (through X.Y.Z)` heading, moves.
- `README.md`, the note that the demo was recorded against a given released
  binary, moves **only if you re-recorded the demo**. Bumping it otherwise makes
  the README claim a recording that does not exist.
- `README.md`, the note that the short `fmg` name exists from 0.4.0 onward, is a
  historical fact and never moves.

A version string that describes when something started, or what a fixed artifact
was built against, stays where it is.

## 5. Commit the bump on its own

The release commit must be mechanical. It contains the version bumps and the
changelog dating, and no behavior change. A commit titled `Release <version>`
that also carries source edits means no reader can trust the version bump at a
glance, and a bisect that lands on it cannot tell which half broke.

If a fix is still needed, land it as its own commit first, then bump.

```console
git commit -m "Release <version>"
```

## 6. Merge to main

Open the release pull request and wait for CI to go green before merging,
including the `platform` job. The release workflow tests again after the tag, but
finding a macOS or Windows failure there means a tagged commit that cannot ship
and a wasted release run.

Merge with rebase so `main` stays linear; it has no merge commits and should keep
it that way.

Changelog entries themselves are not written here. They land in the feature pull
requests under `Unreleased`, per
[`CONTRIBUTING.md`](../CONTRIBUTING.md); step 3 only dates them.

## 7. Tag

```console
git switch main && git pull
git tag -a v<version> -m "Foremerge v<version>"
git push origin v<version>
```

Pushing the tag starts
[`.github/workflows/release.yml`](../.github/workflows/release.yml), which:

1. fails immediately if the tag does not match the `Cargo.toml` version,
2. runs the test suite on Linux, macOS and Windows,
3. builds and packages five targets with SHA-256 sums, and
4. waits for an approval in the protected `release` environment before it
   publishes anything.

The tag alone cannot publish. Nothing is public until the approval is given.

## 8. Approve the GitHub release

GitHub, then Actions, then the Release workflow run for the tag, then Review
deployments, then approve `release`.

The publish job creates the GitHub release with the packaged archives and their
checksums attached.

## 9. Publish to crates.io

This is manual. No workflow does it.

```console
git switch main && git pull
cargo publish -p foremerge --locked --dry-run
cargo publish -p foremerge --locked
```

`-p foremerge` is redundant while the manifest is a single package, and becomes
load-bearing the moment the workspace gains members that must not be published.
Naming the package costs nothing and removes the question.

Publish only after the GitHub release exists, so the two never disagree. A
crates.io version cannot be replaced, only yanked, so the dry run is not
optional.

### Then npm

The first npm release is 0.5.1. Earlier tags predate the npm packaging, so the
build below cannot run from them, by design.

Build on macOS or Linux: CI tests the package builder there, not on Windows.
The launcher and the Windows package it produces are tested on all three.

The npm packages carry the GitHub release's binaries, so they can only be built
once the release exists. The build downloads each archive, refuses any whose
SHA-256 differs from the digest the release published beside it, and writes
six packages to `npm/dist/`, which is ignored. The build refuses to run unless
`HEAD` is the commit the tag names and the tree is clean, and it exports the
launcher package from the tag itself, so an ignored file in the checkout cannot
be published with it:

```console
git switch --detach v<version>
node npm/scripts/build.mjs
```

`--allow-untagged` exists for tests and local experiments. It marks every
package it writes `private`, so npm refuses to publish them.

It prints the publish commands. Run them in that order, the five platform
packages first and `foremerge` last, because the launcher's optional
dependencies must already exist when anyone installs it. Each publish asks for
the npm account's second factor. Then confirm from a directory that is not a
repository:

```console
git switch main
npx -y foremerge@<version> --version
```

An npm version cannot be republished once unpublished, so check the printed
SHA-256 values against the release page before the first `npm publish`.

## 10. Update the MCP registry listing

Foremerge is listed in the official MCP registry as
`io.github.naw103/foremerge`. The listing is metadata only: it points at the
crates.io package, so publish there first (step 9) or the registry will name a
version nobody can install.

Run it as one block. The parentheses make a subshell with `set -euo pipefail`,
so any failing command, the signature check above all, ends the block before
anything later runs; pasted as separate lines instead, a shell carries on to
extract and execute a publisher that just failed verification. The subshell also
keeps the scratch directory out of the repository and leaves your own shell
where it was:

```console
(
  set -euo pipefail
  repo="$(git rev-parse --show-toplevel)"
  cd "$(mktemp -d)"
  v=v1.8.1
  a="mcp-publisher_$(uname -s | tr '[:upper:]' '[:lower:]')_$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/').tar.gz"
  base="https://github.com/modelcontextprotocol/registry/releases/download/$v"
  curl --fail --show-error --location -O "$base/$a" -O "$base/$a.sigstore.json"
  python3 -m venv sigstore-env
  sigstore-env/bin/pip install --quiet 'sigstore==4.5.0'
  sigstore-env/bin/python -m sigstore verify identity \
    --bundle "$a.sigstore.json" \
    --cert-identity "https://github.com/modelcontextprotocol/registry/.github/workflows/release.yml@refs/tags/$v" \
    --cert-oidc-issuer https://token.actions.githubusercontent.com \
    "$a"
  tar xzf "$a" mcp-publisher
  ./mcp-publisher validate "$repo/server.json"
  ./mcp-publisher login github
  ./mcp-publisher publish "$repo/server.json"
)
```

The publisher runs just before a GitHub login, so it is pinned and its
signature checked before it executes. The identity is the registry's own release
workflow at that exact tag, so a checksum is not needed on top of it: a checksum
published beside the archive would match a tampered release too. To move to a
newer publisher, change `v` and nothing else. The verifier is pinned too, though
only at the top level: `sigstore`'s own dependencies still resolve at install
time.

`login github` prints a device code to enter at
<https://github.com/login/device>. Registry versions are immutable: publishing
a version the registry already holds fails with `already exists`. If that
happens, query the listing below and confirm the existing record is the one you
meant to publish rather than retrying.

Two things fail this step rather than the release, so check them here:

- `description` is capped at 100 characters. The entry carries the short
  tagline for that reason, not the longer listing paragraph.
- Ownership is verified by finding `mcp-name: io.github.naw103/foremerge` in
  the README **as crates.io renders it for the version being listed**, not as
  it sits in this repository. `README.md` carries that token in the links list.
  If it is ever edited away, this step fails and only a new crate release can
  fix it.

Verify:

```console
curl "https://registry.modelcontextprotocol.io/v0.1/servers?search=foremerge"
```

Aggregators such as PulseMCP and Glama mirror from the registry on their own
schedule, so a listing that has not propagated within a day is theirs to
explain, not a failed publish.

## 11. Confirm

- `cargo install foremerge` from a clean environment installs the new version
  and `foremerge --version` reports it.
- The release page lists all five archives and their `.sha256` files.
- `foremerge doctor` is healthy against a ledger created by the new build.
- The release notes read like the changelog entry. Until the gap below is
  closed they will be an autogenerated commit list instead, so paste the
  `CHANGELOG.md` section into the release body by hand.

If the changelog entry carries a migration note, also check the upgrade edge
before you announce it: open a ledger written by the **previous** release with
the new binary and confirm it migrates, then point the **previous** binary at a
ledger the new one has migrated and confirm it refuses with the
`UNSUPPORTED_SCHEMA` message the changelog promises. A build that reports a
ledger it cannot open as healthy is worse than one that refuses it loudly.

## Hotfixes

A hotfix is an ordinary patch release: branch from the tag as
`release/<version>` if `main` has moved on, fix, follow this checklist from step
2, and merge back to `main` after tagging.

If the fix is for a reported vulnerability, follow [`SECURITY.md`](../SECURITY.md)
for disclosure timing. Do not describe the vulnerability in a commit message or
changelog entry that lands before the advisory does.

Yank a crates.io version only when it is actively harmful, for example a build
that corrupts a ledger. Yanking does not delete it and does not break anyone who
already has it in a lockfile. Follow it with a fixed release, and say what
happened in the changelog.

## Known gaps

Four steps are not automated, and each is a place a release can go wrong quietly:

- **crates.io publishing is manual** (step 9). A `publish-crate` job in
  `release.yml`, gated on the same `release` environment and holding a
  `CARGO_REGISTRY_TOKEN` secret, would make it impossible to forget while
  keeping the approval requirement.
- **npm publishing is manual** (step 9). A job in `release.yml` after the
  GitHub release, using npm trusted publishing, would remove the second-factor
  prompts and add provenance attestations linking each package to the workflow
  run that built it.
- **The MCP registry listing is published by hand** (step 10). The entry sat
  unpublished from launch week until 0.5.0 because nothing mentioned it.
  `tests/skill_parity.rs` now fails a release whose `server.json` still names
  the previous version, but nothing fails a release that bumps the file and
  then skips the publish.
- **The GitHub release notes are autogenerated.** The publish job passes
  `--generate-notes`, which emits a commit list and ignores `CHANGELOG.md`.
  Extracting the section instead would put the written entry on the release
  page:

  ```console
  awk '/^## \[<version>\]/{f=1;next} /^## \[/{f=0} f' CHANGELOG.md > notes.md
  ```
