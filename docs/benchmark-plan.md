# Benchmark plan

This document defines how Foremerge's value is measured. It does not report
results.

Foremerge's thesis is that semantic coordination lets autonomous coding agents
discover incompatible intent and duplicate work earlier, so less work is thrown
away and fewer incompatible designs reach the repository. That is an outcome
claim, and it is what this plan is built to test.

## What we are trying to claim

Three claims, in the form they would be published. Each names what it needs
before it may be stated at all.

### Claim 1: less work is discarded and fewer changes collide

> Across N paired runs of the same tasks, the coordinated arm discarded X% less
> agent work and produced Y% fewer merge conflicts than the uncoordinated arm.

Requires the three-arm design below. Report discarded work and merge conflicts
as separate numbers.

Foremerge does not change Git's merge behaviour. Two agents editing the same
lines still conflict. What changes is whether they chose to do that after
seeing each other's declared intent, so the effect appears first in discarded
and duplicated work and only second in merge conflicts. A reader who assumes
this patches merging will feel misled on discovering it does not, so the
distinction is stated wherever the claim appears.

### Claim 2: fewer conflicts to review

> The coordinated arm produced X fewer conflicts and Y fewer discarded lines per
> 100 tasks.

Deliberately expressed as counts, not hours.

Converting counts to hours needs a human baseline, and time to review a conflict
varies by an order of magnitude across people and across conflicts. An hours
figure would mostly describe whichever people were recruited. Counts let each
reader apply their own cost, and survive being asked how they were derived.

Foremerge may not publish an hours-saved figure without a separate timed human
study, preregistered as such.

### Claim 3: incompatible architectural decisions are caught before code

> Across N paired runs, the uncoordinated arm produced X incompatible-design
> outcomes and the coordinated arm produced Y.

This is the claim Git cannot address at all, and the most distinctive of the
three. It is phrased comparatively because "prevents" asserts a counterfactual.
The paired design supplies the counterfactual directly: the uncoordinated arm
has to actually reach the incompatible outcome for the coordinated arm's
avoidance to mean anything.

Architectural collisions are adjudicated by reviewers against the rubric, not
detected automatically, because automatic detection here would be the system
grading its own homework.

## Why the obvious comparison is not the headline

An earlier version of this plan led with early-detection rate: the share of
conflict runs where the first correct alert precedes the first code diff. That
is not a comparison. The uncoordinated arm has no mechanism to emit an alert
before code exists, so it scores zero by construction, and "100% versus 0%" is
true and worthless.

Early detection is retained below as a mechanism metric. It explains how an
outcome difference arose. It is not evidence that one exists.

## Design: three arms

The comparison is paired, and it has three arms rather than two:

- **A, uncoordinated.** Isolated Git worktrees, ordinary coding tools, no
  Foremerge, no mention of other agents.
- **B, prompted.** Identical to A, plus a prompt stating that another agent is
  working in this repository and that the agent should coordinate carefully.
  No Foremerge.
- **C, coordinated.** Identical to B, plus Foremerge. The agent declares intent
  and scope operations, reads `related_work`, records assessments, publishes a
  ChangeSet, and records validation.

Arm B exists because arm C receives both the tooling and an instruction that
other agents exist. Without B, any benefit is equally explained by the
instruction, and the honest conclusion would be that the product is a prompt.
If B and C are indistinguishable, that is the finding, and it gets published.

Git worktrees are used in all three arms, which isolates the value of semantic
coordination from the value of filesystem isolation.

## What is now uncertain, and what is not

Intent conflict detection compares operations the agent **declares** on scopes
it declares. Given two declarations on one scope, the verdict is deterministic
and needs no study. This closes the question an earlier version of this plan
was mostly built around, namely whether intent could be recovered from prose.
It could not, which is why it is no longer attempted. See
[`conflict-detection.md`](conflict-detection.md).

Two questions took its place, and both are about agent behaviour rather than
about the detector:

1. **Declaration accuracy.** An agent that declares `extend` and then rewrites
   the thing is undetectable by any rule in the system. This failure mode was
   created by the move to declared operations and is now the largest single
   risk to the whole approach.
