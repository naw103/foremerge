use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use foremerge::api::{self, ApiState};
use foremerge::git;
use foremerge::model::*;
use foremerge::{Foremerge, Store, checks, integrations, mcp};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

const DEFAULT_BIND: &str = "127.0.0.1:47811";

#[derive(Debug, Parser)]
#[command(
    name = "foremerge",
    version,
    about = "Catch intent conflicts before code conflicts",
    long_about = "The open-source coordination protocol for coding agents, built above Git.\n\nForemerge stores semantic intents, advisory claims, conflicts, ChangeSets, validation, decisions, and provenance in SQLite shared through Git's common directory."
)]
struct Cli {
    /// Emit a stable JSON success/error envelope on stdout.
    #[arg(long, global = true)]
    json: bool,

    /// Override the SQLite database path (or set FOREMERGE_DB).
    #[arg(long, global = true, env = "FOREMERGE_DB")]
    database: Option<PathBuf>,

    /// Resolve Git and runtime state from this directory.
    #[arg(long, global = true, default_value = ".")]
    cwd: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialize the shared SQLite store under Git's common directory.
    Init,
    /// Install native skills and MCP configuration for coding-agent clients.
    Setup {
        #[arg(value_enum, default_value_t = ClientArg::All)]
        client: ClientArg,
        /// Replace a differing Foremerge-managed skill or MCP entry.
        #[arg(long)]
        force: bool,
        /// Install skills only; do not register or write MCP configuration.
        #[arg(long)]
        skip_mcp: bool,
    },
    /// Verify Git, SQLite, shared storage, MCP, and optional client integration.
    Doctor {
        #[arg(long, value_enum)]
        client: Option<ClientArg>,
    },
    /// Run the loopback JSON API daemon.
    Daemon {
        #[arg(long, default_value = DEFAULT_BIND)]
        bind: SocketAddr,
        /// Disable bearer auth for a trusted local test only.
        #[arg(long)]
        no_auth: bool,
    },
    /// Run the newline-delimited MCP stdio server.
    Mcp,
    /// Register coding agents.
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Publish semantic intent before editing.
    #[command(subcommand)]
    Intent(IntentCommand),
    /// Claim, query, start, watch, or discard work.
    #[command(subcommand)]
    Work(WorkCommand),
    /// Check, list, and resolve intent conflicts.
    #[command(subcommand)]
    Conflicts(ConflictCommand),
    /// Publish, inspect, validate, accept, and commit ChangeSets.
    #[command(subcommand)]
    Changeset(ChangeSetCommand),
    /// Send coordination messages and read agent inboxes.
    #[command(subcommand)]
    Coordinate(CoordinateCommand),
    /// Inspect semantic events.
    #[command(subcommand)]
    Events(EventCommand),
    /// Export the current semantic dependency graph.
    Graph,
    /// Configure trusted named verification commands available to MCP agents.
    #[command(subcommand)]
    Checks(CheckCommand),
    /// Create an isolated Git worktree.
    #[command(subcommand)]
    Worktree(WorktreeCommand),
    /// Raw JSON API escape hatch using configured local auth.
    Request(RequestArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ClientArg {
    Codex,
    Claude,
    Cursor,
    All,
}

impl ClientArg {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
            Self::All => "all",
        }
    }

    fn clients(self) -> Vec<integrations::Client> {
        match self {
            Self::Codex => vec![integrations::Client::Codex],
            Self::Claude => vec![integrations::Client::Claude],
            Self::Cursor => vec![integrations::Client::Cursor],
            Self::All => vec![
                integrations::Client::Codex,
                integrations::Client::Claude,
                integrations::Client::Cursor,
            ],
        }
    }
}

