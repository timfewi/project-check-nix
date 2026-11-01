use project_check::manifest::{self, Check, ExpectedWarning, Manifest};
use project_check::process::Signals;
use project_check::runner::{self, Tools};
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn shell() -> PathBuf {
    env::split_paths(&env::var_os("PATH").expect("test PATH"))
        .map(|directory| directory.join("sh"))
        .find(|path| {
            path.is_file() && fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0
        })
        .expect("sh must be available on PATH")
        .canonicalize()
        .unwrap()
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!{}\nset -eu\n{body}\n", shell().display())).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

struct Fixture {
    temporary: TempDir,
    root: PathBuf,
    scratch: PathBuf,
    cache: PathBuf,
    tools: Tools,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::Builder::new()
            .prefix("runner contracts ")
            .tempdir()
            .unwrap();
        let root = temporary.path().join("project with spaces");
        let scratch = temporary.path().join("scratch");
        let cache = temporary.path().join("cache");
        let rules = temporary.path().join("rules");
        for directory in [&root, &scratch, &cache, &rules] {
            fs::create_dir(directory).unwrap();
        }
        let ca = temporary.path().join("synthetic-ca.crt");
        fs::write(&ca, "synthetic certificate; not a credential").unwrap();
        let tools = Tools {
            rules,
            scanner: temporary.path().join("scanner"),
            docs: temporary.path().join("docs"),
            ca: Some(ca),
        };
        let fixture = Self {
            temporary,
            root,
            scratch,
            cache,
            tools,
        };
        fixture.scanner(
            Some(&json!({"paths":{"scanned":["module.py"]},"results":[],"errors":[]})),
            "",
            0,
        );
        fixture
    }

    fn check(&self, name: &str, body: &str) -> Check {
        let program = self.temporary.path().join(format!("check-{name}"));
        script(&program, body);
        manifest::validate(
            &self.root,
            &json!({"version":1,"checks":[{
                "name":name,"argv":[program],"requires":[program],
                "timeout_seconds":5,"profiles":["fast","full"]
            }]}),
        )
        .unwrap()
        .checks
        .remove(0)
    }

    fn execute(&self, check: &Check) -> Value {
        runner::run_check(
            &self.root,
            check,
            &self.scratch,
            &self.cache,
            &self.tools,
            &Signals::none(),
        )
        .unwrap()
    }

    fn scanner(&self, report: Option<&Value>, output: &str, code: i32) {
        let report_source = self.temporary.path().join("scanner-report.json");
        if let Some(report) = report {
            fs::write(&report_source, serde_json::to_vec(report).unwrap()).unwrap();
        }
        let output_source = self.temporary.path().join("scanner-output");
        fs::write(&output_source, output).unwrap();
        let arguments = self.temporary.path().join("scanner-arguments");
        let copying = if report.is_some() {
            format!("cp {} \"$report\"", quote(report_source.to_str().unwrap()))
        } else {
            String::new()
        };
        script(
            &self.tools.scanner,
            &format!(
                "printf '%s\\n' \"$@\" > {}\nreport=\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = --json-output ]; then shift; report=$1; fi\n  shift\ndone\n[ -n \"$report\" ]\n[ \"$SEMGREP_SEND_METRICS\" = off ]\n[ \"$SEMGREP_SETTINGS_FILE\" = \"$TMPDIR/semgrep-settings.yml\" ]\n[ \"$SEMGREP_LOG_FILE\" = \"$TMPDIR/semgrep.log\" ]\n{copying}\ncat {}\nexit {code}",
                quote(arguments.to_str().unwrap()),
                quote(output_source.to_str().unwrap())
            ),
        );
    }

    fn baseline(&self) -> Value {
        // Every invocation has a fresh report directory: stale coverage must not mask missing output.
        let scratch = tempfile::tempdir_in(self.temporary.path()).unwrap();
        runner::baseline(&self.root, scratch.path(), &self.tools, &Signals::none()).unwrap()
    }

    fn manifest(&self, checks: Vec<Check>) -> Manifest {
        Manifest {
            version: 2,
            checks,
            watch_ignore: vec![],
        }
    }
}

