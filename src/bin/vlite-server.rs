use std::{
    error::Error,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde_json::json;
use tokio::net::TcpListener;
use vlite::{
    AnnSnapshotLoad, Database, FormatVersion,
    server::{
        BearerToken, ServerOptions, Service, configured_admin_router, read_token_file,
        selected_database,
    },
};

const USAGE: &str = "usage: vlite-server <database-path> [--create-if-missing <dimensions> [--create-format 3|4]] [--bind <ip:port>] [--grpc-bind <ip:port> [--grpc-tls-cert <pem> --grpc-tls-key <pem>]] [--allow-remote] [--ann-snapshot] [--token-file <path> | --insecure-local]; admin: VLITE_AUTH_TOKEN or VLITE_AUTH_TOKEN_FILE; optional read-only: VLITE_READ_ONLY_TOKEN or VLITE_READ_ONLY_TOKEN_FILE; backups/credentials/persistent audit: VLITE_ADMIN_DIR; off-by-default schedules: VLITE_AUTO_MAINTENANCE_INTERVAL_SECS, VLITE_BACKUP_INTERVAL_SECS, VLITE_BACKUP_RETENTION_ENABLED=1, VLITE_BACKUP_RETAIN_LAST, VLITE_BACKUP_RETAIN_DAYS, VLITE_BACKUP_DRILL_EVERY, VLITE_BACKUP_DRILL_MAX_BYTES; VLITE_SLOW_QUERY_MS";

struct Config {
    path: PathBuf,
    dimensions: Option<usize>,
    format: FormatVersion,
    bind: SocketAddr,
    /// Optional second listener for the gRPC transport (feature `grpc`).
    grpc_bind: Option<SocketAddr>,
    /// Native TLS identity for the gRPC listener.
    #[cfg(feature = "grpc")]
    grpc_tls: Option<vlite::server::grpc::GrpcTls>,
    /// Load `<active>.ann` after open; save it after both listeners drain.
    ann_snapshot: bool,
    token: Option<BearerToken>,
    token_file: Option<PathBuf>,
    read_only_token: Option<BearerToken>,
    read_only_file: Option<PathBuf>,
    admin_directory: Option<PathBuf>,
    schedule: Schedule,
}

/// Off-by-default operational settings from the environment.
#[derive(Default)]
struct Schedule {
    auto_maintenance_interval: Option<Duration>,
    backup_interval: Option<Duration>,
    backup_retention_enabled: bool,
    backup_retain_last: Option<usize>,
    backup_retain_days: Option<u64>,
    backup_drill_every: Option<u64>,
    backup_drill_max_bytes: Option<u64>,
    slow_query_threshold: Option<Duration>,
}

fn env_number<T: std::str::FromStr>(name: &str) -> Result<Option<T>, Box<dyn Error>> {
    match std::env::var_os(name) {
        None => Ok(None),
        Some(value) => value
            .to_str()
            .and_then(|text| text.parse().ok())
            .map(Some)
            .ok_or_else(|| format!("{name} must be a non-negative integer").into()),
    }
}

fn schedule() -> Result<Schedule, Box<dyn Error>> {
    let retention = match std::env::var("VLITE_BACKUP_RETENTION_ENABLED").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("0" | "false") => false,
        Ok("1" | "true") => true,
        _ => return Err("VLITE_BACKUP_RETENTION_ENABLED must be 0, 1, true or false".into()),
    };
    Ok(Schedule {
        auto_maintenance_interval: env_number("VLITE_AUTO_MAINTENANCE_INTERVAL_SECS")?
            .map(Duration::from_secs),
        backup_interval: env_number("VLITE_BACKUP_INTERVAL_SECS")?.map(Duration::from_secs),
        backup_retention_enabled: retention,
        backup_retain_last: env_number("VLITE_BACKUP_RETAIN_LAST")?,
        backup_retain_days: env_number("VLITE_BACKUP_RETAIN_DAYS")?,
        backup_drill_every: env_number("VLITE_BACKUP_DRILL_EVERY")?,
        backup_drill_max_bytes: env_number("VLITE_BACKUP_DRILL_MAX_BYTES")?,
        slow_query_threshold: env_number("VLITE_SLOW_QUERY_MS")?.map(Duration::from_millis),
    })
}

