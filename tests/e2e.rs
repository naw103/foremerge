use foremerge::Store;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct RepoFixture {
    temp: TempDir,
    root: PathBuf,
}

struct DaemonGuard {
    child: Option<Child>,
}

impl DaemonGuard {
    fn stop(mut self) {
        if let Some(mut child) = self.child.take() {
            if child.try_wait().expect("inspect daemon state").is_none() {
                child.kill().expect("terminate daemon");
            }
            child.wait().expect("reap daemon process");
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

fn foremerge_bin() -> &'static Path {
    assert_cmd::cargo::cargo_bin!("foremerge")
}

fn git<I, S>(cwd: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git failed in {}\nstdout: {}\nstderr: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_string()
}

fn create_repo() -> RepoFixture {
    let temp = tempfile::tempdir().expect("create fixture directory");
    let root = temp.path().join("repo");
    fs::create_dir(&root).expect("create repository directory");
    git(&root, ["init", "--quiet"]);
    git(&root, ["config", "user.name", "Foremerge Test"]);
    git(
        &root,
        ["config", "user.email", "foremerge-test@example.invalid"],
    );
    git(&root, ["config", "commit.gpgsign", "false"]);
    fs::write(root.join("README.md"), "# fixture\n").expect("write initial file");
    git(&root, ["add", "README.md"]);
    git(&root, ["commit", "--quiet", "-m", "initial fixture"]);
    RepoFixture { temp, root }
}

fn cli_command(cwd: &Path, database: Option<&Path>) -> Command {
    let mut command = Command::new(foremerge_bin());
    command.arg("--json").arg("--cwd").arg(cwd);
    if let Some(path) = database {
        command.arg("--database").arg(path);
    }
    command
}

fn cli_output<I, S>(cwd: &Path, database: Option<&Path>, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    cli_command(cwd, database)
        .args(args)
        .output()
        .expect("run foremerge")
}

fn parse_cli_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "CLI stdout was not one JSON value: {error}\nstatus: {}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn cli_success<I, S>(cwd: &Path, database: Option<&Path>, args: I) -> Value
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = cli_output(cwd, database, args);
    assert!(
        output.status.success(),
        "Foremerge command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value = parse_cli_json(&output);
    assert_eq!(value["ok"], true, "unexpected success envelope: {value}");
    value
}

fn cli_failure<I, S>(cwd: &Path, database: Option<&Path>, args: I) -> Value
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = cli_output(cwd, database, args);
    assert!(
        !output.status.success(),
        "Foremerge command unexpectedly succeeded: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value = parse_cli_json(&output);
    assert_eq!(value["ok"], false, "unexpected error envelope: {value}");
    value
}

fn data_string<'a>(value: &'a Value, field: &str) -> &'a str {
    value["data"][field]
        .as_str()
        .unwrap_or_else(|| panic!("missing data.{field} in {value}"))
}

fn database_from_doctor(cwd: &Path) -> PathBuf {
    let doctor = cli_success(cwd, None, ["doctor"]);
    PathBuf::from(data_string(&doctor, "database"))
}

fn wait_for_sentinel(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "sentinel was not created within ten seconds: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn register_test_agent(cwd: &Path, name: &str) -> String {
    let registered = cli_success(
        cwd,
        None,
        ["agent", "register", "--name", name, "--model", "e2e-test"],
    );
    data_string(&registered, "id").to_string()
}

fn publish_test_intent(
    cwd: &Path,
    agent_id: &str,
    task: &str,
    summary: &str,
    scope: &str,
) -> String {
    let published = cli_success(
        cwd,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_id.to_string(),
            "--task".to_string(),
            task.to_string(),
            "--summary".to_string(),
            summary.to_string(),
            "--scope".to_string(),
            scope.to_string(),
        ],
    );
    published["data"]["intent"]["id"]
        .as_str()
        .expect("published intent id")
        .to_string()
}

fn claim_test_scope(cwd: &Path, agent_id: &str, intent_id: &str, scope: &str) -> Value {
    cli_success(
        cwd,
        None,
        vec![
            "work".to_string(),
            "claim".to_string(),
            "--agent".to_string(),
            agent_id.to_string(),
            "--intent".to_string(),
            intent_id.to_string(),
            "--scope".to_string(),
            scope.to_string(),
        ],
    )
}

fn start_test_work(cwd: &Path, agent_id: &str, intent_id: &str) {
    cli_success(
        cwd,
        None,
        vec![
            "work".to_string(),
            "start".to_string(),
            intent_id.to_string(),
            "--agent".to_string(),
            agent_id.to_string(),
        ],
    );
}

fn publish_test_revision(cwd: &Path, agent_id: &str, intent_id: &str, summary: &str) -> String {
    let published = cli_success(
        cwd,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_id.to_string(),
            "--intent".to_string(),
            intent_id.to_string(),
            "--summary".to_string(),
            summary.to_string(),
            "--symbol".to_string(),
            "RaceTarget".to_string(),
        ],
    );
    data_string(&published, "id").to_string()
}

fn create_active_test_work(cwd: &Path, name: &str, task: &str) -> (String, String) {
    let agent_id = register_test_agent(cwd, name);
    let intent_id = publish_test_intent(
        cwd,
        &agent_id,
        task,
        "Coordinate a lifecycle race safely",
        "symbol:RaceTarget",
    );
    claim_test_scope(cwd, &agent_id, &intent_id, "symbol:RaceTarget");
    start_test_work(cwd, &agent_id, &intent_id);
    (agent_id, intent_id)
}

fn spawn_sentinel_validation(
    cwd: &Path,
    changeset_id: &str,
    started: &Path,
    release: &Path,
) -> Child {
    let mut command = cli_command(cwd, None);
    command
        .args(["changeset", "validate"])
        .arg(changeset_id)
        .args(["--timeout-seconds", "20", "--", "sh", "-c"])
        .arg("printf started > \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.01; done; exit 0")
        .arg("foremerge-validation-sentinel")
        .arg(started)
        .arg(release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.spawn().expect("spawn sentinel validation")
}

fn unused_loopback_address() -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve an ephemeral port");
    let address = listener.local_addr().expect("read ephemeral port");
    drop(listener);
    address
}

