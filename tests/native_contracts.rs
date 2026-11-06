#![forbid(unsafe_code)]
#![cfg(feature = "test-support")]

//! Behavioral contracts shared by Windows, macOS and Linux. The fixture is a
//! native executable: none of these tests needs a shell or an installed scanner.

use project_check::MAX_OUTPUT_BYTES;
use project_check::process::{CaptureFailure, Signals, capture};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const HELPER: &str = env!("CARGO_BIN_EXE_project-check-contract-child");

fn child(arguments: &[&str]) -> Command {
    let mut command = Command::new(HELPER);
    command.args(arguments);
    command
}

fn capture_child(
    arguments: &[&str],
    limit: usize,
    merge: bool,
) -> project_check::process::Captured {
    capture(
        &mut child(arguments),
        Duration::from_secs(10),
        limit,
        &Signals::none(),
        merge,
    )
    .unwrap()
}

#[test]
fn native_capture_preserves_argv_exit_status_and_stream_order() {
    let arguments = [
        "argv",
        "two words",
        "Grüße 東京",
        "",
        "$HOME;`x`&|<>%PATH%",
        "a\"b\\",
    ];
    let result = capture_child(&arguments, 4096, true);
    assert_eq!(result.failure, None);
    assert_eq!(result.returncode, Some(0));
    assert_eq!(
        serde_json::from_slice::<Value>(&result.output).unwrap(),
        json!(&arguments[1..])
    );

    let result = capture_child(&["exit", "23", "stdout", "stderr"], 4096, false);
    assert_eq!(result.failure, None);
    assert_eq!(result.returncode, Some(23));
    assert_eq!(result.output, b"stdout");
    assert_eq!(result.stderr, b"stderr");

    let result = capture_child(&["ordered"], 4096, true);
    assert_eq!(result.output, b"firstsecondthird");
    assert!(result.stderr.is_empty());
}

#[test]
fn native_capture_enforces_exact_and_combined_output_boundaries() {
    for extra in [0, 1] {
        let count = (MAX_OUTPUT_BYTES + extra).to_string();
        let result = capture_child(&["bytes", &count, "out"], MAX_OUTPUT_BYTES, true);
        assert_eq!(result.output.len(), MAX_OUTPUT_BYTES);
        assert_eq!(
            result.failure,
            (extra == 1).then_some(CaptureFailure::OutputLimit)
        );
        if extra == 0 {
            assert_eq!(result.returncode, Some(0));
        }
    }
    let result = capture_child(&["bytes", "6", "split"], 8, false);
    assert_eq!(result.failure, Some(CaptureFailure::OutputLimit));
    assert_eq!(result.output.len() + result.stderr.len(), 8);
}

#[test]
fn native_timeout_cleans_descendants_and_inherited_pipes() {
    for mode in ["wait", "exit"] {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("heartbeat");
        let started = Instant::now();
        let result = capture(
            &mut child(&["tree", marker.to_str().unwrap(), mode]),
            Duration::from_secs(2),
            1024,
            &Signals::none(),
            true,
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            marker.exists(),
            "descendant must start before cleanup is tested"
        );
        if mode == "wait" {
            assert_eq!(result.failure, Some(CaptureFailure::Timeout));
        } else {
            assert_eq!(result.failure, None);
            assert_eq!(result.returncode, Some(0));
        }
        let length = fs::metadata(&marker).unwrap().len();
        thread::sleep(Duration::from_millis(250));
        assert_eq!(
            fs::metadata(&marker).unwrap().len(),
            length,
            "descendant survived owned process cleanup"
        );
    }
}

#[test]
#[cfg(unix)]
fn native_timeout_still_applies_after_both_output_pipes_close() {
    let started = Instant::now();
    let result = capture(
        &mut child(&["close-output", "30000"]),
        Duration::from_millis(200),
        1024,
        &Signals::none(),
        true,
    )
    .unwrap();
    assert_eq!(result.failure, Some(CaptureFailure::Timeout));
    assert!(started.elapsed() < Duration::from_secs(3));
}

