//! `setup` and `doctor` must agree on every platform, for every client, under
//! every name the binary is installed as. 0.5.0 shipped a doctor that rejected
//! the registration its own setup wrote whenever the binary was not named
//! exactly `foremerge`: always on Windows (`foremerge.exe`), and everywhere
//! for `fmg`. The setup and doctor tests in `e2e.rs` use a shell-script Codex
//! stub, so they only run on Unix, and nothing caught it.
//!
//! This file runs on Linux, macOS and Windows. It uses its own harness
//! (`harness = false`) so the Codex stub can be a real executable on every
//! platform: the test binary copies itself in as `codex`, `claude` and
//! `cursor`, and when started under one of those names it acts as that CLI
//! instead of running tests. Windows resolves a bare command to `name.exe`
//! only, so a script stub would never be found there.

use serde_json::{Value, json};
use std::env::consts::EXE_SUFFIX;
use std::ffi::OsStr;
use std::fs;
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Output};
use std::sync::OnceLock;

const CLIENTS: [&str; 3] = ["claude", "cursor", "codex"];

/// Where the copied client stubs live, set once by `main`.
static STUB_DIR: OnceLock<PathBuf> = OnceLock::new();

type Test = (&'static str, fn());

const TESTS: [Test; 2] = [
    (
        "setup_then_doctor_agree_for_every_client_and_binary_name",
        setup_then_doctor_agree_for_every_client_and_binary_name,
    ),
    (
        "doctors_force_step_repairs_a_stale_registration_under_every_name",
        doctors_force_step_repairs_a_stale_registration_under_every_name,
    ),
];

fn main() {
    let rest: Vec<String> = std::env::args().skip(1).collect();
    // The copies are real files, so the running executable's own name says
    // which CLI to be. argv[0] is whatever the caller typed (`codex`, or a
    // resolved path), which differs by platform.
    let this = std::env::current_exe().expect("locate the running executable");
    match this.file_stem().and_then(OsStr::to_str) {
        Some("codex") => process::exit(codex_stub(&rest)),
        Some("claude" | "cursor") => process::exit(version_only_stub(&rest)),
        _ => {}
    }

    let options = Options::parse(&rest);
    let selected: Vec<Test> = TESTS
        .into_iter()
        .filter(|(name, _)| options.selects(name))
        .collect();
    if options.list {
        for (name, _) in &selected {
            println!("{name}: test");
        }
        return;
    }

    let stubs = tempfile::tempdir().expect("create stub directory");
    install_stubs(stubs.path());
    STUB_DIR
        .set(stubs.path().to_path_buf())
        .expect("stub directory is set once");

    println!("\nrunning {} tests", selected.len());
    let mut failed = Vec::new();
    for (name, test) in &selected {
        let outcome = panic::catch_unwind(test);
        println!(
            "test {name} ... {}",
            if outcome.is_ok() { "ok" } else { "FAILED" }
        );
        if outcome.is_err() {
            failed.push(*name);
        }
    }
    println!(
        "\ntest result: {}. {} passed; {} failed",
        if failed.is_empty() { "ok" } else { "FAILED" },
        selected.len() - failed.len(),
        failed.len()
    );
    // process::exit skips destructors, so remove the stubs first.
    drop(stubs);
    if !failed.is_empty() {
        process::exit(1);
    }
}

/// The subset of libtest's command line this harness honours, so `cargo test`
/// and runners built on it select the same tests they would elsewhere.
#[derive(Default)]
struct Options {
    filters: Vec<String>,
    skips: Vec<String>,
    exact: bool,
    list: bool,
    /// Nothing here is `#[ignore]`d, so a run or listing of only ignored tests
    /// selects nothing.
    ignored_only: bool,
}

impl Options {
    fn parse(args: &[String]) -> Options {
        let mut options = Options::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--list" => options.list = true,
                "--exact" => options.exact = true,
                "--ignored" => options.ignored_only = true,
                "--skip" => options.skips.extend(args.next().cloned()),
                // libtest options that take a value as the next argument.
                "--test-threads" | "--format" | "--color" | "--logfile" | "-Z"
                | "--shuffle-seed" | "--report-time" => {
                    args.next();
                }
                flag if flag.starts_with('-') => {}
                filter => options.filters.push(filter.to_string()),
            }
        }
        options
    }

    fn selects(&self, name: &str) -> bool {
        let matches = |pattern: &String| {
            if self.exact {
                name == pattern
            } else {
                name.contains(pattern.as_str())
            }
        };
        !self.ignored_only
            && (self.filters.is_empty() || self.filters.iter().any(matches))
            && !self.skips.iter().any(matches)
    }
}

// ---------------------------------------------------------------------------
// Client stubs
// ---------------------------------------------------------------------------

fn install_stubs(directory: &Path) {
    let this = std::env::current_exe().expect("locate the test binary");
    for client in CLIENTS {
        fs::copy(&this, directory.join(format!("{client}{EXE_SUFFIX}")))
            .unwrap_or_else(|error| panic!("install the {client} stub: {error}"));
    }
}

