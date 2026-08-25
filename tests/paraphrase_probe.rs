//! Adversarial probe for intent conflict detection.
//!
//! The same ten pairs are run twice, and the difference between the two runs
//! is the whole argument for declared operations.
//!
//! * **Declared** is what an agent sends: each scope carries the operation the
//!   intent performs on it. Wording is then irrelevant, because nothing has to
//!   be recovered from it.
//! * **Inferred** is the CLI fallback for a person who typed prose and no
//!   `--operation`. Foremerge guesses, and a guess is capped below HIGH.
//!
//! Sets B and C are genuinely compatible work that mentions a shared scope.
//! Every HIGH there is a false alarm, and a false HIGH is worse than silence:
//! severity is what an agent uses to decide what to stop for.

use foremerge::conflict::{IntentCandidate, detect_pair, infer_operation};
use foremerge::model::{Operation, Scope, ScopeClaim};

struct Pair {
    label: &'static str,
    left: &'static str,
    left_scope: &'static str,
    left_operation: Operation,
    right: &'static str,
    right_scope: &'static str,
    right_operation: Operation,
}

fn pair(
    label: &'static str,
    left: (&'static str, &'static str, Operation),
    right: (&'static str, &'static str, Operation),
) -> Pair {
    Pair {
        label,
        left: left.0,
        left_scope: left.1,
        left_operation: left.2,
        right: right.0,
        right_scope: right.1,
        right_operation: right.2,
    }
}

/// Build a candidate either from the declared operation or, for the inferred
/// arm, from whatever the prose fallback recovers.
fn candidate(
    id: &str,
    summary: &str,
    scope: &str,
    declared: Operation,
    infer: bool,
) -> IntentCandidate {
    let scope = Scope::parse(scope).unwrap();
    let claim = if infer {
        let recovered = infer_operation(summary, &scope).unwrap_or(Operation::Modify);
        ScopeClaim::inferred(scope, recovered)
    } else {
        ScopeClaim::new(scope, declared)
    };
    IntentCandidate {
        id: id.into(),
        summary: summary.into(),
        scopes: vec![claim],
    }
}

fn worst(pair: &Pair, infer: bool) -> (String, String) {
    let left = candidate("a", pair.left, pair.left_scope, pair.left_operation, infer);
    let right = candidate(
        "b",
        pair.right,
        pair.right_scope,
        pair.right_operation,
        infer,
    );
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
    println!(
        "  {:<28} {:<20} inferred from prose",
        "phrasing", "declared"
    );
    let mut declared_high = 0;
    let mut inferred_high = 0;
    for pair in pairs {
        let (declared, declared_kind) = worst(pair, false);
        let (inferred, _) = worst(pair, true);
        if declared == "HIGH" {
            declared_high += 1;
        }
        if inferred == "HIGH" {
            inferred_high += 1;
        }
        println!(
            "  {:<28} {:<20} {}",
            pair.label,
            format!("{declared} {declared_kind}"),
            inferred
        );
    }
    (declared_high, inferred_high)
}

const PS: &str = "symbol:PaymentService";
const TC: &str = "component:ThumbnailCache";

