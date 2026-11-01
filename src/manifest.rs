use crate::diagnostics;
use crate::util::{atomic_write, parse_json, read_limited, resolve_inside};
use crate::{Error, MAX_MANIFEST_BYTES, Result};
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path};
use std::sync::LazyLock;

pub const SCHEMA_VERSION: u64 = 2;
pub const MANIFEST: &str = ".project-checks.json";
const MAX_CHECKS: usize = 128;
const MAX_ARGV: usize = 4096;

pub fn validate_watch_ignore(patterns: &[String]) -> Result<()> {
    if patterns.len() > 256
        || patterns
            .iter()
            .any(|pattern| pattern.len() > 4096 || pattern.contains('\0'))
    {
        return Err(Error::new(
            "watch_ignore permits at most 256 NUL-free patterns of at most 4096 bytes each",
        ));
    }
    Ok(())
}
const DOCUMENT_FIELDS: &[(&str, u64)] = &[
    ("version", 1),
    ("checks", 1),
    ("watch_ignore", 1),
    ("warnings", 2),
];
const CHECK_FIELDS: &[(&str, u64)] = &[
    ("name", 1),
    ("argv", 1),
    ("cwd", 1),
    ("requires", 1),
    ("timeout_seconds", 1),
    ("profiles", 1),
    ("input_paths", 2),
    ("expected_warnings", 2),
    ("warnings", 2),
];

#[derive(Clone, Debug)]
pub struct Manifest {
    pub version: u64,
    pub checks: Vec<Check>,
    pub watch_ignore: Vec<String>,
}

impl Manifest {
    pub fn empty() -> Self {
        Self {
            version: 0,
            checks: Vec::new(),
            watch_ignore: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Check {
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub requires: Vec<String>,
    pub timeout_seconds: f64,
    pub profiles: Vec<String>,
    pub input_paths: Option<Vec<String>>,
    pub expected_warnings: Vec<ExpectedWarning>,
    pub warnings: WarningPolicy,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExpectedWarning {
    pub message: String,
    pub reason: String,
    pub max_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WarningPolicy {
    Fail,
    Report,
}

fn runner_location() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "project-check".into())
}

fn version(document: &Value) -> Result<u64> {
    let value = document
        .as_object()
        .and_then(|object| object.get("version"));
    if let Some(number) = value.and_then(Value::as_i64)
        && number < 1
    {
        return Err(Error::new(format!("{MANIFEST} version must be at least 1")));
    }
    let version = value
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::new(format!("{MANIFEST} requires an integer version")))?;
    if version > SCHEMA_VERSION {
        return Err(Error::new(format!(
            "{MANIFEST} declares version {version}, but this project-check supports versions 1 to {SCHEMA_VERSION} ({}). Update the project-check runner; the manifest is newer.",
            runner_location()
        )));
    }
    Ok(version)
}

fn schema_fields(
    found: &Map<String, Value>,
    known: &[(&str, u64)],
    version: u64,
    label: &str,
) -> Result<()> {
    let mut fields: Vec<&str> = found.keys().map(String::as_str).collect();
    fields.sort_unstable();
    for field in fields {
        let Some((_, introduced)) = known.iter().find(|(name, _)| *name == field) else {
            return Err(Error::new(format!(
                "{label}: unknown field '{field}'; this runner supports manifest versions 1 to {SCHEMA_VERSION} ({}). A newer project-check may define it.",
                runner_location()
            )));
        };
        if *introduced > version {
            return Err(Error::new(format!(
                "{label}: field '{field}' requires manifest version {introduced}, but {MANIFEST} declares version {version}. Run `project-check migrate --write` to upgrade it."
            )));
        }
    }
    Ok(())
}

fn strings(value: Option<&Value>, label: &str) -> Result<Vec<String>> {
    let error = || Error::new(format!("{label} must be a nonempty string array"));
    let values = value
        .and_then(Value::as_array)
        .filter(|values| !values.is_empty())
        .ok_or_else(error)?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|text| !text.is_empty() && !text.contains('\0'))
                .map(str::to_owned)
                .ok_or_else(error)
        })
        .collect()
}

