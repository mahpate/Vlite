#![cfg(feature = "server")]
use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::{TlsConnector, rustls};
use tower::ServiceExt;
use vlite::{
    Database,
    server::{
        BearerToken, ServerOptions, Service, configured_admin_router,
        tls::{TlsIdentity, TlsListener},
    },
};
const TOKEN: &str = "admin_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
fn ca() -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
}
fn leaf(ca: &CertifiedIssuer<'_, KeyPair>) -> (String, String) {
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .signed_by(&key, ca)
        .unwrap();
    (cert.pem(), key.serialize_pem())
}
fn client(ca: &str) -> TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut std::io::Cursor::new(ca)) {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}
async fn get(address: std::net::SocketAddr, ca: &str) -> (Vec<u8>, String) {
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut stream = client(ca)
        .connect("localhost".try_into().unwrap(), stream)
        .await
        .unwrap();
    let cert = stream.get_ref().1.peer_certificates().unwrap()[0]
        .as_ref()
        .to_vec();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    (cert, String::from_utf8(bytes).unwrap())
}
async fn reload(app: &Router) -> u16 {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/admin/tls/reload")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let _ = response.into_body().collect().await.unwrap();
    status
}
#[tokio::test]
async fn rest_tls_reload_is_transactional_and_bad_handshakes_do_not_block_health() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let ca = ca();
    let (cert, key) = leaf(&ca);
    let cp = dir.path().join("cert.pem");
    let kp = dir.path().join("key.pem");
    std::fs::write(&cp, &cert).unwrap();
    std::fs::write(&kp, &key).unwrap();
    let tls = TlsIdentity::from_files(&cp, &kp, None).unwrap();
    let service = Service::new(Database::create(dir.path().join("db.vlite"), 2).unwrap());
    service.register_tls(tls.clone()).unwrap();
    let app = configured_admin_router(
        service.clone(),
        ServerOptions {
            admin_token: Some(BearerToken::new(TOKEN).unwrap()),
            ..ServerOptions::default()
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let routes = app.clone();
    let task = tokio::spawn(async move {
        axum::serve(TlsListener::new(listener, tls).unwrap(), routes)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    let silent = tokio::net::TcpStream::connect(address).await.unwrap();
    let (old, response) = tokio::time::timeout(Duration::from_secs(30), get(address, &ca.pem()))
        .await
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    let (new_cert, new_key) = leaf(&ca);
    std::fs::write(&cp, &new_cert).unwrap();
    std::fs::write(&kp, &new_key).unwrap();
    assert_eq!(reload(&app).await, 200);
    let (new, _) = get(address, &ca.pem()).await;
    assert_ne!(old, new);
    let secondary_cert = dir.path().join("secondary-cert.pem");
    let secondary_key = dir.path().join("secondary-key.pem");
    std::fs::write(&secondary_cert, &new_cert).unwrap();
    std::fs::write(&secondary_key, &new_key).unwrap();
    service
        .register_tls(TlsIdentity::from_files(&secondary_cert, &secondary_key, None).unwrap())
        .unwrap();
    let (candidate_cert, candidate_key) = leaf(&ca);
    std::fs::write(&cp, candidate_cert).unwrap();
    std::fs::write(&kp, candidate_key).unwrap();
    std::fs::write(&secondary_key, "invalid secondary key").unwrap();
    assert_eq!(reload(&app).await, 422);
    assert_eq!(get(address, &ca.pem()).await.0, new);
    std::fs::write(&kp, "bad PEM").unwrap();
    assert_eq!(reload(&app).await, 422);
    assert_eq!(get(address, &ca.pem()).await.0, new);
    drop(silent);
    tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .unwrap()
        .unwrap();
    service.close().await.unwrap();
}

#[test]
fn rest_tls_binary_validates_before_creating_database_and_prints_https() {
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
        sync::mpsc,
    };
    let dir = tempfile::tempdir_in(".").unwrap();
    let path = dir.path().join("db.vlite");
    let cp = dir.path().join("cert.pem");
    let kp = dir.path().join("key.pem");
    let ca = ca();
    let (cert, key) = leaf(&ca);
    std::fs::write(&cp, "invalid").unwrap();
    std::fs::write(&kp, &key).unwrap();
    let command = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vlite-server"));
        c.arg(&path)
            .args([
                "--create-if-missing",
                "2",
                "--bind",
                "127.0.0.1:0",
                "--insecure-local",
                "--tls-cert",
            ])
            .arg(&cp)
            .arg("--tls-key")
            .arg(&kp)
            .env_remove("VLITE_AUTH_TOKEN")
            .env_remove("VLITE_AUTH_TOKEN_FILE")
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT");
        c
    };
    assert!(!command().output().unwrap().status.success());
    assert!(!path.exists());
    std::fs::write(&cp, cert).unwrap();
    let mut child = command().stdout(Stdio::piped()).spawn().unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(out).read_line(&mut line).unwrap();
        let _ = tx.send(line);
    });
    let line = rx.recv_timeout(Duration::from_secs(120));
    if line.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("TLS binary startup timed out");
    }
    let line = line.unwrap();
    let address = line
        .trim()
        .strip_prefix("LISTENING https://")
        .unwrap()
        .parse()
        .unwrap();
    let result = tokio::runtime::Runtime::new().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(30), get(address, &ca.pem())).await
    });
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(result.unwrap().1.starts_with("HTTP/1.1 200"));
}

