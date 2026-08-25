use crate::model::{Event, EventChainAudit, Scope};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params, types::Type,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::Duration;
use uuid::Uuid;

const DATABASE_SCHEMA_VERSION: i64 = 5;
// Event hashing is versioned independently from the mutable SQLite projection
// schema. A database migration must not silently change the hash material for
// otherwise identical events.
const EVENT_SCHEMA_VERSION: i64 = 1;

#[derive(Clone)]
pub struct Store {
    pub(crate) conn: Arc<Mutex<Connection>>,
    path: Arc<PathBuf>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create database directory {}", parent.display()))?;
            #[cfg(unix)]
            if matches!(
                parent.file_name().and_then(|value| value.to_str()),
                Some("foremerge" | ".foremerge")
            ) {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        #[cfg(unix)]
        {
            use std::io::ErrorKind;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            if !path.exists() {
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true).mode(0o600);
                match options.open(&path) {
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                anyhow::bail!(
                    "INVALID_INPUT: database path must be a regular file: {}",
                    path.display()
                );
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )
        .with_context(|| format!("open SQLite database {}", path.display()))?;
        Self::configure(&conn)?;
        Self::migrate(&conn)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            for suffix in ["-wal", "-shm"] {
                let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
                if sidecar.exists() {
                    std::fs::set_permissions(sidecar, std::fs::Permissions::from_mode(0o600))?;
                }
            }
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: Arc::new(path),
        })
    }

    /// Open an existing store without creating files or running migrations.
    /// Diagnostics use this path so observation can never initialize or alter
    /// the system it is inspecting.
    pub fn open_existing_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let metadata = std::fs::symlink_metadata(&path).with_context(|| {
            format!(
                "NOT_INITIALIZED: database does not exist: {}",
                path.display()
            )
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!(
                "INVALID_INPUT: database path must be an existing regular file: {}",
                path.display()
            );
        }
        let conn = Self::open_read_only_connection(&path)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: Arc::new(path),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::configure(&conn)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: Arc::new(PathBuf::from(":memory:")),
        })
    }

    fn configure(conn: &Connection) -> Result<()> {
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.execute_batch(
            // Recursive triggers make the append-only triggers fire for the
            // implicit delete inside an INSERT OR REPLACE. Without it, REPLACE
            // silently bypasses the immutability guards on validation_attempts,
            // conflict_detections, and events.
            "PRAGMA foreign_keys = ON;
             PRAGMA recursive_triggers = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
        )?;
        Ok(())
    }

    fn open_read_only_connection(path: &Path) -> Result<Connection> {
        // Resolve only parent aliases (not the final database component) so
        // macOS's `/var` -> `/private/var` alias works while SQLite NOFOLLOW
        // still rejects replacement of the database itself with a symlink.
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("database path has no parent: {}", path.display()))?;
        let file_name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("database path has no file name: {}", path.display()))?;
        let resolved = std::fs::canonicalize(parent)
            .with_context(|| format!("resolve SQLite database parent {}", parent.display()))?
            .join(file_name);
        let conn = Connection::open_with_flags(
            &resolved,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .with_context(|| format!("open SQLite database read-only {}", resolved.display()))?;
        conn.busy_timeout(Duration::from_millis(500))?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA query_only = ON;")?;
        Ok(conn)
    }

    fn migrate(conn: &Connection) -> Result<()> {
        // One transaction for the whole migration. A partially applied
        // migration would otherwise leave derived projections incomplete, and
        // an incomplete intent_scopes projection silently omits intents from
        // scope queries, which is worse than failing to open.
        // IMMEDIATE, not the default DEFERRED: migration always writes, and a
        // deferred transaction would take a read lock first and then deadlock
        // on the upgrade when two processes open the same store at once,
        // failing instantly instead of waiting out the busy timeout.
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        Self::migrate_in(&tx)?;
        tx.commit()?;
        Ok(())
    }

    fn migrate_in(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS agents (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                model TEXT,
                capabilities_json TEXT NOT NULL,
                worktree TEXT,
                git_branch TEXT,
                git_head TEXT,
                status TEXT NOT NULL,
                registered_at TEXT NOT NULL,
                last_seen_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                task_key TEXT NOT NULL UNIQUE,
                title TEXT NOT NULL,
                created_by_agent_id TEXT NOT NULL REFERENCES agents(id),
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS intents (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL REFERENCES agents(id),
                task_id TEXT NOT NULL REFERENCES tasks(id),
                summary TEXT NOT NULL,
                rationale TEXT,
                scopes_json TEXT NOT NULL,
                depends_on_json TEXT NOT NULL,
                metadata_json TEXT NOT NULL,
                status TEXT NOT NULL,
                version INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_intents_created ON intents(created_at DESC, id DESC);
            CREATE INDEX IF NOT EXISTS idx_intents_agent_created
              ON intents(agent_id, created_at DESC, id DESC);
            CREATE INDEX IF NOT EXISTS idx_intents_status_created
              ON intents(status, created_at DESC, id DESC);
            CREATE INDEX IF NOT EXISTS idx_intents_agent_status_created
              ON intents(agent_id, status, created_at DESC, id DESC);

            CREATE TABLE IF NOT EXISTS intent_scopes (
                intent_id TEXT NOT NULL REFERENCES intents(id),
                scope_kind TEXT NOT NULL,
                scope_key TEXT NOT NULL,
                canonical_scope TEXT NOT NULL,
                PRIMARY KEY(intent_id, canonical_scope)
            );
            CREATE INDEX IF NOT EXISTS idx_intent_scopes_canonical
              ON intent_scopes(canonical_scope, intent_id);

            CREATE TABLE IF NOT EXISTS claims (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL REFERENCES agents(id),
                intent_id TEXT NOT NULL REFERENCES intents(id),
                scope_kind TEXT NOT NULL,
                scope_key TEXT NOT NULL,
                canonical_scope TEXT NOT NULL,
                status TEXT NOT NULL,
                reason TEXT,
                lease_expires_at TEXT NOT NULL,
                created_at TEXT NOT NULL,
                released_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_claims_scope ON claims(canonical_scope, status);
            CREATE INDEX IF NOT EXISTS idx_claims_intent ON claims(intent_id, status);

            CREATE TABLE IF NOT EXISTS changesets (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL REFERENCES agents(id),
                task_id TEXT NOT NULL REFERENCES tasks(id),
                intent_id TEXT NOT NULL REFERENCES intents(id),
                summary TEXT NOT NULL,
                files_json TEXT NOT NULL,
                symbols_json TEXT NOT NULL,
                contracts_json TEXT NOT NULL,
                dependencies_json TEXT NOT NULL,
                tests_json TEXT NOT NULL,
                decisions_json TEXT NOT NULL,
                provenance_json TEXT NOT NULL,
                base_ref TEXT,
                git_ref TEXT,
                accepted_commit TEXT,
                integration_commit TEXT,
                supersedes_changeset_id TEXT REFERENCES changesets(id),
                worktree TEXT,
                acceptance_verification TEXT,
                acceptance_reason TEXT,
                fingerprint TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_changesets_current_intent
              ON changesets(intent_id) WHERE status <> 'SUPERSEDED';
            CREATE INDEX IF NOT EXISTS idx_changesets_intent ON changesets(intent_id, created_at);

            CREATE TABLE IF NOT EXISTS validations (
                id TEXT PRIMARY KEY,
                changeset_id TEXT NOT NULL REFERENCES changesets(id),
                command_json TEXT NOT NULL,
                passed INTEGER NOT NULL,
                exit_code INTEGER,
                stdout TEXT NOT NULL,
                stderr TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                fingerprint TEXT NOT NULL,
                run_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_validations_changeset ON validations(changeset_id, run_at);

            -- This is the table acceptance actually reads, so it needs at least
            -- the protection the audit tables have. A row is written once, in
            -- the same transaction as its authoritative attempt, and is never
            -- updated or deleted afterwards.
            CREATE TRIGGER IF NOT EXISTS validations_no_update
            BEFORE UPDATE ON validations
            BEGIN SELECT RAISE(ABORT, 'validations are append-only'); END;

            CREATE TRIGGER IF NOT EXISTS validations_no_delete
            BEFORE DELETE ON validations
            BEGIN SELECT RAISE(ABORT, 'validations are append-only'); END;

            CREATE TRIGGER IF NOT EXISTS validations_no_replace
            BEFORE INSERT ON validations
            WHEN EXISTS(SELECT 1 FROM validations WHERE id = NEW.id)
            BEGIN SELECT RAISE(ABORT, 'validations are append-only'); END;

            CREATE TABLE IF NOT EXISTS validation_attempts (
                id TEXT PRIMARY KEY,
                changeset_id TEXT NOT NULL REFERENCES changesets(id),
                command_json TEXT NOT NULL,
                passed INTEGER NOT NULL,
                exit_code INTEGER,
                stdout TEXT NOT NULL,
                stderr TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                expected_fingerprint TEXT NOT NULL,
                observed_fingerprint TEXT NOT NULL,
                authoritative INTEGER NOT NULL,
                stale_reason TEXT,
                changed_files_json TEXT NOT NULL,
                excluded_paths_json TEXT NOT NULL,
                exclusion_ruleset_digest TEXT NOT NULL,
                run_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_validation_attempts_changeset
              ON validation_attempts(changeset_id, run_at DESC, id DESC);

            CREATE TRIGGER IF NOT EXISTS validation_attempts_no_update
            BEFORE UPDATE ON validation_attempts
            BEGIN SELECT RAISE(ABORT, 'validation attempts are immutable'); END;

            CREATE TRIGGER IF NOT EXISTS validation_attempts_no_delete
            BEFORE DELETE ON validation_attempts
            BEGIN SELECT RAISE(ABORT, 'validation attempts are immutable'); END;

            CREATE TRIGGER IF NOT EXISTS validation_attempts_no_replace
            BEFORE INSERT ON validation_attempts
            WHEN EXISTS(SELECT 1 FROM validation_attempts WHERE id = NEW.id)
            BEGIN SELECT RAISE(ABORT, 'validation attempts are immutable'); END;

            CREATE TABLE IF NOT EXISTS decisions (
                id TEXT PRIMARY KEY,
                changeset_id TEXT REFERENCES changesets(id),
                intent_id TEXT NOT NULL REFERENCES intents(id),
                title TEXT NOT NULL,
                rationale TEXT NOT NULL,
                alternatives_json TEXT NOT NULL,
                provenance_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS conflicts (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                severity TEXT NOT NULL,
                score REAL NOT NULL,
                source_intent_id TEXT REFERENCES intents(id),
                target_intent_id TEXT NOT NULL REFERENCES intents(id),
                scope_json TEXT,
                scope_identity TEXT NOT NULL DEFAULT '',
                explanation TEXT NOT NULL,
                suggestion TEXT NOT NULL,
                evidence_json TEXT NOT NULL,
                status TEXT NOT NULL,
                detected_at TEXT NOT NULL,
                -- Legacy display-JSON uniqueness retained to avoid a
                -- destructive table rebuild. idx_conflicts_identity below is
                -- the authoritative canonical identity constraint.
                UNIQUE(source_intent_id, target_intent_id, kind, scope_json)
            );
            CREATE INDEX IF NOT EXISTS idx_conflicts_source ON conflicts(source_intent_id, status);
            CREATE INDEX IF NOT EXISTS idx_conflicts_target ON conflicts(target_intent_id, status);

            CREATE TABLE IF NOT EXISTS conflict_detections (
                id TEXT PRIMARY KEY,
                conflict_id TEXT NOT NULL REFERENCES conflicts(id),
                severity TEXT NOT NULL,
                score REAL NOT NULL,
                scope_json TEXT,
                explanation TEXT NOT NULL,
                suggestion TEXT NOT NULL,
                evidence_json TEXT NOT NULL,
                previously_settled INTEGER NOT NULL,
                detected_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_conflict_detections_conflict
              ON conflict_detections(conflict_id, detected_at, id);

            CREATE TRIGGER IF NOT EXISTS conflict_detections_no_update
            BEFORE UPDATE ON conflict_detections
            BEGIN SELECT RAISE(ABORT, 'conflict detections are immutable'); END;

            CREATE TRIGGER IF NOT EXISTS conflict_detections_no_delete
            BEFORE DELETE ON conflict_detections
            BEGIN SELECT RAISE(ABORT, 'conflict detections are immutable'); END;

            -- INSERT OR REPLACE deletes the conflicting row first, and that
            -- delete only fires the trigger above when recursive_triggers is
            -- enabled, which is a per-connection setting an outside client does
            -- not share. Rejecting an insert that reuses an existing id makes
            -- the guarantee live in the schema instead.
            CREATE TRIGGER IF NOT EXISTS conflict_detections_no_replace
            BEFORE INSERT ON conflict_detections
            WHEN EXISTS(SELECT 1 FROM conflict_detections WHERE id = NEW.id)
            BEGIN SELECT RAISE(ABORT, 'conflict detections are immutable'); END;

            CREATE TABLE IF NOT EXISTS coordination_messages (
                id TEXT PRIMARY KEY,
                from_agent_id TEXT NOT NULL REFERENCES agents(id),
                to_agent_id TEXT NOT NULL REFERENCES agents(id),
                conflict_id TEXT REFERENCES conflicts(id),
                changeset_id TEXT REFERENCES changesets(id),
                message TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_recipient ON coordination_messages(to_agent_id, status, created_at);

            CREATE TABLE IF NOT EXISTS graph_nodes (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                node_key TEXT NOT NULL,
                label TEXT NOT NULL,
                data_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                UNIQUE(kind, node_key)
            );
            CREATE INDEX IF NOT EXISTS idx_graph_nodes_kind ON graph_nodes(kind);

            CREATE TABLE IF NOT EXISTS graph_edges (
                id TEXT PRIMARY KEY,
                from_node_id TEXT NOT NULL REFERENCES graph_nodes(id),
                to_node_id TEXT NOT NULL REFERENCES graph_nodes(id),
                kind TEXT NOT NULL,
                data_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                UNIQUE(from_node_id, to_node_id, kind)
            );
            CREATE INDEX IF NOT EXISTS idx_graph_edges_from ON graph_edges(from_node_id, kind);
            CREATE INDEX IF NOT EXISTS idx_graph_edges_to ON graph_edges(to_node_id, kind);

            CREATE TABLE IF NOT EXISTS events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL UNIQUE,
                schema_version INTEGER NOT NULL,
                event_type TEXT NOT NULL,
                entity_type TEXT NOT NULL,
                entity_id TEXT NOT NULL,
                agent_id TEXT,
                payload_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                prev_hash TEXT NOT NULL,
                event_hash TEXT NOT NULL UNIQUE
            );
            CREATE INDEX IF NOT EXISTS idx_events_entity ON events(entity_type, entity_id, seq);

            CREATE TRIGGER IF NOT EXISTS events_no_update
            BEFORE UPDATE ON events
            BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END;

            CREATE TRIGGER IF NOT EXISTS events_no_delete
            BEFORE DELETE ON events
            BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END;

            -- INSERT OR REPLACE resolves against ANY unique key, deleting the
            -- conflicting row, and that delete only fires the trigger above
            -- when recursive_triggers is on, which is per-connection. This
            -- table has three unique keys, so all three must be checked. On an
            -- ordinary append NEW.seq is null (AUTOINCREMENT assigns it after
            -- this trigger) and the other two are fresh, so nothing matches.
            CREATE TRIGGER IF NOT EXISTS events_no_replace
            BEFORE INSERT ON events
            WHEN EXISTS(
                SELECT 1 FROM events
                WHERE seq = NEW.seq
                   OR event_id = NEW.event_id
                   OR event_hash = NEW.event_hash
            )
            BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END;
            "#,
        )?;
        // The schema version the store was last written with. Absent means a
        // brand new database, or one predating the stamp. One-time backfills
        // are gated on this: re-running them on every open is what let a
        // duplicate legacy detection row be minted for conflicts that already
        // had a native one.
        let recorded_version: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let stored_version: i64 = match recorded_version {
            // Parsed strictly. A CAST would silently read a malformed value as
            // zero, which would rerun every one-time backfill against a store
            // that has already had them applied.
            Some(value) => value.trim().parse::<i64>().map_err(|_| {
                anyhow::anyhow!(
                    "CORRUPT_STORE: schema_version is not an integer: {value:?}; this database was not written by Foremerge or is damaged"
                )
            })?,
            None => 0,
        };
        if stored_version > DATABASE_SCHEMA_VERSION {
            // Migrating downwards would rewrite the stamp and let this build
            // write a store it does not understand.
            bail!(
                "UNSUPPORTED_SCHEMA: database schema {stored_version} is newer than this build supports ({DATABASE_SCHEMA_VERSION}); upgrade Foremerge to open it"
            );
        }
        let has_supersedes: bool = conn.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM pragma_table_info('changesets') WHERE name = 'supersedes_changeset_id'
             )",
            [],
            |row| row.get(0),
        )?;
        if !has_supersedes {
            conn.execute(
                "ALTER TABLE changesets ADD COLUMN supersedes_changeset_id TEXT REFERENCES changesets(id)",
                [],
            )?;
        }
        for column in [
            "accepted_commit",
            "integration_commit",
            "acceptance_verification",
            "acceptance_reason",
        ] {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM pragma_table_info('changesets') WHERE name = ?1
                 )",
                [column],
                |row| row.get(0),
            )?;
            if !exists {
                conn.execute(
                    &format!("ALTER TABLE changesets ADD COLUMN {column} TEXT"),
                    [],
                )?;
            }
        }
        conn.execute(
            "UPDATE changesets SET accepted_commit = git_ref
             WHERE accepted_commit IS NULL AND status = 'ACCEPTED'",
            [],
        )?;
        conn.execute(
            "UPDATE changesets SET integration_commit = git_ref
             WHERE integration_commit IS NULL AND status = 'COMMITTED'",
            [],
        )?;
        if stored_version < 2 {
            conn.execute(
                // Explicit NOT EXISTS rather than OR IGNORE: the append-only
                // insert guard raises, and a raise is not a constraint
                // violation that OR IGNORE would swallow.
                "INSERT INTO validation_attempts(
                   id, changeset_id, command_json, passed, exit_code, stdout, stderr, duration_ms,
                   expected_fingerprint, observed_fingerprint, authoritative, stale_reason,
                   changed_files_json, excluded_paths_json, exclusion_ruleset_digest, run_at
                 )
                 SELECT id, changeset_id, command_json, passed, exit_code, stdout, stderr,
                        duration_ms, fingerprint, fingerprint, 1, NULL, '[]', '[]', 'legacy', run_at
                 FROM validations
                 WHERE NOT EXISTS(
                     SELECT 1 FROM validation_attempts a WHERE a.id = validations.id
                 )",
                [],
            )?;
        }
        // Below schema 3 a projection could have been left partially written by
        // an interrupted migration, so every intent is reprojected once.
        // Afterwards only intents with no projection at all need work, which is
        // the ordinary case of a store written by an older binary.
        // Schema 5 normalizes symbol scopes, discarding namespace and path
        // prefixes so `App\\Services\\Report::render` and `Report::render` are
        // one scope. Every canonical form stored under an older schema was
        // computed by the old rule, so a store that kept them would have new
        // rows that could never match old ones: exactly the silent
        // non-detection this release fixes. The projection is therefore rebuilt
        // from `intents.scopes_json`, which is the source of truth and is
        // untouched by the change.
        if stored_version < 5 {
            conn.execute("DELETE FROM intent_scopes", [])?;
            let mut statement = conn.prepare("SELECT id, scope_kind, scope_key FROM claims")?;
            let claims = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            for (claim_id, kind, key) in claims {
                let canonical = Scope::new(&kind, &key).canonical();
                conn.execute(
                    "UPDATE claims SET canonical_scope = ?2 WHERE id = ?1",
                    params![claim_id, canonical],
                )?;
            }
            // Two live claims on one intent can now share a canonical scope
            // where they did not before. Keep the longest-lived and release the
            // rest, so the intent does not appear to hold the same scope twice.
            conn.execute(
                "UPDATE claims SET status = 'RELEASED'
                 WHERE status = 'ACTIVE' AND id NOT IN (
                   SELECT id FROM claims c WHERE c.status = 'ACTIVE'
                   AND c.lease_expires_at = (
                     SELECT MAX(lease_expires_at) FROM claims d
                     WHERE d.intent_id = c.intent_id
                     AND d.canonical_scope = c.canonical_scope
                     AND d.status = 'ACTIVE'
                   )
                   GROUP BY c.intent_id, c.canonical_scope
                 )",
                [],
            )?;
        }
        let mut statement = if stored_version < 5 {
            conn.prepare("SELECT id, scopes_json FROM intents")?
        } else {
            conn.prepare(
                "SELECT id, scopes_json FROM intents
                 WHERE NOT EXISTS(SELECT 1 FROM intent_scopes s WHERE s.intent_id = intents.id)",
            )?
        };
        let intent_scopes = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for (intent_id, scopes_json) in intent_scopes {
            let scopes: Vec<Scope> = serde_json::from_str(&scopes_json)
                .with_context(|| format!("decode scopes for intent {intent_id}"))?;
            for scope in scopes {
                conn.execute(
                    "INSERT OR IGNORE INTO intent_scopes(
                     intent_id, scope_kind, scope_key, canonical_scope) VALUES(?1, ?2, ?3, ?4)",
                    params![intent_id, scope.kind, scope.key, scope.canonical()],
                )?;
            }
        }
        let has_scope_identity: bool = conn.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM pragma_table_info('conflicts') WHERE name = 'scope_identity'
             )",
            [],
            |row| row.get(0),
        )?;
        if !has_scope_identity {
            conn.execute(
                "ALTER TABLE conflicts ADD COLUMN scope_identity TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        let mut statement = conn.prepare(
            "SELECT id, scope_json FROM conflicts WHERE scope_identity = '' ORDER BY detected_at, id",
        )?;
        let conflict_scopes = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for (id, scope_json) in conflict_scopes {
            let identity = match scope_json {
                Some(value) => serde_json::from_str::<Option<Scope>>(&value)
                    .with_context(|| format!("decode scope for conflict {id}"))?
                    .map(|scope| scope.canonical())
                    .unwrap_or_else(|| "<none>".to_string()),
                None => "<none>".to_string(),
            };
            conn.execute(
                "UPDATE conflicts SET scope_identity = ?2 WHERE id = ?1",
                params![id, identity],
            )?;
        }
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_conflicts_identity
             ON conflicts(source_intent_id, target_intent_id, kind, scope_identity)",
            [],
        )
        .context("create canonical conflict identity index")?;
        if stored_version < 2 {
            // Only conflicts carrying no observation of their own need a
            // synthesized one. Without the NOT EXISTS guard this mints a
            // duplicate for every conflict that already recorded a native
            // detection.
            conn.execute(
                "INSERT INTO conflict_detections(
                   id, conflict_id, severity, score, scope_json, explanation, suggestion,
                   evidence_json, previously_settled, detected_at
                 )
                 SELECT 'dtn_legacy_' || id, id, severity, score, scope_json, explanation,
                        suggestion, evidence_json,
                        CASE WHEN status IN ('OPEN', 'COORDINATING') THEN 0 ELSE 1 END,
                        detected_at
                 FROM conflicts
                 WHERE NOT EXISTS(
                     SELECT 1 FROM conflict_detections d WHERE d.conflict_id = conflicts.id
                 )",
                [],
            )?;
        }
        if stored_version < 4 {
            // Schema 2 minted a synthesized detection for every conflict on
            // every open, so conflicts that already had a native observation
            // carry a duplicate. Remove only those duplicates: a legacy row is
            // genuine when it is the conflict's sole observation. The
            // append-only trigger is dropped for the length of this repair and
            // restored inside the same transaction.
            //
            // Every store below schema 4 is swept, not just those stamped 2.
            // Schema 2's migration was not transactional, so an upgrade killed
            // between the backfill and the stamp left duplicates behind a
            // version 1 stamp, and schema 3 repaired only stores stamped 2, so
            // it could stamp such a store as 3 with its duplicates intact.
            conn.execute_batch("DROP TRIGGER IF EXISTS conflict_detections_no_delete;")?;
            // Only a byte-identical twin is a phantom. A legacy row that
            // differs from every native row is a genuine earlier observation,
            // typically the sole record of a detection that predates schema 2,
            // and deleting it would destroy real history.
            let repaired = conn.execute(
                "DELETE FROM conflict_detections
                 WHERE id LIKE 'dtn\\_legacy\\_%' ESCAPE '\\'
                   AND EXISTS(
                       SELECT 1 FROM conflict_detections other
                       WHERE other.conflict_id = conflict_detections.conflict_id
                         AND other.id NOT LIKE 'dtn\\_legacy\\_%' ESCAPE '\\'
                         AND other.detected_at = conflict_detections.detected_at
                         AND other.severity = conflict_detections.severity
                         AND other.score = conflict_detections.score
                         AND other.explanation = conflict_detections.explanation
                         AND other.suggestion = conflict_detections.suggestion
                         AND other.evidence_json = conflict_detections.evidence_json
                         AND other.previously_settled = conflict_detections.previously_settled
                         AND IFNULL(other.scope_json, '')
                             = IFNULL(conflict_detections.scope_json, '')
                   )",
                [],
            )?;
            conn.execute_batch(
                "CREATE TRIGGER IF NOT EXISTS conflict_detections_no_delete
                 BEFORE DELETE ON conflict_detections
                 BEGIN SELECT RAISE(ABORT, 'conflict detections are immutable'); END;",
            )?;
            if repaired > 0 {
                tracing::info!(
                    removed = repaired,
                    "removed duplicate conflict detections written by schema 2"
                );
            }
        }
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [DATABASE_SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite connection lock poisoned"))
    }

    pub(crate) fn immediate_tx(conn: &mut Connection) -> Result<Transaction<'_>> {
        Ok(conn.transaction_with_behavior(TransactionBehavior::Immediate)?)
    }

    pub(crate) fn append_event(
        tx: &Transaction<'_>,
        event_type: &str,
        entity_type: &str,
        entity_id: &str,
        agent_id: Option<&str>,
        payload: &Value,
    ) -> Result<Event> {
        let prev_hash: String = tx
            .query_row(
                "SELECT event_hash FROM events ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_else(|| "GENESIS".to_string());
        let event_id = format!("evt_{}", Uuid::new_v4().simple());
        let created_at = Utc::now().to_rfc3339();
        let payload_json = serde_json::to_string(payload)?;
        let material = format!(
            "{prev_hash}|{event_id}|{EVENT_SCHEMA_VERSION}|{event_type}|{entity_type}|{entity_id}|{}|{created_at}|{payload_json}",
            agent_id.unwrap_or("")
        );
        let event_hash = format!("{:x}", Sha256::digest(material.as_bytes()));
        tx.execute(
            "INSERT INTO events(event_id, schema_version, event_type, entity_type, entity_id,
             agent_id, payload_json, created_at, prev_hash, event_hash)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event_id,
                EVENT_SCHEMA_VERSION,
                event_type,
                entity_type,
                entity_id,
                agent_id,
                payload_json,
                created_at,
                prev_hash,
                event_hash
            ],
        )?;
        let seq = tx.last_insert_rowid();
        Ok(Event {
            seq,
            event_id,
            event_type: event_type.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            agent_id: agent_id.map(str::to_string),
            payload: payload.clone(),
            created_at,
            prev_hash,
            event_hash,
        })
    }

    pub(crate) fn upsert_node(
        tx: &Transaction<'_>,
        kind: &str,
        key: &str,
        label: &str,
        data: &Value,
    ) -> Result<String> {
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM graph_nodes WHERE kind = ?1 AND node_key = ?2",
                params![kind, key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            let is_placeholder = data.as_object().is_some_and(serde_json::Map::is_empty);
            if !is_placeholder {
                tx.execute(
                    "UPDATE graph_nodes SET label = ?2, data_json = ?3 WHERE id = ?1",
                    params![id, label, serde_json::to_string(data)?],
                )?;
            }
            return Ok(id);
        }
        let id = format!("nod_{}", Uuid::new_v4().simple());
        tx.execute(
            "INSERT INTO graph_nodes(id, kind, node_key, label, data_json, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                kind,
                key,
                label,
                serde_json::to_string(data)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(id)
    }

    pub(crate) fn link_nodes(
        tx: &Transaction<'_>,
        from_node_id: &str,
        to_node_id: &str,
        kind: &str,
        data: &Value,
    ) -> Result<String> {
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM graph_edges WHERE from_node_id = ?1 AND to_node_id = ?2 AND kind = ?3",
                params![from_node_id, to_node_id, kind],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(id);
        }
        let id = format!("edg_{}", Uuid::new_v4().simple());
        tx.execute(
            "INSERT INTO graph_edges(id, from_node_id, to_node_id, kind, data_json, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                from_node_id,
                to_node_id,
                kind,
                serde_json::to_string(data)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(id)
    }

    pub fn events(&self, after_seq: i64, limit: usize) -> Result<Vec<Event>> {
        let conn = self.lock()?;
        let mut statement = conn.prepare(
            "SELECT seq, event_id, event_type, entity_type, entity_id, agent_id, payload_json,
             created_at, prev_hash, event_hash
             FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after_seq, limit.min(1000) as i64], |row| {
            let payload_json: String = row.get(6)?;
            Ok(Event {
                seq: row.get(0)?,
                event_id: row.get(1)?,
                event_type: row.get(2)?,
                entity_type: row.get(3)?,
                entity_id: row.get(4)?,
                agent_id: row.get(5)?,
                payload: parse_json_column(payload_json, 6)?,
                created_at: row.get(7)?,
                prev_hash: row.get(8)?,
                event_hash: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn verify_event_chain(&self) -> Result<bool> {
        Ok(self.audit_event_chain(1000)?.valid)
    }

    /// Verify a stable prefix of the append-only event log in bounded pages.
    /// Persistent stores use a dedicated read-only connection, so this audit
    /// never holds the coordinator's shared process mutex.
    pub fn audit_event_chain(&self, page_size: usize) -> Result<EventChainAudit> {
        let page_size = page_size.clamp(1, 10_000);
        if self.path.as_os_str() == ":memory:" {
            let conn = self.lock()?;
            return audit_event_connection(&conn, page_size);
        }
        let conn = Self::open_read_only_connection(self.path())?;
        audit_event_connection(&conn, page_size)
    }

    /// A non-waiting readiness probe. Busy means "not ready now" rather than
    /// making a monitoring request queue behind coordinator work.
    pub fn readiness(&self) -> Result<bool> {
        match self.conn.try_lock() {
            Ok(conn) => Ok(conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))? == 1),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Poisoned(_)) => bail!("SQLite connection lock poisoned"),
        }
    }

    pub fn graph_snapshot(&self) -> Result<Value> {
        let conn = self.lock()?;
        let mut nodes_stmt = conn.prepare(
            "SELECT id, kind, node_key, label, data_json, created_at FROM graph_nodes ORDER BY created_at, id",
        )?;
        let nodes = nodes_stmt
            .query_map([], |row| {
                let data: String = row.get(4)?;
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "kind": row.get::<_, String>(1)?,
                    "key": row.get::<_, String>(2)?,
                    "label": row.get::<_, String>(3)?,
                    "data": parse_json_column(data, 4)?,
                    "created_at": row.get::<_, String>(5)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut edges_stmt = conn.prepare(
            "SELECT id, from_node_id, to_node_id, kind, data_json, created_at
             FROM graph_edges ORDER BY created_at, id",
        )?;
        let edges = edges_stmt
            .query_map([], |row| {
                let data: String = row.get(4)?;
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "from": row.get::<_, String>(1)?,
                    "to": row.get::<_, String>(2)?,
                    "kind": row.get::<_, String>(3)?,
                    "data": parse_json_column(data, 4)?,
                    "created_at": row.get::<_, String>(5)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({ "nodes": nodes, "edges": edges }))
    }

    pub fn counts(&self) -> Result<Value> {
        let conn = self.lock()?;
        let count = |table: &str| -> Result<i64> {
            let sql = format!("SELECT COUNT(*) FROM {table}");
            Ok(conn.query_row(&sql, [], |row| row.get(0))?)
        };
        Ok(json!({
            "agents": count("agents")?,
            "intents": count("intents")?,
            "claims": count("claims")?,
            "changesets": count("changesets")?,
            "validations": count("validations")?,
            "validation_attempts": count("validation_attempts")?,
            "conflicts": count("conflicts")?,
            "conflict_detections": count("conflict_detections")?,
            "events": count("events")?,
        }))
    }
}

fn audit_event_connection(conn: &Connection, page_size: usize) -> Result<EventChainAudit> {
    let max_seq: Option<i64> =
        conn.query_row("SELECT MAX(seq) FROM events", [], |row| row.get(0))?;
    let Some(max_seq) = max_seq else {
        return Ok(EventChainAudit {
            valid: true,
            events_verified: 0,
            last_seq: None,
            head_hash: None,
        });
    };

    let mut after_seq = 0_i64;
    let mut expected_prev = "GENESIS".to_string();
    let mut events_verified = 0_usize;
    let mut last_seq = None;
    while after_seq < max_seq {
        let mut statement = conn.prepare_cached(
            "SELECT seq, event_id, schema_version, event_type, entity_type, entity_id, agent_id,
             payload_json, created_at, prev_hash, event_hash
             FROM events WHERE seq > ?1 AND seq <= ?2 ORDER BY seq LIMIT ?3",
        )?;
        let mut rows = statement.query(params![after_seq, max_seq, page_size as i64])?;
        let mut page_rows = 0_usize;
        while let Some(row) = rows.next()? {
            let seq: i64 = row.get(0)?;
            let event_id: String = row.get(1)?;
            let schema_version: i64 = row.get(2)?;
            let event_type: String = row.get(3)?;
            let entity_type: String = row.get(4)?;
            let entity_id: String = row.get(5)?;
            let agent_id: Option<String> = row.get(6)?;
            let payload_json: String = row.get(7)?;
            let created_at: String = row.get(8)?;
            let prev_hash: String = row.get(9)?;
            let event_hash: String = row.get(10)?;
            let material = format!(
                "{prev_hash}|{event_id}|{schema_version}|{event_type}|{entity_type}|{entity_id}|{}|{created_at}|{payload_json}",
                agent_id.as_deref().unwrap_or("")
            );
            let actual = format!("{:x}", Sha256::digest(material.as_bytes()));
            if prev_hash != expected_prev || actual != event_hash {
                return Ok(EventChainAudit {
                    valid: false,
                    events_verified,
                    last_seq,
                    head_hash: (events_verified > 0).then_some(expected_prev),
                });
            }
            expected_prev = event_hash;
            events_verified += 1;
            last_seq = Some(seq);
            after_seq = seq;
            page_rows += 1;
        }
        if page_rows == 0 {
            return Ok(EventChainAudit {
                valid: false,
                events_verified,
                last_seq,
                head_hash: (events_verified > 0).then_some(expected_prev),
            });
        }
    }
    Ok(EventChainAudit {
        valid: true,
        events_verified,
        last_seq,
        head_hash: Some(expected_prev),
    })
}

fn parse_json_column(value: String, index: usize) -> rusqlite::Result<Value> {
    serde_json::from_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn graph_snapshot_fails_closed_on_corrupt_projection_json() {
        let store = Store::in_memory().unwrap();
        store
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO graph_nodes(id, kind, node_key, label, data_json, created_at)
                 VALUES('node_corrupt', 'Intent', 'intent_corrupt', 'corrupt', '{', ?1)",
                [Utc::now().to_rfc3339()],
            )
            .unwrap();

        assert!(store.graph_snapshot().is_err());
    }

    #[test]
    fn event_verification_uses_each_rows_recorded_schema_version() {
        let store = Store::in_memory().unwrap();
        let event_id = "evt_legacy";
        let created_at = Utc::now().to_rfc3339();
        let payload = "{}";
        let material = format!(
            "GENESIS|{event_id}|0|legacy.recorded|Intent|int_legacy||{created_at}|{payload}"
        );
        let event_hash = format!("{:x}", Sha256::digest(material.as_bytes()));
        store
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO events(event_id, schema_version, event_type, entity_type, entity_id,
                 agent_id, payload_json, created_at, prev_hash, event_hash)
                 VALUES(?1, 0, 'legacy.recorded', 'Intent', 'int_legacy', NULL, ?2, ?3,
                 'GENESIS', ?4)",
                params![event_id, payload, created_at, event_hash],
            )
            .unwrap();

        assert!(store.verify_event_chain().unwrap());
    }

    #[test]
    fn event_verification_hashes_stored_payload_bytes_in_any_key_order() {
        let store = Store::in_memory().unwrap();
        {
            let mut conn = store.lock().unwrap();
            let tx = Store::immediate_tx(&mut conn).unwrap();
            // Insertion-ordered keys (the preserve_order serializer writes
            // them as declared, not sorted).
            Store::append_event(
                &tx,
                "test.recorded",
                "Intent",
                "int_order",
                Some("agt_test"),
                &json!({"zeta": 1, "alpha": {"nested": true}, "mid": [1, 2]}),
            )
            .unwrap();
            tx.commit().unwrap();
        }
        assert!(store.verify_event_chain().unwrap());

        // A chain segment written by a serializer that sorts keys (the 0.1.0
        // binary) must verify equally: the stored bytes are the hashed bytes
        // regardless of which release wrote them.
        let sorted_payload = r#"{"alpha":2,"zeta":1}"#;
        let created_at = Utc::now().to_rfc3339();
        let prev_hash: String = store
            .lock()
            .unwrap()
            .query_row(
                "SELECT event_hash FROM events ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let material = format!(
            "{prev_hash}|evt_sorted|{EVENT_SCHEMA_VERSION}|legacy.sorted|Intent|int_sorted|agt_test|{created_at}|{sorted_payload}"
        );
        let event_hash = format!("{:x}", Sha256::digest(material.as_bytes()));
        store
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO events(event_id, schema_version, event_type, entity_type, entity_id,
                 agent_id, payload_json, created_at, prev_hash, event_hash)
                 VALUES('evt_sorted', ?1, 'legacy.sorted', 'Intent', 'int_sorted', 'agt_test',
                 ?2, ?3, ?4, ?5)",
                params![
                    EVENT_SCHEMA_VERSION,
                    sorted_payload,
                    created_at,
                    prev_hash,
                    event_hash
                ],
            )
            .unwrap();
        assert!(store.verify_event_chain().unwrap());

        // Verification is byte-exact: an event whose hash was computed over
        // differently ordered bytes than the stored payload, even
        // semantically equal JSON, must fail.
        let mismatched_material = format!(
            "{event_hash}|evt_mismatch|{EVENT_SCHEMA_VERSION}|legacy.mismatch|Intent|int_mismatch|agt_test|{created_at}|{{\"alpha\":2,\"zeta\":1}}"
        );
        let mismatched_hash = format!("{:x}", Sha256::digest(mismatched_material.as_bytes()));
        store
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO events(event_id, schema_version, event_type, entity_type, entity_id,
                 agent_id, payload_json, created_at, prev_hash, event_hash)
                 VALUES('evt_mismatch', ?1, 'legacy.mismatch', 'Intent', 'int_mismatch',
                 'agt_test', ?2, ?3, ?4, ?5)",
                params![
                    EVENT_SCHEMA_VERSION,
                    r#"{"zeta":1,"alpha":2}"#,
                    created_at,
                    event_hash,
                    mismatched_hash
                ],
            )
            .unwrap();
        assert!(!store.verify_event_chain().unwrap());
    }

    #[test]
    fn event_chain_audit_pages_the_complete_stable_prefix() {
        let temp = TempDir::new().unwrap();
        let database = temp.path().join("foremerge").join("state.sqlite3");
        let store = Store::open(&database).unwrap();
        {
            let mut conn = store.lock().unwrap();
            let tx = Store::immediate_tx(&mut conn).unwrap();
            for index in 0..2_505 {
                Store::append_event(
                    &tx,
                    "audit.fixture",
                    "Intent",
                    &format!("int_{index}"),
                    None,
                    &json!({ "index": index }),
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        let audit = store.audit_event_chain(17).unwrap();
        assert!(audit.valid);
        assert_eq!(audit.events_verified, 2_505);
        assert_eq!(audit.last_seq, Some(2_505));
        assert!(audit.head_hash.is_some());
    }

    #[test]
    fn read_only_open_never_creates_or_migrates_a_store() {
        let temp = TempDir::new().unwrap();
        let database = temp.path().join("missing").join("state.sqlite3");
        let error = Store::open_existing_read_only(&database)
            .err()
            .expect("missing store must stay missing");
        assert!(format!("{error:#}").contains("NOT_INITIALIZED:"));
        assert!(!temp.path().join("missing").exists());

        let writable = Store::open(&database).unwrap();
        drop(writable);
        let read_only = Store::open_existing_read_only(&database).unwrap();
        assert!(read_only.audit_event_chain(1).unwrap().valid);
        assert!(
            read_only
                .lock()
                .unwrap()
                .execute("INSERT INTO meta(key, value) VALUES('write', 'denied')", [])
                .is_err()
        );
    }

    #[test]
    fn version_two_migration_backfills_scopes_attempts_and_conflict_occurrences() {
        let temp = TempDir::new().unwrap();
        let database = temp.path().join("foremerge").join("state.sqlite3");
        let store = Store::open(&database).unwrap();
        store
            .lock()
            .unwrap()
            .execute_batch(
                r#"
                INSERT INTO agents(id, name, model, capabilities_json, worktree, git_branch,
                  git_head, status, registered_at, last_seen_at)
                VALUES('agt_legacy', 'legacy', NULL, '[]', NULL, NULL, NULL, 'ACTIVE',
                  '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                INSERT INTO tasks(id, task_key, title, created_by_agent_id, created_at)
                VALUES('tsk_legacy', 'legacy-task', 'Legacy task', 'agt_legacy',
                  '2026-01-01T00:00:00Z');
                INSERT INTO intents(id, agent_id, task_id, summary, rationale, scopes_json,
                  depends_on_json, metadata_json, status, version, created_at, updated_at)
                VALUES('int_legacy', 'agt_legacy', 'tsk_legacy', 'Legacy intent', NULL,
                  '[{"kind":"symbol","key":"LegacyTarget"}]', '[]', '{}', 'VALIDATED', 1,
                  '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                INSERT INTO changesets(id, agent_id, task_id, intent_id, summary, files_json,
                  symbols_json, contracts_json, dependencies_json, tests_json, decisions_json,
                  provenance_json, fingerprint, status, created_at, updated_at)
                VALUES('cst_legacy', 'agt_legacy', 'tsk_legacy', 'int_legacy', 'Legacy candidate',
                  '[]', '[]', '[]', '[]', '[]', '[]', '{}', 'sha256:legacy', 'VALIDATED',
                  '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                INSERT INTO validations(id, changeset_id, command_json, passed, exit_code, stdout,
                  stderr, duration_ms, fingerprint, run_at)
                VALUES('val_legacy', 'cst_legacy', '["true"]', 1, 0, '', '', 1,
                  'sha256:legacy', '2026-01-01T00:00:00Z');
                INSERT INTO conflicts(id, kind, severity, score, source_intent_id,
                  target_intent_id, scope_json, scope_identity, explanation, suggestion,
                  evidence_json, status, detected_at)
                VALUES('cfl_legacy', 'overlapping_claim', 'MEDIUM', 0.5, NULL, 'int_legacy',
                  '{"kind":"symbol","key":"LegacyTarget"}', 'symbol:legacytarget',
                  'Legacy evidence', 'Coordinate', '{}', 'RESOLVED',
                  '2026-01-01T00:00:00Z');
                DROP TABLE validation_attempts;
                DROP TABLE conflict_detections;
                DROP TABLE intent_scopes;
                UPDATE meta SET value = '1' WHERE key = 'schema_version';
                "#,
            )
            .unwrap();
        drop(store);

        let migrated = Store::open(&database).unwrap();
        let conn = migrated.lock().unwrap();
        let schema: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema, DATABASE_SCHEMA_VERSION.to_string());
        let scope: String = conn
            .query_row(
                "SELECT canonical_scope FROM intent_scopes WHERE intent_id = 'int_legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(scope, "symbol:legacytarget");
        let attempt: (bool, String) = conn
            .query_row(
                "SELECT authoritative, exclusion_ruleset_digest FROM validation_attempts
                 WHERE id = 'val_legacy'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(attempt, (true, "legacy".to_string()));
        let occurrence: (String, bool) = conn
            .query_row(
                "SELECT conflict_id, previously_settled FROM conflict_detections
                 WHERE id = 'dtn_legacy_cfl_legacy'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(occurrence, ("cfl_legacy".to_string(), true));
    }

    #[cfg(unix)]
    #[test]
    fn persistent_store_is_private_and_rejects_database_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = TempDir::new().unwrap();
        let runtime = temp.path().join("foremerge");
        let database = runtime.join("state.sqlite3");
        let store = Store::open(&database).unwrap();
        assert_eq!(
            std::fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(store);

        let target = temp.path().join("target.sqlite3");
        std::fs::write(&target, []).unwrap();
        let link = temp.path().join("linked.sqlite3");
        symlink(&target, &link).unwrap();
        let error = Store::open(&link).err().expect("symlink must be rejected");
        assert!(format!("{error:#}").starts_with("INVALID_INPUT:"));
    }
}

#[cfg(test)]
mod schema_repair_tests {
    use super::*;

    /// Conflicts reference intents, which reference a task and an agent, so a
    /// fixture needs the whole chain before it can seed a detection.
    fn seed_intents(conn: &Connection) {
        conn.execute(
            "INSERT INTO agents(id, name, model, capabilities_json, worktree, git_branch,
             git_head, status, registered_at, last_seen_at)
             VALUES('agt_x', 'fixture', 'test', '[]', NULL, NULL, NULL, 'ACTIVE',
                    '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("seed agent");
        conn.execute(
            "INSERT INTO tasks(id, task_key, title, created_by_agent_id, created_at)
             VALUES('tsk_x', 'fixture', 'fixture', 'agt_x', '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("seed task");
        for intent in ["int_a", "int_b"] {
            conn.execute(
                "INSERT INTO intents(id, agent_id, task_id, summary, rationale, scopes_json,
                 depends_on_json, metadata_json, status, created_at, updated_at)
                 VALUES(?1, 'agt_x', 'tsk_x', 'fixture intent', NULL, '[]', '[]', '{}', 'INTENT',
                        '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                params![intent],
            )
            .expect("seed intent");
        }
    }

    fn seed_conflict(conn: &Connection, id: &str, detected_at: &str) {
        conn.execute(
            "INSERT INTO conflicts(id, kind, severity, score, source_intent_id, target_intent_id,
             scope_json, scope_identity, explanation, suggestion, evidence_json, status, detected_at)
             VALUES(?1, 'replace_vs_extend', 'HIGH', 0.9, 'int_a', 'int_b', NULL, ?1,
                    'shared scope', 'coordinate', '{}', 'OPEN', ?2)",
            params![id, detected_at],
        )
        .expect("seed conflict");
    }

    #[allow(clippy::too_many_arguments)]
    fn seed_detection(conn: &Connection, id: &str, conflict: &str, explanation: &str, at: &str) {
        conn.execute(
            "INSERT INTO conflict_detections(id, conflict_id, severity, score, scope_json,
             explanation, suggestion, evidence_json, previously_settled, detected_at)
             VALUES(?1, ?2, 'HIGH', 0.9, NULL, ?3, 'coordinate', '{}', 0, ?4)",
            params![id, conflict, explanation, at],
        )
        .expect("seed detection");
    }

    fn detection_ids(conn: &Connection, conflict: &str) -> Vec<String> {
        let mut statement = conn
            .prepare("SELECT id FROM conflict_detections WHERE conflict_id = ?1 ORDER BY id")
            .expect("prepare");
        statement
            .query_map([conflict], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect")
    }

    /// Schema 2 minted a byte-identical twin for a conflict that already had a
    /// native observation. Only that twin may be removed.
    #[test]
    fn repair_removes_only_byte_identical_phantoms() {
        let store = Store::in_memory().expect("open store");
        let conn = store.lock().expect("lock");
        seed_intents(&conn);
        seed_conflict(&conn, "cfl_phantom", "2026-03-01T00:00:00Z");
        seed_detection(
            &conn,
            "dtn_native_p",
            "cfl_phantom",
            "same",
            "2026-03-01T00:00:00Z",
        );
        seed_detection(
            &conn,
            "dtn_legacy_cfl_phantom",
            "cfl_phantom",
            "same",
            "2026-03-01T00:00:00Z",
        );

        seed_conflict(&conn, "cfl_genuine", "2026-01-01T00:00:00Z");
        seed_detection(
            &conn,
            "dtn_legacy_cfl_genuine",
            "cfl_genuine",
            "early observation",
            "2026-01-01T00:00:00Z",
        );
        seed_detection(
            &conn,
            "dtn_native_later",
            "cfl_genuine",
            "later observation",
            "2026-02-01T00:00:00Z",
        );

        conn.execute(
            "UPDATE meta SET value = '2' WHERE key = 'schema_version'",
            [],
        )
        .expect("pretend the store was written by schema 2");
        Store::migrate(&conn).expect("repair migration");

        assert_eq!(
            detection_ids(&conn, "cfl_phantom"),
            vec!["dtn_native_p".to_string()],
            "the identical twin must be removed"
        );
        assert_eq!(
            detection_ids(&conn, "cfl_genuine"),
            vec![
                "dtn_legacy_cfl_genuine".to_string(),
                "dtn_native_later".to_string()
            ],
            "a genuine earlier observation must survive a later redetection"
        );

        // Repeat opens must not remove anything else or reintroduce a twin.
        Store::migrate(&conn).expect("second open");
        Store::migrate(&conn).expect("third open");
        assert_eq!(detection_ids(&conn, "cfl_phantom").len(), 1);
        assert_eq!(detection_ids(&conn, "cfl_genuine").len(), 2);
    }

    /// An insert reusing an existing id is how INSERT OR REPLACE overwrites an
    /// append-only row. The guard must live in the schema, not in a
    /// per-connection pragma.
    #[test]
    fn reusing_an_immutable_id_is_rejected() {
        let store = Store::in_memory().expect("open store");
        let conn = store.lock().expect("lock");
        seed_intents(&conn);
        seed_conflict(&conn, "cfl_a", "2026-01-01T00:00:00Z");
        seed_detection(&conn, "dtn_a", "cfl_a", "original", "2026-01-01T00:00:00Z");
        let replaced = conn.execute(
            "INSERT OR REPLACE INTO conflict_detections(id, conflict_id, severity, score,
             scope_json, explanation, suggestion, evidence_json, previously_settled, detected_at)
             VALUES('dtn_a', 'cfl_a', 'HIGH', 0.9, NULL, 'tampered', 'coordinate', '{}', 0,
                    '2026-01-01T00:00:00Z')",
            [],
        );
        assert!(
            replaced.is_err(),
            "REPLACE must not overwrite an observation"
        );
        let explanation: String = conn
            .query_row(
                "SELECT explanation FROM conflict_detections WHERE id = 'dtn_a'",
                [],
                |row| row.get(0),
            )
            .expect("read back");
        assert_eq!(explanation, "original");
    }

    /// Acceptance reads `validations`, so it must be at least as hard to forge
    /// as the audit tables. Before this it was the only one of the four with no
    /// append-only guard at all.
    #[test]
    fn the_acceptance_gate_projection_is_append_only() {
        let store = Store::in_memory().expect("open store");
        let conn = store.lock().expect("lock");
        seed_intents(&conn);
        conn.execute(
            "INSERT INTO changesets(id, agent_id, task_id, intent_id, summary, files_json,
             symbols_json, contracts_json, dependencies_json, tests_json, decisions_json,
             provenance_json, worktree, git_ref, base_ref, fingerprint, status, created_at,
             updated_at)
             VALUES('chg_a', 'agt_x', 'tsk_x', 'int_a', 's', '[]', '[]', '[]', '[]', '[]', '[]',
                    '{}', NULL, NULL, NULL, 'sha256:aaa', 'PROVISIONAL',
                    '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("seed changeset");
        conn.execute(
            "INSERT INTO validations(id, changeset_id, command_json, passed, exit_code, stdout,
             stderr, duration_ms, fingerprint, run_at)
             VALUES('val_a', 'chg_a', '[\"true\"]', 0, 1, '', '', 5, 'sha256:aaa',
                    '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("seed validation");

        // Each of these is a way to turn a failing gate into a passing one.
        assert!(
            conn.execute("UPDATE validations SET passed = 1 WHERE id = 'val_a'", [])
                .is_err(),
            "a recorded validation must not be rewritten"
        );
        assert!(
            conn.execute("DELETE FROM validations WHERE id = 'val_a'", [])
                .is_err(),
            "a recorded validation must not be removed"
        );
        assert!(
            conn.execute(
                "INSERT OR REPLACE INTO validations(id, changeset_id, command_json, passed,
                 exit_code, stdout, stderr, duration_ms, fingerprint, run_at)
                 VALUES('val_a', 'chg_a', '[\"true\"]', 1, 0, '', '', 5, 'sha256:aaa',
                        '2026-01-01T00:00:00Z')",
                [],
            )
            .is_err(),
            "REPLACE must not overwrite a recorded validation"
        );
        let passed: i64 = conn
            .query_row(
                "SELECT passed FROM validations WHERE id = 'val_a'",
                [],
                |row| row.get(0),
            )
            .expect("read back");
        assert_eq!(passed, 0, "the failing result must survive every attempt");
    }

    /// Schema 2's migration was not transactional, so an interrupted upgrade
    /// could leave duplicates behind a version 1 stamp, and the schema 3 repair
    /// only swept stores stamped 2. Both that store and one already stamped 3
    /// with duplicates intact must still be repaired.
    #[test]
    fn duplicates_are_repaired_from_any_pre_schema_4_stamp() {
        for stamp in ["1", "3"] {
            let store = Store::in_memory().expect("open store");
            let conn = store.lock().expect("lock");
            seed_intents(&conn);
            seed_conflict(&conn, "cfl_dup", "2026-03-01T00:00:00Z");
            seed_detection(
                &conn,
                "dtn_native_dup",
                "cfl_dup",
                "same",
                "2026-03-01T00:00:00Z",
            );
            seed_detection(
                &conn,
                "dtn_legacy_cfl_dup",
                "cfl_dup",
                "same",
                "2026-03-01T00:00:00Z",
            );
            seed_conflict(&conn, "cfl_real", "2026-01-01T00:00:00Z");
            seed_detection(
                &conn,
                "dtn_legacy_cfl_real",
                "cfl_real",
                "early",
                "2026-01-01T00:00:00Z",
            );
            seed_detection(
                &conn,
                "dtn_native_real",
                "cfl_real",
                "later",
                "2026-02-01T00:00:00Z",
            );
            conn.execute(
                "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                [stamp],
            )
            .expect("stamp an earlier schema");

            Store::migrate(&conn).expect("repair migration");

            assert_eq!(
                detection_ids(&conn, "cfl_dup"),
                vec!["dtn_native_dup".to_string()],
                "the identical twin must be removed from a store stamped {stamp}"
            );
            assert_eq!(
                detection_ids(&conn, "cfl_real").len(),
                2,
                "a genuine earlier observation must survive in a store stamped {stamp}"
            );
        }
    }

    /// The event log has three unique keys, so a REPLACE resolving against any
    /// one of them would delete a row without firing the delete trigger on a
    /// connection that has not enabled recursive_triggers.
    #[test]
    fn events_reject_replacement_through_every_unique_key() {
        let store = Store::in_memory().expect("open store");
        let conn = store.lock().expect("lock");
        conn.execute_batch("PRAGMA recursive_triggers = OFF;")
            .expect("simulate an outside connection");
        let tx = conn.unchecked_transaction().expect("transaction");
        Store::append_event(
            &tx,
            "agent.registered",
            "Agent",
            "agt_x",
            None,
            &json!({ "original": true }),
        )
        .expect("append an event");
        tx.commit().expect("commit the event");
        let (seq, event_id, event_hash): (i64, String, String) = conn
            .query_row(
                "SELECT seq, event_id, event_hash FROM events ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read the event back");

        // Each of these collides on a different unique key.
        let collisions = [
            (
                seq.to_string(),
                "evt_other".to_string(),
                "hash_other".to_string(),
            ),
            (
                "999".to_string(),
                event_id.clone(),
                "hash_other".to_string(),
            ),
            (
                "999".to_string(),
                "evt_other".to_string(),
                event_hash.clone(),
            ),
        ];
        for (seq_value, id_value, hash_value) in collisions {
            let replaced = conn.execute(
                "INSERT OR REPLACE INTO events(seq, event_id, schema_version, event_type,
                 entity_type, entity_id, agent_id, payload_json, created_at, prev_hash, event_hash)
                 VALUES(?1, ?2, 1, 'agent.registered', 'Agent', 'agt_x', NULL,
                        '{\"tampered\":true}', '2026-01-01T00:00:00Z', 'GENESIS', ?3)",
                params![seq_value, id_value, hash_value],
            );
            assert!(
                replaced.is_err(),
                "REPLACE colliding on ({seq_value}, {id_value}, {hash_value}) must be rejected"
            );
        }
        let payload: String = conn
            .query_row(
                "SELECT payload_json FROM events WHERE seq = ?1",
                [seq],
                |row| row.get(0),
            )
            .expect("read payload");
        assert!(
            payload.contains("original"),
            "the original event must survive"
        );
        // verify_event_chain takes the store lock, so release this guard first.
        drop(conn);
        assert!(store.verify_event_chain().expect("verify"), "chain intact");
    }

    #[test]
    fn a_newer_schema_is_refused_and_left_untouched() {
        let store = Store::in_memory().expect("open store");
        let conn = store.lock().expect("lock");
        let future = DATABASE_SCHEMA_VERSION + 1;
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
            [future.to_string()],
        )
        .expect("stamp a future version");
        let error = Store::migrate(&conn).expect_err("a newer store must be refused");
        assert!(
            format!("{error:#}").contains("UNSUPPORTED_SCHEMA"),
            "unexpected error: {error:#}"
        );
        let recorded: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("read version");
        assert_eq!(
            recorded,
            future.to_string(),
            "refusing must not rewrite the stamp"
        );
    }

    #[test]
    fn a_malformed_schema_version_is_corrupt_not_zero() {
        let store = Store::in_memory().expect("open store");
        let conn = store.lock().expect("lock");
        conn.execute(
            "UPDATE meta SET value = 'banana' WHERE key = 'schema_version'",
            [],
        )
        .expect("stamp a malformed version");
        let error = Store::migrate(&conn).expect_err("a malformed version must be refused");
        assert!(
            format!("{error:#}").contains("CORRUPT_STORE"),
            "unexpected error: {error:#}"
        );
    }
}
