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

The complete lifecycle is available through 13 MCP tools:

- `accept_changeset`
- `check_conflicts`
- `claim_work`
- `coordinate_with_agent`
- `discard_work`
- `publish_changeset`
- `publish_intent`
- `query_work`
- `record_commit`
- `register_agent`
- `resolve_conflict`
- `run_verification`
- `start_work`

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
foremerge agent register
foremerge intent publish
foremerge work claim|query|start|watch|discard
foremerge conflicts check|list|resolve
foremerge changeset publish|show|validate|accept|commit
foremerge coordinate send|inbox
foremerge events list
foremerge graph
foremerge worktree create
foremerge request get|post <path>
```

Use `foremerge <family> <command> --help` for argument spelling. In particular,
actor flags use `--agent` in the domain CLI, while HTTP/MCP JSON uses
`agent_id`. `work watch` is polling over the event query; it is not a streaming
transport. `request` is an authenticated local HTTP escape hatch, not another
implementation of the service.

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
    {"kind": "symbol", "key": "PaymentService"},
    {"kind": "domain", "key": "payments"}
  ],
  "depends_on": [],
  "metadata": {}
}
```

Publication persists the intent, updates the semantic graph, appends an event,
and checks it against active work. The response returns both the intent and any
immediately detected conflicts.

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
  "worktree": "/repo/worktrees/stripe"
}
```

Foremerge resolves Git context where possible and stores a fingerprint. The
`tests` array is agent-reported history. An executed validation is a separate
record and is the evidence used by the acceptance gate.

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
