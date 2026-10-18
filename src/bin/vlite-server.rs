use std::{error::Error, io::Read, net::SocketAddr, path::PathBuf};

use tokio::net::TcpListener;
use vlite::{
    Database,
    server::{BearerToken, Service},
};

const USAGE: &str = "usage: vlite-server <database-path> [--create-if-missing <dimensions>] [--bind <ip:port>] [--allow-remote] [--token-file <path> | --insecure-local]; token may also come from VLITE_AUTH_TOKEN or VLITE_AUTH_TOKEN_FILE";

struct Config {
    path: PathBuf,
    dimensions: Option<usize>,
    bind: SocketAddr,
    token: Option<BearerToken>,
}

fn config() -> Result<Config, Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().ok_or(USAGE)?;
    let mut dimensions = None;
    let mut bind: SocketAddr = "127.0.0.1:8080".parse()?;
    let mut allow_remote = false;
    let mut seen_bind = false;
    let mut token_file = std::env::var_os("VLITE_AUTH_TOKEN_FILE");
    let mut insecure_local = false;
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
            Some("--token-file") if token_file.is_none() => {
                token_file = Some(args.next().ok_or(USAGE)?);
            }
            Some("--insecure-local") if !insecure_local => insecure_local = true,
            _ => return Err(USAGE.into()),
        }
    }
    if !bind.ip().is_loopback() && !allow_remote {
        return Err(
            "non-loopback bind requires --allow-remote and bearer authentication; TLS must terminate at a proxy"
                .into(),
        );
    }
    let token_env = std::env::var_os("VLITE_AUTH_TOKEN");
    if usize::from(token_file.is_some())
        + usize::from(token_env.is_some())
        + usize::from(insecure_local)
        != 1
    {
        return Err(
            "select exactly one of token file, VLITE_AUTH_TOKEN, or --insecure-local".into(),
        );
    }
    if insecure_local && !bind.ip().is_loopback() {
        return Err("--insecure-local requires a loopback bind, even with --allow-remote".into());
    }
    let token = if insecure_local {
        None
    } else {
        let text = if let Some(path) = token_file {
            let mut text = String::new();
            std::fs::File::open(path)?
                .take(259)
                .read_to_string(&mut text)?;
            text.strip_suffix("\r\n")
                .or_else(|| text.strip_suffix('\n'))
                .unwrap_or(&text)
                .to_owned()
        } else {
            token_env
                .expect("validated token source")
                .into_string()
                .map_err(|_| "token must be valid ASCII")?
        };
        Some(BearerToken::new(&text)?)
    };
    Ok(Config {
        path: path.into(),
        dimensions,
        bind,
        token,
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
    let router = match config.token {
        Some(token) => vlite::server::authenticated_router(service.clone(), token),
        None => vlite::server::router(service.clone()),
    };
    let serving = axum::serve(listener, router)
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
            eprintln!(
                "{}",
                serde_json::json!({"event":"startup_or_shutdown_failure","message":error.to_string()})
            );
            std::process::ExitCode::FAILURE
        }
    }
}
