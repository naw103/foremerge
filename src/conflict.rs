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
    Regex::new(r"\b(?:[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+|[A-Z][A-Za-z0-9_]*)\b")
        .expect("valid identifier regex")
});
static BACKTICK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`([A-Za-z_][A-Za-z0-9_:.-]*)`").expect("valid backtick regex"));

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

fn infer(summary: &str, scopes: &[Scope]) -> Inference {
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
    let subject = select_subject(summary, scopes);
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

/// Operation buckets in destructive-priority order: a destructive keyword
/// anywhere in the summary outranks additive phrasing around it, so "Add a
/// migration to drop the legacy users.email column" classifies as remove.
/// This deliberately errs toward higher severity, because under-flagging
/// destructive work is strictly worse than a cosmetic operation label.
const OPERATION_BUCKETS: &[(Operation, &[&str])] = &[
    (
        Operation::Replace,
        &["replace", "rewrite", "supersede", "swap"],
    ),
    (Operation::Remove, &["remove", "delete", "drop", "retire"]),
    (Operation::Rename, &["rename", "move"]),
    (Operation::Migrate, &["migrate", "convert"]),
    (Operation::Extend, &["extend", "augment"]),
    (Operation::Add, &["add", "introduce", "implement", "create"]),
    (
        Operation::Modify,
        &["modify", "change", "update", "refactor", "fix"],
    ),
];

fn classify_operation(summary: &str) -> Operation {
    let lowered = summary.to_lowercase();
    let tokens: HashSet<&str> = lowered
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    for (operation, keywords) in OPERATION_BUCKETS {
        if keywords.iter().any(|keyword| tokens.contains(keyword))
            || (*operation == Operation::Extend && lowered.contains("support"))
        {
            return *operation;
        }
    }
    Operation::Unknown
}

/// Choose the fallback subject. Backticked spans are extracted first and are
/// the strongest candidates: any backticked token, including lowercase and
/// `::`-qualified names, is accepted unless it is a stoplisted English word.
/// Bare tokens must pass `confident_identifier`. Among the surviving
/// candidates, one whose tokens overlap a semantic scope key the intent
/// declared is preferred; only otherwise does the last candidate win.
fn select_subject(summary: &str, scopes: &[Scope]) -> Option<String> {
    let backticked: Vec<&str> = BACKTICK_RE
        .captures_iter(summary)
        .filter_map(|captures| captures.get(1))
        .map(|value| value.as_str())
        .filter(|word| !SUBJECT_STOPLIST.contains(&word.to_ascii_lowercase().as_str()))
        .collect();
    let bare: Vec<&str> = IDENT_RE
        .find_iter(summary)
        .map(|found| found.as_str())
        .filter(|word| confident_identifier(word))
        .collect();
    let scope_keys: Vec<HashSet<String>> =
        scopes.iter().map(|scope| tokenize(&scope.key)).collect();
    let overlaps_scope = |word: &str| {
        let word_tokens = tokenize(word);
        scope_keys
            .iter()
            .any(|keys| !keys.is_disjoint(&word_tokens))
    };
    backticked
        .iter()
        .copied()
        .rev()
        .find(|word| overlaps_scope(word))
        .or_else(|| bare.iter().copied().rev().find(|word| overlaps_scope(word)))
        .or_else(|| backticked.last().copied())
        .or_else(|| bare.last().copied())
        .map(str::to_string)
}

/// A bare fallback subject must look like a code identifier: CamelCase with at
/// least two humps (PaymentService, CreditLedger), containing `_`, or
/// `::`-qualified (billing::Ledger). It must contain at least one lowercase
/// letter, which rejects ticket and version noise such as JIRA_1234 or
/// Q3_2026, and it must not be a stoplisted English sentence-starter such as
/// "No" or "This".
fn confident_identifier(word: &str) -> bool {
    if SUBJECT_STOPLIST.contains(&word.to_ascii_lowercase().as_str()) {
        return false;
    }
    if !word.chars().any(|character| character.is_ascii_lowercase()) {
        return false;
    }
    word.contains("::") || word.contains('_') || camel_humps(word) >= 2
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

    /// The overlapping scope on whichever side has a migration-ordered kind
    /// (`schema`, `migration`, `config`), independent of which intent is the
    /// source. When both sides qualify, the canonically smaller scope wins so
    /// the advice reads identically in either publish order.
    fn migration_scope(&self) -> Option<&Scope> {
        let source_qualifies = MIGRATION_KINDS.contains(&self.source.kind.as_str());
        let target_qualifies = MIGRATION_KINDS.contains(&self.target.kind.as_str());
        match (source_qualifies, target_qualifies) {
            (true, true) => {
                if self.source.canonical() <= self.target.canonical() {
                    Some(&self.source)
                } else {
                    Some(&self.target)
                }
            }
            (true, false) => Some(&self.source),
            (false, true) => Some(&self.target),
            (false, false) => None,
        }
    }
}

/// Compare every explicit scope pair and keep the best tier, so an exact
/// canonical match is never shadowed by an earlier-listed weak token overlap.
fn scope_overlap(left: &[Scope], right: &[Scope]) -> Option<ScopeOverlap> {
    let mut best: Option<ScopeOverlap> = None;
    for first in left {
        for second in right {
            let candidate = if first.canonical() == second.canonical() {
                Some(ScopeOverlap {
                    source: first.clone(),
                    target: second.clone(),
                    score: 1.0,
                    reason: "exact semantic scope".to_string(),
                })
            } else if first.key.eq_ignore_ascii_case(&second.key) {
                Some(ScopeOverlap {
                    source: first.clone(),
                    target: second.clone(),
                    score: 0.9,
                    reason: "same semantic key across scope kinds".to_string(),
                })
            } else {
                let similarity = jaccard(&tokenize(&first.key), &tokenize(&second.key));
                (similarity >= 0.66).then(|| ScopeOverlap {
                    source: first.clone(),
                    target: second.clone(),
                    score: 0.65 + similarity * 0.2,
                    reason: "overlapping semantic scope tokens".to_string(),
                })
            };
            if let Some(candidate) = candidate {
                if best
                    .as_ref()
                    .is_none_or(|current| candidate.score > current.score)
                {
                    best = Some(candidate);
                }
            }
        }
    }
    best
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

/// `scope` must be the destructive side's overlapping scope, so the fallback
/// abstraction is named after the thing actually being destructively changed.
fn provider_suggestion(scope: &Scope, destructive: &Inference, additive: &Inference) -> String {
    let key = destructive.subject.as_deref().unwrap_or(scope.key.as_str());
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
    let source_inference = infer(&source.summary, &source.scopes);
    let target_inference = infer(&target.summary, &target.scopes);
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
            let (destructive, additive, destructive_scope) =
                if source_inference.operation.destructive() {
                    (&source_inference, &target_inference, &overlap.source)
                } else {
                    (&target_inference, &source_inference, &overlap.target)
                };
            let score =
                (scope_score * destructive.confidence.min(additive.confidence) * 1.02).min(0.99);
            let suggestion = if let Some(migration) = overlap.migration_scope() {
                migration_order_suggestion(migration)
            } else {
                provider_suggestion(destructive_scope, destructive, additive)
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
                    destructive
                        .subject
                        .as_deref()
                        .unwrap_or(&destructive_scope.key),
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
            let suggestion = if let Some(migration) = overlap.migration_scope() {
                migration_order_suggestion(migration)
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
                "Coordinate before publishing ChangeSets: exchange the proposed contract and dependency order. Claims remain advisory and both agents may continue.".to_string(),
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
    fn destructive_keywords_win_over_earlier_additive_words() {
        let inference = infer("Add promotional credits, then migrate callers", &[]);
        assert_eq!(inference.operation, Operation::Migrate);
        assert!(inference.operation.destructive());
    }

    #[test]
    fn additive_phrasing_of_a_destructive_migration_still_gates() {
        let drop_column = candidate(
            "a",
            "Add a migration to drop the legacy users.email column",
            &["schema:users.email"],
        );
        let extend = candidate(
            "b",
            "Extend users.email validation with stricter normalization",
            &["schema:users.email"],
        );
        let conflicts = detect_pair(&drop_column, &extend);
        assert_eq!(conflicts[0].kind, "replace_vs_extend");
        assert_eq!(conflicts[0].severity, "HIGH");
        assert_eq!(conflicts[0].evidence["rule"], "FM-C001");
        assert!(conflicts[0].suggestion.contains("migration order"));
        assert!(!conflicts[0].suggestion.contains("Provider"));
    }

    #[test]
    fn add_support_phrasing_classifies_extend() {
        let inference = infer("Add support for exporting invoices as CSV", &[]);
        assert_eq!(inference.operation, Operation::Extend);
    }

    #[test]
    fn camel_case_identifiers_require_two_humps_or_backticks() {
        assert_eq!(
            infer("Update PaymentService for the new flow", &[])
                .subject
                .as_deref(),
            Some("PaymentService")
        );
        assert_eq!(
            infer("No schema changes; behavior preserved.", &[]).subject,
            None
        );
        assert_eq!(
            infer("Update the `Invoice` model rendering", &[])
                .subject
                .as_deref(),
            Some("Invoice")
        );
        assert_eq!(
            infer("Then update the invoice rendering", &[]).subject,
            None
        );
    }

    #[test]
    fn uppercase_noise_tokens_are_not_subjects() {
        assert_eq!(
            infer(
                "Migrate rate limiting into the new GatewayService (tracked in JIRA_1234)",
                &[]
            )
            .subject
            .as_deref(),
            Some("GatewayService")
        );
        assert_eq!(
            infer("Ship the Q3_2026 rollout checklist", &[]).subject,
            None
        );
    }

    #[test]
    fn subject_prefers_a_declared_scope_key() {
        let scopes = vec![Scope::parse("symbol:CreditLedger").unwrap()];
        assert_eq!(
            infer("Fix JIRA_1234 by extending CreditLedger", &scopes)
                .subject
                .as_deref(),
            Some("CreditLedger")
        );
        assert_eq!(
            infer(
                "Extend CreditLedger and refresh BillingReport styling",
                &scopes
            )
            .subject
            .as_deref(),
            Some("CreditLedger")
        );
    }

    #[test]
    fn backticked_lowercase_identifiers_are_subjects() {
        assert_eq!(
            infer("Update the `invoice_totals` rollup logic", &[])
                .subject
                .as_deref(),
            Some("invoice_totals")
        );
    }

    #[test]
    fn module_qualified_tokens_are_subjects() {
        assert_eq!(
            infer("Rework billing::Ledger rounding", &[])
                .subject
                .as_deref(),
            Some("billing::Ledger")
        );
    }

    #[test]
    fn migration_advice_is_publish_order_independent() {
        let schema_side = candidate(
            "a",
            "Centralize credit ledger arithmetic and migrate callers",
            &["schema:credit_ledger"],
        );
        let symbol_side = candidate(
            "b",
            "Extend the credit ledger with promotional credits",
            &["symbol:CreditLedger"],
        );
        let forward = detect_pair(&schema_side, &symbol_side);
        let reverse = detect_pair(&symbol_side, &schema_side);
        assert_eq!(forward[0].kind, "replace_vs_extend");
        assert_eq!(reverse[0].kind, "replace_vs_extend");
        assert!(forward[0].suggestion.contains("migration order"));
        assert_eq!(forward[0].suggestion, reverse[0].suggestion);
        assert!(!reverse[0].suggestion.contains("Provider"));
    }

    #[test]
    fn fallback_explanation_names_the_destructive_side_scope() {
        let extend = candidate(
            "a",
            "Extend the credit ledger with promotional credits",
            &["symbol:CreditLedger"],
        );
        let migrate = candidate(
            "b",
            "Centralize credit ledger arithmetic into a new service and migrate callers. No schema changes; behavior preserved.",
            &["symbol:CreditLedgerService"],
        );
        let conflicts = detect_pair(&extend, &migrate);
        assert_eq!(conflicts[0].kind, "replace_vs_extend");
        assert!(
            conflicts[0]
                .explanation
                .contains("will migrate `CreditLedgerService`")
        );
        assert!(conflicts[0].suggestion.contains("CreditLedgerProvider"));
    }

    #[test]
    fn exact_scope_match_beats_earlier_token_overlap() {
        let left = vec![
            Scope::parse("symbol:CreditLedgerService").unwrap(),
            Scope::parse("schema:users.email").unwrap(),
        ];
        let right = vec![
            Scope::parse("symbol:CreditLedger").unwrap(),
            Scope::parse("schema:users.email").unwrap(),
        ];
        let overlap = scope_overlap(&left, &right).expect("overlap");
        assert_eq!(overlap.score, 1.0);
        assert_eq!(overlap.reason, "exact semantic scope");
        assert_eq!(overlap.source.canonical(), "schema:users.email");
        assert_eq!(overlap.target.canonical(), "schema:users.email");
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
