# JSON API

The Foremerge daemon exposes the coordination service as local HTTP/JSON. It is
intended for scripts, language-independent clients, and debugging the same
operations used by the CLI and MCP adapter.

Set the daemon URL once for the examples:

```bash
export FOREMERGE_URL=http://127.0.0.1:47811
export FOREMERGE_RUNTIME="$(git rev-parse --path-format=absolute --git-common-dir)/foremerge"
export FOREMERGE_TOKEN="$(cat "$FOREMERGE_RUNTIME/token")"
fm_curl() {
  curl --fail --silent \
    -H "authorization: Bearer $FOREMERGE_TOKEN" \
    "$@"
}
```

`47811` is the default; use the address printed by `foremerge daemon` after a
`--bind` override. `foremerge init` or the authenticated daemon creates the
token with owner-only permissions on Unix. The examples below use `fm_curl` to
send it.

## Conventions

- Request and response bodies use UTF-8 JSON.
- IDs are opaque strings with readable prefixes such as `agt_`, `int_`,
  `chg_`, and `cfl_`.
- Timestamps are RFC 3339 strings.
- Lifecycle and severity values are uppercase.
- Scope objects have `kind` and `key` fields.
- Unknown additive response fields should be ignored.
- Successful responses use `{"ok":true,"data":...}`. Failures use
  `{"ok":false,"error":{"code":"...","message":"..."}}`.
- A non-2xx response is an operation failure. Do not infer success from a JSON
  body alone.

The envelope also applies to malformed JSON, missing JSON content type,
request-deserialization failures, unknown routes, and method-not-allowed
responses.

The daemon is local-first, not a hardened public multi-tenant service; the CLI
refuses non-loopback bind addresses. When a daemon token is configured, every
`/v1` route requires `Authorization: Bearer <token>`; `/healthz` and `/readyz`
remain public and expose no coordination data. Bearer credentials are compared
through fixed-length digests. There is no application-level rate limiter.

## Route summary

The machine-readable form of this contract is
[openapi.yaml](openapi.yaml).

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/healthz` | Process health and version |
| `GET` | `/readyz` | Bounded, non-waiting store readiness |
| `GET` | `/v1/audit/event-chain` | Paged verification of a stable event-log prefix |
| `GET` | `/v1/events` | Read append-only events |
| `GET` | `/v1/graph` | Read the semantic graph snapshot |
| `GET` | `/v1/status` | Read one consistent coordinator status snapshot |
| `GET` | `/v1/work` | Query intents with agents, claims, and conflicts |
| `GET` | `/v1/agents` | List registered agents |
| `GET` | `/v1/intents/{id}` | Read one intent with agent and conflicts |
| `GET` | `/v1/conflicts` | List persisted conflicts |
| `GET` | `/v1/conflicts/{id}/detections` | Read immutable detection occurrences |
| `GET` | `/v1/changesets/{id}` | Read one ChangeSet |
| `GET` | `/v1/changesets/{id}/validation-attempts` | Read all validation attempts |
| `GET` | `/v1/agents/{id}/inbox` | Read coordination messages for an agent |
| `POST` | `/v1/agents/register` | Register an agent session |
| `POST` | `/v1/intents` | Publish an intent and detect conflicts |
| `POST` | `/v1/claims` | Make advisory semantic claims |
| `POST` | `/v1/conflicts/check` | Preflight or recheck intent conflicts |
| `POST` | `/v1/conflicts/{id}/resolve` | Resolve a persisted conflict with rationale |
| `POST` | `/v1/changesets` | Publish implementation and provenance |
| `POST` | `/v1/changesets/{id}/validate` | Execute validation for one fingerprint |
| `POST` | `/v1/changesets/{id}/accept` | Apply the verification/conflict acceptance gate |
| `POST` | `/v1/changesets/{id}/commit` | Record the durable integration Git ref |
| `POST` | `/v1/coordinate` | Store an agent-to-agent coordination message |
| `POST` | `/v1/work/{id}/start` | Mark an intent in progress |
| `POST` | `/v1/work/{id}/discard` | Discard speculative work |

## Health

```bash
fm_curl "$FOREMERGE_URL/healthz" | jq .
```

Health confirms only that the HTTP process is serving and deliberately performs
no SQLite, filesystem, Git, count, or event-chain work. Readiness is a separate
non-waiting store probe:

```bash
fm_curl "$FOREMERGE_URL/readyz" | jq .
```

A busy store returns `503 NOT_READY` immediately instead of making a monitoring
request queue behind coordination work. Full event-chain verification is an
authenticated audit operation. It fixes a stable maximum sequence, opens a
separate read-only SQLite connection, and verifies in bounded pages:

```bash
fm_curl --get "$FOREMERGE_URL/v1/audit/event-chain" \
  --data-urlencode 'page_size=1000' | jq .
