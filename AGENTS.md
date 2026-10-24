# Foremerge: guide for coding agents

Foremerge is a local, Git-compatible coordination layer for parallel coding
agents: a CLI, an MCP server and a loopback JSON API over one SQLite ledger per
repository. This file is the short version for an agent working in this
repository; `CLAUDE.md` imports it. [`CONTRIBUTING.md`](CONTRIBUTING.md) is the full contributor guide
and [`docs/RELEASING.md`](docs/RELEASING.md) is the release checklist.

The repository is public. Everything tracked here is published.

## Rules

1. **Changes reach `main` through a reviewed pull request.** Work on a branch
   or a fork and open a pull request. Tagging, releases, registry publishing
   and the website are the maintainer's: a pushed `v*` tag starts the release
   workflow and a push to `gh-pages` publishes foremerge.com. See
   [How releases and deploys run](#how-releases-and-deploys-run).
2. **Never run a development build against a real repository.** A development
   build can leave a ledger at a schema no released binary reads: it creates a
   new ledger at its own schema, `ledger migrate` and `ledger reset` move an
   existing one to it, and builds before 0.6.0 migrated any ledger they opened,
   one way. Every released binary then refuses that ledger. That covers
   `target/debug/foremerge`, `target/release/foremerge`, the `fmg` twins and
   `cargo run`. `--cwd` defaults to the current directory, so a run from inside
   another repository acts on that repository's ledger. Use the test suite,
   which works in temporary directories, or a throwaway repository with `--cwd`
   and `--database` both pointing at it. Never `cargo install --path .` and
   never point an agent client's configuration at `target/`.
3. **A ledger is the user's data.** Do not delete, move, reset, migrate or
   hand-edit `<git-common-dir>/foremerge/state.sqlite3`. To inspect one, skip
   the CLI and open it read-only:
   `sqlite3 "file:<path>?mode=ro" "select ..."`. If the Foremerge MCP server
   reports that it is unavailable, say so and leave the fix to the user.
4. **Nothing private in tracked files.** The repository is public: no
   secrets, tokens, internal hostnames, private process notes, generated
   databases or `.foremerge` state.
5. **Some paths need the maintainer's review.** [`.github/CODEOWNERS`](.github/CODEOWNERS)
   lists them: the agent guides and client integration, CI and the release
   workflow, distribution, the ledger and acceptance gate, and authentication.
   If a change touches one, say so in the pull request.

### Coordinating through Foremerge

This repository uses Foremerge on itself. [`.mcp.json`](.mcp.json) starts
`foremerge mcp` for clients that read it. If you have Foremerge installed,
follow the `foremerge` skill's "Coordinate before editing" steps before you
change files, documents included, and give each subagent or worktree its own
agent and intent: work that is never declared cannot be seen by anyone else's.

## Stack

- Rust, edition 2024, minimum supported version 1.85 (pinned in `Cargo.toml`,
  the `Makefile` and CI). One crate, `foremerge`: a library and two binaries,
  `foremerge` and `fmg`, which are thin wrappers over `foremerge::cli::run`.
- SQLite through bundled `rusqlite`. Tokio, Axum for the loopback JSON API,
  clap for the CLI, MCP over stdio.
- `npm/` holds the npm launcher and its package builder: plain Node 18+,
  no build step.

| Path | What lives there |
| --- | --- |
| `src/cli.rs` | commands, `doctor`, `setup`, the repository token |
| `src/service.rs` | the lifecycle: intents, claims, ChangeSets, the acceptance gate |
| `src/db.rs` | schema, migrations, the append-only event chain |
| `src/ledger.rs` | `ledger reset` and restore |
| `src/conflict.rs` | conflict detection |
| `src/checks.rs` | named verification checks, which run commands |
| `src/mcp.rs`, `src/api.rs` | the MCP tool surface and the JSON API with its bearer token |
| `src/integrations.rs` | `foremerge setup`: writes agent client configuration, embeds the skill |
| `tests/` | integration suites; `benchmarks/scenarios` is the ground-truth corpus |

## Commands

```console
make verify      # fmt-check, check, clippy, test, query-smoke, doc: the local gate
make msrv        # clippy and tests again on Rust 1.85.0, as CI does
make npm-test    # npm launcher and package builder (Node 18+, tar, zip)
make benchmarks  # the five committed benchmark scenarios
```

The Cargo commands behind them:

```console
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
cargo run --quiet --example query_benchmark -- 200
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo build --workspace --all-targets --all-features
```

One suite or one test:

```console
cargo test --test e2e
cargo test --test e2e <test_name> -- --exact --nocapture
cargo test --lib <module>::
```

## Conventions an agent gets wrong

- **A green `make verify` does not predict a green CI run.** Clippy's lint set
  differs between 1.85 and stable, in both directions. Run `make msrv` too.
- **`cargo test` only compiles the query example.** `make verify` runs it
  (`query-smoke`). A schema change can break its seed while every test passes.
- **A shared `target/` gives phantom failures.** Two sessions building at once
  can leave a stale binary that an end-to-end test then spawns. Set a private
  `CARGO_TARGET_DIR` when anything else is building here.
- **The skill exists in five copies.** `.codex/skills/foremerge/SKILL.md` is
  canonical and the other four are byte-for-byte copies.
  `tests/skill_parity.rs` fails on drift. See `CONTRIBUTING.md`.
- **The version string lives in seven files plus the lockfile.** Do not bump
  it outside a release commit. User-visible changes go under `Unreleased` in
  `CHANGELOG.md`, in the same pull request as the change.
- **A ledger schema bump is a breaking change.** An older binary refuses a
  migrated ledger. It needs a minor version and a migration note, and it is
  worth an issue before the implementation.
- **Integrity code needs more than a green gate.** For migrations, the
  acceptance gate and the append-only tables: reproduce the defect before
  fixing it, re-run the reproduction after, and test both the allowed
  transition and the rejected one. Failed validation must never move a
  ChangeSet to an accepted or committed state.
- **Conflict detection changes need a positive fixture and an
  independent-intent negative control.**
- **Claims are advisory.** New behavior warns and helps agents coordinate. It
  does not introduce hard locks.
- **The default local workflow stays local.** No network service, hosted
  dependency, model API or telemetry.
- **Tests that create repositories** use temporary directories and set a
  repository-local Git identity. They never depend on global Git
  configuration.
- **`main` is linear.** No merge commits.
- **Commit messages** are one plain imperative sentence ("Fix the npm channel
  review findings"), with no type prefix. No AI attribution: no
  `Co-Authored-By` trailer for a tool and no generated-with line in a pull
  request body.
- **Prose** in the README, docs, changelog and site copy uses no em dashes.

## How releases and deploys run

Nothing deploys from `main`. A push or pull request there runs
`.github/workflows/ci.yml` and nothing else.

| Trigger | What it publishes | Gate |
| --- | --- | --- |
| push to `gh-pages` | the website at foremerge.com, through GitHub Pages | none on the host: maintainer only |
| push of a `v*` tag | `.github/workflows/release.yml`: version check, tests on three platforms, five release archives, then the GitHub release | the publish job waits for the maintainer's approval in the `release` environment |
| `cargo publish` | the crate on crates.io, which cannot be replaced | manual, maintainer only |
| `npm publish` | the launcher and five platform packages | manual, maintainer only, second factor |
| `mcp-publisher publish` | the MCP registry listing from `server.json` | manual, maintainer only |

The order, the files to bump and the checks between steps are in
`docs/RELEASING.md`. An agent may prepare a release in the working tree when
asked. Every step in the table is the maintainer's.
