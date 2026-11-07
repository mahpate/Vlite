//! Source check: no server code outside the guard module looks a collection
//! or graph up by name, or decides a reservation by itself.
//!
//! Every reservation, grant and tenant rule of the server is enforced by
//! `server::guard::RequestScope`, which resolves a name once and hands the
//! handler an ID-carrying handle. A handler that calls `Database::collection`,
//! `resolve`, `kv_view` or a sibling by name bypasses all of it, and a handler
//! that re-resolves a name after the guard can bind to a different collection
//! (the tenant-qualified name was checked, the unqualified one was used; fixed
//! in 0.7.0 for named search, sparse search and the metadata route). A list, a
//! log or a feed that asks `Reserved` directly instead of the scope can drift
//! from the rule the lookups obey, so the reservation methods are banned
//! outside the guard and the module that defines them, too.
//!
//! The scan reads `src/server.rs` and `src/server/**/*.rs`, blanks comments
//! and string literals, drops `#[cfg(test)]` items (and every file only test
//! code includes), and fails on any call or path that looks a collection or
//! graph up by name, or enumerates collections, or calls a reservation method,
//! unless a justified entry of `ALLOWED` names its file, enclosing function
//! and pattern. An entry that no longer matches fails too, so the list cannot
//! go stale.
//!
//! The guard's own methods share names with the database's (`collection`,
//! `kv_view`, `drop_collection`, ...). They are told apart by their first
//! argument: a guard method takes the database (`scope.collection(db, ..)`),
//! a lookup by name takes the name.
//!
//! This is a tripwire, not a proof: a method alias or a macro can evade a
//! textual scan. The type-level handles of the guard and the digest assertions
//! of `server_guard_differential` are the other layers.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

/// The structural check of the gRPC adapters (lane F3), in a module of its own.
#[path = "server_name_lookup_scan/grpc_leg.rs"]
mod grpc_leg;

/// Methods that look a collection or graph up by name, create or drop one by
/// name, or enumerate them. Banned on any receiver unless the database is the
/// first argument: no other type of the server layer has a method of these
/// names that takes a name.
const BY_NAME: &[&str] = &[
    "collection",
    "collection_mut",
    "collections",
    "graph",
    "graph_mut",
    "kv_view",
    "kv_store",
    "drop_collection",
    "drop_graph",
    "set_collection_settings",
    "create_collection",
    "create_collection_occupied",
    "create_graph",
    "list_graphs",
    "node_attachment_collections",
    "memory_attachment_owner",
    "build_disk_ann",
];

/// `resolve` is also a method of other types (`Bindings::resolve`,
/// `IdentityInput::resolve`), and `collections` is a field of credentials and
/// requests, so they are banned on a database receiver only: `db.resolve(..)`
/// and the field walk `db.collections.iter()`.
const DATABASE_RECEIVERS: &[&str] = &["db", "database"];

/// The only files exempt from the scan. The guard resolves names by design.
const EXEMPT: &[&str] = &["src/server/guard.rs"];

/// Methods that decide whether a reservation hides a resource. Every
/// reservation decision of the server is the scope's
/// (`server::guard::RequestScope`): lists, import jobs, metrics, the slow-query
/// log and the change feed ask it, so none of them can drift from the rule a
/// name lookup obeys. Banned outside the files that implement the reservation.
const RESERVATION: &[&str] = &[
    "raw_allowed",
    "reserved",
    "attachment_allowed",
    "retain_visible",
    "retain_change_operations",
];

/// The files that may call the reservation methods: the guard, and the module
/// that defines them.
const RESERVATION_OWNERS: &[&str] = &["src/server/guard.rs", "src/server/reserved.rs"];

