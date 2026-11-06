//! The gRPC leg of the reserved-resource differential (lane F3, slice S1).
//!
//! The REST matrix of the parent module proves that a reserved name answers
//! like a missing one, that ordinary names answer like a database without
//! bindings and that no request changes a resource its caller may not reach.
//! The gRPC adapter reaches the same engine through the same guard, so the
//! same three properties are asserted here for every RPC that names a
//! collection, over a real listener, with the credentials, names and seeds of
//! the REST matrix and the same digest of the protected resources (read back
//! over REST by their holders, so a gRPC write that lands in the wrong
//! collection shows). What a gRPC caller sees of an answer is its status code,
//! the stable `vlite-error-code` trailer, the message and the base64 details;
//! the per-request metadata (`x-request-id`) is left out, as the REST matrix
//! leaves out its headers.
//!
//! gRPC authenticates bearer tokens only, so the eight token principals run
//! here; `checks` shows that a console session is no credential.
//!
//! The inventory is read from the proto: every RPC needs a row in `rows()` (it
//! names a collection or a graph) or in `global_rows()` (it names none), so a
//! new RPC cannot ship unchecked. `EXEMPT` is the home of the RPCs that are
//! addressed by a memory binding instead of a collection or graph name, each
//! with its reason, and each is covered by `tests/grpc_memory.rs`. A request
//! field that looks like a name must be varied by a row or be classified in
//! `NOT_A_NAME` (see `checks`), so that a field not called `collection` cannot
//! escape the inventory.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tonic::{Code, Status, transport::Channel};
use vlite::server::grpc::{
    ERROR_CODE_TRAILER, grpc_router,
    proto::{self, kv_client::KvClient, v_lite_client::VLiteClient},
};

use super::*;

#[path = "grpc_checks.rs"]
mod checks;

/// A call that has not answered by then is hung, not slow.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// A stream is read to its end, to the message that says it is complete, or to
/// this many messages: a feed never ends by itself.
const STREAM_LIMIT: usize = 64;
/// How long a feed that has shown something may stay silent before it counts as
/// complete. Everything a feed can show is committed before it opens, and it
/// polls at once, so only the gaps after a first frame are bounded this
/// tightly; the first frame is waited for as long as any call
/// (`CALL_TIMEOUT`), because a slow machine must not turn a feed that would
/// have answered into an empty one.
const FEED_SILENCE: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------- the world

#[derive(Clone)]
struct Clients {
    vlite: VLiteClient<Channel>,
    kv: KvClient<Channel>,
}

/// The REST world of the parent module with a gRPC listener over the same
/// service: what REST seeds, gRPC reads, and what gRPC writes, the digest of
/// the REST holders reads back.
struct GWorld {
    rest: World,
    clients: Clients,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl GWorld {
    fn token(&self, principal: &str) -> &str {
        self.rest.token(principal)
    }
}

/// A world whose REST router is built first, as `vlite-server` builds it, so
/// that the gRPC routes share the tokens, the credential store, the audit log
/// and the reservations that router published.
async fn grpc_world(bound: bool, principal: &Principal) -> GWorld {
    grpc_over(prepared(bound, principal).await).await
}

/// A gRPC listener over the service of a REST world that is already built.
async fn grpc_over(rest: World) -> GWorld {
    let grpc = grpc_router(rest.service.clone()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, stop) = tokio::sync::oneshot::channel();
    tokio::spawn(grpc.serve_with_shutdown(listener, async {
        let _ = stop.await;
    }));
    let channel = Channel::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    GWorld {
        clients: Clients {
            vlite: VLiteClient::new(channel.clone()),
            kv: KvClient::new(channel),
        },
        rest,
        _shutdown: shutdown,
    }
}

fn authorized<T>(token: &str, message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

// --------------------------------------------------------------- the answers

/// What a call answered, as far as its caller can tell.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GAnswer {
    code: Code,
    /// The `vlite-error-code` trailer; empty on success.
    error: String,
    message: String,
    /// The raw `vlite-error-details-base64` trailer.
    details: String,
    /// Everything the answer shows, as text for the marker scan: the debug
    /// form of each response message, its encoded bytes read as text (a
    /// payload or a string field is found as it was written) and, for a
    /// failure, the message and the decoded details.
    text: String,
}

impl GAnswer {
    fn ok<T: prost::Message + std::fmt::Debug>(frames: &[T]) -> Self {
        let mut text = String::new();
        for frame in frames {
            text += &format!("{frame:?}\n");
            text += &String::from_utf8_lossy(&frame.encode_to_vec());
            text.push('\n');
        }
        Self {
            code: Code::Ok,
            error: String::new(),
            message: String::new(),
            details: String::new(),
            text,
        }
    }