fn spawn_daemon(cwd: &Path, database: &Path, address: SocketAddr) -> DaemonGuard {
    let child = Command::new(foremerge_bin())
        .arg("--cwd")
        .arg(cwd)
        .arg("--database")
        .arg(database)
        .arg("daemon")
        .arg("--bind")
        .arg(address.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    DaemonGuard { child: Some(child) }
}

async fn wait_for_health(client: &reqwest::Client, base_url: &str) -> Value {
    let mut last_error = "daemon did not answer".to_string();
    for _ in 0..100 {
        match client.get(format!("{base_url}/healthz")).send().await {
            Ok(response) if response.status().is_success() => {
                return response.json().await.expect("health response is JSON");
            }
            Ok(response) => last_error = format!("health returned {}", response.status()),
            Err(error) => last_error = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon was not healthy within five seconds: {last_error}");
}

#[test]
fn live_cli_detects_the_headline_conflict_before_any_changeset() {
    let repo = create_repo();
    let agent_a = cli_success(
        &repo.root,
        None,
        [
            "agent",
            "register",
            "--name",
            "stripe-agent",
            "--model",
            "test-model-a",
        ],
    );
    let agent_b = cli_success(
        &repo.root,
        None,
        [
            "agent",
            "register",
            "--name",
            "paypal-agent",
            "--model",
            "test-model-b",
        ],
    );
    let agent_a_id = data_string(&agent_a, "id").to_string();
    let agent_b_id = data_string(&agent_b, "id").to_string();

    let first = cli_success(
        &repo.root,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_a_id,
            "--task".to_string(),
            "modernize-payments".to_string(),
            "--summary".to_string(),
            "Replace PaymentService with StripePaymentService".to_string(),
            "--scope".to_string(),
            "symbol:PaymentService".to_string(),
            "--scope".to_string(),
            "contract:payments.provider".to_string(),
        ],
    );
    assert_eq!(first["data"]["conflicts"], json!([]));

    let second = cli_success(
        &repo.root,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_b_id,
            "--task".to_string(),
            "add-paypal".to_string(),
            "--summary".to_string(),
            "Add PayPal support to PaymentService".to_string(),
            "--scope".to_string(),
            "symbol:PaymentService".to_string(),
            "--scope".to_string(),
            "contract:payments.provider".to_string(),
        ],
    );

    let conflicts = second["data"]["conflicts"]
        .as_array()
        .expect("publish_intent returns conflicts");
    let conflict = conflicts
        .iter()
        .find(|value| value["kind"] == "replace_vs_extend")
        .expect("replace-versus-extend conflict");
    assert_eq!(conflict["severity"], "HIGH");
    assert_eq!(conflict["evidence"]["detected_before_code"], true);
    assert!(
        conflict["suggestion"]
            .as_str()
            .expect("suggestion string")
            .contains("PaymentProvider")
    );

    let database = database_from_doctor(&repo.root);
    let connection = Connection::open(database).expect("open coordination database");
    let changesets: i64 = connection
        .query_row("SELECT COUNT(*) FROM changesets", [], |row| row.get(0))
        .expect("count changesets");
    let conflict_seq: i64 = connection
        .query_row(
            "SELECT MIN(seq) FROM events WHERE event_type = 'conflict.detected'",
            [],
            |row| row.get(0),
        )
        .expect("conflict event sequence");
    let latest_seq: i64 = connection
        .query_row("SELECT MAX(seq) FROM events", [], |row| row.get(0))
        .expect("latest event sequence");
    assert_eq!(
        changesets, 0,
        "intent conflict must precede every ChangeSet"
    );
    assert!(conflict_seq > 0 && conflict_seq <= latest_seq);
    assert_eq!(git(&repo.root, ["status", "--porcelain"]), "");
}

#[test]
fn two_git_worktrees_resolve_one_common_coordination_database() {
    let repo = create_repo();
    let worktree_a = repo.temp.path().join("agent-a-worktree");
    let worktree_b = repo.temp.path().join("agent-b-worktree");
    git(
        &repo.root,
        vec![
            "worktree".into(),
            "add".into(),
            "--quiet".into(),
            "-b".into(),
            "agent-a-branch".into(),
            worktree_a.as_os_str().to_owned(),
            "HEAD".into(),
        ],
    );
    git(
        &repo.root,
        vec![
            "worktree".into(),
            "add".into(),
            "--quiet".into(),
            "-b".into(),
            "agent-b-branch".into(),
            worktree_b.as_os_str().to_owned(),
            "HEAD".into(),
        ],
    );

    let agent_a = cli_success(
        &worktree_a,
        None,
        [
            "agent",
            "register",
            "--name",
            "worktree-a",
            "--model",
            "test",
        ],
    );
    let agent_b = cli_success(
        &worktree_b,
        None,
        [
            "agent",
            "register",
            "--name",
            "worktree-b",
            "--model",
            "test",
        ],
    );
    let doctor_a = cli_success(&worktree_a, None, ["doctor"]);
    let doctor_b = cli_success(&worktree_b, None, ["doctor"]);

    assert_eq!(doctor_a["data"]["database"], doctor_b["data"]["database"]);
    assert_eq!(
        doctor_a["data"]["git_common_dir"],
        doctor_b["data"]["git_common_dir"]
    );
    assert_eq!(doctor_a["data"]["shared_across_worktrees"], true);
    assert_eq!(doctor_b["data"]["shared_across_worktrees"], true);
    assert_ne!(agent_a["data"]["worktree"], agent_b["data"]["worktree"]);

    let agent_a_id = data_string(&agent_a, "id").to_string();
    let published = cli_success(
        &worktree_a,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_a_id,
            "--task".to_string(),
            "shared-ledger".to_string(),
            "--summary".to_string(),
            "Add audit entries to SharedLedger".to_string(),
            "--scope".to_string(),
            "symbol:SharedLedger".to_string(),
        ],
    );
    let published_id = published["data"]["intent"]["id"]
        .as_str()
        .expect("intent id");
    let observed = cli_success(
        &worktree_b,
        None,
        ["work", "query", "--scope", "symbol:SharedLedger"],
    );
    let observed_items = observed["data"].as_array().expect("work query array");
    assert!(
        observed_items
            .iter()
            .any(|item| item["intent"]["id"] == published_id),
        "agent B must immediately observe work published from agent A"
    );

    let database = PathBuf::from(data_string(&doctor_a, "database"));
    assert!(database.is_file());
    let connection = Connection::open(database).expect("open shared database");
    let agent_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM agents", [], |row| row.get(0))
        .expect("count shared agents");
    assert_eq!(agent_count, 2);
    assert_eq!(git(&worktree_a, ["status", "--porcelain"]), "");
    assert_eq!(git(&worktree_b, ["status", "--porcelain"]), "");
}