/// `(file, enclosing function, pattern, justification)`. A reviewer reads this
/// list to see every place the server layer reaches the database by name
/// without the guard; each entry says why that is safe.
const ALLOWED: &[(&str, &str, &str, &str)] = &[
    (
        "src/server.rs",
        "enforce_quota",
        "db.collections",
        "Live-vector quotas sum the records of the credential's collections and of the tenant's \
         prefix, reserved collections included on purpose (they are real usage); it names no \
         collection and returns no name.",
    ),
    (
        "src/server/collections.rs",
        "locate",
        "db.collections",
        "Checks by ID that the collection a reserved import job holds still carries the reserved \
         name; the ID was authorized when the job was reserved, and no request name is resolved.",
    ),
    (
        "src/server/collections.rs",
        "earliest_pending",
        "db.collections",
        "Checks by ID that the collection of an earlier reserved import job is still live; it \
         resolves no request name.",
    ),
    (
        "src/server/memory.rs",
        "prepare",
        ".collection(",
        "Startup provisioning of the evidence collection the operator's binding file names, run \
         before any request is served; no request names it.",
    ),
    (
        "src/server/memory.rs",
        "prepare",
        ".graph(",
        "Startup provisioning of the graph the operator's binding file names, run before any \
         request is served; no request names it.",
    ),
    (
        "src/server/memory/routes.rs",
        "covers",
        "db.resolve(",
        "Rechecks the grant's ID and lineage binding of the evidence collection the binding \
         names, after the exact memory scope was authorized; it never resolves a request name.",
    ),
    (
        "src/server/observability.rs",
        "collection_metrics",
        "db.collections",
        "Walks every collection and keeps those the request scope shows the role (grant binding \
         and reservation, in one predicate); it resolves no name.",
    ),
    (
        "src/server/observability.rs",
        "snapshot",
        "db.collections",
        "Database-wide counters of the administrator view; the metrics routes are refused while \
         memory bindings exist, and the per-collection list goes through collection_metrics.",
    ),
    (
        "src/server/observability.rs",
        "summary",
        "db.collections",
        "Database-wide counters of the administrator summary; the summary route is refused while \
         memory bindings exist.",
    ),
    (
        "src/server/ops.rs",
        "auto_tick",
        "db.collections",
        "The maintenance scheduler walks every indexed collection by ID, round-robin; automatic \
         maintenance is refused while memory bindings exist and it answers no request.",
    ),
    (
        "src/server/tenants.rs",
        "delete",
        ".collections(",
        "Administrator-only tenant deletion refuses a tenant that still owns any collection, \
         reserved ones included, so it needs the unfiltered list.",
    ),
    (
        "src/server/tenants.rs",
        "delete",
        ".list_graphs(",
        "Administrator-only tenant deletion refuses a tenant that still owns any graph, reserved \
         ones included, so it needs the unfiltered list.",
    ),
    (
        "src/server/tenants.rs",
        "usage",
        "db.collections",
        "The administrator's tenant usage report lists every collection with the tenant's prefix, \
         reserved ones included because their records count toward its quota; it resolves no name.",
    ),
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Finding {
    file: String,
    function: String,
    pattern: String,
    line: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Ident(String),
    Punct(char),
}

/// Replaces comments, string literals and character literals with spaces,
/// keeping every newline, so offsets and line numbers still match the source.
fn blank(source: &str) -> String {
    blank_with(source, true)
}

/// As [`blank`], but with `literals` false only the comments go: string and
/// character literals stay, still read as literals (a `//` inside a string is
/// no comment), so that text written as a literal of code can be read from
/// code alone.
fn blank_with(source: &str, literals: bool) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    let erase = |out: &mut Vec<u8>, from: usize, to: usize| {
        for byte in &mut out[from..to] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    let erase_literal = |out: &mut Vec<u8>, from: usize, to: usize| {
        if literals {
            erase(out, from, to);
        }
    };
    let identifier_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                erase(&mut out, start, i);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let start = i;
                let mut depth = 0usize;
                while i < bytes.len() {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                erase(&mut out, start, i);
            }
            b'r' if (i == 0 || !identifier_byte(bytes[i - 1]))
                && matches!(bytes.get(i + 1), Some(b'"' | b'#')) =>
            {
                // Raw string: r"..", r#".."#, with any number of hashes.
                let mut hashes = 0;
                let mut j = i + 1;
                while bytes.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if bytes.get(j) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let mut closing = vec![b'"'];
                closing.extend(std::iter::repeat_n(b'#', hashes));
                let body = j + 1;
                let end = source[body..]
                    .find(std::str::from_utf8(&closing).unwrap())
                    .map_or(bytes.len(), |at| body + at + closing.len());
                erase_literal(&mut out, i, end);
                i = end;
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(bytes.len());
                erase_literal(&mut out, start, i);
            }
            b'\'' => {
                // A character literal, not a lifetime: one character (or one
                // escape) between the quotes.
                let rest = &source[i + 1..];
                let length = if rest.starts_with('\\') {
                    rest.find('\'').map(|at| at + 1)
                } else {
                    rest.chars()
                        .next()
                        .filter(|_| {
                            rest[rest.chars().next().unwrap().len_utf8()..].starts_with('\'')
                        })
                        .map(|c| c.len_utf8() + 1)
                };
                if let Some(length) = length {
                    erase_literal(&mut out, i, i + 1 + length);
                    i += 1 + length;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).expect("blanking keeps UTF-8")
}

fn tokens(text: &str) -> Vec<(Token, usize)> {
    let mut out = Vec::new();
    let mut line = 1;
    let mut chars = text.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == '\n' {
            line += 1;
        } else if c.is_whitespace() {
        } else if c.is_ascii_alphabetic() || c == '_' {
            let mut ident = String::from(c);
            while let Some(&(_, next)) = chars.peek() {
                if next.is_ascii_alphanumeric() || next == '_' {
                    ident.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push((Token::Ident(ident), line));
        } else if c.is_ascii_digit() {
            while chars
                .peek()
                .is_some_and(|&(_, next)| next.is_ascii_alphanumeric() || next == '_')
            {
                chars.next();
            }
        } else {
            out.push((Token::Punct(c), line));
        }
    }
    out
}

/// Index of the `}` matching the `{` at `open` (or the end of the text).
fn matching_brace(text: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    for (offset, &byte) in text[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
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

/// End (exclusive) of the item that starts at `at`: just after the first `;`,
/// or after the `}` that matches the first `{`, at bracket depth zero.
fn item_end(bytes: &[u8], at: usize) -> usize {
    let mut depth = 0i32;
    for (offset, &byte) in bytes[at..].iter().enumerate() {
        match byte {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b';' if depth == 0 => return at + offset + 1,
            b'{' if depth == 0 => return matching_brace(bytes, at + offset) + 1,
            _ => {}
        }
    }
    bytes.len()
}

/// Whether a compacted attribute (`cfg(..)`, without whitespace) compiles only
/// under test: `cfg(test)`, or `cfg(all(..))` with such a predicate among its
/// arguments (`all(test, feature = "grpc")`). `any(..)` and `not(..)` do not.
fn requires_test(attribute: &str) -> bool {
    fn predicate(text: &str) -> bool {
        if text == "test" {
            return true;
        }
        let Some(inner) = text
            .strip_prefix("all(")
            .and_then(|rest| rest.strip_suffix(')'))
        else {
            return false;
        };
        let mut depth = 0usize;
        let mut start = 0;
        let mut arguments = Vec::new();
        for (at, c) in inner.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    arguments.push(&inner[start..at]);
                    start = at + 1;
                }
                _ => {}
            }
        }
        arguments.push(&inner[start..]);
        arguments.into_iter().any(predicate)
    }
    attribute
        .strip_prefix("cfg(")
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(predicate)
}

/// Removes every `#[cfg(test)]` item (or `#[cfg(all(test, ..))]`, see
/// `requires_test`) from blanked source, keeping newlines.
fn without_test_items(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut search = 0;
    while let Some(found) = text[search..].find("#[") {
        let start = search + found;
        let Some(close) = text[start..].find(']').map(|at| start + at) else {
            break;
        };
        search = close + 1;
        let attribute: String = text[start + 2..close]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if !requires_test(&attribute) {
            continue;
        }
        // The item starts after this attribute and any further attributes.
        let mut at = close + 1;
        loop {
            let rest = &text[at..];
            let trimmed = rest.trim_start();
            at += rest.len() - trimmed.len();
            if trimmed.starts_with("#[") {
                at = at + trimmed.find(']').expect("attribute closes") + 1;
            } else {
                break;
            }
        }
        let end = item_end(bytes, at);
        for byte in &mut out[start..end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        search = end;
    }
    String::from_utf8(out).expect("blanking keeps UTF-8")
}

/// An out-of-line module (`mod x;`) declared at the top level of a file.
#[derive(Debug, PartialEq, Eq)]
struct ModuleDecl {
    name: String,
    /// Declared under `#[cfg(test)]`.
    test: bool,
    /// The `#[path = "..."]` of the declaration, relative to the directory of
    /// the declaring file.
    path: Option<String>,
}

/// The modules `source` declares. `blanked` is `blank(source)`: it keeps every
/// offset, so the text of a `#[path]` string is read from `source`.
fn module_declarations(source: &str, blanked: &str) -> Vec<ModuleDecl> {
    let bytes = blanked.as_bytes();
    let mut out = Vec::new();
    let mut attributes: Vec<(String, usize, usize)> = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at].is_ascii_whitespace() {
            at += 1;
        } else if bytes[at] == b'#' {
            let Some(open) = blanked[at..].find('[').map(|offset| at + offset) else {
                break;
            };
            let mut depth = 0i32;
            let mut close = open;
            for (offset, &byte) in bytes[open..].iter().enumerate() {
                match byte {
                    b'[' => depth += 1,
                    b']' => {
                        depth -= 1;
                        if depth == 0 {
                            close = open + offset;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if bytes.get(at + 1) != Some(&b'!') {
                let compact: String = blanked[open + 1..close]
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                attributes.push((compact, open + 1, close));
            }
            at = close + 1;
        } else {
            let end = item_end(bytes, at);
            let item = blanked[at..end].trim_end();
            let words: Vec<&str> = item.split_whitespace().take(3).collect();
            let name = match words.as_slice() {
                ["mod", name, ..] if item.ends_with(';') => {
                    Some(name.trim_end_matches(';').to_owned())
                }
                ["pub" | "pub(super)" | "pub(crate)", "mod", name, ..] if item.ends_with(';') => {
                    Some(name.trim_end_matches(';').to_owned())
                }
                _ => None,
            };
            if let Some(name) = name {
                let test = attributes
                    .iter()
                    .any(|(compact, ..)| requires_test(compact));
                let path = attributes
                    .iter()
                    .find(|(compact, ..)| compact.starts_with("path="))
                    .and_then(|(_, from, to)| {
                        let literal = &source[*from..*to];
                        let first = literal.find('"')?;
                        let last = literal.rfind('"')?;
                        (last > first).then(|| literal[first + 1..last].to_owned())
                    });
                out.push(ModuleDecl { name, test, path });
            }
            attributes.clear();
            at = end;
        }
    }
    out
}

fn parse_modules(source: &str) -> Vec<ModuleDecl> {
    module_declarations(source, &blank(source))
}

/// Every by-name lookup in `source`, as the file `file`.
fn scan_source(file: &str, source: &str) -> Vec<Finding> {
    let code = without_test_items(&blank(source));
    let tokens = tokens(&code);
    let mut findings = Vec::new();
    // The functions whose body encloses the current token.
    let mut open: Vec<Option<String>> = Vec::new();
    let mut pending: Option<String> = None;
    let mut index = 0;
    while index < tokens.len() {
        let (token, line) = &tokens[index];
        let ident = |at: usize| match tokens.get(at) {
            Some((Token::Ident(name), _)) => Some(name.as_str()),
            _ => None,
        };
        let punct =
            |at: usize, c: char| matches!(tokens.get(at), Some((Token::Punct(p), _)) if *p == c);
        match token {
            Token::Ident(word) if word == "fn" => {
                pending = ident(index + 1).map(str::to_owned);
            }
            Token::Punct('{') => open.push(pending.take()),
            Token::Punct('}') => {
                open.pop();
            }
            Token::Punct(';') => pending = None,
            Token::Punct('.') => {
                if let Some(name) = ident(index + 1) {
                    let call = punct(index + 2, '(');
                    let receiver = index
                        .checked_sub(1)
                        .and_then(ident)
                        .filter(|receiver| DATABASE_RECEIVERS.contains(receiver));
                    let pattern = match (call, receiver) {
                        (true, _)
                            if BY_NAME.contains(&name) && !database_first(&tokens, index + 3) =>
                        {
                            Some(format!(".{name}("))
                        }
                        (true, _)
                            if RESERVATION.contains(&name)
                                && !RESERVATION_OWNERS.contains(&file) =>
                        {
                            Some(format!(".{name}("))
                        }
                        (true, Some(receiver)) if name == "resolve" => {
                            Some(format!("{receiver}.resolve("))
                        }
                        (false, Some(receiver)) if name == "collections" => {
                            Some(format!("{receiver}.collections"))
                        }
                        _ => None,
                    };
                    if let Some(pattern) = pattern {
                        let function = open
                            .iter()
                            .rev()
                            .find_map(|name| name.clone())
                            .unwrap_or_default();
                        findings.push(Finding {
                            file: file.to_owned(),
                            function,
                            pattern,
                            line: *line,
                        });
                    }
                }
            }
            Token::Punct(':') if punct(index + 1, ':') => {
                if ident(index.wrapping_sub(1)) == Some("Database")
                    && let Some(name) = ident(index + 2)
                    && (BY_NAME.contains(&name) || name == "resolve")
                {
                    let function = open
                        .iter()
                        .rev()
                        .find_map(|name| name.clone())
                        .unwrap_or_default();
                    findings.push(Finding {
                        file: file.to_owned(),
                        function,
                        pattern: format!("Database::{name}"),
                        line: *line,
                    });
                }
            }
            _ => {}
        }
        index += 1;
    }
    findings
}

/// Whether the argument list that starts at `at` passes the database first
/// (`db`, `&db`, `&mut db`, `&*db`, `&mut *db`): the shape of a guard method,
/// which takes the database and never a name.
fn database_first(tokens: &[(Token, usize)], mut at: usize) -> bool {
    while matches!(tokens.get(at), Some((Token::Punct('&' | '*'), _)))
        || matches!(tokens.get(at), Some((Token::Ident(word), _)) if word == "mut")
    {
        at += 1;
    }
    matches!(
        tokens.get(at),
        Some((Token::Ident(name), _)) if DATABASE_RECEIVERS.contains(&name.as_str())
    ) && matches!(tokens.get(at + 1), Some((Token::Punct(',' | ')'), _)))
}

/// The files of a source tree that the scan reads, with their paths relative
/// to the crate root and `/` separators.
fn server_files(root: &Path) -> Vec<(String, PathBuf)> {
    fn walk(directory: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = fs::read_dir(directory)
            .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut paths = vec![root.join("src/server.rs")];
    walk(&root.join("src/server"), &mut paths);
    paths
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (relative, path)
        })
        .collect()
}

/// The module file(s) `mod name;` of `parent` can denote.
fn module_files(parent: &str, name: &str) -> [String; 2] {
    let directory = match parent.strip_suffix("/mod.rs") {
        Some(directory) => directory.to_owned(),
        None => parent.strip_suffix(".rs").unwrap().to_owned(),
    };
    [
        format!("{directory}/{name}.rs"),
        format!("{directory}/{name}/mod.rs"),
    ]
}

/// `relative` resolved against `directory`, both with `/` separators.
fn join_relative(directory: &str, relative: &str) -> String {
    let mut parts: Vec<&str> = directory.split('/').collect();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// The files a declaration of `parent` can denote: its `#[path]` (relative to
/// the directory of the declaring file) or the usual module file names.
fn module_candidates(parent: &str, declaration: &ModuleDecl) -> Vec<String> {
    match &declaration.path {
        Some(path) => {
            let directory = parent
                .rsplit_once('/')
                .map_or("", |(directory, _)| directory);
            vec![join_relative(directory, path)]
        }
        None => module_files(parent, &declaration.name).to_vec(),
    }
}

/// The files only test code includes: modules declared under `#[cfg(test)]`,
/// and every module such a file declares in turn.
fn test_only_files(sources: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut queue = Vec::new();
    for (file, source) in sources {
        for declaration in parse_modules(source).iter().filter(|module| module.test) {
            queue.extend(module_candidates(file, declaration));
        }
    }
    while let Some(file) = queue.pop() {
        let Some(source) = sources.get(&file) else {
            continue;
        };
        if !found.insert(file.clone()) {
            continue;
        }
        for declaration in parse_modules(source) {
            queue.extend(module_candidates(&file, &declaration));
        }
    }
    found
}

/// Scans every server file, returning the findings and the number of files
/// read. Files that only `#[cfg(test)]` code includes are skipped.
fn scan_tree(root: &Path) -> (Vec<Finding>, usize) {
    let sources: BTreeMap<String, String> = server_files(root)
        .into_iter()
        .map(|(relative, path)| (relative, fs::read_to_string(path).unwrap()))
        .collect();
    let test_only = test_only_files(&sources);
    let mut findings = Vec::new();
    let mut read = 0;
    for (relative, source) in &sources {
        if EXEMPT.contains(&relative.as_str()) || test_only.contains(relative) {
            continue;
        }
        read += 1;
        findings.extend(scan_source(relative, source));
    }
    findings.sort();
    (findings, read)
}

/// Compares findings with an allowlist and describes every problem.
fn judge(findings: &[Finding], allowed: &[(&str, &str, &str, &str)]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut used = vec![false; allowed.len()];
    for finding in findings {
        let hit = allowed.iter().position(|(file, function, pattern, _)| {
            *file == finding.file && *function == finding.function && *pattern == finding.pattern
        });
        match hit {
            Some(position) => used[position] = true,
            None => problems.push(format!(
                "{}:{} in fn {}: `{}` looks a collection or graph up by name, or decides a \
                 reservation, outside the guard; use server::guard::RequestScope, or add a \
                 justified entry to ALLOWED",
                finding.file, finding.line, finding.function, finding.pattern
            )),
        }
    }
    for (position, entry) in allowed.iter().enumerate() {
        if !used[position] {
            problems.push(format!("stale ALLOWED entry: {entry:?} matches nothing"));
        }
        if entry.3.trim().len() < 30 {
            problems.push(format!(
                "ALLOWED entry {entry:?} needs a justification of at least 30 characters"
            ));
        }
        if allowed
            .iter()
            .filter(|other| other.0 == entry.0 && other.1 == entry.1 && other.2 == entry.2)
            .count()
            > 1
        {
            problems.push(format!("duplicate ALLOWED entry for {entry:?}"));
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

#[test]
fn no_server_code_looks_a_name_up_outside_the_guard() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (findings, read) = scan_tree(root);
    assert!(
        read > 20,
        "the scan must read the server tree, read {read} files"
    );
    let problems = judge(&findings, ALLOWED);
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

#[test]
fn the_guard_module_is_the_exempt_file_and_exists() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for file in EXEMPT {
        assert!(root.join(file).is_file(), "{file}");
    }
    let guard = fs::read_to_string(root.join("src/server/guard.rs")).unwrap();
    assert!(
        !scan_source("src/server/guard.rs", &guard).is_empty(),
        "the guard is where names are resolved; an empty finding set means the scanner is blind"
    );
}

fn found(source: &str) -> Vec<(String, String, usize)> {
    scan_source("fixture.rs", source)
        .into_iter()
        .map(|finding| (finding.function, finding.pattern, finding.line))
        .collect()
}

#[test]
fn the_scanner_finds_every_banned_form() {
    let source = r#"
fn direct(db: &Database) {
    db.resolve("a");
    db.collection("a");
    db.collection_mut("a");
    db.graph("g");
    db.graph_mut("g");
    db.kv_view("a");
    db.kv_store("a");
    db.drop_collection("a");
    db.drop_graph("g");
    db.set_collection_settings("a", s);
    db.create_collection("a", spec);
    db.create_graph("g");
    db.list_graphs();
    db.collections();
    db.node_attachment_collections("g", "k");
    db.memory_attachment_owner("c", 1, 2);
    db.build_disk_ann("a", path, config, metric);
    db.collections.iter();
    Database::collection(db, "a");
    Database::resolve(db, "a");
}
fn chained(db: &Database) {
    let view = db
        .kv_view(&name)?
        .get(1);
    owner
        .as_ref()
        .unwrap()
        .collection("a");
    database
        .resolve("a");
    scope.graph(&name)?;
    scope.collection(&target, db)?;
}
fn reservations(bindings: &Bindings) {
    bindings.reserved().raw_allowed(None, Resource::Collection, "a");
    reserved.attachment_allowed("g", "c");
    reserved.retain_visible(None, Resource::Graph, &mut items, name);
    reserved.retain_change_operations(None, &mut kind);
}
"#;
    let patterns: Vec<_> = found(source)
        .into_iter()
        .map(|(function, pattern, _)| format!("{function}:{pattern}"))
        .collect();
    assert_eq!(
        patterns,
        [
            "direct:db.resolve(",
            "direct:.collection(",
            "direct:.collection_mut(",
            "direct:.graph(",
            "direct:.graph_mut(",
            "direct:.kv_view(",
            "direct:.kv_store(",
            "direct:.drop_collection(",
            "direct:.drop_graph(",
            "direct:.set_collection_settings(",
            "direct:.create_collection(",
            "direct:.create_graph(",
            "direct:.list_graphs(",
            "direct:.collections(",
            "direct:.node_attachment_collections(",
            "direct:.memory_attachment_owner(",
            "direct:.build_disk_ann(",
            "direct:db.collections",
            "direct:Database::collection",
            "direct:Database::resolve",
            "chained:.kv_view(",
            "chained:.collection(",
            "chained:database.resolve(",
            "chained:.graph(",
            "chained:.collection(",
            "reservations:.reserved(",
            "reservations:.raw_allowed(",
            "reservations:.attachment_allowed(",
            "reservations:.retain_visible(",
            "reservations:.retain_change_operations(",
        ]
    );
    // Line numbers are those of the source, across multi-line chains.
    let line_of = |needle: &str| source.lines().position(|l| l.contains(needle)).unwrap() + 1;
    let lines: Vec<_> = found(source).into_iter().map(|(_, _, line)| line).collect();
    assert_eq!(lines[0], line_of("db.resolve(\"a\")"));
    assert_eq!(lines[20], line_of(".kv_view(&name)"));
    assert_eq!(lines[21], line_of("        .collection(\"a\")"));
    assert_eq!(lines[22], line_of("        .resolve(\"a\")"));
}

#[test]
fn the_scanner_ignores_what_is_not_code_or_not_a_lookup() {
    let source = r##"
// db.resolve("commented");
/// db.collection("documented");
/* db.kv_view("block"); /* nested db.kv_store("x"); */ still comment */
fn quiet(db: &Database, bindings: &Bindings, identity: &Identity) {
    let a = "db.resolve(\"in a string\")";
    let b = r#"db.collection("in a raw string")"#;
    let c = '"';
    db.resolve_visible("n", &visible);
    db.collection_ref(1);
    db.state(1);
    bindings.resolve(None, "binding");
    identity.resolve(db, id, &target);
    limits.resolve();
    let change_collection = change.collection;
    let graph = change.graph;
    request.collections;
    let lifetime: &'a str = "x";
    let _ = (a, b, c, lifetime, graph, change_collection);
    // The guard's methods take the database first.
    scope.collection(db, &target)?;
    scope.collection(&*db, &target)?;
    scope.create_collection(&mut *db, "a", spec)?;
    handle.kv_view(db)?;
    handle.kv_store(&mut db)?;
    handle.drop_collection(database)?;
    handle.build_disk_ann(db, &path, config, metric)?;
    listing.list_graphs(db)?;
    name.drop_graph(db)?;
    scope.guarded_graph(&name)?;
}
#[cfg(test)]
mod tests {
    fn loud(db: &Database) {
        db.resolve("test only");
    }
}
#[cfg(test)]
#[allow(dead_code)]
fn also_test_only(db: &Database) {
    db.collection("x");
}
#[cfg(test)]
use crate::Database as Unused;
fn after_the_test_items(db: &Database) {
    db.collection("counted");
}
"##;
    assert_eq!(
        found(source),
        [(
            "after_the_test_items".to_owned(),
            ".collection(".to_owned(),
            source
                .lines()
                .position(|line| line.contains("\"counted\""))
                .unwrap()
                + 1
        )]
    );
    let owner = "fn decide(&self) { self.raw_allowed(None, r, n); bindings.reserved(); }";
    assert!(scan_source("src/server/reserved.rs", owner).is_empty());
    assert!(scan_source("src/server/guard.rs", owner).is_empty());
    assert_eq!(scan_source("src/server/observability.rs", owner).len(), 2);
}

#[test]
fn the_scanner_names_the_enclosing_function() {
    let source = r#"
impl Adapter {
    async fn handler(&self) {
        let run = move |db| {
            db.collection("a");
        };
        fn nested(db: &Database) { db.graph("g"); }
        db.kv_store("b");
    }
}
#[cfg(test)]
mod inline { fn x(db: &Database) { db.resolve("a"); } }
"#;
    let seen: Vec<_> = scan_source("src/server/x.rs", source)
        .into_iter()
        .map(|finding| (finding.function, finding.pattern))
        .collect();
    assert_eq!(
        seen,
        [
            ("handler".to_owned(), ".collection(".to_owned()),
            ("nested".to_owned(), ".graph(".to_owned()),
            ("handler".to_owned(), ".kv_store(".to_owned()),
        ]
    );
}

#[test]
fn module_declarations_carry_their_cfg_and_path() {
    let source = r#"
#![allow(dead_code)]
mod grpc;
#[cfg(test)]
mod pins_recall;
#[cfg(test)]
pub(super) mod replay;
#[cfg(test)]
#[path = "../pins_recall2.rs"]
mod pins_recall2;
#[cfg(feature = "grpc")]
#[path = "grpc_serving.rs"]
mod serving_grpc;
#[path = "../bin/tool_support/graph_json.rs"]
#[allow(dead_code)]
mod graph_json;
#[cfg(test)]
mod inline { mod hidden; }
fn not_a_module() { mod local; }
"#;
    let declared: Vec<_> = parse_modules(source)
        .into_iter()
        .map(|module| (module.name, module.test, module.path))
        .collect();
    assert_eq!(
        declared,
        [
            ("grpc".to_owned(), false, None),
            ("pins_recall".to_owned(), true, None),
            ("replay".to_owned(), true, None),
            (
                "pins_recall2".to_owned(),
                true,
                Some("../pins_recall2.rs".to_owned())
            ),
            (
                "serving_grpc".to_owned(),
                false,
                Some("grpc_serving.rs".to_owned())
            ),
            (
                "graph_json".to_owned(),
                false,
                Some("../bin/tool_support/graph_json.rs".to_owned())
            ),
        ]
    );
    assert_eq!(
        module_files("src/server/memory.rs", "pins_recall"),
        [
            "src/server/memory/pins_recall.rs".to_owned(),
            "src/server/memory/pins_recall/mod.rs".to_owned()
        ]
    );
    assert_eq!(
        module_files("src/server.rs", "kv"),
        [
            "src/server/kv.rs".to_owned(),
            "src/server/kv/mod.rs".to_owned()
        ]
    );
    assert_eq!(
        join_relative("src/server/memory/routes", "../pins_recall2.rs"),
        "src/server/memory/pins_recall2.rs"
    );
}

#[test]
fn a_cfg_that_requires_test_makes_an_item_or_a_module_test_only() {
    for attribute in [
        "cfg(test)",
        "cfg(all(test,feature=))",
        "cfg(all(feature=,test))",
        "cfg(all(all(test,unix),feature=))",
    ] {
        assert!(requires_test(attribute), "{attribute}");
    }
    for attribute in [
        "cfg(feature=)",
        "cfg(not(test))",
        "cfg(any(test,feature=))",
        "cfg(all(feature=,not(test)))",
        "cfg(all(testing,feature=))",
        "cfg_attr(test,derive(Debug))",
        "test",
        "path=",
    ] {
        assert!(!requires_test(attribute), "{attribute}");
    }
    let source = r##"
#[cfg(all(test, feature = "grpc"))]
mod published;
#[cfg(any(test, feature = "grpc"))]
mod either;
#[cfg(not(test))]
mod production;
#[cfg(all(test, feature = "grpc"))]
fn setup(db: &Database) { db.create_graph("notes"); }
#[cfg(any(test, feature = "grpc"))]
fn shared(db: &Database) { db.create_graph("counted"); }
"##;
    let declared: Vec<_> = parse_modules(source)
        .into_iter()
        .map(|module| (module.name, module.test))
        .collect();
    assert_eq!(
        declared,
        [
            ("published".to_owned(), true),
            ("either".to_owned(), false),
            ("production".to_owned(), false),
        ]
    );
    let seen: Vec<_> = found(source)
        .into_iter()
        .map(|(function, pattern, _)| (function, pattern))
        .collect();
    assert_eq!(
        seen,
        [("shared".to_owned(), ".create_graph(".to_owned())],
        "only an item that compiles under test alone is left out of the scan"
    );
}

#[test]
fn files_only_test_code_includes_are_found_through_paths_and_nesting() {
    let sources: BTreeMap<String, String> = [
        ("src/server.rs", "mod grpc;\n#[cfg(test)]\nmod tests;\n"),
        (
            "src/server/tests.rs",
            "mod first;\n#[path = \"tests/extra.rs\"]\nmod second;\n",
        ),
        ("src/server/tests/first.rs", "mod deep;\n"),
        ("src/server/tests/first/deep.rs", ""),
        ("src/server/tests/extra.rs", ""),
        ("src/server/grpc.rs", "mod kv;\n"),
        ("src/server/grpc/kv.rs", ""),
        (
            "src/server/memory/routes/recall.rs",
            "#[cfg(test)]\n#[path = \"../pins.rs\"]\nmod pins;\n",
        ),
        ("src/server/memory/pins.rs", "mod helper;\n"),
        ("src/server/memory/pins/helper.rs", ""),
    ]
    .into_iter()
    .map(|(file, source)| (file.to_owned(), source.to_owned()))
    .collect();
    assert_eq!(
        test_only_files(&sources).into_iter().collect::<Vec<_>>(),
        [
            "src/server/memory/pins.rs",
            "src/server/memory/pins/helper.rs",
            "src/server/tests.rs",
            "src/server/tests/extra.rs",
            "src/server/tests/first.rs",
            "src/server/tests/first/deep.rs",
        ]
    );
}

#[test]
fn the_allowlist_must_match_and_stay_justified() {
    let finding = |function: &str, pattern: &str| Finding {
        file: "src/server/a.rs".into(),
        function: function.into(),
        pattern: pattern.into(),
        line: 7,
    };
    let findings = [
        finding("covered", "db.resolve("),
        finding("bare", ".collection("),
    ];
    let reason = "trusted: the name comes from the operator's configuration";
    // A finding with no entry fails.
    let problems = judge(
        &findings,
        &[("src/server/a.rs", "covered", "db.resolve(", reason)],
    );
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("src/server/a.rs:7 in fn bare"));
    // An entry that matches nothing is stale.
    let problems = judge(
        &findings[..1],
        &[
            ("src/server/a.rs", "covered", "db.resolve(", reason),
            ("src/server/a.rs", "gone", "db.resolve(", reason),
        ],
    );
    assert_eq!(problems.len(), 1);
    assert!(problems[0].starts_with("stale ALLOWED entry"));
    // An entry needs a real justification.
    let problems = judge(
        &findings[..1],
        &[("src/server/a.rs", "covered", "db.resolve(", "ok")],
    );
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("justification"));
    // A matching, justified list is clean; the entry matches file, function
    // and pattern together.
    assert!(
        judge(
            &findings,
            &[
                ("src/server/a.rs", "covered", "db.resolve(", reason),
                ("src/server/a.rs", "bare", ".collection(", reason),
            ]
        )
        .is_empty()
    );
    let problems = judge(
        &findings[..1],
        &[("src/server/b.rs", "covered", "db.resolve(", reason)],
    );
    assert_eq!(problems.len(), 2, "{problems:?}");
}
