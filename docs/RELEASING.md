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

`make verify` runs fmt, check, clippy, the full test suite, the query benchmark
smoke run and a warning-free doc build. `make msrv` repeats clippy and the tests
on the pinned minimum supported Rust version, currently 1.85.0. Clippy's lint set
differs between the MSRV toolchain and stable, so a green `make verify` on a
newer toolchain does not predict a green CI run.

Both must pass before the version bump, not after.

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

Five files, plus the lockfile. Missing any one of them ships assets that disagree
with each other.

| File | What it is |
| --- | --- |
| `Cargo.toml` | the crate version, and what the release workflow checks the tag against |
| `Cargo.lock` | regenerate with `cargo check` after the `Cargo.toml` edit |
| `README.md` | the status line near the top that names the current version |
| `docs/openapi.yaml` | `info.version`, the published API contract |
| `plugins/foremerge/.claude-plugin/plugin.json` | the Claude Code plugin manifest; the marketplace shows this version and nothing else in this list touches it |
| `CHANGELOG.md` | the section heading and compare links from step 3 |

`tests/skill_parity.rs` fails when the plugin manifest falls behind the crate.
That test is the backstop, not the checklist.

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

Open the release pull request and let CI finish. Merge with rebase so `main`
stays linear; it has no merge commits and should keep it that way.

## 7. Tag

```console
git switch main && git pull
git tag -a v<version> -m "Foremerge v<version>"
git push origin v<version>
```

Pushing the tag starts `.github/workflows/release.yml`, which:

1. fails immediately if the tag does not match the `Cargo.toml` version,
2. runs the test suite on Linux, macOS and Windows,
3. builds and packages five targets with SHA-256 sums, and
4. waits for an approval in the protected `release` environment before it
   publishes anything.

The tag alone cannot publish. Nothing is public until the approval is given.

## 8. Approve the GitHub release

Approve the pending `release` environment deployment in the workflow run. The
publish job creates the GitHub release with the packaged archives and their
checksums attached.

## 9. Publish to crates.io

This is manual. No workflow does it.

```console
git switch main && git pull
cargo publish --locked --dry-run
cargo publish --locked
```

Publish only after the GitHub release exists, so the two never disagree. A
crates.io version cannot be replaced, only yanked, so the dry run is not
optional.

## 10. Confirm

- `cargo install foremerge` from a clean environment installs the new version
  and `foremerge --version` reports it.
- The release page lists all five archives and their `.sha256` files.
- `foremerge doctor` is healthy against a ledger created by the new build.

## Hotfixes

A hotfix is an ordinary patch release: branch from the tag if `main` has moved
on, fix, follow this checklist from step 2, and merge back to `main`.

Yank a crates.io version only when it is actively harmful, for example a build
that corrupts a ledger. Yanking does not delete it and does not break anyone who
already has it in a lockfile. Follow it with a fixed release, and say what
happened in the changelog.

## Known gaps

Two steps are not automated, and both are places a release can go wrong quietly:

- **crates.io publishing is manual** (step 9). A `publish-crate` job in
  `release.yml`, gated on the same `release` environment and holding a
  `CARGO_REGISTRY_TOKEN` secret, would make it impossible to forget while
  keeping the approval requirement.
- **The GitHub release notes are autogenerated.** The publish job passes
  `--generate-notes`, which emits a commit list and ignores `CHANGELOG.md`.
  Extracting the section instead would put the written entry on the release
  page:

  ```console
  awk '/^## \[<version>\]/{f=1;next} /^## \[/{f=0} f' CHANGELOG.md > notes.md
  ```