fn diagnostics(result: &Value) -> &str {
    result["diagnostics"].as_str().unwrap()
}

#[test]
fn exit_status_and_blockers_overrule_claimed_success() {
    let fixture = Fixture::new();
    for (code, expected) in [
        (0, "passed"),
        (1, "failed"),
        (23, "failed"),
        (126, "blocked"),
        (127, "blocked"),
    ] {
        let result = fixture.execute(&fixture.check(
            "evaluation",
            &format!(
                "printf '%s\\n' '{{\"model_claim\":\"passed\",\"confidence\":1.0}}'\nexit {code}"
            ),
        ));
        assert_eq!(result["status"], expected, "{result}");
        assert_eq!(result["returncode"], code);
        assert!(diagnostics(&result).contains("\"model_claim\":\"passed\""));
        if code != 0 {
            assert!(diagnostics(&result).contains(&code.to_string()));
        }
        if expected == "blocked" {
            assert!(diagnostics(&result).contains("devenv shell"));
        }
    }
    for (code, status) in [(0, "passed"), (1, "blocked")] {
        let result = fixture.execute(&fixture.check(
            "fetch",
            &format!("printf '%s\\n' 'crate failed to download, retried'\nexit {code}"),
        ));
        assert_eq!(result["status"], status);
    }
    let result = fixture.execute(&fixture.check("silent", "exit 23"));
    assert_eq!(result["status"], "failed");
    assert!(diagnostics(&result).contains("23"));
}

#[test]
fn missing_requirements_and_unavailable_programs_are_actionable_and_private() {
    let fixture = Fixture::new();
    let mut check = fixture.check("missing", "printf 'must not execute' > executed");
    check.requires = vec![
        fixture
            .root
            .join("synthetic-missing-checker")
            .display()
            .to_string(),
    ];
    let result = fixture.execute(&check);
    assert_eq!(result["status"], "blocked");
    for hint in [
        "synthetic-missing-checker",
        "devenv shell",
        "nix develop .",
        "devShells",
    ] {
        assert!(diagnostics(&result).contains(hint), "{result}");
    }
    assert!(!fixture.root.join("executed").exists());
    let nonexecutable = fixture.root.join("not-executable");
    fs::write(&nonexecutable, "synthetic fixture").unwrap();
    for program in [nonexecutable, fixture.root.join("unavailable-program")] {
        check.requires.clear();
        check.argv = vec![
            program.display().to_string(),
            "sensitive-synthetic-argument".into(),
        ];
        let result = fixture.execute(&check);
        assert_eq!(result["status"], "blocked");
        assert!(diagnostics(&result).contains(program.to_str().unwrap()));
        assert!(diagnostics(&result).contains("devenv shell"));
        assert!(!diagnostics(&result).contains("sensitive-synthetic-argument"));
    }
}

#[test]
fn warning_policy_retains_output_and_never_overrules_exit() {
    let fixture = Fixture::new();
    for (policy, code, status) in [
        ("report", 0, "passed"),
        ("report", 1, "failed"),
        ("fail", 0, "failed"),
    ] {
        let mut check = fixture.check(
            "lint",
            &format!("printf '%s\\n' 'src/a.py:1: DeprecationWarning: old'\nexit {code}"),
        );
        check.warnings = if policy == "report" {
            manifest::WarningPolicy::Report
        } else {
            manifest::WarningPolicy::Fail
        };
        let result = fixture.execute(&check);
        assert_eq!(result["status"], status);
        assert!(diagnostics(&result).contains("DeprecationWarning: old"));
        if policy == "fail" {
            assert!(diagnostics(&result).contains("\"warnings\": \"report\""));
        }
    }
}

