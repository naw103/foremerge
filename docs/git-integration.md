# Git integration

Foremerge coordinates work above Git. It does not implement a new object store,
index, merge algorithm, or working-copy format.

The MVP uses the installed `git` executable for repository discovery and
snapshots. Commands are constructed as argument arrays and executed with
`git -C <worktree> ...`; user input is not interpolated into a shell string.

## Repository discovery

Given a starting path, Foremerge resolves:

- repository root with `git rev-parse --show-toplevel`;
- absolute Git common directory with
  `git rev-parse --path-format=absolute --git-common-dir`;
- current branch with `git symbolic-ref --quiet --short HEAD`;
- current commit with `git rev-parse --verify HEAD`.

Branch and head are optional so detached heads and unborn repositories can be
represented.

## Shared state across worktrees

By default, the database path is:

```text
<git-common-dir>/foremerge/state.sqlite3
```

Linked worktrees have different roots but the same Git common directory. They
therefore share coordination state without sharing checked-out files.

If Foremerge cannot discover a Git repository, it falls back to:

```text
<current-directory>/.foremerge/state.sqlite3
```

An explicit database path overrides both locations. Relative overrides resolve
from the caller's starting directory.

The `common_dir_is_shared` check compares the discovered common directories for
two worktrees. It does not compare path contents or assume a shared working
directory.

Foremerge can create an isolated worktree by delegating to stock Git:

```bash
foremerge worktree create \
  --branch foremerge/paypal-agent \
  --path ../paypal-agent \
  --base HEAD
```

This runs `git worktree add -b`; Foremerge does not maintain a second worktree
registry or remove worktrees.

## Snapshot algorithm

Foremerge snapshots a worktree using:

1. `git status --porcelain=v1 -z --untracked-files=all` for dirty state and
   changed paths;
2. `git diff --no-ext-diff --no-textconv --binary HEAD` for tracked staged and
   unstaged changes;
3. direct content hashing for all individually reported untracked regular
   files (`--untracked-files=all`);
4. `git rev-parse HEAD^{tree}` for the committed tree when available.

Changed paths are sorted and deduplicated. Consumers should treat them as
snapshot evidence, not as a lossless representation of every porcelain status
record (notably, rename metadata is richer than this path list).

The diff digest is SHA-256 over the binary diff plus each untracked path and its
content. The final fingerprint is SHA-256 over:

```text
Git common directory
worktree root
HEAD commit or UNBORN
tree ID or NO_TREE
diff digest
ordered changed paths
normalized validation-exclusion ruleset digest
```

Fingerprints are stored with a `sha256:` prefix.

Including untracked contents matters: changing an untracked regular file changes
the fingerprint even though ordinary `git diff HEAD` omits it. Ignored files are
outside the snapshot. An operator may explicitly exclude exact or prefix-matched
untracked generated paths through [ADR 0001](adr/0001-validation-exclusion-rules.md).
Tracked content is never excludable, and changing the normalized policy digest
changes the fingerprint.

## Symbol hints

Foremerge can scan zero-context added and removed diff lines for declarations
using a lightweight language-neutral expression. It recognizes declarations
introduced by keywords such as:

```text
fn struct enum trait class interface type def function module
```

This produces hints, not a complete symbol table. It does not understand scope,
overloads, macros, generated code, or semantic references. Agents should still
publish explicit symbol and contract scopes.

## ChangeSet provenance

When a ChangeSet is published, Foremerge can derive:

- worktree and repository identity;
- current branch and head;
- the candidate commit and its diff base;
- changed files;
- lightweight symbol hints;
- fingerprint.

The candidate commit is the resolved `git_ref` (default: the worktree `HEAD`).
Its diff base is chosen in this order and recorded as
`provenance.git.base_resolution`:

1. `caller_supplied`: an explicit `base_ref` (CLI `--base-ref`) for callers
   that know their true base, such as the fork point of the agent branch. A
   base that resolves to the candidate itself is rejected as `INVALID_INPUT`.
2. `first_parent`: the candidate commit's first parent, the default.
3. `root_commit`: the candidate has no parent; the diff base is the empty
   tree.
4. `unborn_worktree`: the repository has no commits; there is no candidate
   commit and `diff_hash` falls back to the snapshot's worktree content hash.
5. `shallow_boundary`: the repository is a shallow clone and the candidate's
   parent lies outside it, so no honest diff base exists; `diff_hash` falls
   back to the snapshot's worktree content hash. Pass `--base-ref` with a
   fetched base to record a real diff from a shallow clone.

