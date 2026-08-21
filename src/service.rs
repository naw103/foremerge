use crate::conflict::{self, IntentCandidate};
use crate::db::Store;
use crate::git;
use crate::model::*;
use anyhow::{Context, Result, bail};
use chrono::{Duration as ChronoDuration, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use uuid::Uuid;

const TERMINAL_INTENT_STATES: &[&str] = &["ACCEPTED", "COMMITTED", "DISCARDED"];
const MAX_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub struct Foremerge {
    store: Store,
}

impl Foremerge {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The Git common directory this store is bound to, if any. Repository
    /// binding happens on the first repository-derived mutation, so a fresh or
    /// repository-less store returns `None`.
    pub fn repository_common_dir(&self) -> Result<Option<PathBuf>> {
        let conn = self.store.lock()?;
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'repository_common_dir'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.map(PathBuf::from))
    }

    /// The agent ids whose intents are parties to a conflict, used by less
    /// trusted surfaces to restrict resolution to the involved agents.
    pub fn conflict_party_agents(&self, conflict_id: &str) -> Result<Vec<String>> {
        let conn = self.store.lock()?;
        let conflict = conflict_by_id(&conn, conflict_id)?;
        let mut parties = Vec::new();
        if let Some(source_intent_id) = conflict.source_intent_id.as_deref() {
            parties.push(intent_by_id(&conn, source_intent_id)?.agent_id);
        }
        let target = intent_by_id(&conn, &conflict.target_intent_id)?.agent_id;
        if !parties.contains(&target) {
            parties.push(target);
        }
        Ok(parties)
    }

    pub fn bind_repository_cwd(&self, cwd: &std::path::Path) -> Result<()> {
        let Ok(repo) = git::discover(cwd) else {
            return Ok(());
        };
        let mut conn = self.store.lock()?;
        let tx = Store::immediate_tx(&mut conn)?;
        bind_repository(&tx, &repo.common_dir)?;
        tx.commit()?;
        Ok(())
    }

    pub fn register_agent(&self, request: RegisterAgentRequest) -> Result<Agent> {
        let name = request.name.trim();
        if name.is_empty() {
            bail!("INVALID_INPUT: agent name cannot be empty");
        }
        let worktree = request.worktree.as_deref().map(PathBuf::from);
        let repo = worktree
            .as_deref()
            .map(|path| {
                git::discover(path).with_context(|| {
                    format!(
                        "INVALID_INPUT: registered worktree {} must belong to a Git repository",
                        path.display()
                    )
                })
            })
            .transpose()?;
        let normalized_worktree = repo
            .as_ref()
            .map(|value| canonical_path(&value.root).to_string_lossy().into_owned());
        let now = Utc::now().to_rfc3339();
        let agent = Agent {
            id: id("agt"),
            name: name.to_string(),
            model: request.model,
            capabilities: request.capabilities,
            worktree: normalized_worktree,
            git_branch: repo.as_ref().and_then(|value| value.branch.clone()),
            git_head: repo.as_ref().and_then(|value| value.head.clone()),
            status: "ACTIVE".to_string(),
            registered_at: now.clone(),
        };

        let mut conn = self.store.lock()?;
        let tx = Store::immediate_tx(&mut conn)?;
        if let Some(repo) = &repo {
            bind_repository(&tx, &repo.common_dir)?;
        }
        tx.execute(
            "INSERT INTO agents(id, name, model, capabilities_json, worktree, git_branch, git_head,
             status, registered_at, last_seen_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                agent.id,
                agent.name,
                agent.model,
                to_json(&agent.capabilities)?,
                agent.worktree,
                agent.git_branch,
                agent.git_head,
                agent.status,
                agent.registered_at,
                now,
            ],
        )?;
        Store::upsert_node(
            &tx,
            "Agent",
            &agent.id,
            &agent.name,
            &serde_json::to_value(&agent)?,
        )?;
        Store::append_event(
            &tx,
            "agent.registered",
            "Agent",
            &agent.id,
            Some(&agent.id),
            &serde_json::to_value(&agent)?,
        )?;
        tx.commit()?;
        Ok(agent)
    }

    pub fn publish_intent(
        &self,
        mut request: PublishIntentRequest,
    ) -> Result<PublishIntentOutcome> {
        if request.task.trim().is_empty() || request.summary.trim().is_empty() {
            bail!("INVALID_INPUT: task and summary are required");
        }
        if !request.metadata.is_object() {
            bail!("INVALID_INPUT: intent metadata must be a JSON object");
        }
        request.scopes = normalize_scopes(request.scopes)?;
        let mut conn = self.store.lock()?;
        let tx = Store::immediate_tx(&mut conn)?;
        let agent = agent_by_id(&tx, &request.agent_id)?;
        let now = Utc::now().to_rfc3339();
        let task_key = digest(&request.task);
        let task_id: String = match tx
            .query_row(
                "SELECT id FROM tasks WHERE task_key = ?1",
                [&task_key],
                |row| row.get(0),
            )
            .optional()?
        {
            Some(value) => value,
            None => {
                let value = id("tsk");
                tx.execute(
                    "INSERT INTO tasks(id, task_key, title, created_by_agent_id, created_at)
                     VALUES(?1, ?2, ?3, ?4, ?5)",
                    params![value, task_key, request.task.trim(), agent.id, now],
                )?;
                value
            }
        };
        let intent = Intent {
            id: id("int"),
            agent_id: agent.id.clone(),
            task_id: task_id.clone(),
            task: request.task.trim().to_string(),
            summary: request.summary.trim().to_string(),
            rationale: request.rationale,
            scopes: request.scopes,
            depends_on: request.depends_on,
            metadata: request.metadata,
            status: "INTENT".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
            open_conflicts: None,
        };
        tx.execute(
            "INSERT INTO intents(id, agent_id, task_id, summary, rationale, scopes_json,
             depends_on_json, metadata_json, status, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                intent.id,
                intent.agent_id,
                intent.task_id,
                intent.summary,
                intent.rationale,
                to_json(&intent.scopes)?,
                to_json(&intent.depends_on)?,
                to_json(&intent.metadata)?,
                intent.status,
                intent.created_at,
                intent.updated_at,
            ],
        )?;

        let agent_node = Store::upsert_node(&tx, "Agent", &agent.id, &agent.name, &json!({}))?;
        let task_node = Store::upsert_node(
            &tx,
            "Task",
            &task_id,
            &intent.task,
            &json!({ "task": intent.task }),
        )?;
        let intent_node = Store::upsert_node(
            &tx,
            "Intent",
            &intent.id,
            &intent.summary,
            &serde_json::to_value(&intent)?,
        )?;
        Store::link_nodes(&tx, &agent_node, &task_node, "WORKS_ON", &json!({}))?;
        Store::link_nodes(&tx, &task_node, &intent_node, "HAS_INTENT", &json!({}))?;
        for dependency in &intent.depends_on {
            let dependency_node = Store::upsert_node(
                &tx,
                "Dependency",
                dependency,
                dependency,
                &json!({ "target": dependency }),
            )?;
            Store::link_nodes(
                &tx,
                &intent_node,
                &dependency_node,
                "DEPENDS_ON",
                &json!({}),
            )?;
        }

        Store::append_event(
            &tx,
            "intent.published",
            "Intent",
            &intent.id,
            Some(&agent.id),
            &serde_json::to_value(&intent)?,
        )?;

        let source = IntentCandidate {
            id: intent.id.clone(),
            summary: intent.summary.clone(),
            scopes: intent.scopes.clone(),
        };
        let candidates = active_candidates(&tx, Some(&intent.id))?;
        let mut conflicts = Vec::new();
        for candidate in candidates {
            for mut detected in conflict::detect_pair(&source, &candidate) {
                detected.source_intent_id = Some(intent.id.clone());
                conflicts.push(persist_conflict(&tx, &detected, Some(&agent.id))?);
            }
        }
        tx.commit()?;
        Ok(PublishIntentOutcome { intent, conflicts })
    }

    pub fn claim_work(&self, mut request: ClaimWorkRequest) -> Result<ClaimOutcome> {
        if request.scopes.is_empty() {
            bail!("INVALID_INPUT: claim_work requires at least one semantic scope");
        }
        request.scopes = normalize_scopes(request.scopes)?;
        let mut conn = self.store.lock()?;
        let tx = Store::immediate_tx(&mut conn)?;
        let agent = agent_by_id(&tx, &request.agent_id)?;
        let intent = intent_by_id(&tx, &request.intent_id)?;
        if intent.agent_id != agent.id {
            bail!("FORBIDDEN: an agent may only claim work for its own intent");
        }
        require_state(&intent.status, &["INTENT", "CLAIMED"], "claim work")?;
        let now = Utc::now();
        let expires = now + ChronoDuration::seconds(request.lease_seconds.clamp(60, 86_400) as i64);
        let mut claims = Vec::new();
        let mut warnings = Vec::new();
        for scope in request.scopes {
            let mut statement = tx.prepare(
                "SELECT intent_id FROM claims WHERE canonical_scope = ?1 AND status = 'ACTIVE'
                 AND lease_expires_at > ?2 AND intent_id <> ?3",
            )?;
            let existing = statement
                .query_map(
                    params![scope.canonical(), now.to_rfc3339(), intent.id],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            for other_intent in existing {
                let warning = conflict::claim_overlap_conflict(&intent.id, &other_intent, &scope);
                warnings.push(persist_conflict(&tx, &warning, Some(&agent.id))?);
            }
            let claim = Claim {
                id: id("clm"),
                agent_id: agent.id.clone(),
                intent_id: intent.id.clone(),
                scope: scope.clone(),
                status: "ACTIVE".to_string(),
                reason: request.reason.clone(),
                lease_expires_at: expires.to_rfc3339(),
                created_at: now.to_rfc3339(),
            };
            tx.execute(
                "INSERT INTO claims(id, agent_id, intent_id, scope_kind, scope_key,
                 canonical_scope, status, reason, lease_expires_at, created_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    claim.id,
                    claim.agent_id,
                    claim.intent_id,
                    claim.scope.kind,
                    claim.scope.key,
                    claim.scope.canonical(),
                    claim.status,
                    claim.reason,
                    claim.lease_expires_at,
                    claim.created_at,
                ],
            )?;
            let intent_node =
                Store::upsert_node(&tx, "Intent", &intent.id, &intent.summary, &json!({}))?;
            let claim_node = Store::upsert_node(
                &tx,
                "Claim",
                &claim.id,
                &scope.canonical(),
                &serde_json::to_value(&claim)?,
            )?;
            let scope_kind = if scope.kind == "symbol" {
                "Symbol"
            } else {
                "Scope"
            };
            let scope_node = Store::upsert_node(
                &tx,
                scope_kind,
                &scope.canonical(),
                &scope.key,
                &serde_json::to_value(&scope)?,
            )?;
            Store::link_nodes(&tx, &intent_node, &claim_node, "MAKES_CLAIM", &json!({}))?;
            Store::link_nodes(&tx, &claim_node, &scope_node, "CLAIMS", &json!({}))?;
            for dependency in &intent.depends_on {
                let dependency_node = Store::upsert_node(
                    &tx,
                    "Dependency",
                    dependency,
                    dependency,
                    &json!({ "target": dependency }),
                )?;
                Store::link_nodes(
                    &tx,
                    &scope_node,
                    &dependency_node,
                    "AFFECTS_DEPENDENCY",
                    &json!({}),
                )?;
            }
            Store::append_event(
                &tx,
                "claim.created",
                "Claim",
                &claim.id,
                Some(&agent.id),
                &serde_json::to_value(&claim)?,
            )?;
            claims.push(claim);
        }
        if intent.status == "INTENT" {
            transition_intent(&tx, &intent.id, &agent.id, "INTENT", "CLAIMED")?;
        }
        tx.commit()?;
        Ok(ClaimOutcome {
            claims,
            warnings,
            advisory_only: true,
        })
    }

    pub fn start_work(&self, agent_id: &str, intent_id: &str) -> Result<Intent> {
        let mut conn = self.store.lock()?;
        let tx = Store::immediate_tx(&mut conn)?;
        let intent = intent_by_id(&tx, intent_id)?;
        if intent.agent_id != agent_id {
            bail!("FORBIDDEN: an agent may only start its own intent");
        }
        require_state(&intent.status, &["CLAIMED"], "start work")?;
        transition_intent(&tx, intent_id, agent_id, "CLAIMED", "IN_PROGRESS")?;
        let mut started = intent_by_id(&tx, intent_id)?;
        started.open_conflicts = Some(open_conflicts_for_intent(&tx, intent_id)?);
        tx.commit()?;
        Ok(started)
    }

    pub fn query_work(&self, mut query: WorkQuery) -> Result<Vec<WorkItem>> {
        query.scope = query.scope.map(|scope| scope.normalized()).transpose()?;
        let conn = self.store.lock()?;
        let mut statement = conn.prepare(
            "SELECT i.id FROM intents i
             WHERE (?1 IS NULL OR i.agent_id = ?1)
             AND (?2 IS NULL OR i.status = ?2)
             ORDER BY i.created_at DESC",
        )?;
        let ids = statement
            .query_map(
                params![
                    query.agent_id,
                    query.status.map(|value| value.to_uppercase())
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut items = Vec::new();
        for intent_id in ids {
            let intent = intent_by_id(&conn, &intent_id)?;
            let claims = claims_for_intent(&conn, &intent_id)?;
            if let Some(scope) = &query.scope {
                let matches_intent = intent
                    .scopes
                    .iter()
                    .any(|candidate| candidate.canonical() == scope.canonical());
                let matches_claim = claims
                    .iter()
                    .any(|claim| claim.scope.canonical() == scope.canonical());
                if !matches_intent && !matches_claim {
                    continue;
                }
            }
            let latest_changeset_id: Option<String> = conn
                .query_row(
                    "SELECT id FROM changesets WHERE intent_id = ?1 AND status <> 'SUPERSEDED'
                     ORDER BY created_at DESC, id DESC LIMIT 1",
                    [&intent_id],
                    |row| row.get(0),
                )
                .optional()?;
            let latest_changeset = latest_changeset_id
                .as_deref()
                .map(|id| changeset_by_id(&conn, id))
                .transpose()?;
            let mut dependents_statement =
                conn.prepare("SELECT id, depends_on_json FROM intents")?;
            let dependent_rows = dependents_statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut dependents = Vec::new();
            for (candidate_id, dependencies) in dependent_rows {
                let dependencies: Vec<String> = serde_json::from_str(&dependencies)
                    .context("CORRUPT_STORE: invalid intent dependency JSON")?;
                if dependencies.contains(&intent_id) {
                    dependents.push(candidate_id);
                }
            }
            let open_conflicts: i64 = conn.query_row(
                "SELECT COUNT(*) FROM conflicts WHERE status IN ('OPEN', 'COORDINATING')
                 AND (source_intent_id = ?1 OR target_intent_id = ?1)",
                [&intent_id],
                |row| row.get(0),
            )?;
            items.push(WorkItem {
                agent: agent_by_id(&conn, &intent.agent_id)?,
                intent,
                claims,
                latest_changeset_id,
                latest_changeset,
                dependents,
                open_conflicts: open_conflicts as usize,
            });
            if items.len() >= query.limit.clamp(1, 500) {
                break;
            }
        }
        Ok(items)
    }

    pub fn check_conflicts(&self, mut request: ConflictCheckRequest) -> Result<ConflictReport> {
        if let Some(text) = request.intent.as_deref() {
            // An intent id passed as free-form intent text would be compared
            // as prose and silently return a false all-clear.
            if looks_like_intent_id(text.trim()) {
                bail!(
                    "INVALID_INPUT: '{}' is an intent id, not free-form intent text; pass it as intent_id (CLI: --intent-id) so the persisted intent is checked",
                    text.trim()
                );
            }
        }
        request.scopes = normalize_scopes(request.scopes)?;
        if let Some(intent_id) = request.intent_id.as_deref() {
            if request.scopes.is_empty() {
                let conn = self.store.lock()?;
                intent_by_id(&conn, intent_id)?;
                let checked_intents = active_candidates(&conn, Some(intent_id))?.len();
                let mut statement = conn.prepare(
                    "SELECT id FROM conflicts
                     WHERE status IN ('OPEN', 'COORDINATING')
                     AND (source_intent_id = ?1 OR target_intent_id = ?1)
                     ORDER BY detected_at DESC, id",
                )?;
                let ids = statement
                    .query_map([intent_id], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(statement);
                let conflicts = ids
                    .iter()
                    .map(|id| conflict_by_id(&conn, id))
                    .collect::<Result<Vec<_>>>()?;
                let blocking = conflicts.iter().any(|value| value.severity == "HIGH");
                return Ok(ConflictReport {
                    conflicts,
                    checked_intents,
                    blocking,
                    policy: "Warnings are explainable and advisory during intent/claim; unresolved HIGH conflicts gate acceptance."
                        .to_string(),
                });
            }
        }
        let conn = self.store.lock()?;
        let source = if let Some(intent_id) = request.intent_id.as_deref() {
            let intent = intent_by_id(&conn, intent_id)?;
            IntentCandidate {
                id: intent.id,
                summary: intent.summary,
                scopes: if request.scopes.is_empty() {
                    intent.scopes
                } else {
                    request.scopes
                },
            }
        } else {
            IntentCandidate {
                id: format!("adhoc_{}", Uuid::new_v4().simple()),
                summary: request.intent.ok_or_else(|| {
                    anyhow::anyhow!("INVALID_INPUT: provide intent_id or intent text")
                })?,
                scopes: request.scopes,
            }
        };
        let candidates = active_candidates(&conn, Some(&source.id))?;
        let checked_intents = candidates.len();
        let conflicts = candidates
            .into_iter()
            .flat_map(|candidate| conflict::detect_pair(&source, &candidate))
            .map(|mut finding| {
                finding.id = id("eph");
                if let Some(evidence) = finding.evidence.as_object_mut() {
                    evidence.insert("ephemeral".to_string(), Value::Bool(true));
                }
                finding
            })
            .collect::<Vec<_>>();
        let blocking = conflicts.iter().any(|value| value.severity == "HIGH");
        Ok(ConflictReport {
            conflicts,
            checked_intents,
            blocking,
            policy: "Warnings are explainable and advisory during intent/claim; unresolved HIGH conflicts gate acceptance."
                .to_string(),
        })
    }

    pub fn publish_changeset(&self, mut request: PublishChangeSetRequest) -> Result<ChangeSet> {
        if request.summary.trim().is_empty() {
            bail!("INVALID_INPUT: changeset summary is required");
        }
        if !request.provenance.is_object() {
            bail!("INVALID_INPUT: ChangeSet provenance must be a JSON object");
        }
        let agent;
        let intent;
        {
            let conn = self.store.lock()?;
            agent = agent_by_id(&conn, &request.agent_id)?;
            intent = intent_by_id(&conn, &request.intent_id)?;
        }
        if intent.agent_id != agent.id {
            bail!("FORBIDDEN: an agent may only publish a ChangeSet for its own intent");
        }
        require_state(
            &intent.status,
            &["CLAIMED", "IN_PROGRESS", "PROVISIONAL", "VALIDATED"],
            "publish a ChangeSet revision",
        )?;
        let mut worktree = request
            .worktree
            .clone()
            .or_else(|| agent.worktree.clone())
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?);
        let snapshot = git::snapshot(&worktree).with_context(|| {
            format!(
                "snapshot worktree {}; ChangeSets require a Git worktree",
                worktree.display()
            )
        })?;
        worktree = canonical_path(&snapshot.root);
        if let Some(registered_worktree) = agent.worktree.as_deref() {
            let registered = git::discover(registered_worktree)?;
            if canonical_path(&registered.common_dir) != canonical_path(&snapshot.common_dir) {
                bail!(
                    "INVALID_INPUT: ChangeSet worktree must belong to the agent's registered Git repository"
                );
            }
        }
        if request.files.is_empty() {
            request.files = snapshot.changed_files.clone();
        }
        if request.symbols.is_empty() {
            request.symbols = git::infer_symbols(&worktree).unwrap_or_default();
        }
        // Resolve the candidate commit and its true diff base. Recording the
        // candidate itself as base (the pre-0.2.0 behavior for `--git-ref
        // HEAD` on a clean worktree) produced a self-referential base and the
        // hash of an empty diff, which is vacuous provenance.
        let candidate = match request.git_ref.as_deref() {
            Some(reference) => Some(git::verify_ref(&worktree, reference)?),
            None => snapshot.head.clone(),
        };
        let (base_ref, base_resolution, diff_hash) = match (
            candidate.as_deref(),
            request.base_ref.as_deref(),
        ) {
            (None, Some(_)) => bail!(
                "INVALID_INPUT: base_ref requires a candidate commit, and this worktree has no commits yet"
            ),
            (None, None) => (None, "unborn_worktree", snapshot.diff_hash.clone()),
            (Some(commit), Some(base)) => {
                let base = git::verify_ref(&worktree, base)?;
                if base == commit {
                    bail!(
                        "INVALID_INPUT: base_ref must not resolve to the candidate commit itself; the base is the commit the candidate is diffed against (the candidate's first parent is used when base_ref is omitted)"
                    );
                }
                let hash = git::diff_patch_hash(&worktree, &base, commit)?;
                (Some(base), "caller_supplied", hash)
            }
            (Some(commit), None) => match git::first_parent(&worktree, commit)? {
                Some(parent) => {
                    let hash = git::diff_patch_hash(&worktree, &parent, commit)?;
                    (Some(parent), "first_parent", hash)
                }
                None => {
                    let empty_tree = git::empty_tree_id(&worktree)?;
                    let hash = git::diff_patch_hash(&worktree, &empty_tree, commit)?;
                    (None, "root_commit", hash)
                }
            },
        };
        let now = Utc::now().to_rfc3339();
        let provenance = json!({
            "agent": {
                "id": agent.id,
                "name": agent.name,
                "model": agent.model,
                "capabilities": agent.capabilities,
            },
            "task": {
                "id": intent.task_id,
                "title": intent.task,
            },
            "intent": {
                "id": intent.id,
                "summary": intent.summary,
                "rationale": intent.rationale,
                "scopes": intent.scopes,
                "metadata": intent.metadata,
            },
            "git": {
                "worktree": worktree,
                "branch": snapshot.branch,
                "head": snapshot.head,
                "tree": snapshot.tree,
                "candidate": candidate,
                "base_ref": base_ref,
                "base_resolution": base_resolution,
                "diff_hash": diff_hash,
                "dirty": snapshot.dirty,
            },
            "declared": request.provenance,
            "captured_at": now,
        });
        let mut changeset = ChangeSet {
            id: id("chg"),
            agent_id: agent.id.clone(),
            task_id: intent.task_id.clone(),
            intent_id: intent.id.clone(),
            summary: request.summary.trim().to_string(),
            files: request.files,
            symbols: request.symbols,
            contracts: request.contracts,
            dependencies: request.dependencies,
            tests: request.tests,
            decisions: request.decisions,
            provenance,
            base_ref,
            git_ref: request.git_ref.or(snapshot.head.clone()),
            accepted_commit: None,
            integration_commit: None,
            supersedes_changeset_id: None,
            fingerprint: snapshot.fingerprint,
            status: "PROVISIONAL".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
            open_conflicts: None,
        };

        let mut conn = self.store.lock()?;
        let tx = Store::immediate_tx(&mut conn)?;
        bind_repository(&tx, &snapshot.common_dir)?;
        let fresh_intent = intent_by_id(&tx, &intent.id)?;
        require_state(
            &fresh_intent.status,
            &["CLAIMED", "IN_PROGRESS", "PROVISIONAL", "VALIDATED"],
            "publish a ChangeSet revision",
        )?;
        if fresh_intent.status == "CLAIMED" {
            transition_intent(&tx, &intent.id, &agent.id, "CLAIMED", "IN_PROGRESS")?;
        }
        changeset.supersedes_changeset_id = tx
            .query_row(
                "SELECT id FROM changesets WHERE intent_id = ?1 AND status <> 'SUPERSEDED'
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                [&intent.id],
                |row| row.get(0),
            )
            .optional()?;
        if matches!(fresh_intent.status.as_str(), "PROVISIONAL" | "VALIDATED") {
            let previous = changeset
                .supersedes_changeset_id
                .as_deref()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "STATE_RACE: intent {} has no current ChangeSet to supersede",
                        intent.id
                    )
                })?;
            let changed = tx.execute(
                "UPDATE changesets SET status = 'SUPERSEDED', updated_at = ?2
                 WHERE id = ?1 AND status IN ('PROVISIONAL', 'VALIDATED')",
                params![previous, now],
            )?;
            if changed != 1 {
                bail!("STATE_RACE: current ChangeSet changed before revision publication");
            }
            refresh_changeset_node(&tx, previous)?;
        }
        tx.execute(
            "INSERT INTO changesets(id, agent_id, task_id, intent_id, summary, files_json,
             symbols_json, contracts_json, dependencies_json, tests_json, decisions_json,
             provenance_json, base_ref, git_ref, supersedes_changeset_id, worktree, fingerprint,
             status, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
             ?16, ?17, ?18, ?19, ?20)",
            params![
                changeset.id,
                changeset.agent_id,
                changeset.task_id,
                changeset.intent_id,
                changeset.summary,
                to_json(&changeset.files)?,
                to_json(&changeset.symbols)?,
                to_json(&changeset.contracts)?,
                to_json(&changeset.dependencies)?,
                to_json(&changeset.tests)?,
                to_json(&changeset.decisions)?,
                to_json(&changeset.provenance)?,
                changeset.base_ref,
                changeset.git_ref,
                changeset.supersedes_changeset_id,
                worktree.to_string_lossy(),
                changeset.fingerprint,
                changeset.status,
                changeset.created_at,
                changeset.updated_at,
            ],
        )?;
        let intent_node =
            Store::upsert_node(&tx, "Intent", &intent.id, &intent.summary, &json!({}))?;
        let changeset_node = Store::upsert_node(
            &tx,
            "ChangeSet",
            &changeset.id,
            &changeset.summary,
            &serde_json::to_value(&changeset)?,
        )?;
        Store::link_nodes(&tx, &intent_node, &changeset_node, "PRODUCES", &json!({}))?;
        let provenance_node = Store::upsert_node(
            &tx,
            "Provenance",
            &format!("{}:provenance", changeset.id),
            "ChangeSet provenance",
            &changeset.provenance,
        )?;
        Store::link_nodes(
            &tx,
            &changeset_node,
            &provenance_node,
            "HAS_PROVENANCE",
            &json!({}),
        )?;
        for (index, test) in changeset.tests.iter().enumerate() {
            let test_key = format!("{}:reported-test:{index}", changeset.id);
            let test_node = Store::upsert_node(
                &tx,
                "Test",
                &test_key,
                &test.command,
                &serde_json::to_value(test)?,
            )?;
            let result_node = Store::upsert_node(
                &tx,
                "Result",
                &format!("{test_key}:result"),
                &test.status,
                &serde_json::to_value(test)?,
            )?;
            Store::link_nodes(&tx, &changeset_node, &test_node, "REPORTS_TEST", &json!({}))?;
            Store::link_nodes(&tx, &test_node, &result_node, "HAS_RESULT", &json!({}))?;
        }
        for (index, decision) in changeset.decisions.iter().enumerate() {
            let decision_id = id("dec");
            tx.execute(
                "INSERT INTO decisions(id, changeset_id, intent_id, title, rationale,
                 alternatives_json, provenance_json, created_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    decision_id,
                    changeset.id,
                    changeset.intent_id,
                    decision.title,
                    decision.rationale,
                    to_json(&decision.alternatives)?,
                    to_json(&changeset.provenance)?,
                    now,
                ],
            )?;
            let decision_node = Store::upsert_node(
                &tx,
                "Decision",
                &format!("{}:{index}", changeset.id),
                &decision.title,
                &serde_json::to_value(decision)?,
            )?;
            Store::link_nodes(
                &tx,
                &changeset_node,
                &decision_node,
                "RECORDS_DECISION",
                &json!({}),
            )?;
        }
        match fresh_intent.status.as_str() {
            "CLAIMED" | "IN_PROGRESS" => {
                transition_intent(&tx, &intent.id, &agent.id, "IN_PROGRESS", "PROVISIONAL")?
            }
            "PROVISIONAL" | "VALIDATED" => {
                tx.execute(
                    "UPDATE intents SET status = 'PROVISIONAL', version = version + 1, updated_at = ?2 WHERE id = ?1",
                    params![intent.id, now],
                )?;
                Store::append_event(
                    &tx,
                    "changeset.revised",
                    "ChangeSet",
                    &changeset.id,
                    Some(&agent.id),
                    &json!({
                        "supersedes": changeset.supersedes_changeset_id,
                        "invalidated_state": fresh_intent.status,
                        "new_state": "PROVISIONAL"
                    }),
                )?;
                refresh_intent_node(&tx, &intent.id)?;
            }
            _ => unreachable!("state checked above"),
        }
        Store::append_event(
            &tx,
            "changeset.published",
            "ChangeSet",
            &changeset.id,
            Some(&agent.id),
            &serde_json::to_value(&changeset)?,
        )?;
        // Populate the conflict snapshot after the stored record and event
        // were serialized: it is response-only visibility for the publisher,
        // covering conflicts other agents created since this intent published.
        changeset.open_conflicts = Some(open_conflicts_for_intent(&tx, &intent.id)?);
        tx.commit()?;
        Ok(changeset)
    }

    pub async fn validate_changeset(
        &self,
        changeset_id: &str,
        request: ValidationRequest,
    ) -> Result<Validation> {
        if request.command.is_empty() || request.command[0].trim().is_empty() {
            bail!("INVALID_INPUT: validation command must be a non-empty argv array");
        }
        let changeset;
        let recorded_worktree;
        {
            let conn = self.store.lock()?;
            changeset = changeset_by_id(&conn, changeset_id)?;
            recorded_worktree = conn.query_row(
                "SELECT worktree FROM changesets WHERE id = ?1",
                [changeset_id],
                |row| row.get::<_, Option<String>>(0),
            )?;
        }
        require_state(&changeset.status, &["PROVISIONAL", "VALIDATED"], "validate")?;
        let worktree = request
            .worktree
            .or_else(|| recorded_worktree.clone())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("INVALID_INPUT: validation requires a worktree"))?;
        let current = git::snapshot(&worktree)?;
        if let Some(recorded_worktree) = recorded_worktree.as_deref() {
            let recorded_repo = git::discover(recorded_worktree)?;
            if canonical_path(&recorded_repo.common_dir) != canonical_path(&current.common_dir) {
                bail!(
                    "INVALID_INPUT: validation worktree must belong to the ChangeSet's Git repository"
                );
            }
        }
        {
            let conn = self.store.lock()?;
            ensure_repository_matches(&conn, &current.common_dir)?;
        }
        if current.fingerprint != changeset.fingerprint {
            bail!(
                "STALE_CHANGESET: worktree fingerprint changed after ChangeSet publication; publish a new ChangeSet"
            );
        }

        let started = Instant::now();
        let mut command = Command::new(&request.command[0]);
        command
            .args(&request.command[1..])
            .current_dir(&worktree)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let timeout = Duration::from_secs(request.timeout_seconds.clamp(1, 3600));
        let (passed, exit_code, stdout, stderr) = match command.spawn() {
            Ok(mut child) => {
                #[cfg(unix)]
                let child_pid = child.id();
                let child_stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("capture validation stdout"))?;
                let child_stderr = child
                    .stderr
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("capture validation stderr"))?;
                let execution = async {
                    let (stdout, stderr, status) = tokio::try_join!(
                        read_bounded_tail(child_stdout),
                        read_bounded_tail(child_stderr),
                        child.wait()
                    )?;
                    Ok::<_, std::io::Error>((stdout, stderr, status))
                };
                match tokio::time::timeout(timeout, execution).await {
                    Ok(Ok((stdout, stderr, status))) => (
                        status.success(),
                        status.code(),
                        String::from_utf8_lossy(&stdout).into_owned(),
                        String::from_utf8_lossy(&stderr).into_owned(),
                    ),
                    Ok(Err(error)) => (
                        false,
                        None,
                        String::new(),
                        format!("failed while running: {error}"),
                    ),
                    Err(_) => {
                        #[cfg(unix)]
                        if let Some(pid) = child_pid {
                            let _ = Command::new("/bin/kill")
                                .args(["-KILL", "--", &format!("-{pid}")])
                                .status()
                                .await;
                        }
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        (
                            false,
                            None,
                            String::new(),
                            format!(
                                "timed out after {} seconds",
                                request.timeout_seconds.clamp(1, 3600)
                            ),
                        )
                    }
                }
            }
            Err(error) => (
                false,
                None,
                String::new(),
                format!("failed to run: {error}"),
            ),
        };
        let validation = Validation {
            id: id("val"),
            changeset_id: changeset.id.clone(),
            command: request.command,
            passed,
            exit_code,
            stdout,
            stderr,
            duration_ms: started.elapsed().as_millis(),
            fingerprint: changeset.fingerprint.clone(),
            run_at: Utc::now().to_rfc3339(),
        };
        let mut conn = self.store.lock()?;
        let tx = Store::immediate_tx(&mut conn)?;
        let fresh_changeset = changeset_by_id(&tx, changeset_id)?;
        let fresh_intent = intent_by_id(&tx, &changeset.intent_id)?;
        let latest_changeset_id: Option<String> = tx
            .query_row(
                "SELECT id FROM changesets WHERE intent_id = ?1 AND status <> 'SUPERSEDED'
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                [&changeset.intent_id],
                |row| row.get(0),
            )
            .optional()?;
        let expected_intent_state = match fresh_changeset.status.as_str() {
            "PROVISIONAL" => Some("PROVISIONAL"),
            "VALIDATED" => Some("VALIDATED"),
            _ => None,
        };
        let state_race = if latest_changeset_id.as_deref() != Some(changeset_id) {
            Some("a newer ChangeSet revision is current")
        } else if expected_intent_state.is_none() {
            Some("the ChangeSet left the validation-eligible state")
        } else if expected_intent_state != Some(fresh_intent.status.as_str()) {
            Some("the Intent and ChangeSet lifecycle states no longer agree")
        } else {
            None
        };
        if let Some(reason) = state_race {
            Store::append_event(
                &tx,
                "validation.stale",
                "Result",
                &validation.id,
                Some(&changeset.agent_id),
                &json!({
                    "changeset_id": changeset.id,
                    "passed": validation.passed,
                    "fingerprint": validation.fingerprint,
                    "reason": reason,
                    "stdout_sha256": digest(&validation.stdout),
                    "stderr_sha256": digest(&validation.stderr),
                }),
            )?;
            tx.commit()?;
            bail!("STATE_RACE: validation result was not applied because {reason}");
        }
        let final_snapshot = git::snapshot(&worktree)?;
        if final_snapshot.fingerprint != changeset.fingerprint {
            Store::append_event(
                &tx,
                "validation.stale",
                "Result",
                &validation.id,
                Some(&changeset.agent_id),
                &json!({
                    "changeset_id": changeset.id,
                    "passed": validation.passed,
                    "fingerprint": validation.fingerprint,
                    "reason": "worktree fingerprint changed while validation ran",
                    "stdout_sha256": digest(&validation.stdout),
                    "stderr_sha256": digest(&validation.stderr),
                }),
            )?;
            tx.commit()?;
            bail!(
                "STALE_CHANGESET: worktree fingerprint changed while validation ran; publish a new ChangeSet"
            );
        }
        tx.execute(
            "INSERT INTO validations(id, changeset_id, command_json, passed, exit_code, stdout,
             stderr, duration_ms, fingerprint, run_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                validation.id,
                validation.changeset_id,
                to_json(&validation.command)?,
                validation.passed,
                validation.exit_code,
                validation.stdout,
                validation.stderr,
                validation.duration_ms as i64,
                validation.fingerprint,
                validation.run_at,
            ],
        )?;
        let test_node = Store::upsert_node(
            &tx,
            "Test",
            &format!("{}:validation", validation.id),
            &validation.command.join(" "),
            &json!({ "foremerge_executed": true, "argv": validation.command }),
        )?;
        let result_node = Store::upsert_node(
            &tx,
            "Result",
            &validation.id,
            if validation.passed { "PASS" } else { "FAIL" },
            &serde_json::to_value(&validation)?,
        )?;
        let changeset_node = Store::upsert_node(
            &tx,
            "ChangeSet",
            &changeset.id,
            &changeset.summary,
            &json!({}),
        )?;
        Store::link_nodes(&tx, &changeset_node, &test_node, "RUNS_TEST", &json!({}))?;
        Store::link_nodes(&tx, &test_node, &result_node, "HAS_RESULT", &json!({}))?;
        if validation.passed {
            let changed = tx.execute(
                "UPDATE changesets SET status = 'VALIDATED', updated_at = ?2
                 WHERE id = ?1 AND status IN ('PROVISIONAL', 'VALIDATED')",
                params![changeset.id, validation.run_at],
            )?;
            if changed != 1 {
                bail!("STATE_RACE: ChangeSet changed while applying validation");
            }
            if fresh_intent.status == "PROVISIONAL" {
                transition_intent(
                    &tx,
                    &fresh_intent.id,
                    &changeset.agent_id,
                    "PROVISIONAL",
                    "VALIDATED",
                )?;
            }
        } else {
            let changed = tx.execute(
                "UPDATE changesets SET status = 'PROVISIONAL', updated_at = ?2
                 WHERE id = ?1 AND status IN ('PROVISIONAL', 'VALIDATED')",
                params![changeset.id, validation.run_at],
            )?;
            if changed != 1 {
                bail!("STATE_RACE: ChangeSet changed while applying validation");
            }
            if fresh_intent.status == "VALIDATED" {
                tx.execute(
                    "UPDATE intents SET status = 'PROVISIONAL', version = version + 1,
                     updated_at = ?2 WHERE id = ?1",
                    params![fresh_intent.id, validation.run_at],
                )?;
                Store::append_event(
                    &tx,
                    "validation.invalidated",
                    "Intent",
                    &fresh_intent.id,
                    Some(&changeset.agent_id),
                    &json!({ "from": "VALIDATED", "to": "PROVISIONAL", "validation_id": validation.id }),
                )?;
                refresh_intent_node(&tx, &fresh_intent.id)?;
            }
        }
        refresh_changeset_node(&tx, &changeset.id)?;
        Store::append_event(
            &tx,
            "validation.completed",
            "Result",
            &validation.id,
            Some(&changeset.agent_id),
            &json!({
                "changeset_id": changeset.id,
                "passed": validation.passed,
                "exit_code": validation.exit_code,
                "duration_ms": validation.duration_ms,
                "fingerprint": validation.fingerprint,
                "stdout_sha256": digest(&validation.stdout),
                "stderr_sha256": digest(&validation.stderr),
            }),
        )?;
        tx.commit()?;
        Ok(validation)
    }

    pub fn accept_changeset(
        &self,
        changeset_id: &str,
        request: AcceptRequest,
    ) -> Result<ChangeSet> {
        let changeset;
        let worktree;
        let dependency_refs;
        {
            let conn = self.store.lock()?;
            changeset = changeset_by_id(&conn, changeset_id)?;
            require_state(&changeset.status, &["VALIDATED"], "accept")?;
            worktree = conn
                .query_row(
                    "SELECT worktree FROM changesets WHERE id = ?1",
                    [changeset_id],
                    |row| row.get::<_, Option<String>>(0),
                )?
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("INVALID_INPUT: ChangeSet has no worktree"))?;
            let valid: Option<(bool, String)> = conn
                .query_row(
                    "SELECT passed, fingerprint FROM validations WHERE changeset_id = ?1
                     ORDER BY run_at DESC LIMIT 1",
                    [changeset_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            match valid {
                Some((true, fingerprint)) if fingerprint == changeset.fingerprint => {}
                _ => {
                    bail!(
                        "CHECK_FAILED: no passing Foremerge validation for the current fingerprint"
                    )
                }
            }
            let high: i64 = conn.query_row(
                "SELECT COUNT(*) FROM conflicts WHERE severity = 'HIGH'
                 AND status IN ('OPEN', 'COORDINATING')
                 AND (source_intent_id = ?1 OR target_intent_id = ?1)",
                [&changeset.intent_id],
                |row| row.get(0),
            )?;
            if high > 0 && !request.allow_high_conflicts {
                bail!(
                    "BLOCKING_CONFLICT: {high} unresolved HIGH intent conflict(s); coordinate and resolve them before acceptance"
                );
            }
            if high > 0
                && request
                    .override_reason
                    .as_deref()
                    .is_none_or(|reason| reason.trim().is_empty())
            {
                bail!(
                    "INVALID_INPUT: overriding unresolved HIGH conflicts requires an explicit operator override with a recorded reason"
                );
            }
            let mut resolved_dependencies = Vec::new();
            for dependency in &changeset.dependencies {
                let dependency_record: Option<(String, Option<String>, Option<String>)> = conn
                    .query_row(
                        "SELECT i.status, c.id, c.accepted_commit FROM intents i
                         LEFT JOIN changesets c ON c.id = (
                           SELECT id FROM changesets WHERE intent_id = i.id
                           AND status IN ('ACCEPTED', 'COMMITTED') ORDER BY created_at DESC LIMIT 1
                         ) WHERE i.id = ?1",
                        [dependency],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                let Some((state, dependency_changeset_id, accepted_commit)) = dependency_record
                else {
                    bail!("UNSATISFIED_DEPENDENCY: intent {dependency} does not exist");
                };
                if !matches!(state.as_str(), "ACCEPTED" | "COMMITTED") {
                    bail!(
                        "UNSATISFIED_DEPENDENCY: intent {dependency} is not accepted or committed"
                    );
                }
                let dependency_changeset_id = dependency_changeset_id.ok_or_else(|| {
                    anyhow::anyhow!(
                        "UNSATISFIED_DEPENDENCY: intent {dependency} has no accepted ChangeSet"
                    )
                })?;
                let accepted_commit = accepted_commit.ok_or_else(|| {
                    anyhow::anyhow!(
                        "UNSATISFIED_DEPENDENCY: intent {dependency} has no pinned accepted commit"
                    )
                })?;
                let dependency_ref_name =
                    format!("refs/foremerge/accepted/{dependency_changeset_id}");
                let dependency_ref =
                    git::verify_ref(&worktree, &dependency_ref_name).map_err(|_| {
                        anyhow::anyhow!(
                            "UNSATISFIED_DEPENDENCY: intent {dependency} has no accepted Git ref"
                        )
                    })?;
                if dependency_ref != accepted_commit {
                    bail!(
                        "UNSATISFIED_DEPENDENCY: accepted ref for {dependency} no longer matches its pinned commit"
                    );
                }
                resolved_dependencies.push((dependency.clone(), accepted_commit));
            }
            dependency_refs = resolved_dependencies;
        }
        git::ensure_clean(&worktree)?;
        let current = git::snapshot(&worktree)?;
        if current.fingerprint != changeset.fingerprint {
            bail!("STALE_CHANGESET: worktree changed since validation");
        }
        let candidate_ref = request
            .git_ref
            .as_deref()
            .or(changeset.git_ref.as_deref())
            .or(current.head.as_deref())
            .ok_or_else(|| anyhow::anyhow!("INVALID_INPUT: acceptance requires a Git commit"))?;
        let commit = git::verify_ref(&worktree, candidate_ref)?;
        if current.head.as_deref() != Some(commit.as_str()) {
            bail!(
                "STALE_CHANGESET: accepted git_ref must resolve to the currently validated worktree HEAD"
            );
        }
        for (dependency, accepted_commit) in &dependency_refs {
            if !git::is_ancestor(&worktree, accepted_commit, &commit)? {
                bail!(
                    "UNSATISFIED_DEPENDENCY: accepted ref for {dependency} is not in the candidate ancestry"
                );
            }
        }

        let mut conn = self.store.lock()?;
        let tx = Store::immediate_tx(&mut conn)?;
        let fresh = changeset_by_id(&tx, changeset_id)?;
        require_state(&fresh.status, &["VALIDATED"], "accept")?;
        let fresh_intent = intent_by_id(&tx, &fresh.intent_id)?;
        require_state(&fresh_intent.status, &["VALIDATED"], "accept")?;
        let valid: Option<(bool, String)> = tx
            .query_row(
                "SELECT passed, fingerprint FROM validations WHERE changeset_id = ?1
                 ORDER BY run_at DESC LIMIT 1",
                [changeset_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match valid {
            Some((true, fingerprint)) if fingerprint == fresh.fingerprint => {}
            _ => bail!("CHECK_FAILED: no passing Foremerge validation for the current fingerprint"),
        }
        let high: i64 = tx.query_row(
            "SELECT COUNT(*) FROM conflicts WHERE severity = 'HIGH'
             AND status IN ('OPEN', 'COORDINATING')
             AND (source_intent_id = ?1 OR target_intent_id = ?1)",
            [&fresh.intent_id],
            |row| row.get(0),
        )?;
        if high > 0 && !request.allow_high_conflicts {
            bail!(
                "BLOCKING_CONFLICT: {high} unresolved HIGH intent conflict(s); coordinate and resolve them before acceptance"
            );
        }
        if high > 0
            && request
                .override_reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
        {
            bail!(
                "INVALID_INPUT: overriding unresolved HIGH conflicts requires an explicit operator override with a recorded reason"
            );
        }
        for dependency in &fresh.dependencies {
            let dependency_record: Option<(String, Option<String>, Option<String>)> = tx
                .query_row(
                    "SELECT i.status, c.id, c.accepted_commit FROM intents i
                     LEFT JOIN changesets c ON c.id = (
                       SELECT id FROM changesets WHERE intent_id = i.id
                       AND status IN ('ACCEPTED', 'COMMITTED')
                       ORDER BY created_at DESC, id DESC LIMIT 1
                     ) WHERE i.id = ?1",
                    [dependency],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((state, dependency_changeset_id, accepted_commit)) = dependency_record else {
                bail!("UNSATISFIED_DEPENDENCY: intent {dependency} does not exist");
            };
            if !matches!(state.as_str(), "ACCEPTED" | "COMMITTED") {
                bail!("UNSATISFIED_DEPENDENCY: intent {dependency} is not accepted or committed");
            }
            let dependency_changeset_id = dependency_changeset_id.ok_or_else(|| {
                anyhow::anyhow!(
                    "UNSATISFIED_DEPENDENCY: intent {dependency} has no accepted ChangeSet"
                )
            })?;
            let accepted_commit = accepted_commit.ok_or_else(|| {
                anyhow::anyhow!(
                    "UNSATISFIED_DEPENDENCY: intent {dependency} has no pinned accepted commit"
                )
            })?;
            let dependency_ref_name = format!("refs/foremerge/accepted/{dependency_changeset_id}");
            let dependency_ref =
                git::verify_ref(&worktree, &dependency_ref_name).map_err(|_| {
                    anyhow::anyhow!(
                        "UNSATISFIED_DEPENDENCY: intent {dependency} has no accepted Git ref"
                    )
                })?;
            if dependency_ref != accepted_commit {
                bail!(
                    "UNSATISFIED_DEPENDENCY: accepted ref for {dependency} no longer matches its pinned commit"
                );
            }
            if !git::is_ancestor(&worktree, &accepted_commit, &commit)? {
                bail!(
                    "UNSATISFIED_DEPENDENCY: accepted ref for {dependency} is not in the candidate ancestry"
                );
            }
        }
        let now = Utc::now().to_rfc3339();
        if high > 0 && request.allow_high_conflicts {
            if let Some(reason) = request.override_reason.as_deref() {
                let decision_id = id("dec");
                tx.execute(
                    "INSERT INTO decisions(id, changeset_id, intent_id, title, rationale,
                     alternatives_json, provenance_json, created_at)
                     VALUES(?1, ?2, ?3, 'Override unresolved HIGH conflicts', ?4, '[]', ?5, ?6)",
                    params![
                        decision_id,
                        changeset_id,
                        changeset.intent_id,
                        reason,
                        to_json(
                            &json!({ "agent_id": changeset.agent_id, "explicit_override": true })
                        )?,
                        now,
                    ],
                )?;
                tx.execute(
                    "UPDATE conflicts SET status = 'OVERRIDDEN' WHERE severity = 'HIGH'
                     AND status IN ('OPEN', 'COORDINATING')
                     AND (source_intent_id = ?1 OR target_intent_id = ?1)",
                    [&changeset.intent_id],
                )?;
                let mut overridden_statement = tx.prepare(
                    "SELECT id FROM conflicts WHERE severity = 'HIGH' AND status = 'OVERRIDDEN'
                     AND (source_intent_id = ?1 OR target_intent_id = ?1)",
                )?;
                let overridden_ids = overridden_statement
                    .query_map([&changeset.intent_id], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(overridden_statement);
                for conflict_id in overridden_ids {
                    refresh_conflict_node(&tx, &conflict_id)?;
                }
                Store::append_event(
                    &tx,
                    "conflict.overridden",
                    "Intent",
                    &changeset.intent_id,
                    Some(&changeset.agent_id),
                    &json!({ "reason": reason, "decision_id": decision_id }),
                )?;
            }
        }
        let accepted_ref = git::create_accepted_ref(&worktree, changeset_id, &commit)?;
        let changed = tx.execute(
            "UPDATE changesets SET status = 'ACCEPTED', git_ref = ?2, accepted_commit = ?2,
             integration_commit = NULL, updated_at = ?3
             WHERE id = ?1 AND status = 'VALIDATED'",
            params![changeset_id, commit, now],
        )?;
        if changed != 1 {
            bail!("STATE_RACE: ChangeSet changed while accepting it");
        }
        refresh_changeset_node(&tx, changeset_id)?;
        transition_intent(
            &tx,
            &changeset.intent_id,
            &changeset.agent_id,
            "VALIDATED",
            "ACCEPTED",
        )?;
        Store::append_event(
            &tx,
            "changeset.accepted",
            "ChangeSet",
            changeset_id,
            Some(&changeset.agent_id),
            &json!({
                "git_ref": commit,
                "accepted_commit": commit,
                "accepted_ref": accepted_ref
            }),
        )?;
        tx.commit()?;
        drop(conn);
        self.get_changeset(changeset_id)
    }

    pub fn record_commit(&self, changeset_id: &str, git_ref: &str) -> Result<ChangeSet> {
        let changeset;
        let worktree;
        {
            let conn = self.store.lock()?;
            changeset = changeset_by_id(&conn, changeset_id)?;
            require_state(&changeset.status, &["ACCEPTED"], "record commit")?;
            worktree = conn
                .query_row(
                    "SELECT worktree FROM changesets WHERE id = ?1",
                    [changeset_id],
                    |row| row.get::<_, Option<String>>(0),
                )?
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("ChangeSet has no worktree"))?;
            let repo = git::discover(&worktree)?;
            ensure_repository_matches(&conn, &repo.common_dir)?;
        }
        let commit = git::verify_ref(&worktree, git_ref)?;
        let accepted_commit = changeset.accepted_commit.as_deref().ok_or_else(|| {
            anyhow::anyhow!("INVALID_TRANSITION: accepted ChangeSet has no pinned accepted commit")
        })?;
        let accepted_ref_name = format!("refs/foremerge/accepted/{changeset_id}");
        let accepted_ref = git::verify_ref(&worktree, &accepted_ref_name).map_err(|_| {
            anyhow::anyhow!(
                "CHECK_FAILED: accepted ref {accepted_ref_name} is missing from the ChangeSet repository"
            )
        })?;
        if accepted_ref != accepted_commit {
            bail!("CHECK_FAILED: accepted ref no longer matches the pinned ChangeSet commit");
        }
        if !git::is_ancestor(&worktree, accepted_commit, &commit)? {
            bail!(
                "TARGET_DIVERGED: recorded integration commit does not contain the accepted candidate"
            );
        }
        let mut conn = self.store.lock()?;
        let tx = Store::immediate_tx(&mut conn)?;
        let fresh = changeset_by_id(&tx, changeset_id)?;
        require_state(&fresh.status, &["ACCEPTED"], "record commit")?;
        if fresh.accepted_commit.as_deref() != Some(accepted_commit) {
            bail!("STATE_RACE: accepted commit changed while recording integration");
        }
        let final_accepted_ref = git::verify_ref(&worktree, &accepted_ref_name).map_err(|_| {
            anyhow::anyhow!(
                "CHECK_FAILED: accepted ref {accepted_ref_name} is missing from the ChangeSet repository"
            )
        })?;
        if final_accepted_ref != accepted_commit {
            bail!("CHECK_FAILED: accepted ref no longer matches the pinned ChangeSet commit");
        }
        let now = Utc::now().to_rfc3339();
        let changed = tx.execute(
            "UPDATE changesets SET status = 'COMMITTED', git_ref = ?2,
             integration_commit = ?2, updated_at = ?3 WHERE id = ?1 AND status = 'ACCEPTED'",
            params![changeset_id, commit, now],
        )?;
        if changed != 1 {
            bail!("STATE_RACE: ChangeSet changed while recording integration");
        }
        refresh_changeset_node(&tx, changeset_id)?;
        transition_intent(
            &tx,
            &changeset.intent_id,
            &changeset.agent_id,
            "ACCEPTED",
            "COMMITTED",
        )?;
        tx.execute(
            "UPDATE claims SET status = 'RELEASED', released_at = ?2
             WHERE intent_id = ?1 AND status = 'ACTIVE'",
            params![changeset.intent_id, now],
        )?;
        refresh_claim_nodes_for_intent(&tx, &changeset.intent_id)?;
        Store::append_event(
            &tx,
            "changeset.committed",
            "ChangeSet",
            changeset_id,
            Some(&changeset.agent_id),
            &json!({
                "git_ref": commit,
                "accepted_commit": accepted_commit,
                "integration_commit": commit
            }),
        )?;
        tx.commit()?;
        drop(conn);
        self.get_changeset(changeset_id)
    }

    pub fn discard_work(&self, agent_id: &str, intent_id: &str, reason: &str) -> Result<Intent> {
        if reason.trim().is_empty() {
            bail!("INVALID_INPUT: a non-empty --reason is now required to discard work");
        }
        let mut conn = self.store.lock()?;
        let tx = Store::immediate_tx(&mut conn)?;
        let intent = intent_by_id(&tx, intent_id)?;
        if intent.agent_id != agent_id {
            bail!("FORBIDDEN: an agent may only discard its own work");
        }
        if TERMINAL_INTENT_STATES.contains(&intent.status.as_str()) {
            bail!(
                "INVALID_TRANSITION: cannot discard work in state {}",
                intent.status
            );
        }
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE intents SET status = 'DISCARDED', version = version + 1, updated_at = ?2 WHERE id = ?1",
            params![intent_id, now],
        )?;
        refresh_intent_node(&tx, intent_id)?;
        tx.execute(
            "UPDATE claims SET status = 'RELEASED', released_at = ?2
             WHERE intent_id = ?1 AND status = 'ACTIVE'",
            params![intent_id, now],
        )?;
        refresh_claim_nodes_for_intent(&tx, intent_id)?;
        let mut conflict_statement = tx.prepare(
            "SELECT id FROM conflicts WHERE status IN ('OPEN', 'COORDINATING')
             AND (source_intent_id = ?1 OR target_intent_id = ?1)",
        )?;
        let conflict_ids = conflict_statement
            .query_map([intent_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(conflict_statement);
        for conflict_id in conflict_ids {
            tx.execute(
                "UPDATE conflicts SET status = 'DISMISSED' WHERE id = ?1",
                [&conflict_id],
            )?;
            refresh_conflict_node(&tx, &conflict_id)?;
            Store::append_event(
                &tx,
                "conflict.dismissed",
                "Conflict",
                &conflict_id,
                Some(agent_id),
                &json!({ "reason": "linked work was discarded", "intent_id": intent_id }),
            )?;
        }
        Store::append_event(
            &tx,
            "work.discarded",
            "Intent",
            intent_id,
            Some(agent_id),
            &json!({ "from": intent.status, "to": "DISCARDED", "reason": reason }),
        )?;
        tx.commit()?;
        drop(conn);
        self.get_intent(intent_id)
    }

    pub fn coordinate_with_agent(&self, request: CoordinateRequest) -> Result<CoordinationMessage> {
        if request.message.trim().is_empty() {
            bail!("INVALID_INPUT: coordination message cannot be empty");
        }
        let mut conn = self.store.lock()?;
        let tx = Store::immediate_tx(&mut conn)?;
        agent_by_id(&tx, &request.from_agent_id)?;
        agent_by_id(&tx, &request.to_agent_id)?;
        if let Some(conflict_id) = request.conflict_id.as_deref() {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM conflicts WHERE id = ?1)",
                [conflict_id],
                |row| row.get(0),
            )?;
            if !exists {
                bail!("NOT_FOUND: conflict {conflict_id}");
            }
            tx.execute(
                "UPDATE conflicts SET status = 'COORDINATING' WHERE id = ?1 AND status = 'OPEN'",
                [conflict_id],
            )?;
            refresh_conflict_node(&tx, conflict_id)?;
        }
        if let Some(changeset_id) = request.changeset_id.as_deref() {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM changesets WHERE id = ?1)",
                [changeset_id],
                |row| row.get(0),
            )?;
            if !exists {
                bail!("NOT_FOUND: ChangeSet {changeset_id}");
            }
        }
        let message = CoordinationMessage {
            id: id("msg"),
            from_agent_id: request.from_agent_id,
            to_agent_id: request.to_agent_id,
            message: request.message.trim().to_string(),
            conflict_id: request.conflict_id,
            changeset_id: request.changeset_id,
            status: "UNREAD".to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        tx.execute(
            "INSERT INTO coordination_messages(id, from_agent_id, to_agent_id, conflict_id,
             changeset_id, message, status, created_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                message.id,
                message.from_agent_id,
                message.to_agent_id,
                message.conflict_id,
                message.changeset_id,
                message.message,
                message.status,
                message.created_at,
            ],
        )?;
        Store::append_event(
            &tx,
            "coordination.sent",
            "CoordinationMessage",
            &message.id,
            Some(&message.from_agent_id),
            &serde_json::to_value(&message)?,
        )?;
        tx.commit()?;
        Ok(message)
    }

    pub fn resolve_conflict(
        &self,
        conflict_id: &str,
        request: ResolveConflictRequest,
    ) -> Result<Conflict> {
        let mut conn = self.store.lock()?;
        let tx = Store::immediate_tx(&mut conn)?;
        agent_by_id(&tx, &request.agent_id)?;
        let mut conflict = conflict_by_id(&tx, conflict_id)?;
        if !matches!(conflict.status.as_str(), "OPEN" | "COORDINATING") {
            bail!(
                "INVALID_TRANSITION: conflict is already {}",
                conflict.status
            );
        }
        if request.resolution.trim().is_empty() || request.rationale.trim().is_empty() {
            bail!("INVALID_INPUT: resolution and rationale are required");
        }
        let decision_id = id("dec");
        let intent_id = conflict
            .source_intent_id
            .clone()
            .unwrap_or_else(|| conflict.target_intent_id.clone());
        tx.execute(
            "INSERT INTO decisions(id, changeset_id, intent_id, title, rationale,
             alternatives_json, provenance_json, created_at)
             VALUES(?1, NULL, ?2, ?3, ?4, '[]', ?5, ?6)",
            params![
                decision_id,
                intent_id,
                request.resolution,
                request.rationale,
                to_json(&json!({ "agent_id": request.agent_id, "conflict_id": conflict_id }))?,
                Utc::now().to_rfc3339(),
            ],
        )?;
        tx.execute(
            "UPDATE conflicts SET status = 'RESOLVED' WHERE id = ?1",
            [conflict_id],
        )?;
        conflict.status = "RESOLVED".to_string();
        let decision_node = Store::upsert_node(
            &tx,
            "Decision",
            &decision_id,
            &request.resolution,
            &json!({ "rationale": request.rationale, "conflict_id": conflict_id }),
        )?;
        let conflict_node = Store::upsert_node(
            &tx,
            "Conflict",
            conflict_id,
            &conflict.explanation,
            &serde_json::to_value(&conflict)?,
        )?;
        Store::link_nodes(
            &tx,
            &conflict_node,
            &decision_node,
            "RESOLVED_BY",
            &json!({}),
        )?;
        Store::append_event(
            &tx,
            "conflict.resolved",
            "Conflict",
            conflict_id,
            Some(&request.agent_id),
            &json!({
                "resolution": request.resolution,
                "rationale": request.rationale,
                "decision_id": decision_id,
            }),
        )?;
        tx.commit()?;
        Ok(conflict)
    }

    pub fn inbox(&self, agent_id: &str) -> Result<Vec<CoordinationMessage>> {
        let conn = self.store.lock()?;
        agent_by_id(&conn, agent_id)?;
        let mut statement = conn.prepare(
            "SELECT id, from_agent_id, to_agent_id, message, conflict_id, changeset_id, status,
             created_at FROM coordination_messages WHERE to_agent_id = ?1 ORDER BY created_at",
        )?;
        Ok(statement
            .query_map([agent_id], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_intent(&self, intent_id: &str) -> Result<Intent> {
        let conn = self.store.lock()?;
        intent_by_id(&conn, intent_id)
    }

    /// Full read view of one intent: the intent itself, the owning agent, and
    /// the ids of open or coordinating conflicts touching it.
    pub fn show_intent(&self, intent_id: &str) -> Result<IntentDetail> {
        let conn = self.store.lock()?;
        let intent = intent_by_id(&conn, intent_id)?;
        let agent = agent_by_id(&conn, &intent.agent_id)?;
        let open_conflicts = open_conflicts_for_intent(&conn, intent_id)?;
        Ok(IntentDetail {
            intent,
            agent,
            open_conflicts,
        })
    }

    /// Every registered agent, oldest first.
    pub fn list_agents(&self) -> Result<Vec<Agent>> {
        let conn = self.store.lock()?;
        let mut statement = conn.prepare("SELECT id FROM agents ORDER BY registered_at, id")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        ids.iter().map(|id| agent_by_id(&conn, id)).collect()
    }

    pub fn get_changeset(&self, changeset_id: &str) -> Result<ChangeSet> {
        let conn = self.store.lock()?;
        changeset_by_id(&conn, changeset_id)
    }

    pub fn graph(&self) -> Result<Value> {
        let mut graph = self.store.graph_snapshot()?;
        let now = Utc::now().to_rfc3339();
        if let Some(nodes) = graph.get_mut("nodes").and_then(Value::as_array_mut) {
            for node in nodes {
                if node.get("kind").and_then(Value::as_str) != Some("Claim") {
                    continue;
                }
                let Some(data) = node.get_mut("data").and_then(Value::as_object_mut) else {
                    continue;
                };
                if data.get("status").and_then(Value::as_str) == Some("ACTIVE")
                    && data
                        .get("lease_expires_at")
                        .and_then(Value::as_str)
                        .is_some_and(|expires| expires <= now.as_str())
                {
                    data.insert("status".to_string(), Value::String("EXPIRED".to_string()));
                }
            }
        }
        Ok(graph)
    }

    pub fn events(&self, after_seq: i64, limit: usize) -> Result<Vec<Event>> {
        self.store.events(after_seq, limit)
    }

    pub fn list_conflicts(&self, status: Option<&str>) -> Result<Vec<Conflict>> {
        let conn = self.store.lock()?;
        let mut statement = conn.prepare(
            "SELECT id FROM conflicts WHERE (?1 IS NULL OR status = ?1) ORDER BY detected_at DESC",
        )?;
        let ids = statement
            .query_map([status.map(str::to_uppercase)], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter()
            .map(|value| conflict_by_id(&conn, value))
            .collect()
    }
}

/// True for a string with exactly the shape of a generated intent id:
/// `int_` followed by 32 hexadecimal characters.
fn looks_like_intent_id(value: &str) -> bool {
    value.len() == 36
        && value.starts_with("int_")
        && value.as_bytes()[4..].iter().all(u8::is_ascii_hexdigit)
}

fn open_conflicts_for_intent(conn: &Connection, intent_id: &str) -> Result<OpenConflicts> {
    let mut statement = conn.prepare(
        "SELECT id FROM conflicts WHERE status IN ('OPEN', 'COORDINATING')
         AND (source_intent_id = ?1 OR target_intent_id = ?1)
         ORDER BY detected_at DESC, id",
    )?;
    let ids = statement
        .query_map([intent_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(OpenConflicts {
        count: ids.len(),
        ids,
    })
}

fn active_candidates(conn: &Connection, exclude: Option<&str>) -> Result<Vec<IntentCandidate>> {
    let mut statement = conn.prepare(
        "SELECT id, summary, scopes_json FROM intents
         WHERE status NOT IN ('ACCEPTED', 'COMMITTED', 'DISCARDED')
         AND (?1 IS NULL OR id <> ?1) ORDER BY created_at",
    )?;
    Ok(statement
        .query_map([exclude], |row| {
            let scopes_json: String = row.get(2)?;
            Ok(IntentCandidate {
                id: row.get(0)?,
                summary: row.get(1)?,
                scopes: from_json(scopes_json)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn persist_conflict(
    tx: &Transaction<'_>,
    conflict: &Conflict,
    agent_id: Option<&str>,
) -> Result<Conflict> {
    let mut normalized = conflict.clone();
    if let Some(source) = normalized.source_intent_id.as_mut() {
        if source.as_str() > normalized.target_intent_id.as_str() {
            std::mem::swap(source, &mut normalized.target_intent_id);
            if let Some(evidence) = normalized.evidence.as_object_mut() {
                for (source_key, target_key) in [
                    ("source_operation", "target_operation"),
                    ("source_destination", "target_destination"),
                ] {
                    let source_value = evidence.remove(source_key);
                    let target_value = evidence.remove(target_key);
                    if let Some(value) = target_value {
                        evidence.insert(source_key.to_string(), value);
                    }
                    if let Some(value) = source_value {
                        evidence.insert(target_key.to_string(), value);
                    }
                }
            }
        }
    }
    let scope_json = to_json(&normalized.scope)?;
    let scope_identity = normalized
        .scope
        .as_ref()
        .map(Scope::canonical)
        .unwrap_or_else(|| "<none>".to_string());
    let canonical_id: String = tx.query_row(
        "INSERT INTO conflicts(id, kind, severity, score, source_intent_id, target_intent_id,
         scope_json, scope_identity, explanation, suggestion, evidence_json, status, detected_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(source_intent_id, target_intent_id, kind, scope_identity)
         DO UPDATE SET score = excluded.score, explanation = excluded.explanation,
         suggestion = excluded.suggestion, evidence_json = excluded.evidence_json,
         detected_at = excluded.detected_at
         RETURNING id",
        params![
            normalized.id,
            normalized.kind,
            normalized.severity,
            normalized.score,
            normalized.source_intent_id,
            normalized.target_intent_id,
            scope_json,
            scope_identity,
            normalized.explanation,
            normalized.suggestion,
            to_json(&normalized.evidence)?,
            normalized.status,
            normalized.detected_at,
        ],
        |row| row.get(0),
    )?;
    let canonical = conflict_by_id(tx, &canonical_id)?;
    let conflict_node = Store::upsert_node(
        tx,
        "Conflict",
        &canonical.id,
        &canonical.explanation,
        &serde_json::to_value(&canonical)?,
    )?;
    if let Some(source_intent_id) = canonical.source_intent_id.as_deref() {
        let source_node =
            Store::upsert_node(tx, "Intent", source_intent_id, source_intent_id, &json!({}))?;
        Store::link_nodes(tx, &source_node, &conflict_node, "HAS_CONFLICT", &json!({}))?;
    }
    let target_node = Store::upsert_node(
        tx,
        "Intent",
        &canonical.target_intent_id,
        &canonical.target_intent_id,
        &json!({}),
    )?;
    Store::link_nodes(
        tx,
        &conflict_node,
        &target_node,
        "CONFLICTS_WITH",
        &json!({}),
    )?;
    Store::append_event(
        tx,
        "conflict.detected",
        "Conflict",
        &canonical.id,
        agent_id,
        &serde_json::to_value(&canonical)?,
    )?;
    Ok(canonical)
}

fn transition_intent(
    tx: &Transaction<'_>,
    intent_id: &str,
    agent_id: &str,
    from: &str,
    to: &str,
) -> Result<()> {
    validate_transition(from, to)?;
    let now = Utc::now().to_rfc3339();
    let changed = tx.execute(
        "UPDATE intents SET status = ?3, version = version + 1, updated_at = ?4
         WHERE id = ?1 AND status = ?2",
        params![intent_id, from, to, now],
    )?;
    if changed != 1 {
        bail!("STATE_RACE: intent {intent_id} is no longer {from}");
    }
    Store::append_event(
        tx,
        "lifecycle.transitioned",
        "Intent",
        intent_id,
        Some(agent_id),
        &json!({ "from": from, "to": to }),
    )?;
    refresh_intent_node(tx, intent_id)?;
    Ok(())
}

fn refresh_intent_node(tx: &Transaction<'_>, intent_id: &str) -> Result<()> {
    let intent = intent_by_id(tx, intent_id)?;
    Store::upsert_node(
        tx,
        "Intent",
        intent_id,
        &intent.summary,
        &serde_json::to_value(&intent)?,
    )?;
    Ok(())
}

fn refresh_changeset_node(tx: &Transaction<'_>, changeset_id: &str) -> Result<()> {
    let changeset = changeset_by_id(tx, changeset_id)?;
    Store::upsert_node(
        tx,
        "ChangeSet",
        changeset_id,
        &changeset.summary,
        &serde_json::to_value(&changeset)?,
    )?;
    Ok(())
}

fn refresh_conflict_node(tx: &Transaction<'_>, conflict_id: &str) -> Result<()> {
    let conflict = conflict_by_id(tx, conflict_id)?;
    Store::upsert_node(
        tx,
        "Conflict",
        conflict_id,
        &conflict.explanation,
        &serde_json::to_value(&conflict)?,
    )?;
    Ok(())
}

fn refresh_claim_nodes_for_intent(tx: &Transaction<'_>, intent_id: &str) -> Result<()> {
    for claim in claims_for_intent(tx, intent_id)? {
        Store::upsert_node(
            tx,
            "Claim",
            &claim.id,
            &claim.scope.canonical(),
            &serde_json::to_value(&claim)?,
        )?;
    }
    Ok(())
}

pub fn validate_transition(from: &str, to: &str) -> Result<()> {
    let allowed = matches!(
        (from, to),
        ("INTENT", "CLAIMED")
            | ("CLAIMED", "IN_PROGRESS")
            | ("IN_PROGRESS", "PROVISIONAL")
            | ("PROVISIONAL", "VALIDATED")
            | ("VALIDATED", "ACCEPTED")
            | ("ACCEPTED", "COMMITTED")
    );
    if allowed {
        Ok(())
    } else {
        bail!("INVALID_TRANSITION: {from} -> {to}")
    }
}

fn require_state(current: &str, expected: &[&str], action: &str) -> Result<()> {
    if expected.contains(&current) {
        Ok(())
    } else {
        bail!(
            "INVALID_TRANSITION: cannot {action} while state is {current}; expected {}",
            expected.join(" or ")
        )
    }
}

fn normalize_scopes(scopes: Vec<Scope>) -> Result<Vec<Scope>> {
    scopes.into_iter().map(|scope| scope.normalized()).collect()
}

fn canonical_path(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn ensure_repository_matches(conn: &Connection, common_dir: &std::path::Path) -> Result<()> {
    let expected: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'repository_common_dir'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(expected) = expected {
        if canonical_path(std::path::Path::new(&expected)) != canonical_path(common_dir) {
            bail!(
                "INVALID_INPUT: worktree belongs to a different Git repository than this coordination store"
            );
        }
    }
    Ok(())
}

fn bind_repository(tx: &Transaction<'_>, common_dir: &std::path::Path) -> Result<()> {
    ensure_repository_matches(tx, common_dir)?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES('repository_common_dir', ?1)
         ON CONFLICT(key) DO NOTHING",
        [canonical_path(common_dir).to_string_lossy().into_owned()],
    )?;
    Ok(())
}

fn agent_by_id(conn: &Connection, id: &str) -> Result<Agent> {
    conn.query_row(
        "SELECT id, name, model, capabilities_json, worktree, git_branch, git_head, status,
         registered_at FROM agents WHERE id = ?1",
        [id],
        |row| {
            let capabilities: String = row.get(3)?;
            Ok(Agent {
                id: row.get(0)?,
                name: row.get(1)?,
                model: row.get(2)?,
                capabilities: from_json(capabilities)?,
                worktree: row.get(4)?,
                git_branch: row.get(5)?,
                git_head: row.get(6)?,
                status: row.get(7)?,
                registered_at: row.get(8)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| anyhow::anyhow!("NOT_FOUND: agent {id}"))
}

fn intent_by_id(conn: &Connection, id: &str) -> Result<Intent> {
    conn.query_row(
        "SELECT i.id, i.agent_id, i.task_id, t.title, i.summary, i.rationale,
         i.scopes_json, i.depends_on_json, i.metadata_json, i.status, i.created_at, i.updated_at
         FROM intents i JOIN tasks t ON t.id = i.task_id WHERE i.id = ?1",
        [id],
        |row| {
            let scopes: String = row.get(6)?;
            let dependencies: String = row.get(7)?;
            Ok(Intent {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                task_id: row.get(2)?,
                task: row.get(3)?,
                summary: row.get(4)?,
                rationale: row.get(5)?,
                scopes: from_json(scopes)?,
                depends_on: from_json(dependencies)?,
                metadata: from_json(row.get::<_, String>(8)?)?,
                status: row.get(9)?,
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
                open_conflicts: None,
            })
        },
    )
    .optional()?
    .ok_or_else(|| anyhow::anyhow!("NOT_FOUND: intent {id}"))
}

fn claims_for_intent(conn: &Connection, intent_id: &str) -> Result<Vec<Claim>> {
    let mut statement = conn.prepare(
        "SELECT id, agent_id, intent_id, scope_kind, scope_key, status, reason,
         lease_expires_at, created_at FROM claims WHERE intent_id = ?1 ORDER BY created_at",
    )?;
    let mut claims = statement
        .query_map([intent_id], |row| {
            Ok(Claim {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                intent_id: row.get(2)?,
                scope: Scope::new(row.get::<_, String>(3)?, row.get::<_, String>(4)?),
                status: row.get(5)?,
                reason: row.get(6)?,
                lease_expires_at: row.get(7)?,
                created_at: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let now = Utc::now().to_rfc3339();
    for claim in &mut claims {
        if claim.status == "ACTIVE" && claim.lease_expires_at <= now {
            claim.status = "EXPIRED".to_string();
        }
    }
    Ok(claims)
}

fn changeset_by_id(conn: &Connection, id: &str) -> Result<ChangeSet> {
    conn.query_row(
        "SELECT id, agent_id, task_id, intent_id, summary, files_json, symbols_json,
         contracts_json, dependencies_json, tests_json, decisions_json, provenance_json,
         base_ref, git_ref, accepted_commit, integration_commit, supersedes_changeset_id,
         fingerprint, status, created_at, updated_at
         FROM changesets WHERE id = ?1",
        [id],
        |row| {
            Ok(ChangeSet {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                task_id: row.get(2)?,
                intent_id: row.get(3)?,
                summary: row.get(4)?,
                files: from_json(row.get::<_, String>(5)?)?,
                symbols: from_json(row.get::<_, String>(6)?)?,
                contracts: from_json(row.get::<_, String>(7)?)?,
                dependencies: from_json(row.get::<_, String>(8)?)?,
                tests: from_json(row.get::<_, String>(9)?)?,
                decisions: from_json(row.get::<_, String>(10)?)?,
                provenance: from_json(row.get::<_, String>(11)?)?,
                base_ref: row.get(12)?,
                git_ref: row.get(13)?,
                accepted_commit: row.get(14)?,
                integration_commit: row.get(15)?,
                supersedes_changeset_id: row.get(16)?,
                fingerprint: row.get(17)?,
                status: row.get(18)?,
                created_at: row.get(19)?,
                updated_at: row.get(20)?,
                open_conflicts: None,
            })
        },
    )
    .optional()?
    .ok_or_else(|| anyhow::anyhow!("NOT_FOUND: changeset {id}"))
}

fn conflict_by_id(conn: &Connection, id: &str) -> Result<Conflict> {
    conn.query_row(
        "SELECT id, kind, severity, score, source_intent_id, target_intent_id, scope_json,
         explanation, suggestion, evidence_json, status, detected_at FROM conflicts WHERE id = ?1",
        [id],
        |row| {
            Ok(Conflict {
                id: row.get(0)?,
                kind: row.get(1)?,
                severity: row.get(2)?,
                score: row.get(3)?,
                source_intent_id: row.get(4)?,
                target_intent_id: row.get(5)?,
                scope: from_json(row.get::<_, String>(6)?)?,
                explanation: row.get(7)?,
                suggestion: row.get(8)?,
                evidence: from_json(row.get::<_, String>(9)?)?,
                status: row.get(10)?,
                detected_at: row.get(11)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| anyhow::anyhow!("NOT_FOUND: conflict {id}"))
}

fn message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CoordinationMessage> {
    Ok(CoordinationMessage {
        id: row.get(0)?,
        from_agent_id: row.get(1)?,
        to_agent_id: row.get(2)?,
        message: row.get(3)?,
        conflict_id: row.get(4)?,
        changeset_id: row.get(5)?,
        status: row.get(6)?,
        created_at: row.get(7)?,
    })
}

fn id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn to_json<T: serde::Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn from_json<T: DeserializeOwned>(value: String) -> rusqlite::Result<T> {
    serde_json::from_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            value.len(),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

async fn read_bounded_tail(mut input: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut tail = Vec::with_capacity(MAX_OUTPUT_BYTES);
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = input.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if read >= MAX_OUTPUT_BYTES {
            tail.clear();
            tail.extend_from_slice(&buffer[read - MAX_OUTPUT_BYTES..read]);
            continue;
        }
        let overflow = tail
            .len()
            .saturating_add(read)
            .saturating_sub(MAX_OUTPUT_BYTES);
        if overflow > 0 {
            tail.drain(..overflow);
        }
        tail.extend_from_slice(&buffer[..read]);
    }
    Ok(tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_allows_only_forward_canonical_transitions() {
        let allowed = [
            ("INTENT", "CLAIMED"),
            ("CLAIMED", "IN_PROGRESS"),
            ("IN_PROGRESS", "PROVISIONAL"),
            ("PROVISIONAL", "VALIDATED"),
            ("VALIDATED", "ACCEPTED"),
            ("ACCEPTED", "COMMITTED"),
        ];
        for pair in allowed {
            assert!(validate_transition(pair.0, pair.1).is_ok(), "{pair:?}");
        }
        assert!(validate_transition("INTENT", "PROVISIONAL").is_err());
        assert!(validate_transition("VALIDATED", "IN_PROGRESS").is_err());
        assert!(validate_transition("COMMITTED", "ACCEPTED").is_err());
    }

    #[test]
    fn overlapping_claim_is_advisory_and_both_are_stored() {
        let service = Foremerge::new(Store::in_memory().unwrap());
        let first = service
            .register_agent(RegisterAgentRequest {
                name: "stripe-agent".into(),
                model: Some("test".into()),
                capabilities: vec![],
                worktree: None,
            })
            .unwrap();
        let second = service
            .register_agent(RegisterAgentRequest {
                name: "paypal-agent".into(),
                model: Some("test".into()),
                capabilities: vec![],
                worktree: None,
            })
            .unwrap();
        let make_intent = |agent: &Agent, summary: &str| {
            service
                .publish_intent(PublishIntentRequest {
                    agent_id: agent.id.clone(),
                    task: format!("{} task", agent.name),
                    summary: summary.into(),
                    rationale: None,
                    scopes: vec![Scope::new("symbol", "PaymentService")],
                    depends_on: vec![],
                    metadata: json!({}),
                })
                .unwrap()
                .intent
        };
        let first_intent = make_intent(&first, "Inspect PaymentService health");
        let second_intent = make_intent(&second, "Document PaymentService behavior");
        let scope = Scope::new("symbol", "PaymentService");
        service
            .claim_work(ClaimWorkRequest {
                agent_id: first.id,
                intent_id: first_intent.id,
                scopes: vec![scope.clone()],
                reason: None,
                lease_seconds: 3600,
            })
            .unwrap();
        let outcome = service
            .claim_work(ClaimWorkRequest {
                agent_id: second.id,
                intent_id: second_intent.id,
                scopes: vec![scope],
                reason: None,
                lease_seconds: 3600,
            })
            .unwrap();
        assert!(outcome.advisory_only);
        assert_eq!(outcome.claims.len(), 1);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.kind == "overlapping_claim")
        );
    }

    #[test]
    fn structured_scopes_are_validated_at_the_service_boundary() {
        let service = Foremerge::new(Store::in_memory().unwrap());
        let agent = service
            .register_agent(RegisterAgentRequest {
                name: "scope-agent".into(),
                model: None,
                capabilities: vec![],
                worktree: None,
            })
            .unwrap();

        let error = service
            .publish_intent(PublishIntentRequest {
                agent_id: agent.id.clone(),
                task: "invalid scope".into(),
                summary: "Exercise the typed API boundary".into(),
                rationale: None,
                scopes: vec![Scope::new("made-up-kind", "PaymentService")],
                depends_on: vec![],
                metadata: json!({}),
            })
            .unwrap_err();

        assert!(format!("{error:#}").starts_with("INVALID_INPUT:"));

        let error = service
            .publish_intent(PublishIntentRequest {
                agent_id: agent.id.clone(),
                task: "scalar metadata".into(),
                summary: "Reject scalar metadata".into(),
                rationale: None,
                scopes: vec![],
                depends_on: vec![],
                metadata: json!("scalar"),
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("metadata must be a JSON object"));

        let error = service
            .publish_changeset(PublishChangeSetRequest {
                agent_id: agent.id,
                intent_id: "int_missing".into(),
                summary: "Reject scalar provenance".into(),
                files: vec![],
                symbols: vec![],
                contracts: vec![],
                dependencies: vec![],
                tests: vec![],
                decisions: vec![],
                provenance: json!(42),
                git_ref: None,
                base_ref: None,
                worktree: None,
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("provenance must be a JSON object"));
    }

    #[test]
    fn conflict_identity_uses_canonical_scope_case() {
        let service = Foremerge::new(Store::in_memory().unwrap());
        let register = |name: &str| {
            service
                .register_agent(RegisterAgentRequest {
                    name: name.into(),
                    model: None,
                    capabilities: vec![],
                    worktree: None,
                })
                .unwrap()
        };
        let first = register("canonical-a");
        let second = register("canonical-b");
        let publish = |agent: &Agent, scope: Scope| {
            service
                .publish_intent(PublishIntentRequest {
                    agent_id: agent.id.clone(),
                    task: format!("{} task", agent.name),
                    summary: format!("{} intent", agent.name),
                    rationale: None,
                    scopes: vec![scope],
                    depends_on: vec![],
                    metadata: json!({}),
                })
                .unwrap()
                .intent
        };
        let first_intent = publish(&first, Scope::new("symbol", "PaymentService"));
        let second_intent = publish(&second, Scope::new("symbol", "paymentservice"));
        let claim = |agent_id: &str, intent_id: &str, scope: Scope| {
            service
                .claim_work(ClaimWorkRequest {
                    agent_id: agent_id.into(),
                    intent_id: intent_id.into(),
                    scopes: vec![scope],
                    reason: None,
                    lease_seconds: 3600,
                })
                .unwrap()
        };

        claim(
            &first.id,
            &first_intent.id,
            Scope::new("symbol", "PaymentService"),
        );
        let first_warning = claim(
            &second.id,
            &second_intent.id,
            Scope::new("symbol", "paymentservice"),
        );
        let repeated_warning = claim(
            &first.id,
            &first_intent.id,
            Scope::new("symbol", "PAYMENTSERVICE"),
        );

        assert_eq!(first_warning.warnings.len(), 1);
        assert_eq!(repeated_warning.warnings.len(), 1);
        assert_eq!(
            first_warning.warnings[0].id,
            repeated_warning.warnings[0].id
        );
        assert_eq!(
            service
                .list_conflicts(None)
                .unwrap()
                .into_iter()
                .filter(|conflict| conflict.kind == "overlapping_claim")
                .count(),
            1
        );
    }

    #[test]
    fn scoped_query_applies_limit_after_semantic_filtering() {
        let service = Foremerge::new(Store::in_memory().unwrap());
        let agent = service
            .register_agent(RegisterAgentRequest {
                name: "query-agent".into(),
                model: None,
                capabilities: vec![],
                worktree: None,
            })
            .unwrap();
        let publish = |task: String, scope: Scope| {
            service
                .publish_intent(PublishIntentRequest {
                    agent_id: agent.id.clone(),
                    summary: task.clone(),
                    task,
                    rationale: None,
                    scopes: vec![scope],
                    depends_on: vec![],
                    metadata: json!({}),
                })
                .unwrap()
                .intent
        };
        let target = publish("target work".into(), Scope::new("symbol", "PaymentService"));
        for index in 0..60 {
            publish(
                format!("unrelated work {index}"),
                Scope::new("symbol", format!("Unrelated{index}")),
            );
        }

        let results = service
            .query_work(WorkQuery {
                scope: Some(Scope::new("symbol", "PaymentService")),
                limit: 50,
                ..WorkQuery::default()
            })
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].intent.id, target.id);
    }
}
