//! Azure Blob Storage REST client for cloud backup sets.
//!
//! Microsoft Entra bearer tokens come from the platform managed identity
//! endpoint (Container Apps / App Service `IDENTITY_ENDPOINT`, otherwise the
//! VM instance metadata service). The account shared key is accepted only for
//! a loopback emulator and is read from a file per process, never persisted.
//! Every call blocks: use it from `spawn_blocking` or a plain thread.
//!
//! Conditional writes (`If-None-Match: *`, `If-Match`) are never retried here;
//! an unconfirmed result is returned as `StoreError::Unknown` for the set
//! protocol to resolve. Reads, listings, block staging and deletes are retried.
use super::*;
use reqwest::blocking::{Client, Response};
use reqwest::{Method as Verb, Url};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API_VERSION: &str = "2021-08-06";
const RESOURCE: &str = "https://storage.azure.com/";
const IMDS: &str = "http://169.254.169.254/metadata/identity/oauth2/token";
const SINGLE_PUT_MAX: u64 = 4 * 1024 * 1024;
const BLOCK: u64 = 8 * 1024 * 1024;
const MAX_BLOCKS: u64 = 50_000;
const RANGE: u64 = 8 * 1024 * 1024;
const LIST_BODY_MAX: u64 = 32 * 1024 * 1024;
const TOKEN_BODY_MAX: u64 = 64 * 1024;
const MAX_LISTED: usize = 1_000_000;
const ATTEMPTS: u32 = 3;
const SHARED_KEY_FILE_MAX: u64 = 1024;

/// Where managed identity tokens come from.
pub(in crate::server) enum IdentitySource {
    /// Container Apps / App Service: `IDENTITY_ENDPOINT` + `IDENTITY_HEADER`.
    Platform { endpoint: String, header: String },
    /// Azure VM instance metadata service.
    Imds,
}

impl IdentitySource {
    pub(in crate::server) fn from_env() -> Self {
        match (
            std::env::var("IDENTITY_ENDPOINT"),
            std::env::var("IDENTITY_HEADER"),
        ) {
            (Ok(endpoint), Ok(header)) if !endpoint.is_empty() && !header.is_empty() => {
                Self::Platform { endpoint, header }
            }
            _ => Self::Imds,
        }
    }
}

struct Token {
    value: String,
    expires_at: u64,
}

enum Credential {
    SharedKey {
        account: String,
        key: Vec<u8>,
    },
    Managed {
        source: IdentitySource,
        client_id: Option<String>,
        http: Client,
        cache: Mutex<Option<Token>>,
    },
}

/// Reads a base64 account key file (emulator only).
pub(in crate::server) fn read_shared_key(path: &FilePath) -> Result<Vec<u8>, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read the shared key file: {e}"))?;
    if meta.len() > SHARED_KEY_FILE_MAX {
        return Err("the shared key file is too large".into());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read the shared key file: {e}"))?;
    match BASE64_STANDARD.decode(text.trim()) {
        Ok(key) if !key.is_empty() => Ok(key),
        _ => Err("the shared key file must hold a base64 account key".into()),
    }
}

/// Connects to the target with the platform managed identity, or with the
/// emulator shared key when the target asks for it.
pub(in crate::server) fn connect(
    target: &Target,
    shared_key: Option<Vec<u8>>,
) -> Result<Sets, ApiError> {
    connect_with(target, shared_key, IdentitySource::from_env())
}

pub(in crate::server) fn connect_with(
    target: &Target,
    shared_key: Option<Vec<u8>>,
    identity: IdentitySource,
) -> Result<Sets, ApiError> {
    target.validate().map_err(unprocessable)?;
    let base = target.endpoint_url().map_err(unprocessable)?;
    let loopback = Url::parse(&base).is_ok_and(|url| is_loopback(&url));
    let credential = match (target.auth, shared_key) {
        (AuthMode::EmulatorSharedKey, Some(key)) => Credential::SharedKey {
            account: target.account.clone(),
            key,
        },
        (AuthMode::EmulatorSharedKey, None) => {
            return Err(unprocessable(
                "emulator_shared_key needs a shared key file (VLITE_CLOUD_BACKUP_SHARED_KEY_FILE or --shared-key-file)",
            ));
        }
        (AuthMode::ManagedIdentity, Some(_)) => {
            return Err(unprocessable(
                "a shared key is accepted only with auth emulator_shared_key",
            ));
        }
        (AuthMode::ManagedIdentity, None) => Credential::Managed {
            source: identity,
            client_id: target.client_id.clone(),
            http: client(true)?,
            cache: Mutex::new(None),
        },
    };
    let credential = Arc::new(credential);
    let http = client(loopback)?;
    let container = |name: &str| -> Arc<dyn ObjectStore> {
        Arc::new(Container {
            http: http.clone(),
            base: format!("{base}/{name}"),
            credential: credential.clone(),
        })
    };
    let data = container(&target.container);
    let (meta, shared) = match &target.coordination_container {
        Some(name) => (container(name), false),
        None => (data.clone(), true),
    };
    Ok(Sets::new(meta, data, shared, target.prefix.clone()))
}

