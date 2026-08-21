use crate::model::{Event, Scope};
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params, types::Type,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 1;

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
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
        )?;
        Ok(())
    }

    fn migrate(conn: &Connection) -> Result<()> {
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
                UNIQUE(source_intent_id, target_intent_id, kind, scope_json)
            );
            CREATE INDEX IF NOT EXISTS idx_conflicts_source ON conflicts(source_intent_id, status);
            CREATE INDEX IF NOT EXISTS idx_conflicts_target ON conflicts(target_intent_id, status);

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
            "#,
        )?;
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
        for column in ["accepted_commit", "integration_commit"] {
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
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SCHEMA_VERSION.to_string()],
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
            "{prev_hash}|{event_id}|{SCHEMA_VERSION}|{event_type}|{entity_type}|{entity_id}|{}|{created_at}|{payload_json}",
            agent_id.unwrap_or("")
        );
        let event_hash = format!("{:x}", Sha256::digest(material.as_bytes()));
        tx.execute(
            "INSERT INTO events(event_id, schema_version, event_type, entity_type, entity_id,
             agent_id, payload_json, created_at, prev_hash, event_hash)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event_id,
                SCHEMA_VERSION,
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
        let conn = self.lock()?;
        let mut statement = conn.prepare(
            "SELECT seq, event_id, schema_version, event_type, entity_type, entity_id, agent_id,
             payload_json, created_at, prev_hash, event_hash FROM events ORDER BY seq",
        )?;
        let mut rows = statement.query([])?;
        let mut expected_prev = "GENESIS".to_string();
        while let Some(row) = rows.next()? {
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
            if prev_hash != expected_prev {
                return Ok(false);
            }
            // Hash the payload bytes exactly as stored. Appending hashes the
            // serialized payload it writes, so stored bytes are always the
            // hashed bytes; re-serializing parsed JSON here would tie
            // verification to the verifying binary's map key order, and a
            // chain written by one release could falsely fail audit under
            // another.
            let material = format!(
                "{prev_hash}|{event_id}|{schema_version}|{event_type}|{entity_type}|{entity_id}|{}|{created_at}|{payload_json}",
                agent_id.as_deref().unwrap_or("")
            );
            let actual = format!("{:x}", Sha256::digest(material.as_bytes()));
            if actual != event_hash {
                return Ok(false);
            }
            expected_prev = event_hash;
        }
        Ok(true)
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
            "conflicts": count("conflicts")?,
            "events": count("events")?,
        }))
    }
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
            "{prev_hash}|evt_sorted|{SCHEMA_VERSION}|legacy.sorted|Intent|int_sorted|agt_test|{created_at}|{sorted_payload}"
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
                    SCHEMA_VERSION,
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
            "{event_hash}|evt_mismatch|{SCHEMA_VERSION}|legacy.mismatch|Intent|int_mismatch|agt_test|{created_at}|{{\"alpha\":2,\"zeta\":1}}"
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
                    SCHEMA_VERSION,
                    r#"{"zeta":1,"alpha":2}"#,
                    created_at,
                    event_hash,
                    mismatched_hash
                ],
            )
            .unwrap();
        assert!(!store.verify_event_chain().unwrap());
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
