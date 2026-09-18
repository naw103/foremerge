//! Operator recovery for the coordination ledger.
//!
//! A ledger migrated by a newer build is refused by every older one, and before
//! this module there was no supported way back: the only exit was deleting
//! `state.sqlite3` by hand, with no backup and no record of what was lost.
//! `foremerge ledger reset` sets the ledger aside intact, then starts a fresh
//! one or restores a backup, and refuses while any other process holds the
//! ledger open.

use crate::Store;
use crate::db::{DATABASE_SCHEMA_VERSION, SCHEMA_WRITER_KEY, WRITER_QUERY, readable_writer};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
#[cfg(all(unix, not(target_os = "linux")))]
use std::process::{Command, Stdio};
use std::time::Duration;
#[cfg(all(unix, not(target_os = "linux")))]
use std::time::Instant;

/// SQLite keeps a ledger's committed state across these files. Moving only the
/// main file would leave recent commits behind in the WAL.
const SIDECAR_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Tables whose row counts tell the operator what a ledger held.
const SUMMARY_TABLES: [&str; 6] = [
    "agents",
    "intents",
    "claims",
    "changesets",
    "conflicts",
    "events",
];

#[cfg(all(unix, not(target_os = "linux")))]
const LSOF_TIMEOUT: Duration = Duration::from_secs(20);

/// Another process with the ledger open.
#[derive(Debug, Clone, Serialize)]
pub struct Holder {
    pub pid: u32,
    pub command: Option<String>,
}

/// Whether other processes hold the ledger open, as far as this platform can
/// tell.
#[derive(Debug, Clone)]
pub enum HolderCheck {
    Checked(Vec<Holder>),
    Unavailable(String),
}

/// The ledger's files that exist on disk: the database and its sidecars.
/// The ledger and its sidecars, refusing anything that is not a regular file.
///
/// `reset` moves whatever this returns. A path check that only asked whether
/// something existed would move a directory named as the ledger, contents and
/// all, and replace it with a fresh database; a symlink would be moved as a
/// link and leave the real ledger behind. Both are refused, as are devices and
/// FIFOs, before anything moves.
pub fn ledger_components(database: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for path in std::iter::once(database.to_path_buf()).chain(
        SIDECAR_SUFFIXES
            .iter()
            .map(|suffix| sidecar(database, suffix)),
    ) {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => found.push(path),
            Ok(metadata) => {
                let kind = if metadata.file_type().is_symlink() {
                    "a symbolic link"
                } else if metadata.is_dir() {
                    "a directory"
                } else {
                    "not a regular file"
                };
                bail!(
                    "INVALID_INPUT: {} is {kind}, so it cannot be a Foremerge ledger or one of its sidecar files; nothing was moved",
                    path.display()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(anyhow::Error::from(error))
                    .with_context(|| format!("inspect {}", path.display()));
            }
        }
    }
    Ok(found)
}

pub fn ledger_files(database: &Path) -> Vec<PathBuf> {
    std::iter::once(database.to_path_buf())
        .chain(
            SIDECAR_SUFFIXES
                .iter()
                .map(|suffix| sidecar(database, suffix)),
        )
        .filter(|path| path.exists())
        .collect()
}

fn sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Every other process that has one of the ledger's files open.
///
/// A running MCP server holds the ledger open for its whole life, and moving
/// or migrating the ledger underneath it is exactly how an outage starts. This
/// must run while this process holds no connection to the ledger itself.
pub fn holders(database: &Path) -> HolderCheck {
    let files: Vec<PathBuf> = ledger_files(database)
        .into_iter()
        .map(|path| path.canonicalize().unwrap_or(path))
        .collect();
    if files.is_empty() {
        return HolderCheck::Checked(Vec::new());
    }
    #[cfg(target_os = "linux")]
    {
        proc_holders(&files)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        lsof_holders(&files)
    }
    #[cfg(not(unix))]
    {
        let _ = files;
        HolderCheck::Unavailable(
            "this platform cannot list the processes holding a file open".to_string(),
        )
    }
}

