use super::*;
mod collector;
mod serving;
mod survivors;
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
};

const SECRET: &str = "sentinel-secret-9f31";
const TEXT: &str = "sentinel-private-text-7ab2";
const VECTOR: f32 = 987654.25;
const NETWORK_TEST_TIMEOUT: Duration = Duration::from_secs(12);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(NETWORK_TEST_TIMEOUT, future)
        .await
        .expect("network test exceeded its overall timeout")
}

fn config(endpoint: &str, kind: &str) -> Value {
    let url = reqwest::Url::parse(endpoint).unwrap();
    json!({
        "aliases":[{
            "alias":"test", "kind":kind, "endpoint":endpoint,
            "model":"pinned-model", "version":"2026-01",
            "dimensions": if kind == "embedding" { json!(2) } else { Value::Null },
            "connect_timeout_ms":2000, "total_timeout_ms":10000,
            "concurrency":2, "tenant_concurrency":1,
            "replay_safe":false, "tls_required":false, "allow_local_http":true,
            "endpoint_allowlist":{"schemes":["http"],"hosts":[url.host_str().unwrap()],"addresses":["127.0.0.1"]}
        }],
        "bindings":[{"lineage":42,"collection_id":0,"tenant":"tenant-a","alias":"test","payload_fields":[]}]
    })
}

fn registry(value: &Value) -> ProviderRegistry {
    ProviderRegistry::parse(&serde_json::to_vec(value).unwrap()).unwrap()
}

fn failure(value: &Value) -> ConfigReason {
    ProviderRegistry::parse_detailed(&serde_json::to_vec(value).unwrap())
        .err()
        .expect("invalid config should be rejected")
        .0
}

fn key() -> BindingKey {
    (42, 0, "tenant-a".into(), "test".into())
}

struct Fake {
    calls: AtomicUsize,
    status: u16,
    bytes: Vec<u8>,
    error: Option<ProviderError>,
    bodies: Mutex<Vec<Value>>,
}

impl Fake {
    fn new(status: u16, body: &str) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            status,
            bytes: body.as_bytes().to_vec(),
            error: None,
            bodies: Mutex::default(),
        }
    }
}

impl Transport for Fake {
    fn send<'a>(&'a self, request: TransportRequest<'a>) -> TransportFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.bodies
            .lock()
            .unwrap()
            .push(serde_json::from_slice(request.body).unwrap());
        Box::pin(async move {
            if let Some(error) = self.error {
                return Err(TransportFailure {
                    error,
                    retryable: error == ProviderError::Unavailable,
                });
            }
            Ok(TransportResponse {
                status: self.status,
                body: self.bytes.clone(),
                retry_after: None,
            })
        })
    }
}

struct Loopback {
    endpoint: String,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
    headers: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    active: Arc<Mutex<Option<TcpStream>>>,
    thread: Option<JoinHandle<()>>,
}

impl Loopback {
    fn new(responses: Vec<(String, Duration)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/inference", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let headers = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(Mutex::new(None));
        let (thread_calls, thread_bodies, thread_headers, thread_stop, thread_active) = (
            calls.clone(),
            bodies.clone(),
            headers.clone(),
            stop.clone(),
            active.clone(),
        );
        let thread = std::thread::spawn(move || {
            for (response, delay) in responses {
                let mut stream = loop {
                    if thread_stop.load(Ordering::SeqCst) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1))
                        }
                        Err(e) => panic!("loopback accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                *thread_active.lock().unwrap() = Some(stream.try_clone().unwrap());
                thread_calls.fetch_add(1, Ordering::SeqCst);
                let mut reader = BufReader::new(&mut stream);
                let mut size = 0;
                let mut request_headers = String::new();
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    return;
                }
                request_headers.push_str(&request_line);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    request_headers.push_str(&line);
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        size = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; size];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                thread_bodies.lock().unwrap().push(if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&body).unwrap()
                });
                thread_headers.lock().unwrap().push(request_headers);
                std::thread::sleep(delay);
                // Deadline/cap tests intentionally disconnect while the server writes.
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.shutdown(std::net::Shutdown::Write);
                let mut drain = [0; 1024];
                while matches!(stream.read(&mut drain), Ok(n) if n != 0) {}
                *thread_active.lock().unwrap() = None;
            }
        });
        Self {
            endpoint,
            calls,
            bodies,
            headers,
            stop,
            active,
            thread: Some(thread),
        }
    }
    fn json(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }
    fn one(body: &str) -> Self {
        Self::new(vec![(Self::json(200, body), Duration::ZERO)])
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(stream) = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            assert!(result.is_ok(), "loopback fixture thread failed");
        }
    }
}

struct QueueResolver {
    answers: Mutex<VecDeque<Vec<SocketAddr>>>,
    calls: AtomicUsize,
}

impl QueueResolver {
    fn new(answers: impl IntoIterator<Item = Vec<SocketAddr>>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().collect()),
            calls: AtomicUsize::new(0),
        }
    }
}

impl Resolver for QueueResolver {
    fn resolve(&self, _host: &str, _port: u16) -> Result<Vec<SocketAddr>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.answers.lock().unwrap().pop_front().unwrap_or_default())
    }
}

struct BlockingResolver {
    state: Arc<(Mutex<bool>, std::sync::Condvar)>,
    entered: AtomicBool,
}

impl Resolver for BlockingResolver {
    fn resolve(&self, _host: &str, _port: u16) -> Result<Vec<SocketAddr>, ProviderError> {
        self.entered.store(true, Ordering::SeqCst);
        let (released, changed) = &*self.state;
        let mut released = released.lock().unwrap();
        while !*released {
            let (next, timeout) = changed
                .wait_timeout(released, Duration::from_secs(20))
                .unwrap();
            released = next;
            assert!(
                !timeout.timed_out() || *released,
                "blocking resolver release timed out"
            );
        }
        Ok(vec!["127.0.0.1:1234".parse().unwrap()])
    }
}

struct BlockingReleaseGuard(Arc<(Mutex<bool>, std::sync::Condvar)>);

impl Drop for BlockingReleaseGuard {
    fn drop(&mut self) {
        let (released, changed) = &*self.0;
        *released.lock().unwrap_or_else(|error| error.into_inner()) = true;
        changed.notify_all();
    }
}

#[test]
fn dns_admission_rejects_before_scheduling_blocking_work() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (release, released) = std::sync::mpsc::channel();
    let (entered, observed) = std::sync::mpsc::channel();
    let worker = runtime.spawn_blocking(move || {
        entered.send(()).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(20));
    });
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut configured = hostname_config(
        "http://admission.invalid:1234/inference",
        "admission.invalid",
        "127.0.0.1".parse().unwrap(),
    );
    let resolver = Arc::new(QueueResolver::new([vec![
        "127.0.0.1:1234".parse().unwrap(),
    ]]));
    configured.providers.get_mut("test").unwrap().resolver = resolver.clone();
    let provider = &configured.providers["test"];
    let _slots = provider.dns_slots.try_acquire_many(2).unwrap();
    let result = runtime.block_on(async {
        let operation = HttpTransport.send(TransportRequest {
            provider,
            body: b"{}",
            remaining: Duration::from_secs(10),
        });
        tokio::pin!(operation);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match operation.as_mut().poll(&mut context) {
            std::task::Poll::Ready(Err(failure)) => Some(failure.error),
            _ => None,
        }
    });
    release.send(()).unwrap();
    runtime.block_on(worker).unwrap();
    assert_eq!(result, Some(ProviderError::Busy));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn ranked_ids_and_cardinality_are_checked_before_sorting() {
    let ids = BTreeSet::from([1, 2]);
    assert_eq!(
        check_ranked(
            &[Ranked { id: 1, cost: 0.0 }, Ranked { id: 1, cost: 1.0 }],
            2,
            &ids
        ),
        Err(ProviderError::InvalidResponse)
    );
    assert_eq!(
        check_ranked(&[Ranked { id: 3, cost: 0.0 }], 1, &ids),
        Err(ProviderError::InvalidResponse)
    );
    assert_eq!(
        check_ranked(&[], 1, &ids),
        Err(ProviderError::InvalidResponse)
    );
    assert_eq!(
        check_ranked(&[Ranked { id: 1, cost: 0.0 }], 1, &ids),
        Ok(())
    );
}

