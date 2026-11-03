use super::super::language::{LanguageError, LanguageErrorKind, MAX_QUERY_BYTES, MAX_QUERY_TOKENS};
use super::{error, syntax, unsupported};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Word(String),
    Quoted(String),
    Parameter(String),
    Unsigned(String),
    Symbol(&'static str),
    End,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Token {
    pub(super) kind: Kind,
    pub(super) position: usize,
}

const SYMBOLS: [&str; 22] = [
    "->", "<-", "<>", "<=", ">=", "..", "(", ")", "[", "]", "{", "}", ":", ",", "|", "*", "=", "$",
    ".", "<", ">", "-",
];

fn word_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_'
}

fn word_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

pub(super) fn lex(text: &str) -> Result<Vec<Token>, LanguageError> {
    if text.len() > MAX_QUERY_BYTES {
        return Err(error(
            LanguageErrorKind::Limit,
            MAX_QUERY_BYTES,
            format!("query exceeds {MAX_QUERY_BYTES} UTF-8 bytes"),
        ));
    }

    let mut tokens = Vec::new();
    let mut characters = text.char_indices().peekable();
    while let Some(&(position, character)) = characters.peek() {
        if matches!(character, ' ' | '\t' | '\r' | '\n') {
            characters.next();
            continue;
        }
        if tokens.len() == MAX_QUERY_TOKENS {
            return Err(error(
                LanguageErrorKind::Limit,
                position,
                format!("query exceeds {MAX_QUERY_TOKENS} tokens"),
            ));
        }

        let kind = if character == '`' {
            characters.next();
            let mut value = String::new();
            let mut closed = false;
            for (at, current) in characters.by_ref() {
                if current == '`' {
                    closed = true;
                    break;
                }
                if current == '\0' || current.is_control() {
                    return Err(syntax(at, "quoted names cannot contain control characters"));
                }
                value.push(current);
            }
            if !closed {
                return Err(syntax(position, "unterminated quoted name"));
            }
            if value.is_empty() {
                return Err(syntax(position, "quoted names cannot be empty"));
            }
            Kind::Quoted(value)
        } else if character == '$' {
            characters.next();
            let mut name = String::new();
            while let Some(&(_, current)) = characters.peek() {
                if !word_character(current) || (name.is_empty() && !word_start(current)) {
                    break;
                }
                name.push(current);
                characters.next();
            }
            if name.is_empty() {
                return Err(syntax(position, "expected a bare parameter name after $"));
            }
            Kind::Parameter(name)
        } else if character.is_ascii_digit() {
            let mut value = String::new();
            while let Some(&(_, current)) = characters.peek() {
                if !current.is_ascii_digit() {
                    break;
                }
                value.push(current);
                characters.next();
            }
            Kind::Unsigned(value)
        } else if word_start(character) {
            let mut value = String::new();
            while let Some(&(_, current)) = characters.peek() {
                if !word_character(current) {
                    break;
                }
                value.push(current);
                characters.next();
            }
            Kind::Word(value)
        } else if matches!(character, '\'' | '"') {
            return Err(unsupported(
                position,
                "literal values are not supported; bind data with a typed parameter",
            ));
        } else if character == ';' {
            return Err(unsupported(
                position,
                "statement terminators are not supported",
            ));
        } else if character == '!' && text[position..].starts_with("!=") {
            return Err(unsupported(position, "use <> for inequality"));
        } else if character == '/'
            && (text[position..].starts_with("//") || text[position..].starts_with("/*"))
        {
            return Err(unsupported(position, "comments are not supported"));
        } else if matches!(character, '+' | '%' | '/' | '^') {
            return Err(unsupported(
                position,
                "arithmetic expressions are not supported",
            ));
        } else {
            let remaining = &text[position..];
            let Some(symbol) = SYMBOLS
                .iter()
                .find(|candidate| remaining.starts_with(**candidate))
            else {
                return Err(syntax(position, "unexpected character"));
            };
            for _ in 0..symbol.len() {
                characters.next();
            }
            Kind::Symbol(symbol)
        };
        tokens.push(Token { kind, position });
    }

    tokens.push(Token {
        kind: Kind::End,
        position: text.len(),
    });
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_offsets_are_utf8_byte_offsets_and_symbols_use_longest_match() {
        let tokens = lex("MATCH (`é`)-[:R*1..3]->(n) RETURN n").unwrap();
        let quoted = tokens
            .iter()
            .find(|token| token.kind == Kind::Quoted("é".into()))
            .unwrap();
        assert_eq!(quoted.position, 7);
        assert!(tokens.iter().any(|token| token.kind == Kind::Symbol("..")));
        assert!(tokens.iter().any(|token| token.kind == Kind::Symbol("->")));
        assert_eq!(
            tokens.last().unwrap().position,
            "MATCH (`é`)-[:R*1..3]->(n) RETURN n".len()
        );
    }

    #[test]
    fn only_ascii_whitespace_is_ignored() {
        assert!(lex("MATCH\n(n)").is_ok());
        let error = lex("MATCH\u{00a0}(n)").unwrap_err();
        assert_eq!(error.position, 5);
        assert_eq!(error.kind, LanguageErrorKind::Syntax);
    }

    #[test]
    fn quoted_name_controls_and_oversized_text_are_rejected_at_byte_boundaries() {
        let error = lex("MATCH (`a\u{0001}`) RETURN a").unwrap_err();
        assert_eq!(error.position, 9);
        assert_eq!(error.kind, LanguageErrorKind::Syntax);
        let text = format!("{}é", " ".repeat(MAX_QUERY_BYTES - 1));
        let error = lex(&text).unwrap_err();
        assert_eq!(error.position, MAX_QUERY_BYTES);
        assert_eq!(error.kind, LanguageErrorKind::Limit);
    }

    #[test]
    fn token_limit_accepts_exactly_the_configured_count() {
        let below_limit = std::iter::repeat_n("x", MAX_QUERY_TOKENS - 1)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(lex(&below_limit).unwrap().len(), MAX_QUERY_TOKENS);
        let at_limit = std::iter::repeat_n("x", MAX_QUERY_TOKENS)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(lex(&at_limit).unwrap().len(), MAX_QUERY_TOKENS + 1);
        let above_limit = format!("{at_limit} x");
        let error = lex(&above_limit).unwrap_err();
        assert_eq!(error.kind, LanguageErrorKind::Limit);
        assert_eq!(error.position, at_limit.len() + 1);
    }
}
