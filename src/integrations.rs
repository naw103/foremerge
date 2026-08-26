use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Client probes and Codex registration commands run external CLIs that may
/// hang (first-run prompts, wrapper scripts waiting on stdin). Mirror the
/// validation runner's bounded posture: closed stdin, capped capture, its own
/// process group, and a hard timeout that kills the whole group.
const CLIENT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Extra time allowed for the capture readers to observe EOF after the
/// probe's process group has been killed.
const CAPTURE_GRACE: Duration = Duration::from_secs(2);
const MAX_CLIENT_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Debug)]
struct BoundedOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Reads a probe output stream on a detached thread and delivers the captured
/// bytes over a channel at EOF. The thread is never joined directly: the
/// caller receives with a deadline, and the thread terminates on its own once
/// every writer of the pipe is gone.
fn capture_bounded(stream: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stream = stream;
        let mut captured = Vec::new();
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let remaining = MAX_CLIENT_OUTPUT_BYTES.saturating_sub(captured.len());
                    captured.extend_from_slice(&buffer[..read.min(remaining)]);
                }
            }
        }
        let _ = sender.send(captured);
    });
    receiver
}

fn recv_until(receiver: &mpsc::Receiver<Vec<u8>>, deadline: Instant) -> Option<Vec<u8>> {
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
}

/// Kill the probe's whole process group, not only the direct child: a wrapper
/// script can leave background descendants that inherited the output pipes,
/// and they must die for the capture readers to ever see EOF. Uses the same
/// `/bin/kill` group signal as the validation runner.
fn kill_probe_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .status();
    }
    let _ = child.kill();
}

fn run_bounded(command: Command) -> Result<BoundedOutput> {
    run_bounded_with_timeout(command, CLIENT_COMMAND_TIMEOUT)
}

