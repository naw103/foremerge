# State model

Foremerge records two kinds of state:

1. durable semantic history in its append-only event journal; and
2. queryable projections of agents, intents, claims, changesets, validations,
   conflicts, messages, and graph relationships.

Git remains the authoritative state for files, trees, commits, refs, and
worktrees.

## Core entities

### Agent

An agent is one registered execution identity. Its record includes a display
name, optional model, capabilities, worktree, Git branch and head when
discoverable, status, and registration time.

Registration identifies the actor attached to later intent, claim, changeset,
validation, and coordination events. It is provenance, not authentication.

### Task

A task is a stable user-facing key and title. Publishing an intent creates or
reuses the task. Multiple intents can therefore reveal duplicate or competing
approaches to the same requested outcome.

### Intent

An intent describes planned work before implementation finishes:

- owning agent and task;
- concise summary;
- optional rationale;
- semantic scopes;
- caller-supplied dependency identifiers;
- free-form metadata;
- lifecycle status.

The intent is the lifecycle aggregate in the MVP. A task may have several
intents, and an intent may have several claims and changesets.

### Claim

A claim associates an agent and intent with one semantic scope. Claims have a
lease expiry and remain advisory. Two overlapping active claims may coexist;
Foremerge reports the overlap so agents can coordinate.

### ChangeSet

A ChangeSet is a provenance-rich description of provisional implementation:

- agent, task, and intent;
- files, symbols, and contracts affected;
- dependencies;
- test evidence;
- decisions and alternatives;
- caller-provided provenance;
- base/current Git refs plus distinct immutable `accepted_commit` and later
  `integration_commit` pins when available;
- a content fingerprint;
- lifecycle status.

ChangeSets are separate durable records. Publishing another ChangeSet while an
intent is `PROVISIONAL` or `VALIDATED` creates a revision: the new record names
the prior `supersedes_changeset_id`, the prior ChangeSet becomes `SUPERSEDED`,
and the intent returns to `PROVISIONAL` so validation must run again.

### Validation

A validation records an executed argument vector, result, captured output,
duration, and the exact ChangeSet fingerprint it tested. A passing result only
applies to that fingerprint.

### Conflict

A conflict connects a candidate or persisted source intent to a target intent.
It records kind, severity, score, optional shared scope, explanation,
coordination suggestion, evidence, status, and detection time.

### Decision

A decision preserves the rationale and rejected alternatives attached to an
intent or ChangeSet. Decisions make conflict resolution and architecture choices
inspectable later.

### Coordination message

A coordination message is durable agent-to-agent context. It may reference a
conflict or ChangeSet. The MVP records and queries messages; it is not a live
chat transport.

## Happy-path lifecycle

```text
INTENT
  |
  | claim one or more semantic scopes
  v
CLAIMED
  |
  | implementation begins
  v
IN_PROGRESS
  |
  | publish ChangeSet
  v
PROVISIONAL
  |
  | execute a passing validation on the current fingerprint
  v
VALIDATED
  |
  | pass acceptance gates
  v
ACCEPTED
  |
  | associate durable integration commit
  v
COMMITTED
```

These labels describe the protocol model. Refer to the CLI and API references
for the transitions currently exposed by the MVP; Foremerge does not invent a
Git integration commit on the user's behalf.

## Transition invariants

### Intent to claimed

- The agent and intent must exist.
- The claimant must own the intent.
- CLI, MCP, and structured HTTP scopes must follow the supported semantic
  vocabulary and contain a non-empty key.
- Claims receive a finite lease.
- Overlap creates warnings, never a lock failure.

### Claimed to in progress

Beginning implementation is a semantic boundary, not a stream of edits. An
agent need not publish keystrokes or every filesystem mutation.

### In progress to provisional

Publishing a ChangeSet records implementation summary and provenance. The
server derives Git context where possible and computes the stored fingerprint.
Reported tests are evidence supplied by the agent; they are distinct from
Foremerge-executed validation.

### Provisional to validated

- The requested command must be non-empty.
- The selected worktree must be available.
- The process must finish successfully within its timeout.
- The recorded validation fingerprint must equal the current ChangeSet
  fingerprint.

A later code or Git change detected by Foremerge's fingerprint makes the
validation stale. Ignored files remain a documented limitation in
[Git integration](git-integration.md).

A failed validation leaves, or returns, both the current ChangeSet and intent to
`PROVISIONAL`. When it invalidates an earlier `VALIDATED` state, Foremerge emits
`validation.invalidated`.

### Validated to accepted

Acceptance requires:

