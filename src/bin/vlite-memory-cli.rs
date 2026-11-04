//! One authenticated agent-memory REST operation, without retries or local file access.

use std::collections::BTreeSet;
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
    request: String,
    timeout_ms: u64,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Cli, Failure> {
    let mut args = args.peekable();
    let (mut url, mut binding, mut token_env, mut request, mut timeout) =
        (None, None, None, None, None);
    let mut verb = None;
    while let Some(arg) = args.next() {
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
            .filter(|value| !value.starts_with("--"))
            .ok_or_else(|| Failure::usage("missing option value"))?;
        if arg == "--timeout-ms"
            && (!value.bytes().all(|byte| byte.is_ascii_digit())
                || !matches!(value.parse::<u64>(), Ok(1..=5000)))
        {
            return Err(Failure::usage("--timeout-ms must be 1..5000"));
        }
        *target = Some(value);
    }
    let url = url.ok_or_else(|| Failure::usage("missing --url"))?;
    let binding = binding.ok_or_else(|| Failure::usage("missing --binding"))?;
    let token_env = token_env.ok_or_else(|| Failure::usage("missing --token-env"))?;
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
            value.parse().expect("timeout validated above")
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

fn request_bytes(path: &str) -> Result<Vec<u8>, Failure> {
    let read_error = || Failure::new(2, "invalid_input", "cannot read request file or stdin");
    let reader: Box<dyn Read> = if path == "-" {
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

// Preserve response key order and exact numeric lexemes; redact decoded strings,
// including escaped spellings of the credential, then serialize those strings.
fn response_line(bytes: &[u8], token: &str) -> Result<String, Failure> {
    let invalid = || {
        Failure::new(
            6,
            "invalid_response",
            "server response is not a JSON object",
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
            line.push_str(
                &serde_json::to_string(&decoded.replace(token, "[REDACTED]"))
                    .map_err(|_| invalid())?,
            );
        } else if !ch.is_ascii_whitespace() {
            line.push(ch);
        }
    }
    line.push('\n');
    Ok(line)
}

fn execute(cli: Cli, token: &str) -> Result<String, Failure> {
    let token_error =
        || Failure::usage("token variable must contain a non-empty UTF-8 bearer token");
    if token.is_empty() || token.chars().any(char::is_control) {
        return Err(token_error());
    }
    let mut authorization =
        HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| token_error())?;
    authorization.set_sensitive(true);
    let body = request_bytes(&cli.request)?;
    let client = Client::builder()
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
                "transport_error",
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
        exit_for(status.as_u16(), ""),
        "http_error",
        format!("HTTP {} without a valid error envelope", status.as_u16()),
    ))
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && matches!(args[0].to_str(), Some("--help" | "-h")) {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let mut token = String::new();
    let result = args
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| Failure::usage("arguments must be UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>()
        .and_then(|args| parse_args(args.into_iter()))
        .and_then(|cli| {
            token = std::env::var(&cli.token_env).map_err(|_| {
                Failure::usage("token variable must contain a non-empty UTF-8 bearer token")
            })?;
            execute(cli, &token)
        });
    match result {
        Ok(line) => match io::stdout().lock().write_all(line.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => {
                eprintln!("output_error: cannot write response; reconcile mutations by receipt");
                ExitCode::from(6)
            }
        },
        Err(failure) => {
            let redact = |text: &str| {
                if token.is_empty() {
                    text.to_owned()
                } else {
                    clean(text, &token)
                }
            };
            eprintln!("{}: {}", redact(&failure.code), redact(&failure.message));
            ExitCode::from(failure.exit)
        }
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
    fn response_compaction_preserves_numbers_and_redacts_escaped_credentials() {
        let body = br#"{
              "sequence":"18446744073709551615",
              "as_of_ms":-9223372036854775808,
              "score":1.2300e-9,
              "unknown":{"secret":"\u0073ecret","line":"a\nb","quoted":"\""}
            }"#;
        assert_eq!(
            response_line(body, "secret").unwrap_or_else(|_| panic!("valid response rejected")),
            "{\"sequence\":\"18446744073709551615\",\"as_of_ms\":-9223372036854775808,\"score\":1.2300e-9,\"unknown\":{\"[REDACTED]\":\"[REDACTED]\",\"line\":\"a\\nb\",\"quoted\":\"\\\"\"}}\n"
        );
    }
}
