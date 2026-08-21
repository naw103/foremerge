# Roadmap

Foremerge is building the smallest credible coordination layer above Git. The
roadmap is ordered by evidence and interoperability, not dates. Items are plans,
not promises, unless a release note marks them shipped.

## Current focus: local proof

The first release is intended to establish the core thesis on one developer
machine:

- a local Rust daemon backed by SQLite;
- one coordination database shared across isolated Git worktrees;
- append-only, hash-chained semantic events;
- the Agent → Task → Intent → Claim → Symbol → Dependency → ChangeSet → Test →
  Result → Decision → Provenance graph;
- advisory semantic claims rather than hard locks;
- deterministic intent-conflict and duplicate-work warnings;
- MCP, CLI, and versioned JSON API access to the same service;
- verification-gated ChangeSet acceptance; and
- a reproducible two-agent demo and benchmark specification.

Exit criteria are behavioral: real MCP and HTTP clients must exercise the
interfaces, two independent processes must share state from different worktrees,
the PaymentService conflict must be detected before a code diff, and failed
validation must leave the target Git ref unchanged.

## Next: protocol fidelity and integrations

- Publish versioned JSON Schemas for events, entities, and all 13 MCP
  tools.
- Add export/import and migration tooling for local provenance.
- Add language adapters for symbols and references while retaining manual scopes
  for APIs, schemas, config, infrastructure, tests, migrations, and environment
  variables.
- Improve conflict explanations, confidence calibration, resolution recording,
  and negative-control coverage.
- Add claim renewal, explicit release, stale-agent handling, and better recovery
  diagnostics.
- Continue compatibility testing across releases of Codex, Claude Code, and
  Cursor; native skills, project templates, setup, and diagnostics shipped in
  0.2.0.
- Build the paired benchmark runner described in `benchmark-plan.md` and publish
  raw pilot results before making quantitative claims.

## Later: optional shared coordination

- An authenticated shared daemon for agents on different machines.
- A transport-neutral subscription interface for semantic events.
- Replication, backup, access-control, retention, and redaction policies.
- Git-hosting status checks and review summaries that reference ordinary commits
  and branches.
- Team policy for required validation without making Foremerge a merge queue or
  source-control replacement.

Shared mode will require a separate threat model and concurrency design. The
local SQLite database is not a distributed database and will not be presented as
one.

## Research track

- Cross-language contract and data-flow relationships.
- Calibrated semantic-conflict evaluation on real repository histories.
- Coordination strategies for speculative agents and dependency DAGs.
- Privacy-preserving provenance summaries and selective disclosure.
- Measurements of coordination overhead, alert fatigue, discarded work, and
  post-integration failures across models.

## Deliberate non-goals

- Replacing Git objects, refs, branches, commits, or GitHub workflows.
- Requiring agents to edit one shared filesystem.
- Streaming keystrokes or synchronizing editor buffers.
- Treating CRDT or OT convergence as proof that code is correct.
- Hard-locking a semantic scope.
- Automatically trusting or landing a ChangeSet because an agent says its tests
  passed.
- Building a new storage engine before SQLite provides evidence that one is
  necessary.

See `limitations.md` for what the current implementation does not guarantee and
`CHANGELOG.md` for behavior that has actually shipped.
