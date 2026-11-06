#![forbid(unsafe_code)]

//! Native fixtures for process contracts. This binary is built only when the
//! explicit `test-support` feature is enabled and is never part of a release.

use serde_json::json;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn number(value: &str) -> u64 {
    value.parse().expect("fixture numeric argument")
}

fn pause(milliseconds: &str) {
    thread::sleep(Duration::from_millis(number(milliseconds)));
}

fn bytes(count: usize, stream: &mut impl Write) -> io::Result<()> {
    let block = [b'x'; 8192];
    let mut remaining = count;
    while remaining > 0 {
        let size = remaining.min(block.len());
        stream.write_all(&block[..size])?;
        remaining -= size;
    }
    stream.flush()
}

fn scanner(arguments: &[String]) -> io::Result<()> {
    let output = arguments
        .windows(2)
        .find(|pair| pair[0] == "--json-output")
        .map(|pair| &pair[1])
        .expect("fixture scanner output argument");
    match env::var("CONTRACT_SCANNER_MODE").as_deref() {
        Ok("missing") => return Ok(()),
        Ok("malformed") => return fs::write(output, "invalid report"),
        _ => (),
    }
    let findings = if env::var("CONTRACT_SCANNER_MODE").as_deref() == Ok("finding") {
        json!([{"check_id":"synthetic","extra":{"severity":"ERROR"}}])
    } else {
        json!([])
    };
    fs::write(
        output,
        json!({"paths":{"scanned":["fixture.py"]},"results":findings,"errors":[]}).to_string(),
    )
}

fn run(arguments: &[String]) -> io::Result<()> {
    match arguments.first().map(String::as_str).unwrap_or("") {
        "argv" => println!("{}", json!(&arguments[1..])),
        "exit" => {
            io::stdout().write_all(arguments[2].as_bytes())?;
            io::stdout().flush()?;
            io::stderr().write_all(arguments[3].as_bytes())?;
            io::stderr().flush()?;
            std::process::exit(number(&arguments[1]) as i32);
        }
        "bytes" => match arguments[2].as_str() {
            "out" => bytes(number(&arguments[1]) as usize, &mut io::stdout().lock())?,
            "err" => bytes(number(&arguments[1]) as usize, &mut io::stderr().lock())?,
            "split" => {
                bytes(number(&arguments[1]) as usize, &mut io::stdout().lock())?;
                bytes(number(&arguments[1]) as usize, &mut io::stderr().lock())?;
            }
            _ => panic!("unknown fixture stream"),
        },
        "ordered" => {
            io::stdout().write_all(b"first")?;
            io::stdout().flush()?;
            io::stderr().write_all(b"second")?;
            io::stderr().flush()?;
            io::stdout().write_all(b"third")?;
            io::stdout().flush()?;
        }
        "sleep" => pause(&arguments[1]),
        "write" => fs::write(&arguments[1], &arguments[2])?,
        "invalid-utf8" => io::stdout().write_all(&[0xff, b'\n'])?,
        "watch-check" => {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&arguments[1])?
                .write_all(b"x")?;
            fs::write(&arguments[2], "ready")?;
            pause(&arguments[3]);
        }
        "heartbeat" => {
            let deadline = Instant::now() + Duration::from_millis(number(&arguments[2]));
            while Instant::now() < deadline {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&arguments[1])?
                    .write_all(b"x")?;
                thread::sleep(Duration::from_millis(20));
            }
        }
        "tree" => {
            let mut child = Command::new(env::current_exe()?)
                .args(["heartbeat", &arguments[1], "5000"])
                .stdin(Stdio::null())
                .spawn()?;
            let deadline = Instant::now() + Duration::from_secs(3);
            while !Path::new(&arguments[1]).exists() {
                if Instant::now() >= deadline {
                    child.kill()?;
                    let _ = child.wait();
                    return Err(io::Error::other("fixture descendant did not start"));
                }
                thread::sleep(Duration::from_millis(5));
            }
            if arguments[2] == "wait" {
                thread::sleep(Duration::from_secs(30));
                let _ = child.wait();
            }
        }
        #[cfg(unix)]
        "close-output" => {
            nix::unistd::close(1)?;
            nix::unistd::close(2)?;
            pause(&arguments[1]);
        }
        "scan" => scanner(arguments)?,
        _ => panic!("unknown native fixture mode"),
    }
    Ok(())
}

fn main() {
    if let Err(error) = run(&env::args().skip(1).collect::<Vec<_>>()) {
        eprintln!("contract fixture: {error}");
        std::process::exit(70);
    }
}