#[test]
fn failed_validation_cannot_accept_or_change_the_git_target() {
    let repo = create_repo();
    let original_head = git(&repo.root, ["rev-parse", "HEAD"]);
    let agent = cli_success(
        &repo.root,
        None,
        [
            "agent",
            "register",
            "--name",
            "validator",
            "--model",
            "test",
        ],
    );
    let agent_id = data_string(&agent, "id").to_string();
    let intent = cli_success(
        &repo.root,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_id.clone(),
            "--task".to_string(),
            "provider-routing".to_string(),
            "--summary".to_string(),
            "Introduce PaymentProvider routing".to_string(),
            "--scope".to_string(),
            "symbol:PaymentProvider".to_string(),
        ],
    );
    let intent_id = intent["data"]["intent"]["id"]
        .as_str()
        .expect("intent id")
        .to_string();
    cli_success(
        &repo.root,
        None,
        vec![
            "work".to_string(),
            "claim".to_string(),
            "--agent".to_string(),
            agent_id.clone(),
            "--intent".to_string(),
            intent_id.clone(),
            "--scope".to_string(),
            "symbol:PaymentProvider".to_string(),
        ],
    );
    cli_success(
        &repo.root,
        None,
        vec![
            "work".to_string(),
            "start".to_string(),
            intent_id.clone(),
            "--agent".to_string(),
            agent_id.clone(),
        ],
    );
    let changeset = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_id,
            "--intent".to_string(),
            intent_id,
            "--summary".to_string(),
            "Candidate provider routing".to_string(),
            "--symbol".to_string(),
            "PaymentProvider".to_string(),
        ],
    );
    let changeset_id = data_string(&changeset, "id").to_string();
    let validation = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            changeset_id.clone(),
            "--timeout-seconds".to_string(),
            "5".to_string(),
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            "printf 'expected failure' >&2; exit 7".to_string(),
        ],
    );
    assert_eq!(validation["data"]["passed"], false);
    assert_eq!(validation["data"]["exit_code"], 7);

    let shown = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "show".to_string(),
            changeset_id.clone(),
        ],
    );
    assert_eq!(shown["data"]["status"], "PROVISIONAL");

    let rejected = cli_failure(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "accept".to_string(),
            changeset_id.clone(),
        ],
    );
    assert!(
        matches!(
            rejected["error"]["code"].as_str(),
            Some("INVALID_TRANSITION") | Some("CHECK_FAILED")
        ),
        "unexpected acceptance error: {rejected}"
    );
    assert_eq!(git(&repo.root, ["rev-parse", "HEAD"]), original_head);

    let accepted_ref = format!("refs/foremerge/accepted/{changeset_id}");
    let ref_status = Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["show-ref", "--verify", "--quiet", &accepted_ref])
        .status()
        .expect("check accepted ref");
    assert!(
        !ref_status.success(),
        "failed validation created an accepted ref"
    );

    let database = database_from_doctor(&repo.root);
    let connection = Connection::open(database).expect("open coordination database");
    let stored: (String, bool) = connection
        .query_row(
            "SELECT c.status, v.passed FROM changesets c JOIN validations v ON v.changeset_id = c.id WHERE c.id = ?1 ORDER BY v.run_at DESC LIMIT 1",
            [&changeset_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("stored validation state");
    assert_eq!(stored, ("PROVISIONAL".to_string(), false));
}

#[test]
fn publishing_from_claimed_implicitly_enters_provisional_without_panicking() {
    let repo = create_repo();
    let agent = cli_success(
        &repo.root,
        None,
        [
            "agent",
            "register",
            "--name",
            "implicit-start-agent",
            "--model",
            "test",
        ],
    );
    let agent_id = data_string(&agent, "id").to_string();
    let intent = cli_success(
        &repo.root,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_id.clone(),
            "--task".to_string(),
            "implicit-start".to_string(),
            "--summary".to_string(),
            "Add an audit hook to Ledger".to_string(),
            "--scope".to_string(),
            "symbol:Ledger".to_string(),
        ],
    );
    let intent_id = intent["data"]["intent"]["id"]
        .as_str()
        .expect("intent id")
        .to_string();
    cli_success(
        &repo.root,
        None,
        vec![
            "work".to_string(),
            "claim".to_string(),
            "--agent".to_string(),
            agent_id.clone(),
            "--intent".to_string(),
            intent_id.clone(),
            "--scope".to_string(),
            "symbol:Ledger".to_string(),
        ],
    );

    let changeset = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_id,
            "--intent".to_string(),
            intent_id.clone(),
            "--summary".to_string(),
            "Add the Ledger audit hook".to_string(),
            "--symbol".to_string(),
            "Ledger".to_string(),
        ],
    );
    assert_eq!(changeset["data"]["status"], "PROVISIONAL");

    let work = cli_success(
        &repo.root,
        None,
        vec![
            "work".to_string(),
            "query".to_string(),
            "--status".to_string(),
            "PROVISIONAL".to_string(),
        ],
    );
    assert!(
        work["data"]
            .as_array()
            .expect("work query array")
            .iter()
            .any(|item| item["intent"]["id"] == intent_id)
    );

    let database = database_from_doctor(&repo.root);
    let connection = Connection::open(database).expect("open coordination database");
    let transitions: Vec<Value> = {
        let mut statement = connection
            .prepare(
                "SELECT payload_json FROM events WHERE entity_id = ?1 AND event_type = 'lifecycle.transitioned' ORDER BY seq",
            )
            .expect("prepare transition query");
        statement
            .query_map([&intent_id], |row| row.get::<_, String>(0))
            .expect("query transitions")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect transitions")
            .into_iter()
            .map(|payload| serde_json::from_str(&payload).expect("transition payload is JSON"))
            .collect()
    };
    assert_eq!(
        transitions,
        vec![
            json!({ "from": "INTENT", "to": "CLAIMED" }),
            json!({ "from": "CLAIMED", "to": "IN_PROGRESS" }),
            json!({ "from": "IN_PROGRESS", "to": "PROVISIONAL" }),
        ],
        "claim plus implicit publication must preserve all three lifecycle boundaries"
    );
}

#[test]
fn real_mcp_stdio_initializes_lists_tools_and_calls_one() {
    let temp = tempfile::tempdir().expect("create MCP fixture");
    let database = temp.path().join("mcp.sqlite3");
    let mut child = Command::new(foremerge_bin())
        .arg("--cwd")
        .arg(temp.path())
        .arg("--database")
        .arg(&database)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn MCP server");

    let mut stdin = child.stdin.take().expect("MCP stdin");
    let requests = [
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "foremerge-e2e", "version": "1" }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "register_agent",
                "arguments": {
                    "name": "mcp-e2e-agent",
                    "model": "test-model",
                    "capabilities": ["rust"]
                }
            }
        }),
    ];
    for request in requests {
        writeln!(stdin, "{request}").expect("write MCP request");
    }
    drop(stdin);

    let output = child.wait_with_output().expect("wait for MCP server");
    assert!(
        output.status.success(),
        "MCP process failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("MCP output is UTF-8");
    let responses = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("every MCP stdout line is JSON"))
        .collect::<Vec<_>>();
    assert_eq!(
        responses.len(),
        3,
        "notifications must not receive responses"
    );
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "foremerge");

    let names = responses[1]["result"]["tools"]
        .as_array()
        .expect("tools/list result")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
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
    assert_eq!(responses[2]["id"], 3);
    assert_eq!(responses[2]["result"]["isError"], false);
    assert!(
        responses[2]["result"]["structuredContent"]["id"]
            .as_str()
            .expect("registered agent id")
            .starts_with("agt_")
    );
    assert!(
        Store::open(&database)
            .expect("open MCP database")
            .verify_event_chain()
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_authenticated_json_api_persists_state_across_daemon_restart() {
    let temp = tempfile::tempdir().expect("create daemon fixture");
    let database = temp.path().join("daemon.sqlite3");
    let client = reqwest::Client::new();

    let first_address = unused_loopback_address();
    let first_url = format!("http://{first_address}");
    let first_daemon = spawn_daemon(temp.path(), &database, first_address);
    let initial_health = wait_for_health(&client, &first_url).await;
    assert_eq!(initial_health["ok"], true);
    assert_eq!(initial_health["data"]["status"], "ok");
    assert_eq!(initial_health["data"]["counts"]["agents"], 0);

    let unauthorized = client
        .post(format!("{first_url}/v1/agents/register"))
        .json(&json!({ "name": "unauthorized-agent" }))
        .send()
        .await
        .expect("send unauthenticated request");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    let token_path = temp.path().join(".foremerge").join("token");
    let token = fs::read_to_string(&token_path)
        .unwrap_or_else(|error| panic!("read daemon token at {}: {error}", token_path.display()));
    let registered: Value = client
        .post(format!("{first_url}/v1/agents/register"))
        .bearer_auth(token.trim())
        .json(&json!({
            "name": "api-persisted-agent",
            "model": "test-model",
            "capabilities": ["rust"]
        }))
        .send()
        .await
        .expect("send authenticated registration")
        .error_for_status()
        .expect("registration status")
        .json()
        .await
        .expect("registration JSON");
    assert_eq!(registered["ok"], true);
    let agent_id = registered["data"]["id"]
        .as_str()
        .expect("registered agent id")
        .to_string();
    first_daemon.stop();

    let second_address = unused_loopback_address();
    let second_url = format!("http://{second_address}");
    let second_daemon = spawn_daemon(temp.path(), &database, second_address);
    let restarted_health = wait_for_health(&client, &second_url).await;
    assert_eq!(restarted_health["data"]["counts"]["agents"], 1);
    assert_eq!(restarted_health["data"]["event_chain_ok"], true);

    let graph: Value = client
        .get(format!("{second_url}/v1/graph"))
        .bearer_auth(token.trim())
        .send()
        .await
        .expect("query graph after restart")
        .error_for_status()
        .expect("graph status")
        .json()
        .await
        .expect("graph JSON");
    assert_eq!(graph["ok"], true);
    assert!(
        graph["data"]["nodes"]
            .as_array()
            .expect("graph nodes")
            .iter()
            .any(|node| node["kind"] == "Agent" && node["key"] == agent_id),
        "restarted daemon must expose the agent persisted by the first process"
    );
    second_daemon.stop();
}

#[test]
fn event_log_triggers_reject_mutation_and_hash_chain_detects_insertion() {
    let temp = tempfile::tempdir().expect("create event fixture");
    let database = temp.path().join("events.sqlite3");
    for name in ["event-agent-a", "event-agent-b", "event-agent-c"] {
        cli_success(
            temp.path(),
            Some(&database),
            ["agent", "register", "--name", name, "--no-worktree"],
        );
    }
    assert!(
        Store::open(&database)
            .expect("open event store")
            .verify_event_chain()
            .expect("verify valid chain")
    );

    let connection = Connection::open(&database).expect("open SQLite database");
    let events = {
        let mut statement = connection
            .prepare(
                "SELECT seq, event_id, schema_version, event_type, entity_type, entity_id,
                 agent_id, payload_json, created_at, prev_hash, event_hash
                 FROM events ORDER BY seq",
            )
            .expect("prepare event query");
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                ))
            })
            .expect("read events")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect events")
    };
    assert_eq!(events.len(), 3);
    let mut expected_prev = "GENESIS".to_string();
    for (
        seq,
        event_id,
        schema_version,
        event_type,
        entity_type,
        entity_id,
        agent_id,
        payload,
        created_at,
        prev_hash,
        event_hash,
    ) in &events
    {
        assert_eq!(prev_hash, &expected_prev, "broken link at event {seq}");
        let material = format!(
            "{prev_hash}|{event_id}|{schema_version}|{event_type}|{entity_type}|{entity_id}|{}|{created_at}|{payload}",
            agent_id.as_deref().unwrap_or("")
        );
        let calculated = format!("{:x}", Sha256::digest(material.as_bytes()));
        assert_eq!(event_hash, &calculated, "wrong hash at event {seq}");
        expected_prev = event_hash.clone();
    }

    let update_error = connection
        .execute(
            "UPDATE events SET event_type = 'tampered' WHERE seq = 1",
            [],
        )
        .expect_err("event update must be rejected");
    assert!(
        update_error
            .to_string()
            .contains("event log is append-only")
    );
    let delete_error = connection
        .execute("DELETE FROM events WHERE seq = 1", [])
        .expect_err("event delete must be rejected");
    assert!(
        delete_error
            .to_string()
            .contains("event log is append-only")
    );
    drop(connection);
    assert!(
        Store::open(&database)
            .expect("reopen event store")
            .verify_event_chain()
            .expect("verify preserved chain")
    );

    let connection = Connection::open(&database).expect("open SQLite for tamper simulation");
    connection
        .execute(
            "INSERT INTO events(event_id, schema_version, event_type, entity_type, entity_id,
             agent_id, payload_json, created_at, prev_hash, event_hash)
             VALUES(?1, 1, 'tamper.inserted', 'Test', 'tampered', NULL, '{}', ?2, ?3, ?4)",
            params![
                "evt_deliberate_tamper",
                "2026-08-15T00:00:00Z",
                "not-the-previous-hash",
                "0000000000000000000000000000000000000000000000000000000000000000"
            ],
        )
        .expect("simulate a local actor inserting a malformed event");
    drop(connection);
    assert!(
        !Store::open(&database)
            .expect("reopen tampered event store")
            .verify_event_chain()
            .expect("detect malformed chain")
    );
}

