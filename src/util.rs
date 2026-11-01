use crate::{Error, Result};
use serde::Deserialize;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

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
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::new("input must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
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
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(Error::new(
            "path must be relative without NUL or parent traversal",
        ));
    }
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
    if !resolved.starts_with(root) {
        return Err(Error::new("path escapes the project"));
    }
    Ok(resolved)
}

pub fn atomic_write(path: &Path, text: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new("output has no parent"))?;
    let mode = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            metadata.permissions().mode() & 0o777
        }
        Ok(_) => return Err(Error::new("output must be a regular, non-symlink file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0o644,
        Err(error) => return Err(error.into()),
    };
    let mut temporary = tempfile::Builder::new()
        .prefix(".project-check-")
        .tempfile_in(parent)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.write_all(text.as_bytes())?;
    temporary.as_file().sync_all()?;
    // persist replaces the directory entry, never follows an existing symlink.
    temporary
        .persist(path)
        .map_err(|error| Error::new(error.error.to_string()))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
