use crate::manifest::{self, MANIFEST, Manifest};
use crate::process::Signals;
use crate::runner::{self, Tools};
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[path = "watch_match.rs"]
mod matcher;

fn compile_patterns(patterns: &[String], signals: &Signals) -> Result<Vec<regex::Regex>> {
    manifest::validate_watch_ignore(patterns)?;
    patterns
        .iter()
        .map(|pattern| {
            signals.check()?;
            matcher::compile(pattern)
        })
        .collect()
}

const IGNORED: &[&str] = &[
    ".git",
    ".direnv",
    ".devenv",
    ".cargo-tmp",
    ".ruff_cache",
    ".pytest_cache",
    "__pycache__",
    "target",
    "node_modules",
    "vendor",
    "result",
    "obsidian",
    "law-main",
];
#[derive(Debug, PartialEq, Eq)]
pub struct Stamp {
    modified: (i64, i64),
    changed: (i64, i64),
    size: u64,
    inode: u64,
    device: u64,
}
pub type Snapshot = BTreeMap<PathBuf, Stamp>;

pub fn snapshot(root: &Path, patterns: &[String], signals: &Signals) -> Result<Snapshot> {
    snapshot_filtered(root, &compile_patterns(patterns, signals)?, signals)
}

fn snapshot_filtered(
    root: &Path,
    patterns: &[regex::Regex],
    signals: &Signals,
) -> Result<Snapshot> {
    let ignored = |path: &Path| {
        patterns
            .iter()
            .any(|pattern| pattern.is_match(&path.to_string_lossy()))
    };
    let mut result = BTreeMap::new();
    let mut directories = vec![(root.to_path_buf(), 0_usize)];
    let mut seen = 0;
    while let Some((directory, depth)) = directories.pop() {
        signals.check()?;
        if depth > 256 {
            return Err(Error::new("watch directory depth exceeds 256"));
        }
        for entry in fs::read_dir(directory)? {
            signals.check()?;
            seen += 1;
            if seen > 200_000 {
                return Err(Error::new(
                    "watch tree exceeds 200000 entries; use watch_ignore",
                ));
            }
            let entry = entry?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| Error::new("watch path escapes root"))?;
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                if !IGNORED.iter().any(|name| entry.file_name() == *name) && !ignored(relative) {
                    directories.push((path, depth + 1));
                }
            } else if (relative == Path::new(MANIFEST) || !ignored(relative)) && metadata.is_file()
            {
                result.insert(
                    relative.to_path_buf(),
                    Stamp {
                        modified: (metadata.mtime(), metadata.mtime_nsec()),
                        changed: (metadata.ctime(), metadata.ctime_nsec()),
                        size: metadata.len(),
                        inode: metadata.ino(),
                        device: metadata.dev(),
                    },
                );
            }
        }
    }
    Ok(result)
}

pub fn watch(root: &Path, json_output: bool, tools: &Tools, signals: &Signals) -> Result<()> {
    let mut document = match manifest::load(root) {
        Ok(document) => document,
        Err(error) => {
            crate::cli::report_blocked(&error, json_output);
            Manifest::empty()
        }
    };
    let mut filters = compile_patterns(&document.watch_ignore, signals)?;
    let mut previous = snapshot_filtered(root, &filters, signals)?;
    if document.version != 0 {
        runner::run(root, &document, "fast", json_output, tools, signals, None)?;
    }
    let mut changed_at = None;
    loop {
        signals.check()?;
        std::thread::sleep(Duration::from_millis(200));
        let current = match snapshot_filtered(root, &filters, signals) {
            Ok(snapshot) => snapshot,
            Err(error) if error.interrupted.is_some() => return Err(error),
            Err(error) => {
                crate::cli::report_blocked(&error, json_output);
                continue;
            }
        };
        if current != previous {
            previous = current;
            changed_at = Some(Instant::now());
        }
        if changed_at.is_some_and(|time| time.elapsed() >= Duration::from_millis(500)) {
            changed_at = None;
            match manifest::load(root).and_then(|loaded| {
                compile_patterns(&loaded.watch_ignore, signals).map(|filters| (loaded, filters))
            }) {
                Ok((loaded, compiled)) => {
                    document = loaded;
                    filters = compiled;
                    runner::run(root, &document, "fast", json_output, tools, signals, None)?;
                }
                Err(error) => crate::cli::report_blocked(&error, json_output),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ignores_generated_files_but_not_manifest_and_tracks_ctime() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(MANIFEST), "initial").unwrap();
        fs::create_dir(root.path().join("target")).unwrap();
        fs::write(root.path().join("target/generated"), "a").unwrap();
        let before = snapshot(root.path(), &["*".into()], &Signals::none()).unwrap();
        fs::write(root.path().join("target/generated"), "b").unwrap();
        assert_eq!(
            before,
            snapshot(root.path(), &["*".into()], &Signals::none()).unwrap()
        );
        let metadata = fs::metadata(root.path().join(MANIFEST)).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        fs::write(root.path().join(MANIFEST), "changed").unwrap();
        let file = fs::File::options()
            .write(true)
            .open(root.path().join(MANIFEST))
            .unwrap();
        file.set_times(fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
            .unwrap();
        assert_ne!(
            before,
            snapshot(root.path(), &["*".into()], &Signals::none()).unwrap()
        );
    }
    #[test]
    fn ignore_globs_keep_fnmatch_unclosed_bracket_semantics() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("generated["), "fixture").unwrap();
        assert!(
            snapshot(root.path(), &["*[".into()], &Signals::none())
                .unwrap()
                .is_empty()
        );
    }
}
