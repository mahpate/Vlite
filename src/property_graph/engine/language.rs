//! EPIC-015 ITEM-051 (REQ-071): the narrow read-only graph query text.
//!
//! A separate parser entry point, independent of the vector query language
//! (`src/query_language.rs`, whose queries always start with `SEARCH`). Every
//! accepted text translates to exactly one [`GraphQuery`] of the existing IR
//! plus at most two read-limit overrides (the hop bound of a `*1..N` range and
//! the `LIMIT` of a traversal); it adds no execution semantics. Anything the
//! IR cannot express fails with a typed [`LanguageError`] naming the byte
//! offset and the supported alternative. The grammar is documented in
//! `docs/GRAPH-QUERY.md`; it is VLite's own and is not compatible with any
//! other query language.
//!
//! Values come from literals or `$parameters` bound from a caller-supplied
//! map; a missing or unused parameter is an error. The graph is selected
//! outside the text. Parsing is bounded: at most [`MAX_QUERY_BYTES`] bytes and
//! [`MAX_QUERY_TOKENS`] tokens, one statement, no recursion.

use super::filter::NodeFilter;
use super::limits::GraphLimits;
use super::model::{Direction, EdgeId, NodeId, Properties, PropertyCondition, PropertyValue};
use super::mutate::EdgeSelector;
use super::query::{GraphQuery, NodeSelector, PropertyProjection, QueryOp};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Longest accepted query text, in bytes (as the vector query language).
pub const MAX_QUERY_BYTES: usize = 16_384;
/// Most tokens one query text may have.
pub const MAX_QUERY_TOKENS: usize = 1_024;

/// What kind of problem a [`LanguageError`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanguageErrorKind {
    /// Malformed text: bad token, missing clause, unknown variable.
    Syntax,
    /// Well-formed but not expressible as one `GraphQuery`; the message names
    /// the supported alternative.
    Unsupported,
    /// A `$parameter` is missing, unused or of the wrong type.
    Parameter,
    /// The text or a hop bound exceeds a cap or ceiling.
    Limit,
}

impl LanguageErrorKind {
    /// A stable lowercase name.
    pub fn code(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Unsupported => "unsupported",
            Self::Parameter => "parameter",
            Self::Limit => "limit",
        }
    }
}

/// A rejected query text, with the byte offset of the offending token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageError {
    pub kind: LanguageErrorKind,
    pub position: usize,
    pub message: String,
}

impl fmt::Display for LanguageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            LanguageErrorKind::Syntax => "syntax error",
            LanguageErrorKind::Unsupported => "unsupported construct",
            LanguageErrorKind::Parameter => "parameter error",
            LanguageErrorKind::Limit => "limit exceeded",
        };
        write!(
            f,
            "graph query byte {}: {kind}: {}",
            self.position, self.message
        )
    }
}

impl std::error::Error for LanguageError {}

fn error(kind: LanguageErrorKind, position: usize, message: impl Into<String>) -> LanguageError {
    LanguageError {
        kind,
        position,
        message: message.into(),
    }
}

fn syntax(position: usize, message: impl Into<String>) -> LanguageError {
    error(LanguageErrorKind::Syntax, position, message)
}

fn unsupported(position: usize, message: impl Into<String>) -> LanguageError {
    error(LanguageErrorKind::Unsupported, position, message)
}

/// A parsed query text: one IR query and the read-limit overrides it states.
#[derive(Clone, Debug, PartialEq)]
pub struct TextQuery {
    /// The IR query; `explain` is always false.
    pub query: GraphQuery,
    /// The upper bound `N` of a `*1..N` hop range (traversal, shortest path).
    pub max_hops: Option<u64>,
    /// The `LIMIT` of a traversal (its output node cap).
    pub max_output_nodes: Option<u64>,
}

impl TextQuery {
    /// `base` with this query's overrides applied.
    pub fn limits(&self, base: GraphLimits) -> GraphLimits {
        GraphLimits {
            max_hops: self.max_hops.unwrap_or(base.max_hops),
            max_output_nodes: self.max_output_nodes.unwrap_or(base.max_output_nodes),
            ..base
        }
    }
}

/// Parses `text` with `params` into one IR query.
pub fn parse(text: &str, params: &Properties) -> Result<TextQuery, LanguageError> {
    if text.len() > MAX_QUERY_BYTES {
        return Err(error(
            LanguageErrorKind::Limit,
            MAX_QUERY_BYTES,
            format!("query exceeds {MAX_QUERY_BYTES} bytes"),
        ));
    }
    let tokens = lex(text)?;
    let mut parser = Parser {
        tokens,
        at: 0,
        params,
        used: BTreeSet::new(),
    };
    let parsed = parser.query()?;
    if let Some(unused) = params.keys().find(|name| !parser.used.contains(*name)) {
        return Err(error(
            LanguageErrorKind::Parameter,
            text.len(),
            format!("parameter ${unused} is not used by the query"),
        ));
    }
    lower(parsed)
}

// Tokens.

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    /// A bare identifier or keyword.
    Word(String),
    /// A backquoted identifier, never a keyword.
    Quoted(String),
    Str(String),
    Number {
        text: String,
        float: bool,
    },
    Param(String),
    Sym(&'static str),
    End,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    pos: usize,
}

const SYMBOLS: [&str; 22] = [
    "->", "<-", "<>", "!=", ">=", "<=", "..", "(", ")", "[", "]", "{", "}", ":", ",", ".", "|",
    "*", "=", "<", ">", "-",
];