#[test]
fn nix_notices_are_exact_and_do_not_hide_warnings_or_failure() {
    let fixture = Fixture::new();
    let notices = [
        "warning: Git tree '/workspace/project' is dirty",
        "warning: ignoring untrusted substituter 'https://cache.example', you are not a trusted user.",
        "warning: ignoring the client-specified setting 'max-jobs', because it is a restricted setting and you are not a trusted user",
        "warning: unable to download 'https://cache.example/x.narinfo': timeout; retrying in 263 ms (attempt 1/5)",
    ].join("\n");
    for (output, code, status) in [
        (notices.clone(), 0, "passed"),
        (format!("{notices}\nwarning: unused variable"), 0, "failed"),
        (format!("{notices}\n1 warning generated."), 0, "failed"),
        (notices, 1, "failed"),
        (
            "warning: Git tree '/workspace/project' is dirty: warning: unexpected suffix".into(),
            0,
            "failed",
        ),
        (
            "warning: Git tree '/workspace/project' has uncommitted changes".into(),
            0,
            "failed",
        ),
    ] {
        let result = fixture.execute(&fixture.check(
            "format",
            &format!("printf '%s\\n' {}\nexit {code}", quote(&output)),
        ));
        assert_eq!(result["status"], status, "{result}");
        assert!(diagnostics(&result).contains(&output));
    }
}

#[test]
fn expected_warnings_match_whole_literal_lines_and_count_every_occurrence() {
    let fixture = Fixture::new();
    let message = "evaluation warning: optional synthetic transport [v1].* is disabled";
    let reason = "The isolated fixture deliberately disables this transport.";
    for (output, code, status, count) in [
        (message.into(), 0, "passed", 1),
        (format!("{message}\n{message}"), 0, "passed", 2),
        ("clean output".into(), 0, "passed", 0),
        (format!("{message}\n{message}\n{message}"), 0, "failed", 3),
        (
            format!("{message}\nwarning: unused variable"),
            0,
            "failed",
            1,
        ),
        (format!("{message}\n1 warning generated."), 0, "failed", 1),
        (format!("{message}: unexpected suffix"), 0, "failed", 0),
        (format!("prefix {message}"), 0, "failed", 0),
        (format!("\u{1b}[33m{message}\u{1b}[0m"), 0, "failed", 0),
        (message.into(), 7, "failed", 1),
        (
            format!("{message}\nrequired command not found"),
            1,
            "blocked",
            1,
        ),
    ] {
        let mut check = fixture.check(
            "transport",
            &format!("printf '%s\\n' {}\nexit {code}", quote(&output)),
        );
        check.expected_warnings = vec![ExpectedWarning {
            message: message.into(),
            reason: reason.into(),
            max_count: 2,
        }];
        let result = fixture.execute(&check);
        assert_eq!(result["status"], status, "{result}");
        assert_eq!(result["returncode"], code);
        assert!(diagnostics(&result).contains(&output));
        assert!(diagnostics(&result).contains(reason));
        assert_eq!(
            result["expected_warnings"],
            json!([{"message":message,"reason":reason,"max_count":2,"count":count}])
        );
        if count > 2 {
            assert!(diagnostics(&result).contains("occurrence limit exceeded"));
        }
    }
    assert_eq!(
        fixture
            .execute(&fixture.check("undeclared", &format!("printf '%s\\n' {}", quote(message))))["status"],
        "failed"
    );
}

