# Foremerge

Coordinate parallel coding agents, above Git.

Foremerge is a local CLI and MCP server. Agents publish what they intend to
change before they edit, so duplicate and incompatible work shows up before it
becomes a merge conflict. There is no account, no API key, and no network call
in the coordination path.

This package installs the Foremerge release binary for your platform. The
binary is byte-for-byte the archive from the
[GitHub release](https://github.com/naw103/foremerge/releases), shipped inside
a platform package, so installing does not download or compile anything.

## Install

```sh
npm install -g foremerge@0.5.0
foremerge --version
```

This installs `foremerge` and `fmg`, the same program under a shorter name.

Supported: macOS (Apple silicon and Intel), Linux x64 and arm64 with glibc,
and Windows x64. For Alpine or other platforms, use
`curl -fsSL https://foremerge.com/install.sh | sh` or
`cargo install --locked foremerge`.

## Run the MCP server

From inside the repository you want to coordinate:

```sh
foremerge init
foremerge setup all
```

`setup` writes the native MCP configuration for Claude Code, Codex, and
Cursor, pointing at the absolute path of this installed binary. For any other
client, point it at the same installation:

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

Do not register an unpinned `npx foremerge` command with a client. `npx`
can run whatever `foremerge` the project directory provides, or a version it
fetches on its own, so the server's version could change without you choosing
it. A newer Foremerge migrates the coordination ledger forward, and every older
server and shell then refuses it. If a client can only run `npx`, pin the
exact version:

```json
{
  "mcpServers": {
    "foremerge": {
      "command": "npx",
      "args": ["-y", "foremerge@0.5.0", "mcp"]
    }
  }
}
```

## Upgrading

Upgrade every installation and every client configuration together: run
`npm install -g foremerge@<version>`, change any pinned `npx` version to match,
run `foremerge setup all` again in each coordinated repository, and restart the
clients. The
[upgrade guide](https://github.com/naw103/foremerge/blob/main/docs/mcp-setup.md#upgrading-foremerge)
explains why the versions must match.

## Documentation

- [README and quick start](https://github.com/naw103/foremerge#readme)
- [MCP setup](https://github.com/naw103/foremerge/blob/main/docs/mcp-setup.md)
- [Limitations](https://github.com/naw103/foremerge/blob/main/docs/limitations.md)

Apache-2.0.