/// `claude --version` and `cursor --version` are all doctor asks of them;
/// their MCP registrations are repository files Foremerge reads itself.
fn version_only_stub(args: &[String]) -> i32 {
    if args == ["--version"] {
        println!("client-stub 0.0.0");
        0
    } else {
        1
    }
}

/// The `codex mcp` subcommands Foremerge calls, backed by one JSON file named
/// by `CODEX_STUB_STATE`. Mirrors the Unix shell stub in `e2e.rs`.
fn codex_stub(args: &[String]) -> i32 {
    let Some(state) = std::env::var_os("CODEX_STUB_STATE").map(PathBuf::from) else {
        eprintln!("codex-stub: CODEX_STUB_STATE is not set");
        return 2;
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["--version"] => {
            println!("codex-stub 0.0.0");
            0
        }
        ["mcp", "get", "foremerge", ..] => match fs::read_to_string(&state) {
            Ok(entry) => {
                print!("{entry}");
                0
            }
            Err(_) => 1,
        },
        ["mcp", "list"] => {
            if state.is_file() {
                println!("foremerge");
            }
            0
        }
        ["mcp", "add", "foremerge", rest @ ..] => {
            let rest = match rest {
                ["--", rest @ ..] => rest,
                rest => rest,
            };
            let [command, args @ ..] = rest else {
                return 1;
            };
            let entry = json!({ "command": command, "args": args });
            fs::write(&state, entry.to_string()).map_or(1, |()| 0)
        }
        ["mcp", "remove", "foremerge"] => {
            let _ = fs::remove_file(&state);
            0
        }
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    temp: tempfile::TempDir,
    root: PathBuf,
    codex_state: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let temp = tempfile::tempdir().expect("create fixture directory");
        let root = temp.path().join("repo");
        fs::create_dir(&root).expect("create repository directory");
        let codex_state = temp.path().join("codex-mcp.json");
        let fixture = Fixture {
            temp,
            root,
            codex_state,
        };
        for args in [
            &["init", "--quiet"][..],
            &["config", "user.name", "Foremerge Test"],
            &["config", "user.email", "foremerge-test@example.invalid"],
            &["config", "commit.gpgsign", "false"],
            &[
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "initial fixture",
            ],
        ] {
            let output = fixture.command("git").args(args).output().expect("run git");
            assert!(output.status.success(), "git {args:?}: {output:?}");
        }
        fixture
    }

    /// A command that finds the client stubs first on PATH, keeps the Codex
    /// registration inside this fixture, and cannot see the real home
    /// directory's installations.
    fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let stubs = STUB_DIR.get().expect("stubs are installed").clone();
        let mut paths = vec![stubs];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("PATH", std::env::join_paths(paths).expect("join PATH"))
            .env("CODEX_STUB_STATE", &self.codex_state)
            .env("HOME", self.temp.path())
            .env("USERPROFILE", self.temp.path());
        command
    }

    fn foremerge(&self, binary: &Path, args: &[&str]) -> Value {
        let output = self
            .command(binary)
            .arg("--json")
            .arg("--cwd")
            .arg(&self.root)
            .args(args)
            .output()
            .expect("run foremerge");
        let value = parse(&output);
        assert!(
            output.status.success() && value["ok"] == true,
            "{} {args:?} failed: {value}",
            binary.display()
        );
        value
    }

    /// Doctor run under the same name that ran setup. Under the other name it
    /// rightly warns that the client launches a binary it did not run: it
    /// cannot tell an `fmg` from this build from one an older install left
    /// behind without running it.
    fn doctor(&self, binary: &Path, client: &str) -> Value {
        let doctor = self.foremerge(binary, &["doctor", "--client", client]);
        doctor["data"]["clients"][0].clone()
    }

    /// The `foremerge` MCP command this client is registered to launch.
    fn registered_command(&self, client: &str) -> Option<String> {
        let file = match client {
            "codex" => self.codex_state.clone(),
            "claude" => self.root.join(".mcp.json"),
            "cursor" => self.root.join(".cursor/mcp.json"),
            other => panic!("unknown client {other}"),
        };
        let value: Value = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
        let entry = if client == "codex" {
            &value
        } else {
            &value["mcpServers"]["foremerge"]
        };
        entry["command"].as_str().map(str::to_string)
    }

    /// Writes a registration that names a binary that no longer exists, the
    /// state a deleted or moved install leaves behind.
    fn write_stale_registration(&self, client: &str) {
        let gone = self
            .temp
            .path()
            .join("uninstalled")
            .join(format!("foremerge{EXE_SUFFIX}"));
        let entry = json!({ "command": gone.to_string_lossy(), "args": ["mcp"] });
        let (file, value) = match client {
            "codex" => (self.codex_state.clone(), entry),
            "claude" => (
                self.root.join(".mcp.json"),
                json!({ "mcpServers": { "foremerge": entry } }),
            ),
            "cursor" => (
                self.root.join(".cursor/mcp.json"),
                json!({ "mcpServers": { "foremerge": entry } }),
            ),
            other => panic!("unknown client {other}"),
        };
        fs::create_dir_all(file.parent().expect("registration has a directory"))
            .expect("create registration directory");
        fs::write(file, value.to_string()).expect("write stale registration");
    }
}

