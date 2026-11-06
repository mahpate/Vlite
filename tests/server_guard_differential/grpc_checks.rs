//! Checks around the gRPC matrix (lane F3, slice S1): the inventory read from
//! the proto (its rows, its exempt RPCs and the classification of request
//! fields that look like names), the calls that must take effect, the digest's
//! sensitivity to a gRPC write, the markers a holder reads back, lists and
//! feeds, grants, the RPCs that span the database, import jobs, console
//! sessions, the audit trail of a hidden attempt and the reader of server
//! streams.

use super::*;

const PROTO: &str = include_str!("../../proto/vlite/v1/vlite.proto");

fn row_named<'a>(rows: &'a [GRow], rpc: &str) -> &'a GRow {
    rows.iter()
        .find(|row| row.rpc == rpc)
        .unwrap_or_else(|| panic!("no row for {rpc}"))
}

/// The principals that authenticate with a bearer token.
fn token_principals() -> Vec<&'static Principal> {
    let all: &'static [Principal; 12] = &PRINCIPALS;
    all.iter()
        .filter(|principal| principal.session.is_none())
        .collect()
}

// ------------------------------------------------------------- the inventory

struct ProtoRpc {
    service: String,
    name: String,
    request: String,
}

struct ProtoModel {
    rpcs: Vec<ProtoRpc>,
    /// Every message and its fields as `(type, name)`; a nested message is
    /// keyed by its dotted name (`Operation.Put`). A map is recorded as the
    /// type `map`: its key and value types are not followed.
    messages: BTreeMap<String, Vec<(String, String)>>,
}

impl ProtoModel {
    /// The message that a field of type `ty`, declared in `scope`, refers to,
    /// looked up the way protobuf does: in `scope`, then in each message that
    /// encloses it, then at the top level. Scalars and enums are no message.
    fn resolve(&self, scope: &str, ty: &str) -> Option<String> {
        let mut scope = scope;
        loop {
            let candidate = if scope.is_empty() {
                ty.to_owned()
            } else {
                format!("{scope}.{ty}")
            };
            if self.messages.contains_key(&candidate) {
                return Some(candidate);
            }
            if scope.is_empty() {
                return None;
            }
            scope = scope.rsplit_once('.').map_or("", |(outer, _)| outer);
        }
    }
}

fn proto_tokens(source: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for line in source.lines() {
        let line = line.split("//").next().unwrap_or_default();
        let mut word = String::new();
        for c in line.chars() {
            if c.is_whitespace() || "{};=()<>,".contains(c) {
                if !word.is_empty() {
                    tokens.push(std::mem::take(&mut word));
                }
                if !c.is_whitespace() {
                    tokens.push(c.to_string());
                }
            } else {
                word.push(c);
            }
        }
        if !word.is_empty() {
            tokens.push(word);
        }
    }
    tokens
}

/// The services, their RPCs and the fields of the messages, read from the proto
/// that the server is generated from.
fn proto_model() -> ProtoModel {
    read_proto(PROTO)
}

/// The message a field being read belongs to: the innermost one that is open.
fn innermost_message<'a>(blocks: &'a [(&str, String)]) -> Option<&'a String> {
    blocks
        .iter()
        .rev()
        .find(|(kind, _)| *kind == "message")
        .map(|(_, name)| name)
}

/// The same for any proto source, so that the checks around the inventory can
/// be tried on a proto of their own.
fn read_proto(source: &str) -> ProtoModel {
    let tokens = proto_tokens(source);
    let mut rpcs = Vec::new();
    let mut messages: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut blocks: Vec<(&str, String)> = Vec::new();
    let mut at = 0;
    while at < tokens.len() {
        let word = tokens[at].as_str();
        let inside = blocks.last().map(|(kind, _)| *kind);
        match word {
            "service" if blocks.is_empty() => {
                blocks.push(("service", tokens[at + 1].clone()));
                at += 3;
            }
            "message" => {
                let name = &tokens[at + 1];
                let full = match innermost_message(&blocks) {
                    Some(outer) => format!("{outer}.{name}"),
                    None => name.clone(),
                };
                messages.entry(full.clone()).or_default();
                blocks.push(("message", full));
                at += 3;
            }
            "oneof" | "enum" => {
                let kind = if word == "oneof" && inside == Some("message") {
                    "oneof"
                } else {
                    "other"
                };
                blocks.push((kind, String::new()));
                at += 3;
            }
            "}" => {
                blocks.pop();
                at += 1;
            }
            "rpc" if inside == Some("service") => {
                let mut request = at + 3;
                if tokens[request] == "stream" {
                    request += 1;
                }
                rpcs.push(ProtoRpc {
                    service: blocks.last().unwrap().1.clone(),
                    name: tokens[at + 1].clone(),
                    request: tokens[request].clone(),
                });
                while tokens[at] != ";" {
                    at += 1;
                }
                at += 1;
            }
            _ => {
                let start = at;
                while tokens[at] != ";" {
                    at += 1;
                }
                if matches!(inside, Some("message" | "oneof")) {
                    let owner = innermost_message(&blocks).unwrap();
                    let statement: Vec<&str> =
                        tokens[start..at].iter().map(String::as_str).collect();
                    match statement.first().copied() {
                        Some("reserved" | "option" | "extensions") | None => {}
                        Some("map") => {
                            let close = statement.iter().position(|word| *word == ">").unwrap();
                            messages
                                .get_mut(owner)
                                .unwrap()
                                .push(("map".to_owned(), statement[close + 1].to_owned()));
                        }
                        Some(_) => {
                            let words: Vec<&str> = statement
                                .iter()
                                .copied()
                                .filter(|word| !matches!(*word, "repeated" | "optional"))
                                .collect();
                            messages
                                .get_mut(owner)
                                .unwrap()
                                .push((words[0].to_owned(), words[1].to_owned()));
                        }
                    }
                }
                at += 1;
            }
        }
    }
    ProtoModel { rpcs, messages }
}

/// Request fields that name a collection although they are not called one.
const NAME_ALIASES: &[(&str, &str)] = &[
    ("GetCollectionRequest", "name"),
    ("CreateCollectionRequest", "name"),
    ("DropCollectionRequest", "name"),
];