fn is_word_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn lex(text: &str) -> Result<Vec<Token>, LanguageError> {
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some(&(pos, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if tokens.len() >= MAX_QUERY_TOKENS {
            return Err(error(
                LanguageErrorKind::Limit,
                pos,
                format!("query exceeds {MAX_QUERY_TOKENS} tokens"),
            ));
        }
        let tok = if c == '"' {
            chars.next();
            let mut value = String::new();
            loop {
                match chars.next() {
                    None => return Err(syntax(pos, "unterminated string")),
                    Some((_, '"')) => break,
                    Some((at, '\\')) => match chars.next() {
                        Some((_, '"')) => value.push('"'),
                        Some((_, '\\')) => value.push('\\'),
                        Some((_, 'n')) => value.push('\n'),
                        Some((_, 't')) => value.push('\t'),
                        _ => {
                            return Err(syntax(
                                at,
                                "unsupported escape; use \\\", \\\\, \\n or \\t",
                            ));
                        }
                    },
                    Some((_, c)) => value.push(c),
                }
            }
            Tok::Str(value)
        } else if c == '`' {
            chars.next();
            let mut name = String::new();
            loop {
                match chars.next() {
                    None => return Err(syntax(pos, "unterminated backquoted name")),
                    Some((_, '`')) => break,
                    Some((_, c)) => name.push(c),
                }
            }
            if name.is_empty() {
                return Err(syntax(pos, "empty backquoted name"));
            }
            Tok::Quoted(name)
        } else if c == '\'' {
            return Err(syntax(pos, "strings use double quotes"));
        } else if c == ';' {
            return Err(unsupported(
                pos,
                "one statement per query; send each query separately",
            ));
        } else if c == '$' {
            chars.next();
            let mut name = String::new();
            while let Some(&(_, c)) = chars.peek() {
                if name.is_empty() && !is_word_start(c) || !is_word_char(c) {
                    break;
                }
                name.push(c);
                chars.next();
            }
            if name.is_empty() {
                return Err(syntax(pos, "expected a parameter name after $"));
            }
            Tok::Param(name)
        } else if c.is_ascii_digit() {
            let mut number = String::new();
            let mut float = false;
            while let Some(&(_, c)) = chars.peek() {
                if !c.is_ascii_digit() {
                    break;
                }
                number.push(c);
                chars.next();
            }
            let rest = &text[pos + number.len()..];
            let bytes = rest.as_bytes();
            if bytes.first() == Some(&b'.') && bytes.get(1).is_some_and(u8::is_ascii_digit) {
                float = true;
                number.push('.');
                chars.next();
                while let Some(&(_, c)) = chars.peek() {
                    if !c.is_ascii_digit() {
                        break;
                    }
                    number.push(c);
                    chars.next();
                }
            }
            let rest = &text.as_bytes()[pos + number.len()..];
            if matches!(rest.first(), Some(b'e' | b'E')) {
                let digits = match rest.get(1) {
                    Some(b'+' | b'-') => 2,
                    _ => 1,
                };
                if rest.get(digits).is_some_and(u8::is_ascii_digit) {
                    float = true;
                    for _ in 0..digits {
                        number.push(chars.next().expect("peeked exponent").1);
                    }
                    while let Some(&(_, c)) = chars.peek() {
                        if !c.is_ascii_digit() {
                            break;
                        }
                        number.push(c);
                        chars.next();
                    }
                }
            }
            if chars.peek().is_some_and(|&(_, c)| is_word_char(c)) {
                return Err(syntax(pos, "invalid number"));
            }
            Tok::Number {
                text: number,
                float,
            }
        } else if is_word_start(c) {
            let mut word = String::new();
            while let Some(&(_, c)) = chars.peek() {
                if !is_word_char(c) {
                    break;
                }
                word.push(c);
                chars.next();
            }
            Tok::Word(word)
        } else {
            let rest = &text[pos..];
            let Some(symbol) = SYMBOLS.iter().find(|symbol| rest.starts_with(**symbol)) else {
                return Err(syntax(pos, format!("unexpected character {c:?}")));
            };
            for _ in 0..symbol.len() {
                chars.next();
            }
            Tok::Sym(symbol)
        };
        tokens.push(Token { tok, pos });
    }
    tokens.push(Token {
        tok: Tok::End,
        pos: text.len(),
    });
    Ok(tokens)
}

// Keywords. Supported keywords cannot be variables; the other reserved words
// are recognized only to explain why they are not supported.

const KEYWORDS: [&str; 11] = [
    "MATCH", "WHERE", "AND", "RETURN", "LIMIT", "KEY", "ID", "SHORTEST", "PATH", "TRUE", "FALSE",
];

/// Reserved words that are not supported, with the supported alternative.
fn reserved_hint(word: &str) -> Option<&'static str> {
    let upper = word.to_ascii_uppercase();
    Some(match upper.as_str() {
        "CREATE" | "MERGE" | "DELETE" | "DETACH" | "SET" | "REMOVE" | "DROP" | "FOREACH"
        | "LOAD" | "INSERT" | "UPDATE" | "UPSERT" => {
            "graph queries are read-only; write with the typed graph batch API"
        }
        "SEARCH" | "TEXT" | "NEAR" | "GROUP" | "FACET" | "VERSION" => {
            "this belongs to the vector query language (query_language::parse); graph queries are MATCH ... RETURN"
        }
        "OR" | "XOR" | "NOT" | "IN" | "IS" | "NULL" | "EXISTS" | "CONTAINS" | "STARTS" | "ENDS" => {
            "WHERE supports only AND-joined comparisons var.field = value, >= value or <= value"
        }
        "OPTIONAL" | "WITH" | "UNWIND" | "UNION" | "CALL" | "USE" | "AS" | "CASE" => {
            "a query has one MATCH pattern, an optional WHERE, one RETURN and an optional LIMIT"
        }
        "ORDER" | "SKIP" | "OFFSET" | "DISTINCT" | "BY" | "ASC" | "DESC" => {
            "results use the fixed IR order; page with LIMIT and the typed API cursor"
        }
        "EXPLAIN" | "PROFILE" => "use explain_text (vlite-graph-cli --explain) to explain a query",
        "ALL" | "ANY" | "NONE" | "SINGLE" | "COUNT" => {
            "aggregations and path predicates are not supported; RETURN variables or var.field"
        }
        _ => return None,
    })
}

