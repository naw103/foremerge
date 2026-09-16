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
cargo install --locked foremerge
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

Setup refuses to replace a differing existing entry unless the user explicitly
passes `--force`. Do not pass `--force` on your own initiative.

For a client Foremerge does not yet install natively, including Cline, add a
stdio server entry by hand. The server resolves its repository the way Git
does, from the directory the client spawns it in:

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

**That entry is only correct if the client spawns the server in the
repository.** Not every client does. Cline has been reported to start stdio
servers with a working directory of `/` rather than the open workspace
([cline#9950](https://github.com/cline/cline/issues/9950)); in that case the
server exits before serving a single request:

```
INVALID_INPUT: no Git repository at /; the MCP server resolves its repository
from the directory the client spawns it in, so start the client inside a
repository or register it with an explicit --cwd
```

Pass the repository as an argument rather than relying on the spawn directory.
`--cwd` is Foremerge's own flag, so it works on every client regardless of what
that client does with the working directory:

```json
{
  "mcpServers": {
    "foremerge": {
      "command": "foremerge",
      "args": ["--cwd", "/absolute/path/to/the/repository", "mcp"]
    }
  }
}
```

Ask the user for that path, or read it from `git rev-parse --show-toplevel` in
the repository they are working in. Do not guess it.

Two further notes for hand-written entries:

- Use the absolute path to the binary as `command` if the client cannot find
  `foremerge` on `PATH`. A desktop client does not always inherit the shell
  `PATH` that `cargo install` extends, so `~/.cargo/bin/foremerge` may be
  invisible to it even though it resolves in a terminal.
- Cline also accepts a `cwd` field on a stdio entry, which it passes to the
  spawned process. It is not in Cline's documented schema, so prefer `--cwd`,
  which is Foremerge's own contract and cannot be dropped by a client update.

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

`doctor --client all` inspects the clients Foremerge installs natively, which
today are Claude Code, Codex and Cursor. It cannot see a hand-written entry for
a client it does not know about, so a clean `doctor` is not evidence that a
Cline entry works. Verify that one through the client itself: ask it to list
its MCP tools and confirm the Foremerge tools are present. If the client
reports the server exited, read its MCP error output before changing anything.
The two startup refusals both name their own cause: the `INVALID_INPUT` message
above means the client spawned the server outside a repository, and
`NOT_INITIALIZED` means the repository itself has never run `foremerge init`.
Neither is fixed by editing the entry again.

The same check run from a shell tells you whether the entry's arguments are
right, without involving the client at all:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"probe","version":"1"}}}' \
  | (cd / && foremerge --cwd /absolute/path/to/the/repository mcp)
```

A working entry answers with a `result` naming `foremerge`. Running it from `/`
is the point: it reproduces the working directory a client like Cline would
give the server.

A `NOT_INITIALIZED` reply instead means the arguments are already right and step
2 has not been done in that repository. The server refuses to create the store
on its own, so this is where you stop and ask the user whether to run
`foremerge init`, rather than running it to make the check pass.

## What not to do

- Do not expose the optional HTTP daemon beyond loopback.
- Do not run `foremerge init` without asking.
- Do not add or widen `validation-exclusions`; those are operator-only by
  design and there is deliberately no MCP mutation tool for them.
- Do not use Foremerge to bypass ordinary Git review and integration.
  Acceptance creates a ref, it does not merge code.
