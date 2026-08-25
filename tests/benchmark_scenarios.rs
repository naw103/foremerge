use foremerge::model::*;
use foremerge::{Foremerge, Store};
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Deserialize)]
struct Scenario {
    id: String,
    category: String,
    agents: Vec<ScenarioAgent>,
    ground_truth: GroundTruth,
}

#[derive(Debug, Deserialize)]
struct ScenarioAgent {
    label: String,
    task: String,
    intent: String,
    scopes: Vec<Scope>,
}

#[derive(Debug, Deserialize)]
struct GroundTruth {
    should_warn: bool,
    severity: String,
    required_suggestion_terms: Vec<String>,
}

fn scenarios() -> Vec<Scenario> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benchmarks")
        .join("scenarios");
    let mut paths = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            serde_json::from_slice(&fs::read(&path).unwrap())
                .unwrap_or_else(|error| panic!("decode {}: {error}", path.display()))
        })
        .collect()
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[tokio::test]
async fn committed_benchmark_corpus_matches_executable_ground_truth() {
    let scenarios = scenarios();
    assert_eq!(scenarios.len(), 5, "the complete committed corpus must run");
    let mut executed = Vec::new();

    for scenario in scenarios {
        executed.push(scenario.id.clone());
        if scenario.category == "verification-gate" {
            run_validation_gate(&scenario).await;
            continue;
        }

        let service = Foremerge::new(Store::in_memory().unwrap());
        let mut latest_conflicts = Vec::new();
        for input in &scenario.agents {
            let agent = service
                .register_agent(RegisterAgentRequest {
                    name: input.label.clone(),
                    model: Some("benchmark-scripted".into()),
                    capabilities: vec![],
                    worktree: None,
                })
                .unwrap();
            latest_conflicts = service
                .publish_intent(PublishIntentRequest {
                    agent_id: agent.id,
                    task: input.task.clone(),
                    summary: input.intent.clone(),
                    rationale: None,
                    scopes: input.scopes.clone(),
                    depends_on: vec![],
                    metadata: serde_json::json!({ "benchmark": scenario.id }),
                })
                .unwrap()
                .conflicts;
        }
        let material = latest_conflicts
            .iter()
            .filter(|conflict| matches!(conflict.severity.as_str(), "HIGH" | "MEDIUM"))
            .collect::<Vec<_>>();
        assert_eq!(
            !material.is_empty(),
            scenario.ground_truth.should_warn,
            "scenario {} warning verdict: {latest_conflicts:#?}",
            scenario.id
        );
        if scenario.ground_truth.should_warn {
            assert!(
                material.iter().any(|conflict| {
                    conflict
                        .severity
                        .eq_ignore_ascii_case(&scenario.ground_truth.severity)
                }),
                "scenario {} expected {}: {material:#?}",
                scenario.id,
                scenario.ground_truth.severity
            );
            for term in &scenario.ground_truth.required_suggestion_terms {
                let term = term.to_lowercase();
                assert!(
                    material
                        .iter()
                        .any(|conflict| conflict.suggestion.to_lowercase().contains(&term)),
                    "scenario {} suggestion must contain {term}: {material:#?}",
                    scenario.id
                );
            }
        }
    }

    assert_eq!(executed.len(), 5);
}

async fn run_validation_gate(scenario: &Scenario) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "--quiet"]);
    git(&root, &["config", "user.name", "Foremerge Benchmark"]);
    git(
        &root,
        &["config", "user.email", "benchmark@example.invalid"],
    );
    fs::write(root.join("README.md"), "benchmark\n").unwrap();
    git(&root, &["add", "README.md"]);
    git(&root, &["commit", "--quiet", "-m", "fixture"]);
    let head = git(&root, &["rev-parse", "HEAD"]);

    let database = root.join(".git/foremerge/state.sqlite3");
    let service = Foremerge::new(Store::open(database).unwrap());
    service.bind_repository_cwd(&root).unwrap();
    let input = &scenario.agents[0];
    let agent = service
        .register_agent(RegisterAgentRequest {
            name: input.label.clone(),
            model: Some("benchmark-scripted".into()),
            capabilities: vec![],
            worktree: Some(root.to_string_lossy().into_owned()),
        })
        .unwrap();
    let intent = service
        .publish_intent(PublishIntentRequest {
            agent_id: agent.id.clone(),
            task: input.task.clone(),
            summary: input.intent.clone(),
            rationale: None,
            scopes: input.scopes.clone(),
            depends_on: vec![],
            metadata: serde_json::json!({ "benchmark": scenario.id }),
        })
        .unwrap()
        .intent;
    service
        .claim_work(ClaimWorkRequest {
            agent_id: agent.id.clone(),
            intent_id: intent.id.clone(),
            scopes: input.scopes.clone(),
            reason: None,
            lease_seconds: 3600,
        })
        .unwrap();
    service.start_work(&agent.id, &intent.id).unwrap();
    let changeset = service
        .publish_changeset(PublishChangeSetRequest {
            agent_id: agent.id,
            intent_id: intent.id,
            summary: "benchmark failing candidate".into(),
            files: vec![],
            symbols: vec![],
            contracts: vec![],
            dependencies: vec![],
            tests: vec![],
            decisions: vec![],
            provenance: Value::Object(Default::default()),
            git_ref: Some(head.clone()),
            base_ref: None,
            worktree: Some(root.to_string_lossy().into_owned()),
        })
        .unwrap();
    #[cfg(unix)]
    let command = vec!["sh".into(), "-c".into(), "exit 7".into()];
    #[cfg(windows)]
    let command = vec!["cmd".into(), "/C".into(), "exit /B 7".into()];
    let validation = service
        .validate_changeset(
            &changeset.id,
            ValidationRequest {
                command,
                worktree: None,
                timeout_seconds: 5,
            },
        )
        .await
        .unwrap();
    assert!(!validation.passed);
    assert_eq!(service.validation_attempts(&changeset.id).unwrap().len(), 1);
    assert!(
        service
            .accept_changeset(
                &changeset.id,
                AcceptRequest {
                    git_ref: None,
                    allow_high_conflicts: false,
                    allow_unverified: false,
                    override_reason: None,
                },
            )
            .is_err()
    );
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
    let accepted_ref = format!("refs/foremerge/accepted/{}", changeset.id);
    assert!(
        !Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["show-ref", "--verify", "--quiet", &accepted_ref])
            .status()
            .unwrap()
            .success()
    );
}