fn is_keyword(word: &str) -> bool {
    KEYWORDS
        .iter()
        .any(|keyword| word.eq_ignore_ascii_case(keyword))
}

// Syntax tree.

#[derive(Clone, Copy, Debug, PartialEq)]
enum Compare {
    Eq,
    AtLeast,
    AtMost,
}

#[derive(Clone, Debug)]
struct Condition {
    field: String,
    compare: Compare,
    value: PropertyValue,
}

impl Condition {
    fn ir(self) -> PropertyCondition {
        match self.compare {
            Compare::Eq => PropertyCondition::Eq {
                field: self.field,
                value: self.value,
            },
            Compare::AtLeast => PropertyCondition::Range {
                field: self.field,
                min: Some(self.value),
                max: None,
            },
            Compare::AtMost => PropertyCondition::Range {
                field: self.field,
                min: None,
                max: Some(self.value),
            },
        }
    }
}

#[derive(Clone, Debug)]
enum Anchor {
    Key(String),
    Id(u64),
}

#[derive(Clone, Debug)]
struct Name {
    text: String,
    pos: usize,
}

#[derive(Clone, Debug, Default)]
struct Element {
    pos: usize,
    var: Option<Name>,
    labels: Vec<Name>,
    anchor: Option<(Anchor, usize)>,
    /// Inline `{field: value}` equalities and the position of `{`.
    props: Option<(Vec<Condition>, usize)>,
}

#[derive(Clone, Debug)]
struct Relationship {
    element: Element,
    direction: Direction,
    /// The hop range's upper bound and position.
    range: Option<(u64, usize)>,
}

#[derive(Clone, Debug)]
struct Item {
    var: Name,
    field: Option<String>,
}

#[derive(Clone, Debug)]
struct Parsed {
    path: Option<Name>,
    shortest: Option<usize>,
    start: Element,
    hop: Option<(Relationship, Element)>,
    /// `WHERE` conditions by variable, in text order.
    conditions: Vec<(Name, Condition)>,
    returns: Vec<Item>,
    limit: Option<(u64, usize)>,
}

struct Parser<'p> {
    tokens: Vec<Token>,
    at: usize,
    params: &'p Properties,
    used: BTreeSet<String>,
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Word(word) => format!("{word:?}"),
        Tok::Quoted(name) => format!("`{name}`"),
        Tok::Str(_) => "a string".into(),
        Tok::Number { text, .. } => text.clone(),
        Tok::Param(name) => format!("${name}"),
        Tok::Sym(symbol) => format!("{symbol:?}"),
        Tok::End => "the end of the query".into(),
    }
}