`provenance.git.diff_hash` is a SHA-256 over the actual binary patch bytes of
`git diff <base> <candidate>` (bounded by the same 512 MiB budget as snapshot
hashing), so a non-merge commit with changes never records the hash of an
empty diff. The chosen base commit is stored as the ChangeSet's `base_ref`.

Caller-provided files and symbols remain useful when changes are already
committed or when semantic impact extends beyond the textual diff.

## Validation and freshness

A validation runs in an explicitly selected worktree or the ChangeSet's recorded
worktree. Its command is an argument vector, never a shell string. On the CLI
the operator supplies it directly (`foremerge changeset validate <id> --
cargo test --all-targets`). Over MCP and HTTP the caller names a check from the
repository's trusted registry, which supplies the argument vector and timeout:

```json
{
  "check": "test"
}
```

The check runs at the root of the ChangeSet's recorded worktree. No surface can
run it from a subdirectory: the fingerprint is identical anywhere inside a
worktree, so a nested directory could otherwise turn a failing root command into
a pass that is recorded as verified.

Foremerge records exit status, standard output, standard error, duration, and
the ChangeSet fingerprint associated with the run. A zero exit status is a
pass. The final 16 KiB of each output stream is retained. The final Git snapshot
is captured before the short SQLite write transaction. Every completed command
is retained as an immutable attempt; only a result whose revision, lifecycle,
and final fingerprint still match becomes authoritative. Stale diagnostics name
changed paths and preserve the output without allowing acceptance. Test evidence
reported inside `publish_changeset` is provenance only; it is not equivalent to
a Foremerge-executed validation.

Before acceptance, Foremerge compares the current snapshot with the validated
fingerprint. Code that changed after the test must be tested again.

ChangeSet dependency IDs are checked at acceptance. Every dependency must name
an accepted or committed intent with an immutable stored `accepted_commit`.
Its namespaced accepted ref must still resolve to that exact hash, and the hash
must be an ancestor of the candidate according to `git merge-base
--is-ancestor`. A dependency that is `COMMITTED` is also satisfied when its
recorded `integration_commit` is an ancestor of the candidate. That covers a
dependency that landed through a squash or rebase merge: its accepted commit is
on no branch later work starts from, but the landing commit `record_commit`
verified is. A dependency that landed that way and has not been recorded yet
is refused until its landing commit is recorded. The `changeset.accepted` event
lists each dependency with `satisfied_by` (`accepted_commit` or
`integration_commit`) and the commit that was found.

Validation commands are trusted local code. They inherit the user's environment
and operating-system permissions.

## Acceptance refs

Acceptance requires a clean worktree. The selected Git ref is resolved to a
commit using:

```text
git rev-parse --verify <ref>^{commit}
```

Foremerge then creates a namespaced ref:

```text
refs/foremerge/accepted/<changeset-id>
```