fn run_bounded_with_timeout(mut command: Command, timeout: Duration) -> Result<BoundedOutput> {
    let program = command.get_program().to_string_lossy().into_owned();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {program}"))?;
    let stdout = child
        .stdout
        .take()
        .map(capture_bounded)
        .ok_or_else(|| anyhow::anyhow!("capture {program} stdout"))?;
    let stderr = child
        .stderr
        .take()
        .map(capture_bounded)
        .ok_or_else(|| anyhow::anyhow!("capture {program} stderr"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("wait for {program}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            kill_probe_tree(&mut child);
            let _ = child.wait();
            bail!(
                "RESOURCE_LIMIT: {program} did not finish within {} seconds",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    // The child has exited, but background descendants that inherited its
    // output pipes keep the readers from seeing EOF. Bound the collection by
    // the remaining deadline, then kill the group and allow a short grace for
    // the readers to drain.
    let stdout_bytes = recv_until(&stdout, deadline);
    let stderr_bytes = recv_until(&stderr, deadline);
    let (stdout, stderr) = match (stdout_bytes, stderr_bytes) {
        (Some(stdout), Some(stderr)) => (stdout, stderr),
        (stdout_bytes, stderr_bytes) => {
            kill_probe_tree(&mut child);
            let grace = Instant::now() + CAPTURE_GRACE;
            let stdout_bytes = stdout_bytes.or_else(|| recv_until(&stdout, grace));
            let stderr_bytes = stderr_bytes.or_else(|| recv_until(&stderr, grace));
            match (stdout_bytes, stderr_bytes) {
                (Some(stdout), Some(stderr)) => (stdout, stderr),
                _ => bail!(
                    "RESOURCE_LIMIT: {program} exited but left background processes holding its output pipes, so its output could not be captured within {} seconds",
                    timeout.as_secs()
                ),
            }
        }
    };
    Ok(BoundedOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

/// Wrap `error` with `action` while keeping any UPPER_SNAKE error code first,
/// so typed codes (RESOURCE_LIMIT, ALREADY_EXISTS, ...) are not demoted to the
/// generic ERROR by the setup failure envelope. Errors without a code get
/// `fallback_code`.
fn coded(error: anyhow::Error, fallback_code: &str, action: &str) -> anyhow::Error {
    let message = format!("{error:#}");
    match message.split_once(':') {
        Some((code, rest)) if is_error_code(code) => {
            anyhow::anyhow!("{code}: {action}: {}", rest.trim_start())
        }
        _ => anyhow::anyhow!("{fallback_code}: {action}: {message}"),
    }
}

fn is_error_code(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate
            .chars()
            .all(|character| character.is_ascii_uppercase() || character == '_')
}

pub const SKILL_CONTENT: &str = include_str!("../.codex/skills/foremerge/SKILL.md");

/// Marker Foremerge appends to every skill file it installs, naming the release
/// that produced the body and a digest of that body.
///
/// The digest is what lets a later release tell a file this installer wrote
/// from one an operator edited. Foremerge's own unedited file may be upgraded
/// in place, because replacing it destroys nothing; an edited file still
/// requires --force.
const SKILL_STAMP_PREFIX: &str = "<!-- foremerge:managed ";
const SKILL_STAMP_SUFFIX: &str = "-->";

/// The body of every Foremerge skill file released before stamping existed.
///
/// Stamping arrived after v0.3.1, so the whole pre-stamp installed base is
/// these three bodies: v0.1.0, v0.2.0, and v0.3.0/v0.3.1, which shipped an
/// identical file. A file matching one of them is provably untouched and may
/// be refreshed in place; an unstamped file matching none of them is somebody's
/// own work and needs `--force`.
///
/// Each entry is the SHA-256 of `skill_body` for that release, which for an
/// unstamped file is the file verbatim. Recompute with
/// `git show vX.Y.Z:.codex/skills/foremerge/SKILL.md | shasum -a 256`.
const PRE_STAMP_SKILL_DIGESTS: &[&str] = &[
    "3c16ef8ed787bb749784a2b7fa728a80a931f1fbb7df0a695c5c03325610143b",
    "ffa4d100e05c7ca74df00f83c05ad52a36eafc140465ddab17abbb321d37f9d9",
    "247a1e96ec93a9a3459f530ff331e2c5d1bbfdf8e823650307aef0ace1ddcda2",
];

/// How an installed skill file relates to the one this release would write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillState {
    /// Already carries this release's instructions, stamped or not.
    Current,
    /// Foremerge's own file, untouched since it was written, from a different
    /// release. Upgrading it is not an overwrite of anyone's work.
    Replaceable,
    /// Edited after Foremerge wrote it. Replacing it needs --force.
    Edited,
}

/// The skill text without a trailing managed stamp. Both the embedded copy and
/// on-disk files are canonicalized through this, so a stamp can never nest
/// inside the body it describes.
fn skill_body(content: &str) -> &str {
    let trimmed = content.trim_end_matches('\n');
    match trimmed.rfind('\n') {
        Some(cut) if is_skill_stamp(&trimmed[cut + 1..]) => &content[..=cut],
        None if is_skill_stamp(trimmed) => "",
        _ => content,
    }
}

fn is_skill_stamp(line: &str) -> bool {
    line.starts_with(SKILL_STAMP_PREFIX) && line.ends_with(SKILL_STAMP_SUFFIX)
}

fn skill_digest(body: &str) -> String {
    format!("{:x}", Sha256::digest(body.as_bytes()))
}

/// The digest a file's own stamp records, if it carries a well-formed one.
fn skill_stamp_digest(content: &str) -> Option<&str> {
    let trimmed = content.trim_end_matches('\n');
    let line = trimmed.rsplit('\n').next()?;
    if !is_skill_stamp(line) {
        return None;
    }
    line.split_whitespace()
        .find_map(|token| token.strip_prefix("sha256="))
}

/// The exact bytes setup installs: this release's body plus its stamp.
fn desired_skill() -> String {
    let body = skill_body(SKILL_CONTENT);
    let mut content = String::with_capacity(body.len() + 96);
    content.push_str(body);
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(SKILL_STAMP_PREFIX);
    content.push_str("version=");
    content.push_str(env!("CARGO_PKG_VERSION"));
    content.push_str(" sha256=");
    content.push_str(&skill_digest(body));
    content.push(' ');
    content.push_str(SKILL_STAMP_SUFFIX);
    content.push('\n');
    content
}

fn skill_state(existing: &str) -> SkillState {
    let body = skill_body(existing);
    if body == skill_body(SKILL_CONTENT) {
        // Same instructions this release ships. Nothing to rewrite, even when
        // the file predates stamping and so carries no marker of its own.
        return SkillState::Current;
    }
    match skill_stamp_digest(existing) {
        // A stamp still matching its own body proves nothing was edited after
        // Foremerge wrote the file, so this release may upgrade it in place.
        Some(recorded) if recorded == skill_digest(body) => SkillState::Replaceable,
        Some(_) => SkillState::Edited,
        // Releases before stamping left no marker of their own, so an
        // unstamped file is identified by its content instead. Matching a body
        // Foremerge actually shipped proves nobody has touched it, which is
        // the same thing a stamp proves. Anything else unstamped was written
        // by someone: treating it as Foremerge's own would silently discard
        // their work, which is exactly what the README promises never happens
        // without --force.
        None if PRE_STAMP_SKILL_DIGESTS.contains(&skill_digest(body).as_str()) => {
            SkillState::Replaceable
        }
        None => SkillState::Edited,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    Codex,
    Claude,
    Cursor,
}

impl Client {
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
        }
    }

    fn skill_path(self, root: &Path) -> PathBuf {
        root.join(format!(".{}/skills/foremerge/SKILL.md", self.name()))
    }

    fn mcp_path(self, root: &Path) -> Option<PathBuf> {
        match self {
            Self::Codex => None,
            Self::Claude => Some(root.join(".mcp.json")),
            Self::Cursor => Some(root.join(".cursor/mcp.json")),
        }
    }

    fn executable(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor-agent",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInstall {
    pub path: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInstallReport {
    pub client: String,
    pub skill: FileInstall,
    pub mcp: Option<FileInstall>,
    pub mcp_configured: bool,
    pub next_step: Option<String>,
    pub warning: Option<String>,
    pub error: Option<String>,
}

struct McpOutcome {
    install: Option<FileInstall>,
    configured: bool,
    next_step: Option<String>,
    warning: Option<String>,
}

/// Human-readable scope label for the one setup write that lands outside the
/// repository: the Codex CLI stores MCP registrations in user-level
/// configuration rather than a project file.
pub const CODEX_MCP_SCOPE: &str = "Codex user-level configuration (codex mcp)";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientDiagnostic {
    pub client: String,
    pub client_available: bool,
    pub skill_path: String,
    pub skill_installed: bool,
    pub skill_current: bool,
    pub mcp_path: Option<String>,
    pub mcp_configured: bool,
    pub ready: bool,
    pub next_step: Option<String>,
}

pub fn install(
    root: &Path,
    clients: &[Client],
    foremerge_exe: &Path,
    force: bool,
    configure_mcp: bool,
) -> Vec<ClientInstallReport> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    clients
        .iter()
        .copied()
        .map(|client| install_client(&root, client, foremerge_exe, force, configure_mcp))
        .collect()
}

pub fn diagnose(root: &Path, clients: &[Client]) -> Vec<ClientDiagnostic> {
    clients
        .iter()
        .copied()
        .map(|client| diagnose_client(root, client))
        .collect()
}

fn install_client(
    root: &Path,
    client: Client,
    foremerge_exe: &Path,
    force: bool,
    configure_mcp: bool,
) -> ClientInstallReport {
    let skill_path = client.skill_path(root);
    let mut report = ClientInstallReport {
        client: client.name().to_string(),
        skill: FileInstall {
            path: skill_path.to_string_lossy().into_owned(),
            status: "failed".to_string(),
        },
        mcp: None,
        mcp_configured: false,
        next_step: None,
        warning: None,
        error: None,
    };
    let state = fs::read_to_string(&skill_path)
        .ok()
        .as_deref()
        .map(skill_state);
    let skill = match state {
        // Already this release's instructions. Leave the file exactly as it is,
        // so a source clone's tracked skill files are never rewritten just to
        // gain a stamp.
        Some(SkillState::Current) => Ok(FileInstall {
            path: skill_path.to_string_lossy().into_owned(),
            status: "unchanged".to_string(),
        }),
        // Upgrading Foremerge's own unedited file overwrites nothing the
        // operator wrote, so it does not require --force.
        state => write_managed(
            &skill_path,
            desired_skill().as_bytes(),
            force || state == Some(SkillState::Replaceable),
        ),
    };
    match skill {
        Ok(skill) => report.skill = skill,
        Err(error) => {
            report.error = Some(format!("{error:#}"));
            return report;
        }
    }
    match configure_client_mcp(root, client, foremerge_exe, force, configure_mcp) {
        Ok(outcome) => {
            report.mcp = outcome.install;
            report.mcp_configured = outcome.configured;
            report.next_step = outcome.next_step;
            report.warning = outcome.warning;
        }
        Err(error) => {
            report.error = Some(format!("{error:#}"));
        }
    }
    report
}

fn configure_client_mcp(
    root: &Path,
    client: Client,
    foremerge_exe: &Path,
    force: bool,
    configure_mcp: bool,
) -> Result<McpOutcome> {
    match client {
        Client::Codex => {
            if configure_mcp {
                configure_codex_mcp(root, foremerge_exe, force)
            } else {
                let (configured, warning) = match codex_mcp_configured() {
                    Ok(configured) => (configured, None),
                    Err(error) => (
                        false,
                        Some(format!("Could not probe the codex CLI: {error:#}")),
                    ),
                };
                Ok(McpOutcome {
                    install: None,
                    configured,
                    next_step: (!configured).then(|| {
                        format!(
                            "Run `codex mcp add foremerge -- {} mcp` to enable Foremerge tools.",
                            foremerge_exe.display()
                        )
                    }),
                    warning,
                })
            }
        }
        Client::Claude | Client::Cursor => {
            let path = client.mcp_path(root).expect("project MCP path");
            if configure_mcp {
                let change = merge_mcp_config(&path, root, foremerge_exe, force)?;
                Ok(McpOutcome {
                    install: Some(change),
                    configured: true,
                    next_step: None,
                    warning: None,
                })
            } else {
                let configured = mcp_json_configured(&path, root);
                Ok(McpOutcome {
                    install: None,
                    configured,
                    next_step: (!configured)
                        .then(|| format!("Configure Foremerge in {}.", path.display())),
                    warning: None,
                })
            }
        }
    }
}

fn diagnose_client(root: &Path, client: Client) -> ClientDiagnostic {
    let skill_path = client.skill_path(root);
    let skill_installed = skill_path.is_file();
    // Unreadable or non-UTF-8 content is never Foremerge's own file, so it is
    // treated as edited and kept behind --force.
    let skill_state = skill_installed
        .then(|| fs::read_to_string(&skill_path).ok())
        .map(|text| text.as_deref().map_or(SkillState::Edited, skill_state));
    let skill_current = skill_state == Some(SkillState::Current);
    let mcp_path = client.mcp_path(root);
    // Codex keeps its registration in user-global configuration, so a present
    // entry pointing elsewhere needs --force and a message naming both
    // repositories. Project JSON entries only need the generic --force step.
    let (mcp_configured, mcp_probe_error, mcp_entry_stale) = match mcp_path.as_deref() {
        Some(path) => (
            mcp_json_configured(path, root),
            None,
            mcp_json_entry_stale(path, root),
        ),
        None => match codex_mcp_entry() {
            Ok(Some(entry)) => match entry.registration() {
                CodexRegistration::Current => (true, None, false),
                // Setup upgrades this form on its own, so a plain `setup
                // codex` is the correct next step.
                CodexRegistration::Upgradable => (false, None, false),
                CodexRegistration::Foreign => (false, None, true),
            },
            Ok(None) => (false, None, false),
            Err(error) => (false, Some(format!("{error:#}")), false),
        },
    };
    // Setup refuses to replace managed content that differs from this release,
    // so offering a plain `setup` here would name a command that cannot
    // succeed. Report what actually blocks it and ask for --force instead.
    let mut blocked: Vec<&str> = Vec::new();
    if skill_state == Some(SkillState::Edited) {
        blocked.push("skill file");
    }
    if mcp_entry_stale {
        blocked.push(match client {
            Client::Codex => "Codex MCP registration",
            Client::Claude | Client::Cursor => "mcpServers.foremerge entry",
        });
    }
    let client_available = command_available(client.executable())
        || (client == Client::Cursor && command_available("cursor"));
    let ready = client_available && skill_current && mcp_configured;
    let next_step = if !client_available {
        Some(format!("Install the {} client.", client.name()))
    } else if let Some(error) = mcp_probe_error {
        Some(format!(
            "Could not probe the codex CLI ({error}); check that `codex` works, then run `foremerge doctor --client codex` again."
        ))
    } else if !blocked.is_empty() {
        Some(force_setup_step(client, &blocked))
    } else if !skill_current || !mcp_configured {
        Some(format!(
            "Run `foremerge setup {}` from this repository.",
            client.name()
        ))
    } else {
        None
    };
    ClientDiagnostic {
        client: client.name().to_string(),
        client_available,
        skill_path: skill_path.to_string_lossy().into_owned(),
        skill_installed,
        skill_current,
        mcp_path: mcp_path.map(|value| value.to_string_lossy().into_owned()),
        mcp_configured,
        ready,
        next_step,
    }
}

fn merge_mcp_config(
    path: &Path,
    root: &Path,
    foremerge_exe: &Path,
    force: bool,
) -> Result<FileInstall> {
    let mut document = if path.exists() {
        let bytes =
            fs::read(path).with_context(|| format!("read MCP config {}", path.display()))?;
        serde_json::from_slice::<Value>(&bytes)
            .with_context(|| format!("INVALID_INPUT: parse MCP config {}", path.display()))?
    } else {
        json!({})
    };
    let object = document.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "INVALID_INPUT: MCP config must contain a JSON object: {}",
            path.display()
        )
    })?;
    let servers = object
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("INVALID_INPUT: mcpServers must be a JSON object"))?;
    let desired = json!({
        "command": foremerge_exe.to_string_lossy(),
        "args": ["--cwd", root.to_string_lossy(), "mcp"]
    });
    if let Some(existing) = servers.get("foremerge") {
        if existing == &desired {
            return Ok(FileInstall {
                path: path.to_string_lossy().into_owned(),
                status: "unchanged".to_string(),
            });
        }
        if !force {
            if mcp_entry_current(existing, root) {
                return Ok(FileInstall {
                    path: path.to_string_lossy().into_owned(),
                    status: "unchanged".to_string(),
                });
            }
            bail!(
                "ALREADY_EXISTS: {} already defines mcpServers.foremerge as {}; inspect it or rerun with --force to replace it with the absolute installed path",
                path.display(),
                serde_json::to_string(existing)
                    .unwrap_or_else(|_| "an unserializable entry".to_string())
            );
        }
    }
    servers.insert("foremerge".to_string(), desired);
    let mut encoded = serde_json::to_vec_pretty(&document)?;
    encoded.push(b'\n');
    write_managed(path, &encoded, true)
}

fn configure_codex_mcp(_root: &Path, foremerge_exe: &Path, force: bool) -> Result<McpOutcome> {
    if !command_available("codex") {
        return Ok(McpOutcome {
            install: None,
            configured: false,
            next_step: Some(format!(
                "Run `codex mcp add foremerge -- {} mcp` after installing Codex CLI.",
                foremerge_exe.display()
            )),
            warning: None,
        });
    }
    // A probe failure must abort configuration: treating it as "no entry" would
    // silently replace a registration this process could not even read.
    let existing = codex_mcp_entry()?;
    if let Some(entry) = &existing {
        match entry.registration() {
            CodexRegistration::Current => {
                return Ok(McpOutcome {
                    install: Some(FileInstall {
                        path: CODEX_MCP_SCOPE.to_string(),
                        status: "unchanged".to_string(),
                    }),
                    configured: true,
                    next_step: None,
                    warning: None,
                });
            }
            // Foremerge's own pinned form. Replacing it frees Codex to serve
            // every repository, and destroys nothing an operator wrote.
            CodexRegistration::Upgradable => {}
            CodexRegistration::Foreign if !force => bail!(
                "ALREADY_EXISTS: Codex already defines a user-global `foremerge` MCP entry ({}); inspect it or rerun `foremerge setup codex --force` to replace it",
                entry.serialized()
            ),
            CodexRegistration::Foreign => {}
        }
        let mut remove = Command::new("codex");
        remove.args(["mcp", "remove", "foremerge"]);
        let output = run_bounded(remove)
            .map_err(|error| coded(error, "CHECK_FAILED", "run `codex mcp remove foremerge`"))?;
        if !output.success {
            bail!(
                "CHECK_FAILED: Codex could not replace the existing Foremerge MCP entry: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    // The Codex CLI has no atomic replace: the previous registration is
    // already gone at this point, so a failed add must say so and explain how
    // to restore it.
    let removed_note = existing.as_ref().map(|entry| {
        format!(
            "; note: the previous user-global registration ({}) was already removed; restore it with {}",
            entry.serialized(),
            entry.restore_command()
        )
    });
    // No --cwd: Codex spawns stdio servers in the directory it was launched
    // from, so one registration serves every repository.
    let mut add = Command::new("codex");
    add.args(["mcp", "add", "foremerge", "--"])
        .arg(foremerge_exe)
        .arg("mcp");
    let output = run_bounded(add).map_err(|error| {
        let error = coded(error, "CHECK_FAILED", "run `codex mcp add foremerge`");
        match removed_note.as_deref() {
            Some(note) => anyhow::anyhow!("{error:#}{note}"),
            None => error,
        }
    })?;
    if !output.success {
        bail!(
            "CHECK_FAILED: Codex could not register the Foremerge MCP server: {}{}",
            String::from_utf8_lossy(&output.stderr).trim(),
            removed_note.as_deref().unwrap_or("")
        );
    }
    let warning = existing
        .filter(|entry| matches!(entry.cwd, CodexCwd::Pinned(_)))
        .map(|entry| {
            let previous = match &entry.cwd {
                CodexCwd::Pinned(cwd) => cwd.display().to_string(),
                _ => "one repository".to_string(),
            };
            format!(
                "Codex was pinned to {previous} and is now portable: one registration serves every repository, resolved from the directory Codex is launched in."
            )
        });
    Ok(McpOutcome {
        install: Some(FileInstall {
            path: CODEX_MCP_SCOPE.to_string(),
            status: "written".to_string(),
        }),
        configured: true,
        next_step: None,
        warning,
    })
}

/// The Codex CLI's `foremerge` MCP entry, as far as it can be recovered from
/// `codex mcp get` output. `command` is `None` when only the plain-text form
/// was parseable.
/// How Codex's recorded `foremerge` registration relates to the portable one
/// setup writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexRegistration {
    /// The portable form: resolves its repository from the spawn directory.
    Current,
    /// Foremerge's own earlier form, pinned to one repository with `--cwd`.
    Upgradable,
    /// Anything else, including a disabled or unreadable entry. Never replaced
    /// without --force.
    Foreign,
}

#[derive(Debug)]
struct CodexEntry {
    enabled: bool,
    command: Option<String>,
    cwd: CodexCwd,
}

impl CodexEntry {
    /// How this registration relates to the one setup would write.
    ///
    /// Codex stores MCP registrations in user-global configuration, so a
    /// registration that names a repository can only ever serve that one.
    /// Setup registers the portable form instead, which carries no `--cwd` and
    /// resolves the repository from the working directory Codex spawns it in,
    /// the way git resolves a repository from where it is run.
    fn registration(&self) -> CodexRegistration {
        // A disabled registration does not serve tools, and an entry whose
        // command Codex did not report cannot be verified. Neither is safe to
        // replace on Foremerge's own authority.
        if !self.enabled {
            return CodexRegistration::Foreign;
        }
        let Some(command) = self.command.as_deref() else {
            return CodexRegistration::Foreign;
        };
        if !command_resolves_to_foremerge(command) {
            return CodexRegistration::Foreign;
        }
        match self.cwd {
            // The pre-portable form Foremerge itself used to write. Replacing
            // it destroys nothing an operator authored, and leaving it in place
            // would keep Codex pinned to one repository.
            CodexCwd::Pinned(_) => CodexRegistration::Upgradable,
            CodexCwd::Absent => CodexRegistration::Current,
            // Present but unusable. Calling this current would report a
            // registration Codex refuses to start as healthy, and it is not
            // the pinned form either, so repairing it stays an explicit
            // operator decision rather than something setup does silently.
            CodexCwd::Malformed => CodexRegistration::Foreign,
        }
    }

    fn serialized(&self) -> String {
        format!(
            "command: {}, cwd: {}",
            self.command.as_deref().unwrap_or("unknown"),
            match &self.cwd {
                CodexCwd::Pinned(cwd) => cwd.display().to_string(),
                CodexCwd::Absent => "unset".to_string(),
                CodexCwd::Malformed => "present but empty".to_string(),
            }
        )
    }

    fn restore_command(&self) -> String {
        match (self.command.as_deref(), &self.cwd) {
            (Some(command), CodexCwd::Pinned(cwd)) => {
                format!(
                    "`codex mcp add foremerge -- {command} --cwd {} mcp`",
                    cwd.display()
                )
            }
            _ => {
                "`codex mcp add foremerge -- <foremerge binary> --cwd <repository> mcp`".to_string()
            }
        }
    }
}

/// The Codex CLI's recorded `foremerge` MCP entry. Returns `Ok(Some(_))` when
/// an entry exists, `Ok(None)` only when its absence is verifiable, and `Err`
/// when the codex CLI could not be read at all. Callers must never treat a
/// probe failure as absence: doing so would bypass the second-repository
/// refusal and silently overwrite another repository's registration.
fn codex_mcp_entry() -> Result<Option<CodexEntry>> {
    let mut json_probe = Command::new("codex");
    json_probe.args(["mcp", "get", "foremerge", "--json"]);
    if let Ok(output) = run_bounded(json_probe) {
        if output.success {
            let parsed = serde_json::from_slice::<Value>(&output.stdout)
                .ok()
                .as_ref()
                .and_then(parse_codex_entry);
            if let Some(entry) = parsed {
                return Ok(Some(entry));
            }
        }
    }
    let mut text_probe = Command::new("codex");
    text_probe.args(["mcp", "get", "foremerge"]);
    let output = run_bounded(text_probe).map_err(|error| {
        coded(
            error,
            "CHECK_FAILED",
            "read the Codex MCP registration with `codex mcp get foremerge`; check that the codex CLI works, then re-run setup",
        )
    })?;
    if output.success {
        let text = String::from_utf8_lossy(&output.stdout);
        let parsed = serde_json::from_str::<Value>(&text)
            .ok()
            .as_ref()
            .and_then(parse_codex_entry);
        if let Some(entry) = parsed {
            return Ok(Some(entry));
        }
        let tokens: Vec<&str> = text.split_whitespace().collect();
        let cwd = match tokens.iter().position(|token| *token == "--cwd") {
            Some(position) => match tokens.get(position + 1) {
                Some(value) => parse_cwd(value),
                None => CodexCwd::Malformed,
            },
            None => CodexCwd::Absent,
        };
        // Plain output marks disabled registrations as "name (disabled)".
        return Ok(Some(CodexEntry {
            command: None,
            cwd,
            enabled: !text.contains("(disabled)"),
        }));
    }
    // `codex mcp get` reports a missing entry and a broken CLI the same way
    // (a nonzero exit), so absence is only verifiable through a successful
    // `codex mcp list` that does not name the entry.
    let mut list_probe = Command::new("codex");
    list_probe.args(["mcp", "list"]);
    let listed = run_bounded(list_probe).map_err(|error| {
        coded(
            error,
            "CHECK_FAILED",
            "confirm the Codex MCP registration state with `codex mcp list`; check that the codex CLI works, then re-run setup",
        )
    })?;
    if !listed.success {
        bail!(
            "CHECK_FAILED: `codex mcp get foremerge` and `codex mcp list` both failed ({}); check that the codex CLI works, then re-run setup",
            String::from_utf8_lossy(&listed.stderr).trim()
        );
    }
    let listing = String::from_utf8_lossy(&listed.stdout);
    let names_foremerge = listing
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || character == '_' || character == '-')
        })
        .any(|token| token == "foremerge");
    if names_foremerge {
        bail!(
            "CHECK_FAILED: Codex lists a `foremerge` MCP entry but `codex mcp get foremerge` could not read it; check that the codex CLI works, then re-run setup"
        );
    }
    Ok(None)
}

