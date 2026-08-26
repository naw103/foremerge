# Coordination protocol

Foremerge publishes semantic events at useful engineering boundaries. It does
not transmit keystrokes or synchronize working files.

```text
Agent -> Task -> Intent -> Claim -> Scope -> Dependency
      -> ChangeSet -> Test -> Result -> Decision -> Provenance
```

The protocol is transport-neutral. The MVP presents it through the `foremerge`
CLI, a local JSON API, and an MCP server over standard input/output.

## Protocol principles

1. Git owns code and durable commit history.
2. Agents work in isolated worktrees.
3. Semantic claims are advisory and leased, never file locks.
4. Conflicts should be visible before implementation completes.
5. Tests validate Foremerge's recorded worktree fingerprint, not an intent
   description.
6. Integration decisions retain agent, model, task, rationale, validation, and
   Git provenance.
7. Every mutation produces an append-only semantic event.

## Operation sequence

A normal session uses these domain operations:

```text
register_agent
publish_intent
check_conflicts
claim_work
query_work
coordinate_with_agent as needed
publish_changeset
validate changeset
resolve or explicitly override blocking conflicts
accept changeset
integrate with ordinary Git or a pull request
record the integration commit
```

The complete lifecycle and core read parity are available through 18 MCP tools:

- `accept_changeset`
- `check_conflicts`
- `claim_work`
- `coordinate_with_agent`
- `discard_work`
- `get_changeset`
- `get_intent`
- `list_agents`
- `publish_changeset`
- `publish_intent`
- `query_work`
- `record_assessment`
- `record_commit`
- `register_agent`
- `resolve_conflict`
- `run_verification`
- `start_work`
- `status`

`run_verification` accepts only a trusted check name from repository-private
Foremerge configuration. The CLI and HTTP API retain their direct argv
validation operation. Consult [MCP setup](mcp-setup.md), [agent client setup](agent-clients.md),
and [JSON API](json-api.md) for the exact shipped surfaces.

## CLI surface

The executable has three global options: `--json`, `--database <path>` (also
available as `FOREMERGE_DB`), and `--cwd <directory>`. Global options precede
the command. The MVP command tree is:

```text
foremerge init
foremerge setup codex|claude|cursor|all
foremerge doctor [--client codex|claude|cursor|all]
foremerge daemon [--bind <address>] [--no-auth]
foremerge mcp
foremerge checks set|list|remove
foremerge validation-exclusions show|set
foremerge agent register|list
foremerge intent publish|show
foremerge work claim|query|start|watch|discard
foremerge conflicts check|list|detections|resolve
foremerge changeset publish|show|attempts|validate|accept|commit
foremerge coordinate send|inbox
foremerge events list|audit
foremerge graph
foremerge status
foremerge worktree create
foremerge request get|post <path>
```

Use `foremerge <family> <command> --help` for argument spelling. In particular,
actor flags use `--agent` in the domain CLI, while HTTP/MCP JSON uses
`agent_id`. `coordinate inbox` accepts the agent id either positionally or via
`--agent`; if both are given they must agree. `work watch` is polling over the
event query; it is not a streaming transport. `request` is an authenticated
local HTTP escape hatch, not another implementation of the service.

`agent list`, `intent show <INTENT_ID>`, `changeset show <CHANGESET_ID>`, and
`status` have read-only HTTP and MCP equivalents. This keeps agents from
scraping events or graph output merely to recover typed current state.

`status` is also the human overview: one screen listing active agents,
intents grouped by lifecycle status, unexpired claims, OPEN or COORDINATING
conflicts with both parties named, and ChangeSets grouped by status with ids
for the non-terminal ones. All sections come from a single read transaction,
so they describe one consistent moment. The default output is aligned plain
text; `--json` returns the same typed report in the standard envelope.

## Agent registration

Registration input contains:

```json
{
  "name": "payments-stripe",
  "model": "optional model identifier",
  "capabilities": ["rust", "payments"],
  "worktree": "/absolute/or/relative/path"
}
```

Foremerge records Git branch and head when it can resolve the selected
worktree. The returned agent ID is required for actor-owned operations.

Register once per agent session rather than sharing one identity among multiple
parallel workers.

## Intent publication

An intent is useful before code exists:

```json
{
  "agent_id": "agt_...",
  "task": "payments-provider",
  "summary": "Replace PaymentService with StripePaymentService",
  "rationale": "Move the existing implementation behind Stripe",
  "scopes": [
    {"kind": "symbol", "key": "PaymentService", "operation": "replace"},
    {"kind": "domain", "key": "payments", "operation": "modify"}
  ],
  "depends_on": [],
  "metadata": {}
}
```

Each declared scope carries the operation this intent performs on it. `add`,
`extend` and `modify` preserve what other work depends on; `replace`, `remove`,
`rename` and `migrate` do not. The operation is declared rather than inferred
from the summary, because the agent knows which it means and English paraphrase
is not recoverable by keyword matching. An intent commonly treats its scopes
differently, which is why the operation belongs on the scope.

