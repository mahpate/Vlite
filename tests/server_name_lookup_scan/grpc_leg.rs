//! The gRPC leg of the structural scan (lane F3).
//!
//! `Adapter::call` is the one pipeline of the gRPC transport: admission,
//! authentication, rate limit, static scope policy, the core inside the
//! hidden-attempt tracker, and the request record. The by-name scan of the
//! parent module sees every lookup of a handler, but not an RPC that skips the
//! pipeline and reaches the engine by an ID, so the structure of
//! `src/server/grpc.rs` and `src/server/grpc/*.rs` is checked here, over every
//! `impl` of `Adapter` and every function of those files, at any depth of
//! nesting (a module, a function body):
//!
//! * every RPC of every service has exactly one handler, a member of
//!   `impl <Service> for Adapter`; a second block that declares it again, an
//!   `impl` of a service the check does not know and an `impl` of a known
//!   service for another type fail;
//! * each handler makes exactly one call through the pipeline (itself, or
//!   through an inherent method that makes exactly one) under the route label
//!   of its own RPC, read from code (a route in a comment counts for nothing),
//!   and names no other route;
//! * outside that call a handler takes nothing of the pipeline or the engine:
//!   no `.run*(..)`, `.scope(..)`, `database`, `Sidecar`, `*_core`, use of an
//!   engine helper, pipeline step, `RequestScope::..`, `Role::..` (a handler
//!   never mints a role) or `track_hidden`; inside it, a handler repeats no
//!   pipeline step and mints nothing either;
//! * an engine helper is a function that is neither a handler nor a runner and
//!   takes a `*_core` itself (`apply_chunk`): only a pipeline call or a runner
//!   may call it, and some pipeline call or runner must;
//! * the only other functions that take engine access or a pipeline step are
//!   the continuations of the two streams (`GRPC_RUNNERS`), which run after the
//!   opening call and so authenticate again themselves: each must take every
//!   step listed for it as a call at the top level of its body, use the result
//!   and act for the role it authenticated;
//! * no `use` renames a name the scan reads (`put_core as store`) or brings a
//!   variant of `Role` into scope.
//!
//! The check reads names, not types: it does not see code outside these files,
//! the body of a macro, or a call through a function value defined elsewhere.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use super::{Token, blank, blank_with, matching_brace, server_files, without_test_items};

/// The services the gRPC adapter implements, by the name the proto gives them.
const GRPC_SERVICES: &[&str] = &["VLite", "Kv"];

/// Functions of the gRPC files, other than handlers, that may run engine code,
/// each with the steps its body must take itself: authenticate again (so
/// revocation applies mid-stream) and take its view of the database from the
/// request scope. A step counts as taken when the body calls it at its top
/// level (not in a closure or a branch), uses its result (`?`, a binding or a
/// match, never `let _ =`) and, after `authenticate`, passes the role that call
/// returned.
const GRPC_RUNNERS: &[(&str, &[&str])] = &[
    (
        "poll_changes",
        &["authenticate", "rate_limit", "authorize_need", "scope"],
    ),
    ("export_page", &["authenticate", "scope"]),
];

/// Pipeline steps that only `Adapter::call` (and the runners above) take.
const GRPC_PIPELINE: &[&str] = &[
    "authenticate",
    "authorize_need",
    "rate_limit",
    "record_request",
];

fn ident_at(tokens: &[(Token, usize)], at: usize) -> Option<&str> {
    match tokens.get(at) {
        Some((Token::Ident(word), _)) => Some(word.as_str()),
        _ => None,
    }
}

fn punct_at(tokens: &[(Token, usize)], at: usize, c: char) -> bool {
    matches!(tokens.get(at), Some((Token::Punct(found), _)) if *found == c)
}

/// A token with the byte offset where it starts; numbers are skipped.
fn spanned_tokens(text: &str, base: usize) -> Vec<(Token, usize)> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c.is_ascii_alphabetic() || c == '_' {
            let mut ident = String::from(c);
            while let Some(&(_, next)) = chars.peek() {
                if next.is_ascii_alphanumeric() || next == '_' {
                    ident.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push((Token::Ident(ident), base + at));
        } else if c.is_ascii_digit() {
            while chars
                .peek()
                .is_some_and(|&(_, next)| next.is_ascii_alphanumeric() || next == '_')
            {
                chars.next();
            }
        } else if !c.is_whitespace() {
            out.push((Token::Punct(c), base + at));
        }
    }
    out
}

/// Index of the `)` matching the `(` at `open` (or the end of the text).
fn matching_paren(text: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    for (offset, &byte) in text[open..].iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return open + offset;
                }
            }
            _ => {}
        }
    }
    text.len() - 1
}

// ------------------------------------------------------------------ parsing

/// Where a function is declared.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Home {
    /// A member of an `impl` block of `Adapter`: of the trait named, or of the
    /// inherent block (`None`).
    Member(Option<String>),
    /// A free function, or one nested in a function or a module.
    Free,
}

/// A function and the byte offsets of the braces of its body.
struct GrpcFn {
    name: String,
    open: usize,
    close: usize,
    home: Home,
}

struct GrpcImpl {
    /// The trait implemented (the last segment of its path), `None` for an
    /// inherent block.
    service: Option<String>,
    /// The identifiers of the type the block is for.
    ty: Vec<String>,
}

impl GrpcImpl {
    /// `Adapter`, spelled any way (`crate::server::grpc::Adapter`, `&Adapter`).
    fn for_adapter(&self) -> bool {
        self.ty.iter().any(|word| word == "Adapter")
    }
}

/// Index of the token that opens the body of the item whose tokens start at
/// `from`: the first `{` outside parentheses and brackets, unless a `;` comes
/// first (a declaration without a body).
fn body_open(tokens: &[(Token, usize)], from: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, (token, _)) in tokens.iter().enumerate().skip(from) {
        match token {
            Token::Punct('(' | '[') => depth += 1,
            Token::Punct(')' | ']') => depth = depth.saturating_sub(1),
            Token::Punct('{') if depth == 0 => return Some(index),
            Token::Punct(';') if depth == 0 => return None,
            _ => {}
        }
    }
    None
}

/// Whether the `>` at `at` closes a generic list (and is not the end of `->`).
fn closes_generics(tokens: &[(Token, usize)], at: usize) -> bool {
    punct_at(tokens, at, '>') && !punct_at(tokens, at.wrapping_sub(1), '-')
}

/// The trait and the type identifiers of an `impl` head: the tokens between
/// `impl` and the `{` of its block.
fn impl_head(head: &[(Token, usize)]) -> (Option<String>, Vec<String>) {
    let mut rest = head;
    if punct_at(rest, 0, '<') {
        // The generics of the block itself: `impl<T: Bound> ..`.
        let mut depth = 0usize;
        for index in 0..rest.len() {
            if punct_at(rest, index, '<') {
                depth += 1;
            } else if closes_generics(rest, index) {
                depth -= 1;
                if depth == 0 {
                    rest = &rest[index + 1..];
                    break;
                }
            }
        }
    }
    let end = rest
        .iter()
        .position(|(token, _)| matches!(token, Token::Ident(word) if word == "where"))
        .unwrap_or(rest.len());
    let rest = &rest[..end];
    let mut depth = 0usize;
    let mut at_for = None;
    for index in 0..rest.len() {
        if punct_at(rest, index, '<') {
            depth += 1;
        } else if closes_generics(rest, index) {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && ident_at(rest, index) == Some("for") {
            at_for = Some(index);
            break;
        }
    }
    let idents = |tokens: &[(Token, usize)]| -> Vec<String> {
        tokens
            .iter()
            .filter_map(|(token, _)| match token {
                Token::Ident(word) => Some(word.clone()),
                Token::Punct(_) => None,
            })
            .collect()
    };
    let Some(at_for) = at_for else {
        return (None, idents(rest));
    };
    // The trait is the last identifier of its path outside generic arguments.
    let mut depth = 0usize;
    let mut service = None;
    for index in 0..at_for {
        if punct_at(rest, index, '<') {
            depth += 1;
        } else if closes_generics(rest, index) {
            depth = depth.saturating_sub(1);
        } else if depth == 0
            && let Some(word) = ident_at(rest, index)
        {
            service = Some(word.to_owned());
        }
    }
    (service, idents(&rest[at_for + 1..]))
}

/// A function found by `grpc_parse`, before it is placed in its home.
struct Declared {
    name: String,
    /// Offset of the `fn` keyword.
    start: usize,
    open: usize,
    close: usize,
}

/// An `impl` block: the offsets of its braces and its index in the list of
/// impls.
struct Block {
    open: usize,
    close: usize,
    index: usize,
}

/// The `impl` blocks and the functions of blanked code, at any depth: every
/// function that is a member of an `impl` of `Adapter`, or free (also one
/// nested in a function or a module). Members of the `impl` of another type
/// are not read.
fn grpc_parse(code: &str) -> (Vec<GrpcImpl>, Vec<GrpcFn>) {
    let bytes = code.as_bytes();
    let tokens = spanned_tokens(code, 0);
    let mut impls = Vec::new();
    let mut blocks = Vec::new();
    let mut declared = Vec::new();
    for (index, (token, at)) in tokens.iter().enumerate() {
        let Token::Ident(word) = token else {
            continue;
        };
        match word.as_str() {
            "impl" => {
                // An item, not `impl Trait` in the type of an argument or a
                // return value.
                let item = index == 0
                    || [';', '{', '}', ']']
                        .iter()
                        .any(|c| punct_at(&tokens, index - 1, *c))
                    || ident_at(&tokens, index - 1) == Some("unsafe");
                if item && let Some(open_index) = body_open(&tokens, index + 1) {
                    let (service, ty) = impl_head(&tokens[index + 1..open_index]);
                    let open = tokens[open_index].1;
                    blocks.push(Block {
                        open,
                        close: matching_brace(bytes, open),
                        index: impls.len(),
                    });
                    impls.push(GrpcImpl { service, ty });
                }
            }
            "fn" => {
                if let Some(name) = ident_at(&tokens, index + 1)
                    && let Some(open_index) = body_open(&tokens, index + 2)
                {
                    let open = tokens[open_index].1;
                    declared.push(Declared {
                        name: name.to_owned(),
                        start: *at,
                        open,
                        close: matching_brace(bytes, open),
                    });
                }
            }
            _ => {}
        }
    }
    let mut functions = Vec::new();
    for function in &declared {
        let block = blocks
            .iter()
            .filter(|block| block.open < function.start && function.start < block.close)
            .max_by_key(|block| block.open);
        let enclosing = declared
            .iter()
            .filter(|other| other.open < function.start && function.start < other.close)
            .map(|other| other.open)
            .max();
        let home = match block {
            None => Home::Free,
            Some(block) => {
                let implementation = &impls[block.index];
                if !implementation.for_adapter() {
                    continue;
                }
                // A function nested in a method is no member of the block.
                if enclosing.is_some_and(|open| open > block.open) {
                    Home::Free
                } else {
                    Home::Member(implementation.service.clone())
                }
            }
        };
        functions.push(GrpcFn {
            name: function.name.clone(),
            open: function.open,
            close: function.close,
            home,
        });
    }
    (impls, functions)
}

/// `(method, open, close)` of every `self.method(..)` call in a body, with
/// the offsets of its parentheses.
fn grpc_calls(code: &str, function: &GrpcFn) -> Vec<(String, usize, usize)> {
    let tokens = spanned_tokens(&code[function.open..=function.close], function.open);
    let mut out = Vec::new();
    for index in 0..tokens.len() {
        if let (
            Token::Ident(receiver),
            Some((Token::Punct('.'), _)),
            Some((Token::Ident(method), _)),
            Some((Token::Punct('('), open)),
        ) = (
            &tokens[index].0,
            tokens.get(index + 1),
            tokens.get(index + 2),
            tokens.get(index + 3),
        ) && receiver == "self"
        {
            out.push((
                method.clone(),
                *open,
                matching_paren(code.as_bytes(), *open),
            ));
        }
    }
    out
}

/// The parentheses of the `self.call(..)` calls of a body: the pipeline calls.
fn grpc_pipeline_ranges(code: &str, function: &GrpcFn) -> Vec<(usize, usize)> {
    grpc_calls(code, function)
        .into_iter()
        .filter(|(method, ..)| method == "call")
        .map(|(_, open, close)| (open, close))
        .collect()
}

/// Whether `at` lies inside one of `ranges`.
fn within(ranges: &[(usize, usize)], at: usize) -> bool {
    ranges.iter().any(|(from, to)| (*from..=*to).contains(&at))
}

// -------------------------------------------------------- what a body takes

/// What a body takes that belongs to the pipeline or the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GrpcUse {
    /// A pipeline step: `authenticate`, `rate_limit`, `authorize_need`,
    /// `record_request`, called as a function or a method, or named as a
    /// value.
    Step,
    /// What only the pipeline and the guard create: `track_hidden`, a
    /// `RequestScope::..` and a `Role::..` (a role is minted by
    /// authentication, never by a handler).
    Mint,
    /// Direct engine access: `.run*(..)`, `.scope(..)`, the `database` field,
    /// a `Sidecar`.
    Engine,
    /// A transport-neutral core, `*_core`, which acts for the role the
    /// pipeline authenticated.
    Core,
    /// A call or a mention of an engine helper (see the module
    /// documentation).
    Helper,
}

