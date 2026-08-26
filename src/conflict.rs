use crate::model::{Conflict, Interaction, Operation, Scope, ScopeClaim, ScopeOverlapView};
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

#[derive(Debug, Clone)]
pub struct IntentCandidate {
    pub id: String,
    pub summary: String,
    pub scopes: Vec<ScopeClaim>,
}

#[derive(Debug, Clone)]
struct Inference {
    subject: Option<String>,
    destination: Option<String>,
    added_variant: Option<String>,
}

fn infer(summary: &str, scopes: &[Scope]) -> Inference {
    if let Some(captures) = REPLACE_RE.captures(summary) {
        return Inference {
            subject: captures.get(1).map(|value| value.as_str().to_string()),
            destination: captures.get(2).map(|value| value.as_str().to_string()),
            added_variant: None,
        };
    }
    if let Some(captures) = ADD_SUPPORT_RE.captures(summary) {
        return Inference {
            subject: captures.get(2).map(|value| value.as_str().to_string()),
            destination: None,
            added_variant: captures.get(1).map(|value| value.as_str().to_string()),
        };
    }

    Inference {
        subject: select_subject(summary, scopes),
        destination: None,
        added_variant: None,
    }
}

/// Nouns naming an artefact whose removal or rename never changes the contract
/// of the thing it belongs to. Deleting a test, retiring a feature flag, or
/// dropping a debug counter is not destroying the component the artefact is
/// named after, so a destructive verb governing one of these must not be read
/// as a threat to a shared semantic scope. This list is deliberately small and
/// closed: it names peripheral artefacts, which are a stable category, rather
/// than trying to enumerate destructive verbs, which are not.
const PERIPHERAL_ARTIFACTS: &[&str] = &[
    "test",
    "tests",
    "spec",
    "specs",
    "benchmark",
    "benchmarks",
    "fixture",
    "fixtures",
    "mock",
    "mocks",
    "stub",
    "stubs",
    "flag",
    "flags",
    "counter",
    "counters",
    "metric",
    "metrics",
    "log",
    "logs",
    "logging",
    "comment",
    "comments",
    "docstring",
    "todo",
    "variable",
    "variables",
    "import",
    "imports",
    "dead",
    "unused",
    "lint",
    "typo",
    "whitespace",
];

/// Words that end the phrase a verb governs. Everything after one of these
/// describes where the work happens, not what is being changed, so
/// `drop the unused debug counter from ThumbnailCache` governs the counter
/// and merely mentions the cache.
const OBJECT_BOUNDARY: &[&str] = &[
    "in",
    "into",
    "inside",
    "within",
    "from",
    "to",
    "onto",
    "on",
    "at",
    "for",
    "with",
    "by",
    "under",
    "behind",
    "across",
    "over",
    "around",
    "guarding",
    "protecting",
    "covering",
    "so",
    "then",
    "and",
];

/// Words that name the same entity as the identifier immediately before them,
/// so `the users.email column` and `the PaymentService class` still refer to
/// the scope itself rather than to something merely named after it.
const TRANSPARENT_HEAD: &[&str] = &[
    "class",
    "service",
    "module",
    "struct",
    "type",
    "interface",
    "component",
    "implementation",
    "impl",
    "object",
    "model",
    "entity",
    "record",
    "table",
    "column",
    "field",
    "api",
    "endpoint",
    "abstraction",
    "layer",
];

/// Words that carry no noun of their own and so never end a governed phrase.
const PHRASE_FILLER: &[&str] = &[
    "a",
    "an",
    "the",
    "its",
    "their",
    "our",
    "all",
    "any",
    "some",
    "this",
    "that",
    "these",
    "those",
    "legacy",
    "old",
    "new",
    "existing",
    "current",
    "obsolete",
    "flaky",
    "confusing",
    "local",
    "debug",
];