Publication persists the intent, updates the semantic graph, appends an event,
and checks it against active work. The response returns the intent, any
immediately detected conflicts, and `related_work`: other agents' active
intents that touch this one, each overlapping scope carrying both declared
operations and how they interact.

Foremerge states what overlaps. Whether that constitutes a conflict, a
duplicate or a dependency is a judgement about intent, and the agent doing the
work records it:

```json
{
  "agent_id": "agt_...",
  "intent_id": "int_...",
  "related_intent_id": "int_...",
  "verdict": "conflicts",
  "rationale": "Their replacement removes the extension point this intent needs",
  "action": "rescoping"
}
```

`verdict` is one of `conflicts`, `compatible`, `duplicate`, or `depends_on`.
`action` is one of `proceeding`, `rescoping`, `waiting`, or `abandoning`. The
assessment is stored, linked in the graph, and appended to the event log, so a
later reader sees not only that two intents overlapped but what was decided and
why.

The CLI string parser and MCP schema currently recognize these scope kinds:

```text
symbol api schema config infra test migration env file component contract domain
```

Canonical comparison lowercases both kind and key. The stored key spelling is
retained for display; parsed kinds are normalized to lowercase. CLI, MCP, and
structured HTTP inputs all reject empty or unknown scope kinds.

## Claims

A claim request identifies the owning agent, intent, semantic scopes, optional
reason, and lease duration:

```json
{
  "agent_id": "agt_...",
  "intent_id": "int_...",
  "scopes": [{"kind": "symbol", "key": "PaymentService"}],
  "reason": "changing the provider boundary",
  "lease_seconds": 3600
}
```

The result includes created claims, overlap warnings, and
`"advisory_only": true`. Overlap never means ownership was acquired exclusively.

## Work queries

Work can be filtered by agent, status, exact canonical scope, and result limit.
Each result joins the intent to its agent, recorded claims, latest ChangeSet ID
and object, reverse dependent intent IDs, and count of open or coordinating
conflicts.

This supports questions such as:

- Who is changing this symbol or contract?
- What is about to change?
- Is another agent solving the same task?
- Which work currently has unresolved conflicts?

Dependency and provenance questions can also use the semantic graph snapshot.

## Conflict preflight

`check_conflicts` accepts either a persisted `intent_id` or a provisional intent
summary and scopes. A provisional check lets an agent ask “will this collide?”
before publishing work.

Reports include the number of intents examined, structured findings, a blocking
boolean, and the active policy. Every finding includes evidence and a suggested
coordination action. See [Conflict detection](conflict-detection.md).

An id-shaped value (`int_` followed by 32 hexadecimal characters) passed as
free-form `intent` text is rejected with `INVALID_INPUT` instead of being
compared as prose, because that comparison would silently return a false
all-clear; pass ids as `intent_id` (CLI: `--intent-id`).

Conflicts are detected when the *later* intent publishes, so the earlier
publisher's own publish response legitimately reported no conflicts. Re-run
`check_conflicts` before publishing a ChangeSet and before requesting
verification; `start_work` and `publish_changeset` responses also include an
`open_conflicts` snapshot (count and ids) for the acting intent.

## ChangeSet publication

A ChangeSet captures a useful semantic boundary rather than every edit:

```json
{
  "agent_id": "agt_...",
  "intent_id": "int_...",
  "summary": "Introduce PaymentProvider and add Stripe implementation",
  "files": ["src/payments.rs"],
  "symbols": ["PaymentProvider", "StripePaymentProvider"],
  "contracts": ["payment-provider"],
  "dependencies": [],
  "tests": [
    {"command": "cargo test", "status": "passed", "summary": "unit suite"}
  ],
  "decisions": [
    {
      "title": "Use a provider trait",
      "rationale": "Allows Stripe and PayPal to coexist",
      "alternatives": ["replace PaymentService directly"]
    }
  ],
  "provenance": {"prompt_id": "local-task-42"},
  "git_ref": "HEAD",
  "base_ref": "optional true diff base commit",
  "worktree": "/repo/worktrees/stripe"
}
```

Foremerge resolves Git context where possible and stores a fingerprint. The
`tests` array is agent-reported history. An executed validation is a separate
record and is the evidence used by the acceptance gate.

Every completed command also creates an immutable validation-attempt record.
The attempt is marked `authoritative` only when the same ChangeSet revision is
still current, its lifecycle remains validation-eligible, and the post-command
snapshot has the expected fingerprint. Stale attempts retain output, observed
fingerprint, changed-path diagnostics, and policy digest but cannot gate
acceptance. List them with `changeset attempts` or the matching HTTP read.

