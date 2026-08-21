use crate::git;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const CONFIG_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_ARGS: usize = 128;
const MAX_TIMEOUT_SECONDS: u64 = 3600;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedCheck {
    pub command: Vec<String>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRegistry {
    pub version: u32,
    #[serde(default)]
    pub checks: BTreeMap<String, NamedCheck>,
}

impl Default for CheckRegistry {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            checks: BTreeMap::new(),
        }
    }
}

/// The trusted registry path inside a resolved Git common directory.
pub fn registry_path(common_dir: &Path) -> PathBuf {
    common_dir.join("foremerge").join("checks.json")
}

/// Resolve the registry strictly through Git discovery. The registry is a
/// trusted command source, so it must live under Git's common directory; the
/// distributable `.foremerge` fallback directory is deliberately refused.
pub fn path(cwd: &Path) -> Result<PathBuf> {
    let repo = git::discover(cwd).map_err(|_| {
        anyhow::anyhow!(
            "INVALID_INPUT: verification checks are repository-scoped and live under Git's common directory; run this inside the Git repository whose checks you want to use"
        )
    })?;
    Ok(registry_path(&repo.common_dir))
}

pub fn load(cwd: &Path) -> Result<CheckRegistry> {
    load_path(&path(cwd)?)
}

pub fn load_at(config_path: &Path) -> Result<CheckRegistry> {
    load_path(config_path)
}

pub fn get(cwd: &Path, name: &str) -> Result<NamedCheck> {
    get_at(&path(cwd)?, name)
}

pub fn get_at(config_path: &Path, name: &str) -> Result<NamedCheck> {
    validate_name(name)?;
    load_path(config_path)?
        .checks
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("NOT_FOUND: verification check '{name}' is not configured for this repository; a repository operator can configure trusted checks with the 'foremerge checks' CLI"))
}

pub fn set(cwd: &Path, name: &str, check: NamedCheck) -> Result<CheckRegistry> {
    set_at(&path(cwd)?, name, check)
}

pub fn set_at(config_path: &Path, name: &str, check: NamedCheck) -> Result<CheckRegistry> {
    validate_name(name)?;
    validate_check(&check)?;
    let mut registry = load_path(config_path)?;
    registry.checks.insert(name.to_string(), check);
    save_path(config_path, &registry)?;
    Ok(registry)
}

pub fn remove(cwd: &Path, name: &str) -> Result<CheckRegistry> {
    remove_at(&path(cwd)?, name)
}

pub fn remove_at(config_path: &Path, name: &str) -> Result<CheckRegistry> {
    validate_name(name)?;
    let mut registry = load_path(config_path)?;
    if registry.checks.remove(name).is_none() {
        bail!("NOT_FOUND: verification check '{name}' is not configured");
    }
    save_path(config_path, &registry)?;
    Ok(registry)
}

fn load_path(config_path: &Path) -> Result<CheckRegistry> {
    if !config_path.exists() {
        return Ok(CheckRegistry::default());
    }
    let metadata = fs::symlink_metadata(config_path)
        .with_context(|| format!("inspect verification config {}", config_path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "INVALID_INPUT: verification config must be a regular file: {}",
            config_path.display()
        );
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        bail!(
            "RESOURCE_LIMIT: verification config exceeds {MAX_CONFIG_BYTES} bytes: {}",
            config_path.display()
        );
    }
    let bytes = fs::read(config_path)
        .with_context(|| format!("read verification config {}", config_path.display()))?;
    let registry: CheckRegistry = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "INVALID_INPUT: parse verification config {}",
            config_path.display()
        )
    })?;
    if registry.version != CONFIG_VERSION {
        bail!(
            "INVALID_INPUT: unsupported verification config version {}; expected {CONFIG_VERSION}",
            registry.version
        );
    }
    for (name, check) in &registry.checks {
        validate_name(name)?;
        validate_check(check)?;
    }
    Ok(registry)
}

