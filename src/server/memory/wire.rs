//! Strict memory JSON reading and the memory error body (contract sections
//! 3, 6 and 8; mapping sections 1-2). Duplicate keys are 400 `invalid_json`;
//! every other shape fault is 422 `invalid_input` naming a static schema
//! field in `details.field`, never caller data.

use std::fmt;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value, json};

use super::super::{ApiError, ErrorCode};
use crate::memory::{MemoryScope, WireCode};

/// `MemoryKey` and the source URI: at most 256 UTF-8 bytes.
pub(super) const MAX_KEY_BYTES: usize = 256;

/// A memory error response. `feature` and `field` render as the shared
/// `error.details` context (wire decision 21); both are static names.
#[derive(Debug)]
pub(super) struct Fail {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub feature: Option<&'static str>,
    pub field: Option<&'static str>,
}

impl Fail {
    pub(super) fn invalid(field: &'static str) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "invalid_input",
            message: "memory request field is invalid".into(),
            feature: None,
            field: Some(field),
        }
    }

    pub(super) fn wire(code: WireCode) -> Self {
        Self {
            status: StatusCode::from_u16(code.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            code: code.code,
            message: message(code.code).into(),
            feature: code.feature,
            field: None,
        }
    }

    /// The single body for every binding/scope authorization failure, so an
    /// unknown binding is indistinguishable from an ungranted one.
    pub(super) fn forbidden() -> Self {
        super::super::forbidden_scope().into()
    }
}

fn message(code: &str) -> &'static str {
    match code {
        "memory_capability_unavailable" => "memory capability is not available in this version",
        "memory_feed_unsupported" => "a complete temporal or TTL feed is not available",
        "memory_not_configured" => "memory scope is not provisioned",
        "memory_not_found" => "memory not found",
        "receipt_not_found" => "receipt not found",
        "idempotency_conflict" => "idempotency key was used with a different request",
        "precondition_failed" => "revision precondition failed",
        "memory_attachment_conflict" => "evidence attachment conflict",
        "memory_deletion_pending" => "memory scope deletion is pending",
        "memory_unsupported_ranking" => "ranking selection is not supported for this request",
        "memory_bounded_plan_unsupported" => "no bounded plan is available for this request",
        "deadline_exceeded" => "deadline exceeded",
        "invalid_input" => "memory request field is invalid",
        _ => "internal service error; inspect server logs",
    }
}

impl From<ApiError> for Fail {
    fn from(error: ApiError) -> Self {
        Self {
            status: error.status,
            code: error.code,
            message: error.message,
            feature: None,
            field: None,
        }
    }
}

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        let mut error = json!({"code": self.code, "message": self.message});
        let mut details = Map::new();
        if let Some(feature) = self.feature {
            details.insert("feature".into(), json!(feature));
        }
        if let Some(field) = self.field {
            details.insert("field".into(), json!(field));
        }
        if !details.is_empty() {
            error["details"] = Value::Object(details);
        }
        let mut response = (self.status, Json(json!({ "error": error }))).into_response();
        if self.status == StatusCode::TOO_MANY_REQUESTS {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
            );
        }
        response.extensions_mut().insert(ErrorCode(self.code));
        response
    }
}

fn invalid_json() -> Fail {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_json",
        "request body is not one JSON object with unique keys",
    )
    .into()
}

/// Parses one JSON value, rejecting duplicate object keys at any depth.
pub(super) fn parse(bytes: &[u8]) -> Result<Value, Fail> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = Strict
        .deserialize(&mut deserializer)
        .map_err(|_| invalid_json())?;
    deserializer.end().map_err(|_| invalid_json())?;
    Ok(value)
}

struct Strict;

impl<'de> DeserializeSeed<'de> for Strict {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Strict {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(Strict)? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value_seed(Strict)?;
            if object.insert(key, value).is_some() {
                return Err(serde::de::Error::custom("duplicate key"));
            }
        }
        Ok(Value::Object(object))
    }
}

/// A closed request object: fields are taken by name, `null` is never a
/// value, and anything left over is an unknown field.
pub(super) struct Fields {
    map: Map<String, Value>,
    field: &'static str,
}

impl Fields {
    pub(super) fn new(value: Value, field: &'static str) -> Result<Self, Fail> {
        match value {
            Value::Object(map) => Ok(Self { map, field }),
            _ => Err(Fail::invalid(field)),
        }
    }

    pub(super) fn take(&mut self, name: &'static str) -> Result<Option<Value>, Fail> {
        match self.map.remove(name) {
            Some(Value::Null) => Err(Fail::invalid(name)),
            value => Ok(value),
        }
    }

    pub(super) fn require(&mut self, name: &'static str) -> Result<Value, Fail> {
        self.take(name)?.ok_or(Fail::invalid(name))
    }

    /// The fields not yet taken, for refusal checks before validation.
    pub(super) fn raw(&self) -> &Map<String, Value> {
        &self.map
    }