2. **Engagement.** An agent that receives `related_work` and proceeds without
   assessing it gets no benefit from any of this.

Both are measured inside the paired runs rather than as separate studies.

## Corpus

Versioned scenario specifications live in `benchmarks/scenarios`. Fixtures are
`schema_version` 2 and declare an operation per scope.

The committed corpus is currently weighted toward conflicts, which is backwards.
Concurrent agent pairs in real repositories are overwhelmingly independent, and
a detector's behaviour on independent work is what determines whether people
keep it switched on. Before any comparative result is published the corpus must
contain **at least three independent-work scenarios for every conflict
scenario**, and the realised base rate must be stated alongside every rate
metric.

The corpus must also cover the declare-then-diverge case: an agent that declares
one operation and performs another. No current fixture does.

New conflict fixtures are reviewed by at least two people who do not know which
detector output they are labelling. Disagreements are recorded rather than
silently resolved.

After the synthetic corpus is stable, add tasks sampled from public
repositories, recording the upstream commit and licence for every imported
fixture. Never evaluate against a moving branch.

## Metrics

### Primary, one per claim

- **Discarded work.** Agent-minutes and changed lines abandoned because another
  agent did the same work or invalidated the design. Claim 1 and Claim 2.
- **Merge conflicts.** Count and resolution events when both arms' branches are
  integrated by an identical scripted procedure. Claim 1.
- **Incompatible-design outcomes.** Reviewer-adjudicated, against a rubric
  fixed before the runs. Claim 3.

### Leading indicators

These explain the primary numbers and are recorded in arm C only:

- **Assessment coverage.** Surfaced `related_work` entries assessed via
  `record_assessment`, over all surfaced entries. If this is near zero, a null
  outcome result says nothing about the thesis and everything about the
  plumbing.
- **Assessment outcome mix.** The distribution of verdicts and actions.
- **Declaration accuracy.** Reviewer comparison of each declared scope operation
  against what the resulting diff actually did. This is the metric for risk 1
  above and it cannot be automated.

### Mechanism

- **Detection lead time.** First incompatible-code milestone minus first correct
  alert. Reported with lifecycle phases saved, because timing alone is noisy.
- **Early-detection rate.** Arm C only, reported as a mechanism property and
  never as a comparison.
- **Unsafe integration rate.** Target-ref changes whose required validation was
  missing or failing, over all integration attempts.
- **Successful integration rate.** Candidates accepted with all declared checks
  passing and final target tests green.
- **Provenance completeness.** Populated required provenance fields over
  required fields, measured from persisted records rather than from the agent's
  final answer.

### Cost

A reader's second question is always what this costs. Report:

- prompt tokens added per task by the coordination protocol, including the size
  of `related_work` payloads, which grow with the number of active intents;
- tool calls added per task;
- coordinator operation latency at p50, p95 and maximum;
- database growth and events per completed task; and
- wall-clock overhead per task.

## The runner

None of the above is executable today. The runner is the gating piece of work.

### Responsibilities

1. Materialise a scenario: a fixed seed commit, one fresh Git worktree per
   agent, and for arm C a fresh Foremerge store.
2. Launch each agent with the arm's system prompt, task prompt, model, and
   budgets, holding everything except the arm constant.
3. Record milestones from Git and the Foremerge event log, never from agent
   prose.
4. Integrate both branches at the end by an identical scripted procedure in all
   three arms, so merge-conflict counts are comparable.
5. Emit one normalised record per run.

### Configuration

A run matrix of arm, scenario, model, and repetition index. Randomise arm order
within each repetition. Fresh worktrees and a fresh store for every repetition,
never reused.

### Recorded milestones

Task issued; intent published; scopes claimed; `related_work` returned and its
size; each assessment recorded; conflict first surfaced; first worktree diff;
first ChangeSet published; validation started and completed; target ref changed
if it changed; task ended, was re-scoped, or was discarded.

Event sequence numbers establish ordering inside Foremerge. Git tree and ref
checks establish whether code existed and whether integration occurred.

### Scale