#[test]
fn relative_cwd_requirements_and_literal_argv_survive_execution() {
    let fixture = Fixture::new();
    let work = fixture.root.join("sub dir");
    fs::create_dir(&work).unwrap();
    fs::write(fixture.root.join("evidence.txt"), "fixture").unwrap();
    let default = fixture.check("default-cwd", "[ \"$(cat evidence.txt)\" = fixture ]");
    assert_eq!(fixture.execute(&default)["status"], "passed");
    script(
        &work.join("local-tool"),
        "[ \"$(cat evidence.txt)\" = nested ]\nprintf '%s\\n' \"$1\"\n[ \"$2\" = '' ]",
    );
    fs::write(work.join("evidence.txt"), "nested").unwrap();
    let mut check = fixture.check("arguments", "exit 99");
    check.cwd = "sub dir".into();
    check.argv = vec![
        "./local-tool".into(),
        "literal ; $(touch injected)".into(),
        "".into(),
    ];
    check.requires = vec!["./local-tool".into()];
    let result = fixture.execute(&check);
    assert_eq!(result["status"], "passed", "{result}");
    assert!(diagnostics(&result).contains("literal ; $(touch injected)"));
    assert!(!work.join("injected").exists());
    let plan = runner::plan(
        &fixture.root,
        &fixture.manifest(vec![check]),
        "fast",
        false,
        &fixture.tools,
        &Signals::none(),
    )
    .unwrap();
    assert_eq!(plan["checks"][1]["status"], "ready");
}

#[test]
fn ca_cache_and_scratch_are_explicit_and_cache_is_reused_outside_project() {
    let fixture = Fixture::new();
    let check = fixture.check("environment", &format!(
        "[ \"$NIX_SSL_CERT_FILE\" = {ca} ]\n[ \"$SSL_CERT_FILE\" = {ca} ]\n[ \"$TMPDIR\" = {scratch} ]\n[ \"$CARGO_TARGET_DIR\" = {target} ]\n[ \"$RUFF_CACHE_DIR\" = {ruff} ]\n[ \"$PYTHONDONTWRITEBYTECODE\" = 1 ]\ncase \"$RUSTFLAGS\" in *' -D warnings') ;; *) exit 20;; esac\nif [ -f \"$CARGO_TARGET_DIR/marker\" ]; then printf reused; else printf cold; fi\nmkdir -p \"$CARGO_TARGET_DIR\"\nprintf fixture > \"$CARGO_TARGET_DIR/marker\"",
        ca = quote(fixture.tools.ca.as_ref().unwrap().to_str().unwrap()),
        scratch = quote(fixture.scratch.to_str().unwrap()),
        target = quote(fixture.cache.join("cargo-target").to_str().unwrap()),
        ruff = quote(fixture.cache.join("ruff").to_str().unwrap()),
    ));
    let first = fixture.execute(&check);
    let second = fixture.execute(&check);
    assert_eq!(first["status"], "passed", "{first}");
    assert_eq!(second["status"], "passed", "{second}");
    assert!(diagnostics(&first).contains("cold"));
    assert!(diagnostics(&second).contains("reused"));
    assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 0);
}

#[test]
fn plan_is_read_only_private_and_reports_requirements_and_expected_warnings() {
    let fixture = Fixture::new();
    script(&fixture.tools.scanner, "printf executed > scanner-executed");
    let mut ready = fixture.check("ready", "printf executed > check-executed");
    ready.argv.push("secret-synthetic-argument".into());
    ready.expected_warnings = vec![ExpectedWarning {
        message: "warning: fixture".into(),
        reason: "Deliberate fixture.".into(),
        max_count: 1,
    }];
    let mut missing = fixture.check("missing", "exit 0");
    missing.requires = vec![
        "synthetic-missing-checker".into(),
        "synthetic-missing-checker".into(),
    ];
    let mut bad_cwd = fixture.check("bad-cwd", "exit 0");
    bad_cwd.cwd = "gone".into();
    let mut full = fixture.check("full-only", "exit 1");
    full.profiles = vec!["full".into()];
    let manifest = fixture.manifest(vec![ready, missing, bad_cwd, full]);
    let report = runner::plan(
        &fixture.root,
        &manifest,
        "fast",
        false,
        &fixture.tools,
        &Signals::none(),
    )
    .unwrap();
    assert_eq!(report["status"], "blocked");
    assert_eq!(report["manifest_version"], 2);
    assert_eq!(report["schema_version"], manifest::SCHEMA_VERSION);
    let checks = report["checks"].as_array().unwrap();
    assert_eq!(
        checks
            .iter()
            .map(|item| (
                item["name"].as_str().unwrap(),
                item["status"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        [
            ("baseline", "ready"),
            ("ready", "ready"),
            ("missing", "blocked"),
            ("bad-cwd", "blocked")
        ]
    );
    assert_eq!(
        checks[1]["expected_warnings"],
        json!([{"message":"warning: fixture","reason":"Deliberate fixture.","max_count":1}])
    );
    assert_eq!(
        checks[2]["missing_programs"],
        json!(["synthetic-missing-checker"])
    );
    assert_eq!(checks[3]["cwd_unavailable"], true);
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains("secret-synthetic-argument")
    );
    assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&fixture.scratch).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&fixture.cache).unwrap().count(), 0);
}

