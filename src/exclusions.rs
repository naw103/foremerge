use crate::git;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

const CONFIG_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_PATHS: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationExclusions {
    pub version: u32,
    /// Repository-relative exact paths or directory prefixes ending in `/`.
    /// These rules are applied only to Git-untracked paths.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl Default for ValidationExclusions {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedExclusions {
    pub rules: ValidationExclusions,
    pub digest: String,
}

impl LoadedExclusions {
    pub fn excludes(&self, candidate: &str) -> bool {
        self.rules.paths.iter().any(|rule| {
            rule.strip_suffix('/').map_or(candidate == rule, |prefix| {
                candidate == prefix || candidate.starts_with(&format!("{prefix}/"))
            })
        })
    }
}

pub fn config_path(common_dir: &Path) -> PathBuf {
    common_dir
        .join("foremerge")
        .join("validation-exclusions.json")
}

pub fn path(cwd: &Path) -> Result<PathBuf> {
    let repo = git::discover(cwd).map_err(|_| {
        anyhow::anyhow!(
            "INVALID_INPUT: validation exclusions are repository-scoped; run this inside the Git repository they apply to"
        )
    })?;
    Ok(config_path(&repo.common_dir))
}

pub fn load(cwd: &Path) -> Result<LoadedExclusions> {
    load_at(&path(cwd)?)
}

pub fn load_at(config_path: &Path) -> Result<LoadedExclusions> {
    let rules = if config_path.exists() {
        let metadata = fs::symlink_metadata(config_path)
            .with_context(|| format!("inspect exclusion config {}", config_path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "INVALID_INPUT: exclusion config must be a regular file: {}",
                config_path.display()
            );
        }
        if metadata.len() > MAX_CONFIG_BYTES {
            bail!(
                "RESOURCE_LIMIT: exclusion config exceeds {MAX_CONFIG_BYTES} bytes: {}",
                config_path.display()
            );
        }
        let bytes = fs::read(config_path)
            .with_context(|| format!("read exclusion config {}", config_path.display()))?;
        serde_json::from_slice::<ValidationExclusions>(&bytes).with_context(|| {
            format!(
                "INVALID_INPUT: parse exclusion config {}",
                config_path.display()
            )
        })?
    } else {
        ValidationExclusions::default()
    };
    let rules = normalize(rules)?;
    let encoded = serde_json::to_vec(&rules)?;
    Ok(LoadedExclusions {
        rules,
        digest: format!("sha256:{:x}", Sha256::digest(encoded)),
    })
}

/// Replace the complete operator-owned ruleset. There is intentionally no MCP
/// mutation surface for this trust policy.
pub fn replace(cwd: &Path, paths: Vec<String>) -> Result<LoadedExclusions> {
    let config_path = path(cwd)?;
    let rules = normalize(ValidationExclusions {
        version: CONFIG_VERSION,
        paths,
    })?;
    save_at(&config_path, &rules)?;
    load_at(&config_path)
}

fn normalize(mut rules: ValidationExclusions) -> Result<ValidationExclusions> {
    if rules.version != CONFIG_VERSION {
        bail!(
            "INVALID_INPUT: unsupported exclusion config version {}; expected {CONFIG_VERSION}",
            rules.version
        );
    }
    if rules.paths.len() > MAX_PATHS {
        bail!("RESOURCE_LIMIT: exclusion config exceeds {MAX_PATHS} paths");
    }
    let mut normalized = BTreeSet::new();
    for raw in rules.paths {
        let value = raw.trim();
        if value.is_empty() || value.contains('\\') {
            bail!(
                "INVALID_INPUT: exclusion paths must be non-empty repository-relative paths using '/' separators"
            );
        }
        let directory = value.ends_with('/');
        let without_suffix = value.trim_end_matches('/');
        let path = Path::new(without_suffix);
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::CurDir
                        | Component::ParentDir
                        | Component::RootDir
                        | Component::Prefix(_)
                )
            })
            || without_suffix.split('/').any(str::is_empty)
            || without_suffix == ".git"
            || without_suffix.starts_with(".git/")
        {
            bail!(
                "INVALID_INPUT: exclusion path must stay inside the worktree and may not target .git: {value}"
            );
        }
        let canonical = if directory {
            format!("{without_suffix}/")
        } else {
            without_suffix.to_string()
        };
        normalized.insert(canonical);
    }
    rules.paths = normalized.into_iter().collect();
    Ok(rules)
}

fn save_at(config_path: &Path, rules: &ValidationExclusions) -> Result<()> {
    let parent = config_path.parent().context("exclusion config parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create exclusion config directory {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    if config_path.exists() {
        let metadata = fs::symlink_metadata(config_path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "INVALID_INPUT: exclusion config must be a regular file: {}",
                config_path.display()
            );
        }
    }
    let mut encoded = serde_json::to_vec_pretty(rules)?;
    encoded.push(b'\n');
    let temp_path = parent.join(format!(".exclusions-{}.tmp", Uuid::new_v4().simple()));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_are_normalized_digest_bound_and_prefix_aware() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(temp.path())
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        let loaded = replace(
            temp.path(),
            vec!["coverage/".into(), "coverage/".into(), "junit.xml".into()],
        )
        .unwrap();
        assert_eq!(loaded.rules.paths, vec!["coverage/", "junit.xml"]);
        assert!(loaded.excludes("coverage/report.xml"));
        assert!(loaded.excludes("junit.xml"));
        assert!(!loaded.excludes("src/junit.xml"));
        assert_eq!(loaded.digest, load(temp.path()).unwrap().digest);
        assert!(replace(temp.path(), vec!["../secret".into()]).is_err());
        assert!(replace(temp.path(), vec!["./coverage.log".into()]).is_err());
        assert!(replace(temp.path(), vec!["coverage//report.xml".into()]).is_err());
        assert!(replace(temp.path(), vec![".git/config".into()]).is_err());
    }
}