fn executable(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn copy_executable(source: &Path, target: &Path) {
    if fs::hard_link(source, target).is_err() {
        fs::copy(source, target).unwrap();
    }
}

struct Package {
    directory: tempfile::TempDir,
    project: PathBuf,
    binary: PathBuf,
    docs: PathBuf,
}

impl Package {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().join("package with spaces ü");
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/project-check")).unwrap();
        let binary = prefix.join("bin").join(executable("project-check"));
        let docs = prefix.join("bin").join(executable("project-docs"));
        copy_executable(Path::new(env!("CARGO_BIN_EXE_project-check")), &binary);
        copy_executable(Path::new(env!("CARGO_BIN_EXE_project-docs")), &docs);
        let scanner = prefix.join("bin").join(executable("scanner"));
        copy_executable(Path::new(HELPER), &scanner);
        let rules = prefix.join("share/project-check/rules");
        fs::create_dir(&rules).unwrap();
        fs::write(rules.join("fixture.yml"), "fixture scanner rules").unwrap();
        let ca = prefix.join("share/project-check/ca.pem");
        fs::write(&ca, "fixture CA path").unwrap();
        fs::write(
            prefix.join("share/project-check/tools.json"),
            json!({"version":1,"scanner":scanner,"rules":rules,"docs":docs,"ca":ca}).to_string(),
        )
        .unwrap();
        let project = directory.path().join("project with spaces 東京");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("fixture.py"), "value = 1\n").unwrap();
        Self {
            directory,
            project,
            binary,
            docs,
        }
    }

    fn check(&self, name: &str, arguments: &[&str]) -> Value {
        let mut argv = vec![HELPER];
        argv.extend_from_slice(arguments);
        json!({"name":name,"argv":argv,"requires":[HELPER],"profiles":["fast","full"],"timeout_seconds":10})
    }

    fn manifest(&self, checks: &[Value]) {
        fs::write(
            self.project.join(".project-checks.json"),
            json!({"version":2,"checks":checks,"watch_ignore":["started","count"]}).to_string(),
        )
        .unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .current_dir(&self.project)
            .env("XDG_CACHE_HOME", self.directory.path().join("cache"));
        command
    }

    fn report(&self, args: &[&str]) -> (i32, Value) {
        let output = self.command(args).output().unwrap();
        let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid report: {error}; stdout={:?}; stderr={:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), report)
    }
}

