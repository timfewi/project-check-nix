//! Evaluate only the AST forms accepted by Python's ast.literal_eval.
use super::{DocError, DocResult, Value};
use rustpython_parser::{Mode, Parse, Tok, ast, lexer};
use std::collections::{HashMap, HashSet};

const MAX_NESTING: usize = 128;
const MAX_TOKENS: usize = 100_000;
const MAX_EXPRESSION_TOKENS: usize = 256;
const MAX_STATEMENT_TOKENS: usize = 1024;
const MAX_FSTRING_BYTES: usize = 4096;
const MAX_FSTRING_COMPLEXITY: usize = 128;
const MAX_NUMBER_BYTES: usize = 4096;
const MAX_ELIF_TOKENS: usize = 128;

// Validate the versioned lexer's token stream before constructing any recursive
// AST. Literal evaluation alone is too late to protect parser/AST drop stacks.
fn bounded_tokens(text: &str) -> DocResult<Vec<lexer::LexResult>> {
    struct Scope {
        closing: u8,
        tokens: usize,
    }
    let mut scopes = vec![Scope {
        closing: 0,
        tokens: 0,
    }];
    let mut indentation = 0_usize;
    let mut statement_tokens = 0_usize;
    let mut active_tokens = 0_usize;
    let mut elif_tokens = 0_usize;
    let mut tokens = Vec::new();
    for token in lexer::lex(text, Mode::Module) {
        let (token, range) = token.map_err(|_| invalid())?;
        if tokens.len() == MAX_TOKENS {
            return Err(DocError::new(
                "Python documentation source exceeds 100000 tokens",
            ));
        }
        if matches!(token, Tok::Elif) {
            elif_tokens += 1;
            // A flat elif chain becomes recursively nested StmtIf.orelse nodes,
            // although its indentation and per-statement counters stay small.
            if elif_tokens > MAX_ELIF_TOKENS {
                return Err(DocError::new(
                    "Python documentation limit: source exceeds 128 elif clauses",
                ));
            }
        }
        if matches!(
            token,
            Tok::Int { .. } | Tok::Float { .. } | Tok::Complex { .. }
        ) && usize::from(range.len()) > MAX_NUMBER_BYTES
        {
            return Err(DocError::new(
                "Python documentation numeric token exceeds 4096 bytes",
            ));
        }
        if let Tok::String { kind, value, .. } = &token
            && kind.is_any_fstring()
        {
            // Embedded expressions are not exposed as outer lexer tokens.
            let punctuation = value
                .bytes()
                .filter(|c| b",.:!=<>^|&~%/*-+][)(}{".contains(c))
                .count();
            if value.len() > MAX_FSTRING_BYTES
                || punctuation > MAX_FSTRING_COMPLEXITY
                || value.split_whitespace().count() > MAX_FSTRING_COMPLEXITY
            {
                return Err(DocError::new(
                    "Python documentation f-string exceeds 4096 bytes or 128 punctuation marks/words",
                ));
            }
        }
        if matches!(token, Tok::Newline | Tok::Semi) {
            statement_tokens = 0;
        } else {
            statement_tokens += 1;
            if statement_tokens > MAX_STATEMENT_TOKENS {
                return Err(DocError::new(
                    "Python documentation statement exceeds 1024 tokens",
                ));
            }
        }
        match &token {
            Tok::Indent => {
                indentation += 1;
                if indentation > MAX_NESTING {
                    return Err(DocError::new(
                        "Python documentation indentation exceeds 128 levels",
                    ));
                }
            }
            Tok::Dedent => indentation = indentation.saturating_sub(1),
            Tok::Comma | Tok::Semi | Tok::Newline => {
                let scope = scopes.last_mut().expect("root scope");
                active_tokens -= scope.tokens;
                scope.tokens = 0;
            }
            Tok::Rpar | Tok::Rsqb | Tok::Rbrace => {
                let closing = match &token {
                    Tok::Rpar => b')',
                    Tok::Rsqb => b']',
                    _ => b'}',
                };
                if scopes.len() == 1 {
                    return Err(invalid());
                }
                let scope = scopes.pop().expect("delimiter scope");
                if scope.closing != closing {
                    return Err(invalid());
                }
                active_tokens -= scope.tokens;
            }
            Tok::EndOfFile => (),
            _ => {
                scopes.last_mut().expect("root scope").tokens += 1;
                active_tokens += 1;
                if active_tokens > MAX_EXPRESSION_TOKENS {
                    return Err(DocError::new(
                        "Python documentation expression exceeds 256 active tokens",
                    ));
                }
                let closing = match &token {
                    Tok::Lpar => Some(b')'),
                    Tok::Lsqb => Some(b']'),
                    Tok::Lbrace => Some(b'}'),
                    _ => None,
                };
                if let Some(closing) = closing {
                    if scopes.len() > MAX_NESTING {
                        return Err(DocError::new(
                            "Python documentation delimiter nesting exceeds 128 levels",
                        ));
                    }
                    scopes.push(Scope { closing, tokens: 0 });
                }
            }
        }
        tokens.push(Ok((token, range)));
    }
    if scopes.len() != 1 {
        return Err(invalid());
    }
    Ok(tokens)
}

