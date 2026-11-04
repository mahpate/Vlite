//! One authenticated agent-memory REST operation, without retries or local file access.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

const MAX_BYTES: u64 = 8_388_608;
const HELP: &str = "\
vlite-memory-cli --url URL --binding NAME --token-env ENV VERB --request FILE|-
                   [--timeout-ms 1..5000]
VERB: remember, recall, facts, correct-facts, consolidate, forget, receipt,
      erasure, resume-erasure
Send one JSON object to an authenticated running vlite-server; no local mode.
Requests and responses are bounded to 8388608 bytes. The timeout (default 5000)
affects only HTTP, never the request's deadline_ms. No automatic retries,
paging or job loops. Reconcile ambiguous mutations explicitly using receipt.
HTTPS is required except for loopback HTTP; ambient proxies are disabled.
Errors are coded JSON on stderr (withheld if they would expose the credential).
Exit: 0 success (including pending), 2 usage/input, 3 auth, 4 conflict,
      5 deadline/cancel, 6 transport/storage ambiguity, 1 other failure.
";

// Kept as literal pairs so the API reference can inventory the actual routes.
const ROUTES: &[(&str, &str)] = &[
    ("remember", "/remember"),
    ("recall", "/recall"),
    ("facts", "/facts/query"),
    ("correct-facts", "/facts/correct"),
    ("consolidate", "/consolidate"),
    ("forget", "/forget"),
    ("receipt", "/receipts/lookup"),
    ("erasure", "/erasures/get"),
    ("resume-erasure", "/erasures/resume"),
];

struct Failure {
    exit: u8,
    code: String,
    message: String,
}

impl Failure {
    fn new(exit: u8, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            exit,
            code: code.into(),
            message: message.into(),
        }
    }

    fn usage(message: &str) -> Self {
        Self::new(2, "usage", message)
    }
}

struct Cli {
    url: reqwest::Url,
    binding: String,
    token_env: String,
    suffix: &'static str,
    request: OsString,
    timeout_ms: u64,
}

fn parse_args(args: impl Iterator<Item = OsString>) -> Result<Cli, Failure> {
    let mut args = args.peekable();
    let (mut url, mut binding, mut token_env, mut request, mut timeout) =
        (None, None, None, None, None);
    let mut verb = None;
    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|_| Failure::usage("options and verb must be UTF-8"))?;
        let target = match arg.as_str() {
            "--url" => &mut url,
            "--binding" => &mut binding,
            "--token-env" => &mut token_env,
            "--request" => &mut request,
            "--timeout-ms" => &mut timeout,
            option if option.starts_with('-') => return Err(Failure::usage("unknown option")),
            _ => {
                if verb.replace(arg).is_some() {
                    return Err(Failure::usage("expected exactly one verb"));
                }
                continue;
            }
        };
        if target.is_some() {
            return Err(Failure::usage("duplicate option"));
        }
        let value = args
            .next()
            .filter(|value| !value.to_string_lossy().starts_with("--"))
            .ok_or_else(|| Failure::usage("missing option value"))?;
        if arg == "--timeout-ms"
            && value.to_str().is_none_or(|value| {
                !value.bytes().all(|byte| byte.is_ascii_digit())
                    || !matches!(value.parse::<u64>(), Ok(1..=5000))
            })
        {
            return Err(Failure::usage("--timeout-ms must be 1..5000"));
        }
        *target = Some(value);
    }
    let text = |value: OsString| {
        value
            .into_string()
            .map_err(|_| Failure::usage("non-file option values must be UTF-8"))
    };
    let url = text(url.ok_or_else(|| Failure::usage("missing --url"))?)?;
    let binding = text(binding.ok_or_else(|| Failure::usage("missing --binding"))?)?;
    let token_env = text(token_env.ok_or_else(|| Failure::usage("missing --token-env"))?)?;
    let request = request.ok_or_else(|| Failure::usage("missing --request"))?;
    let suffix = verb
        .and_then(|verb| {
            ROUTES
                .iter()
                .find_map(|&(name, suffix)| (name == verb).then_some(suffix))
        })
        .ok_or_else(|| Failure::usage("unknown verb"))?;
    let url_error = || {
        Failure::usage("--url must be an HTTP(S) endpoint without credentials, query or fragment")
    };
    let parsed = reqwest::Url::parse(&url).map_err(|_| url_error())?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || url.split_once("://").is_some_and(|(_, rest)| {
            rest.split('/')
                .next()
                .is_some_and(|host| host.contains('@'))
        })
    {
        return Err(url_error());
    }
    let loopback = parsed.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if parsed.scheme() == "http" && !loopback {
        return Err(Failure::new(
            2,
            "insecure_url",
            "HTTP is permitted only for loopback hosts; use HTTPS",
        ));
    }
    if binding.is_empty()
        || binding.len() > 63
        || !binding.as_bytes()[0].is_ascii_lowercase()
        || !binding
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
    {
        return Err(Failure::usage("invalid binding name"));
    }
    Ok(Cli {
        url: parsed,
        binding,
        token_env,
        suffix,
        request,
        timeout_ms: timeout.map_or(5000, |value| {
            value
                .to_str()
                .expect("timeout validated above")
                .parse()
                .expect("timeout validated above")
        }),
    })
}