impl Parser<'_> {
    fn peek(&self) -> &Token {
        &self.tokens[self.at]
    }

    fn peek_at(&self, offset: usize) -> &Tok {
        &self.tokens[(self.at + offset).min(self.tokens.len() - 1)].tok
    }

    fn pos(&self) -> usize {
        self.peek().pos
    }

    fn bump(&mut self) -> Token {
        let token = self.tokens[self.at].clone();
        if self.at + 1 < self.tokens.len() {
            self.at += 1;
        }
        token
    }

    fn is_kw(&self, keyword: &str) -> bool {
        matches!(&self.peek().tok, Tok::Word(word) if word.eq_ignore_ascii_case(keyword))
    }

    fn eat_kw(&mut self, keyword: &str) -> Option<usize> {
        self.is_kw(keyword).then(|| self.bump().pos)
    }

    fn is_sym(&self, symbol: &str) -> bool {
        self.peek().tok
            == Tok::Sym(match SYMBOLS.iter().find(|s| **s == symbol) {
                Some(symbol) => symbol,
                None => return false,
            })
    }

    fn eat_sym(&mut self, symbol: &str) -> Option<usize> {
        self.is_sym(symbol).then(|| self.bump().pos)
    }

    /// The error for an unexpected current token.
    fn unexpected(&self, expected: &str) -> LanguageError {
        let token = self.peek();
        if let Tok::Word(word) = &token.tok
            && let Some(hint) = reserved_hint(word)
        {
            return unsupported(
                token.pos,
                format!("{} is not supported: {hint}", word.to_ascii_uppercase()),
            );
        }
        match &token.tok {
            Tok::Sym(">" | "<" | "<>" | "!=") => unsupported(
                token.pos,
                "only =, >= and <= comparisons are supported (ranges are inclusive)",
            ),
            Tok::Sym("*") if expected.contains("RETURN") => unsupported(
                token.pos,
                "RETURN * is not supported; RETURN the pattern variables or var.field",
            ),
            _ => syntax(
                token.pos,
                format!("expected {expected}, found {}", describe(&token.tok)),
            ),
        }
    }

    fn expect_kw(&mut self, keyword: &str) -> Result<usize, LanguageError> {
        self.eat_kw(keyword).ok_or_else(|| self.unexpected(keyword))
    }

    fn expect_sym(&mut self, symbol: &str) -> Result<usize, LanguageError> {
        self.eat_sym(symbol)
            .ok_or_else(|| self.unexpected(&format!("{symbol:?}")))
    }

    /// A variable: a non-keyword word or a backquoted name.
    fn variable(&mut self) -> Option<Result<Name, LanguageError>> {
        let token = self.peek().clone();
        match token.tok {
            Tok::Word(word) => {
                if is_keyword(&word) || reserved_hint(&word).is_some() {
                    return None;
                }
                self.bump();
                Some(Ok(Name {
                    text: word,
                    pos: token.pos,
                }))
            }
            Tok::Quoted(name) => {
                self.bump();
                Some(Ok(Name {
                    text: name,
                    pos: token.pos,
                }))
            }
            _ => None,
        }
    }

    /// A label, relationship type or field name: any word or backquoted name.
    fn name(&mut self, what: &str) -> Result<String, LanguageError> {
        match self.peek().tok.clone() {
            Tok::Word(word) | Tok::Quoted(word) => {
                self.bump();
                Ok(word)
            }
            _ => Err(self.unexpected(what)),
        }
    }

    fn parameter(&mut self, name: &str, pos: usize) -> Result<PropertyValue, LanguageError> {
        let value = self.params.get(name).cloned().ok_or_else(|| {
            error(
                LanguageErrorKind::Parameter,
                pos,
                format!("missing parameter ${name}"),
            )
        })?;
        self.used.insert(name.to_owned());
        Ok(value)
    }

    fn number(text: &str, float: bool, pos: usize) -> Result<PropertyValue, LanguageError> {
        if float {
            match text.parse::<f64>() {
                Ok(value) if value.is_finite() => Ok(PropertyValue::Float(value)),
                _ => Err(syntax(pos, "float literal is not finite")),
            }
        } else {
            text.parse::<i64>()
                .map(PropertyValue::Integer)
                .map_err(|_| syntax(pos, "integer literal exceeds the i64 range"))
        }
    }

    /// A literal or parameter value.
    fn value(&mut self) -> Result<(PropertyValue, usize), LanguageError> {
        let token = self.peek().clone();
        let value = match &token.tok {
            Tok::Str(text) => PropertyValue::String(text.clone()),
            Tok::Number { text, float } => Self::number(text, *float, token.pos)?,
            Tok::Sym("-") => {
                self.bump();
                let Tok::Number { text, float } = self.peek().tok.clone() else {
                    return Err(self.unexpected("a number after -"));
                };
                Self::number(&format!("-{text}"), float, token.pos)?
            }
            Tok::Param(name) => self.parameter(name, token.pos)?,
            Tok::Word(word) if word.eq_ignore_ascii_case("TRUE") => PropertyValue::Boolean(true),
            Tok::Word(word) if word.eq_ignore_ascii_case("FALSE") => PropertyValue::Boolean(false),
            Tok::Word(word) if word.eq_ignore_ascii_case("NULL") => {
                return Err(unsupported(
                    token.pos,
                    "NULL is not a value; a missing field never matches a condition",
                ));
            }
            Tok::Sym("[") => {
                return Err(unsupported(
                    token.pos,
                    "lists are not values; join one = condition per value with AND or use the typed API",
                ));
            }
            _ => return Err(self.unexpected("a value (string, number, true, false or $parameter)")),
        };
        self.bump();
        Ok((value, token.pos))
    }

    fn anchor(&mut self) -> Result<Option<(Anchor, usize)>, LanguageError> {
        if let Some(pos) = self.eat_kw("KEY") {
            let (value, at) = self.value()?;
            let PropertyValue::String(key) = value else {
                return Err(error(
                    LanguageErrorKind::Parameter,
                    at,
                    "KEY takes a string",
                ));
            };
            return Ok(Some((Anchor::Key(key), pos)));
        }
        if let Some(pos) = self.eat_kw("ID") {
            let (value, at) = self.value()?;
            let id = match value {
                PropertyValue::Integer(id) => u64::try_from(id).ok(),
                _ => None,
            }
            .ok_or_else(|| {
                error(
                    LanguageErrorKind::Parameter,
                    at,
                    "ID takes a non-negative integer",
                )
            })?;
            return Ok(Some((Anchor::Id(id), pos)));
        }
        Ok(None)
    }

    /// `{field: value, ...}`.
    fn props(&mut self) -> Result<Option<(Vec<Condition>, usize)>, LanguageError> {
        let Some(pos) = self.eat_sym("{") else {
            return Ok(None);
        };
        let mut conditions = Vec::new();
        if self.eat_sym("}").is_none() {
            loop {
                let field = self.name("a property name")?;
                self.expect_sym(":")?;
                let (value, _) = self.value()?;
                conditions.push(Condition {
                    field,
                    compare: Compare::Eq,
                    value,
                });
                if self.eat_sym(",").is_none() {
                    break;
                }
            }
            self.expect_sym("}")?;
        }
        Ok(Some((conditions, pos)))
    }

    /// `(var :Label... KEY|ID value {props})`.
    fn node(&mut self) -> Result<Element, LanguageError> {
        let pos = self.expect_sym("(")?;
        let var = self.variable().transpose()?;
        let mut labels = Vec::new();
        while let Some(at) = self.eat_sym(":") {
            labels.push(Name {
                text: self.name("a label")?,
                pos: at,
            });
        }
        let anchor = self.anchor()?;
        let props = self.props()?;
        self.expect_sym(")")?;
        Ok(Element {
            pos,
            var,
            labels,
            anchor,
            props,
        })
    }

    /// `*1..N` or `*..N`.
    fn range(&mut self) -> Result<Option<(u64, usize)>, LanguageError> {
        let Some(pos) = self.eat_sym("*") else {
            return Ok(None);
        };
        let bound = |parser: &mut Self| -> Result<(u64, usize), LanguageError> {
            let token = parser.peek().clone();
            match &token.tok {
                Tok::Number { text, float: false } => {
                    parser.bump();
                    let value = text.parse::<u64>().unwrap_or(u64::MAX);
                    Ok((value, token.pos))
                }
                Tok::Param(_) => Err(unsupported(
                    token.pos,
                    "hop bounds are integer literals, for example *1..3",
                )),
                _ => Err(parser.unexpected("an integer hop bound")),
            }
        };
        let (min, min_pos) = if self.is_sym("..") {
            (1, pos)
        } else if matches!(self.peek().tok, Tok::Number { .. } | Tok::Param(_)) {
            let (min, min_pos) = bound(self)?;
            if !self.is_sym("..") {
                return Err(unsupported(
                    pos,
                    format!(
                        "exact hop counts are not supported; write *1..{min} for up to {min} hops"
                    ),
                ));
            }
            (min, min_pos)
        } else {
            return Err(unsupported(
                pos,
                format!(
                    "unbounded hop ranges are not supported; write *1..N with N <= {}",
                    GraphLimits::CEILING.max_hops
                ),
            ));
        };
        self.expect_sym("..")?;
        let (max, max_pos) = bound(self)?;
        if min != 1 {
            return Err(unsupported(
                min_pos,
                "the minimum hop count is always 1 (*1..N); results carry each node's depth",
            ));
        }
        if max == 0 {
            return Err(syntax(max_pos, "the hop range *1..N needs N >= 1"));
        }
        if max > GraphLimits::CEILING.max_hops {
            return Err(error(
                LanguageErrorKind::Limit,
                max_pos,
                format!(
                    "hop bound {max} exceeds the ceiling {}",
                    GraphLimits::CEILING.max_hops
                ),
            ));
        }
        Ok(Some((max, pos)))
    }

    /// `-[...]->`, `<-[...]-`, `-[...]-` or the bracketless `-->`, `<--`, `--`.
    fn relationship(&mut self) -> Result<Relationship, LanguageError> {
        let pos = self.pos();
        let incoming = if self.eat_sym("<-").is_some() {
            true
        } else if self.eat_sym("-").is_some() {
            false
        } else {
            return Err(self.unexpected("a relationship (-[...]->, <-[...]- or -[...]-)"));
        };
        let mut element = Element {
            pos,
            ..Element::default()
        };
        let mut range = None;
        if self.eat_sym("[").is_some() {
            element.var = self.variable().transpose()?;
            if let Some(at) = self.eat_sym(":") {
                loop {
                    element.labels.push(Name {
                        text: self.name("a relationship type")?,
                        pos: at,
                    });
                    if self.eat_sym("|").is_none() {
                        break;
                    }
                    self.eat_sym(":");
                }
            }
            element.anchor = self.anchor()?;
            range = self.range()?;
            element.props = self.props()?;
            self.expect_sym("]")?;
        }
        let outgoing = if self.eat_sym("->").is_some() {
            true
        } else if self.eat_sym("-").is_some() {
            false
        } else {
            return Err(self.unexpected("-> or - to close the relationship"));
        };
        let direction = match (incoming, outgoing) {
            (false, true) => Direction::Out,
            (true, false) => Direction::In,
            (false, false) => Direction::Both,
            (true, true) => {
                return Err(syntax(
                    pos,
                    "a relationship has one direction: ->, <- or none for both",
                ));
            }
        };
        Ok(Relationship {
            element,
            direction,
            range,
        })
    }

    fn query(&mut self) -> Result<Parsed, LanguageError> {
        self.expect_kw("MATCH")?;
        let mut path = None;
        if matches!(self.peek_at(1), Tok::Sym("=")) {
            path = self.variable().transpose()?;
            if path.is_none() {
                return Err(self.unexpected("a path variable"));
            }
            self.expect_sym("=")?;
        }
        let shortest = self.eat_kw("SHORTEST");
        if shortest.is_some() {
            self.expect_kw("PATH")?;
        }
        let start = self.node()?;
        let mut hop = None;
        if !self.is_kw("WHERE") && !self.is_kw("RETURN") && self.peek().tok != Tok::End {
            if self.is_sym(",") {
                return Err(unsupported(
                    self.pos(),
                    "one pattern per query: a node, or a node, one relationship and a node",
                ));
            }
            let relationship = self.relationship()?;
            let end = self.node()?;
            if self.is_sym("-") || self.is_sym("<-") {
                return Err(unsupported(
                    self.pos(),
                    "one relationship per pattern; use a hop range *1..N for multi-hop reads",
                ));
            }
            hop = Some((relationship, end));
        }
        let mut conditions = Vec::new();
        if self.eat_kw("WHERE").is_some() {
            loop {
                let Some(var) = self.variable().transpose()? else {
                    return Err(self.unexpected("var.field"));
                };
                self.expect_sym(".")?;
                let field = self.name("a property name")?;
                let compare = if self.eat_sym("=").is_some() {
                    Compare::Eq
                } else if self.eat_sym(">=").is_some() {
                    Compare::AtLeast
                } else if self.eat_sym("<=").is_some() {
                    Compare::AtMost
                } else {
                    return Err(self.unexpected("=, >= or <="));
                };
                let (value, _) = self.value()?;
                conditions.push((
                    var,
                    Condition {
                        field,
                        compare,
                        value,
                    },
                ));
                if self.eat_kw("AND").is_none() {
                    break;
                }
            }
        }
        self.expect_kw("RETURN")?;
        let mut returns = Vec::new();
        loop {
            let Some(var) = self.variable().transpose()? else {
                return Err(self.unexpected("a variable to RETURN"));
            };
            if self.is_sym("(") {
                return Err(unsupported(
                    var.pos,
                    "functions are not supported; RETURN variables or var.field",
                ));
            }
            let field = if self.eat_sym(".").is_some() {
                Some(self.name("a property name")?)
            } else {
                None
            };
            returns.push(Item { var, field });
            if self.eat_sym(",").is_none() {
                break;
            }
        }
        let mut limit = None;
        if let Some(pos) = self.eat_kw("LIMIT") {
            let (value, at) = self.value()?;
            let count = match value {
                PropertyValue::Integer(count) => u64::try_from(count).ok(),
                _ => None,
            }
            .ok_or_else(|| {
                if matches!(self.tokens[self.at - 1].tok, Tok::Param(_)) {
                    error(
                        LanguageErrorKind::Parameter,
                        at,
                        "LIMIT takes a non-negative integer",
                    )
                } else {
                    syntax(at, "LIMIT takes a non-negative integer")
                }
            })?;
            limit = Some((count, pos));
        }
        if self.peek().tok != Tok::End {
            return Err(self.unexpected("the end of the query"));
        }
        Ok(Parsed {
            path,
            shortest,
            start,
            hop,
            conditions,
            returns,
            limit,
        })
    }
}

