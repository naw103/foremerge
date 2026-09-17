use crate::model::*;
use crate::{Foremerge, checks};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The one MCP revision this server implements.
///
/// It used to answer `2026-07-28` to a client that asked for it, while
/// speaking this revision's wire shape: `initialize` and `ping` exist, results
/// carry no `resultType`, and `server/discover` returned neither
/// `supportedVersions` nor the caching hints that revision requires. A client
/// that speaks only the newer era would have taken that answer at face value
/// and continued against a server that cannot hold up its end.
const PROTOCOL: &str = "2025-11-25";

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

/// What a server with a working store tells the agent during initialize.
const INSTRUCTIONS: &str = "Publish intent and the scopes you will change, declaring what you do to each, before editing. publish_intent returns related_work: assess each entry and call record_assessment before writing code. then claim and start work. Claims are advisory. Resolve durable HIGH conflicts, publish a clean ChangeSet, run a trusted named verification check, and accept before ordinary Git integration. Record the landing commit afterward.";

/// Why the MCP server is running without its coordination store.
///
/// Exiting when the store cannot be opened is reported by clients only as a
/// closed connection: the tools never appear, nothing reaches the agent, and
/// the agent carries on uncoordinated without saying so. A ledger migrated by
/// a newer build went unnoticed that way for days. So the server stays up,
/// completes the handshake, and puts this in front of the agent instead, in
/// the initialize instructions and in the result of every tool call.
#[derive(Debug, Clone)]
pub struct StoreUnavailable {
    code: String,
    message: String,
    database: String,
    remedy: String,
}

impl StoreUnavailable {
    /// `code` is the open error's typed prefix, such as `UNSUPPORTED_SCHEMA`,
    /// and `message` its full text.
    pub fn new(code: impl Into<String>, message: impl Into<String>, database: &Path) -> Self {
        let code = code.into();
        let remedy = remedy(&code);
        Self {
            code,
            message: message.into(),
            database: database.display().to_string(),
            remedy,
        }
    }

    fn instructions(&self) -> String {
        format!(
            "Foremerge unavailable: {}. The server is running, but it could not open the coordination ledger at {}, so every Foremerge tool returns this error and this session is not coordinated with other agents. {UNAVAILABLE_GUIDANCE} For the user: {}",
            self.message.trim_end().trim_end_matches('.'),
            self.database,
            self.remedy
        )
    }

    /// Answer a tool call with the reason instead of a result. An unknown tool
    /// is still a protocol error, exactly as when the store is available.
    fn tool_call(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| (-32602, "tools/call requires a name".to_string()))?;
        if !tool_catalog().iter().any(|tool| tool["name"] == name) {
            return Err((-32602, format!("unknown tool: {name}")));
        }
        Ok(tool_result(
            json!({
                "error": format!("Foremerge unavailable: {}", self.message),
                "code": self.code,
                "message": self.message,
                "database": self.database,
                "guidance": UNAVAILABLE_GUIDANCE,
                "remedy": self.remedy,
            }),
            true,
        ))
    }
}

/// What an agent does on meeting an unavailable store. Tool results carry it
/// as well as the instructions, because not every client shows the agent the
/// instructions, and the remedy is addressed to whoever configures the client.
const UNAVAILABLE_GUIDANCE: &str = "Tell the user and leave the fix to them: do not delete, move, or edit the ledger, and do not change the MCP client configuration.";

/// What the operator does about a store the server could not open.
fn remedy(code: &str) -> String {
    const RELAUNCH: &str = "then restart the client session so it relaunches `foremerge mcp`";
    if code == "NOT_INITIALIZED" {
        // Initializing a repository is the operator's decision, so the server
        // says what to run rather than running it. A client starting is not
        // permission to opt a repository into coordination.
        format!(
            "Run `foremerge init` in this repository if you want its agents coordinated, {RELAUNCH}. Until then Foremerge is inactive here, which is a valid state: nothing else is wrong."
        )
    } else if code == "UNSUPPORTED_SCHEMA" {
        // Name the binary: the client may launch a different one than the
        // `foremerge` on the operator's PATH, and the version alone cannot
        // tell a development build from the release it will become.
        let binary = std::env::current_exe().map_or_else(
            |_| "foremerge".to_string(),
            |path| path.display().to_string(),
        );
        format!(
            "This server is {binary} (version {}). Upgrade the binary this client launches to a Foremerge release that supports the ledger's schema, {RELAUNCH}. If no release supports that schema yet, a development build has migrated this ledger.",
            env!("CARGO_PKG_VERSION")
        )
    } else {
        format!(
            "Run `foremerge doctor` in the repository to diagnose the ledger and fix the cause, {RELAUNCH}. If the cause was transient, such as another process holding the database lock, the restart alone is enough."
        )
    }
}

