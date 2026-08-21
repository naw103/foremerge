# Conflict detection

Foremerge's headline behavior is detecting incompatible intent before Git has a
textual merge conflict to show. The MVP detector is local, deterministic, and
explainable. It does not call an LLM, compute embeddings, parse a full AST, or
claim to prove that two changes are compatible.

## Inputs

The detector compares two intent candidates:

```text
IntentCandidate {
  id
  summary
  scopes[]
}
```

Scopes are structured pairs such as `symbol:PaymentService`,
`api:POST /payments`, or `schema:payments.status`. Intent summaries provide a
second signal when scopes are absent or broad.

Conflict checks happen in two places:

- when a persisted intent is published, against existing active intents; and
- on demand, using either a persisted intent ID or an unpublished summary and
  scopes.

Claim overlap is a separate deterministic warning.

## Operation inference

The summary analyzer maps language to one operation:

```text
add | extend | modify | replace | rename | remove | migrate | unknown
```

Two targeted patterns have high confidence:

```text
replace X with Y                 -> replace X, destination Y, confidence 0.98
add Y support to X               -> extend X with variant Y, confidence 0.97
```

The regular expressions also recognize close variants such as “swap out X for
Y” and “implement Y support for X”.

Otherwise, a small verb lexicon assigns an operation with confidence `0.72`.
The classifier scans the summary word by word and uses the **first** operation
keyword by position, so “Add promotional credits, then migrate callers” reads
as additive even though a destructive word appears later:

| Operation | Representative words |
| --- | --- |
| replace | replace, rewrite, supersede, swap |
| remove | remove, delete, drop, retire |
| rename | rename, move |
| migrate | migrate, convert |
| extend | extend, augment, support(s/ed/ing) |
| add | add, introduce, implement, create |
| modify | modify, change, update, refactor, fix |

If no operation is found, confidence is `0.35`.

The last confident identifier in the summary is used as a possible subject. To
count as confident, a candidate must look like a code identifier:

- CamelCase with at least two humps (`PaymentService`, `CreditLedger`); or
- containing `_` or `::`; or
- wrapped in backticks in the summary;

and it must not be a stoplisted English sentence-starter (“No”, “The”, “This”,
“Then”, and similar), which the pattern would otherwise match at the start of a
sentence. When no confident subject exists, explanations and suggestions fall
back to the overlapping scope’s key instead of a mis-extracted word. This is
intentionally modest natural-language processing, not general semantic
understanding.

## Scope matching

Foremerge visits explicit scope pairs in request order and uses the first pair
that satisfies one of these checks:

1. Exact canonical scope, such as `symbol:PaymentService` against the same
   canonical scope: overlap score `1.0`.
2. Same key across different kinds: overlap score `0.9`.
3. Scope-key token Jaccard similarity of at least `0.66`: overlap score between
   `0.782` and `0.85`.

Canonical scope comparison is case-insensitive. CamelCase is split before token
comparison.

If no explicit scope overlaps but both inferred subjects match, Foremerge adds
an inferred `symbol` overlap with score `0.78`.

The current detector does not inspect changed files, dependency reachability,
or call graphs. Agents improve precision by publishing the narrowest useful
semantic scopes.

## Rules

### FM-C001: replace versus extend

If one operation is destructive (`replace`, `rename`, `remove`, or `migrate`)
and the other is additive (`add`, `extend`, or `modify`) on an overlapping
scope, Foremerge emits:

```text
kind: replace_vs_extend
severity: HIGH
```

This rule returns immediately for the pair because it is the strongest and most
actionable explanation.

The suggestion is scope-kind-aware. For `schema`, `migration`, and `config`
scopes, Foremerge suggests agreeing the migration order explicitly — sequencing
both changes as one migration plan or rebasing one intent onto the other’s
outcome — because a provider abstraction is the wrong advice for a schema
change. For other kinds (such as `symbol`, `contract`, `component`, and `api`)
it keeps the provider-abstraction suggestion described below. Both are
heuristic advice, not automatic design decisions.