fn parse(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not one JSON value: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn foremerge_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_foremerge"))
}

/// Every name the binary is installed under.
fn binaries() -> [&'static Path; 2] {
    [foremerge_bin(), Path::new(env!("CARGO_BIN_EXE_fmg"))]
}

/// Whether two paths name the same file. Windows runners hand out temp paths
/// in 8.3 short form, so compare canonical forms rather than strings.
fn same_file(left: &Path, right: &Path) -> bool {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    canonical(left) == canonical(right)
}

fn assert_registers(fixture: &Fixture, client: &str, binary: &Path) {
    let registered = fixture
        .registered_command(client)
        .unwrap_or_else(|| panic!("{client}: no registration was written"));
    assert!(
        same_file(Path::new(&registered), binary),
        "{client}: setup registers the binary that ran it; expected {}, got {registered}",
        binary.display()
    );
}

/// Doctor, run under `doctor_binary`, reports `client` fully set up.
fn assert_configured(fixture: &Fixture, client: &str, doctor_binary: &Path) -> Value {
    let diagnostic = fixture.doctor(doctor_binary, client);
    let context = format!(
        "{client}, doctor run as {}: {diagnostic}",
        doctor_binary.display()
    );
    assert_eq!(diagnostic["mcp_configured"], true, "{context}");
    assert_eq!(diagnostic["skill_current"], true, "{context}");
    assert_eq!(diagnostic["ready"], true, "{context}");
    assert_eq!(diagnostic["next_step"], Value::Null, "{context}");
    diagnostic
}

/// Set up and diagnosed under the same name: nothing left to do or warn about.
fn assert_ready(fixture: &Fixture, client: &str, binary: &Path) {
    let diagnostic = assert_configured(fixture, client, binary);
    assert_eq!(
        diagnostic["warning"],
        Value::Null,
        "{client}, set up and diagnosed as {}: {diagnostic}",
        binary.display()
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn setup_then_doctor_agree_for_every_client_and_binary_name() {
    for binary in binaries() {
        for client in CLIENTS {
            let fixture = Fixture::new();
            let setup = fixture.foremerge(binary, &["setup", client]);
            let report = &setup["data"]["clients"][0];
            assert_eq!(report["mcp_configured"], true, "{client}: {report}");
            assert_eq!(report["error"], Value::Null, "{client}: {report}");
            assert_registers(&fixture, client, binary);
            assert_ready(&fixture, client, binary);

            // Setup again must recognise its own registration rather than
            // refusing it as someone else's (ALREADY_EXISTS on Windows in 0.5.0).
            let again = fixture.foremerge(binary, &["setup", client]);
            let report = &again["data"]["clients"][0];
            assert_eq!(report["mcp"]["status"], "unchanged", "{client}: {report}");
            assert_eq!(report["error"], Value::Null, "{client}: {report}");
            assert_ready(&fixture, client, binary);
        }
    }
}

/// Doctor must never recommend a command that, once run, leaves its verdict
/// unchanged. 0.5.0 broke this on Windows: doctor asked for `setup --force`,
/// which rewrote the same `.exe` path, which doctor rejected again.
fn doctors_force_step_repairs_a_stale_registration_under_every_name() {
    for binary in binaries() {
        for client in CLIENTS {
            let fixture = Fixture::new();
            fixture.foremerge(binary, &["setup", client, "--skip-mcp"]);
            fixture.write_stale_registration(client);

            let diagnostic = fixture.doctor(binary, client);
            assert_eq!(diagnostic["ready"], false, "{client}: {diagnostic}");
            let step = diagnostic["next_step"]
                .as_str()
                .unwrap_or_else(|| panic!("{client}: doctor names no next step: {diagnostic}"));
            let suggested = step
                .split('`')
                .nth(1)
                .unwrap_or_else(|| panic!("{client}: next step names no command: {step}"));
            let mut words = suggested.split_whitespace();
            assert_eq!(words.next(), Some("foremerge"), "{client}: {step}");
            let args: Vec<&str> = words.collect();
            assert!(args.contains(&"--force"), "{client}: {step}");

            // Run exactly what doctor said. The step names `foremerge` whichever
            // name doctor ran under, so that is the binary a user would run.
            fixture.foremerge(foremerge_bin(), &args);
            assert_registers(&fixture, client, foremerge_bin());
            assert_ready(&fixture, client, foremerge_bin());

            // Doctor under the name that gave the advice must agree the
            // registration is repaired. Under `fmg` it also says, rightly,
            // that the client now launches a different binary than itself.
            let diagnostic = assert_configured(&fixture, client, binary);
            if binary != foremerge_bin() {
                let warning = diagnostic["warning"].as_str().unwrap_or_else(|| {
                    panic!("{client}: doctor as fmg must name the binary the client launches: {diagnostic}")
                });
                let registered = fixture
                    .registered_command(client)
                    .expect("the repaired registration is readable");
                assert!(warning.contains(&registered), "{client}: {warning}");
            }
        }
    }
}