fn options(config: &Config, database_path: Option<PathBuf>) -> ServerOptions {
    let s = &config.schedule;
    ServerOptions {
        admin_token: config.token.clone(),
        read_only_token: config.read_only_token.clone(),
        admin_token_file: config.token_file.clone(),
        read_only_token_file: config.read_only_file.clone(),
        admin_directory: config.admin_directory.clone(),
        auto_maintenance_interval: s.auto_maintenance_interval,
        backup_interval: s.backup_interval,
        backup_retention_enabled: s.backup_retention_enabled,
        backup_retain_last: s.backup_retain_last,
        backup_retain_days: s.backup_retain_days,
        backup_drill_every: s.backup_drill_every,
        backup_drill_max_bytes: s.backup_drill_max_bytes,
        slow_query_threshold: s.slow_query_threshold,
        database_path,
    }
}

fn config() -> Result<Config, Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().ok_or(USAGE)?;
    let mut dimensions = None;
    let mut format = None;
    let mut bind: SocketAddr = "127.0.0.1:8080".parse()?;
    let mut allow_remote = false;
    let mut seen_bind = false;
    let mut token_file = std::env::var_os("VLITE_AUTH_TOKEN_FILE");
    let mut insecure_local = false;
    let mut grpc_bind: Option<SocketAddr> = None;
    let mut ann_snapshot = false;
    let mut grpc_tls_cert: Option<PathBuf> = None;
    let mut grpc_tls_key: Option<PathBuf> = None;
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
            Some("--create-format") if format.is_none() => {
                format = Some(match args.next().ok_or(USAGE)?.to_str() {
                    Some("3") => FormatVersion::V3,
                    Some("4") => FormatVersion::V4,
                    _ => return Err("--create-format must be 3 or 4".into()),
                });
            }
            Some("--bind") if !seen_bind => {
                bind = args.next().ok_or(USAGE)?.to_str().ok_or(USAGE)?.parse()?;
                seen_bind = true;
            }
            Some("--grpc-bind") if grpc_bind.is_none() => {
                grpc_bind = Some(args.next().ok_or(USAGE)?.to_str().ok_or(USAGE)?.parse()?);
            }
            Some("--grpc-tls-cert") if grpc_tls_cert.is_none() => {
                grpc_tls_cert = Some(args.next().ok_or(USAGE)?.into());
            }
            Some("--grpc-tls-key") if grpc_tls_key.is_none() => {
                grpc_tls_key = Some(args.next().ok_or(USAGE)?.into());
            }
            Some("--ann-snapshot") if !ann_snapshot => ann_snapshot = true,
            Some("--allow-remote") if !allow_remote => allow_remote = true,
            Some("--token-file") if token_file.is_none() => {
                token_file = Some(args.next().ok_or(USAGE)?);
            }
            Some("--insecure-local") if !insecure_local => insecure_local = true,
            _ => return Err(USAGE.into()),
        }
    }
    if format.is_some() && dimensions.is_none() {
        return Err("--create-format requires --create-if-missing".into());
    }
    if grpc_bind.is_some() && !cfg!(feature = "grpc") {
        return Err("--grpc-bind requires a build with the grpc feature".into());
    }
    if grpc_tls_cert.is_some() != grpc_tls_key.is_some() {
        return Err("--grpc-tls-cert and --grpc-tls-key must be given together".into());
    }
    if grpc_tls_cert.is_some() && grpc_bind.is_none() {
        return Err("--grpc-tls-cert requires --grpc-bind".into());
    }
    if let Some(grpc) = grpc_bind {
        // Port 0 asks for two distinct ephemeral ports; anything else must differ.
        if grpc == bind && grpc.port() != 0 {
            return Err("--grpc-bind must differ from --bind".into());
        }
        if !grpc.ip().is_loopback() && !allow_remote {
            return Err(
                "non-loopback --grpc-bind requires --allow-remote and bearer authentication; use --grpc-tls-cert/--grpc-tls-key or terminate TLS at a proxy"
                    .into(),
            );
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
    if insecure_local && grpc_bind.is_some_and(|grpc| !grpc.ip().is_loopback()) {
        return Err(
            "--insecure-local requires a loopback --grpc-bind, even with --allow-remote".into(),
        );
    }
    if insecure_local && !bind.ip().is_loopback() {
        return Err("--insecure-local requires a loopback bind, even with --allow-remote".into());
    }
    let token = if insecure_local {
        None
    } else if let Some(ref path) = token_file {
        read_token_file(std::path::Path::new(path), false)?
    } else {
        let text = token_env
            .expect("validated token source")
            .into_string()
            .map_err(|_| "token must be valid ASCII")?;
        Some(BearerToken::new(&text)?)
    };
    let read_only_file = std::env::var_os("VLITE_READ_ONLY_TOKEN_FILE").map(PathBuf::from);
    let read_only_env = std::env::var_os("VLITE_READ_ONLY_TOKEN");
    if read_only_file.is_some() && read_only_env.is_some() {
        return Err("select one read-only token source".into());
    }
    let read_only_token = if let Some(file) = &read_only_file {
        read_token_file(file, true)?
    } else {
        read_only_env
            .map(|value| {
                let text = value
                    .into_string()
                    .map_err(|_| "read-only token must be ASCII")?;
                BearerToken::new(&text)
            })
            .transpose()?
    };
    #[cfg(feature = "grpc")]
    let grpc_tls = match (&grpc_tls_cert, &grpc_tls_key) {
        (Some(cert), Some(key)) => {
            let read = |path: &PathBuf, what: &str| {
                std::fs::read(path)
                    .map_err(|error| format!("cannot read {what} {}: {error}", path.display()))
            };
            Some(
                vlite::server::grpc::GrpcTls::from_pem(
                    read(cert, "--grpc-tls-cert")?,
                    read(key, "--grpc-tls-key")?,
                )
                .map_err(|error| format!("invalid gRPC TLS identity: {error}"))?,
            )
        }
        _ => None,
    };
    #[cfg(not(feature = "grpc"))]
    let _ = (grpc_tls_cert, grpc_tls_key);
    if insecure_local && (read_only_token.is_some() || read_only_file.is_some()) {
        return Err("read-only credentials require admin bearer authentication".into());
    }
    let config = Config {
        path: path.into(),
        dimensions,
        format: format.unwrap_or(FormatVersion::V3),
        bind,
        grpc_bind,
        #[cfg(feature = "grpc")]
        grpc_tls,
        ann_snapshot,
        token,
        token_file: token_file.map(PathBuf::from),
        read_only_token,
        read_only_file,
        admin_directory: std::env::var_os("VLITE_ADMIN_DIR").map(PathBuf::from),
        schedule: schedule()?,
    };
    // Validate every option before creating or opening a database.
    options(&config, None).validate()?;
    Ok(config)
}

fn open(config: &Config) -> vlite::Result<(Database, PathBuf)> {
    let selected = config
        .admin_directory
        .as_deref()
        .map(selected_database)
        .transpose()?
        .flatten();
    let path = selected.as_deref().unwrap_or(&config.path);
    let db = match Database::open(path) {
        Ok(db) => db,
        Err(vlite::Error::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound
                && config.dimensions.is_some()
                && selected.is_none() =>
        {
            Database::create_with_format(
                &config.path,
                config.dimensions.expect("guarded dimensions"),
                config.format,
            )?
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
    Ok((db, path.to_path_buf()))
}

/// Graceful-stop signals, registered before any listener binds: Ctrl-C
/// everywhere, SIGTERM on Unix (container stop) and Ctrl-Break on Windows
/// (the console event a supervisor can send to one process group).
struct Signals {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_break: tokio::signal::windows::CtrlBreak,
}

impl Signals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(windows)]
            ctrl_break: tokio::signal::windows::ctrl_break()?,
        })
    }

    async fn recv(mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        let result = tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = self.terminate.recv() => Ok(()),
        };
        #[cfg(windows)]
        let result = tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = self.ctrl_break.recv() => Ok(()),
        };
        #[cfg(not(any(unix, windows)))]
        let result = tokio::signal::ctrl_c().await;
        result
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Loads `<active>.ann`. Every outcome is one stderr JSON line; no outcome
/// fails startup (the sidecar is a disposable cache).
fn load_ann_snapshot(database: &mut Database, path: &Path) {
    let started = Instant::now();
    let line = match database.load_ann_snapshot(path) {
        Ok(outcome) => {
            let (indexes, reason) = match outcome {
                AnnSnapshotLoad::Loaded { indexes } => (indexes, None),
                AnnSnapshotLoad::Rejected(rejection) => (0, Some(rejection.to_string())),
                _ => (0, None),
            };
            json!({"event":"ann_snapshot_load","outcome":outcome.code(),"reason":reason,
                "indexes":indexes,"duration_ms":elapsed_ms(started)})
        }
        Err(error) => json!({"event":"ann_snapshot_load","outcome":"error",
            "reason":error.to_string(),"indexes":0,"duration_ms":elapsed_ms(started)}),
    };
    eprintln!("{line}");
}

