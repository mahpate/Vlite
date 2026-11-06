//! What a REST router publishes to the other transports of its engine
//! (`Service::publish` and `Service::adopt_published`): the pairing of
//! authentication state and memory reservations, what an unconfigured service
//! adopts, a router that protects less (no token, or reservations that do not
//! cover the held ones) never replacing one that protects more, a lock that a
//! panic poisoned, and the order of the routers: a configured router is
//! refused after a listener that runs without tokens (it adopted nothing, or
//! an open router), a router that reserves is refused after any listener, and
//! a router that protects less in some respect and more in another (the token,
//! each reserved resource) is refused.

use super::*;
use crate::{CollectionSpec, CreateOptions, FormatVersion};
use std::path::{Path, PathBuf};

const FIRST: &str = "first_token_first_token_first_token_01";
const SECOND: &str = "second_token_second_token_second_tok_2";

const BINDINGS: &str = r#"{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root","generation":1,"state":"active","graph":"notes_graph","evidence_collection":"notes_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{"principal":"abcdefghij27","scopes":[{"agent":"a"}]}]}]}"#;

/// A second binding, with its own graph and evidence collection.
const OTHER_BINDING: &str = r#"{"binding":"other","tenant":"@root","generation":1,"state":"active","graph":"other_graph","evidence_collection":"other_ev","schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1","digest_version":"memory-request-v1","receipt_window_ms":86400000,"grants":[{"principal":"abcdefghij34","scopes":[{"agent":"a"}]}]}"#;