fn invalid() -> DocError {
    DocError::new("Python documentation symbol is missing or is not a literal")
}

pub(super) fn parse(text: &str, symbol: &str) -> DocResult<Value> {
    let suite = ast::Suite::parse_tokens(bounded_tokens(text)?, "<documentation source>")
        .map_err(|_| invalid())?;
    for statement in suite {
        if let ast::Stmt::Assign(assign) = statement
            && assign
                .targets
                .iter()
                .any(|target| matches!(target, ast::Expr::Name(name) if name.id.as_str() == symbol))
        {
            return literal(&assign.value, 0);
        }
    }
    Err(invalid())
}

fn constant(value: &ast::Constant, depth: usize) -> DocResult<Value> {
    if depth > MAX_NESTING {
        return Err(DocError::new("Documentation literal nesting exceeds limit"));
    }
    Ok(match value {
        ast::Constant::None => Value::Null,
        ast::Constant::Bool(v) => Value::Bool(*v),
        ast::Constant::Str(v) => Value::String(v.clone()),
        ast::Constant::Bytes(v) => Value::Bytes(v.clone()),
        ast::Constant::Int(v) => Value::Integer(v.to_string()),
        ast::Constant::Float(v) => Value::Float(*v),
        ast::Constant::Complex { real, imag } => Value::Complex(*real, *imag),
        ast::Constant::Ellipsis => Value::Ellipsis,
        ast::Constant::Tuple(v) => Value::Tuple(
            v.iter()
                .map(|v| constant(v, depth + 1))
                .collect::<DocResult<_>>()?,
        ),
    })
}

fn numeric(expression: &ast::Expr, signed: bool) -> DocResult<Value> {
    if let ast::Expr::Constant(value) = expression {
        let value = constant(&value.value, 0)?;
        return if matches!(
            value,
            Value::Integer(_) | Value::Float(_) | Value::Complex(_, _)
        ) {
            Ok(value)
        } else {
            Err(invalid())
        };
    }
    if signed
        && let ast::Expr::UnaryOp(unary) = expression
        && matches!(unary.op, ast::UnaryOp::UAdd | ast::UnaryOp::USub)
    {
        let value = numeric(&unary.operand, false)?;
        if unary.op == ast::UnaryOp::UAdd {
            return Ok(value);
        }
        return Ok(match value {
            Value::Integer(value) => Value::Integer(if value == "0" {
                value
            } else if let Some(value) = value.strip_prefix('-') {
                value.to_string()
            } else {
                format!("-{value}")
            }),
            Value::Float(value) => Value::Float(-value),
            Value::Complex(real, imaginary) => Value::Complex(-real, -imaginary),
            _ => return Err(invalid()),
        });
    }
    Err(invalid())
}

#[derive(Hash, PartialEq, Eq)]
enum Key {
    None,
    Integer(String),
    Float(u64),
    Complex(Box<Key>, Box<Key>),
    String(String),
    Bytes(Vec<u8>),
    Tuple(Vec<Key>),
    Ellipsis,
}
fn float_key(value: f64) -> Key {
    if value.is_finite() && value.fract() == 0.0 {
        Key::Integer(if value == 0.0 {
            "0".into()
        } else {
            format!("{value:.0}")
        })
    } else {
        Key::Float(value.to_bits())
    }
}
fn hash_key(value: &Value) -> DocResult<Key> {
    Ok(match value {
        Value::Null => Key::None,
        Value::Bool(value) => Key::Integer(if *value { "1" } else { "0" }.into()),
        Value::Integer(value) => Key::Integer(value.clone()),
        Value::Float(value) => float_key(*value),
        Value::Complex(real, imag) if *imag == 0.0 => float_key(*real),
        Value::Complex(real, imag) => {
            Key::Complex(Box::new(float_key(*real)), Box::new(float_key(*imag)))
        }
        Value::String(value) => Key::String(value.clone()),
        Value::Bytes(value) => Key::Bytes(value.clone()),
        Value::Tuple(values) => Key::Tuple(values.iter().map(hash_key).collect::<DocResult<_>>()?),
        Value::Ellipsis => Key::Ellipsis,
        _ => return Err(invalid()),
    })
}
fn integer(value: &Value) -> Option<&str> {
    match value {
        Value::Integer(value) => Some(value),
        Value::Bool(true) => Some("1"),
        Value::Bool(false) => Some("0"),
        _ => None,
    }
}
fn real(value: &Value) -> Option<f64> {
    match value {
        Value::Float(value) => Some(*value),
        Value::Complex(real, imaginary) if *imaginary == 0.0 => Some(*real),
        _ => None,
    }
}
fn integer_real_equal(integer: &str, real: f64) -> bool {
    real.is_finite()
        && real.fract() == 0.0
        && (if real == 0.0 {
            "0".into()
        } else {
            format!("{real:.0}")
        }) == integer
}
pub(super) fn equivalent(a: &Value, b: &Value) -> bool {
    if let (Some(a), Some(b)) = (integer(a), integer(b)) {
        return a == b;
    }
    if let (Some(a), Some(b)) = (integer(a), real(b)) {
        return integer_real_equal(a, b);
    }
    if let (Some(a), Some(b)) = (real(a), integer(b)) {
        return integer_real_equal(b, a);
    }
    if let (Some(a), Some(b)) = (real(a), real(b)) {
        return a == b;
    }
    if let (Value::Tuple(a), Value::Tuple(b)) = (a, b) {
        return a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equivalent(a, b));
    }
    a == b
}