/// Saves after both listeners drained. A failed save is logged and never
/// blocks `close` or changes the exit code (the prior sidecar stays intact).
async fn save_ann_snapshot(service: &Service, path: &Path) {
    let started = Instant::now();
    let line = match service.save_ann_snapshot(path).await {
        Ok(report) => json!({"event":"ann_snapshot_save","outcome":"saved","reason":null,
            "indexes":report.indexes,"bytes":report.bytes,"duration_ms":elapsed_ms(started)}),
        Err(error) => json!({"event":"ann_snapshot_save","outcome":"failed",
            "reason":error.to_string(),"indexes":0,"bytes":0,"duration_ms":elapsed_ms(started)}),
    };
    eprintln!("{line}");
}

#[cfg(feature = "grpc")]
type GrpcHealth = vlite::server::grpc::GrpcHealth;
/// Uninhabited: without the feature there is never a gRPC listener.
#[cfg(not(feature = "grpc"))]
#[derive(Clone)]
enum GrpcHealth {}

/// Resolves once the shared stop flag is raised, after reporting gRPC
/// health `NOT_SERVING`, so health flips before either listener drains.
async fn stopped(mut stop: tokio::sync::watch::Receiver<bool>, health: Option<GrpcHealth>) {
    let _ = stop.wait_for(|stop| *stop).await;
    #[cfg(feature = "grpc")]
    if let Some(health) = health {
        health.set_not_serving().await;
    }
    #[cfg(not(feature = "grpc"))]
    let _ = health;
}

