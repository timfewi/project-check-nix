use crate::{Error, Result};
use serde::Deserialize;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

/// Include Windows junctions and other reparse points in the no-link policy.
pub fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub(crate) fn open_regular(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits());
    #[cfg(windows)]
    {
        // Avoid opening named devices or following the final reparse point.
        // Repeat the check on the opened handle to detect ordinary replacements.
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || is_link(&metadata) {
            return Err(Error::new("input must be a regular, non-reparse file"));
        }
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::new("input must be a regular file"));
    }
    #[cfg(windows)]
    if is_link(&metadata) {
        return Err(Error::new("input must be a regular, non-reparse file"));
    }
    Ok(file)
}

pub fn arguments() -> Result<Vec<String>> {
    std::env::args_os()
        .skip(1)
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| Error::new("command-line arguments must be UTF-8"))
        })
        .collect()
}

pub fn read_limited(path: &Path, limit: usize) -> Result<String> {
    let file = open_regular(path)?;
    let mut bytes = Vec::new();
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(Error::new(format!("input exceeds {limit} bytes")));
    }
    String::from_utf8(bytes).map_err(|_| Error::new("input must be UTF-8"))
}

// Reject duplicate object keys instead of silently changing a reviewed contract.
// serde_json also enforces its recursion limit before any recursive validation.
struct Unique;
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: de::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON without duplicate keys")
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_string<E: de::Error>(self, _: String) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Unique, A::Error> {
                while seq.next_element::<Unique>()?.is_some() {}
                Ok(Unique)
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut keys = HashSet::new();
                while let Some((key, _)) = map.next_entry::<String, Unique>()? {
                    if !keys.insert(key) {
                        return Err(de::Error::custom("duplicate JSON object key"));
                    }
                }
                Ok(Unique)
            }
        }
        deserializer.deserialize_any(JsonVisitor)
    }
}

pub fn parse_json(text: &str) -> Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    Unique::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(serde_json::from_str(text)?)
}

pub fn resolve_inside(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if relative.is_empty()
        || relative.contains('\0')
        || path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err(Error::new(
            "path must be relative without NUL or parent traversal",
        ));
    }
    // Windows canonical paths carry a verbatim prefix; compare canonical roots
    // as well as canonical candidates rather than mixing their spellings.
    let root = root.canonicalize()?;
    let candidate = root.join(path);
    let mut ancestor = candidate.as_path();
    let mut missing = Vec::new();
    let mut resolved = loop {
        match ancestor.canonicalize() {
            Ok(path) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Dangling symlinks must not be treated as missing components.
                if fs::symlink_metadata(ancestor).is_ok() {
                    return Err(Error::new("unavailable symlink"));
                }
                missing.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| Error::new("unavailable path"))?,
                );
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| Error::new("unavailable path"))?;
            }
            Err(error) => return Err(error.into()),
        }
    };
    for part in missing.into_iter().rev() {
        resolved.push(part);
    }
    if !resolved.starts_with(&root) {
        return Err(Error::new("path escapes the project"));
    }
    Ok(resolved)
}

pub fn atomic_write(path: &Path, text: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new("output has no parent"))?;
    let permissions = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !is_link(&metadata) => {
            #[cfg(windows)]
            if metadata.permissions().readonly() {
                return Err(Error::new("output is read-only"));
            }
            Some(metadata.permissions())
        }
        Ok(_) => return Err(Error::new("output must be a regular, non-symlink file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut temporary = tempfile::Builder::new()
        .prefix(".project-check-")
        .tempfile_in(parent)?;
    #[cfg(unix)]
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(
            permissions.map_or(0o644, |permissions| permissions.mode() & 0o777),
        ))?;
    #[cfg(windows)]
    if let Some(permissions) = permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    temporary.write_all(text.as_bytes())?;
    temporary.as_file().sync_all()?;
    // persist replaces the directory entry, never follows an existing symlink.
    temporary
        .persist(path)
        .map_err(|error| Error::new(error.error.to_string()))?;
    // Windows does not support opening and flushing a directory through std.
    // The new file is flushed before the platform's atomic replacement.
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reads_reject_directories_oversize_and_non_utf8() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input");
        fs::write(&input, b"abc").unwrap();
        assert_eq!(read_limited(&input, 3).unwrap(), "abc");
        assert!(read_limited(&input, 2).is_err());
        assert!(read_limited(root.path(), 100).is_err());
        fs::write(&input, [0xff]).unwrap();
        assert!(read_limited(&input, 100).is_err());
    }

    #[test]
    fn atomic_replace_and_missing_paths_stay_inside_the_canonical_root() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        atomic_write(&output, "first").unwrap();
        atomic_write(&output, "second").unwrap();
        assert_eq!(fs::read_to_string(&output).unwrap(), "second");
        assert_eq!(
            resolve_inside(root.path(), "missing/child").unwrap(),
            root.path().canonicalize().unwrap().join("missing/child")
        );
        assert!(resolve_inside(root.path(), "../outside").is_err());
        assert!(atomic_write(root.path(), "invalid").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_readonly_output_is_not_replaced() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        fs::write(&output, "original").unwrap();
        let writable = fs::metadata(&output).unwrap().permissions();
        let mut readonly = writable.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&output, readonly).unwrap();
        assert!(atomic_write(&output, "replacement").is_err());
        assert_eq!(fs::read_to_string(&output).unwrap(), "original");
        fs::set_permissions(&output, writable).unwrap();
    }
}