// Validate without round-tripping numbers through binary64 or losing duplicate keys.
struct JsonObject(bool);

impl<'de> Deserialize<'de> for JsonObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = JsonObject;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("JSON without duplicate keys")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<JsonObject, M::Error> {
                let mut keys = BTreeSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key) {
                        return Err(de::Error::custom("duplicate key"));
                    }
                    map.next_value::<JsonObject>()?;
                }
                Ok(JsonObject(true))
            }
            fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<JsonObject, S::Error> {
                while seq.next_element::<JsonObject>()?.is_some() {}
                Ok(JsonObject(false))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<JsonObject, E> {
                Ok(JsonObject(false))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<JsonObject, E> {
                Ok(JsonObject(false))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<JsonObject, E> {
                Ok(JsonObject(false))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<JsonObject, E> {
                Ok(JsonObject(false))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<JsonObject, E> {
                Ok(JsonObject(false))
            }
            fn visit_unit<E: de::Error>(self) -> Result<JsonObject, E> {
                Ok(JsonObject(false))
            }
        }
        deserializer.deserialize_any(JsonVisitor)
    }
}

fn is_json_object(bytes: &[u8]) -> bool {
    matches!(
        serde_json::from_slice::<JsonObject>(bytes),
        Ok(JsonObject(true))
    )
}

fn request_bytes(path: &OsStr) -> Result<Vec<u8>, Failure> {
    let read_error = || Failure::new(2, "invalid_input", "cannot read request file or stdin");
    let reader: Box<dyn Read> = if path == OsStr::new("-") {
        Box::new(io::stdin())
    } else {
        Box::new(File::open(path).map_err(|_| read_error())?)
    };
    let mut bytes = Vec::new();
    reader
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| read_error())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Failure::new(
            2,
            "invalid_json",
            "request exceeds 8388608 bytes",
        ));
    }
    if !is_json_object(&bytes) {
        return Err(Failure::new(
            2,
            "invalid_json",
            "request must be a JSON object without duplicate keys",
        ));
    }
    Ok(bytes)
}

fn transport(error: reqwest::Error) -> Failure {
    Failure::new(
        6,
        "transport_error",
        if error.is_timeout() {
            "HTTP request timed out; reconcile mutations by receipt"
        } else {
            "HTTP request failed; reconcile mutations by receipt"
        },
    )
}

fn exit_for(status: u16, code: &str) -> u8 {
    if matches!(code, "commit_indeterminate" | "poisoned") {
        return 6;
    }
    match status {
        400 | 413 | 415 | 422 => 2,
        401 | 403 => 3,
        409 | 412 => 4,
        504 => 5,
        _ => 1,
    }
}