fn client(no_proxy: bool) -> Result<Client, ApiError> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none());
    if no_proxy {
        builder = builder.no_proxy();
    }
    builder.build().map_err(internal)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `x-ms-date` value, for example `Tue, 14 Nov 2023 22:13:20 GMT`.
fn rfc1123(secs: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = secs / 86_400;
    let (year, month, day) = compaction::civil(days as i64);
    let t = secs % 86_400;
    format!(
        "{}, {day:02} {} {year:04} {:02}:{:02}:{:02} GMT",
        DAYS[(days % 7) as usize],
        MONTHS[(month - 1) as usize],
        t / 3600,
        t / 60 % 60,
        t % 60
    )
}

/// SharedKey string-to-sign (Blob service, version 2009-09-19 and later).
fn string_to_sign(
    verb: &str,
    url: &Url,
    headers: &[(&str, String)],
    length: usize,
    account: &str,
) -> String {
    let h = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map_or("", |(_, v)| v.as_str())
    };
    let length = if length == 0 {
        String::new()
    } else {
        length.to_string()
    };
    let mut text = format!(
        "{verb}\n{}\n{}\n{length}\n{}\n{}\n\n{}\n{}\n{}\n{}\n{}\n",
        h("content-encoding"),
        h("content-language"),
        h("content-md5"),
        h("content-type"),
        h("if-modified-since"),
        h("if-match"),
        h("if-none-match"),
        h("if-unmodified-since"),
        h("range"),
    );
    let mut canonical: Vec<(String, &str)> = headers
        .iter()
        .filter(|(k, _)| k.to_ascii_lowercase().starts_with("x-ms-"))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim()))
        .collect();
    canonical.sort();
    for (name, value) in canonical {
        text.push_str(&name);
        text.push(':');
        text.push_str(value);
        text.push('\n');
    }
    text.push('/');
    text.push_str(account);
    text.push_str(url.path());
    let mut query: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in url.query_pairs() {
        query
            .entry(name.to_lowercase())
            .or_default()
            .push(value.into_owned());
    }
    for (name, mut values) in query {
        values.sort();
        text.push('\n');
        text.push_str(&name);
        text.push(':');
        text.push_str(&values.join(","));
    }
    text
}

fn parse_token(body: &[u8], now: u64) -> Result<Token, StoreError> {
    let invalid = || StoreError::Failed("invalid managed identity token response".into());
    let value: Value = serde_json::from_slice(body).map_err(|_| invalid())?;
    let token = value["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(invalid)?
        .to_owned();
    let number = |v: &Value| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok());
    let expires_at = number(&value["expires_on"])
        .or_else(|| number(&value["expires_in"]).map(|n| now.saturating_add(n)))
        .unwrap_or(now + 300);
    Ok(Token {
        value: token,
        expires_at,
    })
}

