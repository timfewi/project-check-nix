use crate::manifest::{Check, Manifest, WarningPolicy};
use crate::process::{CaptureFailure, Signals, capture};
use crate::{
    Error, MAX_OUTPUT_BYTES, MAX_REPORT_BYTES, MAX_SCANNER_REPORT_BYTES, Result, diagnostics, git,
    util,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, Write};
#[cfg(all(test, unix))]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub const TOOLCHAIN_HINT: &str = "Install the required project tool or enter its development environment, then retry. For Nix projects use `devenv shell` (or `nix develop .`) and declare missing tools in devenv.nix or flake.nix devShells. Check PATH and executable permissions/interpreter. On Windows use native .exe/.com programs; scripts require an explicitly declared interpreter.";

#[derive(Clone, Debug)]
pub struct Tools {
    pub rules: PathBuf,
    pub scanner: PathBuf,
    pub docs: PathBuf,
    pub ca: Option<PathBuf>,
}
impl Default for Tools {
    fn default() -> Self {
        Self {
            rules: Path::new(env!("CARGO_MANIFEST_DIR")).join(".semgrep/portable"),
            scanner: PathBuf::from("semgrep"),
            docs: env::current_exe()
                .unwrap_or_default()
                .with_file_name(format!("project-docs{}", env::consts::EXE_SUFFIX)),
            ca: None,
        }
    }
}

impl Tools {
    pub fn load() -> Result<Self> {
        let executable = env::current_exe()?;
        let prefix = executable
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| Error::new("runner has no package prefix"))?;
        Self::from_prefix(prefix)
    }

    fn from_prefix(prefix: &Path) -> Result<Self> {
        let path = prefix.join("share/project-check/tools.json");
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
            Ok(_) => {
                let config = util::parse_json(&util::read_limited(&path, 16384)?)?;
                let version = config["version"].as_u64().unwrap_or(0);
                if !matches!(version, 1 | 2) {
                    return Err(Error::new("unsupported packaged tooling manifest"));
                }
                let field = |name: &str| -> Result<PathBuf> {
                    let value = config[name]
                        .as_str()
                        .filter(|value| !value.contains('\0'))
                        .ok_or_else(|| Error::new("invalid packaged tooling path"))?;
                    let path = PathBuf::from(value);
                    if version == 1 {
                        if !path.is_absolute() {
                            return Err(Error::new("packaged tooling paths must be absolute"));
                        }
                        return Ok(path);
                    }
                    package_relative_path(prefix, &path)
                };
                Ok(Self {
                    rules: field("rules")?,
                    scanner: field("scanner")?,
                    docs: field("docs")?,
                    ca: if version == 2 && config["ca"].is_null() {
                        None
                    } else {
                        Some(field("ca")?)
                    },
                })
            }
        }
    }
}

fn package_relative_path(prefix: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.as_os_str().is_empty()
        || relative.to_string_lossy().contains(['\\', ':'])
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(Error::new(
            "portable packaged tooling paths must be relative and cannot traverse parents",
        ));
    }
    let prefix = fs::canonicalize(prefix)?;
    let mut path = prefix.clone();
    for component in relative.components() {
        path.push(component.as_os_str());
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                if !fs::canonicalize(&path)?.starts_with(&prefix) {
                    return Err(Error::new(
                        "packaged tooling path escapes the package prefix",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

fn executable(path: &Path) -> bool {
    if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    #[cfg(unix)]
    {
        nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
    }
    #[cfg(windows)]
    {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("com")
            })
    }
}

fn executable_candidate(path: PathBuf) -> Option<PathBuf> {
    if executable(&path) {
        return Some(path);
    }
    #[cfg(windows)]
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["exe", "com", "bat", "cmd"]
                .iter()
                .any(|native| extension.eq_ignore_ascii_case(native))
        })
    {
        // Restrict PATHEXT to native executables. Command would otherwise invoke
        // cmd.exe implicitly for .bat/.cmd, violating the argv-only contract.
        let extensions = env::var_os("PATHEXT").unwrap_or_else(|| ".EXE;.COM".into());
        for extension in extensions.to_string_lossy().split(';') {
            if !extension.eq_ignore_ascii_case(".exe") && !extension.eq_ignore_ascii_case(".com") {
                continue;
            }
            let mut candidate = path.as_os_str().to_os_string();
            candidate.push(extension);
            let candidate = PathBuf::from(candidate);
            if executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

pub(crate) fn resolve_program(program: &str, cwd: &Path) -> Option<PathBuf> {
    let explicit = program.contains('/')
        || cfg!(windows) && program.contains('\\')
        || Path::new(program)
            .components()
            .any(|part| matches!(part, Component::Prefix(_)));
    if explicit {
        let path = Path::new(program);
        return executable_candidate(if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        });
    }
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths).find_map(|path| {
            let path = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            executable_candidate(path.join(program))
        })
    })
}

