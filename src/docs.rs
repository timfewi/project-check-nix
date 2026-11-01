//! Source-backed dependency documentation. Python source is parsed, never executed.
use crate::process::{CaptureFailure, Signals, capture};
use crate::{Error, Result, util};
use regex::Regex;
use serde_json::Value as Json;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::Duration;

#[path = "python_literal.rs"]
mod python_literal;
#[path = "toml_guard.rs"]
mod toml_guard;
#[path = "unicode_printable.rs"]
mod unicode_printable;

const MANIFEST: &str = ".dependency-docs.json";
const LIMIT: usize = 1_000_000;
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{\{\s*([\w.-]+)(?:\s*\|\s*(json|table))?\s*\}\}")
        .expect("constant placeholder pattern")
});
static ATTRIBUTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w.-]+$").expect("constant Nix attribute pattern"));

#[derive(Debug)]
struct DocError {
    message: String,
    blocked: bool,
    interrupted: Option<i32>,
}
type DocResult<T, E = DocError> = std::result::Result<T, E>;
impl DocError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            blocked: false,
            interrupted: None,
        }
    }
    fn blocked(message: &str) -> Self {
        Self {
            message: message.into(),
            blocked: true,
            interrupted: None,
        }
    }
}
impl From<Error> for DocError {
    fn from(error: Error) -> Self {
        Self {
            message: error.message,
            blocked: false,
            interrupted: error.interrupted,
        }
    }
}
impl From<std::io::Error> for DocError {
    fn from(_: std::io::Error) -> Self {
        Self::new("invalid manifest, source or template")
    }
}

// Retain Python literal types instead of coercing tuples, bytes, or dictionary
// keys through JSON. Unsupported JSON/filter combinations fail explicitly.
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Null,
    Bool(bool),
    Integer(String),
    Float(f64),
    Complex(f64, f64),
    String(String),
    Bytes(Vec<u8>),
    Ellipsis,
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Set(Vec<Value>),
    Map(Vec<(Value, Value)>),
    DateTime(String),
}
impl Value {
    fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Map(entries) => entries
                .iter()
                .find_map(|(k, v)| matches!(k, Self::String(s) if s == key).then_some(v)),
            _ => None,
        }
    }
    fn from_json(value: Json) -> Self {
        match value {
            Json::Null => Self::Null,
            Json::Bool(v) => Self::Bool(v),
            Json::Number(v) => {
                let text = v.to_string();
                if text.contains(['.', 'e', 'E']) {
                    Self::Float(text.parse().expect("validated JSON number"))
                } else {
                    Self::Integer(if text == "-0" { "0".into() } else { text })
                }
            }
            Json::String(v) => Self::String(v),
            Json::Array(v) => Self::List(v.into_iter().map(Self::from_json).collect()),
            Json::Object(v) => Self::Map(
                v.into_iter()
                    .map(|(k, v)| (Self::String(k), Self::from_json(v)))
                    .collect(),
            ),
        }
    }
    fn from_toml(value: toml::Value) -> Self {
        match value {
            toml::Value::String(v) => Self::String(v),
            toml::Value::Integer(v) => Self::Integer(v.to_string()),
            toml::Value::Float(v) => Self::Float(v),
            toml::Value::Boolean(v) => Self::Bool(v),
            toml::Value::Datetime(v) => Self::DateTime(datetime_string(&v.to_string())),
            toml::Value::Array(v) => Self::List(v.into_iter().map(Self::from_toml).collect()),
            toml::Value::Table(v) => Self::Map(
                v.into_iter()
                    .map(|(k, v)| (Self::String(k), Self::from_toml(v)))
                    .collect(),
            ),
        }
    }
}

fn datetime_string(text: &str) -> String {
    let text = text.replace('T', " ").replace('Z', "+00:00");
    if let Some((head, tail)) = text.split_once('.') {
        let fraction_end = tail.find(['+', '-']).unwrap_or(tail.len());
        let fraction = &tail[..fraction_end];
        let mut micros = fraction.chars().take(6).collect::<String>();
        while micros.len() < 6 {
            micros.push('0');
        }
        if micros.chars().all(|c| c == '0') {
            format!("{head}{}", &tail[fraction_end..])
        } else {
            format!("{head}.{micros}{}", &tail[fraction_end..])
        }
    } else {
        text
    }
}

fn local_path(root: &Path, value: &Json, output: bool) -> DocResult<PathBuf> {
    let name = value
        .as_str()
        .filter(|name| !name.is_empty() && !name.contains('\0'))
        .ok_or_else(|| DocError::new("Documentation paths must be nonempty strings"))?;
    let relative = Path::new(name);
    if relative.is_absolute()
        || relative
            .components()
            .any(|p| matches!(p, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(DocError::new(
            "Documentation paths must stay inside the repository",
        ));
    }
    let mut candidate = root.to_path_buf();
    for part in relative.components() {
        if let Component::Normal(part) = part {
            if [".git", ".direnv", ".venv", "node_modules", "__pycache__"]
                .iter()
                .any(|state| part == *state)
            {
                return Err(DocError::new(
                    "Documentation paths must not include repository or runtime state",
                ));
            }
            candidate.push(part);
            match fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(DocError::new("Documentation paths must not use symlinks"));
                }
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    if output
        && (candidate.extension().and_then(|s| s.to_str()) != Some("md")
            || !candidate.parent().is_some_and(Path::is_dir))
    {
        return Err(DocError::new(
            "Documentation targets require an existing directory and .md suffix",
        ));
    }
    if candidate.exists() && !candidate.is_file() {
        return Err(DocError::new(
            "Documentation inputs and targets must be regular files",
        ));
    }
    Ok(candidate)
}

fn read(path: &Path) -> DocResult<String> {
    util::read_limited(path, LIMIT).map_err(|error| {
        if error.message.starts_with("input exceeds ") {
            DocError::new("Documentation source exceeds 1 MB")
        } else {
            DocError::new("invalid manifest, source or template")
        }
    })
}

fn nix_source(root: &Path, attribute: &Json, signals: &Signals) -> DocResult<Value> {
    nix_source_program(root, attribute, signals, "nix")
}
fn nix_source_program(
    root: &Path,
    attribute: &Json,
    signals: &Signals,
    program: &str,
) -> DocResult<Value> {
    let attribute = attribute
        .as_str()
        .filter(|v| ATTRIBUTE.is_match(v))
        .ok_or_else(|| DocError::new("Invalid Nix documentation attribute"))?;
    signals.check()?;
    let mut command = Command::new(program);
    command
        .args([
            "eval",
            "--offline",
            "--no-write-lock-file",
            "--json",
            &format!("path:.#{attribute}"),
        ])
        .current_dir(root);
    let captured = capture(
        &mut command,
        Duration::from_secs(60),
        LIMIT + 8192,
        signals,
        false,
    )
    .map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            DocError::blocked("Required program is missing: nix; enter the pinned toolchain")
        } else {
            DocError::blocked("Nix documentation evaluation is unavailable")
        }
    })?;
    signals.check()?;
    match captured.failure {
        Some(CaptureFailure::Interrupted(signal)) => return Err(Error::interrupted(signal).into()),
        Some(CaptureFailure::Timeout) => {
            return Err(DocError::blocked(
                "Nix documentation evaluation is unavailable",
            ));
        }
        Some(CaptureFailure::OutputLimit) => {
            return Err(DocError::new("Documentation source exceeds 1 MB"));
        }
        None => (),
    }
    if captured.returncode != Some(0) {
        let diagnostic =
            String::from_utf8_lossy(&captured.stderr[..captured.stderr.len().min(8192)]);
        if [
            "cannot fetch",
            "unable to download",
            "not valid",
            "No such file or directory",
            "offline",
        ]
        .iter()
        .any(|token| diagnostic.contains(token))
        {
            return Err(DocError::blocked(
                "Offline Nix documentation inputs are unavailable; enter the pinned toolchain",
            ));
        }
        return Err(DocError::new(
            "Nix documentation evaluation failed; check the declared attribute",
        ));
    }
    if captured.output.len() > LIMIT {
        return Err(DocError::new("Documentation source exceeds 1 MB"));
    }
    let text = std::str::from_utf8(&captured.output)
        .map_err(|_| DocError::new("invalid manifest, source or template"))?;
    Ok(Value::from_json(util::parse_json(text)?))
}