#[test]
fn baseline_coverage_errors_and_malformed_reports_never_pass() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join(manifest::MANIFEST),
        "invalid project manifest",
    )
    .unwrap();
    for (report, status) in [
        (
            json!({"paths":{"scanned":["lib/module.py"]},"results":[],"errors":[]}),
            "passed",
        ),
        (
            json!({"paths":{"scanned":[]},"results":[],"errors":[]}),
            "not applicable",
        ),
        (
            json!({"paths":{"scanned":["lib/module.py"]},"results":[],"errors":[{}]}),
            "failed",
        ),
        (
            json!({"paths":{"scanned":["lib/module.py"]},"results":[{}],"errors":[]}),
            "failed",
        ),
        (json!({}), "blocked"),
        (
            json!({"paths":{"scanned":null},"results":[],"errors":[]}),
            "blocked",
        ),
        (
            json!({"paths":{"scanned":[]},"results":null,"errors":[]}),
            "blocked",
        ),
        (
            json!({"paths":{"scanned":[]},"results":[],"errors":null}),
            "blocked",
        ),
    ] {
        fixture.scanner(Some(&report), "", 0);
        let result = fixture.baseline();
        assert_eq!(result["status"], status, "{report}: {result}");
        assert_eq!(result["language_coverage"], json!(["python"]));
        if status == "not applicable" {
            assert!(diagnostics(&result).contains("No supported source files"));
        }
    }
    fixture.scanner(None, "", 0);
    assert_eq!(fixture.baseline()["status"], "blocked");
    fixture.scanner(Some(&json!({})), "", 0);
    fs::write(
        fixture.temporary.path().join("scanner-report.json"),
        "not JSON",
    )
    .unwrap();
    assert_eq!(fixture.baseline()["status"], "blocked");
    fixture.scanner(Some(&json!({})), "", 9);
    assert_eq!(fixture.baseline()["status"], "failed");
}

#[test]
fn baseline_only_known_sql_advisories_are_nonblocking_and_scanner_argv_is_pinned() {
    let fixture = Fixture::new();
    let advisory = json!({"check_id":"python-review-formatted-sql","extra":{"severity":"WARNING"}});
    for (findings, errors, code, status) in [
        (json!([advisory]), json!([]), 0, "passed"),
        (json!([advisory]), json!([]), 3, "failed"),
        (json!([advisory]), json!([{}]), 0, "failed"),
        (json!([advisory, {}]), json!([]), 0, "failed"),
        (
            json!([{"check_id":"unknown","extra":{"severity":"WARNING"}}]),
            json!([]),
            0,
            "failed",
        ),
        (
            json!([{"check_id":"python-review-formatted-sql","extra":{"severity":"ERROR"}}]),
            json!([]),
            0,
            "failed",
        ),
        (json!([null]), json!([]), 0, "failed"),
    ] {
        fixture.scanner(Some(&json!({"paths":{"scanned":["tests/test_real.py"]},"results":findings,"errors":errors})), "", code);
        let result = fixture.baseline();
        assert_eq!(result["status"], status, "{result}");
        assert_eq!(result["findings"], findings);
        assert_eq!(result["parser_errors"], errors);
    }
    let arguments = fs::read_to_string(fixture.temporary.path().join("scanner-arguments")).unwrap();
    let arguments: Vec<_> = arguments.lines().collect();
    assert!(arguments.contains(&"--no-rewrite-rule-ids"));
    assert_eq!(
        arguments
            .windows(2)
            .filter_map(|pair| (pair[0] == "--exclude").then_some(pair[1]))
            .collect::<Vec<_>>(),
        ["/tests/semgrep/", "/tests/portable-quality/"]
    );
    assert_eq!(arguments.last(), Some(&"."));
}