    /// A failure; `shown` is what a stream showed before it failed.
    fn failed(status: &Status, shown: String) -> Self {
        let trailer = |key: &str| {
            status
                .metadata()
                .get(key)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        let details = trailer("vlite-error-details-base64");
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &details)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        Self {
            code: status.code(),
            error: trailer(ERROR_CODE_TRAILER),
            message: status.message().to_owned(),
            text: format!("{shown}{} {decoded}", status.message()),
            details,
        }
    }

    /// The gRPC counterpart of a 5xx: nothing a caller does may cause one.
    fn server_error(&self) -> bool {
        matches!(
            self.code,
            Code::Internal
                | Code::Unknown
                | Code::DataLoss
                | Code::Unimplemented
                | Code::Unavailable
                | Code::DeadlineExceeded
        )
    }

    fn denied(&self) -> bool {
        self.code == Code::PermissionDenied
    }
}

fn describe_g(answer: &GAnswer) -> String {
    format!(
        "{:?} `{}` {:?} {}",
        answer.code, answer.error, answer.message, answer.details
    )
}

/// The start of what an answer showed, for a failure report.
fn brief(text: &str) -> String {
    let shown: String = text.chars().take(300).collect();
    if shown.len() < text.len() {
        format!("{shown}...")
    } else {
        shown
    }
}

fn answer<T: prost::Message + std::fmt::Debug>(
    result: Result<tonic::Response<T>, Status>,
) -> GAnswer {
    match result {
        Ok(response) => GAnswer::ok(&[response.into_inner()]),
        Err(status) => GAnswer::failed(&status, String::new()),
    }
}

/// Reads a server stream: to its end, to the frame `last` calls complete, or
/// until a feed that has shown a frame and has nothing more to say stays silent.
async fn frames<T: prost::Message + std::fmt::Debug>(
    opened: Result<tonic::Response<tonic::Streaming<T>>, Status>,
    last: impl Fn(&T) -> bool,
) -> GAnswer {
    let mut stream = match opened {
        Ok(response) => response.into_inner(),
        Err(status) => return GAnswer::failed(&status, String::new()),
    };
    read_frames(
        async || stream.message().await,
        CALL_TIMEOUT,
        FEED_SILENCE,
        last,
    )
    .await
}

/// The reading of `frames`, over whatever yields the next message. A stream
/// that has shown nothing is waited for up to `first` and is hung, not empty,
/// after that: a feed that answers late must not look like a feed that has
/// nothing to say. Once it has shown a frame it counts as complete when it
/// stays silent for `silence`.
async fn read_frames<T: prost::Message + std::fmt::Debug>(
    mut next: impl AsyncFnMut() -> Result<Option<T>, Status>,
    first: Duration,
    silence: Duration,
    last: impl Fn(&T) -> bool,
) -> GAnswer {
    let mut seen = Vec::new();
    while seen.len() < STREAM_LIMIT {
        let wait = if seen.is_empty() { first } else { silence };
        let Ok(message) = tokio::time::timeout(wait, next()).await else {
            assert!(
                !seen.is_empty(),
                "the stream showed nothing within {first:?}: it is hung, or it is a feed with \
                 nothing to show, which needs a probe that makes it show something"
            );
            break;
        };
        match message {
            Ok(Some(frame)) => {
                let done = last(&frame);
                seen.push(frame);
                if done {
                    break;
                }
            }
            Ok(None) => break,
            Err(status) => return GAnswer::failed(&status, GAnswer::ok(&seen).text),
        }
    }
    GAnswer::ok(&seen)
}

// ------------------------------------------------------------------ the rows

type Sent<'a> = Pin<Box<dyn Future<Output = GAnswer> + 'a>>;

/// What running a row means beyond its answer.
#[derive(Clone, Copy)]
struct Flags {
    /// The call may change state, so the protected resources are read back
    /// after it.
    writes: bool,
    /// Runs after every other row, like the REST rows that delete.
    last: bool,
    /// Answers the same with bindings and without, for ordinary and missing
    /// names. False: refused wholesale while bindings exist.
    lockstep: bool,
}

const READ: Flags = Flags {
    writes: false,
    last: false,
    lockstep: true,
};
const WRITE: Flags = Flags {
    writes: true,
    last: false,
    lockstep: true,
};
const DELETE: Flags = Flags {
    writes: true,
    last: true,
    lockstep: true,
};
const REFUSED: Flags = Flags {
    writes: true,
    last: false,
    lockstep: false,
};

/// One RPC with the request a probe sends for a set of names. The table is
/// listed like the REST one: a schema cannot say which request is worth
/// sending.
struct GRow {
    /// `Service/Rpc`, as the proto spells them.
    rpc: &'static str,
    kind: Kind,
    effect: Effect,
    flags: Flags,
    send: for<'a> fn(&'a Clients, &'a str, &'a Names) -> Sent<'a>,
}