/// Request fields that look like the name of a collection or a graph (a string
/// called `name`, or with `collection` or `graph` in its name) and are none, as
/// `(message, field, reason)`, a nested message by its dotted name. A field
/// that looks like a name is either name-bearing (`NAME_FIELDS`,
/// `NAME_ALIASES`), so that a row varies it, or listed here with its reason:
/// `name_like_request_fields_are_classified` fails for any other, so that a new
/// `source_collection` cannot slip past the inventory because it is not called
/// `collection`.
const NOT_A_NAME: &[(&str, &str, &str)] = &[
    (
        "NamedVectorSpec",
        "name",
        "the vector space being configured in the addressed collection",
    ),
    (
        "NamedVector",
        "name",
        "a dense vector space of the addressed collection",
    ),
    (
        "SparseVector",
        "name",
        "a sparse vector space of the addressed collection",
    ),
    (
        "PutVectorVersionRequest",
        "name",
        "the vector space of the addressed collection that gets a version",
    ),
    (
        "SearchNamedRequest",
        "name",
        "the vector space of the addressed collection that is searched",
    ),
    (
        "SearchSparseRequest",
        "name",
        "the sparse vector space of the addressed collection that is searched",
    ),
];

/// Whether a request field names a collection or a graph by the rule of the
/// inventory: a string called as `NAME_FIELDS` says, or an alias.
fn is_name_bearing(message: &str, ty: &str, field: &str) -> bool {
    (ty == "string" && NAME_FIELDS.contains(&field)) || NAME_ALIASES.contains(&(message, field))
}

/// Where a request message names a collection or a graph, directly or through
/// a message it carries: `collection`, `address.collection`.
fn named_fields(
    model: &ProtoModel,
    message: &str,
    prefix: &str,
    seen: &mut Vec<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (ty, field) in model.messages.get(message).into_iter().flatten() {
        let path = format!("{prefix}{field}");
        if is_name_bearing(message, ty, field) {
            out.push(path);
        } else if let Some(inner) = model
            .resolve(message, ty)
            .filter(|inner| !seen.contains(inner))
        {
            seen.push(inner.clone());
            out.extend(named_fields(model, &inner, &format!("{path}."), seen));
            seen.pop();
        }
    }
    out
}

/// The RPCs whose request names a collection or a graph, with where.
fn name_bearing_rpcs(model: &ProtoModel) -> BTreeMap<String, Vec<String>> {
    let mut bearing = BTreeMap::new();
    for rpc in &model.rpcs {
        let fields = named_fields(model, &rpc.request, "", &mut vec![rpc.request.clone()]);
        if !fields.is_empty() {
            bearing.insert(format!("{}/{}", rpc.service, rpc.name), fields);
        }
    }
    bearing
}

/// The RPCs, exempt ones left out, whose request names a collection or a graph
/// in more than one field. A row varies one name at a time, so no row can cover
/// such an RPC; the ones that a memory binding addresses are exempt and must be
/// covered by `tests/grpc_memory.rs` instead (S2).
fn names_in_several_fields(
    model: &ProtoModel,
    exempt: &BTreeSet<String>,
) -> Vec<(String, Vec<String>)> {
    name_bearing_rpcs(model)
        .into_iter()
        .filter(|(rpc, fields)| !exempt.contains(rpc) && fields.len() > 1)
        .collect()
}

/// A field whose name suggests a collection or a graph, in any letter case.
fn looks_like_a_name(field: &str) -> bool {
    let field = field.to_ascii_lowercase();
    field == "name" || field.contains("collection") || field.contains("graph")
}

/// The request of every RPC that is not exempt and every message those carry,
/// nested ones included.
fn carried_messages(model: &ProtoModel, exempt: &BTreeSet<String>) -> BTreeSet<String> {
    let mut pending: Vec<String> = model
        .rpcs
        .iter()
        .filter(|rpc| !exempt.contains(&format!("{}/{}", rpc.service, rpc.name)))
        .filter_map(|rpc| model.resolve("", &rpc.request))
        .collect();
    let mut found = BTreeSet::new();
    while let Some(message) = pending.pop() {
        if found.insert(message.clone()) {
            pending.extend(
                model.messages[&message]
                    .iter()
                    .filter_map(|(ty, _)| model.resolve(&message, ty)),
            );
        }
    }
    found
}

/// The string and map fields of the request of every RPC that is not exempt,
/// and of the messages it carries, that look like the name of a collection or a
/// graph and are neither name-bearing nor in `classified`, as `Message.field`.
fn unclassified_name_fields(
    model: &ProtoModel,
    exempt: &BTreeSet<String>,
    classified: &[(&str, &str, &str)],
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for message in carried_messages(model, exempt) {
        for (ty, field) in &model.messages[&message] {
            let listed = classified
                .iter()
                .any(|(owner, name, _)| *owner == message.as_str() && *name == field.as_str());
            if matches!(ty.as_str(), "string" | "map")
                && looks_like_a_name(field)
                && !is_name_bearing(&message, ty, field)
                && !listed
            {
                found.insert(format!("{message}.{field}"));
            }
        }
    }
    found
}

/// The RPCs of the `EXEMPT` table, each with its reason.
fn exempt_rpcs() -> BTreeSet<String> {
    EXEMPT
        .iter()
        .map(|(rpc, reason)| {
            assert!(!reason.is_empty(), "{rpc} is exempt without a reason");
            (*rpc).to_owned()
        })
        .collect()
}

/// The exempt RPCs that also have a row. An RPC is in the matrix or exempt, not
/// both: an `EXEMPT` entry beside a row would let a request that the row no
/// longer varies pass the one-name rule.
fn listed_twice(exempt: &BTreeSet<String>, covered: &BTreeSet<String>) -> Vec<String> {
    exempt.intersection(covered).cloned().collect()
}

fn uncovered(
    declared: &BTreeSet<String>,
    exempt: &BTreeSet<String>,
    covered: &BTreeSet<String>,
) -> Vec<String> {
    declared
        .iter()
        .filter(|rpc| !covered.contains(*rpc) && !exempt.contains(*rpc))
        .cloned()
        .collect()
}