struct GrpcWord {
    reach: GrpcUse,
    text: String,
    at: usize,
}

/// Every word of a body that belongs to the pipeline or the engine, with its
/// offset. `helpers` are the engine helpers, whose uses are reported too.
fn grpc_words(code: &str, function: &GrpcFn, helpers: &BTreeSet<String>) -> Vec<GrpcWord> {
    let tokens = spanned_tokens(&code[function.open..=function.close], function.open);
    let mut out = Vec::new();
    for (index, (token, at)) in tokens.iter().enumerate() {
        let Token::Ident(word) = token else {
            continue;
        };
        // A nested definition is no use.
        if ident_at(&tokens, index.wrapping_sub(1)) == Some("fn") {
            continue;
        }
        let after_dot = punct_at(&tokens, index.wrapping_sub(1), '.');
        let after_path = punct_at(&tokens, index.wrapping_sub(1), ':')
            && punct_at(&tokens, index.wrapping_sub(2), ':');
        let called = punct_at(&tokens, index + 1, '(');
        let pathed = punct_at(&tokens, index + 1, ':') && punct_at(&tokens, index + 2, ':');
        let (reach, text) = if GRPC_PIPELINE.contains(&word.as_str()) && (called || !after_dot) {
            // A call, or a reference that could be called later.
            (GrpcUse::Step, word.clone())
        } else if word == "track_hidden" {
            (GrpcUse::Mint, word.clone())
        } else if pathed && matches!(word.as_str(), "RequestScope" | "Role") {
            let tail = ident_at(&tokens, index + 3).unwrap_or_default();
            (GrpcUse::Mint, format!("{word}::{tail}"))
        } else if word == "Sidecar"
            || (after_dot && word == "database")
            || ((after_dot || after_path) && called && (word.starts_with("run") || word == "scope"))
        {
            (GrpcUse::Engine, word.clone())
        } else if word.ends_with("_core") {
            (GrpcUse::Core, word.clone())
        } else if *word != function.name
            && helpers.contains(word.as_str())
            && (called || !after_dot)
        {
            // A call, or a reference that could be called later.
            (GrpcUse::Helper, word.clone())
        } else {
            continue;
        };
        out.push(GrpcWord {
            reach,
            text,
            at: *at,
        });
    }
    out
}

// ------------------------------------------------------------- route labels

/// The route labels a body spells, read from the comment-free source: a route
/// in a comment is no label.
fn grpc_labels(labels: &str, function: &GrpcFn) -> Vec<String> {
    let body = &labels[function.open..=function.close];
    body.match_indices("\"grpc:/vlite.v1.")
        .filter_map(|(at, _)| {
            let rest = &body[at + 1..];
            rest.find('"').map(|end| rest[..end].to_owned())
        })
        .collect()
}

/// The route a call is opened under: the literal it spells first, or the one
/// of the `const` of the handler that it names. The `const` is looked up in
/// code (`code` has strings and comments blanked), so neither a comment nor a
/// string can declare it; its literal is read from `labels`.
fn grpc_opening_label(
    code: &str,
    labels: &str,
    function: &GrpcFn,
    open: usize,
    close: usize,
) -> Option<String> {
    let first = labels[open + 1..close].trim_start();
    let literal = |text: &str| text.strip_prefix('"')?.split('"').next().map(str::to_owned);
    if first.starts_with('"') {
        return literal(first);
    }
    let name: String = first
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let declaration = format!("const {name}: &str = ");
    let at = function.open + code[function.open..=function.close].find(&declaration)?;
    literal(&labels[at + declaration.len()..])
}

fn grpc_camel(snake: &str) -> String {
    snake
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect()
}

// ------------------------------------------------------------------ runners

/// A call of a step in the body of a runner.
struct StepCall {
    /// Index of the step's token in the body.
    index: usize,
    /// Whether the call's result is used: propagated with `?`, bound to a
    /// name that does not start with `_`, or matched.
    used: bool,
    /// The names the statement binds.
    bound: Vec<String>,
    /// The identifiers of the call's arguments.
    args: BTreeSet<String>,
}

/// Whether the result of a call is used, and the names it is bound to, given
/// the tokens of its statement before it (`lead`) and whether a `?` propagates
/// it. A call that is part of a closure, a block or a larger expression is
/// not "used" here: it has to be the statement, the start of the initializer
/// of a `let`, or the scrutinee of a `match`.
fn grpc_used(lead: &[(Token, usize)], propagated: bool) -> (bool, Vec<String>) {
    let plain = |tokens: &[(Token, usize)]| {
        tokens.iter().all(|(token, _)| match token {
            Token::Ident(word) => !matches!(
                word.as_str(),
                "return"
                    | "break"
                    | "async"
                    | "move"
                    | "else"
                    | "loop"
                    | "unsafe"
                    | "for"
                    | "in"
                    | "let"
                    | "if"
                    | "while"
            ),
            Token::Punct(c) => matches!(*c, '.' | ':' | '&' | '*'),
        })
    };
    match ident_at(lead, 0) {
        Some("let") => {
            let Some(equals) = lead
                .iter()
                .position(|(token, _)| matches!(token, Token::Punct('=')))
            else {
                return (false, Vec::new());
            };
            if !plain(&lead[equals + 1..]) {
                return (false, Vec::new());
            }
            let names: Vec<String> = lead[1..equals]
                .iter()
                .filter_map(|(token, _)| match token {
                    Token::Ident(word)
                        if word.starts_with(|c: char| c.is_ascii_lowercase())
                            && !matches!(word.as_str(), "mut" | "ref") =>
                    {
                        Some(word.clone())
                    }
                    _ => None,
                })
                .collect();
            (propagated || !names.is_empty(), names)
        }
        Some("match" | "if" | "while") => (true, Vec::new()),
        _ => (plain(lead) && propagated, Vec::new()),
    }
}

