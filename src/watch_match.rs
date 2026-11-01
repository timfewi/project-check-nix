//! fnmatch-style globs, not pathname globs: `*` crosses separators, repeated
//! stars are ordinary stars, and an unmatched `[` is a literal character.
use crate::{Error, Result};
use regex::Regex;

pub(super) fn compile(pattern: &str) -> Result<Regex> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut output = String::from(r"\A(?s:");
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '*' => {
                output.push_str(".*");
                while index + 1 < chars.len() && chars[index + 1] == '*' {
                    index += 1;
                }
            }
            '?' => output.push('.'),
            '[' => {
                let start = index + 1;
                let mut end = start;
                if chars.get(end) == Some(&'!') {
                    end += 1;
                }
                if chars.get(end) == Some(&']') {
                    end += 1;
                }
                while end < chars.len() && chars[end] != ']' {
                    end += 1;
                }
                if end == chars.len() {
                    output.push_str(r"\[");
                } else {
                    let mut class = &chars[start..end];
                    let negated = class.first() == Some(&'!');
                    if negated {
                        class = &class[1..];
                    }
                    let mut body = String::new();
                    let mut position = 0;
                    while position < class.len() {
                        let first = class[position];
                        if position + 2 < class.len() && class[position + 1] == '-' {
                            let last = class[position + 2];
                            // Python fnmatch removes invalid descending ranges.
                            if first <= last {
                                body.push_str(&format!(
                                    r"\x{{{:x}}}-\x{{{:x}}}",
                                    first as u32, last as u32
                                ));
                            }
                            position += 3;
                        } else {
                            body.push_str(&format!(r"\x{{{:x}}}", first as u32));
                            position += 1;
                        }
                    }
                    if body.is_empty() {
                        output.push_str(if negated { "." } else { r"[^\s\S]" });
                    } else {
                        output.push('[');
                        if negated {
                            output.push('^');
                        }
                        output.push_str(&body);
                        output.push(']');
                    }
                    index = end;
                }
            }
            character => output.push_str(&regex::escape(&character.to_string())),
        }
        index += 1;
    }
    output.push_str(r")\z");
    Regex::new(&output).map_err(|_| Error::new("watch_ignore pattern exceeds matcher limits"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fnmatch_oracle_cases() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fnmatch_cases.json")).unwrap();
        for case in cases["cases"].as_array().unwrap() {
            let pattern = case[0].as_str().unwrap();
            let path = case[1].as_str().unwrap();
            let expected = case[2].as_bool().unwrap();
            assert_eq!(
                compile(pattern).unwrap().is_match(path),
                expected,
                "pattern {pattern:?}, path {path:?}"
            );
        }
    }
}