#[test]
fn concurrent_cli_processes_register_without_losing_events() {
    let temp = tempfile::tempdir().expect("create concurrency fixture");
    let database = temp.path().join("concurrent.sqlite3");
    cli_success(temp.path(), Some(&database), ["init"]);

    let process_count = 12;
    let mut children = Vec::with_capacity(process_count);
    for index in 0..process_count {
        let mut command = cli_command(temp.path(), Some(&database));
        command
            .args(["agent", "register", "--name"])
            .arg(format!("parallel-agent-{index}"))
            .args(["--model", "concurrency-test", "--no-worktree"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        children.push(command.spawn().expect("spawn concurrent registration"));
    }

    let mut ids = HashSet::new();
    for child in children {
        let output = child.wait_with_output().expect("wait for registration");
        assert!(
            output.status.success(),
            "concurrent registration failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let value = parse_cli_json(&output);
        let id = data_string(&value, "id").to_string();
        assert!(ids.insert(id), "registration returned a duplicate ID");
    }

    let connection = Connection::open(&database).expect("open concurrency database");
    let agent_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM agents", [], |row| row.get(0))
        .expect("count registered agents");
    let event_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type = 'agent.registered'",
            [],
            |row| row.get(0),
        )
        .expect("count registration events");
    assert_eq!(ids.len(), process_count);
    assert_eq!(agent_count, process_count as i64);
    assert_eq!(event_count, process_count as i64);
    drop(connection);
    assert!(
        Store::open(&database)
            .expect("open concurrent event store")
            .verify_event_chain()
            .expect("verify concurrent event chain")
    );
}

#[test]
fn validation_finishing_after_accept_cannot_regress_accepted_state() {
    let repo = create_repo();
    let (agent_id, intent_id) =
        create_active_test_work(&repo.root, "accept-race-agent", "accept-race");
    let changeset_id = publish_test_revision(
        &repo.root,
        &agent_id,
        &intent_id,
        "Acceptance race candidate",
    );

    let first_validation = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            changeset_id.clone(),
            "--".to_string(),
            "git".to_string(),
            "diff".to_string(),
            "--quiet".to_string(),
            "HEAD".to_string(),
        ],
    );
    assert_eq!(first_validation["data"]["passed"], true);

    let started = repo.temp.path().join("accept-validation-started");
    let release = repo.temp.path().join("accept-validation-release");
    let slow_validation = spawn_sentinel_validation(&repo.root, &changeset_id, &started, &release);
    wait_for_sentinel(&started);

    let accept_output = cli_output(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "accept".to_string(),
            changeset_id.clone(),
        ],
    );
    fs::write(&release, b"release").expect("release validation command");
    let slow_output = slow_validation
        .wait_with_output()
        .expect("wait for late validation");

    assert!(
        accept_output.status.success(),
        "accept failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&accept_output.stdout),
        String::from_utf8_lossy(&accept_output.stderr)
    );
    assert_eq!(parse_cli_json(&accept_output)["data"]["status"], "ACCEPTED");
    assert!(
        !slow_output.status.success(),
        "late validation unexpectedly applied: {}",
        String::from_utf8_lossy(&slow_output.stdout)
    );
    let late_result = parse_cli_json(&slow_output);
    assert_eq!(late_result["error"]["code"], "STATE_RACE");

    let shown = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "show".to_string(),
            changeset_id.clone(),
        ],
    );
    assert_eq!(shown["data"]["status"], "ACCEPTED");
    let accepted_ref = format!("refs/foremerge/accepted/{changeset_id}");
    assert_eq!(
        git(&repo.root, ["rev-parse", accepted_ref.as_str()]),
        git(&repo.root, ["rev-parse", "HEAD"])
    );

    let database = database_from_doctor(&repo.root);
    let connection = Connection::open(database).expect("open acceptance race database");
    let stored: (String, String, i64, i64) = connection
        .query_row(
            "SELECT c.status, i.status,
                    (SELECT COUNT(*) FROM validations WHERE changeset_id = c.id),
                    (SELECT COUNT(*) FROM events WHERE event_type = 'validation.stale'
                     AND json_extract(payload_json, '$.changeset_id') = c.id)
             FROM changesets c JOIN intents i ON i.id = c.intent_id WHERE c.id = ?1",
            [&changeset_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read acceptance race state");
    assert_eq!(stored, ("ACCEPTED".into(), "ACCEPTED".into(), 1, 1));
}

#[test]
fn validation_of_a_superseded_changeset_is_rejected_and_stays_superseded() {
    let repo = create_repo();
    let (agent_id, intent_id) =
        create_active_test_work(&repo.root, "supersession-race-agent", "supersession-race");
    let old_id = publish_test_revision(
        &repo.root,
        &agent_id,
        &intent_id,
        "Original provisional revision",
    );

    let started = repo.temp.path().join("superseded-validation-started");
    let release = repo.temp.path().join("superseded-validation-release");
    let old_validation = spawn_sentinel_validation(&repo.root, &old_id, &started, &release);
    wait_for_sentinel(&started);

    let revision_output = cli_output(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_id,
            "--intent".to_string(),
            intent_id,
            "--summary".to_string(),
            "Replacement provisional revision".to_string(),
            "--symbol".to_string(),
            "RaceTarget".to_string(),
        ],
    );
    fs::write(&release, b"release").expect("release superseded validation");
    let old_validation_output = old_validation
        .wait_with_output()
        .expect("wait for superseded validation");

    assert!(
        revision_output.status.success(),
        "revision publication failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&revision_output.stdout),
        String::from_utf8_lossy(&revision_output.stderr)
    );
    let new_id = data_string(&parse_cli_json(&revision_output), "id").to_string();
    assert!(
        !old_validation_output.status.success(),
        "superseded validation unexpectedly applied: {}",
        String::from_utf8_lossy(&old_validation_output.stdout)
    );
    assert_eq!(
        parse_cli_json(&old_validation_output)["error"]["code"],
        "STATE_RACE"
    );

    let old = cli_success(
        &repo.root,
        None,
        vec!["changeset".to_string(), "show".to_string(), old_id.clone()],
    );
    let new = cli_success(
        &repo.root,
        None,
        vec!["changeset".to_string(), "show".to_string(), new_id.clone()],
    );
    assert_eq!(old["data"]["status"], "SUPERSEDED");
    assert_eq!(new["data"]["status"], "PROVISIONAL");
    assert_eq!(new["data"]["supersedes_changeset_id"], old_id);

    let rejected_again = cli_failure(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            old_id.clone(),
            "--".to_string(),
            "git".to_string(),
            "diff".to_string(),
            "--quiet".to_string(),
            "HEAD".to_string(),
        ],
    );
    assert_eq!(rejected_again["error"]["code"], "INVALID_TRANSITION");

    let database = database_from_doctor(&repo.root);
    let connection = Connection::open(database).expect("open supersession race database");
    let stored: (String, i64, i64) = connection
        .query_row(
            "SELECT status,
                    (SELECT COUNT(*) FROM validations WHERE changeset_id = ?1),
                    (SELECT COUNT(*) FROM events WHERE event_type = 'validation.stale'
                     AND json_extract(payload_json, '$.changeset_id') = ?1)
             FROM changesets WHERE id = ?1",
            [&old_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read superseded revision state");
    assert_eq!(stored, ("SUPERSEDED".into(), 0, 1));
}

#[test]
fn concurrent_changeset_publication_leaves_one_current_provisional_revision() {
    let repo = create_repo();
    let (agent_id, intent_id) =
        create_active_test_work(&repo.root, "revision-race-agent", "revision-race");
    let release = repo.temp.path().join("publishers-release");
    let process_count = 4;
    let mut children = Vec::with_capacity(process_count);
    let mut ready_files = Vec::with_capacity(process_count);

    for index in 0..process_count {
        let ready = repo.temp.path().join(format!("publisher-{index}-ready"));
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(
                "printf ready > \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.01; done; shift 2; exec \"$@\"",
            )
            .arg("foremerge-publication-barrier")
            .arg(&ready)
            .arg(&release)
            .arg(foremerge_bin())
            .arg("--json")
            .arg("--cwd")
            .arg(&repo.root)
            .args(["changeset", "publish", "--agent"])
            .arg(&agent_id)
            .arg("--intent")
            .arg(&intent_id)
            .arg("--summary")
            .arg(format!("Concurrent revision {index}"))
            .args(["--symbol", "RaceTarget"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        children.push(command.spawn().expect("spawn concurrent publisher"));
        ready_files.push(ready);
    }
    for ready in &ready_files {
        wait_for_sentinel(ready);
    }
    fs::write(&release, b"release").expect("release concurrent publishers");

    let mut published_ids = HashSet::new();
    for child in children {
        let output = child.wait_with_output().expect("wait for publisher");
        assert!(
            output.status.success(),
            "concurrent publication failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let value = parse_cli_json(&output);
        assert!(published_ids.insert(data_string(&value, "id").to_string()));
    }

    let database = database_from_doctor(&repo.root);
    let connection = Connection::open(&database).expect("open revision race database");
    let counts: (i64, i64, i64) = connection
        .query_row(
            "SELECT COUNT(*),
                    SUM(CASE WHEN status = 'PROVISIONAL' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status = 'SUPERSEDED' THEN 1 ELSE 0 END)
             FROM changesets WHERE intent_id = ?1",
            [&intent_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("count concurrent revisions");
    assert_eq!(counts, (process_count as i64, 1, process_count as i64 - 1));

    let revisions = {
        let mut statement = connection
            .prepare(
                "SELECT id, supersedes_changeset_id, status FROM changesets
                 WHERE intent_id = ?1",
            )
            .expect("prepare revision query");
        statement
            .query_map([&intent_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .expect("query revisions")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect revisions")
    };
    assert_eq!(
        revisions
            .iter()
            .filter(|(_, parent, _)| parent.is_none())
            .count(),
        1,
        "the publication chain must have exactly one root"
    );
    for (id, parent, status) in &revisions {
        assert!(published_ids.contains(id));
        if let Some(parent) = parent {
            assert!(published_ids.contains(parent));
        }
        if status == "PROVISIONAL" {
            assert!(
                !revisions
                    .iter()
                    .any(|(_, parent, _)| parent.as_deref() == Some(id.as_str())),
                "the current revision must be the leaf of the supersession chain"
            );
        }
    }
    drop(connection);
    assert!(
        Store::open(&database)
            .expect("reopen revision race store")
            .verify_event_chain()
            .expect("verify revision race event chain")
    );
}

#[test]
fn repeated_claim_overlap_warnings_use_the_persisted_conflict_and_graph_resolution() {
    let repo = create_repo();
    let first_agent = register_test_agent(&repo.root, "claim-owner-a");
    let second_agent = register_test_agent(&repo.root, "claim-owner-b");
    let first_intent = publish_test_intent(
        &repo.root,
        &first_agent,
        "first-claim",
        "Refactor the shared ledger",
        "symbol:SharedLedger",
    );
    let second_intent = publish_test_intent(
        &repo.root,
        &second_agent,
        "second-claim",
        "Instrument the shared ledger",
        "symbol:SharedLedger",
    );

    claim_test_scope(
        &repo.root,
        &first_agent,
        &first_intent,
        "symbol:SharedLedger",
    );
    let first_warning = claim_test_scope(
        &repo.root,
        &second_agent,
        &second_intent,
        "symbol:SharedLedger",
    );
    let repeated_warning = claim_test_scope(
        &repo.root,
        &second_agent,
        &second_intent,
        "symbol:SharedLedger",
    );
    let first_warnings = first_warning["data"]["warnings"]
        .as_array()
        .expect("first overlap warnings");
    let repeated_warnings = repeated_warning["data"]["warnings"]
        .as_array()
        .expect("repeated overlap warnings");
    assert_eq!(first_warnings.len(), 1);
    assert_eq!(repeated_warnings.len(), 1);
    let conflict_id = first_warnings[0]["id"]
        .as_str()
        .expect("first warning conflict id")
        .to_string();
    assert_eq!(
        repeated_warnings[0]["id"], conflict_id,
        "repeated warning must return the canonical persisted conflict ID"
    );

    let database = database_from_doctor(&repo.root);
    let connection = Connection::open(&database).expect("open claim conflict database");
    let persisted = {
        let mut statement = connection
            .prepare("SELECT id, status FROM conflicts WHERE kind = 'overlapping_claim'")
            .expect("prepare claim conflict query");
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query claim conflicts")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect claim conflicts")
    };
    assert_eq!(persisted, vec![(conflict_id.clone(), "OPEN".into())]);
    drop(connection);

    let graph_before = cli_success(&repo.root, None, ["graph"]);
    let before_nodes = graph_before["data"]["nodes"]
        .as_array()
        .expect("graph nodes")
        .iter()
        .filter(|node| node["kind"] == "Conflict" && node["data"]["kind"] == "overlapping_claim")
        .collect::<Vec<_>>();
    assert_eq!(before_nodes.len(), 1);
    assert_eq!(before_nodes[0]["key"], conflict_id);
    assert_eq!(before_nodes[0]["data"]["status"], "OPEN");

    let resolved = cli_success(
        &repo.root,
        None,
        vec![
            "conflicts".to_string(),
            "resolve".to_string(),
            conflict_id.clone(),
            "--agent".to_string(),
            second_agent,
            "--resolution".to_string(),
            "Share the stable ledger contract".to_string(),
            "--rationale".to_string(),
            "Both agents can work behind a stable interface".to_string(),
        ],
    );
    assert_eq!(resolved["data"]["id"], conflict_id);
    assert_eq!(resolved["data"]["status"], "RESOLVED");

    let graph_after = cli_success(&repo.root, None, ["graph"]);
    let after_nodes = graph_after["data"]["nodes"]
        .as_array()
        .expect("graph nodes")
        .iter()
        .filter(|node| node["kind"] == "Conflict" && node["data"]["kind"] == "overlapping_claim")
        .collect::<Vec<_>>();
    assert_eq!(after_nodes.len(), 1);
    assert_eq!(after_nodes[0]["key"], conflict_id);
    assert_eq!(after_nodes[0]["data"]["status"], "RESOLVED");
    let conflict_node_id = after_nodes[0]["id"]
        .as_str()
        .expect("conflict graph node id");
    assert!(
        graph_after["data"]["edges"]
            .as_array()
            .expect("graph edges")
            .iter()
            .any(|edge| edge["from"] == conflict_node_id && edge["kind"] == "RESOLVED_BY"),
        "resolved conflict graph node must link to its recorded decision"
    );

    let connection = Connection::open(database).expect("reopen claim conflict database");
    let final_status: String = connection
        .query_row(
            "SELECT status FROM conflicts WHERE id = ?1",
            [&conflict_id],
            |row| row.get(0),
        )
        .expect("read resolved conflict status");
    assert_eq!(final_status, "RESOLVED");
}

#[test]
fn acceptance_checks_dependencies_against_the_exact_candidate_commit() {
    let repo = create_repo();
    let base_commit = git(&repo.root, ["rev-parse", "HEAD"]);
    git(&repo.root, ["switch", "--quiet", "-c", "dependency-branch"]);
    git(
        &repo.root,
        [
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "accepted dependency",
        ],
    );
    let dependency_commit = git(&repo.root, ["rev-parse", "HEAD"]);
    let candidate_worktree = repo.temp.path().join("dependency-bypass-candidate");
    git(
        &repo.root,
        vec![
            "worktree".into(),
            "add".into(),
            "--quiet".into(),
            "-b".into(),
            "candidate-without-dependency".into(),
            candidate_worktree.as_os_str().to_owned(),
            base_commit.clone().into(),
        ],
    );
    let candidate_commit = git(&candidate_worktree, ["rev-parse", "HEAD"]);
    let ancestry = Command::new("git")
        .arg("-C")
        .arg(&candidate_worktree)
        .args([
            "merge-base",
            "--is-ancestor",
            dependency_commit.as_str(),
            candidate_commit.as_str(),
        ])
        .status()
        .expect("prove candidate omits dependency");
    assert_eq!(
        ancestry.code(),
        Some(1),
        "fixture candidate must not contain the dependency commit"
    );

    let (dependency_agent, dependency_intent) =
        create_active_test_work(&repo.root, "dependency-owner", "accepted-dependency-intent");
    let dependency_changeset = publish_test_revision(
        &repo.root,
        &dependency_agent,
        &dependency_intent,
        "Accepted dependency revision",
    );
    cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            dependency_changeset.clone(),
            "--".to_string(),
            "true".to_string(),
        ],
    );
    let accepted_dependency = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "accept".to_string(),
            dependency_changeset,
        ],
    );
    assert_eq!(accepted_dependency["data"]["git_ref"], dependency_commit);

    let candidate_agent = register_test_agent(&candidate_worktree, "candidate-owner");
    let candidate_intent = cli_success(
        &candidate_worktree,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            candidate_agent.clone(),
            "--task".to_string(),
            "candidate-with-required-dependency".to_string(),
            "--summary".to_string(),
            "Build a candidate that declares the accepted dependency".to_string(),
            "--scope".to_string(),
            "component:DependencyConsumer".to_string(),
            "--depends-on".to_string(),
            dependency_intent.clone(),
        ],
    );
    let candidate_intent = candidate_intent["data"]["intent"]["id"]
        .as_str()
        .expect("candidate intent id")
        .to_string();
    claim_test_scope(
        &candidate_worktree,
        &candidate_agent,
        &candidate_intent,
        "component:DependencyConsumer",
    );
    start_test_work(&candidate_worktree, &candidate_agent, &candidate_intent);
    let candidate_changeset = cli_success(
        &candidate_worktree,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            candidate_agent,
            "--intent".to_string(),
            candidate_intent,
            "--summary".to_string(),
            "Candidate with a misleading publication ref".to_string(),
            "--dependency".to_string(),
            dependency_intent,
            "--git-ref".to_string(),
            dependency_commit,
            "--worktree".to_string(),
            candidate_worktree.to_string_lossy().into_owned(),
        ],
    );
    let candidate_changeset = data_string(&candidate_changeset, "id").to_string();
    cli_success(
        &candidate_worktree,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            candidate_changeset.clone(),
            "--".to_string(),
            "true".to_string(),
        ],
    );

    let rejected = cli_failure(
        &candidate_worktree,
        None,
        vec![
            "changeset".to_string(),
            "accept".to_string(),
            candidate_changeset.clone(),
            "--git-ref".to_string(),
            candidate_commit,
        ],
    );
    assert_eq!(rejected["error"]["code"], "UNSATISFIED_DEPENDENCY");
    let shown = cli_success(
        &candidate_worktree,
        None,
        vec![
            "changeset".to_string(),
            "show".to_string(),
            candidate_changeset.clone(),
        ],
    );
    assert_eq!(shown["data"]["status"], "VALIDATED");
    let accepted_ref = format!("refs/foremerge/accepted/{candidate_changeset}");
    let ref_status = Command::new("git")
        .arg("-C")
        .arg(&candidate_worktree)
        .args(["show-ref", "--verify", "--quiet", &accepted_ref])
        .status()
        .expect("inspect rejected candidate ref");
    assert!(
        !ref_status.success(),
        "dependency-bypassing acceptance must not create an accepted ref"
    );
}