/// What a runner must take itself, as problems: every step of `required` as a
/// call at the top level of its body whose result is used, and, after
/// `authenticate`, with the role that call returned among its arguments.
fn grpc_runner_problems(
    file: &str,
    code: &str,
    function: &GrpcFn,
    required: &[&str],
) -> Vec<String> {
    let tokens = spanned_tokens(&code[function.open..=function.close], function.open);
    // Brace and bracket depth before each token; the body's own brace makes
    // depth 1.
    let mut braces = Vec::with_capacity(tokens.len());
    let mut brackets = Vec::with_capacity(tokens.len());
    let (mut brace, mut bracket) = (0usize, 0usize);
    for (token, _) in &tokens {
        braces.push(brace);
        brackets.push(bracket);
        match token {
            Token::Punct('{') => brace += 1,
            Token::Punct('}') => brace = brace.saturating_sub(1),
            Token::Punct('(' | '[') => bracket += 1,
            Token::Punct(')' | ']') => bracket = bracket.saturating_sub(1),
            _ => {}
        }
    }
    // The first token of the statement each token belongs to. A statement of
    // the body ends at a `;` or at the `}` of a block, outside parentheses.
    let mut statement = vec![1usize; tokens.len()];
    let mut current = 1;
    for (index, slot) in statement.iter_mut().enumerate().skip(2) {
        let before = index - 1;
        let ends = match &tokens[before].0 {
            Token::Punct(';') => braces[before] == 1 && brackets[before] == 0,
            Token::Punct('}') => {
                braces[before] == 2
                    && brackets[before] == 0
                    && ident_at(&tokens, index) != Some("else")
                    && !punct_at(&tokens, index, '.')
                    && !punct_at(&tokens, index, '?')
            }
            _ => false,
        };
        if ends {
            current = index;
        }
        *slot = current;
    }
    let calls_of = |step: &str| -> Vec<StepCall> {
        let mut out = Vec::new();
        for (index, (token, _)) in tokens.iter().enumerate() {
            if !matches!(token, Token::Ident(word) if word == step)
                || braces[index] != 1
                || !punct_at(&tokens, index + 1, '(')
                || ident_at(&tokens, index.wrapping_sub(1)) == Some("fn")
            {
                continue;
            }
            let close = matching_paren(code.as_bytes(), tokens[index + 1].1);
            let after = tokens
                .iter()
                .position(|(_, at)| *at > close)
                .unwrap_or(tokens.len());
            let args = tokens[index + 2..after]
                .iter()
                .filter_map(|(token, _)| match token {
                    Token::Ident(word) => Some(word.clone()),
                    Token::Punct(_) => None,
                })
                .collect();
            let end = (after..tokens.len())
                .find(|&at| statement[at] != statement[index])
                .unwrap_or(tokens.len());
            let propagated = punct_at(&tokens, after, '?')
                || (after..end)
                    .any(|at| punct_at(&tokens, at, '?') && braces[at] == 1 && brackets[at] == 0);
            let (used, bound) = grpc_used(&tokens[statement[index]..index], propagated);
            out.push(StepCall {
                index,
                used,
                bound,
                args,
            });
        }
        out
    };
    let used_calls = |step: &str| -> Vec<StepCall> {
        calls_of(step)
            .into_iter()
            .filter(|call| call.used)
            .collect()
    };
    let name = &function.name;
    let missing = |step: &str| {
        let after = if step != "authenticate" && required.contains(&"authenticate") {
            ", after `authenticate`"
        } else {
            ""
        };
        format!(
            "{file}: the stream runner `{name}` must also take `{step}` itself: a call at the top \
             level of its body{after}, whose result is propagated with `?`, bound or matched, \
             not discarded"
        )
    };
    let mut problems = Vec::new();
    // The role `authenticate` returned: the position of its call and the names
    // it is bound to.
    let mut flow: Option<(usize, BTreeSet<String>)> = None;
    if required.contains(&"authenticate") {
        let calls = used_calls("authenticate");
        let names: BTreeSet<String> = calls
            .iter()
            .flat_map(|call| call.bound.iter().cloned())
            .collect();
        if calls.is_empty() {
            problems.push(missing("authenticate"));
        } else if names.is_empty() {
            problems.push(format!(
                "{file}: the stream runner `{name}` must bind the role `authenticate` returns \
                 (`let role = authenticate(..)..;`), so that the steps after it act for that role"
            ));
        } else if let Some(first) = calls.iter().map(|call| call.index).min() {
            flow = Some((first, names));
        }
    }
    for step in required.iter().filter(|step| **step != "authenticate") {
        let mut calls = used_calls(step);
        if let Some((first, names)) = &flow {
            calls.retain(|call| call.index > *first);
            if !calls.is_empty()
                && calls
                    .iter()
                    .any(|call| !names.iter().any(|name| call.args.contains(name)))
            {
                problems.push(format!(
                    "{file}: the stream runner `{name}` calls `{step}` without the role it \
                     authenticated ({}): every step after `authenticate` must act for that role",
                    names.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }
        if calls.is_empty() {
            problems.push(missing(step));
        }
    }
    problems
}

// ----------------------------------------------------------------- the scan

/// One parsed gRPC adapter file.
struct GrpcFile {
    name: String,
    /// The source with comments, strings and character literals blanked, and
    /// the test items removed.
    code: String,
    /// The same text, offset for offset, with only the comments blanked: the
    /// route labels are string literals of code.
    labels: String,
    impls: Vec<GrpcImpl>,
    functions: Vec<GrpcFn>,
}

impl GrpcFile {
    fn new(name: &str, source: &str) -> Self {
        let code = without_test_items(&blank(source));
        let (impls, functions) = grpc_parse(&code);
        Self {
            name: name.to_owned(),
            code,
            labels: blank_with(source, false),
            impls,
            functions,
        }
    }
}

/// What a function is to the scan.
enum Kind {
    /// The handler of an RPC of a service the check knows.
    Handler(String),
    /// A member of an `impl` of `Adapter` that is not a handler.
    Method,
    /// A free function.
    Free,
    /// `Adapter::call`: the pipeline itself.
    Pipeline,
}

struct Entry<'a> {
    file: &'a GrpcFile,
    function: &'a GrpcFn,
    kind: Kind,
}

#[derive(Default)]
struct GrpcScan {
    problems: Vec<String>,
    /// The RPCs the handlers of each service implement.
    handlers: BTreeMap<String, BTreeSet<String>>,
    /// The functions allowed to run engine code that the files define.
    runners: BTreeSet<String>,
    /// The engine helpers the files define.
    helpers: BTreeSet<String>,
}

fn grpc_kind(function: &GrpcFn, services: &[&str]) -> Kind {
    match &function.home {
        Home::Member(Some(service)) if services.contains(&service.as_str()) => {
            Kind::Handler(service.clone())
        }
        Home::Member(None) if function.name == "call" => Kind::Pipeline,
        Home::Member(_) => Kind::Method,
        Home::Free => Kind::Free,
    }
}

/// The problems of the `impl` blocks of a file: a service the check does not
/// know, or a known service implemented for another type than `Adapter`.
fn grpc_impl_problems(file: &GrpcFile, services: &[&str]) -> Vec<String> {
    let mut problems = Vec::new();
    for implementation in &file.impls {
        let Some(service) = &implementation.service else {
            continue;
        };
        let known = services.contains(&service.as_str());
        if implementation.for_adapter() && !known {
            problems.push(format!(
                "{}: `impl {service} for Adapter` is a service this check does not know; add it \
                 to GRPC_SERVICES, and give every RPC a handler that goes through Adapter::call",
                file.name
            ));
        } else if !implementation.for_adapter() && known {
            problems.push(format!(
                "{}: `impl {service} for {}` implements a gRPC service for a type this check \
                 does not read; implement it on Adapter, so that its handlers are checked",
                file.name,
                implementation.ty.join("::")
            ));
        }
    }
    problems
}

/// What an import could do to hide a word the scan reads: rename it, or bring
/// a variant of `Role` into scope, to name it without its path.
fn grpc_import_problems(file: &GrpcFile, helpers: &BTreeSet<String>) -> Vec<String> {
    let tokens = spanned_tokens(&file.code, 0);
    let name = &file.name;
    let mut problems = Vec::new();
    for (index, (token, _)) in tokens.iter().enumerate() {
        let Token::Ident(word) = token else {
            continue;
        };
        let read = GRPC_PIPELINE.contains(&word.as_str())
            || matches!(
                word.as_str(),
                "track_hidden" | "Role" | "RequestScope" | "Sidecar"
            )
            || word.ends_with("_core")
            || helpers.contains(word.as_str());
        if read && ident_at(&tokens, index + 1) == Some("as") {
            problems.push(format!(
                "{name}: `{word}` is renamed with `as`; the scan reads names, so a renamed \
                 pipeline step, core, role or engine helper would go unseen"
            ));
        }
        if word == "use" {
            let end = (index..tokens.len())
                .find(|&at| punct_at(&tokens, at, ';'))
                .unwrap_or(tokens.len());
            let imports_a_role = (index..end).any(|at| {
                ident_at(&tokens, at) == Some("Role")
                    && punct_at(&tokens, at + 1, ':')
                    && punct_at(&tokens, at + 2, ':')
            });
            if imports_a_role {
                problems.push(format!(
                    "{name}: a `use` brings a variant of `Role` into scope; a role is minted by \
                     authentication, so the adapter never names one"
                ));
            }
        }
    }
    problems
}

/// How many pipeline calls a function makes itself, and how many it delegates
/// to an inherent method of `Adapter`.
fn grpc_pipeline_count(entry: &Entry, inherent: &BTreeMap<&str, &Entry>) -> (usize, usize) {
    let calls = grpc_calls(&entry.file.code, entry.function);
    let direct = calls.iter().filter(|(method, ..)| method == "call").count();
    let delegated = calls
        .iter()
        .filter(|(method, ..)| method != "call" && inherent.contains_key(method.as_str()))
        .count();
    (direct, delegated)
}

/// The problems of one handler: its calls through the pipeline, its route, and
/// what it takes around and inside them.
fn grpc_handler_problems(
    entry: &Entry,
    service: &str,
    inherent: &BTreeMap<&str, &Entry>,
    helpers: &BTreeSet<String>,
    called: &mut BTreeSet<String>,
) -> Vec<String> {
    let file = entry.file.name.as_str();
    let (code, labels) = (entry.file.code.as_str(), entry.file.labels.as_str());
    let handler = entry.function;
    let own = format!("grpc:/vlite.v1.{service}/{}", grpc_camel(&handler.name));
    let calls = grpc_calls(code, handler);
    let mut problems = Vec::new();
    let mut skip = Vec::new();
    match grpc_pipeline_count(entry, inherent) {
        (1, 0) => {
            // A stream may repeat its own route for its continuation's
            // failure record, but never another handler's.
            let spelled = grpc_labels(labels, handler);
            let opening =
                calls
                    .iter()
                    .find(|(method, ..)| method == "call")
                    .and_then(|(_, open, close)| {
                        grpc_opening_label(code, labels, handler, *open, *close)
                    });
            if opening.as_deref() != Some(own.as_str()) || spelled.iter().any(|label| *label != own)
            {
                problems.push(format!(
                    "{file}: handler `{}` must open its call under its own route `{own}` and name \
                     no other route, found {spelled:?}",
                    handler.name
                ));
            }
            skip.extend(
                calls
                    .iter()
                    .filter(|(method, ..)| method == "call")
                    .map(|(_, open, close)| (*open, *close)),
            );
        }
        (0, 1) => {
            if let Some((method, open, close)) = calls
                .iter()
                .find(|(method, ..)| inherent.contains_key(method.as_str()))
            {
                let target = inherent[method.as_str()];
                if grpc_pipeline_count(target, inherent) != (1, 0) {
                    problems.push(format!(
                        "{file}: `{method}`, which handler `{}` delegates to, must make exactly \
                         one call through the pipeline",
                        handler.name
                    ));
                }
                if !grpc_labels(&target.file.labels, target.function).contains(&own) {
                    problems.push(format!(
                        "{file}: `{method}` must name the route `{own}` of the handler `{}` that \
                         delegates to it",
                        handler.name
                    ));
                }
                skip.push((*open, *close));
            }
        }
        (0, 0) => problems.push(format!(
            "{file}: handler `{}` makes no call through the pipeline (`self.call(..)`): it would \
             answer without authentication, scope policy or a request record",
            handler.name
        )),
        (direct, delegated) => problems.push(format!(
            "{file}: handler `{}` must make exactly one call through the pipeline, found \
             {direct} direct and {delegated} delegated",
            handler.name
        )),
    }
    let name = &handler.name;
    for word in grpc_words(code, handler, helpers) {
        let text = &word.text;
        match (word.reach, within(&skip, word.at)) {
            (GrpcUse::Step, true) => problems.push(format!(
                "{file}: handler `{name}` repeats the pipeline step `{text}` inside its pipeline \
                 call; Adapter::call takes each step once, around the core"
            )),
            (GrpcUse::Mint, true) => problems.push(format!(
                "{file}: handler `{name}` takes `{text}` inside its pipeline call; only the \
                 pipeline mints a role, a scope or the hidden-attempt tracker"
            )),
            (GrpcUse::Helper, true) => {
                called.insert(word.text.clone());
            }
            (GrpcUse::Helper, false) => problems.push(format!(
                "{file}: handler `{name}` uses the engine helper `{text}` outside its pipeline \
                 call; an engine helper runs only inside the core of a pipeline call"
            )),
            (_, false) => problems.push(format!(
                "{file}: handler `{name}` takes `{text}` outside its pipeline call; engine \
                 access and pipeline steps belong to Adapter::call and its core closure"
            )),
            (GrpcUse::Engine | GrpcUse::Core, true) => {}
        }
    }
    problems
}

/// The problems of a function that is no handler: a runner must do its part
/// itself; any other function takes no engine access and no pipeline step, and
/// calls no engine helper, except inside a pipeline call of its own.
fn grpc_helper_problems(
    entry: &Entry,
    runners: &[(&str, &[&str])],
    helpers: &BTreeSet<String>,
    called: &mut BTreeSet<String>,
    ran: &mut BTreeSet<String>,
) -> Vec<String> {
    let file = entry.file.name.as_str();
    let code = entry.file.code.as_str();
    let function = entry.function;
    let name = &function.name;
    let words = grpc_words(code, function, helpers);
    let mut problems = Vec::new();
    if let Some((_, required)) = runners.iter().find(|(runner, _)| runner == name) {
        if !words.is_empty() {
            ran.insert(name.clone());
        }
        for word in &words {
            let text = &word.text;
            match word.reach {
                GrpcUse::Mint => problems.push(format!(
                    "{file}: the stream runner `{name}` takes `{text}`; a runner acts for the \
                     role `authenticate` returned and never mints a role, a scope or the \
                     hidden-attempt tracker"
                )),
                GrpcUse::Helper => {
                    called.insert(word.text.clone());
                }
                GrpcUse::Step | GrpcUse::Engine | GrpcUse::Core => {}
            }
        }
        problems.extend(grpc_runner_problems(file, code, function, required));
        return problems;
    }
    let own = grpc_pipeline_ranges(code, function);
    let mut taken = None;
    for word in &words {
        let text = &word.text;
        match (word.reach, within(&own, word.at)) {
            (GrpcUse::Step, true) => problems.push(format!(
                "{file}: `{name}` repeats the pipeline step `{text}` inside its pipeline call; \
                 Adapter::call takes each step once, around the core"
            )),
            (GrpcUse::Mint, true) => problems.push(format!(
                "{file}: `{name}` takes `{text}` inside its pipeline call; only the pipeline \
                 mints a role, a scope or the hidden-attempt tracker"
            )),
            (GrpcUse::Helper, true) => {
                called.insert(word.text.clone());
            }
            (GrpcUse::Helper, false) => problems.push(format!(
                "{file}: `{name}` uses the engine helper `{text}` outside a pipeline call; an \
                 engine helper runs only inside the core of a pipeline call"
            )),
            (GrpcUse::Step | GrpcUse::Mint | GrpcUse::Engine, false) => {
                taken = taken.or(Some(text.clone()));
            }
            // A function that takes a core outside a pipeline call of its own
            // is an engine helper; one that also makes a pipeline call is not.
            (GrpcUse::Core, false) if !own.is_empty() => taken = taken.or(Some(text.clone())),
            _ => {}
        }
    }
    if let Some(text) = taken {
        problems.push(format!(
            "{file}: `{name}` takes `{text}` and is neither a handler nor one of GRPC_RUNNERS; \
             engine access and pipeline steps belong to Adapter::call"
        ));
    }
    problems
}

/// Every structural problem of the gRPC adapter files `(name, source)`.
fn grpc_structure_files(
    files: &[(&str, &str)],
    services: &[&str],
    runners: &[(&str, &[&str])],
) -> GrpcScan {
    let parsed: Vec<GrpcFile> = files
        .iter()
        .map(|(name, source)| GrpcFile::new(name, source))
        .collect();
    let mut scan = GrpcScan::default();
    for file in &parsed {
        scan.problems.extend(grpc_impl_problems(file, services));
    }
    let entries: Vec<Entry> = parsed
        .iter()
        .flat_map(|file| {
            file.functions.iter().map(move |function| Entry {
                file,
                function,
                kind: grpc_kind(function, services),
            })
        })
        .collect();
    // Every RPC has one handler, in one block.
    let mut declared: BTreeMap<(String, String), Vec<&str>> = BTreeMap::new();
    for entry in &entries {
        if let Kind::Handler(service) = &entry.kind {
            let rpc = grpc_camel(&entry.function.name);
            declared
                .entry((service.clone(), rpc.clone()))
                .or_default()
                .push(entry.file.name.as_str());
            scan.handlers
                .entry(service.clone())
                .or_default()
                .insert(rpc);
        }
    }
    for ((service, rpc), places) in &declared {
        if places.len() > 1 {
            scan.problems.push(format!(
                "{service}/{rpc} has more than one handler ({}); a second `impl {service} for \
                 Adapter` block must not declare it again",
                places.join(", ")
            ));
        }
    }
    let inherent: BTreeMap<&str, &Entry> = entries
        .iter()
        .filter(|entry| entry.function.home == Home::Member(None))
        .filter(|entry| !matches!(entry.kind, Kind::Pipeline))
        .map(|entry| (entry.function.name.as_str(), entry))
        .collect();
    // The engine helpers: functions that are no handler, no runner and make no
    // pipeline call of their own, but take a core.
    let none = BTreeSet::new();
    let helpers: BTreeSet<String> = entries
        .iter()
        .filter(|entry| matches!(entry.kind, Kind::Method | Kind::Free))
        .filter(|entry| !runners.iter().any(|(name, _)| *name == entry.function.name))
        .filter(|entry| grpc_pipeline_ranges(&entry.file.code, entry.function).is_empty())
        .filter(|entry| {
            grpc_words(&entry.file.code, entry.function, &none)
                .iter()
                .any(|word| word.reach == GrpcUse::Core)
        })
        .map(|entry| entry.function.name.clone())
        .collect();
    for file in &parsed {
        scan.problems.extend(grpc_import_problems(file, &helpers));
    }
    let mut called = BTreeSet::new();
    for entry in &entries {
        match &entry.kind {
            Kind::Pipeline => {}
            Kind::Handler(service) => scan.problems.extend(grpc_handler_problems(
                entry,
                service,
                &inherent,
                &helpers,
                &mut called,
            )),
            Kind::Method | Kind::Free => scan.problems.extend(grpc_helper_problems(
                entry,
                runners,
                &helpers,
                &mut called,
                &mut scan.runners,
            )),
        }
    }
    for helper in &helpers {
        if !called.contains(helper) {
            scan.problems.push(format!(
                "the engine helper `{helper}` is never called from inside a pipeline call or by a \
                 runner; a function that takes a core must be reached only through the pipeline"
            ));
        }
    }
    scan.helpers = helpers;
    scan
}

/// Every structural problem of one gRPC adapter file.
fn grpc_structure(
    file: &str,
    source: &str,
    services: &[&str],
    runners: &[(&str, &[&str])],
) -> GrpcScan {
    grpc_structure_files(&[(file, source)], services, runners)
}

/// The RPCs of every service of the proto, one `rpc` line each.
fn proto_services(root: &Path) -> BTreeMap<String, BTreeSet<String>> {
    let text = fs::read_to_string(root.join("proto/vlite/v1/vlite.proto")).unwrap();
    let mut services: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or_default().trim();
        if let Some(rest) = line.strip_prefix("service ") {
            let name = rest.trim_end_matches('{').trim().to_owned();
            services.entry(name.clone()).or_default();
            current = Some(name);
        } else if line.starts_with('}') {
            current = None;
        } else if let (Some(service), Some(rest)) = (&current, line.strip_prefix("rpc ")) {
            let name = rest.split('(').next().unwrap_or_default().trim().to_owned();
            services.get_mut(service).unwrap().insert(name);
        }
    }
    services
}

#[test]
fn every_grpc_rpc_goes_through_the_adapter_pipeline() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files: Vec<(String, String)> = server_files(root)
        .into_iter()
        .filter(|(relative, _)| {
            relative == "src/server/grpc.rs" || relative.starts_with("src/server/grpc/")
        })
        .map(|(relative, path)| (relative, fs::read_to_string(path).unwrap()))
        .collect();
    assert!(
        files.iter().any(|(file, _)| file == "src/server/grpc.rs")
            && files
                .iter()
                .any(|(file, _)| file == "src/server/grpc/kv.rs"),
        "the check must read the adapters, read {:?}",
        files.iter().map(|(file, _)| file).collect::<Vec<_>>()
    );
    let sources: Vec<(&str, &str)> = files
        .iter()
        .map(|(file, source)| (file.as_str(), source.as_str()))
        .collect();
    let scan = grpc_structure_files(&sources, GRPC_SERVICES, GRPC_RUNNERS);
    let mut problems = scan.problems;
    let handlers = scan.handlers;
    let runners = scan.runners;
    let declared = proto_services(root);
    for service in GRPC_SERVICES {
        assert!(
            declared.contains_key(*service),
            "{service} is in GRPC_SERVICES but not in the proto"
        );
    }
    for (service, rpcs) in &declared {
        let implemented = handlers.get(service).cloned().unwrap_or_default();
        for rpc in rpcs.difference(&implemented) {
            problems.push(format!(
                "{service}/{rpc} is in the proto, but the adapter has no handler the check can read \
                 (is `{service}` in GRPC_SERVICES?)"
            ));
        }
        for rpc in implemented.difference(rpcs) {
            problems.push(format!(
                "{service}/{rpc} has a handler but is not in the proto"
            ));
        }
    }
    for (name, _) in GRPC_RUNNERS {
        if !runners.contains(*name) {
            problems.push(format!(
                "stale GRPC_RUNNERS entry `{name}`: no such function takes engine access or a pipeline step"
            ));
        }
    }
    assert!(
        declared.values().map(BTreeSet::len).sum::<usize>() >= 39,
        "the proto reader found too few RPCs: {declared:?}"
    );
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

// ---------------------------------------------------------------- fixtures

const GRPC_GOOD: &str = r#"
    async fn get_thing(&self, request: Request<A>) -> Result<Response<B>, Status> {
        self.call(
            "grpc:/vlite.v1.VLite/GetThing",
            request,
            Need::Admin,
            true,
            false,
            |service, role, _, request| async move {
                service.run_read(move |db| Ok(db.state())).await
            },
        )
        .await
    }
"#;

fn grpc_fixture(handlers: &str, rest: &str) -> String {
    format!("#[tonic::async_trait]\nimpl VLite for Adapter {{{handlers}}}\n{rest}\n")
}

// A stream names its own route twice: to open the call, and in the failure
// record of its continuation.
const GRPC_STREAM: &str = r#"
    async fn get_thing(&self, request: Request<A>) -> Result<Response<B>, Status> {
        let opened = self
            .call(
                "grpc:/vlite.v1.VLite/GetThing",
                request,
                Need::Admin,
                true,
                false,
                |service, role, _, request| async move { Ok(request) },
            )
            .await?;
        record_stream_failure(&self.service, "grpc:/vlite.v1.VLite/GetThing");
        Ok(opened)
    }
"#;

// A handler may name its route through a constant of its own.
const GRPC_CONST: &str = r#"
    async fn get_thing(&self, request: Request<A>) -> Result<Response<B>, Status> {
        const ROUTE: &str = "grpc:/vlite.v1.VLite/GetThing";
        self.call(
            ROUTE,
            request,
            Need::Admin,
            true,
            false,
            |service, role, _, request| async move { Ok(B::default()) },
        )
        .await
    }
"#;

fn grpc_problems_of(source: &str) -> GrpcScan {
    grpc_structure(
        "fixture.rs",
        source,
        &["VLite"],
        &[("poll_changes", &["authenticate", "scope"])],
    )
}

#[test]
fn the_grpc_check_accepts_a_handler_that_goes_through_the_pipeline() {
    let scan = grpc_problems_of(&grpc_fixture(GRPC_GOOD, ""));
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert_eq!(
        scan.handlers["VLite"].iter().collect::<Vec<_>>(),
        ["GetThing"]
    );
    let scan = grpc_problems_of(&grpc_fixture(GRPC_STREAM, ""));
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    let scan = grpc_problems_of(&grpc_fixture(GRPC_CONST, ""));
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    // What is not code is not read, and test-only items are not code of the
    // adapter.
    let quiet = grpc_fixture(
        GRPC_GOOD,
        "// a.run_read(x)\n#[cfg(test)]\nfn helper(service: &Service) { service.run(|db| ()); }\n\
         fn text() -> &'static str { \"self.service.run_read(x)\" }\n",
    );
    let scan = grpc_problems_of(&quiet);
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
}

#[test]
fn the_grpc_check_finds_every_way_around_the_pipeline() {
    let handler = |body: &str| {
        format!(
            "async fn get_thing(&self, request: Request<A>) -> Result<Response<B>, Status> {{ {body} }}"
        )
    };
    let call = |label: &str| {
        format!(
            "self.call(\"{label}\", request, Need::Admin, true, false, |service, role, _, request| \
             async move {{ service.run_read(move |db| Ok(db.state())).await }}).await"
        )
    };
    let own = "grpc:/vlite.v1.VLite/GetThing";
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "no call",
            grpc_fixture(&handler("Ok(Response::new(B::default()))"), ""),
            "makes no call through the pipeline",
        ),
        (
            "two calls",
            grpc_fixture(
                &handler(&format!("let _ = {}; {}", call(own), call(own))),
                "",
            ),
            "exactly one call through the pipeline",
        ),
        (
            "engine before the call",
            grpc_fixture(
                &handler(&format!(
                    "let _ = self.service.run_read(move |db| Ok(db.state())).await; {}",
                    call(own)
                )),
                "",
            ),
            "takes `run_read` outside its pipeline call",
        ),
        (
            "database field",
            grpc_fixture(
                &handler(&format!(
                    "let db = self.service.database.read(); {}",
                    call(own)
                )),
                "",
            ),
            "takes `database` outside its pipeline call",
        ),
        (
            "a scope of its own",
            grpc_fixture(
                &handler(&format!("let s = self.service.scope(role); {}", call(own))),
                "",
            ),
            "takes `scope` outside its pipeline call",
        ),
        (
            "a pipeline step of its own",
            grpc_fixture(
                &handler(&format!(
                    "let role = authenticate(&self.service, &headers)?; {}",
                    call(own)
                )),
                "",
            ),
            "takes `authenticate` outside its pipeline call",
        ),
        (
            "another RPC's route",
            grpc_fixture(&handler(&call("grpc:/vlite.v1.VLite/OtherThing")), ""),
            "must open its call under its own route",
        ),
        (
            "a continuation that names another route",
            grpc_fixture(
                &handler(&format!(
                    "let r = {}; record_stream_failure(&self.service, \"grpc:/vlite.v1.VLite/OtherThing\"); r",
                    call(own)
                )),
                "",
            ),
            "must open its call under its own route",
        ),
        (
            "a constant route of another RPC",
            grpc_fixture(&GRPC_CONST.replace("GetThing", "OtherThing"), ""),
            "must open its call under its own route",
        ),
        (
            "a route that is neither a literal nor a constant",
            grpc_fixture(
                &handler(
                    "self.call(route, request, Need::Admin, true, false, |service, role, _, request| async move { Ok(B::default()) }).await",
                ),
                "",
            ),
            "must open its call under its own route",
        ),
        (
            "delegate without a call",
            grpc_fixture(
                &handler("self.close(request).await"),
                "impl Adapter { async fn close(&self, request: Request<A>) -> Result<Response<B>, Status> { Ok(Response::new(B::default())) } }",
            ),
            "must make exactly one call through the pipeline",
        ),
        (
            "delegate under another route",
            grpc_fixture(
                &handler("self.close(request).await"),
                &format!(
                    "impl Adapter {{ async fn close(&self, request: Request<A>) -> Result<Response<B>, Status> {{ {} }} }}",
                    call("grpc:/vlite.v1.VLite/OtherThing")
                ),
            ),
            "must name the route",
        ),
        (
            "a helper that runs the engine",
            grpc_fixture(
                GRPC_GOOD,
                "async fn sneaky(service: &Service) { service.run_read(|db| Ok(())).await }",
            ),
            "neither a handler nor one of GRPC_RUNNERS",
        ),
        (
            "an inherent helper that runs the engine",
            grpc_fixture(
                GRPC_GOOD,
                "impl Adapter { async fn sneaky(&self) { self.service.run_read(|db| Ok(())).await } }",
            ),
            "neither a handler nor one of GRPC_RUNNERS",
        ),
        (
            "a runner that does not authenticate",
            grpc_fixture(
                GRPC_GOOD,
                "async fn poll_changes(service: &Service) { let s = service.scope(role); service.run_read(|db| Ok(())).await }",
            ),
            "must also take `authenticate` itself",
        ),
        (
            "a service nobody checks",
            grpc_fixture(GRPC_GOOD, "impl Other for Adapter { async fn x(&self) {} }"),
            "does not know",
        ),
    ];
    for (what, source, expected) in cases {
        let scan = grpc_problems_of(&source);
        assert!(
            scan.problems
                .iter()
                .any(|problem| problem.contains(expected)),
            "{what}: expected a problem mentioning `{expected}`, got {:?}",
            scan.problems
        );
    }
}