```

The response reports `valid`, `events_verified`, `last_seq`, and `head_hash`.
`foremerge events audit` provides the same operator surface, while `foremerge
doctor` includes the audit in its broader read-only Git/database/client report.

## Register an agent

```bash
fm_curl \
  -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/agents/register" \
  -d "$(jq -n --arg worktree "$PWD" '{
    name: "stripe-agent",
    model: "example-model",
    capabilities: ["rust", "payments"],
    worktree: $worktree
  }')" | jq .
```

Request fields:

| Field | Required | Meaning |
| --- | --- | --- |
| `name` | yes | Human-readable session name |
| `model` | no | Model identifier retained as provenance |
| `capabilities` | no | Free-form capability labels |
| `worktree` | no | Worktree used to derive Git branch/head |

`data` is an agent record including generated `id`, Git context when available,
status, and registration time.

## Publish an intent

```bash
fm_curl \
  -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/intents" \
  -d '{
    "agent_id": "agt_REPLACE_ME",
    "task": "modernize-payments",
    "summary": "Replace PaymentService with StripePaymentService",
    "rationale": "Introduce Stripe as a provider implementation",
    "scopes": [
      {"kind": "symbol", "key": "PaymentService", "operation": "replace"},
      {"kind": "contract", "key": "PaymentService", "operation": "replace"}
    ],
    "depends_on": [],
    "metadata": {}
  }' | jq .
```

The response envelope contains:

Each declared scope carries the operation the intent performs on it: `add`,
`extend` or `modify`, which preserve what other work depends on, or `replace`,
`remove`, `rename` or `migrate`, which do not.

The response envelope contains:

```json
{
  "ok": true,
  "data": {
    "intent": {},
    "conflicts": [],
    "related_work": [
      {
        "intent_id": "int_...",
        "agent": "paypal-agent",
        "summary": "Back PaymentService with an additional PayPal gateway",
        "status": "CLAIMED",
        "overlap": [
          {
            "scope": {"kind": "symbol", "key": "PaymentService"},
            "your_operation": "replace",
            "their_operation": "extend",
            "interaction": "destructive_vs_additive",
            "asserted": true
          }
        ],
        "asserted": true
      }
    ],
    "assessment_required": true,
    "next": "Assess each entry in related_work ..."
  }
}
```

Conflict detection is part of publication, so a client does not need a second
request to learn about existing incompatible work.

`related_work` states what overlaps. Deciding what the overlap means is the
caller's, recorded with `POST /v1/assessments`:

```bash
fm_curl \
  -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/assessments" \
  -d '{
    "agent_id": "agt_REPLACE_ME",
    "intent_id": "int_REPLACE_ME",
    "related_intent_id": "int_OTHER",
    "verdict": "conflicts",
    "rationale": "Their replacement removes the extension point this intent needs",
    "action": "rescoping"
  }' | jq .
```

`GET /v1/intents/{id}/assessments` reads them back.

## Claim semantic work

```bash
fm_curl \
  -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/claims" \
  -d '{
    "agent_id": "agt_REPLACE_ME",
    "intent_id": "int_REPLACE_ME",
    "scopes": [{"kind": "symbol", "key": "PaymentService"}],
    "reason": "Changing the provider boundary",
    "lease_seconds": 3600
  }' | jq .
```

Claims are always advisory. The response contains `claims`, `warnings`, and
`"advisory_only": true`.

## Query work

Supported query parameters mirror the `WorkQuery` model:

```bash
fm_curl --get "$FOREMERGE_URL/v1/work" \
  --data-urlencode 'agent_id=agt_REPLACE_ME' \
  --data-urlencode 'status=IN_PROGRESS' \
  --data-urlencode 'scope=symbol:PaymentService' \
  --data-urlencode 'limit=50' | jq .
