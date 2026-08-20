use crate::model::{Conflict, Scope};
use chrono::Utc;
use regex::Regex;
use serde_json::json;
use std::collections::HashSet;
use std::sync::LazyLock;
use uuid::Uuid;

static REPLACE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:replace|swap(?:\s+out)?|supersede)\s+([A-Za-z_][A-Za-z0-9_:.-]*)\s+(?:with|for|by)\s+([A-Za-z_][A-Za-z0-9_:.-]*)",
    )
    .expect("valid replace regex")
});
static ADD_SUPPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:add|introduce|implement)\s+([A-Za-z_][A-Za-z0-9_:.-]*)\s+support\s+(?:to|for|in)\s+([A-Za-z_][A-Za-z0-9_:.-]*)",
    )
    .expect("valid support regex")
});
static IDENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z][A-Za-z0-9_]*(?:Service|Provider|Client|API|Schema)?\b").unwrap()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Add,
    Extend,
    Modify,
    Replace,
    Rename,
    Remove,
    Migrate,
    Unknown,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Extend => "extend",
            Self::Modify => "modify",
            Self::Replace => "replace",
            Self::Rename => "rename",
            Self::Remove => "remove",
            Self::Migrate => "migrate",
            Self::Unknown => "unknown",
        }
    }

    fn destructive(self) -> bool {
        matches!(
            self,
            Self::Replace | Self::Rename | Self::Remove | Self::Migrate
        )
    }

    fn additive(self) -> bool {
        matches!(self, Self::Add | Self::Extend | Self::Modify)
    }
}

#[derive(Debug, Clone)]
pub struct IntentCandidate {
    pub id: String,
    pub summary: String,
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone)]
struct Inference {
    operation: Operation,
    subject: Option<String>,
    destination: Option<String>,
    added_variant: Option<String>,
    confidence: f64,
}

fn infer(summary: &str) -> Inference {
    if let Some(captures) = REPLACE_RE.captures(summary) {
        return Inference {
            operation: Operation::Replace,
            subject: captures.get(1).map(|value| value.as_str().to_string()),
            destination: captures.get(2).map(|value| value.as_str().to_string()),
            added_variant: None,
            confidence: 0.98,
        };
    }
    if let Some(captures) = ADD_SUPPORT_RE.captures(summary) {
        return Inference {
            operation: Operation::Extend,
            subject: captures.get(2).map(|value| value.as_str().to_string()),
            destination: None,
            added_variant: captures.get(1).map(|value| value.as_str().to_string()),
            confidence: 0.97,
        };
    }

    let lower = summary.to_lowercase();
    let operation = if contains_word(&lower, &["replace", "rewrite", "supersede", "swap"]) {
        Operation::Replace
    } else if contains_word(&lower, &["remove", "delete", "drop", "retire"]) {
        Operation::Remove
    } else if contains_word(&lower, &["rename", "move"]) {
        Operation::Rename
    } else if contains_word(&lower, &["migrate", "convert"]) {
        Operation::Migrate
    } else if lower.contains("support") || contains_word(&lower, &["extend", "augment"]) {
        Operation::Extend
    } else if contains_word(&lower, &["add", "introduce", "implement", "create"]) {
        Operation::Add
    } else if contains_word(&lower, &["modify", "change", "update", "refactor", "fix"]) {
        Operation::Modify
    } else {
        Operation::Unknown
    };
    let subject = IDENT_RE
        .find_iter(summary)
        .last()
        .map(|value| value.as_str().to_string());
    Inference {
        operation,
        subject,
        destination: None,
        added_variant: None,
        confidence: if operation == Operation::Unknown {
            0.35
        } else {
            0.72
        },
    }
}

fn contains_word(text: &str, words: &[&str]) -> bool {
    tokenize(text)
        .iter()
        .any(|token| words.iter().any(|word| token == word))
}

pub fn tokenize(text: &str) -> HashSet<String> {
    let mut expanded = String::with_capacity(text.len() * 2);
    let mut previous_lower = false;
    for character in text.chars() {
        if character.is_ascii_uppercase() && previous_lower {
            expanded.push(' ');
        }
        if character.is_ascii_alphanumeric() {
            expanded.push(character.to_ascii_lowercase());
            previous_lower = character.is_ascii_lowercase();
        } else {
            expanded.push(' ');
            previous_lower = false;
        }
    }
    const STOP: &[&str] = &[
        "a", "an", "and", "as", "at", "by", "for", "from", "in", "of", "on", "the", "to", "with",
    ];
    expanded
        .split_whitespace()
        .filter(|token| token.len() > 1 && !STOP.contains(token))
        .map(str::to_string)
        .collect()
}

fn jaccard(left: &HashSet<String>, right: &HashSet<String>) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(right).count() as f64;
    let union = left.union(right).count() as f64;
    intersection / union
}

