use crate::manifest::{self, Manifest};
use crate::process::Signals;
use crate::runner::{self, Tools};
use crate::{Error, Result, watch};
use serde_json::{Value, json};
use std::io::{self, Write};

const HELP: &str = "Usage: project-check [--plan] [--changed] [--write] [--json] {baseline,fast,full,watch,migrate}\n\nExplicit, bounded, argv-only project checks; never invoked by discovery.\n\n  --plan       inspect prerequisites without running checks\n  --changed    annotate a plan with advisory Git change relevance\n  --write      with migrate: upgrade the manifest instead of previewing\n  --json       emit JSON (JSON lines in watch mode)\n  -h, --help   show this help message\n";

#[derive(Debug)]
struct Arguments {
    profile: String,
    plan: bool,
    changed: bool,
    write: bool,
    json: bool,
}
fn parse(args: impl Iterator<Item = String>) -> std::result::Result<Option<Arguments>, String> {
    let mut arguments = Arguments {
        profile: String::new(),
        plan: false,
        changed: false,
        write: false,
        json: false,
    };
    let mut positional = false;
    for argument in args {
        match argument.as_str() {
            "-h" | "--help" if !positional => return Ok(None),
            "--plan" if !positional => arguments.plan = true,
            "--changed" if !positional => arguments.changed = true,
            "--write" if !positional => arguments.write = true,
            "--json" if !positional => arguments.json = true,
            "--" if !positional => positional = true,
            "baseline" | "fast" | "full" | "watch" | "migrate" if arguments.profile.is_empty() => {
                arguments.profile = argument
            }
            _ => return Err("unrecognized argument or profile".into()),
        }
    }
    if arguments.profile.is_empty() {
        return Err("a profile is required".into());
    }
    if arguments.plan && matches!(arguments.profile.as_str(), "watch" | "migrate") {
        return Err(format!("--plan does not support {}", arguments.profile));
    }
    if arguments.changed && !arguments.plan {
        return Err("--changed requires --plan".into());
    }
    if arguments.write && arguments.profile != "migrate" {
        return Err("--write requires migrate".into());
    }
    Ok(Some(arguments))
}

pub fn report_blocked(error: &Error, json_output: bool) {
    if json_output {
        println!(
            "{}",
            json!({"version":1,"status":"blocked","diagnostics":error.to_string(),"checks":[]})
        );
    } else {
        eprintln!("project-check: blocked: {error}");
    }
    let _ = io::stdout().flush();
}

fn print_plan(report: &Value) {
    for check in report["checks"].as_array().expect("plan checks") {
        let mut problems: Vec<String> = check["missing_programs"]
            .as_array()
            .expect("plan programs")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        if check["cwd_unavailable"] == true {
            problems.push("working directory unavailable".into());
        }
        if check["rules_unavailable"] == true {
            problems.push("portable rules unavailable".into());
        }
        let detail = if problems.is_empty() {
            String::new()
        } else {
            format!(" ({})", problems.join(", "))
        };
        let relevance = check["relevance"]
            .as_str()
            .map(|text| format!(" [{text}]"))
            .unwrap_or_default();
        println!(
            "{}: {}{relevance}{detail}",
            check["status"].as_str().unwrap_or("blocked"),
            check["name"].as_str().unwrap_or("unknown")
        );
        if let Some(expected) = check["expected_warnings"].as_array() {
            for warning in expected {
                println!(
                    "  expected diagnostic (at most {} occurrences): {}\n  reason: {}",
                    warning["max_count"],
                    warning["message"].as_str().unwrap_or(""),
                    warning["reason"].as_str().unwrap_or("")
                );
            }
        }
    }
    println!(
        "project-check plan {}: {}",
        report["profile"].as_str().unwrap_or(""),
        report["status"].as_str().unwrap_or("blocked")
    );
}

fn execute(arguments: &Arguments, signals: &Signals) -> Result<i32> {
    let root = std::env::current_dir()?.canonicalize()?;
    let tools = Tools::load()?;
    if arguments.profile == "watch" {
        watch::watch(&root, arguments.json, &tools, signals)?;
        return Ok(0);
    }
    if arguments.profile == "migrate" {
        signals.check()?;
        let report = manifest::migrate(&root, arguments.write)?;
        signals.check()?;
        if arguments.json {
            println!("{report}");
        } else if report["status"] == "outdated" {
            println!(
                "{} version {} cannot express its content; `project-check migrate --write` upgrades it to {}.",
                manifest::MANIFEST,
                report["from_version"],
                report["to_version"]
            );
        } else {
            println!(
                "{}: {} (version {})",
                manifest::MANIFEST,
                report["status"].as_str().unwrap_or("blocked"),
                report["to_version"]
            );
        }
        return Ok(i32::from(report["status"] == "outdated"));
    }
    let manifest = if arguments.profile == "baseline" {
        Manifest::empty()
    } else {
        manifest::load(&root)?
    };
    if arguments.plan {
        let report = runner::plan(
            &root,
            &manifest,
            &arguments.profile,
            arguments.changed,
            &tools,
            signals,
        )?;
        signals.check()?;
        if arguments.json {
            println!("{report}");
        } else {
            print_plan(&report);
        }
        Ok(i32::from(report["status"] != "ready"))
    } else {
        let report = runner::run(
            &root,
            &manifest,
            &arguments.profile,
            arguments.json,
            &tools,
            signals,
            None,
        )?;
        Ok(i32::from(report["status"] != "passed"))
    }
}

pub fn main(args: impl Iterator<Item = String>) -> i32 {
    let arguments = match parse(args) {
        Ok(Some(arguments)) => arguments,
        Ok(None) => {
            print!("{HELP}");
            return 0;
        }
        Err(error) => {
            eprintln!("{HELP}\nproject-check: error: {error}");
            return 2;
        }
    };
    let signals = match Signals::install() {
        Ok(signals) => signals,
        Err(error) => {
            report_blocked(&Error::from(error), arguments.json);
            return 1;
        }
    };
    finish(execute(&arguments, &signals), &signals, arguments.json)
}

fn finish(outcome: Result<i32>, signals: &Signals, json_output: bool) -> i32 {
    if let Some(signal) = signals.received() {
        return 128 + signal;
    }
    match outcome {
        Ok(code) => code,
        Err(error) => match error.interrupted {
            Some(signal) => 128 + signal,
            None => {
                report_blocked(&error, json_output);
                1
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_signal_overrules_success_or_error_without_an_active_child() {
        for signal in [2, 15] {
            let signals = Signals::pending(signal);
            assert_eq!(finish(Ok(0), &signals, true), 128 + signal);
            assert_eq!(
                finish(Err(Error::new("late error")), &signals, true),
                128 + signal
            );
        }
    }
    #[test]
    fn rejects_usage_errors_and_accepts_flag_order() {
        for args in [
            vec![],
            vec!["watch", "--plan"],
            vec!["fast", "--changed"],
            vec!["fast", "--write"],
            vec!["full", "fast"],
        ] {
            assert!(parse(args.into_iter().map(str::to_owned)).is_err());
        }
        assert!(
            parse(["--json", "fast", "--plan"].into_iter().map(str::to_owned))
                .unwrap()
                .is_some()
        );
    }
}
