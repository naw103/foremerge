---
name: foremerge
description: Coordinate parallel coding agents with Foremerge's local Git-compatible CLI and MCP server. Use when an agent should publish implementation intent, claim semantic scope, check for duplicate or incompatible work, inspect ownership and dependencies, exchange coordination messages, publish a provenance-rich ChangeSet, run verification, or record accepted Git work.
---

# Foremerge

Use Foremerge before editing and again at semantic boundaries. Keep Git as the code history; use Foremerge for shared intent, advisory ownership, dependencies, conflicts, validation, decisions, and provenance.

## Start

Prefer the installed binary. From this repository, fall back to `cargo run --`.

```bash
command -v foremerge
foremerge --json doctor
```

If it is not installed:

```bash
cargo install --path .
foremerge --json init
```

No cloud account or API key is required. Foremerge resolves its SQLite database through Git's common directory, so isolated worktrees share coordination without sharing a working tree.

## Coordinate before editing

1. Register the current agent and worktree.
2. Publish the task, outcome-oriented intent, semantic scopes, dependencies, and prompt metadata.
3. Inspect the returned conflicts. Run an explicit check when comparing a proposed intent.
4. Claim scopes. Treat overlap as a coordination warning, never a lock.
5. Query the scope immediately before implementation in case another agent published new work.

```bash
foremerge --json agent register --name payments-agent --model codex
foremerge --json intent publish --agent AGENT_ID --task add-paypal \
  --summary 'Add PayPal support to PaymentService' \
  --scope symbol:PaymentService --scope contract:payments.provider
foremerge --json work claim --agent AGENT_ID --intent INTENT_ID \
  --scope symbol:PaymentService
foremerge --json work query --scope symbol:PaymentService
```

When a persisted `cfl_*` conflict appears, use `coordinate send --conflict` with its ID and record the resulting decision with `conflicts resolve`; do not erase the original evidence. An `eph_*` preflight finding is not stored and cannot be linked or resolved. Publish the intent to generate a durable `cfl_*` finding, or send an unlinked message while the plan remains provisional.

## Publish and verify work

Start work explicitly, commit a candidate with normal Git, then publish the clean commit as a ChangeSet. Dirty provisional snapshots are useful for awareness, but publish a new revision after committing before validation and acceptance.

```bash
foremerge --json work start INTENT_ID --agent AGENT_ID
foremerge --json changeset publish --agent AGENT_ID --intent INTENT_ID \
  --summary 'Add provider-neutral payment routing' \
  --symbol PaymentProvider --contract payments.provider \
  --provenance-json '{"prompt_digest":"sha256:..."}'
foremerge --json changeset validate CHANGESET_ID -- cargo test
foremerge --json changeset accept CHANGESET_ID
```

Acceptance pins the candidate at `refs/foremerge/accepted/<changeset-id>`; it does not integrate the branch. Merge, rebase, cherry-pick, or land the pull request with ordinary Git first. Only then record the real target-branch landing commit, for example `foremerge --json changeset commit CHANGESET_ID --git-ref main`. Do not record a feature-branch HEAD as committed merely because ancestry is reflexive.

Only Foremerge-executed argv validation—directly through the CLI or through the daemon API—satisfies the acceptance gate. Agent-reported tests are provenance. Acceptance requires the same clean Git HEAD and fingerprint that passed validation, satisfied dependencies, and no unresolved HIGH conflicts. An explicit HIGH-conflict override requires a written rationale.

Use `work discard` for speculation that should not land. It releases active claims and preserves the audit trail; it does not delete a worktree or reset Git.

## Read and repair

- Use `work query` for owner, intent, claims, dependents, upcoming ChangeSet, decisions, and provenance.
- Use `coordinate inbox AGENT_ID` for directed proposals and questions.
- Use `events list` or `work watch` for semantic boundaries. Foremerge never streams keystrokes.
- Use `graph` to export the semantic dependency graph.
- With `foremerge daemon` running, use `request get /v1/...` only when a high-level command is missing. It reuses the local bearer token.
- Use `foremerge mcp` for MCP stdio clients. Its core tools are `register_agent`, `publish_intent`, `claim_work`, `query_work`, `check_conflicts`, `publish_changeset`, and `coordinate_with_agent`.

Do not expose the daemon beyond loopback, run validation from an untrusted agent or repository, accept a stale ChangeSet, delete coordination SQLite files, or treat heuristic conflict suggestions as authoritative architecture decisions.