#[test]
fn default_store_rejects_changeset_worktrees_from_another_repository() {
    let project = create_repo();
    let foreign = create_repo();
    cli_success(&project.root, None, ["init"]);

    let registered_agent = register_test_agent(&project.root, "project-agent");
    let registered_intent = publish_test_intent(
        &project.root,
        &registered_agent,
        "registered-cross-repository-attempt",
        "Keep the ChangeSet in the registered repository",
        "component:RegisteredProjectWork",
    );
    claim_test_scope(
        &project.root,
        &registered_agent,
        &registered_intent,
        "component:RegisteredProjectWork",
    );
    start_test_work(&project.root, &registered_agent, &registered_intent);
    let registered_rejection = cli_failure(
        &project.root,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            registered_agent,
            "--intent".to_string(),
            registered_intent,
            "--summary".to_string(),
            "Attempt to publish into a foreign repository".to_string(),
            "--worktree".to_string(),
            foreign.root.to_string_lossy().into_owned(),
        ],
    );
    assert_eq!(registered_rejection["error"]["code"], "INVALID_INPUT");

    let no_worktree_agent = cli_success(
        &project.root,
        None,
        [
            "agent",
            "register",
            "--name",
            "no-worktree-project-agent",
            "--model",
            "e2e-test",
            "--no-worktree",
        ],
    );
    let no_worktree_agent = data_string(&no_worktree_agent, "id").to_string();
    let no_worktree_intent = publish_test_intent(
        &project.root,
        &no_worktree_agent,
        "no-worktree-cross-repository-attempt",
        "Use the default project database without registered worktree metadata",
        "component:NoWorktreeProjectWork",
    );
    claim_test_scope(
        &project.root,
        &no_worktree_agent,
        &no_worktree_intent,
        "component:NoWorktreeProjectWork",
    );
    start_test_work(&project.root, &no_worktree_agent, &no_worktree_intent);
    let no_worktree_rejection = cli_failure(
        &project.root,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            no_worktree_agent,
            "--intent".to_string(),
            no_worktree_intent,
            "--summary".to_string(),
            "Attempt to bind the default project database to a foreign repository".to_string(),
            "--worktree".to_string(),
            foreign.root.to_string_lossy().into_owned(),
        ],
    );
    assert_eq!(no_worktree_rejection["error"]["code"], "INVALID_INPUT");

    let database = database_from_doctor(&project.root);
    let connection = Connection::open(database).expect("open repository-bound store");
    let counts: (i64, i64) = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM changesets),
                    (SELECT COUNT(*) FROM meta WHERE key = 'repository_common_dir')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read repository binding state");
    assert_eq!(counts, (0, 1));
    let foreign_refs = git(
        &foreign.root,
        ["for-each-ref", "--format=%(refname)", "refs/foremerge"],
    );
    assert!(
        foreign_refs.is_empty(),
        "a rejected foreign worktree must not receive Foremerge refs: {foreign_refs}"
    );
}

