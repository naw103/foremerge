use crate::model::*;
use crate::{Foremerge, checks};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::IsTerminal;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const CURRENT_PROTOCOL: &str = "2026-07-28";
const LEGACY_PROTOCOL: &str = "2025-11-25";

/// Shown once when a person runs `foremerge mcp` in a terminal.
///
/// Coding agents launch this command with a pipe on stdin, so they never see
/// this. It goes to stderr because stdout carries the protocol.
const INTERACTIVE_NOTICE: &str = "\
foremerge mcp is a machine interface. It speaks JSON-RPC over stdin and stdout and is
meant to be launched by a coding agent, not typed at directly. It is now waiting for a
message on stdin, which is normal.

To wire it into Claude Code, Codex, or Cursor instead:

    foremerge setup all

To read coordination state yourself, use the ordinary CLI:

    foremerge status
    foremerge agent list
    foremerge --help

Press Ctrl-C to exit.
";

pub async fn run_stdio(service: Foremerge) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    // A terminal on stdin means a person is here, not a client. Real clients
    // get byte-identical behaviour because their stdin is a pipe.
    let interactive = std::io::stdin().is_terminal();
    if interactive {
        eprint!("{INTERACTIVE_NOTICE}");
    }
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle_message(&service, message).await,
            Err(error) => {
                if interactive {
                    eprintln!("\n{}", interactive_parse_hint(line.trim()));
                }
                Some(jsonrpc_error(
                    Value::Null,
                    -32700,
                    &format!("parse error: {error}"),
                ))
            }
        };
        if let Some(response) = response {
            let mut encoded = serde_json::to_vec(&response)?;
            encoded.push(b'\n');
            stdout.write_all(&encoded).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

/// Explains a parse failure to a person typing at the terminal.
///
/// Typing a bare tool name is the likely mistake, because tool names are what
/// the agent-facing documentation shows, so that case gets the exact line to
/// paste. The catalog is consulted rather than a hand-written list so the hint
/// cannot drift as tools are added.
fn interactive_parse_hint(input: &str) -> String {
    let Some(tool) = tool_catalog()
        .into_iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(input))
    else {
        return format!(
            "This interface reads one JSON-RPC message per line, so \"{input}\" was not understood.\n\
             If you meant to use Foremerge yourself, run `foremerge --help` instead."
        );
    };

    // Most tools take arguments, so an empty-argument call would be rejected.
    // Only offer a line to paste when that line actually works, which means
    // the tool accepts no input at all. An empty `required` list is not enough:
    // `check_conflicts` declares none yet still demands an intent at runtime.
    let schema = tool.get("inputSchema");
    let required: Vec<&str> = schema
        .and_then(|schema| schema.get("required"))
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let takes_no_input = schema
        .and_then(|schema| schema.get("properties"))
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.is_empty());

    if takes_no_input {
        let call = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": input, "arguments": {} }
        });
        return format!(
            "\"{input}\" is a real tool, but this interface reads JSON-RPC rather than bare\n\
             tool names. The equivalent line to paste is:\n\n    {call}\n\n\
             Reading the same state with `foremerge status` in another terminal is easier."
        );
    }

    let schema_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    });
    let needs = if required.is_empty() {
        "this one takes arguments".to_string()
    } else {
        format!("this one needs arguments: {}", required.join(", "))
    };
    format!(
        "\"{input}\" is a real tool, but this interface reads JSON-RPC rather than bare\n\
         tool names, and {needs}.\n\n\
         To see its full schema, paste:\n\n    {schema_request}\n\n\
         Driving the lifecycle by hand is rarely what you want. `foremerge --help` exposes\n\
         the same operations as ordinary commands."
    )
}