fn literal(expression: &ast::Expr, depth: usize) -> DocResult<Value> {
    if depth > MAX_NESTING {
        return Err(DocError::new("Documentation literal nesting exceeds limit"));
    }
    match expression {
        ast::Expr::Constant(value) => constant(&value.value, depth),
        ast::Expr::List(items) => Ok(Value::List(
            items
                .elts
                .iter()
                .map(|item| literal(item, depth + 1))
                .collect::<DocResult<_>>()?,
        )),
        ast::Expr::Tuple(items) => Ok(Value::Tuple(
            items
                .elts
                .iter()
                .map(|item| literal(item, depth + 1))
                .collect::<DocResult<_>>()?,
        )),
        ast::Expr::Set(items) => {
            let mut values = Vec::new();
            let mut keys = HashSet::new();
            for item in &items.elts {
                let item = literal(item, depth + 1)?;
                if keys.insert(hash_key(&item)?) {
                    values.push(item);
                }
            }
            Ok(Value::Set(values))
        }
        ast::Expr::Dict(dictionary) => {
            let mut values: Vec<(Value, Value)> = Vec::new();
            let mut keys: HashMap<Key, usize> = HashMap::new();
            for (key, value) in dictionary.keys.iter().zip(&dictionary.values) {
                let key = literal(key.as_ref().ok_or_else(invalid)?, depth + 1)?;
                let hash = hash_key(&key)?;
                let value = literal(value, depth + 1)?;
                if let Some(index) = keys.get(&hash) {
                    values[*index].1 = value;
                } else {
                    keys.insert(hash, values.len());
                    values.push((key, value));
                }
            }
            Ok(Value::Map(values))
        }
        ast::Expr::Call(call)
            if call.args.is_empty()
                && call.keywords.is_empty()
                && matches!(call.func.as_ref(), ast::Expr::Name(name) if name.id.as_str() == "set") =>
        {
            Ok(Value::Set(Vec::new()))
        }
        ast::Expr::BinOp(binary)
            if matches!(binary.op, ast::Operator::Add | ast::Operator::Sub) =>
        {
            // literal_eval permits only a signed real number +/- an unsigned
            // complex constant, not general arithmetic or chained additions.
            let real = match numeric(&binary.left, true)? {
                Value::Integer(value) => value
                    .parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite())
                    .ok_or_else(invalid)?,
                Value::Float(value) => value,
                _ => return Err(invalid()),
            };
            let ast::Expr::Constant(right) = binary.right.as_ref() else {
                return Err(invalid());
            };
            let ast::Constant::Complex {
                real: right_real,
                imag,
            } = &right.value
            else {
                return Err(invalid());
            };
            if binary.op == ast::Operator::Add {
                Ok(Value::Complex(real + right_real, *imag))
            } else {
                Ok(Value::Complex(real - right_real, -imag))
            }
        }
        _ => numeric(expression, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elif_source(count: usize) -> String {
        format!(
            "if False:\n    pass\n{}X = 1\n",
            "elif False:\n    pass\n".repeat(count)
        )
    }

    #[test]
    fn lexer_preflight_rejects_flat_elif_recursion_before_ast_construction() {
        assert!(bounded_tokens(&elif_source(128)).is_ok());
        assert!(
            bounded_tokens(&elif_source(129))
                .unwrap_err()
                .message
                .contains("128 elif")
        );
        assert!(
            bounded_tokens(&elif_source(10_000))
                .unwrap_err()
                .message
                .contains("128 elif")
        );
    }

    #[test]
    fn symbol_parser_reports_elif_bounds_without_a_cli_or_subprocess() {
        assert!(
            parse(&elif_source(129), "X")
                .unwrap_err()
                .message
                .contains("128 elif")
        );
        assert_eq!(
            parse("TEXT = 'elif elif'\n# elif\nX = 1\n", "X").unwrap(),
            Value::Integer("1".into())
        );
    }
}
