# Conflict detection

Foremerge's headline behavior is detecting incompatible intent before Git has a
textual merge conflict to show. The detector is local, deterministic, and
explainable. It does not call an LLM, compute embeddings, parse a full AST, or
claim to prove that two changes are compatible.

It also does not decide what an overlap means. That judgement belongs to the
calling agent, which records it with `record_assessment`. What this document
describes is the narrower thing Foremerge does on its own authority: comparing
declared operations on declared scopes.

## Inputs

The detector compares two intent candidates:

```text
IntentCandidate {
  id
  summary
  scopes[]   // ScopeClaim: scope + operation
}
```

A `ScopeClaim` is a structured scope such as `symbol:PaymentService`,
`api:POST /payments`, or `schema:payments.status`, together with the operation
the intent performs on it:

| Operation | Preserves what other work depends on? |
| --- | --- |
| `add`, `extend`, `modify` | yes |
| `replace`, `remove`, `rename`, `migrate` | no |

The operation is declared by the agent, not read out of the summary. This is
the load-bearing decision in the whole design. An earlier version inferred it
from keywords in the prose, which cannot work: "consolidate onto Stripe", "cut
over to Stripe" and "replace with Stripe" are one operation written three ways,
and no fixed vocabulary catches every phrasing. Worse, a destructive keyword
anywhere in a sentence was read as destroying the shared scope, so "delete the
flaky ThumbnailCache benchmark test" collided with anything extending
`ThumbnailCache`. Measured on `tests/paraphrase_probe.rs`, inference caught one
of ten real conflicts and raised a false HIGH on nine of nine compatible pairs.

Conflict checks happen in two places:

- when a persisted intent is published, against existing active intents; and
- on demand, using either a persisted intent ID or an unpublished summary and
  scopes.

Claim overlap is a separate deterministic warning.

## What Foremerge asserts, and what it only surfaces

A finding is **asserted** when both sides *declared* an operation on the *same
canonical scope*. That is a fact derived from two declarations, so Foremerge
states it and may reach HIGH.

Everything else is **surfaced**: reported for the agent to judge and capped
below HIGH. Two cases qualify:

- the scopes matched loosely rather than exactly; or
- either operation was inferred from prose rather than declared.

The reasoning is the same in both: the ambiguity that had to be resolved to
produce the finding is exactly the ambiguity that cannot be resolved reliably,
so it must not carry the severity that stops an agent.

Every finding carries `asserted` in its evidence, along with
`source_operation_inferred` and `target_operation_inferred`.

## Prose inference

Prose inference survives for one caller: a person typing `foremerge intent
publish` who gives a summary and no `--operation`. Agents always declare.

Inference is deliberately conservative. A destructive reading is withdrawn, and
degraded to `modify`, when the verb governs only a peripheral artefact (a test,
a flag, a counter, dead code) or when the scope is merely a modifier inside the
phrase the verb governs. `Delete the flaky ThumbnailCache benchmark test`
destroys a test; `Move the ThumbnailCache eviction loop into a background task`
moves a loop. Neither threatens the cache.

Anything inferred is marked `inferred: true` and can never assert.

## Scope matching

Foremerge scores every declared scope pair:

1. Exact canonical scope, such as `symbol:PaymentService` against the same
   canonical scope: overlap score `1.0`. **Only this tier can assert.**
2. Same key across different kinds: overlap score `0.9`.
3. Scope-key token Jaccard similarity of at least `0.66`: overlap score between
   `0.782` and `0.85`.

Canonical scope comparison is case-insensitive. CamelCase is split before token
comparison. Symbol keys are reduced to `container::member` with namespace and
path prefixes discarded, so `App\Services\Report::render` and
`Report::render` are one scope.

The strongest interaction is reported once per scope pair, so an intent
declaring several related scopes does not produce a wall of findings.

The detector does not inspect changed files, dependency reachability, or call
graphs. Agents improve precision by declaring the narrowest useful scopes.

## Rules

### FM-C001: destructive versus additive

If one declared operation is destructive (`replace`, `remove`, `rename`, or
`migrate`) and the other is additive (`add`, `extend`, or `modify`) on an
overlapping scope, Foremerge emits:

```text
kind: destructive_vs_additive
severity: HIGH   (MEDIUM unless asserted)
```

One intent removes or relocates the extension point the other is building on.
This is the strongest and most actionable finding.

The suggestion is scope-kind-aware. When **either** side’s overlapping scope
kind is `schema`, `migration`, or `config`, regardless of which intent was
published first or ran the check, Foremerge suggests agreeing the migration
order explicitly: sequencing both changes as one migration plan or rebasing
one intent onto the other’s outcome. A provider abstraction is the wrong
advice for a schema change. When neither side has such a kind (for example
`symbol`, `contract`, `component`, and `api`) it keeps the
provider-abstraction suggestion described below, named after the destructive
side’s subject or overlapping scope key. Both are heuristic advice, not
automatic design decisions.

### FM-C002: divergent rewrite

If both declared operations are destructive on an overlapping scope, Foremerge
emits:

```text
kind: divergent_rewrite
severity: HIGH   (MEDIUM unless asserted)
```

The suggestion asks agents to choose and record one target design before
continuing. When either side’s overlapping scope kind is `schema`,
`migration`, or `config`, it instead suggests agreeing the migration order
explicitly, as in FM-C001.

### FM-C003: shared contract

If both declared operations are additive, Foremerge emits:

```text
kind: shared_contract
severity: MEDIUM
```

This is a coordination warning. The changes may be compatible after agents
agree on a contract or dependency order.

### FM-C004: duplicate work