#[test]
fn the_grpc_check_accepts_a_delegate_and_a_runner_that_does_its_part() {
    let source = grpc_fixture(
        "async fn get_thing(&self, request: Request<A>) -> Result<Response<B>, Status> { self.close(request, false).await }",
        "impl Adapter { async fn close(&self, request: Request<A>, abort: bool) -> Result<Response<B>, Status> {
             let route = if abort { \"grpc:/vlite.v1.VLite/AbortThing\" } else { \"grpc:/vlite.v1.VLite/GetThing\" };
             self.call(route, request, Need::Admin, false, false, |service, role, _, request| async move { Ok(B::default()) }).await } }
         async fn poll_changes(service: &Service) { let role = authenticate(service, headers)?; let s = service.scope(role); service.run_read(|db| Ok(())).await }",
    );
    let scan = grpc_problems_of(&source);
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    assert!(scan.runners.contains("poll_changes"));
}

// ------------------------------------------------- fixtures of the new rules

/// The handler of `GRPC_GOOD` with `body`.
fn grpc_handler(body: &str) -> String {
    format!(
        "async fn get_thing(&self, request: Request<A>) -> Result<Response<B>, Status> {{ {body} }}"
    )
}

const GRPC_OWN: &str = "grpc:/vlite.v1.VLite/GetThing";

