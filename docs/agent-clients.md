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
path; the installer also accepts an existing entry that is verifiably current:
its command is an absolute path to an existing `foremerge` executable and any
`--cwd` argument points at this repository. A bare or relative command is not
current, because the MCP client would resolve it in its own `PATH` and working
directory rather than this process's. That includes the portable `.mcp.json`
and `.cursor/mcp.json` templates tracked in a source clone, so running setup in
a clone reports its MCP entry as stale and rewrites it with absolute paths under
`--force`. Setup is idempotent when installed content is current.

Every installed skill file ends with a managed stamp naming the release that
wrote it and a digest of the instructions above it. Upgrading Foremerge
therefore replaces its own unedited skill file in place, with no `--force`:
the digest proves nothing was changed after Foremerge wrote it. A file whose
body no longer matches its own stamp was edited, and is never replaced without
`--force`. A file already carrying this release's instructions is left exactly
as it is, stamped or not, so a source clone's tracked skill files are not
rewritten.

Setup never replaces an edited skill file or a differing `mcpServers.foremerge`
entry by default; a stale entry (moved repository, deleted binary) is refused
rather than reported as configured. Inspect the existing content first; use
`--force` only when replacing it is intentional. Use `--skip-mcp` to install
the skill without changing MCP configuration.

`setup all` attempts every requested client even when one fails: the report
lists each client's result, failed clients carry an `error` field, and the
command exits nonzero if any client failed.

## What each client receives

| Client | Skill | MCP configuration |
| --- | --- | --- |
| Codex | `.codex/skills/foremerge/SKILL.md` | Registered as `foremerge` through `codex mcp add` |
| Claude Code | `.claude/skills/foremerge/SKILL.md` | Project `.mcp.json` |
| Cursor | `.cursor/skills/foremerge/SKILL.md` | Project `.cursor/mcp.json` |

Codex's CLI stores MCP registrations in user-global configuration, not in the
repository, so Foremerge uses the supported `codex mcp` command rather than
editing that file directly. Setup reports this one out-of-repository write as
`Codex user-level configuration (codex mcp)`. If the Codex CLI is unavailable,
setup still installs the repository skill and returns the exact registration
command as its next step.

Because the registration is user-global, Codex coordinates one repository at a
time: the single `foremerge` entry bakes in the repository's `--cwd`. Running
`foremerge setup codex` in a second repository refuses when the entry points at
a different repository; pass `--force` to repoint Codex at the current
repository. The report then carries an explicit warning naming the repository
Codex now coordinates and the previous repository where `foremerge setup codex`
must be re-run to switch back.

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

If a diagnostic reports stale content, rerun the matching setup command. The
diagnostic's `next_step` names `--force` only when setup would otherwise refuse,
which for a skill file means it was edited after Foremerge wrote it. Inspect the
diff before forcing in that case.

To remove an integration:

- remove only the client's `skills/foremerge` directory;
- remove only `mcpServers.foremerge` from Claude or Cursor JSON; and
- run `codex mcp remove foremerge` for Codex.

Do not delete the complete shared client configuration file. Removing client
integration does not delete Foremerge's SQLite state, accepted Git refs, or
event history.