Generated untracked validation output may be excluded only by an operator-owned
ruleset under Git's common directory. Its normalized digest is part of the
fingerprint, exclusions never apply to tracked changes, and MCP has no mutation
surface. See [ADR 0001](adr/0001-validation-exclusion-rules.md).

The stored `base_ref` is the commit the candidate is diffed against, and
`provenance.git.diff_hash` is a SHA-256 over the actual `git diff <base>
<candidate>` patch bytes. When the caller omits `base_ref` the base is derived
from the candidate commit's first parent (a root commit is diffed against the
empty tree). `provenance.git.base_resolution` records how the base was chosen
(`caller_supplied`, `first_parent`, `root_commit`, `shallow_boundary`, or
`unborn_worktree`), and a caller-supplied base equal to the candidate itself is
rejected, so a non-merge commit with changes never records an empty-diff hash.

A candidate at a shallow clone's boundary is not a root commit even though Git
reports it without parents; Foremerge records `shallow_boundary` and falls
back to the snapshot content hash for `diff_hash` rather than misrecording
`root_commit`. Pass `base_ref` explicitly (after fetching the true base) to
get a real commit-range diff hash from a shallow clone.

The publish response also carries `open_conflicts` (count and ids of OPEN or
COORDINATING conflicts touching the intent at that moment), so a publisher
learns about conflicts that later publishes created against it.

On acceptance, the exact validated hash is stored as `accepted_commit` and
mirrored by `refs/foremerge/accepted/<changeset-id>`. Recording a later landing
stores `integration_commit`; it does not overwrite the accepted pin.

Publishing again after `PROVISIONAL` or `VALIDATED` creates a new ChangeSet with
`supersedes_changeset_id`, marks the previous record `SUPERSEDED`, and returns
the intent to `PROVISIONAL`.

## Coordination messages

Agents can send a durable message referencing a conflict or ChangeSet:

```json
{
  "from_agent_id": "agt_stripe",
  "to_agent_id": "agt_paypal",
  "conflict_id": "cfl_...",
  "message": "Let's introduce PaymentProvider first; I will own the trait."
}
```

This operation records coordination and provenance. It does not interrupt or
control the target process; the target discovers the message through its normal
query loop or client integration.

## Conflict resolution

`resolve_conflict` (CLI: `conflicts resolve`) records an audited decision and
moves a persisted `cfl_*` conflict to `RESOLVED`.

- `resolution` is a free-form decision title describing the agreed outcome; no
  fixed vocabulary is enforced. Short imperative titles work well, for example
  `sequenced: provider abstraction lands first`, `split scopes`, or
  `duplicate: second intent discarded`.
- `rationale` is required and should reference the coordination that produced
  the agreement: name the `msg_*` coordination message ids so the decision is
  auditable against the durable message log.
- Who may resolve: on the trusted CLI and HTTP surfaces, any registered agent
  (typically a human operator or an integrator acting through one). Over MCP,
  resolution is accepted only from an agent whose intent is a party to the
  conflict, after real agreement with the other party; other agents receive
  `FORBIDDEN`. The decision is always recorded under the resolving agent's id.

Resolving is a statement that the parties agreed how the work coexists, not a
merge operation; it unblocks the acceptance gate for both intents.

The `cfl_*` row is a stable lifecycle identity. Every observation is appended
as an immutable detection occurrence. The first emits `conflict.detected`;
later observations emit `conflict.redetected`. Redetection preserves original
evidence and never auto-reopens a settled decision; responses identify that case
with `previously_settled: true`.

## Event envelope

Semantic mutations append an envelope of this form:

```json
{
  "seq": 12,
  "event_id": "evt_...",
  "event_type": "intent.published",
  "entity_type": "Intent",
  "entity_id": "int_...",
  "agent_id": "agt_...",
  "payload": {},
  "created_at": "2026-08-15T12:00:00Z",
  "prev_hash": "...",
  "event_hash": "..."
}
```

Consumers page by `seq`. The schema version is stored internally but is not yet
included in the public `Event` JSON model, so consumers version against the
Foremerge release and ignore unknown additive fields.

## Guarantees

Within one database, Foremerge guarantees:

- projection update and event append are atomic;
- event sequence order is total;
- published history cannot be updated or deleted through SQLite;
- a validation refers to one exact ChangeSet fingerprint;
- claims do not prevent another claim;
- conflict findings include deterministic evidence.

Foremerge does not guarantee:

- distributed consensus between separate databases;
- that self-reported provenance is truthful;
- that semantic rules find every design conflict;
- that two Git branches will merge;
- that a passing test proves code correctness;
- delivery of live push notifications.

## Compatibility

The MVP protocol is local and versioned with the Foremerge release. Clients
should:

- tolerate additive response fields;
- avoid depending on database tables directly;
- preserve unknown event payload fields when relaying events;
- use stable IDs instead of display labels;
- treat scope comparison and conflict scoring as server behavior rather than
  duplicating it client-side.
