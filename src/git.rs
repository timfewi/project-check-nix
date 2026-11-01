use crate::manifest::{Check, MANIFEST};
use crate::process::{Signals, capture};
use crate::{Error, MAX_OUTPUT_BYTES, Result};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::path::{Component, Path};
use std::process::Command;
use std::time::Duration;

fn output(
    root: &Path,
    arguments: &[OsString],
    allowed_codes: &[i32],
    signals: &Signals,
) -> Result<Vec<u8>> {
    signals.check()?;
    let mut command = Command::new("git");
    command.args(arguments).current_dir(root).env_clear();
    for (key, value) in env::vars_os().filter(|(key, _)| !key.to_string_lossy().starts_with("GIT_"))
    {
        command.env(key, value);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    let captured = capture(
        &mut command,
        Duration::from_secs(15),
        MAX_OUTPUT_BYTES,
        signals,
        false,
    )
    .map_err(|_| Error::new("cannot inspect Git changes"))?;
    if let Some(crate::process::CaptureFailure::Interrupted(signal)) = captured.failure {
        return Err(Error::interrupted(signal));
    }
    if captured.failure.is_some()
        || !captured
            .returncode
            .is_some_and(|code| allowed_codes.contains(&code))
    {
        return Err(Error::new("Git change list unavailable or too large"));
    }
    Ok(captured.output)
}

pub fn changed_paths(root: &Path, signals: &Signals) -> Result<Vec<String>> {
    let mut command: Vec<OsString> = [
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let mut query = command.clone();
    query.extend(
        [
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            r"^filter\..*\.(clean|smudge|process|required)$",
        ]
        .into_iter()
        .map(Into::into),
    );
    let names = output(root, &query, &[0, 1], signals)?;
    let drivers: BTreeSet<&[u8]> = names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .filter_map(|name| {
            name.iter()
                .rposition(|byte| *byte == b'.')
                .map(|index| &name[..index])
        })
        .collect();
    if names.len() > 32768
        || drivers.len() > 64
        || drivers.iter().any(|driver| driver.contains(&b'='))
    {
        return Err(Error::new(
            "Git filter configuration cannot be overridden within safe bounds",
        ));
    }
    for driver in drivers {
        let driver = std::str::from_utf8(driver)
            .map_err(|_| Error::new("non-UTF-8 Git filter is unsupported"))?;
        for (setting, value) in [
            ("clean", ""),
            ("smudge", ""),
            ("process", ""),
            ("required", "false"),
        ] {
            command.extend([
                OsString::from("-c"),
                format!("{driver}.{setting}={value}").into(),
            ]);
        }
    }
    query = command.clone();
    query.extend(["rev-parse", "--show-prefix"].into_iter().map(Into::into));
    let prefix = output(root, &query, &[0], signals)?;
    command.extend(
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=dirty",
            "--",
            ".",
        ]
        .into_iter()
        .map(Into::into),
    );
    let raw = output(root, &command, &[0], signals)?;
    parse_status(&prefix, &raw)
}

pub fn parse_status(prefix: &[u8], raw: &[u8]) -> Result<Vec<String>> {
    let prefix = prefix
        .strip_suffix(b"\n")
        .ok_or_else(|| Error::new("incomplete Git project prefix"))?;
    let prefix =
        std::str::from_utf8(prefix).map_err(|_| Error::new("non-UTF-8 Git path is unsupported"))?;
    if !raw.is_empty() && !raw.ends_with(b"\0") {
        return Err(Error::new("incomplete Git status output"));
    }
    let mut records = raw
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    let mut paths = BTreeSet::new();
    while let Some(record) = records.next() {
        if record.len() < 4
            || record[2] != b' '
            || !record[..2].iter().all(|byte| b" MADRCUT?!".contains(byte))
        {
            return Err(Error::new("invalid Git status record"));
        }
        let mut names = vec![&record[3..]];
        if record[..2].contains(&b'R') || record[..2].contains(&b'C') {
            names.push(
                records
                    .next()
                    .ok_or_else(|| Error::new("incomplete Git rename record"))?,
            );
        }
        for name in names {
            let path = std::str::from_utf8(name)
                .map_err(|_| Error::new("non-UTF-8 Git path is unsupported"))?;
            if path.is_empty()
                || Path::new(path).is_absolute()
                || Path::new(path)
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
            {
                return Err(Error::new("invalid Git path"));
            }
            if let Some(relative) = path.strip_prefix(prefix) {
                paths.insert(relative.to_owned());
            }
        }
    }
    Ok(paths.into_iter().collect())
}

pub fn relevance(check: &Check, paths: &[String]) -> Value {
    if paths
        .iter()
        .any(|path| [MANIFEST, "flake.nix", "flake.lock"].contains(&path.as_str()))
    {
        return json!({"relevance":"affected","matched_total":0,"matched_examples":["check or toolchain declaration changed"]});
    }
    let Some(inputs) = &check.input_paths else {
        return json!({"relevance":"unknown","matched_total":0,"matched_examples":[]});
    };
    let matched: Vec<_> = paths
        .iter()
        .filter(|path| {
            inputs.iter().any(|input| {
                if input.ends_with('/') {
                    path.starts_with(input)
                } else {
                    *path == input
                }
            })
        })
        .collect();
    json!({"relevance":if matched.is_empty() {"unaffected"} else {"affected"},"matched_total":matched.len(),"matched_examples":matched.into_iter().take(10).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn porcelain_rejects_malformed_and_retains_rename_sides() {
        assert_eq!(
            parse_status(
                b"nested/\n",
                b"R  nested/new.py\0nested/old.py\0?? nested/added.py\0 M other.py\0"
            )
            .unwrap(),
            ["added.py", "new.py", "old.py"]
        );
        for raw in [
            b"M bad\0".as_slice(),
            b"R  new\0",
            b"?? name",
            b"?? ../escape\0",
            b"?? \xff\0",
        ] {
            assert!(parse_status(b"\n", raw).is_err());
        }
        assert!(parse_status(b"incomplete", b"").is_err());
    }
}