// Lowering to the IR.

/// What a pattern element may carry for the chosen operation.
fn forbid(
    element: &Element,
    what: &str,
    labels: bool,
    anchor: bool,
    props: bool,
) -> Result<(), LanguageError> {
    if labels && let Some(label) = element.labels.first() {
        return Err(unsupported(
            label.pos,
            format!("{what} takes no labels or types"),
        ));
    }
    if anchor && let Some((_, pos)) = &element.anchor {
        return Err(unsupported(*pos, format!("{what} takes no KEY or ID")));
    }
    if props && let Some((_, pos)) = &element.props {
        return Err(unsupported(*pos, format!("{what} takes no property map")));
    }
    Ok(())
}

fn selector(anchor: &Anchor) -> NodeSelector {
    match anchor {
        Anchor::Key(key) => NodeSelector::Key(key.clone()),
        Anchor::Id(id) => NodeSelector::Id(NodeId(*id)),
    }
}

fn types(relationship: &Relationship) -> Option<BTreeSet<String>> {
    let labels = &relationship.element.labels;
    (!labels.is_empty()).then(|| labels.iter().map(|name| name.text.clone()).collect())
}

fn inline(element: &Element) -> Vec<PropertyCondition> {
    element
        .props
        .iter()
        .flat_map(|(conditions, _)| conditions.iter().cloned().map(Condition::ir))
        .collect()
}

