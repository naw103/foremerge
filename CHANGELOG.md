# Changelog

All notable changes to Foremerge will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project intends to follow [Semantic Versioning](https://semver.org/) once its
public protocol begins releasing. Before 1.0, minor versions may contain breaking
changes when they are called out here with a migration note.

## [Unreleased]

## [0.4.0] - 2026-08-26

### Changed

- **Breaking.** An intent now declares what it does to each scope. `scopes`
  entries carry an `operation`: `add`, `extend` or `modify`, which preserve
  what other work depends on, or `replace`, `remove`, `rename` or `migrate`,
  which do not. The operation sits on the scope rather than the intent because
  an intent routinely replaces one thing while adding another. Conflict
  detection is then a comparison of two declared operations on one declared
  scope, which is a fact rather than a reading of the summary.

  Detection previously recovered the operation from prose by keyword. That
  cannot work, and the probe added alongside this change measured how badly:
  nine of ten genuine replace-versus-extend conflicts went unreported because
  their phrasing fell outside a 26-word vocabulary, while every one of nine
  compatible pairs raised a HIGH by containing a destructive word somewhere in
  the sentence. "Delete the flaky ThumbnailCache benchmark test" was read as
  destroying `ThumbnailCache`. Widening the vocabulary only moves the boundary,
  and detection needs a known verb on both sides, so coverage was the product
  of two incomplete lists. The agent already knows which operation it means, so
  it now says so. The same ten phrasings all reach the same verdict.

  Migration: MCP and HTTP callers add `"operation"` to each scope object. CLI
  callers write `--scope symbol:PaymentService=replace`. Omitting it at the CLI
  still works, and infers the operation from the summary, but an inferred
  operation never asserts.

- Conflict kinds renamed to match what they describe: `replace_vs_extend` is
  now `destructive_vs_additive`, `divergent_replacement` is
  `divergent_rewrite`, and `shared_semantic_scope` is `shared_contract`.

- Foremerge now distinguishes what it asserts from what it merely surfaces.
  A finding is asserted only when both sides declared an operation on the same
  canonical scope. A fuzzy scope match, or an operation inferred from prose, is
  capped below HIGH and offered as a candidate instead, because the ambiguity
  that had to be resolved to produce it is the ambiguity that cannot be
  resolved reliably. Every finding carries `asserted` in its evidence.

### Added

- Registering an agent now reports when an ACTIVE agent of the same name is
  already registered for the same worktree. Registration deliberately stays
  insert-always, because two genuinely separate processes may share a name and
  silently reusing a record could attach one process to work another still
  owns. What it should not do is hide the duplicate: a dogfood run produced 11
  agent records for 9 logical roles with nothing said about it. The response
  gains a `warnings` entry naming the existing agent and pointing at
  `foremerge work adopt`. Agent fields stay at the top level of the response,
  so callers reading `data.id` are unaffected.

- Test wait bounds scale with `FOREMERGE_TEST_TIMEOUT_SCALE`. Three validation
  tests raced a fixed ten-second deadline and failed on a loaded machine while
  the code was correct, which costs a debugging cycle every time because the
  first hypothesis is always a regression. The bounds are now generous by
  default and raisable, and the failure messages name the variable.
- `publish_intent` returns `related_work` alongside `conflicts`: each other
  active intent that touches this one, with its agent, status, and every
  overlapping scope showing both declared operations and how they interact.
  Foremerge states what overlaps; whether that is a conflict, a duplicate or a
  dependency is a judgement about intent, and the agent doing the work is
  better placed to make it than any rule in the detector.
- `record_assessment` stores that judgement: a verdict of `conflicts`,
  `compatible`, `duplicate` or `depends_on`, a rationale, and the action taken.
  It is available as an MCP tool, as `POST /v1/assessments`, and as
  `foremerge assess record`. `GET /v1/intents/{id}/assessments` and
  `foremerge assess list` read them back. This is better provenance than a
  similarity score, and it is the measurement that matters: whether agents
  engage with what they are shown.
- `tests/paraphrase_probe.rs` runs the same ten pairs through both paths and
  fails the build if a declared verdict ever depends on wording, if compatible
  work reaches HIGH, or if an inferred operation asserts.
- `tests/full_flow_probe.rs` reports everything two agents exchange across
  register, publish, claim and assess, rather than the output of a single
  detector call.


- `foremerge work adopt` transfers an intent whose agent has stopped. An agent
  that died mid-task previously left its intent owned forever by an agent that
  would never return, so the work could only be duplicated. Adoption requires
  the same three things `status` requires before it calls an intent stranded: an
  eligible lifecycle state, no live claim, and an owner that has actually fallen
  silent. No live claim is not on its own evidence that the owner stopped, since
  a freshly published intent has never held one, so checking the owner's silence
  is what stops a handover from racing an agent that is merely busy. The
  handover records the previous owner and a reason.
- `foremerge checks policy <strict|advisory>` sets what acceptance requires when
  Foremerge verified nothing itself. Strict, the default and the previous
  behaviour, accepts only verified work. Advisory accepts work with nothing to
  verify, recorded as `UNVERIFIED` with a reason. A check that ran and *failed*
  is never cleared by policy, because a failure is evidence of breakage rather
  than an absence of evidence.
- `foremerge changeset accept --allow-unverified --override-reason` is the
  operator equivalent, available regardless of policy. Agents cannot reach it
  over MCP, which rejects both overrides; the CLI and the HTTP API are the
  operator surfaces and accept them.
- `foremerge checks verify-symbols true` warns when a published `symbol:` scope
  names something that appears nowhere in the worktree. Off by default, and
  always a warning, because a scope may legitimately name something the agent is
  about to create.
- `foremerge doctor` reports whether the registered checks can actually run
  here, and warns when none are registered under a strict policy.

- `fmg` is a short name for the `foremerge` command. It is the same binary
  installed under a second name, so `fmg status` and `foremerge status` are
  identical, and usage and error text name whichever one you ran. The release
  archives and `cargo install` provide both. The name was chosen after checking
  that nothing else installs a binary called `fmg`; `fm` was rejected because a
  Go terminal file manager already claims it.

- `foremerge mcp` explains itself when it is run in a terminal. It is a stdio
  protocol server, so a person who runs it sees an apparently idle process, and
  typing a tool name such as `list_agents` returns only a parse error. It now
  prints guidance to stderr on startup, and answers a bare tool name with the
  equivalent JSON-RPC line. Both are suppressed when stdin is a pipe, so client
  sessions are byte for byte unchanged.
- `tests/paraphrase_probe.rs`, an adversarial gate over the intent detector. It
  restates one genuine conflict in ten phrasings while holding the declared
  scopes identical, so every miss is a paraphrase failure, and pairs genuinely
  compatible work with destructive keywords on a shared scope, so every HIGH is
  a false alarm. The build fails if compatible work ever raises a HIGH again or
  if paraphrase coverage falls below its recorded floor. Per
  `docs/benchmark-plan.md`, raising that floor by tuning the vocabulary against
  this file does not count as an improvement; held-back phrasings do.
- `tests/full_flow_probe.rs`, which reports every warning two agents actually
  receive across register, publish, and claim, rather than the output of a
  single detector call.

### Fixed

- Symbol scopes are normalized to `container::member`, discarding namespace,
  module and path prefixes, so `App\Services\Report::render` and
  `Report::render` are one scope. Agents describe the same method differently
  and previously never collided at all, which silently defeated overlap
  detection for the case it exists to catch. The deliberate cost is that two
  same-named classes in different namespaces now share a scope and can warn
  about each other: for an advisory system a spurious warning is cheap and a
  missed collision is the failure that matters. Other scope kinds keep their key
  verbatim, because a path or a route is already unambiguous.
- An agent may claim while its intent is `IN_PROGRESS`, which is how a long task
  renews its lease. Previously there was no renew and no re-claim, so work that
  outlasted its lease lost its claims with no way to hold them and no signal
  that it had happened. Re-claiming a scope the intent already holds now extends
  that claim in place instead of stacking a second row for the same scope.
- Acceptance can record that nothing was verified. A repository with no test
  suite could not complete the lifecycle at all, and the only way through was to
  validate a no-op command such as `true`, which wrote a passing validation into
  the append-only, hash-chained log for work that nothing had checked. The gate
  exists so an agent's assertion is never mistaken for evidence, and its only
  workaround was to fabricate evidence. ChangeSets now carry the honest outcome
  (`VERIFIED`, `FAILED` or `UNVERIFIED`) and the reason, on the record and in
  the acceptance event.
- `foremerge status` no longer counts silent agents as active. Registration
  status never expired, so a fleet that died hours earlier still reported as
  fully working; agents unseen for over two hours are now shown as silent and
  excluded from the active count. Claims already expired correctly.
- A destructive verb anywhere in an intent summary no longer implies that the
  destruction applies to a shared semantic scope. The detector previously read
  "Delete the flaky ThumbnailCache benchmark test" as a removal of
  `ThumbnailCache` itself, and paired it with any additive intent on the same
  scope to raise a HIGH replace-versus-extend conflict. The explanation it
  produced was false, and a false HIGH is worse than silence: severity is the
  signal an agent uses to decide what to stop for, so crying wolf at the top
  severity trains agents to ignore it. Two structural checks now gate that
  finding. The verb must not govern only a peripheral artefact, because
  deleting a test, retiring a feature flag, or dropping a debug counter does
  not change the contract of the component it is named after. The scope must
  also be the object of the verb rather than a modifier inside it, because
  "Move the ThumbnailCache eviction loop into a background task" moves the
  loop, not the cache. When either check trips, the pair is still reported, at
  MEDIUM, as a shared semantic scope. Nothing goes unreported; the detector
  simply stops asserting an incompatibility it cannot support.
- Widened the operation vocabulary so that common phrasings of the same
  conflict are classified rather than ignored. Detection requires a known verb
  on both sides, so coverage was the product of two incomplete word lists and a
  single unfamiliar word on either side silenced the pair entirely. This is a
  bounded improvement and not a general solution: a fixed vocabulary cannot
  cover English, and the remaining misses are words that have not been added
  yet rather than a different class of problem. Deciding whether intent
  classification should stay lexical is tracked separately.

### Migration

Opening a store with this release migrates it to database schema 9. Two things
change that an earlier build wrote differently.

Conflict scope identities are recomputed under the rule this build uses and
rows that now share one identity are folded together, carrying their
detections, coordination messages and graph projection onto the survivor. This
repair also runs for stores an affected build already migrated, because that
build filled only blank identities and left stale ones in place.

Intent scope uniqueness moves off the canonical alias and onto the precise
scope, so an intent may declare two symbols whose names differ only by
namespace. `intent_scopes` is rebuilt from `intents.scopes_json`, which is the
source of truth and is untouched, and the alias remains a non-unique search
index. Scopes an older build dropped as alias collisions reappear.

An older Foremerge build will refuse to open a schema 9 store rather than
migrate it backwards, so upgrade every agent on a shared repository together.

Earlier schema notes from this release follow.

Opening a store with an earlier build of this release migrated it to database
schema 6. The `intent_scopes` projection gained `operation` and
`operation_inferred` columns
and an `assessments` table is created. Intents written by an older build
recorded only the scope, so their operation is recovered from the summary and
marked inferred, which is the strongest claim the stored data supports. An
older Foremerge build will refuse to open a schema 6 store rather than migrate
it backwards.


Opening a store with this release migrates it to database schema 5. Symbol
scopes are now normalized, so every canonical form stored under an older schema
is recomputed: the scope projection is rebuilt from `intents.scopes_json`, which
is the source of truth and is untouched, and claims are recomputed in place.
Where normalization makes two live claims on one intent share a scope, the
longest-lived is kept and the others are released. An older Foremerge build will
refuse to open a schema 5 store rather than migrate it backwards.


- Conflict identities written before the scope canonicalization change are
  recomputed on upgrade. The earlier backfill filled only blank identities, so
  a store carrying identities from an older rule kept them; the conflict upsert
  then missed those rows and collided with the legacy uniqueness constraint,
  surfacing a SQLite error where an advisory warning belongs. Conflicts that now
  share one identity are folded into a single row, carrying their detections,
  coordination messages, and graph projection with them, and the survivor keeps
  the least settled status so an open conflict is never hidden behind a
  resolved twin.

  Migration: schema 9. Stores already upgraded by an affected build are
  repaired on open, so no manual step is needed. An older binary will refuse to
  open a schema 9 store, so upgrade every agent on a shared repository together.

- An intent may declare two symbols whose names differ only by namespace, such
  as `App\Billing\Report::render` and `App\Admin\Report::render`. Scope
  uniqueness keyed on the canonical alias, which discards the namespace, so
  publication failed outright with a primary-key error and the reprojection
  quietly dropped the second scope. Uniqueness now keys on the precise scope
  and the alias remains a non-unique search index, so differently qualified
  names still find each other.

- Same-named symbols in different namespaces no longer assert a HIGH conflict.
  The reduced scope name is a search alias, and an overlap found only through it
  is capped below HIGH and offered as a candidate; an asserted finding now
  requires the full name to match. Unrelated code no longer blocks acceptance.

- Adoption establishes that the previous owner actually stopped. It checked
  only for live claims, which a freshly published intent has never had, so a
  handover could race an agent between publishing and claiming. It now requires
  the owner to have fallen silent, and a successful handover touches the
  adopter, advances the intent, and refreshes the graph, so rescued work does
  not immediately read as stranded again under its new owner.

- `last_seen_at` advances when an agent acts. It was written only at
  registration, so every agent read as stale once the staleness window elapsed
  regardless of activity, which made both the status report and the adoption
  gate unreliable.

- Setup no longer overwrites a skill file it did not write. An unstamped file is
  now recognised by matching a body Foremerge actually released; anything else
  unstamped is treated as the operator's own and needs `--force`, as the README
  has always promised.

- A `git grep` that fails to run is no longer read as a missing symbol. Exit 128
  was indistinguishable from the exit 1 that means a clean no-match, so a broken
  search could manufacture the very warning its own contract said it never
  could.

- `foremerge doctor` separates infrastructure readiness from acceptance
  readiness. A healthy installation under a strict policy with no runnable check
  can never accept a ChangeSet, and reporting only `ready` made that look fine;
  the report now also carries `acceptance_ready`. A malformed check registry is
  reported as unreadable rather than defaulted to an empty one, which had sent
  operators to register a check when the file they already had would not parse.

- One scope named twice in a request is folded before storage. Two spellings
  of one symbol reach the same identity, so publishing both surfaced a raw
  SQLite primary-key violation, and claiming both recorded one claim while
  reporting it twice. Entries that agree are collapsed; entries that declare
  different operations on one scope are refused as `INVALID_INPUT`, because
  only the caller knows which was meant.

- A folded conflict's detections carry the survivor's id in their graph payload
  as well as in the row and the edge. The payload kept naming the conflict that
  was folded away, so the graph told two stories about one observation.

- Upgrading no longer drops a live claim. The schema 5 recompute released
  claims that had collapsed onto one scope, keyed on the canonical alias, so
  two same-named symbols in different namespaces looked like duplicates and one
  of them was released. It keys on the precise scope now.

- Renewing a claim with an equivalent spelling updates the stored scope.
  Renewal folds separator and case spellings together, so the stored row kept
  the original spelling while the response and the graph carried the new one.

- One overlapping intent is reported once. The alias is deliberately
  non-unique, so an intent holding several claims that share it was returned
  once per row and the same overlap was persisted and warned about repeatedly.

- Folding a conflict repoints its detections in the graph as well as in the
  table. Their `OCCURRENCE_OF` edges were deleted instead, leaving every
  observation from the folded conflict floating with nothing to occur on.
  Detection nodes whose row is gone are swept, and a missing edge is restored.

- Claims are renewed by their precise scope. Renewal matched on the canonical
  alias, so claiming two symbols whose names differ only by namespace renewed
  the first instead of recording the second: the response carried two claims
  sharing one id, the table kept only the first scope, and the graph showed
  only the second. The alias remains the advisory cross-intent overlap index.

- The conflict graph is reconciled with the conflicts table on upgrade,
  independently of whether anything is folded. A store an earlier build had
  already folded kept a node for the deleted conflict and a survivor whose
  projected status had drifted from its row, and a repair that ran only while
  folding would not have reached it.

- `foremerge doctor` no longer reports `acceptance_ready` outside a repository.
  It defaulted to true when there was no diagnosis to consult, so a directory
  with no store and no repository reported that it was ready to accept work.

- MCP refuses `allow_unverified` by name. The field was absent from the tool's
  request type, so serde discarded it and the acceptance proceeded normally
  while the tool description and the documentation both promised a refusal.

- A Codex registration whose `--cwd` carries no value is diagnosed as malformed
  rather than current. Codex refuses to start it, so reporting it as the
  portable form called a broken registration healthy.

## [0.3.1] - 2026-08-24

Opening a store with this release migrates it to database schema 4. The upgrade
is automatic and one way: it repairs the duplicate detection rows that earlier
schemas could write, completes any partially written scope projection, and
stamps the new version. Schema 4 sweeps stores stamped 1, 2, or 3, because an
interrupted upgrade could leave duplicates behind any of them. Observations that
genuinely differ are preserved. An older Foremerge build will refuse to open a
schema 4 store rather than migrate it backwards.

### Added

- The release workflow refuses a tag that disagrees with the crate version, and
  publication requires an approval in the protected `release` environment.

### Fixed

- Validation now refuses to start when excluded generated files are already
  present. Exclusions exist so a check may create such files without
  invalidating its own fingerprint; a file that exists beforehand is different,
  because the command may consume it, and a pass could then depend on content
  absent from the candidate commit and uncovered by the fingerprint. Requiring a
  clean start means every excluded path seen afterwards was produced by that
  run. Migration: delete generated artifacts between validations.
- Acceptance now enforces the documented requirement that excluded generated
  files be removed first. Excluded paths are deliberately outside the
  fingerprint, so they never made the worktree dirty and `ensure_clean` let a
  candidate through while they were still present. That allowed a ChangeSet to
  be accepted whose validation may have depended on content absent from the
  accepted commit, which is exactly what the fingerprint exists to prevent.
  Removing the files cannot invalidate the ChangeSet, because their presence
  and content were excluded from the fingerprint to begin with.
- Migration now runs in a single immediate transaction. An interrupted upgrade
  previously left the `intent_scopes` projection partially written, and the
  per-intent skip guard then treated the partial intent as done forever, so the
  intent silently disappeared from scope queries with no error. The migration is
  now all or nothing, and the upgrade reprojects every intent once to repair
  stores already damaged this way.
- One-time backfills are gated on the stored schema version instead of running
  on every `Store::open`. Schema 2 re-ran them on every process start, which
  minted a duplicate synthesized detection for every conflict that already had a
  native one: two immutable observations for a single detection, and an
  occurrence table that disagreed with the event log. The upgrade removes those
  duplicates on first open, keeping genuine legacy rows where they are a
  conflict's only observation. Gating also removes a full rescan of `intents`,
  `conflicts`, and `validations` from every CLI invocation.
- The `validations` projection is append-only. It is the table acceptance
  actually reads, and it was the only one of the four record tables with no
  guard, so a rewritten or replaced row could turn a failing gate into a passing
  one while the audit tables looked untouched.
- Reusing the id of an existing `validation_attempts` or `conflict_detections`
  row is rejected by the schema itself. `INSERT OR REPLACE` deletes the
  conflicting row first, and that delete only fires the append-only trigger when
  `recursive_triggers` is on, which is a per-connection setting that any other
  SQLite client can ignore. `PRAGMA recursive_triggers` is now enabled as
  defense in depth rather than as the guarantee.
- The schema version is parsed strictly and a store newer than the running build
  is refused with `UNSUPPORTED_SCHEMA` instead of being migrated backwards and
  restamped. A malformed value is reported as `CORRUPT_STORE` rather than
  silently reading as zero, which would have rerun every one-time backfill.
- The duplicate repair removes only a byte-identical duplicate observation. An
  earlier condition deleted any synthesized row whenever a native one existed
  for the same conflict, which destroyed a genuine earlier observation that a
  later redetection happened to follow.
- The event log rejects `INSERT OR REPLACE`. It had guards against `UPDATE` and
  `DELETE`, but a replace deletes the conflicting row first, and that delete
  only fires the guard when `recursive_triggers` is on, which is a
  per-connection setting any other SQLite client can ignore. Unlike the other
  record tables, `events` has three unique keys, so the guard checks `seq`,
  `event_id`, and `event_hash`.
- The duplicate repair now sweeps every store below the current schema rather
  than only those stamped with the schema that introduced the duplicates. That
  earlier migration was not transactional, so an upgrade interrupted between the
  backfill and the version stamp left duplicates behind an older stamp, and a
  repair keyed to one exact stamp could restamp such a store with its duplicates
  intact.
- The documented daemon shutdown grace is now a process-exit bound. The daemon
  builds and owns its Tokio runtime, so when the 30 second HTTP grace expires it
  abandons the remaining requests (terminating the validation subprocess trees
  it started), waits at most 5 further seconds for blocking work that cannot be
  cancelled, and then exits: 0 after a clean drain, 1 when the bound had to be
  enforced. Previously the runtime was dropped at the end of `main`, which
  blocked forever on an uncancellable blocking task such as a wedged `git`
  child, leaving the daemon alive and swallowing later signals. Documentation
  now also states what still survives the forced exit: a child process started
  outside a validation guard is not killed, and signals sent during the drain
  are ignored.

## [0.3.0] - 2026-08-22

### Added

- `foremerge status`: one human-first screen answering "what are my agents
  doing right now" with active agents, intents grouped by lifecycle status,
  unexpired claims with their scopes, OPEN or COORDINATING conflicts naming
  both parties and both sides' scopes, and ChangeSets grouped by status with
  ids for the non-terminal ones. The default output is readable aligned plain
  text; `--json` returns the typed report in the standard envelope. The whole
  report is read in one transaction, so its sections describe one consistent
  moment.
- Immutable `validation_attempts` retain every completed command with explicit
  authoritative/stale state, expected and observed fingerprints, bounded
  output, changed-path diagnostics, excluded paths, and policy digest.
- Stable conflict lifecycle identities now have immutable detection occurrences,
  `conflict.redetected` events, and `previously_settled` responses.
- Operator-only `validation-exclusions show|set` with exact/prefix untracked
  rules, normalized digest binding, atomic private storage, and ADR 0001. MCP
  intentionally has no policy-mutation tool.
- Authenticated paged event-chain audit over a separate read-only connection,
  exposed through HTTP and `events audit`.
- Read parity across HTTP and MCP for agent list, intent show, ChangeSet show,
  and the consistent status report, bringing the MCP surface to 17 tools.
- An executable five-scenario correctness corpus and an optimized query-work
  microbenchmark harness with JSON output.
- macOS and Windows CI/release test gates, including platform-specific
  validation-timeout cleanup coverage.

### Changed

- Breaking HTTP observability change: `/healthz` now reports process liveness
  only and performs no database or Git work. Use public `/readyz` for bounded
  non-waiting readiness and authenticated `/v1/audit/event-chain` for integrity
  audit. Migration: monitoring that consumed health counts or `event_chain_ok`
  must move to the typed read/audit routes.
- Breaking diagnostic change: `foremerge doctor` is strictly read-only. It no
  longer initializes or migrates a missing database and instead returns
  `database_ok: false` with `foremerge init` as the next step.
- HTTP and MCP dispatch synchronous SQLite/Git service operations through
  Tokio's blocking pool. SIGINT/SIGTERM drain in-flight HTTP requests for at
  most 30 seconds and terminate remaining validation process trees.
- Work queries use dynamic indexed SQL, a normalized `intent_scopes`
  projection, SQL-side limits, cached statements, and one reverse-dependency
  scan per request.
- Validation captures the final Git snapshot before opening its short write
  transaction, then rechecks current revision and lifecycle state atomically.
- Symbol inference provenance reports when the 4 MiB diff capture was
  truncated, with captured and total byte counts.

### Fixed

- A passing check that generated an untracked artifact no longer disappears:
  the stale attempt is queryable and names the path while remaining unable to
  satisfy acceptance.
- Redetecting a resolved, overridden, or dismissed conflict no longer rewrites
  its original evidence or silently reopens it.
- Full event-chain verification no longer holds the coordinator's shared mutex
  or runs through the unauthenticated liveness route.

### Migration

- Opening a tagged 0.1/0.2 database creates and backfills
  `validation_attempts`, `conflict_detections`, and `intent_scopes`, plus the
  order/filter indexes used by the 0.3 query path. Existing authoritative
  validations and conflicts become legacy immutable observations.

## [0.2.0] - 2026-08-21

### Added

- Native Foremerge skills and MCP setup for Codex, Claude Code, and Cursor.
- Safe `foremerge setup codex|claude|cursor|all` installation with explicit
  `--force` replacement and `doctor --client` diagnostics.
- Repository-scoped trusted checks configured through `foremerge checks`.
- Complete 13-tool MCP lifecycle, including start, resolution, named
  verification, acceptance, discard, and integration-commit recording.
- Read commands `foremerge intent show <ID>` and `foremerge agent list`.
- `--base-ref` on `changeset publish`; the diff base defaults to the candidate
  commit's first parent and provenance records a real diff hash and the base
  resolution mode.
- `work start` and `changeset publish` responses report `open_conflicts` for
  the intent, so an earlier publisher learns about conflicts created by later
  publishes.
- `coordinate inbox` accepts `--agent` alongside the positional agent id.

### Changed

- Breaking: `work discard` (CLI, HTTP, and MCP) now rejects an empty reason.
  Migration: pass a non-empty `--reason`/`reason` value.
- Over MCP, `accept_changeset` no longer accepts HIGH-conflict overrides
  (overrides are CLI operator actions) and `resolve_conflict` is limited to
  agents that are parties to the conflict.
- The trusted-check registry requires a real Git repository and is resolved
  from the bound repository, never from the MCP server's process working
  directory or a `.foremerge` fallback directory.
- ChangeSet `base_ref` records the diff base instead of repeating the
  candidate ref.
- `conflicts check --intent` rejects values shaped like intent ids and points
  the caller at `--intent-id`.

### Fixed

- Conflict explanations no longer extract sentence-starting English words as
  subjects (previously producing text like "will migrate `No`"); suggestions
  are scope-kind aware (migration ordering for schema/migration/config scopes,
  coordination-first wording for duplicate work), and conflict evidence names
  the overlapping scope from both sides.
- `setup --force` can repair a stale Foremerge MCP entry, and stale entries
  are no longer reported as configured by setup or `doctor`.
- Codex MCP registration is treated as user-global configuration: setting up a
  second repository now produces an explicit error, or a disclosed repoint
  with `--force`, instead of a false "configured" report or a silent hijack.
- `setup all` attempts every requested client and reports each result instead
  of aborting on the first failure.
- MCP config merges preserve the order of unrelated entries, dangling
  symlinks are refused, and client probes are time- and output-bounded.

## [0.1.0] - 2026-08-20

### Added

- Initial Rust crate and `foremerge` binary package metadata.
- Typed coordination models for agents, intents, semantic claims, conflicts,
  ChangeSets, validation, decisions, messages, and hash-chained events.
- Apache-2.0 licensing, contribution and security policies, CI, release checks,
  limitations, roadmap, and a reproducible benchmark specification.

[Unreleased]: https://github.com/naw103/foremerge/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/naw103/foremerge/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/naw103/foremerge/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/naw103/foremerge/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/naw103/foremerge/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/naw103/foremerge/releases/tag/v0.1.0