/// A core that runs the engine, as the real handlers do.
const GRPC_CORE: &str = "service.run_read(move |db| Ok(db.state())).await";

/// A call through the pipeline under `label`, with `core` as its core.
fn grpc_call(label: &str, core: &str) -> String {
    format!(
        "self.call(\"{label}\", request, Need::Admin, true, false, \
         |service, role, _, request| async move {{ {core} }}).await"
    )
}

fn assert_problem(what: &str, scan: &GrpcScan, expected: &str) {
    assert!(
        scan.problems
            .iter()
            .any(|problem| problem.contains(expected)),
        "{what}: expected a problem mentioning `{expected}`, got {:?}",
        scan.problems
    );
}

fn assert_clean(what: &str, scan: &GrpcScan) {
    assert!(scan.problems.is_empty(), "{what}: {:?}", scan.problems);
}

/// `source` with the first `from` replaced by `to`. `from` must be there: a
/// fixture that changed nothing would prove nothing.
fn grpc_mutated(source: &str, from: &str, to: &str) -> String {
    assert!(source.contains(from), "`{from}` is not in: {source}");
    source.replacen(from, to, 1)
}

#[test]
fn the_comment_free_view_drops_comments_and_keeps_literals() {
    let source = concat!(
        "let a = \"see // not a comment\"; // \"grpc:/vlite.v1.VLite/InComment\"\n",
        "/* \"grpc:/vlite.v1.VLite/InBlock\" /* nested */ still a comment */ let b = '\"';\n",
        "let c = r#\"/* raw */ // raw\"#; let d: &'static str = \"x\"; let e = '/';\n",
    );
    let view = blank_with(source, false);
    assert_eq!(view.len(), source.len());
    assert_eq!(view.matches('\n').count(), source.matches('\n').count());
    for kept in [
        "\"see // not a comment\"",
        "let b = '\"';",
        "r#\"/* raw */ // raw\"#",
        "&'static str",
        "let e = '/';",
    ] {
        assert!(view.contains(kept), "`{kept}` is gone from {view}");
    }
    for dropped in ["InComment", "InBlock", "nested", "still a comment"] {
        assert!(!view.contains(dropped), "`{dropped}` is left in {view}");
    }
    // The code view of the same source has no literal left either.
    let code = blank(source);
    for gone in ["not a comment", "raw", "'/'"] {
        assert!(!code.contains(gone), "`{gone}` is left in {code}");
    }
}