/// A `--cwd` carrying no value pins the registration to nothing. Reading it as
/// a path would make a malformed entry look like Foremerge's own pinned form
/// and so replaceable without --force. Reading it as *absent* is equally wrong
/// in the other direction: absent is the portable form setup writes, so the
/// entry would be reported as current when Codex will not start it at all.
fn parse_cwd(value: &str) -> CodexCwd {
    if value.trim().is_empty() {
        CodexCwd::Malformed
    } else {
        CodexCwd::Pinned(PathBuf::from(value))
    }
}

/// What a Codex registration's `--cwd` says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexCwd {
    /// No `--cwd` at all: the portable form, which resolves the repository
    /// from the directory Codex spawns the server in.
    Absent,
    /// A `--cwd` naming a directory: the pre-portable form Foremerge used to
    /// write, which serves only that one repository.
    Pinned(PathBuf),
    /// A `--cwd` present but carrying no usable value. Codex rejects it, so
    /// this registration cannot serve anything.
    Malformed,
}

fn parse_codex_entry(value: &Value) -> Option<CodexEntry> {
    let object = find_command_object(value)?;
    let command = object
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cwd = object
        .get("args")
        .and_then(Value::as_array)
        .map(
            |args| match args.iter().position(|arg| arg.as_str() == Some("--cwd")) {
                Some(position) => match args.get(position + 1).and_then(Value::as_str) {
                    Some(value) => parse_cwd(value),
                    // `--cwd` as the final argument has no value to take.
                    None => CodexCwd::Malformed,
                },
                None => CodexCwd::Absent,
            },
        )
        .unwrap_or(CodexCwd::Absent);
    Some(CodexEntry {
        command,
        cwd,
        enabled: find_enabled_flag(value).unwrap_or(true),
    })
}