fn warning_policy(value: &Value, label: &str) -> Result<WarningPolicy> {
    match value.as_str() {
        Some("fail") => Ok(WarningPolicy::Fail),
        Some("report") => Ok(WarningPolicy::Report),
        _ => Err(Error::new(format!("{label} must be 'fail' or 'report'"))),
    }
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_input_path(path: &str) -> bool {
    if path.starts_with('/') || path == "." || path.contains(['*', '?', '[', '\\']) {
        return false;
    }
    let without_suffix = path.strip_suffix('/').unwrap_or(path);
    // Match pathlib's normalized spelling, including the old accepted "./" spelling.
    without_suffix == "."
        || without_suffix
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn line_break(character: char) -> bool {
    matches!(
        character,
        '\n' | '\r' | '\u{000b}' | '\u{000c}' | '\u{001c}'
            ..='\u{001e}' | '\u{0085}' | '\u{2028}' | '\u{2029}'
    )
}

fn bounded_line(value: Option<&Value>) -> Option<&str> {
    let text = value.and_then(Value::as_str)?;
    if text.trim().is_empty() || text.contains('\0') || text.chars().count() > 4096 {
        return None;
    }
    // Python splitlines() permits one final line terminator, but not a second line.
    let line = text
        .strip_suffix("\r\n")
        .unwrap_or_else(|| text.strip_suffix(line_break).unwrap_or(text));
    if line.contains(line_break) {
        None
    } else {
        Some(text)
    }
}

fn expected_warnings(value: Option<&Value>, name: &str) -> Result<Vec<ExpectedWarning>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let entries = value
        .as_array()
        .filter(|entries| entries.len() <= 32)
        .ok_or_else(|| {
            Error::new(format!(
                "{name}.expected_warnings must be an array of at most 32 entries"
            ))
        })?;
    let mut messages = HashSet::new();
    entries
        .iter()
        .map(|entry| {
            let entry = entry
                .as_object()
                .filter(|entry| {
                    entry.len() == 3
                        && ["message", "reason", "max_count"]
                            .iter()
                            .all(|field| entry.contains_key(*field))
                })
                .ok_or_else(|| {
                    Error::new(format!(
                        "{name}.expected_warnings require message, reason and max_count"
                    ))
                })?;
            let message = bounded_line(entry.get("message"));
            let reason = bounded_line(entry.get("reason"));
            let (Some(message), Some(reason)) = (message, reason) else {
                return Err(Error::new(format!(
                    "{name}.expected_warnings require bounded, nonempty single-line text"
                )));
            };
            if !diagnostics::is_warning(message) || !messages.insert(message) {
                return Err(Error::new(format!(
                    "{name}.expected_warnings require distinct warning lines"
                )));
            }
            let max_count = entry
                .get("max_count")
                .and_then(Value::as_u64)
                .filter(|count| (1..=100).contains(count))
                .ok_or_else(|| {
                    Error::new(format!(
                        "{name}.expected_warnings max_count must be an integer between 1 and 100"
                    ))
                })?;
            Ok(ExpectedWarning {
                message: message.into(),
                reason: reason.into(),
                max_count: max_count as usize,
            })
        })
        .collect()
}

fn read_manifest(root: &Path) -> Result<(String, Value)> {
    let text = read_limited(&root.join(MANIFEST), MAX_MANIFEST_BYTES)
        .map_err(|error| Error::new(format!("cannot read {MANIFEST}: {error}")))?;
    let document = parse_json(&text)
        .map_err(|error| Error::new(format!("cannot read {MANIFEST}: {error}")))?;
    version(&document)?;
    Ok((text, document))
}

pub fn load(root: &Path) -> Result<Manifest> {
    let (_, document) = read_manifest(root)?;
    validate(root, &document)
}

pub fn validate(root: &Path, document: &Value) -> Result<Manifest> {
    let version = version(document)?;
    let document = document.as_object().expect("version requires an object");
    schema_fields(document, DOCUMENT_FIELDS, version, MANIFEST)?;
    let default_policy = match document.get("warnings") {
        Some(value) => warning_policy(value, "warnings")?,
        None if version == 1 => WarningPolicy::Fail,
        None => WarningPolicy::Report,
    };
    let entries = document
        .get("checks")
        .and_then(Value::as_array)
        .filter(|entries| !entries.is_empty())
        .ok_or_else(|| Error::new("checks must be a nonempty array"))?;
    if entries.len() > MAX_CHECKS {
        return Err(Error::new(format!(
            "checks must contain at most {MAX_CHECKS} entries"
        )));
    }
    let root = root.canonicalize()?;
    let mut names = HashSet::new();
    let mut checks = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let check = entry
            .as_object()
            .ok_or_else(|| Error::new(format!("checks[{index}] must be an object")))?;
        let label = check
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty());
        schema_fields(
            check,
            CHECK_FIELDS,
            version,
            label.unwrap_or(&format!("checks[{index}]")),
        )?;
        let name = check
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| valid_name(name) && names.insert(*name))
            .ok_or_else(|| Error::new("check names must be unique lowercase identifiers"))?;
        let argv_error = || {
            Error::new(format!(
                "{name}.argv must contain a program and string arguments"
            ))
        };
        let argv = check
            .get("argv")
            .and_then(Value::as_array)
            .filter(|argv| !argv.is_empty())
            .ok_or_else(argv_error)?;
        if argv.len() > MAX_ARGV {
            return Err(Error::new(format!(
                "{name}.argv must contain at most {MAX_ARGV} entries"
            )));
        }
        let argv: Vec<String> = argv
            .iter()
            .enumerate()
            .map(|(index, value)| {
                value
                    .as_str()
                    .filter(|arg| !arg.contains('\0') && (index > 0 || !arg.is_empty()))
                    .map(str::to_owned)
                    .ok_or_else(argv_error)
            })
            .collect::<Result<_>>()?;
        let requires = strings(check.get("requires"), &format!("{name}.requires"))?;
        let profiles = strings(check.get("profiles"), &format!("{name}.profiles"))?;
        if profiles
            .iter()
            .any(|profile| profile != "fast" && profile != "full")
        {
            return Err(Error::new(format!("{name}: unknown profile")));
        }
        let timeout_seconds = check
            .get("timeout_seconds")
            .and_then(Value::as_f64)
            .filter(|timeout| timeout.is_finite() && *timeout > 0.0 && *timeout <= 3600.0)
            .ok_or_else(|| {
                Error::new(format!(
                    "{name}: timeout must be between 0 and 3600 seconds"
                ))
            })?;
        let cwd = match check.get("cwd") {
            None => ".",
            Some(value) => value.as_str().unwrap_or(""),
        };
        if cwd.is_empty()
            || cwd.contains('\0')
            || Path::new(cwd).is_absolute()
            || Path::new(cwd)
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(Error::new(format!(
                "{name}: cwd must be relative without parent traversal"
            )));
        }
        resolve_inside(&root, cwd)
            .map_err(|_| Error::new(format!("{name}: cwd escapes the project")))?;
        let input_paths = match check.get("input_paths") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let paths = strings(Some(value), &format!("{name}.input_paths"))?;
                if paths.iter().any(|path| !valid_input_path(path)) {
                    return Err(Error::new(format!(
                        "{name}.input_paths must be exact relative paths or directories ending in /"
                    )));
                }
                Some(paths)
            }
        };
        let expected_warnings = expected_warnings(check.get("expected_warnings"), name)?;
        let warnings = match check.get("warnings") {
            Some(value) => warning_policy(value, &format!("{name}.warnings"))?,
            None if !expected_warnings.is_empty() => WarningPolicy::Fail,
            None => default_policy,
        };
        if warnings == WarningPolicy::Report && !expected_warnings.is_empty() {
            return Err(Error::new(format!(
                "{name}: expected_warnings only apply when warnings fail the check"
            )));
        }
        checks.push(Check {
            name: name.into(),
            argv,
            cwd: cwd.into(),
            requires,
            timeout_seconds,
            profiles,
            input_paths,
            expected_warnings,
            warnings,
        });
    }
    let watch_ignore = match document.get("watch_ignore") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or_else(|| Error::new("watch_ignore must be a string array"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .filter(|text| !text.contains('\0'))
                    .map(str::to_owned)
                    .ok_or_else(|| Error::new("watch_ignore must be a string array"))
            })
            .collect::<Result<_>>()?,
    };
    validate_watch_ignore(&watch_ignore)?;
    Ok(Manifest {
        version,
        checks,
        watch_ignore,
    })
}

