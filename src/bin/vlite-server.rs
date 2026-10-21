use std::{error::Error, net::SocketAddr, path::PathBuf, time::Duration};

use tokio::net::TcpListener;
use vlite::{
    Database, FormatVersion,
    server::{
        BearerToken, ServerOptions, Service, configured_admin_router, read_token_file,
        selected_database,
    },
};

const USAGE: &str = "usage: vlite-server <database-path> [--create-if-missing <dimensions> [--create-format 3|4]] [--bind <ip:port>] [--allow-remote] [--token-file <path> | --insecure-local]; admin: VLITE_AUTH_TOKEN or VLITE_AUTH_TOKEN_FILE; optional read-only: VLITE_READ_ONLY_TOKEN or VLITE_READ_ONLY_TOKEN_FILE; backups/credentials/persistent audit: VLITE_ADMIN_DIR; off-by-default schedules: VLITE_AUTO_MAINTENANCE_INTERVAL_SECS, VLITE_BACKUP_INTERVAL_SECS, VLITE_BACKUP_RETENTION_ENABLED=1, VLITE_BACKUP_RETAIN_LAST, VLITE_BACKUP_RETAIN_DAYS, VLITE_BACKUP_DRILL_EVERY, VLITE_BACKUP_DRILL_MAX_BYTES; VLITE_SLOW_QUERY_MS";

struct Config {
    path: PathBuf,
    dimensions: Option<usize>,
    format: FormatVersion,
    bind: SocketAddr,
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
    if insecure_local && (read_only_token.is_some() || read_only_file.is_some()) {
        return Err("read-only credentials require admin bearer authentication".into());
    }
    let config = Config {
        path: path.into(),
        dimensions,
        format: format.unwrap_or(FormatVersion::V3),
        bind,
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

async fn run() -> Result<(), Box<dyn Error>> {
    let config = config()?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let listener = TcpListener::bind(config.bind).await?;
    let address = listener.local_addr()?;
    let (database, active) = open(&config)?;
    let service = Service::try_new(database)?;
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
    let router = match configured_admin_router(service.clone(), options(&config, Some(active))) {
        Ok(router) => router,
        Err(error) => {
            // Never leave a scheduler or the database open on failed startup.
            let _ = service.close().await;
            return Err(error.into());
        }
    };
    println!("LISTENING http://{address}");
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
