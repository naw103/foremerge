use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
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
    match write_managed(&skill_path, SKILL_CONTENT.as_bytes(), force) {
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
                let (configured, warning) = match codex_mcp_configured(root) {
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
                            "Run `codex mcp add foremerge -- {} --cwd {} mcp` to enable Foremerge tools.",
                            foremerge_exe.display(),
                            root.display()
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
    let skill_bytes = fs::read(&skill_path).ok();
    let skill_installed = skill_bytes.is_some();
    let skill_current = skill_bytes.as_deref() == Some(SKILL_CONTENT.as_bytes());
    let mcp_path = client.mcp_path(root);
    // Codex keeps its registration in user-global configuration, so a present
    // entry pointing elsewhere needs --force and a message naming both
    // repositories. Project JSON entries only need the generic --force step.
    let (mcp_configured, mcp_probe_error, mcp_repoint_step, mcp_entry_stale) =
        match mcp_path.as_deref() {
            Some(path) => (
                mcp_json_configured(path, root),
                None,
                None,
                mcp_json_entry_stale(path, root),
            ),
            None => match codex_mcp_entry() {
                Ok(Some(entry)) if entry.is_current_for(root) => (true, None, None, false),
                Ok(Some(entry)) => (false, None, Some(codex_repoint_step(&entry, root)), false),
                Ok(None) => (false, None, None, false),
                Err(error) => (false, Some(format!("{error:#}")), None, false),
            },
        };
    // Setup refuses to replace managed content that differs from this release,
    // so offering a plain `setup` here would name a command that cannot
    // succeed. Report what actually blocks it and ask for --force instead.
    let mut blocked: Vec<&str> = Vec::new();
    if skill_installed && !skill_current {
        blocked.push("skill file");
    }
    if mcp_entry_stale {
        blocked.push("mcpServers.foremerge entry");
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
    } else if let Some(step) = mcp_repoint_step {
        Some(step)
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

fn configure_codex_mcp(root: &Path, foremerge_exe: &Path, force: bool) -> Result<McpOutcome> {
    if !command_available("codex") {
        return Ok(McpOutcome {
            install: None,
            configured: false,
            next_step: Some(format!(
                "Run `codex mcp add foremerge -- {} --cwd {} mcp` after installing Codex CLI.",
                foremerge_exe.display(),
                root.display()
            )),
            warning: None,
        });
    }
    // A probe failure must abort configuration: treating it as "no entry"
    // would skip the second-repository refusal below and silently repoint a
    // registration this process could not even read.
    let existing = codex_mcp_entry()?;
    if let Some(entry) = &existing {
        if entry.is_current_for(root) {
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
        if !force {
            bail!(
                "ALREADY_EXISTS: Codex MCP configuration is user-global and its `foremerge` entry currently points at {}; rerun `foremerge setup codex --force` to repoint Codex at {}",
                entry.target_description(),
                root.display()
            );
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
    let mut add = Command::new("codex");
    add.args(["mcp", "add", "foremerge", "--"])
        .arg(foremerge_exe)
        .arg("--cwd")
        .arg(root)
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
    let warning = existing.map(|entry| {
        let mut message = format!(
            "Codex MCP registration is user-global: Codex now coordinates {}",
            root.display()
        );
        match entry.cwd {
            Some(previous) if !paths_match(&previous, root) => {
                message.push_str(&format!(
                    "; re-run `foremerge setup codex` in {} to switch back.",
                    previous.display()
                ));
            }
            _ => message.push('.'),
        }
        message
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
#[derive(Debug)]
struct CodexEntry {
    enabled: bool,
    command: Option<String>,
    cwd: Option<PathBuf>,
}

impl CodexEntry {
    fn is_current_for(&self, root: &Path) -> bool {
        // A disabled registration does not serve tools, and an entry whose
        // command Codex did not report cannot be verified; neither counts as
        // current, so setup offers --force and doctor reports not configured.
        if !self.enabled {
            return false;
        }
        let Some(cwd) = self.cwd.as_deref() else {
            return false;
        };
        if !paths_match(cwd, root) {
            return false;
        }
        match self.command.as_deref() {
            Some(command) => command_resolves_to_foremerge(command),
            None => false,
        }
    }

    /// How this registration's target reads in operator-facing messages.
    /// Shared by setup's refusal and doctor's next step so both name the same
    /// repository instead of drifting apart.
    fn target_description(&self) -> String {
        self.cwd.as_deref().map_or_else(
            || "a different or stale target".to_string(),
            |cwd| format!("repository {}", cwd.display()),
        )
    }

    fn serialized(&self) -> String {
        format!(
            "command: {}, cwd: {}",
            self.command.as_deref().unwrap_or("unknown"),
            self.cwd
                .as_deref()
                .map_or_else(|| "unknown".to_string(), |cwd| cwd.display().to_string())
        )
    }

    fn restore_command(&self) -> String {
        match (self.command.as_deref(), self.cwd.as_deref()) {
            (Some(command), Some(cwd)) => {
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
        let cwd = tokens
            .iter()
            .position(|token| *token == "--cwd")
            .and_then(|position| tokens.get(position + 1))
            .map(PathBuf::from);
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

fn parse_codex_entry(value: &Value) -> Option<CodexEntry> {
    let object = find_command_object(value)?;
    let command = object
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cwd = object
        .get("args")
        .and_then(Value::as_array)
        .and_then(|args| {
            let position = args.iter().position(|arg| arg.as_str() == Some("--cwd"))?;
            args.get(position + 1)?.as_str().map(PathBuf::from)
        });
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

/// The corrective step for a Codex registration that exists but does not serve
/// `root`. Such an entry is refused by a plain `foremerge setup codex`, so the
/// step must name `--force` and the repository Codex is currently registered
/// for; otherwise the diagnostic sends the operator to a command that cannot
/// succeed.
fn codex_repoint_step(entry: &CodexEntry, root: &Path) -> String {
    format!(
        "Codex MCP configuration is user-global and its `foremerge` entry currently points at {}; run `foremerge setup codex --force` from this repository to repoint Codex at {}.",
        entry.target_description(),
        root.display()
    )
}

/// Ok(true): a current, enabled registration exists. Ok(false): verifiably
/// absent, stale, or disabled. Err: the codex CLI could not be probed, which
/// callers must surface rather than treat as absence.
fn codex_mcp_configured(root: &Path) -> Result<bool> {
    Ok(matches!(codex_mcp_entry()?, Some(entry) if entry.is_current_for(root)))
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
            fs::read(temp.path().join(".claude/skills/foremerge/SKILL.md")).unwrap(),
            SKILL_CONTENT.as_bytes()
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
    fn disabled_or_unverifiable_codex_entries_are_not_current() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let disabled = serde_json::json!({
            "name": "foremerge",
            "enabled": false,
            "transport": {
                "command": "/usr/local/bin/foremerge",
                "args": ["--cwd", root.to_string_lossy(), "mcp"],
            },
        });
        let entry = parse_codex_entry(&disabled).expect("entry parses");
        assert!(!entry.enabled);
        assert!(!entry.is_current_for(&root));

        let unverifiable = CodexEntry {
            command: None,
            cwd: Some(root.clone()),
            enabled: true,
        };
        assert!(!unverifiable.is_current_for(&root));

        let no_flag = serde_json::json!({
            "transport": { "command": "foremerge", "args": [] },
        });
        assert!(parse_codex_entry(&no_flag).expect("entry parses").enabled);
    }

    #[test]
    fn codex_diagnostic_step_for_another_repository_names_force() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let elsewhere = PathBuf::from("/somewhere/else");
        let entry = CodexEntry {
            command: Some("/usr/local/bin/foremerge".to_string()),
            cwd: Some(elsewhere.clone()),
            enabled: true,
        };
        assert!(!entry.is_current_for(&root));

        let step = codex_repoint_step(&entry, &root);
        // A plain `setup codex` refuses this entry, so the diagnostic must not
        // offer it as the next step.
        assert!(
            step.contains("`foremerge setup codex --force`"),
            "step must name --force: {step}"
        );
        assert!(
            step.contains(&elsewhere.display().to_string()),
            "step must name the repository Codex currently serves: {step}"
        );
        assert!(
            step.contains(&root.display().to_string()),
            "step must name the repository being repointed to: {step}"
        );
    }

    #[test]
    fn codex_target_description_survives_an_unreadable_cwd() {
        let unknown = CodexEntry {
            command: Some("/usr/local/bin/foremerge".to_string()),
            cwd: None,
            enabled: true,
        };
        assert_eq!(
            unknown.target_description(),
            "a different or stale target",
            "an entry whose cwd codex did not report still needs --force"
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

    #[test]
    fn skill_overwrite_requires_force() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".claude/skills/foremerge/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "custom instructions").unwrap();
        let refused = install(
            temp.path(),
            &[Client::Claude],
            Path::new("foremerge"),
            false,
            false,
        );
        assert!(
            refused[0]
                .error
                .as_deref()
                .is_some_and(|error| error.starts_with("ALREADY_EXISTS"))
        );
        assert_eq!(refused[0].skill.status, "failed");
        let forced = install(
            temp.path(),
            &[Client::Claude],
            Path::new("foremerge"),
            true,
            false,
        );
        assert!(forced[0].error.is_none());
        assert_eq!(fs::read(path).unwrap(), SKILL_CONTENT.as_bytes());
    }
}
