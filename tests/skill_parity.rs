//! The agent skill ships as one canonical file and several byte-identical
//! copies, one per client discovery convention. `src/integrations.rs` embeds
//! the canonical copy at compile time, so a drifted twin is not a cosmetic
//! problem: it is a client reading instructions the binary does not install.
//!
//! The copies exist because clients do not agree on where skills live, and
//! because the Claude Code plugin is distributed as a `git-subdir` checkout
//! that cannot reference a path outside its own directory.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Every client copy of the skill must equal the canonical one byte for byte.
#[test]
fn every_skill_copy_matches_the_canonical_source() {
    let root = repo_root();
    let canonical_path = root.join(".codex/skills/foremerge/SKILL.md");
    let canonical = read(&canonical_path);

    // `.codex` is the canonical copy embedded by `src/integrations.rs`.
    // `.claude` and `.cursor` are the clients' own conventions. `.agents` is
    // the portable Agent Skills location read by Cursor, Codex CLI, Gemini
    // CLI, Copilot and OpenClaw. The plugin copy ships inside the Claude Code
    // plugin subtree.
    let copies = [
        ".claude/skills/foremerge/SKILL.md",
        ".cursor/skills/foremerge/SKILL.md",
        ".agents/skills/foremerge/SKILL.md",
        "plugins/foremerge/skills/foremerge/SKILL.md",
    ];

    for copy in copies {
        let path = root.join(copy);
        assert_eq!(
            read(&path),
            canonical,
            "{copy} has drifted from {}; copy the canonical file byte for byte",
            canonical_path.display()
        );
    }
}

/// The plugin is distributed as a standalone subtree, so it carries its own
/// MCP registration rather than referencing the repository's.
#[test]
fn the_plugin_mcp_registration_matches_the_repository_one() {
    let root = repo_root();
    assert_eq!(
        read(&root.join("plugins/foremerge/.mcp.json")),
        read(&root.join(".mcp.json")),
        "the plugin's .mcp.json has drifted from the repository's"
    );
}

/// A released plugin whose manifest still claims the previous version is
/// wrong in a way nothing else catches: the marketplace pins a tag, and the
/// version it shows comes from this file rather than from the crate.
#[test]
fn the_plugin_manifest_version_matches_the_crate_version() {
    let manifest = read(&repo_root().join("plugins/foremerge/.claude-plugin/plugin.json"));
    let manifest: Value = serde_json::from_str(&manifest).expect("plugin.json is valid JSON");

    assert_eq!(
        manifest["version"]
            .as_str()
            .expect("version in plugin.json"),
        env!("CARGO_PKG_VERSION"),
        "plugins/foremerge/.claude-plugin/plugin.json must be bumped with the crate"
    );
}

/// A plugin directory is not installable on its own. Claude Code resolves
/// `/plugin install <plugin>@<marketplace>` through a marketplace catalogue at
/// the repository root, so an entry that stops naming the plugin directory
/// breaks installation while every file it points at stays valid.
#[test]
fn the_marketplace_lists_the_plugin_directory() {
    let root = repo_root();
    let marketplace = read(&root.join(".claude-plugin/marketplace.json"));
    let marketplace: Value =
        serde_json::from_str(&marketplace).expect("marketplace.json is valid JSON");

    let plugins = marketplace["plugins"]
        .as_array()
        .expect("marketplace.json lists plugins");
    let entry = plugins
        .iter()
        .find(|entry| entry["name"] == "foremerge")
        .expect("marketplace.json has a foremerge entry");

    let source = entry["source"]
        .as_str()
        .expect("the foremerge entry has a string source");
    assert_eq!(
        source, "./plugins/foremerge",
        "the marketplace entry must point at the plugin directory in this repository"
    );
    assert!(
        root.join(source)
            .join(".claude-plugin/plugin.json")
            .is_file(),
        "the marketplace source {source} does not contain a plugin manifest"
    );

    let manifest = read(&root.join("plugins/foremerge/.claude-plugin/plugin.json"));
    let manifest: Value = serde_json::from_str(&manifest).expect("plugin.json is valid JSON");
    assert_eq!(
        entry["name"], manifest["name"],
        "the marketplace entry name must match the plugin manifest name, or \
         `/plugin install foremerge@foremerge` resolves to nothing"
    );
}