/// A row from a body that sends the request: `clients`, the bearer `token`
/// and the `names` of the probe are in scope under the given names.
macro_rules! row {
    ($rpc:literal, $kind:ident, $effect:ident, $flags:expr, |$clients:ident, $token:ident, $names:ident| $body:expr) => {
        GRow {
            rpc: $rpc,
            kind: Kind::$kind,
            effect: Effect::$effect,
            flags: $flags,
            send: {
                #[allow(unused_variables)]
                fn send<'a>($clients: &'a Clients, $token: &'a str, $names: &'a Names) -> Sent<'a> {
                    Box::pin(async move { $body })
                }
                send
            },
        }
    };
}

/// A unary row: `|names| client.method(request)`, with or without braces around
/// the call (the formatter adds them to a call that spans lines).
macro_rules! unary {
    ($rpc:literal, $kind:ident, $effect:ident, $flags:expr, |$names:ident| $client:ident . $method:ident ($request:expr)) => {
        row!($rpc, $kind, $effect, $flags, |clients, token, $names| {
            let mut client = clients.$client.clone();
            answer(client.$method(authorized(token, $request)).await)
        })
    };
    ($rpc:literal, $kind:ident, $effect:ident, $flags:expr, |$names:ident| { $client:ident . $method:ident ($request:expr) }) => {
        unary!($rpc, $kind, $effect, $flags, |$names| $client
            .$method($request))
    };
}

fn text_value(value: &str) -> proto::MetadataValue {
    proto::MetadataValue {
        kind: Some(proto::metadata_value::Kind::StringValue(value.to_owned())),
    }
}

fn metadata_of(key: &str, value: &str) -> HashMap<String, proto::MetadataValue> {
    HashMap::from([(key.to_owned(), text_value(value))])
}

fn put_operation(id: u64) -> proto::Operation {
    proto::Operation {
        kind: Some(proto::operation::Kind::Put(proto::operation::Put {
            id,
            vector: vec![0.0, 1.0],
            ..Default::default()
        })),
    }
}

fn kv_address(names: &Names) -> Option<proto::KvAddress> {
    Some(proto::KvAddress {
        collection: names.c.clone(),
        namespace: "n".into(),
        key: "k".into(),
    })
}