    /// Removes an empty `evidence` list from the request, from each item and
    /// from the output. It names no record, so it is the same as an absent
    /// field; anything else stays for the refusal check.
    pub(super) fn drop_empty_evidence(&mut self) {
        fn empty(value: &Value) -> bool {
            value.as_array().is_some_and(Vec::is_empty)
        }
        fn strip(value: &mut Value) {
            if let Value::Object(map) = value
                && map.get("evidence").is_some_and(empty)
            {
                map.remove("evidence");
            }
        }
        if self.map.get("evidence").is_some_and(empty) {
            self.map.remove("evidence");
        }
        if let Some(Value::Array(items)) = self.map.get_mut("items") {
            items.iter_mut().for_each(strip);
        }
        if let Some(output) = self.map.get_mut("output") {
            strip(output);
        }
    }

    pub(super) fn finish(self) -> Result<(), Fail> {
        if self.map.is_empty() {
            Ok(())
        } else {
            Err(Fail::invalid(self.field))
        }
    }
}

/// An opaque key: 1..=256 UTF-8 bytes without any control character.
pub(super) fn key(value: Value, field: &'static str) -> Result<String, Fail> {
    match value {
        Value::String(text) if opaque(&text, MAX_KEY_BYTES) => Ok(text),
        _ => Err(Fail::invalid(field)),
    }
}

pub(super) fn opaque(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}

/// `MemoryU64` input: a canonical decimal string or an exact JSON integer.
pub(super) fn u64_value(value: Value, field: &'static str) -> Result<u64, Fail> {
    match &value {
        Value::String(text) if canonical(text, false) => text.parse().ok(),
        Value::Number(number) => number.as_u64(),
        _ => None,
    }
    .ok_or(Fail::invalid(field))
}

/// `MemoryI64` input: a canonical decimal string or an exact JSON integer.
pub(super) fn i64_value(value: Value, field: &'static str) -> Result<i64, Fail> {
    match &value {
        Value::String(text) if canonical(text, true) => text.parse().ok(),
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
    .ok_or(Fail::invalid(field))
}

fn canonical(text: &str, signed: bool) -> bool {
    let digits = match text.strip_prefix('-') {
        Some(rest) if signed => rest,
        Some(_) => return false,
        None => text,
    };
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" && digits.len() == text.len() || !digits.starts_with('0'))
}

/// An exact JSON integer in `min..=max`, `default` when absent.
pub(super) fn integer(
    value: Option<Value>,
    field: &'static str,
    min: u64,
    max: u64,
    default: u64,
) -> Result<u64, Fail> {
    let Some(value) = value else {
        return Ok(default);
    };
    value
        .as_u64()
        .filter(|n| (min..=max).contains(n))
        .ok_or(Fail::invalid(field))
}

pub(super) fn boolean(value: Option<Value>, field: &'static str) -> Result<bool, Fail> {
    match value {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(value),
        Some(_) => Err(Fail::invalid(field)),
    }
}

/// A finite JSON number in `min..=max`.
pub(super) fn number(value: Value, field: &'static str, min: f64, max: f64) -> Result<f64, Fail> {
    value
        .as_f64()
        .filter(|n| n.is_finite() && (min..=max).contains(n))
        .ok_or(Fail::invalid(field))
}

pub(super) fn array(
    value: Value,
    field: &'static str,
    min: usize,
    max: usize,
) -> Result<Vec<Value>, Fail> {
    match value {
        Value::Array(items) if (min..=max).contains(&items.len()) => Ok(items),
        _ => Err(Fail::invalid(field)),
    }
}

/// `MemoryScope`: agent required, session requires user, no tenant field.
/// The effective tenant is the binding's, never caller-supplied.
pub(super) fn scope(value: Value, tenant: &str, field: &'static str) -> Result<MemoryScope, Fail> {
    let mut fields = Fields::new(value, field)?;
    let agent = key(
        fields.require("agent").map_err(|_| Fail::invalid(field))?,
        field,
    )?;
    let user = fields
        .take("user")
        .map_err(|_| Fail::invalid(field))?
        .map(|value| key(value, field))
        .transpose()?;
    let session = fields
        .take("session")
        .map_err(|_| Fail::invalid(field))?
        .map(|value| key(value, field))
        .transpose()?;
    fields.finish()?;
    if session.is_some() && user.is_none() {
        return Err(Fail::invalid(field));
    }
    Ok(MemoryScope {
        tenant: tenant.to_owned(),
        agent,
        user,
        session,
    })
}

/// `MemoryEffectiveScope` output.
pub(super) fn effective(scope: &MemoryScope) -> Value {
    let mut value = json!({"tenant": scope.tenant, "agent": scope.agent});
    if let Some(user) = &scope.user {
        value["user"] = json!(user);
    }
    if let Some(session) = &scope.session {
        value["session"] = json!(session);
    }
    value
}