#[test]
fn route_labels_are_read_from_code_not_from_comments() {
    let spelled = |body: &str| grpc_problems_of(&grpc_fixture(&grpc_handler(body), ""));
    let computed = "self.call(ROUTE, request, Need::Admin, true, false, \
                    |service, role, _, request| async move { Ok(B::default()) }).await";
    // The route of the handler is only in a comment, next to a call that opens
    // under a route it computes.
    for comment in [
        "// const ROUTE: &str = \"grpc:/vlite.v1.VLite/GetThing\";",
        "/// const ROUTE: &str = \"grpc:/vlite.v1.VLite/GetThing\";",
        "/* const ROUTE: &str = \"grpc:/vlite.v1.VLite/GetThing\"; */",
    ] {
        let scan = spelled(&format!(
            "{comment}\nconst ROUTE: &str = route_for(\"x\");\n{computed}"
        ));
        assert_problem(comment, &scan, "must open its call under its own route");
    }
    // Neither can a string declare the constant that names the route.
    let scan = spelled(&format!(
        "let note = r#\"const ROUTE: &str = \"grpc:/vlite.v1.VLite/GetThing\"\"#; {computed}"
    ));
    assert_problem(
        "a constant declared in a string",
        &scan,
        "must open its call",
    );
    // A route in a comment does not open the call...
    let scan = spelled(&format!(
        "// self.call(\"grpc:/vlite.v1.VLite/GetThing\", request)\n{}",
        grpc_call("grpc:/vlite.v1.VLite/OtherThing", GRPC_CORE)
    ));
    assert_problem("another route opens the call", &scan, "must open its call");
    // ...and does not make a delegate name the route of its handler.
    let delegate = |comment: &str, label: &str| {
        grpc_fixture(
            &grpc_handler("self.close(request).await"),
            &format!(
                "impl Adapter {{ async fn close(&self, request: Request<A>) -> Result<Response<B>, \
                 Status> {{ {comment}\n{} }} }}",
                grpc_call(label, GRPC_CORE)
            ),
        )
    };
    let in_a_comment = delegate(
        "// \"grpc:/vlite.v1.VLite/GetThing\"",
        "grpc:/vlite.v1.VLite/OtherThing",
    );
    assert_problem(
        "a delegate whose route is in a comment",
        &grpc_problems_of(&in_a_comment),
        "must name the route",
    );
    let in_code = delegate("// \"grpc:/vlite.v1.VLite/OtherThing\"", GRPC_OWN);
    assert_clean(
        "a delegate that names the route",
        &grpc_problems_of(&in_code),
    );
    // The routes of other RPCs in comments are not routes the handler names; a
    // string with `//` in it hides nothing after it.
    let quiet = spelled(&format!(
        "// Not \"grpc:/vlite.v1.VLite/OtherThing\": that is another RPC.\n\
         /* \"grpc:/vlite.v1.VLite/OtherThing\" */\n\
         let note = \"see // below\"; {}",
        grpc_call(GRPC_OWN, GRPC_CORE)
    ));
    assert_clean("routes of other RPCs in comments", &quiet);
    // A stream's failure record may repeat its own route, in code, once the
    // comments are gone.
    let stream = grpc_mutated(
        GRPC_STREAM,
        "Ok(opened)",
        "// \"grpc:/vlite.v1.VLite/OtherThing\"\n        Ok(opened)",
    );
    assert_clean(
        "a stream with a comment",
        &grpc_problems_of(&grpc_fixture(&stream, "")),
    );
}

#[test]
fn the_grpc_check_finds_engine_access_and_pipeline_steps_outside_the_call() {
    let call = grpc_call(GRPC_OWN, GRPC_CORE);
    // What a handler must not take outside its call, and the name a problem
    // gives it.
    let taken = [
        (
            "collections::import_chunk_core(&service, role, request).await;",
            "import_chunk_core",
        ),
        ("put_core(&service, role, request).await;", "put_core"),
        ("let create = collections::create_core;", "create_core"),
        (
            "let scope = guard::RequestScope::new(role);",
            "RequestScope::new",
        ),
        (
            "guard::track_hidden(scope, async {}).await;",
            "track_hidden",
        ),
        ("let role = admin::Role::Admin;", "Role::Admin"),
        ("let role = admin::Role::ReadOnly;", "Role::ReadOnly"),
        ("let role = Role::Scoped(grant);", "Role::Scoped"),
        ("let sidecar = Sidecar::choose(&self.service);", "Sidecar"),
        ("let db = self.service.database;", "database"),
        ("let scope = self.service.scope(role);", "scope"),
        ("Service::run_read(&self.service, |db| Ok(()));", "run_read"),
        (
            "self.service.run_mutation(|db| Ok(())).await;",
            "run_mutation",
        ),
        (
            "let who = self.service.credentials.authenticate(token, now);",
            "authenticate",
        ),
        ("authenticate(&self.service, &headers)?;", "authenticate"),
        ("let step = authenticate;", "authenticate"),
        (
            "authorize_need(&self.service, &role, Need::Admin, false, false)?;",
            "authorize_need",
        ),
        ("rate_limit(&self.service, &role)?;", "rate_limit"),
        (
            "record_request(&self.service, route, status);",
            "record_request",
        ),
    ];
    for (statement, word) in taken {
        let expected = format!("takes `{word}` outside its pipeline call");
        let before = grpc_handler(&format!("{statement}\n{call}"));
        let after = grpc_handler(&format!("let reply = {call};\n{statement}\nreply"));
        for (when, body) in [("before", before), ("after", after)] {
            let scan = grpc_problems_of(&grpc_fixture(&body, ""));
            assert_problem(&format!("`{statement}` {when} the call"), &scan, &expected);
        }
    }
    // Inside the call a core takes the engine, the sidecar choice and the
    // transport-neutral cores, for the role the pipeline authenticated.
    let inside = grpc_call(
        GRPC_OWN,
        "let sidecar = Sidecar::choose(&service); let scope = service.scope(role); \
         let page = collections::put_core(&service, role, request).await?; \
         service.run_mutation(move |db| Ok(db.state())).await",
    );
    let scan = grpc_problems_of(&grpc_fixture(&grpc_handler(&inside), ""));
    assert_clean("engine access inside the call", &scan);
}

// An engine helper, as in the real code: a free function that takes a core, so
// that two RPCs share its semantics.
const GRPC_CHUNK: &str = "async fn apply_chunk(service: &Service, role: admin::Role, chunk: u64) \
     -> Result<Value, ApiError> { let operations = prepare(chunk)?; \
     collections::import_chunk_core(service, role, operations).await }";

#[test]
fn the_grpc_check_confines_an_engine_helper_to_the_pipeline() {
    let through = |core: &str| grpc_handler(&grpc_call(GRPC_OWN, core));
    let call = grpc_call(GRPC_OWN, GRPC_CORE);
    // Accepted: inside the core of a pipeline call, and by a runner.
    let scan = grpc_problems_of(&grpc_fixture(
        &through("apply_chunk(&service, role, 1).await.and_then(json)"),
        GRPC_CHUNK,
    ));
    assert_clean("a free helper inside the call", &scan);
    assert_eq!(scan.helpers.iter().collect::<Vec<_>>(), ["apply_chunk"]);
    let method = "impl Adapter { async fn stash(service: &Service, role: admin::Role) \
                  -> Result<Value, ApiError> { collections::put_core(service, role).await } }";
    let scan = grpc_problems_of(&grpc_fixture(
        &through("Adapter::stash(&service, role).await.and_then(json)"),
        method,
    ));
    assert_clean("a method helper inside the call", &scan);
    assert_eq!(scan.helpers.iter().collect::<Vec<_>>(), ["stash"]);
    let runner = "async fn poll_changes(service: &Service, headers: &HeaderMap) \
                  -> Result<Value, ApiError> { let role = authenticate(service, headers)?; \
                  let scope = service.scope(role); apply_chunk(service, role, 1).await }";
    let scan = grpc_problems_of(&grpc_fixture(GRPC_GOOD, &format!("{GRPC_CHUNK}\n{runner}")));
    assert_clean("a helper called by a runner", &scan);
    assert!(scan.runners.contains("poll_changes"));

    let wrapper = "async fn wrapper(service: &Service, role: admin::Role) \
                   -> Result<Value, ApiError> { apply_chunk(service, role, 1).await }";
    let chain = "async fn first(service: &Service) -> R { second(service).await?; \
                 collections::put_core(service).await } \
                 async fn second(service: &Service) -> R { collections::get_core(service).await }";
    let greedy = "async fn greedy(service: &Service) -> R { \
                  service.run_read(|db| Ok(())).await?; collections::put_core(service).await }";
    let delegate = format!(
        "impl Adapter {{ async fn close(&self, request: Request<A>) -> Result<Response<B>, Status> \
         {{ collections::close_core(&self.service, role).await?; {call} }} }}"
    );
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "a handler that calls it before its call",
            grpc_fixture(
                &grpc_handler(&format!(
                    "apply_chunk(&self.service, role, 1).await?;\n{call}"
                )),
                GRPC_CHUNK,
            ),
            "uses the engine helper `apply_chunk` outside its pipeline call",
        ),
        (
            "a handler that calls it after its call",
            grpc_fixture(
                &grpc_handler(&format!(
                    "let reply = {call};\napply_chunk(&self.service, role, 1).await?;\nreply"
                )),
                GRPC_CHUNK,
            ),
            "uses the engine helper `apply_chunk` outside its pipeline call",
        ),
        (
            "a handler that keeps it as a value",
            grpc_fixture(
                &grpc_handler(&format!("let later = apply_chunk;\n{call}")),
                GRPC_CHUNK,
            ),
            "uses the engine helper `apply_chunk` outside its pipeline call",
        ),
        (
            "a helper that wraps it",
            grpc_fixture(
                &through("wrapper(&service, role).await"),
                &format!("{GRPC_CHUNK}\n{wrapper}"),
            ),
            "`wrapper` uses the engine helper `apply_chunk` outside a pipeline call",
        ),
        (
            "helpers that call each other",
            grpc_fixture(&through("first(&service).await"), chain),
            "`first` uses the engine helper `second` outside a pipeline call",
        ),
        (
            "a helper that nothing calls",
            grpc_fixture(GRPC_GOOD, GRPC_CHUNK),
            "the engine helper `apply_chunk` is never called from inside a pipeline call or by a runner",
        ),
        (
            "a helper that also takes the engine",
            grpc_fixture(&through("greedy(&service).await"), greedy),
            "`greedy` takes `run_read` and is neither a handler nor one of GRPC_RUNNERS",
        ),
        (
            "a delegate that takes a core outside its call",
            grpc_fixture(&grpc_handler("self.close(request).await"), &delegate),
            "`close` takes `close_core` and is neither a handler nor one of GRPC_RUNNERS",
        ),
    ];
    for (what, source, expected) in cases {
        assert_problem(what, &grpc_problems_of(&source), expected);
    }
}

