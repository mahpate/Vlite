//! Native TLS with transactional, cross-platform certificate reloads.
use super::*;
use std::{
    io,
    path::{Path as FilePath, PathBuf},
    sync::RwLock,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{TlsAcceptor, rustls};

#[derive(Clone)]
pub struct TlsIdentity {
    config: Arc<RwLock<Arc<rustls::ServerConfig>>>,
    source: Option<(PathBuf, PathBuf, Option<PathBuf>)>,
}

impl TlsIdentity {
    pub(super) fn same_config(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.config, &other.config)
    }

    /// Validates the certificate chain, key match and optional client CA.
    pub fn from_pem(cert: &[u8], key: &[u8], client_ca: Option<&[u8]>) -> io::Result<Self> {
        Ok(Self {
            config: Arc::new(RwLock::new(Arc::new(parse(cert, key, client_ca)?))),
            source: None,
        })
    }

    /// File-backed identities can be reloaded using the admin TLS endpoint.
    pub fn from_files(
        cert: &FilePath,
        key: &FilePath,
        client_ca: Option<&FilePath>,
    ) -> io::Result<Self> {
        let mut identity = Self::from_pem(
            &std::fs::read(cert)?,
            &std::fs::read(key)?,
            client_ca.map(std::fs::read).transpose()?.as_deref(),
        )?;
        identity.source = Some((cert.into(), key.into(), client_ca.map(Into::into)));
        Ok(identity)
    }

    fn candidate(&self) -> io::Result<Arc<rustls::ServerConfig>> {
        let (cert, key, ca) = self
            .source
            .as_ref()
            .ok_or_else(|| io::Error::other("TLS identity is not file-backed"))?;
        Ok(Arc::new(parse(
            &std::fs::read(cert)?,
            &std::fs::read(key)?,
            ca.as_ref().map(std::fs::read).transpose()?.as_deref(),
        )?))
    }

    fn acceptor(&self) -> io::Result<TlsAcceptor> {
        Ok(TlsAcceptor::from(
            self.config
                .read()
                .map_err(|e| io::Error::other(e.to_string()))?
                .clone(),
        ))
    }
}

fn parse(cert: &[u8], key: &[u8], ca: Option<&[u8]>) -> io::Result<rustls::ServerConfig> {
    let certificates =
        rustls_pemfile::certs(&mut io::Cursor::new(cert)).collect::<Result<Vec<_>, _>>()?;
    if certificates.is_empty() {
        return Err(io::Error::other("empty TLS certificate chain"));
    }
    let key = rustls_pemfile::private_key(&mut io::Cursor::new(key))?
        .ok_or_else(|| io::Error::other("missing PEM private key"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?;
    let builder = if let Some(ca) = ca {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut io::Cursor::new(ca)) {
            roots.add(cert?).map_err(io::Error::other)?;
        }
        let verifier =
            rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .map_err(io::Error::other)?;
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };
    let mut config = builder
        .with_single_cert(certificates, key)
        .map_err(io::Error::other)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// A bounded TLS handshake loop; failed handshakes never terminate the listener.
/// Handshakes run independently so a silent TCP peer cannot block other clients.
pub struct TlsListener {
    address: std::net::SocketAddr,
    receiver: tokio::sync::mpsc::Receiver<(TlsStream, std::net::SocketAddr)>,
    task: tokio::task::JoinHandle<()>,
}
impl TlsListener {
    pub fn new(listener: tokio::net::TcpListener, identity: TlsIdentity) -> io::Result<Self> {
        let address = listener.local_addr()?;
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let task = tokio::spawn(async move {
            let permits = Arc::new(Semaphore::new(64));
            loop {
                let Ok(permit) = permits.clone().acquire_owned().await else {
                    break;
                };
                let Ok((stream, peer)) = listener.accept().await else {
                    break;
                };
                let Ok(acceptor) = identity.acceptor() else {
                    break;
                };
                let sender = sender.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Ok(Ok(stream)) = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        acceptor.accept(stream),
                    )
                    .await
                    {
                        let _ = sender.send((TlsStream(stream), peer)).await;
                    }
                });
            }
        });
        Ok(Self {
            address,
            receiver,
            task,
        })
    }
    pub async fn next(&mut self) -> Option<io::Result<TlsStream>> {
        self.receiver.recv().await.map(|(stream, _)| Ok(stream))
    }
}
impl Drop for TlsListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl axum::serve::Listener for TlsListener {
    type Io = TlsStream;
    type Addr = std::net::SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        self.receiver.recv().await.expect("TLS accept loop stopped")
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.address)
    }
}

pub struct TlsStream(tokio_rustls::server::TlsStream<tokio::net::TcpStream>);
impl AsyncRead for TlsStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl AsyncWrite for TlsStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
#[cfg(feature = "grpc")]
impl tonic::transport::server::Connected for TlsStream {
    type ConnectInfo = ();
    fn connect_info(&self) {}
}

pub(super) fn routes() -> Router<Service> {
    Router::new().route("/v1/admin/tls/reload", post(reload))
}
async fn reload(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    let identities = service.tls.clone();
    tokio::task::spawn_blocking(move || {
        let identities = identities.lock().map_err(internal)?;
        if identities.is_empty() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "tls_disabled",
                "no file-backed TLS listeners configured",
            ));
        }
        // Validate every candidate before publishing any. Existing sessions keep
        // their original configuration; only new handshakes see the new identity.
        let candidates = identities
            .iter()
            .map(TlsIdentity::candidate)
            .collect::<io::Result<Vec<_>>>()
            .map_err(|error| {
                eprintln!("TLS reload failed: {error}");
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "tls_reload_failed",
                    "invalid TLS files; previous identities remain active",
                )
            })?;
        let mut guards = identities
            .iter()
            .map(|i| i.config.write().map_err(internal))
            .collect::<Result<Vec<_>, _>>()?;
        for (guard, candidate) in guards.iter_mut().zip(candidates) {
            **guard = candidate;
        }
        Ok(Json(json!({"reloaded": identities.len()})))
    })
    .await
    .map_err(internal)?
}
