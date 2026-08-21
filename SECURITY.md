# Security policy

Foremerge is pre-1.0 software that coordinates local developer agents. It is not
a sandbox or an authorization boundary. Run it only on repositories, worktrees,
and validation commands you trust.

## Supported versions

Security fixes are made on the default branch and, after the first tagged
release, on the latest release line. Older pre-1.0 versions may be asked to
upgrade before receiving a fix.

| Version | Supported |
| --- | --- |
| Default branch | Yes, best effort |
| Latest tagged release | Yes, best effort |
| Older releases | No |

## Reporting a vulnerability

Use GitHub's private vulnerability reporting feature in the repository's
**Security** tab. Do not open a public issue for a suspected vulnerability.

Include, when possible:

- the affected version or commit;
- operating system and Rust version;
- a minimal reproduction;
- expected and observed behavior;
- potential impact; and
- any suggested mitigation.

Please avoid accessing data that is not yours, degrading third-party systems, or
publishing the report before maintainers have had a reasonable opportunity to
investigate. This is a volunteer open-source project, so response times are best
effort rather than a service-level guarantee. Maintainers will try to acknowledge
a complete report within seven days and will coordinate disclosure when a fix is
available.

If GitHub private vulnerability reporting is not available, use a private contact
method listed on a maintainer's GitHub profile. Do not send secrets in a public
issue.

## Security-relevant behavior

- The HTTP daemon should remain bound to loopback unless the operator supplies
  an authentication and network-isolation strategy. Local mode is the supported
  default.
- Validation commands run with the invoking user's permissions. Foremerge does
  not sandbox them, inspect them for safety, or make untrusted commands safe.
- Git worktrees and repository content are untrusted input. Paths and arguments
  that reach Git or the filesystem must not be interpolated into a shell command.
- SQLite coordination state and its hash-chained event log provide auditability,
  not protection from a local user who can replace or edit the database.
- Intents, prompts, decisions, command output, and provenance may contain source
  code or secrets. Keep the state directory private and redact sensitive values
  before publishing or exporting it.
- The MCP stdio transport reserves stdout for protocol messages. Diagnostics and
  logs belong on stderr.
- Named MCP verification checks are stored under Git's common directory with
  private file permissions where the platform supports them. The registry
  requires a real Git repository and is never read from a plain `.foremerge`
  fallback directory, but it is not a sandbox or multi-user authorization
  boundary. Anyone permitted to change that local repository state can change
  executable validation policy; review checks as trusted automation.

See [`docs/limitations.md`](docs/limitations.md) for the broader trust model and
known product limitations.