/// Codex reports `"enabled": false` for disabled registrations; the flag can
/// sit above the transport object, so search the whole value. Absence means
/// enabled, matching Codex versions that predate the flag.
fn find_enabled_flag(value: &Value) -> Option<bool> {
    match value {
        Value::Object(map) => map
            .get("enabled")
            .and_then(Value::as_bool)
            .or_else(|| map.values().find_map(find_enabled_flag)),
        Value::Array(items) => items.iter().find_map(find_enabled_flag),
        _ => None,
    }
}

fn find_command_object(value: &Value) -> Option<&Map<String, Value>> {
    match value {
        Value::Object(map) => {
            if map.get("command").is_some_and(Value::is_string) {
                return Some(map);
            }
            map.values().find_map(find_command_object)
        }
        Value::Array(items) => items.iter().find_map(find_command_object),
        _ => None,
    }
}

/// Ok(true): a current, enabled registration exists. Ok(false): verifiably
/// absent, stale, or disabled. Err: the codex CLI could not be probed, which
/// callers must surface rather than treat as absence.
fn codex_mcp_configured() -> Result<bool> {
    Ok(
        matches!(codex_mcp_entry()?, Some(entry) if entry.registration() == CodexRegistration::Current),
    )
}

fn command_available(command: &str) -> bool {
    let mut probe = Command::new(command);
    probe.arg("--version");
    run_bounded(probe).is_ok_and(|output| output.success)
}