fn preserve_layout(text: &str, document: &Value, upgraded: &Value) -> Option<String> {
    let original = document.as_object()?;
    let upgraded_fields = upgraded.as_object()?;
    if original
        .iter()
        .any(|(key, value)| key != "version" && upgraded_fields.get(key) != Some(value))
    {
        return None;
    }
    static VERSION_LINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)^([ \t]*)"version"\s*:\s*\d+"#).expect("valid version-line regex")
    });
    let captures = VERSION_LINE.captures(text)?;
    let matched = captures.get(0)?;
    let indent = captures.get(1)?.as_str();
    let mut replacement = format!("{indent}\"version\": {}", upgraded.get("version")?);
    for (key, value) in upgraded_fields {
        if !original.contains_key(key) {
            replacement.push_str(&format!(
                ",\n{indent}{}: {value}",
                serde_json::to_string(key).ok()?
            ));
        }
    }
    let mut rewritten = String::with_capacity(text.len() + replacement.len());
    rewritten.push_str(&text[..matched.start()]);
    rewritten.push_str(&replacement);
    rewritten.push_str(&text[matched.end()..]);
    (parse_json(&rewritten).ok()?.eq(upgraded)).then_some(rewritten)
}

fn regular_manifest(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::new(format!(
            "{MANIFEST} migration requires a regular, non-symlink file"
        )));
    }
    Ok(())
}