#[cfg(target_os = "linux")]
fn proc_holders(files: &[PathBuf]) -> HolderCheck {
    let own = std::process::id();
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(error) => return HolderCheck::Unavailable(format!("read /proc: {error}")),
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own {
            continue;
        }
        // Another user's descriptors are unreadable, and a process that exits
        // mid-scan vanishes: both simply contribute nothing.
        let Ok(descriptors) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let holds = descriptors.flatten().any(|descriptor| {
            std::fs::read_link(descriptor.path()).is_ok_and(|target| files.contains(&target))
        });
        if holds {
            let command = std::fs::read_to_string(entry.path().join("comm"))
                .ok()
                .map(|value| value.trim().to_string());
            found.push(Holder { pid, command });
        }
    }
    found.sort_by_key(|holder| holder.pid);
    HolderCheck::Checked(found)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn lsof_holders(files: &[PathBuf]) -> HolderCheck {
    let Some(lsof) = ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.is_file())
    else {
        return HolderCheck::Unavailable("lsof is not installed".to_string());
    };
    let mut command = Command::new(lsof);
    command
        .args(["-w", "-F", "pc", "--"])
        .args(files)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = match run_with_timeout(command, LSOF_TIMEOUT) {
        Ok(output) => output,
        Err(error) => return HolderCheck::Unavailable(format!("run lsof: {error:#}")),
    };
    // lsof exits 1 both when no process has the files open and on some
    // partial failures, so the output, not the status, is the answer.
    let own = std::process::id();
    let mut found: Vec<Holder> = Vec::new();
    for line in String::from_utf8_lossy(&output).lines() {
        if let Some(pid) = line.strip_prefix('p') {
            if let Ok(pid) = pid.parse::<u32>() {
                if pid != own {
                    found.push(Holder { pid, command: None });
                }
            }
        } else if let Some(name) = line.strip_prefix('c') {
            if let Some(last) = found.last_mut() {
                if last.command.is_none() {
                    last.command = Some(name.to_string());
                }
            }
        }
    }
    found.sort_by_key(|holder| holder.pid);
    found.dedup_by_key(|holder| holder.pid);
    HolderCheck::Checked(found)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut child = command.spawn().context("spawn")?;
    let mut stdout = child.stdout.take().context("capture stdout")?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {} seconds", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("lsof output reader panicked"))
}

/// Refuse when another process holds the ledger open, naming the processes.
/// Returns a warning when this platform cannot tell.
pub fn ensure_not_in_use(database: &Path, action: &str) -> Result<Option<String>> {
    match holders(database) {
        HolderCheck::Checked(found) if found.is_empty() => Ok(None),
        HolderCheck::Checked(found) => bail!(
            "LEDGER_IN_USE: {} other process(es) have the ledger at {} open: {}. Close the agent client sessions (Claude Code, Codex, Cursor) and any `foremerge mcp` or `foremerge daemon` using this repository, then {action} again",
            found.len(),
            database.display(),
            describe_holders(&found)
        ),
        HolderCheck::Unavailable(reason) => Ok(Some(format!(
            "Could not check whether other processes have the ledger open ({reason}). Close every agent client session using this repository before continuing."
        ))),
    }
}