fn mcp_json_configured(path: &Path, root: &Path) -> bool {
    mcp_json_entry(path).is_some_and(|entry| mcp_entry_current(&entry, root))
}

/// The `mcpServers.foremerge` entry recorded in a project MCP file, if the file
/// exists, parses, and defines one.
fn mcp_json_entry(path: &Path) -> Option<Value> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("mcpServers")?.get("foremerge").cloned())
}

/// The corrective step when setup would refuse to replace managed content that
/// differs from this release. Naming `--force` is what separates this from the
/// plain step: without it the suggested command fails with ALREADY_EXISTS.
fn force_setup_step(client: Client, blocked: &[&str]) -> String {
    format!(
        "Run `foremerge setup {} --force` from this repository: setup will not replace the existing {} without it.",
        client.name(),
        blocked.join(" and ")
    )
}

/// True when the project MCP file already defines `mcpServers.foremerge` but
/// the entry is not current for this repository. Setup refuses to replace such
/// an entry without --force, so the diagnostic must ask for --force rather than
/// a plain `setup`.
fn mcp_json_entry_stale(path: &Path, root: &Path) -> bool {
    mcp_json_entry(path).is_some_and(|entry| !mcp_entry_current(&entry, root))
}

/// True only for an entry that is verifiably current for this repository: its
/// command is an absolute path to an existing `foremerge` binary, its last
/// argument is `mcp`, and any `--cwd` argument canonicalizes to `root`.
fn mcp_entry_current(entry: &Value, root: &Path) -> bool {
    let Some(command) = entry.get("command").and_then(Value::as_str) else {
        return false;
    };
    let Some(args) = entry.get("args").and_then(Value::as_array) else {
        return false;
    };
    if args.last().and_then(Value::as_str) != Some("mcp") || !args.iter().all(Value::is_string) {
        return false;
    }
    if !command_resolves_to_foremerge(command) {
        return false;
    }
    match args.iter().position(|arg| arg.as_str() == Some("--cwd")) {
        Some(position) => args
            .get(position + 1)
            .and_then(Value::as_str)
            .is_some_and(|cwd| paths_match(Path::new(cwd), root)),
        None => true,
    }
}

