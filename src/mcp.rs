use crate::Foremerge;
use crate::model::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const CURRENT_PROTOCOL: &str = "2026-07-28";
const LEGACY_PROTOCOL: &str = "2025-11-25";

pub async fn run_stdio(service: Foremerge) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle_message(&service, message).await,
            Err(error) => Some(jsonrpc_error(
                Value::Null,
                -32700,
                &format!("parse error: {error}"),
            )),
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
                "instructions": "Publish intent and semantic scopes before editing. Claims are advisory; unresolved HIGH conflicts gate acceptance."
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
        "register_agent" => parse::<RegisterAgentRequest>(arguments)
            .and_then(|request| service.register_agent(request))
            .and_then(to_value),
        "publish_intent" => parse::<PublishIntentRequest>(arguments)
            .and_then(|request| service.publish_intent(request))
            .and_then(to_value),
        "claim_work" => parse::<ClaimWorkRequest>(arguments)
            .and_then(|request| service.claim_work(request))
            .and_then(to_value),
        "query_work" => parse::<WorkQuery>(arguments)
            .and_then(|request| service.query_work(request))
            .and_then(to_value),
        "check_conflicts" => parse::<ConflictCheckRequest>(arguments)
            .and_then(|request| service.check_conflicts(request))
            .and_then(to_value),
        "publish_changeset" => parse::<PublishChangeSetRequest>(arguments)
            .and_then(|request| service.publish_changeset(request))
            .and_then(to_value),
        "coordinate_with_agent" => parse::<CoordinateRequest>(arguments)
            .and_then(|request| service.coordinate_with_agent(request))
            .and_then(to_value),
        _ => return Err((-32602, format!("unknown tool: {name}"))),
    };
    match outcome {
        Ok(value) => Ok(tool_result(value, false)),
        Err(error) => Ok(tool_result(json!({ "error": format!("{error:#}") }), true)),
    }
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

pub fn tool_catalog() -> Vec<Value> {
    let scope = scope_schema();
    vec![
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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[tokio::test]
    async fn lists_the_seven_required_tools_in_deterministic_order() {
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
                "check_conflicts",
                "claim_work",
                "coordinate_with_agent",
                "publish_changeset",
                "publish_intent",
                "query_work",
                "register_agent",
            ]
        );
        assert!(tools.iter().all(|tool| tool.get("outputSchema").is_none()));
        assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
        assert_eq!(tools[0]["annotations"]["idempotentHint"], false);
        assert_eq!(
            tools[0]["inputSchema"]["anyOf"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            tools[3]["inputSchema"]["properties"]["tests"]["items"]["required"],
            json!(["command", "status"])
        );
        assert_eq!(
            tools[3]["inputSchema"]["properties"]["decisions"]["items"]["required"],
            json!(["title", "rationale"])
        );
        assert_eq!(tools[5]["annotations"]["readOnlyHint"], true);
        assert_eq!(tools[5]["annotations"]["idempotentHint"], true);
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