impl Credential {
    fn authorization(
        &self,
        verb: &str,
        url: &Url,
        headers: &[(&str, String)],
        length: usize,
    ) -> Result<String, StoreError> {
        match self {
            Credential::SharedKey { account, key } => {
                let text = string_to_sign(verb, url, headers, length, account);
                let signature = webhooks::hmac_sha256_bytes(key, text.as_bytes());
                Ok(format!(
                    "SharedKey {account}:{}",
                    BASE64_STANDARD.encode(signature)
                ))
            }
            Credential::Managed {
                source,
                client_id,
                http,
                cache,
            } => {
                let now = now_secs();
                let mut cache = cache
                    .lock()
                    .map_err(|_| StoreError::Failed("token cache unavailable".into()))?;
                if let Some(token) = cache.as_ref()
                    && token.expires_at > now + 300
                {
                    return Ok(format!("Bearer {}", token.value));
                }
                let (endpoint, version) = match source {
                    IdentitySource::Platform { endpoint, .. } => (endpoint.as_str(), "2019-08-01"),
                    IdentitySource::Imds => (IMDS, "2018-02-01"),
                };
                let mut url = Url::parse(endpoint).map_err(|_| {
                    StoreError::Failed("the managed identity endpoint is invalid".into())
                })?;
                {
                    let mut query = url.query_pairs_mut();
                    query.append_pair("api-version", version);
                    query.append_pair("resource", RESOURCE);
                    if let Some(id) = client_id {
                        query.append_pair("client_id", id);
                    }
                }
                let request = match source {
                    IdentitySource::Platform { header, .. } => {
                        http.get(url).header("X-IDENTITY-HEADER", header)
                    }
                    IdentitySource::Imds => http.get(url).header("Metadata", "true"),
                };
                let response = request.send().map_err(transport)?;
                let status = response.status().as_u16();
                let body = read_limited(response, TOKEN_BODY_MAX)?;
                if !(200..300).contains(&status) {
                    let detail = format!("managed identity token request returned HTTP {status}");
                    return Err(if status == 429 || status >= 500 {
                        StoreError::Unknown(detail)
                    } else {
                        StoreError::Failed(detail)
                    });
                }
                let token = parse_token(&body, now)?;
                let header = format!("Bearer {}", token.value);
                *cache = Some(token);
                Ok(header)
            }
        }
    }
}

fn transport(error: reqwest::Error) -> StoreError {
    StoreError::Unknown(
        if error.is_timeout() {
            "transport error (timeout)"
        } else if error.is_connect() {
            "transport error (connect)"
        } else {
            "transport error"
        }
        .into(),
    )
}

fn classify(status: u16, code: &str) -> StoreError {
    let code: String = code
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(64)
        .collect();
    match status {
        404 if code == "ContainerNotFound" => {
            StoreError::Failed("HTTP 404 ContainerNotFound: the container does not exist".into())
        }
        404 => StoreError::NotFound,
        412 => StoreError::Conflict,
        409 if code == "BlobAlreadyExists" => StoreError::Conflict,
        408 | 429 | 500..=599 => StoreError::Unknown(format!("HTTP {status} {code}")),
        _ => StoreError::Failed(format!("HTTP {status} {code}")),
    }
}

fn failure(response: &Response) -> StoreError {
    let code = response
        .headers()
        .get("x-ms-error-code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    classify(response.status().as_u16(), code)
}

fn etag(response: &Response) -> Result<String, StoreError> {
    response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| StoreError::Failed("the response has no ETag".into()))
}

fn read_limited(response: Response, max: u64) -> Result<Vec<u8>, StoreError> {
    if response.content_length().is_some_and(|n| n > max) {
        return Err(StoreError::Failed(
            "the object exceeds the size limit".into(),
        ));
    }
    let mut bytes = Vec::new();
    response
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| StoreError::Unknown("transport error (body)".into()))?;
    if bytes.len() as u64 > max {
        return Err(StoreError::Failed(
            "the object exceeds the size limit".into(),
        ));
    }
    Ok(bytes)
}

fn retry<T>(mut f: impl FnMut() -> Result<T, StoreError>) -> Result<T, StoreError> {
    let mut attempt = 0;
    loop {
        match f() {
            Err(StoreError::Unknown(_)) if attempt + 1 < ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(200 << attempt));
            }
            other => return other,
        }
    }
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Blob names and the continuation marker of one List Blobs page.
fn parse_listing(xml: &str) -> (Vec<String>, Option<String>) {
    let mut names = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Name>") {
        let after = &rest[start + 6..];
        let Some(end) = after.find("</Name>") else {
            break;
        };
        names.push(unescape(&after[..end]));
        rest = &after[end + 7..];
    }
    let next = xml
        .find("<NextMarker>")
        .and_then(|start| {
            let after = &xml[start + 12..];
            after
                .find("</NextMarker>")
                .map(|end| unescape(&after[..end]))
        })
        .filter(|marker| !marker.is_empty());
    (names, next)
}

struct Container {
    http: Client,
    /// `{endpoint}/{container}`.
    base: String,
    credential: Arc<Credential>,
}

