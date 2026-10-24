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

/// Whether anything at or above `start` could make it a repository: a `.git`
/// directory or gitfile, or `GIT_DIR` in the environment.
///
/// This tells "not a repository" apart from "a repository Git refused to
/// open" (dubious ownership, a broken gitfile, unreadable metadata) without
/// parsing Git's messages, which follow the user's locale. A `false` here
/// means no answer exists; a `true` alongside a failed [`discover`] means Git
/// had one and refused it, and its own error says why.
pub fn repository_marker_exists(start: impl AsRef<Path>) -> bool {
    std::env::var_os("GIT_DIR").is_some()
        || start
            .as_ref()
            .ancestors()
            .any(|directory| std::fs::symlink_metadata(directory.join(".git")).is_ok())
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

/// How a commit that is not a descendant of an accepted candidate was shown
/// to hold every change the candidate made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentContainment {
    /// The merge bases of the candidate and the landed commit. The candidate's
    /// changes are measured against each one.
    pub merge_bases: Vec<String>,
    /// How many distinct paths the candidate changed, all of which were found
    /// holding the candidate's change.
    pub paths_checked: usize,
    /// Paths where the landed commit holds the candidate's change and other
    /// changes besides, such as edits that landed on the target branch first.
    pub paths_with_other_changes: Vec<String>,
}

/// Why content containment could not be shown. Each variant is a refusal, not
/// a Git failure: the commit may still be the right one, but nothing here
/// proves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainmentRefusal {
    /// The landed commit is already in the candidate's own history.
    Predates,
    /// The two commits have no common history.
    NoMergeBase { shallow: bool },
    /// The candidate changes nothing relative to this merge base, so every
    /// commit would trivially "contain" it.
    NoChange { merge_base: String },
    /// These paths do not hold the candidate's change, each with the reason.
    Paths {
        merge_base: String,
        changed: usize,
        missing: Vec<(String, &'static str)>,
    },
}

impl ContainmentRefusal {
    /// What the caller can do next, in one sentence.
    pub fn remedy(&self) -> &'static str {
        match self {
            Self::Predates => {
                "Record the commit the work landed in, which comes after the accepted candidate."
            }
            Self::NoMergeBase { .. } => {
                "Record the commit in this repository's history that the work landed in."
            }
            Self::NoChange { .. } => {
                "An empty candidate can be recorded only through ancestry: record a commit that has the accepted commit in its history."
            }
            Self::Paths { .. } => {
                "Record the commit the work actually landed in, or, if what landed differs from what was accepted, publish and validate a revision for it."
            }
        }
    }
}

impl std::fmt::Display for ContainmentRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const SHOWN: usize = 10;
        match self {
            Self::Predates => write!(
                formatter,
                "it predates the accepted candidate (it is already in the candidate's own history)"
            ),
            Self::NoMergeBase { shallow } => {
                write!(
                    formatter,
                    "it shares no history with the accepted candidate"
                )?;
                if *shallow {
                    write!(
                        formatter,
                        "; this repository is shallow, so fetch more history and retry"
                    )?;
                }
                Ok(())
            }
            Self::NoChange { merge_base } => write!(
                formatter,
                "the accepted candidate makes no change relative to merge base {merge_base}, so content cannot show that it landed"
            ),
            Self::Paths {
                merge_base,
                changed,
                missing,
            } => {
                write!(
                    formatter,
                    "{} of the {changed} path(s) the candidate changed since merge base {merge_base} do not hold its change: ",
                    missing.len()
                )?;
                let shown = missing
                    .iter()
                    .take(SHOWN)
                    .map(|(path, reason)| format!("{path} ({reason})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(formatter, "{shown}")?;
                if missing.len() > SHOWN {
                    write!(formatter, ", and {} more", missing.len() - SHOWN)?;
                }
                Ok(())
            }
        }
    }
}

/// One side of a `git diff --raw` entry: a mode and an object id, or `None`
/// when the path is absent on that side.
type RawSide = Option<(String, String)>;

