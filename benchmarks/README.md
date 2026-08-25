# Foremerge benchmark corpus

This directory contains reviewable scenario specifications for comparing
Git-worktree-only agents with agents using Foremerge. The JSON files are ground
truth and experimental inputs. They are not benchmark measurements.

All five fixtures are executable correctness cases:

```sh
make benchmarks
# equivalent: cargo test --test benchmark_scenarios -- --nocapture
```

The first four run the actual deterministic detector; the fifth builds a real
temporary Git repository and proves a failed validation cannot create an
accepted ref. CI executes the complete corpus on Linux, macOS, and Windows.

The separate query harness makes local timing claims reproducible without
turning one machine's output into a published benchmark result:

```sh
make query-benchmark
# custom scales:
cargo run --release --example query_benchmark -- 500 5000 20000
```

It emits newline-delimited JSON with seed time and median timings for unfiltered,
agent/status-filtered, semantic-scope-hit, and semantic-scope-miss queries.

The full method, metrics, controls, and reporting requirements are in
[`docs/benchmark-plan.md`](../docs/benchmark-plan.md).

## Scenario format

Each file in `scenarios/` contains:

- `schema_version`: version of this fixture shape (2 declares scope operations);
- `id`, `title`, and `category`: stable identifiers and grouping;
- `description`: the coordination problem under test;
- `agents`: task, intent, and typed semantic scopes supplied to each agent,
  each scope carrying the operation that agent performs on it;
- `ground_truth`: reviewer-defined expected warning behavior and why; and
- `success_criteria`: observable conditions, not subjective marketing claims.

Scopes use the same `kind`, `key` and `operation` representation as the
Foremerge protocol. Declaring the operation is what makes a fixture test the
detector rather than the phrasing of its prose.
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
6. Validate JSON syntax and run the executable benchmark corpus.

Do not tune a detector only against these public fixtures. Hold back evaluation
cases when measuring precision and recall.

## Results policy

Result artifacts belong under `results/<date>-<runner-version>/` with the manifest
defined in the benchmark plan. A valid report states whether agents were
scripted, simulated, or model-driven and identifies every model and source commit.

Do not describe fixture success as proof of reduced engineering time. Comparative
claims require paired repetitions, raw results, and the analysis described in the
benchmark plan.