#[test]
fn nonfinite_embedding_components_and_ranked_scores_are_rejected() {
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            check_embedding(&[value, 0.0], 2),
            Err(ProviderError::InvalidResponse)
        );
        assert_eq!(
            check_ranked(&[Ranked { id: 1, cost: value }], 1, &BTreeSet::from([1])),
            Err(ProviderError::InvalidResponse)
        );
    }
    assert_eq!(check_embedding(&[0.0, 1.0], 2), Ok(()));
    assert_eq!(
        check_ranked(&[Ranked { id: 1, cost: -1.0 }], 1, &BTreeSet::from([1])),
        Ok(())
    );
}

#[test]
fn strict_config_roundtrip_and_invalid_shapes() {
    let base = config("http://127.0.0.1:1234/inference", "embedding");
    let parsed: Config = serde_json::from_value(base.clone()).unwrap();
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&parsed).unwrap()).is_ok());
    let mut cases = vec![
        json!({}),
        json!([]),
        json!({"aliases":[]}),
        json!({"aliases":[],"bindings":[],"secret":SECRET}),
    ];
    for field in base["aliases"][0].as_object().unwrap().keys() {
        let mut bad = base.clone();
        bad["aliases"][0].as_object_mut().unwrap().remove(field);
        cases.push(bad);
    }

    for (field, value) in [
        ("unexpected", json!(SECRET)),
        ("alias", json!("")),
        ("kind", json!("unknown")),
        ("dimensions", json!(0)),
        ("dimensions", json!(crate::MAX_DIMENSIONS + 1)),
        ("dimensions", json!("2")),
        ("model", json!("")),
        ("version", json!("latest")),
        ("endpoint", json!("not a URL")),
        (
            "endpoint",
            json!(format!("http://user:{SECRET}@127.0.0.1/")),
        ),
        ("endpoint", json!(format!("http://127.0.0.1/?key={SECRET}"))),
        ("endpoint", json!("http://example.invalid/")),
        ("secret", json!(SECRET)),
        ("secret", json!({"literal":SECRET})),
        (
            "secret",
            json!({"env":"VLITE_TEST_MISSING_SECRET_7AB24F31"}),
        ),
        ("secret", json!({"file":"relative-secret"})),
        ("connect_timeout_ms", json!(0)),
        ("connect_timeout_ms", json!(2001)),
        ("total_timeout_ms", json!(0)),
        ("total_timeout_ms", json!(10001)),
        ("total_timeout_ms", json!(50)),
        ("concurrency", json!(0)),
        ("concurrency", json!(65)),
        ("tenant_concurrency", json!(0)),
        ("tenant_concurrency", json!(3)),
        ("tls_required", json!(true)),
        ("allow_local_http", json!(false)),
        (
            "endpoint_allowlist",
            json!({"schemes":["http"],"hosts":["outside.invalid"],"addresses":["127.0.0.1"]}),
        ),
        (
            "endpoint_allowlist",
            json!({"schemes":["http"],"hosts":["127.0.0.1"],"addresses":["127.0.0.2"]}),
        ),
        (
            "endpoint_allowlist",
            json!({"schemes":["http"],"hosts":["127.0.0.1"],"addresses":["127.0.0.1"],"unknown":true}),
        ),
    ] {
        let mut bad = base.clone();
        bad["aliases"][0][field] = value;
        cases.push(bad);
    }
    let mut duplicate = base.clone();
    duplicate["aliases"]
        .as_array_mut()
        .unwrap()
        .push(base["aliases"][0].clone());
    cases.push(duplicate);
    let mut unknown_binding = base.clone();
    unknown_binding["bindings"][0]["unexpected"] = json!(SECRET);
    cases.push(unknown_binding);
    let mut unknown_alias = base.clone();
    unknown_alias["bindings"][0]["alias"] = json!("absent");
    cases.push(unknown_alias);
    let mut duplicate_binding = base.clone();
    duplicate_binding["bindings"]
        .as_array_mut()
        .unwrap()
        .push(base["bindings"][0].clone());
    cases.push(duplicate_binding);
    let mut invalid_fields = base.clone();
    invalid_fields["bindings"][0]["payload_fields"] = json!(["body"]);
    cases.push(invalid_fields);
    let mut too_many = base.clone();
    too_many["aliases"] = json!(vec![base["aliases"][0].clone(); 65]);
    cases.push(too_many);
    let mut too_many_bindings = base.clone();
    too_many_bindings["bindings"] = json!(vec![base["bindings"][0].clone(); 4097]);
    cases.push(too_many_bindings);
    for bad in cases {
        let error = match ProviderRegistry::parse(&serde_json::to_vec(&bad).unwrap()) {
            Err(error) => error,
            Ok(_) => panic!("accepted invalid shape"),
        };
        assert!(!error.contains(SECRET), "{error}");
        assert!(!error.contains(TEXT), "{error}");
    }
}

#[test]
fn configuration_failures_report_private_reasons_without_changing_public_message() {
    let base = config("http://127.0.0.1:1234/inference", "embedding");
    let cases = [
        ("alias", json!("a b"), ConfigReason::AliasName),
        ("alias", json!("a".repeat(257)), ConfigReason::AliasName),
        ("model", json!("m".repeat(257)), ConfigReason::ModelName),
        ("version", json!("Latest"), ConfigReason::Version),
        ("version", json!("DEFAULT"), ConfigReason::Version),
        ("version", json!("latest"), ConfigReason::Version),
        (
            "endpoint",
            json!("http://user@127.0.0.1/"),
            ConfigReason::EndpointCredentials,
        ),
        (
            "endpoint",
            json!("http://:pass@127.0.0.1/"),
            ConfigReason::EndpointCredentials,
        ),
        (
            "endpoint",
            json!("http://127.0.0.1/#frag"),
            ConfigReason::EndpointCredentials,
        ),
        (
            "endpoint",
            json!("http://127.0.0.1/?q=1"),
            ConfigReason::EndpointCredentials,
        ),
        ("connect_timeout_ms", json!(2001), ConfigReason::Timeout),
        ("total_timeout_ms", json!(1000), ConfigReason::Timeout),
        ("concurrency", json!(65), ConfigReason::Concurrency),
        ("tenant_concurrency", json!(3), ConfigReason::Concurrency),
        ("dimensions", json!(0), ConfigReason::Dimensions),
    ];
    for (field, value, reason) in cases {
        let mut candidate = base.clone();
        candidate["aliases"][0][field] = value;
        assert_eq!(failure(&candidate), reason);
        assert_eq!(ConfigFailure(reason).public_message(), CONFIG_ERROR);
    }

    let mut secret = base.clone();
    secret["aliases"][0]["secret"] = json!(SECRET);
    let error = ProviderRegistry::parse(&serde_json::to_vec(&secret).unwrap())
        .err()
        .expect("literal secrets must be rejected");
    assert!(!error.contains(SECRET));
}