/// The phrase a destructive verb governs: everything after the first
/// destructive keyword, stopping at a clause boundary or punctuation. Returns
/// words in their original case so scope identifiers stay comparable.
fn governed_phrase(summary: &str, operation: Operation) -> Option<Vec<String>> {
    if !operation.destructive() {
        return None;
    }
    let keywords = OPERATION_BUCKETS
        .iter()
        .find(|(bucket, _)| *bucket == operation)
        .map(|(_, keywords)| *keywords)?;

    let trim = |word: &str| {
        word.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric()
                && character != '.'
                && character != '_'
                && character != ':'
        })
        .to_string()
    };

    let mut phrase = Vec::new();
    let mut seen_verb = false;
    for raw in summary.split_whitespace() {
        let word = trim(raw);
        if word.is_empty() {
            continue;
        }
        let lowered = word.to_ascii_lowercase();
        if !seen_verb {
            seen_verb = keywords.contains(&lowered.as_str());
            continue;
        }
        if OBJECT_BOUNDARY.contains(&lowered.as_str()) {
            break;
        }
        phrase.push(word);
        // A sentence or clause ending closes the phrase.
        if raw.ends_with([',', ';', '.', ')']) {
            break;
        }
    }
    if !seen_verb || phrase.is_empty() {
        return None;
    }
    Some(phrase)
}

/// Infer an operation from prose, for CLI callers who did not declare one.
///
/// A destructive reading is withdrawn when the verb governs only a peripheral
/// artefact, or when the scope is merely a modifier inside the phrase it
/// governs: "Delete the flaky ThumbnailCache benchmark test" destroys a test,
/// not the cache. Such an intent still changes the scope, so it degrades to
/// `Modify` rather than disappearing.
pub fn infer_operation(summary: &str, scope: &Scope) -> Option<Operation> {
    let operation = classify_operation(summary)?;
    if operation.destructive()
        && (destructive_object_is_peripheral(summary, operation)
            || scope_is_modifier_in_object(summary, operation, scope))
    {
        return Some(Operation::Modify);
    }
    Some(operation)
}

/// Does the destructive verb govern only a peripheral artefact?
///
/// `Delete the flaky ThumbnailCache benchmark test` destroys a test, not the
/// cache. Used to hold back the HIGH replace-versus-extend finding without
/// silencing the pair: the caller still reports the overlap at MEDIUM.
fn destructive_object_is_peripheral(summary: &str, operation: Operation) -> bool {
    let Some(phrase) = governed_phrase(summary, operation) else {
        return false;
    };
    phrase
        .iter()
        .any(|word| PERIPHERAL_ARTIFACTS.contains(&word.to_ascii_lowercase().as_str()))
}

/// Is the shared scope only a modifier inside the governed phrase?
///
/// `Move the ThumbnailCache eviction loop into a background task` governs the
/// loop; `ThumbnailCache` qualifies which loop. The scope counts as the real
/// object only when nothing but transparent head nouns follows it.
fn scope_is_modifier_in_object(summary: &str, operation: Operation, scope: &Scope) -> bool {
    let Some(phrase) = governed_phrase(summary, operation) else {
        return false;
    };
    let scope_tokens = tokenize(&scope.key);
    let Some(position) = phrase.iter().rposition(|word| {
        // Require the word to carry the whole key. A partial token hit
        // ("payment" against `PaymentService`) is a topical match, not a
        // naming of the scope, and must not suppress anything.
        !scope_tokens.is_empty() && scope_tokens.is_subset(&tokenize(word))
    }) else {
        // The scope is not inside the governed phrase at all, so this rule
        // does not apply and must not suppress anything.
        return false;
    };
    phrase[position + 1..].iter().any(|word| {
        let lowered = word.to_ascii_lowercase();
        !TRANSPARENT_HEAD.contains(&lowered.as_str()) && !PHRASE_FILLER.contains(&lowered.as_str())
    })
}