#[test]
fn every_rpc_has_a_row() {
    let model = proto_model();
    let declared: BTreeSet<String> = model
        .rpcs
        .iter()
        .map(|rpc| format!("{}/{}", rpc.service, rpc.name))
        .collect();
    assert!(
        declared.len() >= 39,
        "the proto walker found only {} RPCs: {declared:?}",
        declared.len()
    );
    for expected in [
        "VLite/GetCollection",
        "VLite/StreamImport",
        "Kv/ListKvEntries",
    ] {
        assert!(declared.contains(expected), "the walker lost {expected}");
    }
    let table = rows();
    let globals = global_rows();
    let named: BTreeSet<String> = table.iter().map(|row| row.rpc.to_owned()).collect();
    let global: BTreeSet<String> = globals.iter().map(|row| row.rpc.to_owned()).collect();
    let exempt = exempt_rpcs();
    let covered: BTreeSet<String> = named.union(&global).cloned().collect();

    let missing = uncovered(&declared, &exempt, &covered);
    assert!(
        missing.is_empty(),
        "an RPC needs a row in rows() (it names a collection) or global_rows() (it names none) \
         before it ships; an RPC that a memory binding addresses instead of a collection or a \
         graph name goes in the EXEMPT table of grpc_leg.rs with its reason, and \
         tests/grpc_memory.rs must cover it (S2): {missing:?}"
    );
    let orphans: Vec<_> = covered.difference(&declared).collect();
    assert!(
        orphans.is_empty(),
        "rows for RPCs the proto lacks: {orphans:?}"
    );
    let stale: Vec<_> = exempt.difference(&declared).collect();
    assert!(stale.is_empty(), "EXEMPT entries without an RPC: {stale:?}");
    let twice = listed_twice(&exempt, &covered);
    assert!(
        twice.is_empty(),
        "an RPC is in the matrix or EXEMPT, not both: {twice:?}"
    );
    let both: Vec<_> = named.intersection(&global).collect();
    assert!(both.is_empty(), "rows in both tables: {both:?}");

    // The rows and the proto agree on which RPCs name a collection.
    for (message, field) in NAME_ALIASES {
        assert!(
            model
                .messages
                .get(*message)
                .is_some_and(|fields| fields.iter().any(|(_, name)| name == field)),
            "{message}.{field} is not in the proto"
        );
    }
    let bearing = name_bearing_rpcs(&model);
    let bearing_rpcs: BTreeSet<String> = bearing
        .keys()
        .filter(|rpc| !exempt.contains(*rpc))
        .cloned()
        .collect();
    assert_eq!(
        bearing_rpcs, named,
        "the RPCs whose request names a collection must be exactly the rows of rows()"
    );
    // One row varies one name; an RPC that a memory binding addresses may carry
    // several collection paths and is exempt.
    let several = names_in_several_fields(&model, &exempt);
    assert!(
        several.is_empty(),
        "{several:?}: an RPC that names a collection in several fields needs a row that varies \
         each of them, or, when a memory binding addresses it, an EXEMPT entry that \
         tests/grpc_memory.rs must cover (S2)"
    );
    // The RPCs that address `default` when their collection is empty name it
    // in a field called `collection`.
    for rpc in LEGACY {
        assert_eq!(
            bearing.get(*rpc).map(Vec::as_slice),
            Some(&["collection".to_owned()][..]),
            "{rpc} is listed as legacy"
        );
    }

    // Losing the row of one RPC must be seen.
    for omitted in declared.difference(&exempt) {
        let mutant: BTreeSet<String> = covered
            .iter()
            .filter(|rpc| *rpc != omitted)
            .cloned()
            .collect();
        assert_eq!(
            uncovered(&declared, &exempt, &mutant),
            vec![omitted.clone()],
            "the inventory must notice a missing row for {omitted}"
        );
    }
}

#[test]
fn name_like_request_fields_are_classified() {
    let model = proto_model();
    let exempt = exempt_rpcs();
    let unclassified = unclassified_name_fields(&model, &exempt, NOT_A_NAME);
    assert!(
        unclassified.is_empty(),
        "{unclassified:?} look like the name of a collection or a graph (a string called `name`, \
         or with `collection` or `graph` in its name), but no row varies them and none is \
         classified. If one is a name, add it to NAME_ALIASES and give its RPC a row that varies \
         it; if it is not, add it to NOT_A_NAME with the reason"
    );
    // The table holds what the proto needs and no more: without it, the fields
    // that are found are its entries.
    let listed: BTreeSet<String> = NOT_A_NAME
        .iter()
        .map(|(message, field, reason)| {
            assert!(
                !reason.is_empty(),
                "{message}.{field} is not a name without a reason"
            );
            format!("{message}.{field}")
        })
        .collect();
    assert_eq!(
        unclassified_name_fields(&model, &exempt, &[]),
        listed,
        "NOT_A_NAME must list exactly the name-like request fields that are not names; remove \
         an entry whose field is gone, renamed or now treated as a name"
    );
}

/// A proto shaped like the `Memory` service that is to come, next to ordinary
/// RPCs: requests that a binding addresses and that carry two collection
/// paths, requests that name a collection in several fields, and fields that
/// only look like names, in a nested message, in a oneof, repeated and in a map.
const SYNTHETIC: &str = r#"
service Memory {
  rpc Remember(RememberRequest) returns (Reply);
  rpc Merge(MergeRequest) returns (Reply);
}
service Plain {
  rpc Copy(CopyRequest) returns (Reply);
  rpc Fetch(FetchRequest) returns (Reply);
  rpc Grow(stream GrowRequest) returns (Reply);
}
message Path { string collection = 1; }
message RememberRequest { Path source = 1; Path evidence = 2; }
message MergeRequest { string collection = 1; string graph = 2; string source_collection = 3; }
message CopyRequest { string collection = 1; string graph = 2; string source_collection = 3; }
message FetchRequest { string collection = 1; string evidenceGraph = 2; }
message GrowRequest {
  message Inner {
    oneof pick { string graph_name = 1; Path path = 2; }
    repeated string evidence_collections = 3;
  }
  Inner inner = 1;
  string name = 2;
  uint32 collection_id = 3;
  map<string, string> graphs = 4;
}
message Reply {}
"#;