fn save_path(config_path: &Path, registry: &CheckRegistry) -> Result<()> {
    let parent = config_path.parent().context("verification config parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create verification config directory {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    if config_path.exists() {
        let metadata = fs::symlink_metadata(config_path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "INVALID_INPUT: verification config must be a regular file: {}",
                config_path.display()
            );
        }
    }
    let mut encoded = serde_json::to_vec_pretty(registry)?;
    encoded.push(b'\n');
    let temp_path = parent.join(format!(".checks-{}.tmp", Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write_result = (|| -> Result<()> {
        let mut file = options.open(&temp_path)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        fs::rename(&temp_path, config_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(config_path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_' | b'.'))
        && name.as_bytes()[0].is_ascii_alphanumeric();
    if !valid {
        bail!(
            "INVALID_INPUT: check name must start with an ASCII letter or number and contain at most 64 letters, numbers, '.', '-', or '_'"
        );
    }
    Ok(())
}

fn validate_check(check: &NamedCheck) -> Result<()> {
    if check.command.is_empty() || check.command[0].trim().is_empty() {
        bail!("INVALID_INPUT: verification command must contain an executable argv element");
    }
    if check.command.len() > MAX_ARGS {
        bail!("RESOURCE_LIMIT: verification command exceeds {MAX_ARGS} argv elements");
    }
    let bytes = check.command.iter().map(String::len).sum::<usize>();
    if bytes > MAX_COMMAND_BYTES {
        bail!("RESOURCE_LIMIT: verification command exceeds {MAX_COMMAND_BYTES} bytes");
    }
    if !(1..=MAX_TIMEOUT_SECONDS).contains(&check.timeout_seconds) {
        bail!(
            "INVALID_INPUT: verification timeout must be between 1 and {MAX_TIMEOUT_SECONDS} seconds"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(cwd: &Path) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn named_checks_round_trip_and_reject_invalid_input() {
        let temp = tempfile::tempdir().unwrap();
        init_repo(temp.path());
        let expected = NamedCheck {
            command: vec!["cargo".into(), "test".into()],
            timeout_seconds: 300,
        };
        let saved = set(temp.path(), "test", expected.clone()).unwrap();
        assert_eq!(saved.checks["test"], expected);
        assert_eq!(get(temp.path(), "test").unwrap(), expected);
        assert!(set(temp.path(), "bad name", expected.clone()).is_err());
        assert!(
            set(
                temp.path(),
                "empty",
                NamedCheck {
                    command: vec![],
                    timeout_seconds: 1,
                },
            )
            .is_err()
        );
        assert!(remove(temp.path(), "test").unwrap().checks.is_empty());
        let missing = get(temp.path(), "test").unwrap_err();
        let message = format!("{missing:#}");
        assert!(message.starts_with("NOT_FOUND:"), "{message}");
        assert!(
            !message.contains("checks set test"),
            "the error must not coach callers to self-provision trusted checks: {message}"
        );
    }

    #[test]
    fn checks_require_a_git_repository_and_never_use_the_fallback_directory() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".foremerge")).unwrap();
        std::fs::write(
            temp.path().join(".foremerge/checks.json"),
            br#"{"version":1,"checks":{"build":{"command":["true"],"timeout_seconds":10}}}"#,
        )
        .unwrap();
        for result in [
            path(temp.path()).map(|_| ()),
            load(temp.path()).map(|_| ()),
            get(temp.path(), "build").map(|_| ()),
            set(
                temp.path(),
                "build",
                NamedCheck {
                    command: vec!["true".into()],
                    timeout_seconds: 10,
                },
            )
            .map(|_| ()),
            remove(temp.path(), "build").map(|_| ()),
        ] {
            let error = format!("{:#}", result.unwrap_err());
            assert!(error.starts_with("INVALID_INPUT:"), "{error}");
            assert!(error.contains("repository-scoped"), "{error}");
        }
    }
}