#[test]
fn committed_dependency_keeps_its_original_accepted_ref_for_ancestry_checks() {
    let repo = create_repo();
    git(
        &repo.root,
        ["switch", "--quiet", "-c", "accepted-dependency"],
    );
    git(
        &repo.root,
        [
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "accepted dependency candidate",
        ],
    );
    let accepted_commit = git(&repo.root, ["rev-parse", "HEAD"]);

    let (dependency_agent, dependency_intent) = create_active_test_work(
        &repo.root,
        "accepted-ref-dependency-owner",
        "accepted-ref-dependency",
    );
    let dependency_changeset = publish_test_revision(
        &repo.root,
        &dependency_agent,
        &dependency_intent,
        "Dependency candidate pinned at the accepted commit",
    );
    cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            dependency_changeset.clone(),
            "--".to_string(),
            "true".to_string(),
        ],
    );
    let accepted = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "accept".to_string(),
            dependency_changeset.clone(),
        ],
    );
    assert_eq!(accepted["data"]["git_ref"], accepted_commit);

    git(
        &repo.root,
        ["switch", "--quiet", "-c", "dependency-integration"],
    );
    git(
        &repo.root,
        [
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "integrate accepted dependency",
        ],
    );
    let integration_commit = git(&repo.root, ["rev-parse", "HEAD"]);
    assert_ne!(integration_commit, accepted_commit);
    let committed = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "commit".to_string(),
            dependency_changeset.clone(),
            "--git-ref".to_string(),
            integration_commit.clone(),
        ],
    );
    assert_eq!(committed["data"]["status"], "COMMITTED");
    assert_eq!(committed["data"]["git_ref"], integration_commit);
    let accepted_ref = format!("refs/foremerge/accepted/{dependency_changeset}");
    assert_eq!(
        git(&repo.root, ["rev-parse", accepted_ref.as_str()]),
        accepted_commit,
        "recording integration must not move the immutable accepted ref"
    );

    let candidate_worktree = repo.temp.path().join("candidate-from-accepted-ref");
    git(
        &repo.root,
        vec![
            "worktree".into(),
            "add".into(),
            "--quiet".into(),
            "-b".into(),
            "candidate-from-accepted-ref".into(),
            candidate_worktree.as_os_str().to_owned(),
            accepted_commit.clone().into(),
        ],
    );
    let candidate_agent = register_test_agent(&candidate_worktree, "accepted-ref-consumer");
    let candidate_intent = cli_success(
        &candidate_worktree,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            candidate_agent.clone(),
            "--task".to_string(),
            "consume-original-accepted-ref".to_string(),
            "--summary".to_string(),
            "Build on the dependency candidate without its later integration commit".to_string(),
            "--scope".to_string(),
            "component:AcceptedRefConsumer".to_string(),
            "--depends-on".to_string(),
            dependency_intent.clone(),
        ],
    );
    let candidate_intent = candidate_intent["data"]["intent"]["id"]
        .as_str()
        .expect("candidate intent id")
        .to_string();
    claim_test_scope(
        &candidate_worktree,
        &candidate_agent,
        &candidate_intent,
        "component:AcceptedRefConsumer",
    );
    start_test_work(&candidate_worktree, &candidate_agent, &candidate_intent);
    let candidate_changeset = cli_success(
        &candidate_worktree,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            candidate_agent,
            "--intent".to_string(),
            candidate_intent,
            "--summary".to_string(),
            "Consumer based on the dependency's accepted candidate".to_string(),
            "--dependency".to_string(),
            dependency_intent,
            "--symbol".to_string(),
            "AcceptedRefConsumer".to_string(),
        ],
    );
    let candidate_changeset = data_string(&candidate_changeset, "id").to_string();
    cli_success(
        &candidate_worktree,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            candidate_changeset.clone(),
            "--".to_string(),
            "true".to_string(),
        ],
    );
    let candidate_accepted = cli_success(
        &candidate_worktree,
        None,
        vec![
            "changeset".to_string(),
            "accept".to_string(),
            candidate_changeset,
        ],
    );
    assert_eq!(candidate_accepted["data"]["status"], "ACCEPTED");
    assert_eq!(candidate_accepted["data"]["git_ref"], accepted_commit);
}

