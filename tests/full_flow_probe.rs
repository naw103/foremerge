//! What does an agent ACTUALLY receive end to end? Registers two agents,
//! publishes both intents, then has both claim their scopes, exactly as a
//! real coordinated run would. Prints every warning either agent sees.

use foremerge::model::*;
use foremerge::{Foremerge, Store};

fn scope(kind: &str, key: &str) -> Scope {
    Scope {
        kind: kind.into(),
        key: key.into(),
    }
}

fn run(label: &str, a: (&str, &str, Vec<Scope>), b: (&str, &str, Vec<Scope>)) {
    let service = Foremerge::new(Store::in_memory().unwrap());
    let mut seen: Vec<(String, String, String)> = Vec::new();

    for (who, task, summary, scopes) in [
        ("agent-a", a.0, a.1, a.2.clone()),
        ("agent-b", b.0, b.1, b.2.clone()),
    ] {
        let agent = service
            .register_agent(RegisterAgentRequest {
                name: who.into(),
                model: Some("probe".into()),
                capabilities: vec![],
                worktree: None,
            })
            .unwrap()
            .agent;

        let published = service
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
        for c in &published.conflicts {
            seen.push((format!("{who} publish"), c.severity.clone(), c.kind.clone()));
        }

        let claimed = service
            .claim_work(ClaimWorkRequest {
                agent_id: agent.id,
                intent_id: published.intent.id,
                scopes,
                reason: None,
                lease_seconds: 3600,
            })
            .unwrap();
        for c in &claimed.warnings {
            seen.push((format!("{who} claim"), c.severity.clone(), c.kind.clone()));
        }
    }

    println!("\n--- {label} ---");
    if seen.is_empty() {
        println!("  (no warning of any kind)");
    }
    for (stage, severity, kind) in seen {
        println!("  {:<16} {:<7} {}", stage, severity, kind);
    }
}

#[test]
fn full_flow_probe() {
    let pay = vec![
        scope("symbol", "PaymentService"),
        scope("contract", "payments.provider"),
    ];

    run(
        "A1 REAL CONFLICT, in-vocab wording (your demo)",
        (
            "Modernize payments",
            "Replace PaymentService with StripePaymentService",
            pay.clone(),
        ),
        (
            "Add payment option",
            "Add PayPal support to PaymentService",
            pay.clone(),
        ),
    );

    run(
        "A2 SAME REAL CONFLICT, reworded",
        (
            "Modernize payments",
            "Consolidate all payment handling onto Stripe in PaymentService",
            pay.clone(),
        ),
        (
            "Add payment option",
            "Back PaymentService with an additional PayPal gateway",
            pay.clone(),
        ),
    );

    run(
        "A3 SAME REAL CONFLICT, reworded again",
        (
            "Modernize payments",
            "Cut PaymentService over to Stripe exclusively",
            pay.clone(),
        ),
        (
            "Add payment option",
            "Teach PaymentService to accept PayPal payments",
            pay.clone(),
        ),
    );

    let cache = vec![scope("component", "ThumbnailCache")];

    run(
        "B1 NO CONFLICT, harmless work, same scope",
        (
            "Clean up tests",
            "Delete the flaky ThumbnailCache benchmark test",
            cache.clone(),
        ),
        (
            "Bound the cache",
            "Implement a size limit for ThumbnailCache",
            cache.clone(),
        ),
    );

    run(
        "B2 NO CONFLICT, harmless work, DISJOINT scopes",
        (
            "Clean up tests",
            "Delete the flaky ThumbnailCache benchmark test",
            vec![scope("file", "tests/thumbnail_bench.rs")],
        ),
        (
            "Bound the cache",
            "Implement a size limit for ThumbnailCache",
            vec![scope("symbol", "ThumbnailCache")],
        ),
    );
    println!();
}
