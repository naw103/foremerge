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

/// What acceptance requires when Foremerge has not verified the work itself.
///
/// This governs agents. A human operator can always override from the CLI, and
/// either way the outcome is recorded on the ChangeSet rather than disguised as
/// a check that passed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AcceptancePolicy {
    /// Only work Foremerge verified itself may be accepted. The default,
    /// because it is the existing behaviour and the stronger guarantee.
    #[default]
    Strict,
    /// Work with nothing to verify may be accepted, and is recorded as
    /// `UNVERIFIED` with a reason. A check that ran and *failed* still needs an
    /// operator override, because a failure is real evidence rather than an
    /// absence of it.
    Advisory,
}

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
    #[serde(default)]
    pub acceptance_policy: AcceptancePolicy,
    /// Warn when a published `symbol:` scope names something that appears
    /// nowhere in the worktree. Off by default, because a scope may legitimately
    /// name something the agent is about to create.
    #[serde(default)]
    pub verify_symbol_scopes: bool,
}

impl Default for CheckRegistry {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            checks: BTreeMap::new(),
            acceptance_policy: AcceptancePolicy::default(),
            verify_symbol_scopes: false,
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

/// Set the acceptance policy for this repository.
pub fn set_policy_at(config_path: &Path, policy: AcceptancePolicy) -> Result<CheckRegistry> {
    let mut registry = load_path(config_path)?;
    registry.acceptance_policy = policy;
    save_path(config_path, &registry)?;
    Ok(registry)
}

/// The acceptance policy recorded for a repository, defaulting to strict when
/// no registry file exists yet. Read failures also fall back to strict: a
/// damaged registry must never be a route to accepting unverified work.
pub fn acceptance_policy_at(config_path: &Path) -> AcceptancePolicy {
    load_path(config_path)
        .map(|registry| registry.acceptance_policy)
        .unwrap_or_default()
}

/// Turn the symbol-existence advisory on or off for this repository.
pub fn set_verify_symbol_scopes_at(config_path: &Path, enabled: bool) -> Result<CheckRegistry> {
    let mut registry = load_path(config_path)?;
    registry.verify_symbol_scopes = enabled;
    save_path(config_path, &registry)?;
    Ok(registry)
}

/// Whether the symbol-existence advisory is enabled, defaulting to off.
pub fn verify_symbol_scopes_at(config_path: &Path) -> bool {
    load_path(config_path)
        .map(|registry| registry.verify_symbol_scopes)
        .unwrap_or(false)
}

/// Whether the registered checks can actually run from `cwd`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckDiagnostic {
    pub registered: usize,
    pub acceptance_policy: AcceptancePolicy,
    /// Checks whose executable could not be found, or whose relative path
    /// argument does not exist here.
    pub unrunnable: Vec<String>,
    pub warnings: Vec<String>,
}

/// Diagnose the registry without running anything.
///
/// A check is reported as unrunnable when its executable is not on `PATH` and
/// not a file here, or when a relative path in its argv does not exist in this
/// worktree. That second case is the one that bites: dependency directories are
/// usually gitignored, so `git worktree add` never creates them and every check
/// fails in every agent worktree while the primary checkout looks fine.
pub fn diagnose(config_path: &Path, cwd: &Path) -> CheckDiagnostic {
    let registry = load_path(config_path).unwrap_or_default();
    let mut unrunnable = Vec::new();
    let mut warnings = Vec::new();
    for (name, check) in &registry.checks {
        let Some(program) = check.command.first() else {
            continue;
        };
        if !executable_exists(program, cwd) {
            unrunnable.push(name.clone());
            warnings.push(format!(
                "check '{name}' cannot run here: '{program}' is not on PATH and not a file in this worktree"
            ));
            continue;
        }
        // A relative path argument that does not exist is the gitignored
        // dependency directory problem.
        if let Some(missing) = check.command[1..]
            .iter()
            .filter(|argument| argument.contains('/') && !argument.starts_with('-'))
            .find(|argument| !cwd.join(argument).exists())
        {
            unrunnable.push(name.clone());
            warnings.push(format!(
                "check '{name}' refers to '{missing}', which does not exist in this worktree; if it is gitignored, `git worktree add` will not have created it"
            ));
        }
    }
    if registry.checks.is_empty() && registry.acceptance_policy == AcceptancePolicy::Strict {
        warnings.push(
            "no verification checks are registered and the acceptance policy is strict, so no ChangeSet can be accepted here. Register one with `foremerge checks set <name> -- <command>`, or, if this repository has nothing to verify, run `foremerge checks policy advisory`.".to_string(),
        );
    }
    CheckDiagnostic {
        registered: registry.checks.len(),
        acceptance_policy: registry.acceptance_policy,
        unrunnable,
        warnings,
    }
}

fn executable_exists(program: &str, cwd: &Path) -> bool {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return cwd.join(candidate).exists() || candidate.exists();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
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