#[test]
fn moved_dependency_ref_cannot_replace_the_pinned_accepted_commit() {
    let repo = create_repo();
    let base_commit = git(&repo.root, ["rev-parse", "HEAD"]);
    git(&repo.root, ["switch", "--quiet", "-c", "tamper-dependency"]);
    git(
        &repo.root,
        [
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "dependency candidate",
        ],
    );
    let accepted_commit = git(&repo.root, ["rev-parse", "HEAD"]);
    let (dependency_agent, dependency_intent) =
        create_active_test_work(&repo.root, "tamper-dependency-owner", "tamper-dependency");
    let dependency_changeset = publish_test_revision(
        &repo.root,
        &dependency_agent,
        &dependency_intent,
        "Dependency with an immutable accepted pin",
    );
    cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            dependency_changeset.clone(),
            "--".to_string(),
            "true".to_string(),
        ],
    );
    let accepted = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "accept".to_string(),
            dependency_changeset.clone(),
        ],
    );
    assert_eq!(accepted["data"]["accepted_commit"], accepted_commit);
    assert_eq!(accepted["data"]["integration_commit"], Value::Null);

    git(
        &repo.root,
        [
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "integrate dependency",
        ],
    );
    let integration_commit = git(&repo.root, ["rev-parse", "HEAD"]);
    let committed = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "commit".to_string(),
            dependency_changeset.clone(),
            "--git-ref".to_string(),
            integration_commit.clone(),
        ],
    );
    assert_eq!(committed["data"]["accepted_commit"], accepted_commit);
    assert_eq!(committed["data"]["integration_commit"], integration_commit);

    git(
        &repo.root,
        [
            "switch",
            "--quiet",
            "-C",
            "tampered-accepted-ref",
            base_commit.as_str(),
        ],
    );
    git(
        &repo.root,
        [
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "unrelated replacement commit",
        ],
    );
    let replacement_commit = git(&repo.root, ["rev-parse", "HEAD"]);
    let accepted_ref = format!("refs/foremerge/accepted/{dependency_changeset}");
    git(
        &repo.root,
        [
            "update-ref",
            accepted_ref.as_str(),
            replacement_commit.as_str(),
        ],
    );
    let ancestry = Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args([
            "merge-base",
            "--is-ancestor",
            accepted_commit.as_str(),
            replacement_commit.as_str(),
        ])
        .status()
        .expect("prove replacement omits the pinned dependency");
    assert_eq!(ancestry.code(), Some(1));

    let consumer_agent = register_test_agent(&repo.root, "tampered-ref-consumer");
    let consumer = cli_success(
        &repo.root,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            consumer_agent.clone(),
            "--task".to_string(),
            "consume-tampered-ref".to_string(),
            "--summary".to_string(),
            "Reject a moved dependency ref".to_string(),
            "--scope".to_string(),
            "component:TamperedRefConsumer".to_string(),
            "--depends-on".to_string(),
            dependency_intent.clone(),
        ],
    );
    let consumer_intent = consumer["data"]["intent"]["id"]
        .as_str()
        .expect("consumer intent id")
        .to_string();
    claim_test_scope(
        &repo.root,
        &consumer_agent,
        &consumer_intent,
        "component:TamperedRefConsumer",
    );
    start_test_work(&repo.root, &consumer_agent, &consumer_intent);
    let published = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            consumer_agent,
            "--intent".to_string(),
            consumer_intent,
            "--summary".to_string(),
            "Consumer built only on the moved ref".to_string(),
            "--dependency".to_string(),
            dependency_intent,
            "--symbol".to_string(),
            "TamperedRefConsumer".to_string(),
        ],
    );
    let consumer_changeset = data_string(&published, "id").to_string();
    cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            consumer_changeset.clone(),
            "--".to_string(),
            "true".to_string(),
        ],
    );
    let rejected = cli_failure(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "accept".to_string(),
            consumer_changeset.clone(),
        ],
    );
    assert_eq!(rejected["error"]["code"], "UNSATISFIED_DEPENDENCY");
    assert!(
        rejected["error"]["message"]
            .as_str()
            .expect("rejection message")
            .contains("no longer matches its pinned commit")
    );
    let shown = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "show".to_string(),
            consumer_changeset.clone(),
        ],
    );
    assert_eq!(shown["data"]["status"], "VALIDATED");
    let consumer_ref = format!("refs/foremerge/accepted/{consumer_changeset}");
    let ref_lookup = Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["show-ref", "--verify", consumer_ref.as_str()])
        .output()
        .expect("inspect rejected consumer ref");
    assert!(!ref_lookup.status.success());
}