fn rpc_ids(rpcs: &[&str]) -> BTreeSet<String> {
    rpcs.iter().map(|rpc| (*rpc).to_owned()).collect()
}

fn fields_of<'a>(model: &'a ProtoModel, message: &str) -> Vec<(&'a str, &'a str)> {
    model.messages[message]
        .iter()
        .map(|(ty, field)| (ty.as_str(), field.as_str()))
        .collect()
}

#[test]
fn an_exempt_rpc_may_name_a_collection_in_several_fields() {
    let model = read_proto(SYNTHETIC);
    let several = |exempt: &[&str]| -> Vec<String> {
        names_in_several_fields(&model, &rpc_ids(exempt))
            .into_iter()
            .map(|(rpc, _)| rpc)
            .collect()
    };
    // Without an exemption every RPC that names a collection or a graph twice
    // trips the rule, also when it carries one of the names in a message.
    assert_eq!(
        several(&[]),
        ["Memory/Merge", "Memory/Remember", "Plain/Copy"]
    );
    assert_eq!(
        name_bearing_rpcs(&model)["Memory/Remember"],
        ["source.collection", "evidence.collection"]
    );
    // An exempt RPC is skipped, one that is not still trips the rule.
    assert_eq!(
        several(&["Memory/Merge", "Memory/Remember"]),
        ["Plain/Copy"]
    );
    assert!(several(&["Memory/Merge", "Memory/Remember", "Plain/Copy"]).is_empty());
}

#[test]
fn a_name_like_field_nobody_classified_is_noticed() {
    let model = read_proto(SYNTHETIC);
    let found = |exempt: &[&str], classified: &[(&str, &str, &str)]| -> Vec<String> {
        unclassified_name_fields(&model, &rpc_ids(exempt), classified)
            .into_iter()
            .collect()
    };
    // A field that looks like a name without being called `collection` is
    // found in a request and in the messages it carries (nested ones, oneofs,
    // repeated fields and maps included, in any letter case); a field the rule
    // knows is not, and neither is an id.
    assert_eq!(
        found(&[], &[]),
        [
            "CopyRequest.source_collection",
            "FetchRequest.evidenceGraph",
            "GrowRequest.Inner.evidence_collections",
            "GrowRequest.Inner.graph_name",
            "GrowRequest.graphs",
            "GrowRequest.name",
            "MergeRequest.source_collection",
        ]
    );
    // The requests of exempt RPCs are left to the file that covers them.
    assert!(!found(&["Memory/Merge"], &[]).contains(&"MergeRequest.source_collection".to_owned()));
    // A field with a reason is classified, and only that field.
    let classified = [
        ("GrowRequest", "name", "a label"),
        ("GrowRequest.Inner", "graph_name", "a label"),
    ];
    assert_eq!(
        found(&[], &classified),
        [
            "CopyRequest.source_collection",
            "FetchRequest.evidenceGraph",
            "GrowRequest.Inner.evidence_collections",
            "GrowRequest.graphs",
            "MergeRequest.source_collection",
        ]
    );
}

#[test]
fn the_proto_reader_follows_nested_messages() {
    let model = read_proto(SYNTHETIC);
    assert_eq!(
        fields_of(&model, "GrowRequest.Inner"),
        [
            ("string", "graph_name"),
            ("Path", "path"),
            ("string", "evidence_collections")
        ]
    );
    assert_eq!(
        fields_of(&model, "GrowRequest"),
        [
            ("Inner", "inner"),
            ("string", "name"),
            ("uint32", "collection_id"),
            ("map", "graphs")
        ]
    );
    // A type is looked up in its message, in the messages around it, then at
    // the top level.
    assert_eq!(
        model.resolve("GrowRequest", "Inner").as_deref(),
        Some("GrowRequest.Inner")
    );
    assert_eq!(
        model.resolve("GrowRequest.Inner", "Path").as_deref(),
        Some("Path")
    );
    assert_eq!(model.resolve("GrowRequest", "uint32"), None);
    let streamed = model.rpcs.iter().find(|rpc| rpc.name == "Grow");
    assert_eq!(
        streamed.map(|rpc| rpc.request.as_str()),
        Some("GrowRequest")
    );
    // The proto of the server has nested messages too.
    assert_eq!(
        proto_model().resolve("Operation", "Put").as_deref(),
        Some("Operation.Put")
    );
}

#[test]
fn an_rpc_is_exempt_or_in_the_matrix_never_both() {
    let exempt = rpc_ids(&["Memory/Remember"]);
    assert!(listed_twice(&exempt, &rpc_ids(&["VLite/Search"])).is_empty());
    assert_eq!(
        listed_twice(&exempt, &rpc_ids(&["VLite/Search", "Memory/Remember"])),
        ["Memory/Remember"]
    );
}

// ----------------------------------------------------------- what must work

