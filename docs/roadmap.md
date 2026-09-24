# Roadmap

Foremerge is building the smallest credible coordination layer above Git. The
roadmap is ordered by evidence and interoperability, not dates. Items are plans,
not promises, unless a release note marks them shipped.

## Shipped local proof (through 0.5.0)

The current local release establishes the core thesis on one developer machine:

- a local Rust daemon backed by SQLite;
- one coordination database shared across isolated Git worktrees;
- append-only, hash-chained semantic events;
- the Agent → Task → Intent → Claim → Symbol → Dependency → ChangeSet → Test →
  Result → Decision → Provenance graph;
- advisory semantic claims rather than hard locks, renewed by re-claiming a
  scope, and transferable with `work adopt` when an agent stops mid-task;
- declared scope operations (`add`, `extend`, `modify`, `replace`, `remove`,
  `rename`, `migrate`), so a finding compares two stated operations on one
  stated scope instead of reading a summary for verbs;
- findings that separate what Foremerge asserts from what it surfaces: HIGH is
  reached only by a declared operation on a canonical scope, and an inferred
  operation or a fuzzy scope match is capped below it as a candidate;
- deterministic intent-conflict and duplicate-work warnings, `related_work` on
  publish, and `record_assessment` for the agent's own verdict, rationale, and
  action;
- MCP, CLI, and versioned JSON API access to the same service, across 18 MCP
  tools whose descriptions and parameters are documented and size-budgeted;
- verification-gated ChangeSet acceptance, where MCP and the HTTP validate
  endpoint run only a check registered by name, at the root of the ChangeSet's
  worktree, rather than an argument vector from the request. The CLI still takes
  a command directly, as the operator surface;
- a strict or advisory check policy, with unverified overrides available to
  operators on the CLI and HTTP API and refused over MCP;
- upgrades that fail closed and recover: the schema stamp is re-read on every
  call, an MCP server reports a ledger it cannot open through the handshake and
  every tool call rather than exiting into a closed connection, and
  `foremerge ledger reset` sets a ledger aside or restores a backup into the
  running build's schema;
- a reproducible two-agent demo and benchmark specification;
- split process liveness, bounded readiness, and authenticated paged audit;
- immutable validation attempts and conflict-detection occurrences;
- digest-bound validation exclusions for generated untracked output;
- sargable indexed work queries with a reproducible timing harness; and
- Linux, macOS, and Windows portability gates before binary release.

Exit criteria are behavioral: real MCP and HTTP clients must exercise the
interfaces, two independent processes must share state from different worktrees,
the PaymentService conflict must be detected before a code diff, and failed
validation must leave the target Git ref unchanged.

## Next: protocol fidelity and integrations

- Publish versioned JSON Schemas for events, entities, and all 18 MCP
  tools.
- Add export/import and migration tooling for local provenance.
- Add language adapters for symbols and references while retaining manual scopes
  for APIs, schemas, config, infrastructure, tests, migrations, and environment
  variables.
- Improve conflict explanations, confidence calibration, resolution recording,
  and negative-control coverage.
- Add explicit claim release and stale-agent handling. Claim renewal, intent
  adoption, and recovery diagnostics shipped across 0.4.x.
- Support the 2026-07-28 MCP protocol revision alongside 2025-11-25, rather than
  declining it ([#23](https://github.com/naw103/foremerge/issues/23)).
- Add `foremerge update`, and tell an installation that a newer release exists
  ([#33](https://github.com/naw103/foremerge/issues/33)).
- Community-raised during launch week (August 2026), in design order:
  - `scope_drift`: derive touched paths and symbols from the candidate diff at
    ChangeSet publish and compare them against declared scopes, raising a
    finding that can gate acceptance. Closes the declared-versus-touched gap
    the limitations doc currently assigns to provenance.
  - An approval-policy layer: declare which operations, verdicts, or lifecycle
    transitions require a recorded human acknowledgement (for example,
    destructive operations on `contract:` scopes, or graduating an
    experiment). Composes with immutable intents: approvals bind to intent ids.
  - Experiment intents: a declared exploratory state with downgraded
    cross-severity and sanctioned parallel attempts, which cannot reach
    acceptance and must graduate through a fresh full-severity declaration.
  - Scope lineage: rename and extraction relations between scopes, and
    explicit supersedes edges when a changed plan is republished.
- Continue compatibility testing across releases of Codex, Claude Code, and
  Cursor. Native skills, project templates, setup, and diagnostics shipped in
  0.2.0; a Claude Code plugin with its marketplace entry, and the portable
  `.agents/skills` location alongside the three client-specific ones, shipped in
  0.4.1.
- Build the paired model-driven benchmark runner described in
  `benchmark-plan.md` and publish raw pilot results before making quantitative
  claims. The scripted five-scenario correctness runner and query microbenchmark
  are already committed.

## Later: deeper coordination on one machine

- A subscription interface for semantic events on the local store, so an agent
  learns that a later intent collided with its own without re-running a check.
  `work watch` polls today.

Foremerge coordinates agents that share one machine and one Git repository, and
that is the scope of this repository. Earlier versions of this roadmap listed
cross-machine operation and the policy and retention features that go with it;
they are removed rather than left as commitments this project has not made.

What that scope rules out is not a small matter of effort. Agent identity over
MCP and the loopback API is self-asserted, the local SQLite database is not a
distributed database and will not be presented as one, and coordination between
machines needs an identity boundary, a threat model and a concurrency design
that none of the above provides.

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