#[test]
fn schema_caps_names_allowlists_concurrency_and_payload_fields_have_distinct_boundaries() {
    let base = config("http://127.0.0.1:1234/inference", "rerank");
    let alias = base["aliases"][0].clone();
    let mut at_alias_cap = base.clone();
    at_alias_cap["aliases"] = json!(
        (0..MAX_ALIASES)
            .map(|i| {
                let mut item = alias.clone();
                item["alias"] = json!(format!("alias-{i}"));
                item
            })
            .collect::<Vec<_>>()
    );
    at_alias_cap["bindings"] = json!([]);
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&at_alias_cap).unwrap()).is_ok());
    let mut over_alias_cap = at_alias_cap.clone();
    over_alias_cap["aliases"]
        .as_array_mut()
        .unwrap()
        .push(alias);
    assert_eq!(failure(&over_alias_cap), ConfigReason::AliasCount);

    let binding = base["bindings"][0].clone();
    let mut at_binding_cap = base.clone();
    at_binding_cap["bindings"] = json!(
        (0..MAX_BINDINGS)
            .map(|id| {
                let mut item = binding.clone();
                item["collection_id"] = json!(id);
                item
            })
            .collect::<Vec<_>>()
    );
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&at_binding_cap).unwrap()).is_ok());
    let mut over_binding_cap = at_binding_cap.clone();
    let mut next = binding.clone();
    next["collection_id"] = json!(MAX_BINDINGS);
    over_binding_cap["bindings"]
        .as_array_mut()
        .unwrap()
        .push(next);
    assert_eq!(failure(&over_binding_cap), ConfigReason::BindingCount);

    let mut max_names = config("http://127.0.0.1:1234/inference", "rerank");
    max_names["aliases"][0]["alias"] = json!("a".repeat(256));
    max_names["aliases"][0]["model"] = json!("m".repeat(256));
    max_names["aliases"][0]["version"] = json!("v".repeat(256));
    max_names["bindings"][0]["alias"] = json!("a".repeat(256));
    max_names["bindings"][0]["tenant"] = json!("t".repeat(256));
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&max_names).unwrap()).is_ok());

    let mut payloads = config("http://127.0.0.1:1234/inference", "rerank");
    payloads["bindings"][0]["payload_fields"] =
        json!((0..64).map(|i| format!("field{i}")).collect::<Vec<_>>());
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&payloads).unwrap()).is_ok());
    for fields in [
        json!((0..65).map(|i| format!("field{i}")).collect::<Vec<_>>()),
        json!(["field", "field"]),
        json!(["bad field"]),
    ] {
        payloads["bindings"][0]["payload_fields"] = fields;
        assert_eq!(failure(&payloads), ConfigReason::BindingFields);
    }
    let mut embedding_fields = config("http://127.0.0.1:1234/inference", "embedding");
    embedding_fields["bindings"][0]["payload_fields"] = json!(["field"]);
    assert_eq!(failure(&embedding_fields), ConfigReason::BindingFields);

    let allowed_64: Vec<_> = (1..=64).map(|value| format!("127.0.0.{value}")).collect();
    let allowed_65: Vec<_> = (1..=65).map(|value| format!("127.0.0.{value}")).collect();
    let mut address_cap = config("http://127.0.0.1:1234/inference", "embedding");
    address_cap["aliases"][0]["endpoint_allowlist"]["addresses"] = json!(allowed_64);
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&address_cap).unwrap()).is_ok());
    address_cap["aliases"][0]["endpoint_allowlist"]["addresses"] = json!(allowed_65);
    assert_eq!(failure(&address_cap), ConfigReason::EndpointAllowlist);

    let mut max_concurrency = config("http://127.0.0.1:1234/inference", "embedding");
    max_concurrency["aliases"][0]["connect_timeout_ms"] = json!(2000);
    max_concurrency["aliases"][0]["total_timeout_ms"] = json!(2000);
    max_concurrency["aliases"][0]["concurrency"] = json!(64);
    max_concurrency["aliases"][0]["tenant_concurrency"] = json!(64);
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&max_concurrency).unwrap()).is_ok());

    for (field, invalid) in [
        ("schemes", json!([])),
        ("schemes", json!(["http", "https"])),
        ("schemes", json!(["https"])),
        ("hosts", json!([])),
        ("hosts", json!(["outside.invalid"])),
        ("addresses", json!([])),
        ("addresses", json!(["127.0.0.2"])),
    ] {
        let mut candidate = config("http://127.0.0.1:1234/inference", "embedding");
        candidate["aliases"][0]["endpoint_allowlist"][field] = invalid;
        assert_eq!(failure(&candidate), ConfigReason::EndpointAllowlist);
    }

    let mut outside_http = config("http://example.invalid:1234/inference", "embedding");
    outside_http["aliases"][0]["endpoint_allowlist"]["hosts"] = json!(["example.invalid"]);
    outside_http["aliases"][0]["endpoint_allowlist"]["addresses"] = json!(["192.0.2.1"]);
    assert_eq!(failure(&outside_http), ConfigReason::TlsPolicy);

    let mut localhost_http = config("http://localhost:1234/inference", "embedding");
    localhost_http["aliases"][0]["endpoint_allowlist"]["hosts"] = json!(["localhost"]);
    localhost_http["aliases"][0]["endpoint_allowlist"]["addresses"] = json!(["127.0.0.1"]);
    assert_eq!(failure(&localhost_http), ConfigReason::TlsPolicy);
}

#[test]
fn secret_policy_helpers_are_platform_neutral_and_precise() {
    assert!(mode_is_private(0o600));
    for mode in [0o640, 0o604, 0o077] {
        assert!(!mode_is_private(mode));
    }
    assert_eq!(trim_secret_file(b"SECRET\r\n").unwrap(), b"SECRET");
    assert_eq!(validate_secret_bytes(b"SECRET").unwrap(), "SECRET");
    let raw_limit_with_newline = vec![b'a'; MAX_SECRET_BYTES - 1]
        .into_iter()
        .chain(*b"\n")
        .collect::<Vec<_>>();
    assert_eq!(
        trim_secret_file(&raw_limit_with_newline).unwrap().len(),
        MAX_SECRET_BYTES - 1
    );
    let raw_over_limit_with_newline = vec![b'a'; MAX_SECRET_BYTES]
        .into_iter()
        .chain(*b"\n")
        .collect::<Vec<_>>();
    assert_eq!(
        trim_secret_file(&raw_over_limit_with_newline),
        Err(SecretReason::TooLarge)
    );
    assert_eq!(
        trim_secret_file(&vec![b'a'; 4097]),
        Err(SecretReason::TooLarge)
    );
    for invalid in [b"".as_slice(), b"has space"] {
        let bytes = trim_secret_file(invalid).unwrap();
        assert!(validate_secret_bytes(bytes).is_err());
    }
}

#[test]
fn address_check_rejects_empty_mixed_and_over_limit_answers() {
    let allowed: IpAddr = "127.0.0.1".parse().unwrap();
    let other: IpAddr = "127.0.0.2".parse().unwrap();
    assert_eq!(
        check_addresses(
            &[SocketAddr::new(allowed, 1234), SocketAddr::new(other, 1234)],
            &[allowed]
        ),
        Err(ProviderError::Unavailable)
    );
    assert_eq!(
        check_addresses(&[], &[allowed]),
        Err(ProviderError::Unavailable)
    );
    let at_limit: Vec<_> = (0..MAX_ALLOWLIST_ADDRESSES)
        .map(|p| SocketAddr::new(allowed, 1000 + p as u16))
        .collect();
    assert_eq!(
        check_addresses(&at_limit, &[allowed]).unwrap().len(),
        MAX_ALLOWLIST_ADDRESSES
    );
    let over: Vec<_> = (0..=MAX_ALLOWLIST_ADDRESSES)
        .map(|p| SocketAddr::new(allowed, 1000 + p as u16))
        .collect();
    assert_eq!(
        check_addresses(&over, &[allowed]),
        Err(ProviderError::Unavailable)
    );
}

fn hostname_config(endpoint: &str, host: &str, address: IpAddr) -> ProviderRegistry {
    let url = reqwest::Url::parse(endpoint).unwrap();
    let port = url.port().unwrap();
    let local_endpoint = format!("http://{address}:{port}/inference");
    let value = config(&local_endpoint, "embedding");
    let mut registry = registry(&value);
    registry.providers.get_mut("test").unwrap().endpoint = reqwest::Url::parse(endpoint).unwrap();
    registry
        .providers
        .get_mut("test")
        .unwrap()
        .definition
        .endpoint_allowlist
        .hosts = vec![host.to_owned()];
    registry
}