#[test]
fn baseline_f3_overflow_is_not_downgraded_by_missing_or_malformed_coverage() {
    let fixture = Fixture::new();
    for report in [None, Some(json!({}))] {
        fixture.scanner(report.as_ref(), "", 0);
        let mut body = fs::read_to_string(&fixture.tools.scanner).unwrap();
        body = body.replace("exit 0", "printf '%s\\n' 'required command not found'\ndd if=/dev/zero bs=65536 count=65 2>/dev/null\nexit 0");
        fs::write(&fixture.tools.scanner, body).unwrap();
        let result = fixture.baseline();
        assert_eq!(result["status"], "failed", "{result}");
        assert_eq!(result["output_truncated"], true);
        assert!(diagnostics(&result).contains("output limit exceeded"));
        assert!(diagnostics(&result).contains("valid coverage report"));
    }
}

#[test]
fn run_continues_after_failures_and_blockers_and_skips_full_only_checks() {
    let fixture = Fixture::new();
    let failure = fixture.check("failure", "printf '%s\\n' 'native error'\nexit 3");
    let warning = fixture.check("warning", "printf '%s\\n' 'warning: synthetic finding'");
    let mut missing = fixture.check("missing", "exit 0");
    missing.requires = vec!["synthetic-missing-checker".into()];
    let last = fixture.check("last", "printf done > last-executed\nprintf done");
    let mut full = fixture.check("full-only", "printf unexpected > full-executed\nexit 1");
    full.profiles = vec!["full".into()];
    let manifest = fixture.manifest(vec![failure, warning, missing, last, full]);
    let report = runner::run(
        &fixture.root,
        &manifest,
        "fast",
        true,
        &fixture.tools,
        &Signals::none(),
        Some(&fixture.cache),
    )
    .unwrap();
    assert_eq!(report["status"], "failed");
    assert_eq!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["status"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "passed",
            "failed",
            "failed",
            "blocked",
            "passed",
            "not applicable"
        ]
    );
    assert_eq!(report["checks"][1]["returncode"], 3);
    assert!(fixture.root.join("last-executed").exists());
    assert!(!fixture.root.join("full-executed").exists());
    assert_eq!(
        runner::aggregate(&[json!({"status":"not applicable"})]),
        "failed"
    );
    assert_eq!(runner::aggregate(&[]), "failed");
}

#[test]
fn run_environment_only_blockers_remain_blocked_in_both_output_modes() {
    let fixture = Fixture::new();
    let mut missing = fixture.check("missing", "exit 0");
    missing.requires = vec!["synthetic-missing-checker".into()];
    let manifest = fixture.manifest(vec![missing]);
    for json_output in [false, true] {
        let report = runner::run(
            &fixture.root,
            &manifest,
            "fast",
            json_output,
            &fixture.tools,
            &Signals::none(),
            Some(&fixture.cache),
        )
        .unwrap();
        assert_eq!(report["status"], "blocked");
        assert_eq!(report["checks"][0]["status"], "passed");
        assert_eq!(report["checks"][1]["status"], "blocked");
    }
}