fn exact_fields(object: &serde_json::Map<String, Json>, fields: &[&str]) -> bool {
    object.len() == fields.len() && fields.iter().all(|field| object.contains_key(*field))
}
fn source(root: &Path, spec: &Json, signals: &Signals) -> DocResult<Value> {
    let spec = spec
        .as_object()
        .ok_or_else(|| DocError::new("Documentation source must be an object"))?;
    let kind = spec.get("kind").and_then(Json::as_str);
    if kind == Some("nix") {
        if !exact_fields(spec, &["kind", "attribute"]) {
            return Err(DocError::new("Nix sources require only kind and attribute"));
        }
        return nix_source(root, &spec["attribute"], signals);
    }
    let fields: &[&str] = if kind == Some("python-literal") {
        &["kind", "path", "symbol"]
    } else {
        &["kind", "path"]
    };
    if !exact_fields(spec, fields) {
        return Err(DocError::new("Invalid documentation source fields"));
    }
    let text = read(&local_path(root, &spec["path"], false)?)?;
    match kind {
        Some("json") => Ok(Value::from_json(util::parse_json(&text)?)),
        Some("toml") => {
            toml_guard::validate(&text)?;
            toml::from_str::<toml::Value>(&text)
                .map(Value::from_toml)
                .map_err(|_| DocError::new("invalid manifest, source or template"))
        }
        Some("python-literal") => {
            let symbol = spec["symbol"].as_str().ok_or_else(|| {
                DocError::new("Python documentation symbol is missing or is not a literal")
            })?;
            python_literal::parse(&text, symbol)
        }
        _ => Err(DocError::new("Unknown documentation source kind")),
    }
}

fn float_string(value: f64) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value == f64::INFINITY {
        return "inf".into();
    }
    if value == f64::NEG_INFINITY {
        return "-inf".into();
    }
    let text = format!("{value:?}");
    if let Some((mantissa, exponent)) = text.split_once('e') {
        let exponent: i32 = exponent.parse().expect("formatted float exponent");
        format!(
            "{mantissa}e{}{exponent:02}",
            if exponent < 0 { "-" } else { "+" },
            exponent = exponent.abs()
        )
    } else {
        text
    }
}