#[test]
fn paraphrase_probe() {
    // Every Set A pair is the same genuine conflict as fixture 01, reworded.
    let set_a = vec![
        pair(
            "baseline",
            (
                "Replace PaymentService with StripePaymentService",
                PS,
                Operation::Replace,
            ),
            (
                "Add PayPal support to PaymentService",
                PS,
                Operation::Extend,
            ),
        ),
        pair(
            "consolidate / back with",
            (
                "Consolidate all payment handling onto Stripe",
                PS,
                Operation::Replace,
            ),
            (
                "Back PaymentService with an additional PayPal gateway",
                PS,
                Operation::Extend,
            ),
        ),
        pair(
            "port / wire up",
            (
                "Port PaymentService over to the Stripe SDK",
                PS,
                Operation::Replace,
            ),
            (
                "Wire up PayPal as another option in PaymentService",
                PS,
                Operation::Extend,
            ),
        ),
        pair(
            "cut over / teach",
            (
                "Cut PaymentService over to Stripe exclusively",
                PS,
                Operation::Replace,
            ),
            (
                "Teach PaymentService to accept PayPal payments",
                PS,
                Operation::Extend,
            ),
        ),
        pair(
            "deprecate / expose",
            (
                "Deprecate the current PaymentService internals",
                PS,
                Operation::Replace,
            ),
            (
                "Expose a PayPal path through PaymentService",
                PS,
                Operation::Extend,
            ),
        ),
        pair(
            "sunset / plug in",
            (
                "Sunset the hand-rolled logic inside PaymentService",
                PS,
                Operation::Replace,
            ),
            (
                "Plug a PayPal client into PaymentService",
                PS,
                Operation::Extend,
            ),
        ),
        pair(
            "overhaul / enable",
            (
                "Overhaul PaymentService so Stripe is the only backend",
                PS,
                Operation::Replace,
            ),
            (
                "Enable PayPal alongside the existing flow",
                PS,
                Operation::Extend,
            ),
        ),
        pair(
            "tear out / offer",
            (
                "Tear the legacy processor out of PaymentService",
                PS,
                Operation::Replace,
            ),
            ("Offer PayPal as a second processor", PS, Operation::Extend),
        ),
        pair(
            "standardize / handle",
            (
                "Standardize PaymentService on the Stripe integration",
                PS,
                Operation::Replace,
            ),
            (
                "Handle PayPal callbacks inside PaymentService",
                PS,
                Operation::Extend,
            ),
        ),
        pair(
            "collapse / integrate",
            (
                "Collapse PaymentService to a single Stripe implementation",
                PS,
                Operation::Replace,
            ),
            (
                "Integrate PayPal into PaymentService",
                PS,
                Operation::Extend,
            ),
        ),
    ];

    // Set B: compatible work, both sides declaring the same scope honestly.
    let set_b = vec![
        pair(
            "move loop / add metric",
            (
                "Move the ThumbnailCache eviction loop into a task",
                TC,
                Operation::Modify,
            ),
            (
                "Add a hit-rate metric to ThumbnailCache",
                TC,
                Operation::Extend,
            ),
        ),
        pair(
            "drop counter / add TTL",
            (
                "Drop the unused debug counter from ThumbnailCache",
                TC,
                Operation::Modify,
            ),
            (
                "Add TTL-based expiry to ThumbnailCache",
                TC,
                Operation::Extend,
            ),
        ),
        pair(
            "convert logging / warm",
            (
                "Convert ThumbnailCache logging to structured fields",
                TC,
                Operation::Modify,
            ),
            (
                "Add a cache-warming step to ThumbnailCache",
                TC,
                Operation::Extend,
            ),
        ),
        pair(
            "remove dead code / extend",
            (
                "Remove dead code left behind in ThumbnailCache",
                TC,
                Operation::Modify,
            ),
            (
                "Extend ThumbnailCache to store WebP variants",
                TC,
                Operation::Extend,
            ),
        ),
        pair(
            "rename locals / bounds",
            (
                "Rename the confusing local variables in ThumbnailCache",
                TC,
                Operation::Modify,
            ),
            (
                "Add bounds checking to ThumbnailCache",
                TC,
                Operation::Extend,
            ),
        ),
        pair(
            "delete test / add limit",
            (
                "Delete the flaky ThumbnailCache benchmark test",
                TC,
                Operation::Modify,
            ),
            (
                "Implement a size limit for ThumbnailCache",
                TC,
                Operation::Extend,
            ),
        ),
        pair(
            "retire flag / add AVIF",
            (
                "Retire the obsolete feature flag guarding ThumbnailCache",
                TC,
                Operation::Modify,
            ),
            ("Add AVIF support to ThumbnailCache", TC, Operation::Extend),
        ),
    ];

    let (a_declared, a_inferred) = report("SET A: real conflicts, ten phrasings", &set_a);
    let (b_declared, b_inferred) = report("SET B: compatible work, shared scope", &set_b);

    println!(
        "\n  Set A caught at HIGH   declared {a_declared}/{}   inferred {a_inferred}/{}",
        set_a.len(),
        set_a.len()
    );
    println!(
        "  Set B false HIGH       declared {b_declared}/{}   inferred {b_inferred}/{}\n",
        set_b.len(),
        set_b.len()
    );

    // The declared path must be phrasing-independent: all ten, every wording.
    assert_eq!(
        a_declared,
        set_a.len(),
        "a declared operation must not depend on how the summary is worded"
    );
    // Compatible work must never reach HIGH by either path.
    assert_eq!(
        b_declared, 0,
        "compatible declared work must never raise a HIGH"
    );
    assert_eq!(
        b_inferred, 0,
        "compatible inferred work must never raise a HIGH"
    );
    // An operation Foremerge guessed is never allowed to assert.
    assert_eq!(
        a_inferred, 0,
        "inferred operations must never assert HIGH, however confident the wording looks"
    );
}
