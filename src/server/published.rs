//! What a REST router publishes to the other transports of its engine
//! (`Service::publish` and `Service::adopt_published`): the pairing of
//! authentication state and memory reservations, what an unconfigured service
//! adopts, a router that protects less (no token, or no reserved resources)
//! never replacing one that protects more, a lock that a panic poisoned, the
//! order of the two routers, and a router that reserves being refused once a
//! gRPC router exists.

use super::*;
use crate::{CollectionSpec, CreateOptions, FormatVersion};
use std::path::{Path, PathBuf};

const FIRST: &str = "first_token_first_token_first_token_01";
const SECOND: &str = "second_token_second_token_second_tok_2";

const BINDINGS: &str = r#"{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{"principal":"abcdefghij27","scopes":[{"agent":"a"}]}]}]}"#;

fn service() -> Service {
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_graph("notes_graph").unwrap();
    db.create_collection(
        "notes_ev",
        CollectionSpec {
            dimensions: 2,
            settings: Default::default(),
        },
    )
    .unwrap();
    Service::new(db)
}

fn bindings_file(directory: &Path) -> PathBuf {
    let path = directory.join("bindings.json");
    std::fs::write(&path, BINDINGS).unwrap();
    path
}

fn options(token: &str, bindings: Option<&Path>) -> ServerOptions {
    ServerOptions {
        admin_token: Some(BearerToken::new(token).unwrap()),
        memory_bindings_file: bindings.map(Path::to_owned),
        ..ServerOptions::default()
    }
}

fn has_token(service: &Service, token: &str) -> bool {
    service
        .admin
        .auth
        .lock()
        .unwrap()
        .admin
        .as_ref()
        .is_some_and(|admin| admin.accepts(token))
}

fn poison<T: Send + 'static>(lock: &Arc<Mutex<T>>) {
    let lock = lock.clone();
    let _ = std::thread::spawn(move || {
        let _held = lock.lock().unwrap();
        panic!("poison the lock on purpose");
    })
    .join();
}

fn sequence(service: &Service) -> u64 {
    service.database.read().unwrap().as_ref().unwrap().sequence
}

const AFTER_GRPC: &str = "build every REST router before the gRPC router";

#[test]
fn a_service_that_no_router_configured_adopts_neither_and_says_so() {
    let mut service = service();
    let admin = service.admin.clone();
    let bindings = service.memory_bindings.clone();
    assert!(!service.grpc_runs_unconfigured());
    assert!(!service.adopt_published());
    assert!(Arc::ptr_eq(&service.admin, &admin));
    assert!(Arc::ptr_eq(&service.memory_bindings, &bindings));
    assert!(service.grpc_runs_unconfigured());
}

#[test]
fn a_router_publishes_its_tokens_and_its_reservations_together() {
    let directory = tempfile::tempdir().unwrap();
    let service = service();
    let _router = configured_admin_router(
        service.clone(),
        options(FIRST, Some(&bindings_file(directory.path()))),
    )
    .unwrap();
    // The caller's own handle never saw the router's state.
    assert!(!has_token(&service, FIRST));
    assert!(!service.memory_bindings.is_configured());

    let mut adopter = service.clone();
    assert!(adopter.adopt_published());
    assert!(has_token(&adopter, FIRST), "the tokens are adopted");
    assert!(
        adopter.memory_bindings.is_configured() && !adopter.memory_bindings.is_empty(),
        "the reservations are adopted"
    );
    assert!(
        !adopter.grpc_runs_unconfigured(),
        "an adopter that found a configuration is not a listener without one"
    );
}

#[test]
fn a_router_without_bindings_publishes_empty_reservations_with_its_tokens() {
    let service = service();
    let _router = configured_admin_router(service.clone(), options(FIRST, None)).unwrap();
    let mut adopter = service.clone();
    assert!(adopter.adopt_published());
    assert!(has_token(&adopter, FIRST));
    assert!(adopter.memory_bindings.is_empty());
}