#[tokio::test]
async fn pinned_hostname_uses_only_the_injected_allowed_address() {
    let server = Loopback::one(r#"{"data":[{"embedding":[1,2]}]}"#);
    let socket: SocketAddr = server
        .endpoint
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let mut registry = hostname_config(
        &format!("http://pinned.invalid:{}/inference", socket.port()),
        "pinned.invalid",
        socket.ip(),
    );
    let resolver = Arc::new(QueueResolver::new([vec![socket]]));
    registry.providers.get_mut("test").unwrap().resolver = resolver.clone();
    assert_eq!(
        bounded(registry.embed(&key(), TEXT, &HttpTransport))
            .await
            .unwrap(),
        vec![1.0, 2.0]
    );
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    assert!(
        server.headers.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("host: pinned.invalid:")
    );
}

#[tokio::test]
async fn replay_retry_re_resolves_and_rejects_a_new_disallowed_answer() {
    let server = Loopback::new(vec![
        (Loopback::json(503, ""), Duration::ZERO),
        (
            Loopback::json(200, r#"{"data":[{"embedding":[1,2]}]}"#),
            Duration::ZERO,
        ),
    ]);
    let socket: SocketAddr = server
        .endpoint
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let other = SocketAddr::new("127.0.0.2".parse().unwrap(), socket.port());
    let mut registry = hostname_config(
        &format!("http://pinned.invalid:{}/inference", socket.port()),
        "pinned.invalid",
        socket.ip(),
    );
    registry
        .providers
        .get_mut("test")
        .unwrap()
        .definition
        .replay_safe = true;
    let resolver = Arc::new(QueueResolver::new([vec![socket], vec![socket, other]]));
    registry.providers.get_mut("test").unwrap().resolver = resolver.clone();
    assert_eq!(
        bounded(registry.embed(&key(), TEXT, &HttpTransport))
            .await
            .unwrap_err(),
        ProviderError::Unavailable
    );
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn timed_out_blocking_dns_keeps_its_admission_slot_until_returned() {
    let mut registry = hostname_config(
        "http://stalled.invalid:1234/inference",
        "stalled.invalid",
        "127.0.0.1".parse().unwrap(),
    );
    registry
        .providers
        .get_mut("test")
        .unwrap()
        .definition
        .concurrency = 1;
    registry
        .providers
        .get_mut("test")
        .unwrap()
        .definition
        .tenant_concurrency = 1;
    registry
        .providers
        .get_mut("test")
        .unwrap()
        .definition
        .connect_timeout_ms = 1000;
    registry
        .providers
        .get_mut("test")
        .unwrap()
        .definition
        .total_timeout_ms = 1000;
    registry.providers.get_mut("test").unwrap().slots = Arc::new(Semaphore::new(1));
    registry.providers.get_mut("test").unwrap().dns_slots = Arc::new(Semaphore::new(1));
    registry.bindings.get_mut(&key()).unwrap().slots = Arc::new(Semaphore::new(1));
    let resolver = Arc::new(BlockingResolver {
        state: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
        entered: AtomicBool::new(false),
    });
    let _release_guard = BlockingReleaseGuard(resolver.state.clone());
    registry.providers.get_mut("test").unwrap().resolver = resolver.clone();
    assert_eq!(
        bounded(registry.embed(&key(), TEXT, &HttpTransport))
            .await
            .unwrap_err(),
        ProviderError::Timeout
    );
    let entered_deadline = Instant::now() + Duration::from_secs(5);
    while !resolver.entered.load(Ordering::SeqCst) && Instant::now() < entered_deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        resolver.entered.load(Ordering::SeqCst),
        "resolver thread did not start within 5 seconds"
    );
    let second = tokio::time::timeout(
        Duration::from_secs(2),
        registry.embed(&key(), TEXT, &HttpTransport),
    )
    .await;
    assert_eq!(
        second
            .expect("provider admission must not wait")
            .unwrap_err(),
        ProviderError::Busy
    );
}

#[test]
fn named_environment_secret_is_loaded_without_disclosure() {
    const VARIABLE: &str = "VLITE_PROVIDERS_FAKE_TEST_SECRET";
    if std::env::var_os("VLITE_PROVIDERS_TEST_CHILD").is_some() {
        assert_eq!(
            load_secret(&SecretSource::Env(VARIABLE.into())).unwrap(),
            SECRET
        );
        let mut value = config("http://127.0.0.1:1234/inference", "embedding");
        value["aliases"][0]["secret"] = json!({"env":VARIABLE});
        let registry = registry(&value);
        assert_eq!(registry.providers["test"].secret.as_deref(), Some(SECRET));
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "server::providers::tests::named_environment_secret_is_loaded_without_disclosure",
            "--nocapture",
        ])
        .env("VLITE_PROVIDERS_TEST_CHILD", "1")
        .env(VARIABLE, SECRET)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
}

#[test]
fn environment_secret_values_are_bounded_printable_and_sanitized() {
    const CHILD: &str = "VLITE_PROVIDERS_INVALID_SECRET_CHILD";
    const VARIABLE: &str = "VLITE_PROVIDERS_INVALID_SECRET_VALUE";
    if std::env::var_os(CHILD).is_some() {
        assert_eq!(
            load_secret(&SecretSource::Env(VARIABLE.into())),
            Err(SecretReason::InvalidValue)
        );
        return;
    }
    for value in [
        "".to_owned(),
        "x".repeat(MAX_SECRET_BYTES + 1),
        "has space".into(),
        "SECRET\n".into(),
        "SECRET ".into(),
        "SECRET\r\n".into(),
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "server::providers::tests::environment_secret_values_are_bounded_printable_and_sanitized",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(VARIABLE, &value)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        if !value.is_empty() {
            assert!(!String::from_utf8_lossy(&output.stdout).contains(&value));
            assert!(!String::from_utf8_lossy(&output.stderr).contains(&value));
        }
    }
}

#[tokio::test]
async fn every_provoked_error_and_captured_log_is_sanitized() {
    let mut registry = registry(&config("http://127.0.0.1:1234/inference", "embedding"));
    registry.providers.get_mut("test").unwrap().secret = Some(SECRET.into());
    let body = format!(r#"{{"secret":"{SECRET}","text":"{TEXT}","vector":[{VECTOR}]}}"#);
    let fake = Fake::new(200, &body);
    let missing = registry
        .embed(&(99, 0, "tenant-a".into(), "test".into()), TEXT, &fake)
        .await
        .unwrap_err();
    let invalid = registry
        .embed(&key(), &format!("{TEXT}{}", "x".repeat(16384)), &fake)
        .await
        .unwrap_err();
    let response = registry.embed(&key(), TEXT, &fake).await.unwrap_err();
    let unavailable = registry
        .embed(&key(), TEXT, &Fake::new(401, &body))
        .await
        .unwrap_err();
    let mut timeout = Fake::new(200, &body);
    timeout.error = Some(ProviderError::Timeout);
    let timeout = registry.embed(&key(), TEXT, &timeout).await.unwrap_err();
    let permit = tokio::time::timeout(
        Duration::from_secs(5),
        registry.providers["test"].slots.acquire_many(2),
    )
    .await
    .expect("provider permits were not returned within five seconds")
    .unwrap();
    let busy = bounded(registry.embed(&key(), TEXT, &fake))
        .await
        .unwrap_err();
    drop(permit);
    assert_eq!(
        [missing, invalid, response, unavailable, timeout, busy],
        [
            ProviderError::NotConfigured,
            ProviderError::InvalidInput,
            ProviderError::InvalidResponse,
            ProviderError::Unavailable,
            ProviderError::Timeout,
            ProviderError::Busy,
        ]
    );
    let captured = serde_json::to_string(&super::super::logs::buffer().snapshot().0).unwrap();
    for error in [missing, invalid, response, unavailable, timeout, busy] {
        for sentinel in [SECRET.to_owned(), TEXT.to_owned(), VECTOR.to_string()] {
            assert!(!error.to_string().contains(&sentinel));
            assert!(!captured.contains(&sentinel));
        }
    }
}

#[tokio::test]
async fn encoded_request_exact_byte_boundary_and_dimension_boundaries() {
    let registry = registry(&config("http://127.0.0.1:1234/inference", "embedding"));
    let (provider, bound) = registry.bound(&key(), Kind::Embedding).unwrap();
    let fake = Fake::new(200, "{}");
    let overhead = serde_json::to_vec(&json!({"pad":""})).unwrap().len();
    for (bytes, valid) in [(REQUEST_BYTES, true), (REQUEST_BYTES + 1, false)] {
        let body = json!({"pad":"x".repeat(bytes-overhead)});
        assert_eq!(serde_json::to_vec(&body).unwrap().len(), bytes);
        let result = call(provider, bound, body, Instant::now(), &fake).await;
        if valid {
            assert!(result.is_ok());
        } else {
            assert_eq!(result.unwrap_err(), ProviderError::InvalidInput);
        }
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    for dimension in [1, crate::MAX_DIMENSIONS] {
        let mut value = config("http://127.0.0.1:1234/inference", "embedding");
        value["aliases"][0]["dimensions"] = json!(dimension);
        let fake = Fake::new(
            200,
            &json!({"data":[{"embedding":vec![1.0;dimension]}]}).to_string(),
        );
        assert_eq!(
            super::ProviderRegistry::parse(&serde_json::to_vec(&value).unwrap())
                .unwrap()
                .embed(&key(), TEXT, &fake)
                .await
                .unwrap()
                .len(),
            dimension
        );
    }
}
#[test]
fn private_secret_files_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("private");
    std::fs::write(&path, SECRET).unwrap();
    let absolute = std::fs::canonicalize(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_secret(&SecretSource::File(absolute.clone())).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            load_secret(&SecretSource::File(absolute.clone())).unwrap(),
            SECRET
        );
        std::fs::write(&path, format!("{SECRET}{}", "\n".repeat(4097))).unwrap();
        assert!(load_secret(&SecretSource::File(absolute.clone())).is_err());
        std::fs::write(&path, SECRET).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&absolute, &link).unwrap();
        assert!(load_secret(&SecretSource::File(link)).is_err());
    }
    #[cfg(not(unix))]
    assert!(
        load_secret(&SecretSource::File(absolute)).unwrap_err()
            == SecretReason::UnsupportedPlatform
    );
}