pub async fn handle_message(service: &Foremerge, message: Value) -> Option<Value> {
    let id = message.get("id").cloned()?;
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(LEGACY_PROTOCOL);
            Ok(json!({
                "protocolVersion": if requested == CURRENT_PROTOCOL { CURRENT_PROTOCOL } else { LEGACY_PROTOCOL },
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": server_info(),
                "instructions": "Publish intent and semantic scopes before editing, then claim and start work. Claims are advisory. Resolve durable HIGH conflicts, publish a clean ChangeSet, run a trusted named verification check, and accept before ordinary Git integration. Record the landing commit afterward."
            }))
        }
        "server/discover" => Ok(json!({
            "capabilities": { "tools": {} },
            "protocolVersion": CURRENT_PROTOCOL,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_catalog() })),
        "tools/call" => call_tool(service, params).await,
        _ => {
            return Some(jsonrpc_error(
                id,
                -32601,
                &format!("method not found: {method}"),
            ));
        }
    };
    match result {
        Ok(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "_meta".to_string(),
                    json!({ "io.modelcontextprotocol/serverInfo": server_info() }),
                );
            }
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": value }))
        }
        Err((code, message)) => Some(jsonrpc_error(id, code, &message)),
    }
}

#[derive(Debug, Deserialize)]
struct StartWorkToolRequest {
    agent_id: String,
    intent_id: String,
}

#[derive(Debug, Deserialize)]
struct RunVerificationToolRequest {
    changeset_id: String,
    check: String,
}

#[derive(Debug, Deserialize)]
struct ResolveConflictToolRequest {
    conflict_id: String,
    agent_id: String,
    resolution: String,
    rationale: String,
}

