//! Adversarial probe for the intent detector.
//!
//! The detector decides severity from the verbs an agent happens to use. This
//! test measures how far that generalises, in both directions:
//!
//! * **Set A** restates one genuine replace-versus-extend conflict ten ways,
//!   holding the declared scopes identical, so every miss is a paraphrase
//!   failure and nothing else.
//! * **Sets B and C** are pairs of genuinely compatible work that mention a
//!   shared scope while containing destructive keywords. Every HIGH here is a
//!   false alarm, and a false HIGH is worse than silence: it trains agents to
//!   ignore the one severity that is supposed to stop them.
//!
//! The gate below asserts only the property that generalises (no false HIGH)
//! plus a floor on Set A to catch regressions. Raising that floor by adding
//! more verbs to `OPERATION_BUCKETS` would be tuning against this file, which
//! `docs/benchmark-plan.md` explicitly forbids. Paraphrases held back from
//! this file are the real measure.

use foremerge::conflict::{IntentCandidate, detect_pair};
use foremerge::model::Scope;

struct Pair {
    label: &'static str,
    left: &'static str,
    left_scopes: Vec<Scope>,
    right: &'static str,
    right_scopes: Vec<Scope>,
}

fn scope(kind: &str, key: &str) -> Scope {
    Scope {
        kind: kind.into(),
        key: key.into(),
    }
}

fn payment_scopes() -> Vec<Scope> {
    vec![
        scope("symbol", "PaymentService"),
        scope("contract", "payments.provider"),
    ]
}

fn cache_scopes() -> Vec<Scope> {
    vec![scope("component", "ThumbnailCache")]
}

/// Same declared scopes on both sides; only the wording differs.
fn symmetric(label: &'static str, left: &'static str, right: &'static str, s: Vec<Scope>) -> Pair {
    Pair {
        label,
        left,
        left_scopes: s.clone(),
        right,
        right_scopes: s,
    }
}

/// Highest severity either agent sees, in both publish orders.
fn worst(pair: &Pair) -> (String, String) {
    let left = IntentCandidate {
        id: "a".into(),
        summary: pair.left.into(),
        scopes: pair.left_scopes.clone(),
    };
    let right = IntentCandidate {
        id: "b".into(),
        summary: pair.right.into(),
        scopes: pair.right_scopes.clone(),
    };
    let mut all = detect_pair(&left, &right);
    all.extend(detect_pair(&right, &left));
    all.iter()
        .max_by_key(|conflict| match conflict.severity.as_str() {
            "HIGH" => 3,
            "MEDIUM" => 2,
            "LOW" => 1,
            _ => 0,
        })
        .map(|conflict| (conflict.severity.clone(), conflict.kind.clone()))
        .unwrap_or_else(|| ("NONE".into(), "-".into()))
}

fn report(title: &str, pairs: &[Pair]) -> (usize, usize) {
    println!("\n=== {title} ===");
    let mut high = 0;
    let mut any = 0;
    for pair in pairs {
        let (severity, kind) = worst(pair);
        if severity == "HIGH" {
            high += 1;
        }
        if severity != "NONE" {
            any += 1;
        }
        println!("  {:<28} {severity:<7} {kind}", pair.label);
    }
    (high, any)
}

