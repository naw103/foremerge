# Foremerge plugin for Claude Code

Coordinate parallel coding agents, above Git.

Agents publish intent and claim semantic scopes before writing code, so two
plans that cannot both be true collide in a queryable store instead of in your
merge. Local-first, deterministic, no cloud account and no API key.

## What this plugin adds

- The `foremerge` skill, which teaches the agent when to coordinate and what
  the lifecycle boundaries are.
- The Foremerge MCP server, exposing the coordination lifecycle as tools:
  `register_agent`, `publish_intent`, `claim_work`, `check_conflicts`,
  `publish_changeset`, `run_verification`, `accept_changeset`, `record_commit`,
  and the read tools.

## Installing

The plugin is catalogued by the marketplace at the root of this repository, so
add the marketplace first and then install by name:

```
/plugin marketplace add naw103/foremerge
```

```
/plugin install foremerge@foremerge
```

The first command registers the catalogue; the second installs the plugin from
it. `foremerge@foremerge` is `<plugin>@<marketplace>`, and both are named
`foremerge` here because this repository publishes one plugin.

## Prerequisites

The plugin does not install the binary, and it does not initialise the
repository. Install the binary first:

```bash
cargo install --locked foremerge
```

Or use the install script:

```bash
curl -fsSL https://foremerge.com/install.sh | sh
```

Then opt the repository into coordination, once per repository:

```bash
foremerge init
```

Foremerge deliberately will not create its store on an agent's initiative.
Whether a repository coordinates is the operator's decision.

## Named verification checks

Acceptance requires a passing run of a check that a human configured. Agents
can run a named check but cannot invent one, which is what keeps agent-reported
test results out of the acceptance path.

```bash
foremerge checks set test -- cargo test --all-targets
foremerge checks set lint -- cargo clippy --all-targets -- -D warnings
```

Substitute your own project's commands. Without at least one named check,
`run_verification` has nothing to run and a ChangeSet cannot be accepted.

## Verifying the setup

```bash
foremerge --json doctor --client claude
```

`git_repository: false` means this is not a Git repository. `database_ok: false`
means the repository has never run `foremerge init`.

Claude Code starts the plugin's MCP server as soon as the plugin is enabled, so
in a repository that has not run `foremerge init` the server exits immediately
and `/mcp` lists it as failed. Its error names `NOT_INITIALIZED`. That is the
opt-in working as intended, not a broken install: run `foremerge init` in the
repository and restart the server.

## Links

- Website: https://foremerge.com
- Repository: https://github.com/naw103/foremerge
- Documentation: https://github.com/naw103/foremerge#readme

Apache-2.0.