#[derive(Debug, Deserialize)]
struct AcceptChangeSetToolRequest {
    changeset_id: String,
    #[serde(default)]
    git_ref: Option<String>,
    #[serde(default)]
    allow_high_conflicts: bool,
    #[serde(default)]
    override_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscardWorkToolRequest {
    agent_id: String,
    intent_id: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct RecordCommitToolRequest {
    changeset_id: String,
    git_ref: String,
}

#[derive(Debug, Deserialize)]
struct IdToolRequest {
    id: String,
}

/// Resolve a named check from the registry of the repository this service's
/// store is bound to. MCP callers never influence which registry is trusted:
/// neither the server's spawn directory nor tool arguments select it.
fn trusted_check(service: &Foremerge, name: &str) -> anyhow::Result<checks::NamedCheck> {
    let common_dir = service.repository_common_dir()?.ok_or_else(|| {
        anyhow::anyhow!(
            "INVALID_INPUT: verification checks are repository-scoped and this coordination store is not bound to a Git repository yet; register an agent with a worktree inside the repository first"
        )
    })?;
    checks::get_at(&checks::registry_path(&common_dir), name)
}

async fn call_tool(service: &Foremerge, params: Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| (-32602, "tools/call requires a name".to_string()))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let outcome: anyhow::Result<Value> = match name {
        "register_agent" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<RegisterAgentRequest>(arguments)
                    .and_then(|request| service.register_agent(request))
                    .and_then(to_value)
            })
            .await
        }
        "publish_intent" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<PublishIntentRequest>(arguments)
                    .and_then(|request| service.publish_intent(request))
                    .and_then(to_value)
            })
            .await
        }
        "claim_work" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<ClaimWorkRequest>(arguments)
                    .and_then(|request| service.claim_work(request))
                    .and_then(to_value)
            })
            .await
        }
        "query_work" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<WorkQuery>(arguments)
                    .and_then(|request| service.query_work(request))
                    .and_then(to_value)
            })
            .await
        }
        "check_conflicts" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<ConflictCheckRequest>(arguments)
                    .and_then(|request| service.check_conflicts(request))
                    .and_then(to_value)
            })
            .await
        }
        "publish_changeset" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<PublishChangeSetRequest>(arguments)
                    .and_then(|request| service.publish_changeset(request))
                    .and_then(to_value)
            })
            .await
        }
        "coordinate_with_agent" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<CoordinateRequest>(arguments)
                    .and_then(|request| service.coordinate_with_agent(request))
                    .and_then(to_value)
            })
            .await
        }
        "start_work" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<StartWorkToolRequest>(arguments)
                    .and_then(|request| service.start_work(&request.agent_id, &request.intent_id))
                    .and_then(to_value)
            })
            .await
        }
        "run_verification" => match parse::<RunVerificationToolRequest>(arguments) {
            Ok(request) => {
                let check_service = service.clone();
                let check_name = request.check.clone();
                match mcp_blocking(move || trusted_check(&check_service, &check_name)).await {
                    Ok(check) => service
                        .validate_changeset(
                            &request.changeset_id,
                            ValidationRequest {
                                command: check.command,
                                worktree: None,
                                timeout_seconds: check.timeout_seconds,
                            },
                        )
                        .await
                        .and_then(to_value),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        },
        "resolve_conflict" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<ResolveConflictToolRequest>(arguments)
                    .and_then(|request| {
                        let parties = service.conflict_party_agents(&request.conflict_id)?;
                        if !parties.iter().any(|party| party == &request.agent_id) {
                            anyhow::bail!(
                                "FORBIDDEN: over MCP a conflict may be resolved only by an agent whose intent is a party to it, after real agreement; coordinate with the parties via coordinate_with_agent or ask a human operator to resolve it from the CLI"
                            );
                        }
                        service.resolve_conflict(
                            &request.conflict_id,
                            ResolveConflictRequest {
                                agent_id: request.agent_id,
                                resolution: request.resolution,
                                rationale: request.rationale,
                            },
                        )
                    })
                    .and_then(to_value)
            })
            .await
        }
        "accept_changeset" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<AcceptChangeSetToolRequest>(arguments)
                    .and_then(|request| {
                        if request.allow_high_conflicts || request.override_reason.is_some() {
                            anyhow::bail!(
                                "FORBIDDEN: explicit HIGH-conflict overrides are CLI-only operator actions and are not accepted over MCP; ask a human operator to review the conflict and run the override from the CLI"
                            );
                        }
                        service.accept_changeset(
                            &request.changeset_id,
                            AcceptRequest {
                                git_ref: request.git_ref,
                                allow_high_conflicts: false,
                                // Agents never override. Accepting work with
                                // nothing to verify is governed by the
                                // repository's acceptance policy, which a human
                                // sets once, not by the agent asking nicely.
                                allow_unverified: false,
                                override_reason: None,
                            },
                        )
                    })
                    .and_then(to_value)
            })
            .await
        }
        "discard_work" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<DiscardWorkToolRequest>(arguments)
                    .and_then(|request| {
                        service.discard_work(&request.agent_id, &request.intent_id, &request.reason)
                    })
                    .and_then(to_value)
            })
            .await
        }
        "record_commit" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<RecordCommitToolRequest>(arguments)
                    .and_then(|request| {
                        service.record_commit(&request.changeset_id, &request.git_ref)
                    })
                    .and_then(to_value)
            })
            .await
        }
        "list_agents" => {
            let service = service.clone();
            mcp_blocking(move || service.list_agents().and_then(to_value)).await
        }
        "get_intent" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<IdToolRequest>(arguments)
                    .and_then(|request| service.show_intent(&request.id))
                    .and_then(to_value)
            })
            .await
        }
        "get_changeset" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<IdToolRequest>(arguments)
                    .and_then(|request| service.get_changeset(&request.id))
                    .and_then(to_value)
            })
            .await
        }
        "status" => {
            let service = service.clone();
            mcp_blocking(move || service.status().and_then(to_value)).await
        }
        _ => return Err((-32602, format!("unknown tool: {name}"))),
    };
    match outcome {
        Ok(value) => Ok(tool_result(value, false)),
        Err(error) => Ok(tool_result(json!({ "error": format!("{error:#}") }), true)),
    }
}

async fn mcp_blocking<T, F>(operation: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| anyhow::anyhow!("blocking coordinator operation failed: {error}"))?
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> anyhow::Result<T> {
    serde_json::from_value(value).map_err(Into::into)
}

fn to_value<T: serde::Serialize>(value: T) -> anyhow::Result<Value> {
    serde_json::to_value(value).map_err(Into::into)
}

fn tool_result(value: Value, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string()) }],
        "structuredContent": value,
        "isError": is_error,
    })
}