```

All filters are optional. The default limit is 50 and the service clamps it to
1–500. Each returned item includes the intent, registered agent, recorded
claims, latest ChangeSet ID and object, reverse dependent intent IDs, and number
of open or coordinating conflicts.

Core reads are intentionally available over HTTP, MCP, and CLI with the same
domain shapes:

```bash
fm_curl "$FOREMERGE_URL/v1/agents" | jq .
fm_curl "$FOREMERGE_URL/v1/intents/int_REPLACE_ME" | jq .
fm_curl "$FOREMERGE_URL/v1/changesets/chg_REPLACE_ME" | jq .
fm_curl "$FOREMERGE_URL/v1/status" | jq .
```

## Check conflicts

Check an existing intent:

```bash
fm_curl \
  -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/conflicts/check" \
  -d '{"intent_id":"int_REPLACE_ME"}' | jq .
```

Or preflight unpublished work:

```bash
fm_curl \
  -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/conflicts/check" \
  -d '{
    "agent_id": "agt_REPLACE_ME",
    "intent": "Add PayPal support to PaymentService",
    "scopes": [{"kind": "symbol", "key": "PaymentService"}]
  }' | jq .
```

Free-form `intent` text must be prose: an id-shaped value (`int_` followed by
32 hexadecimal characters) is rejected with `INVALID_INPUT` instead of being
compared as text, because that comparison would silently return a false
all-clear. Pass ids as `intent_id`.

The response includes `conflicts`, `checked_intents`, `blocking`, and `policy`.
See [Conflict detection](conflict-detection.md) for rule evidence and limits.

Persisted conflicts can be listed with:

```bash
fm_curl "$FOREMERGE_URL/v1/conflicts" | jq .
```

Use `?status=OPEN`, `COORDINATING`, `RESOLVED`, `OVERRIDDEN`, or `DISMISSED` to
filter the list. `DISMISSED` means one of the linked intents was explicitly
discarded; the original evidence and event history remain.

A `cfl_*` row is a stable lifecycle identity. Each observation is retained
separately, so redetection cannot rewrite evidence attached to a resolution or
silently reopen `RESOLVED`, `OVERRIDDEN`, or `DISMISSED` work:

```bash
fm_curl "$FOREMERGE_URL/v1/conflicts/cfl_REPLACE_ME/detections" | jq .
```

The first observation emits `conflict.detected`; later observations emit
`conflict.redetected` and return `previously_settled: true` when the identity is
already terminal.

Resolve a conflict by recording both the agreed outcome and its rationale:

```bash
fm_curl \
  -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/conflicts/cfl_REPLACE_ME/resolve" \
  -d '{
    "agent_id":"agt_REPLACE_ME",
    "resolution":"Introduce PaymentProvider first",
    "rationale":"Both provider implementations can depend on one contract"
  }' | jq .
```

## Start or discard work

Lifecycle operations act on an intent ID in the path:

```bash
fm_curl -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/work/int_REPLACE_ME/start" \
  -d '{"agent_id":"agt_REPLACE_ME"}' | jq .

fm_curl -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/work/int_REPLACE_ME/discard" \
  -d '{"agent_id":"agt_REPLACE_ME","reason":"superseded by shared provider work"}' | jq .
```

The start response is the updated intent and includes an `open_conflicts`
snapshot (count and ids of OPEN or COORDINATING conflicts touching it),
because a later publish by another agent can create a conflict after your own
publish returned none.

Discarding records the speculative outcome; it does not delete history or Git
objects. The `reason` must be non-empty: an empty or whitespace-only reason is
rejected with `INVALID_INPUT` (a breaking change in 0.2.0).

## Publish a ChangeSet

```bash
fm_curl \
  -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/changesets" \
  -d "$(jq -n --arg worktree "$PWD" '{
    agent_id: "agt_REPLACE_ME",
    intent_id: "int_REPLACE_ME",
    summary: "Introduce PaymentProvider and StripePaymentProvider",
    files: ["src/payments.rs"],
    symbols: ["PaymentProvider", "StripePaymentProvider"],
    contracts: ["payment-provider"],
    dependencies: [],
    tests: [{command:"cargo test",status:"passed",summary:"reported by agent"}],
    decisions: [{
      title:"Use a provider trait",
      rationale:"Keep Stripe and PayPal compatible",
      alternatives:["replace PaymentService directly"]
    }],
    provenance: {source:"agent"},
    git_ref: "HEAD",
    worktree: $worktree
  }')" | jq .