/// The administrator names the sidecar of a disk ANN call itself, and must;
/// every other credential gets the file of the collection and may not name
/// one. A file of the administrator's is made for the call and removed after.
fn sidecar_path(token: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    if token != ADMIN {
        return String::new();
    }
    std::env::temp_dir()
        .join(format!(
            "vlite-f3-grpc-{}-{}.dann",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
        .to_string_lossy()
        .into_owned()
}

fn discard_sidecar(path: &str) {
    if !path.is_empty() {
        let _ = std::fs::remove_file(path);
    }
}

/// The RPCs that name a collection, with the request of the REST row of the
/// same operation where there is one, so that "a missing name answers like a
/// reserved one" means the same thing on both transports.
fn rows() -> Vec<GRow> {
    vec![
        unary!("VLite/GetCollection", Reference, Succeeds, READ, |n| vlite
            .get_collection(proto::GetCollectionRequest {
                name: n.c.clone()
            })),
        unary!("VLite/GetVector", Reference, Carries, READ, |n| vlite
            .get_vector(proto::GetVectorRequest {
                collection: n.c.clone(),
                id: 7
            })),
        row!(
            "VLite/ExportVectors",
            Reference,
            Carries,
            READ,
            |clients, token, n| {
                let mut client = clients.vlite.clone();
                let opened = client
                    .export_vectors(authorized(
                        token,
                        proto::ExportVectorsRequest {
                            collection: n.c.clone(),
                            after: None,
                            page_size: 0,
                        },
                    ))
                    .await;
                frames(opened, |_| false).await
            }
        ),
        unary!("VLite/PutVector", Reference, Succeeds, WRITE, |n| vlite
            .put_vector(proto::PutVectorRequest {
                collection: n.c.clone(),
                id: 7,
                vector: vec![0.0, 1.0],
                payload: b"PROBE".to_vec(),
                metadata: metadata_of("probe", "touched"),
                ..Default::default()
            })),
        unary!("VLite/UpdateMetadata", Reference, Succeeds, WRITE, |n| {
            vlite.update_metadata(proto::UpdateMetadataRequest {
                collection: n.c.clone(),
                id: 7,
                metadata: metadata_of("probe", "patched"),
            })
        }),
        unary!(
            "VLite/ConfigureNamedVector",
            Reference,
            Succeeds,
            WRITE,
            |n| vlite.configure_named_vector(proto::ConfigureNamedVectorRequest {
                collection: n.c.clone(),
                spec: Some(proto::NamedVectorSpec {
                    name: "w".into(),
                    dimensions: 2,
                    metric: proto::Metric::Cosine as i32,
                    active_version: "v1".into(),
                    history: 0,
                }),
            })
        ),
        unary!("VLite/PutNamedVectors", Reference, Succeeds, WRITE, |n| {
            vlite.put_named_vectors(proto::PutNamedVectorsRequest {
                collection: n.c.clone(),
                id: 7,
                dense: vec![proto::NamedVector {
                    name: "v".into(),
                    version: "v1".into(),
                    vector: vec![0.0, 1.0],
                }],
                sparse: vec![proto::SparseVector {
                    name: "s".into(),
                    indices: vec![1],
                    values: vec![1.0],
                }],
            })
        }),
        unary!("VLite/PutVectorVersion", Reference, Unchecked, WRITE, |n| {
            vlite.put_vector_version(proto::PutVectorVersionRequest {
                collection: n.c.clone(),
                id: 7,
                name: "v".into(),
                version: "v2".into(),
                vector: vec![0.0, 1.0],
            })
        }),
        unary!("VLite/WriteBatch", Reference, Succeeds, WRITE, |n| vlite
            .write_batch(proto::WriteBatchRequest {
                collection: n.c.clone(),
                operations: vec![put_operation(9)],
            })),
        unary!("VLite/Search", Reference, Carries, READ, |n| vlite.search(
            proto::SearchRequest {
                collection: n.c.clone(),
                vector: vec![1.0, 0.0],
                k: 5,
                ..Default::default()
            }
        )),
        unary!("VLite/SearchGrouped", Reference, Carries, READ, |n| vlite
            .search_grouped(proto::GroupedSearchRequest {
                collection: n.c.clone(),
                vector: vec![1.0, 0.0],
                groups: 3,
                hits_per_group: 3,
                field: "tag".into(),
                ..Default::default()
            })),
        unary!("VLite/GeoSearch", Reference, Carries, READ, |n| vlite
            .geo_search(proto::GeoSearchRequest {
                collection: n.c.clone(),
                vector: vec![1.0, 0.0],
                k: 5,
                field: "location".into(),
                latitude: 0.0,
                longitude: 0.0,
                radius_meters: 1.0,
                ..Default::default()
            })),
        unary!("VLite/Recommend", Reference, Carries, READ, |n| vlite
            .recommend(proto::RecommendRequest {
                collection: n.c.clone(),
                positive_ids: vec![8],
                k: 5,
                ..Default::default()
            })),
        unary!("VLite/Facets", Reference, Carries, READ, |n| vlite.facets(
            proto::FacetsRequest {
                collection: n.c.clone(),
                field: "tag".into(),
                filter: Vec::new(),
            }
        )),
        unary!("VLite/NumericStats", Reference, Carries, READ, |n| vlite
            .numeric_stats(proto::NumericStatsRequest {
                collection: n.c.clone(),
                field: "score".into(),
                filter: Vec::new(),
            })),
        unary!("VLite/Query", Reference, Carries, READ, |n| vlite.query(
            proto::QueryRequest {
                collection: n.c.clone(),
                query: "SEARCH default NEAR $q K 5".into(),
                parameters: HashMap::from([(
                    "q".to_owned(),
                    proto::VectorInput {
                        vector: vec![1.0, 0.0]
                    }
                )]),
            }
        )),
        unary!("VLite/EmbedText", Reference, Unchecked, READ, |n| vlite
            .embed_text(proto::EmbedTextRequest {
                collection: n.c.clone(),
                text: "fake text".into(),
                provider: String::new(),
            })),
        unary!("VLite/SearchNamed", Reference, Carries, READ, |n| vlite
            .search_named(proto::SearchNamedRequest {
                collection: n.c.clone(),
                name: "v".into(),
                version: None,
                vector: vec![1.0, 0.0],
                k: 5,
            })),
        unary!("VLite/SearchSparse", Reference, Carries, READ, |n| vlite
            .search_sparse(proto::SearchSparseRequest {
                collection: n.c.clone(),
                name: "s".into(),
                indices: vec![1],
                values: vec![1.0],
                k: 5,
            })),
        unary!("VLite/SearchDenseSparse", Reference, Carries, READ, |n| {
            vlite.search_dense_sparse(proto::SearchDenseSparseRequest {
                collection: n.c.clone(),
                dense_name: "v".into(),
                version: None,
                dense: vec![1.0, 0.0],
                sparse_name: "s".into(),
                sparse_indices: vec![1],
                sparse_values: vec![1.0],
                k: 5,
                fusion: None,
            })
        }),
        row!(
            "VLite/BuildDiskAnn",
            Reference,
            Unchecked,
            WRITE,
            |clients, token, n| {
                let path = sidecar_path(token);
                let mut client = clients.vlite.clone();
                let built = client
                    .build_disk_ann(authorized(
                        token,
                        proto::BuildDiskAnnRequest {
                            collection: n.c.clone(),
                            path: path.clone(),
                            config: None,
                            metric: 0,
                        },
                    ))
                    .await;
                let got = answer(built);
                discard_sidecar(&path);
                got
            }
        ),
        row!(
            "VLite/SearchDiskAnn",
            Reference,
            Unchecked,
            READ,
            |clients, token, n| {
                let path = sidecar_path(token);
                let mut client = clients.vlite.clone();
                if !path.is_empty() {
                    // The administrator's search opens the file it names, which
                    // must exist for the call to reach the engine.
                    let _ = client
                        .build_disk_ann(authorized(
                            token,
                            proto::BuildDiskAnnRequest {
                                collection: "ordinary".into(),
                                path: path.clone(),
                                config: None,
                                metric: 0,
                            },
                        ))
                        .await;
                }
                let found = client
                    .search_disk_ann(authorized(
                        token,
                        proto::SearchDiskAnnRequest {
                            collection: n.c.clone(),
                            path: path.clone(),
                            vector: vec![0.0, 1.0],
                            k: 1,
                            ef_search: 1,
                            cache_bytes: 0,
                        },
                    ))
                    .await;
                let got = answer(found);
                discard_sidecar(&path);
                got
            }
        ),
        // With bindings every import is refused, ordinary names included.
        unary!("VLite/ReserveImport", Reference, Unchecked, REFUSED, |n| {
            vlite.reserve_import(proto::ReserveImportRequest {
                collection: n.c.clone(),
            })
        }),
        // Key-value, named by address or by collection.
        unary!("Kv/GetKvEntry", Reference, Unchecked, READ, |n| kv
            .get_kv_entry(proto::KvGetRequest {
                address: kv_address(n)
            })),
        unary!("Kv/PutKvEntry", Reference, Unchecked, WRITE, |n| kv
            .put_kv_entry(proto::KvPutRequest {
                address: kv_address(n),
                value: b"PROBE".to_vec(),
                ..Default::default()
            })),
        unary!("Kv/ListKvEntries", Reference, Unchecked, READ, |n| kv
            .list_kv_entries(proto::KvListRequest {
                collection: n.c.clone(),
                namespace: "n".into(),
                ..Default::default()
            })),
        // Creating a name: a reserved one answers like an occupied one.
        unary!("VLite/CreateCollection", Create, Succeeds, WRITE, |n| vlite
            .create_collection(proto::CreateCollectionRequest {
                name: n.c.clone(),
                dimensions: 2,
                settings_json: None,
            })),
        // Deletions run last, in table order, so every other row still has its
        // resource to act on.
        unary!("Kv/DeleteKvEntry", Reference, Unchecked, DELETE, |n| kv
            .delete_kv_entry(proto::KvDeleteRequest {
                address: kv_address(n),
                expected: None
            })),
        unary!("VLite/DeleteVector", Reference, Succeeds, DELETE, |n| vlite
            .delete_vector(proto::DeleteVectorRequest {
                collection: n.c.clone(),
                id: 7,
                expected: None
            })),
        unary!("VLite/DropCollection", Reference, Unchecked, DELETE, |n| {
            vlite.drop_collection(proto::DropCollectionRequest { name: n.c.clone() })
        }),
    ]
}

/// The RPCs that name no collection. Run by every principal under the marker
/// scan and the digest, like the REST rows that span the database or name a
/// job through its ID.
fn global_rows() -> Vec<GRow> {
    vec![
        unary!("VLite/ListCollections", Reference, Succeeds, READ, |n| {
            vlite.list_collections(proto::ListCollectionsRequest {})
        }),
        unary!("VLite/Info", Reference, Unchecked, READ, |n| vlite
            .info(proto::InfoRequest {})),
        unary!("VLite/Changes", Reference, Unchecked, READ, |n| vlite
            .changes(proto::ChangesRequest {
                cursor: String::new(),
                limit: 1000
            })),
        row!(
            "VLite/WatchChanges",
            Reference,
            Unchecked,
            READ,
            |clients, token, n| {
                let mut client = clients.vlite.clone();
                let opened = client
                    .watch_changes(authorized(
                        token,
                        proto::WatchChangesRequest {
                            cursor: String::new(),
                            limit: 1000,
                            poll_interval_ms: 100,
                        },
                    ))
                    .await;
                frames(opened, |frame| !frame.has_more).await
            }
        ),
        unary!(
            "VLite/GetImportStatus",
            Reference,
            Unchecked,
            REFUSED,
            |n| vlite.get_import_status(proto::ImportJobRequest { job: 1 })
        ),
        unary!("VLite/ImportChunk", Reference, Unchecked, REFUSED, |n| {
            vlite.import_chunk(proto::ImportChunkRequest {
                job: 1,
                chunk: 1,
                operations: vec![put_operation(9)],
                policy: proto::ImportPolicy::RejectChunk as i32,
            })
        }),
        unary!("VLite/FinishImport", Reference, Unchecked, REFUSED, |n| {
            vlite.finish_import(proto::ImportJobRequest { job: 1 })
        }),
        unary!("VLite/AbortImport", Reference, Unchecked, REFUSED, |n| {
            vlite.abort_import(proto::ImportJobRequest { job: 1 })
        }),
        row!(
            "VLite/StreamImport",
            Reference,
            Unchecked,
            REFUSED,
            |clients, token, n| {
                let mut client = clients.vlite.clone();
                let messages =
                    tonic::codegen::tokio_stream::iter(vec![proto::StreamImportRequest {
                        job: 1,
                        chunk: 1,
                        operations: vec![put_operation(9)],
                        policy: proto::ImportPolicy::RejectChunk as i32,
                        finish: true,
                    }]);
                answer(client.stream_import(authorized(token, messages)).await)
            }
        ),
    ]
}

/// The RPCs that address `default` when their collection is empty, like the
/// legacy `/v1/...` routes of REST, which span the database.
const LEGACY: &[&str] = &[
    "VLite/GetVector",
    "VLite/PutVector",
    "VLite/DeleteVector",
    "VLite/WriteBatch",
    "VLite/Search",
    "VLite/EmbedText",
    "VLite/UpdateMetadata",
    "VLite/SearchGrouped",
    "VLite/Recommend",
    "VLite/Facets",
    "VLite/NumericStats",
    "VLite/GeoSearch",
    "VLite/ConfigureNamedVector",
    "VLite/PutNamedVectors",
    "VLite/PutVectorVersion",
    "VLite/SearchNamed",
    "VLite/SearchSparse",
    "VLite/SearchDenseSparse",
    "VLite/Query",
    "VLite/SearchDiskAnn",
    "VLite/ExportVectors",
];

/// The RPCs that stay out of the matrix because a memory binding addresses them
/// instead of a collection or graph name (their requests may carry several
/// collection paths, which one row cannot vary), each with its reason. Every
/// one of them is covered by `tests/grpc_memory.rs`. None today.
const EXEMPT: &[(&str, &str)] = &[];

fn ordered_rows(rows: &[GRow]) -> impl Iterator<Item = &GRow> {
    rows.iter()
        .filter(|row| !row.flags.last)
        .chain(rows.iter().filter(|row| row.flags.last))
}

/// The names a legacy RPC is addressed with: none, which means `default`.
fn legacy_names() -> Names {
    Names {
        c: String::new(),
        g: String::new(),
        o: "ordinary".to_owned(),
        og: "ordinary_graph".to_owned(),
        sc: String::new(),
        so: "ordinary".to_owned(),
    }
}

// ---------------------------------------------------------------- the runs

async fn call(world: &GWorld, token: &str, row: &GRow, names: &Names) -> GAnswer {
    tokio::time::timeout(CALL_TIMEOUT, (row.send)(&world.clients, token, names))
        .await
        .unwrap_or_else(|_| panic!("{} did not answer within {CALL_TIMEOUT:?}", row.rpc))
}

/// Same status and, for a failure, the same error; a success body is not
/// compared because counters and timings differ legitimately.
fn same_g(actual: &GAnswer, reference: &GAnswer) -> bool {
    if actual.code != reference.code {
        return false;
    }
    actual.code == Code::Ok
        || (actual.error == reference.error
            && actual.message == reference.message
            && actual.details == reference.details)
}

/// What a name that does not exist (or, for a creation, one that is taken)
/// answers a principal while bindings exist: the outcome of the lookup itself
/// unless something refuses the request before it.
fn natural_outcome(principal: &Principal, row: &GRow) -> Code {
    if principal.name == "memory_only"
        || !row.flags.lockstep
        || (principal.name == "read_only" && row.flags.writes)
    {
        // Outside the grant, refused wholesale while bindings exist, or a write
        // of the read-only token: refused before any lookup.
        Code::PermissionDenied
    } else if row.kind == Kind::Create {
        Code::AlreadyExists
    } else {
        Code::NotFound
    }
}

struct GRun<'a> {
    world: &'a GWorld,
    principal: &'a Principal,
    token: String,
    markers: Vec<String>,
    last: BTreeMap<&'static str, String>,
    report: Vec<String>,
    executed: usize,
}

impl<'a> GRun<'a> {
    async fn new(world: &'a GWorld, principal: &'a Principal) -> GRun<'a> {
        GRun {
            world,
            principal,
            token: world.token(principal.name).to_owned(),
            markers: forbidden_markers(principal, world.rest.bound),
            last: snapshot(&world.rest).await,
            report: Vec::new(),
            executed: 0,
        }
    }

    async fn send(&mut self, row: &GRow, names: &Names) -> GAnswer {
        let answer = call(self.world, &self.token, row, names).await;
        self.executed += 1;
        for marker in &self.markers {
            if answer.text.contains(marker.as_str()) {
                self.report.push(format!(
                    "{}: {} with `{}` returned data it must not see ({marker}): {}",
                    self.principal.name,
                    row.rpc,
                    names.c,
                    brief(&answer.text)
                ));
            }
        }
        if row.flags.writes {
            let after = snapshot(&self.world.rest).await;
            for (label, now) in &after {
                if protected(self.principal, label, self.world.rest.bound)
                    && self.last[label] != *now
                {
                    self.report.push(format!(
                        "{}: {} with `{}` (answered {}) changed {label}",
                        self.principal.name,
                        row.rpc,
                        names.c,
                        describe_g(&answer)
                    ));
                }
            }
            self.last = after;
        }
        answer
    }

    async fn probe(&mut self, row: &GRow, variant: &Variant) {
        let hidden = self.send(row, &variant.reserved).await;
        // A credential limited to one name is refused every other name before
        // anything is looked up, so a name it may not create answers like any
        // other name it may not create, and nothing is created.
        let twin_names = if row.kind == Kind::Create && self.principal.name != "memory_only" {
            &variant.occupied
        } else {
            &variant.missing
        };
        let twin = self.send(row, twin_names).await;
        // Proof that the request is well-formed enough to reach the lookup, so
        // that "reserved answers like missing" compares two answers of the
        // lookup and not two answers of one early refusal.
        let natural = natural_outcome(self.principal, row);
        if twin.code != natural {
            self.report.push(format!(
                "{}: {} with `{}` must answer {natural:?} for a name that does not exist: {}",
                self.principal.name,
                row.rpc,
                twin_names.c,
                describe_g(&twin)
            ));
        }
        if hidden.server_error() || twin.server_error() {
            self.report.push(format!(
                "{}: {} answered a server error: [{}] / [{}]",
                self.principal.name,
                row.rpc,
                describe_g(&hidden),
                describe_g(&twin)
            ));
        }
        if variant.held {
            if row.rpc == "VLite/GetCollection" && hidden.code != Code::Ok {
                self.report.push(format!(
                    "{}: a raw holder must reach {}: {}",
                    self.principal.name,
                    variant.reserved.c,
                    describe_g(&hidden)
                ));
            }
            return;
        }
        if hidden != twin {
            self.report.push(format!(
                "{}: {} with {} answered [{}] but with {} answered [{}]",
                self.principal.name,
                row.rpc,
                variant.reserved.c,
                describe_g(&hidden),
                if row.kind == Kind::Create {
                    "an occupied name"
                } else {
                    "a missing name"
                },
                describe_g(&twin)
            ));
        }
    }
}

