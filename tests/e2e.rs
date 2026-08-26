use foremerge::{Foremerge, Store, checks, mcp};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};
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

fn fmg_bin() -> &'static Path {
    assert_cmd::cargo::cargo_bin!("fmg")
}

/// `fmg` is the short name for the same program, so it has to be built, has to
/// behave identically, and has to name itself rather than `foremerge` in help.
#[test]
fn the_short_name_is_the_same_program_and_calls_itself_by_the_short_name() {
    let long = Command::new(foremerge_bin())
        .arg("--version")
        .output()
        .expect("foremerge runs");
    let short = Command::new(fmg_bin())
        .arg("--version")
        .output()
        .expect("fmg runs");
    assert!(long.status.success() && short.status.success());

    // Byte-identical version output, which is the simplest proof that the two
    // names are one build rather than two things that can drift apart. Both
    // report the product name, so `fmg --version` says `foremerge`: usage text
    // should echo what you typed, but a version should identify the product.
    let long_version = String::from_utf8_lossy(&long.stdout).trim().to_string();
    let short_version = String::from_utf8_lossy(&short.stdout).trim().to_string();
    assert_eq!(
        long_version, short_version,
        "the two names should report one version"
    );
    assert!(
        short_version.starts_with("foremerge "),
        "expected the product name in --version, got {short_version}"
    );

    // Usage text has to say `fmg`, or the short name would tell people to type
    // the long one, which defeats the point of having it.
    let help = Command::new(fmg_bin())
        .arg("--help")
        .output()
        .expect("fmg --help runs");
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.contains("Usage: fmg"),
        "fmg --help should name fmg, got:\n{help}"
    );

    // The subcommand tree is the same program, not a reduced shim.
    let long_help = Command::new(foremerge_bin())
        .arg("--help")
        .output()
        .expect("foremerge --help runs");
    let long_help = String::from_utf8_lossy(&long_help.stdout);
    assert_eq!(
        help.replace("Usage: fmg", "Usage: foremerge"),
        long_help.to_string(),
        "the two names should present the same interface"
    );
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
    cli_success(cwd, None, ["init"]);
    let doctor = cli_success(cwd, None, ["doctor"]);
    assert_eq!(doctor["data"]["database_ok"], true);
    PathBuf::from(data_string(&doctor, "database"))
}

/// Multiplier for every wall-clock wait bound in this suite.
///
/// Deadline-based waits race real time, so a machine busy with other work can
/// fail them while the code under test is perfectly correct. That happened at
/// a load average around 748: three validation tests failed non-deterministically
/// and passed again once the machine was idle, which costs a debugging cycle
/// every time because the first hypothesis is always "I broke something".
///
/// Raising `FOREMERGE_TEST_TIMEOUT_SCALE` gives those waits more room without
/// touching any duration a test actually asserts on.
fn timeout_scale() -> u32 {
    std::env::var("FOREMERGE_TEST_TIMEOUT_SCALE")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .filter(|scale| *scale >= 1)
        .unwrap_or(1)
}

/// How long a test may wait for something to happen.
///
/// Only for wait bounds. Never use this for a duration under test, such as a
/// configured validation timeout, or the test would stop checking the thing it
/// exists to check.
fn wait_budget(base: Duration) -> Duration {
    base * timeout_scale()
}

fn wait_for_sentinel(path: &Path) {
    let budget = wait_budget(Duration::from_secs(30));
    let deadline = Instant::now() + budget;
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "sentinel was not created within {}s: {}\n\
             If this machine is busy, raise FOREMERGE_TEST_TIMEOUT_SCALE and retry \
             before assuming a regression.",
            budget.as_secs(),
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
        "symbol:RaceTarget=extend",
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
            "symbol:PaymentService=replace".to_string(),
            "--scope".to_string(),
            "contract:payments.provider=replace".to_string(),
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
            "symbol:PaymentService=extend".to_string(),
            "--scope".to_string(),
            "contract:payments.provider=extend".to_string(),
        ],
    );

    let conflicts = second["data"]["conflicts"]
        .as_array()
        .expect("publish_intent returns conflicts");
    let conflict = conflicts
        .iter()
        .find(|value| value["kind"] == "destructive_vs_additive")
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
            "symbol:SharedLedger=extend".to_string(),
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
            "symbol:PaymentProvider=extend".to_string(),
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
            "symbol:PaymentProvider=extend".to_string(),
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
fn stale_validation_attempt_is_retained_and_exclusion_rules_enable_a_safe_revision() {
    let repo = create_repo();
    let (agent_id, intent_id) =
        create_active_test_work(&repo.root, "artifact-agent", "validation-artifact");
    let first_changeset = publish_test_revision(
        &repo.root,
        &agent_id,
        &intent_id,
        "Candidate before artifact policy",
    );

    let stale = cli_failure(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            first_changeset.clone(),
            "--timeout-seconds".to_string(),
            "5".to_string(),
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            "printf coverage > coverage.log".to_string(),
        ],
    );
    assert_eq!(stale["error"]["code"], "STALE_CHANGESET");
    assert!(
        stale["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("coverage.log")),
        "stale diagnostic must name the generated path: {stale}"
    );
    assert!(
        !stale["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exclusion rules changed")
    );
    let attempts = cli_success(
        &repo.root,
        None,
        ["changeset", "attempts", &first_changeset],
    );
    assert_eq!(attempts["data"].as_array().map(Vec::len), Some(1));
    assert_eq!(attempts["data"][0]["passed"], true);
    assert_eq!(attempts["data"][0]["authoritative"], false);
    assert_eq!(
        attempts["data"][0]["changed_files"],
        json!(["coverage.log"])
    );
    assert!(
        attempts["data"][0]["stale_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("fingerprint changed"))
    );
    let database = database_from_doctor(&repo.root);
    let connection = Connection::open(database).expect("open coordination database");
    let attempt_counts: (i64, i64, i64) = connection
        .query_row(
            "SELECT
             (SELECT COUNT(*) FROM validation_attempts WHERE changeset_id = ?1),
             (SELECT COUNT(*) FROM validation_attempts WHERE changeset_id = ?1 AND authoritative = 1),
             (SELECT COUNT(*) FROM validations WHERE changeset_id = ?1)",
            [&first_changeset],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("inspect validation projections");
    assert_eq!(attempt_counts, (1, 0, 0));
    let attempt_id = attempts["data"][0]["id"]
        .as_str()
        .expect("validation attempt id");
    assert!(
        connection
            .execute(
                "UPDATE validation_attempts SET authoritative = 1 WHERE id = ?1",
                [attempt_id],
            )
            .is_err(),
        "validation attempt rows must be immutable"
    );
    let rejected = cli_failure(&repo.root, None, ["changeset", "accept", &first_changeset]);
    assert_ne!(rejected["error"]["code"], "INTERNAL_ERROR");

    let policy = cli_success(
        &repo.root,
        None,
        ["validation-exclusions", "set", "--path", "coverage.log"],
    );
    assert_eq!(policy["data"]["ruleset"]["paths"], json!(["coverage.log"]));
    assert_eq!(policy["data"]["mcp_mutation_allowed"], false);
    let second_changeset = publish_test_revision(
        &repo.root,
        &agent_id,
        &intent_id,
        "Candidate with digest-bound artifact policy",
    );
    // A validation must begin with no excluded artifacts present, so the
    // leftover from the previous run has to go first. Otherwise this run could
    // consume content that is not in the candidate commit, and Foremerge cannot
    // tell consumption from overwriting.
    let stale_artifact = cli_failure(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            second_changeset.clone(),
            "--".to_string(),
            "true".to_string(),
        ],
    );
    assert_eq!(stale_artifact["error"]["code"], "CHECK_FAILED");
    assert!(
        stale_artifact["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("coverage.log")),
        "the refusal must name the artifact: {stale_artifact}"
    );
    fs::remove_file(repo.root.join("coverage.log")).expect("clear the previous artifact");

    let validation = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "validate".to_string(),
            second_changeset.clone(),
            "--timeout-seconds".to_string(),
            "5".to_string(),
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            "printf newer-coverage > coverage.log".to_string(),
        ],
    );
    assert_eq!(validation["data"]["passed"], true);
    let authoritative = cli_success(
        &repo.root,
        None,
        ["changeset", "attempts", &second_changeset],
    );
    assert_eq!(authoritative["data"][0]["authoritative"], true);
    assert_eq!(
        authoritative["data"][0]["excluded_paths"],
        json!(["coverage.log"])
    );
    assert!(
        authoritative["data"][0]["exclusion_ruleset_digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:"))
    );

    // The excluded artifact is deliberately outside the fingerprint, so it does
    // not make the worktree dirty. Acceptance must still refuse while it is
    // present: the validated tree contained it and the accepted commit does
    // not, so accepting here would bless a candidate whose validation may have
    // depended on content missing from the commit.
    let blocked = cli_failure(&repo.root, None, ["changeset", "accept", &second_changeset]);
    assert_eq!(blocked["error"]["code"], "CHECK_FAILED");
    assert!(
        blocked["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("coverage.log")),
        "acceptance must name the excluded artifact: {blocked}"
    );

    fs::remove_file(repo.root.join("coverage.log")).expect("remove generated artifact");
    let accepted = cli_success(&repo.root, None, ["changeset", "accept", &second_changeset]);
    assert_eq!(accepted["data"]["status"], "ACCEPTED");
}