#[test]
fn the_rest_router_must_come_before_the_grpc_router() {
    let directory = tempfile::tempdir().unwrap();
    let service = service();
    let mut early = service.clone();
    assert!(!early.adopt_published());

    let error = configured_admin_router(
        service.clone(),
        options(FIRST, Some(&bindings_file(directory.path()))),
    )
    .expect_err("a REST router after a gRPC router that adopted nothing is refused");
    assert!(
        error.to_string().contains("build the REST router first"),
        "{error}"
    );
    assert!(
        service.published_admin.lock().unwrap().is_none()
            && service.published_bindings.lock().unwrap().is_none(),
        "a refused router publishes nothing"
    );
}

#[test]
fn a_later_router_replaces_the_publication_for_listeners_built_after_it() {
    let service = service();
    let _first = configured_admin_router(service.clone(), options(FIRST, None)).unwrap();
    let mut early = service.clone();
    assert!(early.adopt_published());
    let _second = configured_admin_router(service.clone(), options(SECOND, None)).unwrap();
    let mut late = service.clone();
    assert!(late.adopt_published());

    assert!(has_token(&early, FIRST) && !has_token(&early, SECOND));
    assert!(has_token(&late, SECOND) && !has_token(&late, FIRST));
}

#[test]
fn a_router_that_reserves_nothing_does_not_replace_one_that_does() {
    let directory = tempfile::tempdir().unwrap();
    let service = service();
    let _reserving = configured_admin_router(
        service.clone(),
        options(FIRST, Some(&bindings_file(directory.path()))),
    )
    .unwrap();
    // A plain router and a token router of the same engine, built afterwards.
    let _plain = router(service.clone());
    let _token = authenticated_router(service.clone(), BearerToken::new(SECOND).unwrap());

    let mut adopter = service.clone();
    assert!(adopter.adopt_published());
    assert!(
        has_token(&adopter, FIRST) && !has_token(&adopter, SECOND),
        "the tokens that go with the reservations stay"
    );
    assert!(
        adopter.memory_bindings.is_configured() && !adopter.memory_bindings.is_empty(),
        "the reservations stay"
    );
}

#[test]
fn a_router_without_a_token_does_not_replace_one_that_has_one() {
    let service = service();
    let _secured = configured_admin_router(service.clone(), options(FIRST, None)).unwrap();
    let _open = router(service.clone());

    let mut adopter = service.clone();
    assert!(adopter.adopt_published());
    assert!(
        has_token(&adopter, FIRST),
        "a listener built after an open router stays authenticated"
    );
}

#[test]
fn a_router_with_a_token_replaces_one_without() {
    let service = service();
    let _open = router(service.clone());
    let _secured = authenticated_router(service.clone(), BearerToken::new(FIRST).unwrap());

    let mut adopter = service.clone();
    assert!(adopter.adopt_published());
    assert!(has_token(&adopter, FIRST));
}

#[test]
fn a_router_that_reserves_is_refused_once_a_grpc_router_exists() {
    let directory = tempfile::tempdir().unwrap();
    let service = service();
    // A plain router, then the listener that adopts it, then a router with a
    // token and a file of bindings: the listener would serve what it hides.
    let _plain = router(service.clone());
    let mut listener = service.clone();
    assert!(listener.adopt_published());
    let written = sequence(&service);

    let admin_directory = directory.path().join("admin");
    let mut late = options(FIRST, Some(&bindings_file(directory.path())));
    late.admin_directory = Some(admin_directory.clone());
    let error = configured_admin_router(service.clone(), late)
        .expect_err("a router that reserves is refused after a gRPC router");
    assert!(error.to_string().contains(AFTER_GRPC), "{error}");
    assert_eq!(
        sequence(&service),
        written,
        "the records of the file were not written"
    );
    assert!(!admin_directory.exists(), "nothing was set up");

    assert!(!has_token(&listener, FIRST) && listener.memory_bindings.is_empty());
    let mut next = service.clone();
    assert!(next.adopt_published());
    assert!(
        !has_token(&next, FIRST) && next.memory_bindings.is_empty(),
        "a refused router publishes nothing"
    );
}

