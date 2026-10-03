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
npm install -g foremerge
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
Cursor, pointing at this installed binary. For any other client, add a stdio
server:

```json
{
  "mcpServers": {
    "foremerge": {
      "command": "npx",
      "args": ["-y", "foremerge", "mcp"]
    }
  }
}
```

Run `foremerge setup` from a global install rather than through `npx`: setup
records the absolute path of the binary it runs as, and an `npx` cache path
can be cleared.

## Documentation

- [README and quick start](https://github.com/naw103/foremerge#readme)
- [MCP setup](https://github.com/naw103/foremerge/blob/main/docs/mcp-setup.md)
- [Limitations](https://github.com/naw103/foremerge/blob/main/docs/limitations.md)

Apache-2.0.