/// Operation buckets in destructive-priority order: a destructive keyword
/// anywhere in the summary outranks additive phrasing around it, so "Add a
/// migration to drop the legacy users.email column" classifies as remove.
/// This deliberately errs toward higher severity, because under-flagging
/// destructive work is strictly worse than a cosmetic operation label.
const OPERATION_BUCKETS: &[(Operation, &[&str])] = &[
    (
        Operation::Replace,
        &[
            "replace",
            "rewrite",
            "supersede",
            "swap",
            "consolidate",
            "unify",
            "standardize",
            "standardise",
            "collapse",
            "reimplement",
            "overhaul",
        ],
    ),
    (
        Operation::Remove,
        &[
            "remove",
            "delete",
            "drop",
            "retire",
            "deprecate",
            "sunset",
            "decommission",
            "eliminate",
        ],
    ),
    (Operation::Rename, &["rename", "move"]),
    (Operation::Migrate, &["migrate", "convert"]),
    (
        Operation::Extend,
        &[
            "extend",
            "augment",
            "back",
            "wire",
            "plug",
            "teach",
            "expose",
            "enable",
            "offer",
            "accept",
            "allow",
            "integrate",
            "hook",
        ],
    ),
    (Operation::Add, &["add", "introduce", "implement", "create"]),
    (
        Operation::Modify,
        &["modify", "change", "update", "refactor", "fix"],
    ),
];

fn classify_operation(summary: &str) -> Option<Operation> {
    let lowered = summary.to_lowercase();
    let tokens: HashSet<&str> = lowered
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    for (operation, keywords) in OPERATION_BUCKETS {
        if keywords.iter().any(|keyword| tokens.contains(keyword))
            || (*operation == Operation::Extend && lowered.contains("support"))
        {
            return Some(*operation);
        }
    }
    None
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

/// Every scope both intents declare, paired with what each will do to it.
///
/// Two tiers, and the difference is what Foremerge is entitled to say. An
/// exact canonical match between two *declared* operations is a fact, so the
/// finding is asserted. A fuzzy key match, or an operation inferred from
/// prose, is a candidate: it is surfaced for the calling agent to judge and
/// never asserted, because the ambiguity Foremerge resolved to produce it is
/// exactly the ambiguity it cannot resolve reliably.
#[derive(Debug, Clone)]
pub struct DeclaredOverlap {
    pub source: ScopeClaim,
    pub target: ScopeClaim,
    pub interaction: Interaction,
    pub asserted: bool,
    pub score: f64,
    pub reason: String,
}

impl DeclaredOverlap {
    fn keys_differ(&self) -> bool {
        !self
            .source
            .scope
            .key
            .eq_ignore_ascii_case(&self.target.scope.key)
    }

    fn describe(&self) -> String {
        if self.keys_differ() {
            format!(
                "`{}` overlaps `{}` ({})",
                self.source.scope.key, self.target.scope.key, self.reason
            )
        } else {
            format!("both declare {}", self.reason)
        }
    }

    /// Keyed on the precise scope, not the canonical alias.
    ///
    /// Collapsing on the alias would report only the strongest of several
    /// genuinely distinct overlaps: an intent touching both
    /// `App\Billing\Report::render` and `App\Admin\Report::render` shares one
    /// alias across both, so the second asserted finding would be silently
    /// dropped as a duplicate of the first.
    fn pair_key(&self) -> (String, String) {
        let source = self.source.scope.precise();
        let target = self.target.scope.precise();
        if source <= target {
            (source, target)
        } else {
            (target, source)
        }
    }

    fn migration_scope(&self) -> Option<&Scope> {
        let source_qualifies = MIGRATION_KINDS.contains(&self.source.scope.kind.as_str());
        let target_qualifies = MIGRATION_KINDS.contains(&self.target.scope.kind.as_str());
        match (source_qualifies, target_qualifies) {
            (true, true) => {
                if self.source.scope.canonical() <= self.target.scope.canonical() {
                    Some(&self.source.scope)
                } else {
                    Some(&self.target.scope)
                }
            }
            (true, false) => Some(&self.source.scope),
            (false, true) => Some(&self.target.scope),
            (false, false) => None,
        }
    }
}

/// Pair every declared scope on one side with every declared scope on the
/// other, keeping only pairs that overlap, strongest first.
pub fn declared_overlaps(left: &[ScopeClaim], right: &[ScopeClaim]) -> Vec<DeclaredOverlap> {
    let mut found: Vec<DeclaredOverlap> = Vec::new();
    for first in left {
        for second in right {
            let (score, reason, exact) = if first.scope.precise() == second.scope.precise() {
                (1.0, "the same semantic scope", true)
            } else if first.scope.canonical() == second.scope.canonical() {
                // The reduced form matched but the full one did not, so these
                // are same-named symbols under different namespaces or paths.
                // Worth surfacing, never worth asserting: `severity_for` caps
                // an unasserted overlap below HIGH so it cannot block.
                (
                    0.9,
                    "the same symbol name under a different namespace or path",
                    false,
                )
            } else if first.scope.key.eq_ignore_ascii_case(&second.scope.key) {
                (0.9, "the same semantic key across scope kinds", false)
            } else {
                let similarity = jaccard(&tokenize(&first.scope.key), &tokenize(&second.scope.key));
                if similarity < 0.66 {
                    continue;
                }
                (
                    0.65 + similarity * 0.2,
                    "overlapping semantic scope tokens",
                    false,
                )
            };
            let Some(interaction) = Interaction::classify(first.operation, second.operation) else {
                continue;
            };
            found.push(DeclaredOverlap {
                source: first.clone(),
                target: second.clone(),
                interaction,
                // Only an exact scope match between two declared operations is
                // a fact this detector may state on its own authority.
                asserted: exact && !first.inferred && !second.inferred,
                score,
                reason: reason.to_string(),
            });
        }
    }
    found.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.pair_key().cmp(&b.pair_key()))
    });
    found
}

