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

/// English sentence-starters and connectives that IDENT_RE can match at the
/// start of a sentence but that are never code identifiers.
const SUBJECT_STOPLIST: &[&str] = &[
    "a", "add", "after", "all", "also", "an", "and", "any", "as", "at", "before", "both", "but",
    "by", "do", "does", "each", "ensure", "first", "fix", "for", "from", "if", "in", "it", "its",
    "keep", "make", "new", "next", "no", "none", "not", "note", "now", "of", "on", "once", "only",
    "or", "our", "second", "so", "some", "the", "then", "these", "they", "this", "those", "thus",
    "to", "update", "use", "we", "when", "while", "with", "yes",
];

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

    let operation = classify_operation(summary);
    let subject = IDENT_RE
        .find_iter(summary)
        .filter(|found| confident_identifier(summary, found))
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

/// Classify by the FIRST operation keyword by position in the summary, so a
/// summary such as "Add promotional credits, then migrate callers" reads as
/// additive rather than destructive.
fn classify_operation(summary: &str) -> Operation {
    summary
        .to_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .find_map(keyword_operation)
        .unwrap_or(Operation::Unknown)
}

fn keyword_operation(token: &str) -> Option<Operation> {
    match token {
        "replace" | "rewrite" | "supersede" | "swap" => Some(Operation::Replace),
        "remove" | "delete" | "drop" | "retire" => Some(Operation::Remove),
        "rename" | "move" => Some(Operation::Rename),
        "migrate" | "convert" => Some(Operation::Migrate),
        "extend" | "augment" | "support" | "supports" | "supported" | "supporting" => {
            Some(Operation::Extend)
        }
        "add" | "introduce" | "implement" | "create" => Some(Operation::Add),
        "modify" | "change" | "update" | "refactor" | "fix" => Some(Operation::Modify),
        _ => None,
    }
}

/// A fallback-extracted subject must look like a code identifier: CamelCase
/// with at least two humps (PaymentService, CreditLedger), containing `_` or
/// `::`, or wrapped in backticks in the summary — and never a stoplisted
/// English sentence-starter such as "No" or "This".
fn confident_identifier(summary: &str, found: &regex::Match) -> bool {
    let word = found.as_str();
    if SUBJECT_STOPLIST.contains(&word.to_ascii_lowercase().as_str()) {
        return false;
    }
    if word.contains('_') || word.contains("::") || camel_humps(word) >= 2 {
        return true;
    }
    let bytes = summary.as_bytes();
    found.start() > 0 && bytes[found.start() - 1] == b'`' && bytes.get(found.end()) == Some(&b'`')
}