/// The state the server answers from.
#[derive(Clone, Copy)]
enum Backend<'a> {
    Ready(&'a Foremerge),
    Unavailable(&'a StoreUnavailable),
}

pub async fn run_stdio(service: Foremerge) -> anyhow::Result<()> {
    serve_stdio(Backend::Ready(&service)).await
}

/// Serve MCP without a store, so the client can still tell the agent why.
pub async fn run_stdio_unavailable(unavailable: StoreUnavailable) -> anyhow::Result<()> {
    serve_stdio(Backend::Unavailable(&unavailable)).await
}

async fn serve_stdio(backend: Backend<'_>) -> anyhow::Result<()> {
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
            Ok(message) => respond(backend, message).await,
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
    respond(Backend::Ready(service), message).await
}

async fn respond(backend: Backend<'_>, message: Value) -> Option<Value> {
    let id = message.get("id").cloned()?;
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => {
            // Answered with what this server speaks, whatever was asked for.
            // Echoing a client's newer version back is how the mismatch
            // started.
            let instructions = match backend {
                Backend::Ready(_) => INSTRUCTIONS.to_string(),
                Backend::Unavailable(unavailable) => unavailable.instructions(),
            };
            Ok(json!({
                "protocolVersion": PROTOCOL,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": server_info(),
                "instructions": instructions
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_catalog() })),
        "tools/call" => match backend {
            Backend::Ready(service) => call_tool(service, params).await,
            Backend::Unavailable(unavailable) => unavailable.tool_call(&params),
        },
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
    /// Accepted into the struct only so it can be refused by name. Without the
    /// field serde drops it silently, and an agent sending it would get a
    /// normal acceptance while the documentation promised a refusal.
    #[serde(default)]
    allow_unverified: bool,
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
        "record_assessment" => {
            let service = service.clone();
            mcp_blocking(move || {
                parse::<RecordAssessmentRequest>(arguments)
                    .and_then(|request| service.record_assessment(request))
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
                        if request.allow_high_conflicts
                            || request.allow_unverified
                            || request.override_reason.is_some()
                        {
                            anyhow::bail!(
                                "FORBIDDEN: acceptance overrides are operator actions and are not accepted over MCP; ask a human operator to review and run the override from the CLI, or from the HTTP API with the daemon token"
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
    let mut result = json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string()) }],
        "isError": is_error,
    });
    // MCP defines structuredContent as a JSON object, and clients that validate
    // results (Claude Code among them) reject the whole response when it holds
    // anything else. query_work and list_agents answer with arrays, so a
    // non-object result travels in the text block alone.
    if value.is_object() {
        result["structuredContent"] = value;
    }
    result
}

/// A JSON-RPC 2.0 error response: `jsonrpc`, `id` and `error`, and nothing
/// else.
///
/// It used to carry a top-level `_meta` with the server's identity. MCP puts
/// `_meta` inside a result or params object, never beside `error`, and strict
/// clients validate the envelope: the official TypeScript client rejects the
/// extra member as an unrecognized key. That made its `server/discover` probe
/// look unanswered, so instead of falling back to `initialize` at once it
/// waited out the whole probe timeout, sixty seconds by default, before
/// connecting.
fn jsonrpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn server_info() -> Value {
    json!({ "name": "foremerge", "version": env!("CARGO_PKG_VERSION") })
}

/// A scope an intent declares, together with what it will do to that scope.
///
/// The operation is required because the agent knows it and Foremerge cannot
/// reliably recover it from prose. Declaring it is what lets a conflict be
/// stated as fact rather than guessed from wording.
fn scope_claim_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["symbol", "api", "schema", "config", "infra", "test", "migration", "env", "file", "component", "contract", "domain"],
                "description": SCOPE_KIND_DESCRIPTION
            },
            "key": { "type": "string", "minLength": 1, "description": SCOPE_KEY_DESCRIPTION },
            "operation": {
                "type": "string",
                "enum": ["add", "extend", "modify", "replace", "remove", "rename", "migrate"],
                "description": "What this intent does to this scope. add/extend/modify preserve what other work depends on; replace/remove/rename/migrate do not."
            }
        },
        "required": ["kind", "key", "operation"],
        "additionalProperties": false
    })
}

const SCOPE_KIND_DESCRIPTION: &str = "What sort of thing the scope names: a code symbol, an API route, a data schema, a config key, infrastructure, a test, a migration, an environment variable, a file path, a component, a contract, or a broader domain.";
const SCOPE_KEY_DESCRIPTION: &str = "The name within that kind, for example PaymentService, POST /orders, or src/billing.rs. Comparison is case-insensitive.";

