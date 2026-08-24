# Architecture

Foremerge is a local-first coordination layer above Git. Git remains the durable
source of code, history, branches, and worktrees. Foremerge records the semantic
context that Git does not: which agent intends to do what, which conceptual
scopes it has claimed, how work depends on other work, which conflicts were
found, what was validated, and why a change was accepted.

The MVP is deliberately a single Rust binary backed by SQLite. It does not
replace Git, synchronize file contents, stream edits, or attempt distributed
text convergence.

## System shape

```text
                     semantic requests
  CLI --------------------------------------------------+
  MCP client -- stdio JSON-RPC --> MCP adapter ---------+--> Foremerge service
  HTTP client ------------------> JSON API -------------+         |
                                                                  +-- SQLite
                                                                  +-- git CLI
                                                                  +-- test processes

  agent worktree A ----------- Git common repository -------- agent worktree B
          |                                                        |
          +---------------- isolated working files ----------------+
```

All transports call the same `Foremerge` application service. Business rules do
not live in the CLI, HTTP handlers, or MCP request loop. This keeps behavior
consistent regardless of how an agent connects.

## Source layout

The crate exposes a library and the `foremerge` binary:

```text
src/
  main.rs      command-line entry point and process startup
  lib.rs       public library surface
  model.rs     request, response, and domain data types
  db.rs        SQLite schema, transactions, event log, and graph storage
  service.rs   coordination use cases and lifecycle gates
  conflict.rs  deterministic intent analysis and conflict rules
  git.rs       repository discovery, snapshots, and command execution
  exclusions.rs operator-owned validation exclusion policy
  checks.rs    private named-verification registry
  integrations.rs safe Codex, Claude Code, and Cursor installation/diagnostics
  api.rs       Axum JSON API
  mcp.rs       MCP stdio JSON-RPC adapter
```

Foremerge is one package rather than a multi-crate workspace. The module
boundaries are intentional seams; they can become separate crates if a stable
SDK or alternate storage adapter eventually justifies the build complexity.

## Runtime data

Foremerge discovers the repository through Git and keeps coordination state out
of the checked-out files. For linked worktrees, Git's common directory is the
stable rendezvous point: each worktree has isolated files but resolves the same
coordination database.

The database uses:

- foreign-key enforcement;
- WAL journal mode;
- `NORMAL` synchronous mode;
- a ten-second busy timeout;
- a full-mutex SQLite connection shared by the application service.

This is intended for low-volume local semantic events, not high-rate telemetry.
A committed query microbenchmark makes scaling measurements reproducible, but
no deployment-scale performance claim is implied. There is no broker,
background queue, or separate graph database in the MVP.

## Persistence model

SQLite contains three complementary forms of state.

### Domain projections

`agents`, `tasks`, `intents`, `intent_scopes`, `claims`, `changesets`,
`validations`, `validation_attempts`, `decisions`, `conflicts`,
`conflict_detections`, and `coordination_messages` provide direct queries for
current coordination state and immutable observations. Order-compatible intent
indexes and the normalized `intent_scopes` projection keep work filters
sargable; reverse dependencies are scanned once per query rather than once per
returned intent.

### Semantic graph

`graph_nodes` and `graph_edges` materialize relationships among agents, tasks,
intents, scopes, changesets, tests, decisions, and Git provenance. The graph is
not a second database or an AST index. It is a query-friendly view of semantic
facts published through the protocol.

### Append-only journal

Every semantic mutation appends an `events` row in the same immediate
transaction as its projection update. Events have a monotonic sequence number,
schema version, actor, entity identity, JSON payload, previous hash, and SHA-256
event hash. SQLite triggers reject updates and deletes.

The chain detects modification inside the retained chain; without an external
checkpoint it cannot prove that the tail was not truncated. It is not a digital
signature and does not establish the identity of a remote actor. See
[Protocol](protocol.md) for event semantics.

## Write consistency

Mutation flow is:

```text
validate request
    -> BEGIN IMMEDIATE
    -> read current state
    -> enforce lifecycle and ownership rules
    -> update projections and graph
    -> append event using the previous event hash
    -> COMMIT
```

`BEGIN IMMEDIATE` serializes competing writers before they inspect coordination
state. This is important for intent publication: two agents publishing at the
same time cannot both scan a stale view and miss one another.