```

Foremerge derives available Git context and returns the persisted ChangeSet,
including its fingerprint, status, and `supersedes_changeset_id` when it is a
revision. Publishing a revision after validation invalidates the prior gate and
returns the intent to `PROVISIONAL`.

The request also accepts an optional `base_ref` naming the true diff base
commit; a base equal to the candidate itself is rejected with `INVALID_INPUT`.
When omitted, the base is derived from the candidate commit's first parent.
The response's `provenance.git` object records the resolved `candidate`,
`base_ref`, `base_resolution` (`caller_supplied`, `first_parent`,
`root_commit`, `shallow_boundary`, or `unborn_worktree`), and `diff_hash` over
the actual `git diff <base> <candidate>` patch bytes.

The response also carries an `open_conflicts` snapshot (count and ids of OPEN
or COORDINATING conflicts touching the intent at that moment), so an earlier
publisher learns about conflicts that later publishes created against it.

## Validate a ChangeSet

Commands are JSON argument arrays, not shell strings:

```bash
fm_curl \
  -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/changesets/chg_REPLACE_ME/validate" \
  -d "$(jq -n --arg worktree "$PWD" '{
    command:["cargo","test","--all-targets"],
    worktree:$worktree,
    timeout_seconds:300
  }')" | jq .
```

The immediate result includes pass/fail, exit code, stdout, stderr, duration,
fingerprint, and run time. Stdout and stderr are each limited to their final
16 KiB. Treat captured output as potentially sensitive.

Every completed command is immutable audit evidence, including a passing run
whose final snapshot became stale before it could update lifecycle state:

```bash
fm_curl \
  "$FOREMERGE_URL/v1/changesets/chg_REPLACE_ME/validation-attempts" | jq .
```

An attempt records `authoritative`, `stale_reason`, expected and observed
fingerprints, changed paths, excluded untracked paths, and the exclusion-ruleset
digest. Acceptance consults only an authoritative passing validation for the
current fingerprint. A stale error names newly changed and removed paths.

Checks that intentionally create generated output can use operator-owned,
digest-bound exclusions configured only through `foremerge
validation-exclusions`. See [ADR 0001](adr/0001-validation-exclusion-rules.md).
Excluded paths are untracked-only and must still be removed before acceptance's
strict clean-worktree check.

## Accept a ChangeSet

```bash
fm_curl \
  -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/changesets/chg_REPLACE_ME/accept" \
  -d '{
    "git_ref":"HEAD",
    "allow_high_conflicts":false,
    "allow_unverified":false,
    "override_reason":null
  }' | jq .
```

Acceptance requires fresh passing validation and a clean resolvable Git ref.
Open high-severity conflicts fail the gate unless the caller explicitly opts
into the override and supplies a non-empty `override_reason`; Foremerge stores
that reason as a decision and marks those findings `OVERRIDDEN`.

`allow_unverified` is the second operator override. It accepts work Foremerge
did not verify, or verified as failing, regardless of the configured acceptance
policy, and it also requires a non-empty `override_reason`. The outcome is
recorded on the ChangeSet as `UNVERIFIED` with that reason rather than being
presented as a check that passed.

Both overrides are operator actions. This API and the CLI are the operator
surfaces and accept them; MCP is the agent surface and rejects them outright,
so an agent that believes an override is justified has to ask a person. That
split is a guardrail against a confused agent, not an authorization boundary:
the daemon token is what separates an operator from anyone else, and anyone
holding it can act on either surface. See [`SECURITY.md`](../SECURITY.md) and
[limitations](limitations.md).

Declared
ChangeSet dependencies must already be accepted or committed; each dependency's
stored `accepted_commit` must remain matched by its namespaced ref and be present
in the candidate's Git ancestry. The selected ref must resolve to the validated
worktree `HEAD`. Successful acceptance records both that immutable hash and the
accepted namespaced Git ref; it does not merge a branch.

## Record a commit

After integration through ordinary Git or a pull request, record the durable
commit against the ChangeSet:

```bash
fm_curl \
  -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/changesets/chg_REPLACE_ME/commit" \
  -d '{"git_ref":"main"}' | jq .
