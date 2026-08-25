use crate::exclusions;
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

static DECLARATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)\b(?:fn|struct|enum|trait|class|interface|type|def|function|module)\s+([A-Za-z_][A-Za-z0-9_]*)",
    )
    .expect("valid declaration regex")
});

const MAX_HASH_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoContext {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSnapshot {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub tree: Option<String>,
    pub dirty: bool,
    pub changed_files: Vec<String>,
    pub excluded_paths: Vec<String>,
    pub exclusion_ruleset_digest: String,
    pub diff_hash: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolInference {
    pub symbols: Vec<String>,
    pub truncated: bool,
    pub captured_bytes: usize,
    pub total_bytes: u64,
}

pub fn available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

pub fn discover(start: impl AsRef<Path>) -> Result<RepoContext> {
    let start = start.as_ref();
    let root = git_output(start, &["rev-parse", "--show-toplevel"])
        .context("not inside a Git worktree")?;
    let common = git_output(
        start,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .context("resolve Git common directory")?;
    let branch = git_output(start, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    let head = git_output(start, &["rev-parse", "--verify", "HEAD"]).ok();
    Ok(RepoContext {
        root: PathBuf::from(root),
        common_dir: PathBuf::from(common),
        branch,
        head,
    })
}

pub fn resolve_database_path(start: impl AsRef<Path>, override_path: Option<&Path>) -> PathBuf {
    if let Some(path) = override_path {
        return absolutize(start.as_ref(), path);
    }
    match discover(start.as_ref()) {
        Ok(repo) => repo.common_dir.join("foremerge").join("state.sqlite3"),
        Err(_) => start.as_ref().join(".foremerge").join("state.sqlite3"),
    }
}

pub fn runtime_dir(start: impl AsRef<Path>) -> PathBuf {
    discover(start.as_ref())
        .map(|repo| repo.common_dir.join("foremerge"))
        .unwrap_or_else(|_| start.as_ref().join(".foremerge"))
}

fn absolutize(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

pub fn snapshot(worktree: impl AsRef<Path>) -> Result<GitSnapshot> {
    let worktree = worktree.as_ref();
    let repo = discover(worktree)?;
    let exclusions = exclusions::load_at(&exclusions::config_path(&repo.common_dir))?;
    let status = git_output_bytes(
        worktree,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let fields = status
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    let mut changed_files = BTreeSet::new();
    let mut untracked_files = BTreeSet::new();
    let mut excluded_paths = BTreeSet::new();
    let mut index = 0;
    while index < fields.len() {
        let entry = fields[index];
        index += 1;
        if entry.len() < 3 {
            continue;
        }
        let status_code = &entry[..2];
        let path = String::from_utf8_lossy(&entry[3..]).into_owned();
        if !path.is_empty() && status_code == b"??" && exclusions.excludes(&path) {
            excluded_paths.insert(path);
        } else if !path.is_empty() {
            changed_files.insert(path.clone());
            if status_code == b"??" {
                untracked_files.insert(path);
            }
        }
        if (status_code.contains(&b'R') || status_code.contains(&b'C')) && index < fields.len() {
            let original = String::from_utf8_lossy(fields[index]).into_owned();
            index += 1;
            if !original.is_empty() {
                changed_files.insert(original);
            }
        }
    }

    let mut digest = Sha256::new();
    let mut hashed_bytes = if repo.head.is_some() {
        hash_git_output(
            worktree,
            &["diff", "--no-ext-diff", "--no-textconv", "--binary", "HEAD"],
            &mut digest,
            "reduce or split the ChangeSet",
        )?
    } else {
        0
    };
    for file in &untracked_files {
        let path = worktree.join(file);
        digest.update(file.as_bytes());
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("inspect untracked path {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            digest.update(b"symlink\0");
            let target = std::fs::read_link(&path)
                .with_context(|| format!("read untracked symlink {}", path.display()))?;
            hashed_bytes = hashed_bytes.saturating_add(target.as_os_str().len() as u64);
            if hashed_bytes > MAX_HASH_BYTES {
                bail!(
                    "RESOURCE_LIMIT: snapshot content exceeds {MAX_HASH_BYTES} bytes; ignore generated files before publishing"
                );
            }
            digest.update(target.to_string_lossy().as_bytes());
        } else if metadata.is_file() {
            if hashed_bytes.saturating_add(metadata.len()) > MAX_HASH_BYTES {
                bail!(
                    "RESOURCE_LIMIT: snapshot content exceeds {MAX_HASH_BYTES} bytes; ignore generated files before publishing"
                );
            }
            digest.update(b"file\0");
            let mut input = open_untracked_file(&path)?;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = input
                    .read(&mut buffer)
                    .with_context(|| format!("read untracked file {}", path.display()))?;
                if read == 0 {
                    break;
                }
                hashed_bytes = hashed_bytes.saturating_add(read as u64);
                if hashed_bytes > MAX_HASH_BYTES {
                    bail!(
                        "RESOURCE_LIMIT: snapshot content exceeds {MAX_HASH_BYTES} bytes; ignore generated files before publishing"
                    );
                }
                digest.update(&buffer[..read]);
            }
        } else {
            digest.update(b"other\0");
        }
    }
    let diff_hash = format!("{:x}", digest.finalize());
    let tree = git_output(worktree, &["rev-parse", "HEAD^{tree}"]).ok();
    let fingerprint_material = format!(
        "{}|{}|{}|{}|{}|{}|{}",
        repo.common_dir.display(),
        repo.root.display(),
        repo.head.as_deref().unwrap_or("UNBORN"),
        tree.as_deref().unwrap_or("NO_TREE"),
        diff_hash,
        changed_files.iter().cloned().collect::<Vec<_>>().join("\0"),
        exclusions.digest,
    );
    let fingerprint = format!(
        "sha256:{:x}",
        Sha256::digest(fingerprint_material.as_bytes())
    );
    Ok(GitSnapshot {
        root: repo.root,
        common_dir: repo.common_dir,
        branch: repo.branch,
        head: repo.head,
        tree,
        dirty: !changed_files.is_empty(),
        changed_files: changed_files.into_iter().collect(),
        excluded_paths: excluded_paths.into_iter().collect(),
        exclusion_ruleset_digest: exclusions.digest,
        diff_hash,
        fingerprint,
    })
}

pub fn diff_files(worktree: impl AsRef<Path>) -> Result<Vec<String>> {
    Ok(snapshot(worktree)?.changed_files)
}

/// Whether the repository history is shallow. Commits at a shallow clone's
/// boundary are reported without parents even though the real history has
/// them, so a missing first parent in a shallow repository must not be
/// treated as a root commit.
pub fn is_shallow(worktree: impl AsRef<Path>) -> Result<bool> {
    Ok(
        git_output(worktree.as_ref(), &["rev-parse", "--is-shallow-repository"])
            .context("determine whether the repository is shallow")?
            == "true",
    )
}

/// The first parent of a commit, or `None` for a root commit. In a shallow
/// repository a `None` is ambiguous; check [`is_shallow`] before treating it
/// as a true root.
pub fn first_parent(worktree: impl AsRef<Path>, commit: &str) -> Result<Option<String>> {
    let line = git_output(
        worktree.as_ref(),
        &["rev-list", "--parents", "--max-count=1", commit],
    )
    .with_context(|| format!("INVALID_INPUT: resolve parents of commit '{commit}'"))?;
    Ok(line.split_whitespace().nth(1).map(str::to_string))
}

/// The empty tree object id in this repository's object format, used as the
/// diff base for a root commit.
pub fn empty_tree_id(worktree: impl AsRef<Path>) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree.as_ref())
        .args(["hash-object", "-t", "tree", "--stdin"])
        .stdin(Stdio::null())
        .output()
        .context("run git hash-object")?;
    if !output.status.success() {
        bail!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// SHA-256 over the binary patch bytes of `git diff <base> <commit>`, bounded
/// by the same content budget as snapshot hashing.
pub fn diff_patch_hash(worktree: impl AsRef<Path>, base: &str, commit: &str) -> Result<String> {
    let mut digest = Sha256::new();
    hash_git_output(
        worktree.as_ref(),
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            base,
            commit,
        ],
        &mut digest,
        "if the candidate is a merge commit, its default first-parent diff spans the entire merged-in branch, so pass --base-ref (HTTP/MCP: base_ref) with the true fork point; otherwise reduce or split the ChangeSet",
    )?;
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(unix)]
fn open_untracked_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| {
            format!(
                "open untracked file without following links {}",
                path.display()
            )
        })?;
    if !file.metadata()?.is_file() {
        bail!("untracked path is not a regular file: {}", path.display());
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_untracked_file(path: &Path) -> Result<File> {
    let file =
        File::open(path).with_context(|| format!("open untracked file {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("untracked path is not a regular file: {}", path.display());
    }
    Ok(file)
}

pub fn infer_symbols(worktree: impl AsRef<Path>) -> Result<Vec<String>> {
    Ok(infer_symbols_report(worktree)?.symbols)
}

pub fn infer_symbols_report(worktree: impl AsRef<Path>) -> Result<SymbolInference> {
    let worktree = worktree.as_ref();
    if discover(worktree)?.head.is_none() {
        return Ok(SymbolInference {
            symbols: Vec::new(),
            truncated: false,
            captured_bytes: 0,
            total_bytes: 0,
        });
    }
    let diff = git_output_bytes_limited(
        worktree,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--unified=0",
            "HEAD",
        ],
    )?;
    let diff_text = String::from_utf8_lossy(&diff.bytes);
    let mut symbols = BTreeSet::new();
    for line in diff_text.lines() {
        if !(line.starts_with('+') || line.starts_with('-'))
            || line.starts_with("+++")
            || line.starts_with("---")
        {
            continue;
        }
        for captures in DECLARATION_RE.captures_iter(line) {
            if let Some(name) = captures.get(1) {
                symbols.insert(name.as_str().to_string());
            }
        }
    }
    Ok(SymbolInference {
        symbols: symbols.into_iter().collect(),
        truncated: diff.truncated,
        captured_bytes: diff.bytes.len(),
        total_bytes: diff.total_bytes,
    })
}

pub fn verify_ref(worktree: impl AsRef<Path>, git_ref: &str) -> Result<String> {
    git_output(
        worktree.as_ref(),
        &["rev-parse", "--verify", &format!("{git_ref}^{{commit}}")],
    )
    .with_context(|| format!("INVALID_INPUT: Git ref '{git_ref}' does not resolve to a commit"))
}

pub fn ensure_clean(worktree: impl AsRef<Path>) -> Result<()> {
    let snapshot = snapshot(worktree)?;
    if snapshot.dirty {
        bail!(
            "CHECK_FAILED: worktree {} is dirty; commit or discard changes before acceptance: {}",
            snapshot.root.display(),
            snapshot.changed_files.join(", ")
        );
    }
    // Excluded generated files are deliberately kept out of the fingerprint, so
    // they do not make the worktree dirty. They must still be gone before
    // acceptance: the validated tree contained them and the accepted commit
    // does not, so leaving them lets a candidate be accepted whose validation
    // depended on content that is not in the commit. Deleting them cannot
    // invalidate the ChangeSet, because their presence and content were
    // excluded from the fingerprint in the first place.
    if !snapshot.excluded_paths.is_empty() {
        bail!(
            "CHECK_FAILED: worktree {} still holds excluded generated files; remove them before acceptance (this does not change the fingerprint): {}",
            snapshot.root.display(),
            snapshot.excluded_paths.join(", ")
        );
    }
    Ok(())
}

pub fn create_accepted_ref(
    worktree: impl AsRef<Path>,
    changeset_id: &str,
    commit: &str,
) -> Result<String> {
    let ref_name = format!("refs/foremerge/accepted/{changeset_id}");
    let resolved = verify_ref(&worktree, commit)?;
    match git_output(worktree.as_ref(), &["update-ref", &ref_name, &resolved, ""]) {
        Ok(_) => {}
        Err(error) => {
            let existing = verify_ref(&worktree, &ref_name).ok();
            if existing.as_deref() != Some(resolved.as_str()) {
                return Err(error).with_context(|| format!("create accepted ref {ref_name}"));
            }
        }
    }
    Ok(ref_name)
}

pub fn is_ancestor(worktree: impl AsRef<Path>, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree.as_ref())
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .context("run git merge-base --is-ancestor")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

pub fn common_dir_is_shared(first: impl AsRef<Path>, second: impl AsRef<Path>) -> Result<bool> {
    Ok(discover(first)?.common_dir == discover(second)?.common_dir)
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!("git {} failed: {stderr}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_output_bytes(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    const MAX_STATUS_BYTES: usize = 16 * 1024 * 1024;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("capture git stdout"))?;
    let mut captured = Vec::new();
    let mut exceeded = false;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = stdout
            .read(&mut buffer)
            .with_context(|| format!("read git {} output", args.join(" ")))?;
        if read == 0 {
            break;
        }
        let remaining = MAX_STATUS_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
        exceeded |= read > remaining;
    }
    let status = child
        .wait()
        .with_context(|| format!("wait for git {}", args.join(" ")))?;
    if !status.success() {
        bail!("git {} failed with status {status}", args.join(" "));
    }
    if exceeded {
        bail!(
            "RESOURCE_LIMIT: git status exceeded {MAX_STATUS_BYTES} bytes; clean or ignore generated files before publishing"
        );
    }
    Ok(captured)
}

fn hash_git_output(
    cwd: &Path,
    args: &[&str],
    digest: &mut Sha256,
    over_budget_hint: &str,
) -> Result<u64> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("capture git stdout"))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut hashed_bytes = 0_u64;
    loop {
        let read = stdout
            .read(&mut buffer)
            .with_context(|| format!("read git {} output", args.join(" ")))?;
        if read == 0 {
            break;
        }
        hashed_bytes = hashed_bytes.saturating_add(read as u64);
        if hashed_bytes > MAX_HASH_BYTES {
            let _ = child.kill();
            let _ = child.wait();
            bail!("RESOURCE_LIMIT: git diff exceeds {MAX_HASH_BYTES} bytes; {over_budget_hint}");
        }
        digest.update(&buffer[..read]);
    }
    let status = child
        .wait()
        .with_context(|| format!("wait for git {}", args.join(" ")))?;
    if status.success() {
        Ok(hashed_bytes)
    } else {
        bail!("git {} failed with status {status}", args.join(" "))
    }
}

struct LimitedOutput {
    bytes: Vec<u8>,
    truncated: bool,
    total_bytes: u64,
}

fn git_output_bytes_limited(cwd: &Path, args: &[&str]) -> Result<LimitedOutput> {
    const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("capture git stdout"))?;
    let mut captured = Vec::new();
    let mut total_bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = stdout
            .read(&mut buffer)
            .with_context(|| format!("read git {} output", args.join(" ")))?;
        if read == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(read as u64);
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    let status = child
        .wait()
        .with_context(|| format!("wait for git {}", args.join(" ")))?;
    if status.success() {
        Ok(LimitedOutput {
            truncated: total_bytes > captured.len() as u64,
            total_bytes,
            bytes: captured,
        })
    } else {
        bail!("git {} failed with status {status}", args.join(" "));
    }
}

/// Whether `needle` appears anywhere in the repository's tracked files.
///
/// Used only for the opt-in symbol-existence advisory. `git grep` is used
/// because it already respects tracked files and ignore rules, and because Git
/// is a dependency this project already has. A failure to run is reported as
/// "found", so a broken search can never manufacture a warning.
pub fn tracked_content_contains(worktree: &Path, needle: &str) -> bool {
    if needle.trim().is_empty() {
        return true;
    }
    Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["grep", "--ignore-case", "--fixed-strings", "--quiet", "-e"])
        .arg(needle)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn snapshot_includes_untracked_content() {
        let temp = TempDir::new().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "Test"]);
        fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
        git(temp.path(), &["add", "tracked.txt"]);
        git(temp.path(), &["commit", "-qm", "initial"]);
        fs::write(temp.path().join("new.txt"), "first\n").unwrap();
        let first = snapshot(temp.path()).unwrap();
        fs::write(temp.path().join("new.txt"), "second\n").unwrap();
        let second = snapshot(temp.path()).unwrap();
        assert!(first.dirty);
        assert!(first.changed_files.contains(&"new.txt".to_string()));
        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn snapshot_parses_rename_records_without_phantom_paths() {
        let temp = TempDir::new().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        git(temp.path(), &["config", "user.name", "Test"]);
        fs::write(temp.path().join("abcdef.txt"), "one\n").unwrap();
        git(temp.path(), &["add", "abcdef.txt"]);
        git(temp.path(), &["commit", "-qm", "initial"]);
        git(temp.path(), &["mv", "abcdef.txt", "renamed.txt"]);

        let snapshot = snapshot(temp.path()).unwrap();
        assert!(snapshot.changed_files.contains(&"abcdef.txt".to_string()));
        assert!(snapshot.changed_files.contains(&"renamed.txt".to_string()));
        assert!(!snapshot.changed_files.contains(&"def.txt".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_hashes_untracked_symlink_text_without_following_target() {
        use std::os::unix::fs::symlink;

        let repo = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test"]);
        fs::write(repo.path().join("tracked.txt"), "one\n").unwrap();
        git(repo.path(), &["add", "tracked.txt"]);
        git(repo.path(), &["commit", "-qm", "initial"]);
        let target = external.path().join("secret.txt");
        fs::write(&target, "first secret\n").unwrap();
        symlink(&target, repo.path().join("link.txt")).unwrap();

        let first = snapshot(repo.path()).unwrap();
        fs::write(&target, "different secret\n").unwrap();
        let second = snapshot(repo.path()).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
        assert!(first.changed_files.contains(&"link.txt".to_string()));
    }

    #[test]
    fn snapshot_rejects_untracked_content_over_the_hash_budget() {
        let repo = TempDir::new().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test"]);
        git(repo.path(), &["commit", "--allow-empty", "-qm", "initial"]);
        File::create(repo.path().join("generated.bin"))
            .unwrap()
            .set_len(MAX_HASH_BYTES + 1)
            .unwrap();

        let error = snapshot(repo.path()).unwrap_err();
        assert!(format!("{error:#}").starts_with("RESOURCE_LIMIT:"));
    }

    #[test]
    fn validation_exclusions_apply_only_to_untracked_paths_and_bind_the_digest() {
        let repo = TempDir::new().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test"]);
        fs::write(repo.path().join("tracked.txt"), "one\n").unwrap();
        git(repo.path(), &["add", "tracked.txt"]);
        git(repo.path(), &["commit", "-qm", "initial"]);

        crate::exclusions::replace(repo.path(), vec!["generated/".into()]).unwrap();
        let clean_with_rules = snapshot(repo.path()).unwrap();
        fs::create_dir(repo.path().join("generated")).unwrap();
        fs::write(repo.path().join("generated/coverage.log"), "first\n").unwrap();
        let excluded = snapshot(repo.path()).unwrap();
        assert_eq!(clean_with_rules.fingerprint, excluded.fingerprint);
        assert!(!excluded.dirty);
        assert_eq!(excluded.excluded_paths, vec!["generated/coverage.log"]);
        fs::write(repo.path().join("generated/coverage.log"), "second\n").unwrap();
        assert_eq!(
            excluded.fingerprint,
            snapshot(repo.path()).unwrap().fingerprint
        );

        fs::write(repo.path().join("tracked.txt"), "changed\n").unwrap();
        let tracked = snapshot(repo.path()).unwrap();
        assert!(tracked.dirty);
        assert!(tracked.changed_files.contains(&"tracked.txt".to_string()));
        assert_ne!(tracked.fingerprint, excluded.fingerprint);

        fs::write(repo.path().join("tracked.txt"), "one\n").unwrap();
        crate::exclusions::replace(repo.path(), vec!["other-generated/".into()]).unwrap();
        fs::remove_dir_all(repo.path().join("generated")).unwrap();
        let different_rules = snapshot(repo.path()).unwrap();
        assert_ne!(clean_with_rules.fingerprint, different_rules.fingerprint);
    }

    #[test]
    fn symbol_inference_reports_truncation() {
        let repo = TempDir::new().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test"]);
        git(repo.path(), &["commit", "--allow-empty", "-qm", "initial"]);
        let large = format!("fn CapturedSymbol() {{}}\n{}", "x".repeat(5 * 1024 * 1024));
        fs::write(repo.path().join("large.rs"), large).unwrap();
        git(repo.path(), &["add", "large.rs"]);

        let report = infer_symbols_report(repo.path()).unwrap();
        assert!(report.truncated);
        assert!(report.total_bytes > report.captured_bytes as u64);
        assert!(report.symbols.contains(&"CapturedSymbol".to_string()));
    }
}