fn camel_humps(word: &str) -> usize {
    let mut humps = 0;
    let mut previous_is_lower_or_digit = false;
    for character in word.chars() {
        if character.is_ascii_uppercase() {
            if humps == 0 || previous_is_lower_or_digit {
                humps += 1;
            }
            previous_is_lower_or_digit = false;
        } else {
            previous_is_lower_or_digit =
                character.is_ascii_lowercase() || character.is_ascii_digit();
        }
    }
    humps
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

#[derive(Debug, Clone)]
struct ScopeOverlap {
    /// Best-overlapping scope declared by the source intent.
    source: Scope,
    /// Best-overlapping scope declared by the target intent.
    target: Scope,
    score: f64,
    reason: String,
}

impl ScopeOverlap {
    fn keys_differ(&self) -> bool {
        !self.source.key.eq_ignore_ascii_case(&self.target.key)
    }

    /// Human-readable clause naming both sides when their keys differ, so
    /// each agent can match the conflict against a scope it declared.
    fn describe(&self) -> String {
        if self.keys_differ() {
            format!(
                "`{}` overlaps `{}` ({})",
                self.source.key, self.target.key, self.reason
            )
        } else {
            format!("both rely on {}", self.reason)
        }
    }
}

fn scope_overlap(left: &[Scope], right: &[Scope]) -> Option<ScopeOverlap> {
    for first in left {
        for second in right {
            if first.canonical() == second.canonical() {
                return Some(ScopeOverlap {
                    source: first.clone(),
                    target: second.clone(),
                    score: 1.0,
                    reason: "exact semantic scope".to_string(),
                });
            }
            let first_key = first.key.to_lowercase();
            let second_key = second.key.to_lowercase();
            if first_key == second_key {
                return Some(ScopeOverlap {
                    source: first.clone(),
                    target: second.clone(),
                    score: 0.9,
                    reason: "same semantic key across scope kinds".to_string(),
                });
            }
            let first_tokens = tokenize(&first.key);
            let second_tokens = tokenize(&second.key);
            let similarity = jaccard(&first_tokens, &second_tokens);
            if similarity >= 0.66 {
                return Some(ScopeOverlap {
                    source: first.clone(),
                    target: second.clone(),
                    score: 0.65 + similarity * 0.2,
                    reason: "overlapping semantic scope tokens".to_string(),
                });
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

/// Scope kinds where the right coordination advice is agreeing an explicit
/// migration order rather than introducing a provider abstraction.
const MIGRATION_KINDS: &[&str] = &["schema", "migration", "config"];

fn migration_order_suggestion(scope: &Scope) -> String {
    format!(
        "Agree the migration order first: land both changes to `{}` as one sequenced migration plan, or rebase one intent onto the other's outcome. This is a heuristic suggestion, not an automatic design decision.",
        scope.key
    )
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
        inferred.map(|scope| ScopeOverlap {
            source: scope.clone(),
            target: scope,
            score: 0.78,
            reason: "shared identifier inferred from intent".to_string(),
        })
    });
    let summary_similarity = jaccard(&tokenize(&source.summary), &tokenize(&target.summary));
    let mut results = Vec::new();

    if let Some(overlap) = &overlap {
        let scope = overlap.source.clone();
        let scope_score = overlap.score;
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
            let suggestion = if MIGRATION_KINDS.contains(&scope.kind.as_str()) {
                migration_order_suggestion(&scope)
            } else {
                provider_suggestion(&scope, destructive, additive)
            };
            results.push(make_conflict(
                "replace_vs_extend",
                "HIGH",
                score,
                source,
                target,
                Some(scope.clone()),
                format!(
                    "One intent will {} `{}` while the other will {} it; {}.",
                    destructive.operation.as_str(),
                    destructive.subject.as_deref().unwrap_or(&scope.key),
                    additive.operation.as_str(),
                    overlap.describe()
                ),
                suggestion,
                json!({
                    "rule": "FM-C001",
                    "source_operation": source_inference.operation.as_str(),
                    "target_operation": target_inference.operation.as_str(),
                    "overlap": overlap.reason,
                    "source_scope": overlap.source.canonical(),
                    "target_scope": overlap.target.canonical(),
                    "detected_before_code": true,
                }),
            ));
            return results;
        }

        if source_inference.operation.destructive()
            && target_inference.operation.destructive()
            && source_inference.destination != target_inference.destination
        {
            let explanation = if overlap.keys_differ() {
                format!(
                    "Both intents destructively change `{}` (overlapping `{}`), but they point toward different outcomes.",
                    overlap.source.key, overlap.target.key
                )
            } else {
                format!(
                    "Both intents destructively change `{}`, but they point toward different outcomes.",
                    scope.key
                )
            };
            let suggestion = if MIGRATION_KINDS.contains(&scope.kind.as_str()) {
                migration_order_suggestion(&scope)
            } else {
                format!(
                    "Choose one target design for `{}` and record the decision before either agent continues.",
                    scope.key
                )
            };
            results.push(make_conflict(
                "divergent_replacement",
                "HIGH",
                (scope_score * 0.94).min(0.97),
                source,
                target,
                Some(scope.clone()),
                explanation,
                suggestion,
                json!({
                    "rule": "FM-C002",
                    "source_destination": source_inference.destination,
                    "target_destination": target_inference.destination,
                    "source_scope": overlap.source.canonical(),
                    "target_scope": overlap.target.canonical(),
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
                    "Both intents change `{}` ({}); their implementations may depend on the same contract.",
                    scope.key,
                    overlap.describe()
                ),
                "Exchange the proposed contract and dependency order before publishing ChangeSets. Claims remain advisory and both agents may continue.".to_string(),
                json!({
                    "rule": "FM-C003",
                    "source_operation": source_inference.operation.as_str(),
                    "target_operation": target_inference.operation.as_str(),
                    "source_scope": overlap.source.canonical(),
                    "target_scope": overlap.target.canonical(),
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
            overlap.as_ref().map(|value| value.source.clone()),
            "The two intents have substantially similar goals and may be solving the same work twice."
                .to_string(),
            "Coordinate ownership: compare intended outcomes and either split the scope or pick one implementation owner."
                .to_string(),
            json!({
                "rule": "FM-C004",
                "summary_jaccard": summary_similarity,
                "source_operation": source_inference.operation.as_str(),
                "target_operation": target_inference.operation.as_str(),
                "source_scope": overlap.as_ref().map(|value| value.source.canonical()),
                "target_scope": overlap.as_ref().map(|value| value.target.canonical()),
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
        json!({
            "rule": "FM-C005",
            "soft_claim": true,
            "source_scope": scope.canonical(),
            "target_scope": scope.canonical(),
        }),
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
        let conflicts = detect_pair(&first, &second);
        let duplicate = conflicts
            .iter()
            .find(|conflict| conflict.kind == "duplicate_work")
            .expect("duplicate_work finding");
        assert!(duplicate.suggestion.to_lowercase().contains("coordinate"));
    }

    #[test]
    fn sentence_starters_are_not_extracted_as_subjects() {
        let migrate = candidate(
            "a",
            "Centralize credit ledger arithmetic into a new service and migrate callers. No schema changes; behavior preserved.",
            &["symbol:CreditLedgerService"],
        );
        let extend = candidate(
            "b",
            "Extend the credit ledger with promotional credits",
            &["symbol:CreditLedger"],
        );
        let conflicts = detect_pair(&migrate, &extend);
        assert_eq!(conflicts[0].kind, "replace_vs_extend");
        assert!(!conflicts[0].explanation.contains("`No`"));
        assert!(!conflicts[0].explanation.contains("No`"));
        assert!(!conflicts[0].suggestion.contains("NoProvider"));
        assert!(!conflicts[0].suggestion.contains("`No`"));
        assert!(conflicts[0].explanation.contains("CreditLedgerService"));
    }

    #[test]
    fn first_operation_keyword_wins_over_later_destructive_words() {
        let inference = infer("Add promotional credits, then migrate callers");
        assert_eq!(inference.operation, Operation::Add);
        assert!(inference.operation.additive());
    }

    #[test]
    fn camel_case_identifiers_require_two_humps_or_backticks() {
        assert_eq!(
            infer("Update PaymentService for the new flow")
                .subject
                .as_deref(),
            Some("PaymentService")
        );
        assert_eq!(
            infer("No schema changes; behavior preserved.").subject,
            None
        );
        assert_eq!(
            infer("Update the `Invoice` model rendering")
                .subject
                .as_deref(),
            Some("Invoice")
        );
        assert_eq!(infer("Then update the invoice rendering").subject, None);
    }

    #[test]
    fn schema_conflicts_suggest_migration_order() {
        let rename = candidate(
            "a",
            "Rename users.email to users.primary_email and update the public user contract",
            &["schema:users.email", "contract:api.User.email"],
        );
        let index = candidate(
            "b",
            "Add a unique index and normalization migration for users.email",
            &["schema:users.email", "migration:users-email-unique"],
        );
        let conflicts = detect_pair(&rename, &index);
        assert_eq!(conflicts[0].kind, "replace_vs_extend");
        assert_eq!(conflicts[0].severity, "HIGH");
        assert!(conflicts[0].suggestion.contains("migration order"));
        assert!(!conflicts[0].suggestion.contains("Provider"));
    }

    #[test]
    fn token_overlap_evidence_names_both_scopes() {
        let migrate = candidate(
            "a",
            "Centralize credit ledger arithmetic into a new service and migrate callers",
            &["symbol:CreditLedgerService"],
        );
        let extend = candidate(
            "b",
            "Extend the credit ledger with promotional credits",
            &["symbol:CreditLedger"],
        );
        let conflicts = detect_pair(&migrate, &extend);
        assert_eq!(conflicts[0].kind, "replace_vs_extend");
        assert_eq!(
            conflicts[0].evidence["source_scope"],
            "symbol:creditledgerservice"
        );
        assert_eq!(conflicts[0].evidence["target_scope"], "symbol:creditledger");
        assert!(conflicts[0].explanation.contains("CreditLedgerService"));
        assert!(conflicts[0].explanation.contains("overlaps `CreditLedger`"));
    }
}