fn scope_overlap(left: &[Scope], right: &[Scope]) -> Option<(Scope, f64, String)> {
    for first in left {
        for second in right {
            if first.canonical() == second.canonical() {
                return Some((first.clone(), 1.0, "exact semantic scope".to_string()));
            }
            let first_key = first.key.to_lowercase();
            let second_key = second.key.to_lowercase();
            if first_key == second_key {
                return Some((
                    first.clone(),
                    0.9,
                    "same semantic key across scope kinds".to_string(),
                ));
            }
            let first_tokens = tokenize(&first.key);
            let second_tokens = tokenize(&second.key);
            let similarity = jaccard(&first_tokens, &second_tokens);
            if similarity >= 0.66 {
                return Some((
                    first.clone(),
                    0.65 + similarity * 0.2,
                    "overlapping semantic scope tokens".to_string(),
                ));
            }
        }
    }
    None
}

fn inferred_scope(left: &Inference, right: &Inference) -> Option<Scope> {
    match (&left.subject, &right.subject) {
        (Some(first), Some(second)) if first.eq_ignore_ascii_case(second) => {
            Some(Scope::new("symbol", first))
        }
        _ => None,
    }
}

fn provider_suggestion(scope: &Scope, destructive: &Inference, additive: &Inference) -> String {
    let key = destructive
        .subject
        .as_deref()
        .or(Some(scope.key.as_str()))
        .unwrap_or("the shared component");
    let stem = key.strip_suffix("Service").unwrap_or(key);
    let abstraction = if stem.is_empty() {
        format!("{key}Provider")
    } else {
        format!("{stem}Provider")
    };
    let mut implementations = Vec::new();
    if let Some(destination) = &destructive.destination {
        let destination_stem = destination.strip_suffix("Service").unwrap_or(destination);
        implementations.push(format!("{destination_stem}Provider"));
    }
    if let Some(variant) = &additive.added_variant {
        let variant_stem = variant.strip_suffix("Service").unwrap_or(variant);
        if variant_stem.to_lowercase().contains(&stem.to_lowercase()) {
            implementations.push(format!("{variant_stem}Provider"));
        } else {
            implementations.push(format!("{variant_stem}{stem}Provider"));
        }
    }
    let implementations = if implementations.is_empty() {
        "the competing implementations".to_string()
    } else {
        implementations.join(" and ")
    };
    format!(
        "Coordinate on a stable `{abstraction}` contract first, then implement {implementations} behind it and migrate callers deliberately. This is a heuristic suggestion, not an automatic design decision."
    )
}

pub fn detect_pair(source: &IntentCandidate, target: &IntentCandidate) -> Vec<Conflict> {
    let source_inference = infer(&source.summary);
    let target_inference = infer(&target.summary);
    let explicit_overlap = scope_overlap(&source.scopes, &target.scopes);
    let inferred = inferred_scope(&source_inference, &target_inference);
    let overlap = explicit_overlap.or_else(|| {
        inferred.map(|scope| {
            (
                scope,
                0.78,
                "shared identifier inferred from intent".to_string(),
            )
        })
    });
    let summary_similarity = jaccard(&tokenize(&source.summary), &tokenize(&target.summary));
    let mut results = Vec::new();

    if let Some((scope, scope_score, overlap_reason)) = overlap.clone() {
        let operations_collide = (source_inference.operation.destructive()
            && target_inference.operation.additive())
            || (target_inference.operation.destructive() && source_inference.operation.additive());
        if operations_collide {
            let (destructive, additive) = if source_inference.operation.destructive() {
                (&source_inference, &target_inference)
            } else {
                (&target_inference, &source_inference)
            };
            let score =
                (scope_score * destructive.confidence.min(additive.confidence) * 1.02).min(0.99);
            results.push(make_conflict(
                "replace_vs_extend",
                "HIGH",
                score,
                source,
                target,
                Some(scope.clone()),
                format!(
                    "One intent will {} `{}` while the other will {} it; both rely on {overlap_reason}.",
                    destructive.operation.as_str(),
                    destructive.subject.as_deref().unwrap_or(&scope.key),
                    additive.operation.as_str()
                ),
                provider_suggestion(&scope, destructive, additive),
                json!({
                    "rule": "FM-C001",
                    "source_operation": source_inference.operation.as_str(),
                    "target_operation": target_inference.operation.as_str(),
                    "overlap": overlap_reason,
                    "detected_before_code": true,
                }),
            ));
            return results;
        }

        if source_inference.operation.destructive()
            && target_inference.operation.destructive()
            && source_inference.destination != target_inference.destination
        {
            results.push(make_conflict(
                "divergent_replacement",
                "HIGH",
                (scope_score * 0.94).min(0.97),
                source,
                target,
                Some(scope.clone()),
                format!(
                    "Both intents destructively change `{}`, but they point toward different outcomes.",
                    scope.key
                ),
                format!(
                    "Choose one target design for `{}` and record the decision before either agent continues.",
                    scope.key
                ),
                json!({
                    "rule": "FM-C002",
                    "source_destination": source_inference.destination,
                    "target_destination": target_inference.destination,
                }),
            ));
            return results;
        }

        if source_inference.operation == Operation::Modify
            || target_inference.operation == Operation::Modify
            || (source_inference.operation.additive() && target_inference.operation.additive())
        {
            results.push(make_conflict(
                "shared_semantic_scope",
                "MEDIUM",
                (scope_score * 0.76).min(0.86),
                source,
                target,
                Some(scope.clone()),
                format!(
                    "Both intents change `{}` ({overlap_reason}); their implementations may depend on the same contract.",
                    scope.key
                ),
                "Exchange the proposed contract and dependency order before publishing ChangeSets. Claims remain advisory and both agents may continue.".to_string(),
                json!({
                    "rule": "FM-C003",
                    "source_operation": source_inference.operation.as_str(),
                    "target_operation": target_inference.operation.as_str(),
                }),
            ));
        }
    }

    let same_operation = source_inference.operation == target_inference.operation
        && source_inference.operation != Operation::Unknown;
    if summary_similarity >= 0.62
        || (same_operation && overlap.is_some() && summary_similarity > 0.42)
    {
        results.push(make_conflict(
            "duplicate_work",
            "MEDIUM",
            (0.55 + summary_similarity * 0.4).min(0.96),
            source,
            target,
            overlap.map(|value| value.0),
            "The two intents have substantially similar goals and may be solving the same work twice."
                .to_string(),
            "Compare intended outcomes and either split the scope or select one agent as the implementation owner."
                .to_string(),
            json!({
                "rule": "FM-C004",
                "summary_jaccard": summary_similarity,
                "source_operation": source_inference.operation.as_str(),
                "target_operation": target_inference.operation.as_str(),
            }),
        ));
    }
    results
}

