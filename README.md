# Foremerge

**Catch intent conflicts before code conflicts.**

[![CI](https://github.com/naw103/foremerge/actions/workflows/ci.yml/badge.svg)](https://github.com/naw103/foremerge/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/foremerge.svg)](https://crates.io/crates/foremerge)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

Foremerge is the open-source coordination protocol for coding agents, built
above Git. Agents keep isolated worktrees while sharing intent, semantic
claims, dependencies, provisional ChangeSets, decisions, validation, and
provenance.

> **Status:** Foremerge `0.3.1` is a pre-1.0, local-first MVP. The CLI, JSON API,
> MCP server, SQLite store, deterministic conflict detector, and
> verification-gated lifecycle are implemented. Public schemas may still
> change. Shared multi-machine mode and published benchmark results do not yet
> exist.

## The conflict Git cannot see yet

```text
Agent A: Replace PaymentService with StripePaymentService
Agent B: Add PayPal support to PaymentService
```

These agents can work in different trees without touching the same line. The
plans still collide: one removes the extension point while the other depends on
it.

Foremerge compares their declared `symbol:PaymentService` scope and intent
language before implementation. Its deterministic `replace_vs_extend` rule
raises a `HIGH` advisory and suggests coordinating on a stable abstraction such
as `PaymentProvider`. That suggestion is explainable evidence, not an automatic
architecture decision or a hard lock.

Git remains the durable repository. Foremerge supplies the missing shared
awareness above it.

![Terminal rendering of an actual Foremerge release demo detecting the PaymentService conflict before either worktree changed](https://raw.githubusercontent.com/naw103/foremerge/main/docs/assets/foremerge-terminal-demo.png)

_Rendered from the actual conflict fields captured by the `0.1.0`
release-binary run in
[`examples/terminal-session.txt`](examples/terminal-session.txt). The displayed
command uses the shown `jq` filter; output is abridged for readability._

## Quickstart: first conflict in under five minutes

### Let your coding agent do it

Paste this into Claude Code, Codex, or Cursor from inside the repository you
want to coordinate:

```text
Set up Foremerge in this repository so we can coordinate parallel agents.

1. Install it:      curl -fsSL https://foremerge.com/install.sh | sh
2. Initialize:      foremerge init
3. Wire this client and any others in use: foremerge setup all
4. Register the check I should be validated against, for example:
                    foremerge checks set test -- cargo test --all-targets
5. Confirm:         foremerge doctor --client all

Then read the Foremerge skill that step 3 installed for this client and follow
it from now on: publish your intent with semantic scopes before editing, claim
the scope, and check for conflicts before you start.
```

Adjust step 4 to whatever this repository's real test command is. Step 3 asks
the client to enable an MCP server, so it will prompt you before doing so, and
the Codex registration is user level and points at one repository at a time.

### Or do it yourself

You need a recent Git and `jq`. Install a prebuilt, checksum-verified release
binary (macOS and Linux; the script installs to `~/.local/bin`):

```sh
curl -fsSL https://foremerge.com/install.sh | sh
```

Or build from source with Rust 1.85+: `cargo install --locked --git
https://github.com/naw103/foremerge foremerge`, or `cargo install --locked
--path .` from a checkout. Windows binaries are on the
[releases page](https://github.com/naw103/foremerge/releases). To update,
re-run the installer. Then, inside the repository you want to coordinate:

```sh
foremerge init
foremerge doctor
```

Every command is also available as `fmg`, the same binary under a shorter name,
so `fmg status` and `foremerge status` do the same thing. `cargo install` and
the release archives carry both names, starting with the first release that
includes them. The `curl` install script does not install the short name yet.

Install the native skill and MCP entry for any clients used in this repository,
then define the trusted checks agents may request by name:

```sh
foremerge setup all
foremerge checks set test -- cargo test --all-targets
foremerge doctor --client all
```

Use `setup codex`, `setup claude`, or `setup cursor` for one client. Setup
preserves unrelated configuration (including key order in project MCP JSON) and
refuses to replace a differing or stale skill or Foremerge MCP entry unless you
explicitly pass `--force`. `setup all` attempts every client and reports each
result, exiting nonzero if any failed. The Codex MCP registration is user-level
and points at one repository at a time; see
[agent client setup](docs/agent-clients.md).

`init` creates local coordination state under the repository's Git common
directory. It does not change tracked files. The following no-worktree sessions
are enough to exercise pre-code detection; real coding agents should register
their isolated worktrees and actual model identifiers.

```sh
STRIPE_AGENT=$(
  foremerge --json agent register \
    --name stripe-agent \
    --no-worktree |
  jq -er '.data.id'
)

STRIPE_RESULT=$(
  foremerge --json intent publish \
    --agent "$STRIPE_AGENT" \
    --task "modernize-payments" \
    --summary "Replace PaymentService with StripePaymentService" \
    --scope symbol:PaymentService
)
STRIPE_INTENT=$(printf '%s\n' "$STRIPE_RESULT" | jq -er '.data.intent.id')

PAYPAL_AGENT=$(
  foremerge --json agent register \
    --name paypal-agent \
    --no-worktree |
  jq -er '.data.id'
)

PAYPAL_RESULT=$(
  foremerge --json intent publish \
    --agent "$PAYPAL_AGENT" \
    --task "add-paypal" \
    --summary "Add PayPal support to PaymentService" \
    --scope symbol:PaymentService
)
PAYPAL_INTENT=$(printf '%s\n' "$PAYPAL_RESULT" | jq -er '.data.intent.id')

printf '%s\n' "$PAYPAL_RESULT" |
  jq '.data.conflicts[] | {kind, severity, scope, explanation, suggestion}'
```

The last command prints the live finding from your local run. No files need to
change first. Inspect the captured, clearly labeled transcript in
[`examples/terminal-session.txt`](examples/terminal-session.txt).

Claims add ownership context without blocking either agent:

```sh
foremerge --json work claim \
  --agent "$STRIPE_AGENT" \
  --intent "$STRIPE_INTENT" \
  --scope symbol:PaymentService \
  --reason "Changing the provider boundary" >/dev/null

foremerge --json work claim \
  --agent "$PAYPAL_AGENT" \
  --intent "$PAYPAL_INTENT" \
  --scope symbol:PaymentService \
  --reason "Adding another provider" |
  jq '.data | {advisory_only, warnings}'

foremerge --json work query --scope symbol:PaymentService |
  jq '.data[] | {agent: .agent.name, intent: .intent.summary, open_conflicts}'
```

Both claims succeed. The second response includes an overlap warning because a
claim is a leased advisory, never exclusive ownership.

## How it fits above Git

```text
  coding agent A                                  coding agent B
        |                                               |
  isolated worktree A                            isolated worktree B
        |                                               |
        +--------- semantic events, not edits ----------+
                              |
                    CLI / MCP / JSON API
                              |
                     Foremerge service
                    /        |        \
       SQLite coordination   git CLI   validation argv
       in <git-common-dir>       |           |
                    \         Git repository /
                     durable commits and refs
```

Every frontend uses the same service and store. The semantic graph is:

```text
Agent → Task → Intent → Claim → Symbol → Dependency
      → ChangeSet → Test → Result → Decision → Provenance
```

Mutations update typed SQLite projections, materialize graph edges, and append a
hash-chained semantic event in one transaction. The log is useful tamper
evidence; it is not a remote identity signature or distributed consensus.

## Git worktrees: isolated files, shared awareness

Foremerge resolves the Git common directory and stores its default database at:

```text
<git-common-dir>/foremerge/state.sqlite3
```

Linked worktrees share that common directory even though their checked-out
files are separate. Create a worktree with Foremerge's thin wrapper around
stock Git:

```sh
foremerge worktree create \
  --branch agent/paypal \
  --path ../payments-paypal \
  --base HEAD

foremerge --cwd ../payments-paypal --json agent register \
  --name paypal-agent \
  --model "$ACTUAL_MODEL_ID"
```

Another worktree in the same repository sees the registered agent and its
intents immediately. You can override storage with `--database PATH` or
`FOREMERGE_DB`, but every local agent must point at the same database to share
state. The MVP does not replicate SQLite across machines; do not infer
distributed safety from a network-mounted database.

Foremerge snapshots Git state for ChangeSet fingerprints and accepted refs. It
does not automatically merge, rebase, cherry-pick, push, or update a target
branch.

## Semantic workflow

```text
INTENT ─claim→ CLAIMED ─start→ IN_PROGRESS ─publish→ PROVISIONAL
       ─validate current fingerprint→ VALIDATED
       ─accept gates→ ACCEPTED ─record Git ref→ COMMITTED
```

Supported scope kinds are:

```text
symbol api schema config infra test migration env file component contract domain
```

Publish the narrowest useful semantic scope. File paths alone miss API,
configuration, schema, infrastructure, and cross-language collisions.

Common commands:

| Boundary | Command |
| --- | --- |
| Register provenance | `foremerge agent register --name NAME --model MODEL` |
| Publish intent | `foremerge intent publish --agent ID --task TASK --summary TEXT --scope KIND:KEY` |
| Claim scope | `foremerge work claim --agent ID --intent ID --scope KIND:KEY` |
| Start implementation | `foremerge work start INTENT_ID --agent AGENT_ID` |
| Ask who is changing it | `foremerge work query --scope KIND:KEY` |
| See what every agent is doing | `foremerge status` |
| Preflight a plan | `foremerge conflicts check --intent TEXT --scope KIND:KEY` |
| Send coordination | `foremerge coordinate send --from ID --to ID --message TEXT` |
| Watch semantic events | `foremerge work watch --after-seq 0` |

Run `foremerge <command> --help` for the complete current flags. Global flags
such as `--json`, `--cwd`, and `--database` may appear before or after
subcommands.

## ChangeSets and the verification gate

A ChangeSet captures the agent/model, task and intent, affected
files/symbols/contracts, dependencies, implementation summary, reported tests,
decisions, provenance, worktree, fingerprint, status, and Git ref.
The accepted candidate and its later landing commit are retained separately as
`accepted_commit` and `integration_commit`.

The honest integration order is:

1. Publish intent, claim semantic scope, and mark implementation in progress.
2. Work and commit on the isolated agent branch.
3. Publish a ChangeSet for that clean candidate.
4. Ask Foremerge to execute validation against its exact fingerprint.
5. Resolve high conflicts, then accept the still-clean, still-validated ref.
6. Integrate with ordinary Git or a pull request.
7. Record the durable integration commit in Foremerge.

```sh
foremerge work claim \
  --agent "$AGENT_ID" \
  --intent "$INTENT_ID" \
  --scope component:payments
foremerge work start "$INTENT_ID" --agent "$AGENT_ID"

# Implement the change and commit it on this isolated branch before publishing.
CHANGESET_ID=$(
  foremerge --json changeset publish \
    --agent "$AGENT_ID" \
    --intent "$INTENT_ID" \
    --summary "Introduce PaymentProvider and StripePaymentProvider" \
    --file src/payments.rs \
    --symbol PaymentProvider \
    --symbol StripePaymentProvider \
    --contract payment-provider \
    --provenance-json '{"source":"coding-agent"}' \
    --git-ref HEAD \
    --worktree "$PWD" |
  jq -er '.data.id'
)

foremerge changeset validate "$CHANGESET_ID" \
  --worktree "$PWD" \
  -- cargo test --all-targets

foremerge changeset accept "$CHANGESET_ID" --git-ref HEAD

# Integrate with ordinary Git, then record the commit that actually landed.
foremerge changeset commit "$CHANGESET_ID" --git-ref main
```

Agent-reported `--reported-test COMMAND=STATUS` values are provenance only.
They do not satisfy acceptance. Foremerge-owned validation records the command
argument vector, exit status, output, duration, and candidate fingerprint. Any
detected change after validation makes that attempt non-authoritative, but its
output and changed-path diagnostic remain queryable with `changeset attempts`.

For trusted checks that generate disposable untracked output, an operator may
set exact or directory-prefix rules without changing tracked files:

```sh
foremerge validation-exclusions set \
  --path coverage.log \
  --path target/validation-reports/
```

The normalized policy digest is part of the candidate fingerprint, tracked
changes are never excludable, MCP cannot change the policy, and generated files
must still be removed before acceptance. See
[ADR 0001](docs/adr/0001-validation-exclusion-rules.md).

Acceptance also requires a clean worktree and no unresolved `HIGH` conflict,
unless the caller deliberately uses the visible `--allow-high-conflicts`
override together with `--override-reason "..."`. Prefer resolving a conflict
with an explicit rationale. Acceptance creates
`refs/foremerge/accepted/<changeset-id>`; it does not merge code.

Validation commands run as trusted local code with your operating-system
permissions. Foremerge does not sandbox them.

## Agent clients and MCP: complete lifecycle tools

Run `foremerge mcp` over stdio. MCP does not require the HTTP daemon; both are
adapters over the same database.

| Tool | Purpose |
| --- | --- |
| `register_agent` | Record agent, model, capabilities, and worktree provenance |
| `publish_intent` | Announce planned work and receive immediate conflicts |
| `claim_work` | Create leased advisory claims on semantic scopes |
| `query_work` | Find agents, intents, claims, ChangeSets, and conflicts |
| `check_conflicts` | Check a published or provisional intent before code changes |
| `publish_changeset` | Record implementation, tests, decisions, and Git provenance |
| `coordinate_with_agent` | Send a durable message linked to a conflict or ChangeSet |
| `start_work` | Advance claimed work into implementation |
| `resolve_conflict` | Record an audited resolution for a durable conflict |
| `run_verification` | Run a trusted repository check by name, never raw MCP argv |
| `accept_changeset` | Apply final conflict, dependency, validation, and Git gates |
| `record_commit` | Record the actual Git integration commit |
| `discard_work` | Preserve abandoned work while releasing claims and blockers |
| `list_agents` | Read registered agent provenance |
| `get_intent` | Read one intent and current conflict snapshot |
| `get_changeset` | Read one ChangeSet and Git/provenance state |
| `status` | Read one consistent coordinator status snapshot |

Start from the valid minimal config in
[`examples/mcp-config.json`](examples/mcp-config.json). It assumes the client
launches `foremerge` with the repository as its working directory. Clients
without a repository working-directory setting should pass an absolute
`--database` before `mcp`; derive the Git common directory instead of assuming
that a linked worktree's `.git` is a directory.

See [agent client setup](docs/agent-clients.md) for the installer, native skill
locations, client-specific MCP files, diagnostics, and safe replacement rules.
See [MCP setup](docs/mcp-setup.md) for transport behavior, schemas, named checks,
example inputs, and multi-worktree configuration.

Source clones include equivalent skills in `.codex/skills`, `.claude/skills`,
and `.cursor/skills`, plus portable Claude and Cursor MCP templates. A Cargo
installation embeds the canonical skill so `foremerge setup` can install it
into another repository without copying this source tree.

## Local JSON API

The daemon defaults to authenticated loopback HTTP on
`http://127.0.0.1:47811`. `init` creates a bearer token with private file
permissions where the platform supports them.

In one terminal:

```sh
foremerge daemon
```

In another terminal, read the token path from Foremerge rather than guessing
it:

```sh
export FOREMERGE_URL=http://127.0.0.1:47811
TOKEN_FILE=$(foremerge --json init | jq -er '.data.token_file')
FOREMERGE_TOKEN=$(tr -d '\r\n' < "$TOKEN_FILE")

curl --fail --silent --show-error \
  --header "Authorization: Bearer $FOREMERGE_TOKEN" \
  --get "$FOREMERGE_URL/v1/work" \
  --data-urlencode 'scope=symbol:PaymentService' |
  jq .
```

Do not print, commit, or share the token. `/healthz` is database-free process
liveness and `/readyz` is a bounded non-waiting store probe; both are public.
Every `/v1` route, including the paged event-chain audit, requires the token unless
the daemon was deliberately started with `--no-auth` for a trusted local test.
The MVP refuses non-loopback binds and is not a hardened multi-tenant service.

The CLI escape hatch `foremerge request` reads local auth automatically. A
runnable curl walkthrough is in
[`examples/api-requests.sh`](examples/api-requests.sh); the full route and error
reference is [JSON API](docs/json-api.md).

## What the MVP deliberately does not claim

- Conflict detection is deterministic and explainable, but heuristic. It can
  miss synonymous concepts and warn on compatible work.
- Claims warn; they never lock files, symbols, or agents.
- Passing validation proves only that the recorded command passed for the
  recorded fingerprint, not that the test plan was complete.
- Git refs and process results are stronger evidence than self-reported model,
  prompt, or test prose.
- The event chain detects changes inside the retained chain; it is not a
  signature, remote attestation, or external checkpoint.
- Local SQLite is not shared-mode consensus, and the loopback bearer token is
  not a public deployment security model.
- There are executable benchmark fixtures, a reproducible query harness, and a
  benchmark plan, but no published
  coordinated-vs-uncoordinated performance results yet.
- Foremerge does not replace code review, architecture ownership, CI, security
  scanning, Git hosting rules, or backups.

Read the complete [limitations and trust model](docs/limitations.md) before
using Foremerge as an integration gate.

## Documentation

| Document | What it answers |
| --- | --- |
| [Architecture](docs/architecture.md) | Why one Rust binary, SQLite, Git CLI, and shared common-dir state? |
| [Protocol](docs/protocol.md) | What do agents publish and when? |
| [State model](docs/state-model.md) | Which transitions and invariants gate work? |
| [Conflict detection](docs/conflict-detection.md) | Which deterministic rules produce findings and suggestions? |
| [Git integration](docs/git-integration.md) | How do fingerprints, worktrees, and accepted refs behave? |
| [Agent clients](docs/agent-clients.md) | How do Codex, Claude Code, and Cursor discover the skill and MCP server? |
| [MCP setup](docs/mcp-setup.md) | How do clients configure and call the 17 lifecycle/read tools? |
| [JSON API](docs/json-api.md) | Which routes, request bodies, auth, and errors are shipped? |
| [OpenAPI schema](docs/openapi.yaml) | What is the machine-readable HTTP contract? |
| [Benchmark plan](docs/benchmark-plan.md) | How will coordinated and uncoordinated runs be compared? |
| [Validation exclusion ADR](docs/adr/0001-validation-exclusion-rules.md) | Which generated paths may validation ignore, and why? |
| [Roadmap](docs/roadmap.md) | What is current, next, later, or a non-goal? |
| [Limitations](docs/limitations.md) | What does the MVP not guarantee? |

Also see the [changelog](CHANGELOG.md), [security policy](SECURITY.md), and
[code of conduct](CODE_OF_CONDUCT.md).

## Contributing and license

Contributions are welcome, especially protocol feedback on scope vocabulary,
conflict evidence, ChangeSet provenance, and verification policy. Read
[CONTRIBUTING.md](CONTRIBUTING.md), then run the complete local gate:

```sh
make verify
```

Foremerge is licensed under the [Apache License 2.0](LICENSE).
