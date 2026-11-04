#![cfg(unix)]

use project_check::git;
use project_check::manifest::{self, Check, Manifest};
use project_check::process::Signals;
use project_check::runner::{self, Tools};
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

fn marker_script(path: &Path, marker: &Path) {
    fs::write(
        path,
        format!(
            "#!{}\nprintf executed > {}\nprintf 'filtered\\n'\n",
            shell().display(),
            quote(marker)
        ),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn isolated(command: &mut Command) -> &mut Command {
    command.env_clear();
    for (key, value) in env::vars_os().filter(|(key, _)| !key.to_string_lossy().starts_with("GIT_"))
    {
        command.env(key, value);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
}

fn git(root: &Path, args: &[&str]) -> Output {
    let output = isolated(Command::new("git").current_dir(root))
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn init(root: &Path) {
    fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "--initial-branch=main"]);
}

fn commit(root: &Path, message: &str) {
    git(root, &["commit", "-qm", message]);
}

struct Fixture {
    temporary: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::Builder::new()
            .prefix("git contracts ")
            .tempdir()
            .unwrap();
        let root = temporary.path().join("project");
        init(&root);
        fs::write(root.join("tracked.py"), "before\n").unwrap();
        git(&root, &["add", "."]);
        commit(&root, "fixture");
        // Different length defeats Git's cached-stat shortcut without sleeps or timestamp mutation.
        fs::write(root.join("tracked.py"), "after and different size\n").unwrap();
        Self { temporary, root }
    }

    fn paths(&self) -> Vec<String> {
        git::changed_paths(&self.root, &Signals::none()).unwrap()
    }

    fn check(&self, name: &str, inputs: Option<&[&str]>) -> Check {
        let mut declaration = json!({"name":name,"argv":[shell(),"-c","exit 99","synthetic-secret-argument"],
            "requires":[shell()],"profiles":["fast","full"],"timeout_seconds":5});
        if let Some(inputs) = inputs {
            declaration["input_paths"] = json!(inputs);
        }
        manifest::validate(&self.root, &json!({"version":2,"checks":[declaration]}))
            .unwrap()
            .checks
            .remove(0)
    }

    fn tools(&self) -> Tools {
        let rules = self.temporary.path().join("rules");
        fs::create_dir_all(&rules).unwrap();
        Tools {
            rules,
            scanner: shell(),
            docs: shell(),
            ca: None,
        }
    }
}

#[test]
fn inherited_repository_and_config_overrides_do_not_redirect_cli_change_scope() {
    let fixture = Fixture::new();
    let foreign = fixture.temporary.path().join("foreign");
    init(&foreign);
    fs::write(foreign.join("foreign.py"), "foreign\n").unwrap();
    let invalid = fixture.temporary.path().join("invalid-config");
    fs::write(&invalid, "not a git configuration\n").unwrap();
    let marker = fixture.temporary.path().join("injected-monitor-executed");
    let hook = fixture.temporary.path().join("injected-monitor");
    marker_script(&hook, &marker);
    fs::write(fixture.root.join(manifest::MANIFEST), serde_json::to_vec(&json!({"version":2,"checks":[{
        "name":"tracked","argv":[shell(),"-c","printf executed > should-not-exist","synthetic-secret-argument"],
        "requires":[shell()],"timeout_seconds":5,"profiles":["fast"],"input_paths":["tracked.py"]
    }]})).unwrap()).unwrap();
    git(&fixture.root, &["add", manifest::MANIFEST]);
    commit(&fixture.root, "manifest fixture");
    let output =
        isolated(Command::new(env!("CARGO_BIN_EXE_project-check")).current_dir(&fixture.root))
            .args(["fast", "--plan", "--changed", "--json"])
            .env("GIT_DIR", foreign.join(".git"))
            .env("GIT_WORK_TREE", &foreign)
            .env("GIT_INDEX_FILE", foreign.join(".git/index"))
            .env("GIT_CONFIG_GLOBAL", &invalid)
            .env("GIT_CONFIG_SYSTEM", &invalid)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.fsmonitor")
            .env("GIT_CONFIG_VALUE_0", quote(&hook))
            .output()
            .unwrap();
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["mode"], "plan", "{report}");
    assert_eq!(report["change_scope"]["examples"], json!(["tracked.py"]));
    assert_eq!(report["checks"][1]["relevance"], "affected");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-secret-argument"));
    assert!(!marker.exists());
    assert!(!fixture.root.join("should-not-exist").exists());
}

