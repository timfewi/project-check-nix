//! Bound the logical TOML tree before deserialization constructs recursive maps.
use super::{DocError, DocResult};
use toml_parser::{Source, lexer::TokenKind};

const MAX_DEPTH: usize = 128;

pub(super) fn validate(text: &str) -> DocResult<()> {
    // A dotted key adds a map at every component. A header may add an array
    // and table at every component. Inline containers can repeat dotted keys.
    // Global maxima deliberately overestimate even unrelated branches.
    let mut max_header = 0_usize;
    let mut max_key = 1_usize;
    let mut max_containers = 0_usize;
    let mut containers = Vec::new();
    let mut statement_start = true;
    let mut header_brackets = 0_usize;
    let mut header_components = 1_usize;
    let mut in_key = true;
    let mut key_components = 1_usize;
    for token in Source::new(text).lex() {
        let kind = token.kind();
        if matches!(
            kind,
            TokenKind::Whitespace | TokenKind::Comment | TokenKind::Eof
        ) {
            continue;
        }
        if header_brackets > 0 {
            match kind {
                TokenKind::LeftSquareBracket => header_brackets += 1,
                TokenKind::RightSquareBracket => header_brackets -= 1,
                TokenKind::Dot => header_components += 1,
                _ => (),
            }
            max_header = max_header.max(header_components);
        } else {
            match kind {
                TokenKind::Newline if containers.is_empty() => {
                    statement_start = true;
                    in_key = true;
                    key_components = 1;
                }
                TokenKind::LeftSquareBracket if statement_start && containers.is_empty() => {
                    header_brackets = 1;
                    header_components = 1;
                    max_header = max_header.max(1);
                    statement_start = false;
                    in_key = false;
                }
                TokenKind::Dot if in_key => {
                    key_components += 1;
                    max_key = max_key.max(key_components);
                }
                TokenKind::Equals => {
                    in_key = false;
                    statement_start = false;
                }
                TokenKind::LeftCurlyBracket | TokenKind::LeftSquareBracket => {
                    containers.push(kind);
                    max_containers = max_containers.max(containers.len());
                    in_key = kind == TokenKind::LeftCurlyBracket;
                    key_components = 1;
                    statement_start = false;
                }
                TokenKind::RightCurlyBracket | TokenKind::RightSquareBracket => {
                    let expected = if kind == TokenKind::RightCurlyBracket {
                        TokenKind::LeftCurlyBracket
                    } else {
                        TokenKind::LeftSquareBracket
                    };
                    if containers.pop() != Some(expected) {
                        return Err(DocError::new("invalid manifest, source or template"));
                    }
                    in_key = false;
                }
                TokenKind::Comma => {
                    in_key = containers.last() == Some(&TokenKind::LeftCurlyBracket);
                    key_components = 1;
                }
                TokenKind::Newline => (),
                _ => statement_start = false,
            }
        }
        let bound = 1_usize
            .saturating_add(2 * max_header)
            .saturating_add(max_key.saturating_mul(max_containers + 1))
            .saturating_add(max_containers);
        if bound > MAX_DEPTH {
            return Err(DocError::new(
                "TOML documentation combined depth limit: estimate exceeds 128 levels",
            ));
        }
    }
    if !containers.is_empty() || header_brackets != 0 {
        return Err(DocError::new("invalid manifest, source or template"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_dotted_inline_keys_are_rejected_before_deserialization() {
        let key = std::iter::repeat_n("a", 80).collect::<Vec<_>>().join(".");
        let mut source = format!("root = {}", format!("{{{key} = ").repeat(79));
        source.push('1');
        source.push_str(&"}".repeat(79));
        assert!(
            validate(&source)
                .unwrap_err()
                .message
                .contains("combined depth")
        );
    }

    #[test]
    fn header_array_and_inline_depths_are_combined() {
        let header = std::iter::repeat_n("a", 30).collect::<Vec<_>>().join(".");
        let key = std::iter::repeat_n("b", 20).collect::<Vec<_>>().join(".");
        let source = format!("[[{header}]]\n{key} = [{{ x = 1 }}]\n");
        assert!(validate(&source).is_ok());
        let source = format!("[[{header}]]\n{key} = [[{{ x = 1 }}]]\n");
        assert!(
            validate(&source)
                .unwrap_err()
                .message
                .contains("combined depth")
        );
    }

    #[test]
    fn strings_quoted_keys_comments_and_numbers_do_not_add_fake_depth() {
        let punctuation = ".[]{}".repeat(1000);
        let source = format!(
            "# {punctuation}\n\"a.b\" = '{punctuation}'\n[\"c.d\"]\nvalue = 1.234e5\nmultiline = '''\n{punctuation}\n'''\n"
        );
        assert!(validate(&source).is_ok());
        assert!(toml::from_str::<toml::Value>(&source).is_ok());
    }

    #[test]
    fn ordinary_multiline_arrays_and_inline_tables_are_accepted() {
        assert!(validate("[package]\nversions = [\n { a.b = 1 },\n { c = [1, 2] },\n]\n").is_ok());
    }
}