/// Only an absolute path to an existing file named `foremerge` is verifiably
/// current. A bare or relative command would be resolved in the MCP client's
/// own PATH and working directory, which need not match this process's, so
/// such entries are treated as not current; setup writes absolute paths, so
/// legitimate installs still pass, and `--force` normalizes an unverifiable
/// entry to the absolute installed path.
fn command_resolves_to_foremerge(command: &str) -> bool {
    let path = Path::new(command);
    path.is_absolute()
        && path.file_name().and_then(|value| value.to_str()) == Some("foremerge")
        && path.is_file()
}

fn paths_match(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn write_managed(path: &Path, content: &[u8], force: bool) -> Result<FileInstall> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "INVALID_INPUT: managed integration path must be a regular file: {}",
                    path.display()
                );
            }
            let existing = fs::read(path)?;
            if existing == content {
                return Ok(FileInstall {
                    path: path.to_string_lossy().into_owned(),
                    status: "unchanged".to_string(),
                });
            }
            if !force {
                bail!(
                    "ALREADY_EXISTS: {} already exists with different content; inspect it or rerun with --force",
                    path.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow::Error::from(error))
                .with_context(|| format!("inspect {}", path.display()));
        }
    }
    let parent = path.parent().context("integration file parent")?;
    fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(".foremerge-{}.tmp", Uuid::new_v4().simple()));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(content)?;
        file.sync_all()?;
        fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result?;
    Ok(FileInstall {
        path: path.to_string_lossy().into_owned(),
        status: "written".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_integrations_merge_without_clobbering_other_servers() {
        let temp = tempfile::tempdir().unwrap();
        let cursor_config = temp.path().join(".cursor/mcp.json");
        fs::create_dir_all(cursor_config.parent().unwrap()).unwrap();
        fs::write(
            &cursor_config,
            br#"{"mcpServers":{"existing":{"command":"existing"}},"custom":true}"#,
        )
        .unwrap();
        let reports = install(
            temp.path(),
            &[Client::Claude, Client::Cursor],
            Path::new("/usr/local/bin/foremerge"),
            false,
            true,
        );
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().all(|report| report.error.is_none()));
        assert_eq!(
            fs::read_to_string(temp.path().join(".claude/skills/foremerge/SKILL.md")).unwrap(),
            desired_skill()
        );
        let cursor: Value = serde_json::from_slice(&fs::read(cursor_config).unwrap()).unwrap();
        assert_eq!(cursor["custom"], true);
        assert_eq!(cursor["mcpServers"]["existing"]["command"], "existing");
        assert_eq!(
            cursor["mcpServers"]["foremerge"]["command"],
            "/usr/local/bin/foremerge"
        );
        let second = install(
            temp.path(),
            &[Client::Claude, Client::Cursor],
            Path::new("/usr/local/bin/foremerge"),
            false,
            true,
        );
        assert!(
            second
                .iter()
                .all(|report| report.skill.status == "unchanged" && report.error.is_none())
        );
    }

    #[test]
    fn only_an_absolute_existing_foremerge_binary_is_verifiably_current() {
        // Bare and relative commands resolve in the MCP client's own PATH and
        // working directory, not this process's, so they are never blessed.
        assert!(!command_resolves_to_foremerge("foremerge"));
        assert!(!command_resolves_to_foremerge("target/debug/foremerge"));
        assert!(!command_resolves_to_foremerge("/nonexistent/foremerge"));
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("foremerge");
        fs::write(&path, b"#!/bin/sh\n").unwrap();
        assert!(command_resolves_to_foremerge(&path.to_string_lossy()));
        let other = temp.path().join("not-foremerge");
        fs::write(&other, b"#!/bin/sh\n").unwrap();
        assert!(!command_resolves_to_foremerge(&other.to_string_lossy()));
    }

    #[cfg(unix)]
    #[test]
    fn run_bounded_is_not_hung_by_a_background_descendant_holding_the_pipes() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "echo probe-ok; sleep 30 & exit 0"]);
        let started = Instant::now();
        let output = run_bounded_with_timeout(command, Duration::from_secs(3)).unwrap();
        assert!(output.success);
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "probe-ok");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "capture must be bounded even when a descendant holds the pipes; took {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_bounded_kills_a_probe_that_exceeds_its_deadline() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let started = Instant::now();
        let error = run_bounded_with_timeout(command, Duration::from_secs(1)).unwrap_err();
        assert!(format!("{error:#}").starts_with("RESOURCE_LIMIT:"));
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "the deadline must bound the probe; took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_portable_codex_entry_is_current_and_a_pinned_one_upgrades() {
        // The form setup writes: no --cwd, so it serves whichever repository
        // Codex is launched in.
        let portable = serde_json::json!({
            "name": "foremerge",
            "enabled": true,
            "transport": { "command": "/usr/local/bin/foremerge", "args": ["mcp"] },
        });
        // command_resolves_to_foremerge requires the binary to exist, so point
        // at one that does.
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("foremerge");
        fs::write(&exe, b"#!/bin/sh\n").unwrap();
        let mut entry = parse_codex_entry(&portable).expect("entry parses");
        entry.command = Some(exe.to_string_lossy().into_owned());
        assert_eq!(entry.registration(), CodexRegistration::Current);

        // Foremerge's earlier pinned form upgrades without --force.
        let pinned = CodexEntry {
            command: Some(exe.to_string_lossy().into_owned()),
            cwd: CodexCwd::Pinned(PathBuf::from("/somewhere/else")),
            enabled: true,
        };
        assert_eq!(pinned.registration(), CodexRegistration::Upgradable);
    }

    #[test]
    fn a_cwd_with_no_value_does_not_read_as_a_pinned_registration() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("foremerge");
        fs::write(&exe, b"#!/bin/sh\n").unwrap();

        // A `--cwd` carrying no value pins the entry to nothing. Reading it as
        // Pinned("") would classify a malformed entry as Foremerge's own pinned
        // form, which setup replaces without --force. Absent is wrong the other
        // way: it is the portable form, so a registration Codex cannot start
        // would be reported as current and left alone.
        for empty in ["", "   "] {
            let json = serde_json::json!({
                "transport": {
                    "command": exe.to_string_lossy(),
                    "args": ["--cwd", empty, "mcp"],
                },
            });
            let entry = parse_codex_entry(&json).expect("entry parses");
            assert_eq!(
                entry.cwd,
                CodexCwd::Malformed,
                "an empty --cwd is neither a path nor the portable form: {empty:?}"
            );
            assert_eq!(
                entry.registration(),
                CodexRegistration::Foreign,
                "a registration Codex will not start must not be reported as current"
            );
        }

        // A real path still reads as pinned.
        let pinned = serde_json::json!({
            "transport": {
                "command": exe.to_string_lossy(),
                "args": ["--cwd", "/some/repo", "mcp"],
            },
        });
        let entry = parse_codex_entry(&pinned).expect("entry parses");
        assert_eq!(entry.cwd, CodexCwd::Pinned(PathBuf::from("/some/repo")));
        assert_eq!(entry.registration(), CodexRegistration::Upgradable);
    }

    #[test]
    fn disabled_unverifiable_or_foreign_codex_entries_need_force() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("foremerge");
        fs::write(&exe, b"#!/bin/sh\n").unwrap();

        // A registration the operator disabled must not be silently re-added.
        let disabled = CodexEntry {
            command: Some(exe.to_string_lossy().into_owned()),
            cwd: CodexCwd::Absent,
            enabled: false,
        };
        assert_eq!(disabled.registration(), CodexRegistration::Foreign);

        // An entry whose command Codex did not report cannot be verified.
        let unverifiable = CodexEntry {
            command: None,
            cwd: CodexCwd::Absent,
            enabled: true,
        };
        assert_eq!(unverifiable.registration(), CodexRegistration::Foreign);

        // Someone else's `foremerge` entry pointing at a different program.
        let foreign = CodexEntry {
            command: Some("/usr/bin/env".to_string()),
            cwd: CodexCwd::Absent,
            enabled: true,
        };
        assert_eq!(foreign.registration(), CodexRegistration::Foreign);

        let no_flag = serde_json::json!({
            "transport": { "command": "foremerge", "args": [] },
        });
        assert!(parse_codex_entry(&no_flag).expect("entry parses").enabled);
    }

    #[test]
    fn a_foreign_codex_registration_is_reported_as_needing_force() {
        let step = force_setup_step(Client::Codex, &["Codex MCP registration"]);
        assert!(
            step.contains("`foremerge setup codex --force`"),
            "step must name --force: {step}"
        );
        assert!(
            step.contains("Codex MCP registration"),
            "step must name the Codex registration, not a project JSON entry: {step}"
        );
    }

    #[test]
    fn stale_managed_content_asks_for_force_and_names_what_blocks_setup() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join(".mcp.json");

        // An absent file and a current entry must not demand --force.
        assert!(!mcp_json_entry_stale(&path, &root));
        let exe = root.join("foremerge");
        fs::write(&exe, b"#!/bin/sh\n").unwrap();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "mcpServers": {
                    "foremerge": {
                        "command": exe.to_string_lossy(),
                        "args": ["--cwd", root.to_string_lossy(), "mcp"],
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            !mcp_json_entry_stale(&path, &root),
            "a current entry is replaced in place, so it must not demand --force"
        );

        // An entry pointing at another repository is refused without --force.
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "mcpServers": {
                    "foremerge": {
                        "command": exe.to_string_lossy(),
                        "args": ["--cwd", "/somewhere/else", "mcp"],
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(mcp_json_entry_stale(&path, &root));

        let step = force_setup_step(
            Client::Claude,
            &["skill file", "mcpServers.foremerge entry"],
        );
        assert!(
            step.contains("`foremerge setup claude --force`"),
            "step must name --force: {step}"
        );
        assert!(
            step.contains("skill file and mcpServers.foremerge entry"),
            "step must name every blocking artifact: {step}"
        );
    }

    #[test]
    fn installed_skill_carries_a_stamp_over_this_release_body() {
        let installed = desired_skill();
        assert_eq!(
            skill_body(&installed),
            skill_body(SKILL_CONTENT),
            "stamping must not alter the instructions themselves"
        );
        assert_eq!(
            skill_stamp_digest(&installed),
            Some(skill_digest(skill_body(SKILL_CONTENT)).as_str()),
            "the stamp must record the digest of the body it follows"
        );
        assert!(installed.contains(env!("CARGO_PKG_VERSION")));
        assert_eq!(
            skill_state(&installed),
            SkillState::Current,
            "the bytes setup writes must read back as current"
        );
        // Stamping is a fixed point. If a source clone's tracked file ever ends
        // up stamped, canonicalizing it must strip that stamp rather than nest
        // a second one inside the body.
        let body = skill_body(&installed);
        let restamped = format!(
            "{body}{SKILL_STAMP_PREFIX}version=9.9.9 sha256={} {SKILL_STAMP_SUFFIX}\n",
            skill_digest(body)
        );
        assert_eq!(skill_body(&restamped), skill_body(SKILL_CONTENT));
    }

    #[test]
    fn an_unedited_older_skill_upgrades_without_force() {
        let body = "# Foremerge\n\nOld instructions from an earlier release.\n";
        let stamped = format!(
            "{body}{SKILL_STAMP_PREFIX}version=0.0.1 sha256={} {SKILL_STAMP_SUFFIX}\n",
            skill_digest(body)
        );
        assert_eq!(skill_state(&stamped), SkillState::Replaceable);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".claude/skills/foremerge/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &stamped).unwrap();
        let reports = install(
            temp.path(),
            &[Client::Claude],
            Path::new("/usr/local/bin/foremerge"),
            false,
            false,
        );
        assert!(
            reports[0].error.is_none(),
            "an unedited older file must upgrade without --force: {:?}",
            reports[0].error
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), desired_skill());
    }

    #[test]
    fn an_edited_skill_still_requires_force() {
        let body = "# Foremerge\n\nOld instructions from an earlier release.\n";
        let edited = format!(
            "{body}A line the operator added.\n{SKILL_STAMP_PREFIX}version=0.0.1 sha256={} {SKILL_STAMP_SUFFIX}\n",
            skill_digest(body)
        );
        assert_eq!(
            skill_state(&edited),
            SkillState::Edited,
            "a body that no longer matches its own stamp was edited"
        );

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".claude/skills/foremerge/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &edited).unwrap();
        let reports = install(
            temp.path(),
            &[Client::Claude],
            Path::new("/usr/local/bin/foremerge"),
            false,
            false,
        );
        assert!(
            reports[0]
                .error
                .as_deref()
                .is_some_and(|error| error.starts_with("ALREADY_EXISTS:")),
            "edited content must stay behind --force: {:?}",
            reports[0].error
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            edited,
            "the operator's file must be left byte-for-byte intact"
        );
    }

    #[test]
    fn an_unstamped_body_matching_this_release_is_left_alone() {
        // A source clone's tracked skill files carry no stamp. Setup must
        // recognize them as current rather than rewriting tracked content.
        assert_eq!(skill_state(SKILL_CONTENT), SkillState::Current);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".claude/skills/foremerge/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, SKILL_CONTENT).unwrap();
        let reports = install(
            temp.path(),
            &[Client::Claude],
            Path::new("/usr/local/bin/foremerge"),
            false,
            false,
        );
        assert!(reports[0].error.is_none());
        assert_eq!(reports[0].skill.status, "unchanged");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            SKILL_CONTENT,
            "an unstamped current body must not be rewritten"
        );
    }

    #[test]
    fn coded_preserves_typed_error_codes_through_context() {
        let wrapped = coded(
            anyhow::anyhow!("RESOURCE_LIMIT: codex did not finish within 10 seconds"),
            "CHECK_FAILED",
            "run `codex mcp add foremerge`",
        );
        assert_eq!(
            format!("{wrapped:#}"),
            "RESOURCE_LIMIT: run `codex mcp add foremerge`: codex did not finish within 10 seconds"
        );
        let fallback = coded(
            anyhow::anyhow!("no such file"),
            "CHECK_FAILED",
            "run `codex mcp add foremerge`",
        );
        assert!(format!("{fallback:#}").starts_with("CHECK_FAILED: "));
    }

    /// An unstamped file carries no provenance of its own, so content is the
    /// only evidence available. Matching nothing Foremerge ever shipped means
    /// somebody wrote it, and the README promises that survives an upgrade.
    #[test]
    fn an_unstamped_custom_skill_is_not_replaced_without_force() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".claude/skills/foremerge/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "custom instructions").unwrap();
        assert_eq!(skill_state("custom instructions"), SkillState::Edited);
        let reports = install(
            temp.path(),
            &[Client::Claude],
            Path::new("foremerge"),
            false,
            false,
        );
        assert!(
            reports[0]
                .error
                .as_deref()
                .is_some_and(|error| error.starts_with("ALREADY_EXISTS:")),
            "unstamped custom content must stay behind --force: {:?}",
            reports[0].error
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "custom instructions",
            "an upgrade must not discard instructions somebody wrote"
        );

        // --force is how the operator says it may go.
        let forced = install(
            temp.path(),
            &[Client::Claude],
            Path::new("foremerge"),
            true,
            false,
        );
        assert!(forced[0].error.is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), desired_skill());
    }

    /// The pre-stamp installed base still has to upgrade cleanly: a file whose
    /// body is exactly what an earlier release shipped is provably untouched,
    /// so refreshing it is not an overwrite and must not demand --force.
    #[test]
    fn a_released_pre_stamp_skill_still_upgrades_in_place() {
        for digest in PRE_STAMP_SKILL_DIGESTS {
            assert_eq!(
                digest.len(),
                64,
                "a pre-stamp digest must be a full SHA-256: {digest}"
            );
        }
        let shipped = std::process::Command::new("git")
            .args(["show", "v0.1.0:.codex/skills/foremerge/SKILL.md"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output();
        let Ok(shipped) = shipped else { return };
        if !shipped.status.success() {
            return;
        }
        let body = String::from_utf8(shipped.stdout).expect("skill text is UTF-8");
        assert_eq!(
            skill_state(&body),
            SkillState::Replaceable,
            "the v0.1.0 body must still be recognised as Foremerge's own"
        );
    }
}