pub fn program_available(program: &str, cwd: &Path) -> bool {
    resolve_program(program, cwd).is_some()
}

pub fn run_check(
    root: &Path,
    check: &Check,
    scratch: &Path,
    cache: &Path,
    tools: &Tools,
    signals: &Signals,
) -> Result<Value> {
    signals.check()?;
    let mut result =
        json!({"name":check.name,"status":"blocked","returncode":null,"diagnostics":""});
    let cwd = match util::resolve_inside(root, &check.cwd) {
        Ok(cwd) if cwd.is_dir() => cwd,
        _ => {
            result["diagnostics"] =
                json!("working directory is unavailable or escapes the project");
            return Ok(result);
        }
    };
    let missing: Vec<_> = check
        .requires
        .iter()
        .filter(|name| !program_available(name, &cwd))
        .cloned()
        .collect();
    if !missing.is_empty() {
        result["diagnostics"] = json!(format!(
            "missing required programs: {}\n{TOOLCHAIN_HINT}",
            missing.join(", ")
        ));
        return Ok(result);
    }
    let Some(program) = resolve_program(&check.argv[0], &cwd) else {
        result["diagnostics"] = json!(format!(
            "runner could not start {:?}: executable is unavailable\n{TOOLCHAIN_HINT}",
            check.argv[0]
        ));
        return Ok(result);
    };
    let mut command = Command::new(program);
    command
        .args(&check.argv[1..])
        .current_dir(&cwd)
        .env("TMPDIR", scratch)
        .env("TMP", scratch)
        .env("TEMP", scratch)
        .env("CARGO_TARGET_DIR", cache.join("cargo-target"))
        .env("RUFF_CACHE_DIR", cache.join("ruff"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env(
            "RUSTFLAGS",
            format!("{} -D warnings", env::var("RUSTFLAGS").unwrap_or_default()),
        );
    if let Some(ca) = &tools.ca {
        command
            .env("NIX_SSL_CERT_FILE", ca)
            .env("SSL_CERT_FILE", ca);
    }
    if check.name == "baseline"
        && check
            .argv
            .first()
            .is_some_and(|program| Path::new(program) == tools.scanner)
    {
        command
            .env(
                "SEMGREP_SETTINGS_FILE",
                scratch.join("semgrep-settings.yml"),
            )
            .env("SEMGREP_LOG_FILE", scratch.join("semgrep.log"))
            .env("SEMGREP_SEND_METRICS", "off");
    }
    let started = Instant::now();
    let captured = match capture(
        &mut command,
        Duration::from_secs_f64(check.timeout_seconds),
        MAX_OUTPUT_BYTES,
        signals,
        true,
    ) {
        Ok(captured) => captured,
        Err(error) => {
            result["diagnostics"] = json!(format!(
                "runner could not start {:?}: {error}\n{TOOLCHAIN_HINT}",
                check.argv[0]
            ));
            return Ok(result);
        }
    };
    match captured.failure {
        Some(CaptureFailure::Interrupted(signal)) => return Err(Error::interrupted(signal)),
        Some(CaptureFailure::Timeout) => {
            result["status"] = json!("failed");
            result["timed_out"] = json!(true);
            result["diagnostics"] = json!(format!(
                "timeout after {} seconds; raise timeout_seconds or move the check to the full profile\n",
                check.timeout_seconds
            ));
        }
        Some(CaptureFailure::OutputLimit) => {
            result["status"] = json!("failed");
            result["returncode"] = json!(captured.returncode);
            result["output_truncated"] = json!(true);
            result["diagnostics"] = json!(format!(
                "output limit exceeded ({MAX_OUTPUT_BYTES} bytes); partial diagnostics cannot establish a passing check\n"
            ));
        }
        None => {
            result["returncode"] = json!(captured.returncode);
            result["status"] = json!(if captured.returncode == Some(0) {
                "passed"
            } else {
                "failed"
            });
        }
    }
    let output = String::from_utf8_lossy(&captured.output);
    let prefix = result["diagnostics"].as_str().unwrap_or("");
    result["diagnostics"] = json!(format!("{prefix}{output}"));
    diagnostics::apply(check, &output, &mut result);
    result["duration_seconds"] = json!((started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0);
    Ok(result)
}

pub fn baseline(root: &Path, scratch: &Path, tools: &Tools, signals: &Signals) -> Result<Value> {
    if !tools.rules.is_dir() {
        return Ok(
            json!({"name":"baseline","status":"blocked","diagnostics":"portable quality rules are unavailable"}),
        );
    }
    let report_path = scratch.join("semgrep.json");
    let scanner = tools.scanner.to_string_lossy().into_owned();
    let check = Check {
        name: "baseline".into(),
        argv: vec![
            scanner.clone(),
            "scan".into(),
            "--config".into(),
            tools.rules.display().to_string(),
            "--strict".into(),
            "--no-rewrite-rule-ids".into(),
            "--exclude".into(),
            "/tests/semgrep/".into(),
            "--exclude".into(),
            "/tests/portable-quality/".into(),
            "--json-output".into(),
            report_path.display().to_string(),
            "--metrics=off".into(),
            "--disable-version-check".into(),
            "--jobs".into(),
            "1".into(),
            ".".into(),
        ],
        cwd: ".".into(),
        requires: vec![scanner],
        timeout_seconds: 180.0,
        profiles: vec![],
        input_paths: None,
        expected_warnings: vec![],
        warnings: WarningPolicy::Fail,
    };
    let mut result = run_check(root, &check, scratch, scratch, tools, signals)?;
    result["rules_package"] = json!(tools.rules);
    result["language_coverage"] = json!(["python"]);
    let parsed = util::read_limited(&report_path, MAX_SCANNER_REPORT_BYTES)
        .and_then(|text| util::parse_json(&text));
    match parsed {
        Ok(report)
            if report["paths"]["scanned"].is_array()
                && report["results"].is_array()
                && report["errors"].is_array() =>
        {
            let findings = report["results"].as_array().expect("validated findings");
            let errors = report["errors"].as_array().expect("validated errors");
            let advisory: Vec<_> = findings
                .iter()
                .filter(|finding| {
                    finding["check_id"] == "python-review-formatted-sql"
                        && finding["extra"]["severity"] == "WARNING"
                })
                .cloned()
                .collect();
            let blocking = findings.len() - advisory.len();
            let scanned = report["paths"]["scanned"]
                .as_array()
                .expect("validated paths")
                .len();
            result["scanned_files"] = json!(scanned);
            result["findings"] = json!(findings);
            result["parser_errors"] = json!(errors);
            result["advisory_findings"] = json!(advisory);
            let mut extra = format!(
                "\nBaseline policy: {blocking} blocking findings, {} SQL review advisories, {} parser errors.\n",
                advisory.len(),
                errors.len()
            );
            if blocking > 0 || !errors.is_empty() {
                result["status"] = json!("failed");
            } else if scanned == 0 && result["status"] == "passed" {
                result["status"] = json!("not applicable");
                extra.push_str("\nNo supported source files were scanned.\n");
            }
            extra.push_str(
                "\nBaseline covers Python only; project checks must cover other languages.\n",
            );
            append_diagnostic(&mut result, &extra);
        }
        _ => {
            // Never downgrade an already observed failure, including F3's
            // output overflow, just because a coverage file is missing.
            if result["status"] != "failed" {
                result["status"] = json!("blocked");
            }
            append_diagnostic(
                &mut result,
                "\nScanner did not produce a valid coverage report (maximum 16 MiB).\n",
            );
        }
    }
    signals.check()?;
    Ok(result)
}

fn append_diagnostic(result: &mut Value, extra: &str) {
    let text = result["diagnostics"].as_str().unwrap_or("");
    result["diagnostics"] = json!(format!("{text}{extra}"));
}

pub fn compiler_cache(root: &Path) -> Option<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(default_cache_home)?;
    if !base.is_absolute() {
        return None;
    }
    let hash = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
    let cache = base.join("project-check").join(format!(
        "{}-{}",
        root.file_name()?.to_string_lossy(),
        &hash[..16]
    ));
    fs::create_dir_all(&cache).ok()?;
    // Probe actual writability rather than trusting permission bits (ACLs and
    // read-only mounts can differ); the temporary probe removes itself.
    tempfile::Builder::new()
        .prefix(".writable-")
        .tempfile_in(&cache)
        .ok()?;
    Some(cache)
}

fn default_cache_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("USERPROFILE")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join("AppData/Local"))
            })
    }
    #[cfg(unix)]
    {
        env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(|home| PathBuf::from(home).join(".cache"))
    }
}