```

This stores the supplied hash as `integration_commit` without replacing
`accepted_commit`, updates protocol provenance, and releases active claims.
Foremerge verifies that the supplied ref resolves to a commit containing the
accepted candidate as an ancestor. The caller remains responsible for supplying
the intended target ref; the MVP does not perform the merge.

## Coordinate and read the inbox

```bash
fm_curl \
  -H 'content-type: application/json' \
  -X POST "$FOREMERGE_URL/v1/coordinate" \
  -d '{
    "from_agent_id":"agt_STRIPE",
    "to_agent_id":"agt_PAYPAL",
    "conflict_id":"cfl_REPLACE_ME",
    "message":"I will extract PaymentProvider first; please depend on that intent."
  }' | jq .

fm_curl \
  "$FOREMERGE_URL/v1/agents/agt_PAYPAL/inbox" | jq .
```

Messages are durable polling state. The MVP does not push them to a running
agent.

## Events

Read events after a known sequence:

```bash
fm_curl --get "$FOREMERGE_URL/v1/events" \
  --data-urlencode 'after_seq=0' \
  --data-urlencode 'limit=100' | jq .
```

Store the last consumed `seq` and request later events. The endpoint returns
ordinary JSON; it is not SSE or a long-lived stream in the MVP.

## Graph

```bash
fm_curl "$FOREMERGE_URL/v1/graph" | jq .
```

The response has `nodes` and `edges` arrays. Use typed endpoints for lifecycle
automation; the graph is intended for exploration, dependency inspection, and
visualization.

## Errors and retries

Invalid scopes handled inside an operation, missing entities, ownership
violations, illegal state transitions, stale validation, and acceptance
failures return non-2xx responses with an explanatory error. A validation
command that exits nonzero, times out, or cannot start is still a completed
validation operation: it returns `200` with `passed: false`, records the
evidence, and leaves the ChangeSet unvalidated.
Malformed JSON can be rejected earlier by Axum as described above.

The current status mapping is:

| Error code | HTTP status |
| --- | --- |
| `INVALID_INPUT` | `400` |
| `UNAUTHORIZED` | `401` |
| `FORBIDDEN` | `403` |
| `RESOURCE_LIMIT` | `413` |
| `NOT_FOUND` | `404` |
| `METHOD_NOT_ALLOWED` | `405` |
| `STATE_RACE` | `409` |
| `NOT_READY` | `503` |
| `INVALID_TRANSITION`, `CHECK_FAILED`, `STALE_CHANGESET`, `BLOCKING_CONFLICT`, `UNSATISFIED_DEPENDENCY`, `TARGET_DIVERGED` | `422` |
| unclassified failure | `500` |

Before retrying a mutation after a transport failure, query current state. The
MVP API does not promise general HTTP idempotency keys. Generated IDs and the
append-only event log make duplicate semantic actions visible, but clients
should not assume a repeated POST is deduplicated.

### Shutdown

On SIGINT or SIGTERM the daemon stops accepting new connections and gives
in-flight operations, including validation, up to 30 seconds to finish. If the
drain completes the daemon exits 0.

If the grace expires with requests still in flight, the daemon does not wait for
them. It abandons the remaining request futures (which terminates the validation
subprocess trees Foremerge started itself), then allows up to 5 more seconds for
blocking work that cannot be cancelled, and then exits 1. The process therefore
exits within roughly 35 seconds of the signal.

Two things survive that forced exit:

- A child process Foremerge did not start under a validation guard, such as a
  wedged `git` invocation inside a request, is not killed and can outlive the
  daemon. Foremerge stops waiting for it; it does not reap it.
- Further signals sent during the drain are ignored. The first SIGINT or SIGTERM
  starts the bounded sequence above and the daemon exits on its own; SIGKILL is
  the only way to end it sooner.