struct RawChange {
    /// The path exactly as Git stores it. Paths are bytes, not text, so two
    /// distinct non-UTF-8 names must never collapse into one key.
    path: Vec<u8>,
    old: RawSide,
    new: RawSide,
}

const ZERO_MODE: &str = "000000";
const MAX_RAW_DIFF_BYTES: usize = 64 * 1024 * 1024;
const MAX_COMPARED_BLOB_BYTES: usize = 64 * 1024 * 1024;

/// Whether `landed` holds every change `candidate` made since their common
/// history, judged by content rather than ancestry.
///
/// This is for a landing that rewrote the candidate's commits: a squash merge
/// or a rebase merge. For each merge base of the two commits, every path the
/// candidate changed relative to that base must, at `landed`:
///
/// - be identical to the candidate's version, or
/// - for a regular file, already contain the candidate's change, meaning a
///   line-level three-way merge of the candidate's version into the landed
///   version (with the merge base's version as the common ancestor) is clean
///   and changes nothing. Other edits to the same file, such as entries that
///   landed on the target branch first, are allowed.
///
/// A deletion must be a deletion at `landed`, and a mode the candidate set
/// must be the mode at `landed`. Symbolic links and submodules count only
/// when they match exactly. A commit already in the candidate's history, and
/// a candidate that changes nothing, are refused outright, since content
/// would prove nothing about either.
///
/// The merge is Git's own `merge-file`, which reads no attributes, so a
/// repository's custom merge drivers cannot make a missing change look
/// present. Paths are compared without rename detection: a rename is a
/// deletion plus an addition, and both must have landed.
///
/// `Ok(Err(_))` is a refusal with its reason; `Err(_)` means Git itself failed.
pub fn content_containment(
    worktree: impl AsRef<Path>,
    candidate: &str,
    landed: &str,
) -> Result<std::result::Result<ContentContainment, ContainmentRefusal>> {
    let worktree = worktree.as_ref();
    if is_ancestor(worktree, landed, candidate)? {
        return Ok(Err(ContainmentRefusal::Predates));
    }
    let merge_bases = merge_bases(worktree, candidate, landed)?;
    if merge_bases.is_empty() {
        return Ok(Err(ContainmentRefusal::NoMergeBase {
            shallow: is_shallow(worktree)?,
        }));
    }
    let landed_differences: std::collections::BTreeMap<Vec<u8>, RawChange> =
        raw_diff(worktree, candidate, landed)?
            .into_iter()
            .map(|change| (change.path.clone(), change))
            .collect();
    let mut checked = BTreeSet::new();
    let mut with_other_changes = BTreeSet::new();
    let mut scratch = None;
    for merge_base in &merge_bases {
        let changes = raw_diff(worktree, merge_base, candidate)?;
        if changes.is_empty() {
            return Ok(Err(ContainmentRefusal::NoChange {
                merge_base: merge_base.clone(),
            }));
        }
        let mut missing = Vec::new();
        for change in &changes {
            checked.insert(change.path.clone());
            // Absent from the candidate-to-landed diff: identical at both.
            let Some(difference) = landed_differences.get(&change.path) else {
                continue;
            };
            // `difference.old` is the candidate's side, `.new` the landed one.
            let verdict = match (&change.new, &difference.new) {
                (None, None) => Ok(()),
                (None, Some(_)) => Err("still present; the candidate deleted it"),
                (Some(_), None) => Err("missing"),
                (Some((candidate_mode, candidate_blob)), Some((landed_mode, landed_blob))) => {
                    let mode_set_by_candidate = change
                        .old
                        .as_ref()
                        .is_none_or(|(base_mode, _)| base_mode != candidate_mode);
                    if landed_mode != candidate_mode && mode_set_by_candidate {
                        Err("different file mode")
                    } else if landed_blob == candidate_blob {
                        Ok(())
                    } else if !is_regular_file(candidate_mode) || !is_regular_file(landed_mode) {
                        Err("different, and not a regular file")
                    } else {
                        let base_blob = change
                            .old
                            .as_ref()
                            .filter(|(mode, _)| is_regular_file(mode))
                            .map(|(_, blob)| blob.as_str());
                        if scratch.is_none() {
                            scratch = Some(ScratchDir::create()?);
                        }
                        let directory = scratch.as_ref().expect("scratch directory");
                        line_merge_contains(
                            worktree,
                            &directory.path,
                            base_blob,
                            candidate_blob,
                            landed_blob,
                        )?
                    }
                }
            };
            match verdict {
                Ok(()) => {
                    if difference.new.is_some() {
                        with_other_changes.insert(change.path.clone());
                    }
                }
                Err(reason) => missing.push((display_path(&change.path), reason)),
            }
        }
        if !missing.is_empty() {
            return Ok(Err(ContainmentRefusal::Paths {
                merge_base: merge_base.clone(),
                changed: changes.len(),
                missing,
            }));
        }
    }
    Ok(Ok(ContentContainment {
        merge_bases,
        paths_checked: checked.len(),
        paths_with_other_changes: with_other_changes
            .iter()
            .map(|path| display_path(path))
            .collect(),
    }))
}