fn nonprintable(character: char) -> bool {
    unicode_printable::nonprintable(character)
}
fn quoted(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut output = String::new();
    output.push(quote);
    for character in text.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            c if c == quote => {
                output.push('\\');
                output.push(c);
            }
            c if nonprintable(c) => {
                let code = c as u32;
                if code < 256 {
                    output.push_str(&format!("\\x{code:02x}"));
                } else if code < 65536 {
                    output.push_str(&format!("\\u{code:04x}"));
                } else {
                    output.push_str(&format!("\\U{code:08x}"));
                }
            }
            c => output.push(c),
        }
    }
    output.push(quote);
    output
}
fn python_string(value: &Value, repr: bool) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(v) => if *v { "True" } else { "False" }.into(),
        Value::Integer(v) => v.clone(),
        Value::Float(v) => float_string(*v),
        Value::Complex(real, imaginary) => {
            let part = |n: f64| float_string(n).trim_end_matches(".0").to_string();
            if *real == 0.0 && !real.is_sign_negative() {
                format!("{}j", part(*imaginary))
            } else {
                format!(
                    "({}{}{}j)",
                    part(*real),
                    if imaginary.is_sign_negative() {
                        "-"
                    } else {
                        "+"
                    },
                    part(imaginary.abs())
                )
            }
        }
        Value::String(v) => {
            if repr {
                quoted(v)
            } else {
                v.clone()
            }
        }
        Value::Bytes(v) => {
            let quote = if v.contains(&b'\'') && !v.contains(&b'"') {
                b'"'
            } else {
                b'\''
            };
            let mut text = format!("b{}", quote as char);
            for byte in v {
                match *byte {
                    b'\\' => text.push_str("\\\\"),
                    b'\n' => text.push_str("\\n"),
                    b'\r' => text.push_str("\\r"),
                    b'\t' => text.push_str("\\t"),
                    b if b == quote => {
                        text.push('\\');
                        text.push(b as char);
                    }
                    32..=126 => text.push(*byte as char),
                    b => text.push_str(&format!("\\x{b:02x}")),
                }
            }
            text.push(quote as char);
            text
        }
        Value::Ellipsis => "Ellipsis".into(),
        Value::List(v) | Value::Tuple(v) | Value::Set(v) => {
            if matches!(value, Value::Set(_)) && v.is_empty() {
                return "set()".into();
            }
            let (start, end) = match value {
                Value::List(_) => ("[", "]"),
                Value::Tuple(_) => ("(", ")"),
                _ => ("{", "}"),
            };
            let contents = v
                .iter()
                .map(|v| python_string(v, true))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{start}{contents}{}{end}",
                if matches!(value, Value::Tuple(_)) && v.len() == 1 {
                    ","
                } else {
                    ""
                }
            )
        }
        Value::Map(v) => format!(
            "{{{}}}",
            v.iter()
                .map(|(k, v)| format!("{}: {}", python_string(k, true), python_string(v, true)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::DateTime(v) => v.clone(),
    }
}

fn json_key(value: &Value) -> DocResult<String> {
    match value {
        Value::String(v) | Value::Integer(v) => Ok(v.clone()),
        Value::Null => Ok("null".into()),
        Value::Bool(v) => Ok(v.to_string()),
        Value::Float(v) => Ok(json_float(*v)),
        _ => Err(DocError::new(
            "Documentation json filter requires JSON-compatible dictionary keys",
        )),
    }
}
fn json_float(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value == f64::INFINITY {
        "Infinity".into()
    } else if value == f64::NEG_INFINITY {
        "-Infinity".into()
    } else {
        float_string(value)
    }
}
fn append(output: &mut String, text: &str) -> DocResult<()> {
    if text.len() > LIMIT.saturating_sub(output.len()) {
        return Err(DocError::new(
            "Malformed documentation placeholder or oversized output",
        ));
    }
    output.push_str(text);
    Ok(())
}
fn json_render(value: &Value, output: &mut String, depth: usize) -> DocResult<()> {
    if depth > 128 {
        return Err(DocError::new("Documentation literal nesting exceeds limit"));
    }
    match value {
        Value::Null => append(output, "null"),
        Value::Bool(v) => append(output, if *v { "true" } else { "false" }),
        Value::Integer(v) => append(output, v),
        Value::Float(v) => append(output, &json_float(*v)),
        Value::String(v) => append(output, &serde_json::to_string(v).map_err(Error::from)?),
        Value::List(v) | Value::Tuple(v) => {
            append(output, "[")?;
            for (index, item) in v.iter().enumerate() {
                append(output, if index == 0 { "\n" } else { ",\n" })?;
                append(output, &"  ".repeat(depth + 1))?;
                json_render(item, output, depth + 1)?;
            }
            if !v.is_empty() {
                append(output, "\n")?;
                append(output, &"  ".repeat(depth))?;
            }
            append(output, "]")
        }
        Value::Map(v) => {
            append(output, "{")?;
            for (index, (key, item)) in v.iter().enumerate() {
                append(output, if index == 0 { "\n" } else { ",\n" })?;
                append(output, &"  ".repeat(depth + 1))?;
                append(
                    output,
                    &serde_json::to_string(&json_key(key)?).map_err(Error::from)?,
                )?;
                append(output, ": ")?;
                json_render(item, output, depth + 1)?;
            }
            if !v.is_empty() {
                append(output, "\n")?;
                append(output, &"  ".repeat(depth))?;
            }
            append(output, "}")
        }
        _ => Err(DocError::new(
            "Documentation json filter requires JSON-compatible literal values",
        )),
    }
}
fn compare_integers(a: &str, b: &str) -> std::cmp::Ordering {
    let (a_negative, a_digits) = (a.starts_with('-'), a.trim_start_matches('-'));
    let (b_negative, b_digits) = (b.starts_with('-'), b.trim_start_matches('-'));
    match (a_negative, b_negative) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => {
            let order = a_digits
                .len()
                .cmp(&b_digits.len())
                .then_with(|| a_digits.cmp(b_digits));
            if a_negative { order.reverse() } else { order }
        }
    }
}
fn compare_integer_float(integer: &str, float: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if float.is_nan() {
        return Ordering::Equal;
    }
    if float == f64::INFINITY {
        return Ordering::Less;
    }
    if float == f64::NEG_INFINITY {
        return Ordering::Greater;
    }
    let truncated = float.trunc();
    let truncated = if truncated == 0.0 {
        "0".into()
    } else {
        format!("{truncated:.0}")
    };
    let order = compare_integers(integer, &truncated);
    if order != Ordering::Equal || float.fract() == 0.0 {
        order
    } else if float > 0.0 {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}
fn integer_key(value: &Value) -> Option<&str> {
    match value {
        Value::Integer(v) => Some(v),
        Value::Bool(v) => Some(if *v { "1" } else { "0" }),
        _ => None,
    }
}
fn compare_keys(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    if let (Some(a), Some(b)) = (integer_key(a), integer_key(b)) {
        return Some(compare_integers(a, b));
    }
    if let (Some(a), Value::Float(b)) = (integer_key(a), b) {
        return Some(compare_integer_float(a, *b));
    }
    if let (Value::Float(a), Some(b)) = (a, integer_key(b)) {
        return Some(compare_integer_float(b, *a).reverse());
    }
    match (a, b) {
        (Value::Float(a), Value::Float(b)) => {
            Some(a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        }
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Bytes(a), Value::Bytes(b)) => Some(a.cmp(b)),
        (Value::Tuple(a), Value::Tuple(b)) => {
            for (a, b) in a.iter().zip(b) {
                if !python_literal::equivalent(a, b) {
                    return compare_keys(a, b);
                }
            }
            Some(a.len().cmp(&b.len()))
        }
        _ if python_literal::equivalent(a, b) => Some(std::cmp::Ordering::Equal),
        _ => None,
    }
}
fn scalar_string(value: &Value) -> DocResult<String> {
    fn has_unordered_set(value: &Value) -> bool {
        match value {
            Value::Set(v) if !v.is_empty() => true,
            Value::List(v) | Value::Tuple(v) => v.iter().any(has_unordered_set),
            Value::Map(v) => v
                .iter()
                .any(|(k, v)| has_unordered_set(k) || has_unordered_set(v)),
            _ => false,
        }
    }
    if has_unordered_set(value) {
        return Err(DocError::new(
            "Documentation set values have no deterministic scalar rendering; use a list or tuple",
        ));
    }
    Ok(python_string(value, false))
}
fn sortable_key(value: &Value) -> bool {
    match value {
        Value::Float(value) => !value.is_nan(),
        Value::Complex(real, imaginary) => !real.is_nan() && !imaginary.is_nan(),
        Value::Tuple(values) => values.iter().all(sortable_key),
        Value::List(_) | Value::Map(_) | Value::Set(_) | Value::DateTime(_) => false,
        _ => true,
    }
}
// Extend supported Python comparisons to one total order for the sort itself.
// Incomparability is reported separately, never represented as equality. Purely
// real complex numbers share the numeric domain, preserving Python tuple-key
// equality with real numbers without breaking transitivity.
fn total_compare_keys(a: &Value, b: &Value) -> std::cmp::Ordering {
    if python_literal::equivalent(a, b) {
        return std::cmp::Ordering::Equal;
    }
    let rank = |value: &Value| match value {
        Value::Integer(_) | Value::Bool(_) | Value::Float(_) => 0,
        Value::Complex(_, imaginary) if *imaginary == 0.0 => 0,
        Value::String(_) => 1,
        Value::Bytes(_) => 2,
        Value::Tuple(_) => 3,
        Value::Complex(_, _) => 4,
        Value::Null => 5,
        Value::Ellipsis => 6,
        _ => unreachable!("validated dictionary key"),
    };
    let order = rank(a).cmp(&rank(b));
    if order != std::cmp::Ordering::Equal {
        return order;
    }
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),
        (Value::Tuple(a), Value::Tuple(b)) => {
            for (a, b) in a.iter().zip(b) {
                let order = total_compare_keys(a, b);
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
            }
            a.len().cmp(&b.len())
        }
        (Value::Complex(ar, ai), Value::Complex(br, bi)) if *ai != 0.0 => ar
            .partial_cmp(br)
            .expect("non-NaN key")
            .then_with(|| ai.partial_cmp(bi).expect("non-NaN key")),
        _ => {
            let a_real;
            let b_real;
            let a = if let Value::Complex(real, _) = a {
                a_real = Value::Float(*real);
                &a_real
            } else {
                a
            };
            let b = if let Value::Complex(real, _) = b {
                b_real = Value::Float(*real);
                &b_real
            } else {
                b
            };
            compare_keys(a, b).expect("validated real numeric keys")
        }
    }
}
fn table(value: &Value) -> DocResult<String> {
    let Value::Map(entries) = value else {
        return Err(DocError::new("Documentation tables require a flat object"));
    };
    if entries
        .iter()
        .any(|(_, v)| matches!(v, Value::Map(_) | Value::List(_)))
    {
        return Err(DocError::new("Documentation tables require a flat object"));
    }
    if entries.iter().any(|(key, _)| !sortable_key(key)) {
        return Err(DocError::new("Documentation table keys must be comparable"));
    }
    let mut entries: Vec<_> = entries.iter().collect();
    let mut incomparable = false;
    entries.sort_by(|(a, _), (b, _)| {
        if compare_keys(a, b).is_none() {
            incomparable = true;
        }
        total_compare_keys(a, b)
    });
    if incomparable {
        return Err(DocError::new("Documentation table keys must be comparable"));
    }
    let cell = |v: &Value| scalar_string(v).map(|s| s.replace('|', "\\|").replace('\n', " "));
    let mut output = "| Dependency | Pinned version |\n| --- | --- |".to_string();
    for (key, value) in entries {
        append(
            &mut output,
            &format!("\n| {} | {} |", cell(key)?, cell(value)?),
        )?;
    }
    Ok(output)
}
fn render(template: &str, values: &Value) -> DocResult<String> {
    let mut output = String::with_capacity(template.len().min(LIMIT));
    let mut previous = 0;
    for matched in PLACEHOLDER.captures_iter(template) {
        let whole = matched.get(0).expect("whole match");
        append(&mut output, &template[previous..whole.start()])?;
        let mut value = values;
        for component in matched[1].split('.') {
            value = value.get(component).ok_or_else(|| {
                DocError::new(format!(
                    "Unknown documentation placeholder: {}",
                    &matched[1]
                ))
            })?;
        }
        match matched.get(2).map(|v| v.as_str()) {
            Some("json") => json_render(value, &mut output, 0)?,
            Some("table") => append(&mut output, &table(value)?)?,
            _ => {
                if matches!(value, Value::Map(_) | Value::List(_)) {
                    return Err(DocError::new(
                        "Structured documentation values require a json or table filter",
                    ));
                }
                append(&mut output, &scalar_string(value)?)?;
            }
        }
        previous = whole.end();
    }
    append(&mut output, &template[previous..])?;
    if output.contains("{{") || output.contains("}}") {
        return Err(DocError::new(
            "Malformed documentation placeholder or oversized output",
        ));
    }
    Ok(output)
}