#[test]
fn run_retains_expected_warning_details_and_reuses_explicit_cache() {
    let fixture = Fixture::new();
    let mut check = fixture.check("transport", "printf '%s\\n' 'warning: optional synthetic transport is disabled'\nif [ -f \"$CARGO_TARGET_DIR/marker\" ]; then printf reused; else printf cold; fi\nmkdir -p \"$CARGO_TARGET_DIR\"\nprintf fixture > \"$CARGO_TARGET_DIR/marker\"");
    check.expected_warnings = vec![ExpectedWarning {
        message: "warning: optional synthetic transport is disabled".into(),
        reason: "The fixture disables it deliberately.".into(),
        max_count: 1,
    }];
    let manifest = fixture.manifest(vec![check]);
    for (json_output, cache_state) in [(false, "cold"), (true, "reused")] {
        let report = runner::run(
            &fixture.root,
            &manifest,
            "fast",
            json_output,
            &fixture.tools,
            &Signals::none(),
            Some(&fixture.cache),
        )
        .unwrap();
        assert_eq!(report["status"], "passed", "{report}");
        assert_eq!(
            report["checks"][1]["expected_warnings"],
            json!([{
                "message":"warning: optional synthetic transport is disabled",
                "reason":"The fixture disables it deliberately.","max_count":1,"count":1,
            }])
        );
        assert!(
            diagnostics(&report["checks"][1]).contains("The fixture disables it deliberately.")
        );
        assert!(diagnostics(&report["checks"][1]).contains(cache_state));
    }
    assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 0);
}

#[test]
fn run_output_modes_render_expected_warnings_and_complete_json_reports() {
    const CHILD_MODE: &str = "PROJECT_CHECK_CONTRACT_RENDER_CHILD";
    if let Ok(mode) = env::var(CHILD_MODE) {
        let fixture = Fixture::new();
        let mut check = fixture.check(
            "transport",
            "printf '%s\\n' 'warning: fixture transport disabled'",
        );
        check.expected_warnings = vec![ExpectedWarning {
            message: "warning: fixture transport disabled".into(),
            reason: "Synthetic transport is deliberately disabled.".into(),
            max_count: 1,
        }];
        let report = runner::run(
            &fixture.root,
            &fixture.manifest(vec![check]),
            "fast",
            mode == "json",
            &fixture.tools,
            &Signals::none(),
            Some(&fixture.cache),
        )
        .unwrap();
        assert_eq!(report["status"], "passed");
        return;
    }
    for mode in ["text", "json"] {
        let output = std::process::Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "run_output_modes_render_expected_warnings_and_complete_json_reports",
                "--nocapture",
            ])
            .env(CHILD_MODE, mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains("Synthetic transport is deliberately disabled."),
            "{stdout}"
        );
        if mode == "text" {
            assert!(stdout.contains("project-check fast: passed"), "{stdout}");
            assert!(
                stdout.contains("Expected diagnostic (1/1 occurrences)"),
                "{stdout}"
            );
        } else {
            let report: Value = stdout
                .lines()
                .find_map(|line| {
                    line.find('{')
                        .and_then(|start| serde_json::from_str(&line[start..]).ok())
                })
                .expect("complete JSON report on one stdout line");
            assert_eq!(report["status"], "passed");
            assert_eq!(report["checks"][1]["expected_warnings"][0]["count"], 1);
            assert_eq!(
                report["checks"][1]["expected_warnings"][0]["reason"],
                "Synthetic transport is deliberately disabled."
            );
        }
    }
}