- the latest validation to pass for the current fingerprint;
- the current worktree to be clean and retain that fingerprint;
- the selected ref to resolve to the validated worktree `HEAD`;
- every ChangeSet dependency to name an `ACCEPTED` or `COMMITTED` intent whose
  stored `accepted_commit` is in the candidate's Git ancestry, with its
  namespaced accepted ref still matching that pin; and
- no open or coordinating high-severity conflict, unless the caller supplies
  both the explicit override and a non-empty reason. Foremerge records that
  reason as a decision and marks the affected findings `OVERRIDDEN`.

Acceptance creates `refs/foremerge/accepted/<changeset-id>` without merging or
moving a branch, and stores the same hash as `accepted_commit`.

### Accepted to committed

`COMMITTED` means the caller recorded a Git ref that resolves to a commit and
Git proves the immutable `accepted_commit` is its ancestor. Foremerge stores
that later hash as `integration_commit`; recording it never replaces the
accepted pin. The MVP does not prove that the commit landed on a particular
target branch. Automatic landing is outside its scope.

## Auxiliary states

Agents and coordination messages use their own small statuses, such as active
agent registration, active claims, open conflicts, and pending messages. These
must not be confused with the intent/ChangeSet lifecycle.

Claim expiry is time-based advisory state. Expiring a claim does not delete its
history or discard its intent. Work-query and graph responses report an elapsed
active lease as effective status `EXPIRED` without turning a GET/read-only tool
into a database mutation.

`SUPERSEDED` is a ChangeSet status, not an intent lifecycle state. It preserves
the older implementation snapshot when a new revision becomes current.

## Validation freshness

Conceptually, a validation key is:

```text
(changeset_id, changeset_fingerprint)
```

Acceptance selects the latest validation and requires it to be passing with the
same fingerprint. A pass for an older fingerprint remains useful provenance but
cannot gate changed work.

## Conflict gating

Conflict discovery and lifecycle state are related but separate:

- low and medium findings inform coordination;
- high findings make `blocking` true in a conflict report;
- claims are still granted;
- acceptance checks current high findings;
- an explicit override requires a reason, which becomes a durable decision and
  `conflict.overridden` event;
- resolving an agreement marks the finding `RESOLVED`; and
- discarding linked work marks its open findings `DISMISSED`, preserving the
  original evidence while removing the acceptance gate.

This preserves cheap speculation while making risky integration deliberate.

## Event invariants

Every stored event row contains:

- monotonic local sequence number;
- globally unique event ID;
- schema version (currently internal and omitted from public `Event` JSON);
- event and entity types;
- entity ID and optional acting agent;
- JSON payload;
- RFC 3339 creation time;
- previous event hash and current event hash.

The first event points to `GENESIS`. Each later hash includes the previous hash,
event ID, schema-version constant, event type, entity type and ID, actor,
timestamp, and serialized payload. The database sequence number is not hash
material. The log is append-only at the database level.

The chain verifier pages through the complete retained event sequence. The
chain cannot prove tail completeness without an external checkpoint.

The chain proves internal continuity, not that an agent's self-reported model or
prompt was externally authenticated.

## Graph relationships

The materialized graph provides an inspectable version of the protocol model.
The MVP materializes these relationships:

```text
agent      --WORKS_ON--------> task
task       --HAS_INTENT------> intent
intent     --MAKES_CLAIM-----> claim
claim      --CLAIMS----------> scope or symbol
scope      --AFFECTS_DEPENDENCY--> dependency declaration
intent     --DEPENDS_ON------> dependency declaration
intent     --PRODUCES--------> changeset
changeset  --HAS_PROVENANCE--> provenance
changeset  --REPORTS_TEST----> reported test --HAS_RESULT--> result
changeset  --RUNS_TEST-------> executed test --HAS_RESULT--> result
changeset  --RECORDS_DECISION> decision
intent     --HAS_CONFLICT----> conflict --CONFLICTS_WITH--> intent
conflict   --RESOLVED_BY-----> decision
```

Dependency nodes contain caller-supplied identifiers; publication does not
require them to resolve to existing intents. Clients should use typed API
responses for core behavior and use the graph snapshot for exploration and
visualization.

## Concurrency model

Foremerge uses one SQLite connection protected by a process mutex and immediate
write transactions. In-process reads and writes serialize on that mutex; a read
sees committed state after acquiring it. Competing writers across processes are
serialized by SQLite before checking current state, updating projections,
linking graph nodes, and appending events.

This is sufficient for a local team of agents. It is not a claim of distributed
consensus across independent databases.