/// Coordination advice for one overlap. The verdict comes from the declared
/// operations; the prose is read only to name things helpfully, so a bad
/// reading costs a vaguer suggestion rather than a wrong severity.
fn suggestion_for(
    overlap: &DeclaredOverlap,
    source: &IntentCandidate,
    target: &IntentCandidate,
) -> String {
    match overlap.interaction {
        Interaction::DestructiveVsAdditive => {
            if let Some(migration) = overlap.migration_scope() {
                return migration_order_suggestion(migration);
            }
            let (destructive_summary, additive_summary, destructive_scope) =
                if overlap.source.operation.destructive() {
                    (&source.summary, &target.summary, &overlap.source.scope)
                } else {
                    (&target.summary, &source.summary, &overlap.target.scope)
                };
            provider_suggestion(
                destructive_scope,
                &infer(destructive_summary, &[]),
                &infer(additive_summary, &[]),
            )
        }
        Interaction::DivergentRewrite => {
            if let Some(migration) = overlap.migration_scope() {
                migration_order_suggestion(migration)
            } else {
                format!(
                    "Choose one target design for `{}` and record the decision before either agent continues.",
                    overlap.source.scope.key
                )
            }
        }
        Interaction::SharedContract => {
            "Coordinate before publishing ChangeSets: exchange the proposed contract and dependency order. Claims remain advisory and both agents may continue."
                .to_string()
        }
    }
}

fn explain(overlap: &DeclaredOverlap) -> String {
    let source_operation = overlap.source.operation.as_str();
    let target_operation = overlap.target.operation.as_str();
    let key = &overlap.source.scope.key;
    match overlap.interaction {
        Interaction::DestructiveVsAdditive => {
            let (destructive, additive) = if overlap.source.operation.destructive() {
                (source_operation, target_operation)
            } else {
                (target_operation, source_operation)
            };
            format!(
                "One intent will {destructive} `{key}` while the other will {additive} it; {}.",
                overlap.describe()
            )
        }
        Interaction::DivergentRewrite => format!(
            "Both intents will {source_operation} `{key}` ({}), so they point toward different outcomes.",
            overlap.describe()
        ),
        Interaction::SharedContract => format!(
            "Both intents change `{key}` ({}); their implementations may depend on the same contract.",
            overlap.describe()
        ),
    }
}