pub fn claim_overlap_conflict(
    source_intent_id: &str,
    target_intent_id: &str,
    scope: &Scope,
) -> Conflict {
    let source = IntentCandidate {
        id: source_intent_id.to_string(),
        summary: "semantic claim".to_string(),
        scopes: vec![scope.clone()],
    };
    let target = IntentCandidate {
        id: target_intent_id.to_string(),
        summary: "existing semantic claim".to_string(),
        scopes: vec![scope.clone()],
    };
    make_conflict(
        "overlapping_claim",
        "MEDIUM",
        0.88,
        &source,
        &target,
        Some(scope.clone()),
        format!("Another active agent already claims `{}`.", scope.canonical()),
        "Coordinate ownership or split the semantic scope. This warning is advisory; Foremerge does not hard-lock work."
            .to_string(),
        json!({ "rule": "FM-C005", "soft_claim": true }),
    )
}

#[allow(clippy::too_many_arguments)]
fn make_conflict(
    kind: &str,
    severity: &str,
    score: f64,
    source: &IntentCandidate,
    target: &IntentCandidate,
    scope: Option<Scope>,
    explanation: String,
    suggestion: String,
    evidence: serde_json::Value,
) -> Conflict {
    Conflict {
        id: format!("cfl_{}", Uuid::new_v4().simple()),
        kind: kind.to_string(),
        severity: severity.to_string(),
        score: (score * 1000.0).round() / 1000.0,
        source_intent_id: Some(source.id.clone()),
        target_intent_id: target.id.clone(),
        scope,
        explanation,
        suggestion,
        evidence,
        status: "OPEN".to_string(),
        detected_at: Utc::now().to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, summary: &str, scopes: &[&str]) -> IntentCandidate {
        IntentCandidate {
            id: id.to_string(),
            summary: summary.to_string(),
            scopes: scopes
                .iter()
                .map(|value| Scope::parse(value).unwrap())
                .collect(),
        }
    }

    #[test]
    fn payment_service_conflict_is_high_and_suggests_provider() {
        let replace = candidate(
            "a",
            "Replace PaymentService with StripePaymentService",
            &["symbol:PaymentService"],
        );
        let extend = candidate(
            "b",
            "Add PayPal support to PaymentService",
            &["symbol:PaymentService"],
        );
        let conflicts = detect_pair(&extend, &replace);
        assert_eq!(conflicts[0].severity, "HIGH");
        assert_eq!(conflicts[0].kind, "replace_vs_extend");
        assert!(conflicts[0].suggestion.contains("PaymentProvider"));
        assert!(conflicts[0].suggestion.contains("StripePaymentProvider"));
        assert!(conflicts[0].suggestion.contains("PayPalPaymentProvider"));
    }

    #[test]
    fn independent_scopes_do_not_conflict() {
        let first = candidate(
            "a",
            "Add PayPal support to PaymentService",
            &["symbol:PaymentService"],
        );
        let second = candidate(
            "b",
            "Add avatar upload to UserProfile",
            &["api:POST /avatar"],
        );
        assert!(detect_pair(&first, &second).is_empty());
    }

    #[test]
    fn similar_work_is_reported_as_duplicate() {
        let first = candidate(
            "a",
            "Add retry support to BillingClient",
            &["symbol:BillingClient"],
        );
        let second = candidate(
            "b",
            "Implement retry support for BillingClient",
            &["symbol:BillingClient"],
        );
        assert!(
            detect_pair(&first, &second)
                .iter()
                .any(|conflict| conflict.kind == "duplicate_work")
        );
    }
}