fn write_migration(path: &Path, original: &str, rewritten: &str) -> Result<()> {
    regular_manifest(path)?;
    let current = read_limited(path, MAX_MANIFEST_BYTES)?;
    if current.as_bytes() != original.as_bytes() {
        return Err(Error::new(format!(
            "{MANIFEST} changed during migration; refusing to overwrite it"
        )));
    }
    // The comparison detects intervening edits; the checkout must remain stable during replacement.
    // atomic_write exclusively creates its temporary and never follows the old fixed temporary path.
    atomic_write(path, rewritten)
}

pub fn migrate(root: &Path, write: bool) -> Result<Value> {
    let path = root.join(MANIFEST);
    regular_manifest(&path)?;
    let (text, document) = read_manifest(root)?;
    let original = version(&document)?;
    let mut upgraded = document.clone();
    if original == 1 {
        let fields = upgraded
            .as_object_mut()
            .expect("version requires an object");
        fields.entry("warnings").or_insert_with(|| json!("fail"));
        fields.insert("version".into(), json!(SCHEMA_VERSION));
    }
    let status = if validate(root, &document).is_ok() {
        "current"
    } else {
        validate(root, &upgraded)?;
        if write { "migrated" } else { "outdated" }
    };
    if status == "migrated" {
        let rewritten = match preserve_layout(&text, &document, &upgraded) {
            Some(text) => text,
            None => format!("{}\n", serde_json::to_string_pretty(&upgraded)?),
        };
        write_migration(&path, &text, &rewritten)?;
    }
    Ok(json!({
        "version": 1,
        "mode": "migrate",
        "status": status,
        "from_version": original,
        "to_version": if status == "current" { original } else { SCHEMA_VERSION },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn check() -> Value {
        json!({
            "name": "one", "argv": ["program", ""], "requires": ["program"],
            "profiles": ["fast", "full"], "timeout_seconds": 0.5,
        })
    }

    fn document(version: u64) -> Value {
        json!({"version": version, "checks": [check()]})
    }

    fn warning() -> Value {
        json!({"message": "warning: fixture", "reason": "Deliberate fixture.", "max_count": 2})
    }

    fn invalid(root: &Path, document: &Value, diagnostic: &str) {
        let error = validate(root, document).unwrap_err();
        assert!(error.to_string().contains(diagnostic), "{error}");
    }

    #[test]
    fn defaults_preserve_versions_and_empty_arguments() {
        let root = TempDir::new().unwrap();
        for (version, policy) in [(1, WarningPolicy::Fail), (2, WarningPolicy::Report)] {
            let manifest = validate(root.path(), &document(version)).unwrap();
            assert_eq!(manifest.version, version);
            assert_eq!(manifest.checks[0].cwd, ".");
            assert_eq!(manifest.checks[0].argv, ["program", ""]);
            assert_eq!(manifest.checks[0].warnings, policy);
            assert_eq!(manifest.checks[0].timeout_seconds, 0.5);
            assert!(manifest.watch_ignore.is_empty());
            assert!(manifest.checks[0].input_paths.is_none());
        }
        let mut declared = document(2);
        declared["checks"][0]["name"] = json!("baseline");
        assert_eq!(
            validate(root.path(), &declared).unwrap().checks[0].name,
            "baseline"
        );
        assert!(Manifest::empty().checks.is_empty());
    }

    #[test]
    fn versions_and_schema_drift_are_actionable() {
        let root = TempDir::new().unwrap();
        for version in [Value::Null, json!(true), json!(1.0), json!("1")] {
            let mut declared = document(1);
            declared["version"] = version;
            invalid(root.path(), &declared, "requires an integer version");
        }
        for version in [json!(0), json!(-1)] {
            let mut declared = document(1);
            declared["version"] = version;
            invalid(root.path(), &declared, "version must be at least 1");
        }
        invalid(root.path(), &document(3), "Update the project-check runner");
        invalid(root.path(), &json!([]), "integer version");
        let mut declared = document(1);
        declared["checks"][0]["input_paths"] = json!(["src/"]);
        invalid(
            root.path(),
            &declared,
            "'input_paths' requires manifest version 2",
        );
        invalid(root.path(), &declared, "migrate --write");
        declared["version"] = json!(2);
        declared["checks"][0]["surprise"] = json!(true);
        invalid(root.path(), &declared, "unknown field 'surprise'");
        declared["checks"][0]
            .as_object_mut()
            .unwrap()
            .remove("surprise");
        declared["surprise"] = json!(true);
        invalid(
            root.path(),
            &declared,
            ".project-checks.json: unknown field 'surprise'",
        );
        declared.as_object_mut().unwrap().remove("surprise");
        declared["version"] = json!(1);
        declared["warnings"] = json!("fail");
        invalid(
            root.path(),
            &declared,
            "field 'warnings' requires manifest version 2",
        );
    }

    #[test]
    fn validation_enforces_limits_and_contract_fields() {
        let root = TempDir::new().unwrap();
        let mut declared = document(2);
        declared["checks"] = json!([]);
        invalid(root.path(), &declared, "nonempty array");
        declared["checks"] = json!([null]);
        invalid(root.path(), &declared, "checks[0] must be an object");
        let checks: Vec<Value> = (0..MAX_CHECKS)
            .map(|index| {
                let mut item = check();
                item["name"] = json!(format!("check-{index}"));
                item
            })
            .collect();
        declared["checks"] = json!(checks);
        assert_eq!(
            validate(root.path(), &declared).unwrap().checks.len(),
            MAX_CHECKS
        );
        declared["checks"].as_array_mut().unwrap().push(check());
        invalid(root.path(), &declared, "at most 128");
        declared = document(2);
        declared["checks"][0]["argv"] = json!(vec!["program"; MAX_ARGV]);
        assert_eq!(
            validate(root.path(), &declared).unwrap().checks[0]
                .argv
                .len(),
            MAX_ARGV
        );
        declared["checks"][0]["argv"]
            .as_array_mut()
            .unwrap()
            .push(json!(""));
        invalid(root.path(), &declared, "at most 4096");
        for (field, values, message) in [
            (
                "name",
                vec![json!("Upper"), json!("a_b"), json!("")],
                "lowercase identifiers",
            ),
            (
                "argv",
                vec![
                    json!([]),
                    json!([""]),
                    json!(["program", null]),
                    json!(["a\0b"]),
                ],
                "string arguments",
            ),
            (
                "requires",
                vec![json!([]), json!([""]), json!(["a\0b"])],
                "nonempty string array",
            ),
            ("profiles", vec![json!([]), json!(["guessed"])], "profile"),
            (
                "timeout_seconds",
                vec![json!(true), json!(0), json!(-1), json!(3600.1)],
                "timeout",
            ),
        ] {
            for value in values {
                declared = document(2);
                declared["checks"][0][field] = value;
                invalid(root.path(), &declared, message);
            }
        }
        declared = document(2);
        declared["checks"][0]["timeout_seconds"] = json!(3600);
        validate(root.path(), &declared).unwrap();
        declared["checks"].as_array_mut().unwrap().push(check());
        invalid(root.path(), &declared, "unique lowercase identifiers");
    }

    #[test]
    fn paths_reject_nul_and_escapes_but_allow_missing_cwd() {
        let root = TempDir::new().unwrap();
        for cwd in [
            json!("bad\0cwd"),
            json!("../escape"),
            json!("/absolute"),
            json!(null),
            json!(""),
        ] {
            let mut declared = document(2);
            declared["checks"][0]["cwd"] = cwd;
            invalid(root.path(), &declared, "cwd must be relative");
        }
        let mut declared = document(2);
        declared["checks"][0]["cwd"] = json!("not-yet-created/subdirectory");
        validate(root.path(), &declared).unwrap();
        for value in [json!(null), json!("*.tmp"), json!(["bad\0pattern"])] {
            declared["watch_ignore"] = value;
            invalid(root.path(), &declared, "watch_ignore");
        }
        declared["watch_ignore"] = json!([]);
        assert!(
            validate(root.path(), &declared)
                .unwrap()
                .watch_ignore
                .is_empty()
        );
        declared["watch_ignore"] = json!(["", "*.tmp"]);
        validate(root.path(), &declared).unwrap();
        for path in [
            "../outside",
            "/absolute",
            "src/*",
            "src//file",
            ".",
            "src/./file",
            "src/../file",
            "src\\file",
            "src//",
        ] {
            let mut declared = document(2);
            declared["checks"][0]["input_paths"] = json!([path]);
            invalid(root.path(), &declared, "input_paths");
        }
        for paths in [json!(null), json!(["src/", "docs/readme.md"])] {
            let mut declared = document(2);
            declared["checks"][0]["input_paths"] = paths.clone();
            let loaded = validate(root.path(), &declared).unwrap();
            assert_eq!(loaded.checks[0].input_paths.is_none(), paths.is_null());
        }
    }

    #[test]
    fn warning_policy_precedence_and_declarations() {
        let root = TempDir::new().unwrap();
        let mut declared = document(2);
        declared["warnings"] = json!("fail");
        declared["checks"][0]["warnings"] = json!("report");
        assert_eq!(
            validate(root.path(), &declared).unwrap().checks[0].warnings,
            WarningPolicy::Report
        );
        declared["warnings"] = json!("report");
        declared["checks"][0]
            .as_object_mut()
            .unwrap()
            .remove("warnings");
        declared["checks"][0]["expected_warnings"] = json!([warning()]);
        let loaded = validate(root.path(), &declared).unwrap();
        assert_eq!(loaded.checks[0].warnings, WarningPolicy::Fail);
        assert_eq!(
            serde_json::to_value(&loaded.checks[0].expected_warnings).unwrap(),
            json!([warning()])
        );
        declared["checks"][0]["warnings"] = json!("report");
        invalid(root.path(), &declared, "only apply when warnings fail");
        declared["checks"][0]["expected_warnings"] = json!([]);
        validate(root.path(), &declared).unwrap();
        declared["warnings"] = json!("ignore");
        invalid(
            root.path(),
            &declared,
            "warnings must be 'fail' or 'report'",
        );
        declared.as_object_mut().unwrap().remove("warnings");
        declared["checks"][0]["warnings"] = json!(null);
        invalid(
            root.path(),
            &declared,
            "one.warnings must be 'fail' or 'report'",
        );
        let mut invalid_entries = vec![
            Value::Null,
            json!("warning: guessed"),
            json!([null]),
            json!([warning(), warning()]),
        ];
        invalid_entries.push(json!(
            (0..33)
                .map(|index| {
                    let mut entry = warning();
                    entry["message"] = json!(format!("warning: fixture {index}"));
                    entry
                })
                .collect::<Vec<_>>()
        ));
        for field in ["message", "reason", "max_count"] {
            let mut entry = warning();
            entry.as_object_mut().unwrap().remove(field);
            invalid_entries.push(json!([entry]));
        }
        for (field, values) in [
            (
                "message",
                vec![
                    json!("not a diagnostic"),
                    json!("warning: first\nwarning: second"),
                    json!("warning: first\rsecond"),
                    json!("warning: first\0second"),
                    json!(format!("warning: {}", "x".repeat(4096))),
                ],
            ),
            (
                "reason",
                vec![json!(" "), json!("first\nsecond"), json!("x".repeat(4097))],
            ),
            (
                "max_count",
                vec![json!(true), json!(0), json!(-1), json!(101), json!(1.5)],
            ),
        ] {
            for value in values {
                let mut entry = warning();
                entry[field] = value;
                invalid_entries.push(json!([entry]));
            }
        }
        let mut entry = warning();
        entry["unknown"] = json!(true);
        invalid_entries.push(json!([entry]));
        for entries in invalid_entries {
            let mut declared = document(2);
            declared["checks"][0]["expected_warnings"] = entries;
            invalid(root.path(), &declared, "expected_warnings");
        }
        for count in [1, 100] {
            let mut declared = document(2);
            let mut entry = warning();
            entry["max_count"] = json!(count);
            entry["reason"] = json!("é".repeat(4096));
            declared["checks"][0]["expected_warnings"] = json!([entry]);
            validate(root.path(), &declared).unwrap();
        }
    }

    #[test]
    fn load_rejects_duplicate_keys_size_and_excessive_nesting() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(MANIFEST);
        fs::write(&path, r#"{"version":1,"version":2,"checks":[]}"#).unwrap();
        assert!(
            load(root.path())
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        fs::write(&path, " ".repeat(MAX_MANIFEST_BYTES + 1)).unwrap();
        assert!(
            load(root.path())
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
        fs::write(&path, format!("{}0{}", "[".repeat(256), "]".repeat(256))).unwrap();
        assert!(
            load(root.path())
                .unwrap_err()
                .to_string()
                .contains("recursion")
        );
        let valid = serde_json::to_string(&document(2)).unwrap();
        fs::write(
            &path,
            format!("{valid}{}", " ".repeat(MAX_MANIFEST_BYTES - valid.len())),
        )
        .unwrap();
        load(root.path()).unwrap();
    }

    #[test]
    fn migration_preview_write_and_current_preserve_layout_and_strictness() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(MANIFEST);
        let plain = serde_json::to_string(&document(1)).unwrap();
        fs::write(&path, &plain).unwrap();
        let report = migrate(root.path(), true).unwrap();
        assert_eq!(report["status"], "current");
        assert_eq!(report["to_version"], 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), plain);
        let mut old = document(1);
        old["checks"][0]["input_paths"] = json!(["src/"]);
        let formatted = format!(
            "{{\n    \"version\": 1,\n    \"checks\": [{}]\n}}\n",
            serde_json::to_string(&old["checks"][0]).unwrap()
        );
        fs::write(&path, &formatted).unwrap();
        let preview = migrate(root.path(), false).unwrap();
        assert_eq!(
            preview,
            json!({"version":1, "mode":"migrate", "status":"outdated", "from_version":1, "to_version":2})
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), formatted);
        assert_eq!(migrate(root.path(), true).unwrap()["status"], "migrated");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            formatted.replace(
                "    \"version\": 1",
                "    \"version\": 2,\n    \"warnings\": \"fail\""
            )
        );
        assert_eq!(
            load(root.path()).unwrap().checks[0].warnings,
            WarningPolicy::Fail
        );
        assert_eq!(migrate(root.path(), false).unwrap()["status"], "current");
        fs::write(&path, serde_json::to_string(&old).unwrap()).unwrap();
        migrate(root.path(), true).unwrap();
        let mut expected = old;
        expected["version"] = json!(2);
        expected["warnings"] = json!("fail");
        assert_eq!(
            parse_json(&fs::read_to_string(&path).unwrap()).unwrap(),
            expected
        );
        assert!(fs::read_to_string(&path).unwrap().ends_with('\n'));
        let invalid = r#"{"version":1,"checks":[{"name":"x"}]}"#;
        fs::write(&path, invalid).unwrap();
        assert!(migrate(root.path(), true).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
    }

    #[test]
    fn migration_refuses_intervening_bytes() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(MANIFEST);
        fs::write(&path, "concurrent edit").unwrap();
        let error = write_migration(&path, "original", "replacement").unwrap_err();
        assert!(error.to_string().contains("changed during migration"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "concurrent edit");
    }

    #[cfg(unix)]
    #[test]
    fn migration_ignores_fixed_temporary_symlink_and_refuses_manifest_symlink() {
        use std::os::unix::fs::symlink;
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let protected = outside.path().join("protected.txt");
        fs::write(&protected, "untouched fixture").unwrap();
        let fixed = root.path().join(format!(".{MANIFEST}.migrate"));
        symlink(&protected, &fixed).unwrap();
        let path = root.path().join(MANIFEST);
        let mut old = document(1);
        old["checks"][0]["input_paths"] = json!(["src/"]);
        fs::write(&path, serde_json::to_string(&old).unwrap()).unwrap();
        assert_eq!(migrate(root.path(), true).unwrap()["status"], "migrated");
        assert_eq!(fs::read_to_string(&protected).unwrap(), "untouched fixture");
        assert_eq!(fs::read_link(&fixed).unwrap(), protected);
        assert!(
            !fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_file(&path).unwrap();
        symlink(&protected, &path).unwrap();
        for write in [false, true] {
            assert!(
                migrate(root.path(), write)
                    .unwrap_err()
                    .to_string()
                    .contains("non-symlink")
            );
        }
        assert_eq!(fs::read_to_string(&protected).unwrap(), "untouched fixture");
        fs::remove_file(&path).unwrap();
        fs::write(&path, serde_json::to_string(&document(2)).unwrap()).unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        let mut declared = document(2);
        declared["checks"][0]["cwd"] = json!("escape");
        invalid(root.path(), &declared, "cwd escapes the project");
    }

    #[test]
    fn watch_filter_compiler_work_has_explicit_bounds() {
        assert!(validate_watch_ignore(&vec!["".into(); 256]).is_ok());
        assert!(validate_watch_ignore(&vec!["".into(); 257]).is_err());
        assert!(validate_watch_ignore(&["x".repeat(4096)]).is_ok());
        assert!(validate_watch_ignore(&["x".repeat(4097)]).is_err());
        let root = TempDir::new().unwrap();
        let mut declared = document(2);
        declared["watch_ignore"] = json!(vec![""; 257]);
        invalid(root.path(), &declared, "256");
    }
}