#[test]
fn tls_and_loopback_policy_are_validated_without_dns() {
    let mut value = config("http://127.0.0.1:1234/inference", "embedding");
    value["aliases"][0]["endpoint"] = json!("http://example.invalid/inference");
    value["aliases"][0]["endpoint_allowlist"]["hosts"] = json!(["example.invalid"]);
    value["aliases"][0]["endpoint_allowlist"]["addresses"] = json!(["192.0.2.1"]);
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    value["aliases"][0]["endpoint"] = json!("https://example.invalid/inference");
    value["aliases"][0]["endpoint_allowlist"]["schemes"] = json!(["https"]);
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    value["aliases"][0]["tls_required"] = json!(true);
    value["aliases"][0]["allow_local_http"] = json!(false);
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&value).unwrap()).is_ok());
    let mut ipv6 = config("http://[::1]:1234/inference", "embedding");
    ipv6["aliases"][0]["endpoint_allowlist"]["addresses"] = json!(["::1"]);
    assert!(ProviderRegistry::parse(&serde_json::to_vec(&ipv6).unwrap()).is_ok());
    ipv6["aliases"][0]["endpoint"] = json!("http://localhost:1234/inference");
    ipv6["aliases"][0]["endpoint_allowlist"]["hosts"] = json!(["localhost"]);
    assert_eq!(failure(&ipv6), ConfigReason::TlsPolicy);
}