#[test]
fn nested_newline_project_paths_keep_both_rename_sides_and_untracked_files() {
    let fixture = Fixture::new();
    let project = fixture.root.join("nested project\n");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("old\nname.py"), "before\n").unwrap();
    git(&fixture.root, &["add", "."]);
    commit(&fixture.root, "nested fixture");
    fs::rename(project.join("old\nname.py"), project.join("new\nname.py")).unwrap();
    git(&project, &["add", "."]);
    fs::write(project.join("added file\n.py"), "untracked\n").unwrap();
    fs::write(
        fixture.root.join("outside.py"),
        "outside selected project\n",
    )
    .unwrap();
    let paths = git::changed_paths(&project, &Signals::none()).unwrap();
    assert_eq!(paths, ["added file\n.py", "new\nname.py", "old\nname.py"]);
    let mut check = fixture.check("rename", Some(&["new\nname.py"]));
    assert_eq!(git::relevance(&check, &paths)["relevance"], "affected");
    check.input_paths = Some(vec!["old\nname.py".into()]);
    assert_eq!(git::relevance(&check, &paths)["relevance"], "affected");
}

#[test]
fn repository_fsmonitor_is_never_executed() {
    let fixture = Fixture::new();
    let marker = fixture.temporary.path().join("monitor-executed");
    let hook = fixture.temporary.path().join("monitor");
    marker_script(&hook, &marker);
    git(&fixture.root, &["config", "core.fsmonitor", &quote(&hook)]);
    assert!(fixture.paths().contains(&"tracked.py".into()));
    assert!(!marker.exists());
}

#[test]
fn required_clean_process_and_smudge_filters_are_disabled_during_inspection() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join(".gitattributes"),
        "tracked.py filter=fixture.driver\n",
    )
    .unwrap();
    for kind in ["clean", "process", "smudge"] {
        let marker = fixture.temporary.path().join(format!("{kind}-executed"));
        let program = fixture.temporary.path().join(kind);
        marker_script(&program, &marker);
        git(
            &fixture.root,
            &[
                "config",
                &format!("filter.fixture.driver.{kind}"),
                &quote(&program),
            ],
        );
        git(
            &fixture.root,
            &["config", "filter.fixture.driver.required", "true"],
        );
        let paths = fixture.paths();
        assert!(paths.contains(&"tracked.py".into()), "{kind}: {paths:?}");
        assert!(!marker.exists(), "{kind} filter executed");
        git(
            &fixture.root,
            &["config", "--remove-section", "filter.fixture.driver"],
        );
    }
}

#[test]
fn submodule_dirty_content_is_excluded_but_commit_changes_are_reported() {
    let fixture = Fixture::new();
    let child = fixture.root.join("child");
    init(&child);
    fs::write(child.join("tracked.py"), "before\n").unwrap();
    git(&child, &["add", "."]);
    commit(&child, "child fixture");
    git(&fixture.root, &["add", "child"]);
    commit(&fixture.root, "gitlink fixture");
    let marker = fixture.temporary.path().join("child-filter-executed");
    let program = fixture.temporary.path().join("child-filter");
    marker_script(&program, &marker);
    fs::write(child.join(".gitattributes"), "tracked.py filter=fixture\n").unwrap();
    git(
        &child,
        &["config", "filter.fixture.clean", &quote(&program)],
    );
    git(&child, &["config", "filter.fixture.required", "true"]);
    fs::write(
        child.join("tracked.py"),
        "changed child content with different size\n",
    )
    .unwrap();
    assert!(!fixture.paths().contains(&"child".into()));
    assert!(!marker.exists());
    assert!(
        git::changed_paths(&child, &Signals::none())
            .unwrap()
            .contains(&"tracked.py".into())
    );
    assert!(!marker.exists());
    git(&child, &["config", "--remove-section", "filter.fixture"]);
    git(&child, &["add", "tracked.py"]);
    commit(&child, "child commit changed");
    assert!(fixture.paths().contains(&"child".into()));
}

#[test]
fn unrepresentable_and_excessive_filter_metadata_block_safely() {
    for names in [
        vec!["bad=driver".into()],
        (0..65).map(|index| format!("driver{index}")).collect(),
        vec!["x".repeat(32768)],
    ] {
        let fixture = Fixture::new();
        for name in names {
            git(
                &fixture.root,
                &["config", &format!("filter.{name}.required"), "true"],
            );
        }
        let error = git::changed_paths(&fixture.root, &Signals::none()).unwrap_err();
        assert!(
            error.to_string().contains("filter configuration"),
            "{error}"
        );
    }
}