/// A Git path for messages and evidence. Non-UTF-8 bytes are shown as
/// U+FFFD; comparison always uses the exact bytes.
fn display_path(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn is_regular_file(mode: &str) -> bool {
    matches!(mode, "100644" | "100755")
}

/// Whether the landed version of a regular file already holds the
/// candidate's change: the three-way line merge of the candidate's version
/// into it is clean and leaves it byte-for-byte unchanged.
fn line_merge_contains(
    worktree: &Path,
    scratch: &Path,
    base_blob: Option<&str>,
    candidate_blob: &str,
    landed_blob: &str,
) -> Result<std::result::Result<(), &'static str>> {
    let Some(landed) = blob_bytes(worktree, landed_blob)? else {
        return Ok(Err("too large to compare line by line"));
    };
    let Some(candidate) = blob_bytes(worktree, candidate_blob)? else {
        return Ok(Err("too large to compare line by line"));
    };
    let base = match base_blob {
        Some(blob) => match blob_bytes(worktree, blob)? {
            Some(bytes) => bytes,
            None => return Ok(Err("too large to compare line by line")),
        },
        None => Vec::new(),
    };
    let landed_path = scratch.join("landed");
    let base_path = scratch.join("base");
    let candidate_path = scratch.join("candidate");
    std::fs::write(&landed_path, &landed).context("write landed version for comparison")?;
    std::fs::write(&base_path, &base).context("write merge base version for comparison")?;
    std::fs::write(&candidate_path, &candidate)
        .context("write candidate version for comparison")?;
    // `merge-file` exits with the number of conflicts, or a negative status
    // for an error such as a binary input. Only a clean merge can show
    // containment, so every other status is a refusal.
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["merge-file", "-p", "-q"])
        .arg(&landed_path)
        .arg(&base_path)
        .arg(&candidate_path)
        .stdin(Stdio::null())
        .output()
        .context("run git merge-file")?;
    Ok(match output.status.code() {
        Some(0) if output.stdout == landed => Ok(()),
        Some(0) => Err("lacks the candidate's change"),
        Some(1..=127) => Err("conflicts with the candidate's change"),
        _ => Err("different, and could not be compared line by line"),
    })
}