/// Ordinary and missing names, with bindings and without: the same outcome.
async fn lockstep_g(principal: &Principal, report: &mut Vec<String>) -> usize {
    let bound = grpc_world(true, principal).await;
    let unbound = grpc_world(false, principal).await;
    let rows = rows();
    let mut pairs = 0;
    for (names, exists) in lockstep_names(principal) {
        for row in ordered_rows(&rows) {
            if row.kind == Kind::Create && !exists {
                continue;
            }
            let actual = call(&bound, bound.token(principal.name), row, &names).await;
            if !row.flags.lockstep {
                if exists && !actual.denied() {
                    report.push(format!(
                        "{}: {} with `{}` must stay refused while bindings exist: {}",
                        principal.name,
                        row.rpc,
                        names.c,
                        describe_g(&actual)
                    ));
                }
                continue;
            }
            let reference = call(&unbound, unbound.token(principal.name), row, &names).await;
            pairs += 1;
            if actual.server_error() || reference.server_error() {
                report.push(format!(
                    "{}: {} with `{}` answered a server error: with bindings [{}], without [{}]",
                    principal.name,
                    row.rpc,
                    names.c,
                    describe_g(&actual),
                    describe_g(&reference)
                ));
            }
            if !same_g(&actual, &reference) {
                report.push(format!(
                    "{}: {} with `{}`: with bindings [{}], without [{}]",
                    principal.name,
                    row.rpc,
                    names.c,
                    describe_g(&actual),
                    describe_g(&reference)
                ));
            }
            if exists
                && row.rpc == "VLite/GetCollection"
                && granted(principal, &names.c)
                && actual.code != Code::Ok
            {
                report.push(format!(
                    "{}: an ordinary name must stay reachable: GetCollection `{}`: {}",
                    principal.name,
                    names.c,
                    describe_g(&actual)
                ));
            }
        }
    }
    pairs
}