fn service() -> Service {
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    for (graph, evidence) in [("notes_graph", "notes_ev"), ("other_graph", "other_ev")] {
        db.create_graph(graph).unwrap();
        db.create_collection(
            evidence,
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap();
    }
    Service::new(db)
}

fn bindings_file(directory: &Path) -> PathBuf {
    let path = directory.join("bindings.json");
    std::fs::write(&path, BINDINGS).unwrap();
    path
}

/// The text of `BINDINGS` with `OTHER_BINDING` added.
fn both_text() -> String {
    format!(
        "{},{OTHER_BINDING}]}}",
        BINDINGS.strip_suffix("]}").unwrap()
    )
}

/// `OTHER_BINDING` alone.
fn other_text() -> String {
    format!(r#"{{"version":"memory-bindings-v1","bindings":[{OTHER_BINDING}]}}"#)
}

/// The file of `bindings_file` with `OTHER_BINDING` added.
fn both_bindings_file(directory: &Path) -> PathBuf {
    let path = directory.join("both.json");
    std::fs::write(&path, both_text()).unwrap();
    path
}

fn options(token: &str, bindings: Option<&Path>) -> ServerOptions {
    ServerOptions {
        admin_token: Some(BearerToken::new(token).unwrap()),
        memory_bindings_file: bindings.map(Path::to_owned),
        ..ServerOptions::default()
    }
}

fn open_options(bindings: &Path) -> ServerOptions {
    ServerOptions {
        memory_bindings_file: Some(bindings.to_owned()),
        ..ServerOptions::default()
    }
}

/// A service whose own state has an administration token, as the state of a
/// router has.
fn with_token(mut service: Service, token: &str) -> Service {
    service.admin = Arc::new(admin::AdminState::default());
    service.admin.auth.lock().unwrap().admin = Some(BearerToken::new(token).unwrap());
    service
}

fn with_bindings_text(service: Service, text: &str) -> Service {
    service.with_bindings(reserved::Bindings::new(
        crate::memory::binding::parse(text.as_bytes()).unwrap(),
    ))
}

/// A service that reserves what `BINDINGS` names.
fn with_notes(service: Service) -> Service {
    with_bindings_text(service, BINDINGS)
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
const INCOMPARABLE: &str = "each protect something the other does not";

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
    let written = sequence(&service);

    let admin_directory = directory.path().join("admin");
    let mut late = options(FIRST, Some(&bindings_file(directory.path())));
    late.admin_directory = Some(admin_directory.clone());
    let error = configured_admin_router(service.clone(), late)
        .expect_err("a REST router after a gRPC router that adopted nothing is refused");
    assert!(
        error.to_string().contains("build the REST router first"),
        "{error}"
    );
    assert_eq!(sequence(&service), written, "nothing was written");
    assert!(!admin_directory.exists(), "nothing was set up");
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
    // A router with a token, then the listener that adopts it, then a router
    // with a file of bindings: the listener would serve what it hides.
    let _secured = authenticated_router(service.clone(), BearerToken::new(SECOND).unwrap());
    let mut listener = service.clone();
    assert!(listener.adopt_published());
    assert!(!listener.grpc_runs_unconfigured());
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

    let mut next = service.clone();
    assert!(next.adopt_published());
    assert!(
        has_token(&next, SECOND) && !has_token(&next, FIRST) && next.memory_bindings.is_empty(),
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
    let _secured = authenticated_router(service.clone(), BearerToken::new(SECOND).unwrap());
    let mut listener = service.clone();
    assert!(listener.adopt_published());
    // A token and reservations, over a held router that has a token and
    // reserves nothing: the only thing against it is the gRPC router.
    let reserving = with_notes(with_token(service.clone(), FIRST));

    let error = reserving
        .publish_reserving()
        .expect_err("the check and the publication share the locks");
    assert!(error.to_string().contains(AFTER_GRPC), "{error}");
    let mut next = service.clone();
    assert!(next.adopt_published());
    assert!(
        has_token(&next, SECOND) && !has_token(&next, FIRST) && next.memory_bindings.is_empty(),
        "nothing was published"
    );
    // A router that reserves nothing is never refused.
    service.clone().publish_reserving().unwrap();
}

#[test]
fn publication_itself_refuses_a_router_built_after_a_listener_without_tokens() {
    let service = service();
    let mut listener = service.clone();
    assert!(!listener.adopt_published());

    let error = service
        .clone()
        .publish_reserving()
        .expect_err("the check and the publication share the locks");
    assert!(
        error.to_string().contains("build the REST router first"),
        "{error}"
    );
    assert!(service.published_admin.lock().unwrap().is_none());
    assert!(service.published_bindings.lock().unwrap().is_none());
}

#[test]
fn a_listener_that_adopted_an_open_router_refuses_a_configured_one_after_it() {
    let directory = tempfile::tempdir().unwrap();
    let service = service();
    // `router` publishes an open state: the listener adopts it and serves every
    // caller as the administrator, which a router with a token must not follow.
    let _open = router(service.clone());
    let mut listener = service.clone();
    assert!(listener.adopt_published(), "the open state is adopted");
    assert!(listener.grpc_runs_unconfigured(), "and it is remembered");
    let written = sequence(&service);

    let admin_directory = directory.path().join("admin");
    let mut late = options(FIRST, None);
    late.admin_directory = Some(admin_directory.clone());
    let error = configured_admin_router(service.clone(), late)
        .expect_err("a router with a token is refused after a listener that is open");
    assert!(
        error.to_string().contains("build the REST router first"),
        "{error}"
    );
    assert_eq!(sequence(&service), written);
    assert!(!admin_directory.exists(), "nothing was set up");
    let mut next = service.clone();
    assert!(next.adopt_published());
    assert!(
        !has_token(&next, FIRST),
        "a refused router publishes nothing"
    );
}

#[test]
fn a_listener_that_adopted_a_router_with_a_token_is_not_open() {
    let service = service();
    let _secured = authenticated_router(service.clone(), BearerToken::new(FIRST).unwrap());
    let mut listener = service.clone();
    assert!(listener.adopt_published());
    assert!(has_token(&listener, FIRST));
    assert!(!listener.grpc_runs_unconfigured());
    // A router with another token is accepted, and the listener that exists
    // keeps the tokens it adopted (token files reloaded in place are followed,
    // another token is not): the tokens of a listener change by a new service
    // with new routers, not by another router.
    let _again = configured_admin_router(service.clone(), options(SECOND, None)).unwrap();
    assert!(has_token(&listener, FIRST) && !has_token(&listener, SECOND));
    let mut later = service.clone();
    assert!(later.adopt_published());
    assert!(has_token(&later, SECOND) && !has_token(&later, FIRST));
}

#[test]
fn a_router_that_reserves_without_a_token_is_refused_over_one_that_has_a_token_and_reserves_nothing()
 {
    let service = service();
    let _secured = authenticated_router(service.clone(), BearerToken::new(FIRST).unwrap());
    let reserving = with_notes(service.clone());

    let error = reserving
        .publish_reserving()
        .expect_err("keeping either state would leave a gRPC router without the other");
    assert!(error.to_string().contains(INCOMPARABLE), "{error}");
    let mut adopter = service.clone();
    assert!(adopter.adopt_published());
    assert!(
        has_token(&adopter, FIRST) && adopter.memory_bindings.is_empty(),
        "the publication that was held is untouched"
    );
    // The routers that cannot be refused keep it and warn.
    reserving.publish();
    let mut adopter = service.clone();
    assert!(adopter.adopt_published());
    assert!(has_token(&adopter, FIRST) && adopter.memory_bindings.is_empty());
}

#[test]
fn a_router_that_reserves_more_without_a_token_is_refused_over_one_with_a_token() {
    let directory = tempfile::tempdir().unwrap();
    let service = service();
    // The first router has a token and reserves `notes`; the second has no
    // token and reserves `notes` and `other`. Keeping the first would leave a
    // gRPC router that serves `other` raw to the holders of its token while
    // the second router hides it; following the second would leave it open.
    let _first = configured_admin_router(
        service.clone(),
        options(FIRST, Some(&bindings_file(directory.path()))),
    )
    .unwrap();
    let error = configured_admin_router(
        service.clone(),
        open_options(&both_bindings_file(directory.path())),
    )
    .expect_err("neither state can be kept without losing the other");
    assert!(error.to_string().contains(INCOMPARABLE), "{error}");

    let mut listener = service.clone();
    assert!(listener.adopt_published());
    assert!(has_token(&listener, FIRST), "the held tokens are untouched");
    let reserved = listener.memory_bindings.reserved();
    assert!(reserved.is_reserved(reserved::Resource::Graph, "notes_graph"));
    assert!(
        !reserved.is_reserved(reserved::Resource::Graph, "other_graph"),
        "the held reservations are untouched"
    );
}

#[test]
fn a_router_with_a_token_that_reserves_less_is_refused_over_an_open_one_that_reserves() {
    let service = service();
    // An open router that reserves `notes`, published as one is whose records
    // were never written (a read-only database): the router built next finds
    // no records, so it reserves nothing.
    with_notes(service.clone()).publish();
    let error = configured_admin_router(service.clone(), options(SECOND, None))
        .expect_err("neither state can be kept without losing the other");
    assert!(error.to_string().contains(INCOMPARABLE), "{error}");

    let mut listener = service.clone();
    assert!(listener.adopt_published());
    assert!(!has_token(&listener, SECOND), "the held state is untouched");
    assert!(!listener.memory_bindings.is_empty());
}

#[test]
fn a_router_that_reserves_part_of_what_the_held_one_does_is_kept_out_not_refused() {
    let service = service();
    // Both have a token. The second reserves `notes` and not `other`: following
    // it would leave a listener that serves `other` raw while the first router
    // hides it. Keeping it out loses nothing, because the held publication
    // protects at least as much in every respect, so it is not refused.
    with_bindings_text(with_token(service.clone(), FIRST), &both_text()).publish();
    with_notes(with_token(service.clone(), SECOND))
        .publish_reserving()
        .expect("a router that protects nothing more is kept out");

    let mut listener = service.clone();
    assert!(listener.adopt_published());
    assert!(has_token(&listener, FIRST) && !has_token(&listener, SECOND));
    let reserved = listener.memory_bindings.reserved();
    assert!(reserved.is_reserved(reserved::Resource::Graph, "notes_graph"));
    assert!(
        reserved.is_reserved(reserved::Resource::Graph, "other_graph"),
        "the held reservations stay"
    );
}

#[test]
fn two_routers_that_each_reserve_what_the_other_does_not_are_refused() {
    let service = service();
    with_notes(with_token(service.clone(), FIRST)).publish();
    let error = with_bindings_text(with_token(service.clone(), SECOND), &other_text())
        .publish_reserving()
        .expect_err("neither reserves everything the other does");
    assert!(error.to_string().contains(INCOMPARABLE), "{error}");

    let mut listener = service.clone();
    assert!(listener.adopt_published());
    assert!(has_token(&listener, FIRST) && !has_token(&listener, SECOND));
    let reserved = listener.memory_bindings.reserved();
    assert!(reserved.is_reserved(reserved::Resource::Graph, "notes_graph"));
    assert!(
        !reserved.is_reserved(reserved::Resource::Graph, "other_graph"),
        "the held reservations are untouched"
    );
}

#[test]
fn a_router_that_reserves_what_the_held_one_does_and_more_replaces_it() {
    let directory = tempfile::tempdir().unwrap();
    let service = service();
    let _first = configured_admin_router(
        service.clone(),
        options(FIRST, Some(&bindings_file(directory.path()))),
    )
    .unwrap();
    let _wider = configured_admin_router(
        service.clone(),
        options(SECOND, Some(&both_bindings_file(directory.path()))),
    )
    .unwrap();

    let mut listener = service.clone();
    assert!(listener.adopt_published());
    assert!(has_token(&listener, SECOND) && !has_token(&listener, FIRST));
    let reserved = listener.memory_bindings.reserved();
    assert!(reserved.is_reserved(reserved::Resource::Graph, "notes_graph"));
    assert!(reserved.is_reserved(reserved::Resource::Graph, "other_graph"));
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
fn a_router_that_reserves_without_a_token_is_kept_out_by_one_that_has_both() {
    let directory = tempfile::tempdir().unwrap();
    let service = service();
    let _held = configured_admin_router(
        service.clone(),
        options(FIRST, Some(&bindings_file(directory.path()))),
    )
    .unwrap();
    let reserving = with_notes(service.clone());

    // Nothing is lost by keeping the held publication: it has the token and
    // the reservations, so the router is kept out without a refusal.
    reserving.publish_reserving().unwrap();
    let mut adopter = service.clone();
    assert!(adopter.adopt_published());
    assert!(has_token(&adopter, FIRST));
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