fn scope_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["symbol", "api", "schema", "config", "infra", "test", "migration", "env", "file", "component", "contract", "domain"],
                "description": SCOPE_KIND_DESCRIPTION
            },
            "key": { "type": "string", "minLength": 1, "description": SCOPE_KEY_DESCRIPTION }
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
    let scope_claim = scope_claim_schema();
    vec![
        tool(
            "accept_changeset",
            "Accept a validated ChangeSet",
            "Accept a ChangeSet as done so other agents may build on it. Use this after run_verification passes; use record_commit later, once the work has actually landed on the target branch. Foremerge re-checks every gate before accepting: the worktree must be clean and unchanged since publication, verification must have passed (unless the repository's policy is advisory and nothing was verified), no HIGH conflict on the intent may be OPEN or COORDINATING (only resolve_conflict clears one), and every intent in the ChangeSet's dependencies must already be accepted, with its accepted commit in this commit's history. On success it marks the ChangeSet and intent ACCEPTED, pins the commit as accepted_commit, writes refs/foremerge/accepted/<changeset_id>, and returns the updated ChangeSet. A failed gate returns an error naming it (CHECK_FAILED, BLOCKING_CONFLICT, UNSATISFIED_DEPENDENCY, STALE_CHANGESET, INVALID_TRANSITION) and changes nothing. Overriding a gate is an operator action on the CLI or HTTP API and is refused over MCP, so ask a human when a gate should be bypassed.",
            json!({
                "changeset_id": { "type": "string", "minLength": 1, "description": "The ChangeSet to accept (chg_...), as returned by publish_changeset." },
                "git_ref": { "type": "string", "minLength": 1, "description": "Optional commit to accept. Defaults to the ref recorded at publication, then the worktree HEAD. Whatever it names must resolve to the current worktree HEAD." }
            }),
            &["changeset_id"],
            false,
            false,
        ),
        with_input_any_of(
            tool(
                "check_conflicts",
                "Check intent conflicts",
                "Ask whether planned work collides with other agents' active work, before any code changes exist. Read-only: nothing is stored. Pass intent_id alone to list the persisted OPEN or COORDINATING conflicts on a published intent (use this before publish_changeset and before run_verification, since conflicts are raised when the later intent publishes). Pass intent text with scopes, or intent_id with replacement scopes, for a what-if check against every non-terminal intent; those findings carry ephemeral eph_ ids and are not recorded. Returns conflicts (each with severity, explanation, evidence and a suggested coordination step), checked_intents, blocking (true when any finding is HIGH), and the active policy. To record a decision about overlap, use record_assessment or resolve_conflict instead.",
                json!({
                    "agent_id": { "type": "string", "description": "Optional caller agent id. Informational only; it does not change which work is compared." },
                    "intent_id": { "type": "string", "minLength": 1, "description": "A published intent (int_...) to check. Provide this or intent." },
                    "intent": { "type": "string", "minLength": 1, "description": "Free-form summary of work not yet published, for a what-if check. Provide this or intent_id. Passing an intent id here is rejected; use intent_id." },
                    "scopes": { "type": "array", "items": scope_claim.clone(), "default": [], "description": "Scopes to compare, each with the operation you would perform. With intent text, these are the proposed scopes. With intent_id, non-empty scopes replace the intent's own for this check only." }
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
            "Tell other agents you are working on specific scopes of your own intent, for a limited time. Use this after publish_intent and record_assessment, and before start_work. Claims are advisory and never lock: if any other intent, another of yours included, holds a live claim on a matching scope (symbols match loosely by name), you still get the claim, and a claim-overlap conflict is recorded and returned as a warning. The first claim moves the intent from INTENT to CLAIMED. Claiming a scope the intent already holds renews its lease instead of adding a second claim, which is how long-running work keeps its claims. Only the intent's owner may claim, and only while the intent is INTENT, CLAIMED or IN_PROGRESS. Returns the created or renewed claims, overlap warnings, and advisory_only: true.",
            json!({
                "agent_id": { "type": "string", "description": "Your agent id (agt_...). Must own the intent." },
                "intent_id": { "type": "string", "description": "Your intent (int_...) that the claims belong to." },
                "scopes": { "type": "array", "items": scope.clone(), "minItems": 1, "description": "The scopes to claim. Use the scopes declared on the intent; operations are not repeated here." },
                "reason": { "type": "string", "description": "Optional note shown to other agents explaining why you hold the claim." },
                "lease_seconds": { "type": "integer", "minimum": 60, "maximum": 86400, "default": 3600, "description": "How long the claim lasts before it expires, from 60 seconds to 24 hours. Default one hour. Claim again to renew." }
            }),
            &["agent_id", "intent_id", "scopes"],
            false,
            false,
        ),
        tool(
            "coordinate_with_agent",
            "Coordinate with another agent",
            "Send a stored message to another registered agent, usually to agree how two overlapping pieces of work should coexist. Use it when check_conflicts or related_work shows a clash you need the other agent to act on; afterwards, a party to the conflict records the agreement with resolve_conflict, naming this message's id. Linking a conflict_id moves that conflict from OPEN to COORDINATING. The message is appended to a durable log with status UNREAD; it does not interrupt or control the other agent. No MCP tool reads messages: the recipient reads them with the CLI, foremerge coordinate inbox. Returns the stored message, including its msg_ id.",
            json!({
                "from_agent_id": { "type": "string", "description": "Your agent id (agt_...)." },
                "to_agent_id": { "type": "string", "description": "The recipient's agent id (agt_...), for example the owner of the conflicting intent." },
                "message": { "type": "string", "minLength": 1, "description": "What you propose or need, in plain language." },
                "conflict_id": { "type": "string", "description": "Optional stored conflict (cfl_...) this message is about. An eph_ id from a what-if check_conflicts is rejected with NOT_FOUND. May be combined with changeset_id." },
                "changeset_id": { "type": "string", "description": "Optional ChangeSet (chg_...) this message is about. Must exist." }
            }),
            &["from_agent_id", "to_agent_id", "message"],
            false,
            false,
        ),
        with_destructive_hint(tool(
            "discard_work",
            "Discard work",
            "Abandon one of your own intents that will not be finished, for example a duplicate or work another agent took over. It sets the intent to DISCARDED, releases all of its active claims, and dismisses its OPEN or COORDINATING conflicts so they stop blocking the other party. The history stays in the event log, but a discarded intent cannot be resumed; publish a new intent instead. Only the owner may discard, and not once the intent is ACCEPTED, COMMITTED or already DISCARDED. Returns the updated intent. To agree that two intents can coexist, use resolve_conflict instead.",
            json!({
                "agent_id": { "type": "string", "minLength": 1, "description": "Your agent id (agt_...). Must own the intent." },
                "intent_id": { "type": "string", "minLength": 1, "description": "The intent (int_...) to discard." },
                "reason": { "type": "string", "minLength": 1, "description": "Why the work is being abandoned. Required and recorded in the event log." }
            }),
            &["agent_id", "intent_id", "reason"],
            false,
            false,
        )),
        tool(
            "get_changeset",
            "Get a ChangeSet",
            "Read one ChangeSet by id. Use it to check a ChangeSet's status (PROVISIONAL, VALIDATED, ACCEPTED, COMMITTED or SUPERSEDED), its fingerprint, and its commit provenance: accepted_commit, pinned at acceptance and never changed, and integration_commit, set by record_commit. Read-only. Use get_intent for the work item itself. To find an id, status lists PROVISIONAL, VALIDATED and ACCEPTED ChangeSets, and query_work shows each intent's latest one.",
            json!({ "id": { "type": "string", "minLength": 1, "description": "The ChangeSet id (chg_...)." } }),
            &["id"],
            true,
            true,
        ),
        tool(
            "get_intent",
            "Get an intent",
            "Read one intent by id, with its owning agent and the count and ids of its OPEN or COORDINATING conflicts (check_conflicts with intent_id shows severity). Use it to see an intent's current status (INTENT, CLAIMED, IN_PROGRESS, PROVISIONAL, VALIDATED, ACCEPTED, COMMITTED or DISCARDED), declared scopes and depends_on, for example before assessing someone else's related work. Read-only. Use query_work to search intents by agent, status or scope, and status for everything at once.",
            json!({ "id": { "type": "string", "minLength": 1, "description": "The intent id (int_...)." } }),
            &["id"],
            true,
            true,
        ),
        tool(
            "list_agents",
            "List coding agents",
            "List every registered agent in registration order, with its id, name, model, capabilities, worktree, Git branch and head at registration, and status. Read-only and takes no arguments. Use it to find the agent id of another participant before coordinate_with_agent. Use status for the active agents alongside their current work.",
            json!({}),
            &[],
            true,
            true,
        ),
        tool(
            "publish_changeset",
            "Publish a provisional ChangeSet",
            "Record a finished unit of implementation for your intent so it can be verified and accepted. Use it once the change is committed in your worktree, after check_conflicts and before run_verification. Foremerge snapshots the worktree (defaulting to your registered one), resolves the candidate commit and its diff base, and stores a fingerprint that verification and acceptance are later checked against. files and symbols are inferred only from uncommitted changes, so list them yourself for committed work. The ChangeSet starts PROVISIONAL and the intent becomes PROVISIONAL. Publishing again while the previous ChangeSet is PROVISIONAL or VALIDATED creates a new one, marks the old one SUPERSEDED, and resets verification. Only the intent's owner may publish, while the intent is CLAIMED, IN_PROGRESS, PROVISIONAL or VALIDATED. Git itself is not modified. Returns the ChangeSet, including its chg_ id, fingerprint, and open_conflicts on the intent at that moment.",
            json!({
                "agent_id": { "type": "string", "description": "Your agent id (agt_...). Must own the intent." },
                "intent_id": { "type": "string", "description": "The intent (int_...) this implementation fulfils." },
                "summary": { "type": "string", "minLength": 1, "description": "What the change does, in one or two sentences." },
                "files": { "type": "array", "items": { "type": "string" }, "default": [], "description": "Changed file paths. Left empty, they are inferred from uncommitted changes only, so list them for committed work." },
                "symbols": { "type": "array", "items": { "type": "string" }, "default": [], "description": "Code symbols added or changed. Left empty, they are inferred from uncommitted changes only, so list them for committed work." },
                "contracts": { "type": "array", "items": { "type": "string" }, "default": [], "description": "Named interfaces or agreements this change affects, for example payment-provider." },
                "dependencies": { "type": "array", "items": { "type": "string" }, "default": [], "description": "Intent ids (int_...) this change builds on, and the dependency list acceptance enforces. accept_changeset refuses with UNSATISFIED_DEPENDENCY unless each is ACCEPTED or COMMITTED and its accepted commit is in this change's Git history. Intent ids only: not package or library names." },
                "tests": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string", "description": "The command you ran, for example cargo test." },
                            "status": { "type": "string", "description": "Its outcome as you observed it, for example passed or failed." },
                            "summary": { "type": "string", "description": "Optional short note on what ran or failed." }
                        },
                        "required": ["command", "status"],
                        "additionalProperties": false
                    },
                    "default": [],
                    "description": "Tests you ran yourself. Recorded as history only; acceptance relies on run_verification, not on this list."
                },
                "decisions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string", "description": "The decision, for example Use a provider trait." },
                            "rationale": { "type": "string", "description": "Why you chose it." },
                            "alternatives": {
                                "type": "array",
                                "items": { "type": "string" },
                                "default": [],
                                "description": "Options you considered and rejected."
                            }
                        },
                        "required": ["title", "rationale"],
                        "additionalProperties": false
                    },
                    "default": [],
                    "description": "Design decisions a reviewer or later agent should know about."
                },
                "provenance": { "type": "object", "default": {}, "description": "Optional JSON object of your own context, such as a prompt or task reference. Foremerge adds Git provenance under provenance.git." },
                "git_ref": { "type": "string", "description": "Candidate commit. Omit it: it defaults to the worktree's HEAD, and acceptance rejects any other commit with STALE_CHANGESET." },
                "base_ref": {
                    "type": "string",
                    "minLength": 1,
                    "description": "True diff base when known (for example the fork point of this agent branch); defaults to the candidate commit's first parent."
                },
                "worktree": { "type": "string", "description": "Path of the Git worktree holding the change. Defaults to your registered worktree, then the server's working directory. Must belong to your registered repository." }
            }),
            &["agent_id", "intent_id", "summary"],
            false,
            false,
        ),
        tool(
            "publish_intent",
            "Publish intent",
            "Announce work you are about to do, and the scopes it will change, before editing any code. This is the first call for every task, after register_agent. The intent is stored with status INTENT and compared against every other active intent. Returns the intent (with its int_ id), any conflicts detected immediately, and related_work: active intents, your own others included, that may relate to yours. Entries with asserted: true are collisions with both declared operations stated; the rest are candidates, with why_surfaced saying why. Assess each related_work entry and call record_assessment before writing code, then claim_work and start_work. Use check_conflicts instead for a what-if check that stores nothing.",
            json!({
                "agent_id": { "type": "string", "description": "Your agent id (agt_...) from register_agent." },
                "task": { "type": "string", "minLength": 1, "description": "Short name of the task this belongs to, for example payments-provider. Intents with the same task text share one task record." },
                "summary": { "type": "string", "minLength": 1, "description": "What you are going to change, in one sentence." },
                "rationale": { "type": "string", "description": "Optional reason for the change." },
                "scopes": { "type": "array", "items": scope_claim.clone(), "default": [], "description": "Every scope this work will touch, each with the operation performed on it. Conflict detection relies on these, so declare them all." },
                "depends_on": { "type": "array", "items": { "type": "string" }, "default": [], "description": "Intent ids (int_...) this work relies on, recorded for the coordination graph and reported as dependents by query_work. Not checked or enforced: to make acceptance require them, also list them in publish_changeset's dependencies." },
                "metadata": { "type": "object", "default": {}, "description": "Optional JSON object of extra context stored with the intent." }
            }),
            &["agent_id", "task", "summary"],
            false,
            false,
        ),
        tool(
            "query_work",
            "Query active work",
            "Search intents by owner, status or scope, to answer questions like who is changing this symbol, or which work still has open conflicts. Read-only. Each result joins an intent to its agent, its claims, its latest ChangeSet (id and full object), the ids of intents that depend on it, and its count of OPEN or COORDINATING conflicts. Results are an array, capped by limit. With no filters it returns intents of every status, including finished ones. Use get_intent when you already have an id, and status for a grouped overview of everything at once.",
            json!({
                "agent_id": { "type": "string", "description": "Only intents owned by this agent (agt_...)." },
                "status": { "type": "string", "description": "Only intents in this lifecycle status: INTENT, CLAIMED, IN_PROGRESS, PROVISIONAL, VALIDATED, ACCEPTED, COMMITTED or DISCARDED. Case-insensitive." },
                "scope": {
                    "type": "object",
                    "properties": scope["properties"].clone(),
                    "required": ["kind", "key"],
                    "additionalProperties": false,
                    "description": "Intents that declared, or ever claimed, a matching scope. Symbols match loosely, on their last two :: segments ignoring namespace and case, so unrelated same-named symbols can appear."
                },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50, "description": "Maximum number of intents to return, from 1 to 500. Default 50." }
            }),
            &[],
            true,
            true,
        ),
        tool(
            "record_assessment",
            "Record an assessment of related work",
            "Record what you concluded about one entry from related_work. Foremerge states which scopes overlap and how the declared operations relate; deciding what that means is yours. Call this once per related intent, after publish_intent and before you write code. Each call appends a new assessment rather than replacing an earlier one, and changes no statuses: it does not open, resolve or dismiss conflicts. Act on the verdict with coordinate_with_agent, resolve_conflict or discard_work as needed. Only the intent's owner may assess it. Returns the stored assessment with its asm_ id.",
            json!({
                "agent_id": { "type": "string", "minLength": 1, "description": "Your agent id (agt_...). Must own intent_id." },
                "intent_id": { "type": "string", "minLength": 1, "description": "Your intent (int_...) whose publish returned the related_work." },
                "related_intent_id": { "type": "string", "minLength": 1, "description": "The other agent's intent (int_...) from the related_work entry you are assessing." },
                "verdict": {
                    "type": "string",
                    "enum": ["conflicts", "compatible", "duplicate", "depends_on"],
                    "description": "conflicts: the two plans cannot both land as written. compatible: they can. duplicate: the same work twice. depends_on: yours needs theirs to land first."
                },
                "rationale": { "type": "string", "minLength": 1, "description": "Why you reached that verdict, specific enough for a later reader to check." },
                "action": {
                    "type": "string",
                    "enum": ["proceeding", "rescoping", "waiting", "abandoning"],
                    "description": "What you will do next. proceeding: continue as planned. rescoping: change your scopes first. waiting: hold until the other work lands. abandoning: drop your intent (then call discard_work)."
                }
            }),
            &[
                "agent_id",
                "intent_id",
                "related_intent_id",
                "verdict",
                "rationale",
                "action",
            ],
            false,
            false,
        ),
        tool(
            "record_commit",
            "Record integration commit",
            "Record where accepted work finally landed, after it has been merged through ordinary Git or a pull request. Use it last, after accept_changeset and the merge. The commit must contain the accepted commit in its history, otherwise the call fails with TARGET_DIVERGED. On success it stores integration_commit, moves the ChangeSet to COMMITTED, and returns the updated ChangeSet. accepted_commit is kept unchanged, so the record shows both what was verified and where it landed. Foremerge does not merge or push anything itself.",
            json!({
                "changeset_id": { "type": "string", "minLength": 1, "description": "An ACCEPTED ChangeSet (chg_...)." },
                "git_ref": { "type": "string", "minLength": 1, "description": "The landed commit, as a SHA or ref such as main, resolved in the ChangeSet's worktree." }
            }),
            &["changeset_id", "git_ref"],
            false,
            false,
        ),
        tool(
            "register_agent",
            "Register coding agent",
            "Register yourself as a participant and get the agent id every other write tool needs. Call it once at the start of each session, before publish_intent. Every call creates a new agent record, even for a name already in use; a warning is returned when an active agent with the same name and worktree exists, because that record's intents cannot be claimed by the new one. Passing a worktree records its Git branch and head; it must be inside the repository this server is bound to, normally the one it was launched in, or it is rejected with INVALID_INPUT. Returns the agent, including its agt_ id, and any warnings.",
            json!({
                "name": { "type": "string", "minLength": 1, "description": "A readable name for this agent, for example payments-stripe." },
                "model": { "type": "string", "description": "Optional model identifier, for example the LLM you are running as." },
                "capabilities": { "type": "array", "items": { "type": "string" }, "default": [], "description": "Optional skills or areas, for example rust or payments, shown to other agents." },
                "worktree": { "type": "string", "description": "Path to your Git worktree, inside the repository this server is bound to. Recommended." }
            }),
            &["name"],
            false,
            false,
        ),
        tool(
            "resolve_conflict",
            "Resolve a persisted conflict",
            "Record an audited resolution decision for a durable cfl_* conflict so blocked work can proceed. Over MCP only an agent whose intent is a party may resolve it, and the decision is recorded under its agent id. Foremerge does not verify agreement or message ids, so either party can clear the gate alone: resolve only after the other party agrees. Use coordinate_with_agent first to reach that agreement, and discard_work instead when one side is simply dropping its work. Resolving moves the conflict to RESOLVED, which clears it from the acceptance gate for both intents; it does not change any code. It fails if the conflict is already resolved or dismissed. Returns the updated conflict.",
            json!({
                "conflict_id": { "type": "string", "pattern": "^cfl_", "description": "The conflict to resolve (cfl_...), from check_conflicts, publish_intent or status." },
                "agent_id": { "type": "string", "minLength": 1, "description": "Your agent id (agt_...). Your intent must be one of the conflict's two parties." },
                "resolution": { "type": "string", "minLength": 1, "description": "Short title for the agreed outcome, for example sequenced: provider abstraction lands first, or split scopes. No fixed vocabulary." },
                "rationale": { "type": "string", "minLength": 1, "description": "Why this resolves the clash, naming the msg_ ids where you agreed it. Must be non-empty; its content is not checked." }
            }),
            &["conflict_id", "agent_id", "resolution", "rationale"],
            false,
            false,
        ),
        tool(
            "run_verification",
            "Run a trusted verification check",
            "Run one named check from the trusted Foremerge registry of the repository this store is bound to. Raw commands are intentionally not accepted over MCP, and the registry cannot be selected by the caller or the server's working directory. Use it after publish_changeset and before accept_changeset; acceptance relies on this result, not on tests you report yourself. The check's command runs in the ChangeSet's worktree with the registry's timeout. It may create only generated files excluded from the fingerprint: changing any other file fails with STALE_CHANGESET, and excluded files left from an earlier run fail with CHECK_FAILED before it starts. The worktree must still match the fingerprint recorded at publication, or the call fails with STALE_CHANGESET and you must publish again. A pass moves the ChangeSet to VALIDATED; a failure leaves or returns it to PROVISIONAL. Returns the validation record: whether it passed, the exit code, captured stdout and stderr, the duration, and the fingerprint it ran against.",
            json!({
                "changeset_id": { "type": "string", "minLength": 1, "description": "The ChangeSet (chg_...) to verify. Must be PROVISIONAL or VALIDATED." },
                "check": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 64,
                    "pattern": "^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$",
                    "description": "Name of a check an operator registered with foremerge checks set, for example test. Unknown names are rejected."
                }
            }),
            &["changeset_id", "check"],
            false,
            false,
        ),
        tool(
            "start_work",
            "Start claimed work",
            "Mark your claimed intent as being implemented. Call it after claim_work, immediately before you begin editing code. It moves the intent from CLAIMED to IN_PROGRESS and fails in any other state, or if you do not own the intent. It does not create or renew claims; use claim_work for that. Returns the updated intent with open_conflicts, the count and ids of its OPEN or COORDINATING conflicts; check their severity with check_conflicts before going further.",
            json!({
                "agent_id": { "type": "string", "minLength": 1, "description": "Your agent id (agt_...). Must own the intent." },
                "intent_id": { "type": "string", "minLength": 1, "description": "Your CLAIMED intent (int_...)." }
            }),
            &["agent_id", "intent_id"],
            false,
            false,
        ),
        tool(
            "status",
            "Read coordinator status",
            "Get the whole coordination picture in one call, taken from a single consistent read: active agents, all intents grouped by lifecycle status, unexpired claims, OPEN or COORDINATING conflicts with both parties named, and ChangeSets grouped by status. Read-only and takes no arguments. Use it to orient at the start of a session or before choosing work. Use query_work to filter by agent, status or scope, and get_intent or get_changeset for full detail on one item.",
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

    /// Claude Code rejected every query_work and list_agents call while their
    /// array results were sent as structuredContent, which MCP defines as an object.
    #[test]
    fn structured_content_is_only_sent_for_object_results() {
        let object = tool_result(json!({ "id": "agt_1" }), false);
        assert_eq!(object["structuredContent"], json!({ "id": "agt_1" }));

        let array = tool_result(json!([{ "id": "int_1" }]), false);
        assert!(array.get("structuredContent").is_none(), "{array}");
        assert_eq!(array["isError"], false);
        let text = array["content"][0]["text"].as_str().expect("text content");
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap(),
            json!([{ "id": "int_1" }])
        );
    }

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

    /// Agents choose and fill tools from these strings alone, and registries
    /// such as Glama grade servers on them, so an undocumented parameter is a
    /// regression even when the schema is otherwise valid.
    /// The two dependency fields are the ones whose descriptions were once
    /// written backwards, and prose presence alone cannot catch that. These
    /// say what `acceptance_enforces_changeset_dependencies_and_not_intent_depends_on`
    /// proves: acceptance enforces a ChangeSet's `dependencies`, and an
    /// intent's `depends_on` is not checked.
    #[test]
    fn dependency_descriptions_match_what_acceptance_enforces() {
        let catalog = tool_catalog();
        let property = |tool: &str, field: &str| -> String {
            catalog
                .iter()
                .find(|entry| entry["name"] == tool)
                .unwrap_or_else(|| panic!("tool {tool}"))["inputSchema"]["properties"][field]
                ["description"]
                .as_str()
                .unwrap_or_else(|| panic!("{tool}.{field} description"))
                .to_string()
        };
        let enforced = property("publish_changeset", "dependencies");
        assert!(enforced.contains("UNSATISFIED_DEPENDENCY"), "{enforced}");
        assert!(enforced.contains("not package"), "{enforced}");
        let recorded = property("publish_intent", "depends_on");
        assert!(recorded.contains("Not checked or enforced"), "{recorded}");
        let accept = catalog
            .iter()
            .find(|entry| entry["name"] == "accept_changeset")
            .expect("accept_changeset")["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!accept.contains("depends_on"), "{accept}");
    }

    /// Many clients put the whole tool catalogue into the model's context on
    /// every session, so its size is a recurring token cost for every user.
    /// Descriptions grew from about 13 kB to about 31 kB when they were written
    /// out properly, which was deliberate. This budget exists so the next
    /// increase is deliberate too: raise it knowingly, never to make a test
    /// pass.
    #[test]
    fn the_tool_catalogue_stays_within_its_context_budget() {
        const BUDGET_BYTES: usize = 36_000;
        let size = serde_json::to_string(&Value::Array(tool_catalog()))
            .expect("serialize the catalogue")
            .len();
        assert!(
            size <= BUDGET_BYTES,
            "tools/list is {size} bytes, over the {BUDGET_BYTES}-byte budget; tighten descriptions or raise the budget on purpose"
        );
    }

    #[test]
    fn every_tool_parameter_is_described() {
        fn undocumented(tool: &str, path: &str, properties: &Value, missing: &mut Vec<String>) {
            for (name, schema) in properties.as_object().into_iter().flatten() {
                let here = format!("{path}.{name}");
                let described = schema
                    .get("description")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty());
                if !described {
                    missing.push(format!("{tool}: {here}"));
                }
                if let Some(nested) = schema.get("properties") {
                    undocumented(tool, &here, nested, missing);
                }
                if let Some(nested) = schema
                    .get("items")
                    .and_then(|items| items.get("properties"))
                {
                    undocumented(tool, &format!("{here}[]"), nested, missing);
                }
            }
        }
        let mut missing = Vec::new();
        for tool in tool_catalog() {
            let name = tool["name"].as_str().unwrap();
            undocumented(name, "", &tool["inputSchema"]["properties"], &mut missing);
        }
        assert!(
            missing.is_empty(),
            "undocumented parameters:\n{}",
            missing.join("\n")
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
                "record_assessment",
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
        assert_eq!(initialized["result"]["protocolVersion"], PROTOCOL);

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

    const LEDGER: &str = "/repo/.git/foremerge/state.sqlite3";

    fn unsupported_schema() -> StoreUnavailable {
        StoreUnavailable::new(
            "UNSUPPORTED_SCHEMA",
            "UNSUPPORTED_SCHEMA: database schema 10 is newer than this build supports (9); upgrade Foremerge to open it",
            Path::new(LEDGER),
        )
    }

    #[tokio::test]
    async fn a_working_store_keeps_the_lifecycle_instructions() {
        let service = Foremerge::new(Store::in_memory().unwrap());
        let initialized = handle_message(
            &service,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
        )
        .await
        .unwrap();
        assert_eq!(initialized["result"]["instructions"], INSTRUCTIONS);
    }

    #[tokio::test]
    async fn an_unavailable_store_still_completes_the_handshake_and_lists_every_tool() {
        let unavailable = unsupported_schema();
        let backend = Backend::Unavailable(&unavailable);
        let initialized = respond(
            backend,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2026-07-28", "capabilities": {} }
            }),
        )
        .await
        .unwrap();
        // Asked for a revision this server does not speak, it still answers
        // with the one it does.
        assert_eq!(initialized["result"]["protocolVersion"], PROTOCOL);
        assert_eq!(initialized["result"]["serverInfo"]["name"], "foremerge");
        let instructions = initialized["result"]["instructions"].as_str().unwrap();
        assert!(
            instructions.starts_with(
                "Foremerge unavailable: UNSUPPORTED_SCHEMA: database schema 10 is newer than this build supports (9); upgrade Foremerge to open it. "
            ),
            "{instructions}"
        );
        assert!(instructions.contains(LEDGER), "{instructions}");
        assert!(
            instructions.contains("restart the client session"),
            "{instructions}"
        );
        assert!(
            instructions.contains(UNAVAILABLE_GUIDANCE),
            "{instructions}"
        );
        assert!(!instructions.contains('\u{2014}'), "{instructions}");

        let ping = respond(
            backend,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }),
        )
        .await
        .unwrap();
        assert!(ping.get("error").is_none(), "{ping}");

        let listed = respond(
            backend,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {} }),
        )
        .await
        .unwrap();
        assert_eq!(listed["result"]["tools"], Value::Array(tool_catalog()));
    }

    /// A dual-era client, one that speaks both revisions, probes
    /// `server/discover` first and falls back to the `initialize` handshake
    /// when that fails, per the 2026-07-28 stdio backward-compatibility rule. Answering the probe
    /// at all, with a result missing everything the revision requires, is what
    /// put such a client on the wrong path.
    #[tokio::test]
    async fn a_modern_discovery_probe_is_refused_so_the_client_falls_back() {
        let service = Foremerge::new(Store::in_memory().unwrap());
        let probe = handle_message(
            &service,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "server/discover",
                "params": { "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": { "name": "modern", "version": "1" }
                }}
            }),
        )
        .await
        .unwrap();
        assert_eq!(probe["error"]["code"], -32601, "{probe}");
        assert!(probe.get("result").is_none(), "{probe}");

        // The fallback then works, and names this revision rather than the
        // client's.
        let initialized = handle_message(
            &service,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "initialize",
                "params": { "protocolVersion": "2026-07-28", "capabilities": {} }
            }),
        )
        .await
        .unwrap();
        assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");
    }

    /// Every JSON-RPC error this server sends has exactly the three members a
    /// JSON-RPC 2.0 error response may have. The unknown-method case matters
    /// most: it is how a dual-era client's discovery probe is refused, and an
    /// extra member made the official TypeScript client ignore the refusal and
    /// wait out its probe timeout.
    #[tokio::test]
    async fn every_error_response_is_a_strict_json_rpc_envelope() {
        let assert_strict = |response: &Value| {
            let object = response.as_object().expect("a response object");
            let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(keys, ["error", "id", "jsonrpc"], "{response}");
            let mut error_keys: Vec<&str> = response["error"]
                .as_object()
                .expect("an error object")
                .keys()
                .map(String::as_str)
                .collect();
            error_keys.sort_unstable();
            assert_eq!(error_keys, ["code", "message"], "{response}");
        };
        let service = Foremerge::new(Store::in_memory().unwrap());
        let unavailable = unsupported_schema();
        for backend in [Backend::Ready(&service), Backend::Unavailable(&unavailable)] {
            for request in [
                json!({ "jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {} }),
                json!({ "jsonrpc": "2.0", "id": 2, "method": "no/such/method" }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": { "name": "no_such_tool", "arguments": {} }
                }),
                json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {} }),
            ] {
                let response = respond(backend, request).await.expect("a response");
                assert_strict(&response);
            }
        }
        // The parse-error path builds its response with the same function.
        assert_strict(&jsonrpc_error(Value::Null, -32700, "parse error"));
    }

    /// The refusal does not depend on the store. A server that cannot open its
    /// ledger must route a dual-era client to the same working handshake, where
    /// the instructions then explain what is wrong.
    #[tokio::test]
    async fn the_discovery_probe_is_refused_the_same_way_while_the_store_is_unavailable() {
        let unavailable = unsupported_schema();
        let backend = Backend::Unavailable(&unavailable);
        let probe = respond(
            backend,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {} }),
        )
        .await
        .unwrap();
        assert_eq!(probe["error"]["code"], -32601, "{probe}");
        let initialized = respond(
            backend,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {} }),
        )
        .await
        .unwrap();
        // No protocolVersion requested at all, as a legacy client may send.
        assert_eq!(initialized["result"]["protocolVersion"], PROTOCOL);
        assert!(
            initialized["result"]["instructions"]
                .as_str()
                .is_some_and(|text| text.starts_with("Foremerge unavailable:")),
            "{initialized}"
        );
    }

    #[tokio::test]
    async fn every_tool_answers_with_the_reason_while_the_store_is_unavailable() {
        let unavailable = unsupported_schema();
        let backend = Backend::Unavailable(&unavailable);
        for tool in tool_catalog() {
            let name = tool["name"].as_str().unwrap();
            let response = respond(
                backend,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": { "name": name, "arguments": {} }
                }),
            )
            .await
            .unwrap();
            assert_eq!(response["result"]["isError"], true, "{name}: {response}");
            let error = &response["result"]["structuredContent"];
            assert_eq!(error["code"], "UNSUPPORTED_SCHEMA", "{name}: {response}");
            assert_eq!(error["message"], unavailable.message, "{name}: {response}");
            assert_eq!(
                error["error"],
                format!("Foremerge unavailable: {}", unavailable.message),
                "{name}: {response}"
            );
            assert_eq!(error["database"], LEDGER, "{name}: {response}");
            assert_eq!(
                error["guidance"], UNAVAILABLE_GUIDANCE,
                "{name}: {response}"
            );
            assert!(
                error["remedy"]
                    .as_str()
                    .is_some_and(|remedy| remedy.contains("supports the ledger's schema")),
                "{name}: {response}"
            );
        }

        // An unknown tool is still a protocol error, as with a working store.
        let unknown = respond(
            backend,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": "no_such_tool", "arguments": {} }
            }),
        )
        .await
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    }

    #[test]
    fn a_store_that_fails_for_another_reason_points_at_doctor() {
        let unavailable = StoreUnavailable::new(
            "ERROR",
            "open SQLite database: unable to open database file.",
            Path::new(LEDGER),
        );
        assert!(
            unavailable.remedy.contains("foremerge doctor"),
            "{}",
            unavailable.remedy
        );
        let instructions = unavailable.instructions();
        assert!(
            instructions
                .starts_with("Foremerge unavailable: open SQLite database: unable to open database file. The server"),
            "{instructions}"
        );
        assert!(!instructions.contains('\u{2014}'), "{instructions}");
    }

    async fn tools_call_result(service: &Foremerge, name: &str, arguments: Value) -> Value {
        let message = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        });
        handle_message(service, message)
            .await
            .expect("a tools/call is answered")["result"]
            .clone()
    }

    /// An MCP caller passes the key as a structured field, so none of the
    /// CLI's parsing ever sees it. An operation restated in that key has to be
    /// refused as a tool error rather than stored as a claim on a symbol that
    /// overlaps nothing.
    #[tokio::test]
    async fn claim_work_and_query_work_refuse_an_operation_restated_in_the_key() {
        let service = Foremerge::new(Store::in_memory().unwrap());
        let agent = tools_call_result(
            &service,
            "register_agent",
            json!({ "name": "mcp-agent", "model": "test" }),
        )
        .await["structuredContent"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let intent = tools_call_result(
            &service,
            "publish_intent",
            json!({
                "agent_id": agent,
                "task": "payments",
                "summary": "Replace PaymentService with Stripe",
                "scopes": [{ "kind": "symbol", "key": "PaymentService", "operation": "replace" }]
            }),
        )
        .await["structuredContent"]["intent"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let restated = json!({ "kind": "symbol", "key": "PaymentService=replace" });

        let claimed = tools_call_result(
            &service,
            "claim_work",
            json!({ "agent_id": agent, "intent_id": intent, "scopes": [restated] }),
        )
        .await;
        let queried = tools_call_result(&service, "query_work", json!({ "scope": restated })).await;
        for (tool, result) in [("claim_work", claimed), ("query_work", queried)] {
            assert_eq!(result["isError"], true, "{tool}: {result}");
            let error = result["structuredContent"]["error"]
                .as_str()
                .unwrap_or_default();
            assert!(error.starts_with("INVALID_INPUT:"), "{tool}: {error}");
            assert!(
                error.contains("publish_intent"),
                "{tool}: the refusal names the tool that takes an operation: {error}"
            );
        }
    }
}