#[test]
fn discarding_one_side_of_a_high_conflict_unblocks_the_survivor() {
    let repo = create_repo();
    let survivor_agent = register_test_agent(&repo.root, "surviving-stripe-agent");
    let discarded_agent = register_test_agent(&repo.root, "discarded-paypal-agent");
    let survivor_intent = cli_success(
        &repo.root,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            survivor_agent.clone(),
            "--task".to_string(),
            "replace-payment-service".to_string(),
            "--summary".to_string(),
            "Replace PaymentService with StripePaymentService".to_string(),
            "--scope".to_string(),
            "symbol:PaymentService".to_string(),
            "--scope".to_string(),
            "contract:payments.provider".to_string(),
        ],
    );
    let survivor_intent = survivor_intent["data"]["intent"]["id"]
        .as_str()
        .expect("survivor intent id")
        .to_string();
    let discarded_intent = cli_success(
        &repo.root,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            discarded_agent.clone(),
            "--task".to_string(),
            "add-paypal-support".to_string(),
            "--summary".to_string(),
            "Add PayPal support to PaymentService".to_string(),
            "--scope".to_string(),
            "symbol:PaymentService".to_string(),
            "--scope".to_string(),
            "contract:payments.provider".to_string(),
        ],
    );
    let high_conflict = discarded_intent["data"]["conflicts"]
        .as_array()
        .expect("published conflicts")
        .iter()
        .find(|conflict| conflict["severity"] == "HIGH")
        .expect("HIGH conflict")
        .clone();
    let high_conflict_id = high_conflict["id"]
        .as_str()
        .expect("persisted HIGH conflict id")
        .to_string();
    let discarded_intent = discarded_intent["data"]["intent"]["id"]
        .as_str()
        .expect("discarded intent id")
        .to_string();

    claim_test_scope(
        &repo.root,
        &survivor_agent,
        &survivor_intent,
        "symbol:PaymentService",
    );
    start_test_work(&repo.root, &survivor_agent, &survivor_intent);
    let survivor_changeset = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            survivor_agent.clone(),
            "--intent".to_string(),
            survivor_intent,
            "--summary".to_string(),
            "Validated Stripe provider candidate".to_string(),
            "--symbol".to_string(),
            "PaymentService".to_string(),
        ],
    );
    let survivor_changeset = data_string(&survivor_changeset, "id").to_string();
    cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            survivor_changeset.clone(),
            "--".to_string(),
            "true".to_string(),
        ],
    );
    let blocked = cli_failure(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "accept".to_string(),
            survivor_changeset.clone(),
        ],
    );
    assert_eq!(blocked["error"]["code"], "BLOCKING_CONFLICT");

    let discarded = cli_success(
        &repo.root,
        None,
        vec![
            "work".to_string(),
            "discard".to_string(),
            discarded_intent,
            "--agent".to_string(),
            discarded_agent,
            "--reason".to_string(),
            "Stripe owns the provider migration".to_string(),
        ],
    );
    assert_eq!(discarded["data"]["status"], "DISCARDED");
    let database = database_from_doctor(&repo.root);
    let connection = Connection::open(database).expect("open discarded conflict database");
    let conflict_status: String = connection
        .query_row(
            "SELECT status FROM conflicts WHERE id = ?1",
            [&high_conflict_id],
            |row| row.get(0),
        )
        .expect("read dismissed HIGH conflict");
    assert_eq!(conflict_status, "DISMISSED");
    drop(connection);

    let accepted = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "accept".to_string(),
            survivor_changeset,
        ],
    );
    assert_eq!(accepted["data"]["status"], "ACCEPTED");
}

#[cfg(unix)]
#[test]
fn validation_timeout_kills_descendants_in_the_validation_process_group() {
    let repo = create_repo();
    let (agent_id, intent_id) =
        create_active_test_work(&repo.root, "timeout-agent", "timeout-process-group");
    let changeset_id = publish_test_revision(
        &repo.root,
        &agent_id,
        &intent_id,
        "Validation with a background descendant",
    );
    let descendant_started = repo.temp.path().join("validation-descendant-started");
    let descendant_survived = repo.temp.path().join("validation-descendant-survived");
    let validation = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            changeset_id,
            "--timeout-seconds".to_string(),
            "1".to_string(),
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            "(sleep 2; printf survived > \"$2\") & descendant=$!; printf '%s' \"$descendant\" > \"$1\"; wait \"$descendant\"".to_string(),
            "foremerge-validation-timeout".to_string(),
            descendant_started.to_string_lossy().into_owned(),
            descendant_survived.to_string_lossy().into_owned(),
        ],
    );
    assert_eq!(validation["data"]["passed"], false);
    assert!(
        validation["data"]["stderr"]
            .as_str()
            .expect("validation stderr")
            .contains("timed out after 1 seconds")
    );
    assert!(
        descendant_started.is_file(),
        "the background descendant must have started before the timeout"
    );
    std::thread::sleep(Duration::from_millis(2_200));
    assert!(
        !descendant_survived.exists(),
        "a descendant survived long enough to write after the process-group timeout"
    );
}

#[test]
fn validation_output_is_bounded_to_the_latest_sixteen_kibibytes_per_stream() {
    let repo = create_repo();
    let (agent_id, intent_id) = create_active_test_work(
        &repo.root,
        "bounded-output-agent",
        "bounded-validation-output",
    );
    let changeset_id = publish_test_revision(
        &repo.root,
        &agent_id,
        &intent_id,
        "Validation with deliberately large output",
    );
    let validation = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            changeset_id,
            "--timeout-seconds".to_string(),
            "10".to_string(),
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            "printf '%050000d' 0; printf '%050000d' 1 >&2".to_string(),
        ],
    );
    assert_eq!(validation["data"]["passed"], true);
    let stdout = validation["data"]["stdout"]
        .as_str()
        .expect("bounded validation stdout");
    let stderr = validation["data"]["stderr"]
        .as_str()
        .expect("bounded validation stderr");
    assert_eq!(stdout.len(), 16 * 1024);
    assert_eq!(stderr.len(), 16 * 1024);
    assert!(stdout.ends_with('0'));
    assert!(stderr.ends_with('1'));
}
