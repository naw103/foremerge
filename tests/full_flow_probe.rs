//! What two agents actually exchange, end to end.
//!
//! Registers both agents, publishes both intents, claims both sets of scopes,
//! and prints everything either agent receives, including the `related_work`
//! the second publisher is asked to assess. A single detector call cannot show
//! this, and reading one is how the claim path went unexamined for so long.

use foremerge::model::*;
use foremerge::{Foremerge, Store};

fn claim(value: &str, operation: Operation) -> ScopeClaim {
    ScopeClaim::new(Scope::parse(value).unwrap(), operation)
}

fn run(label: &str, first: (&str, &str, Vec<ScopeClaim>), second: (&str, &str, Vec<ScopeClaim>)) {
    let service = Foremerge::new(Store::in_memory().unwrap());
    println!("\n--- {label} ---");

    let mut published = Vec::new();
    for (name, (task, summary, scopes)) in [("agent-a", first), ("agent-b", second)] {
        let agent = service
            .register_agent(RegisterAgentRequest {
                name: name.into(),
                model: Some("probe".into()),
                capabilities: vec![],
                worktree: None,
            })
            .unwrap();
        let outcome = service
            .publish_intent(PublishIntentRequest {
                agent_id: agent.id.clone(),
                task: task.into(),
                summary: summary.into(),
                rationale: None,
                scopes: scopes.clone(),
                depends_on: vec![],
                metadata: serde_json::json!({}),
            })
            .unwrap();

        for conflict in &outcome.conflicts {
            println!(
                "  {name} publish  {:<7} {}",
                conflict.severity, conflict.kind
            );
        }
        for related in &outcome.related_work {
            println!(
                "  {name} related  {} ({}) asserted={}",
                related.agent, related.summary, related.asserted
            );
            for view in &related.overlap {
                println!(
                    "      {} : you {} / they {} -> {}",
                    view.scope.canonical(),
                    view.your_operation.as_str(),
                    view.their_operation.as_str(),
                    view.interaction.as_str()
                );
            }
        }
        if outcome.assessment_required {
            println!(
                "  {name} must assess {} item(s)",
                outcome.related_work.len()
            );
        }

        let claimed = service
            .claim_work(ClaimWorkRequest {
                agent_id: agent.id.clone(),
                intent_id: outcome.intent.id.clone(),
                scopes: scopes.iter().map(|claim| claim.scope.clone()).collect(),
                reason: None,
                lease_seconds: 3600,
            })
            .unwrap();
        for warning in &claimed.warnings {
            println!("  {name} claim    {:<7} {}", warning.severity, warning.kind);
        }
        published.push((agent, outcome));
    }

    // The second publisher closes the loop by recording what it concluded.
    let (agent, outcome) = published.pop().unwrap();
    if let Some(related) = outcome.related_work.first() {
        let assessment = service
            .record_assessment(RecordAssessmentRequest {
                agent_id: agent.id,
                intent_id: outcome.intent.id.clone(),
                related_intent_id: related.intent_id.clone(),
                verdict: AssessmentVerdict::Conflicts,
                rationale: "The other intent removes the extension point this one needs".into(),
                action: AssessmentAction::Rescoping,
            })
            .unwrap();
        println!(
            "  agent-b assessed {} -> {} / {}",
            related.intent_id,
            assessment.verdict.as_str(),
            assessment.action.as_str()
        );
    }
}

#[test]
fn full_flow_probe() {
    let pay = vec![
        claim("symbol:PaymentService", Operation::Replace),
        claim("contract:payments.provider", Operation::Replace),
    ];
    let pay_extend = vec![
        claim("symbol:PaymentService", Operation::Extend),
        claim("contract:payments.provider", Operation::Extend),
    ];
    run(
        "REAL CONFLICT, declared operations",
        (
            "Modernize payments",
            "Consolidate all payment handling onto Stripe",
            pay,
        ),
        (
            "Add payment option",
            "Back PaymentService with a PayPal gateway",
            pay_extend,
        ),
    );

    let cache_modify = vec![claim("component:ThumbnailCache", Operation::Modify)];
    let cache_extend = vec![claim("component:ThumbnailCache", Operation::Extend)];
    run(
        "COMPATIBLE WORK, same scope",
        (
            "Clean up tests",
            "Delete the flaky ThumbnailCache benchmark test",
            cache_modify,
        ),
        (
            "Bound the cache",
            "Implement a size limit for ThumbnailCache",
            cache_extend,
        ),
    );
    println!();
}