The ref points to the resolved commit and keeps that object reachable; the same
hash is stored independently as the ChangeSet's `accepted_commit`. `git
update-ref` is supplied an empty expected old value, so Foremerge creates the
ref only when absent and never overwrites it. Ordinary Git commands can still
update or delete the ref, but Foremerge detects a mismatch with the stored pin
and refuses dependency acceptance or integration recording.

An accepted ref records the exact validated worktree `HEAD`. If an explicitly
supplied ref resolves to a different commit, acceptance fails as stale.
Acceptance does not move the current branch, update a worktree, merge code, or
push anything to a remote.

Useful inspection commands are ordinary Git:

```bash
git show-ref refs/foremerge/accepted/<changeset-id>
git show refs/foremerge/accepted/<changeset-id>
git diff main...refs/foremerge/accepted/<changeset-id>
```

## Integration workflow

The honest MVP workflow is:

1. Work and commit on an isolated agent branch.
2. Publish a ChangeSet tied to that worktree/ref.
3. Run validation through Foremerge.
4. Accept the clean `HEAD`, creating an accepted ref.
5. Merge, rebase, cherry-pick, or open a pull request using ordinary Git tooling.
6. Record the integration ref. Foremerge requires that commit to contain the
   accepted candidate, by ancestry or, after a squash or rebase merge, by
   content (see below), then releases the intent's active claims.

Foremerge does not currently implement automatic fast-forward integration,
rebasing, conflict resolution, target-branch updates, or GitHub operations.

## Recording the landing commit

`record_commit` (CLI `foremerge changeset commit`) accepts a commit only when it
contains the accepted candidate. It checks in two steps.

1. **Ancestry.** If `accepted_commit` is an ancestor of the recorded commit
   (`git merge-base --is-ancestor`), the commit contains it. A merge commit, a
   fast-forward, and the accepted commit itself all pass here. The ChangeSet's
   `provenance.integration` and the `changeset.committed` event record
   `method: "ancestor"`.
2. **Content.** A squash merge and a rebase merge write new commits, so the
   accepted commit is not in the landed commit's history. Foremerge then
   accepts the commit only if it holds every change the candidate made. For
   each merge base of the accepted commit and the recorded commit, it lists
   the paths the candidate changed relative to that base, without rename
   detection, and requires each one to have landed:
   - a path identical to the candidate's version has landed;
   - a regular file that differs has landed only if a three-way line merge of
     the candidate's version into the recorded version, with the merge base's
     version as the ancestor, is clean and leaves the recorded version
     unchanged. That allows other edits to the same file, such as changelog
     entries that reached the target branch first, while refusing a file that
     lacks or contradicts any of the candidate's lines;
   - a path the candidate deleted must be absent;
   - the kind of entry must match, a regular file, a symbolic link or a
     submodule, checked before contents are compared, because a link's blob
     is its target text and can equal a file's blob while reading as
     something else;
   - a file mode the candidate set must match, and symbolic links,
     submodules and binary files must match exactly.

   The merge is `git merge-file`, which reads no attributes, so a custom merge
   driver configured in the repository cannot make a missing change look
   present. Every diff option that configuration could change is pinned
   (`diff.renames`, `diff.relative`, `diff.ignoreSubmodules`, `core.abbrev`,
   external diff and text conversion), paths are compared as exact bytes, and
   the result does not depend on the directory the command runs from. On
   success the method is `content`, and the evidence also carries
   `merge_bases`, `paths_checked`, and `paths_with_other_changes`.

Content containment refuses, with `TARGET_DIVERGED` and the reason:

- a commit already in the candidate's own history, such as its fork point or
  one of its earlier commits;
- a commit that shares no history with the candidate;
- any commit at all when the candidate makes no change relative to the merge
  base, since every commit would trivially hold an empty change;
- a commit where any path the candidate changed is missing, still present
  after the candidate deleted it, or lacks or conflicts with its change. The
  message names each such path and why.

Each refusal ends with the next step: record the commit the work actually
landed in, or, if what landed differs from what was accepted, publish and
validate a revision for it.

The evidence is stored in the ChangeSet's `provenance.integration`, beside the
provenance captured at publication, so `get_changeset` and `foremerge changeset
show` return it on every later read. The `changeset.committed` event holds the
same object and is the immutable copy.

A squash that combined this candidate with other work, such as an extra file
or extra edits to the candidate's files, is accepted, because every change the
candidate made is present. This is deliberate and matches ancestry, which has
always accepted a merge commit that also brings in other commits: the question
is whether this ChangeSet landed, not whether nothing else did. A squash or rebase that dropped part of the candidate is
refused. If the rest lands later, recording the later commit succeeds.

Content cannot tell where a change came from. A commit that makes exactly the
candidate's changes by another route is indistinguishable from its landing,
and is accepted. As with ancestry, the recorded commit is caller-attested:
Foremerge does not check that it reached any particular branch or remote.

## Failure behavior

- Outside Git, discovery reports that no worktree was found and storage falls
  back to `.foremerge`.
- A missing or invalid ref fails acceptance before an accepted ref is created.
- A dirty worktree fails acceptance and lists changed paths.
- An already existing accepted ref is accepted only when it already points to
  the same commit; it is never overwritten with a different commit.
- An unborn repository has no head/tree but can still carry local coordination
  state.
- If the worktree changes after validation, the fingerprint changes and the old
  result is stale.

## Known limitations

- Git must be installed and available on `PATH`.
- `--path-format=absolute` requires a sufficiently recent Git version.
- Snapshot hashing streams tracked diffs and untracked regular files with a
  512 MiB aggregate content budget; oversized snapshots fail closed.
- Validation retains only the latest 16 KiB from each output stream in SQLite,
  but that tail may still be sensitive.
- Lightweight declaration inference is not AST analysis. Diff capture is
  bounded at 4 MiB and provenance explicitly reports truncation, captured
  bytes, and total bytes.
- Ignored files do not contribute content to the fingerprint.
- NUL-delimited rename/copy records retain both old and new paths, but the
  changed-path list does not expose Git's similarity score.
- Foremerge can create a branch and worktree through `git worktree add -b`, but
  does not remove, prune, repair, or otherwise manage them.
- There is no atomic transaction spanning Git ref mutation and SQLite. Accepted
  refs are deliberately namespaced and inspectable so interrupted operations can
  be diagnosed with normal Git commands.