#[test]
fn the_grpc_check_reads_every_impl_and_function_wherever_it_is() {
    let call = grpc_call(GRPC_OWN, GRPC_CORE);
    let good = grpc_handler(&call);
    let no_call = grpc_handler("Ok(Response::new(B::default()))");
    let peeking = grpc_handler(&format!(
        "let page = service.run_read(|db| Ok(())).await;\n{call}"
    ));
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "a second block that declares the handler again",
            grpc_fixture(
                &good,
                &format!("#[tonic::async_trait]\nimpl VLite for Adapter {{ {good} }}"),
            ),
            "VLite/GetThing has more than one handler",
        ),
        (
            "a handler declared again in a nested module",
            grpc_fixture(
                &good,
                &format!("mod again {{ impl VLite for Adapter {{ {good} }} }}"),
            ),
            "VLite/GetThing has more than one handler",
        ),
        (
            "a handler of a nested module",
            format!("mod nested {{ {} }}", grpc_fixture(&no_call, "")),
            "makes no call through the pipeline",
        ),
        (
            "a free function of a nested module",
            grpc_fixture(
                &good,
                "mod util { pub fn peek(service: &Service) { service.run_read(|db| Ok(())); } }",
            ),
            "`peek` takes `run_read` and is neither a handler nor one of GRPC_RUNNERS",
        ),
        (
            "an impl in the body of a function",
            grpc_fixture(
                &good,
                "fn build() { impl Adapter { async fn sneaky(&self, service: &Service) { \
                 service.run_read(|db| Ok(())); } } }",
            ),
            "`sneaky` takes `run_read` and is neither a handler nor one of GRPC_RUNNERS",
        ),
        (
            "a function nested in a handler",
            grpc_fixture(
                &grpc_handler(&format!(
                    "fn inner(service: &Service) {{ service.run_read(|db| Ok(())); }}\n{call}"
                )),
                "",
            ),
            "`inner` takes `run_read` and is neither a handler nor one of GRPC_RUNNERS",
        ),
        (
            "an impl for a path to Adapter",
            format!("impl VLite for crate::server::grpc::Adapter {{ {peeking} }}"),
            "takes `run_read` outside its pipeline call",
        ),
        (
            "an impl for a reference to Adapter",
            format!("impl<'a> VLite for &'a Adapter {{ {no_call} }}"),
            "makes no call through the pipeline",
        ),
        (
            "a known service for another type",
            grpc_fixture(
                &good,
                "impl VLite for Other { async fn get_thing(&self) {} }",
            ),
            "implements a gRPC service for a type this check does not read",
        ),
        (
            "a service nobody checks, spelled with its path",
            grpc_fixture(
                &good,
                "mod nested { impl crate::proto::other_server::Other for crate::server::grpc::Adapter { } }",
            ),
            "`impl Other for Adapter` is a service this check does not know",
        ),
    ];
    for (what, source, expected) in cases {
        assert_problem(what, &grpc_problems_of(&source), expected);
    }
    // The handlers of a nested module and of an impl for a path are the
    // handlers of their service.
    for source in [
        format!("mod nested {{ {} }}", grpc_fixture(&good, "")),
        format!("impl VLite for crate::server::grpc::Adapter {{ {good} }}"),
        format!("impl<'a> VLite for &'a Adapter {{ {good} }}"),
    ] {
        let scan = grpc_problems_of(&source);
        assert_clean("a handler of a service", &scan);
        assert_eq!(
            scan.handlers["VLite"].iter().collect::<Vec<_>>(),
            ["GetThing"]
        );
    }
    // What is not an `Adapter` is not read: the sidecar choice mints the admin
    // role itself.
    let sidecar = "impl Sidecar { async fn choose(service: &Service) -> admin::Role { \
                   admin::Role::Admin } }";
    assert_clean(
        "an impl of another type",
        &grpc_problems_of(&grpc_fixture(&good, sidecar)),
    );
    // Over several files an RPC has one handler as well.
    let one = grpc_fixture(&good, "");
    let scan = grpc_structure_files(
        &[("grpc.rs", &one), ("grpc/more.rs", &one)],
        &["VLite"],
        &[],
    );
    assert_problem(
        "the same handler in two files",
        &scan,
        "has more than one handler (grpc.rs, grpc/more.rs)",
    );
    let listing = grpc_mutated(
        &grpc_handler(&grpc_call("grpc:/vlite.v1.VLite/ListThings", GRPC_CORE)),
        "get_thing",
        "list_things",
    );
    let other = grpc_fixture(&listing, "");
    let scan = grpc_structure_files(
        &[("grpc.rs", &one), ("grpc/more.rs", &other)],
        &["VLite"],
        &[],
    );
    assert_clean("two RPCs in two files", &scan);
    assert_eq!(
        scan.handlers["VLite"].iter().collect::<Vec<_>>(),
        ["GetThing", "ListThings"]
    );
}

#[test]
fn the_grpc_check_finds_a_pipeline_step_repeated_inside_the_call() {
    let handler_with = |core: &str| grpc_fixture(&grpc_handler(&grpc_call(GRPC_OWN, core)), "");
    // The steps Adapter::call takes once, around the core: a core that takes
    // one again takes it a second time.
    let steps = [
        (
            "let who = authenticate(&service, &headers)?;",
            "authenticate",
        ),
        (
            "let who = service.credentials.authenticate(token, now);",
            "authenticate",
        ),
        (
            "authorize_need(&service, &role, Need::Admin, false, false)?;",
            "authorize_need",
        ),
        ("rate_limit(&service, &role)?;", "rate_limit"),
        ("record_request(&service, route, status);", "record_request"),
        ("let step = rate_limit;", "rate_limit"),
    ];
    for (statement, step) in steps {
        let scan = grpc_problems_of(&handler_with(&format!("{statement} {GRPC_CORE}")));
        assert_problem(
            statement,
            &scan,
            &format!("repeats the pipeline step `{step}` inside its pipeline call"),
        );
    }
    // Every repeat is a problem of its own.
    let twice = format!("authenticate(&service, &h)?; authenticate(&service, &h)?; {GRPC_CORE}");
    let scan = grpc_problems_of(&handler_with(&twice));
    let repeats = scan
        .problems
        .iter()
        .filter(|problem| problem.contains("repeats the pipeline step `authenticate`"))
        .count();
    assert_eq!(repeats, 2, "{:?}", scan.problems);
    // A core does not mint what only the pipeline creates either.
    for (statement, word) in [
        ("let role = admin::Role::Admin;", "Role::Admin"),
        (
            "guard::track_hidden(scope, async {}).await;",
            "track_hidden",
        ),
        (
            "let scope = guard::RequestScope::new(role);",
            "RequestScope::new",
        ),
    ] {
        let scan = grpc_problems_of(&handler_with(&format!("{statement} {GRPC_CORE}")));
        assert_problem(
            statement,
            &scan,
            &format!("takes `{word}` inside its pipeline call"),
        );
    }
    // The delegate of a handler makes the call, and is held to the same rule.
    let delegate = |core: &str| {
        grpc_fixture(
            &grpc_handler("self.close(request).await"),
            &format!(
                "impl Adapter {{ async fn close(&self, request: Request<A>) -> Result<Response<B>, \
                 Status> {{ {} }} }}",
                grpc_call(GRPC_OWN, core)
            ),
        )
    };
    let scan = grpc_problems_of(&delegate(&format!(
        "authenticate(&service, &h)?; {GRPC_CORE}"
    )));
    assert_problem(
        "a delegate that repeats a step",
        &scan,
        "`close` repeats the pipeline step `authenticate` inside its pipeline call",
    );
    assert_clean(
        "a delegate with a core",
        &grpc_problems_of(&delegate(GRPC_CORE)),
    );
    // And nothing is repeated, or minted, by a core that only runs the engine.
    assert_clean("a plain core", &grpc_problems_of(&handler_with(GRPC_CORE)));
}

// A runner takes the steps of `GRPC_RUNNERS` for `poll_changes` itself, and
// every one after `authenticate` acts for the role that call returned.
const GRPC_POLL_STEPS: &[&str] = &["authenticate", "rate_limit", "authorize_need", "scope"];

const GRPC_POLL_BODY: &str = "let role = authenticate(service, headers)?; \
     rate_limit(service, &role)?; \
     authorize_need(service, &role, Need::Admin, false, false)?; \
     let scope = service.scope(role); \
     service.run_read(move |db| Ok(scope.keep(db))).await";

/// The scan of a runner `poll_changes` with `body`.
fn grpc_runner_scan(body: &str) -> GrpcScan {
    let runner = format!(
        "async fn poll_changes(service: &Service, headers: &HeaderMap) -> Result<Page, ApiError> \
         {{ {body} }}"
    );
    grpc_structure(
        "fixture.rs",
        &grpc_fixture(GRPC_GOOD, &runner),
        &["VLite"],
        &[("poll_changes", GRPC_POLL_STEPS)],
    )
}

