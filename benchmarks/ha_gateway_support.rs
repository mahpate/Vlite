#![allow(dead_code)]
use reqwest::blocking::Client;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub type Result<T> = std::result::Result<T, String>;
pub const BODY_LIMIT: usize = 65_536;
pub const CONFIG_LIMIT: u64 = 65_536;
pub const PATH: &str = "/v1/collections/probe/vectors/1";

pub fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = bytes(path, CONFIG_LIMIT)?;
    serde_json::from_slice(&bytes).map_err(|_| "invalid_config".into())
}
pub fn bytes(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(limit + 1).read_to_end(&mut bytes))
        .map_err(|_| "file_read_failed")?;
    if bytes.len() as u64 > limit {
        return Err("file_too_large".into());
    }
    Ok(bytes)
}
pub fn token(path: &Path) -> Result<String> {
    let text = String::from_utf8(bytes(path, 1024)?).map_err(|_| "token_invalid")?;
    let text = text.trim();
    if text.is_empty() || text.chars().any(char::is_control) {
        return Err("token_invalid".into());
    }
    Ok(text.to_owned())
}
pub fn config_arg() -> Result<PathBuf> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    match args.as_slice() {
        [flag, path] if flag == "--config" => Ok(path.into()),
        _ => Err("usage: --config <json>".into()),
    }
}
pub fn bounded(name: &str, value: u64, min: u64, max: u64) -> Result<()> {
    if !(min..=max).contains(&value) {
        Err(format!("{name}_out_of_range"))
    } else {
        Ok(())
    }
}
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_millis() as u64
}
pub fn client(timeout_ms: u64) -> Result<Client> {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_millis(timeout_ms))
        .connect_timeout(Duration::from_millis(timeout_ms))
        .pool_max_idle_per_host(256)
        .build()
        .map_err(|_| "http_client_failed".into())
}
pub fn origin(text: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(text).map_err(|_| "origin_invalid")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("origin_invalid".into());
    }
    Ok(url)
}
pub fn label(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
pub fn summary(values: &[f64]) -> Value {
    if values.is_empty() {
        return json!({"count":0,"min":null,"p50":null,"p95":null,"p99":null,"max":null,
            "mean":null,"method":"nearest_rank"});
    }
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let rank = |p: f64| values[((p * values.len() as f64).ceil() as usize).max(1) - 1];
    json!({"count":values.len(),"min":values[0],"p50":rank(0.50),"p95":rank(0.95),
        "p99":rank(0.99),"max":values[values.len()-1],
        "mean":values.iter().sum::<f64>() / values.len() as f64,"method":"nearest_rank"})
}
pub fn provenance() -> Value {
    match (
        env::var("VLITE_RESOURCE_SOURCE_REVISION"),
        env::var("VLITE_RESOURCE_SOURCE_DIRTY_PATHS"),
    ) {
        (Ok(revision), Ok(dirty)) if revision.len() <= 128 && dirty.len() <= 16384 => {
            json!({"revision":revision,"dirty_paths":dirty.lines().collect::<Vec<_>>()})
        }
        _ => Value::Null,
    }
}
pub fn finish(result: Result<Value>) {
    match result {
        Ok(report) => println!("{report}"),
        Err(code) => {
            eprintln!("{}", json!({"error":{"code":code}}));
            std::process::exit(1);
        }
    }
}
pub struct Reply {
    pub status: u16,
    pub body: Value,
}
pub fn request(client: &Client, url: &str, header: &str, secret: &str) -> Result<Reply> {
    let mut request = client.get(url);
    if !header.is_empty() {
        request = request.header(header, secret);
    }
    let mut response = request.send().map_err(|_| "transport_error")?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(BODY_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "body_read_error")?;
    if bytes.len() > BODY_LIMIT {
        return Err("body_too_large".into());
    }
    let body = serde_json::from_slice(&bytes).map_err(|_| "body_invalid_json")?;
    Ok(Reply { status, body })
}

/// A child whose output is bounded on disk and never forwarded to evidence.
/// Timeout/error cleanup kills and reaps this exact process.
pub struct Job {
    child: Child,
    stdout: tempfile::NamedTempFile,
    stderr: tempfile::NamedTempFile,
    start: Instant,
    timeout: Duration,
}
impl Job {
    pub fn start(program: &Path, args: &[String], timeout_ms: u64) -> Result<Self> {
        let stdout = tempfile::NamedTempFile::new().map_err(|_| "command_output_failed")?;
        let stderr = tempfile::NamedTempFile::new().map_err(|_| "command_output_failed")?;
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(stdout.reopen().map_err(|_| "command_output_failed")?)
            .stderr(stderr.reopen().map_err(|_| "command_output_failed")?)
            .spawn()
            .map_err(|_| "command_spawn_failed")?;
        Ok(Self {
            child,
            stdout,
            stderr,
            start: Instant::now(),
            timeout: Duration::from_millis(timeout_ms),
        })
    }
    pub fn poll(&mut self) -> Result<Option<Value>> {
        if self
            .stdout
            .as_file()
            .metadata()
            .map_err(|_| "command_output_failed")?
            .len()
            > 4_194_304
            || self
                .stderr
                .as_file()
                .metadata()
                .map_err(|_| "command_output_failed")?
                .len()
                > 65_536
        {
            return Err("command_output_limit".into());
        }
        if let Some(status) = self.child.try_wait().map_err(|_| "command_wait_failed")? {
            if !status.success() {
                return Err("command_failed".into());
            }
            return read_large(self.stdout.path()).map(Some);
        }
        if self.start.elapsed() > self.timeout {
            return Err("command_timeout".into());
        }
        Ok(None)
    }
    pub fn wait(&mut self) -> Result<Value> {
        loop {
            if let Some(value) = self.poll()? {
                return Ok(value);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
fn read_large(path: &Path) -> Result<Value> {
    serde_json::from_slice(&bytes(path, 4_194_304)?).map_err(|_| "command_json_invalid".into())
}
impl Drop for Job {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Program {
    pub program: PathBuf,
    pub args: Vec<String>,
}
impl Program {
    pub fn validate(&self) -> Result<()> {
        if !self.program.is_absolute()
            || self.args.len() > 64
            || self.args.iter().any(|a| a.len() > 2048)
        {
            return Err("command_invalid".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn successful_only_nearest_rank_and_empty_are_explicit() {
        assert_eq!(summary(&[])["p99"], Value::Null);
        assert_eq!(summary(&[1., 2., 3., 4.])["p95"], 4.);
        assert_eq!(summary(&[1., 2., 3., 4.])["p50"], 2.);
    }
    #[test]
    fn origins_refuse_secret_and_extra_components() {
        for value in [
            "http://user:secret@localhost",
            "https://host/x",
            "https://host/?sig=x",
            "file:///x",
        ] {
            assert!(origin(value).is_err());
        }
        assert!(origin("http://127.0.0.1:8080").is_ok());
    }
}
