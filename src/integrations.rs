use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

pub const SKILL_CONTENT: &str = include_str!("../.codex/skills/foremerge/SKILL.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    Codex,
    Claude,
    Cursor,
}

impl Client {
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
        }
    }

    fn skill_path(self, root: &Path) -> PathBuf {
        root.join(format!(".{}/skills/foremerge/SKILL.md", self.name()))
    }

    fn mcp_path(self, root: &Path) -> Option<PathBuf> {
        match self {
            Self::Codex => None,
            Self::Claude => Some(root.join(".mcp.json")),
            Self::Cursor => Some(root.join(".cursor/mcp.json")),
        }
    }

    fn executable(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor-agent",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInstall {
    pub path: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInstallReport {
    pub client: String,
    pub skill: FileInstall,
    pub mcp: Option<FileInstall>,
    pub mcp_configured: bool,
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientDiagnostic {
    pub client: String,
    pub client_available: bool,
    pub skill_path: String,
    pub skill_installed: bool,
    pub skill_current: bool,
    pub mcp_path: Option<String>,
    pub mcp_configured: bool,
    pub ready: bool,
    pub next_step: Option<String>,
}

pub fn install(
    root: &Path,
    clients: &[Client],
    foremerge_exe: &Path,
    force: bool,
    configure_mcp: bool,
) -> Result<Vec<ClientInstallReport>> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    clients
        .iter()
        .copied()
        .map(|client| install_client(&root, client, foremerge_exe, force, configure_mcp))
        .collect()
}

pub fn diagnose(root: &Path, clients: &[Client]) -> Vec<ClientDiagnostic> {
    clients
        .iter()
        .copied()
        .map(|client| diagnose_client(root, client))
        .collect()
}

fn install_client(
    root: &Path,
    client: Client,
    foremerge_exe: &Path,
    force: bool,
    configure_mcp: bool,
) -> Result<ClientInstallReport> {
    let skill_path = client.skill_path(root);
    let skill = write_managed(&skill_path, SKILL_CONTENT.as_bytes(), force)?;
    let (mcp, mcp_configured, next_step) = match client {
        Client::Codex => {
            if configure_mcp {
                let configured = configure_codex_mcp(root, foremerge_exe, force)?;
                let next = (!configured).then(|| {
                    format!(
                        "Run `codex mcp add foremerge -- {} --cwd {} mcp` after installing Codex CLI.",
                        foremerge_exe.display(),
                        root.display()
                    )
                });
                (None, configured, next)
            } else {
                let configured = codex_mcp_configured();
                (
                    None,
                    configured,
                    (!configured).then(|| format!(
                        "Run `codex mcp add foremerge -- {} --cwd {} mcp` to enable Foremerge tools.",
                        foremerge_exe.display(),
                        root.display()
                    )),
                )
            }
        }
        Client::Claude | Client::Cursor => {
            let path = client.mcp_path(root).expect("project MCP path");
            if configure_mcp {
                let change = merge_mcp_config(&path, root, foremerge_exe, force)?;
                (Some(change), true, None)
            } else {
                let configured = mcp_json_configured(&path);
                (
                    None,
                    configured,
                    (!configured).then(|| format!("Configure Foremerge in {}.", path.display())),
                )
            }
        }
    };
    Ok(ClientInstallReport {
        client: client.name().to_string(),
        skill,
        mcp,
        mcp_configured,
        next_step,
    })
}

fn diagnose_client(root: &Path, client: Client) -> ClientDiagnostic {
    let skill_path = client.skill_path(root);
    let skill_bytes = fs::read(&skill_path).ok();
    let skill_installed = skill_bytes.is_some();
    let skill_current = skill_bytes.as_deref() == Some(SKILL_CONTENT.as_bytes());
    let mcp_path = client.mcp_path(root);
    let mcp_configured = match mcp_path.as_deref() {
        Some(path) => mcp_json_configured(path),
        None => codex_mcp_configured(),
    };
    let client_available = command_available(client.executable())
        || (client == Client::Cursor && command_available("cursor"));
    let ready = client_available && skill_current && mcp_configured;
    let next_step = if !client_available {
        Some(format!("Install the {} client.", client.name()))
    } else if !skill_current || !mcp_configured {
        Some(format!(
            "Run `foremerge setup {}` from this repository.",
            client.name()
        ))
    } else {
        None
    };
    ClientDiagnostic {
        client: client.name().to_string(),
        client_available,
        skill_path: skill_path.to_string_lossy().into_owned(),
        skill_installed,
        skill_current,
        mcp_path: mcp_path.map(|value| value.to_string_lossy().into_owned()),
        mcp_configured,
        ready,
        next_step,
    }
}

fn merge_mcp_config(
    path: &Path,
    root: &Path,
    foremerge_exe: &Path,
    force: bool,
) -> Result<FileInstall> {
    let mut document = if path.exists() {
        let bytes =
            fs::read(path).with_context(|| format!("read MCP config {}", path.display()))?;
        serde_json::from_slice::<Value>(&bytes)
            .with_context(|| format!("INVALID_INPUT: parse MCP config {}", path.display()))?
    } else {
        json!({})
    };
    let object = document.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "INVALID_INPUT: MCP config must contain a JSON object: {}",
            path.display()
        )
    })?;
    let servers = object
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("INVALID_INPUT: mcpServers must be a JSON object"))?;
    let desired = json!({
        "command": foremerge_exe.to_string_lossy(),
        "args": ["--cwd", root.to_string_lossy(), "mcp"]
    });
    if let Some(existing) = servers.get("foremerge") {
        if existing == &desired || mcp_entry_configured(existing) {
            return Ok(FileInstall {
                path: path.to_string_lossy().into_owned(),
                status: "unchanged".to_string(),
            });
        }
        if !force {
            bail!(
                "ALREADY_EXISTS: {} already defines mcpServers.foremerge differently; inspect it or rerun with --force",
                path.display()
            );
        }
    }
    servers.insert("foremerge".to_string(), desired);
    let mut encoded = serde_json::to_vec_pretty(&document)?;
    encoded.push(b'\n');
    write_managed(path, &encoded, true)
}