fn describe_holders(found: &[Holder]) -> String {
    found
        .iter()
        .map(|holder| match holder.command.as_deref() {
            Some(command) => format!("pid {} ({command})", holder.pid),
            None => format!("pid {}", holder.pid),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a ledger file holds, read without writing to it. Every field is best
/// effort: this describes ledgers this build cannot use, whose tables may have
/// changed shape.
pub fn describe(database: &Path) -> Result<Value> {
    let conn = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open ledger read-only {}", database.display()))?;
    conn.busy_timeout(Duration::from_secs(2))?;
    let meta = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .ok()
        .flatten()
    };
    let schema_version = meta("schema_version");
    // Untrusted: whatever build wrote this ledger put it there, and a ledger
    // can arrive from anywhere through `--from`.
    // Read bounded, so an oversized value is never loaded whole.
    let written_by = conn
        .query_row(WRITER_QUERY, [SCHEMA_WRITER_KEY], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()
        .map(|value| {
            readable_writer(&value).unwrap_or_else(|| "an unreadable version".to_string())
        });
    let mut counts = serde_json::Map::new();
    for table in SUMMARY_TABLES {
        let count: Option<i64> = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .ok();
        counts.insert(table.to_string(), json!(count));
    }
    Ok(json!({
        "path": database,
        "schema_version": schema_version.as_deref().and_then(|value| value.trim().parse::<i64>().ok()),
        "schema_written_by": written_by,
        "repository_common_dir": meta("repository_common_dir"),
        "counts": counts,
    }))
}

/// Copy a backup into a new self-contained file and confirm this build can use
/// it. The copy, not the backup, is what gets restored, so a restore never
/// writes to the operator's backup and never depends on its sidecar files.
fn stage_restore(source: &Path, staged: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open backup read-only {}", source.display()))?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.execute("VACUUM INTO ?1", [staged.to_string_lossy()])
        .with_context(|| format!("copy backup {} to {}", source.display(), staged.display()))?;
    drop(conn);
    let copy = Connection::open_with_flags(
        staged,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let integrity: String = copy.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!(
            "CORRUPT_STORE: the backup {} fails SQLite's integrity check: {integrity}",
            source.display()
        );
    }
    Ok(())
}

/// Refuse a restore source that is valid SQLite but not a usable Foremerge
/// ledger, before anything moves.
///
/// `stage_restore` proves only that the file is intact SQLite. A database with
/// a `meta` table claiming the current schema and an `agents` table of the
/// wrong shape passed, the live ledger was set aside, and the next command
/// failed with "no such column". So the staged copy is migrated, on a
/// throwaway copy of its own, exactly as `Store::open` will migrate it once
/// installed, and then compared with a fresh ledger of this build: every table
/// must exist with every column, and every trigger, since the triggers are what
/// keep the event journal and the decision records append-only.
fn validate_staged_schema(staged: &Path) -> Result<()> {
    let parent = staged
        .parent()
        .context("the staged restore has no parent directory")?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let candidate = parent.join(format!(".validate-{token}.sqlite3"));
    let reference = parent.join(format!(".reference-{token}.sqlite3"));
    let result = (|| -> Result<()> {
        std::fs::copy(staged, &candidate)
            .with_context(|| format!("copy {} for validation", staged.display()))?;
        drop(
            Store::open(&candidate).context(
                "INVALID_INPUT: the backup could not be brought up to this build's schema",
            )?,
        );
        drop(Store::open(&reference)?);
        let open = |path: &Path| {
            Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
        };
        let (have, want) = (open(&candidate)?, open(&reference)?);
        let names = |conn: &Connection, kind: &str| -> Result<Vec<String>> {
            let mut statement = conn.prepare(
                "SELECT name FROM sqlite_master WHERE type = ?1 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )?;
            let rows = statement
                .query_map([kind], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        };
        let columns = |conn: &Connection, table: &str| -> Result<Vec<String>> {
            let mut statement = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
            let rows = statement
                .query_map([table], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        };
        let have_tables = names(&have, "table")?;
        for table in names(&want, "table")? {
            if !have_tables.contains(&table) {
                bail!(
                    "INVALID_INPUT: the backup is not a usable Foremerge ledger: it has no {table} table"
                );
            }
            let present = columns(&have, &table)?;
            for column in columns(&want, &table)? {
                if !present.contains(&column) {
                    bail!(
                        "INVALID_INPUT: the backup is not a usable Foremerge ledger: table {table} has no {column} column"
                    );
                }
            }
        }
        let have_triggers = names(&have, "trigger")?;
        for trigger in names(&want, "trigger")? {
            if !have_triggers.contains(&trigger) {
                bail!(
                    "INVALID_INPUT: the backup is not a usable Foremerge ledger: it lacks the {trigger} trigger that keeps its records append-only"
                );
            }
        }
        Ok(())
    })();
    for path in [&candidate, &reference] {
        for file in std::iter::once(path.to_path_buf())
            .chain(SIDECAR_SUFFIXES.iter().map(|suffix| sidecar(path, suffix)))
        {
            let _ = std::fs::remove_file(file);
        }
    }
    // Returned as is: the typed prefix is what callers and agents act on.
    result
}

fn validate_restore_source(
    source: &Path,
    database: &Path,
    repository: Option<&Path>,
) -> Result<Value> {
    let metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("INVALID_INPUT: cannot read backup {}", source.display()))?;
    if !metadata.is_file() {
        bail!(
            "INVALID_INPUT: the backup to restore must be a regular file: {}",
            source.display()
        );
    }
    if let (Ok(left), Ok(right)) = (source.canonicalize(), database.canonicalize()) {
        if left == right {
            bail!("INVALID_INPUT: --from names the live ledger itself; name a backup");
        }
    }
    let summary = describe(source)?;
    match summary["schema_version"].as_i64() {
        None => bail!(
            "INVALID_INPUT: {} is not a Foremerge ledger this build can read (no schema_version)",
            source.display()
        ),
        Some(version) if version > DATABASE_SCHEMA_VERSION => bail!(
            "UNSUPPORTED_SCHEMA: the backup {} is database schema {version}, newer than this build supports ({DATABASE_SCHEMA_VERSION}); restore it with a Foremerge build that supports that schema",
            source.display()
        ),
        Some(_) => {}
    }
    // A ledger records the repository it belongs to, and every command checks
    // that binding. Restoring another repository's ledger therefore succeeds
    // and then refuses every command, while `doctor`, which the recovery guide
    // tells the operator to run, reports it healthy. Refuse it here, where the
    // cause is still obvious.
    if let (Some(source_repo), Some(target_repo)) =
        (summary["repository_common_dir"].as_str(), repository)
    {
        let target = canonical(target_repo);
        let recorded = canonical(Path::new(source_repo));
        if target != recorded {
            bail!(
                "INVALID_INPUT: the backup {} belongs to a different Git repository ({}); restore it there, or start a fresh ledger with `foremerge ledger reset` and no --from",
                source.display(),
                source_repo
            );
        }
    }
    Ok(summary)
}

/// Compare paths by their resolved form where possible, so a symlinked or
/// relative spelling of one directory is not read as two.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn backup_directory(database: &Path, schema: Option<i64>) -> Result<PathBuf> {
    let parent = database
        .parent()
        .context("the ledger path has no parent directory")?;
    let root = parent.join("backups");
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let label = schema.map_or_else(|| "unknown-schema".to_string(), |v| format!("schema{v}"));
    let mut candidate = root.join(format!("{stamp}-{label}"));
    let mut attempt = 1;
    while candidate.exists() {
        attempt += 1;
        candidate = root.join(format!("{stamp}-{label}-{attempt}"));
    }
    Ok(candidate)
}

/// Create a backup directory and its `backups` parent, private to the owner.
///
/// Only the directories this creates are made private. An earlier version also
/// chmodded the parent of whatever it was given, which for the default ledger
/// path is the repository's `.git` directory, and for an unusual `--database`
/// could be any directory at all.
fn create_backup_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    if let Some(root) = path.parent() {
        // Only tighten what this call brings into existence. An operator's own
        // `backups` directory, or a symlink standing in for one, is theirs: an
        // earlier version chmodded whatever it found, including through a
        // symlink to a directory elsewhere.
        let existed = root.symlink_metadata().is_ok();
        std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
        #[cfg(unix)]
        if !existed {
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let existed = path.symlink_metadata().is_ok();
    std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    if !existed {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// `foremerge ledger reset`: set the ledger aside intact and start a fresh one
/// or restore a backup. Without `apply` it only reports what it would do.
pub fn reset(
    database: &Path,
    from: Option<&Path>,
    apply: bool,
    repository: Option<&Path>,
) -> Result<Value> {
    // A symlink at the ledger path would be moved as a symlink: the real file
    // would stay behind, the report would describe nothing, and the backup
    // could not be restored. Refuse instead of half-working; the real path
    // works with `--database`.
    if let Ok(metadata) = std::fs::symlink_metadata(database) {
        if metadata.file_type().is_symlink() {
            bail!(
                "INVALID_INPUT: {} is a symbolic link; run `foremerge ledger reset --database` against the ledger it points at",
                database.display()
            );
        }
    }
    let exists = std::fs::symlink_metadata(database).is_ok_and(|metadata| metadata.is_file());
    // Sidecars without a main file are not nothing: SQLite rebuilds a ledger
    // from a leftover `-wal`, so a stale sidecar would resurrect the old
    // ledger under a restored one, or corrupt it. They are set aside too.
    let present = !ledger_components(database)?.is_empty();
    if !present && from.is_none() {
        bail!(
            "NOT_INITIALIZED: there is no ledger at {} to reset; run `foremerge init` to create one",
            database.display()
        );
    }
    // Before any connection is opened here: the check must not see this
    // process, and nothing may be moved while another process writes. A dry
    // run reports the holders rather than refusing, so the operator learns
    // everything that stands in the way at once.
    let mut warnings: Vec<String> = Vec::new();
    let in_use_by = if apply {
        if let Some(warning) = ensure_not_in_use(database, "run `foremerge ledger reset --yes`")? {
            warnings.push(warning);
        }
        Vec::new()
    } else {
        match holders(database) {
            HolderCheck::Checked(found) => found,
            HolderCheck::Unavailable(reason) => {
                warnings.push(format!(
                    "Could not check whether other processes have the ledger open ({reason}). Close every agent client session using this repository before continuing."
                ));
                Vec::new()
            }
        }
    };
    let current = if exists {
        Some(
            describe(database)
                .unwrap_or_else(|error| json!({ "path": database, "error": format!("{error:#}") })),
        )
    } else {
        None
    };
    let restore = from
        .map(|source| validate_restore_source(source, database, repository))
        .transpose()?;
    let backup_dir = if present {
        Some(backup_directory(
            database,
            current
                .as_ref()
                .and_then(|value| value["schema_version"].as_i64()),
        )?)
    } else {
        None
    };
    let next_step = "Restart every agent client session in this repository so its MCP server opens the new ledger, then run `foremerge doctor`";
    if !apply {
        return Ok(json!({
            "applied": false,
            "database": database,
            "current": current,
            "restore_from": restore,
            "backup_dir": backup_dir,
            "in_use_by": in_use_by,
            "warnings": warnings,
            "next_step": if in_use_by.is_empty() {
                "Nothing was changed. Rerun with --yes to set the current ledger aside and continue".to_string()
            } else {
                format!(
                    "Nothing was changed. Close the processes in in_use_by ({}), then rerun with --yes",
                    describe_holders(&in_use_by)
                )
            },
        }));
    }

    let staged = match from {
        Some(source) => {
            let parent = database
                .parent()
                .context("the ledger path has no parent directory")?;
            // Only create what must exist. `Store::open` sets the ledger
            // directory's own permissions; nothing here touches the
            // permissions of a directory it did not create.
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
            let staged = parent.join(format!(
                ".restore-{}.sqlite3",
                uuid::Uuid::new_v4().simple()
            ));
            if let Err(error) =
                stage_restore(source, &staged).and_then(|()| validate_staged_schema(&staged))
            {
                let _ = std::fs::remove_file(&staged);
                return Err(error);
            }
            Some(staged)
        }
        None => None,
    };

    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    if let Some(backup_dir) = backup_dir.as_ref() {
        // A staged copy is the whole coordination history sitting beside the
        // ledger. Failing to make the backup directory must not leave it there.
        if let Err(error) = create_backup_dir(backup_dir) {
            if let Some(staged) = staged.as_ref() {
                let _ = std::fs::remove_file(staged);
            }
            return Err(error);
        }
        let components = match ledger_components(database) {
            Ok(components) => components,
            Err(error) => {
                if let Some(staged) = staged.as_ref() {
                    let _ = std::fs::remove_file(staged);
                }
                return Err(error);
            }
        };
        for file in components {
            let target = backup_dir.join(file.file_name().context("ledger file name")?);
            if let Err(error) = std::fs::rename(&file, &target) {
                // Put back what already moved, so a failure leaves the ledger
                // where every client expects it.
                for (original, backup) in moved.iter().rev() {
                    let _ = std::fs::rename(backup, original);
                }
                if let Some(staged) = staged.as_ref() {
                    let _ = std::fs::remove_file(staged);
                }
                return Err(anyhow::Error::from(error))
                    .with_context(|| format!("move {} to {}", file.display(), target.display()));
            }
            moved.push((file, target));
        }
        // A client that started between the check and the move now holds the
        // backup, and anything it writes would land there rather than in the
        // new ledger: two diverging histories. That is a refusal, not a
        // warning, and the files go back where every client expects them.
        if let HolderCheck::Checked(found) =
            holders(&backup_dir.join(database.file_name().context("ledger file name")?))
        {
            if !found.is_empty() {
                for (original, backup) in moved.iter().rev() {
                    let _ = std::fs::rename(backup, original);
                }
                let _ = std::fs::remove_dir(backup_dir);
                if let Some(staged) = staged.as_ref() {
                    let _ = std::fs::remove_file(staged);
                }
                bail!(
                    "LEDGER_IN_USE: {} process(es) opened the ledger while it was being set aside: {}. It was put back unchanged; stop them and run `foremerge ledger reset --yes` again.",
                    found.len(),
                    describe_holders(&found)
                );
            }
        }
    }

    if let Some(staged) = staged.as_ref() {
        std::fs::rename(staged, database).with_context(|| {
            format!(
                "move restored ledger {} to {}",
                staged.display(),
                database.display()
            )
        })?;
    }
    // Opening creates a fresh ledger, or brings a restored older one up to
    // this build's schema, so the next client finds it ready.
    let store = Store::open(database)?;
    drop(store);
    let result = describe(database)?;

    Ok(json!({
        "applied": true,
        "database": database,
        "set_aside": current,
        "backup_dir": backup_dir,
        "restored_from": restore,
        "ledger": result,
        "warnings": warnings,
        "next_step": next_step,
    }))
}