pub fn documentation_checks(
    root: &Path,
    manifest: &Manifest,
    profile: &str,
    tools: &Tools,
) -> Vec<Check> {
    let mut checks = manifest.checks.clone();
    if !matches!(profile, "fast" | "full") || !root.join(".dependency-docs.json").exists() {
        return checks;
    }
    let mut name = "dependency-docs".to_owned();
    while checks.iter().any(|check| check.name == name) {
        name.push_str("-auto");
    }
    checks.insert(
        0,
        Check {
            name,
            argv: vec![tools.docs.display().to_string(), "--write".into()],
            cwd: ".".into(),
            requires: vec![],
            timeout_seconds: 120.0,
            profiles: vec!["fast".into(), "full".into()],
            input_paths: Some(vec![]),
            expected_warnings: vec![],
            warnings: WarningPolicy::Fail,
        },
    );
    checks
}

pub fn aggregate(results: &[Value]) -> &'static str {
    let selected: Vec<_> = results
        .iter()
        .filter(|result| result["status"] != "not applicable")
        .collect();
    if selected.is_empty() || selected.iter().any(|result| result["status"] == "failed") {
        "failed"
    } else if selected.iter().any(|result| result["status"] != "passed") {
        "blocked"
    } else {
        "passed"
    }
}

fn compact_record(name: &Value, returncode: &Value) -> Value {
    json!({"name":name,"status":"failed","diagnostics":"aggregate report limit exceeded (32 MiB); diagnostics omitted; checks continue","output_truncated":true,"returncode":returncode})
}