async fn run_principal_g(index: usize) {
    let principal = &PRINCIPALS[index];
    assert!(
        principal.session.is_none(),
        "{} signs in with a console session, which gRPC does not accept",
        principal.name
    );
    let world = grpc_world(true, principal).await;
    let rows = rows();
    let mut run = GRun::new(&world, principal).await;
    for variant in variants(principal) {
        for row in ordered_rows(&rows) {
            run.probe(row, &variant).await;
        }
    }
    for namesake in namesakes(principal) {
        for row in ordered_rows(&rows) {
            run.send(row, &namesake).await;
        }
    }
    let mut report = std::mem::take(&mut run.report);
    let executed = run.executed;
    let pairs = lockstep_g(principal, &mut report).await;
    assert!(
        report.is_empty(),
        "{} mismatches for {}:\n{}",
        report.len(),
        principal.name,
        report
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        executed >= rows.len() * 3 && pairs >= rows.len() * 2,
        "{}: only {executed} probes and {pairs} pairs ran",
        principal.name
    );
}

macro_rules! grpc_principal_tests {
    ($($name:ident => $index:expr),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                run_principal_g($index).await;
            }
        )*
    };
}

grpc_principal_tests! {
    grpc_matrix_wildcard => 0,
    grpc_matrix_memory_only => 1,
    grpc_matrix_raw_other => 2,
    grpc_matrix_raw => 3,
    grpc_matrix_tenant => 4,
    grpc_matrix_tenant_raw => 5,
    grpc_matrix_admin => 6,
    grpc_matrix_read_only => 7,
}

#[test]
fn the_grpc_matrix_runs_every_token_principal() {
    let tokens: Vec<_> = PRINCIPALS
        .iter()
        .enumerate()
        .filter(|(_, principal)| principal.session.is_none())
        .map(|(index, _)| index)
        .collect();
    assert_eq!(tokens, [0, 1, 2, 3, 4, 5, 6, 7]);
}