#[derive(Debug, Subcommand)]
enum CheckCommand {
    /// Add or replace a trusted argv validation command in private shared state.
    Set {
        name: String,
        #[arg(long, default_value_t = 300)]
        timeout_seconds: u64,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// List trusted verification checks available to MCP agents.
    List,
    /// Remove a trusted verification check.
    Remove { name: String },
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Register an agent/model and its isolated worktree.
    Register {
        #[arg(long)]
        name: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        #[arg(long)]
        worktree: Option<PathBuf>,
        /// Register without Git worktree metadata.
        #[arg(long)]
        no_worktree: bool,
    },
    /// List registered agents with id, name, model, worktree, and status.
    List,
}

#[derive(Debug, Subcommand)]
enum IntentCommand {
    /// Publish intent, scopes, and dependencies; returns pre-code conflicts.
    Publish {
        #[arg(long = "agent")]
        agent_id: String,
        #[arg(long)]
        task: String,
        #[arg(long)]
        summary: String,
        #[arg(long)]
        rationale: Option<String>,
        #[arg(long = "scope")]
        scopes: Vec<String>,
        #[arg(long = "depends-on")]
        depends_on: Vec<String>,
        #[arg(long = "metadata-json", default_value = "{}")]
        metadata: String,
    },
    /// Show one intent: summary, task, owning agent, scopes, status, and open
    /// conflict ids.
    Show { intent_id: String },
}

#[derive(Debug, Subcommand)]
enum WorkCommand {
    /// Create advisory semantic claims; overlap warns but succeeds.
    Claim {
        #[arg(long = "agent")]
        agent_id: String,
        #[arg(long = "intent")]
        intent_id: String,
        #[arg(long = "scope", required = true)]
        scopes: Vec<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long, default_value_t = 3600)]
        lease_seconds: u64,
    },
    /// Query ownership, upcoming work, claims, ChangeSets, and conflict counts.
    Query {
        #[arg(long = "agent")]
        agent_id: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Transition claimed work to IN_PROGRESS.
    Start {
        intent_id: String,
        #[arg(long = "agent")]
        agent_id: String,
    },
    /// Poll semantic events as they cross useful boundaries.
    Watch {
        #[arg(long, default_value_t = 0)]
        after_seq: i64,
        #[arg(long, default_value_t = 500)]
        interval_ms: u64,
        /// Print currently available events and exit.
        #[arg(long)]
        once: bool,
    },
    /// Discard speculative work while preserving its audit trail.
    Discard {
        intent_id: String,
        #[arg(long = "agent")]
        agent_id: String,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Debug, Subcommand)]