#[tokio::test]
async fn generated_calls_take_effect_when_nothing_is_hidden() {
    let world = grpc_world(false, &PRINCIPALS[6]).await;
    let table = rows();
    let ordinary = names("", "ordinary", "ordinary_graph");
    let fresh = names("", "fresh_ev", "fresh_graph");
    let mut failures = Vec::new();
    let mut checked = 0;
    for row in ordered_rows(&table) {
        if row.effect == Effect::Unchecked {
            continue;
        }
        let spelled = if row.kind == Kind::Reference {
            &ordinary
        } else {
            &fresh
        };
        let answer = call(&world, ADMIN, row, spelled).await;
        checked += 1;
        if answer.code != Code::Ok {
            failures.push(format!("{}: {}", row.rpc, describe_g(&answer)));
        } else if row.effect == Effect::Carries
            && !identities("ROOTORDINARY")
                .iter()
                .any(|form| answer.text.contains(form.as_str()))
        {
            failures.push(format!(
                "{} carries nothing of the resource it names: {}",
                row.rpc,
                brief(&answer.text)
            ));
        }
    }
    assert!(
        checked >= 10,
        "only {checked} rows are meant to take effect"
    );
    assert!(
        failures.is_empty(),
        "rows meant to take effect did not:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
async fn the_digest_sees_a_grpc_write_to_every_protected_resource() {
    let world = grpc_world(true, &PRINCIPALS[6]).await;
    let labels: Vec<_> = snapshot(&world.rest).await.into_keys().collect();
    assert_eq!(
        labels,
        [
            "reserved.root.notes",
            "reserved.root.other",
            "reserved.tenant.notes",
            "root.kvstore",
            "root.lists",
            "root.ordinary"
        ],
        "the digest must cover every protected resource"
    );
    let table = rows();
    let mut failures = Vec::new();
    let mut resources: Vec<(&str, &str, &str)> = RESERVED
        .iter()
        .map(|(label, holder, collection, _, _)| (*label, *holder, *collection))
        .collect();
    resources.push(("root.ordinary", "admin", "ordinary"));
    for (label, writer, collection) in resources {
        let spelled = names("", collection, "unused_graph");
        for rpc in [
            "VLite/PutVector",
            "VLite/UpdateMetadata",
            "VLite/ConfigureNamedVector",
            "VLite/PutNamedVectors",
            "VLite/WriteBatch",
        ] {
            let before = snapshot(&world.rest).await;
            let answer = call(
                &world,
                world.token(writer),
                row_named(&table, rpc),
                &spelled,
            )
            .await;
            assert_eq!(
                answer.code,
                Code::Ok,
                "{writer} {rpc} on {collection}: {}",
                describe_g(&answer)
            );
            let after = snapshot(&world.rest).await;
            for (name, now) in &after {
                let changed = before[name] != *now;
                if (*name == label) != changed {
                    failures.push(format!(
                        "{rpc}: {name} {} but {label} was written",
                        if changed { "changed" } else { "did not change" }
                    ));
                }
            }
        }
    }
    for (rpc, spelled, label) in [
        (
            "Kv/PutKvEntry",
            names("", "kvstore", "unused_graph"),
            "root.kvstore",
        ),
        (
            "Kv/WriteKvBatch",
            names("", "kvstore", "unused_graph"),
            "root.kvstore",
        ),
        (
            "VLite/CreateCollection",
            names("", "listed_ev", "unused_graph"),
            "root.lists",
        ),
    ] {
        let before = snapshot(&world.rest).await;
        let answer = call(&world, ADMIN, row_named(&table, rpc), &spelled).await;
        assert_eq!(answer.code, Code::Ok, "{rpc}: {}", describe_g(&answer));
        let after = snapshot(&world.rest).await;
        for (name, now) in &after {
            if (*name == label) != (before[name] != *now) {
                failures.push(format!("{rpc}: {name} is not the only label that changed"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn markers_reach_their_holders_over_grpc_so_a_leak_would_show() {
    let world = grpc_world(true, &PRINCIPALS[6]).await;
    let table = rows();
    for (_, holder, collection, _, marker) in RESERVED {
        let spelled = names("", collection, "unused_graph");
        for (rpc, wants_text, wants_id) in [
            ("VLite/GetVector", true, false),
            ("VLite/ExportVectors", true, true),
            ("VLite/Search", false, true),
            ("VLite/SearchNamed", false, true),
        ] {
            let answer = call(
                &world,
                world.token(holder),
                row_named(&table, rpc),
                &spelled,
            )
            .await;
            assert_eq!(
                answer.code,
                Code::Ok,
                "{holder} {rpc} on {collection}: {}",
                describe_g(&answer)
            );
            if wants_text {
                assert!(
                    forms(marker)
                        .iter()
                        .any(|form| answer.text.contains(form.as_str())),
                    "{holder} must read back {marker} from {rpc} on {collection}: {}",
                    brief(&answer.text)
                );
            }
            if wants_id {
                assert!(
                    answer.text.contains(&marker_id(marker).to_string()),
                    "{holder} must read back the id of {marker} from {rpc} on {collection}: {}",
                    brief(&answer.text)
                );
            }
        }
    }
    let kv = names("", "kvstore", "unused_graph");
    for (reader, marker) in [("admin", "ROOTORDINARY"), ("tenant", "TENANTORDINARY")] {
        let answer = call(
            &world,
            world.token(reader),
            row_named(&table, "Kv/GetKvEntry"),
            &kv,
        )
        .await;
        assert!(
            forms(marker)
                .iter()
                .any(|form| answer.text.contains(form.as_str())),
            "{reader} must read back {marker} from kvstore: {}",
            describe_g(&answer)
        );
    }
}

// ------------------------------------------------------ what must not leak

#[tokio::test]
async fn lists_and_the_feed_never_name_a_reserved_resource_to_a_caller_that_may_not_reach_it() {
    let rest = build(true);
    seed(&rest).await;
    // A root-only collection and graph no tenant has, so a tenant's list that
    // fell back to the whole database would show them.
    for (path, body) in [
        (
            "/v1/collections",
            r#"{"name":"rootonly_ev","dimensions":2}"#,
        ),
        ("/v1/graphs", r#"{"graph":"rootonly_graph"}"#),
    ] {
        let created = request(&rest.app, ADMIN, "POST", path, body).await;
        assert_eq!(created.0, 201, "{}", describe(&created));
    }
    let world = grpc_over(rest).await;
    let globals = global_rows();
    let listing = row_named(&globals, "VLite/ListCollections");
    let feeds = [
        listing,
        row_named(&globals, "VLite/Changes"),
        row_named(&globals, "VLite/WatchChanges"),
    ];
    let nothing = legacy_names();
    let mut failures = Vec::new();
    for principal in token_principals() {
        let token = world.token(principal.name);
        let mut hidden = hidden_names(principal);
        if principal.tenant {
            hidden.extend(["rootonly_ev".to_owned(), "rootonly_graph".to_owned()]);
        }
        let forbidden = forbidden_markers(principal, true);
        for row in feeds {
            let answer = call(&world, token, row, &nothing).await;
            for name in &hidden {
                if answer.text.contains(&format!("\"{name}\"")) {
                    failures.push(format!(
                        "{}: {} names the hidden {name}: {}",
                        principal.name,
                        row.rpc,
                        brief(&answer.text)
                    ));
                }
            }
            for marker in &forbidden {
                if answer.text.contains(marker.as_str()) {
                    failures.push(format!(
                        "{}: {} carries {marker}: {}",
                        principal.name,
                        row.rpc,
                        brief(&answer.text)
                    ));
                }
            }
        }
    }
    // The checks are able to fail: a holder's list does carry its own names, a
    // tenant's carries its own collections and the administrator's feed does
    // carry the ordinary resources.
    for (holder, collection) in [
        ("raw", "notes_ev"),
        ("raw_other", "other_ev"),
        ("tenant_raw", "notes_ev"),
    ] {
        let answer = call(&world, world.token(holder), listing, &nothing).await;
        if !answer.text.contains(&format!("\"{collection}\"")) {
            failures.push(format!(
                "{holder}: ListCollections must list {collection}: {}",
                brief(&answer.text)
            ));
        }
    }
    let tenant = call(&world, world.token("tenant"), listing, &nothing).await;
    if !tenant.text.contains("\"ordinary\"") || !tenant.text.contains("\"default\"") {
        failures.push(format!(
            "tenant: its own collections must be listed: {}",
            brief(&tenant.text)
        ));
    }
    for row in &feeds[1..] {
        let answer = call(&world, ADMIN, row, &nothing).await;
        for name in ["ordinary", "ordinary_graph"] {
            if !answer.text.contains(&format!("\"{name}\"")) {
                failures.push(format!(
                    "admin: {} must carry {name}: {}",
                    row.rpc,
                    brief(&answer.text)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A name outside a credential's grant is refused with `forbidden_scope` before
/// anything is looked up, so it must not matter whether the name is reserved,
/// missing or an ordinary collection that exists: otherwise a credential could
/// tell which collections exist outside its grant. The matrix compares
/// reserved with missing names only; this adds the third and holds every row
/// to the documented status.
#[tokio::test]
async fn a_name_outside_a_credentials_grant_is_refused_whatever_it_names() {
    let principal = &PRINCIPALS[1];
    let world = grpc_world(true, principal).await;
    let table = rows();
    let tenanted_ordinary = format!("t{TENANT}__ordinary");
    let mut compared = 0;
    for row in ordered_rows(&table) {
        for (what, name) in [
            ("a reserved name", "notes_ev"),
            ("a missing name", "absent_ev"),
            ("an ordinary name that exists", tenanted_ordinary.as_str()),
        ] {
            let answer = call(
                &world,
                world.token(principal.name),
                row,
                &names("", name, "unused_graph"),
            )
            .await;
            assert!(
                answer.denied() && answer.error == "forbidden_scope",
                "{}: {} with {what}: {}",
                principal.name,
                row.rpc,
                describe_g(&answer)
            );
        }
        compared += 1;
    }
    assert!(compared >= 10, "only {compared} rows name a resource");
}

/// Legacy addressing (an empty collection means `default`) and the RPCs that
/// span the database are no tenant's: SECURITY.md says a tenant credential's
/// wildcard grants only that tenant's namespace, not global administration,
/// default-collection access or other tenants.
#[tokio::test]
async fn a_tenant_is_refused_every_grpc_call_that_spans_the_database() {
    let table = rows();
    let globals = global_rows();
    let nothing = legacy_names();
    let mut failures = Vec::new();
    let mut sent = 0;
    for bound in [false, true] {
        for principal in token_principals().into_iter().filter(|p| p.tenant) {
            let world = grpc_world(bound, principal).await;
            let token = world.token(principal.name);
            let spanning = ["VLite/Info", "VLite/Changes", "VLite/WatchChanges"];
            let rows_to_try = table
                .iter()
                .filter(|row| LEGACY.contains(&row.rpc))
                .chain(globals.iter().filter(|row| spanning.contains(&row.rpc)));
            for row in rows_to_try {
                let answer = call(&world, token, row, &nothing).await;
                sent += 1;
                if !answer.denied() || answer.error != "forbidden_scope" {
                    failures.push(format!(
                        "{} (bindings {bound}): {} must be refused with forbidden_scope: {}",
                        principal.name,
                        row.rpc,
                        describe_g(&answer)
                    ));
                }
            }
        }
    }
    assert!(sent >= 2 * 2 * (LEGACY.len() + 3), "only {sent} calls ran");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The RPCs that name no collection, and the legacy ones, run by every
/// principal under the marker scan and the digest.
#[tokio::test]
async fn rpcs_that_name_no_resource_leak_nothing_and_change_nothing() {
    let table = rows();
    let globals = global_rows();
    let nothing = legacy_names();
    let mut failures = Vec::new();
    let mut sent = 0;
    for bound in [true, false] {
        for principal in token_principals() {
            let world = grpc_world(bound, principal).await;
            let mut run = GRun::new(&world, principal).await;
            let rows_to_try = globals
                .iter()
                .chain(table.iter().filter(|row| LEGACY.contains(&row.rpc)));
            for row in rows_to_try {
                let answer = run.send(row, &nothing).await;
                sent += 1;
                if answer.server_error() {
                    failures.push(format!(
                        "{} (bindings {bound}): {} answered a server error: {}",
                        principal.name,
                        row.rpc,
                        describe_g(&answer)
                    ));
                }
                // Imports are refused wholesale while bindings exist.
                if bound && !row.flags.lockstep && !answer.denied() {
                    failures.push(format!(
                        "{}: {} must stay refused while bindings exist: {}",
                        principal.name,
                        row.rpc,
                        describe_g(&answer)
                    ));
                }
            }
            failures.append(&mut run.report);
        }
    }
    assert!(
        sent >= 2 * token_principals().len() * (globals.len() + LEGACY.len()),
        "only {sent} calls ran"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ------------------------------------------------------------- import jobs

async fn reserve(world: &GWorld, actor: &str, collection: &str) -> u64 {
    let mut client = world.clients.vlite.clone();
    let response = client
        .reserve_import(authorized(
            world.token(actor),
            proto::ReserveImportRequest {
                collection: collection.to_owned(),
            },
        ))
        .await
        .unwrap_or_else(|status| panic!("{actor} reserves on {collection}: {status:?}"));
    let value: Value = serde_json::from_str(&response.into_inner().json).unwrap();
    value["job"].as_str().unwrap().parse().unwrap()
}

async fn job_status(world: &GWorld, who: &str, job: u64) -> (GAnswer, Value) {
    let mut client = world.clients.vlite.clone();
    let result = client
        .get_import_status(authorized(
            world.token(who),
            proto::ImportJobRequest { job },
        ))
        .await;
    let state = result
        .as_ref()
        .ok()
        .and_then(|response| serde_json::from_str(&response.get_ref().json).ok())
        .unwrap_or(Value::Null);
    (answer(result), state)
}

/// An import job names a collection through its ID, so the job RPCs carry no
/// name the matrix could probe. SECURITY.md: "Import job IDs are allocated for
/// the whole file, so a request about another tenant's job answers `403` while
/// an unused ID answers `404`". Imports are refused wholesale while bindings
/// exist, so this runs without them.
#[tokio::test]
async fn import_jobs_answer_only_to_the_credentials_that_reach_their_collection() {
    let world = grpc_world(false, &PRINCIPALS[6]).await;
    let root_job = reserve(&world, "admin", "ordinary").await;
    let tenant_job = reserve(&world, "tenant", "ordinary").await;
    let unused = root_job.max(tenant_job) + 1_000;
    // The root job turns durable with a first chunk, so both the reserved and
    // the durable path of a job are probed.
    let mut client = world.clients.vlite.clone();
    let chunk = answer(
        client
            .import_chunk(authorized(
                ADMIN,
                proto::ImportChunkRequest {
                    job: root_job,
                    chunk: 1,
                    operations: vec![put_operation(41)],
                    policy: proto::ImportPolicy::RejectChunk as i32,
                },
            ))
            .await,
    );
    assert_eq!(chunk.code, Code::Ok, "{}", describe_g(&chunk));
    let second = reserve(&world, "admin", "ordinary").await;
    let before = snapshot(&world.rest).await;
    let mut failures = Vec::new();
    for (who, job, expected) in [
        ("tenant", root_job, Code::PermissionDenied),
        ("tenant", second, Code::PermissionDenied),
        ("tenant", tenant_job, Code::Ok),
        ("tenant", unused, Code::NotFound),
        ("tenant_raw", root_job, Code::PermissionDenied),
        ("tenant_raw", tenant_job, Code::Ok),
        ("memory_only", root_job, Code::Ok),
        ("memory_only", tenant_job, Code::PermissionDenied),
        ("wildcard", root_job, Code::Ok),
        ("admin", root_job, Code::Ok),
        ("admin", tenant_job, Code::Ok),
        ("admin", unused, Code::NotFound),
    ] {
        let (read, _) = job_status(&world, who, job).await;
        if read.code != expected {
            failures.push(format!(
                "{who} reading job {job}: expected {expected:?}: {}",
                describe_g(&read)
            ));
        }
    }
    // Acting on a job of another namespace is refused as well and changes
    // nothing: not the job, and not the collection it would have written to.
    for who in ["tenant", "tenant_raw"] {
        for job in [root_job, second] {
            let mut client = world.clients.vlite.clone();
            let token = world.token(who);
            let acts = [
                (
                    "ImportChunk",
                    answer(
                        client
                            .import_chunk(authorized(
                                token,
                                proto::ImportChunkRequest {
                                    job,
                                    chunk: 2,
                                    operations: vec![put_operation(42)],
                                    policy: proto::ImportPolicy::RejectChunk as i32,
                                },
                            ))
                            .await,
                    ),
                ),
                (
                    "FinishImport",
                    answer(
                        client
                            .finish_import(authorized(token, proto::ImportJobRequest { job }))
                            .await,
                    ),
                ),
                (
                    "AbortImport",
                    answer(
                        client
                            .abort_import(authorized(token, proto::ImportJobRequest { job }))
                            .await,
                    ),
                ),
            ];
            for (rpc, got) in acts {
                if !got.denied() {
                    failures.push(format!(
                        "{who}: {rpc} of job {job} must be refused: {}",
                        describe_g(&got)
                    ));
                }
            }
        }
    }
    let after = snapshot(&world.rest).await;
    for (label, text) in &before {
        if after[label] != *text {
            failures.push(format!("{label} changed under a foreign job"));
        }
    }
    for job in [root_job, second] {
        let (read, state) = job_status(&world, "admin", job).await;
        if state["state"] != "open" {
            failures.push(format!(
                "job {job} must still be open: {}",
                describe_g(&read)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// --------------------------------------------------- credentials and audit

/// gRPC authenticates bearer tokens only. A console session is a cookie of the
/// REST console and must be no credential here, however it is presented.
#[tokio::test]
async fn a_console_session_is_not_a_grpc_credential() {
    let mut rest = build(true);
    let sessions: Vec<_> = PRINCIPALS
        .iter()
        .filter(|principal| principal.session.is_some())
        .collect();
    for principal in &sessions {
        let role = principal.session.unwrap();
        sign_in(&mut rest, principal.name, role, principal.tenant).await;
    }
    seed(&rest).await;
    let world = grpc_over(rest).await;
    assert_eq!(sessions.len(), 4);
    for principal in sessions {
        let cookie = world.token(principal.name).to_owned();
        assert!(cookie.starts_with("vlite_session="), "{cookie}");
        let mut client = world.clients.vlite.clone();
        let as_bearer = answer(
            client
                .list_collections(authorized(&cookie, proto::ListCollectionsRequest {}))
                .await,
        );
        let mut presented = tonic::Request::new(proto::ListCollectionsRequest {});
        presented
            .metadata_mut()
            .insert("cookie", cookie.parse().unwrap());
        let as_cookie = answer(client.list_collections(presented).await);
        for (how, got) in [("bearer", as_bearer), ("cookie", as_cookie)] {
            assert_eq!(
                got.code,
                Code::Unauthenticated,
                "{} as {how}: {}",
                principal.name,
                describe_g(&got)
            );
        }
    }
}

/// Like REST, a credential's request for a reserved name answers as for a
/// missing one, and the operator learns of it from the audit log, which names
/// the credential and the route but never the resource. The gRPC pipeline
/// records the same note, with its own route label.
#[tokio::test]
async fn a_credentials_hidden_attempt_over_grpc_is_audited_without_the_name() {
    let principal = &PRINCIPALS[0];
    let world = grpc_world(true, principal).await;
    let table = rows();
    let get = row_named(&table, "VLite/GetCollection");
    let audit = || async {
        let answer = request(
            &world.rest.app,
            ADMIN,
            "GET",
            "/v1/admin/audit/persistent?limit=1000",
            "",
        )
        .await;
        assert_eq!(answer.0, 200, "{}", describe(&answer));
        serde_json::from_slice::<Value>(&answer.2).unwrap()
    };
    let attempts = |audit: &Value| -> Vec<Value> {
        audit["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["event"] == "reserved_hidden_attempt")
            .cloned()
            .collect()
    };
    // Missing and ordinary names, and the administrator's own request for the
    // reserved one, leave no entry.
    for name in ["absent_ev", "ordinary"] {
        call(
            &world,
            world.token(principal.name),
            get,
            &names("", name, "unused_graph"),
        )
        .await;
    }
    call(&world, ADMIN, get, &names("", "notes_ev", "unused_graph")).await;
    assert!(attempts(&audit().await).is_empty());
    // The credential cannot tell its attempt from a missing name...
    let hidden = call(
        &world,
        world.token(principal.name),
        get,
        &names("", "notes_ev", "unused_graph"),
    )
    .await;
    let missing = call(
        &world,
        world.token(principal.name),
        get,
        &names("", "absent_ev", "unused_graph"),
    )
    .await;
    assert_eq!(hidden.code, Code::NotFound, "{}", describe_g(&hidden));
    assert_eq!(hidden, missing);
    // ...and the operator can: the credential and the route, never the name.
    let audit = audit().await;
    let noted = attempts(&audit);
    assert_eq!(noted.len(), 1, "{audit}");
    assert_eq!(noted[0]["credential_id"], "abcdefghij22");
    assert_eq!(noted[0]["route"], "grpc:/vlite.v1.VLite/GetCollection");
    assert!(!audit.to_string().contains("notes_ev"));
}

// ------------------------------------------------------- the stream reader

fn frame(cursor: &str, has_more: bool) -> proto::ChangesResponse {
    proto::ChangesResponse {
        cursor: cursor.to_owned(),
        has_more,
        ..Default::default()
    }
}

/// How long a step of a scripted stream waits, in milliseconds, and what it
/// then yields.
type Step = (u64, Result<Option<proto::ChangesResponse>, Status>);

/// A stream that answers as scripted; once the script is spent it stays silent
/// for good.
fn scripted(
    steps: Vec<Step>,
) -> impl AsyncFnMut() -> Result<Option<proto::ChangesResponse>, Status> {
    let mut steps = steps.into_iter();
    async move || match steps.next() {
        Some((wait, step)) => {
            tokio::time::sleep(Duration::from_millis(wait)).await;
            step
        }
        None => std::future::pending().await,
    }
}

/// Far longer than any test waits: a read that takes this long has waited for a
/// bound it must not wait for.
const LONG: Duration = Duration::from_secs(30);
const SHORT: Duration = Duration::from_millis(50);

/// A read that is over by itself, long before any `LONG` bound it was given.
async fn read(
    steps: Vec<Step>,
    first: Duration,
    silence: Duration,
    last: impl Fn(&proto::ChangesResponse) -> bool,
) -> GAnswer {
    let reading = read_frames(scripted(steps), first, silence, last);
    tokio::time::timeout(Duration::from_secs(10), reading)
        .await
        .expect("the read waited for a bound that it must not wait for")
}

/// The first frame of a stream is waited for as long as a call may take; only
/// the gaps after it are short. A feed that answers after the silence bound is
/// not an empty feed, or a slow machine would make every feed look empty.
#[tokio::test]
async fn a_feed_that_answers_late_is_not_an_empty_feed() {
    let steps = vec![(500, Ok(Some(frame("late", false))))];
    let shown = read(steps, LONG, SHORT, |frame| !frame.has_more).await;
    assert_eq!(shown.code, Code::Ok);
    assert!(shown.text.contains("late"), "{}", brief(&shown.text));
}

#[tokio::test]
async fn a_feed_that_goes_silent_after_a_frame_is_complete() {
    let steps = vec![(0, Ok(Some(frame("only", true))))];
    let shown = read(steps, LONG, SHORT, |_| false).await;
    assert!(shown.text.contains("only"), "{}", brief(&shown.text));
}

#[tokio::test]
async fn a_stream_is_complete_at_its_last_frame_and_at_its_end() {
    // The frame that says it is the last ends the read at once, whatever the
    // bound.
    let steps = vec![(0, Ok(Some(frame("last", false))))];
    let shown = read(steps, LONG, LONG, |frame| !frame.has_more).await;
    assert!(shown.text.contains("last"), "{}", brief(&shown.text));
    // So does the end of the stream, and a failure after a frame keeps it.
    let steps = vec![(0, Ok(Some(frame("one", true)))), (0, Ok(None))];
    let shown = read(steps, LONG, LONG, |_| false).await;
    assert_eq!((shown.code, shown.text.contains("one")), (Code::Ok, true));
    let steps = vec![
        (0, Ok(Some(frame("before", true)))),
        (0, Err(Status::aborted("broken"))),
    ];
    let shown = read(steps, LONG, LONG, |_| false).await;
    assert_eq!(shown.code, Code::Aborted);
    assert!(shown.text.contains("before"), "{}", brief(&shown.text));
}

#[tokio::test]
#[should_panic(expected = "the stream showed nothing")]
async fn a_stream_that_never_shows_anything_is_hung_not_empty() {
    let first = Duration::from_millis(200);
    read_frames(
        scripted(Vec::new()),
        first,
        SHORT,
        |_: &proto::ChangesResponse| false,
    )
    .await;
}