#[test]
fn a_router_that_finds_reserved_records_is_refused_once_a_grpc_router_exists() {
    let directory = tempfile::tempdir().unwrap();
    let service = service();
    // The first router writes the records of the file, which the second is
    // not given: it finds records the file does not name, which reserve their
    // resources.
    let _first = configured_admin_router(
        service.clone(),
        options(FIRST, Some(&bindings_file(directory.path()))),
    )
    .unwrap();
    let mut listener = service.clone();
    assert!(listener.adopt_published());

    let admin_directory = directory.path().join("admin");
    let mut late = options(SECOND, None);
    late.admin_directory = Some(admin_directory.clone());
    let error = configured_admin_router(service.clone(), late)
        .expect_err("a router that finds reserved records is refused after a gRPC router");
    assert!(error.to_string().contains(AFTER_GRPC), "{error}");
    assert!(!admin_directory.exists(), "nothing was set up");
}

#[test]
fn publication_itself_refuses_a_router_that_reserves_after_a_grpc_router() {
    let service = service();
    let mut listener = service.clone();
    assert!(!listener.adopt_published());
    let reserving = service.clone().with_bindings(reserved::Bindings::new(
        crate::memory::binding::parse(BINDINGS.as_bytes()).unwrap(),
    ));

    let error = reserving
        .publish_reserving()
        .expect_err("the check and the publication share the locks");
    assert!(error.to_string().contains(AFTER_GRPC), "{error}");
    assert!(service.published_admin.lock().unwrap().is_none());
    assert!(service.published_bindings.lock().unwrap().is_none());
    // A router that reserves nothing is never refused.
    service.clone().publish_reserving().unwrap();
    assert!(service.published_admin.lock().unwrap().is_some());
}

#[test]
fn a_router_that_holds_reserved_records_without_a_file_still_reserves() {
    let directory = tempfile::tempdir().unwrap();
    let service = service();
    let _first = configured_admin_router(
        service.clone(),
        options(FIRST, Some(&bindings_file(directory.path()))),
    )
    .unwrap();
    // The file is gone and its records are not: the second router reserves
    // their resources with no owner, so it is no router that reserves nothing.
    let _second = configured_admin_router(service.clone(), options(SECOND, None)).unwrap();

    let mut adopter = service.clone();
    assert!(adopter.adopt_published());
    assert!(has_token(&adopter, SECOND) && !has_token(&adopter, FIRST));
    assert!(!adopter.memory_bindings.is_configured());
    assert!(
        !adopter.memory_bindings.is_empty(),
        "the resources of the records stay reserved"
    );
}

#[test]
fn a_router_that_reserves_replaces_one_that_does_not() {
    let directory = tempfile::tempdir().unwrap();
    let service = service();
    let _plain = authenticated_router(service.clone(), BearerToken::new(SECOND).unwrap());
    let _reserving = configured_admin_router(
        service.clone(),
        options(FIRST, Some(&bindings_file(directory.path()))),
    )
    .unwrap();

    let mut adopter = service.clone();
    assert!(adopter.adopt_published());
    assert!(has_token(&adopter, FIRST) && !has_token(&adopter, SECOND));
    assert!(adopter.memory_bindings.is_configured());
}

#[test]
fn a_publication_lock_that_a_panic_poisoned_is_recovered_not_skipped() {
    for which in 0..2 {
        let directory = tempfile::tempdir().unwrap();
        let service = service();
        if which == 0 {
            poison(&service.published_admin);
            assert!(service.published_admin.is_poisoned());
        } else {
            poison(&service.published_bindings);
            assert!(service.published_bindings.is_poisoned());
        }
        let _router = configured_admin_router(
            service.clone(),
            options(FIRST, Some(&bindings_file(directory.path()))),
        )
        .unwrap();

        let mut adopter = service.clone();
        assert!(adopter.adopt_published(), "lock {which}");
        assert!(has_token(&adopter, FIRST), "lock {which}: tokens");
        assert!(
            adopter.memory_bindings.is_configured(),
            "lock {which}: reservations"
        );
    }
}