/// The pattern variables, which must be distinct.
fn bind(parsed: &Parsed) -> Result<BTreeMap<String, &'static str>, LanguageError> {
    let mut bound = BTreeMap::new();
    let mut add = |name: &Option<Name>, role: &'static str| -> Result<(), LanguageError> {
        if let Some(name) = name
            && bound.insert(name.text.clone(), role).is_some()
        {
            return Err(unsupported(
                name.pos,
                format!(
                    "variable {} is bound twice; each variable names one pattern element",
                    name.text
                ),
            ));
        }
        Ok(())
    };
    add(&parsed.path, "path")?;
    add(&parsed.start.var, "start")?;
    if let Some((relationship, end)) = &parsed.hop {
        add(&relationship.element.var, "relationship")?;
        add(&end.var, "end")?;
    }
    Ok(bound)
}

fn describe_role(role: &str) -> &'static str {
    match role {
        "start" => "the node variable",
        "relationship" => "the relationship variable",
        "end" => "the last node variable",
        _ => "the path variable",
    }
}

fn lower(parsed: Parsed) -> Result<TextQuery, LanguageError> {
    let bound = bind(&parsed)?;
    let role = |name: &Name| -> Result<&'static str, LanguageError> {
        bound
            .get(&name.text)
            .copied()
            .ok_or_else(|| syntax(name.pos, format!("unknown variable {}", name.text)))
    };
    // WHERE conditions by role, checked against the bound variables.
    let mut where_by_role: BTreeMap<&'static str, Vec<PropertyCondition>> = BTreeMap::new();
    let mut where_pos: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (name, condition) in &parsed.conditions {
        let role = role(name)?;
        if role == "path" {
            return Err(unsupported(name.pos, "paths have no properties to compare"));
        }
        where_pos.entry(role).or_insert(name.pos);
        where_by_role
            .entry(role)
            .or_default()
            .push(condition.clone().ir());
    }
    let mut returned = BTreeSet::new();
    let mut fields = BTreeSet::new();
    let mut bare = None;
    for item in &parsed.returns {
        let role = role(&item.var)?;
        returned.insert(role);
        match &item.field {
            Some(field) => {
                if role == "path" {
                    return Err(unsupported(
                        item.var.pos,
                        "paths have no properties to return",
                    ));
                }
                fields.insert(field.clone());
                if bare == Some(true) {
                    return Err(unsupported(
                        item.var.pos,
                        "RETURN either whole variables or only var.field items, not both",
                    ));
                }
                bare = Some(false);
            }
            None => {
                if bare == Some(false) {
                    return Err(unsupported(
                        item.var.pos,
                        "RETURN either whole variables or only var.field items, not both",
                    ));
                }
                bare = Some(true);
            }
        }
    }
    let projection = if fields.is_empty() {
        PropertyProjection::All
    } else {
        PropertyProjection::Only(fields)
    };
    let check_returns = |allowed: &[&str], what: &str| -> Result<(), LanguageError> {
        for item in &parsed.returns {
            let role = role(&item.var)?;
            if !allowed.contains(&role) {
                let names: Vec<&str> = allowed.iter().map(|role| describe_role(role)).collect();
                return Err(unsupported(
                    item.var.pos,
                    format!("{what} can only RETURN {}", names.join(" or ")),
                ));
            }
        }
        Ok(())
    };
    let check_where = |allowed: &[&str], what: &str| -> Result<(), LanguageError> {
        for (role, pos) in &where_pos {
            if !allowed.contains(role) {
                let alternative = if allowed.is_empty() {
                    "it takes no WHERE".to_owned()
                } else {
                    format!("WHERE may only filter the {}", allowed.join(" or "))
                };
                return Err(unsupported(*pos, format!("{what}: {alternative}")));
            }
        }
        Ok(())
    };
    let no_limit = |what: &str| -> Result<(), LanguageError> {
        match parsed.limit {
            Some((_, pos)) => Err(unsupported(pos, format!("{what} takes no LIMIT"))),
            None => Ok(()),
        }
    };
    let where_of = |role: &str| where_by_role.get(role).cloned().unwrap_or_default();
    let mut max_hops = None;
    let mut max_output_nodes = None;

    let op = match (&parsed.hop, parsed.shortest) {
        (None, Some(pos)) => {
            return Err(unsupported(
                pos,
                "SHORTEST PATH needs (a KEY ...)-[:TYPE*1..N]->(b KEY ...)",
            ));
        }
        (None, None) => {
            let start = &parsed.start;
            if let Some(path) = &parsed.path {
                return Err(unsupported(
                    path.pos,
                    "a path variable needs a relationship pattern",
                ));
            }
            if start.var.is_none() {
                return Err(syntax(start.pos, "the node needs a variable to RETURN"));
            }
            check_returns(&["start"], "this pattern")?;
            if let Some((anchor, _)) = &start.anchor {
                forbid(start, "a KEY or ID lookup", true, false, true)?;
                check_where(&[], "a KEY or ID lookup")?;
                no_limit("a KEY or ID lookup")?;
                QueryOp::GetNode {
                    node: selector(anchor),
                }
            } else {
                if let Some(extra) = start.labels.get(1) {
                    return Err(unsupported(
                        extra.pos,
                        "finding nodes takes at most one label; filter the others with the typed traverse API",
                    ));
                }
                let mut conditions = inline(start);
                conditions.extend(where_of("start"));
                QueryOp::FindNodes {
                    label: start.labels.first().map(|label| label.text.clone()),
                    conditions,
                    limit: parsed.limit.map(|(limit, _)| limit),
                    cursor: None,
                }
            }
        }
        (Some((relationship, end)), shortest) => {
            let start = &parsed.start;
            let rel = &relationship.element;
            if let Some((anchor, _)) = &rel.anchor {
                if shortest.is_some() || relationship.range.is_some() {
                    return Err(unsupported(
                        rel.pos,
                        "an edge KEY or ID lookup takes no SHORTEST PATH or hop range",
                    ));
                }
                if let Some(path) = &parsed.path {
                    return Err(unsupported(path.pos, "an edge lookup has no path"));
                }
                for node in [start, end] {
                    forbid(node, "an edge lookup endpoint", true, true, true)?;
                }
                forbid(rel, "an edge KEY or ID lookup", true, false, true)?;
                if rel.var.is_none() {
                    return Err(syntax(
                        rel.pos,
                        "the relationship needs a variable to RETURN",
                    ));
                }
                check_returns(&["relationship"], "an edge lookup")?;
                check_where(&[], "an edge lookup")?;
                no_limit("an edge lookup")?;
                QueryOp::GetEdge {
                    edge: match anchor {
                        Anchor::Key(key) => EdgeSelector::Key(key.clone()),
                        Anchor::Id(id) => EdgeSelector::Id(EdgeId(*id)),
                    },
                }
            } else {
                let Some((source, _)) = &start.anchor else {
                    return Err(unsupported(
                        start.pos,
                        "the first node must be anchored with KEY or ID; find candidates with MATCH (n:Label) first",
                    ));
                };
                forbid(start, "the anchored first node", true, false, true)?;
                if let Some(shortest) = shortest {
                    let Some((hops, _)) = relationship.range else {
                        return Err(unsupported(
                            shortest,
                            "SHORTEST PATH needs a hop range *1..N",
                        ));
                    };
                    let Some((destination, _)) = &end.anchor else {
                        return Err(unsupported(
                            end.pos,
                            "SHORTEST PATH needs the last node anchored with KEY or ID",
                        ));
                    };
                    forbid(end, "the anchored last node", true, false, true)?;
                    forbid(rel, "a SHORTEST PATH relationship", false, false, true)?;
                    if let Some(var) = &rel.var {
                        return Err(unsupported(
                            var.pos,
                            "a SHORTEST PATH relationship takes no variable; RETURN the path",
                        ));
                    }
                    let Some(_) = &parsed.path else {
                        return Err(unsupported(
                            shortest,
                            "name the path: MATCH p = SHORTEST PATH ... RETURN p",
                        ));
                    };
                    check_returns(&["path"], "SHORTEST PATH")?;
                    check_where(&[], "SHORTEST PATH")?;
                    no_limit("SHORTEST PATH")?;
                    max_hops = Some(hops);
                    QueryOp::ShortestPath {
                        source: selector(source),
                        destination: selector(destination),
                        direction: relationship.direction,
                        types: types(relationship),
                    }
                } else if let Some((hops, _)) = relationship.range {
                    if let Some((_, pos)) = &end.anchor {
                        return Err(unsupported(
                            *pos,
                            "a traversal returns every matching node; use SHORTEST PATH to reach one anchored node",
                        ));
                    }
                    check_returns(&["end", "relationship", "path"], "a traversal")?;
                    check_where(&["end", "relationship"], "a traversal")?;
                    let mut edge_filter = inline(rel);
                    edge_filter.extend(where_of("relationship"));
                    let mut conditions = inline(end);
                    conditions.extend(where_of("end"));
                    max_hops = Some(hops);
                    max_output_nodes = parsed.limit.map(|(limit, _)| limit);
                    QueryOp::Traverse {
                        start: vec![selector(source)],
                        direction: relationship.direction,
                        types: types(relationship),
                        expand_filter: NodeFilter::default(),
                        return_filter: NodeFilter {
                            labels: end.labels.iter().map(|label| label.text.clone()).collect(),
                            conditions,
                        },
                        edge_filter,
                        include_paths: returned.contains("path"),
                    }
                } else {
                    if let Some(path) = &parsed.path {
                        return Err(unsupported(
                            path.pos,
                            "a single-hop neighbors read has no paths; use a hop range *1..1",
                        ));
                    }
                    forbid(
                        end,
                        "the neighbor node (use a hop range *1..1 to filter it)",
                        true,
                        true,
                        true,
                    )?;
                    check_returns(&["end", "relationship"], "a neighbors read")?;
                    check_where(
                        &["relationship"],
                        "a neighbors read (use a hop range *1..1 to filter nodes)",
                    )?;
                    let mut edge_conditions = inline(rel);
                    edge_conditions.extend(where_of("relationship"));
                    QueryOp::Neighbors {
                        node: selector(source),
                        direction: relationship.direction,
                        types: types(relationship),
                        edge_conditions,
                        limit: parsed.limit.map(|(limit, _)| limit),
                        cursor: None,
                    }
                }
            }
        }
    };
    Ok(TextQuery {
        query: GraphQuery {
            op,
            projection,
            explain: false,
        },
        max_hops,
        max_output_nodes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(text: &str) -> TextQuery {
        parse(text, &Properties::new()).unwrap_or_else(|error| panic!("{text}: {error}"))
    }

    fn kind(text: &str) -> (LanguageErrorKind, usize) {
        let error = parse(text, &Properties::new()).unwrap_err();
        (error.kind, error.position)
    }

    #[test]
    fn documented_examples_parse() {
        let param = |name: &str, value: PropertyValue| {
            let mut params = Properties::new();
            params.insert(name.into(), value);
            params
        };
        for (text, params) in [
            (
                "MATCH (n:Service) WHERE n.tier >= $min RETURN n.tier LIMIT 20",
                param("min", PropertyValue::Integer(2)),
            ),
            (
                r#"MATCH (a KEY "api")<-[r:CALLS]-(b) WHERE r.weight >= 0.5 RETURN r, b"#,
                Properties::new(),
            ),
            (
                r#"MATCH p = (a KEY $k)-[:CALLS|READS*1..3]->(b:Database {region: "eu"}) RETURN b, p LIMIT 50"#,
                param("k", PropertyValue::String("api".into())),
            ),
            (
                r#"MATCH p = SHORTEST PATH (a KEY "api")-[*..4]-(b KEY "db") RETURN p"#,
                Properties::new(),
            ),
        ] {
            parse(text, &params).unwrap_or_else(|error| panic!("{text}: {error}"));
        }
    }
    #[test]
    fn node_lookup_and_find() {
        assert_eq!(
            parse_ok(r#"MATCH (n KEY "a") RETURN n"#).query.op,
            QueryOp::GetNode {
                node: NodeSelector::Key("a".into())
            }
        );
        let find = parse_ok("match (n:L {s: \"x\"}) where n.w >= 2 return n.w limit 3");
        assert_eq!(
            find.query.op,
            QueryOp::FindNodes {
                label: Some("L".into()),
                conditions: vec![
                    PropertyCondition::Eq {
                        field: "s".into(),
                        value: PropertyValue::String("x".into())
                    },
                    PropertyCondition::Range {
                        field: "w".into(),
                        min: Some(PropertyValue::Integer(2)),
                        max: None
                    },
                ],
                limit: Some(3),
                cursor: None,
            }
        );
        assert_eq!(
            find.query.projection,
            PropertyProjection::Only(["w".to_owned()].into())
        );
    }

    #[test]
    fn traversal_sets_hops_and_output_cap() {
        let query = parse_ok("MATCH p = (a ID 7)<-[:R|S*1..3]-(b:B) RETURN b, p LIMIT 5");
        assert_eq!(query.max_hops, Some(3));
        assert_eq!(query.max_output_nodes, Some(5));
        let QueryOp::Traverse {
            direction,
            include_paths,
            types,
            ..
        } = query.query.op
        else {
            panic!("traverse expected");
        };
        assert_eq!(direction, Direction::In);
        assert!(include_paths);
        assert_eq!(types, Some(["R".to_owned(), "S".to_owned()].into()));
    }

    #[test]
    fn errors_are_typed_with_offsets() {
        assert_eq!(
            kind("SEARCH v NEAR $q"),
            (LanguageErrorKind::Unsupported, 0)
        );
        assert_eq!(
            kind("MATCH (n) RETURN n;"),
            (LanguageErrorKind::Unsupported, 18)
        );
        assert_eq!(
            kind("MATCH (a KEY \"a\")-[*1..5]->(b) RETURN b").0,
            LanguageErrorKind::Limit
        );
        assert_eq!(
            kind("MATCH (n:L) WHERE n.w > 1 RETURN n"),
            (LanguageErrorKind::Unsupported, 22)
        );
        assert_eq!(kind("MATCH (n:L) RETURN $x").0, LanguageErrorKind::Syntax);
        assert_eq!(
            kind("MATCH (n KEY $k) RETURN n"),
            (LanguageErrorKind::Parameter, 13)
        );
        assert_eq!(
            kind(&"(".repeat(MAX_QUERY_TOKENS + 1)).0,
            LanguageErrorKind::Limit
        );
    }
}