fn configure_codex_mcp(root: &Path, foremerge_exe: &Path, force: bool) -> Result<bool> {
    if !command_available("codex") {
        return Ok(false);
    }
    let configured = codex_mcp_configured();
    if configured && !force {
        return Ok(true);
    }
    if configured {
        let output = Command::new("codex")
            .args(["mcp", "remove", "foremerge"])
            .output()
            .context("run `codex mcp remove foremerge`")?;
        if !output.status.success() {
            bail!(
                "CHECK_FAILED: Codex could not replace the existing Foremerge MCP entry: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    let output = Command::new("codex")
        .args(["mcp", "add", "foremerge", "--"])
        .arg(foremerge_exe)
        .arg("--cwd")
        .arg(root)
        .arg("mcp")
        .output()
        .context("run `codex mcp add foremerge`")?;
    if !output.status.success() {
        bail!(
            "CHECK_FAILED: Codex could not register the Foremerge MCP server: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(true)
}

fn codex_mcp_configured() -> bool {
    Command::new("codex")
        .args(["mcp", "get", "foremerge"])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn mcp_json_configured(path: &Path) -> bool {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("mcpServers")?.get("foremerge").cloned())
        .is_some_and(|entry| mcp_entry_configured(&entry))
}

fn mcp_entry_configured(entry: &Value) -> bool {
    let Some(command) = entry.get("command").and_then(Value::as_str) else {
        return false;
    };
    let executable = Path::new(command)
        .file_name()
        .and_then(|value| value.to_str());
    let Some(args) = entry.get("args").and_then(Value::as_array) else {
        return false;
    };
    executable == Some("foremerge")
        && args.last().and_then(Value::as_str) == Some("mcp")
        && args.iter().all(Value::is_string)
}

fn write_managed(path: &Path, content: &[u8], force: bool) -> Result<FileInstall> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "INVALID_INPUT: managed integration path must be a regular file: {}",
                path.display()
            );
        }
        let existing = fs::read(path)?;
        if existing == content {
            return Ok(FileInstall {
                path: path.to_string_lossy().into_owned(),
                status: "unchanged".to_string(),
            });
        }
        if !force {
            bail!(
                "ALREADY_EXISTS: {} already exists with different content; inspect it or rerun with --force",
                path.display()
            );
        }
    }
    let parent = path.parent().context("integration file parent")?;
    fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(".foremerge-{}.tmp", Uuid::new_v4().simple()));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(content)?;
        file.sync_all()?;
        fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result?;
    Ok(FileInstall {
        path: path.to_string_lossy().into_owned(),
        status: "written".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_integrations_merge_without_clobbering_other_servers() {
        let temp = tempfile::tempdir().unwrap();
        let cursor_config = temp.path().join(".cursor/mcp.json");
        fs::create_dir_all(cursor_config.parent().unwrap()).unwrap();
        fs::write(
            &cursor_config,
            br#"{"mcpServers":{"existing":{"command":"existing"}},"custom":true}"#,
        )
        .unwrap();
        let reports = install(
            temp.path(),
            &[Client::Claude, Client::Cursor],
            Path::new("/usr/local/bin/foremerge"),
            false,
            true,
        )
        .unwrap();
        assert_eq!(reports.len(), 2);
        assert_eq!(
            fs::read(temp.path().join(".claude/skills/foremerge/SKILL.md")).unwrap(),
            SKILL_CONTENT.as_bytes()
        );
        let cursor: Value = serde_json::from_slice(&fs::read(cursor_config).unwrap()).unwrap();
        assert_eq!(cursor["custom"], true);
        assert_eq!(cursor["mcpServers"]["existing"]["command"], "existing");
        assert_eq!(
            cursor["mcpServers"]["foremerge"]["command"],
            "/usr/local/bin/foremerge"
        );
        let second = install(
            temp.path(),
            &[Client::Claude, Client::Cursor],
            Path::new("/usr/local/bin/foremerge"),
            false,
            true,
        )
        .unwrap();
        assert!(
            second
                .iter()
                .all(|report| report.skill.status == "unchanged")
        );
    }

    #[test]
    fn skill_overwrite_requires_force() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".claude/skills/foremerge/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "custom instructions").unwrap();
        assert!(
            install(
                temp.path(),
                &[Client::Claude],
                Path::new("foremerge"),
                false,
                false,
            )
            .is_err()
        );
        install(
            temp.path(),
            &[Client::Claude],
            Path::new("foremerge"),
            true,
            false,
        )
        .unwrap();
        assert_eq!(fs::read(path).unwrap(), SKILL_CONTENT.as_bytes());
    }
}
