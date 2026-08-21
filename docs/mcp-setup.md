# MCP setup

Foremerge includes a Model Context Protocol server so coding agents can publish
and query semantic coordination state without scripting the HTTP API.

The server uses standard input/output. One JSON-RPC message is read per line;
protocol responses are written to stdout. Human-readable logs must go to stderr
so they do not corrupt the MCP stream.

## Prerequisites

1. Install or build the `foremerge` binary.
2. Make sure `git` is available on `PATH`.
3. Initialize or otherwise open the target repository once with Foremerge.
4. Use an absolute database path or launch the MCP process with the repository
   as its working directory.

Build from a clone:

```bash
cargo build --release
./target/release/foremerge init
./target/release/foremerge doctor
```

## Recommended configuration

For Codex, Claude Code, or Cursor, prefer the safe repository installer and
then inspect the result:

```bash
foremerge setup claude   # or codex, cursor, all
foremerge doctor --client claude
```

See [agent client setup](agent-clients.md) for client-specific discovery paths
and overwrite behavior. The manual forms below remain useful for other MCP
hosts.

The most predictable setup gives each MCP process the same explicit database:

```json
{
  "mcpServers": {
    "foremerge": {
      "command": "/absolute/path/to/foremerge",
      "args": [
        "--database",
        "/absolute/path/to/repository/.git/foremerge/state.sqlite3",
        "mcp"
      ]
    }
  }
}
```

For linked worktrees, do not guess `.git` from the worktree: it may be a file.
Ask Git for the common directory from any worktree:

```bash
git rev-parse --path-format=absolute --git-common-dir
```

The database is `foremerge/state.sqlite3` below that directory.

With an explicit `--database`, the spawn directory of the MCP process does not
matter for trust decisions: named verification checks are read from the
`checks.json` registry of the repository the store is bound to, not from the
process working directory.

If the MCP client supports a working-directory setting, repository discovery is
also sufficient:

```json
{
  "mcpServers": {
    "foremerge": {
      "command": "foremerge",
      "args": ["mcp"],
      "cwd": "/absolute/path/to/repository"
    }
  }
}
```

The equivalent configuration without relying on a client-specific `cwd` field
uses Foremerge's global option before the subcommand:

```json
{
  "mcpServers": {
    "foremerge": {
      "command": "foremerge",
      "args": ["--cwd", "/absolute/path/to/repository", "mcp"]
    }
  }
}
```

The MCP command does not start or discover an HTTP daemon. MCP and daemon modes
are separate frontends over the same local storage. Running the daemon is not a
prerequisite for MCP when both are configured for the same database.

## Tool inventory

The complete MVP MCP tool surface is:

| Tool | Purpose |
| --- | --- |
| `register_agent` | Register agent/model/worktree provenance |
| `publish_intent` | Announce planned work and receive immediate conflicts |
| `claim_work` | Make leased advisory semantic claims |
| `query_work` | Find agents, intents, claims, ChangeSets, and open conflicts |
| `check_conflicts` | Preflight or re-evaluate semantic conflicts |
| `publish_changeset` | Record implementation, tests, decisions, and Git provenance |
| `coordinate_with_agent` | Store a durable message for another agent |
| `start_work` | Advance a claimed intent to `IN_PROGRESS` |
| `resolve_conflict` | Record an audited decision for a durable `cfl_*` finding |
| `run_verification` | Execute a trusted repository check by name |
| `accept_changeset` | Apply the final conflict, dependency, validation, and Git gates |
| `record_commit` | Record the actual post-integration Git commit |
| `discard_work` | Preserve abandoned work while releasing claims and linked blockers |

Tool names are stable protocol identifiers; CLI command spelling is allowed to
differ. Direct arbitrary validation argv remains a CLI/JSON API operation;
MCP verification is deliberately limited to configured check names, and the
check registry is resolved from the repository the coordination store is bound
to, never from the MCP server process's working directory.

Two further operations are deliberately narrower over MCP than over the trusted
CLI and HTTP operator surfaces:

- `accept_changeset` rejects `allow_high_conflicts` and `override_reason`.
  Explicit HIGH-conflict overrides are CLI-only operator actions; an agent that
  believes an override is justified must ask a human operator.
- `resolve_conflict` is accepted only from an agent whose intent is a party to
  the conflict, after real agreement with the other party. The recorded
  decision carries the resolver's agent id.

## Suggested agent workflow

At the start of a coding session:

1. Call `register_agent` with a unique session name, actual model identifier,
   and isolated worktree.