Git and process execution cannot participate in a SQLite transaction.
Foremerge therefore records the exact Git fingerprint associated with a
validation and checks it again at acceptance. A changed worktree or ref makes
old evidence stale rather than silently applying it to new code.

Validation uses an explicit split phase: load state, snapshot, and run the
process without a write transaction; capture the final snapshot; then open one
short immediate transaction to reload lifecycle state, append the immutable
attempt, and apply an authoritative result only if every check still matches.

## Component responsibilities

### Application service

The service owns registration, intent publication, claims, queries, conflict
checks, changeset publication, validation, acceptance, messages, graph reads,
and event reads. It is the only layer allowed to coordinate a projection update
with an event append.

### Conflict engine

The engine compares intent language and structured semantic scopes. It uses
deterministic rules and returns evidence with every result. Changed files are
not currently an input to intent conflict scoring. See
[Conflict detection](conflict-detection.md).

### Git adapter

The Git adapter shells out with argument arrays, discovers worktrees and the Git
common directory, and captures reproducible fingerprints. The application
service executes validation commands in a selected worktree. Git remains
authoritative for code and commit identity. See
[Git integration](git-integration.md).

### HTTP and MCP adapters

The JSON API is useful for local scripts and language-independent integrations.
The MCP adapter exposes the same operations to coding agents over standard I/O.
Neither transport contains an independent coordination implementation.
MCP verification resolves a trusted check name through repository-private
configuration before calling the same validation service used by other
frontends; raw validation argv is not accepted from MCP.

Axum liveness performs no I/O, readiness uses a non-waiting store probe, and
full event-chain audit is authenticated and paged through a separate read-only
connection. Synchronous SQLite and Git service calls are dispatched with
`spawn_blocking` so they do not occupy Tokio worker threads. SIGINT/SIGTERM stop
new HTTP work and bound in-flight draining to 30 seconds; validation cancellation
terminates the subprocess tree. The daemon owns its own Tokio runtime so the
bound is a process-exit bound: when the grace expires it abandons the remaining
requests, gives uncancellable blocking work 5 further seconds, and exits 1
(a clean drain exits 0). A child process started outside a validation guard, for
example a wedged `git` call, is not killed and may outlive the daemon.

## Trust and safety boundaries

- Claims are advisory. Foremerge warns; it does not lock files or symbols.
- Validation commands execute local programs and therefore inherit the user's
  operating-system permissions. Run only commands you trust.
- The local HTTP listener is not a production multi-tenant security boundary.
- Provenance is self-reported unless Foremerge derives it from Git or executes
  the validation itself.
- Captured command output may contain sensitive data. Avoid printing secrets in
  validation commands or provenance fields.
- All Git and child-process arguments are passed as argument vectors, not
  interpolated into a shell command.

## MVP boundaries

The current architecture does **not** provide:

- a hosted or cross-machine coordination service;
- SSE, WebSocket push, or presence streaming;
- daemon discovery or automatic daemon startup;
- automatic rebasing, merging, cherry-picking, or branch landing;
- mandatory AST extraction or language-server integration;
- embeddings or an LLM dependency for conflict detection;
- cryptographic signatures or access-control roles.

These are extension points, not prerequisites for proving the thesis. The MVP
proves earlier semantic conflict detection and richer provenance while keeping
Git fully usable on its own.

## Decisions and trade-offs

| Decision | Why now | Cost |
| --- | --- | --- |
| SQLite | Zero infrastructure, transactional local state | A single local writer; no native cross-machine sharing |
| Git CLI | Matches the Git users already have | Subprocess and porcelain parsing overhead |
| Soft semantic claims | Preserves agent autonomy and speculation | Conflicts remain possible and require coordination |
| Deterministic conflict rules | Offline, cheap, explainable, testable | Lower recall than a mature semantic model |
| Materialized graph plus typed tables | Simple operational queries and graph inspection | Some facts are represented in two projections |
| Hash-chained events | Useful audit and corruption signal | Tamper evidence is not actor authentication |
| One Rust package | Fast build and simple installation | Internal APIs are not independently versioned |

## Growth path

Evidence should drive additions. Likely next seams are a pluggable intent
analyzer, symbol extractors, long-poll or push notifications, policy-configured
verification suites, and a shared HTTP deployment using the same protocol. A
new storage engine is warranted only when measured coordination load or
deployment requirements exceed SQLite's operating envelope.