#[test]
fn the_grpc_check_accepts_a_runner_that_uses_its_steps() {
    let scan = grpc_runner_scan(GRPC_POLL_BODY);
    assert_clean("the compact runner", &scan);
    assert!(scan.runners.contains("poll_changes"));
    // The shape of the real runner: a permit first, the role out of an
    // `ok_or_else` closure, the engine in a closure at the end.
    let real = "let _permit = service.requests.clone().acquire_owned().await.map_err(internal)?; \
         let role = authenticate(service, headers)?.ok_or_else(|| { \
             ApiError::new(StatusCode::UNAUTHORIZED, \"unauthorized\", \"valid bearer token required\") \
         })?; \
         rate_limit(service, &role)?; \
         authorize_need(service, &role, Need::Admin, false, false)?; \
         let scope = service.scope(role); \
         service.run_read(move |db| { \
             let mut batch = db.changes_after(&cursor, limit)?; \
             batch.changes.retain_mut(|change| scope.retain_feed_change(change)); \
             Ok(batch) \
         }).await";
    assert_clean("the shape of the real runner", &grpc_runner_scan(real));
    let renamed = GRPC_POLL_BODY.replace("role", "who");
    assert_ne!(renamed, GRPC_POLL_BODY);
    assert_clean("another name for the role", &grpc_runner_scan(&renamed));
    for (what, from, to) in [
        (
            "a role bound by a pattern",
            "let role = authenticate(service, headers)?;",
            "let Some(role) = authenticate(service, headers)? else { return Err(denied()); };",
        ),
        (
            "a role bound out of a match",
            "let role = authenticate(service, headers)?;",
            "let role = match authenticate(service, headers)? { Some(role) => role, \
             None => return Err(denied()) };",
        ),
        (
            "a step whose value is dropped after `?`",
            "rate_limit(service, &role)?;",
            "let _ = rate_limit(service, &role)?;",
        ),
        (
            "a step whose value is kept",
            "rate_limit(service, &role)?;",
            "let admitted = rate_limit(service, &role)?;",
        ),
        (
            "a role that is cloned into a step",
            "rate_limit(service, &role)?;",
            "rate_limit(service, &role.clone())?;",
        ),
    ] {
        let body = grpc_mutated(GRPC_POLL_BODY, from, to);
        assert_clean(what, &grpc_runner_scan(&body));
    }
    // The other runner takes fewer steps.
    let export = "async fn export_page(service: &Service, headers: &HeaderMap) -> R { \
                  let _permit = service.requests.clone().acquire_owned().await?; \
                  let role = authenticate(service, headers)?.ok_or_else(|| unauthorized())?; \
                  let scope = service.scope(role); \
                  service.run_read(move |db| Ok(scope.page(db))).await }";
    let scan = grpc_structure(
        "fixture.rs",
        &grpc_fixture(GRPC_GOOD, export),
        &["VLite"],
        &[("export_page", &["authenticate", "scope"])],
    );
    assert_clean("the page runner", &scan);
    assert!(scan.runners.contains("export_page"));
}

#[test]
fn the_grpc_check_finds_a_runner_that_only_mentions_its_steps() {
    // The role comes from somewhere else than a call of `authenticate`.
    let denied = "let role = fallback(headers);";
    let swap = |from: &str, to: &str| grpc_mutated(GRPC_POLL_BODY, from, to);
    let authenticate = "let role = authenticate(service, headers)?;";
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "`authenticate` named and not called",
            swap(authenticate, &format!("let _ = authenticate; {denied}")),
            "must also take `authenticate` itself",
        ),
        (
            "`authenticate` in an unused closure",
            swap(
                authenticate,
                &format!("let unused = || authenticate(service, headers); {denied}"),
            ),
            "must also take `authenticate` itself",
        ),
        (
            "`authenticate` in an unused closure with a block",
            swap(
                authenticate,
                &format!("let unused = move || {{ authenticate(service, headers) }}; {denied}"),
            ),
            "must also take `authenticate` itself",
        ),
        (
            "`authenticate` in a branch",
            swap(
                authenticate,
                &format!("if flagged {{ authenticate(service, headers)?; }} {denied}"),
            ),
            "must also take `authenticate` itself",
        ),
        (
            "`authenticate` in an async block",
            swap(
                authenticate,
                &format!("let task = async {{ authenticate(service, headers) }}; {denied}"),
            ),
            "must also take `authenticate` itself",
        ),
        (
            "`authenticate` inside another call",
            swap(
                authenticate,
                "let role = fallback(authenticate(service, headers)?);",
            ),
            "must also take `authenticate` itself",
        ),
        (
            "the call of `authenticate` discarded with `let _ =`",
            swap(
                authenticate,
                &format!("let _ = authenticate(service, headers); {denied}"),
            ),
            "must also take `authenticate` itself",
        ),
        (
            "a bare call of `authenticate`",
            swap(
                authenticate,
                &format!("authenticate(service, headers); {denied}"),
            ),
            "must also take `authenticate` itself",
        ),
        (
            "a role that is not bound",
            swap(
                authenticate,
                &format!("let _ = authenticate(service, headers)?; {denied}"),
            ),
            "must bind the role `authenticate` returns",
        ),
        (
            "a role bound to an underscore name",
            swap(
                authenticate,
                &format!("let _role = authenticate(service, headers)?; {denied}"),
            ),
            "must bind the role `authenticate` returns",
        ),
        (
            "a role that is only checked",
            swap(
                authenticate,
                &format!("authenticate(service, headers)?; {denied}"),
            ),
            "must bind the role `authenticate` returns",
        ),
        (
            "a rate limit that is dropped with `let _ =`",
            swap(
                "rate_limit(service, &role)?;",
                "let _ = rate_limit(service, &role);",
            ),
            "must also take `rate_limit` itself",
        ),
        (
            "a rate limit whose result nobody reads",
            swap(
                "rate_limit(service, &role)?;",
                "rate_limit(service, &role);",
            ),
            "must also take `rate_limit` itself",
        ),
        (
            "a rate limit in a closure",
            swap(
                "rate_limit(service, &role)?;",
                "let check = || rate_limit(service, &role);",
            ),
            "must also take `rate_limit` itself",
        ),
        (
            "a rate limit in a branch",
            swap(
                "rate_limit(service, &role)?;",
                "if limited { rate_limit(service, &role)?; }",
            ),
            "must also take `rate_limit` itself",
        ),
        (
            "a policy check bound to an underscore name",
            swap(
                "authorize_need(service, &role, Need::Admin, false, false)?;",
                "let _checked = authorize_need(service, &role, Need::Admin, false, false);",
            ),
            "must also take `authorize_need` itself",
        ),
        (
            "a policy check that is only named",
            swap(
                "authorize_need(service, &role, Need::Admin, false, false)?;",
                "let _ = authorize_need;",
            ),
            "must also take `authorize_need` itself",
        ),
        (
            "a scope that is only named",
            swap(
                "let scope = service.scope(role);",
                "let scope = service.scope;",
            ),
            "must also take `scope` itself",
        ),
        (
            "a step for another role",
            swap(
                "rate_limit(service, &role)?;",
                "rate_limit(service, &stranger)?;",
            ),
            "calls `rate_limit` without the role it authenticated",
        ),
        (
            "a scope for another role",
            swap(
                "let scope = service.scope(role);",
                "let scope = service.scope(stranger);",
            ),
            "calls `scope` without the role it authenticated",
        ),
        (
            "a role that is minted",
            swap(
                "let scope = service.scope(role);",
                "let scope = service.scope(admin::Role::Admin);",
            ),
            "takes `Role::Admin`",
        ),
        (
            "a scope that comes before `authenticate`",
            format!(
                "let scope = service.scope(role); {authenticate} \
                 rate_limit(service, &role)?; \
                 authorize_need(service, &role, Need::Admin, false, false)?; \
                 service.run_read(move |db| Ok(scope.keep(db))).await"
            ),
            "after `authenticate`",
        ),
    ];
    for (what, body, expected) in cases {
        assert_problem(what, &grpc_runner_scan(&body), expected);
    }
    // A runner that authenticates and then never uses the role fails every
    // step it takes for somebody else.
    let unused = "let role = authenticate(service, headers)?; \
                  rate_limit(service, limits)?; \
                  authorize_need(service, grant, Need::Admin, false, false)?; \
                  let scope = service.scope(other); \
                  service.run_read(move |db| Ok(scope.keep(db))).await";
    let scan = grpc_runner_scan(unused);
    let strangers = scan
        .problems
        .iter()
        .filter(|problem| problem.contains("without the role it authenticated"))
        .count();
    assert_eq!(strangers, 3, "{:?}", scan.problems);
}

#[test]
fn the_grpc_check_finds_a_name_hidden_by_an_import() {
    let cases = [
        (
            "a renamed core",
            "use crate::server::collections::put_core as store;",
            "`put_core` is renamed with `as`",
        ),
        (
            "a renamed step",
            "use super::authenticate as log_in;",
            "`authenticate` is renamed with `as`",
        ),
        (
            "a renamed role",
            "use crate::server::admin::Role as Rank;",
            "`Role` is renamed with `as`",
        ),
        (
            "a renamed tracker",
            "use crate::server::guard::track_hidden as hide;",
            "`track_hidden` is renamed with `as`",
        ),
        (
            "a renamed helper",
            "use super::apply_chunk as apply;",
            "`apply_chunk` is renamed with `as`",
        ),
        (
            "a role variant",
            "use crate::server::admin::Role::Admin;",
            "brings a variant of `Role` into scope",
        ),
        (
            "every role variant",
            "use crate::server::admin::Role::*;",
            "brings a variant of `Role` into scope",
        ),
        (
            "role variants in a list",
            "use crate::server::admin::Role::{Admin, ReadOnly};",
            "brings a variant of `Role` into scope",
        ),
    ];
    for (what, import, expected) in cases {
        let scan = grpc_problems_of(&grpc_fixture(GRPC_GOOD, &format!("{import}\n{GRPC_CHUNK}")));
        assert_problem(what, &scan, expected);
    }
    // An import inside a function is read as well.
    let inside = grpc_handler(&format!(
        "use crate::server::admin::Role::Admin;\n{}",
        grpc_call(GRPC_OWN, GRPC_CORE)
    ));
    let scan = grpc_problems_of(&grpc_fixture(&inside, ""));
    assert_problem(
        "an import in a handler",
        &scan,
        "brings a variant of `Role` into scope",
    );
    // Other imports, renames and casts are none of the check's business.
    let quiet = "use super::admin::{self, Role}; \
                 use tonic::metadata::MetadataValue as Ascii; \
                 use super::{authenticate, rate_limit}; \
                 fn size(n: u64) -> usize { n as usize }";
    assert_clean(
        "other imports",
        &grpc_problems_of(&grpc_fixture(GRPC_GOOD, quiet)),
    );
}
