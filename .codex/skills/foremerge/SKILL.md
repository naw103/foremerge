---
name: foremerge
description: Coordinate parallel coding agents with Foremerge's local Git-compatible CLI and MCP server. Use when an agent should publish intent before editing, claim semantic scope, detect duplicate or incompatible work, coordinate on durable conflicts, publish a ChangeSet, run a trusted named verification check, accept validated work, or record its Git integration.
---

# Foremerge

Use Foremerge at semantic engineering boundaries. Git owns files and commit history; Foremerge owns shared intent, advisory claims, dependencies, conflicts, validation evidence, decisions, and provenance across isolated worktrees.

## Confirm the integration

Prefer MCP tools when they are available. Use the CLI for setup, diagnostics, event watching, and commands not exposed by the client.

```bash
foremerge --json doctor --client all
foremerge --json checks list
```

If the client integration is missing, run the relevant installer from the repository:

```bash
foremerge --json setup codex
foremerge --json setup claude
foremerge --json setup cursor
```

Setup refuses to replace differing skill or MCP entries unless the human explicitly chooses `--force`. No cloud account or API key is required. The coordination database and named-check registry live under Git's common directory, so linked worktrees share coordination without sharing a working tree.

## Coordinate before editing

1. Register this agent with its actual model and worktree.
2. Query expected scopes for active owners and related work.
3. Publish an outcome-oriented intent with semantic scopes and dependencies.
4. Inspect returned findings; use `check_conflicts` for a provisional preflight.
5. Claim the scopes. Claims are leased advice, never locks.
6. Start the claimed work before implementation.

Prefer semantic scopes such as `symbol:PaymentService`, `api:/payments`, `schema:billing.payments`, or `contract:payments.provider`; file scopes are weaker evidence.

A durable `cfl_*` conflict can be linked to `coordinate_with_agent` and resolved with `resolve_conflict`. An `eph_*` preflight is intentionally not stored: publish/claim first to obtain a durable finding, or send an unlinked message.

CLI equivalent:

```bash
foremerge --json agent register --name payments-agent --model MODEL
foremerge --json intent publish --agent AGENT_ID --task add-paypal \
  --summary 'Add PayPal support to PaymentService' \
  --scope symbol:PaymentService --scope contract:payments.provider
foremerge --json work claim --agent AGENT_ID --intent INTENT_ID \
  --scope symbol:PaymentService
foremerge --json work start INTENT_ID --agent AGENT_ID
```

## Publish, verify, and accept

Conflicts are not delivered as push notifications: a later publish by another agent can create a conflict against your intent after your own publish returned `conflicts: []`. Re-run `check_conflicts` (CLI: `foremerge --json conflicts check --intent-id INTENT_ID`) before publishing a ChangeSet and again before requesting verification. `start_work` and `publish_changeset` responses also include an `open_conflicts` snapshot for your intent. Pass an intent id only via `intent_id`/`--intent-id`; the free-form `intent` text field rejects id-shaped input.

Commit a clean candidate with ordinary Git, then:

1. Call `publish_changeset` with implementation, dependency, decision, and provenance evidence. Pass `base_ref` when you know the true diff base (for example your branch's fork point); otherwise Foremerge derives it from the candidate commit's first parent.
2. Call `run_verification` with a configured check name such as `test`.
3. Resolve any persisted HIGH conflict you are a party to, only after real agreement with the other party; name the agreeing coordination message in the rationale. Explicit HIGH-conflict overrides are CLI-only operator actions and are rejected over MCP: ask a human operator instead of overriding yourself.
4. Call `accept_changeset`; it must match the clean Git commit and passing fingerprint.
5. Land the accepted commit through ordinary Git or a pull request.
6. Call `record_commit` with the actual target-branch integration ref.

`run_verification` deliberately accepts a check name, not raw argv. Named checks are trusted local code configured outside the MCP call, for example by a repository maintainer:

```bash
foremerge checks set test -- cargo test --all-targets
foremerge checks set lint -- cargo clippy --all-targets -- -D warnings
```

Do not add or replace named checks unless the user has authorized that repository configuration. If a check you need is not configured, ask a human operator to configure it rather than provisioning it yourself. Checks are repository-scoped: the registry lives under Git's common directory and the commands refuse to run outside a Git repository. Agent-reported tests on `publish_changeset` are provenance only and never satisfy acceptance.

Acceptance creates `refs/foremerge/accepted/<changeset-id>`; it does not merge code. Do not call `record_commit` on the feature-branch HEAD merely because ancestry is reflexive.

## Abandon or inspect work

Use `discard_work` for work that should not land. It preserves history, releases claims, and dismisses linked blockers; it does not delete a worktree or reset Git.

Use `query_work` for current semantic state, `coordinate_with_agent` for durable proposals, and these CLI commands when needed:

```bash
foremerge --json agent list
foremerge --json intent show INTENT_ID
foremerge --json coordinate inbox --agent AGENT_ID
foremerge --json events list
foremerge --json graph
foremerge work watch
```

`agent list` and `intent show` are the read surfaces for mapping a conflict's intent ids to agents before `coordinate send`.

## MCP tools

The complete lifecycle surface is:

- `register_agent`, `query_work`, `publish_intent`, `check_conflicts`
- `claim_work`, `start_work`, `coordinate_with_agent`, `resolve_conflict`
- `publish_changeset`, `run_verification`, `accept_changeset`
- `record_commit`, `discard_work`

Do not expose the optional HTTP daemon beyond loopback, treat heuristic suggestions as authoritative architecture, accept stale evidence, delete coordination state, or use Foremerge to bypass ordinary Git review and integration.
