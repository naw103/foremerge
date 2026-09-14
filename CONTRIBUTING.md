# Contributing to Foremerge

Foremerge is an early-stage coordination protocol for autonomous software
engineering. Contributions that make the local workflow easier to understand,
safer to operate, or simpler to integrate with Git are especially welcome.

## Before you start

- Use an issue to discuss protocol changes, database migrations, or new public
  interfaces before investing in a large implementation.
- Keep Git as the durable compatibility layer. A proposal to replace Git or to
  require one shared worktree is outside the current project direction.
- Claims are advisory. New behavior must warn and help agents coordinate rather
  than introduce hard locks.
- Do not add a network service, hosted dependency, or model API requirement to
  the default local workflow.

Small fixes can go directly to a pull request.

## Development setup

Prerequisites:

- Rust 1.85 or newer
- a recent Git release with worktree support

From your clone of the repository, run the verification suite:

```console
cd foremerge
make verify
```

The equivalent Cargo commands are:

```console
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
```

Run `cargo run -- --help` to inspect the current command-line interface. Tests
that create repositories must use temporary directories and set repository-local
Git identity; they must not depend on a contributor's global Git configuration.

### The repository dogfoods its own coordination

This repository ships its own Foremerge client integration: `.mcp.json` (Claude
Code), `.cursor/mcp.json` (Cursor), and five copies of the agent skill. If you
open the repository with one of those coding-agent clients, the client will
offer to enable the Foremerge MCP server and skill; clients prompt before
enabling project-level configuration, so nothing runs without your consent.

`.codex/skills/foremerge/SKILL.md` is the canonical copy. `src/integrations.rs`
embeds it at compile time and `foremerge setup` installs it for each native
client it supports, so edit that file and copy it byte-for-byte to the four
twins:

| Copy | Why it exists |
| --- | --- |
| `.claude/skills/foremerge/SKILL.md` | Claude Code's own convention |
| `.cursor/skills/foremerge/SKILL.md` | Cursor's own convention |
| `.agents/skills/foremerge/SKILL.md` | The portable Agent Skills location, read by Cursor, Codex CLI, Gemini CLI, Copilot and OpenClaw |
| `plugins/foremerge/skills/foremerge/SKILL.md` | The Claude Code plugin, distributed as a `git-subdir` checkout that cannot reference paths outside itself |

`tests/skill_parity.rs` fails if any copy drifts, if the plugin's `.mcp.json`
diverges from the repository's, if `.claude-plugin/marketplace.json` stops
pointing at the plugin directory, or if the plugin manifest version falls
behind the crate version. Bump
`plugins/foremerge/.claude-plugin/plugin.json` with every release.

Expect the skill to appear more than once in some clients while you work in
this repository. Cursor reads `.cursor/skills/` and `.agents/skills/` and also
scans `.claude/skills/` and `.codex/skills/` for compatibility, so all four
copies are discoverable at once. Cursor currently surfaces those copies
separately rather than deduplicating them by skill name. The parity test keeps
their instructions byte-identical, but the duplicate discovery can still spend
context and clutter skill selection. The copies are kept rather than collapsed
because each one is some client's own documented convention, and a repository
that dogfoods its integration should carry the layout its users will have. If a
client ever resolves two copies to *different* instructions, that is the parity
test failing, not a client bug.

### The plugin is catalogued separately from the plugin directory

`plugins/foremerge/` is the plugin; `.claude-plugin/marketplace.json` at the
repository root is what makes it installable. Claude Code resolves
`/plugin install <plugin>@<marketplace>` through a marketplace catalogue, so a
valid plugin directory with no catalogue entry cannot be installed by name. The
two are edited together and the parity test ties them.

## What a good change includes

1. A focused problem statement and the reason the change belongs in the MVP.
2. Tests at the narrowest useful level, plus an integration test when behavior
   crosses SQLite, MCP, HTTP, process, or Git boundaries.
3. Documentation for any public command, protocol field, state transition, or
   operational limitation that changes.
4. No generated databases, worktrees, credentials, build output, or benchmark
   results without their run metadata.

For conflict-detection changes, include at least one positive fixture and one
independent-intent negative control. A detector that only makes the headline demo
pass is not sufficient.

For lifecycle changes, demonstrate both the allowed transition and rejection of
an invalid transition. Failed validation must never move a ChangeSet into an
accepted or committed state.

## Pull requests

- Keep changes small enough to review and explain noteworthy design choices.
- Run `make verify` from a clean checkout.
- Run `make msrv` before pushing. CI lints on the pinned minimum supported Rust
  version as well as stable, and Clippy's lint set differs between them, so a
  clean `make verify` on a newer toolchain does not guarantee a green CI run.
- Update `CHANGELOG.md` under `Unreleased` for user-visible behavior.
- Call out migrations, compatibility implications, and security considerations.
- Do not rewrite unrelated work or commit generated `.foremerge` state.

Maintainers may ask for an architecture decision to be documented before merging
a protocol or storage change. Public interfaces are still pre-1.0, but avoid
breaking them without a migration path and a clear changelog entry.

## Benchmark contributions

The benchmark corpus lives in `benchmarks/scenarios`. Scenario files describe
ground truth; they are not performance results. See
[`benchmarks/README.md`](benchmarks/README.md) and
[`docs/benchmark-plan.md`](docs/benchmark-plan.md) before adding a fixture or
publishing a comparative claim.

## Reporting security and conduct concerns

Do not disclose vulnerabilities in a public issue. Follow `SECURITY.md` for a
private report. Conduct concerns are handled under `CODE_OF_CONDUCT.md`.

## License

By submitting a contribution, you agree that it is licensed under the
[Apache License 2.0](LICENSE), without additional terms unless agreed in writing
by the project maintainers.
