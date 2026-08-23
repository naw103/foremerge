# Changelog

All notable changes to Foremerge will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project intends to follow [Semantic Versioning](https://semver.org/) once its
public protocol begins releasing. Before 1.0, minor versions may contain breaking
changes when they are called out here with a migration note.

## [Unreleased]

## [0.3.0] - 2026-08-22

### Added

- `foremerge status`: one human-first screen answering "what are my agents
  doing right now" with active agents, intents grouped by lifecycle status,
  unexpired claims with their scopes, OPEN or COORDINATING conflicts naming
  both parties and both sides' scopes, and ChangeSets grouped by status with
  ids for the non-terminal ones. The default output is readable aligned plain
  text; `--json` returns the typed report in the standard envelope. The whole
  report is read in one transaction, so its sections describe one consistent
  moment.
- Immutable `validation_attempts` retain every completed command with explicit
  authoritative/stale state, expected and observed fingerprints, bounded
  output, changed-path diagnostics, excluded paths, and policy digest.
- Stable conflict lifecycle identities now have immutable detection occurrences,
  `conflict.redetected` events, and `previously_settled` responses.
- Operator-only `validation-exclusions show|set` with exact/prefix untracked
  rules, normalized digest binding, atomic private storage, and ADR 0001. MCP
  intentionally has no policy-mutation tool.
- Authenticated paged event-chain audit over a separate read-only connection,
  exposed through HTTP and `events audit`.
- Read parity across HTTP and MCP for agent list, intent show, ChangeSet show,
  and the consistent status report, bringing the MCP surface to 17 tools.
- An executable five-scenario correctness corpus and an optimized query-work
  microbenchmark harness with JSON output.
- macOS and Windows CI/release test gates, including platform-specific
  validation-timeout cleanup coverage.

### Changed

- Breaking HTTP observability change: `/healthz` now reports process liveness
  only and performs no database or Git work. Use public `/readyz` for bounded
  non-waiting readiness and authenticated `/v1/audit/event-chain` for integrity
  audit. Migration: monitoring that consumed health counts or `event_chain_ok`
  must move to the typed read/audit routes.
- Breaking diagnostic change: `foremerge doctor` is strictly read-only. It no
  longer initializes or migrates a missing database and instead returns
  `database_ok: false` with `foremerge init` as the next step.
- HTTP and MCP dispatch synchronous SQLite/Git service operations through
  Tokio's blocking pool. SIGINT/SIGTERM drain in-flight HTTP requests for at
  most 30 seconds and terminate remaining validation process trees.
- Work queries use dynamic indexed SQL, a normalized `intent_scopes`
  projection, SQL-side limits, cached statements, and one reverse-dependency
  scan per request.
- Validation captures the final Git snapshot before opening its short write
  transaction, then rechecks current revision and lifecycle state atomically.
- Symbol inference provenance reports when the 4 MiB diff capture was
  truncated, with captured and total byte counts.

### Fixed

- A passing check that generated an untracked artifact no longer disappears:
  the stale attempt is queryable and names the path while remaining unable to
  satisfy acceptance.
- Redetecting a resolved, overridden, or dismissed conflict no longer rewrites
  its original evidence or silently reopens it.
- Full event-chain verification no longer holds the coordinator's shared mutex
  or runs through the unauthenticated liveness route.

### Migration

- Opening a tagged 0.1/0.2 database creates and backfills
  `validation_attempts`, `conflict_detections`, and `intent_scopes`, plus the
  order/filter indexes used by the 0.3 query path. Existing authoritative
  validations and conflicts become legacy immutable observations.

## [0.2.0] - 2026-08-21

### Added

- Native Foremerge skills and MCP setup for Codex, Claude Code, and Cursor.
- Safe `foremerge setup codex|claude|cursor|all` installation with explicit
  `--force` replacement and `doctor --client` diagnostics.
- Repository-scoped trusted checks configured through `foremerge checks`.
- Complete 13-tool MCP lifecycle, including start, resolution, named
  verification, acceptance, discard, and integration-commit recording.
- Read commands `foremerge intent show <ID>` and `foremerge agent list`.
- `--base-ref` on `changeset publish`; the diff base defaults to the candidate
  commit's first parent and provenance records a real diff hash and the base
  resolution mode.
- `work start` and `changeset publish` responses report `open_conflicts` for
  the intent, so an earlier publisher learns about conflicts created by later
  publishes.
- `coordinate inbox` accepts `--agent` alongside the positional agent id.

### Changed

- Breaking: `work discard` (CLI, HTTP, and MCP) now rejects an empty reason.
  Migration: pass a non-empty `--reason`/`reason` value.
- Over MCP, `accept_changeset` no longer accepts HIGH-conflict overrides
  (overrides are CLI operator actions) and `resolve_conflict` is limited to
  agents that are parties to the conflict.
- The trusted-check registry requires a real Git repository and is resolved
  from the bound repository, never from the MCP server's process working
  directory or a `.foremerge` fallback directory.
- ChangeSet `base_ref` records the diff base instead of repeating the
  candidate ref.
- `conflicts check --intent` rejects values shaped like intent ids and points
  the caller at `--intent-id`.

### Fixed

- Conflict explanations no longer extract sentence-starting English words as
  subjects (previously producing text like "will migrate `No`"); suggestions
  are scope-kind aware (migration ordering for schema/migration/config scopes,
  coordination-first wording for duplicate work), and conflict evidence names
  the overlapping scope from both sides.
- `setup --force` can repair a stale Foremerge MCP entry, and stale entries
  are no longer reported as configured by setup or `doctor`.
- Codex MCP registration is treated as user-global configuration: setting up a
  second repository now produces an explicit error, or a disclosed repoint
  with `--force`, instead of a false "configured" report or a silent hijack.
- `setup all` attempts every requested client and reports each result instead
  of aborting on the first failure.
- MCP config merges preserve the order of unrelated entries, dangling
  symlinks are refused, and client probes are time- and output-bounded.

## [0.1.0] - 2026-08-20

### Added

- Initial Rust crate and `foremerge` binary package metadata.
- Typed coordination models for agents, intents, semantic claims, conflicts,
  ChangeSets, validation, decisions, messages, and hash-chained events.
- Apache-2.0 licensing, contribution and security policies, CI, release checks,
  limitations, roadmap, and a reproducible benchmark specification.

[Unreleased]: https://github.com/naw103/foremerge/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/naw103/foremerge/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/naw103/foremerge/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/naw103/foremerge/releases/tag/v0.1.0
