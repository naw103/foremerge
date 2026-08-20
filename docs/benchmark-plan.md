# Benchmark plan

Foremerge's thesis is testable: semantic coordination should help autonomous
coding agents discover incompatible intent and duplicate work earlier, preserve
better provenance, and prevent unverified integration without replacing Git.
This document defines how to test that thesis. It does not report results.

## Questions and hypotheses

The benchmark answers four primary questions:

1. Does coordination move conflict detection earlier than Git diff, merge, or
   test failure?
2. Does it reduce work discarded because another agent did the same task or
   invalidated its design?
3. Does verification gating reduce failed changes reaching the target branch?
4. Does it produce more complete, queryable provenance at acceptable overhead?

The primary comparison is paired:

- **Git-only:** agents use isolated Git worktrees and the same normal coding
  tools, but no Foremerge tools or coordination events.
- **Coordinated:** agents receive the same task, worktree isolation, budget, and
  coding tools, plus Foremerge. They publish intent, make semantic claims, check
  conflicts, coordinate, publish a ChangeSet, and record validation.

Git worktrees are used in both conditions. This isolates the value of semantic
coordination from the value of filesystem isolation.

## Corpus

Versioned scenario specifications live in `benchmarks/scenarios`. The initial
corpus covers:

| Scenario | Expected signal |
| --- | --- |
| Payment provider replacement versus extension | Architectural intent conflict before implementation |
| Two agents implementing the same retry policy | Duplicate-work warning |
| Schema rename versus an index on the old column | Schema/dependency conflict |
| Independent documentation and caching work | Negative control; no high-severity warning |
| Candidate with a failing validation command | Integration remains unchanged |

The fixture is the ground-truth specification, not evidence that a detector
already meets it. New conflict fixtures must be reviewed by at least two people
without knowing which detector output they are labeling. Disagreements are
recorded rather than silently resolved.

After the synthetic corpus is stable, add tasks sampled from public repositories.
Record the upstream commit and license for every imported fixture. Never evaluate
against a moving branch.

## Experimental controls

For each paired run, hold constant:

- repository seed commit and starting tests;
- agent system prompt, task prompt, model/provider/version, and temperature;
- number of agents and their worktree starting refs;
- wall-clock, token, tool-call, and retry budgets;
- machine class, operating system, and network policy;
- validation commands; and
- success criteria and human grading rubric.

Use fresh worktrees and a fresh Foremerge database for every repetition. Randomize
which condition runs first. Preserve raw prompts, responses, Git refs, command
logs, test results, coordinator events, and timing data, subject to secret
redaction.

Run at least 20 paired repetitions per scenario and model configuration before
making comparative productivity claims. A smaller run may be labeled a smoke
test or pilot, never a benchmark result.

## Instrumentation

Use monotonic time for durations and UTC timestamps for cross-process correlation.
The runner should record these milestones independently of agent self-report:

1. task issued;
2. intent published;
3. claim published;
4. conflict first surfaced;
5. first worktree diff;
6. first ChangeSet published;
7. validation started and completed;
8. target ref changed, if it changed; and
9. task ended, was re-scoped, or was discarded.

Event sequence numbers establish ordering inside Foremerge. Git tree and ref
checks establish whether code existed or integration occurred. Do not infer those
facts from prose summaries.

## Metrics

### Primary metrics

- **Early-detection rate:** conflict runs where the first correct alert precedes
  the first code diff, divided by all ground-truth conflict runs.
- **Detection lead time:** first incompatible-code milestone minus first correct
  alert time. Also report lifecycle phases saved; timing alone is noisy.
- **Duplicate effort:** agent-minutes and changed lines discarded because another
  agent completed substantially the same task.
- **Unsafe integration rate:** target-ref changes whose required validation was
  missing or failing, divided by all integration attempts.
- **Successful integration rate:** candidates accepted into the target ref with
  all declared checks passing and the final target tests green.
- **Provenance completeness:** populated required provenance fields divided by
  required fields, measured from persisted records rather than the final answer.

### Quality and cost metrics

- conflict precision, recall, and false-positive rate against reviewed labels;
- semantic conflicts discovered only by post-merge tests or human review;
- merge conflicts and manual conflict-resolution time;
- task wall time, model tokens, tool calls, and retries;
- coordinator operation latency at p50, p95, and maximum; and
- database size and events per completed task.

Report both medians and individual distributions. One fast or unusually slow run
must not determine the conclusion.

## Mechanism acceptance checks

These deterministic checks gate the benchmark harness itself:

- The PaymentService fixture warns before either agent enters implementation and
  recommends a shared provider-style abstraction.
- The duplicate-work fixture surfaces a duplicate warning before both agents
  publish ChangeSets.
- The negative-control fixture produces no high-severity conflict.
- A failed validation leaves the target Git ref byte-for-byte unchanged.
- The coordinated run records agent/model, task and intent, semantic scopes,
  decisions, validation result, status, and Git ref where applicable.
- Concurrent agents use distinct worktrees and no events are lost.

These checks demonstrate product mechanics. They are not substitutes for the
paired productivity study.

## Analysis

Publish raw, redacted run records and the analysis script with every report. Use
paired differences for time and effort metrics. Report median paired difference
with a bootstrap 95% confidence interval; report a confidence interval for rate
metrics as well. Include all excluded runs and the preregistered exclusion reason.

Break results down by scenario and model. An aggregate improvement must not hide
a high false-positive rate or a regression on independent tasks. Treat human
adjudication and model output as separate sources of uncertainty.

## Result artifact

Each published result set should contain:

```text
benchmarks/results/<date>-<runner-version>/
├── manifest.json       # source commit, runner commit, environment and budgets
├── runs.jsonl          # one normalized record per run
├── events/             # redacted Foremerge event exports
├── git/                # starting and ending refs plus diff statistics
├── analysis.json       # computed metrics
└── report.md           # methods, results, limitations and exact claim language
```

Do not commit credentials, unredacted prompts, proprietary code, or a database
containing them. A report must say whether agents were simulated, scripted, or
model-driven.

## Reproduction status

The JSON scenarios currently define the initial corpus. Until a versioned runner
and raw result directory are committed, the repository has a benchmark plan but
no comparative benchmark result. Launch material must preserve that distinction.