fn compact_capacity(name: &str) -> Result<usize> {
    // Any real return code/null serializes within this worst-case signed integer.
    Ok(serde_json::to_vec(&compact_record(&json!(name), &json!(i64::MIN)))?.len() + 1)
}

fn retain_result(mut result: Value, retained: &mut usize, reserved: usize) -> Result<Value> {
    let room = (MAX_REPORT_BYTES - 1024)
        .saturating_sub(*retained)
        .saturating_sub(reserved);
    let size = serde_json::to_vec(&result)?.len() + 1;
    if size > room {
        result = compact_record(&result["name"], &result["returncode"]);
    }
    let size = serde_json::to_vec(&result)?.len() + 1;
    if size > room {
        return Err(Error::new(
            "aggregate report cannot retain check identities within 32 MiB",
        ));
    }
    *retained += size;
    Ok(result)
}

pub fn run(
    root: &Path,
    manifest: &Manifest,
    profile: &str,
    json_output: bool,
    tools: &Tools,
    signals: &Signals,
    cache_override: Option<&Path>,
) -> Result<Value> {
    let scratch = tempfile::Builder::new()
        .prefix("project-check-")
        .tempdir()?;
    let cache = cache_override
        .map(Path::to_path_buf)
        .or_else(|| compiler_cache(root))
        .unwrap_or_else(|| scratch.path().to_path_buf());
    let mut retained = 0;
    let checks = documentation_checks(root, manifest, profile, tools);
    let mut reserved = checks.iter().try_fold(0, |sum, check| {
        compact_capacity(&check.name).map(|size| sum + size)
    })?;
    let mut results = vec![retain_result(
        baseline(root, scratch.path(), tools, signals)?,
        &mut retained,
        reserved,
    )?];
    if !json_output {
        println!("{}", results[0]["diagnostics"].as_str().unwrap_or(""));
    }
    for check in checks {
        signals.check()?;
        reserved -= compact_capacity(&check.name)?;
        let result = if check.profiles.iter().any(|item| item == profile) {
            if !json_output {
                println!("==> {}", check.name);
                io::stdout().flush()?;
            }
            run_check(root, &check, scratch.path(), &cache, tools, signals)?
        } else {
            json!({"name":check.name,"status":"not applicable","diagnostics":""})
        };
        let result = retain_result(result, &mut retained, reserved)?;
        if !json_output
            && let Some(text) = result["diagnostics"]
                .as_str()
                .filter(|text| !text.is_empty())
        {
            println!("{text}");
        }
        results.push(result);
    }
    signals.check()?;
    let report =
        json!({"version":1,"profile":profile,"status":aggregate(&results),"checks":results});
    if json_output {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        for result in &results {
            println!(
                "{}: {}",
                result["status"].as_str().unwrap_or("failed"),
                result["name"].as_str().unwrap_or("unknown")
            );
        }
        println!(
            "project-check {profile}: {}",
            report["status"].as_str().unwrap_or("failed")
        );
    }
    io::stdout().flush()?;
    signals.check()?;
    Ok(report)
}