#[test]
fn paraphrase_probe() {
    // Set A: every pair is the same genuine conflict as fixture 01.
    let set_a = vec![
        symmetric(
            "baseline (in-vocabulary)",
            "Replace PaymentService with StripePaymentService",
            "Add PayPal support to PaymentService",
            payment_scopes(),
        ),
        symmetric(
            "consolidate / back with",
            "Consolidate all payment handling onto Stripe in PaymentService",
            "Back PaymentService with an additional PayPal gateway",
            payment_scopes(),
        ),
        symmetric(
            "port / wire up",
            "Port PaymentService over to the Stripe SDK",
            "Wire up PayPal as another option in PaymentService",
            payment_scopes(),
        ),
        symmetric(
            "cut over / teach",
            "Cut PaymentService over to Stripe exclusively",
            "Teach PaymentService to accept PayPal payments",
            payment_scopes(),
        ),
        symmetric(
            "deprecate / expose",
            "Deprecate the current PaymentService internals in favour of Stripe",
            "Expose a PayPal path through PaymentService",
            payment_scopes(),
        ),
        symmetric(
            "sunset / plug in",
            "Sunset the hand-rolled logic inside PaymentService for Stripe",
            "Plug a PayPal client into PaymentService",
            payment_scopes(),
        ),
        symmetric(
            "overhaul / enable",
            "Overhaul PaymentService so Stripe is the only backend",
            "Enable PayPal alongside the existing PaymentService flow",
            payment_scopes(),
        ),
        symmetric(
            "tear out / offer",
            "Tear the legacy processor out of PaymentService and use Stripe",
            "Offer PayPal as a second processor in PaymentService",
            payment_scopes(),
        ),
        symmetric(
            "standardize / handle",
            "Standardize PaymentService on the Stripe integration",
            "Handle PayPal callbacks inside PaymentService",
            payment_scopes(),
        ),
        symmetric(
            "collapse / integrate",
            "Collapse PaymentService down to a single Stripe implementation",
            "Integrate PayPal into PaymentService",
            payment_scopes(),
        ),
    ];

    // Set B: compatible work, both agents claiming the same coarse scope.
    let set_b = vec![
        symmetric(
            "move loop / add metric",
            "Move the ThumbnailCache eviction loop into a background task",
            "Add a hit-rate metric to ThumbnailCache",
            cache_scopes(),
        ),
        symmetric(
            "drop counter / add TTL",
            "Drop the unused debug counter from ThumbnailCache",
            "Add TTL-based expiry to ThumbnailCache",
            cache_scopes(),
        ),
        symmetric(
            "convert logging / add warm",
            "Convert ThumbnailCache logging to structured fields",
            "Add a cache-warming step to ThumbnailCache",
            cache_scopes(),
        ),
        symmetric(
            "remove dead code / extend",
            "Remove dead code left behind in ThumbnailCache",
            "Extend ThumbnailCache to store WebP variants",
            cache_scopes(),
        ),
        symmetric(
            "rename locals / add bounds",
            "Rename the confusing local variables inside ThumbnailCache",
            "Add bounds checking to ThumbnailCache",
            cache_scopes(),
        ),
        symmetric(
            "delete test / add limit",
            "Delete the flaky ThumbnailCache benchmark test",
            "Implement a size limit for ThumbnailCache",
            cache_scopes(),
        ),
        symmetric(
            "retire flag / add AVIF",
            "Retire the obsolete feature flag guarding ThumbnailCache",
            "Add AVIF support to ThumbnailCache",
            cache_scopes(),
        ),
    ];

    // Set C: the same compatible work, but each agent claims a precise,
    // disjoint scope. Anything firing here is reading the prose, not the claim.
    let set_c = vec![
        Pair {
            label: "delete test / add limit",
            left: "Delete the flaky ThumbnailCache benchmark test",
            left_scopes: vec![scope("file", "tests/thumbnail_bench.rs")],
            right: "Implement a size limit for ThumbnailCache",
            right_scopes: vec![scope("symbol", "ThumbnailCache")],
        },
        Pair {
            label: "retire flag / add AVIF",
            left: "Retire the obsolete feature flag guarding ThumbnailCache",
            left_scopes: vec![scope("config", "features.thumbnails_v2")],
            right: "Add AVIF support to ThumbnailCache",
            right_scopes: vec![scope("symbol", "ThumbnailCache")],
        },
    ];

    let (a_high, _) = report(
        "SET A: real conflicts, paraphrased (scopes identical)",
        &set_a,
    );
    let (b_high, _) = report("SET B: compatible work, shared scope", &set_b);
    let (c_high, _) = report("SET C: compatible work, disjoint scopes", &set_c);

    println!(
        "\n  Set A conflicts detected at HIGH: {a_high}/{}",
        set_a.len()
    );
    println!("  Set B false HIGH: {b_high}/{}", set_b.len());
    println!("  Set C false HIGH: {c_high}/{}\n", set_c.len());

    assert_eq!(
        b_high, 0,
        "compatible work must never raise a HIGH intent conflict"
    );
    assert_eq!(
        c_high, 0,
        "compatible work with disjoint claims must never raise a HIGH intent conflict"
    );
    assert!(
        a_high >= 6,
        "paraphrase coverage regressed below the recorded floor: {a_high}/10"
    );
}
