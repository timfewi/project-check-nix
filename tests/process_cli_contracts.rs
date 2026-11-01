#![forbid(unsafe_code)]

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use project_check::MAX_OUTPUT_BYTES;
use project_check::process::{CaptureFailure, Signals, capture};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

fn program(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("test needs {name}"))
}
fn shell(code: &str) -> Command {
    let mut command = Command::new(program("sh"));
    command.args(["-c", code]);
    command
}
fn script(path: &Path, code: &str) {
    fs::write(path, format!("#!{}\n{code}\n", program("sh").display())).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn capture_preserves_exact_boundary_and_fails_on_one_extra_byte() {
    for (extra, failure) in [(false, None), (true, Some(CaptureFailure::OutputLimit))] {
        let code = format!(
            "dd if=/dev/zero bs=1048576 count=4 status=none; {}",
            if extra { "printf x" } else { ":" }
        );
        let result = capture(
            &mut shell(&code),
            Duration::from_secs(5),
            MAX_OUTPUT_BYTES,
            &Signals::none(),
            true,
        )
        .unwrap();
        assert_eq!(result.output.len(), MAX_OUTPUT_BYTES);
        assert_eq!(result.failure, failure);
    }
}

#[test]
fn closed_output_and_inherited_pipes_remain_bounded() {
    for code in ["exec 1>&- 2>&-; sleep 30", "sleep 30 & exit 0"] {
        let started = Instant::now();
        let result = capture(
            &mut shell(code),
            Duration::from_millis(150),
            1024,
            &Signals::none(),
            true,
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        if code.starts_with("exec") {
            assert_eq!(result.failure, Some(CaptureFailure::Timeout));
        } else {
            assert_eq!(result.returncode, Some(0));
            assert!(result.failure.is_none());
        }
    }
}

#[test]
fn merged_output_order_split_streams_and_total_limit() {
    let result = capture(
        &mut shell("printf first; printf second >&2; printf third"),
        Duration::from_secs(2),
        1024,
        &Signals::none(),
        true,
    )
    .unwrap();
    assert_eq!(result.output, b"firstsecondthird");
    let result = capture(
        &mut shell("printf 123456; printf abcdef >&2"),
        Duration::from_secs(2),
        8,
        &Signals::none(),
        false,
    )
    .unwrap();
    assert_eq!(result.failure, Some(CaptureFailure::OutputLimit));
    assert_eq!(result.output.len() + result.stderr.len(), 8);
}

struct Package {
    root: tempfile::TempDir,
    project: PathBuf,
    binary: PathBuf,
}
impl Package {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let prefix = root.path().join("package");
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/project-check")).unwrap();
        for (name, source) in [
            ("project-check", env!("CARGO_BIN_EXE_project-check")),
            ("project-docs", env!("CARGO_BIN_EXE_project-docs")),
        ] {
            let target = prefix.join("bin").join(name);
            if fs::hard_link(source, &target).is_err() {
                fs::copy(source, &target).unwrap();
            }
        }
        let scanner = root.path().join("scanner");
        script(
            &scanner,
            "while [ \"$#\" -gt 0 ]; do if [ \"$1\" = --json-output ]; then shift; report=$1; fi; shift; done\nprintf '%s' '{\"paths\":{\"scanned\":[\"fixture.py\"]},\"results\":[],\"errors\":[]}' > \"$report\"",
        );
        let rules = root.path().join("rules");
        fs::create_dir(&rules).unwrap();
        fs::write(prefix.join("share/project-check/tools.json"),json!({"version":1,"scanner":scanner,"rules":rules,"docs":prefix.join("bin/project-docs"),"ca":root.path().join("synthetic-ca")}).to_string()).unwrap();
        let project = root.path().join("project with spaces");
        fs::create_dir(&project).unwrap();
        Self {
            binary: prefix.join("bin/project-check"),
            root,
            project,
        }
    }
    fn manifest(&self, code: &str) {
        fs::write(self.project.join(".project-checks.json"),json!({"version":2,"checks":[{"name":"fixture","argv":[program("sh"),"-c",code],"requires":[program("sh")],"profiles":["fast","full"],"timeout_seconds":5}],"watch_ignore":["started","child.pid","count"]}).to_string()).unwrap();
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .current_dir(&self.project)
            .env("XDG_CACHE_HOME", self.root.path().join("cache"));
        command
    }
    fn report(&self, args: &[&str]) -> (i32, Value) {
        let output = self.command(args).output().unwrap();
        let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid report: {error}; {:?}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        (output.status.code().unwrap(), report)
    }
}

#[test]
fn native_cli_plan_run_profiles_json_and_usage_contract() {
    let package = Package::new();
    package.manifest("printf 'literal ; $(false)' ");
    let (code, plan) = package.report(&["fast", "--plan", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(plan["status"], "ready");
    assert_eq!(plan["mode"], "plan");
    let (code, run) = package.report(&["fast", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(run["status"], "passed");
    assert_eq!(run["checks"][1]["diagnostics"], "literal ; $(false)");
    let (code, run) = package.report(&["full", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(run["profile"], "full");
    for args in [
        vec![],
        vec!["watch", "--plan"],
        vec!["migrate", "--plan"],
        vec!["fast", "--changed"],
        vec!["fast", "--write"],
    ] {
        assert_eq!(
            package.command(&args).output().unwrap().status.code(),
            Some(2)
        );
    }
    assert!(
        package
            .command(&["--help"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn malformed_manifest_reports_json_without_running_a_child() {
    let package = Package::new();
    package.manifest("touch should-not-exist");
    let path = package.project.join(".project-checks.json");
    let mut manifest: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    manifest["checks"][0]["cwd"] = json!("bad\0cwd");
    fs::write(&path, manifest.to_string()).unwrap();
    let (code, report) = package.report(&["fast", "--plan", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(report["status"], "blocked");
    assert_eq!(report["checks"], json!([]));
    assert!(!package.project.join("should-not-exist").exists());
    fs::write(&path, "{\"version\":2,\"version\":1}").unwrap();
    let (code, report) = package.report(&["fast", "--json"]);
    assert_eq!(code, 1);
    assert!(
        report["diagnostics"]
            .as_str()
            .unwrap()
            .contains("duplicate")
    );
    fs::write(&path, "not a manifest").unwrap();
    let (code, report) = package.report(&["baseline", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(report["checks"].as_array().unwrap().len(), 1);
}

#[test]
fn documentation_companion_is_native_and_plan_never_writes() {
    let package = Package::new();
    package.manifest(":");
    fs::write(
        package.project.join("package.json"),
        "{\"version\":\"2.0\"}",
    )
    .unwrap();
    fs::write(
        package.project.join("readme.md.in"),
        "Version {{package.version}}\n",
    )
    .unwrap();
    fs::write(package.project.join(".dependency-docs.json"),json!({"version":1,"sources":{"package":{"kind":"json","path":"package.json"}},"templates":[{"source":"readme.md.in","target":"README.md"}]}).to_string()).unwrap();
    let (_, report) = package.report(&["fast", "--plan", "--json"]);
    assert_eq!(report["checks"][1]["name"], "dependency-docs");
    assert!(!package.project.join("README.md").exists());
    let (code, report) = package.report(&["fast", "--json"]);
    assert_eq!(code, 0, "{report}");
    assert_eq!(
        fs::read_to_string(package.project.join("README.md")).unwrap(),
        "Version 2.0\n"
    );
    assert_eq!(report["checks"][1]["status"], "passed");
}

struct Running {
    child: Child,
    lines: Receiver<String>,
    known_child: Option<i32>,
}
impl Running {
    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            known_child: None,
        }
    }
    fn report(&self) -> Value {
        serde_json::from_str(
            &self
                .lines
                .recv_timeout(Duration::from_secs(5))
                .expect("CLI report deadline"),
        )
        .unwrap()
    }
    fn signal_and_wait(&mut self, signal: Signal) -> i32 {
        kill(Pid::from_raw(self.child.id() as i32), signal).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().unwrap();
            }
            assert!(Instant::now() < deadline, "CLI did not stop");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = killpg(Pid::from_raw(self.child.id() as i32), Signal::SIGKILL);
            let _ = self.child.wait();
        }
        if let Some(pid) = self.known_child {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}
fn wait_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.metadata().is_ok_and(|metadata| metadata.len() > 0) {
        assert!(Instant::now() < deadline, "fixture marker deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn running(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .is_some_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, tail)| !tail.trim_start().starts_with('Z'))
        })
}

#[test]
fn sigint_and_sigterm_clean_up_the_owned_check_group() {
    for (signal, code) in [(Signal::SIGINT, 130), (Signal::SIGTERM, 143)] {
        let package = Package::new();
        package.manifest("sleep 30 & echo $! > child.pid; printf started > started; wait");
        let mut command = package.command(&["fast", "--json"]);
        command.stdin(Stdio::null());
        let mut running_cli = Running::spawn(command);
        wait_path(&package.project.join("child.pid"));
        let pid = fs::read_to_string(package.project.join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        running_cli.known_child = Some(pid);
        assert_eq!(running_cli.signal_and_wait(signal), code);
        assert!(!running(pid), "owned check child survived signal");
    }
}

#[test]
fn watch_starts_blocked_recovers_and_batches_edits_during_a_run() {
    let package = Package::new();
    fs::write(package.project.join(".project-checks.json"), "incomplete").unwrap();
    let mut running_cli = Running::spawn(package.command(&["watch", "--json"]));
    assert_eq!(running_cli.report()["status"], "blocked");
    package.manifest("printf x >> count; printf started > started; sleep 0.3");
    wait_path(&package.project.join("started"));
    fs::write(package.project.join("source"), "one").unwrap();
    fs::write(package.project.join("source"), "two").unwrap();
    assert_eq!(running_cli.report()["status"], "passed");
    assert_eq!(running_cli.report()["status"], "passed");
    assert!(
        running_cli
            .lines
            .recv_timeout(Duration::from_millis(900))
            .is_err(),
        "unexpected extra watch rerun"
    );
    assert_eq!(
        fs::read_to_string(package.project.join("count")).unwrap(),
        "xx"
    );
    fs::write(
        package.project.join(".project-checks.json"),
        "temporarily invalid",
    )
    .unwrap();
    assert_eq!(running_cli.report()["status"], "blocked");
    package.manifest("printf x >> count; printf started > started; sleep 0.3");
    assert_eq!(running_cli.report()["status"], "passed");
    assert_eq!(
        fs::read_to_string(package.project.join("count")).unwrap(),
        "xxx"
    );
    assert_eq!(running_cli.signal_and_wait(Signal::SIGINT), 130);
}

#[test]
fn timeout_warning_evidence_is_retained_and_later_checks_still_run() {
    let package = Package::new();
    package.manifest("printf 'warning: fixture\\n'; exec 1>&- 2>&-; sleep 30");
    let path = package.project.join(".project-checks.json");
    let mut document: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    document["checks"][0]["timeout_seconds"] = json!(0.1);
    document["checks"][0]["expected_warnings"] =
        json!([{"message":"warning: fixture","reason":"Deliberate test warning.","max_count":1}]);
    let mut last = document["checks"][0].clone();
    last["name"] = json!("last");
    last["argv"] = json!([program("sh"), "-c", "printf done > last-executed"]);
    last["timeout_seconds"] = json!(2);
    document["checks"].as_array_mut().unwrap().push(last);
    fs::write(path, document.to_string()).unwrap();
    let (code, report) = package.report(&["fast", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(report["checks"][1]["status"], "failed");
    assert_eq!(report["checks"][1]["timed_out"], true);
    assert!(report["checks"][1]["returncode"].is_null());
    assert_eq!(report["checks"][1]["expected_warnings"][0]["count"], 1);
    assert_eq!(report["checks"][2]["status"], "passed");
    assert!(package.project.join("last-executed").exists());
}

#[test]
fn invalid_utf8_child_output_is_replaced_not_panicked() {
    let package = Package::new();
    package.manifest("printf '\\377'");
    let (code, report) = package.report(&["fast", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(report["checks"][1]["diagnostics"], "\u{fffd}");
}

#[test]
fn non_utf8_cli_arguments_are_usage_errors_not_panics() {
    let package = Package::new();
    for binary in [
        &package.binary,
        &package.binary.with_file_name("project-docs"),
    ] {
        let output = Command::new(binary)
            .arg(std::ffi::OsString::from_vec(vec![0xff]))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("must be UTF-8"));
    }
}

fn assert_renderer_depth_rejected(
    kind: &str,
    path: &str,
    valid: &str,
    oversized: &str,
    symbol: Option<&str>,
) {
    let root = tempfile::tempdir().unwrap();
    let mut source = json!({"kind": kind, "path": path});
    if let Some(symbol) = symbol {
        source["symbol"] = json!(symbol);
    }
    let manifest = json!({
        "version": 1,
        "sources": {"source": source},
        "templates": [{"source": "template.md.in", "target": "output.md"}]
    });
    fs::write(
        root.path().join(".dependency-docs.json"),
        manifest.to_string(),
    )
    .unwrap();
    fs::write(root.path().join("template.md.in"), "{{source | json}}").unwrap();
    for (input, expected_code) in [(valid, 0), (oversized, 1)] {
        fs::write(root.path().join(path), input).unwrap();
        fs::write(root.path().join("output.md"), "unchanged\n").unwrap();
        // Isolate parser failures from the test harness and never produce a core dump.
        // The valid control must work with the same deliberately small stack.
        let mut command = shell("ulimit -c 0; ulimit -s 1024; exec \"$@\"");
        command
            .args([
                "depth-fixture",
                env!("CARGO_BIN_EXE_project-docs"),
                "--write",
            ])
            .current_dir(root.path());
        let result = capture(
            &mut command,
            Duration::from_secs(5),
            64 * 1024,
            &Signals::none(),
            true,
        )
        .unwrap();
        let diagnostics = String::from_utf8_lossy(&result.output);
        assert_eq!(result.failure, None, "{diagnostics}");
        assert_eq!(result.returncode, Some(expected_code), "{diagnostics}");
        if expected_code != 0 {
            assert!(diagnostics.contains("limit"), "{diagnostics}");
            assert_eq!(
                fs::read_to_string(root.path().join("output.md")).unwrap(),
                "unchanged\n",
            );
        }
    }
}

#[test]
fn renderer_bounds_flat_python_elif_chains_before_ast_construction() {
    let oversized = format!(
        "if True: pass\n{}X = 1\n",
        "elif True: pass\n".repeat(10_000)
    );
    assert_renderer_depth_rejected(
        "python-literal",
        "module.py",
        "X = 1\n",
        &oversized,
        Some("X"),
    );
}

#[test]
fn renderer_bounds_combined_toml_dotted_keys_and_inline_tables_before_values() {
    let key = std::iter::repeat_n("a", 80).collect::<Vec<_>>().join(".");
    let oversized = format!(
        "{}leaf = 1{}",
        format!("{key} = {{ ").repeat(79),
        " }".repeat(79)
    );
    assert_renderer_depth_rejected("toml", "source.toml", "leaf = 1\n", &oversized, None);
}