enum ConflictCommand {
    /// Run an explainable preflight against active intent.
    Check {
        #[arg(long = "agent")]
        agent_id: Option<String>,
        /// Check a persisted intent by id (int_*).
        #[arg(long = "intent-id", conflicts_with = "intent")]
        intent_id: Option<String>,
        /// Free-form intent text for a provisional preflight. Intent ids are
        /// rejected here; pass them with --intent-id instead.
        #[arg(long, conflicts_with = "intent_id")]
        intent: Option<String>,
        #[arg(long = "scope")]
        scopes: Vec<String>,
    },
    /// List persisted conflicts.
    List {
        #[arg(long)]
        status: Option<String>,
    },
    /// Record a decision and resolve a persisted conflict.
    #[command(long_about = "Record a decision and resolve a persisted conflict.\n\n\
            On this trusted CLI surface any registered agent (or a human operator acting as one) \
            may resolve a conflict; over MCP resolution is accepted only from an agent whose \
            intent is a party to the conflict, after real agreement with the other party.\n\n\
            Resolve only after the parties actually agreed (for example via `coordinate send`), \
            and reference that coordination in --rationale.")]
    Resolve {
        conflict_id: String,
        /// The resolving agent; the decision is recorded under this id.
        #[arg(long = "agent")]
        agent_id: String,
        /// Free-form decision title describing the agreed outcome, for example
        /// 'sequenced: provider abstraction lands first' or 'split scopes'.
        #[arg(long)]
        resolution: String,
        /// Why this resolution is correct; reference the coordination message
        /// ids (msg_*) that produced the agreement.
        #[arg(long)]
        rationale: String,
    },
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)] // Parsed once at startup; boxing would only complicate clap's surface.
enum ChangeSetCommand {
    /// Publish a provisional semantic ChangeSet linked to the current Git state.
    Publish {
        #[arg(long = "agent")]
        agent_id: String,
        #[arg(long = "intent")]
        intent_id: String,
        #[arg(long)]
        summary: String,
        #[arg(long = "file")]
        files: Vec<String>,
        #[arg(long = "symbol")]
        symbols: Vec<String>,
        #[arg(long = "contract")]
        contracts: Vec<String>,
        #[arg(long = "dependency")]
        dependencies: Vec<String>,
        #[arg(long = "reported-test")]
        reported_tests: Vec<String>,
        #[arg(long)]
        decision: Option<String>,
        #[arg(long = "decision-rationale", requires = "decision")]
        decision_rationale: Option<String>,
        #[arg(long = "provenance-json", default_value = "{}")]
        provenance: String,
        #[arg(long)]
        git_ref: Option<String>,
        /// True diff base when known (for example the fork point of this
        /// agent branch); defaults to the candidate commit's first parent.
        #[arg(long)]
        base_ref: Option<String>,
        #[arg(long)]
        worktree: Option<PathBuf>,
    },
    /// Show one ChangeSet including provenance and Git fingerprint.
    Show { changeset_id: String },
    /// Run a Foremerge-owned argv validation against the exact ChangeSet fingerprint.
    Validate {
        changeset_id: String,
        #[arg(long)]
        worktree: Option<PathBuf>,
        #[arg(long, default_value_t = 300)]
        timeout_seconds: u64,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Pin a validated, clean candidate as refs/foremerge/accepted/<id>.
    Accept {
        changeset_id: String,
        #[arg(long)]
        git_ref: Option<String>,
        #[arg(long)]
        allow_high_conflicts: bool,
        /// Required rationale when explicitly overriding unresolved HIGH conflicts.
        #[arg(long, requires = "allow_high_conflicts")]
        override_reason: Option<String>,
    },
    /// Record the durable Git commit after integration.
    Commit {
        changeset_id: String,
        #[arg(long = "git-ref")]
        git_ref: String,
    },
}

#[derive(Debug, Subcommand)]
enum CoordinateCommand {
    /// Send a durable directed coordination message.
    Send {
        #[arg(long = "from")]
        from_agent_id: String,
        #[arg(long = "to")]
        to_agent_id: String,
        #[arg(long)]
        message: String,
        #[arg(long = "conflict")]
        conflict_id: Option<String>,
        #[arg(long = "changeset")]
        changeset_id: Option<String>,
    },
    /// Read durable messages for an agent.
    Inbox {
        /// Agent id (positional form, kept for compatibility with --agent).
        agent_id: Option<String>,
        /// Agent id, matching the --agent convention of sibling commands.
        #[arg(long = "agent")]
        agent: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum EventCommand {
    /// List append-only semantic events after a sequence number.
    List {
        #[arg(long, default_value_t = 0)]
        after_seq: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

#[derive(Debug, Subcommand)]
enum WorktreeCommand {
    /// Create a branch in a new isolated Git worktree using stock Git.
    Create {
        #[arg(long)]
        branch: String,
        #[arg(long)]
        path: PathBuf,
        #[arg(long, default_value = "HEAD")]
        base: String,
    },
}

#[derive(Debug, Clone, ValueEnum)]
enum HttpMethod {
    Get,
    Post,
}

#[derive(Debug, Args)]
struct RequestArgs {
    #[arg(value_enum)]
    method: HttpMethod,
    /// Versioned path, for example /v1/work?status=INTENT.
    path: String,
    /// JSON request body for POST.
    #[arg(long)]
    body: Option<String>,
    #[arg(long, env = "FOREMERGE_URL", default_value = "http://127.0.0.1:47811")]
    url: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let mcp_mode = matches!(&cli.command, Commands::Mcp);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let json_mode = cli.json;
    if let Err(error) = execute(cli).await {
        if error
            .downcast_ref::<io::Error>()
            .is_some_and(|value| value.kind() == io::ErrorKind::BrokenPipe)
        {
            return;
        }
        if json_mode && !mcp_mode {
            let line = serde_json::to_string(&json!({
                "ok": false,
                "error": { "code": error_code(&error), "message": format!("{error:#}") }
            }))
            .expect("serialize error");
            if let Err(write_error) = write_stdout_line(&line) {
                if write_error
                    .downcast_ref::<io::Error>()
                    .is_none_or(|value| value.kind() != io::ErrorKind::BrokenPipe)
                {
                    eprintln!("error: {write_error:#}");
                }
            }
        } else {
            eprintln!("error: {error:#}");
        }
        std::process::exit(1);
    }
}

async fn execute(cli: Cli) -> Result<()> {
    let cwd = cli.cwd.canonicalize().unwrap_or(cli.cwd.clone());
    let database = git::resolve_database_path(&cwd, cli.database.as_deref());
    match cli.command {
        Commands::Init => {
            let service = open_service(&database, &cwd)?;
            let token_path = ensure_token(&cwd)?;
            emit(
                cli.json,
                json!({
                    "database": service.store().path(),
                    "runtime_dir": token_path.parent(),
                    "token_file": token_path,
                    "shared_across_worktrees": git::discover(&cwd).is_ok(),
                    "next_step": "foremerge --json doctor"
                }),
            )?;
        }
        Commands::Setup {
            client,
            force,
            skip_mcp,
        } => {
            let repo = git::discover(&cwd).context(
                "INVALID_INPUT: client setup must run inside the Git repository to coordinate",
            )?;
            let service = open_service(&database, &cwd)?;
            let token_path = ensure_token(&cwd)?;
            let executable = std::env::current_exe().context("resolve Foremerge executable")?;
            let doctor_client = client.name();
            let clients = client.clients();
            let reports =
                integrations::install(&repo.root, &clients, &executable, force, !skip_mcp);
            let failures: Vec<String> = reports
                .iter()
                .filter_map(|report| {
                    report
                        .error
                        .as_ref()
                        .map(|error| format!("{}: {error}", report.client))
                })
                .collect();
            let data = json!({
                "repository": repo.root,
                "database": service.store().path(),
                "token_file": token_path,
                "clients": reports,
                "next_step": format!("foremerge --json doctor --client {doctor_client}")
            });
            if failures.is_empty() {
                emit(cli.json, data)?;
            } else {
                let code = reports
                    .iter()
                    .find_map(|report| report.error.as_deref())
                    .map(error_code_from_message)
                    .unwrap_or_else(|| "ERROR".to_string());
                let message = format!(
                    "setup did not complete for {} of {} client(s): {}",
                    failures.len(),
                    reports.len(),
                    failures.join("; ")
                );
                if cli.json {
                    write_stdout_line(&serde_json::to_string(&json!({
                        "ok": false,
                        "error": { "code": code, "message": message },
                        "data": data
                    }))?)?;
                } else {
                    write_stdout_line(&serde_json::to_string_pretty(&data)?)?;
                    eprintln!("error: {message}");
                }
                std::process::exit(1);
            }
        }
        Commands::Doctor { client } => {
            let store = Store::open(&database);
            let repo = git::discover(&cwd).ok();
            let token_path = git::runtime_dir(&cwd).join("token");
            let client_diagnostics = client.map(|value| {
                integrations::diagnose(
                    repo.as_ref()
                        .map_or(cwd.as_path(), |value| value.root.as_path()),
                    &value.clients(),
                )
            });
            let clients_ready = client_diagnostics
                .as_ref()
                .is_none_or(|values| values.iter().all(|value| value.ready));
            let next_client_step = client_diagnostics
                .as_ref()
                .and_then(|values| values.iter().find_map(|value| value.next_step.clone()));
            let report = DoctorReport {
                version: env!("CARGO_PKG_VERSION").to_string(),
                database: database.to_string_lossy().into_owned(),
                database_ok: store.is_ok(),
                git_available: git::available(),
                git_repository: repo.is_some(),
                git_root: repo
                    .as_ref()
                    .map(|value| value.root.to_string_lossy().into_owned()),
                git_common_dir: repo
                    .as_ref()
                    .map(|value| value.common_dir.to_string_lossy().into_owned()),
                shared_across_worktrees: repo.is_some(),
                api_bind: DEFAULT_BIND.to_string(),
                token_configured: token_path.is_file(),
                mcp_transport: "stdio (newline-delimited JSON-RPC; MCP 2026-07-28 with 2025-11-25 initialize compatibility)".to_string(),
                ready: store.is_ok() && git::available() && clients_ready,
                next_step: if let Some(next_step) = next_client_step {
                    next_step
                } else if repo.is_some() {
                    "foremerge --json agent register --name agent-1 --model your-model".to_string()
                } else {
                    "Run inside a Git worktree (recommended), or continue with --database PATH."
                        .to_string()
                },
                clients: client_diagnostics,
            };
            emit(cli.json, serde_json::to_value(report)?)?;
        }
        Commands::Daemon { bind, no_auth } => {
            if !bind.ip().is_loopback() {
                bail!(
                    "REFUSED_BIND: the MVP daemon binds loopback only; use optional shared mode when available"
                )
            }
            let service = open_service(&database, &cwd)?;
            let token = if no_auth {
                None
            } else {
                Some(read_or_create_token(&cwd)?)
            };
            if !cli.json {
                eprintln!("Foremerge API listening on http://{bind}");
            }
            api::serve(ApiState { service, token }, bind).await?;
        }
        Commands::Mcp => {
            let service = open_service(&database, &cwd)?;
            mcp::run_stdio(service).await?;
        }
        Commands::Checks(command) => {
            // The trusted check registry is repository-scoped: resolve it
            // through Git discovery first so a non-Git directory fails before
            // any store file is created.
            let registry_path = checks::path(&cwd)?;
            // Opening the service surfaces store problems and binds the shared
            // store to this repository before validation policy changes.
            open_service(&database, &cwd)?;
            let registry = match command {
                CheckCommand::Set {
                    name,
                    timeout_seconds,
                    command,
                } => checks::set_at(
                    &registry_path,
                    &name,
                    checks::NamedCheck {
                        command,
                        timeout_seconds,
                    },
                )?,
                CheckCommand::List => checks::load_at(&registry_path)?,
                CheckCommand::Remove { name } => checks::remove_at(&registry_path, &name)?,
            };
            emit(
                cli.json,
                json!({ "path": registry_path, "registry": registry }),
            )?;
        }
        Commands::Request(request) => run_raw_request(&cwd, request, cli.json).await?,
        Commands::Worktree(WorktreeCommand::Create { branch, path, base }) => {
            let output = Command::new("git")
                .arg("-C")
                .arg(&cwd)
                .args(["worktree", "add", "-b", &branch])
                .arg(&path)
                .arg(&base)
                .output()?;
            if !output.status.success() {
                bail!(
                    "GIT_ERROR: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            emit(
                cli.json,
                json!({ "branch": branch, "path": path, "base": base, "created": true }),
            )?;
        }
        command => {
            let service = open_service(&database, &cwd)?;
            execute_service(command, service, &cwd, cli.json).await?;
        }
    }
    Ok(())
}

fn open_service(database: &Path, cwd: &Path) -> Result<Foremerge> {
    let service = Foremerge::new(Store::open(database)?);
    service.bind_repository_cwd(cwd)?;
    Ok(service)
}

async fn execute_service(
    command: Commands,
    service: Foremerge,
    cwd: &Path,
    json_mode: bool,
) -> Result<()> {
    let value = match command {
        Commands::Agent(AgentCommand::Register {
            name,
            model,
            capabilities,
            worktree,
            no_worktree,
        }) => {
            let worktree = if no_worktree {
                None
            } else {
                Some(
                    worktree
                        .unwrap_or_else(|| cwd.to_path_buf())
                        .to_string_lossy()
                        .into_owned(),
                )
            };
            serde_json::to_value(service.register_agent(RegisterAgentRequest {
                name,
                model,
                capabilities,
                worktree,
            })?)?
        }
        Commands::Agent(AgentCommand::List) => serde_json::to_value(service.list_agents()?)?,
        Commands::Intent(IntentCommand::Show { intent_id }) => {
            serde_json::to_value(service.show_intent(&intent_id)?)?
        }
        Commands::Intent(IntentCommand::Publish {
            agent_id,
            task,
            summary,
            rationale,
            scopes,
            depends_on,
            metadata,
        }) => serde_json::to_value(service.publish_intent(PublishIntentRequest {
            agent_id,
            task,
            summary,
            rationale,
            scopes: parse_scopes(&scopes)?,
            depends_on,
            metadata: serde_json::from_str(&metadata).context("parse --metadata-json")?,
        })?)?,
        Commands::Work(WorkCommand::Claim {
            agent_id,
            intent_id,
            scopes,
            reason,
            lease_seconds,
        }) => serde_json::to_value(service.claim_work(ClaimWorkRequest {
            agent_id,
            intent_id,
            scopes: parse_scopes(&scopes)?,
            reason,
            lease_seconds,
        })?)?,
        Commands::Work(WorkCommand::Query {
            agent_id,
            status,
            scope,
            limit,
        }) => serde_json::to_value(service.query_work(WorkQuery {
            agent_id,
            status,
            scope: scope.as_deref().map(Scope::parse).transpose()?,
            limit,
        })?)?,
        Commands::Work(WorkCommand::Start {
            intent_id,
            agent_id,
        }) => serde_json::to_value(service.start_work(&agent_id, &intent_id)?)?,
        Commands::Work(WorkCommand::Discard {
            intent_id,
            agent_id,
            reason,
        }) => serde_json::to_value(service.discard_work(&agent_id, &intent_id, &reason)?)?,
        Commands::Work(WorkCommand::Watch {
            mut after_seq,
            interval_ms,
            once,
        }) => {
            loop {
                let events = service.events(after_seq, 100)?;
                for event in &events {
                    emit(json_mode, serde_json::to_value(event)?)?;
                    after_seq = event.seq;
                }
                if once {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(interval_ms.clamp(100, 10_000))).await;
            }
            return Ok(());
        }
        Commands::Conflicts(ConflictCommand::Check {
            agent_id,
            intent_id,
            intent,
            scopes,
        }) => serde_json::to_value(service.check_conflicts(ConflictCheckRequest {
            agent_id,
            intent_id,
            intent,
            scopes: parse_scopes(&scopes)?,
        })?)?,
        Commands::Conflicts(ConflictCommand::List { status }) => {
            serde_json::to_value(service.list_conflicts(status.as_deref())?)?
        }
        Commands::Conflicts(ConflictCommand::Resolve {
            conflict_id,
            agent_id,
            resolution,
            rationale,
        }) => serde_json::to_value(service.resolve_conflict(
            &conflict_id,
            ResolveConflictRequest {
                agent_id,
                resolution,
                rationale,
            },
        )?)?,
        Commands::Changeset(ChangeSetCommand::Publish {
            agent_id,
            intent_id,
            summary,
            files,
            symbols,
            contracts,
            dependencies,
            reported_tests,
            decision,
            decision_rationale,
            provenance,
            git_ref,
            base_ref,
            worktree,
        }) => {
            let tests = reported_tests
                .into_iter()
                .map(|value| {
                    let (command, status) = value.split_once('=').unwrap_or((&value, "REPORTED"));
                    TestEvidence {
                        command: command.to_string(),
                        status: status.to_string(),
                        summary: Some(
                            "Agent-reported provenance; does not satisfy acceptance gate."
                                .to_string(),
                        ),
                    }
                })
                .collect();
            let decisions = decision
                .map(|title| DecisionInput {
                    title,
                    rationale: decision_rationale.unwrap_or_default(),
                    alternatives: vec![],
                })
                .into_iter()
                .collect();
            serde_json::to_value(service.publish_changeset(PublishChangeSetRequest {
                agent_id,
                intent_id,
                summary,
                files,
                symbols,
                contracts,
                dependencies,
                tests,
                decisions,
                provenance: serde_json::from_str(&provenance).context("parse --provenance-json")?,
                git_ref,
                base_ref,
                worktree: worktree.map(|value| value.to_string_lossy().into_owned()),
            })?)?
        }
        Commands::Changeset(ChangeSetCommand::Show { changeset_id }) => {
            serde_json::to_value(service.get_changeset(&changeset_id)?)?
        }
        Commands::Changeset(ChangeSetCommand::Validate {
            changeset_id,
            worktree,
            timeout_seconds,
            command,
        }) => serde_json::to_value(
            service
                .validate_changeset(
                    &changeset_id,
                    ValidationRequest {
                        command,
                        worktree: worktree.map(|value| value.to_string_lossy().into_owned()),
                        timeout_seconds,
                    },
                )
                .await?,
        )?,
        Commands::Changeset(ChangeSetCommand::Accept {
            changeset_id,
            git_ref,
            allow_high_conflicts,
            override_reason,
        }) => serde_json::to_value(service.accept_changeset(
            &changeset_id,
            AcceptRequest {
                git_ref,
                allow_high_conflicts,
                override_reason,
            },
        )?)?,
        Commands::Changeset(ChangeSetCommand::Commit {
            changeset_id,
            git_ref,
        }) => serde_json::to_value(service.record_commit(&changeset_id, &git_ref)?)?,
        Commands::Coordinate(CoordinateCommand::Send {
            from_agent_id,
            to_agent_id,
            message,
            conflict_id,
            changeset_id,
        }) => serde_json::to_value(service.coordinate_with_agent(CoordinateRequest {
            from_agent_id,
            to_agent_id,
            message,
            conflict_id,
            changeset_id,
        })?)?,
        Commands::Coordinate(CoordinateCommand::Inbox { agent_id, agent }) => {
            let agent_id = match (agent_id, agent) {
                (Some(positional), Some(flag)) if positional != flag => bail!(
                    "INVALID_INPUT: the positional AGENT_ID ({positional}) and --agent ({flag}) disagree; pass the agent id once"
                ),
                (Some(positional), _) => positional,
                (None, Some(flag)) => flag,
                (None, None) => {
                    bail!("INVALID_INPUT: coordinate inbox requires an agent id via --agent")
                }
            };
            serde_json::to_value(service.inbox(&agent_id)?)?
        }
        Commands::Events(EventCommand::List { after_seq, limit }) => {
            serde_json::to_value(service.events(after_seq, limit)?)?
        }
        Commands::Graph => service.graph()?,
        _ => bail!("unsupported command dispatch"),
    };
    emit(json_mode, value)
}

fn parse_scopes(values: &[String]) -> Result<Vec<Scope>> {
    values.iter().map(|value| Scope::parse(value)).collect()
}

fn emit(json_mode: bool, value: Value) -> Result<()> {
    if json_mode {
        write_stdout_line(&serde_json::to_string(
            &json!({ "ok": true, "data": value }),
        )?)?;
    } else if let Some(array) = value.as_array() {
        if array.is_empty() {
            write_stdout_line("No results.")?;
        } else {
            write_stdout_line(&serde_json::to_string_pretty(&value)?)?;
        }
    } else {
        write_stdout_line(&serde_json::to_string_pretty(&value)?)?;
    }
    Ok(())
}

fn write_stdout_line(value: &str) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(value.as_bytes())?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn ensure_token(cwd: &Path) -> Result<PathBuf> {
    let path = git::runtime_dir(cwd).join("token");
    let parent = path.parent().expect("token parent");
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    if !path.exists() {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => file.write_all(Uuid::new_v4().simple().to_string().as_bytes())?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!(
            "INVALID_INPUT: token path must be a regular file: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

fn read_or_create_token(cwd: &Path) -> Result<String> {
    let path = ensure_token(cwd)?;
    let token = std::fs::read_to_string(&path)?.trim().to_string();
    if token.is_empty() {
        bail!("CORRUPT_STORE: token file is empty: {}", path.display());
    }
    Ok(token)
}

async fn run_raw_request(cwd: &Path, request: RequestArgs, json_mode: bool) -> Result<()> {
    if !request.path.starts_with('/') || request.path.contains("..") {
        bail!("INVALID_INPUT: API path must be absolute and cannot contain '..'");
    }
    let url = format!("{}{}", request.url.trim_end_matches('/'), request.path);
    let parsed_url = reqwest::Url::parse(&url).context("INVALID_INPUT: parse --url")?;
    let loopback = parsed_url
        .host_str()
        .and_then(|host| host.parse::<std::net::IpAddr>().ok())
        .is_some_and(|address| address.is_loopback());
    if parsed_url.scheme() != "http" || !loopback {
        bail!(
            "INVALID_INPUT: raw requests may send the repository token only to a loopback HTTP address"
        );
    }
    let client = reqwest::Client::builder().no_proxy().build()?;
    let mut builder = match request.method {
        HttpMethod::Get => client.get(&url),
        HttpMethod::Post => client.post(&url),
    };
    let token_path = git::runtime_dir(cwd).join("token");
    if token_path.exists() {
        builder = builder.bearer_auth(read_or_create_token(cwd)?);
    }
    if let Some(body) = request.body {
        builder = builder.json(&serde_json::from_str::<Value>(&body).context("parse --body JSON")?);
    }
    let response = builder.send().await?;
    let status = response.status();
    let value = response.json::<Value>().await?;
    if !status.is_success() {
        bail!("API_ERROR: HTTP {status}: {value}");
    }
    if json_mode {
        write_stdout_line(&serde_json::to_string(&value)?)?;
    } else {
        write_stdout_line(&serde_json::to_string_pretty(&value)?)?;
    }
    Ok(())
}

fn error_code(error: &anyhow::Error) -> String {
    error_code_from_message(&format!("{error:#}"))
}

fn error_code_from_message(message: &str) -> String {
    message
        .split_once(':')
        .map(|(value, _)| value)
        .filter(|value| {
            value
                .chars()
                .all(|character| character.is_ascii_uppercase() || character == '_')
        })
        .unwrap_or("ERROR")
        .to_string()
}