#[test]
fn installed_native_runner_plans_without_execution_and_runs_literal_arguments() {
    let package = Package::new();
    let marker = package.project.join("must-not-exist");
    package.manifest(&[package.check("fixture", &["write", marker.to_str().unwrap(), "ran"])]);
    let (code, report) = package.report(&["fast", "--plan", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(report["status"], "ready");
    assert!(!marker.exists());

    let arguments = [
        "argv",
        "two words",
        "Grüße 東京",
        "",
        "$HOME;`x`&|<>%PATH%",
        "a\"b\\",
    ];
    package.manifest(&[package.check("fixture", &arguments)]);
    let (code, report) = package.report(&["fast", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(report["status"], "passed");
    assert_eq!(report["checks"][0]["name"], "baseline");
    assert_eq!(report["checks"][0]["scanned_files"], 1);
    let output = report["checks"][1]["diagnostics"].as_str().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(output.trim()).unwrap(),
        json!(&arguments[1..])
    );
}

#[test]
fn portable_bundle_keeps_working_after_its_entire_prefix_is_relocated() {
    let mut package = Package::new();
    let prefix = package.binary.parent().unwrap().parent().unwrap();
    fs::write(prefix.join("share/project-check/tools.json"), json!({
        "version":2,"scanner":format!("bin/{}", executable("scanner")),
        "rules":"share/project-check/rules","docs":format!("bin/{}", executable("project-docs")),
        "ca":null
    }).to_string()).unwrap();
    let relocated = package.directory.path().join("relocated package 東京");
    fs::rename(prefix, &relocated).unwrap();
    package.binary = relocated.join("bin").join(executable("project-check"));
    package.docs = relocated.join("bin").join(executable("project-docs"));
    package.manifest(&[package.check("fixture", &["sleep", "0"])]);
    let (code, report) = package.report(&["fast", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(report["status"], "passed");
    assert_eq!(report["checks"][0]["scanned_files"], 1);
    assert!(
        report["checks"][0]["rules_package"]
            .as_str()
            .unwrap()
            .contains("relocated package")
    );
}

#[test]
#[cfg(windows)]
fn windows_runner_refuses_implicit_batch_shell_execution() {
    let package = Package::new();
    fs::write(
        package.project.join("implicit.cmd"),
        "@echo escaped>escaped\r\n",
    )
    .unwrap();
    let mut check = package.check("batch", &["sleep", "0"]);
    check["argv"] = json!([".\\implicit.cmd"]);
    package.manifest(&[check]);
    let (code, report) = package.report(&["fast", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(report["checks"][1]["status"], "blocked");
    assert!(!package.project.join("escaped").exists());
}

#[test]
fn installed_runner_retains_failure_blockers_and_runs_remaining_checks() {
    let package = Package::new();
    let marker = package.project.join("later");
    let mut blocked = package.check("missing", &["sleep", "0"]);
    blocked["requires"] = json!(["project-check-deliberately-missing-tool"]);
    let mut timeout = package.check("timeout", &["sleep", "30000"]);
    timeout["timeout_seconds"] = json!(0.2);
    package.manifest(&[
        package.check("failure", &["exit", "23", "claimed success", ""]),
        blocked,
        timeout,
        package.check("later", &["write", marker.to_str().unwrap(), "completed"]),
    ]);
    let (code, report) = package.report(&["fast", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(report["status"], "failed");
    assert_eq!(report["checks"][1]["returncode"], 23);
    assert_eq!(report["checks"][2]["status"], "blocked");
    assert_eq!(report["checks"][3]["timed_out"], true);
    assert_eq!(report["checks"][4]["status"], "passed");
    assert_eq!(fs::read_to_string(marker).unwrap(), "completed");
}

#[test]
fn installed_baseline_never_passes_missing_malformed_or_finding_reports() {
    let package = Package::new();
    for (mode, expected) in [
        ("missing", "blocked"),
        ("malformed", "blocked"),
        ("finding", "failed"),
    ] {
        let output = package
            .command(&["baseline", "--json"])
            .env("CONTRACT_SCANNER_MODE", mode)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["status"], expected);
        assert_eq!(report["checks"][0]["status"], expected);
    }
}

#[test]
fn native_renderer_previews_and_atomically_replaces_existing_documentation() {
    let package = Package::new();
    fs::write(
        package.project.join("package.json"),
        "{\"version\":\"1.0\"}",
    )
    .unwrap();
    fs::write(
        package.project.join("README.md.in"),
        "Version {{package.version}}\n",
    )
    .unwrap();
    fs::write(
        package.project.join(".dependency-docs.json"),
        json!({
            "version":1,"sources":{"package":{"kind":"json","path":"package.json"}},
            "templates":[{"source":"README.md.in","target":"README.md"}]
        })
        .to_string(),
    )
    .unwrap();
    for version in ["1.0", "2.0"] {
        fs::write(
            package.project.join("package.json"),
            json!({"version":version}).to_string(),
        )
        .unwrap();
        let previous = fs::read(package.project.join("README.md")).ok();
        let preview = Command::new(&package.docs)
            .current_dir(&package.project)
            .output()
            .unwrap();
        assert_eq!(preview.status.code(), Some(1));
        assert_eq!(fs::read(package.project.join("README.md")).ok(), previous);
        let output = Command::new(&package.docs)
            .arg("--write")
            .current_dir(&package.project)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(package.project.join("README.md")).unwrap(),
            format!("Version {version}\n")
        );
    }
    assert!(
        Command::new(&package.docs)
            .current_dir(&package.project)
            .status()
            .unwrap()
            .success()
    );
    assert!(fs::read_dir(&package.project).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".project-check-")
    }));
}

#[test]
fn native_schema_migration_previews_then_replaces_without_changing_warning_policy() {
    let package = Package::new();
    let check = package.check("fixture", &["sleep", "0"]);
    let mut original = json!({"version":1,"checks":[check]});
    original["checks"][0]["input_paths"] = json!(["fixture.py"]);
    let bytes = serde_json::to_vec(&original).unwrap();
    let manifest = package.project.join(".project-checks.json");
    fs::write(&manifest, &bytes).unwrap();
    let (code, report) = package.report(&["migrate", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(report["status"], "outdated");
    assert_eq!(fs::read(&manifest).unwrap(), bytes);
    let (code, _) = package.report(&["migrate", "--write", "--json"]);
    assert_eq!(code, 0);
    let migrated: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(migrated["version"], project_check::manifest::SCHEMA_VERSION);
    assert_eq!(migrated["warnings"], "fail");
    assert_eq!(migrated["checks"], original["checks"]);
}

#[test]
fn native_git_change_plan_keeps_unicode_rename_paths_without_running_checks() {
    let package = Package::new();
    let marker = package.project.join("must-not-run");
    let mut check = package.check("fixture", &["write", marker.to_str().unwrap(), "ran"]);
    check["input_paths"] = json!(["fixture.py", "renamed ü.py", "untracked 東京.py"]);
    package.manifest(&[check]);
    let hooks = package.directory.path().join("empty-hooks");
    fs::create_dir(&hooks).unwrap();
    for arguments in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec!["commit", "--quiet", "--no-gpg-sign", "-m", "native fixture"],
    ] {
        let output = Command::new("git")
            .current_dir(&package.project)
            .args([
                "-c",
                "user.name=Contract Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
            ])
            .arg(format!("core.hooksPath={}", hooks.display()))
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::rename(
        package.project.join("fixture.py"),
        package.project.join("renamed ü.py"),
    )
    .unwrap();
    assert!(
        Command::new("git")
            .current_dir(&package.project)
            .args(["add", "--all"])
            .status()
            .unwrap()
            .success()
    );
    fs::write(package.project.join("untracked 東京.py"), "value = 2\n").unwrap();
    let (code, report) = package.report(&["fast", "--plan", "--changed", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(report["status"], "ready");
    assert_eq!(report["change_scope"]["total"], 3);
    assert_eq!(report["checks"][1]["matched_total"], 3);
    assert_eq!(
        report["checks"][1]["matched_examples"],
        json!(["fixture.py", "renamed ü.py", "untracked 東京.py"])
    );
    assert!(!marker.exists());
}

struct Running {
    child: Child,
    reports: Receiver<Value>,
}

impl Running {
    fn start(mut command: Command) -> Self {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let (sender, reports) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                if let Ok(report) = serde_json::from_str(&line)
                    && sender.send(report).is_err()
                {
                    break;
                }
            }
        });
        Self { child, reports }
    }
    fn report(&self) -> Value {
        self.reports
            .recv_timeout(Duration::from_secs(15))
            .expect("watch report")
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn native_watch_recovers_invalid_manifest_and_batches_changes_during_a_check() {
    let package = Package::new();
    fs::write(package.project.join(".project-checks.json"), "incomplete").unwrap();
    let running = Running::start(package.command(&["watch", "--json"]));
    assert_eq!(running.report()["status"], "blocked");
    let count = package.project.join("count");
    let started = package.project.join("started");
    package.manifest(&[package.check(
        "fixture",
        &[
            "watch-check",
            count.to_str().unwrap(),
            started.to_str().unwrap(),
            "300",
        ],
    )]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.exists() {
        assert!(Instant::now() < deadline, "watch check did not start");
        thread::sleep(Duration::from_millis(10));
    }
    fs::write(package.project.join("source.txt"), "one").unwrap();
    fs::write(package.project.join("source.txt"), "two").unwrap();
    assert_eq!(running.report()["status"], "passed");
    assert_eq!(running.report()["status"], "passed");
    assert!(
        running
            .reports
            .recv_timeout(Duration::from_millis(900))
            .is_err(),
        "unexpected extra watch run"
    );
    assert_eq!(fs::read_to_string(count).unwrap(), "xx");
}