fn jsonrpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
        "_meta": { "io.modelcontextprotocol/serverInfo": server_info() },
    })
}

fn server_info() -> Value {
    json!({ "name": "foremerge", "version": env!("CARGO_PKG_VERSION") })
}

fn scope_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["symbol", "api", "schema", "config", "infra", "test", "migration", "env", "file", "component", "contract", "domain"]
            },
            "key": { "type": "string", "minLength": 1 }
        },
        "required": ["kind", "key"],
        "additionalProperties": false
    })
}

fn tool(
    name: &str,
    title: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
    idempotent: bool,
) -> Value {
    // Tool results intentionally keep their domain shape: most are objects,
    // while query_work is an array. Omit the optional outputSchema instead of
    // advertising a false common object contract.
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        },
        "annotations": {
            "readOnlyHint": read_only,
            "destructiveHint": false,
            "idempotentHint": idempotent,
            "openWorldHint": false
        }
    })
}

fn with_input_any_of(mut definition: Value, any_of: Value) -> Value {
    definition["inputSchema"]["anyOf"] = any_of;
    definition
}

fn with_destructive_hint(mut definition: Value) -> Value {
    definition["annotations"]["destructiveHint"] = Value::Bool(true);
    definition
}

pub fn tool_catalog() -> Vec<Value> {
    let scope = scope_schema();
    vec![
        tool(
            "accept_changeset",
            "Accept a validated ChangeSet",
            "Apply Foremerge's final conflict, dependency, fingerprint, validation, and Git gates, then pin the accepted commit. Explicit HIGH-conflict overrides (allow_high_conflicts, override_reason) are CLI-only operator actions and are rejected over MCP; ask a human operator instead.",
            json!({
                "changeset_id": { "type": "string", "minLength": 1 },
                "git_ref": { "type": "string", "minLength": 1 }
            }),
            &["changeset_id"],
            false,
            false,
        ),
        with_input_any_of(
            tool(
                "check_conflicts",
                "Check intent conflicts",
                "Compare a published or proposed intent with active work before code changes exist.",
                json!({
                    "agent_id": { "type": "string" },
                    "intent_id": { "type": "string", "minLength": 1 },
                    "intent": { "type": "string", "minLength": 1 },
                    "scopes": { "type": "array", "items": scope.clone(), "default": [] }
                }),
                &[],
                true,
                false,
            ),
            json!([
                { "required": ["intent_id"] },
                { "required": ["intent"] }
            ]),
        ),
        tool(
            "claim_work",
            "Claim semantic work",
            "Create advisory, leased claims on symbols, APIs, schemas, config, infra, tests, migrations, env vars, files, contracts, components, or domains. Overlap warns but never locks.",
            json!({
                "agent_id": { "type": "string" },
                "intent_id": { "type": "string" },
                "scopes": { "type": "array", "items": scope.clone(), "minItems": 1 },
                "reason": { "type": "string" },
                "lease_seconds": { "type": "integer", "minimum": 60, "maximum": 86400, "default": 3600 }
            }),
            &["agent_id", "intent_id", "scopes"],
            false,
            false,
        ),
        tool(
            "coordinate_with_agent",
            "Coordinate with another agent",
            "Send a durable directed message linked to a conflict or ChangeSet.",
            json!({
                "from_agent_id": { "type": "string" },
                "to_agent_id": { "type": "string" },
                "message": { "type": "string", "minLength": 1 },
                "conflict_id": { "type": "string" },
                "changeset_id": { "type": "string" }
            }),
            &["from_agent_id", "to_agent_id", "message"],
            false,
            false,
        ),
        with_destructive_hint(tool(
            "discard_work",
            "Discard work",
            "Discard a nonterminal intent, release its claims, and dismiss conflicts linked to the discarded work.",
            json!({
                "agent_id": { "type": "string", "minLength": 1 },
                "intent_id": { "type": "string", "minLength": 1 },
                "reason": { "type": "string", "minLength": 1 }
            }),
            &["agent_id", "intent_id", "reason"],
            false,
            false,
        )),
        tool(
            "get_changeset",
            "Get a ChangeSet",
            "Read one ChangeSet, including immutable accepted and integration commit provenance.",
            json!({ "id": { "type": "string", "minLength": 1 } }),
            &["id"],
            true,
            true,
        ),
        tool(
            "get_intent",
            "Get an intent",
            "Read one intent with its agent and current open-conflict snapshot.",
            json!({ "id": { "type": "string", "minLength": 1 } }),
            &["id"],
            true,
            true,
        ),
        tool(
            "list_agents",
            "List coding agents",
            "Read every registered coding agent in deterministic registration order.",
            json!({}),
            &[],
            true,
            true,
        ),
        tool(
            "publish_changeset",
            "Publish a provisional ChangeSet",
            "Capture implementation summary, affected files/symbols/contracts, dependencies, tests, decisions, provenance, and Git state.",
            json!({
                "agent_id": { "type": "string" },
                "intent_id": { "type": "string" },
                "summary": { "type": "string", "minLength": 1 },
                "files": { "type": "array", "items": { "type": "string" }, "default": [] },
                "symbols": { "type": "array", "items": { "type": "string" }, "default": [] },
                "contracts": { "type": "array", "items": { "type": "string" }, "default": [] },
                "dependencies": { "type": "array", "items": { "type": "string" }, "default": [] },
                "tests": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string" },
                            "status": { "type": "string" },
                            "summary": { "type": "string" }
                        },
                        "required": ["command", "status"],
                        "additionalProperties": false
                    },
                    "default": []
                },
                "decisions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" },
                            "rationale": { "type": "string" },
                            "alternatives": {
                                "type": "array",
                                "items": { "type": "string" },
                                "default": []
                            }
                        },
                        "required": ["title", "rationale"],
                        "additionalProperties": false
                    },
                    "default": []
                },
                "provenance": { "type": "object", "default": {} },
                "git_ref": { "type": "string" },
                "base_ref": {
                    "type": "string",
                    "minLength": 1,
                    "description": "True diff base when known (for example the fork point of this agent branch); defaults to the candidate commit's first parent."
                },
                "worktree": { "type": "string" }
            }),
            &["agent_id", "intent_id", "summary"],
            false,
            false,
        ),
        tool(
            "publish_intent",
            "Publish intent",
            "Publish task intent, semantic scopes, and dependencies; immediately returns pre-code conflicts.",
            json!({
                "agent_id": { "type": "string" },
                "task": { "type": "string", "minLength": 1 },
                "summary": { "type": "string", "minLength": 1 },
                "rationale": { "type": "string" },
                "scopes": { "type": "array", "items": scope.clone(), "default": [] },
                "depends_on": { "type": "array", "items": { "type": "string" }, "default": [] },
                "metadata": { "type": "object", "default": {} }
            }),
            &["agent_id", "task", "summary"],
            false,
            false,
        ),
        tool(
            "query_work",
            "Query active work",
            "Find owners, intents, semantic claims, pending ChangeSets, and conflict counts.",
            json!({
                "agent_id": { "type": "string" },
                "status": { "type": "string" },
                "scope": scope,
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }
            }),
            &[],
            true,
            true,
        ),
        tool(
            "record_commit",
            "Record integration commit",
            "After ordinary Git or PR integration, record the durable target commit while preserving the immutable accepted commit.",
            json!({
                "changeset_id": { "type": "string", "minLength": 1 },
                "git_ref": { "type": "string", "minLength": 1 }
            }),
            &["changeset_id", "git_ref"],
            false,
            false,
        ),
        tool(
            "register_agent",
            "Register coding agent",
            "Register an agent/model and its isolated Git worktree in the shared coordination graph.",
            json!({
                "name": { "type": "string", "minLength": 1 },
                "model": { "type": "string" },
                "capabilities": { "type": "array", "items": { "type": "string" }, "default": [] },
                "worktree": { "type": "string" }
            }),
            &["name"],
            false,
            false,
        ),
        tool(
            "resolve_conflict",
            "Resolve a persisted conflict",
            "Record an audited resolution decision for a durable cfl_* conflict so blocked work can proceed. Over MCP only an agent whose intent is a party to the conflict may resolve it, after real agreement with the other party (name the coordination message in the rationale); the decision is recorded under the resolver's agent id.",
            json!({
                "conflict_id": { "type": "string", "pattern": "^cfl_" },
                "agent_id": { "type": "string", "minLength": 1 },
                "resolution": { "type": "string", "minLength": 1 },
                "rationale": { "type": "string", "minLength": 1 }
            }),
            &["conflict_id", "agent_id", "resolution", "rationale"],
            false,
            false,
        ),
        tool(
            "run_verification",
            "Run a trusted verification check",
            "Run one named check from the trusted Foremerge registry of the repository this store is bound to. Raw commands are intentionally not accepted over MCP, and the registry cannot be selected by the caller or the server's working directory.",
            json!({
                "changeset_id": { "type": "string", "minLength": 1 },
                "check": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 64,
                    "pattern": "^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$"
                }
            }),
            &["changeset_id", "check"],
            false,
            false,
        ),
        tool(
            "start_work",
            "Start claimed work",
            "Advance an agent's claimed intent into IN_PROGRESS before implementation.",
            json!({
                "agent_id": { "type": "string", "minLength": 1 },
                "intent_id": { "type": "string", "minLength": 1 }
            }),
            &["agent_id", "intent_id"],
            false,
            false,
        ),
        tool(
            "status",
            "Read coordinator status",
            "Read one consistent snapshot of active agents, lifecycle groups, claims, conflicts, and ChangeSets.",
            json!({}),
            &[],
            true,
            true,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[test]
    fn a_bare_tool_name_is_answered_with_the_line_that_would_have_worked() {
        let hint = interactive_parse_hint("list_agents");
        // The pasteable line has to be a real request, not prose about one.
        let line = hint
            .lines()
            .find(|line| line.trim_start().starts_with('{'))
            .expect("hint offers a JSON-RPC line");
        let parsed: Value = serde_json::from_str(line.trim()).expect("the line is valid JSON");
        assert_eq!(parsed["method"], "tools/call");
        assert_eq!(parsed["params"]["name"], "list_agents");
    }

    #[test]
    fn every_catalog_tool_is_recognised_by_the_hint() {
        // Guards against the hint drifting as tools are added or renamed.
        for tool in tool_catalog() {
            let name = tool["name"].as_str().unwrap();
            assert!(
                interactive_parse_hint(name).contains("is a real tool"),
                "{name} was not recognised"
            );
        }
    }

    /// The previous version of this test only checked that the offered line was
    /// syntactically valid JSON, which it was, while being rejected at runtime
    /// for every tool that takes required arguments. Execute it instead.
    #[tokio::test]
    async fn every_line_the_hint_offers_actually_succeeds() {
        let service = Foremerge::new(Store::in_memory().unwrap());
        for tool in tool_catalog() {
            let name = tool["name"].as_str().unwrap().to_string();
            let required = tool
                .get("inputSchema")
                .and_then(|schema| schema.get("required"))
                .and_then(Value::as_array)
                .map(|names| names.len())
                .unwrap_or(0);

            let hint = interactive_parse_hint(&name);
            let line = hint
                .lines()
                .find(|line| line.trim_start().starts_with('{'))
                .unwrap_or_else(|| panic!("{name}: hint offered no request at all"));
            let message: Value =
                serde_json::from_str(line.trim()).expect("the offered line is valid JSON");

            // A tools/call may only be offered when it needs no arguments.
            if message["method"] == "tools/call" {
                assert_eq!(
                    required, 0,
                    "{name} requires {required} argument(s) but was offered as a bare call"
                );
            }

            let response = handle_message(&service, message)
                .await
                .unwrap_or_else(|| panic!("{name}: no response"));
            assert!(
                response.get("error").is_none(),
                "{name}: offered line returned a protocol error: {response}"
            );
            assert!(
                !response["result"]["isError"].as_bool().unwrap_or(false),
                "{name}: offered line failed at runtime: {}",
                response["result"]["structuredContent"]
            );
        }
    }

    #[test]
    fn tools_needing_arguments_name_them_instead_of_offering_a_broken_call() {
        // publish_intent requires agent_id, task and summary.
        let hint = interactive_parse_hint("publish_intent");
        for field in ["agent_id", "task", "summary"] {
            assert!(hint.contains(field), "hint should name {field}:\n{hint}");
        }
        assert!(
            !hint.contains("tools/call"),
            "a tool needing arguments must not be offered as a call:\n{hint}"
        );
    }

    #[test]
    fn unrecognised_input_is_pointed_at_the_command_line_interface() {
        let hint = interactive_parse_hint("hello");
        assert!(hint.contains("foremerge --help"));
        assert!(!hint.contains("is a real tool"));
    }

    #[test]
    fn terminal_guidance_carries_no_em_dashes() {
        assert!(!INTERACTIVE_NOTICE.contains('\u{2014}'));
        assert!(!interactive_parse_hint("list_agents").contains('\u{2014}'));
        assert!(!interactive_parse_hint("hello").contains('\u{2014}'));
    }

    #[tokio::test]
    async fn lists_the_complete_lifecycle_tools_in_deterministic_order() {
        let service = Foremerge::new(Store::in_memory().unwrap());
        let response = handle_message(
            &service,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }),
        )
        .await
        .unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();
        let names = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "accept_changeset",
                "check_conflicts",
                "claim_work",
                "coordinate_with_agent",
                "discard_work",
                "get_changeset",
                "get_intent",
                "list_agents",
                "publish_changeset",
                "publish_intent",
                "query_work",
                "record_commit",
                "register_agent",
                "resolve_conflict",
                "run_verification",
                "start_work",
                "status",
            ]
        );
        let named = |name: &str| {
            tools
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("missing tool {name}"))
        };
        assert!(tools.iter().all(|tool| tool.get("outputSchema").is_none()));
        assert_eq!(
            named("check_conflicts")["annotations"]["readOnlyHint"],
            true
        );
        assert_eq!(
            named("check_conflicts")["annotations"]["idempotentHint"],
            false
        );
        assert_eq!(
            named("check_conflicts")["inputSchema"]["anyOf"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            named("publish_changeset")["inputSchema"]["properties"]["tests"]["items"]["required"],
            json!(["command", "status"])
        );
        assert_eq!(
            named("publish_changeset")["inputSchema"]["properties"]["decisions"]["items"]["required"],
            json!(["title", "rationale"])
        );
        assert_eq!(
            named("discard_work")["annotations"]["destructiveHint"],
            true
        );
        for name in [
            "get_changeset",
            "get_intent",
            "list_agents",
            "query_work",
            "status",
        ] {
            assert_eq!(named(name)["annotations"]["readOnlyHint"], true);
            assert_eq!(named(name)["annotations"]["idempotentHint"], true);
        }
        assert!(
            named("run_verification")["inputSchema"]["properties"]
                .get("command")
                .is_none()
        );
        assert_eq!(
            named("run_verification")["inputSchema"]["properties"]["check"]["type"],
            "string"
        );
        assert_eq!(
            named("run_verification")["inputSchema"]["properties"]["check"]["pattern"],
            "^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$"
        );
    }

    #[tokio::test]
    async fn supports_legacy_initialize_and_direct_stateless_calls() {
        let service = Foremerge::new(Store::in_memory().unwrap());
        let initialized = handle_message(
            &service,
            json!({
                "jsonrpc": "2.0",
                "id": "init",
                "method": "initialize",
                "params": { "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } }
            }),
        )
        .await
        .unwrap();
        assert_eq!(initialized["result"]["protocolVersion"], LEGACY_PROTOCOL);

        let registered = handle_message(
            &service,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": "register_agent", "arguments": { "name": "mcp-agent", "model": "test" } }
            }),
        )
        .await
        .unwrap();
        assert_eq!(registered["result"]["isError"], false);
        assert!(
            registered["result"]["structuredContent"]["id"]
                .as_str()
                .unwrap()
                .starts_with("agt_")
        );
    }
}