/// Severity for one overlap.
///
/// An asserted overlap takes the interaction's own severity, because both
/// agents declared what they will do and the collision follows from those
/// declarations. Anything unasserted is capped below HIGH: a fuzzy scope match
/// or a prose-inferred operation is a reason to look, not a reason to stop.
fn severity_for(overlap: &DeclaredOverlap) -> &'static str {
    let declared = overlap.interaction.severity();
    if overlap.asserted || declared != "HIGH" {
        declared
    } else {
        "MEDIUM"
    }
}

pub fn detect_pair(source: &IntentCandidate, target: &IntentCandidate) -> Vec<Conflict> {
    let mut results = Vec::new();
    let overlaps = declared_overlaps(&source.scopes, &target.scopes);

    // Report the strongest interaction per canonical scope pair, so an intent
    // declaring several related scopes does not produce a wall of findings.
    let mut reported: HashSet<(String, String)> = HashSet::new();
    for overlap in &overlaps {
        if !reported.insert(overlap.pair_key()) {
            continue;
        }
        let rule = match overlap.interaction {
            Interaction::DestructiveVsAdditive => "FM-C001",
            Interaction::DivergentRewrite => "FM-C002",
            Interaction::SharedContract => "FM-C003",
        };
        results.push(make_conflict(
            overlap.interaction.as_str(),
            severity_for(overlap),
            (overlap.score * 0.97).min(0.99),
            source,
            target,
            Some(overlap.source.scope.clone()),
            explain(overlap),
            suggestion_for(overlap, source, target),
            json!({
                "rule": rule,
                "source_operation": overlap.source.operation.as_str(),
                "target_operation": overlap.target.operation.as_str(),
                "source_scope": overlap.source.scope.canonical(),
                "target_scope": overlap.target.scope.canonical(),
                "source_operation_inferred": overlap.source.inferred,
                "target_operation_inferred": overlap.target.inferred,
                "asserted": overlap.asserted,
                "overlap": overlap.reason,
                "detected_before_code": true,
            }),
        ));
    }

    // Duplicated work is never asserted: whether two intents are the same work
    // is a judgement about goals, which is the caller's to make.
    let summary_similarity = jaccard(&tokenize(&source.summary), &tokenize(&target.summary));
    let same_operation = overlaps
        .first()
        .is_some_and(|overlap| overlap.source.operation == overlap.target.operation);
    if summary_similarity >= 0.62
        || (same_operation && !overlaps.is_empty() && summary_similarity > 0.42)
    {
        results.push(make_conflict(
            "duplicate_work",
            "MEDIUM",
            (0.55 + summary_similarity * 0.4).min(0.96),
            source,
            target,
            overlaps.first().map(|overlap| overlap.source.scope.clone()),
            "The two intents have substantially similar goals and may be solving the same work twice."
                .to_string(),
            "Coordinate ownership: compare intended outcomes and either split the scope or pick one implementation owner."
                .to_string(),
            json!({
                "rule": "FM-C004",
                "summary_jaccard": summary_similarity,
                "asserted": false,
                "source_scope": overlaps.first().map(|overlap| overlap.source.scope.canonical()),
                "target_scope": overlaps.first().map(|overlap| overlap.target.scope.canonical()),
            }),
        ));
    }
    results
}

/// The factual view of one overlap, for the `related_work` response.
pub fn overlap_views(source: &[ScopeClaim], target: &[ScopeClaim]) -> Vec<ScopeOverlapView> {
    let mut seen = HashSet::new();
    declared_overlaps(source, target)
        .into_iter()
        .filter(|overlap| seen.insert(overlap.pair_key()))
        .map(|overlap| ScopeOverlapView {
            scope: overlap.source.scope.clone(),
            your_operation: overlap.source.operation,
            their_operation: overlap.target.operation,
            interaction: overlap.interaction,
            asserted: overlap.asserted,
        })
        .collect()
}