/// A private directory for the three files a line merge compares, removed
/// when dropped.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn create() -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "foremerge-containment-{}",
            uuid::Uuid::new_v4().simple()
        ));
        // The files compared here are repository source, so the directory is
        // private to the owner on a shared machine.
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .with_context(|| format!("create comparison directory {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A blob's bytes, or `None` when it is over the comparison budget.
fn blob_bytes(worktree: &Path, blob: &str) -> Result<Option<Vec<u8>>> {
    match git_output_bounded(
        worktree,
        &["cat-file", "blob", blob],
        MAX_COMPARED_BLOB_BYTES,
    )? {
        Bounded::Complete(bytes) => Ok(Some(bytes)),
        Bounded::Exceeded => Ok(None),
    }
}

/// Every merge base of two commits; empty when they share no history.
fn merge_bases(worktree: &Path, first: &str, second: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["merge-base", "--all", first, second])
        .output()
        .context("run git merge-base --all")?;
    match output.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect()),
        Some(1) if output.stdout.is_empty() => Ok(Vec::new()),
        _ => bail!(
            "git merge-base --all failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// The paths that differ between two commits, each with its mode and object id
/// on both sides. Every option that configuration could otherwise change is
/// pinned: `diff.renames` (`--no-renames`), `diff.relative` and the current
/// directory (`--no-relative`, so paths are always from the repository root and
/// never filtered to a subdirectory), `diff.ignoreSubmodules`
/// (`--ignore-submodules=none`), `core.abbrev` (`--no-abbrev`), and external
/// diff and text conversion drivers. `-z` keeps every byte of a path,
/// newlines included.
fn raw_diff(worktree: &Path, from: &str, to: &str) -> Result<Vec<RawChange>> {
    let bytes = match git_output_bounded(
        worktree,
        &[
            "diff",
            "--raw",
            "-z",
            "--no-renames",
            "--no-abbrev",
            "--no-ext-diff",
            "--no-textconv",
            "--no-relative",
            "--ignore-submodules=none",
            from,
            to,
            "--",
        ],
        MAX_RAW_DIFF_BYTES,
    )? {
        Bounded::Complete(bytes) => bytes,
        Bounded::Exceeded => bail!(
            "RESOURCE_LIMIT: the path list between {from} and {to} exceeds {MAX_RAW_DIFF_BYTES} bytes"
        ),
    };
    let mut fields = bytes.split(|byte| *byte == 0);
    let mut changes = Vec::new();
    while let Some(header) = fields.next() {
        if header.is_empty() {
            continue;
        }
        let header = String::from_utf8_lossy(header);
        let path = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("git diff --raw entry without a path: {header}"))?;
        let path = path.to_vec();
        let parts: Vec<&str> = header.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old_id, new_id, _status] = parts.as_slice() else {
            bail!("unexpected git diff --raw entry: {header}");
        };
        let side = |mode: &str, object: &str| {
            (mode != ZERO_MODE).then(|| (mode.to_string(), object.to_string()))
        };
        changes.push(RawChange {
            path,
            old: side(old_mode, old_id),
            new: side(new_mode, new_id),
        });
    }
    Ok(changes)
}

enum Bounded {
    Complete(Vec<u8>),
    Exceeded,
}