#[test]
fn plan_cli_renders_expected_warnings_without_executing_or_leaking_argv() {
    let fixture = Fixture::new();
    let check = fixture.check("transport", "printf executed > should-not-exist");
    fs::write(
        fixture.root.join(manifest::MANIFEST),
        serde_json::to_vec(&json!({
            "version":2,"checks":[{
                "name":"transport","argv":[check.argv[0],"synthetic-private-argument"],
                "requires":[check.argv[0]],"profiles":["fast"],"timeout_seconds":5,
                "expected_warnings":[{"message":"warning: fixture transport disabled",
                    "reason":"Synthetic transport is deliberately disabled.","max_count":1}]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    for json_output in [false, true] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_project-check"));
        command.current_dir(&fixture.root).args(["fast", "--plan"]);
        if json_output {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        assert!(
            matches!(output.status.code(), Some(0 | 1)),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains("Synthetic transport is deliberately disabled."),
            "{stdout}"
        );
        assert!(!stdout.contains("synthetic-private-argument"), "{stdout}");
        if json_output {
            let report: Value = serde_json::from_str(&stdout).unwrap();
            assert_eq!(report["mode"], "plan");
            assert_eq!(report["checks"][1]["expected_warnings"][0]["max_count"], 1);
            assert!(
                report["checks"][1]["expected_warnings"][0]
                    .get("count")
                    .is_none()
            );
        } else {
            assert!(
                stdout.contains("expected diagnostic (at most 1 occurrences)"),
                "{stdout}"
            );
        }
    }
    assert!(!fixture.root.join("should-not-exist").exists());
    assert_eq!(fs::read_dir(&fixture.cache).unwrap().count(), 0);
}

#[test]
fn baseline_tooling_requirements_are_checked_without_running_scanner() {
    let mut fixture = Fixture::new();
    fixture.tools.rules = fixture.temporary.path().join("missing-rules");
    let result = fixture.baseline();
    assert_eq!(result["status"], "blocked");
    assert!(diagnostics(&result).contains("rules are unavailable"));
    assert!(!fixture.temporary.path().join("scanner-arguments").exists());
    fixture.tools.rules = fixture.temporary.path().join("rules");
    fixture.tools.scanner = fixture.temporary.path().join("missing-scanner");
    let result = fixture.baseline();
    assert_eq!(result["status"], "blocked");
    assert!(diagnostics(&result).contains("missing-scanner"));
    assert!(diagnostics(&result).contains("devenv shell"));
    assert!(diagnostics(&result).contains("valid coverage report"));
}

#[test]
fn compiler_cache_selection_is_stable_isolated_and_falls_back_safely() {
    const CHILD_ROOT: &str = "PROJECT_CHECK_CONTRACT_CACHE_ROOT";
    const CHILD_BASE: &str = "PROJECT_CHECK_CONTRACT_CACHE_BASE";
    if let Some(root) = env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let cache = runner::compiler_cache(&root);
        if let Some(base) = env::var_os(CHILD_BASE) {
            let cache = cache.expect("disposable compiler cache is available");
            assert!(cache.starts_with(PathBuf::from(base).join("project-check")));
            assert!(!cache.starts_with(&root));
            assert_eq!(runner::compiler_cache(&root), Some(cache.clone()));
            let other = root.with_file_name("other-project");
            assert_ne!(runner::compiler_cache(&other), Some(cache));
        } else {
            assert!(cache.is_none(), "{cache:?}");
        }
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("project");
    let home = temporary.path().join("home");
    let xdg = temporary.path().join("xdg-cache");
    let not_directory = temporary.path().join("not-a-directory");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&home).unwrap();
    fs::write(&not_directory, "synthetic fixture").unwrap();
    for (xdg_value, home_value, base) in [
        (xdg.clone(), home.clone(), Some(xdg.clone())),
        (PathBuf::new(), home.clone(), Some(home.join(".cache"))),
        (PathBuf::from("relative-cache"), home.clone(), None),
        (not_directory.clone(), home, None),
        (PathBuf::new(), not_directory, None),
    ] {
        let mut command = std::process::Command::new(env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "compiler_cache_selection_is_stable_isolated_and_falls_back_safely",
                "--nocapture",
            ])
            .env(CHILD_ROOT, &root)
            .env_remove(CHILD_BASE)
            .env("XDG_CACHE_HOME", xdg_value)
            .env("HOME", home_value);
        if let Some(base) = base {
            command.env(CHILD_BASE, base);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