pub fn claim_overlap_conflict(
    source_intent_id: &str,
    target_intent_id: &str,
    scope: &Scope,
) -> Conflict {
    let source = IntentCandidate {
        id: source_intent_id.to_string(),
        summary: "semantic claim".to_string(),
        scopes: vec![ScopeClaim::new(scope.clone(), Operation::Modify)],
    };
    let target = IntentCandidate {
        id: target_intent_id.to_string(),
        summary: "existing semantic claim".to_string(),
        scopes: vec![ScopeClaim::new(scope.clone(), Operation::Modify)],
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
        previously_settled: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(value: &str, operation: Operation) -> ScopeClaim {
        ScopeClaim::new(Scope::parse(value).unwrap(), operation)
    }

    fn candidate(id: &str, summary: &str, scopes: Vec<ScopeClaim>) -> IntentCandidate {
        IntentCandidate {
            id: id.to_string(),
            summary: summary.to_string(),
            scopes,
        }
    }

    #[test]
    fn declared_replace_against_declared_extend_is_asserted_high() {
        let replace = candidate(
            "a",
            "Replace PaymentService with StripePaymentService",
            vec![claim("symbol:PaymentService", Operation::Replace)],
        );
        let extend = candidate(
            "b",
            "Add PayPal support to PaymentService",
            vec![claim("symbol:PaymentService", Operation::Extend)],
        );
        let conflicts = detect_pair(&extend, &replace);
        assert_eq!(conflicts[0].severity, "HIGH");
        assert_eq!(conflicts[0].kind, "destructive_vs_additive");
        assert_eq!(conflicts[0].evidence["asserted"], true);
        assert!(conflicts[0].suggestion.contains("PaymentProvider"));
    }

    /// Two same-named classes in different namespaces are unrelated code. The
    /// reduced scope name brings them together so the overlap is still
    /// surfaced, but asserting it would block acceptance on a collision that
    /// does not exist, so it stays advisory.
    #[test]
    fn same_name_under_different_namespaces_warns_without_asserting() {
        let replace = candidate(
            "a",
            "Rewrite billing report rendering",
            vec![claim(
                "symbol:App\\Billing\\Report::render",
                Operation::Replace,
            )],
        );
        let extend = candidate(
            "b",
            "Add a column to the admin report",
            vec![claim(
                "symbol:App\\Admin\\Report::render",
                Operation::Extend,
            )],
        );
        let conflicts = detect_pair(&extend, &replace);
        assert_eq!(
            conflicts[0].evidence["asserted"], false,
            "different namespaces are not an exact scope match"
        );
        assert_eq!(
            conflicts[0].severity, "MEDIUM",
            "an unasserted overlap must be capped below HIGH so it cannot block acceptance"
        );
    }

    /// The reduction still has to do its job: the same symbol written with and
    /// without its namespace is one scope, and an exact match on the full name
    /// is still asserted.
    #[test]
    fn the_same_symbol_written_with_either_separator_is_still_asserted() {
        let replace = candidate(
            "a",
            "Replace the report renderer",
            vec![claim(
                "symbol:App\\Billing\\Report::render",
                Operation::Replace,
            )],
        );
        let extend = candidate(
            "b",
            "Extend the report renderer",
            vec![claim(
                "symbol:app/billing/Report::render",
                Operation::Extend,
            )],
        );
        let conflicts = detect_pair(&extend, &replace);
        assert_eq!(conflicts[0].evidence["asserted"], true);
        assert_eq!(conflicts[0].severity, "HIGH");
    }

    /// The point of the redesign: wording carries no weight once the operation
    /// is declared, so a phrasing no keyword list knows still collides.
    #[test]
    fn paraphrased_wording_does_not_change_a_declared_verdict() {
        let replace = candidate(
            "a",
            "Consolidate all payment handling onto Stripe",
            vec![claim("symbol:PaymentService", Operation::Replace)],
        );
        let extend = candidate(
            "b",
            "Back PaymentService with an additional PayPal gateway",
            vec![claim("symbol:PaymentService", Operation::Extend)],
        );
        let conflicts = detect_pair(&replace, &extend);
        assert_eq!(conflicts[0].severity, "HIGH");
        assert_eq!(conflicts[0].kind, "destructive_vs_additive");
    }

    /// An operation Foremerge guessed is never allowed to stop an agent.
    #[test]
    fn an_inferred_operation_is_capped_below_high() {
        let inferred = candidate(
            "a",
            "Delete the flaky ThumbnailCache benchmark test",
            vec![ScopeClaim::inferred(
                Scope::parse("component:ThumbnailCache").unwrap(),
                Operation::Remove,
            )],
        );
        let extend = candidate(
            "b",
            "Add TTL-based expiry to ThumbnailCache",
            vec![claim("component:ThumbnailCache", Operation::Extend)],
        );
        let conflicts = detect_pair(&inferred, &extend);
        assert_eq!(conflicts[0].severity, "MEDIUM");
        assert_eq!(conflicts[0].evidence["asserted"], false);
    }

    /// A fuzzy scope match is a reason to look, not a reason to stop.
    #[test]
    fn a_fuzzy_scope_match_is_never_asserted() {
        let replace = candidate(
            "a",
            "Replace the ledger service",
            vec![claim("symbol:CreditLedgerService", Operation::Replace)],
        );
        let extend = candidate(
            "b",
            "Extend the credit ledger with promotional credits",
            vec![claim("symbol:CreditLedger", Operation::Extend)],
        );
        let conflicts = detect_pair(&replace, &extend);
        assert_eq!(conflicts[0].kind, "destructive_vs_additive");
        assert_eq!(conflicts[0].severity, "MEDIUM");
        assert_eq!(conflicts[0].evidence["asserted"], false);
    }

    #[test]
    fn independent_scopes_do_not_conflict() {
        let first = candidate(
            "a",
            "Add PayPal support to PaymentService",
            vec![claim("symbol:PaymentService", Operation::Extend)],
        );
        let second = candidate(
            "b",
            "Add avatar upload to UserProfile",
            vec![claim("api:POST /avatar", Operation::Add)],
        );
        assert!(detect_pair(&first, &second).is_empty());
    }

    #[test]
    fn two_destructive_operations_are_a_divergent_rewrite() {
        let first = candidate(
            "a",
            "Rewrite PaymentService on Stripe",
            vec![claim("symbol:PaymentService", Operation::Replace)],
        );
        let second = candidate(
            "b",
            "Rewrite PaymentService on Adyen",
            vec![claim("symbol:PaymentService", Operation::Replace)],
        );
        let conflicts = detect_pair(&first, &second);
        assert_eq!(conflicts[0].kind, "divergent_rewrite");
        assert_eq!(conflicts[0].severity, "HIGH");
    }

    #[test]
    fn two_additive_operations_share_a_contract() {
        let first = candidate(
            "a",
            "Add promotional credits",
            vec![claim("symbol:CreditLedger", Operation::Extend)],
        );
        let second = candidate(
            "b",
            "Add referral credits",
            vec![claim("symbol:CreditLedger", Operation::Extend)],
        );
        let conflicts = detect_pair(&first, &second);
        assert_eq!(conflicts[0].kind, "shared_contract");
        assert_eq!(conflicts[0].severity, "MEDIUM");
    }

    #[test]
    fn a_migration_scope_gets_ordering_advice_rather_than_a_provider() {
        let drop_column = candidate(
            "a",
            "Drop the legacy users.email column",
            vec![claim("schema:users.email", Operation::Remove)],
        );
        let extend = candidate(
            "b",
            "Extend users.email validation with stricter normalization",
            vec![claim("schema:users.email", Operation::Extend)],
        );
        let conflicts = detect_pair(&drop_column, &extend);
        assert_eq!(conflicts[0].kind, "destructive_vs_additive");
        assert_eq!(conflicts[0].severity, "HIGH");
        assert!(conflicts[0].suggestion.contains("migration order"));
        assert!(!conflicts[0].suggestion.contains("Provider"));
    }

    #[test]
    fn similar_work_is_reported_as_duplicate() {
        let first = candidate(
            "a",
            "Add retry support to BillingClient",
            vec![claim("symbol:BillingClient", Operation::Extend)],
        );
        let second = candidate(
            "b",
            "Implement retry support for BillingClient",
            vec![claim("symbol:BillingClient", Operation::Extend)],
        );
        let conflicts = detect_pair(&first, &second);
        let duplicate = conflicts
            .iter()
            .find(|conflict| conflict.kind == "duplicate_work")
            .expect("duplicate_work finding");
        assert_eq!(duplicate.evidence["asserted"], false);
        assert!(duplicate.suggestion.to_lowercase().contains("coordinate"));
    }

    #[test]
    fn overlap_views_state_both_declared_operations() {
        let views = overlap_views(
            &[claim("symbol:PaymentService", Operation::Extend)],
            &[claim("symbol:PaymentService", Operation::Replace)],
        );
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].your_operation, Operation::Extend);
        assert_eq!(views[0].their_operation, Operation::Replace);
        assert_eq!(views[0].interaction, Interaction::DestructiveVsAdditive);
        assert!(views[0].asserted);
    }

    // --- prose inference, which now only serves CLI callers ---

    #[test]
    fn inference_withdraws_a_destructive_reading_of_a_peripheral_artefact() {
        let cache = Scope::parse("component:ThumbnailCache").unwrap();
        assert_eq!(
            infer_operation("Delete the flaky ThumbnailCache benchmark test", &cache),
            Some(Operation::Modify)
        );
        assert_eq!(
            infer_operation(
                "Retire the obsolete feature flag guarding ThumbnailCache",
                &cache
            ),
            Some(Operation::Modify)
        );
    }

    #[test]
    fn inference_withdraws_a_destructive_reading_when_the_scope_is_a_modifier() {
        let cache = Scope::parse("component:ThumbnailCache").unwrap();
        assert_eq!(
            infer_operation(
                "Move the ThumbnailCache eviction loop into a background task",
                &cache
            ),
            Some(Operation::Modify)
        );
    }

    #[test]
    fn inference_keeps_a_destructive_reading_that_governs_the_scope() {
        assert_eq!(
            infer_operation(
                "Add a migration to drop the legacy users.email column",
                &Scope::parse("schema:users.email").unwrap()
            ),
            Some(Operation::Remove)
        );
        assert_eq!(
            infer_operation(
                "Replace PaymentService with StripePaymentService",
                &Scope::parse("symbol:PaymentService").unwrap()
            ),
            Some(Operation::Replace)
        );
    }

    #[test]
    fn inference_returns_nothing_for_wording_it_does_not_know() {
        // "cut over" is ordinary English for a replacement and is not in the
        // vocabulary. Widening the list would only move the boundary, which is
        // why the declared operation exists.
        assert_eq!(
            infer_operation(
                "Cut PaymentService over to Stripe exclusively",
                &Scope::parse("symbol:PaymentService").unwrap()
            ),
            None
        );
    }

    #[test]
    fn subject_selection_still_names_things_for_suggestions() {
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
            infer("Rework billing::Ledger rounding", &[])
                .subject
                .as_deref(),
            Some("billing::Ledger")
        );
        assert_eq!(
            infer("Update the `invoice_totals` rollup logic", &[])
                .subject
                .as_deref(),
            Some("invoice_totals")
        );
        assert_eq!(
            infer(
                "Migrate rate limiting into the new GatewayService (tracked in JIRA_1234)",
                &[]
            )
            .subject
            .as_deref(),
            Some("GatewayService")
        );
    }
}