Whether two intents are the same work is a judgement about goals, so this
finding is never asserted. Foremerge tokenizes both summaries, splits CamelCase,
removes a small stop-word set, and calculates Jaccard similarity. It emits a
medium `duplicate_work` finding when:

- summary similarity is at least `0.62`; or
- the declared operations match, scopes overlap, and summary similarity is
  above `0.42`.

One pair can receive both shared-scope and duplicate-work findings. The
suggestion asks the agents to coordinate ownership: compare intended outcomes
and either split the scope or pick one implementation owner.

### FM-C005: overlapping claim

When an active claim already exists for the same canonical semantic scope,
Foremerge emits:

```text
kind: overlapping_claim
severity: MEDIUM
score: 0.88
```

The new claim is still created. This is the concrete meaning of “soft claim”.

## Headline example

Agent A publishes:

```text
Consolidate all payment handling onto Stripe
scope: symbol:PaymentService  operation: replace
```

Agent B preflights or publishes:

```text
Back PaymentService with an additional PayPal gateway
scope: symbol:PaymentService  operation: extend
```

Both declared an operation on the same canonical scope, so FM-C001 asserts a
high-severity conflict before either agent needs to modify a file.

Note that neither summary contains the word "replace" or "add". Under the old
prose inference this pair produced nothing at all. The verdict now follows from
the declarations, so how either agent phrased its plan is irrelevant.

The suggestion generator strips a `Service` suffix and proposes a stable
provider contract:

```text
PaymentProvider
StripePaymentProvider
PayPalPaymentProvider
```

The response explicitly calls this a heuristic suggestion, not an automatic
architecture decision.

## Finding format

Each finding contains:

```json
{
  "id": "cfl_...",
  "kind": "destructive_vs_additive",
  "severity": "HIGH",
  "score": 0.989,
  "source_intent_id": "int_...",
  "target_intent_id": "int_...",
  "scope": {"kind": "symbol", "key": "PaymentService"},
  "explanation": "...",
  "suggestion": "...",
  "evidence": {
    "rule": "FM-C001",
    "asserted": true,
    "source_operation": "extend",
    "target_operation": "replace",
    "overlap": "exact semantic scope",
    "source_scope": "symbol:paymentservice",
    "target_scope": "symbol:paymentservice",
    "detected_before_code": true
  },
  "status": "OPEN",
  "previously_settled": false,
  "detected_at": "..."
}
```

Scores are rounded to three decimal places. They rank evidence inside the
ruleset; they are not calibrated probabilities.

The top-level `scope` names the source intent’s best-overlapping scope, but the
evidence records the canonical best-overlapping scope from **each** side as
`source_scope` and `target_scope` (or `null` when no scope overlap was
determinable, as in a purely summary-based duplicate-work finding). When the
two keys differ (for example a token-based overlap between
`symbol:CreditLedgerService` and `symbol:CreditLedger`), the explanation names
both, so each agent can match the conflict against a scope it actually
declared.

## Persistence and gating

Findings discovered while an intent is published, and warnings created by
overlapping claims, are stored with a canonical unordered intent pair plus
kind and scope. A check for a persisted intent returns those canonical stored
findings. An ad-hoc `check_conflicts` preflight returns `eph_`-prefixed findings
whose evidence contains `ephemeral: true`; they are not valid durable links for
coordination messages. Conflict reports set `blocking` when they contain a
high-severity result.

The canonical `cfl_*` row owns lifecycle state and the evidence from its first
detection. Every observation is also appended to immutable
`conflict_detections`. Redetection emits `conflict.redetected`, preserves the
canonical evidence and `RESOLVED`/`OVERRIDDEN`/`DISMISSED` state, and returns
`previously_settled: true` when a decision already exists. Foremerge
intentionally does not auto-reopen: a lease renewal after two agents agreed to
share a scope must not resurrect their settled warning. Operators can inspect
the observation history with `foremerge conflicts detections <cfl_id>` or the
matching authenticated HTTP read.

Claims never fail because of a finding. Acceptance evaluates high-severity
conflicts separately. The MVP permits an explicit `allow_high_conflicts`
override only with a non-empty reason. Foremerge records that reason as a
decision, appends `conflict.overridden`, and changes affected high findings to
`OVERRIDDEN`. Explicitly discarded work changes linked open findings to
`DISMISSED`, so abandoned speculation cannot block the surviving intent.

## Known limitations

- A declared operation is only as good as the declaration. An agent that
  declares `extend` and then rewrites the thing is not detectable here, and no
  rule in this document claims otherwise.
- Scope hierarchy is not modeled; `domain:payments` does not automatically imply
  every payment symbol.
- Dependency edges and schema compatibility are not yet evaluated by rules.
- File overlap and AST ownership are not conflict signals in this release.
- Prose inference, used only by CLI callers who omit `--operation`, remains
  English-only and misses phrasings outside its lexicon. That is why it never
  asserts.
- Broad or inaccurate agent-declared scopes cause false positives or negatives.
- A missing finding never proves semantic compatibility.

These limitations are why Foremerge returns evidence and suggestions rather
than silently blocking work at claim time.

## Extension path

Additional analyzers should preserve the same finding contract, remain
optional, and respect the same boundary: assert only what follows from
declarations, and surface everything else for the agent to judge. Candidates
include scope hierarchies, dependency reachability, tree-sitter symbol
extraction, and API/schema compatibility rules. Deterministic rules should
remain available as the offline baseline and test oracle.

Foremerge deliberately ships no model of its own. The clients driving it are
already models, and asking the calling agent to judge an overlap costs no
inference, no API key, and no network round trip, while producing better
provenance than a similarity score: `record_assessment` stores what the agent
concluded and why.
