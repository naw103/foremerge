# Installing Foremerge (instructions for an AI assistant)

This file tells an AI assistant how to install and configure Foremerge as an
MCP server. If you are a human, the README covers the same ground with more
context.

Foremerge is local-first. There is no account, no API key, and no network call
in the coordination path. Everything below happens on the user's machine.

## 1. Install the binary

The MCP server is a subcommand of the `foremerge` binary, so the binary must
exist before any MCP configuration will work.

```bash
cargo install foremerge
```

If Rust is not available, use the install script instead:

```bash
curl -fsSL https://foremerge.com/install.sh | sh
```

Confirm it resolved:

```bash
foremerge --version
```

## 2. Ask before initialising the repository

Foremerge stores coordination state under Git's common directory. It will not
create that store on its own initiative, and neither should you.

```bash
foremerge --json doctor --client all
```

- `git_repository: false` means this directory is not a Git repository. Stop
  and say so.
- `database_ok: false` means the repository has never opted into coordination.
  **Ask the user before running `foremerge init`.** Whether a repository
  coordinates is the operator's decision, and a store created on your
  initiative coordinates nothing while looking as though it does.

`doctor` opens the store read-only and never creates one, so a negative answer
here is trustworthy.

## 3. Register the MCP server

Foremerge writes the client's own native configuration for you:

```bash
foremerge --json setup claude
foremerge --json setup codex
foremerge --json setup cursor
```

For a client Foremerge does not yet install natively, including Cline, add a
stdio server entry by hand. The server takes no arguments beyond `mcp` and
resolves the repository from the working directory it is launched in:

```json
{
  "mcpServers": {
    "foremerge": {
      "command": "foremerge",
      "args": ["mcp"]
    }
  }
}
```

Setup refuses to replace a differing existing entry unless the user explicitly
passes `--force`. Do not pass `--force` on your own initiative.

## 4. Configure at least one named verification check

**This is the step most MCP servers do not have, and skipping it leaves the
install non-functional for acceptance.**

Foremerge will not accept a ChangeSet on an agent's own report that tests
passed. Acceptance requires a passing run of a *named check*: a command a human
configured outside the MCP surface. Agents can run a named check by name, but
`run_verification` deliberately accepts a check name rather than raw argv, so
an agent cannot invent the command it is judged by.

Ask the user which commands verify their project, then register them:

```bash
foremerge checks set test -- <the project's test command>
foremerge checks set lint -- <the project's lint command>
```

For example, in a Rust project:

```bash
foremerge checks set test -- cargo test --all-targets
foremerge checks set lint -- cargo clippy --all-targets -- -D warnings
```

Confirm what is registered:

```bash
foremerge --json checks list
```

Do not add or replace named checks without the user's authorisation. If a check
you need is missing, ask for it rather than provisioning it yourself.

## 5. Verify the install

```bash
foremerge --json doctor --client all
foremerge --json status
```

A healthy install reports `git_repository: true`, `database_ok: true`, the
client's MCP entry present, and at least one named check.

## What not to do

- Do not expose the optional HTTP daemon beyond loopback.
- Do not run `foremerge init` without asking.
- Do not add or widen `validation-exclusions`; those are operator-only by
  design and there is deliberately no MCP mutation tool for them.
- Do not use Foremerge to bypass ordinary Git review and integration.
  Acceptance creates a ref, it does not merge code.