Three arms times five scenarios times twenty repetitions is 300 arm/scenario
executions per model. Each execution runs two agents, so it is 600 agent
invocations, not 300. Quote whichever number a cost or scheduling estimate
actually needs, and say which one it is.

The unit of analysis is the arm/scenario execution, and the design is matched
triplets: the three arms see the same scenario seed, so each scenario
contributes one block of three paired observations rather than three
independent samples.

Twenty repetitions per cell is a preregistered design choice, not a derived
sample size. This plan does not yet carry a power calculation, and until it
does, no claim about which effects the design can or cannot detect belongs
here. Supplying one means fixing, in advance: the outcome model for each
primary measure, the baseline rate or mean and its assumed variance, the test
and its alpha, the correction across the two planned contrasts, and the
resulting minimum detectable effect. Report results as a pilot until that
exists.

Anything smaller is labelled a pilot or a smoke test, never a benchmark result.

## Experimental controls

For each paired run, hold constant: repository seed commit and starting tests;
agent system prompt, task prompt, model, provider, version and temperature;
number of agents and their worktree starting refs; wall-clock, token, tool-call
and retry budgets; machine class, operating system and network policy;
validation commands; the integration procedure; and the grading rubric.

Preserve raw prompts, responses, Git refs, command logs, test results,
coordination events, and timing data, subject to secret redaction.

## Preregistered thresholds

Recorded before the first run, so the study can fail:

- the minimum discarded-work reduction that counts as supporting Claim 1;
- the assessment coverage below which an outcome result is reported as
  inconclusive rather than negative;
- the declaration accuracy below which the declared-operation design is treated
  as unsound and revisited; and
- the exclusion criteria for a run, with every excluded run reported.

## Analysis

Publish raw redacted run records and the analysis script with every report. Use
paired differences for time and effort metrics. Report the median paired
difference with a bootstrap 95% confidence interval, and a confidence interval
for every rate metric.

Report A versus C, and B versus C. The second comparison is the one that says
whether the tooling did anything the prompt did not, and it is published
whichever way it comes out.

Break results down by scenario and by model. An aggregate improvement must not
hide a regression on independent work. Treat human adjudication and model output
as separate sources of uncertainty.

## Mechanism acceptance checks

Deterministic checks gating the harness itself, demonstrating product mechanics
rather than substituting for the study:

- Two declared operations that collide on one declared scope produce an asserted
  finding before either agent enters implementation.
- An operation inferred from prose never produces an asserted finding.
- Compatible work on a shared scope never produces a HIGH finding.
- The duplicate-work fixture surfaces before both agents publish ChangeSets.
- The independent-work fixtures produce no high-severity conflict.
- A failed validation leaves the target Git ref byte-for-byte unchanged.
- The coordinated run records agent, model, task, intent, declared scope
  operations, assessments, decisions, validation result, status, and Git ref.
- Concurrent agents use distinct worktrees and no events are lost.

`cargo test --test benchmark_scenarios` and `tests/paraphrase_probe.rs` execute
these today.

## Result artifact

```text
benchmarks/results/<date>-<runner-version>/
├── manifest.json       # source commit, runner commit, environment, budgets, arms
├── runs.jsonl          # one normalised record per run
├── events/             # redacted Foremerge event exports
├── git/                # starting and ending refs plus diff statistics
├── adjudication/       # reviewer labels, rubric, inter-rater agreement
├── analysis.json       # computed metrics
└── report.md           # methods, results, limitations, exact claim language
```

Never commit credentials, unredacted prompts, proprietary code, or a database
containing them. A report must state whether agents were simulated, scripted, or
model-driven, and must name every model and source commit.

## Reproduction status

`cargo test --test benchmark_scenarios` executes the five JSON fixtures against
the real detector and validation gate. `tests/paraphrase_probe.rs` holds the
detector to its adversarial set. `cargo run --release --example query_benchmark
-- 500 5000 20000` emits reproducible local query timings at declared scales.

These are correctness and microbenchmark harnesses. The three-arm runner
described above does not exist yet.

Until paired raw result directories and an analysis script are published, **the
repository has no comparative productivity result, and none of the three claims
above may be stated in any launch material.**