#[cfg(unix)]
#[test]
fn final_validation_snapshot_does_not_hold_the_coordination_write_lock() {
    use std::os::unix::fs::PermissionsExt;

    let repo = create_repo();
    let (agent_id, intent_id) =
        create_active_test_work(&repo.root, "snapshot-lock-agent", "snapshot-lock");
    let changeset_id = publish_test_revision(
        &repo.root,
        &agent_id,
        &intent_id,
        "Candidate whose final snapshot is deliberately paused",
    );

    let started = repo.temp.path().join("final-snapshot-started");
    let release = repo.temp.path().join("final-snapshot-release");
    let fsmonitor_hook = repo.temp.path().join("paused-fsmonitor-hook.sh");
    fs::write(
        &fsmonitor_hook,
        format!(
            "#!/bin/sh\nprintf started > '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\nexit 1\n",
            started.display(),
            release.display()
        ),
    )
    .expect("write paused fsmonitor hook");
    let mut permissions = fs::metadata(&fsmonitor_hook)
        .expect("read fsmonitor hook permissions")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&fsmonitor_hook, permissions).expect("make fsmonitor hook executable");

    let mut validation = cli_command(&repo.root, None);
    validation
        .args(["changeset", "validate"])
        .arg(&changeset_id)
        .args(["--timeout-seconds", "20", "--", "sh", "-c"])
        .arg("git config core.fsmonitor \"$1\"; printf changed >> README.md")
        .arg("foremerge-final-snapshot")
        .arg(&fsmonitor_hook)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let validation = validation.spawn().expect("spawn paused validation");
    wait_for_sentinel(&started);

    let mut registration = cli_command(&repo.root, None);
    registration
        .args(["agent", "register", "--name", "snapshot-concurrent-agent"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut registration = registration.spawn().expect("spawn concurrent registration");
    // A genuine write-lock block never completes, so a generous bound still
    // catches the bug while surviving a loaded machine.
    let deadline = Instant::now() + wait_budget(Duration::from_secs(5));
    loop {
        if registration
            .try_wait()
            .expect("inspect concurrent registration")
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            fs::write(&release, b"release").expect("release final snapshot after failure");
            let _ = registration.kill();
            let _ = registration.wait();
            let _ = validation.wait_with_output();
            panic!(
                "concurrent mutation was blocked while the final Git snapshot was running.\n\
                 If this machine is busy, raise FOREMERGE_TEST_TIMEOUT_SCALE and retry \
                 before assuming a regression."
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let registration_output = registration
        .wait_with_output()
        .expect("collect concurrent registration");
    assert!(
        registration_output.status.success(),
        "concurrent registration failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&registration_output.stdout),
        String::from_utf8_lossy(&registration_output.stderr)
    );

    fs::write(&release, b"release").expect("release final snapshot");
    let validation_output = validation
        .wait_with_output()
        .expect("collect paused validation");
    assert!(!validation_output.status.success());
    assert_eq!(
        parse_cli_json(&validation_output)["error"]["code"],
        "STALE_CHANGESET"
    );
}

#[test]
fn doctor_outside_initialized_state_is_strictly_read_only() {
    let repo = create_repo();
    let runtime = repo.root.join(".git").join("foremerge");
    assert!(!runtime.exists());

    let doctor = cli_success(&repo.root, None, ["doctor"]);
    assert_eq!(doctor["data"]["database_ok"], false);
    assert_eq!(doctor["data"]["event_chain_ok"], Value::Null);
    assert_eq!(doctor["data"]["events_verified"], 0);
    assert_eq!(doctor["data"]["next_step"], "foremerge init");
    assert!(!runtime.exists(), "doctor must not create runtime state");
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
            "symbol:Ledger=extend".to_string(),
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
            "symbol:Ledger=extend".to_string(),
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

/// Registration stays insert-always, because two separate processes may share
/// a name and silently reusing a record could attach one to another's work.
/// What it must not do is hide the duplicate: the GPTree dogfood run produced
/// 11 agent records for 9 logical roles with nothing said about it.
#[test]
fn registering_a_second_agent_for_the_same_name_and_worktree_says_so() {
    let repo = create_repo();

    let first = cli_success(
        &repo.root,
        None,
        ["agent", "register", "--name", "twin", "--model", "e2e-test"],
    );
    assert!(
        first["data"]["warnings"].is_null(),
        "a first registration has nothing to warn about: {first}"
    );

    let second = cli_success(
        &repo.root,
        None,
        ["agent", "register", "--name", "twin", "--model", "e2e-test"],
    );

    // The agent fields stay at the top level of `data`, so the added warnings
    // field cannot break a caller that reads `data.id` today.
    let first_id = data_string(&first, "id").to_string();
    let second_id = data_string(&second, "id").to_string();
    assert_ne!(first_id, second_id, "each registration is its own record");

    let warnings = second["data"]["warnings"]
        .as_array()
        .unwrap_or_else(|| panic!("second registration should warn: {second}"));
    assert_eq!(warnings.len(), 1, "{second}");
    let warning = warnings[0].as_str().unwrap();
    assert!(
        warning.contains(&first_id),
        "the warning must name the agent it collides with, got: {warning}"
    );
    assert!(
        warning.contains("work adopt"),
        "the warning must point at the recovery path, got: {warning}"
    );
}

#[test]
fn a_different_name_in_the_same_worktree_is_not_a_duplicate() {
    let repo = create_repo();
    let first = cli_success(
        &repo.root,
        None,
        [
            "agent", "register", "--name", "alpha", "--model", "e2e-test",
        ],
    );
    let second = cli_success(
        &repo.root,
        None,
        ["agent", "register", "--name", "beta", "--model", "e2e-test"],
    );
    assert!(first["data"]["warnings"].is_null(), "{first}");
    assert!(
        second["data"]["warnings"].is_null(),
        "distinct names in one worktree are ordinary: {second}"
    );
}

#[test]
fn status_renders_one_screen_in_text_and_a_typed_json_envelope() {
    let repo = create_repo();
    let stripe_id = register_test_agent(&repo.root, "stripe-status-agent");
    let paypal_id = register_test_agent(&repo.root, "paypal-status-agent");
    let stripe_intent = publish_test_intent(
        &repo.root,
        &stripe_id,
        "status-replace",
        "Replace PaymentService with StripePaymentService",
        "symbol:PaymentService=replace",
    );
    let paypal_intent = publish_test_intent(
        &repo.root,
        &paypal_id,
        "status-extend",
        "Add PayPal support to PaymentService",
        "symbol:PaymentService=extend",
    );
    claim_test_scope(
        &repo.root,
        &stripe_id,
        &stripe_intent,
        "symbol:PaymentService",
    );
    start_test_work(&repo.root, &stripe_id, &stripe_intent);
    let changeset_id = publish_test_revision(
        &repo.root,
        &stripe_id,
        &stripe_intent,
        "Provider routing candidate",
    );

    let output = Command::new(foremerge_bin())
        .arg("--cwd")
        .arg(&repo.root)
        .arg("status")
        .output()
        .expect("run foremerge status");
    assert!(
        output.status.success(),
        "status failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("status text is UTF-8");
    assert!(
        !text.contains('\u{1b}'),
        "status text must not contain color escape codes:\n{text}"
    );
    for needle in [
        "Agents (2 active)".to_string(),
        "stripe-status-agent".to_string(),
        "paypal-status-agent".to_string(),
        "Intents (2)".to_string(),
        "INTENT (1)".to_string(),
        "PROVISIONAL (1)".to_string(),
        "Active claims (1)".to_string(),
        "symbol:PaymentService".to_string(),
        "Conflicts (1 open or coordinating)".to_string(),
        "destructive_vs_additive".to_string(),
        "HIGH".to_string(),
        format!("stripe-status-agent ({stripe_intent})"),
        format!("paypal-status-agent ({paypal_intent})"),
        format!("PROVISIONAL (1): {changeset_id}"),
    ] {
        assert!(
            text.contains(&needle),
            "status text is missing {needle:?}:\n{text}"
        );
    }

    let status = cli_success(&repo.root, None, ["status"]);
    let data = &status["data"];
    assert_eq!(data["agents"].as_array().expect("agents array").len(), 2);
    assert_eq!(data["agents"][0]["name"], "stripe-status-agent");
    assert_eq!(data["agents"][0]["model"], "e2e-test");
    let groups = data["intents"].as_array().expect("intent groups");
    assert!(
        groups.iter().any(|group| group["status"] == "INTENT"
            && group["count"] == 1
            && group["intents"][0]["id"] == paypal_intent),
        "missing the INTENT group: {data}"
    );
    assert!(
        groups.iter().any(|group| group["status"] == "PROVISIONAL"
            && group["intents"][0]["agent_name"] == "stripe-status-agent"),
        "missing the PROVISIONAL group: {data}"
    );
    assert_eq!(data["claims"].as_array().expect("claims array").len(), 1);
    assert_eq!(data["claims"][0]["agent_name"], "stripe-status-agent");
    assert_eq!(data["claims"][0]["scope"]["key"], "PaymentService");
    let conflict = &data["conflicts"][0];
    assert_eq!(conflict["kind"], "destructive_vs_additive");
    assert_eq!(conflict["severity"], "HIGH");
    assert_eq!(conflict["status"], "OPEN");
    let mut parties = vec![
        conflict["source_agent_name"]
            .as_str()
            .expect("source agent"),
        conflict["target_agent_name"]
            .as_str()
            .expect("target agent"),
    ];
    parties.sort_unstable();
    assert_eq!(parties, ["paypal-status-agent", "stripe-status-agent"]);
    assert_eq!(conflict["source_scopes"][0], "symbol:PaymentService");
    assert_eq!(conflict["target_scopes"][0], "symbol:PaymentService");
    assert_eq!(data["changesets"][0]["status"], "PROVISIONAL");
    assert_eq!(data["changesets"][0]["count"], 1);
    assert_eq!(data["changesets"][0]["ids"][0], changeset_id);
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

#[test]
fn setup_installs_native_claude_and_cursor_integrations_and_named_checks() {
    let repo = create_repo();
    let claude = cli_success(&repo.root, None, ["setup", "claude"]);
    assert_eq!(claude["data"]["clients"][0]["client"], "claude");
    assert_eq!(claude["data"]["clients"][0]["skill"]["status"], "written");
    let cursor = cli_success(&repo.root, None, ["setup", "cursor"]);
    assert_eq!(cursor["data"]["clients"][0]["client"], "cursor");

    let canonical_skill = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(".codex/skills/foremerge/SKILL.md"),
    )
    .expect("read canonical skill");
    for client in [".claude", ".cursor"] {
        let installed =
            fs::read_to_string(repo.root.join(client).join("skills/foremerge/SKILL.md")).unwrap();
        // Installed files carry a managed stamp over the canonical body, which
        // is what lets a later release upgrade its own unedited file in place.
        assert!(
            installed.starts_with(&canonical_skill),
            "{client}: installed skill must preserve the canonical instructions"
        );
        let stamp = installed[canonical_skill.len()..].trim();
        assert!(
            stamp.starts_with("<!-- foremerge:managed ") && stamp.ends_with("-->"),
            "{client}: installed skill must end with a managed stamp, got {stamp:?}"
        );
        assert!(
            stamp.contains("sha256="),
            "{client}: stamp must record the body digest, got {stamp:?}"
        );
    }
    for config in [
        repo.root.join(".mcp.json"),
        repo.root.join(".cursor/mcp.json"),
    ] {
        let value: Value = serde_json::from_slice(&fs::read(config).unwrap()).unwrap();
        assert_eq!(
            Path::new(
                value["mcpServers"]["foremerge"]["command"]
                    .as_str()
                    .unwrap()
            )
            .file_name(),
            Some(OsStr::new("foremerge"))
        );
        assert_eq!(
            value["mcpServers"]["foremerge"]["args"]
                .as_array()
                .unwrap()
                .last()
                .unwrap(),
            "mcp"
        );
    }

    let configured = cli_success(
        &repo.root,
        None,
        ["checks", "set", "test", "--", "git", "diff", "--check"],
    );
    assert_eq!(
        configured["data"]["registry"]["checks"]["test"]["command"],
        json!(["git", "diff", "--check"])
    );
    let listed = cli_success(&repo.root, None, ["checks", "list"]);
    assert_eq!(
        listed["data"]["registry"]["checks"]["test"]["timeout_seconds"],
        300
    );
    let doctor = cli_success(&repo.root, None, ["doctor", "--client", "claude"]);
    assert_eq!(doctor["data"]["clients"][0]["skill_current"], true);
    assert_eq!(doctor["data"]["clients"][0]["mcp_configured"], true);
    let removed = cli_success(&repo.root, None, ["checks", "remove", "test"]);
    assert_eq!(
        removed["data"]["registry"]["checks"],
        json!({}),
        "removal must return a registry without the removed check"
    );
    let listed_after_removal = cli_success(&repo.root, None, ["checks", "list"]);
    assert!(
        listed_after_removal["data"]["registry"]["checks"]
            .get("test")
            .is_none(),
        "removed check must be gone from a subsequent list"
    );
    let missing = cli_failure(&repo.root, None, ["checks", "remove", "test"]);
    assert_eq!(
        missing["error"]["code"], "NOT_FOUND",
        "removing an unknown check must fail distinctly: {missing}"
    );
}

async fn mcp_tool_call(service: &Foremerge, id: u64, name: &str, arguments: Value) -> Value {
    let response = mcp::handle_message(
        service,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }),
    )
    .await
    .expect("MCP tool response");
    assert_eq!(
        response["result"]["isError"], false,
        "MCP tool {name} failed: {response}"
    );
    response["result"]["structuredContent"].clone()
}

#[tokio::test]
async fn mcp_exposes_the_complete_work_lifecycle_with_named_verification() {
    let repo = create_repo();
    let database = database_from_doctor(&repo.root);
    let service = Foremerge::new(Store::open(database).unwrap());

    let agent = mcp_tool_call(
        &service,
        1,
        "register_agent",
        json!({ "name": "mcp-lifecycle", "model": "e2e", "worktree": repo.root }),
    )
    .await;
    let agent_id = agent["id"].as_str().unwrap();
    let published = mcp_tool_call(
        &service,
        2,
        "publish_intent",
        json!({
            "agent_id": agent_id,
            "task": "mcp-lifecycle",
            "summary": "Extend CheckoutRouter with a stable retry policy",
            "scopes": [{ "kind": "symbol", "key": "CheckoutRouter" , "operation": "extend" }],
            "metadata": { "source": "e2e" }
        }),
    )
    .await;
    let intent_id = published["intent"]["id"].as_str().unwrap();
    mcp_tool_call(
        &service,
        3,
        "check_conflicts",
        json!({ "intent_id": intent_id }),
    )
    .await;
    mcp_tool_call(
        &service,
        4,
        "claim_work",
        json!({
            "agent_id": agent_id,
            "intent_id": intent_id,
            "scopes": [{ "kind": "symbol", "key": "CheckoutRouter" , "operation": "extend" }]
        }),
    )
    .await;
    let started = mcp_tool_call(
        &service,
        5,
        "start_work",
        json!({ "agent_id": agent_id, "intent_id": intent_id }),
    )
    .await;
    assert_eq!(started["status"], "IN_PROGRESS");
    mcp_tool_call(&service, 6, "query_work", json!({ "agent_id": agent_id })).await;
    let changeset = mcp_tool_call(
        &service,
        7,
        "publish_changeset",
        json!({
            "agent_id": agent_id,
            "intent_id": intent_id,
            "summary": "Record the clean retry-policy candidate",
            "symbols": ["CheckoutRouter"],
            "provenance": { "source": "mcp-e2e" }
        }),
    )
    .await;
    let changeset_id = changeset["id"].as_str().unwrap();
    checks::set(
        &repo.root,
        "test",
        checks::NamedCheck {
            command: vec!["git".into(), "diff".into(), "--check".into()],
            timeout_seconds: 30,
        },
    )
    .unwrap();
    let validation = mcp_tool_call(
        &service,
        8,
        "run_verification",
        json!({ "changeset_id": changeset_id, "check": "test" }),
    )
    .await;
    assert_eq!(validation["passed"], true);
    let accepted = mcp_tool_call(
        &service,
        9,
        "accept_changeset",
        json!({ "changeset_id": changeset_id }),
    )
    .await;
    assert_eq!(accepted["status"], "ACCEPTED");
    fs::write(repo.root.join("landed.txt"), "integrated\n").unwrap();
    git(&repo.root, ["add", "landed.txt"]);
    git(
        &repo.root,
        ["commit", "--quiet", "-m", "integrate MCP candidate"],
    );
    let committed = mcp_tool_call(
        &service,
        10,
        "record_commit",
        json!({ "changeset_id": changeset_id, "git_ref": "HEAD" }),
    )
    .await;
    assert_eq!(committed["status"], "COMMITTED");

    let peer = mcp_tool_call(
        &service,
        11,
        "register_agent",
        json!({ "name": "mcp-peer", "model": "e2e", "worktree": repo.root }),
    )
    .await;
    let peer_id = peer["id"].as_str().unwrap();
    let replacement = mcp_tool_call(
        &service,
        12,
        "publish_intent",
        json!({
            "agent_id": agent_id,
            "task": "replace-payments",
            "summary": "Replace PaymentService with StripePaymentService",
            "scopes": [{ "kind": "symbol", "key": "PaymentService" , "operation": "replace" }]
        }),
    )
    .await;
    let replacement_id = replacement["intent"]["id"].as_str().unwrap();
    let extension = mcp_tool_call(
        &service,
        13,
        "publish_intent",
        json!({
            "agent_id": peer_id,
            "task": "extend-payments",
            "summary": "Add PayPal support to PaymentService",
            "scopes": [{ "kind": "symbol", "key": "PaymentService" , "operation": "extend" }]
        }),
    )
    .await;
    let extension_id = extension["intent"]["id"].as_str().unwrap();
    let conflict_id = extension["conflicts"][0]["id"].as_str().unwrap();
    mcp_tool_call(
        &service,
        14,
        "coordinate_with_agent",
        json!({
            "from_agent_id": peer_id,
            "to_agent_id": agent_id,
            "message": "Use a PaymentProvider boundary first",
            "conflict_id": conflict_id
        }),
    )
    .await;
    let resolved = mcp_tool_call(
        &service,
        15,
        "resolve_conflict",
        json!({
            "conflict_id": conflict_id,
            "agent_id": peer_id,
            "resolution": "Introduce PaymentProvider before provider-specific work",
            "rationale": "Both intents can depend on the stable abstraction"
        }),
    )
    .await;
    assert_eq!(resolved["status"], "RESOLVED");
    let rejected_discard = mcp::handle_message(
        &service,
        json!({
            "jsonrpc": "2.0",
            "id": 16,
            "method": "tools/call",
            "params": {
                "name": "discard_work",
                "arguments": {
                    "agent_id": agent_id,
                    "intent_id": replacement_id,
                    "reason": ""
                }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(rejected_discard["result"]["isError"], true);
    assert!(
        rejected_discard["result"]["structuredContent"]["error"]
            .as_str()
            .unwrap()
            .contains("non-empty --reason is now required")
    );
    let discarded = mcp_tool_call(
        &service,
        17,
        "discard_work",
        json!({
            "agent_id": agent_id,
            "intent_id": replacement_id,
            "reason": "Superseded by the provider abstraction plan"
        }),
    )
    .await;
    assert_eq!(discarded["status"], "DISCARDED");
    assert_ne!(replacement_id, extension_id);

    let agents = mcp_tool_call(&service, 18, "list_agents", json!({})).await;
    assert_eq!(agents.as_array().map(Vec::len), Some(2));
    let shown_intent =
        mcp_tool_call(&service, 19, "get_intent", json!({ "id": extension_id })).await;
    assert_eq!(shown_intent["intent"]["id"], extension_id);
    let shown_changeset =
        mcp_tool_call(&service, 20, "get_changeset", json!({ "id": changeset_id })).await;
    assert_eq!(shown_changeset["status"], "COMMITTED");
    let status = mcp_tool_call(&service, 21, "status", json!({})).await;
    assert_eq!(status["agents"].as_array().map(Vec::len), Some(2));
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
    assert_eq!(initial_health["data"]["status"], "alive");
    assert!(initial_health["data"].get("counts").is_none());
    let readiness: Value = client
        .get(format!("{first_url}/readyz"))
        .send()
        .await
        .expect("send readiness request")
        .json()
        .await
        .expect("decode readiness response");
    assert_eq!(readiness["data"]["status"], "ready");

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
    assert_eq!(restarted_health["data"]["status"], "alive");
    let agents: Value = client
        .get(format!("{second_url}/v1/agents"))
        .bearer_auth(token.trim())
        .send()
        .await
        .expect("list agents after restart")
        .error_for_status()
        .expect("agent list status")
        .json()
        .await
        .expect("agent list JSON");
    assert_eq!(agents["data"].as_array().map(Vec::len), Some(1));
    assert_eq!(agents["data"][0]["id"], agent_id);
    let audit: Value = client
        .get(format!("{second_url}/v1/audit/event-chain?page_size=1"))
        .bearer_auth(token.trim())
        .send()
        .await
        .expect("audit event chain after restart")
        .error_for_status()
        .expect("audit status")
        .json()
        .await
        .expect("audit JSON");
    assert_eq!(audit["data"]["valid"], true);

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_exposes_core_read_parity_with_cli_and_mcp() {
    let repo = create_repo();
    let database = database_from_doctor(&repo.root);
    let (agent_id, intent_id) =
        create_active_test_work(&repo.root, "http-read-agent", "http-read-parity");
    let changeset_id = publish_test_revision(
        &repo.root,
        &agent_id,
        &intent_id,
        "HTTP read parity candidate",
    );
    let token =
        fs::read_to_string(repo.root.join(".git/foremerge/token")).expect("read initialized token");
    let address = unused_loopback_address();
    let base_url = format!("http://{address}");
    let daemon = spawn_daemon(&repo.root, &database, address);
    let client = reqwest::Client::new();
    wait_for_health(&client, &base_url).await;

    let agents: Value = client
        .get(format!("{base_url}/v1/agents"))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(agents["data"][0]["id"], agent_id);
    let intent: Value = client
        .get(format!("{base_url}/v1/intents/{intent_id}"))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(intent["data"]["intent"]["id"], intent_id);
    let changeset: Value = client
        .get(format!("{base_url}/v1/changesets/{changeset_id}"))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changeset["data"]["id"], changeset_id);
    let status: Value = client
        .get(format!("{base_url}/v1/status"))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["data"]["agents"][0]["id"], agent_id);
    daemon.stop();
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
        "symbol:SharedLedger=extend",
    );
    let second_intent = publish_test_intent(
        &repo.root,
        &second_agent,
        "second-claim",
        "Instrument the shared ledger",
        "symbol:SharedLedger=extend",
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
            "component:DependencyConsumer=extend".to_string(),
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
        "component:RegisteredProjectWork=extend",
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
        "component:NoWorktreeProjectWork=extend",
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
            "component:AcceptedRefConsumer=extend".to_string(),
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
            "component:TamperedRefConsumer=extend".to_string(),
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
            "symbol:PaymentService=replace".to_string(),
            "--scope".to_string(),
            "contract:payments.provider=replace".to_string(),
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
            "symbol:PaymentService=extend".to_string(),
            "--scope".to_string(),
            "contract:payments.provider=extend".to_string(),
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

#[cfg(windows)]
#[test]
fn validation_timeout_kills_direct_child_on_windows() {
    let repo = create_repo();
    let (agent_id, intent_id) =
        create_active_test_work(&repo.root, "windows-timeout-agent", "windows-timeout");
    let changeset_id = publish_test_revision(
        &repo.root,
        &agent_id,
        &intent_id,
        "Validation with a long-lived Windows child",
    );
    let system_root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .expect("Windows system root");
    let child_name = "fm-timeout-child.exe";
    let child_path = repo.temp.path().join(child_name);
    fs::copy(
        PathBuf::from(system_root).join("System32").join("ping.exe"),
        &child_path,
    )
    .expect("copy a uniquely named long-lived Windows fixture");
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
            child_path.to_string_lossy().into_owned(),
            "-t".to_string(),
            "127.0.0.1".to_string(),
        ],
    );
    assert_eq!(validation["data"]["passed"], false);
    assert!(
        validation["data"]["stderr"]
            .as_str()
            .expect("validation stderr")
            .contains("timed out after 1 seconds")
    );
    let output = Command::new("tasklist")
        .args([
            "/FI",
            &format!("IMAGENAME eq {child_name}"),
            "/FO",
            "CSV",
            "/NH",
        ])
        .output()
        .expect("query Windows process list");
    assert!(output.status.success());
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(
        !listing.to_ascii_lowercase().contains(child_name),
        "validation child survived timeout: {listing}"
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

// ---------------------------------------------------------------------------
// Setup / client-integration coverage (stubbed `codex` CLI on PATH).
// ---------------------------------------------------------------------------

#[cfg(unix)]
struct CodexStub {
    bin_dir: PathBuf,
    log: PathBuf,
    state: PathBuf,
}

#[cfg(unix)]
impl CodexStub {
    fn create(dir: &Path) -> CodexStub {
        use std::os::unix::fs::PermissionsExt;
        let bin_dir = dir.join("codex-stub-bin");
        fs::create_dir_all(&bin_dir).expect("create stub bin directory");
        let log = dir.join("codex-stub.log");
        let state = dir.join("codex-stub-state.json");
        let script = r#"#!/bin/sh
printf '%s\n' "$*" >> "$CODEX_STUB_LOG"
case "$1" in
  --version)
    echo "codex-stub 0.0.0"
    exit 0
    ;;
  mcp)
    case "$2" in
      get)
        if [ -n "$CODEX_STUB_FAIL_MCP_READS" ]; then
          echo "codex-stub: cannot read the MCP registry" >&2
          exit 3
        fi
        if [ -f "$CODEX_STUB_STATE" ]; then
          cat "$CODEX_STUB_STATE"
          exit 0
        fi
        exit 1
        ;;
      list)
        if [ -n "$CODEX_STUB_FAIL_MCP_READS" ]; then
          echo "codex-stub: cannot read the MCP registry" >&2
          exit 3
        fi
        if [ -f "$CODEX_STUB_STATE" ]; then
          echo "foremerge"
        fi
        exit 0
        ;;
      add)
        if [ -n "$CODEX_STUB_FAIL_MCP_ADD" ]; then
          echo "codex-stub: add rejected" >&2
          exit 3
        fi
        shift 3
        if [ "$1" = "--" ]; then shift; fi
        exe="$1"
        if [ "$2" = "--cwd" ]; then
          printf '{"command":"%s","args":["--cwd","%s","mcp"]}\n' "$exe" "$3" > "$CODEX_STUB_STATE"
        else
          printf '{"command":"%s","args":["mcp"]}\n' "$exe" > "$CODEX_STUB_STATE"
        fi
        exit 0
        ;;
      remove)
        rm -f "$CODEX_STUB_STATE"
        exit 0
        ;;
    esac
    exit 1
    ;;
esac
exit 1
"#;
        let script_path = bin_dir.join("codex");
        fs::write(&script_path, script).expect("write codex stub");
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .expect("mark codex stub executable");
        CodexStub {
            bin_dir,
            log,
            state,
        }
    }

    fn command<I, S>(&self, cwd: &Path, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(foremerge_bin());
        command.arg("--json").arg("--cwd").arg(cwd).args(args);
        let mut paths = vec![self.bin_dir.clone()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command.env("PATH", std::env::join_paths(paths).expect("join PATH"));
        command.env("CODEX_STUB_LOG", &self.log);
        command.env("CODEX_STUB_STATE", &self.state);
        command
    }

    fn run_success<I, S>(&self, cwd: &Path, args: I) -> Value
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.command(cwd, args).output().expect("run foremerge");
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

    fn run_failure<I, S>(&self, cwd: &Path, args: I) -> Value
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.command(cwd, args).output().expect("run foremerge");
        assert!(
            !output.status.success(),
            "Foremerge command unexpectedly succeeded: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let value = parse_cli_json(&output);
        assert_eq!(value["ok"], false, "unexpected error envelope: {value}");
        value
    }

    fn log_contents(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[cfg(unix)]
#[test]
fn setup_codex_registers_the_global_mcp_entry_and_is_idempotent() {
    let repo = create_repo();
    let stub = CodexStub::create(repo.temp.path());
    let setup = stub.run_success(&repo.root, ["setup", "codex"]);
    let client = &setup["data"]["clients"][0];
    assert_eq!(client["client"], "codex");
    assert_eq!(client["skill"]["status"], "written");
    assert_eq!(client["mcp_configured"], true);
    assert_eq!(
        client["mcp"]["path"],
        "Codex user-level configuration (codex mcp)"
    );
    assert_eq!(client["mcp"]["status"], "written");
    assert_eq!(client["error"], Value::Null);
    assert_eq!(client["warning"], Value::Null);

    let log = stub.log_contents();
    let add_line = log
        .lines()
        .find(|line| line.starts_with("mcp add foremerge"))
        .expect("codex mcp add was invoked");
    // The registration carries no --cwd: Codex spawns the server in the
    // directory it was launched from, so one entry serves every repository.
    assert!(
        add_line.ends_with(" mcp") && !add_line.contains("--cwd"),
        "the Codex registration must be portable, got: {add_line}"
    );

    let again = stub.run_success(&repo.root, ["setup", "codex"]);
    assert_eq!(again["data"]["clients"][0]["mcp"]["status"], "unchanged");
    assert_eq!(again["data"]["clients"][0]["mcp_configured"], true);
    let adds = stub
        .log_contents()
        .lines()
        .filter(|line| line.starts_with("mcp add"))
        .count();
    assert_eq!(adds, 1, "second setup must not re-register the entry");

    let doctor = stub.run_success(&repo.root, ["doctor", "--client", "codex"]);
    assert_eq!(doctor["data"]["clients"][0]["mcp_configured"], true);
}

#[test]
fn mcp_outside_a_repository_fails_instead_of_creating_a_stray_store() {
    // The portable registration resolves its repository from the directory the
    // client spawns the server in. Outside a repository there is no answer, and
    // silently coordinating against a store created beside the spawn directory
    // would be worse than refusing.
    let temp = tempfile::tempdir().expect("temp dir");
    let outside = temp.path().canonicalize().expect("canonicalize temp dir");
    let output = Command::new(foremerge_bin())
        .arg("--json")
        .arg("--cwd")
        .arg(&outside)
        .arg("mcp")
        .output()
        .expect("run foremerge mcp");
    assert!(
        !output.status.success(),
        "mcp must refuse outside a repository"
    );
    // The diagnosis goes to stderr: anything written to stdout would corrupt
    // the JSON-RPC stream the client is reading.
    assert!(
        output.stdout.is_empty(),
        "mcp must not write to stdout before the stream is usable: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(
        message.contains("INVALID_INPUT"),
        "the refusal must carry a typed code: {message}"
    );
    assert!(
        message.contains(&outside.display().to_string()),
        "the error must name the directory it was spawned in: {message}"
    );
    assert!(
        !outside.join(".foremerge").exists(),
        "refusing must not leave a stray store behind"
    );
}

#[cfg(unix)]
#[test]
fn setup_codex_in_a_second_repository_is_a_no_op() {
    let repo_a = create_repo();
    let repo_b = create_repo();
    let stub = CodexStub::create(repo_a.temp.path());
    stub.run_success(&repo_a.root, ["setup", "codex"]);

    // The registration carries no repository, so it already serves repo B.
    let second = stub.run_success(&repo_b.root, ["setup", "codex"]);
    let client = &second["data"]["clients"][0];
    assert_eq!(client["mcp"]["status"], "unchanged");
    assert_eq!(client["mcp_configured"], true);
    assert_eq!(client["error"], Value::Null);
    assert_eq!(client["warning"], Value::Null);

    let adds = stub
        .log_contents()
        .lines()
        .filter(|line| line.starts_with("mcp add"))
        .count();
    assert_eq!(
        adds, 1,
        "a second repository must not re-register the entry"
    );

    // Both repositories report the same registration as ready.
    for root in [&repo_a.root, &repo_b.root] {
        let doctor = stub.run_success(root, ["doctor", "--client", "codex"]);
        assert_eq!(doctor["data"]["clients"][0]["mcp_configured"], true);
    }
}

#[cfg(unix)]
#[test]
fn setup_codex_upgrades_a_pinned_entry_but_refuses_a_foreign_one() {
    let repo = create_repo();
    let stub = CodexStub::create(repo.temp.path());
    let canonical = repo.root.canonicalize().expect("canonicalize repo");

    // Foremerge's own earlier form, pinned to one repository, upgrades without
    // --force: replacing it destroys nothing an operator authored.
    let exe = foremerge_bin();
    fs::write(
        &stub.state,
        format!(
            "{{\"command\":\"{}\",\"args\":[\"--cwd\",\"{}\",\"mcp\"]}}\n",
            exe.display(),
            canonical.display()
        ),
    )
    .expect("seed pinned registration");
    let upgraded = stub.run_success(&repo.root, ["setup", "codex"]);
    let client = &upgraded["data"]["clients"][0];
    assert_eq!(client["mcp"]["status"], "written");
    let warning = client["warning"].as_str().expect("upgrade warning");
    assert!(
        warning.contains(&canonical.display().to_string()) && warning.contains("portable"),
        "the warning must say the pinned registration became portable: {warning}"
    );

    // Someone else's `foremerge` entry is not Foremerge's to replace.
    fs::write(
        &stub.state,
        "{\"command\":\"/usr/bin/env\",\"args\":[\"mcp\"]}\n",
    )
    .expect("seed foreign registration");
    let refusal = stub.run_failure(&repo.root, ["setup", "codex"]);
    assert_eq!(refusal["error"]["code"], "ALREADY_EXISTS");
    let message = refusal["error"]["message"].as_str().expect("error message");
    assert!(
        message.contains("/usr/bin/env"),
        "the refusal must name the entry it will not replace: {message}"
    );

    let forced = stub.run_success(&repo.root, ["setup", "codex", "--force"]);
    assert_eq!(forced["data"]["clients"][0]["mcp"]["status"], "written");
    let log = stub.log_contents();
    assert!(
        log.lines().any(|line| line == "mcp remove foremerge"),
        "forcing over a foreign entry must remove it first: {log}"
    );
}

#[cfg(unix)]
#[test]
fn setup_codex_treats_a_plain_text_entry_as_unverifiable() {
    let repo = create_repo();
    let stub = CodexStub::create(repo.temp.path());
    let canonical_root = repo.root.canonicalize().expect("canonicalize repo root");
    // An older codex renders `mcp get` as plain text; the token fallback
    // recovers only the --cwd target, so the entry's command is unverifiable.
    // An unverifiable command must not be blessed as current: setup refuses
    // without --force, and --force repoints it to the absolute installed path.
    fs::write(
        &stub.state,
        format!(
            "foremerge enabled command foremerge args --cwd {} mcp\n",
            canonical_root.display()
        ),
    )
    .expect("write plain-text stub state");

    let refusal = stub.run_failure(&repo.root, ["setup", "codex"]);
    assert_eq!(refusal["error"]["code"], "ALREADY_EXISTS", "{refusal}");
    let log = stub.log_contents();
    assert!(
        !log.contains("mcp add") && !log.contains("mcp remove"),
        "an unverifiable entry must not be touched without --force: {log}"
    );

    let forced = stub.run_success(&repo.root, ["setup", "codex", "--force"]);
    let client = &forced["data"]["clients"][0];
    assert_eq!(client["mcp"]["status"], "written", "{client}");
    assert_eq!(client["mcp_configured"], true, "{client}");
    let log = stub.log_contents();
    assert!(
        log.contains("mcp remove") && log.contains("mcp add"),
        "--force must repoint the unverifiable entry: {log}"
    );
}

#[cfg(unix)]
#[test]
fn setup_codex_aborts_when_the_probe_fails_instead_of_silently_adding() {
    let repo = create_repo();
    let stub = CodexStub::create(repo.temp.path());
    // `codex mcp get` and `codex mcp list` both failing means the CLI state
    // is unreadable; setup must abort rather than treat that as "no entry"
    // and add over a registration it could not read.
    let output = {
        let mut command = stub.command(&repo.root, ["setup", "codex"]);
        command.env("CODEX_STUB_FAIL_MCP_READS", "1");
        command.output().expect("run foremerge")
    };
    assert!(
        !output.status.success(),
        "setup must fail when the codex probe fails: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value = parse_cli_json(&output);
    assert_eq!(value["ok"], false, "{value}");
    assert_eq!(value["error"]["code"], "CHECK_FAILED", "{value}");
    let message = value["error"]["message"].as_str().expect("error message");
    assert!(
        message.contains("codex CLI"),
        "the error must tell the user to check the codex CLI: {message}"
    );
    let log = stub.log_contents();
    assert!(
        !log.contains("mcp add"),
        "a failed probe must never lead to an add: {log}"
    );
}

#[cfg(unix)]
#[test]
fn setup_codex_discloses_a_removed_registration_when_the_replacement_add_fails() {
    let repo = create_repo();
    let stub = CodexStub::create(repo.temp.path());
    let canonical = repo.root.canonicalize().expect("canonicalize repo");

    // A pinned registration is replaced by the portable one. The Codex CLI has
    // no atomic replace, so if the add fails the previous entry is already gone
    // and the error must say so.
    fs::write(
        &stub.state,
        format!(
            "{{\"command\":\"{}\",\"args\":[\"--cwd\",\"{}\",\"mcp\"]}}\n",
            foremerge_bin().display(),
            canonical.display()
        ),
    )
    .expect("seed pinned registration");

    let output = {
        let mut command = stub.command(&repo.root, ["setup", "codex"]);
        command.env("CODEX_STUB_FAIL_MCP_ADD", "1");
        command.output().expect("run foremerge")
    };
    assert!(
        !output.status.success(),
        "setup must fail when the add fails: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value = parse_cli_json(&output);
    assert_eq!(value["error"]["code"], "CHECK_FAILED", "{value}");
    let message = value["error"]["message"].as_str().expect("error message");
    assert!(
        message.contains("was already removed"),
        "the error must disclose the destroyed registration: {message}"
    );
    assert!(
        message.contains(&canonical.display().to_string()),
        "the error must name the removed registration's repository: {message}"
    );
    assert!(
        message.contains("restore it with"),
        "the error must explain how to restore the previous entry: {message}"
    );
}

#[test]
fn setup_claude_refuses_a_bare_command_entry_it_cannot_verify() {
    let repo = create_repo();
    let canonical_root = repo.root.canonicalize().expect("canonicalize repo root");
    // A bare command resolves in the MCP client's own PATH, not this
    // process's, so it can never be blessed as current even when a binary of
    // that name is findable here.
    fs::write(
        repo.root.join(".mcp.json"),
        serde_json::to_vec_pretty(&json!({
            "mcpServers": {
                "foremerge": {
                    "command": "foremerge",
                    "args": ["--cwd", canonical_root.to_string_lossy(), "mcp"]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let refusal = cli_failure(&repo.root, None, ["setup", "claude"]);
    assert_eq!(refusal["error"]["code"], "ALREADY_EXISTS", "{refusal}");
    assert!(
        refusal["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("--force")),
        "the refusal must suggest --force normalization: {refusal}"
    );

    let forced = cli_success(&repo.root, None, ["setup", "claude", "--force"]);
    assert_eq!(forced["data"]["clients"][0]["mcp"]["status"], "written");
    let value: Value =
        serde_json::from_slice(&fs::read(repo.root.join(".mcp.json")).unwrap()).unwrap();
    let command = value["mcpServers"]["foremerge"]["command"]
        .as_str()
        .expect("rewritten command");
    assert!(
        Path::new(command).is_absolute(),
        "--force must normalize to the absolute installed path: {command}"
    );
}

#[cfg(unix)]
#[test]
fn setup_all_attempts_every_client_and_reports_the_failing_one() {
    let repo = create_repo();
    let stub = CodexStub::create(repo.temp.path());
    fs::write(
        repo.root.join(".mcp.json"),
        serde_json::to_vec_pretty(&json!({
            "mcpServers": {
                "foremerge": { "command": "some-other-tool", "args": ["serve"] }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let value = stub.run_failure(&repo.root, ["setup", "all"]);
    assert_eq!(value["error"]["code"], "ALREADY_EXISTS");
    let clients = value["data"]["clients"].as_array().expect("clients array");
    assert_eq!(clients.len(), 3, "every requested client must be reported");
    let by_name = |name: &str| {
        clients
            .iter()
            .find(|client| client["client"] == name)
            .unwrap_or_else(|| panic!("missing report for {name}"))
    };
    let claude = by_name("claude");
    assert!(
        claude["error"]
            .as_str()
            .is_some_and(|error| error.starts_with("ALREADY_EXISTS")),
        "claude must report its merge refusal: {claude}"
    );
    assert_eq!(claude["skill"]["status"], "written");
    let codex = by_name("codex");
    assert_eq!(codex["error"], Value::Null);
    assert_eq!(codex["mcp_configured"], true);
    let cursor = by_name("cursor");
    assert_eq!(cursor["error"], Value::Null);
    assert_eq!(cursor["mcp"]["status"], "written");
    assert!(
        repo.root.join(".cursor/mcp.json").is_file(),
        "cursor must still be configured after claude failed"
    );
}

#[cfg(unix)]
#[test]
fn setup_skip_mcp_installs_skills_without_touching_mcp_configuration() {
    let repo = create_repo();
    let stub = CodexStub::create(repo.temp.path());
    let setup = stub.run_success(&repo.root, ["setup", "all", "--skip-mcp"]);
    let clients = setup["data"]["clients"].as_array().expect("clients array");
    assert_eq!(clients.len(), 3);
    for client in clients {
        assert_eq!(client["skill"]["status"], "written", "{client}");
        assert_eq!(client["mcp"], Value::Null, "{client}");
        assert_eq!(client["mcp_configured"], false, "{client}");
        assert!(client["next_step"].as_str().is_some(), "{client}");
        assert_eq!(client["error"], Value::Null, "{client}");
    }
    for skill_dir in [".codex", ".claude", ".cursor"] {
        assert!(
            repo.root
                .join(skill_dir)
                .join("skills/foremerge/SKILL.md")
                .is_file()
        );
    }
    assert!(!repo.root.join(".mcp.json").exists());
    assert!(!repo.root.join(".cursor/mcp.json").exists());
    let log = stub.log_contents();
    assert!(
        !log.contains("mcp add") && !log.contains("mcp remove"),
        "--skip-mcp must not mutate Codex configuration: {log}"
    );
}

#[test]
fn setup_claude_merge_preserves_unrelated_entries_and_key_order() {
    let repo = create_repo();
    fs::write(
        repo.root.join(".mcp.json"),
        concat!(
            "{\n",
            "  \"zeta\": {\"first\": true},\n",
            "  \"mcpServers\": {\n",
            "    \"other\": {\"command\": \"other-server\", \"args\": [\"--flag\"]}\n",
            "  },\n",
            "  \"alpha\": 1\n",
            "}\n"
        ),
    )
    .unwrap();
    cli_success(&repo.root, None, ["setup", "claude"]);
    let raw = fs::read_to_string(repo.root.join(".mcp.json")).unwrap();
    let value: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["mcpServers"]["other"]["command"], "other-server");
    assert_eq!(value["mcpServers"]["other"]["args"], json!(["--flag"]));
    assert!(value["mcpServers"]["foremerge"].is_object());
    assert_eq!(value["alpha"], 1);
    assert_eq!(value["zeta"]["first"], true);
    let zeta = raw.find("\"zeta\"").expect("zeta key");
    let servers = raw.find("\"mcpServers\"").expect("mcpServers key");
    let alpha = raw.find("\"alpha\"").expect("alpha key");
    assert!(
        zeta < servers && servers < alpha,
        "top-level key order was not preserved:\n{raw}"
    );
    assert!(
        raw.contains("\n  \"mcpServers\""),
        "output must stay 2-space pretty-printed:\n{raw}"
    );
}

#[test]
fn setup_claude_force_repairs_a_stale_entry_pointing_at_another_repository() {
    let repo = create_repo();
    let exe = foremerge_bin();
    fs::write(
        repo.root.join(".mcp.json"),
        serde_json::to_vec_pretty(&json!({
            "mcpServers": {
                "foremerge": {
                    "command": exe.to_string_lossy(),
                    "args": ["--cwd", "/somewhere/that/moved", "mcp"]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let refusal = cli_failure(&repo.root, None, ["setup", "claude"]);
    assert_eq!(
        refusal["error"]["code"], "ALREADY_EXISTS",
        "a stale --cwd must not be reported as unchanged: {refusal}"
    );
    assert!(
        refusal["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("/somewhere/that/moved")),
        "refusal must name the differing entry: {refusal}"
    );

    let forced = cli_success(&repo.root, None, ["setup", "claude", "--force"]);
    let client = &forced["data"]["clients"][0];
    assert_eq!(client["mcp"]["status"], "written");
    let value: Value =
        serde_json::from_slice(&fs::read(repo.root.join(".mcp.json")).unwrap()).unwrap();
    let canonical_root = repo.root.canonicalize().unwrap();
    assert_eq!(
        value["mcpServers"]["foremerge"]["args"][1],
        canonical_root.to_string_lossy().as_ref(),
        "--force must repoint the entry at this repository"
    );
}

#[cfg(unix)]
#[test]
fn setup_refuses_a_dangling_symlinked_mcp_config() {
    let repo = create_repo();
    std::os::unix::fs::symlink(
        repo.root.join("missing-target.json"),
        repo.root.join(".mcp.json"),
    )
    .unwrap();
    let refusal = cli_failure(&repo.root, None, ["setup", "claude"]);
    assert_eq!(refusal["error"]["code"], "INVALID_INPUT");
    assert!(
        refusal["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("regular file")),
        "{refusal}"
    );
    let metadata = fs::symlink_metadata(repo.root.join(".mcp.json")).unwrap();
    assert!(
        metadata.file_type().is_symlink(),
        "the user's symlink must be left in place"
    );
    let forced = cli_failure(&repo.root, None, ["setup", "claude", "--force"]);
    assert_eq!(
        forced["error"]["code"], "INVALID_INPUT",
        "--force must not bypass the symlink refusal: {forced}"
    );
}

#[test]
fn named_check_registry_resolves_from_the_repository_not_the_mcp_process_cwd() {
    let repo = create_repo();
    let database = database_from_doctor(&repo.root);
    let (agent_id, intent_id) = create_active_test_work(&repo.root, "registry-agent", "verify");
    let changeset_id = publish_test_revision(&repo.root, &agent_id, &intent_id, "Clean candidate");
    cli_success(
        &repo.root,
        None,
        [
            "checks",
            "set",
            "repo-check",
            "--",
            "git",
            "diff",
            "--check",
        ],
    );

    // A decoy registry in the MCP server's spawn directory must never become a
    // trusted command source: neither for names the repository configured nor
    // for names only the decoy defines.
    let unrelated = tempfile::tempdir().expect("create unrelated spawn directory");
    fs::create_dir_all(unrelated.path().join(".foremerge")).unwrap();
    fs::write(
        unrelated.path().join(".foremerge/checks.json"),
        br#"{"version":1,"checks":{"repo-check":{"command":["false"],"timeout_seconds":10},"decoy-only":{"command":["false"],"timeout_seconds":10}}}"#,
    )
    .unwrap();

    let mut child = Command::new(foremerge_bin())
        .current_dir(unrelated.path())
        .arg("--database")
        .arg(&database)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn MCP server outside the repository");
    let mut stdin = child.stdin.take().expect("MCP stdin");
    let requests = [
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": { "name": "foremerge-e2e", "version": "1" }
            }
        }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "run_verification",
                "arguments": { "changeset_id": changeset_id, "check": "repo-check" }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "run_verification",
                "arguments": { "changeset_id": changeset_id, "check": "decoy-only" }
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
    let responses = String::from_utf8(output.stdout)
        .expect("MCP output is UTF-8")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("every MCP stdout line is JSON"))
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(
        responses[1]["result"]["isError"], false,
        "repository-configured check must run from an unrelated spawn cwd: {}",
        responses[1]
    );
    assert_eq!(responses[1]["result"]["structuredContent"]["passed"], true);
    assert_eq!(responses[2]["id"], 3);
    assert_eq!(
        responses[2]["result"]["isError"], true,
        "a check defined only by the spawn directory's decoy registry must stay unknown: {}",
        responses[2]
    );
    assert!(
        responses[2]["result"]["structuredContent"]["error"]
            .as_str()
            .is_some_and(|message| message.starts_with("NOT_FOUND")
                && message.contains("not configured")
                && !message.contains("checks set")),
        "{}",
        responses[2]
    );
}

#[test]
fn checks_commands_refuse_outside_a_git_repository() {
    let temp = tempfile::tempdir().expect("create non-git directory");
    fs::create_dir_all(temp.path().join(".foremerge")).unwrap();
    fs::write(
        temp.path().join(".foremerge/checks.json"),
        br#"{"version":1,"checks":{"build":{"command":["false"],"timeout_seconds":10}}}"#,
    )
    .unwrap();
    for args in [
        vec!["checks", "list"],
        vec!["checks", "set", "build", "--", "true"],
        vec!["checks", "remove", "build"],
    ] {
        let refusal = cli_failure(temp.path(), None, args);
        assert_eq!(refusal["error"]["code"], "INVALID_INPUT", "{refusal}");
        assert!(
            refusal["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("repository-scoped")),
            "{refusal}"
        );
    }
    assert!(
        !temp.path().join(".foremerge/state.sqlite3").exists(),
        "refused checks commands must not create a fallback store"
    );
}

#[tokio::test]
async fn mcp_accept_changeset_rejects_acceptance_overrides() {
    let service = Foremerge::new(Store::in_memory().unwrap());
    for arguments in [
        json!({ "changeset_id": "chg_x", "allow_high_conflicts": true, "override_reason": "self-service" }),
        json!({ "changeset_id": "chg_x", "allow_high_conflicts": true }),
        json!({ "changeset_id": "chg_x", "override_reason": "self-service" }),
        // Refused by name rather than dropped by serde: an unknown field would
        // be ignored and the acceptance would proceed, which is the opposite
        // of what the tool description and the docs promise.
        json!({ "changeset_id": "chg_x", "allow_unverified": true }),
        json!({ "changeset_id": "chg_x", "allow_unverified": true, "override_reason": "self-service" }),
    ] {
        let response = mcp::handle_message(
            &service,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "accept_changeset", "arguments": arguments }
            }),
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert!(
            response["result"]["structuredContent"]["error"]
                .as_str()
                .is_some_and(|message| message.starts_with("FORBIDDEN")
                    && message.contains("not accepted over MCP")),
            "{response}"
        );
    }
    // Without override arguments the request reaches the ordinary gates.
    let plain = mcp::handle_message(
        &service,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "accept_changeset", "arguments": { "changeset_id": "chg_x" } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(plain["result"]["isError"], true);
    assert!(
        plain["result"]["structuredContent"]["error"]
            .as_str()
            .is_some_and(|message| message.starts_with("NOT_FOUND")),
        "{plain}"
    );
}

#[tokio::test]
async fn mcp_resolve_conflict_is_limited_to_conflict_parties() {
    let service = Foremerge::new(Store::in_memory().unwrap());
    let replacer = mcp_tool_call(
        &service,
        1,
        "register_agent",
        json!({ "name": "party-replacer", "model": "e2e" }),
    )
    .await;
    let replacer_id = replacer["id"].as_str().unwrap();
    let extender = mcp_tool_call(
        &service,
        2,
        "register_agent",
        json!({ "name": "party-extender", "model": "e2e" }),
    )
    .await;
    let extender_id = extender["id"].as_str().unwrap();
    let outsider = mcp_tool_call(
        &service,
        3,
        "register_agent",
        json!({ "name": "outsider", "model": "e2e" }),
    )
    .await;
    let outsider_id = outsider["id"].as_str().unwrap();
    mcp_tool_call(
        &service,
        4,
        "publish_intent",
        json!({
            "agent_id": replacer_id,
            "task": "replace-payments",
            "summary": "Replace PaymentService with StripePaymentService",
            "scopes": [{ "kind": "symbol", "key": "PaymentService" , "operation": "replace" }]
        }),
    )
    .await;
    let extension = mcp_tool_call(
        &service,
        5,
        "publish_intent",
        json!({
            "agent_id": extender_id,
            "task": "extend-payments",
            "summary": "Add PayPal support to PaymentService",
            "scopes": [{ "kind": "symbol", "key": "PaymentService" , "operation": "extend" }]
        }),
    )
    .await;
    let conflict_id = extension["conflicts"][0]["id"].as_str().unwrap();

    let rejected = mcp::handle_message(
        &service,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "resolve_conflict",
                "arguments": {
                    "conflict_id": conflict_id,
                    "agent_id": outsider_id,
                    "resolution": "Outsider closes the gate",
                    "rationale": "Trying to clear another pair's blocker"
                }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(rejected["result"]["isError"], true, "{rejected}");
    assert!(
        rejected["result"]["structuredContent"]["error"]
            .as_str()
            .is_some_and(|message| message.starts_with("FORBIDDEN") && message.contains("party")),
        "{rejected}"
    );

    let resolved = mcp_tool_call(
        &service,
        7,
        "resolve_conflict",
        json!({
            "conflict_id": conflict_id,
            "agent_id": extender_id,
            "resolution": "Introduce PaymentProvider before provider-specific work",
            "rationale": "Agreed in coordination message with party-replacer"
        }),
    )
    .await;
    assert_eq!(resolved["status"], "RESOLVED");
    let resolution_event = service
        .events(0, 500)
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == "conflict.resolved")
        .expect("resolution event recorded");
    assert_eq!(
        resolution_event.agent_id.as_deref(),
        Some(extender_id),
        "the recorded resolution must carry the resolver's agent id"
    );
}

#[test]
fn doctor_gates_readiness_on_unconfigured_clients_and_keeps_a_typed_envelope() {
    let repo = create_repo();
    let plain = cli_success(&repo.root, None, ["doctor"]);
    assert!(
        plain["data"].get("clients").is_none(),
        "doctor without --client must omit the clients field: {plain}"
    );
    let gated = cli_success(&repo.root, None, ["doctor", "--client", "claude"]);
    assert_eq!(
        gated["data"]["ready"], false,
        "an unconfigured client must gate overall readiness: {gated}"
    );
    assert_eq!(gated["data"]["clients"][0]["client"], "claude");
    assert_eq!(gated["data"]["clients"][0]["skill_installed"], false);
    assert_eq!(gated["data"]["clients"][0]["skill_current"], false);
    assert_eq!(gated["data"]["clients"][0]["ready"], false);
    assert_eq!(
        gated["data"]["next_step"].as_str(),
        Some("foremerge init"),
        "store initialization must precede client remediation: {gated}"
    );
}

const EMPTY_STRING_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[test]
fn changeset_provenance_derives_first_parent_base_and_a_real_diff_hash() {
    let repo = create_repo();
    let initial_commit = git(&repo.root, ["rev-parse", "HEAD"]);
    let (agent_id, intent_id) =
        create_active_test_work(&repo.root, "provenance-agent", "provenance-diff");

    fs::write(repo.root.join("src.txt"), "first change\n").expect("write candidate change");
    git(&repo.root, ["add", "src.txt"]);
    git(&repo.root, ["commit", "--quiet", "-m", "candidate change"]);
    let candidate = git(&repo.root, ["rev-parse", "HEAD"]);

    let published = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_id.clone(),
            "--intent".to_string(),
            intent_id.clone(),
            "--summary".to_string(),
            "Candidate with real committed changes".to_string(),
            "--git-ref".to_string(),
            "HEAD".to_string(),
        ],
    );
    assert_eq!(
        published["data"]["base_ref"].as_str(),
        Some(initial_commit.as_str()),
        "the default base must be the candidate's first parent: {published}"
    );
    let git_provenance = &published["data"]["provenance"]["git"];
    assert_eq!(
        git_provenance["candidate"].as_str(),
        Some(candidate.as_str())
    );
    assert_eq!(
        git_provenance["base_ref"].as_str(),
        Some(initial_commit.as_str())
    );
    assert_eq!(git_provenance["base_resolution"], "first_parent");
    let diff_hash = git_provenance["diff_hash"].as_str().expect("diff hash");
    assert_ne!(
        diff_hash, EMPTY_STRING_SHA256,
        "a non-merge commit with changes must never record an empty-diff hash"
    );

    fs::write(repo.root.join("src.txt"), "second change\n").expect("write second change");
    git(&repo.root, ["add", "src.txt"]);
    git(
        &repo.root,
        ["commit", "--quiet", "-m", "second candidate change"],
    );
    let revised = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_id.clone(),
            "--intent".to_string(),
            intent_id.clone(),
            "--summary".to_string(),
            "Revision with an explicit caller-supplied base".to_string(),
            "--base-ref".to_string(),
            initial_commit.clone(),
        ],
    );
    assert_eq!(
        revised["data"]["base_ref"].as_str(),
        Some(initial_commit.as_str()),
        "an explicit --base-ref must be respected: {revised}"
    );
    assert_eq!(
        revised["data"]["provenance"]["git"]["base_resolution"],
        "caller_supplied"
    );
    assert_ne!(
        revised["data"]["provenance"]["git"]["diff_hash"].as_str(),
        Some(EMPTY_STRING_SHA256)
    );

    let vacuous = cli_failure(
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
            "Self-referential base must be rejected".to_string(),
            "--base-ref".to_string(),
            "HEAD".to_string(),
        ],
    );
    assert_eq!(vacuous["error"]["code"], "INVALID_INPUT");
    assert!(
        vacuous["error"]["message"]
            .as_str()
            .unwrap()
            .contains("candidate commit itself"),
        "unexpected vacuous-base error: {vacuous}"
    );
}

#[test]
fn changeset_provenance_records_shallow_boundary_not_root_commit() {
    let upstream = create_repo();
    fs::write(upstream.root.join("second.txt"), "two\n").expect("write second file");
    git(&upstream.root, ["add", "second.txt"]);
    git(&upstream.root, ["commit", "--quiet", "-m", "second"]);

    // A depth-1 clone reports its boundary commit without parents even
    // though the real history has one; provenance must say so instead of
    // misrecording a root commit.
    let clone_parent = tempfile::tempdir().expect("create clone directory");
    let clone_root = clone_parent.path().join("shallow");
    let upstream_url = format!(
        "file://{}",
        upstream
            .root
            .canonicalize()
            .expect("canonicalize upstream")
            .display()
    );
    git(
        clone_parent.path(),
        [
            "clone",
            "--quiet",
            "--depth",
            "1",
            upstream_url.as_str(),
            clone_root.to_str().expect("UTF-8 clone path"),
        ],
    );
    git(&clone_root, ["config", "user.name", "Foremerge Test"]);
    git(
        &clone_root,
        ["config", "user.email", "foremerge-test@example.invalid"],
    );

    let (agent_id, intent_id) =
        create_active_test_work(&clone_root, "shallow-agent", "shallow-task");
    let published = cli_success(
        &clone_root,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            agent_id,
            "--intent".to_string(),
            intent_id,
            "--summary".to_string(),
            "Publish a candidate at the shallow boundary".to_string(),
            "--git-ref".to_string(),
            "HEAD".to_string(),
        ],
    );
    let git_provenance = &published["data"]["provenance"]["git"];
    assert_eq!(
        git_provenance["base_resolution"], "shallow_boundary",
        "a shallow boundary commit must not be recorded as a root commit: {published}"
    );
    assert_eq!(git_provenance["base_ref"], Value::Null);
    assert_eq!(published["data"]["base_ref"], Value::Null);
}

#[test]
fn conflicts_check_rejects_an_intent_id_passed_as_intent_text() {
    let repo = create_repo();
    let agent_id = register_test_agent(&repo.root, "misparse-agent");
    let intent_id = publish_test_intent(
        &repo.root,
        &agent_id,
        "misparse-task",
        "Extend PaymentService with retries",
        "symbol:PaymentService=extend",
    );

    let rejected = cli_failure(
        &repo.root,
        None,
        ["conflicts", "check", "--intent", intent_id.as_str()],
    );
    assert_eq!(rejected["error"]["code"], "INVALID_INPUT");
    assert!(
        rejected["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--intent-id"),
        "the error must point at --intent-id: {rejected}"
    );

    let prose = cli_success(
        &repo.root,
        None,
        [
            "conflicts",
            "check",
            "--intent",
            "Harden int_ prefix parsing in the id layer",
        ],
    );
    assert_eq!(prose["data"]["blocking"], false);
    assert!(
        prose["data"]["checked_intents"].as_u64().unwrap() >= 1,
        "prose containing int_ mid-sentence must still be checked: {prose}"
    );
}

#[test]
fn intent_show_and_agent_list_expose_the_missing_read_surfaces() {
    let repo = create_repo();
    let replacer = register_test_agent(&repo.root, "read-surface-replacer");
    let extender = register_test_agent(&repo.root, "read-surface-extender");
    let replace_intent = publish_test_intent(
        &repo.root,
        &replacer,
        "replace-payments",
        "Replace PaymentService with StripePaymentService",
        "symbol:PaymentService=replace",
    );
    let extend = cli_success(
        &repo.root,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            extender.clone(),
            "--task".to_string(),
            "extend-payments".to_string(),
            "--summary".to_string(),
            "Add PayPal support to PaymentService".to_string(),
            "--scope".to_string(),
            "symbol:PaymentService=extend".to_string(),
        ],
    );
    let conflict_id = extend["data"]["conflicts"][0]["id"]
        .as_str()
        .expect("conflicting publish returns the conflict")
        .to_string();

    let shown = cli_success(
        &repo.root,
        None,
        ["intent", "show", replace_intent.as_str()],
    );
    assert_eq!(
        shown["data"]["intent"]["id"].as_str(),
        Some(replace_intent.as_str())
    );
    assert_eq!(
        shown["data"]["intent"]["summary"],
        "Replace PaymentService with StripePaymentService"
    );
    assert_eq!(shown["data"]["intent"]["task"], "replace-payments");
    assert_eq!(shown["data"]["intent"]["status"], "INTENT");
    assert_eq!(
        shown["data"]["intent"]["scopes"][0],
        json!({ "kind": "symbol", "key": "PaymentService", "operation": "replace" })
    );
    assert_eq!(
        shown["data"]["agent"]["id"].as_str(),
        Some(replacer.as_str())
    );
    assert_eq!(shown["data"]["agent"]["name"], "read-surface-replacer");
    assert_eq!(shown["data"]["open_conflicts"]["count"], 1);
    assert_eq!(
        shown["data"]["open_conflicts"]["ids"][0].as_str(),
        Some(conflict_id.as_str())
    );
    let missing = cli_failure(&repo.root, None, ["intent", "show", "int_missing"]);
    assert_eq!(missing["error"]["code"], "NOT_FOUND");

    let listed = cli_success(&repo.root, None, ["agent", "list"]);
    let agents = listed["data"].as_array().expect("agent list is an array");
    assert_eq!(agents.len(), 2);
    let ids: Vec<&str> = agents
        .iter()
        .map(|agent| agent["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&replacer.as_str()) && ids.contains(&extender.as_str()));
    for agent in agents {
        assert!(agent["name"].as_str().is_some_and(|name| !name.is_empty()));
        assert_eq!(agent["model"], "e2e-test");
        assert_eq!(agent["status"], "ACTIVE");
        assert!(agent.get("worktree").is_some(), "worktree field present");
    }
}

#[test]
fn coordinate_inbox_accepts_the_agent_flag_and_keeps_the_positional_form() {
    let repo = create_repo();
    let sender = register_test_agent(&repo.root, "inbox-sender");
    let receiver = register_test_agent(&repo.root, "inbox-receiver");
    cli_success(
        &repo.root,
        None,
        vec![
            "coordinate".to_string(),
            "send".to_string(),
            "--from".to_string(),
            sender.clone(),
            "--to".to_string(),
            receiver.clone(),
            "--message".to_string(),
            "Flag and positional must agree".to_string(),
        ],
    );

    let positional = cli_success(&repo.root, None, ["coordinate", "inbox", receiver.as_str()]);
    let flagged = cli_success(
        &repo.root,
        None,
        ["coordinate", "inbox", "--agent", receiver.as_str()],
    );
    assert_eq!(positional["data"], flagged["data"]);
    assert_eq!(flagged["data"].as_array().map(Vec::len), Some(1));

    let agreeing = cli_success(
        &repo.root,
        None,
        [
            "coordinate",
            "inbox",
            receiver.as_str(),
            "--agent",
            receiver.as_str(),
        ],
    );
    assert_eq!(agreeing["data"], flagged["data"]);

    let disagreeing = cli_failure(
        &repo.root,
        None,
        [
            "coordinate",
            "inbox",
            receiver.as_str(),
            "--agent",
            sender.as_str(),
        ],
    );
    assert_eq!(disagreeing["error"]["code"], "INVALID_INPUT");

    let unspecified = cli_failure(&repo.root, None, ["coordinate", "inbox"]);
    assert_eq!(unspecified["error"]["code"], "INVALID_INPUT");
}

#[test]
fn later_conflicting_publish_surfaces_open_conflicts_to_the_earlier_publisher() {
    let repo = create_repo();
    let first = register_test_agent(&repo.root, "first-publisher");
    let first_intent = publish_test_intent(
        &repo.root,
        &first,
        "extend-payments",
        "Add PayPal support to PaymentService",
        "symbol:PaymentService=extend",
    );
    claim_test_scope(&repo.root, &first, &first_intent, "symbol:PaymentService");
    start_test_work(&repo.root, &first, &first_intent);
    let early = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            first.clone(),
            "--intent".to_string(),
            first_intent.clone(),
            "--summary".to_string(),
            "Early candidate before any conflict exists".to_string(),
        ],
    );
    assert_eq!(
        early["data"]["open_conflicts"]["count"], 0,
        "the early publish precedes the conflict: {early}"
    );

    let second = register_test_agent(&repo.root, "second-publisher");
    let conflicting = cli_success(
        &repo.root,
        None,
        vec![
            "intent".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            second.clone(),
            "--task".to_string(),
            "replace-payments".to_string(),
            "--summary".to_string(),
            "Replace PaymentService with StripePaymentService".to_string(),
            "--scope".to_string(),
            "symbol:PaymentService=replace".to_string(),
        ],
    );
    let conflict_id = conflicting["data"]["conflicts"][0]["id"]
        .as_str()
        .expect("later publish creates the conflict")
        .to_string();
    let second_intent = conflicting["data"]["intent"]["id"]
        .as_str()
        .expect("second intent id")
        .to_string();

    let revised = cli_success(
        &repo.root,
        None,
        vec![
            "changeset".to_string(),
            "publish".to_string(),
            "--agent".to_string(),
            first,
            "--intent".to_string(),
            first_intent,
            "--summary".to_string(),
            "Revision published after the conflict appeared".to_string(),
        ],
    );
    let open = &revised["data"]["open_conflicts"];
    assert!(
        open["count"].as_u64().unwrap() >= 1,
        "the earlier publisher must see the later conflict: {revised}"
    );
    assert!(
        open["ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id.as_str() == Some(conflict_id.as_str())),
        "open_conflicts must carry the conflict id: {revised}"
    );

    claim_test_scope(&repo.root, &second, &second_intent, "symbol:PaymentService");
    let started = cli_success(
        &repo.root,
        None,
        vec![
            "work".to_string(),
            "start".to_string(),
            second_intent,
            "--agent".to_string(),
            second,
        ],
    );
    assert!(
        started["data"]["open_conflicts"]["ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id.as_str() == Some(conflict_id.as_str())),
        "work start must surface open conflicts: {started}"
    );
}

fn stdio_request(
    stdin: &mut impl Write,
    reader: &mut impl std::io::BufRead,
    request: Value,
) -> Value {
    writeln!(stdin, "{request}").expect("write MCP request");
    stdin.flush().expect("flush MCP request");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read MCP response line");
    assert!(!line.trim().is_empty(), "MCP server closed the stream");
    serde_json::from_str(&line).expect("MCP response is JSON")
}

fn stdio_tool_call(
    stdin: &mut impl Write,
    reader: &mut impl std::io::BufRead,
    id: u64,
    name: &str,
    arguments: Value,
) -> Value {
    let response = stdio_request(
        stdin,
        reader,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }),
    );
    assert_eq!(
        response["result"]["isError"], false,
        "MCP tool {name} failed over stdio: {response}"
    );
    response["result"]["structuredContent"].clone()
}

#[test]
fn real_mcp_stdio_drives_the_lifecycle_through_named_verification_and_acceptance() {
    let repo = create_repo();
    cli_success(
        &repo.root,
        None,
        ["checks", "set", "test", "--", "git", "diff", "--check"],
    );

    let mut child = Command::new(foremerge_bin())
        .arg("--cwd")
        .arg(&repo.root)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn MCP server");
    let mut stdin = child.stdin.take().expect("MCP stdin");
    let mut reader = std::io::BufReader::new(child.stdout.take().expect("MCP stdout"));

    let initialized = stdio_request(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": { "name": "foremerge-e2e", "version": "1" }
            }
        }),
    );
    assert_eq!(initialized["result"]["protocolVersion"], "2026-07-28");

    let agent = stdio_tool_call(
        &mut stdin,
        &mut reader,
        2,
        "register_agent",
        json!({ "name": "stdio-lifecycle", "model": "e2e", "worktree": repo.root }),
    );
    let agent_id = agent["id"].as_str().expect("agent id").to_string();
    let published = stdio_tool_call(
        &mut stdin,
        &mut reader,
        3,
        "publish_intent",
        json!({
            "agent_id": agent_id,
            "task": "stdio-lifecycle",
            "summary": "Drive the full lifecycle over the real stdio transport",
            "scopes": [{ "kind": "symbol", "key": "StdioLifecycle" , "operation": "extend" }]
        }),
    );
    let intent_id = published["intent"]["id"]
        .as_str()
        .expect("intent id")
        .to_string();
    stdio_tool_call(
        &mut stdin,
        &mut reader,
        4,
        "claim_work",
        json!({
            "agent_id": agent_id,
            "intent_id": intent_id,
            "scopes": [{ "kind": "symbol", "key": "StdioLifecycle" , "operation": "extend" }]
        }),
    );
    let started = stdio_tool_call(
        &mut stdin,
        &mut reader,
        5,
        "start_work",
        json!({ "agent_id": agent_id, "intent_id": intent_id }),
    );
    assert_eq!(started["status"], "IN_PROGRESS");
    assert_eq!(started["open_conflicts"]["count"], 0);
    let changeset = stdio_tool_call(
        &mut stdin,
        &mut reader,
        6,
        "publish_changeset",
        json!({
            "agent_id": agent_id,
            "intent_id": intent_id,
            "summary": "Record the clean stdio lifecycle candidate",
            "symbols": ["StdioLifecycle"]
        }),
    );
    let changeset_id = changeset["id"].as_str().expect("changeset id").to_string();
    assert_eq!(changeset["open_conflicts"]["count"], 0);
    // The fixture has exactly one commit, so the candidate is a root commit
    // and its diff base is the empty tree.
    assert_eq!(
        changeset["provenance"]["git"]["base_resolution"],
        "root_commit"
    );
    let validation = stdio_tool_call(
        &mut stdin,
        &mut reader,
        7,
        "run_verification",
        json!({ "changeset_id": changeset_id, "check": "test" }),
    );
    assert_eq!(
        validation["passed"], true,
        "named verification must pass over stdio: {validation}"
    );

    let rejected_override = stdio_request(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "accept_changeset",
                "arguments": {
                    "changeset_id": changeset_id,
                    "allow_high_conflicts": true,
                    "override_reason": "not allowed over MCP"
                }
            }
        }),
    );
    assert_eq!(rejected_override["result"]["isError"], true);
    assert!(
        rejected_override["result"]["structuredContent"]["error"]
            .as_str()
            .unwrap()
            .starts_with("FORBIDDEN"),
        "override must be rejected over stdio: {rejected_override}"
    );

    let accepted = stdio_tool_call(
        &mut stdin,
        &mut reader,
        9,
        "accept_changeset",
        json!({ "changeset_id": changeset_id }),
    );
    assert_eq!(accepted["status"], "ACCEPTED");

    drop(stdin);
    let status = child.wait().expect("wait for MCP server");
    assert!(status.success(), "MCP server exited abnormally");
    assert!(
        Store::open(database_from_doctor(&repo.root))
            .expect("open lifecycle database")
            .verify_event_chain()
            .unwrap()
    );
}

/// Every CLI invocation reopens the store and therefore reruns migration. A
/// one-time backfill that is not gated on the stored schema version mints a
/// duplicate row on the second open, which is how schema 2 fabricated a second
/// "immutable" detection for conflicts that already had a native one. The
/// in-process tests could not see this because they never reopen.
#[test]
fn reopening_the_store_does_not_fabricate_extra_conflict_detections() {
    let repo = create_repo();
    cli_success(&repo.root, None, ["init"]);
    let first = cli_success(
        &repo.root,
        None,
        ["agent", "register", "--name", "stripe", "--no-worktree"],
    )["data"]["id"]
        .as_str()
        .expect("agent id")
        .to_string();
    let second = cli_success(
        &repo.root,
        None,
        ["agent", "register", "--name", "paypal", "--no-worktree"],
    )["data"]["id"]
        .as_str()
        .expect("agent id")
        .to_string();
    cli_success(
        &repo.root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &first,
            "--task",
            "modernize",
            "--summary",
            "Replace PaymentService with StripePaymentService",
            "--scope",
            "symbol:PaymentService",
        ],
    );
    cli_success(
        &repo.root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &second,
            "--task",
            "paypal",
            "--summary",
            "Add PayPal support to PaymentService",
            "--scope",
            "symbol:PaymentService",
        ],
    );
    let conflicts = cli_success(&repo.root, None, ["conflicts", "list"]);
    let conflict_id = conflicts["data"][0]["id"]
        .as_str()
        .expect("a conflict was detected")
        .to_string();

    // Each of these is a separate process, so each reopens and remigrates.
    for _ in 0..3 {
        cli_success(&repo.root, None, ["status"]);
    }

    let detections = cli_success(&repo.root, None, ["conflicts", "detections", &conflict_id]);
    let observations = detections["data"].as_array().expect("detections array");
    assert_eq!(
        observations.len(),
        1,
        "one detection must record exactly one observation across reopens: {detections}"
    );
    assert!(
        !observations[0]["id"]
            .as_str()
            .expect("detection id")
            .starts_with("dtn_legacy_"),
        "a natively detected conflict must not be backfilled as legacy: {detections}"
    );

    let events = cli_success(&repo.root, None, ["events", "list", "--after-seq", "0"]);
    let detected = events["data"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|event| event["event_type"] == "conflict.detected")
        .count();
    assert_eq!(
        detected, 1,
        "the event log and the occurrence table must agree: {events}"
    );
}

/// Drive a repository to a published ChangeSet, returning (repo, agent, intent,
/// changeset). Every test below needs the same runway.
fn repo_with_published_changeset() -> (RepoFixture, String, String, String) {
    let repo = create_repo();
    let root = repo.root.clone();
    cli_success(&root, None, ["init"]);
    let agent = cli_success(
        &root,
        None,
        [
            "agent",
            "register",
            "--name",
            "worker",
            "--worktree",
            root.to_str().unwrap(),
        ],
    )["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let intent = cli_success(
        &root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &agent,
            "--task",
            "t",
            "--summary",
            "Add a greeting helper",
            "--scope",
            "symbol:Greeter::greet",
        ],
    )["data"]["intent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    cli_success(
        &root,
        None,
        [
            "work",
            "claim",
            "--agent",
            &agent,
            "--intent",
            &intent,
            "--scope",
            "symbol:Greeter::greet",
        ],
    );
    cli_success(&root, None, ["work", "start", "--agent", &agent, &intent]);
    fs::write(root.join("greeter.txt"), "greet\n").expect("write file");
    git(&root, ["add", "-A"]);
    git(&root, ["commit", "--quiet", "-m", "add greeter"]);
    let changeset = cli_success(
        &root,
        None,
        [
            "changeset",
            "publish",
            "--agent",
            &agent,
            "--intent",
            &intent,
            "--summary",
            "Adds a greeting helper",
            "--file",
            "greeter.txt",
        ],
    )["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    (repo, agent, intent, changeset)
}

/// A repository with no test suite could not finish the lifecycle at all, and
/// the only way through was to validate a no-op command, which wrote a passing
/// validation into the append-only log for work nothing had checked.
#[test]
fn unverified_work_is_recorded_as_unverified_rather_than_faked() {
    let (repo, _agent, _intent, changeset) = repo_with_published_changeset();
    let root = repo.root.clone();

    // Strict is the default, so this still refuses, but it now names the way out.
    let refusal = cli_failure(&root, None, ["changeset", "accept", &changeset]);
    let message = refusal["error"]["message"].as_str().unwrap();
    assert!(message.contains("UNVERIFIED"), "{message}");
    assert!(message.contains("--allow-unverified"), "{message}");

    let accepted = cli_success(
        &root,
        None,
        [
            "changeset",
            "accept",
            &changeset,
            "--allow-unverified",
            "--override-reason",
            "this repository has no test suite",
        ],
    );
    assert_eq!(accepted["data"]["status"], "ACCEPTED");
    assert_eq!(accepted["data"]["acceptance_verification"], "UNVERIFIED");
    assert_eq!(
        accepted["data"]["acceptance_reason"],
        "this repository has no test suite"
    );

    // The immutable record must say the same thing, since that is the artifact
    // an audit actually reads.
    let events = cli_success(&root, None, ["events", "list", "--limit", "200"]);
    let accepted_event = events["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event_type"] == "changeset.accepted")
        .expect("an acceptance event");
    assert_eq!(accepted_event["payload"]["verification"], "UNVERIFIED");
}

/// An advisory policy is for repositories with nothing to verify. It must never
/// wave through a check that ran and failed, which is evidence of breakage
/// rather than an absence of evidence.
#[test]
fn an_advisory_policy_allows_unverified_work_but_never_a_failing_check() {
    let (repo, _agent, _intent, changeset) = repo_with_published_changeset();
    let root = repo.root.clone();
    cli_success(&root, None, ["checks", "policy", "advisory"]);

    // Nothing ran: permitted, and recorded honestly.
    let accepted = cli_success(&root, None, ["changeset", "accept", &changeset]);
    assert_eq!(accepted["data"]["acceptance_verification"], "UNVERIFIED");

    // A check that ran and failed: refused under the same policy.
    let (repo2, _a2, _i2, changeset2) = repo_with_published_changeset();
    let root2 = repo2.root.clone();
    cli_success(&root2, None, ["checks", "policy", "advisory"]);
    cli_success(
        &root2,
        None,
        ["changeset", "validate", &changeset2, "--", "false"],
    );
    let refusal = cli_failure(&root2, None, ["changeset", "accept", &changeset2]);
    let message = refusal["error"]["message"].as_str().unwrap();
    assert!(message.contains("FAILED"), "{message}");
    // The remedy for a failure is to fix it, not to loosen the policy.
    assert!(
        !message.contains("checks policy advisory"),
        "advisory policy must not be offered as the answer to a real failure: {message}"
    );
}

/// An agent whose work outlasts its lease previously had no way to hold its
/// claims, and lost collision protection silently.
#[test]
fn an_agent_can_renew_its_lease_while_working_without_stacking_claims() {
    let repo = create_repo();
    let root = repo.root.clone();
    cli_success(&root, None, ["init"]);
    let agent = cli_success(
        &root,
        None,
        [
            "agent",
            "register",
            "--name",
            "worker",
            "--worktree",
            root.to_str().unwrap(),
        ],
    )["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let intent = cli_success(
        &root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &agent,
            "--task",
            "t",
            "--summary",
            "s",
            "--scope",
            "symbol:Greeter::greet",
        ],
    )["data"]["intent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let claim_args = [
        "work",
        "claim",
        "--agent",
        &agent,
        "--intent",
        &intent,
        "--scope",
        "symbol:Greeter::greet",
    ];
    let first = cli_success(&root, None, claim_args);
    let first_lease = first["data"]["claims"][0]["lease_expires_at"]
        .as_str()
        .unwrap()
        .to_string();
    cli_success(&root, None, ["work", "start", "--agent", &agent, &intent]);

    // Renewing mid-work is the whole point: previously IN_PROGRESS was refused.
    let renewed = cli_success(
        &root,
        None,
        [
            "work",
            "claim",
            "--agent",
            &agent,
            "--intent",
            &intent,
            "--scope",
            "symbol:Greeter::greet",
            "--lease-seconds",
            "7200",
        ],
    );
    let renewed_lease = renewed["data"]["claims"][0]["lease_expires_at"]
        .as_str()
        .unwrap();
    assert!(
        renewed_lease > first_lease.as_str(),
        "lease should extend: {first_lease} -> {renewed_lease}"
    );

    // The same scope must not end up claimed twice, or `status` overstates what
    // is actually held.
    let status = cli_success(&root, None, ["status"]);
    let claims = status["data"]["claims"].as_array().unwrap();
    assert_eq!(
        claims.len(),
        1,
        "renewal stacked a duplicate claim: {claims:?}"
    );
}

/// Adoption exists to rescue work from an agent that died. The absence of a
/// live claim is not evidence of death: a freshly published intent has never
/// held one, and an intent whose lease merely lapsed may belong to an agent
/// that is still working. Taking either would be theft rather than rescue.
#[test]
fn adoption_is_refused_while_the_owning_agent_is_still_active() {
    let repo = create_repo();
    let root = repo.root.clone();

    let owner = cli_success(
        &root,
        None,
        [
            "agent", "register", "--name", "owner", "--model", "e2e-test",
        ],
    )["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let rival = cli_success(
        &root,
        None,
        [
            "agent", "register", "--name", "rival", "--model", "e2e-test",
        ],
    )["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let intent = cli_success(
        &root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &owner,
            "--task",
            "t",
            "--summary",
            "Add a greeting helper",
            "--scope",
            "symbol:Greeter::greet",
        ],
    )["data"]["intent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    cli_success(
        &root,
        None,
        [
            "work",
            "claim",
            "--agent",
            &owner,
            "--intent",
            &intent,
            "--scope",
            "symbol:Greeter::greet",
        ],
    );

    let refused = cli_failure(
        &root,
        None,
        [
            "work",
            "adopt",
            "--agent",
            &rival,
            "--reason",
            "taking over",
            &intent,
        ],
    );
    let message = refused["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an error message: {refused}"));
    assert!(
        message.contains("has not stopped"),
        "adoption must say the owner is still working, got: {message}"
    );

    // Ownership must be untouched by the refusal.
    let after = cli_success(&root, None, ["intent", "show", &intent]);
    assert_eq!(
        after["data"]["intent"]["agent_id"].as_str().unwrap(),
        owner,
        "a refused adoption must not transfer ownership"
    );
}

/// `last_seen_at` is documented as the agent's last Foremerge call, and both
/// the status report and adoption read staleness from it. Written only at
/// registration it would decay on a schedule instead of tracking activity, and
/// an agent still working after the stale window would read as abandoned.
#[test]
fn acting_on_an_intent_advances_the_agents_last_seen_time() {
    let repo = create_repo();
    let root = repo.root.clone();

    let agent = cli_success(
        &root,
        None,
        [
            "agent", "register", "--name", "worker", "--model", "e2e-test",
        ],
    )["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let last_seen = |root: &Path| -> String {
        let status = cli_success(root, None, ["status"]);
        status["data"]["agents"]
            .as_array()
            .expect("agents array")
            .iter()
            .find(|entry| entry["id"].as_str() == Some(&agent))
            .expect("the registered agent appears in status")["last_seen_at"]
            .as_str()
            .expect("last_seen_at is a string")
            .to_string()
    };

    let at_registration = last_seen(&root);

    cli_success(
        &root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &agent,
            "--task",
            "t",
            "--summary",
            "Add a greeting helper",
            "--scope",
            "symbol:Greeter::greet",
        ],
    );

    let after_publishing = last_seen(&root);
    assert!(
        after_publishing > at_registration,
        "publishing an intent must advance last_seen_at: {at_registration} -> {after_publishing}"
    );
}

/// Two same-named symbols in different namespaces are different code, and one
/// intent may legitimately touch both. Keyed on the lossy canonical alias they
/// collided, so publication failed outright with a primary-key error and the
/// migration's reprojection quietly dropped the second scope.
#[test]
fn an_intent_may_declare_the_same_symbol_name_under_two_namespaces() {
    let repo = create_repo();
    let root = repo.root.clone();

    let agent = cli_success(
        &root,
        None,
        [
            "agent", "register", "--name", "author", "--model", "e2e-test",
        ],
    )["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let published = cli_success(
        &root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &agent,
            "--task",
            "reports",
            "--summary",
            "Rework both report renderers",
            "--scope",
            "symbol:App\\Billing\\Report::render",
            "--scope",
            "symbol:App\\Admin\\Report::render",
        ],
    );

    let scopes: Vec<String> = published["data"]["intent"]["scopes"]
        .as_array()
        .expect("scopes array")
        .iter()
        .map(|scope| scope["key"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        scopes,
        vec![
            "App\\Billing\\Report::render".to_string(),
            "App\\Admin\\Report::render".to_string()
        ],
        "both declared scopes must survive publication"
    );

    // The alias is still the search net: an agent naming the bare symbol finds
    // this work even though it declared the fully qualified names.
    let found = cli_success(
        &root,
        None,
        ["work", "query", "--scope", "symbol:Report::render"],
    );
    assert!(
        found["data"]
            .as_array()
            .is_some_and(|intents| !intents.is_empty()),
        "the canonical alias must still match a differently qualified name: {found}"
    );
}

/// Claim renewal matched on the canonical alias, so claiming two symbols whose
/// names differ only by namespace renewed the first instead of recording the
/// second. The response carried two claims sharing one id, the table kept only
/// the first scope, and the graph showed only the second: three views of the
/// same moment, all disagreeing.
#[test]
fn claiming_two_namespaced_symbols_records_two_distinct_claims() {
    let repo = create_repo();
    let root = repo.root.clone();

    let agent = cli_success(
        &root,
        None,
        [
            "agent", "register", "--name", "author", "--model", "e2e-test",
        ],
    )["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let intent = cli_success(
        &root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &agent,
            "--task",
            "reports",
            "--summary",
            "Rework both report renderers",
            "--scope",
            "symbol:App\\Billing\\Report::render",
            "--scope",
            "symbol:App\\Admin\\Report::render",
        ],
    )["data"]["intent"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let claimed = cli_success(
        &root,
        None,
        [
            "work",
            "claim",
            "--agent",
            &agent,
            "--intent",
            &intent,
            "--scope",
            "symbol:App\\Billing\\Report::render",
            "--scope",
            "symbol:App\\Admin\\Report::render",
        ],
    );

    let claims = claimed["data"]["claims"].as_array().expect("claims array");
    assert_eq!(claims.len(), 2, "{claimed}");
    let ids: BTreeSet<&str> = claims
        .iter()
        .map(|claim| claim["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "two claims must not share one id: {claimed}");
    let response_scopes: BTreeSet<&str> = claims
        .iter()
        .map(|claim| claim["scope"]["key"].as_str().unwrap())
        .collect();

    // Status and the graph project the same rows, so all three must agree.
    let status = cli_success(&root, None, ["status"]);
    let status_scopes: BTreeSet<&str> = status["data"]["claims"]
        .as_array()
        .expect("status claims")
        .iter()
        .map(|claim| claim["scope"]["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        status_scopes, response_scopes,
        "status must show the same two claims the response returned: {status}"
    );

    let graph = cli_success(&root, None, ["graph"]);
    let graph_scopes: BTreeSet<&str> = graph["data"]["nodes"]
        .as_array()
        .expect("graph nodes")
        .iter()
        .filter(|node| node["kind"] == "Claim")
        .map(|node| node["data"]["scope"]["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        graph_scopes, response_scopes,
        "the graph must project the same two claims the response returned"
    );
}

/// Renewal matches on the precise identity, which folds separator and case
/// spellings together, so a renewal can arrive spelled differently from the row
/// it renews. The stored row used to keep the original spelling while the
/// response and the graph carried the new one: one claim, three descriptions.
#[test]
fn renewing_a_claim_with_an_equivalent_spelling_keeps_every_view_agreed() {
    let repo = create_repo();
    let root = repo.root.clone();

    let agent = cli_success(
        &root,
        None,
        [
            "agent", "register", "--name", "author", "--model", "e2e-test",
        ],
    )["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let intent = cli_success(
        &root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &agent,
            "--task",
            "reports",
            "--summary",
            "Rework the report renderer",
            "--scope",
            "symbol:App\\Billing\\Report::render",
        ],
    )["data"]["intent"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let claim = |scope: &str| {
        cli_success(
            &root,
            None,
            [
                "work", "claim", "--agent", &agent, "--intent", &intent, "--scope", scope,
            ],
        )
    };
    claim("symbol:App\\Billing\\Report::render");
    let renewed = claim("symbol:app/billing/Report::render");

    let claims = renewed["data"]["claims"].as_array().expect("claims");
    assert_eq!(claims.len(), 1, "an equivalent scope renews, never stacks");
    let returned = claims[0]["scope"]["key"].as_str().unwrap();

    let status = cli_success(&root, None, ["status"]);
    let stored: Vec<&str> = status["data"]["claims"]
        .as_array()
        .expect("status claims")
        .iter()
        .map(|entry| entry["scope"]["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        stored,
        vec![returned],
        "the stored claim must carry the spelling the response reported: {status}"
    );

    let graph = cli_success(&root, None, ["graph"]);
    let projected: Vec<&str> = graph["data"]["nodes"]
        .as_array()
        .expect("graph nodes")
        .iter()
        .filter(|node| node["kind"] == "Claim")
        .map(|node| node["data"]["scope"]["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        projected,
        vec![returned],
        "the graph must project the same spelling: {graph}"
    );
}

/// The canonical alias is deliberately non-unique, so one intent can hold
/// several claims that share it. The overlap lookup returned that intent once
/// per matching row, reporting the same overlap two or three times.
#[test]
fn an_intent_holding_two_aliased_claims_is_reported_as_one_overlap() {
    let repo = create_repo();
    let root = repo.root.clone();

    let register = |name: &str| {
        cli_success(
            &root,
            None,
            ["agent", "register", "--name", name, "--model", "e2e-test"],
        )["data"]["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let holder = register("holder");
    let newcomer = register("newcomer");

    let held = cli_success(
        &root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &holder,
            "--task",
            "reports",
            "--summary",
            "Rework both report renderers",
            "--scope",
            "symbol:App\\Billing\\Report::render",
            "--scope",
            "symbol:App\\Admin\\Report::render",
        ],
    )["data"]["intent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    cli_success(
        &root,
        None,
        [
            "work",
            "claim",
            "--agent",
            &holder,
            "--intent",
            &held,
            "--scope",
            "symbol:App\\Billing\\Report::render",
            "--scope",
            "symbol:App\\Admin\\Report::render",
        ],
    );

    let arriving = cli_success(
        &root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &newcomer,
            "--task",
            "reports-2",
            "--summary",
            "Touch the report renderer",
            "--scope",
            "symbol:Report::render",
        ],
    )["data"]["intent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let claimed = cli_success(
        &root,
        None,
        [
            "work",
            "claim",
            "--agent",
            &newcomer,
            "--intent",
            &arriving,
            "--scope",
            "symbol:Report::render",
        ],
    );

    let warnings = claimed["data"]["warnings"].as_array().expect("warnings");
    assert_eq!(
        warnings.len(),
        1,
        "one overlapping intent is one overlap, however many aliased claims it holds: {claimed}"
    );
}

/// Two spellings of one symbol name one scope. Storage keys on that identity,
/// so a request carrying both used to reach SQLite as a duplicate and surface a
/// raw primary-key violation, or record one claim and report it twice.
#[test]
fn equivalent_scopes_in_one_request_are_folded_before_storage() {
    let repo = create_repo();
    let root = repo.root.clone();

    let agent = cli_success(
        &root,
        None,
        [
            "agent", "register", "--name", "author", "--model", "e2e-test",
        ],
    )["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let published = cli_success(
        &root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &agent,
            "--task",
            "reports",
            "--summary",
            "Rework the report renderer",
            "--scope",
            "symbol:App\\Billing\\Report::render",
            "--scope",
            "symbol:app/billing/Report::render",
        ],
    );
    let intent = published["data"]["intent"]["id"].as_str().unwrap();
    assert_eq!(
        published["data"]["intent"]["scopes"]
            .as_array()
            .expect("scopes")
            .len(),
        1,
        "one scope named twice is one scope: {published}"
    );

    let claimed = cli_success(
        &root,
        None,
        [
            "work",
            "claim",
            "--agent",
            &agent,
            "--intent",
            intent,
            "--scope",
            "symbol:App\\Billing\\Report::render",
            "--scope",
            "symbol:app/billing/Report::render",
        ],
    );
    let claims = claimed["data"]["claims"].as_array().expect("claims");
    assert_eq!(claims.len(), 1, "one claim, reported once: {claimed}");
}

/// Folding cannot silently pick a winner when the two entries disagree about
/// what will happen to the scope. That is a contradiction only the caller can
/// resolve, and it must be reported as bad input rather than as a database
/// error.
#[test]
fn contradictory_operations_on_one_scope_are_rejected_as_bad_input() {
    let repo = create_repo();
    let root = repo.root.clone();

    let agent = cli_success(
        &root,
        None,
        [
            "agent", "register", "--name", "author", "--model", "e2e-test",
        ],
    )["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let refused = cli_failure(
        &root,
        None,
        [
            "intent",
            "publish",
            "--agent",
            &agent,
            "--task",
            "reports",
            "--summary",
            "Rework the report renderer",
            "--scope",
            "symbol:App\\Billing\\Report::render=replace",
            "--scope",
            "symbol:app/billing/Report::render=extend",
        ],
    );
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.starts_with("INVALID_INPUT"),
        "a contradiction is bad input, not a database failure: {refused}"
    );
    assert!(
        message.contains("different operations"),
        "the message must say what is contradictory: {message}"
    );
}