#[tokio::test]
async fn custom_candidate_ceiling_is_inclusive() {
    let registry = registry(&config("http://127.0.0.1:1234/inference", "rerank"));
    let fake = Fake::new(200, r#"{"results":[{"id":1,"cost":0.1}]}"#);
    let candidates: Vec<_> = (1..=1000).map(candidate).collect();
    assert_eq!(
        registry
            .rerank(&key(), &[VECTOR, 1.0], &candidates, 1, &fake)
            .await
            .unwrap()[0]
            .id,
        1
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    let candidates: Vec<_> = (1..=1001).map(candidate).collect();
    assert_eq!(
        registry
            .rerank(&key(), &[VECTOR, 1.0], &candidates, 1, &fake)
            .await
            .unwrap_err(),
        ProviderError::InvalidInput
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}
#[test]
fn file_read_errors_and_size_are_sanitized() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        ProviderRegistry::from_file(&directory.path().join(SECRET))
            .err()
            .unwrap()
            .contains("unavailable")
    );
    let path = directory.path().join("config.json");
    std::fs::write(&path, vec![b' '; CONFIG_BYTES + 1]).unwrap();
    assert!(
        ProviderRegistry::from_file(&path)
            .err()
            .unwrap()
            .contains("exceeds")
    );
    std::fs::write(&path, format!("{{\"secret\":\"{SECRET}\"}}")).unwrap();
    assert!(
        !ProviderRegistry::from_file(&path)
            .err()
            .unwrap()
            .contains(SECRET)
    );
}

#[test]
fn error_mapping_is_exact_and_sanitized() {
    for (error, expected) in [
        (
            ProviderError::NotConfigured,
            (409, "provider_not_configured", "FAILED_PRECONDITION"),
        ),
        (
            ProviderError::InvalidInput,
            (422, "invalid_provider_input", "INVALID_ARGUMENT"),
        ),
        (
            ProviderError::InvalidResponse,
            (502, "provider_invalid_response", "UNAVAILABLE"),
        ),
        (
            ProviderError::Unavailable,
            (503, "provider_unavailable", "UNAVAILABLE"),
        ),
        (
            ProviderError::Timeout,
            (504, "provider_timeout", "DEADLINE_EXCEEDED"),
        ),
        (
            ProviderError::Busy,
            (429, "provider_busy", "RESOURCE_EXHAUSTED"),
        ),
    ] {
        assert_eq!(error.mapping(), expected);
        for sentinel in [SECRET.to_owned(), TEXT.to_owned(), VECTOR.to_string()] {
            assert!(!error.to_string().contains(&sentinel));
            assert!(!format!("{error:?}").contains(&sentinel));
        }
    }
}

#[test]
fn rerank_signed_zero_costs_tie_by_id() {
    let mut ranked = [Ranked { id: 2, cost: -0.0 }, Ranked { id: 1, cost: 0.0 }];
    sort_ranked(&mut ranked);
    assert_eq!(
        ranked.iter().map(|result| result.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(ranked.iter().all(|result| result.cost.is_sign_positive()));
}

#[tokio::test]
async fn zero_outbound_without_config_or_bindings_including_existing_handlers() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    for value in [json!({"aliases":[],"bindings":[]}), {
        let mut value = config("http://127.0.0.1:1234/inference", "embedding");
        value["bindings"] = json!([]);
        value
    }] {
        let fake = Arc::new(Fake::new(200, r#"{"data":[{"embedding":[1,2]}]}"#));
        let mut registry = registry(&value);
        registry.transport = fake.clone();
        assert!(registry.configured());
        assert_eq!(
            registry
                .embed_bound(&key(), TEXT, Instant::now())
                .await
                .unwrap_err(),
            ProviderError::NotConfigured
        );
        let db = crate::Database::create_in_memory(2, Default::default()).unwrap();
        let service = super::super::Service::new(db).with_providers(registry);
        assert!(service.providers().bindings.is_empty());
        let app = super::super::router(service);
        for (route, body) in [
            ("/v1/search", json!({"vector":[1,0],"k":1})),
            (
                "/v1/search",
                json!({"vector":[1,0],"k":1,"rerank":{"mode":"exact","candidates":2}}),
            ),
            ("/v1/collections/default/embed", json!({"text":TEXT})),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(route)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(response.status().is_success(), "{}", response.status());
        }
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("bound-providers.vlite");
    let mut database = crate::Database::create(&database_path, 2).unwrap();
    let lineage = database.lineage().unwrap();
    let collection_id = database.collection("default").unwrap().id();
    database.put(1, &[1.0, 0.0], b"one").unwrap();
    database.put(2, &[0.0, 1.0], b"two").unwrap();
    let mut value = config("http://127.0.0.1:1234/inference", "embedding");
    value["bindings"][0]["lineage"] = json!(lineage);
    value["bindings"][0]["collection_id"] = json!(collection_id);
    value["bindings"][0]["tenant"] = json!("global");
    let fake = Arc::new(Fake::new(200, r#"{"data":[{"embedding":[1,2]}]}"#));
    let mut registry = registry(&value);
    registry.transport = fake.clone();
    let key = (lineage, collection_id, "global".into(), "test".into());
    let service = super::super::Service::new(database).with_providers(registry);
    assert_eq!(
        service
            .providers()
            .embed_bound(&key, TEXT, Instant::now())
            .await
            .unwrap(),
        vec![1.0, 2.0]
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    fake.calls.store(0, Ordering::SeqCst);
    let app = super::super::router(service);
    for (route, body) in [
        ("/v1/search", json!({"vector":[1,0],"k":1})),
        (
            "/v1/search",
            json!({"vector":[1,0],"k":1,"rerank":{"mode":"exact","candidates":2}}),
        ),
        ("/v1/collections/default/embed", json!({"text":TEXT})),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post(route)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    assert!(ProviderRegistry::default().providers.is_empty());
    assert!(!ProviderRegistry::default().configured());
}

#[tokio::test]
async fn provider_wire_contract_uses_only_pinned_fields_and_bearer_auth() {
    let server = Loopback::one(r#"{"data":[{"embedding":[1,2]}]}"#);
    let mut configured = registry(&config(&server.endpoint, "embedding"));
    configured.providers.get_mut("test").unwrap().secret = Some(SECRET.into());
    assert_eq!(
        bounded(configured.embed_bound(&key(), TEXT, Instant::now()))
            .await
            .unwrap(),
        vec![1.0, 2.0]
    );
    {
        let headers = server.headers.lock().unwrap();
        assert_eq!(
            headers[0]
                .lines()
                .filter(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                .count(),
            1
        );
        assert!(
            headers[0]
                .to_ascii_lowercase()
                .contains(&format!("authorization: bearer {SECRET}").to_ascii_lowercase())
        );
        assert!(
            headers[0]
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert!(!server.endpoint.contains(SECRET));
        let bodies = server.bodies.lock().unwrap();
        let body = &bodies[0];
        assert_eq!(
            body,
            &json!({"model":"pinned-model","version":"2026-01","input":TEXT})
        );
        assert!(!body.to_string().contains(SECRET));
    }

    let server = Loopback::one(r#"{"data":[{"embedding":[1,2]}]}"#);
    assert!(
        bounded(
            registry(&config(&server.endpoint, "embedding")).embed_bound(
                &key(),
                TEXT,
                Instant::now()
            )
        )
        .await
        .is_ok()
    );
    assert!(
        !server.headers.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("authorization:")
    );
}

#[tokio::test]
async fn provider_http_client_ignores_proxy_environment() {
    const CHILD: &str = "VLITE_PROVIDER_PROXY_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let target = Loopback::one(r#"{"data":[{"embedding":[1,2]}]}"#);
        let socket: SocketAddr = target
            .endpoint
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let mut configured = hostname_config(
            &format!("http://pinned.invalid:{}/inference", socket.port()),
            "pinned.invalid",
            socket.ip(),
        );
        configured.providers.get_mut("test").unwrap().resolver =
            Arc::new(QueueResolver::new([vec![socket]]));
        assert_eq!(
            bounded(configured.embed(&key(), TEXT, &HttpTransport))
                .await
                .unwrap(),
            vec![1.0, 2.0]
        );
        assert_eq!(target.calls.load(Ordering::SeqCst), 1);
        return;
    }
    let proxy = Loopback::new(vec![(Loopback::json(502, "proxy"), Duration::ZERO)]);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "server::providers::tests::provider_http_client_ignores_proxy_environment",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("HTTP_PROXY", &proxy.endpoint)
        .env("http_proxy", &proxy.endpoint)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    assert_eq!(proxy.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provider_log_contains_only_alias_fingerprint_outcome_and_batch_count() {
    let alias = "unique-log-alias-7391";
    let tenant = "unique-log-tenant-2648";
    let host = "unique-log-host.invalid";
    let path = "/unique-private-path-613";
    let mut value = config("http://127.0.0.1:1234/inference", "embedding");
    value["aliases"][0]["alias"] = json!(alias);
    value["bindings"][0]["alias"] = json!(alias);
    value["bindings"][0]["tenant"] = json!(tenant);
    let mut registry = registry(&value);
    registry.providers.get_mut(alias).unwrap().endpoint =
        reqwest::Url::parse(&format!("http://{host}{path}")).unwrap();
    let log_key = (42, 0, tenant.to_owned(), alias.to_owned());
    let transport = Fake::new(200, r#"{"data":[{"embedding":[1,2]}]}"#);
    assert_eq!(
        registry.embed(&log_key, TEXT, &transport).await.unwrap(),
        vec![1.0, 2.0]
    );
    let entries: Vec<_> = super::super::logs::buffer()
        .snapshot()
        .0
        .into_iter()
        .filter(|entry| entry.target == "providers" && entry.fields["alias"] == alias)
        .collect();
    assert!(!entries.is_empty());
    for entry in entries {
        assert_eq!(entry.fields["outcome"], "ok");
        assert_eq!(entry.fields["batch_count"], 1);
        assert!(!entry.fields["tenant_fingerprint"].is_null());
        let serialized = serde_json::to_string(&entry).unwrap();
        for forbidden in [tenant, host, path, SECRET, TEXT] {
            assert!(!serialized.contains(forbidden));
        }
        assert!(!serialized.contains(&VECTOR.to_string()));
    }
}

#[tokio::test]
async fn loopback_embedding_success_and_bad_response_shapes() {
    for (body, expected) in [
        (r#"{"data":[{"embedding":[1,2]}]}"#, None),
        ("malformed", Some(ProviderError::InvalidResponse)),
        (
            r#"{"data":[{"embedding":[NaN,2]}]}"#,
            Some(ProviderError::InvalidResponse),
        ),
        (
            r#"{"data":[{"embedding":[Infinity,2]}]}"#,
            Some(ProviderError::InvalidResponse),
        ),
        (
            r#"{"data":[{"embedding":[1e100,2]}]}"#,
            Some(ProviderError::InvalidResponse),
        ),
        (
            r#"{"data":[{"embedding":[1]}]}"#,
            Some(ProviderError::InvalidResponse),
        ),
        (r#"{"data":[]}"#, Some(ProviderError::InvalidResponse)),
        (
            r#"{"data":[{"embedding":[1,2]},{"embedding":[1,2]}]}"#,
            Some(ProviderError::InvalidResponse),
        ),
        (
            r#"{"data":[{"embedding":[1,2],"extra":true}]}"#,
            Some(ProviderError::InvalidResponse),
        ),
        (
            r#"{"data":[{"embedding":[1,2]}],"extra":true}"#,
            Some(ProviderError::InvalidResponse),
        ),
    ] {
        let server = Loopback::one(body);
        let result = bounded(registry(&config(&server.endpoint, "embedding")).embed(
            &key(),
            TEXT,
            &HttpTransport,
        ))
        .await;
        if let Some(error) = expected {
            assert_eq!(result.unwrap_err(), error);
        } else {
            assert_eq!(result.unwrap(), vec![1.0, 2.0]);
        }
        assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let target = Loopback::one(r#"{"data":[{"embedding":[1,2]}]}"#);
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        target.endpoint
    );
    let source = Loopback::new(vec![(response, Duration::ZERO)]);
    let result = bounded(registry(&config(&source.endpoint, "embedding")).embed(
        &key(),
        TEXT,
        &HttpTransport,
    ))
    .await;
    assert_eq!(result.unwrap_err(), ProviderError::Unavailable);
    assert_eq!(target.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn response_cap_checks_declared_and_chunked_lengths() {
    let oversize = "x".repeat(RESPONSE_BYTES + 1);
    let prefix = r#"{"data":[{"embedding":[1,2]}]}"#;
    let valid_json_over_limit =
        format!("{prefix}{}", " ".repeat(RESPONSE_BYTES + 1 - prefix.len()));
    for response in [
        Loopback::json(200, &oversize),
        Loopback::json(200, &valid_json_over_limit),
        format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            oversize.len(),
            oversize
        ),
    ] {
        let server = Loopback::new(vec![(response, Duration::ZERO)]);
        let registry = registry(&config(&server.endpoint, "embedding"));
        let (provider, _) = registry.bound(&key(), Kind::Embedding).unwrap();
        let response = bounded(HttpTransport.send(TransportRequest {
            provider,
            body: b"{}",
            remaining: Duration::from_secs(10),
        }))
        .await;
        assert!(matches!(
            response,
            Err(TransportFailure {
                error: ProviderError::InvalidResponse,
                ..
            })
        ));
    }
}

#[tokio::test]
async fn response_cap_accepts_exact_limit_and_rejects_declared_oversize_without_body() {
    let prefix = r#"{"data":[{"embedding":[1,2]}]}"#;
    let body = format!("{prefix}{}", " ".repeat(RESPONSE_BYTES - prefix.len()));
    let server = Loopback::one(&body);
    assert_eq!(
        bounded(registry(&config(&server.endpoint, "embedding")).embed(
            &key(),
            TEXT,
            &HttpTransport
        ))
        .await
        .unwrap(),
        vec![1.0, 2.0]
    );

    for (status, expected) in [
        (500, ProviderError::Unavailable),
        (200, ProviderError::InvalidResponse),
    ] {
        let response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            RESPONSE_BYTES + 1
        );
        let server = Loopback::new(vec![(response, Duration::ZERO)]);
        assert_eq!(
            bounded(registry(&config(&server.endpoint, "embedding")).embed(
                &key(),
                TEXT,
                &HttpTransport
            ))
            .await
            .unwrap_err(),
            expected
        );
    }
}

#[tokio::test]
async fn total_deadline_includes_slow_response_and_retry_after() {
    let server = Loopback::new(vec![(
        Loopback::json(200, r#"{"data":[{"embedding":[1,2]}]}"#),
        Duration::from_millis(500),
    )]);
    let mut value = config(&server.endpoint, "embedding");
    value["aliases"][0]["connect_timeout_ms"] = json!(100);
    value["aliases"][0]["total_timeout_ms"] = json!(200);
    let started = Instant::now();
    assert_eq!(
        bounded(registry(&value).embed(&key(), TEXT, &HttpTransport))
            .await
            .unwrap_err(),
        ProviderError::Timeout
    );
    assert!(started.elapsed() < Duration::from_millis(1200));
    let server = Loopback::new(vec![(
        "HTTP/1.1 503 Busy\r\nRetry-After: 20\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .into(),
        Duration::ZERO,
    )]);
    value["aliases"][0]["endpoint"] = json!(server.endpoint);
    value["aliases"][0]["replay_safe"] = json!(true);
    value["aliases"][0]["connect_timeout_ms"] = json!(2000);
    value["aliases"][0]["total_timeout_ms"] = json!(10000);
    let started = Instant::now();
    assert_eq!(
        bounded(registry(&value).embed(&key(), TEXT, &HttpTransport))
            .await
            .unwrap_err(),
        ProviderError::Timeout
    );
    assert!(started.elapsed() < Duration::from_secs(11));
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn replay_safe_retry_is_at_most_once_and_only_for_transients() {
    let server = Loopback::new(vec![
        (Loopback::json(503, SECRET), Duration::ZERO),
        (
            Loopback::json(200, r#"{"data":[{"embedding":[1,2]}]}"#),
            Duration::ZERO,
        ),
    ]);
    let mut value = config(&server.endpoint, "embedding");
    value["aliases"][0]["replay_safe"] = json!(true);
    assert_eq!(
        bounded(registry(&value).embed(&key(), TEXT, &HttpTransport))
            .await
            .unwrap(),
        vec![1.0, 2.0]
    );
    assert_eq!(server.calls.load(Ordering::SeqCst), 2);
    {
        let bodies = server.bodies.lock().unwrap();
        assert_eq!(bodies[0], bodies[1]);
        assert_eq!(bodies[0]["version"], "2026-01");
    }
    for safe in [false, true] {
        for status in [302, 401, 403, 408, 429, 500, 502, 503, 504] {
            let responses = vec![(Loopback::json(status, SECRET), Duration::ZERO); 2];
            let server = Loopback::new(responses);
            let mut value = config(&server.endpoint, "embedding");
            value["aliases"][0]["replay_safe"] = json!(safe);
            let error = bounded(registry(&value).embed(&key(), TEXT, &HttpTransport))
                .await
                .unwrap_err();
            assert_eq!(
                error,
                ProviderError::Unavailable,
                "replay_safe={safe}, HTTP {status}"
            );
            assert!(!error.to_string().contains(SECRET));
            assert_eq!(
                server.calls.load(Ordering::SeqCst),
                if safe && matches!(status, 429 | 503) {
                    2
                } else {
                    1
                }
            );
        }
        for error in [
            ProviderError::Unavailable,
            ProviderError::InvalidResponse,
            ProviderError::Timeout,
        ] {
            let mut value = config("http://127.0.0.1:1234/inference", "embedding");
            value["aliases"][0]["replay_safe"] = json!(safe);
            let mut fake = Fake::new(200, "");
            fake.error = Some(error);
            assert_eq!(
                registry(&value)
                    .embed(&key(), TEXT, &fake)
                    .await
                    .unwrap_err(),
                error
            );
            assert_eq!(
                fake.calls.load(Ordering::SeqCst),
                if safe && error == ProviderError::Unavailable {
                    2
                } else {
                    1
                }
            );
        }
    }
}

#[tokio::test]
async fn retry_after_delta_seconds_is_honored_and_invalid_values_do_not_retry() {
    let server = Loopback::new(vec![
        (
            "HTTP/1.1 503 Busy\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into(),
            Duration::ZERO,
        ),
        (
            Loopback::json(200, r#"{"data":[{"embedding":[1,2]}]}"#),
            Duration::ZERO,
        ),
    ]);
    let mut value = config(&server.endpoint, "embedding");
    value["aliases"][0]["replay_safe"] = json!(true);
    let started = Instant::now();
    assert_eq!(
        bounded(registry(&value).embed(&key(), TEXT, &HttpTransport))
            .await
            .unwrap(),
        vec![1.0, 2.0]
    );
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(server.calls.load(Ordering::SeqCst), 2);

    for retry_after in ["Sun, 06 Nov 1994 08:49:37 GMT", "-1", "1.5", "20"] {
        let started = Instant::now();
        let response = format!(
            "HTTP/1.1 503 Busy\r\nRetry-After: {retry_after}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let server = Loopback::new(vec![
            (response, Duration::ZERO),
            (
                Loopback::json(200, r#"{"data":[{"embedding":[1,2]}]}"#),
                Duration::ZERO,
            ),
        ]);
        let mut value = config(&server.endpoint, "embedding");
        value["aliases"][0]["replay_safe"] = json!(true);
        assert_eq!(
            bounded(registry(&value).embed(&key(), TEXT, &HttpTransport))
                .await
                .unwrap_err(),
            ProviderError::Timeout
        );
        assert!(started.elapsed() < Duration::from_secs(11));
        assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn refused_connect_is_retried_only_for_replay_safe_aliases() {
    let reservation = tokio::net::TcpSocket::new_v4().unwrap();
    reservation.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let socket = reservation.local_addr().unwrap();
    let endpoint_port = if cfg!(windows) { 0 } else { socket.port() };
    for replay_safe in [true, false] {
        let mut configured = hostname_config(
            &format!("http://pinned.invalid:{endpoint_port}/inference"),
            "pinned.invalid",
            socket.ip(),
        );
        configured
            .providers
            .get_mut("test")
            .unwrap()
            .definition
            .replay_safe = replay_safe;
        let resolver = Arc::new(QueueResolver::new([vec![socket], vec![socket]]));
        configured.providers.get_mut("test").unwrap().resolver = resolver.clone();
        let error = bounded(configured.embed(&key(), TEXT, &HttpTransport))
            .await
            .unwrap_err();
        let expected_attempts = match error {
            ProviderError::Unavailable if replay_safe => 2,
            ProviderError::Unavailable | ProviderError::Timeout => 1,
            other => panic!("unexpected connect error: {other:?}"),
        };
        assert_eq!(
            resolver.calls.load(Ordering::SeqCst),
            expected_attempts,
            "replay_safe={replay_safe}, error={error:?}"
        );
    }
}

#[tokio::test]
async fn retryable_transport_failure_is_retried_only_for_replay_safe_aliases() {
    for replay_safe in [true, false] {
        for expected_error in [ProviderError::Unavailable, ProviderError::Timeout] {
            let mut value = config("http://127.0.0.1:1234/inference", "embedding");
            value["aliases"][0]["replay_safe"] = json!(replay_safe);
            let configured = registry(&value);
            let mut transport = Fake::new(200, "");
            transport.error = Some(expected_error);
            assert_eq!(
                bounded(configured.embed(&key(), TEXT, &transport))
                    .await
                    .unwrap_err(),
                expected_error
            );
            let expected_attempts = if replay_safe && expected_error == ProviderError::Unavailable {
                2
            } else {
                1
            };
            assert_eq!(
                transport.calls.load(Ordering::SeqCst),
                expected_attempts,
                "replay_safe={replay_safe}, error={expected_error:?}"
            );
        }
    }
}

fn candidate(id: u64) -> Candidate {
    Candidate {
        id,
        vector: vec![VECTOR, 1.0],
        payload: BTreeMap::from([
            ("allowed".into(), json!("approved")),
            ("private".into(), json!(SECRET)),
        ]),
    }
}

#[tokio::test]
async fn rerank_validates_ids_costs_cardinality_and_payload_allowlist() {
    for (body, valid) in [
        (
            r#"{"results":[{"id":2,"cost":0.1},{"id":1,"cost":0.1}]}"#,
            true,
        ),
        (
            r#"{"results":[{"id":1,"cost":NaN},{"id":2,"cost":0.2}]}"#,
            false,
        ),
        (
            r#"{"results":[{"id":1,"cost":Infinity},{"id":2,"cost":0.2}]}"#,
            false,
        ),
        (
            r#"{"results":[{"id":1,"cost":1e100},{"id":2,"cost":0.2}]}"#,
            false,
        ),
        (
            r#"{"results":[{"id":1,"cost":0.1},{"id":1,"cost":0.2}]}"#,
            false,
        ),
        (
            r#"{"results":[{"id":1,"cost":0.1},{"id":2,"cost":0.2}],"extra":true}"#,
            false,
        ),
        (
            r#"{"results":[{"id":999,"cost":0.1},{"id":2,"cost":0.2}]}"#,
            false,
        ),
        (r#"{"results":[{"id":1,"cost":0.1}]}"#, false),
        (r#"{"results":[]}"#, false),
    ] {
        let server = Loopback::one(body);
        let mut value = config(&server.endpoint, "rerank");
        value["bindings"][0]["payload_fields"] = json!(["allowed"]);
        let result = bounded(registry(&value).rerank(
            &key(),
            &[VECTOR, 1.0],
            &[candidate(1), candidate(2)],
            2,
            &HttpTransport,
        ))
        .await;
        if valid {
            assert_eq!(
                result.unwrap().iter().map(|r| r.id).collect::<Vec<_>>(),
                vec![1, 2]
            );
            let bodies = server.bodies.lock().unwrap();
            let request_keys: BTreeSet<_> = bodies[0]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                request_keys,
                BTreeSet::from(["candidates", "k", "model", "query", "version"])
            );
            let candidate_keys: BTreeSet<_> = bodies[0]["candidates"][0]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(candidate_keys, BTreeSet::from(["id", "payload", "vector"]));
        } else {
            assert_eq!(result.unwrap_err(), ProviderError::InvalidResponse);
        }
        let bodies = server.bodies.lock().unwrap();
        assert_eq!(
            bodies[0]["candidates"][0]["payload"],
            json!({"allowed":"approved"})
        );
        assert!(!bodies[0].to_string().contains(SECRET));
    }
}

#[tokio::test]
async fn request_byte_text_candidate_and_vector_limits_precede_transport() {
    let fake = Fake::new(200, r#"{"data":[{"embedding":[1,2]}]}"#);
    let embedding = registry(&config("http://127.0.0.1:1234/inference", "embedding"));
    assert!(
        embedding
            .embed(&key(), &"a".repeat(16384), &fake)
            .await
            .is_ok()
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    for input in ["a".repeat(16385), "é".repeat(8193)] {
        assert_eq!(
            embedding.embed(&key(), &input, &fake).await.unwrap_err(),
            ProviderError::InvalidInput
        );
    }
    let mut value = config("http://127.0.0.1:1234/inference", "rerank");
    value["bindings"][0]["payload_fields"] = json!(["allowed"]);
    let rerank = registry(&value);
    let fake = Fake::new(200, r#"{"results":[{"id":1,"cost":0.1}]}"#);
    let mut huge = candidate(1);
    huge.payload
        .insert("allowed".into(), json!("x".repeat(REQUEST_BYTES)));
    assert_eq!(
        rerank
            .rerank(&key(), &[VECTOR, 1.0], &[huge], 1, &fake)
            .await
            .unwrap_err(),
        ProviderError::InvalidInput
    );
    for (candidates, k, query) in [
        (vec![candidate(1)], 0, vec![VECTOR, 1.0]),
        (vec![candidate(1)], 2, vec![VECTOR, 1.0]),
        ((0..1001).map(candidate).collect(), 1, vec![VECTOR, 1.0]),
        (vec![candidate(1), candidate(1)], 1, vec![VECTOR, 1.0]),
        (vec![candidate(1)], 1, vec![f32::NAN, 1.0]),
        (vec![candidate(1)], 1, vec![VECTOR]),
        (
            {
                let mut item = candidate(1);
                item.vector[0] = f32::INFINITY;
                vec![item]
            },
            1,
            vec![VECTOR, 1.0],
        ),
    ] {
        assert_eq!(
            rerank
                .rerank(&key(), &query, &candidates, k, &fake)
                .await
                .unwrap_err(),
            ProviderError::InvalidInput
        );
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provider_and_tenant_admission_is_immediate_and_shared_across_collections() {
    let mut value = config("http://127.0.0.1:1234/inference", "embedding");
    let mut second = value["bindings"][0].clone();
    second["collection_id"] = json!(1);
    value["bindings"].as_array_mut().unwrap().push(second);
    let registry = registry(&value);
    let fake = Fake::new(200, r#"{"data":[{"embedding":[1,2]}]}"#);
    let provider = registry.providers.get("test").unwrap();
    let provider_permit = provider.slots.acquire_many(2).await.unwrap();
    assert_eq!(
        bounded(registry.embed(&key(), TEXT, &fake))
            .await
            .unwrap_err(),
        ProviderError::Busy
    );
    drop(provider_permit);
    let tenant_permit = registry
        .bindings
        .get(&key())
        .unwrap()
        .slots
        .acquire()
        .await
        .unwrap();
    assert_eq!(
        bounded(registry.embed(&(42, 1, "tenant-a".into(), "test".into()), TEXT, &fake))
            .await
            .unwrap_err(),
        ProviderError::Busy
    );
    drop(tenant_permit);
    for wrong in [
        (41, 0, "tenant-a".into(), "test".into()),
        (42, 1, "tenant-b".into(), "test".into()),
        (42, 2, "tenant-a".into(), "test".into()),
    ] {
        assert_eq!(
            registry.embed(&wrong, TEXT, &fake).await.unwrap_err(),
            ProviderError::NotConfigured
        );
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn address_allowlist_is_checked_before_connecting() {
    let server = Loopback::one(SECRET);
    let value = config(&server.endpoint, "embedding");
    let mut registry = registry(&value);
    registry
        .providers
        .get_mut("test")
        .unwrap()
        .definition
        .endpoint_allowlist
        .addresses = vec!["127.0.0.2".parse().unwrap()];
    assert_eq!(
        bounded(registry.embed(&key(), TEXT, &HttpTransport))
            .await
            .unwrap_err(),
        ProviderError::Unavailable
    );
    assert_eq!(server.calls.load(Ordering::SeqCst), 0);
}