impl Container {
    fn url(&self, name: &str, query: &[(&str, &str)]) -> Result<Url, StoreError> {
        let text = if name.is_empty() {
            self.base.clone()
        } else {
            format!("{}/{name}", self.base)
        };
        let mut url =
            Url::parse(&text).map_err(|_| StoreError::Failed("invalid object name".into()))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    fn send(
        &self,
        verb: Verb,
        url: Url,
        mut headers: Vec<(&'static str, String)>,
        body: Option<Vec<u8>>,
    ) -> Result<Response, StoreError> {
        headers.push(("x-ms-version", API_VERSION.into()));
        headers.push(("x-ms-date", rfc1123(now_secs())));
        let length = body.as_ref().map_or(0, Vec::len);
        let authorization = self
            .credential
            .authorization(verb.as_str(), &url, &headers, length)?;
        let mut request = self
            .http
            .request(verb, url)
            .header("Authorization", authorization);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        request.send().map_err(transport)
    }

    fn put_once(
        &self,
        url: Url,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    ) -> Result<String, StoreError> {
        let response = self.send(Verb::PUT, url, headers, Some(body))?;
        if response.status().is_success() {
            etag(&response)
        } else {
            Err(failure(&response))
        }
    }
}

fn condition(headers: &mut Vec<(&'static str, String)>, cond: &Cond<'_>) {
    match cond {
        Cond::Create => headers.push(("if-none-match", "*".into())),
        Cond::Match(etag) => headers.push(("if-match", (*etag).to_owned())),
    }
}

impl ObjectStore for Container {
    fn get(&self, name: &str, max: u64) -> Result<Blob, StoreError> {
        retry(|| {
            let response = self.send(Verb::GET, self.url(name, &[])?, Vec::new(), None)?;
            if !response.status().is_success() {
                return Err(failure(&response));
            }
            let etag = etag(&response)?;
            Ok(Blob {
                bytes: read_limited(response, max)?,
                etag,
            })
        })
    }

    fn download(&self, name: &str, out: &mut dyn Write, max: u64) -> Result<u64, StoreError> {
        let (length, tag) = retry(|| {
            let response = self.send(Verb::HEAD, self.url(name, &[])?, Vec::new(), None)?;
            if !response.status().is_success() {
                return Err(failure(&response));
            }
            let length = response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| StoreError::Failed("the response has no length".into()))?;
            Ok((length, etag(&response)?))
        })?;
        if length > max {
            return Err(StoreError::Failed(
                "the object is larger than its manifest entry".into(),
            ));
        }
        let mut offset = 0;
        while offset < length {
            let end = (offset + RANGE).min(length) - 1;
            let want = end - offset + 1;
            let chunk = retry(|| {
                let headers = vec![
                    ("x-ms-range", format!("bytes={offset}-{end}")),
                    ("if-match", tag.clone()),
                ];
                let response = self.send(Verb::GET, self.url(name, &[])?, headers, None)?;
                if !response.status().is_success() {
                    return Err(failure(&response));
                }
                let bytes = read_limited(response, want)?;
                if bytes.len() as u64 != want {
                    return Err(StoreError::Unknown("short range response".into()));
                }
                Ok(bytes)
            })?;
            out.write_all(&chunk)
                .map_err(|e| StoreError::Failed(format!("local write failed: {e}")))?;
            offset = end + 1;
        }
        Ok(length)
    }

    fn put(&self, name: &str, bytes: &[u8], cond: Cond<'_>) -> Result<String, StoreError> {
        let mut headers = vec![("x-ms-blob-type", "BlockBlob".to_owned())];
        condition(&mut headers, &cond);
        self.put_once(self.url(name, &[])?, headers, bytes.to_vec())
    }

    fn put_file(&self, name: &str, path: &FilePath, cond: Cond<'_>) -> Result<String, StoreError> {
        let local = |e: std::io::Error| StoreError::Failed(format!("local read failed: {e}"));
        let mut file = std::fs::File::open(path).map_err(local)?;
        let length = file.metadata().map_err(local)?.len();
        if length <= SINGLE_PUT_MAX {
            let mut bytes = Vec::with_capacity(length as usize);
            file.read_to_end(&mut bytes).map_err(local)?;
            return self.put(name, &bytes, cond);
        }
        let blocks = length.div_ceil(BLOCK);
        if blocks > MAX_BLOCKS {
            return Err(StoreError::Failed("the object is too large".into()));
        }
        let mut ids = Vec::with_capacity(blocks as usize);
        for index in 0..blocks {
            let size = BLOCK.min(length - index * BLOCK) as usize;
            let mut chunk = vec![0; size];
            file.read_exact(&mut chunk).map_err(local)?;
            let id = BASE64_STANDARD.encode(format!("{index:08}"));
            retry(|| {
                let url = self.url(name, &[("comp", "block"), ("blockid", &id)])?;
                self.put_once(url, Vec::new(), chunk.clone()).map(|_| ())
            })?;
            ids.push(id);
        }
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
        for id in &ids {
            xml.push_str("<Latest>");
            xml.push_str(id);
            xml.push_str("</Latest>");
        }
        xml.push_str("</BlockList>");
        let mut headers = Vec::new();
        condition(&mut headers, &cond);
        self.put_once(
            self.url(name, &[("comp", "blocklist")])?,
            headers,
            xml.into_bytes(),
        )
    }

    fn delete(&self, name: &str) -> Result<(), StoreError> {
        retry(|| {
            let response = self.send(Verb::DELETE, self.url(name, &[])?, Vec::new(), None)?;
            if response.status().is_success() {
                return Ok(());
            }
            match failure(&response) {
                StoreError::NotFound => Ok(()),
                error => Err(error),
            }
        })
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let mut names = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let body = retry(|| {
                let mut query = vec![
                    ("restype", "container"),
                    ("comp", "list"),
                    ("prefix", prefix),
                    ("maxresults", "5000"),
                ];
                if let Some(marker) = &marker {
                    query.push(("marker", marker.as_str()));
                }
                let response = self.send(Verb::GET, self.url("", &query)?, Vec::new(), None)?;
                if !response.status().is_success() {
                    return Err(failure(&response));
                }
                read_limited(response, LIST_BODY_MAX)
            })?;
            let text = String::from_utf8(body)
                .map_err(|_| StoreError::Failed("invalid listing response".into()))?;
            let (page, next) = parse_listing(&text);
            names.extend(page);
            if names.len() > MAX_LISTED {
                return Err(StoreError::Failed(
                    "too many objects under the prefix".into(),
                ));
            }
            match next {
                Some(next) => marker = Some(next),
                None => return Ok(names),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    #[test]
    fn dates_use_rfc1123() {
        assert_eq!(rfc1123(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(rfc1123(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 GMT");
        assert_eq!(rfc1123(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
    }

    #[test]
    fn string_to_sign_follows_the_documented_layout() {
        let url = Url::parse(
            "http://127.0.0.1:10000/devstoreaccount1/data?restype=container&comp=list&prefix=p%2Fsets%2F",
        )
        .unwrap();
        let headers = vec![
            ("if-match", "\"0x1\"".to_owned()),
            ("x-ms-version", API_VERSION.to_owned()),
            ("x-ms-date", "Tue, 14 Nov 2023 22:13:20 GMT".to_owned()),
            ("x-ms-blob-type", "BlockBlob".to_owned()),
        ];
        let text = string_to_sign("PUT", &url, &headers, 12, "devstoreaccount1");
        assert_eq!(
            text,
            "PUT\n\n\n12\n\n\n\n\n\"0x1\"\n\n\n\n\
             x-ms-blob-type:BlockBlob\nx-ms-date:Tue, 14 Nov 2023 22:13:20 GMT\nx-ms-version:2021-08-06\n\
             /devstoreaccount1/devstoreaccount1/data\ncomp:list\nprefix:p/sets/\nrestype:container"
        );
        // A zero length is signed as an empty field.
        let empty = string_to_sign("GET", &url, &[], 0, "a");
        assert!(empty.starts_with("GET\n\n\n\n"));
    }

    #[test]
    fn listing_pages_are_parsed_and_unescaped() {
        let xml = "<?xml version=\"1.0\"?><EnumerationResults ContainerName=\"c\"><Prefix>p/</Prefix>\
            <Blobs><Blob><Name>p/a&amp;b</Name><Properties/></Blob><Blob><Name>p/c</Name></Blob></Blobs>\
            <NextMarker>2!8!abc&lt;</NextMarker></EnumerationResults>";
        let (names, next) = parse_listing(xml);
        assert_eq!(names, vec!["p/a&b", "p/c"]);
        assert_eq!(next.as_deref(), Some("2!8!abc<"));
        let (names, next) = parse_listing("<Blobs></Blobs><NextMarker />");
        assert!(names.is_empty() && next.is_none());
        let (_, next) = parse_listing("<NextMarker></NextMarker>");
        assert!(next.is_none());
    }

    #[test]
    fn statuses_map_to_protocol_outcomes() {
        assert_eq!(classify(404, "BlobNotFound"), StoreError::NotFound);
        assert!(matches!(
            classify(404, "ContainerNotFound"),
            StoreError::Failed(_)
        ));
        assert_eq!(classify(412, "ConditionNotMet"), StoreError::Conflict);
        assert_eq!(classify(409, "BlobAlreadyExists"), StoreError::Conflict);
        assert!(matches!(
            classify(409, "BlobImmutableDueToPolicy"),
            StoreError::Failed(_)
        ));
        assert!(matches!(
            classify(403, "AuthorizationPermissionMismatch"),
            StoreError::Failed(d) if d.contains("AuthorizationPermissionMismatch")
        ));
        for status in [408, 429, 500, 503] {
            assert!(matches!(classify(status, ""), StoreError::Unknown(_)));
        }
        assert!(matches!(
            classify(400, "<script>\nx"),
            StoreError::Failed(d) if d == "HTTP 400 scriptx"
        ));
    }

    #[test]
    fn token_responses_accept_string_or_numeric_expiry() {
        let token = parse_token(br#"{"access_token":"t","expires_on":"1700000600"}"#, 1).unwrap();
        assert_eq!(
            (token.value.as_str(), token.expires_at),
            ("t", 1_700_000_600)
        );
        let token = parse_token(br#"{"access_token":"t","expires_in":3599}"#, 100).unwrap();
        assert_eq!(token.expires_at, 3699);
        assert!(parse_token(br#"{"access_token":""}"#, 1).is_err());
        assert!(parse_token(b"not json", 1).is_err());
    }

    fn target(auth: AuthMode) -> Target {
        Target {
            account: "devstoreaccount1".into(),
            container: "vlite-data".into(),
            coordination_container: Some("vlite-coord".into()),
            prefix: "p/".into(),
            endpoint: Some("http://127.0.0.1:9/devstoreaccount1".into()),
            auth,
            client_id: None,
        }
    }

    #[test]
    fn credentials_must_match_the_auth_mode() {
        let err = connect_with(
            &target(AuthMode::EmulatorSharedKey),
            None,
            IdentitySource::Imds,
        )
        .err()
        .unwrap();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        let err = connect_with(
            &target(AuthMode::ManagedIdentity),
            Some(vec![1]),
            IdentitySource::Imds,
        )
        .err()
        .unwrap();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, "  AQID\n").unwrap();
        assert_eq!(read_shared_key(&path).unwrap(), vec![1, 2, 3]);
        std::fs::write(&path, "not base64!").unwrap();
        assert!(read_shared_key(&path).is_err());
    }

    /// One-shot HTTP responder; returns the request head it received.
    fn serve_once(response: String) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                head.push_str(&line);
            }
            let mut stream = stream;
            stream.write_all(response.as_bytes()).unwrap();
            head
        });
        (address, handle)
    }

    #[test]
    fn platform_managed_identity_tokens_are_requested_and_cached() {
        let body = r#"{"access_token":"tok123","expires_on":"99999999999"}"#;
        let (address, handle) = serve_once(format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ));
        let credential = Credential::Managed {
            source: IdentitySource::Platform {
                endpoint: format!("{address}/msi/token"),
                header: "hdr-secret".into(),
            },
            client_id: Some("01234567-89ab-cdef-0123-456789abcdef".into()),
            http: client(true).unwrap(),
            cache: Mutex::new(None),
        };
        let url = Url::parse("https://a.blob.core.windows.net/c/b").unwrap();
        let auth = credential.authorization("GET", &url, &[], 0).unwrap();
        assert_eq!(auth, "Bearer tok123");
        let head = handle.join().unwrap().to_ascii_lowercase();
        assert!(
            head.starts_with("get /msi/token?api-version=2019-08-01"),
            "{head}"
        );
        assert!(
            head.contains("resource=https%3a%2f%2fstorage.azure.com%2f"),
            "{head}"
        );
        assert!(head.contains("client_id=01234567-89ab-cdef-0123-456789abcdef"));
        assert!(head.contains("x-identity-header: hdr-secret"));
        // Cached: no second request (the responder has exited).
        assert_eq!(
            credential.authorization("GET", &url, &[], 0).unwrap(),
            "Bearer tok123"
        );
    }

    #[test]
    fn unreachable_endpoints_are_unknown_not_failed() {
        let sets = connect_with(
            &target(AuthMode::EmulatorSharedKey),
            Some(vec![7; 32]),
            IdentitySource::Imds,
        )
        .unwrap();
        let err = sets.owner().err().unwrap();
        assert_eq!(err.code, "cloud_store_unavailable");
    }
}
