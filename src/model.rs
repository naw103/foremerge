use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Scope {
    pub kind: String,
    pub key: String,
}

impl Scope {
    pub fn new(kind: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            kind: kind.into().to_lowercase(),
            key: key.into(),
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        let (kind, key) = value.split_once(':').ok_or_else(|| {
            anyhow::anyhow!(
                "INVALID_INPUT: invalid scope '{value}'; expected KIND:KEY, for example symbol:PaymentService"
            )
        })?;
        if kind.trim().is_empty() || key.trim().is_empty() {
            anyhow::bail!("INVALID_INPUT: invalid scope '{value}'; kind and key must be non-empty");
        }
        let kind = kind.trim().to_lowercase();
        const VALID: &[&str] = &[
            "symbol",
            "api",
            "schema",
            "config",
            "infra",
            "test",
            "migration",
            "env",
            "file",
            "component",
            "contract",
            "domain",
        ];
        if !VALID.contains(&kind.as_str()) {
            anyhow::bail!(
                "INVALID_INPUT: unknown scope kind '{kind}'; use one of {}",
                VALID.join(", ")
            );
        }
        Ok(Self::new(kind, key.trim()))
    }

    pub fn normalized(&self) -> anyhow::Result<Self> {
        Self::parse(&format!("{}:{}", self.kind, self.key))
    }

    pub fn canonical(&self) -> String {
        format!("{}:{}", self.kind.to_lowercase(), self.key.to_lowercase())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterAgentRequest {
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub worktree: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub model: Option<String>,
    pub capabilities: Vec<String>,
    pub worktree: Option<String>,
    pub git_branch: Option<String>,
    pub git_head: Option<String>,
    pub status: String,
    pub registered_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishIntentRequest {
    pub agent_id: String,
    pub task: String,
    pub summary: String,
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub scopes: Vec<Scope>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default = "empty_object")]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub id: String,
    pub agent_id: String,
    pub task_id: String,
    pub task: String,
    pub summary: String,
    pub rationale: Option<String>,
    pub scopes: Vec<Scope>,
    pub depends_on: Vec<String>,
    pub metadata: Value,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishIntentOutcome {
    pub intent: Intent,
    pub conflicts: Vec<Conflict>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimWorkRequest {
    pub agent_id: String,
    pub intent_id: String,
    pub scopes: Vec<Scope>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default = "default_lease_seconds")]
    pub lease_seconds: u64,
}

fn default_lease_seconds() -> u64 {
    3600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub id: String,
    pub agent_id: String,
    pub intent_id: String,
    pub scope: Scope,
    pub status: String,
    pub reason: Option<String>,
    pub lease_expires_at: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimOutcome {
    pub claims: Vec<Claim>,
    pub warnings: Vec<Conflict>,
    pub advisory_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkQuery {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub scope: Option<Scope>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub intent: Intent,
    pub agent: Agent,
    pub claims: Vec<Claim>,
    pub latest_changeset_id: Option<String>,
    pub latest_changeset: Option<ChangeSet>,
    pub dependents: Vec<String>,
    pub open_conflicts: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictCheckRequest {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub intent_id: Option<String>,
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub id: String,
    pub kind: String,
    pub severity: String,
    pub score: f64,
    pub source_intent_id: Option<String>,
    pub target_intent_id: String,
    pub scope: Option<Scope>,
    pub explanation: String,
    pub suggestion: String,
    pub evidence: Value,
    pub status: String,
    pub detected_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictReport {
    pub conflicts: Vec<Conflict>,
    pub checked_intents: usize,
    pub blocking: bool,
    pub policy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestEvidence {
    pub command: String,
    pub status: String,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionInput {
    pub title: String,
    pub rationale: String,
    #[serde(default)]
    pub alternatives: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishChangeSetRequest {
    pub agent_id: String,
    pub intent_id: String,
    pub summary: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub contracts: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub tests: Vec<TestEvidence>,
    #[serde(default)]
    pub decisions: Vec<DecisionInput>,
    #[serde(default = "empty_object")]
    pub provenance: Value,
    #[serde(default)]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub worktree: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeSet {
    pub id: String,
    pub agent_id: String,
    pub task_id: String,
    pub intent_id: String,
    pub summary: String,
    pub files: Vec<String>,
    pub symbols: Vec<String>,
    pub contracts: Vec<String>,
    pub dependencies: Vec<String>,
    pub tests: Vec<TestEvidence>,
    pub decisions: Vec<DecisionInput>,
    pub provenance: Value,
    pub base_ref: Option<String>,
    pub git_ref: Option<String>,
    #[serde(default)]
    pub accepted_commit: Option<String>,
    #[serde(default)]
    pub integration_commit: Option<String>,
    pub supersedes_changeset_id: Option<String>,
    pub fingerprint: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationRequest {
    pub command: Vec<String>,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_timeout_seconds() -> u64 {
    300
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Validation {
    pub id: String,
    pub changeset_id: String,
    pub command: Vec<String>,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u128,
    pub fingerprint: String,
    pub run_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptRequest {
    #[serde(default)]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub allow_high_conflicts: bool,
    #[serde(default)]
    pub override_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinateRequest {
    pub from_agent_id: String,
    pub to_agent_id: String,
    pub message: String,
    #[serde(default)]
    pub conflict_id: Option<String>,
    #[serde(default)]
    pub changeset_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinationMessage {
    pub id: String,
    pub from_agent_id: String,
    pub to_agent_id: String,
    pub message: String,
    pub conflict_id: Option<String>,
    pub changeset_id: Option<String>,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveConflictRequest {
    pub agent_id: String,
    pub resolution: String,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordCommitRequest {
    pub git_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: i64,
    pub event_id: String,
    pub event_type: String,
    pub entity_type: String,
    pub entity_id: String,
    pub agent_id: Option<String>,
    pub payload: Value,
    pub created_at: String,
    pub prev_hash: String,
    pub event_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub version: String,
    pub database: String,
    pub database_ok: bool,
    pub git_available: bool,
    pub git_repository: bool,
    pub git_root: Option<String>,
    pub git_common_dir: Option<String>,
    pub shared_across_worktrees: bool,
    pub api_bind: String,
    pub token_configured: bool,
    pub mcp_transport: String,
    pub ready: bool,
    pub next_step: String,
}

pub fn empty_object() -> Value {
    json!({})
}
