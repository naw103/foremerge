# ADR 0001: Digest-bound validation exclusions

- Status: Accepted
- Date: 2026-08-22
- Applies from: Foremerge 0.3.0

## Context

Foremerge fingerprints committed, tracked-dirty, and untracked worktree content so
validation cannot silently bless a different candidate. Some legitimate checks
create disposable output such as `coverage.log`, JUnit XML, or a report directory.
Before 0.3.0, that output changed the post-command fingerprint: the completed test
was correctly rejected as stale, but retrying required cleaning the artifact and
the otherwise useful run was not represented as authoritative evidence.

Using `.gitignore` as validation policy is unsuitable. It is a tracked project
file, so changing it changes the candidate, and it mixes product ignore policy
with coordinator trust policy. Ignoring newly discovered paths automatically
would create a quiet false-positive validation boundary.

## Decision

Foremerge supports a versioned repository-private ruleset at:

```text
<git-common-dir>/foremerge/validation-exclusions.json
```

Only a trusted operator may replace it, using the CLI:

```console
foremerge validation-exclusions set \
  --path coverage.log \
  --path target/validation-reports/
foremerge --json validation-exclusions show
```

Rules are repository-relative exact paths or directory prefixes ending in `/`.
There are no globs, negation, environment expansion, or absolute paths. Rules are
normalized, sorted, deduplicated, bounded, and serialized with a format version.

The following invariants are mandatory:

1. Exclusions apply only to Git status `??` paths. Tracked, staged, modified,
   renamed, copied, or deleted content is never excludable.
2. The SHA-256 digest of the normalized ruleset is part of every snapshot
   fingerprint. Changing policy invalidates an already-published ChangeSet.
3. Publication, pre-validation, post-validation, and acceptance all use the same
   repository-private policy location and fingerprint algorithm.
4. ChangeSet provenance records the ruleset digest, changed paths, excluded
   untracked paths, and whether symbol inference was truncated. Every validation
   attempt records its observed paths and ruleset digest independently.
5. There is no MCP mutation tool. Agents may observe the resulting provenance but
   cannot widen their own validation boundary.
6. Acceptance still calls Git's strict clean-worktree gate. Generated excluded
   files must be removed before acceptance. Removing them does not change the
   fingerprint because their content and presence were deliberately excluded.
7. The config must be a bounded regular file; symlinks and rules that escape the
   worktree or target `.git` are rejected. Writes are atomic and private.

## Consequences

Validation tools can generate explicitly approved disposable output without
invalidating an otherwise identical candidate. Policy changes remain visible and
fail closed because their digest changes the candidate fingerprint. Operators
must keep the list narrow and review it like verification configuration.

Every completed command is also retained in the immutable `validation_attempts`
audit table. A fingerprint-changing run remains non-authoritative and cannot gate
acceptance, even when it passed; its diagnostic names newly changed and removed
paths so the operator can decide whether to clean, revise, or add a narrowly scoped
rule before publishing a new ChangeSet revision.

## Alternatives considered

### Disposable validation worktrees

Running every check in a materialized disposable worktree gives the strongest
isolation and remains the preferred long-term direction. It is deferred because
Foremerge currently supports dirty and untracked provisional candidates whose
exact state cannot be reproduced by checking out one commit alone. A correct
implementation needs a content-addressed snapshot materializer and explicit
sandbox/resource policy.

### Documentation and diagnostics only

Retaining stale attempts and naming generated paths fixes lost provenance but
leaves common checks operationally awkward. It is useful defense in depth and is
implemented, but does not replace an explicit, digest-bound policy.

### Narrow the fingerprint to known paths

Rejected. A validation or concurrent process could create a brand-new untracked
file outside the recorded set and Foremerge would falsely label an untested tree
as validated. Full discovery remains the default; exclusions are explicit policy.
