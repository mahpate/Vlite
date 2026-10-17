use std::{error::Error, net::SocketAddr, path::PathBuf};

use tokio::net::TcpListener;
use vlite::{Database, server::Service};

const USAGE: &str = "usage: vlite-server <database-path> [--create-if-missing <dimensions>] [--bind <ip:port>] [--allow-remote]";

struct Config {
    path: PathBuf,
    dimensions: Option<usize>,
    bind: SocketAddr,
}

fn config() -> Result<Config, Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().ok_or(USAGE)?;
    let mut dimensions = None;
    let mut bind: SocketAddr = "127.0.0.1:8080".parse()?;
    let mut allow_remote = false;
    let mut seen_bind = false;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--create-if-missing") if dimensions.is_none() => {
                let value = args.next().ok_or(USAGE)?;
                let count: usize = value.to_str().ok_or(USAGE)?.parse()?;
                if !(1..=vlite::MAX_DIMENSIONS).contains(&count) {
                    return Err(vlite::Error::InvalidDimensions(count).into());
                }
                dimensions = Some(count);
            }
            Some("--bind") if !seen_bind => {
                bind = args.next().ok_or(USAGE)?.to_str().ok_or(USAGE)?.parse()?;
                seen_bind = true;
            }
            Some("--allow-remote") if !allow_remote => allow_remote = true,
            _ => return Err(USAGE.into()),
        }
    }
    if !bind.ip().is_loopback() && !allow_remote {
        return Err(
            "non-loopback bind requires --allow-remote; this service has no authentication or TLS"
                .into(),
        );
    }
    Ok(Config {
        path: path.into(),
        dimensions,
        bind,
    })
}

fn open(config: &Config) -> vlite::Result<Database> {
    let db = match Database::open(&config.path) {
        Ok(db) => db,
        Err(vlite::Error::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound && config.dimensions.is_some() =>
        {
            Database::create(&config.path, config.dimensions.expect("guarded dimensions"))?
        }
        Err(error) => return Err(error),
    };
    if let Some(expected) = config.dimensions
        && db.dimensions() != expected
    {
        return Err(vlite::Error::DimensionMismatch {
            expected,
            actual: db.dimensions(),
        });
    }
    Ok(db)
}

async fn run() -> Result<(), Box<dyn Error>> {
    let config = config()?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let listener = TcpListener::bind(config.bind).await?;
    let address = listener.local_addr()?;
    let service = Service::new(open(&config)?);
    let (signal_tx, signal_rx) = tokio::sync::oneshot::channel();
    let shutdown = async move {
        #[cfg(unix)]
        let result = tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        };
        #[cfg(not(unix))]
        let result = tokio::signal::ctrl_c().await;
        let _ = signal_tx.send(result);
    };
    println!("LISTENING http://{address}");
    let serving = axum::serve(listener, vlite::server::router(service.clone()))
        .with_graceful_shutdown(shutdown)
        .await;
    let closing = service.close().await;
    serving?;
    closing?;
    signal_rx.await??;
    Ok(())
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("FAIL: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
