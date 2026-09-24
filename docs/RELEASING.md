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
```

`make verify` runs fmt, check, clippy, the full test suite, the `query-smoke`
harness (one 200-row run, not the full `query-benchmark`) and a warning-free doc
build. `make msrv` repeats clippy and the tests
on the pinned minimum supported Rust version, currently 1.85.0. Clippy's lint set
differs between the MSRV toolchain and stable, so a green `make verify` on a
newer toolchain does not predict a green CI run.

Both must pass before the version bump, not after.

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

Five files carry the version string, plus the lockfile. `CHANGELOG.md` is step
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

`tests/skill_parity.rs` fails when the plugin manifest falls behind the crate.
That test is the backstop, not the checklist.

Then sweep for anything the table does not know about:

```console
rg '0\.4\.0' --glob '!target/**' --glob '!Cargo.lock' --glob '!CHANGELOG.md'
```

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

## 10. Update the MCP registry listing

Foremerge is listed in the official MCP registry as
`io.github.naw103/foremerge`. The listing is metadata only: it points at the
crates.io package, so publish there first (step 9) or the registry will name a
version nobody can install.

```console
curl -L "https://github.com/modelcontextprotocol/registry/releases/latest/download/mcp-publisher_$(uname -s | tr '[:upper:]' '[:lower:]')_$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/').tar.gz" | tar xz mcp-publisher
./mcp-publisher validate
./mcp-publisher login github
./mcp-publisher publish
```

`login github` prints a device code to enter at
<https://github.com/login/device>. Publishing is idempotent per version, and
the registry refuses a version it already holds.

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

Three steps are not automated, and each is a place a release can go wrong quietly:

- **crates.io publishing is manual** (step 9). A `publish-crate` job in
  `release.yml`, gated on the same `release` environment and holding a
  `CARGO_REGISTRY_TOKEN` secret, would make it impossible to forget while
  keeping the approval requirement.
- **The MCP registry listing is published by hand** (step 10), and nothing
  fails when it falls behind. The entry sat unpublished from launch week until
  0.5.0 for exactly that reason. A test asserting `server.json` matches the
  crate version, as `tests/skill_parity.rs` does for the plugin manifest, would
  catch a stale entry before the tag; it would not catch a release that skips
  the publish.
- **The GitHub release notes are autogenerated.** The publish job passes
  `--generate-notes`, which emits a commit list and ignores `CHANGELOG.md`.
  Extracting the section instead would put the written entry on the release
  page:

  ```console
  awk '/^## \[<version>\]/{f=1;next} /^## \[/{f=0} f' CHANGELOG.md > notes.md
  ```
