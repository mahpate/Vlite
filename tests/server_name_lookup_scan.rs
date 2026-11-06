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
                erase(&mut out, i, end);
                i = end;
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(bytes.len());
                erase(&mut out, start, i);
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
                    erase(&mut out, i, i + 1 + length);
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

/// Removes every `#[cfg(test)]` item from blanked source, keeping newlines.
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
        if attribute != "cfg(test)" {
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
                    .any(|(compact, ..)| compact == "cfg(test)");
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

// ------------------------------------------------------- the gRPC adapters
//
// `Adapter::call` is the one pipeline of the gRPC transport: admission,
// authentication, rate limit, static scope policy, the core inside the
// hidden-attempt tracker, and the request record. The by-name scan above sees
// every lookup of a handler, but not an RPC that skips the pipeline and reaches
// the engine by an ID, so the structure is checked here: every RPC of every
// service has exactly one handler; each handler makes exactly one call through
// the pipeline (itself, or through an inherent method that makes exactly one)
// under the route label of its own RPC; and nothing outside that call runs
// engine code or repeats a pipeline step. The only other functions that run
// engine code are the continuations of the two streams, which run after the
// opening call and so authenticate again themselves.

/// The services the gRPC adapter implements, by the name the proto gives them.
const GRPC_SERVICES: &[&str] = &["VLite", "Kv"];

/// Functions of the gRPC files, other than handlers, that may run engine code,
/// each with what its body must do itself: authenticate again (so revocation
/// applies mid-stream) and take its view of the database from the request
/// scope.
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

/// The items declared directly between `from` and `to` of blanked code, as byte
/// ranges, with their attributes skipped.
fn grpc_items(code: &str, from: usize, to: usize) -> Vec<(usize, usize)> {
    let bytes = code.as_bytes();
    let mut out = Vec::new();
    let mut at = from;
    while at < to {
        if bytes[at].is_ascii_whitespace() {
            at += 1;
        } else if bytes[at] == b'#' {
            let Some(open) = code[at..].find('[').map(|offset| at + offset) else {
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
            at = close + 1;
        } else {
            let end = item_end(bytes, at).min(to);
            out.push((at, end));
            at = end;
        }
    }
    out
}

/// A function and the byte offsets of the braces of its body.
struct GrpcFn {
    name: String,
    open: usize,
    close: usize,
}

struct GrpcImpl {
    /// The trait implemented, `None` for an inherent block.
    service: Option<String>,
    ty: String,
    members: Vec<GrpcFn>,
}

fn grpc_fn(code: &str, start: usize, end: usize) -> Option<GrpcFn> {
    let item = &code[start..end];
    let brace = item.find('{')?;
    let head = spanned_tokens(&item[..brace], start);
    let at = head
        .iter()
        .position(|(token, _)| matches!(token, Token::Ident(word) if word == "fn"))?;
    let Some((Token::Ident(name), _)) = head.get(at + 1) else {
        return None;
    };
    let open = start + brace;
    Some(GrpcFn {
        name: name.clone(),
        open,
        close: matching_brace(code.as_bytes(), open),
    })
}

/// The implementations and free functions declared at the top level.
fn grpc_parse(code: &str) -> (Vec<GrpcImpl>, Vec<GrpcFn>) {
    let mut impls = Vec::new();
    let mut free = Vec::new();
    for (start, end) in grpc_items(code, 0, code.len()) {
        let item = &code[start..end];
        let head_end = item.find('{').unwrap_or(item.len());
        let head = spanned_tokens(&item[..head_end], start);
        let words: Vec<&str> = head
            .iter()
            .filter_map(|(token, _)| match token {
                Token::Ident(word) => Some(word.as_str()),
                Token::Punct(_) => None,
            })
            .collect();
        if words.first() == Some(&"impl") {
            let (service, ty) = match words.iter().position(|word| *word == "for") {
                Some(at) => (
                    words.get(at.wrapping_sub(1)).map(|word| (*word).to_owned()),
                    words.get(at + 1),
                ),
                None => (None, words.get(1)),
            };
            let open = start + head_end;
            let close = matching_brace(code.as_bytes(), open);
            let members = grpc_items(code, open + 1, close)
                .into_iter()
                .filter_map(|(from, to)| grpc_fn(code, from, to))
                .collect();
            impls.push(GrpcImpl {
                service,
                ty: ty.map(|word| (*word).to_owned()).unwrap_or_default(),
                members,
            });
        } else if let Some(function) = grpc_fn(code, start, end) {
            free.push(function);
        }
    }
    (impls, free)
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

/// What a body does outside `skip` (the arguments of its pipeline call) that
/// belongs to the pipeline or the engine: a `.run*(..)` or `.scope(..)` call,
/// the `database` field, a pipeline step.
fn grpc_outside(code: &str, function: &GrpcFn, skip: &[(usize, usize)]) -> Vec<String> {
    let tokens = spanned_tokens(&code[function.open..=function.close], function.open);
    let mut out = Vec::new();
    for (index, (token, at)) in tokens.iter().enumerate() {
        if skip.iter().any(|(from, to)| (*from..=*to).contains(at)) {
            continue;
        }
        let Token::Ident(word) = token else {
            continue;
        };
        let after_dot = index > 0 && matches!(tokens[index - 1].0, Token::Punct('.'));
        let called = matches!(tokens.get(index + 1), Some((Token::Punct('('), _)));
        let engine = after_dot
            && ((called && (word.starts_with("run") || word == "scope")) || word == "database");
        let step = !after_dot && called && GRPC_PIPELINE.contains(&word.as_str());
        if engine || step {
            out.push(word.clone());
        }
    }
    out
}

/// The route labels a body spells, read from the unblanked source.
fn grpc_labels(source: &str, function: &GrpcFn) -> Vec<String> {
    let body = &source[function.open..=function.close];
    body.match_indices("\"grpc:/vlite.v1.")
        .filter_map(|(at, _)| {
            let rest = &body[at + 1..];
            rest.find('"').map(|end| rest[..end].to_owned())
        })
        .collect()
}

/// The route a call is opened under: the literal it spells first, or the one
/// of the `const` of the handler that it names.
fn grpc_opening_label(
    source: &str,
    function: &GrpcFn,
    open: usize,
    close: usize,
) -> Option<String> {
    let first = source[open + 1..close].trim_start();
    let literal = |text: &str| text.strip_prefix('"')?.split('"').next().map(str::to_owned);
    if first.starts_with('"') {
        return literal(first);
    }
    let name: String = first
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let declaration = format!("const {name}: &str = ");
    let body = &source[function.open..=function.close];
    let at = body.find(&declaration)?;
    literal(&body[at + declaration.len()..])
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

#[derive(Default)]
struct GrpcScan {
    problems: Vec<String>,
    /// The RPCs the handlers of each service implement.
    handlers: BTreeMap<String, BTreeSet<String>>,
    /// The functions allowed to run engine code that the file defines.
    runners: BTreeSet<String>,
}

/// Every structural problem of one gRPC adapter file.
fn grpc_structure(
    file: &str,
    source: &str,
    services: &[&str],
    runners: &[(&str, &[&str])],
) -> GrpcScan {
    let code = without_test_items(&blank(source));
    let (impls, free) = grpc_parse(&code);
    let mut scan = GrpcScan::default();
    let inherent: BTreeMap<&str, &GrpcFn> = impls
        .iter()
        .filter(|implementation| implementation.service.is_none() && implementation.ty == "Adapter")
        .flat_map(|implementation| implementation.members.iter())
        .map(|member| (member.name.as_str(), member))
        .collect();
    let makes_pipeline_call = |function: &GrpcFn| -> (usize, usize) {
        let calls = grpc_calls(&code, function);
        let direct = calls.iter().filter(|(method, ..)| method == "call").count();
        let delegated = calls
            .iter()
            .filter(|(method, ..)| method != "call" && inherent.contains_key(method.as_str()))
            .count();
        (direct, delegated)
    };
    let mut helpers: Vec<&GrpcFn> = free.iter().collect();
    for implementation in impls
        .iter()
        .filter(|implementation| implementation.ty == "Adapter")
    {
        let Some(service) = &implementation.service else {
            helpers.extend(
                implementation
                    .members
                    .iter()
                    .filter(|member| member.name != "call"),
            );
            continue;
        };
        if !services.contains(&service.as_str()) {
            scan.problems.push(format!(
                "{file}: `impl {service} for Adapter` is a service this check does not know; add it \
                 to GRPC_SERVICES, and give every RPC a handler that goes through Adapter::call"
            ));
            continue;
        }
        for handler in &implementation.members {
            let rpc = grpc_camel(&handler.name);
            let own = format!("grpc:/vlite.v1.{service}/{rpc}");
            scan.handlers
                .entry(service.clone())
                .or_default()
                .insert(rpc);
            let calls = grpc_calls(&code, handler);
            let (direct, delegated) = makes_pipeline_call(handler);
            let mut skip = Vec::new();
            match (direct, delegated) {
                (1, 0) => {
                    // A stream may repeat its own route for its continuation's
                    // failure record, but never another handler's.
                    let labels = grpc_labels(source, handler);
                    let opening = calls.iter().find(|(method, ..)| method == "call").and_then(
                        |(_, open, close)| grpc_opening_label(source, handler, *open, *close),
                    );
                    if opening.as_deref() != Some(own.as_str())
                        || labels.iter().any(|label| *label != own)
                    {
                        scan.problems.push(format!(
                            "{file}: handler `{}` must open its call under its own route `{own}` \
                             and name no other route, found {labels:?}",
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
                    let (method, open, close) = calls
                        .iter()
                        .find(|(method, ..)| inherent.contains_key(method.as_str()))
                        .unwrap();
                    let target = inherent[method.as_str()];
                    if makes_pipeline_call(target) != (1, 0) {
                        scan.problems.push(format!(
                            "{file}: `{method}`, which handler `{}` delegates to, must make \
                             exactly one call through the pipeline",
                            handler.name
                        ));
                    }
                    if !grpc_labels(source, target).contains(&own) {
                        scan.problems.push(format!(
                            "{file}: `{method}` must name the route `{own}` of the handler `{}` \
                             that delegates to it",
                            handler.name
                        ));
                    }
                    skip.push((*open, *close));
                }
                (0, 0) => scan.problems.push(format!(
                    "{file}: handler `{}` makes no call through the pipeline (`self.call(..)`): \
                     it would answer without authentication, scope policy or a request record",
                    handler.name
                )),
                _ => scan.problems.push(format!(
                    "{file}: handler `{}` must make exactly one call through the pipeline, \
                     found {direct} direct and {delegated} delegated",
                    handler.name
                )),
            }
            for word in grpc_outside(&code, handler, &skip) {
                scan.problems.push(format!(
                    "{file}: handler `{}` takes `{word}` outside its pipeline call; engine \
                     access and pipeline steps belong to Adapter::call and its core closure",
                    handler.name
                ));
            }
        }
    }
    for function in helpers {
        let skip: Vec<(usize, usize)> = grpc_calls(&code, function)
            .into_iter()
            .filter(|(method, ..)| method == "call")
            .map(|(_, open, close)| (open, close))
            .collect();
        let outside = grpc_outside(&code, function, &skip);
        if outside.is_empty() {
            continue;
        }
        let Some((_, required)) = runners.iter().find(|(name, _)| *name == function.name) else {
            scan.problems.push(format!(
                "{file}: `{}` takes `{}` and is neither a handler nor one of GRPC_RUNNERS; engine \
                 access and pipeline steps belong to Adapter::call",
                function.name, outside[0]
            ));
            continue;
        };
        scan.runners.insert(function.name.clone());
        let body: BTreeSet<String> =
            spanned_tokens(&code[function.open..=function.close], function.open)
                .into_iter()
                .filter_map(|(token, _)| match token {
                    Token::Ident(word) => Some(word),
                    Token::Punct(_) => None,
                })
                .collect();
        for step in *required {
            if !body.contains(*step) {
                scan.problems.push(format!(
                    "{file}: the stream runner `{}` must also take `{step}` itself",
                    function.name
                ));
            }
        }
    }
    scan
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
    let mut problems = Vec::new();
    let mut handlers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut runners = BTreeSet::new();
    for (file, source) in &files {
        let scan = grpc_structure(file, source, GRPC_SERVICES, GRPC_RUNNERS);
        problems.extend(scan.problems);
        for (service, rpcs) in scan.handlers {
            handlers.entry(service).or_default().extend(rpcs);
        }
        runners.extend(scan.runners);
    }
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