#[test]
fn filter_metadata_output_capture_is_bounded() {
    use std::io::Write;
    let fixture = Fixture::new();
    let mut config = fs::OpenOptions::new()
        .append(true)
        .open(fixture.root.join(".git/config"))
        .unwrap();
    // A subsection can be much larger than an argv element: write config data, not a shell command.
    writeln!(
        config,
        "[filter \"{}\"]\nrequired = true",
        "x".repeat(project_check::MAX_OUTPUT_BYTES + 1)
    )
    .unwrap();
    drop(config);
    let error = git::changed_paths(&fixture.root, &Signals::none()).unwrap_err();
    assert!(error.to_string().contains("too large"), "{error}");
}

#[test]
fn changed_plan_annotates_relevance_without_skipping_or_executing_checks() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("src")).unwrap();
    fs::create_dir(fixture.root.join("docs")).unwrap();
    fs::write(fixture.root.join("src/main.py"), "code\n").unwrap();
    fs::write(fixture.root.join("docs/readme.md"), "docs\n").unwrap();
    fs::write(fixture.root.join(".gitignore"), "ignored.py\n").unwrap();
    git(&fixture.root, &["add", "."]);
    commit(&fixture.root, "plan fixture");
    fs::write(fixture.root.join("src/main.py"), "changed code\n").unwrap();
    fs::write(fixture.root.join("docs/readme.md"), "changed docs\n").unwrap();
    fs::write(fixture.root.join("ignored.py"), "ignored\n").unwrap();
    let manifest = Manifest {
        version: 2,
        checks: vec![
            fixture.check("code", Some(&["src/"])),
            fixture.check("docs", Some(&["docs/readme.md"])),
            fixture.check("other", Some(&["other/"])),
            fixture.check("unmapped", None),
        ],
        watch_ignore: vec![],
    };
    let report = runner::plan(
        &fixture.root,
        &manifest,
        "fast",
        true,
        &fixture.tools(),
        &Signals::none(),
    )
    .unwrap();
    assert_eq!(report["status"], "ready");
    assert_eq!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|check| check["relevance"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["required", "affected", "affected", "unaffected", "unknown"]
    );
    assert_eq!(report["change_scope"]["advisory_only"], true);
    assert_eq!(report["change_scope"]["total"], 2);
    assert_eq!(report["checks"][1]["matched_total"], 1);
    assert_eq!(
        report["checks"][1]["matched_examples"],
        json!(["src/main.py"])
    );
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains("synthetic-secret-argument")
    );
    for declaration in [manifest::MANIFEST, "flake.nix", "flake.lock"] {
        fs::write(fixture.root.join(declaration), "changed declaration\n").unwrap();
        let report = runner::plan(
            &fixture.root,
            &manifest,
            "fast",
            true,
            &fixture.tools(),
            &Signals::none(),
        )
        .unwrap();
        assert!(
            report["checks"].as_array().unwrap()[1..]
                .iter()
                .all(|check| check["relevance"] == "affected")
        );
        fs::remove_file(fixture.root.join(declaration)).unwrap();
    }
}

#[test]
fn changed_plan_limits_examples_but_counts_all_matches() {
    let fixture = Fixture::new();
    git(&fixture.root, &["add", "."]);
    commit(&fixture.root, "clean fixture");
    fs::create_dir(fixture.root.join("src")).unwrap();
    for index in 0..25 {
        fs::write(
            fixture.root.join(format!("src/{index:02}.py")),
            "untracked\n",
        )
        .unwrap();
    }
    let manifest = Manifest {
        version: 2,
        checks: vec![fixture.check("code", Some(&["src/"]))],
        watch_ignore: vec![],
    };
    let report = runner::plan(
        &fixture.root,
        &manifest,
        "fast",
        true,
        &fixture.tools(),
        &Signals::none(),
    )
    .unwrap();
    assert_eq!(report["checks"][1]["matched_total"], 25);
    assert_eq!(
        report["checks"][1]["matched_examples"]
            .as_array()
            .unwrap()
            .len(),
        10
    );
    assert_eq!(report["change_scope"]["total"], 25);
    assert_eq!(
        report["change_scope"]["examples"].as_array().unwrap().len(),
        20
    );
}

#[test]
fn change_inspection_outside_a_repository_is_an_error_not_an_empty_change_list() {
    let temporary = tempfile::tempdir().unwrap();
    let error = git::changed_paths(temporary.path(), &Signals::none()).unwrap_err();
    assert!(
        error.to_string().contains("Git change list unavailable"),
        "{error}"
    );
}