### FM-C002: divergent replacement

If both intents are destructive on an overlapping scope and point toward
different destinations, Foremerge emits:

```text
kind: divergent_replacement
severity: HIGH
```

The suggestion asks agents to choose and record one target design before
continuing. For `schema`, `migration`, and `config` scopes it instead suggests
agreeing the migration order explicitly, as in FM-C001.

### FM-C003: shared semantic scope

If either intent modifies the scope, or both intents are additive, Foremerge
emits:

```text
kind: shared_semantic_scope
severity: MEDIUM
```

This is a coordination warning. The changes may be compatible after agents
agree on a contract or dependency order.

### FM-C004: duplicate work

Foremerge tokenizes both summaries, splits CamelCase, removes a small stop-word
set, and calculates Jaccard similarity. It emits a medium `duplicate_work`
finding when:

- summary similarity is at least `0.62`; or
- operations match, scopes overlap, and summary similarity is above `0.42`.

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
Replace PaymentService with StripePaymentService
scope: symbol:PaymentService
```

Agent B preflights or publishes:

```text
Add PayPal support to PaymentService
scope: symbol:PaymentService
```

The summaries infer `replace PaymentService` and `extend PaymentService`, while
the explicit scopes match exactly. FM-C001 returns a high-severity conflict
before either agent needs to modify a file.

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
  "kind": "replace_vs_extend",
  "severity": "HIGH",
  "score": 0.989,
  "source_intent_id": "int_...",
  "target_intent_id": "int_...",
  "scope": {"kind": "symbol", "key": "PaymentService"},
  "explanation": "...",
  "suggestion": "...",
  "evidence": {
    "rule": "FM-C001",
    "source_operation": "extend",
    "target_operation": "replace",
    "overlap": "exact semantic scope",
    "source_scope": "symbol:paymentservice",
    "target_scope": "symbol:paymentservice",
    "detected_before_code": true
  },
  "status": "OPEN",
  "detected_at": "..."
}
```

Scores are rounded to three decimal places. They rank evidence inside the
ruleset; they are not calibrated probabilities.

The top-level `scope` names the source intent’s best-overlapping scope, but the
evidence records the canonical best-overlapping scope from **each** side as
`source_scope` and `target_scope` (or `null` when no scope overlap was
determinable, as in a purely summary-based duplicate-work finding). When the
two keys differ — for example a token-based overlap between
`symbol:CreditLedgerService` and `symbol:CreditLedger` — the explanation names
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

Claims never fail because of a finding. Acceptance evaluates high-severity
conflicts separately. The MVP permits an explicit `allow_high_conflicts`
override only with a non-empty reason. Foremerge records that reason as a
decision, appends `conflict.overridden`, and changes affected high findings to
`OVERRIDDEN`. Explicitly discarded work changes linked open findings to
`DISMISSED`, so abandoned speculation cannot block the surviving intent.

## Known limitations

- Verb and identifier extraction is English-only and heuristic.
- Scope hierarchy is not modeled; `domain:payments` does not automatically imply
  every payment symbol.
- Dependency edges and schema compatibility are not yet evaluated by rules.
- File overlap and AST ownership are not conflict signals in this release.
- Synonyms outside the lexicon can be missed.
- Broad or inaccurate agent-supplied scopes cause false positives or negatives.
- A missing finding never proves semantic compatibility.

These limitations are why Foremerge returns evidence and suggestions rather
than silently blocking work at claim time.

## Extension path

Additional analyzers should preserve the same finding contract and remain
optional. Candidates include explicit structured effect objects, scope
hierarchies, dependency reachability, tree-sitter symbol extraction, API/schema
compatibility rules, embeddings, and model-assisted suggestions. Deterministic
rules should remain available as the offline baseline and test oracle.