2. Call `query_work` for the task or scopes you expect to touch.
3. Call `publish_intent` before editing files.
4. Inspect the returned conflicts or call `check_conflicts` for a provisional
   preflight.
5. Call `claim_work` with semantic scopes.
6. Call `start_work` before implementation.

At a useful implementation boundary:

1. Re-run `check_conflicts` with your `intent_id`: a later publish by another
   agent can create a conflict against your intent after your own publish
   returned `conflicts: []`. `start_work` and `publish_changeset` responses
   include an `open_conflicts` snapshot for the same reason.
2. Call `publish_changeset` with affected files, symbols, contracts,
   dependencies, reported tests, decisions, and provenance.
3. Re-run `check_conflicts`, then call `run_verification` with a trusted check
   name configured by a maintainer.
4. Re-coordinate high conflicts with `coordinate_with_agent`, then call
   `resolve_conflict` for the durable decision. Over MCP only a party to the
   conflict may resolve it, after real agreement; name the coordination
   message in the rationale.
5. Call `accept_changeset` only for the validated clean Git state.
6. Integrate through ordinary Git, then call `record_commit` with the commit
   that actually landed.

If the work should not land, call `discard_work` instead of deleting its
coordination record.

Do not call tools for every keystroke. Foremerge events are semantic boundaries.

## Example tool inputs

### `register_agent`

```json
{
  "name": "paypal-agent",
  "model": "actual-model-id",
  "capabilities": ["payments"],
  "worktree": "/repo/worktrees/paypal"
}
```

### `publish_intent`

```json
{
  "agent_id": "agt_...",
  "task": "add-paypal",
  "summary": "Add PayPal support to PaymentService",
  "rationale": "Support an additional provider",
  "scopes": [
    {"kind": "symbol", "key": "PaymentService"},
    {"kind": "contract", "key": "PaymentService"}
  ],
  "depends_on": [],
  "metadata": {}
}
```

### `claim_work`

```json
{
  "agent_id": "agt_...",
  "intent_id": "int_...",
  "scopes": [{"kind": "symbol", "key": "PaymentService"}],
  "reason": "Extend provider behavior",
  "lease_seconds": 3600
}
```

### `query_work`

```json
{
  "scope": {"kind": "symbol", "key": "PaymentService"},
  "limit": 50
}
```

### `check_conflicts`

Persisted intent:

```json
{"intent_id":"int_..."}
```

Unpublished preflight:

```json
{
  "agent_id": "agt_...",
  "intent": "Replace PaymentService with StripePaymentService",
  "scopes": [{"kind": "symbol", "key": "PaymentService"}]
}
```

### `publish_changeset`

```json
{
  "agent_id": "agt_...",
  "intent_id": "int_...",
  "summary": "Add PayPalPaymentProvider",
  "files": ["src/payments/paypal.rs"],
  "symbols": ["PayPalPaymentProvider"],
  "contracts": ["payment-provider"],
  "dependencies": ["int_provider_abstraction"],
  "tests": [
    {"command":"cargo test payments","status":"passed","summary":"reported"}
  ],
  "decisions": [],
  "provenance": {"source":"agent"},
  "git_ref": "HEAD",
  "base_ref": "optional true diff base, e.g. the branch fork point",
  "worktree": "/repo/worktrees/paypal"
}
```

Omit `base_ref` unless you know the true base: Foremerge then derives it from
the candidate commit's first parent and records the actual `git diff` patch
hash in provenance (`provenance.git.base_resolution` says which one happened).

### `coordinate_with_agent`

```json
{
  "from_agent_id": "agt_paypal",
  "to_agent_id": "agt_stripe",
  "conflict_id": "cfl_...",
  "message": "Let's depend on a shared PaymentProvider contract."
}
```

### `start_work`

```json
{"agent_id":"agt_...","intent_id":"int_..."}
```

### `resolve_conflict`

```json
{
  "conflict_id": "cfl_...",
  "agent_id": "agt_...",
  "resolution": "Introduce PaymentProvider first",
  "rationale": "Both provider changes can depend on the stable contract"
}
```

`agent_id` must be a party to the conflict (the agent behind its source or
target intent); other agents are rejected. Resolve only after real agreement,
and name the agreeing coordination message in the rationale.

### `run_verification`

Configure trusted argv outside the MCP request:

```bash
foremerge checks set test -- cargo test --all-targets
foremerge checks list
```

The agent then sends only:

```json
{"changeset_id":"chg_...","check":"test"}
```

Be honest about a stdio consequence: the MCP server processes one message at a
time, and `run_verification` runs the named check inline, so the serial stdio
loop is blocked until the check finishes or its configured timeout (up to 3600
seconds) expires. Every queued client request, including `ping` liveness
probes, waits behind it, and clients with their own tool timeouts may report
the call failed or kill the server while Foremerge is still recording the
validation. Mitigations: keep named check commands short, configure realistic
`timeout_seconds` on the check, and run long validation through the CLI
(`foremerge changeset validate`) or the HTTP daemon instead of MCP.

### `accept_changeset`

```json
{"changeset_id":"chg_...","git_ref":"HEAD"}
```

MCP acceptance always applies the full HIGH-conflict gate:
`allow_high_conflicts` and `override_reason` are rejected over MCP. Explicit
overrides are CLI-only operator actions
(`foremerge changeset accept ... --allow-high-conflicts --override-reason`),
so ask a human operator instead of overriding.

### `record_commit`

```json
{"changeset_id":"chg_...","git_ref":"main"}
```

Call this only after ordinary Git or pull-request integration.

### `discard_work`

```json
{
  "agent_id": "agt_...",
  "intent_id": "int_...",
  "reason": "The experiment will not land"
}
```

## Transport smoke test

For protocol debugging, run `foremerge mcp` directly and send one compact JSON
object per line. A real MCP client should begin with `initialize`, followed by
the initialized notification and `tools/list`.

The server currently negotiates MCP protocol version `2026-07-28` when the
client requests it and otherwise falls back to `2025-11-25`. It also responds to
`ping` and `server/discover`. Notifications have no response.

Successful tool calls return both text content and `structuredContent`. Domain
failures are returned as a tool result with `isError: true`; malformed JSON-RPC,
unknown methods, and invalid tool names use JSON-RPC errors.

Do not use pretty-printed multi-line JSON on stdin because newline framing is
significant. Do not write banners or shell prompts into the process.

## Multiple worktrees and agents

Each agent may run its own MCP process. Configure every process to the same
database under the Git common directory, while registering the agent's distinct
worktree path. SQLite serializes semantic writes; Git worktrees preserve file
isolation.

Agent IDs are session identities. Do not copy one registered ID into all MCP
clients, because that erases ownership and model provenance.

## Troubleshooting

### The client shows no tools

- Run `foremerge mcp --help` in a terminal.
- Confirm the configured binary path is absolute or on the client's `PATH`.
- Ensure global flags such as `--database` appear before the `mcp` subcommand.
- Check the MCP client's stderr log for database or Git discovery errors.

### Agents cannot see each other's work

- Confirm both clients use the same absolute database path.
- In linked worktrees, compare
  `git rev-parse --path-format=absolute --git-common-dir`.
- Check that neither client accidentally fell back to a worktree-local
  `.foremerge/state.sqlite3`.

### JSON-RPC parsing fails

- Ensure no logs are being written to stdout.
- Send one compact request per line.
- Verify request IDs are valid JSON-RPC IDs.

### Verification reports that a check is not configured

- Run `foremerge checks list` from the same Git repository. The checks
  commands are repository-scoped and refuse to run outside a Git repository.
- Ask a trusted maintainer to add the intended argv with `foremerge checks set`.
- Do not work around the named-check boundary by accepting agent-reported tests;
  they are provenance only.

### Validation fails to start through the CLI or API

- Pass a non-empty argv array rather than a shell command string.
- Use an absolute worktree path visible to the MCP process.
- Remember that validation commands run with the MCP process's permissions and
  environment.

### The HTTP daemon is not running

That does not prevent the stdio MCP mode from using its configured SQLite
database. The MVP has no daemon autostart or automatic endpoint discovery.

## Security notes

- MCP can execute only named checks from the trusted registry under the bound
  repository's Git common directory. The registry requires a real Git
  repository, is never read from a plain `.foremerge` fallback directory or
  the server's spawn directory, and the check argv is trusted local code that
  is not sandboxed.
- HIGH-conflict overrides on `accept_changeset` are CLI-only operator actions
  and are rejected over MCP; `resolve_conflict` over MCP is limited to agents
  that are parties to the conflict.
- `publish_changeset` inspects the supplied local worktree through Git. Only
  connect clients you trust with local path access.
- Test output and caller-provided provenance are stored locally in SQLite.
- The event hash chain detects history modification but does not authenticate
  the model or user behind an agent ID.