pub fn plan(
    root: &Path,
    manifest: &Manifest,
    profile: &str,
    changed: bool,
    tools: &Tools,
    signals: &Signals,
) -> Result<Value> {
    let paths = if changed {
        git::changed_paths(root, signals)?
    } else {
        vec![]
    };
    let scanner = tools.scanner.to_string_lossy();
    let missing: Vec<_> = (!program_available(&scanner, root))
        .then(|| scanner.to_string())
        .into_iter()
        .collect();
    let mut checks = vec![
        json!({"name":"baseline","status":if missing.is_empty() && tools.rules.is_dir() {"ready"} else {"blocked"},"missing_programs":missing,"cwd_unavailable":false,"rules_unavailable":!tools.rules.is_dir()}),
    ];
    if changed {
        checks[0]["relevance"] = json!("required");
        checks[0]["matched_total"] = json!(0);
        checks[0]["matched_examples"] = json!([]);
    }
    for check in documentation_checks(root, manifest, profile, tools)
        .iter()
        .filter(|check| check.profiles.iter().any(|item| item == profile))
    {
        signals.check()?;
        let cwd = util::resolve_inside(root, &check.cwd).ok();
        let unavailable = cwd.as_ref().is_none_or(|cwd| !cwd.is_dir());
        let mut seen = BTreeSet::new();
        let missing: Vec<_> = std::iter::once(&check.argv[0])
            .chain(&check.requires)
            .filter(|program| seen.insert(*program))
            .filter(|program| !program_available(program, cwd.as_deref().unwrap_or(root)))
            .cloned()
            .collect();
        let mut planned = json!({"name":check.name,"status":if unavailable || !missing.is_empty() {"blocked"} else {"ready"},"missing_programs":missing,"cwd_unavailable":unavailable,"timeout_seconds":check.timeout_seconds});
        if !check.expected_warnings.is_empty() {
            planned["expected_warnings"] = json!(check.expected_warnings);
        }
        if changed {
            for (key, value) in git::relevance(check, &paths)
                .as_object()
                .expect("relevance object")
            {
                planned[key] = value.clone();
            }
        }
        checks.push(planned);
    }
    let status = if checks.iter().any(|check| check["status"] == "blocked") {
        "blocked"
    } else {
        "ready"
    };
    let mut report =
        json!({"version":1,"mode":"plan","profile":profile,"status":status,"checks":checks});
    if manifest.version > 0 {
        report["manifest_version"] = json!(manifest.version);
        report["schema_version"] = json!(crate::manifest::SCHEMA_VERSION);
    }
    if changed {
        report["change_scope"] = json!({"corpus":"Git tracked and untracked paths beneath selected project; ignored paths and submodule contents excluded","total":paths.len(),"examples":paths.iter().take(20).collect::<Vec<_>>(),"advisory_only":true});
    }
    signals.check()?;
    Ok(report)
}

#[cfg(test)]
mod safety_regressions {
    use super::*;

