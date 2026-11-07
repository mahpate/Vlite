//! The publication in a build without gRPC. Nothing there reads it, so the
//! rules that exist for the listener (the refusal of a router that protects
//! less in some respect and more in another, the warning for a router that is
//! kept out) must neither refuse a router nor log in such a build: a REST-only
//! server behaves as it did before the publication existed.

use super::*;

const FIRST: &str = "first_token_first_token_first_token_01";

const NOTES: &str = r#"{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{"principal":"abcdefghij27","scopes":[{"agent":"a"}]}]}]}"#;

fn service() -> Service {
    Service::new(Database::create_in_memory(2, Default::default()).unwrap())
}

/// A router over `service` that reserves what `bindings` names, with an
/// administration token when `token` is given and without one otherwise.
fn router(service: &Service, bindings: &str, token: Option<&str>) -> Service {
    let mut router = service.clone().with_bindings(reserved::Bindings::new(
        crate::memory::binding::parse(bindings.as_bytes()).unwrap(),
    ));
    if let Some(token) = token {
        router.admin = Arc::new(admin::AdminState::default());
        router.admin.auth.lock().unwrap().admin = Some(BearerToken::new(token).unwrap());
    }
    router
}

#[test]
fn a_build_without_grpc_refuses_no_router_and_logs_nothing_for_the_publication() {
    let service = service();
    router(&service, NOTES, Some(FIRST)).publish();
    let (_, _, _, seen) = logs::buffer().snapshot();

    // Open and reserving `other`, over a router with a token that reserves
    // `notes`: each protects something the other does not, which a build with
    // gRPC refuses.
    router(&service, &NOTES.replace("notes", "other"), None)
        .publish_reserving()
        .expect("a build without gRPC reads no publication, so it refuses no router for it");
    // Open and reserving `notes`: kept out, with a warning in a build with gRPC.
    router(&service, NOTES, None)
        .publish_reserving()
        .expect("a router that protects less is kept out, not refused");

    let (entries, ..) = logs::buffer().snapshot();
    assert!(
        !entries
            .iter()
            .any(|entry| entry.seq > seen && entry.message.contains("publication_kept")),
        "the warning is for the listener that a build without gRPC does not have"
    );
}