async fn run() -> Result<(), Box<dyn Error>> {
    let config = config()?;
    let signals = Signals::new()?;
    // Bind both listeners before opening, so a port conflict never opens
    // (or creates) the database.
    let listener = TcpListener::bind(config.bind).await?;
    let address = listener.local_addr()?;
    let grpc_listener = match config.grpc_bind {
        Some(bind) => Some(TcpListener::bind(bind).await?),
        None => None,
    };
    let (mut database, active) = open(&config)?;
    let snapshot = config
        .ann_snapshot
        .then(|| Database::ann_snapshot_path(&active));
    if let Some(path) = &snapshot {
        load_ann_snapshot(&mut database, path);
    }
    let service = Service::try_new(database)?;
    let router = match configured_admin_router(service.clone(), options(&config, Some(active))) {
        Ok(router) => router,
        Err(error) => {
            // Never leave a scheduler or the database open on failed startup.
            let _ = service.close().await;
            return Err(error.into());
        }
    };
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let signal = tokio::spawn({
        let stop_tx = stop_tx.clone();
        async move {
            let result = signals.recv().await;
            let _ = stop_tx.send(true);
            result
        }
    });

    #[cfg(feature = "grpc")]
    let grpc = match grpc_listener {
        // Built after the REST router so it shares its tokens and audit log.
        Some(listener) => {
            let server = vlite::server::grpc::grpc_router(service.clone()).await;
            let server = match config.grpc_tls.clone() {
                Some(tls) => server.with_tls(tls),
                None => server,
            };
            Some((server, listener))
        }
        None => None,
    };
    #[cfg(not(feature = "grpc"))]
    let grpc: Option<((), TcpListener)> = {
        // `config` rejects --grpc-bind without the feature.
        debug_assert!(grpc_listener.is_none());
        None
    };
    #[cfg(feature = "grpc")]
    let health = grpc.as_ref().map(|(server, _)| server.health());
    #[cfg(not(feature = "grpc"))]
    let health: Option<GrpcHealth> = None;

    println!("LISTENING http://{address}");
    if let Some((_, listener)) = &grpc {
        #[cfg(feature = "grpc")]
        let scheme = if config.grpc_tls.is_some() {
            "grpcs"
        } else {
            "grpc"
        };
        #[cfg(not(feature = "grpc"))]
        let scheme = "grpc";
        println!("LISTENING {scheme}://{}", listener.local_addr()?);
    }
    let rest = {
        let stop_tx = stop_tx.clone();
        let shutdown = stopped(stop_rx.clone(), health.clone());
        async move {
            let result = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown)
                .await;
            // Either listener failing stops the other one.
            let _ = stop_tx.send(true);
            result.map_err(|error| -> Box<dyn Error> { error.into() })
        }
    };
    let grpc = async move {
        let Some((server, listener)) = grpc else {
            return Ok(());
        };
        #[cfg(feature = "grpc")]
        let result = server
            .serve_with_shutdown(listener, stopped(stop_rx, health))
            .await
            .map_err(|error| -> Box<dyn Error> { error.into() });
        #[cfg(not(feature = "grpc"))]
        let result: Result<(), Box<dyn Error>> = {
            let _ = (server, listener, stop_rx, health);
            Ok(())
        };
        let _ = stop_tx.send(true);
        result
    };
    let (rest, grpc) = tokio::join!(rest, grpc);
    if let Some(path) = &snapshot {
        save_ann_snapshot(&service, path).await;
    }
    let closing = service.close().await;
    rest?;
    grpc?;
    closing?;
    if signal.is_finished() {
        signal.await??;
    } else {
        signal.abort();
    }
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
