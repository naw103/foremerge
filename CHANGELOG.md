# Changelog

All notable changes to Foremerge will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project intends to follow [Semantic Versioning](https://semver.org/) once its
public protocol begins releasing. Before 1.0, minor versions may contain breaking
changes when they are called out here with a migration note.

## [Unreleased]

### Added

- `foremerge status`: one human-first screen answering "what are my agents
  doing right now" with active agents, intents grouped by lifecycle status,
  unexpired claims with their scopes, OPEN or COORDINATING conflicts naming
  both parties and both sides' scopes, and ChangeSets grouped by status with
  ids for the non-terminal ones. The default output is readable aligned plain
  text; `--json` returns the typed report in the standard envelope. The whole
  report is read in one transaction, so its sections describe one consistent
  moment.

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

[Unreleased]: https://github.com/naw103/foremerge/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/naw103/foremerge/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/naw103/foremerge/releases/tag/v0.1.0