fn clean(text: &str, token: &str) -> String {
    text.replace(token, "[REDACTED]")
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn diagnostic_line(failure: &Failure, token: &str) -> String {
    let redact = |text: &str| {
        if token.is_empty() {
            text.to_owned()
        } else {
            clean(text, token)
        }
    };
    let mut line = format!(
        "{}\n",
        serde_json::json!({"error": {
            "code": redact(&failure.code), "message": redact(&failure.message)
        }})
    );
    if !token.is_empty() && line.contains(token) {
        line = "{\"error\":{\"code\":\"error\",\"message\":\"error details withheld\"}}\n".into();
        if line.contains(token) {
            // Some credentials collide with JSON syntax or the fixed fallback itself.
            line.clear();
        }
    }
    line
}

fn print_failure(failure: Failure, token: &str) -> ExitCode {
    let _ = io::stderr()
        .lock()
        .write_all(diagnostic_line(&failure, token).as_bytes());
    ExitCode::from(failure.exit)
}

// Preserve string and numeric lexemes and key order; refuse credential collisions.
fn response_line(bytes: &[u8], token: &str) -> Result<String, Failure> {
    let invalid = || {
        Failure::new(
            6,
            "invalid_response",
            "server response is not a JSON object; reconcile mutations by receipt",
        )
    };
    if !is_json_object(bytes) {
        return Err(invalid());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    let mut line = String::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        if ch == '"' {
            let mut end = start + 1;
            while let Some((index, next)) = chars.next() {
                if next == '\\' {
                    chars.next();
                } else if next == '"' {
                    end = index + 1;
                    break;
                }
            }
            let decoded: String = serde_json::from_str(&text[start..end]).map_err(|_| invalid())?;
            if decoded.contains(token) {
                return Err(credential_in_response());
            }
            line.push_str(&text[start..end]);
        } else if !ch.is_ascii_whitespace() {
            line.push(ch);
        }
    }
    line.push('\n');
    if line.contains(token) {
        return Err(credential_in_response());
    }
    if line.len() as u64 > MAX_BYTES {
        return Err(Failure::new(
            6,
            "response_too_large",
            "compact response exceeds 8388608 bytes including LF; reconcile mutations by receipt",
        ));
    }
    Ok(line)
}

fn credential_in_response() -> Failure {
    Failure::new(
        6,
        "credential_in_response",
        "response contained the bearer credential and was not printed; reconcile mutations by receipt",
    )
}

fn execute(cli: Cli, token: &str) -> Result<String, Failure> {
    let token_error = || Failure::usage("token variable must contain 1..4096 visible ASCII bytes");
    if token.is_empty()
        || token.len() > 4096
        || !token.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err(token_error());
    }
    let mut authorization =
        HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| token_error())?;
    authorization.set_sensitive(true);
    let body = request_bytes(&cli.request)?;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(cli.timeout_ms))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|_| Failure::new(6, "transport_error", "cannot initialize HTTP client"))?;
    let url = format!(
        "{}/v1/memory/{}{}",
        cli.url.as_str().trim_end_matches('/'),
        cli.binding,
        cli.suffix
    );
    let response = client
        .post(url)
        .header(AUTHORIZATION, authorization)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .map_err(transport)?;
    let status = response.status();
    let mut bytes = Vec::new();
    response
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            Failure::new(
                6,
                "invalid_response",
                "cannot read HTTP response; reconcile mutations by receipt",
            )
        })?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Failure::new(
            6,
            "response_too_large",
            "response exceeds 8388608 bytes; reconcile mutations by receipt",
        ));
    }
    if status.is_success() {
        return response_line(&bytes, token);
    }
    if is_json_object(&bytes)
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
        && let (Some(code), Some(message)) = (
            value["error"]["code"].as_str(),
            value["error"]["message"].as_str(),
        )
    {
        return Err(Failure::new(exit_for(status.as_u16(), code), code, message));
    }
    Err(Failure::new(
        6,
        "invalid_response",
        "server response is not a valid error envelope; reconcile mutations by receipt",
    ))
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && matches!(args[0].to_str(), Some("--help" | "-h")) {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let mut token = String::new();
    let result = parse_args(args.into_iter()).and_then(|cli| {
        token = std::env::var(&cli.token_env).map_err(|_| {
            Failure::usage("token variable must contain 1..4096 visible ASCII bytes")
        })?;
        execute(cli, &token)
    });
    match result {
        Ok(line) => match io::stdout().lock().write_all(line.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => print_failure(
                Failure::new(
                    6,
                    "output_error",
                    "cannot write response; reconcile mutations by receipt",
                ),
                &token,
            ),
        },
        Err(failure) => print_failure(failure, &token),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_keys_are_rejected_recursively_after_unescaping() {
        assert!(!is_json_object(
            br#"{"scope":{"agent":"a","\u0061gent":"b"}}"#
        ));
        assert!(!is_json_object(br#"{"items":[{"id":"a","id":"b"}]}"#));
        assert!(!is_json_object(br#"{} {}"#));
        assert!(is_json_object(br#"{"scope":{"agent":"a"},"id":"a"}"#));
    }

    #[test]
    fn response_compaction_preserves_numbers_and_string_lexemes() {
        let body = br#"{
              "sequence":"18446744073709551615",
              "as_of_ms":-9223372036854775808,
              "score":1.2300e-9,
              "unknown":{"secret":"\u0073ecret","line":"a\nb","quoted":"\""}
            }"#;
        assert_eq!(
            response_line(body, "absent-token")
                .unwrap_or_else(|_| panic!("valid response rejected")),
            "{\"sequence\":\"18446744073709551615\",\"as_of_ms\":-9223372036854775808,\"score\":1.2300e-9,\"unknown\":{\"secret\":\"\\u0073ecret\",\"line\":\"a\\nb\",\"quoted\":\"\\\"\"}}\n"
        );
    }

    #[test]
    fn response_rejects_decoded_and_serialized_credential_matches() {
        for (body, token) in [
            (br#"{"\u0073ecret":"ok"}"#.as_slice(), "secret"),
            (br#"{"value":"\u0073ecret"}"#.as_slice(), "secret"),
            (br#"{"value":1}"#.as_slice(), "1"),
            (br#"{"value":"a\nb"}"#.as_slice(), r"\n"),
        ] {
            let failure = response_line(body, token).expect_err("credential accepted");
            assert_eq!(failure.exit, 6);
            assert_eq!(failure.code, "credential_in_response");
        }
    }

    #[test]
    fn response_output_bound_counts_the_final_lf() {
        let body = format!("{{\"value\":\"{}\"}}", "x".repeat(MAX_BYTES as usize - 12));
        assert_eq!(body.len(), MAX_BYTES as usize);
        assert_eq!(
            response_line(body.as_bytes(), "absent-token")
                .expect_err("LF overflow accepted")
                .code,
            "response_too_large"
        );
    }
}
