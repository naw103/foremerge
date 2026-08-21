# Limitations and trust model

Foremerge is pre-1.0 local coordination software. It makes concurrent agent work
more visible; it does not make untrusted code, commands, agents, or repositories
safe. This document is intentionally direct about the boundary.

## Conflict detection is advisory

Intent and duplicate detection use deterministic semantic scopes and textual
signals. They can miss incompatible plans, especially when agents use different
names for the same concept, and they can warn on work that is actually compatible.
A warning is evidence to coordinate, not a proof that either task is wrong.

Foremerge does not currently provide whole-program semantic analysis. Manual
scopes remain important for APIs, database schemas, configuration, infrastructure,
migrations, tests, and environment variables. Human review and tests remain
authoritative.

Conflict detection also runs when the *later* intent publishes, and there are
no push notifications: an earlier publisher's own publish response
legitimately reported no conflicts. `start_work` and `publish_changeset`
responses include an `open_conflicts` snapshot, but between those boundaries
an agent only learns of new conflicts by re-running `check_conflicts` (or
reading its inbox). Treat an empty conflicts array as "none known yet", not
"none will exist".

## Claims are not locks

Two agents may claim the same scope. Foremerge records the overlap and returns an
advisory warning so the agents can coordinate. It does not serialize work or
prevent an agent from continuing. Lease expiry reduces stale ownership but does
not prove that an agent stopped changing code.

## Local mode is not distributed consensus

The SQLite database is designed for coordination through a local daemon and for
visibility across worktrees belonging to the same Git repository. It does not
replicate across machines, resolve network partitions, or provide multi-host
consensus. Do not put the database on an ad hoc network filesystem and infer
distributed safety from SQLite locking.

The event hash chain makes accidental changes and simple tampering detectable. It
does not make the log immutable against a local user who can replace the database
and recompute the chain.

## Git remains responsible for code history

Foremerge records coordination state and Git refs; Git owns commits, branches,
merges, and repository durability. Accepting a ChangeSet is not the same as
merging, cherry-picking, pushing, or satisfying a hosting provider's protection
rules. Operators must use normal Git review and integration workflows.

The integration commit recorded by `record_commit` is caller-attested. Foremerge
verifies that the recorded commit contains the pinned accepted commit in its
ancestry; it does not verify that integration actually reached any particular
branch or remote. Because ancestry is reflexive, a fast-forward integration
legitimately records `integration_commit == accepted_commit`, and a dishonest or
confused caller can record a commit that never landed anywhere. Treat the
recorded integration commit as provenance to audit against Git hosting state,
not as proof of landing.

Local coordination data stored under Git's common directory is not included in a
normal clone or push. Until export/import or Git-hosted provenance support lands,
back up that state separately if its history matters beyond the machine.

## Validation commands are trusted code

Validation runs with the same operating-system permissions as Foremerge. There
is no command sandbox, container boundary, resource governor, or guarantee that a
test is hermetic. Only run commands from agents and repositories you trust.
Passing Foremerge-executed validation is a gate; agent-reported tests are
provenance only. A passing command is not proof that the chosen validation was
complete or that the software has no defects.

## Provenance is only as accurate as its inputs

Agent identity, model, prompt, decisions, summaries, dependencies, and test
evidence may be supplied by the agent. Foremerge preserves these records but
cannot independently prove every statement. Git refs, process exit status, event
ordering, and database integrity checks are stronger evidence than prose.

Prompts, command output, decisions, and intent summaries may contain secrets or
proprietary source. The local state is not automatically redacted. Inspect and
sanitize it before sharing benchmark data or exports.

## Network exposure

Loopback is the only bind mode accepted by the MVP CLI. Removing that guard or
embedding the API on a non-loopback listener can expose repository intent,
paths, validation output, and coordination controls; doing so requires an
explicit authentication, encryption, and network-isolation plan. MCP stdio
clients inherit the permissions of the process that launches them.

## Scale and portability

The MVP targets small, local agent teams and repository-sized event histories.
Large-scale latency, long-running retention, and hundreds of concurrent writers
have not been established until published benchmark artifacts say otherwise.
Worktree paths and Git status entries are represented in the public protocol as
UTF-8 strings; repositories that depend on non-UTF-8 filenames are not currently
supported.

CI is the authoritative platform-compatibility record. A platform absent from CI
should be treated as unverified. Public CLI, API, MCP, and event schemas may still
change before 1.0; migrations and breaking changes will be called out in the
changelog.

The tagged `0.1.0` schema is the first supported database boundary. SQLite files
created by untagged development snapshots before that release may need their
repository-local `foremerge` runtime directory reset; they are not an upgrade
compatibility promise. Tagged-release schema changes will carry migrations and
changelog guidance.

## What Foremerge does not replace

- code review and architecture ownership;
- tests, static analysis, security scanning, or deployment checks;
- Git hosting permissions and protected branches;
- backups of repositories or coordination state; or
- judgment about whether a suggested abstraction is appropriate.

If a workflow needs any of those controls, keep using it alongside Foremerge.