fn synchronize_inner(root: &Path, write: bool, signals: &Signals) -> DocResult<Vec<String>> {
    signals.check()?;
    let root = root.canonicalize()?;
    let manifest = util::parse_json(&read(&local_path(
        &root,
        &Json::String(MANIFEST.into()),
        false,
    )?)?)?;
    let document = manifest
        .as_object()
        .filter(|m| {
            exact_fields(m, &["version", "sources", "templates"])
                && m["version"].as_u64() == Some(1)
        })
        .ok_or_else(|| {
            DocError::new("Dependency documentation requires version 1, sources and templates")
        })?;
    let sources = document["sources"]
        .as_object()
        .filter(|sources| !sources.is_empty() && sources.len() <= 32)
        .ok_or_else(|| DocError::new("Documentation requires 1–32 named sources"))?;
    let templates = document["templates"]
        .as_array()
        .filter(|templates| !templates.is_empty() && templates.len() <= 32)
        .ok_or_else(|| DocError::new("Documentation requires 1–32 templates"))?;
    let mut values = Vec::with_capacity(sources.len());
    let mut inputs = HashSet::new();
    inputs.insert(root.join(MANIFEST));
    for (name, spec) in sources {
        signals.check()?;
        values.push((Value::String(name.clone()), source(&root, spec, signals)?));
        if let Some(path) = spec.get("path") {
            inputs.insert(local_path(&root, path, false)?);
        }
    }
    let values = Value::Map(values);
    let mut targets = HashSet::new();
    let mut rendered = Vec::with_capacity(templates.len());
    for spec in templates {
        signals.check()?;
        let spec = spec
            .as_object()
            .filter(|spec| exact_fields(spec, &["source", "target"]))
            .ok_or_else(|| DocError::new("Templates require only source and target"))?;
        let template = local_path(&root, &spec["source"], false)?;
        let target = local_path(&root, &spec["target"], true)?;
        if !targets.insert(target.clone()) || target == template {
            return Err(DocError::new(
                "Duplicate or self-overwriting documentation target",
            ));
        }
        inputs.insert(template.clone());
        rendered.push((target, render(&read(&template)?, &values)?));
    }
    if !targets.is_disjoint(&inputs) {
        return Err(DocError::new(
            "Documentation targets must not overwrite source inputs",
        ));
    }
    let mut changed = Vec::new();
    for (target, content) in &rendered {
        signals.check()?;
        if !target.exists() || read(target)? != *content {
            changed.push(target.clone());
        }
    }
    if write {
        // Validation and rendering of every template has completed before any mutation.
        for (target, content) in &rendered {
            signals.check()?;
            if changed.contains(target) {
                let relative = target
                    .strip_prefix(&root)
                    .expect("validated relative target");
                local_path(
                    &root,
                    &Json::String(relative.to_string_lossy().into_owned()),
                    true,
                )?;
                util::atomic_write(target, content)?;
            }
        }
    }
    Ok(changed
        .into_iter()
        .map(|path| {
            path.strip_prefix(&root)
                .expect("validated target")
                .to_string_lossy()
                .into_owned()
        })
        .collect())
}

/// Render all declared templates before optionally replacing changed Markdown files.
pub fn synchronize(root: &Path, write: bool, signals: &Signals) -> Result<Vec<String>> {
    synchronize_inner(root, write, signals).map_err(|error| Error {
        message: error.message,
        interrupted: error.interrupted,
    })
}

