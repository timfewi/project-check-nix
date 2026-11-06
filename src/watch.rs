use crate::manifest::{self, MANIFEST, Manifest};
use crate::process::Signals;
use crate::runner::{self, Tools};
use crate::{Error, Result, util};
#[cfg(windows)]
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
#[cfg(windows)]
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

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
    modified: SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
    size: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(windows)]
    fingerprint: [u8; 32],
}
pub type Snapshot = BTreeMap<PathBuf, Stamp>;

fn logical_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(windows)]
const MAX_FINGERPRINT_FILE_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(windows)]
const MAX_FINGERPRINT_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;

#[cfg(windows)]
fn fingerprint(path: &Path, remaining: &mut u64, signals: &Signals) -> Result<[u8; 32]> {
    let mut file = util::open_regular(path)?;
    if file.metadata()?.len() > MAX_FINGERPRINT_FILE_BYTES {
        return Err(Error::new(
            "Windows watch file exceeds 64 MiB fingerprint limit; use watch_ignore",
        ));
    }
    let mut hash = Sha256::new();
    let mut read = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        signals.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        read += count as u64;
        if read > MAX_FINGERPRINT_FILE_BYTES {
            return Err(Error::new(
                "Windows watch file exceeds 64 MiB fingerprint limit; use watch_ignore",
            ));
        }
        *remaining = remaining.checked_sub(count as u64).ok_or_else(|| {
            Error::new("Windows watch exceeds 256 MiB fingerprint budget; use watch_ignore")
        })?;
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().into())
}

pub fn snapshot(root: &Path, patterns: &[String], signals: &Signals) -> Result<Snapshot> {
    snapshot_filtered(root, &compile_patterns(patterns, signals)?, signals)
}

fn snapshot_filtered(
    root: &Path,
    patterns: &[regex::Regex],
    signals: &Signals,
) -> Result<Snapshot> {
    let ignored = |path: &Path| {
        let path = logical_path(path);
        patterns.iter().any(|pattern| pattern.is_match(&path))
    };
    let mut result = BTreeMap::new();
    let mut directories = vec![(root.to_path_buf(), 0_usize)];
    let mut seen = 0;
    #[cfg(windows)]
    let mut fingerprint_budget = MAX_FINGERPRINT_SNAPSHOT_BYTES;
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
            if util::is_link(&metadata) {
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
                        modified: metadata.modified()?,
                        #[cfg(unix)]
                        changed: (metadata.ctime(), metadata.ctime_nsec()),
                        size: metadata.len(),
                        #[cfg(unix)]
                        inode: metadata.ino(),
                        #[cfg(unix)]
                        device: metadata.dev(),
                        // Windows creation time is not Unix change time. A
                        // bounded content fingerprint also detects same-size
                        // edits with restored modification timestamps.
                        #[cfg(windows)]
                        fingerprint: fingerprint(&path, &mut fingerprint_budget, signals)?,
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
    fn ignores_generated_files_but_not_manifest_and_tracks_restored_mtime_edits() {
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

    #[test]
    fn logical_slashes_match_nested_ignore_patterns() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("generated")).unwrap();
        fs::write(root.path().join("generated/output.txt"), "fixture").unwrap();
        fs::write(root.path().join("source.txt"), "source").unwrap();
        let state = snapshot(root.path(), &["generated/*".into()], &Signals::none()).unwrap();
        assert_eq!(state.keys().collect::<Vec<_>>(), [Path::new("source.txt")]);
        assert_eq!(
            logical_path(&Path::new("nested").join("file.txt")),
            "nested/file.txt"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_fingerprint_budget_is_enforced_and_cancellable() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input");
        fs::write(&input, "fixture").unwrap();
        let mut budget = 3;
        assert!(fingerprint(&input, &mut budget, &Signals::none()).is_err());
        let mut budget = 100;
        let cancelled = Signals::pending(2);
        assert!(fingerprint(&input, &mut budget, &cancelled).is_err());
    }
}
