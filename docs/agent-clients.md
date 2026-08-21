# Agent client setup

Foremerge ships one workflow skill and one stdio MCP server for Codex, Claude
Code, and Cursor. The skill teaches the coordination lifecycle; MCP gives the
client typed operations against the repository's shared Foremerge state.

## Install one client

Run setup from the Git repository the agents will coordinate:

```bash
foremerge setup codex
foremerge setup claude
foremerge setup cursor
```

Or configure all three:

```bash
foremerge setup all
foremerge doctor --client all
```

Setup initializes Foremerge state if needed and reports every file or client
registration it changed. New MCP entries use an absolute binary and repository
path; the installer also recognizes the equivalent portable templates shipped
in a source clone. Setup is idempotent when installed content is current.

Setup never replaces a differing skill file or `mcpServers.foremerge` entry by
default. Inspect the existing content first; use `--force` only when replacing
it is intentional. Use `--skip-mcp` to install the skill without changing MCP
configuration.

## What each client receives

| Client | Skill | MCP configuration |
| --- | --- | --- |
| Codex | `.codex/skills/foremerge/SKILL.md` | Registered as `foremerge` through `codex mcp add` |
| Claude Code | `.claude/skills/foremerge/SKILL.md` | Project `.mcp.json` |
| Cursor | `.cursor/skills/foremerge/SKILL.md` | Project `.cursor/mcp.json` |

Codex's CLI stores MCP registrations in its own configuration, so Foremerge uses
the supported `codex mcp` command rather than editing that file directly. If
the Codex CLI is unavailable, setup still installs the repository skill and
returns the exact registration command as its next step.

Claude Code and Cursor use project JSON files. Foremerge merges only the
`mcpServers.foremerge` entry and preserves other top-level keys and servers.
The source repository also includes portable project templates. Entries created
in other repositories use an absolute binary and `--cwd` path to avoid
worktree-dependent launch behavior.

## Verify discovery

Run a targeted diagnostic:

```bash
foremerge --json doctor --client claude
foremerge --json doctor --client cursor
foremerge --json doctor --client codex
```

Each client report separates:

- whether the client executable is available;
- whether the skill exists and matches this Foremerge release;
- whether its MCP entry is configured; and
- the next corrective command.

A client can still read a project skill when its desktop or CLI executable is
not on the shell `PATH`; in that case the diagnostic remains not ready because
Foremerge cannot verify the executable.

Restart or reload the client after changing project MCP configuration. Then ask
it to use Foremerge for parallel work. Codex can explicitly invoke
`$foremerge`; Claude Code exposes the project skill as `/foremerge`; Cursor
discovers the project skill from its skills directory and selects it from the
description when the task matches.

## Configure verification policy

MCP does not accept an arbitrary command from an agent. A trusted maintainer
defines named argv checks in repository-private Foremerge state:

```bash
foremerge checks set test --timeout-seconds 600 -- cargo test --all-targets
foremerge checks set lint -- cargo clippy --all-targets -- -D warnings
foremerge checks list
```

Agents call `run_verification` with `{"changeset_id":"chg_...","check":"test"}`.
The command, timeout, output, exit status, duration, and exact candidate
fingerprint are recorded by Foremerge. The registry is shared across linked
worktrees through Git's common directory and is not committed to source.

Named checks are an interface boundary, not a sandbox. They execute with the
Foremerge process's local permissions. Review them like any other repository
automation.

## Source clone versus installed binary

A source clone contains native skill and MCP template files so its own agents
can coordinate immediately. The Rust package embeds the canonical skill, so a
binary installed through Cargo can install the same content into any other
repository with `foremerge setup`.

The generated MCP command runs `foremerge --cwd /absolute/repository mcp`.
MCP is a direct stdio adapter over the shared SQLite store and does not require
the optional HTTP daemon.

## Repair or remove

If a diagnostic reports stale content, inspect the diff and rerun the matching
setup command with `--force`.

To remove an integration:

- remove only the client's `skills/foremerge` directory;
- remove only `mcpServers.foremerge` from Claude or Cursor JSON; and
- run `codex mcp remove foremerge` for Codex.

Do not delete the complete shared client configuration file. Removing client
integration does not delete Foremerge's SQLite state, accepted Git refs, or
event history.