pub fn main(args: impl Iterator<Item = String>) -> i32 {
    let mut write = false;
    for argument in args {
        match argument.as_str() {
            "-h" | "--help" => {
                println!(
                    "usage: project-docs [-h] [--write]\n\nRender dependency documentation from explicit, source-backed templates.\n\noptions:\n  -h, --help  show this help message and exit\n  --write     Update only declared Markdown targets"
                );
                return 0;
            }
            "--write" => write = true,
            _ => {
                eprintln!(
                    "usage: project-docs [-h] [--write]\nproject-docs: error: unrecognized arguments: {argument}"
                );
                return 2;
            }
        }
    }
    let signals = match Signals::install() {
        Ok(signals) => signals,
        Err(_) => {
            println!("project-docs: invalid manifest, source or template");
            return 1;
        }
    };
    let result = std::env::current_dir()
        .map_err(DocError::from)
        .and_then(|root| synchronize_inner(&root, write, &signals));
    if let Some(signal) = signals.received() {
        return 128 + signal;
    }
    match result {
        Ok(changed) if changed.is_empty() => {
            println!("Dependency documentation is current");
            0
        }
        Ok(changed) => {
            println!(
                "{}{}",
                if write {
                    "Updated: "
                } else {
                    "Stale documentation: "
                },
                changed.join(", ")
            );
            if write { 0 } else { 1 }
        }
        Err(error) => {
            if let Some(signal) = error.interrupted {
                return 128 + signal;
            }
            println!(
                "project-docs: {}{}",
                if error.blocked { "blocked: " } else { "" },
                error.message
            );
            if error.blocked { 127 } else { 1 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture {
        directory: tempfile::TempDir,
        manifest: Json,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            fs::write(
                directory.path().join("package.json"),
                r#"{"dependencies":{"example":"2.4.0"}}"#,
            )
            .unwrap();
            fs::write(
                directory.path().join("README.md.in"),
                "Version: {{package.dependencies.example}}\n",
            )
            .unwrap();
            let fixture = Self {
                directory,
                manifest: json!({
                    "version": 1,
                    "sources": {"package": {"kind": "json", "path": "package.json"}},
                    "templates": [{"source": "README.md.in", "target": "README.md"}]
                }),
            };
            fixture.save();
            fixture
        }
        fn root(&self) -> &Path {
            self.directory.path()
        }
        fn save(&self) {
            fs::write(
                self.root().join(MANIFEST),
                serde_json::to_vec(&self.manifest).unwrap(),
            )
            .unwrap();
        }
        fn run(&self, write: bool) -> DocResult<Vec<String>> {
            synchronize_inner(self.root(), write, &Signals::none())
        }
        fn output(&self) -> String {
            fs::read_to_string(self.root().join("README.md")).unwrap()
        }
    }
    fn values(document: Json) -> Value {
        Value::from_json(document)
    }
    fn write_shell_script(path: &Path, body: &str) {
        static SHELL: LazyLock<PathBuf> = LazyLock::new(|| {
            std::env::split_paths(&std::env::var_os("PATH").expect("test PATH"))
                .map(|directory| directory.join("sh"))
                .find(|path| {
                    fs::metadata(path)
                        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                })
                .expect("sh available on test PATH")
                .canonicalize()
                .unwrap()
        });
        fs::write(path, format!("#!{}\n{body}", SHELL.display())).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn drift_is_read_only_and_changes_follow_sources() {
        let fixture = Fixture::new();
        assert_eq!(fixture.run(false).unwrap(), ["README.md"]);
        assert!(!fixture.root().join("README.md").exists());
        assert_eq!(fixture.run(true).unwrap(), ["README.md"]);
        assert_eq!(fixture.output(), "Version: 2.4.0\n");
        assert!(fixture.run(false).unwrap().is_empty());
        fs::write(
            fixture.root().join("package.json"),
            r#"{"dependencies":{"example":"3.0.0"}}"#,
        )
        .unwrap();
        fixture.run(true).unwrap();
        assert_eq!(fixture.output(), "Version: 3.0.0\n");
    }

    #[test]
    fn python_sources_are_parsed_without_running_any_statement() {
        let fixture = Fixture::new();
        let source_text = format!(
            "raise RuntimeError('never execute')\nopen({:?}, 'w').write('unsafe')\nDEFAULTS = {{'model': 'example/v2'}}\n",
            fixture.root().join("executed").to_string_lossy()
        );
        fs::write(fixture.root().join("app.py"), source_text).unwrap();
        let source = source(
            fixture.root(),
            &json!({"kind":"python-literal","path":"app.py","symbol":"DEFAULTS"}),
            &Signals::none(),
        )
        .unwrap();
        let values = Value::Map(vec![(Value::String("defaults".into()), source)]);
        assert_eq!(render("{{defaults.model}}", &values).unwrap(), "example/v2");
        assert!(!fixture.root().join("executed").exists());
    }

    #[test]
    fn literal_assignments_types_and_numeric_signs_are_preserved() {
        let literal = python_literal::parse(
            "OTHER = DEFAULTS = {'negative': -42, 'positive': +2.5, 'truth': True, 'false': False, 'none': None, 'items': [1, -2], 'tuple': ('a',), 'huge': 123456789012345678901234567890, 'complex': 1-2j, 'bytes': b'a\\xff', 'ellipsis': ..., 'empty': set()}\n",
            "DEFAULTS",
        ).unwrap();
        let values = Value::Map(vec![(Value::String("v".into()), literal)]);
        assert_eq!(
            render(
                "{{v.negative}} {{v.positive}} {{v.truth}} {{v.false}} {{v.none}}",
                &values
            )
            .unwrap(),
            "-42 2.5 True False None"
        );
        assert_eq!(
            render(
                "{{v.tuple}} {{v.huge}} {{v.complex}} {{v.bytes}} {{v.ellipsis}} {{v.empty}}",
                &values
            )
            .unwrap(),
            "('a',) 123456789012345678901234567890 (1-2j) b'a\\xff' Ellipsis set()"
        );
        assert_eq!(
            render("{{v.items | json}}", &values).unwrap(),
            "[\n  1,\n  -2\n]"
        );
        assert_eq!(
            render("{{v.tuple | json}}", &values).unwrap(),
            "[\n  \"a\"\n]"
        );
        assert!(render("{{v.bytes | json}}", &values).is_err());
        assert!(render("{{v.complex | json}}", &values).is_err());
        assert!(render("{{v.empty | json}}", &values).is_err());
    }

    #[test]
    fn unsupported_python_expressions_are_never_evaluated() {
        for expression in [
            "run()",
            "__import__('os')",
            "[x for x in []]",
            "object.value",
            "f'{run()}'",
            "1 + 2",
            "--1",
            "~1",
            "not True",
            "{**OTHER}",
            "{[]: 1}",
            "{[1]}",
            "set([1])",
            "1 + -2j",
        ] {
            assert!(
                python_literal::parse(&format!("DEFAULTS = {expression}\n"), "DEFAULTS").is_err(),
                "{expression}"
            );
        }
        assert!(python_literal::parse("DEFAULTS: str = 'annotation'\n", "DEFAULTS").is_err());
        assert!(python_literal::parse("OTHER = 1\n", "DEFAULTS").is_err());
        assert!(python_literal::parse("DEFAULTS = 1\nsyntax error ???", "DEFAULTS").is_err());
        // The first matching top-level assignment is the source of truth.
        assert_eq!(
            python_literal::parse("DEFAULTS = 1\nDEFAULTS = run()\n", "DEFAULTS").unwrap(),
            Value::Integer("1".into())
        );
    }

    #[test]
    fn literal_dictionary_duplicates_follow_python_key_equality() {
        let value = python_literal::parse(
            "DEFAULTS = {True: 'old', 1: 'new', 'x': 2, 'x': 3}\n",
            "DEFAULTS",
        )
        .unwrap();
        assert_eq!(
            value,
            Value::Map(vec![
                (Value::Bool(true), Value::String("new".into())),
                (Value::String("x".into()), Value::Integer("3".into())),
            ])
        );
        let mut rendered = String::new();
        json_render(&value, &mut rendered, 0).unwrap();
        assert_eq!(rendered, "{\n  \"true\": \"new\",\n  \"x\": 3\n}");
    }

    #[test]
    fn toml_sources_and_json_filter_preserve_unicode_and_order() {
        let fixture = Fixture::new();
        fs::write(
            fixture.root().join("Cargo.toml"),
            "[package]\nversion = \"1.2.3\"\nname = \"é\"\n",
        )
        .unwrap();
        let source = source(
            fixture.root(),
            &json!({"kind":"toml","path":"Cargo.toml"}),
            &Signals::none(),
        )
        .unwrap();
        let values = Value::Map(vec![(Value::String("cargo".into()), source)]);
        assert_eq!(
            render("{{cargo.package.version}}", &values).unwrap(),
            "1.2.3"
        );
        assert_eq!(
            render("{{cargo | json}}", &values).unwrap(),
            "{\n  \"package\": {\n    \"version\": \"1.2.3\",\n    \"name\": \"é\"\n  }\n}"
        );
    }

    #[test]
    fn toml_dates_are_plain_scalars_but_not_json_serializable() {
        let value = Value::from_toml(toml::from_str("date = 1979-05-27T07:32:00.123Z\n").unwrap());
        assert_eq!(
            render("{{date}}", &value).unwrap(),
            "1979-05-27 07:32:00.123000+00:00"
        );
        assert!(render("{{date | json}}", &value).is_err());
    }

    #[test]
    fn tables_sort_escape_cells_and_use_python_scalar_spelling() {
        let value = values(json!({"pins":{"z":"2|3\n4","a":true,"b":null,"c":false,"d":-1}}));
        assert_eq!(
            render("{{pins | table}}", &value).unwrap(),
            "| Dependency | Pinned version |\n| --- | --- |\n| a | True |\n| b | None |\n| c | False |\n| d | -1 |\n| z | 2\\|3 4 |"
        );
        for value in [
            json!({"pins":[1]}),
            json!({"pins":{"nested":{}}}),
            json!({"pins":{"nested":[]}}),
        ] {
            assert!(render("{{pins | table}}", &values(value)).is_err());
        }
    }

    #[test]
    fn unknown_malformed_and_structured_placeholders_fail() {
        let value = values(json!({"v":{"list":[1],"scalar":"{{bad}}"}}));
        for template in [
            "{{missing}}",
            "{{v.absent}}",
            "{{v.list}}",
            "{{v}}",
            "{{v | yaml}}",
            "{{v",
            "}}",
            "{{v.scalar}}",
        ] {
            assert!(render(template, &value).is_err(), "{template}");
        }
        assert_eq!(render("literal text", &value).unwrap(), "literal text");
    }

    #[test]
    fn all_templates_are_rendered_before_the_first_write() {
        let mut fixture = Fixture::new();
        fs::write(fixture.root().join("bad.md.in"), "{{missing.version}}").unwrap();
        fixture.manifest["templates"]
            .as_array_mut()
            .unwrap()
            .push(json!({"source":"bad.md.in","target":"other.md"}));
        fixture.save();
        assert!(fixture.run(true).is_err());
        assert!(!fixture.root().join("README.md").exists());
        assert!(!fixture.root().join("other.md").exists());
    }

    #[test]
    fn paths_reject_escapes_runtime_state_nonregular_files_and_symlinks() {
        let fixture = Fixture::new();
        for name in [
            "",
            "../outside.md",
            "/tmp/outside.md",
            "bad\0name",
            ".git/config",
            ".direnv/state.json",
            ".venv/x",
            "node_modules/x",
            "__pycache__/x",
        ] {
            assert!(
                local_path(fixture.root(), &json!(name), false).is_err(),
                "{name:?}"
            );
        }
        assert!(local_path(fixture.root(), &Json::Null, false).is_err());
        fs::create_dir(fixture.root().join("directory.md")).unwrap();
        assert!(local_path(fixture.root(), &json!("directory.md"), true).is_err());
        assert!(local_path(fixture.root(), &json!("missing/directory.md"), true).is_err());
        assert!(local_path(fixture.root(), &json!("target.txt"), true).is_err());
        symlink("package.json", fixture.root().join("link.json")).unwrap();
        symlink("missing.json", fixture.root().join("dangling.json")).unwrap();
        symlink(fixture.root(), fixture.root().join("linkdir")).unwrap();
        for name in [
            "link.json",
            "dangling.json",
            "linkdir/package.json",
            "linkdir/new.md",
        ] {
            assert!(
                local_path(fixture.root(), &json!(name), false).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn outputs_cannot_overwrite_sources_templates_or_other_outputs() {
        for target in [
            "../outside.md",
            "/tmp/outside.md",
            "package.json",
            "README.md.in",
        ] {
            let mut fixture = Fixture::new();
            fixture.manifest["templates"][0]["target"] = json!(target);
            fixture.save();
            assert!(fixture.run(true).is_err(), "{target}");
        }
        for second in [
            json!({"source":"README.md.in","target":"README.md"}),
            json!({"source":"README.md","target":"other.md"}),
        ] {
            let mut fixture = Fixture::new();
            fs::write(fixture.root().join("README.md"), "old").unwrap();
            fixture.manifest["templates"]
                .as_array_mut()
                .unwrap()
                .push(second);
            fixture.save();
            assert!(fixture.run(true).is_err());
            assert_eq!(fixture.output(), "old");
        }
        let mut fixture = Fixture::new();
        fs::write(
            fixture.root().join("source.md"),
            "{{package.dependencies.example}}",
        )
        .unwrap();
        fixture.manifest["sources"]["source"] =
            json!({"kind":"python-literal","path":"source.md","symbol":"X"});
        fs::write(fixture.root().join("source.md"), "X = 'source'\n").unwrap();
        fixture.manifest["templates"][0]["target"] = json!("source.md");
        fixture.save();
        assert!(fixture.run(true).is_err());
        assert_eq!(
            fs::read_to_string(fixture.root().join("source.md")).unwrap(),
            "X = 'source'\n"
        );
        fixture.manifest["sources"]
            .as_object_mut()
            .unwrap()
            .remove("source");
        fixture.manifest["templates"][0] = json!({"source":"source.md","target":"./source.md"});
        fixture.save();
        assert!(fixture.run(true).is_err());
    }

    #[test]
    fn symlink_outputs_are_rejected_and_existing_modes_are_preserved() {
        let fixture = Fixture::new();
        symlink("package.json", fixture.root().join("README.md")).unwrap();
        assert!(fixture.run(true).is_err());
        fs::remove_file(fixture.root().join("README.md")).unwrap();
        fixture.run(true).unwrap();
        assert_eq!(
            fs::metadata(fixture.root().join("README.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        fs::set_permissions(
            fixture.root().join("README.md"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        fs::write(fixture.root().join("README.md"), "stale").unwrap();
        fixture.run(true).unwrap();
        assert_eq!(fixture.output(), "Version: 2.4.0\n");
        assert_eq!(
            fs::metadata(fixture.root().join("README.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }

    #[test]
    fn manifest_fields_versions_and_source_fields_are_strict() {
        for manifest in [
            json!([]),
            json!({"version":true,"sources":{},"templates":[]}),
            json!({"version":1.0,"sources":{},"templates":[]}),
            json!({"version":2,"sources":{},"templates":[]}),
            json!({"version":1,"sources":{},"templates":[],"extra":true}),
        ] {
            let mut fixture = Fixture::new();
            fixture.manifest = manifest;
            fixture.save();
            assert!(fixture.run(false).is_err());
        }
        let fixture = Fixture::new();
        for spec in [
            json!(null),
            json!({"kind":"json"}),
            json!({"kind":"json","path":"package.json","extra":1}),
            json!({"kind":"python-literal","path":"package.json"}),
            json!({"kind":"nix","attribute":"lib.x","path":"package.json"}),
            json!({"kind":"unknown","path":"package.json"}),
        ] {
            assert!(source(fixture.root(), &spec, &Signals::none()).is_err());
        }
        fs::write(
            fixture.root().join(MANIFEST),
            r#"{"version":1,"version":1,"sources":{},"templates":[]}"#,
        )
        .unwrap();
        assert!(fixture.run(false).is_err());
    }

    #[test]
    fn source_template_and_output_bounds_are_enforced_before_writing() {
        let mut fixture = Fixture::new();
        fs::write(fixture.root().join("README.md.in"), "x".repeat(LIMIT + 1)).unwrap();
        assert!(fixture.run(true).unwrap_err().message.contains("1 MB"));
        assert!(!fixture.root().join("README.md").exists());
        fs::write(fixture.root().join("README.md.in"), "x".repeat(LIMIT)).unwrap();
        fixture.run(true).unwrap();
        assert_eq!(fixture.output().len(), LIMIT);
        let value = values(json!({"v":"é".repeat(LIMIT / 2)}));
        assert_eq!(render("{{v}}", &value).unwrap().len(), LIMIT);
        assert!(render("{{v}}x", &value).is_err());
        assert!(render("{{v}}{{v}}", &value).is_err());
        assert!(render("{{v | json}}", &value).is_err());
        for count in [0, 33] {
            fixture.manifest["templates"] = Json::Array(
                (0..count)
                    .map(|i| json!({"source":"README.md.in","target":format!("file{i}.md")}))
                    .collect(),
            );
            fixture.save();
            assert!(fixture.run(true).is_err());
        }
        fixture.manifest["templates"] = json!([{"source":"README.md.in","target":"README.md"}]);
        for count in [0, 33] {
            fixture.manifest["sources"] = Json::Object(
                (0..count)
                    .map(|i| {
                        (
                            format!("p{i}"),
                            json!({"kind":"json","path":"package.json"}),
                        )
                    })
                    .collect(),
            );
            fixture.save();
            assert!(fixture.run(true).is_err());
        }
    }

    #[test]
    fn missing_nix_and_bad_attributes_are_classified_without_environment_mutation() {
        let fixture = Fixture::new();
        let missing = fixture.root().join("missing-nix");
        let error = nix_source_program(
            fixture.root(),
            &json!("lib.versions"),
            &Signals::none(),
            missing.to_str().unwrap(),
        )
        .unwrap_err();
        assert!(error.blocked);
        assert_eq!(
            error.message,
            "Required program is missing: nix; enter the pinned toolchain"
        );
        for attribute in [json!(""), json!("lib x"), json!("x;touch"), json!(null)] {
            let error = nix_source_program(
                fixture.root(),
                &attribute,
                &Signals::none(),
                missing.to_str().unwrap(),
            )
            .unwrap_err();
            assert!(!error.blocked);
            assert_eq!(error.message, "Invalid Nix documentation attribute");
        }
    }

    #[test]
    fn nix_uses_offline_argv_and_separate_bounded_diagnostics() {
        let fixture = Fixture::new();
        let fake = fixture.root().join("fake-nix");
        let script = "[ \"$#\" = 5 ] && [ \"$1\" = eval ] && [ \"$2\" = --offline ] && [ \"$3\" = --no-write-lock-file ] && [ \"$4\" = --json ] && [ \"$5\" = 'path:.#lib.versions' ] || exit 9\nprintf '{\"pin\":\"2.4.0\"}'\nprintf 'diagnostic only' >&2\n";
        write_shell_script(&fake, script);
        let value = nix_source_program(
            fixture.root(),
            &json!("lib.versions"),
            &Signals::none(),
            fake.to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(value.get("pin"), Some(&Value::String("2.4.0".into())));
        write_shell_script(&fake, "printf 'cannot fetch offline' >&2\nexit 1\n");
        let error = nix_source_program(
            fixture.root(),
            &json!("lib.versions"),
            &Signals::none(),
            fake.to_str().unwrap(),
        )
        .unwrap_err();
        assert!(error.blocked);
        assert_eq!(
            error.message,
            "Offline Nix documentation inputs are unavailable; enter the pinned toolchain"
        );
        write_shell_script(&fake, "printf 'unknown attribute' >&2\nexit 1\n");
        let error = nix_source_program(
            fixture.root(),
            &json!("lib.versions"),
            &Signals::none(),
            fake.to_str().unwrap(),
        )
        .unwrap_err();
        assert!(!error.blocked);
        assert_eq!(
            error.message,
            "Nix documentation evaluation failed; check the declared attribute"
        );
    }

    #[test]
    fn huge_json_integers_and_float_scalars_keep_python_formatting() {
        let value = Value::from_json(util::parse_json(
            r#"{"huge":123456789012345678901234567890,"negative":-0,"small":1e-7,"large":1e20,"zero":-0.0,"overflow":1e999}"#
        ).unwrap());
        assert_eq!(
            render(
                "{{huge}} {{negative}} {{small}} {{large}} {{zero}} {{overflow}}",
                &value
            )
            .unwrap(),
            "123456789012345678901234567890 0 1e-07 1e+20 -0.0 inf"
        );
        assert_eq!(render("{{overflow | json}}", &value).unwrap(), "Infinity");
    }

    #[test]
    fn literal_table_keys_sort_without_losing_integer_precision() {
        let value = python_literal::parse(
            "X = {9007199254740993: 'b', 9007199254740992: 'a', -5: 'n'}",
            "X",
        )
        .unwrap();
        assert_eq!(
            table(&value).unwrap(),
            "| Dependency | Pinned version |\n| --- | --- |\n| -5 | n |\n| 9007199254740992 | a |\n| 9007199254740993 | b |"
        );
        let value = python_literal::parse("X = {(2,): 'b', (1,): 'a'}", "X").unwrap();
        assert!(
            table(&value)
                .unwrap()
                .contains("| (1,) | a |\n| (2,) | b |")
        );
        let value = python_literal::parse("X = {'a': {1, 2}}", "X").unwrap();
        assert!(table(&value).unwrap_err().message.contains("deterministic"));
        assert!(
            render("{{a}}", &value)
                .unwrap_err()
                .message
                .contains("deterministic")
        );
    }

    #[test]
    fn nix_stdout_and_combined_capture_limits_are_enforced() {
        let fixture = Fixture::new();
        let fake = fixture.root().join("fake-nix");
        write_shell_script(&fake, "printf '%1000001s' ''\n");
        let error = nix_source_program(
            fixture.root(),
            &json!("lib.versions"),
            &Signals::none(),
            fake.to_str().unwrap(),
        )
        .unwrap_err();
        assert!(!error.blocked);
        assert!(error.message.contains("1 MB"));
        write_shell_script(&fake, "printf '%1008193s' '' >&2\n");
        let error = nix_source_program(
            fixture.root(),
            &json!("lib.versions"),
            &Signals::none(),
            fake.to_str().unwrap(),
        )
        .unwrap_err();
        assert!(!error.blocked);
        assert!(error.message.contains("1 MB"));
    }

    #[test]
    fn maximum_source_and_template_counts_are_accepted() {
        let mut fixture = Fixture::new();
        fs::write(fixture.root().join("README.md.in"), "constant\n").unwrap();
        fixture.manifest["sources"] = Json::Object(
            (0..32)
                .map(|i| {
                    (
                        format!("p{i}"),
                        json!({"kind":"json","path":"package.json"}),
                    )
                })
                .collect(),
        );
        fixture.manifest["templates"] = Json::Array(
            (0..32)
                .map(|i| json!({"source":"README.md.in","target":format!("file{i}.md")}))
                .collect(),
        );
        fixture.save();
        assert_eq!(fixture.run(false).unwrap().len(), 32);
        assert!(!fixture.root().join("file0.md").exists());
    }

    #[test]
    fn oversized_sources_manifests_and_existing_targets_fail_without_writes() {
        let fixture = Fixture::new();
        fs::write(fixture.root().join("package.json"), " ".repeat(LIMIT + 1)).unwrap();
        assert!(fixture.run(true).unwrap_err().message.contains("1 MB"));
        assert!(!fixture.root().join("README.md").exists());
        fs::write(
            fixture.root().join("package.json"),
            r#"{"dependencies":{"example":"2.4.0"}}"#,
        )
        .unwrap();
        fs::write(fixture.root().join("README.md"), "x".repeat(LIMIT + 1)).unwrap();
        assert!(fixture.run(true).unwrap_err().message.contains("1 MB"));
        assert_eq!(
            fs::metadata(fixture.root().join("README.md"))
                .unwrap()
                .len(),
            (LIMIT + 1) as u64
        );
        fs::write(fixture.root().join(MANIFEST), " ".repeat(LIMIT + 1)).unwrap();
        assert!(fixture.run(true).unwrap_err().message.contains("1 MB"));
    }

    #[test]
    fn source_syntax_and_encoding_errors_do_not_modify_outputs() {
        let fixture = Fixture::new();
        for contents in [b"{invalid}".as_slice(), &[0xff, 0xfe]] {
            fs::write(fixture.root().join("package.json"), contents).unwrap();
            assert!(fixture.run(true).is_err());
            assert!(!fixture.root().join("README.md").exists());
        }
    }

    #[test]
    fn python_lexer_bounds_precede_recursive_ast_parsing() {
        let excessive_parentheses = format!("X = {}1{}\n", "(".repeat(129), ")".repeat(129));
        assert!(
            python_literal::parse(&excessive_parentheses, "X")
                .unwrap_err()
                .message
                .contains("delimiter nesting")
        );
        for expression in [
            format!("{}1", "-".repeat(400)),
            format!("{}True", "not ".repeat(400)),
            format!("{}0", "1 + ".repeat(300)),
            format!("{}0", "f(1, 2) + ".repeat(300)),
        ] {
            let error = python_literal::parse(&format!("X = {expression}\n"), "X").unwrap_err();
            assert!(
                error.message.contains("expression exceeds"),
                "{}",
                error.message
            );
        }
        let large_statement = format!("X = [{}]\n", "1,".repeat(600));
        assert!(
            python_literal::parse(&large_statement, "X")
                .unwrap_err()
                .message
                .contains("statement exceeds")
        );
        let huge_number = format!("X = {}\n", "9".repeat(4097));
        assert!(
            python_literal::parse(&huge_number, "X")
                .unwrap_err()
                .message
                .contains("numeric token")
        );
    }

    #[test]
    fn python_lexer_ignores_punctuation_in_ordinary_strings_and_comments() {
        let text = format!("{}{}", "([{".repeat(3000), "-+*/".repeat(3000));
        let source = format!("# {}\nX = {text:?}\n", "(".repeat(1000));
        assert_eq!(
            python_literal::parse(&source, "X").unwrap(),
            Value::String(text)
        );
        let boundary = format!("X = {}1{}\n", "(".repeat(128), ")".repeat(128));
        assert_eq!(
            python_literal::parse(&boundary, "X").unwrap(),
            Value::Integer("1".into())
        );
    }

    #[test]
    fn ordinary_unrelated_fstrings_remain_valid_but_hidden_complexity_is_bounded() {
        let source = "def label(name):\n    return f'hello {name}'\nX = {'pin': '2.4.0'}\n";
        assert_eq!(
            python_literal::parse(source, "X").unwrap().get("pin"),
            Some(&Value::String("2.4.0".into()))
        );
        for body in [
            "[".repeat(129),
            "a".repeat(4097),
            "word ".repeat(129),
            format!("{{{}True}}", "not ".repeat(129)),
        ] {
            let source = format!("UNRELATED = f'{body}'\nX = 1\n");
            assert!(
                python_literal::parse(&source, "X")
                    .unwrap_err()
                    .message
                    .contains("f-string exceeds")
            );
        }
        assert!(python_literal::parse("X = f'{1}'\n", "X").is_err());
    }

    #[test]
    fn python_repr_escapes_nonprintable_unicode_but_preserves_printable_text() {
        assert_eq!(quoted("é漢字🙂 normal"), "'é漢字🙂 normal'");
        let nonprintable = "\u{a0}\u{85}\u{2028}\u{200b}\u{e000}\u{378}\u{f0000}";
        assert_eq!(
            quoted(nonprintable),
            "'\\xa0\\x85\\u2028\\u200b\\ue000\\u0378\\U000f0000'"
        );
        assert_eq!(quoted(" \t\n\r"), "' \\t\\n\\r'");
        let value = Value::Map(vec![
            (
                Value::String("tuple".into()),
                Value::Tuple(vec![Value::String(nonprintable.into())]),
            ),
            (
                Value::String("plain".into()),
                Value::String(nonprintable.into()),
            ),
        ]);
        assert_eq!(
            render("{{tuple}}", &value).unwrap(),
            "('\\xa0\\x85\\u2028\\u200b\\ue000\\u0378\\U000f0000',)"
        );
        assert_eq!(render("{{plain}}", &value).unwrap(), nonprintable);
    }

    #[test]
    fn mixed_table_keys_use_a_total_sort_order_and_report_incomparability() {
        for count in [3, 128, 1024] {
            let keys = Value::Map(
                (0..count)
                    .map(|i| {
                        let key = if i % 3 == 1 {
                            Value::String(format!("s{i}"))
                        } else {
                            Value::Integer(i.to_string())
                        };
                        (key, Value::String("pin".into()))
                    })
                    .collect(),
            );
            assert!(table(&keys).unwrap_err().message.contains("comparable"));
        }
        let nan_keys = Value::Map(vec![
            (Value::Float(f64::NAN), Value::Null),
            (Value::Integer("1".into()), Value::Null),
            (Value::Integer("2".into()), Value::Null),
        ]);
        assert!(table(&nan_keys).is_err());
        let tuple_keys =
            python_literal::parse("X = {(None, 2): 'b', (None, 1): 'a'}", "X").unwrap();
        assert!(
            table(&tuple_keys)
                .unwrap()
                .contains("| (None, 1) | a |\n| (None, 2) | b |")
        );
        let keys = [
            Value::Integer("1".into()),
            Value::Integer("2".into()),
            Value::String("x".into()),
            Value::Tuple(vec![Value::Integer("1".into()), Value::Null]),
            Value::Tuple(vec![Value::Complex(1.0, 0.0), Value::Integer("2".into())]),
            Value::Complex(-0.0, 1.0),
            Value::Complex(0.0, 1.0),
            Value::Complex(-0.0, 2.0),
        ];
        for a in &keys {
            for b in &keys {
                assert_eq!(total_compare_keys(a, b), total_compare_keys(b, a).reverse());
                if let Some(order) = compare_keys(a, b) {
                    assert_eq!(total_compare_keys(a, b), order);
                }
                for c in &keys {
                    if total_compare_keys(a, b).is_le() && total_compare_keys(b, c).is_le() {
                        assert!(total_compare_keys(a, c).is_le());
                    }
                }
            }
        }
    }

    #[test]
    fn toml_source_guards_combined_depth_before_value_deserialization() {
        let fixture = Fixture::new();
        let key = std::iter::repeat_n("a", 80).collect::<Vec<_>>().join(".");
        let nested = format!(
            "X = {}1{}\n",
            format!("{{ {key} = ").repeat(79),
            "}".repeat(79)
        );
        fs::write(fixture.root().join("pins.toml"), nested).unwrap();
        let error = source(
            fixture.root(),
            &json!({"kind":"toml","path":"pins.toml"}),
            &Signals::none(),
        )
        .unwrap_err();
        assert!(error.message.contains("combined depth"));
        assert!(!fixture.root().join("README.md").exists());
    }
}
