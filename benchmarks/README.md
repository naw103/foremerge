# Foremerge benchmark corpus

This directory contains reviewable scenario specifications for comparing
Git-worktree-only agents with agents using Foremerge. The JSON files are ground
truth and experimental inputs. They are not benchmark measurements.

The full method, metrics, controls, and reporting requirements are in
[`docs/benchmark-plan.md`](../docs/benchmark-plan.md).

## Scenario format

Each file in `scenarios/` contains:

- `schema_version`: version of this fixture shape;
- `id`, `title`, and `category`: stable identifiers and grouping;
- `description`: the coordination problem under test;
- `agents`: task, intent, and typed semantic scopes supplied to each agent;
- `ground_truth`: reviewer-defined expected warning behavior and why; and
- `success_criteria`: observable conditions, not subjective marketing claims.

Scopes use the same `kind` and `key` representation as the Foremerge protocol.
Scenario prose intentionally avoids prescribing file names when the conflict is
conceptual.

## Initial scenarios

| File | Purpose |
| --- | --- |
| `01-payment-provider-conflict.json` | Headline replace-versus-extend conflict |
| `02-duplicate-retry-work.json` | Duplicate implementation detection |
| `03-schema-rename-conflict.json` | Cross-task schema dependency conflict |
| `04-independent-negative-control.json` | False-positive control |
| `05-validation-gate.json` | Failed verification must not change target Git ref |

## Adding a scenario

1. Start from a fixed repository commit or a fully generated fixture.
2. Write each task as it would be presented to an agent; do not add hints only to
   the coordinated condition.
3. Label the expected conflict independently with a second reviewer.
4. Include a negative control when introducing a new detector rule.
5. Make every success criterion observable in events, process output, tests, or
   Git refs.
6. Validate JSON syntax and run the benchmark smoke checks once a runner exists.

Do not tune a detector only against these public fixtures. Hold back evaluation
cases when measuring precision and recall.

## Results policy

Result artifacts belong under `results/<date>-<runner-version>/` with the manifest
defined in the benchmark plan. A valid report states whether agents were
scripted, simulated, or model-driven and identifies every model and source commit.

Do not describe fixture success as proof of reduced engineering time. Comparative
claims require paired repetitions, raw results, and the analysis described in the
benchmark plan.