/// A Git command's stdout, stopping at `max_bytes`. A failure carries Git's
/// stderr.
fn git_output_bounded(cwd: &Path, args: &[&str], max_bytes: usize) -> Result<Bounded> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("capture git stdout"))?;
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = stdout
            .read(&mut buffer)
            .with_context(|| format!("read git {} output", args.join(" ")))?;
        if read == 0 {
            break;
        }
        if captured.len() + read > max_bytes {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Bounded::Exceeded);
        }
        captured.extend_from_slice(&buffer[..read]);
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("wait for git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(Bounded::Complete(captured))
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
        .map(|output| match output.status.code() {
            // `git grep` reports a match with 0 and a clean "no match" with 1.
            // Only 1 is evidence of absence. Anything else, including the 128
            // Git uses for its own failures and the None of a signal, means the
            // search did not run, and a search that did not run must not be
            // read as a missing symbol.
            Some(0) => true,
            Some(1) => false,
            _ => true,
        })
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

#[cfg(test)]
mod symbol_search_tests {
    use super::*;

    /// The advisory exists to say a declared symbol appears nowhere. A search
    /// that could not run has not established that. `git grep` exits 128 for
    /// its own failures, which `ExitStatus::success` cannot tell apart from the
    /// 1 it uses for an honest "no match".
    #[test]
    fn a_search_that_could_not_run_is_not_evidence_of_a_missing_symbol() {
        let temp = tempfile::tempdir().expect("temp dir");
        // Not a repository, so `git grep` fails with 128 rather than answering.
        let probe = std::process::Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .args(["grep", "--quiet", "-e", "Anything"])
            .output()
            .expect("run git grep");
        assert_eq!(
            probe.status.code(),
            Some(128),
            "this test is meaningless unless git actually fails here"
        );

        assert!(
            tracked_content_contains(temp.path(), "Anything"),
            "a failed search must report found, so it cannot manufacture a warning"
        );
    }
}

#[cfg(test)]
mod containment_tests {
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
        assert!(status.success(), "git {args:?} failed");
    }

    /// A repository on `trunk` with one commit holding `files`, ready for a
    /// candidate branch and a landing to be built on it.
    fn containment_repo(files: &[(&str, &[u8])]) -> TempDir {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        git(root, &["checkout", "-q", "-b", "trunk"]);
        for (path, content) in files {
            fs::write(root.join(path), content).unwrap();
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);
        temp
    }

    /// Write `files` (a `None` content deletes the path), commit everything on
    /// the current branch, and return the new commit.
    fn commit_files(root: &Path, files: &[(&str, Option<&[u8]>)]) -> String {
        for (path, content) in files {
            match content {
                Some(content) => fs::write(root.join(path), content).unwrap(),
                None => fs::remove_file(root.join(path)).unwrap(),
            }
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "change", "--allow-empty"]);
        git_output(root, &["rev-parse", "HEAD"]).unwrap()
    }

    fn refused_paths(
        result: std::result::Result<ContentContainment, ContainmentRefusal>,
    ) -> Vec<(String, &'static str)> {
        match result {
            Err(ContainmentRefusal::Paths { missing, .. }) => missing,
            other => panic!("expected a refusal naming paths, got {other:?}"),
        }
    }

    #[test]
    fn a_repository_merge_driver_cannot_make_a_missing_change_look_present() {
        // A driver that always keeps "our" side would make any merge-based
        // check report the landed file as already holding the candidate's
        // change. The line merge used here reads no attributes.
        let temp = containment_repo(&[
            ("pinned.txt", b"one\n"),
            (".gitattributes", b"pinned.txt merge=keep-ours\n"),
        ]);
        let root = temp.path();
        git(root, &["config", "merge.keep-ours.driver", "true"]);
        git(root, &["checkout", "-q", "-b", "candidate"]);
        let candidate = commit_files(root, &[("pinned.txt", Some(b"two\n"))]);
        git(root, &["checkout", "-q", "trunk"]);
        let landed = commit_files(root, &[("other.txt", Some(b"other\n"))]);
        let missing = refused_paths(content_containment(root, &candidate, &landed).unwrap());
        assert_eq!(
            missing,
            vec![("pinned.txt".to_string(), "lacks the candidate's change")]
        );
    }

    #[test]
    fn a_squash_combined_with_other_work_holds_the_candidate() {
        let temp = containment_repo(&[("shared.txt", b"a\nb\nc\nd\ne\nf\ng\nh\n")]);
        let root = temp.path();
        git(root, &["checkout", "-q", "-b", "candidate"]);
        let candidate = commit_files(root, &[("shared.txt", Some(b"A\nb\nc\nd\ne\nf\ng\nh\n"))]);
        git(root, &["checkout", "-q", "trunk"]);
        let landed = commit_files(
            root,
            &[
                ("shared.txt", Some(b"A\nb\nc\nd\ne\nf\ng\nH\n")),
                ("extra.txt", Some(b"another candidate\n")),
            ],
        );
        let contained = content_containment(root, &candidate, &landed)
            .unwrap()
            .unwrap();
        assert_eq!(contained.paths_checked, 1);
        assert_eq!(
            contained.paths_with_other_changes,
            vec!["shared.txt".to_string()]
        );
    }

    #[test]
    fn deletions_renames_and_modes_must_land_exactly() {
        let temp = containment_repo(&[("old.txt", b"moved\n"), ("tool.sh", b"echo\n")]);
        let root = temp.path();
        // Modes are set in the index only, so the worktree's must not count.
        git(root, &["config", "core.fileMode", "false"]);
        git(root, &["checkout", "-q", "-b", "candidate"]);
        fs::rename(root.join("old.txt"), root.join("new.txt")).unwrap();
        git(root, &["add", "-A"]);
        git(root, &["update-index", "--chmod=+x", "tool.sh"]);
        git(root, &["commit", "-qm", "rename and make executable"]);
        let candidate = git_output(root, &["rev-parse", "HEAD"]).unwrap();

        // The landing copied the file instead of moving it and kept the mode.
        git(root, &["checkout", "-q", "trunk"]);
        let partial = commit_files(root, &[("new.txt", Some(b"moved\n"))]);
        let mut missing = refused_paths(content_containment(root, &candidate, &partial).unwrap());
        missing.sort();
        assert_eq!(
            missing,
            vec![
                (
                    "old.txt".to_string(),
                    "still present; the candidate deleted it"
                ),
                ("tool.sh".to_string(), "different file mode"),
            ]
        );

        // The full landing, rewritten as a squash, holds all three.
        commit_files(root, &[("old.txt", None)]);
        git(root, &["update-index", "--chmod=+x", "tool.sh"]);
        git(root, &["commit", "-qm", "finish the landing"]);
        let complete = git_output(root, &["rev-parse", "HEAD"]).unwrap();
        let contained = content_containment(root, &candidate, &complete)
            .unwrap()
            .unwrap();
        assert_eq!(contained.paths_checked, 3);
        assert!(contained.paths_with_other_changes.is_empty());
    }

    #[test]
    fn a_binary_file_counts_only_when_it_matches_exactly() {
        let temp = containment_repo(&[("image.bin", b"\0base\0")]);
        let root = temp.path();
        git(root, &["checkout", "-q", "-b", "candidate"]);
        let candidate = commit_files(root, &[("image.bin", Some(b"\0candidate\0"))]);
        git(root, &["checkout", "-q", "trunk"]);
        let different = commit_files(root, &[("image.bin", Some(b"\0candidate\0plus\0"))]);
        let missing = refused_paths(content_containment(root, &candidate, &different).unwrap());
        assert_eq!(
            missing,
            vec![(
                "image.bin".to_string(),
                "different, and could not be compared line by line"
            )]
        );
        let exact = commit_files(root, &[("image.bin", Some(b"\0candidate\0"))]);
        assert!(
            content_containment(root, &candidate, &exact)
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn a_landing_in_the_candidates_own_history_or_with_no_shared_history_is_refused() {
        let temp = containment_repo(&[("file.txt", b"one\n")]);
        let root = temp.path();
        let base = git_output(root, &["rev-parse", "HEAD"]).unwrap();
        git(root, &["checkout", "-q", "-b", "candidate"]);
        let candidate = commit_files(root, &[("file.txt", Some(b"two\n"))]);
        assert_eq!(
            content_containment(root, &candidate, &base).unwrap(),
            Err(ContainmentRefusal::Predates)
        );
        git(root, &["checkout", "-q", "--orphan", "unrelated"]);
        let unrelated = commit_files(root, &[("file.txt", Some(b"two\n"))]);
        assert_eq!(
            content_containment(root, &candidate, &unrelated).unwrap(),
            Err(ContainmentRefusal::NoMergeBase { shallow: false })
        );
    }
    fn git_stdin(cwd: &Path, args: &[&str], input: &[u8]) -> String {
        use std::io::Write;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// A tree holding exactly `entries` (mode, object id, path bytes), built
    /// through the index so that no filesystem limit on names applies.
    fn tree_of(root: &Path, entries: &[(&str, String, Vec<u8>)]) -> String {
        git(root, &["read-tree", "--empty"]);
        let mut input = Vec::new();
        for (mode, object, path) in entries {
            input.extend_from_slice(format!("{mode} {object}\t").as_bytes());
            input.extend_from_slice(path);
            input.push(0);
        }
        git_stdin(root, &["update-index", "-z", "--index-info"], &input);
        git_output(root, &["write-tree"]).unwrap()
    }

    #[test]
    fn hostile_paths_and_diff_configuration_cannot_change_the_verdict() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        let blob = |content: &[u8]| git_stdin(root, &["hash-object", "-w", "--stdin"], content);
        let base_entries = vec![
            ("100644", blob(b"echo\n"), b"tool.sh".to_vec()),
            ("100644", blob(b"\0old\xff"), b"image.bin".to_vec()),
            ("100644", blob(b"plain\n"), b"plain.txt".to_vec()),
        ];
        let base_tree = tree_of(root, &base_entries);
        let base = git_output(root, &["commit-tree", &base_tree, "-m", "base"]).unwrap();

        let mut candidate_entries = vec![
            // An executable-bit change with the same content.
            ("100755", base_entries[0].1.clone(), b"tool.sh".to_vec()),
            ("100644", blob(b"\0new\xff"), b"image.bin".to_vec()),
            base_entries[2].clone(),
            ("100644", blob(b"odd path\n"), b"odd\nname".to_vec()),
            ("120000", blob(b"plain.txt"), b"link".to_vec()),
            ("160000", base.clone(), b"submodule".to_vec()),
        ];
        // Two invalid UTF-8 names that read the same once made lossy, so a
        // text key would merge them.
        #[cfg(unix)]
        {
            candidate_entries.push(("100644", blob(b"first\n"), b"invalid\xfe".to_vec()));
            candidate_entries.push(("100644", blob(b"second\n"), b"invalid\xff".to_vec()));
        }
        let candidate_tree = tree_of(root, &candidate_entries);
        let candidate = git_output(
            root,
            &[
                "commit-tree",
                &candidate_tree,
                "-p",
                &base,
                "-m",
                "candidate",
            ],
        )
        .unwrap();
        let rewritten = git_output(
            root,
            &[
                "commit-tree",
                &candidate_tree,
                "-p",
                &base,
                "-m",
                "rewritten",
            ],
        )
        .unwrap();
        assert_ne!(candidate, rewritten);

        // Configuration that would hide renames, submodules or paths outside
        // the current directory, run from a subdirectory.
        git(root, &["config", "diff.renames", "true"]);
        git(root, &["config", "diff.relative", "true"]);
        git(root, &["config", "diff.ignoreSubmodules", "all"]);
        git(root, &["config", "core.abbrev", "4"]);
        fs::create_dir(root.join("nested")).unwrap();
        let nested = root.join("nested");

        let contained = content_containment(&nested, &candidate, &rewritten)
            .unwrap()
            .unwrap();
        assert_eq!(contained.merge_bases, vec![base.clone()]);
        assert_eq!(contained.paths_checked, candidate_entries.len() - 1);
        assert!(contained.paths_with_other_changes.is_empty());

        // Each changed path, dropped on its own, is named as missing.
        for (index, (_, _, path)) in candidate_entries.iter().enumerate() {
            if path.as_slice() == b"plain.txt" {
                continue;
            }
            let mut entries = candidate_entries.clone();
            entries.remove(index);
            let tree = tree_of(root, &entries);
            let variant =
                git_output(root, &["commit-tree", &tree, "-p", &base, "-m", "variant"]).unwrap();
            let missing =
                refused_paths(content_containment(&nested, &candidate, &variant).unwrap());
            assert_eq!(
                missing,
                vec![(String::from_utf8_lossy(path).into_owned(), "missing")],
                "dropping {path:?}"
            );
        }

        // The executable bit and the submodule commit must match too.
        let mut entries = candidate_entries.clone();
        entries[0].0 = "100644";
        entries[5].1 = candidate.clone();
        let tree = tree_of(root, &entries);
        let variant =
            git_output(root, &["commit-tree", &tree, "-p", &base, "-m", "variant"]).unwrap();
        let mut missing =
            refused_paths(content_containment(&nested, &candidate, &variant).unwrap());
        missing.sort();
        assert_eq!(
            missing,
            vec![
                ("submodule".to_string(), "different, and not a regular file"),
                ("tool.sh".to_string(), "different file mode"),
            ]
        );
    }
}