    fn portable_tools_manifest(prefix: &Path, scanner: &str) {
        fs::create_dir_all(prefix.join("share/project-check")).unwrap();
        let config = json!({
            "version": 2,
            "rules": "share/project-check/rules",
            "scanner": scanner,
            "docs": format!("bin/project-docs{}", env::consts::EXE_SUFFIX),
            "ca": null
        });
        fs::write(
            prefix.join("share/project-check/tools.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn portable_tooling_paths_follow_the_package_prefix() {
        let temporary = tempfile::tempdir().unwrap();
        let original = temporary.path().join("original package");
        let moved = temporary.path().join("moved package");
        portable_tools_manifest(&original, "lib/semgrep/scanner");
        let before = Tools::from_prefix(&original).unwrap();
        assert_eq!(
            before.scanner,
            fs::canonicalize(&original)
                .unwrap()
                .join("lib/semgrep/scanner")
        );
        assert!(before.ca.is_none());
        fs::rename(&original, &moved).unwrap();
        let after = Tools::from_prefix(&moved).unwrap();
        let resolved = fs::canonicalize(&moved).unwrap();
        assert_eq!(after.rules, resolved.join("share/project-check/rules"));
        assert_eq!(after.scanner, resolved.join("lib/semgrep/scanner"));
        assert_eq!(
            after.docs,
            resolved.join(format!("bin/project-docs{}", env::consts::EXE_SUFFIX))
        );
    }

    #[test]
    fn portable_tooling_rejects_paths_outside_the_package() {
        let temporary = tempfile::tempdir().unwrap();
        let prefix = temporary.path().join("package");
        for path in [
            "../scanner",
            "/scanner",
            "C:\\scanner.exe",
            "bin/scanner:stream",
            "",
        ] {
            portable_tools_manifest(&prefix, path);
            assert!(Tools::from_prefix(&prefix).is_err(), "accepted {path:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn portable_tooling_rejects_symlink_escape() {
        let temporary = tempfile::tempdir().unwrap();
        let prefix = temporary.path().join("package");
        portable_tools_manifest(&prefix, "scanner");
        let outside = temporary.path().join("outside-scanner");
        fs::write(&outside, "scanner fixture").unwrap();
        std::os::unix::fs::symlink(outside, prefix.join("scanner")).unwrap();
        assert!(Tools::from_prefix(&prefix).is_err());
    }

    #[test]
    fn nix_tooling_keeps_absolute_version_one_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let prefix = temporary.path();
        fs::create_dir_all(prefix.join("share/project-check")).unwrap();
        let rules = prefix.join("nix-store-rules");
        let scanner = prefix.join("nix-store-scanner");
        let docs = prefix.join("nix-store-docs");
        let ca = prefix.join("nix-store-ca");
        fs::write(
            prefix.join("share/project-check/tools.json"),
            serde_json::to_vec(
                &json!({"version":1,"rules":rules,"scanner":scanner,"docs":docs,"ca":ca}),
            )
            .unwrap(),
        )
        .unwrap();
        let tools = Tools::from_prefix(prefix).unwrap();
        assert_eq!(tools.rules, rules);
        assert_eq!(tools.scanner, scanner);
        assert_eq!(tools.docs, docs);
        assert_eq!(tools.ca, Some(ca));
    }

    #[cfg(windows)]
    #[test]
    fn windows_resolution_adds_exe_and_rejects_implicit_batch_scripts() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("native.exe"), "native fixture").unwrap();
        fs::write(root.join("native.v2.exe"), "native fixture").unwrap();
        fs::write(root.join("implicit.cmd"), "batch fixture").unwrap();
        fs::write(root.join("implicit.bat"), "batch fixture").unwrap();
        assert!(program_available("./native", root));
        assert!(program_available("./native.exe", root));
        assert!(program_available("./native.v2", root));
        assert!(!program_available("./implicit", root));
        assert!(!program_available("./implicit.cmd", root));
        assert!(!program_available("./implicit.bat", root));
    }

    #[cfg(unix)]
    #[test]
    fn prerequisites_observe_the_callers_execute_permission_class() {
        let root = tempfile::tempdir().unwrap();
        let tool = root.path().join("tool");
        fs::write(&tool, "synthetic permission fixture").unwrap();
        for mode in [0o700, 0o600, 0o641] {
            fs::set_permissions(&tool, fs::Permissions::from_mode(mode)).unwrap();
            let expected = nix::unistd::access(&tool, nix::unistd::AccessFlags::X_OK).is_ok();
            assert_eq!(
                program_available(tool.to_str().unwrap(), root.path()),
                expected,
                "mode {mode:o}"
            );
        }
    }

    #[test]
    fn oversized_compact_record_cannot_exceed_aggregate_budget() {
        let mut retained = MAX_REPORT_BYTES - 100_000;
        let record =
            json!({"name":"a".repeat(128_000),"status":"passed","diagnostics":"","returncode":0});
        let result = retain_result(record, &mut retained, 0);
        assert!(result.is_err() || retained <= MAX_REPORT_BYTES);
    }

    #[test]
    fn compact_capacity_reserves_long_valid_check_identity() {
        let name = "a".repeat(128_000);
        let reserved = compact_capacity(&name).unwrap();
        let mut retained = MAX_REPORT_BYTES - reserved - 2048;
        let first = retain_result(json!({"name":"earlier","status":"passed","diagnostics":"x".repeat(4096),"returncode":0}), &mut retained, reserved).unwrap();
        assert_eq!(first["status"], "failed");
        let last = retain_result(
            json!({"name":name,"status":"passed","diagnostics":"x".repeat(4096),"returncode":0}),
            &mut retained,
            0,
        )
        .unwrap();
        assert_eq!(last["name"], name);
        assert_eq!(last["status"], "failed");
        assert!(retained < MAX_REPORT_BYTES);
    }
}