#[cfg(feature = "grpc")]
#[tokio::test]
async fn grpc_mtls_requires_trusted_client_and_reloads_ca_transactionally() {
    use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
    use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};
    async fn accepts(address: std::net::SocketAddr, tls: ClientTlsConfig) -> bool {
        let channel = Channel::from_shared(format!("https://{address}"))
            .unwrap()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(30))
            .tls_config(tls)
            .unwrap()
            .connect()
            .await;
        match channel {
            Ok(channel) => HealthClient::new(channel)
                .check(HealthCheckRequest {
                    service: String::new(),
                })
                .await
                .is_ok(),
            Err(_) => false,
        }
    }
    let dir = tempfile::tempdir_in(".").unwrap();
    let issuer = ca();
    let next_ca = ca();
    let (cert, key) = leaf(&issuer);
    let (cc, ck) = leaf(&issuer);
    let (next_cc, next_ck) = leaf(&next_ca);
    let cp = dir.path().join("cert.pem");
    let kp = dir.path().join("key.pem");
    let cap = dir.path().join("client-ca.pem");
    std::fs::write(&cp, cert).unwrap();
    std::fs::write(&kp, key).unwrap();
    std::fs::write(&cap, issuer.pem()).unwrap();
    let identity = TlsIdentity::from_files(&cp, &kp, Some(&cap)).unwrap();
    let service = Service::new(Database::create(dir.path().join("db.vlite"), 2).unwrap());
    service.register_tls(identity.clone()).unwrap();
    let app = configured_admin_router(
        service.clone(),
        ServerOptions {
            admin_token: Some(BearerToken::new(TOKEN).unwrap()),
            ..ServerOptions::default()
        },
    )
    .unwrap();
    let server = vlite::server::grpc::grpc_router(service.clone())
        .await
        .with_reloadable_tls(identity);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve_with_shutdown(listener, async {
        let _ = rx.await;
    }));
    let tls = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(issuer.pem()));
    assert!(!accepts(address, tls.clone()).await);
    let old_client = tls.clone().identity(Identity::from_pem(cc, ck));
    let new_client = tls.identity(Identity::from_pem(next_cc, next_ck));
    assert!(accepts(address, old_client.clone()).await);
    assert!(!accepts(address, new_client.clone()).await);
    std::fs::write(&cap, next_ca.pem()).unwrap();
    assert_eq!(reload(&app).await, 200);
    assert!(!accepts(address, old_client).await);
    assert!(accepts(address, new_client.clone()).await);
    std::fs::write(&cap, "bad CA PEM").unwrap();
    assert_eq!(reload(&app).await, 422);
    assert!(accepts(address, new_client).await);
    tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    service.close().await.unwrap();
}
