//! design-tenant-provisioning EPIC-008: the Azure Resource Manager adapter
//! behind the [`Arm`] port, for `Microsoft.App` at api-version
//! [`API_VERSION`]. Nothing in production constructs it: no deployment
//! profile is wired yet and every production refusal stands.
//!
//! Safety, in the order the checks run:
//! - only a mapped, non-singleton container app is ever written, deleted
//!   or stopped, and a write sends exactly the profile body whose digest
//!   the driver asked for, plus the owner tag; all of this is checked
//!   before any request;
//! - a create never adopts: a resource found by the pre-check refuses it,
//!   and the PUT also asks `If-None-Match: *` (best effort only: the
//!   provider does not document it); an update, delete or stop needs our
//!   owner tag on the live resource;
//! - the managed identity token for `https://management.azure.com/` goes
//!   only to URLs under the configured base; redirects are not followed, a
//!   monitor or next-page URL elsewhere is never called and a mutating
//!   request is never retried;
//! - [`ArmError::Refused`] means nothing mutating took effect; anything
//!   uncertain is [`ArmError::Unknown`].
//!
//! Operation handles are self-contained (a restart resumes from the string
//! alone); [`Handle`] documents their grammar.
use super::aca::{Arm, ArmError, Call, OWNER_TAG, Observed, Poll, Revision, Revisions, Verb};
use super::resource_id::{self, Type};
use super::{digest, invalid, is_hex, text};
use crate::server::ApiError;
use crate::server::cloud_backup::azure::{self, IdentitySource, ManagementCredential};
use reqwest::blocking::Client;
use reqwest::{Method, Url};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The public-cloud management endpoint.
const BASE: &str = "https://management.azure.com";
/// The `Microsoft.App` api-version of every URL the adapter builds.
const API_VERSION: &str = "2026-07-01";
/// The most of any response body read: a resource or one revision page.
const BODY_MAX: u64 = 4 * 1024 * 1024;
/// The most revision pages one inventory reads; more is incomplete.
const MAX_PAGES: usize = 20;
/// The configuration digest of a resource without a profile body: never a
/// SHA-256 hex digest, so it matches nothing.
const UNMAPPED: &str = "unmapped";
/// The longest error code kept (as `aca.rs` keeps them).
const CODE_MAX: usize = 128;
/// The longest revision name addressed.
const NAME_MAX: usize = 128;

const ASYNC: &str = "async:";
const LOCATION: &str = "location:";
const RESOURCE: &str = "resource:";
const UPDATE: &str = "update:";
const ABSENT: &str = "absent:";
const STOP: &str = "stop:";
const STOP_FAILED: &str = "stop-failed:";
/// Separates an ID from what follows it in one handle: never in a valid
/// resource ID, a SHA-256 hex digest or a sanitized error code.
const AT: char = '@';
/// The length of a configuration digest (SHA-256 hex).
const DIGEST_LEN: usize = 64;
/// The code a throttled (429) request is refused with.
const THROTTLED: &str = "TooManyRequests";

/// The Resource Manager adapter: the desired profile body of every resource
/// it may address, a managed identity for the management audience, and the
/// endpoint its token may go to.
pub(super) struct ArmClient {
    base: Url,
    /// Desired bodies by ASCII-lowercased resource ID.
    bodies: BTreeMap<String, Value>,
    credential: ManagementCredential,
    http: Client,
}

impl ArmClient {
    /// The adapter for the public-cloud endpoint ([`BASE`]).
    pub(super) fn new(
        bodies: BTreeMap<String, Value>,
        identity: IdentitySource,
        client_id: Option<String>,
    ) -> Result<Self, ApiError> {
        Self::with_base(BASE, bodies, identity, client_id, false)
    }

    /// The adapter for a loopback test endpoint.
    #[cfg(test)]
    fn for_test(
        base: &str,
        bodies: BTreeMap<String, Value>,
        identity: IdentitySource,
        client_id: Option<String>,
    ) -> Result<Self, ApiError> {
        Self::with_base(base, bodies, identity, client_id, true)
    }

    fn with_base(
        base: &str,
        bodies: BTreeMap<String, Value>,
        identity: IdentitySource,
        client_id: Option<String>,
        no_proxy: bool,
    ) -> Result<Self, ApiError> {
        let base = Url::parse(base)
            .ok()
            .filter(Url::has_host)
            .ok_or_else(|| invalid("the Resource Manager endpoint is invalid"))?;
        let mut keyed = BTreeMap::new();
        for (id, body) in bodies {
            if keyed.insert(id.to_ascii_lowercase(), body).is_some() {
                return Err(invalid("profile resource IDs differ only in case"));
            }
        }
        Ok(Self {
            base,
            bodies: keyed,
            // Its token client bypasses any proxy on purpose: IMDS is link-local.
            credential: ManagementCredential::new(identity, client_id)?,
            http: azure::client(no_proxy)?,
        })
    }

    /// Whether `url` is under the base: same scheme, host and port, no
    /// credentials, and a path inside the base path.
    fn under(&self, url: &Url) -> bool {
        let base = self.base.path().trim_end_matches('/');
        url.scheme() == self.base.scheme()
            && url.host_str() == self.base.host_str()
            && url.port_or_known_default() == self.base.port_or_known_default()
            && url.username().is_empty()
            && url.password().is_none()
            && url
                .path()
                .strip_prefix(base)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }

    /// `{base}{id}{suffix}?api-version=…`; `id` is a parsed resource ID and
    /// `suffix` checked path text, so neither escapes the path.
    fn url(&self, id: &str, suffix: &str) -> Url {
        let mut url = self.base.clone();
        url.set_path(&format!(
            "{}{id}{suffix}",
            self.base.path().trim_end_matches('/')
        ));
        url.set_query(Some(&format!("api-version={API_VERSION}")));
        url
    }

    /// The profile body of `id`, matched ASCII case-insensitively.
    fn body(&self, id: &str) -> Option<&Value> {
        self.bodies.get(&id.to_ascii_lowercase())
    }

    /// `Bearer <token>` for the management audience, if one is available.
    fn token(&self) -> Option<String> {
        self.credential.authorization().ok()
    }

    /// One request with the token, only ever to a URL under the base; no
    /// redirect is followed and nothing is retried. `None` when no response
    /// arrived.
    fn send(
        &self,
        method: Method,
        url: &Url,
        token: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Option<Reply> {
        if !self.under(url) {
            return None;
        }
        let mut request = self
            .http
            .request(method, url.clone())
            .header("authorization", token);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request.send().ok()?;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let async_operation = header("azure-asyncoperation");
        let location = header("location");
        Some(Reply {
            status: response.status().as_u16(),
            async_operation,
            location,
            body: azure::read_limited(response, BODY_MAX).ok(),
        })
    }

    /// The resource `id` as a JSON object, `None` when ARM says it does not
    /// exist; `Err` names why neither is known.
    fn get(&self, id: &str, token: &str) -> Result<Option<Value>, String> {
        let reply = self
            .send(Method::GET, &self.url(id, ""), token, &[], None)
            .ok_or("unavailable")?;
        match reply.status {
            404 => Ok(None),
            200 => reply.json().map(Some).ok_or_else(|| "unparseable".into()),
            status => Err(format!("http_{status}")),
        }
    }

    /// The local checks of a call, before any request: a valid, mapped,
    /// non-singleton container app and, for a write, the profile body the
    /// driver's digest names, with our owner tag.
    fn plan(&self, call: &Call) -> Result<Plan, ArmError> {
        let (kind, name) =
            resource_id::parse(&call.resource).ok_or_else(|| refused("resource_id_invalid"))?;
        if resource_id::singleton(name) {
            return Err(refused("singleton_refused"));
        }
        if kind != Type::ContainerApp {
            return Err(refused("resource_type_unsupported"));
        }
        let body = self
            .body(&call.resource)
            .ok_or_else(|| refused("resource_unmapped"))?;
        let sendable = |config_digest: &str| {
            if digest::config_digest(body) == config_digest {
                tagged(body, &call.owner)
            } else {
                Err(refused("config_digest_mismatch"))
            }
        };
        Ok(match &call.verb {
            Verb::Put { config_digest } => Plan::Create(sendable(config_digest)?),
            Verb::Update { config_digest } => {
                Plan::Update(sendable(config_digest)?, config_digest.clone())
            }
            Verb::Delete => Plan::Delete,
            Verb::Stop => Plan::Stop,
        })
    }

    /// PUT the tagged profile body, watched by `fallback` (`resource:` for a
    /// create, `update:` for an update). A create also sends
    /// `If-None-Match: *`, which the provider does not document: the
    /// pre-check is what refuses an existing app.
    ///
    /// A PUT whose outcome is unknown may have taken effect, so it is
    /// pollable as `fallback` alone: that readback settles only on a live
    /// resource, and the driver still requires our owner tag and the exact
    /// configuration before it reports one created, so nothing is adopted.
    fn write(
        &self,
        id: &str,
        token: &str,
        body: Vec<u8>,
        fallback: Handle,
    ) -> Result<String, ArmError> {
        let create = matches!(fallback, Handle::Resource(..));
        let headers = [("content-type", "application/json"), ("if-none-match", "*")];
        let headers = if create { &headers[..] } else { &headers[..1] };
        let url = self.url(id, "");
        match accepted(self.send(Method::PUT, &url, token, headers, Some(body))) {
            Ok(reply) => Ok(self.monitor(&reply, fallback)),
            Err(ArmError::Unknown) => Ok(fallback.persisted()),
            Err(refused) => Err(refused),
        }
    }

    /// DELETE; an app ARM reports gone at once, or whose DELETE may have
    /// taken effect, is `absent:`, still verified by polling.
    fn delete(&self, id: &str, token: &str) -> Result<String, ArmError> {
        let absent = Handle::Absent(id);
        let reply = self.send(Method::DELETE, &self.url(id, ""), token, &[], None);
        if reply.as_ref().is_some_and(|reply| reply.status == 404) {
            return Ok(absent.persisted());
        }
        Ok(match accepted(reply) {
            Ok(reply) if reply.status == 202 => self.monitor(&reply, absent),
            Ok(_) | Err(ArmError::Unknown) => absent.persisted(),
            Err(refused) => return Err(refused),
        })
    }

    /// Deactivates every revision flagged active in a complete inventory of
    /// the app `found`, attempting each one whatever became of the others
    /// (a deactivation is idempotent and the app is ours). Refused only when
    /// a deactivation was refused and none may have taken effect;
    /// `stop-failed:` (a terminal failure) when one was refused after
    /// another may have; otherwise `stop:`, polled until none is active.
    fn stop(&self, id: &str, token: &str, found: &Value) -> Result<String, ArmError> {
        let revisions = self
            .inventory(id, token, found)
            .ok_or_else(|| refused("revision_inventory_incomplete"))?;
        let mut effect = false;
        let mut refusal = None;
        for (revision, flagged) in revisions {
            if !flagged {
                continue;
            }
            let url = self.url(id, &format!("/revisions/{}/deactivate", revision.name));
            match accepted(self.send(Method::POST, &url, token, &[], Some(Vec::new()))) {
                Ok(_) | Err(ArmError::Unknown) => effect = true,
                Err(ArmError::Refused(code)) => {
                    refusal.get_or_insert(code);
                }
            }
        }
        match (effect, refusal) {
            (false, Some(code)) => Err(ArmError::Refused(code)),
            (true, Some(code)) => Ok(Handle::StopFailed(id, &code).persisted()),
            (_, None) => Ok(Handle::Stop(id).persisted()),
        }
    }

    /// Every revision of the app `app` with its `active` flag as listed, or
    /// `None` unless every page was read and every entry is well-formed.
    /// Next-page links are followed only under the base, for at most
    /// [`MAX_PAGES`] pages.
    fn inventory(&self, id: &str, token: &str, app: &Value) -> Option<Vec<(Revision, bool)>> {
        let body = self.body(id);
        let mut shadow = app.clone();
        let mut revisions: Vec<(Revision, bool)> = Vec::new();
        let mut next = Some(self.url(id, "/revisions"));
        for _ in 0..MAX_PAGES {
            let Some(url) = next.take() else {
                break;
            };
            let reply = self.send(Method::GET, &url, token, &[], None)?;
            if reply.status != 200 {
                return None;
            }
            let page = reply.json()?;
            for item in page.get("value")?.as_array()? {
                let listed = revision(item, &mut shadow, body)?;
                let name = &listed.0.name;
                if revisions
                    .iter()
                    .any(|(seen, _)| seen.name.eq_ignore_ascii_case(name))
                {
                    return None;
                }
                revisions.push(listed);
            }
            next = match page.get("nextLink") {
                None | Some(Value::Null) => None,
                Some(Value::String(link)) if link.is_empty() => None,
                Some(Value::String(link)) => Some(Url::parse(link).ok()?),
                Some(_) => return None,
            };
        }
        next.is_none().then_some(revisions)
    }

    /// A persisted handle parsed back: a monitor URL under the base with a
    /// fallback (see [`fallback`]), a fallback alone, or a stop handle.
    fn handle<'a>(&self, operation: &'a str) -> Option<Handle<'a>> {
        let monitor = |rest: &'a str| {
            let (url, rest) = rest.split_once(' ')?;
            let url = Url::parse(url).ok().filter(|url| self.under(url))?;
            Some((url, Box::new(fallback(rest)?)))
        };
        if let Some(rest) = operation.strip_prefix(ASYNC) {
            monitor(rest).map(|(url, fallback)| Handle::Async(url, fallback))
        } else if let Some(rest) = operation.strip_prefix(LOCATION) {
            monitor(rest).map(|(url, fallback)| Handle::Location(url, fallback))
        } else if let Some(id) = operation.strip_prefix(STOP) {
            app(id).map(Handle::Stop)
        } else if let Some(rest) = operation.strip_prefix(STOP_FAILED) {
            let (id, code) = rest.split_once(AT)?;
            let code = Some(code).filter(|code| sanitize(code).as_deref() == Some(*code))?;
            app(id).map(|id| Handle::StopFailed(id, code))
        } else {
            fallback(operation)
        }
    }

    /// The handle to watch an accepted request by: its
    /// `Azure-AsyncOperation`, else its `Location`, when under the base and
    /// short enough to persist with `fallback`; else `fallback` alone.
    fn monitor(&self, reply: &Reply, fallback: Handle) -> String {
        let fallback = Box::new(fallback);
        let watched = |url: &Option<String>| {
            Url::parse(url.as_deref()?)
                .ok()
                .filter(|url| self.under(url))
        };
        [
            watched(&reply.async_operation).map(|url| Handle::Async(url, fallback.clone())),
            watched(&reply.location).map(|url| Handle::Location(url, fallback.clone())),
        ]
        .into_iter()
        .flatten()
        .map(|handle| handle.persisted())
        .find(|handle| text(handle))
        .unwrap_or_else(|| fallback.persisted())
    }

    /// Polls `handle`. A monitor that no longer answers definitively (a
    /// Location reply other than 2xx or a coded 400/409/422, an
    /// Azure-AsyncOperation reply other than 200) is replaced by its
    /// fallback, a conservative readback of the resource.
    fn watch(&self, handle: &Handle, token: &str) -> Result<Poll, ArmError> {
        let fetch = |url: &Url| self.send(Method::GET, url, token, &[], None);
        match handle {
            Handle::Location(url, fallback) => {
                let reply = fetch(url).ok_or(ArmError::Unknown)?;
                match (reply.status, reply.code()) {
                    (202, _) => Ok(Poll::Running),
                    (200 | 201 | 204, _) => Ok(Poll::Succeeded),
                    (400 | 409 | 422, Some(code)) => Ok(Poll::Failed(code)),
                    _ => self.watch(fallback, token),
                }
            }
            Handle::Async(url, fallback) => {
                let reply = fetch(url).ok_or(ArmError::Unknown)?;
                if reply.status != 200 {
                    return self.watch(fallback, token);
                }
                let operation = reply.json().ok_or(ArmError::Unknown)?;
                let status = operation
                    .get("status")
                    .and_then(Value::as_str)
                    .ok_or(ArmError::Unknown)?;
                Ok(terminal(status, reply.code()))
            }
            Handle::Resource(id, kind) => match self.get(id, token) {
                Ok(Some(_)) if *kind == Type::Storage => Ok(Poll::Succeeded),
                Ok(Some(found)) => Ok(terminal(state(&found), None)),
                Ok(None) | Err(_) => Err(ArmError::Unknown),
            },
            Handle::Update(id, sent) => {
                // The profile body is only the shape the observed body is
                // projected onto; the comparison is with the digest sent.
                let body = self.body(id).ok_or(ArmError::Unknown)?;
                match self.get(id, token) {
                    // Any terminal state of another configuration is stale.
                    Ok(Some(found)) => Ok(match terminal(state(&found), None) {
                        poll if digest::observed_digest(body, &found) == *sent => poll,
                        _ => Poll::Running,
                    }),
                    Ok(None) | Err(_) => Err(ArmError::Unknown),
                }
            }
            Handle::Absent(id) => match self.get(id, token) {
                Ok(None) => Ok(Poll::Succeeded),
                Ok(Some(_)) => Ok(Poll::Running),
                Err(_) => Err(ArmError::Unknown),
            },
            Handle::Stop(id) => match self.get(id, token) {
                Ok(None) => Ok(Poll::Succeeded),
                Ok(Some(found)) => {
                    let revisions = self.inventory(id, token, &found).ok_or(ArmError::Unknown)?;
                    Ok(if revisions.iter().any(|(revision, _)| revision.active) {
                        Poll::Running
                    } else {
                        Poll::Succeeded
                    })
                }
                Err(_) => Err(ArmError::Unknown),
            },
            Handle::StopFailed(_, code) => Ok(Poll::Failed((*code).into())),
        }
    }
}

impl Arm for ArmClient {
    fn begin(&self, call: &Call) -> Result<String, ArmError> {
        let plan = self.plan(call)?;
        let id = call.resource.as_str();
        let token = self.token().ok_or_else(|| refused("token_unavailable"))?;
        // Nothing mutating has been sent yet, so a failed pre-check refuses.
        let found = self
            .get(id, &token)
            .map_err(|code| ArmError::Refused(format!("precheck_{code}")))?;
        match (plan, found) {
            // Never adopt or overwrite what already exists.
            (Plan::Create(_), Some(_)) => Err(refused("resource_exists")),
            (Plan::Create(body), None) => {
                self.write(id, &token, body, Handle::Resource(id, Type::ContainerApp))
            }
            (Plan::Update(..), None) => Err(refused("resource_missing")),
            (Plan::Update(body, sent), Some(found)) => {
                owned(&found, &call.owner)?;
                self.write(id, &token, body, Handle::Update(id, &sent))
            }
            (Plan::Delete, None) => Ok(Handle::Absent(id).persisted()),
            (Plan::Delete, Some(found)) => {
                owned(&found, &call.owner)?;
                self.delete(id, &token)
            }
            (Plan::Stop, None) => Ok(Handle::Stop(id).persisted()),
            (Plan::Stop, Some(found)) => {
                owned(&found, &call.owner)?;
                self.stop(id, &token, &found)
            }
        }
    }

    fn poll(&self, operation: &str) -> Result<Poll, ArmError> {
        // A handle that is not ours never gets the token.
        let handle = self.handle(operation).ok_or(ArmError::Unknown)?;
        if let Handle::StopFailed(_, code) = handle {
            return Ok(Poll::Failed(code.into()));
        }
        let token = self.token().ok_or(ArmError::Unknown)?;
        self.watch(&handle, &token)
    }

    fn read(&self, resource: &str) -> Result<Option<Observed>, ArmError> {
        let (kind, _) = resource_id::parse(resource).ok_or(ArmError::Unknown)?;
        let token = self.token().ok_or(ArmError::Unknown)?;
        let Some(found) = self.get(resource, &token).map_err(|_| ArmError::Unknown)? else {
            return Ok(None);
        };
        let owner = owner_tag(&found)?;
        let config_digest = match self.body(resource) {
            Some(body) => digest::observed_digest(body, &found),
            None => UNMAPPED.into(),
        };
        let revisions = match kind {
            Type::ContainerApp => match self.inventory(resource, &token, &found) {
                Some(listed) => {
                    Revisions::Complete(listed.into_iter().map(|(revision, _)| revision).collect())
                }
                None => Revisions::Incomplete,
            },
            Type::ManagedEnvironment | Type::Storage => Revisions::NotApplicable,
        };
        Ok(Some(Observed {
            owner,
            config_digest,
            // A storage definition has no provisioning state.
            succeeded: kind == Type::Storage || state(&found).eq_ignore_ascii_case("Succeeded"),
            revisions,
        }))
    }
}

/// What a call does once the live resource has been read.
enum Plan {
    Create(Vec<u8>),
    /// The body and the configuration digest it is sent under.
    Update(Vec<u8>, String),
    Delete,
    Stop,
}

/// An operation handle; [`ArmClient::handle`] parses what
/// [`Handle::persisted`] formats. The persisted grammar:
///
/// ```text
/// handle   = "async:" url " " fallback      ; Azure-AsyncOperation monitor
///          | "location:" url " " fallback   ; Location monitor
///          | fallback
///          | "stop:" id                     ; until no revision is active
///          | "stop-failed:" id "@" code     ; a refused stop, terminal
/// fallback = "resource:" id                 ; its provisioning state
///          | "update:" id "@" digest        ; succeeded with what was sent
///          | "absent:" id                   ; until it is gone
/// ```
///
/// `id` is a resource ID `resource_id::parse` accepts (an app's, except for
/// `resource:` and `absent:`), `digest` the 64 lowercase hex digits of the
/// configuration digest sent and `code` a sanitized error code. A monitor
/// handle splits at its first space: `url` (under the base) never holds
/// one, as the `url` crate percent-encodes spaces, and neither does a
/// fallback, as `resource_id::parse` rejects them. `@` likewise occurs in
/// no ID, digest or code. A monitor whose combined handle would exceed 2048
/// bytes is dropped and the fallback alone persisted.
#[derive(Clone, Debug, PartialEq)]
enum Handle<'a> {
    Async(Url, Box<Handle<'a>>),
    Location(Url, Box<Handle<'a>>),
    Resource(&'a str, Type),
    Update(&'a str, &'a str),
    Absent(&'a str),
    Stop(&'a str),
    StopFailed(&'a str, &'a str),
}

impl Handle<'_> {
    fn persisted(&self) -> String {
        match self {
            Self::Async(url, fallback) => format!("{ASYNC}{url} {}", fallback.persisted()),
            Self::Location(url, fallback) => format!("{LOCATION}{url} {}", fallback.persisted()),
            Self::Resource(id, _) => format!("{RESOURCE}{id}"),
            Self::Update(id, digest) => format!("{UPDATE}{id}{AT}{digest}"),
            Self::Absent(id) => format!("{ABSENT}{id}"),
            Self::Stop(id) => format!("{STOP}{id}"),
            Self::StopFailed(id, code) => format!("{STOP_FAILED}{id}{AT}{code}"),
        }
    }
}

/// A monitor's fallback parsed back, never itself a monitor or a stop.
fn fallback(operation: &str) -> Option<Handle<'_>> {
    if let Some(id) = operation.strip_prefix(RESOURCE) {
        resource_id::parse(id).map(|(kind, _)| Handle::Resource(id, kind))
    } else if let Some(rest) = operation.strip_prefix(UPDATE) {
        let (id, digest) = rest.split_once(AT)?;
        let digest = Some(digest).filter(|digest| is_hex(digest, DIGEST_LEN))?;
        app(id).map(|id| Handle::Update(id, digest))
    } else {
        let id = operation.strip_prefix(ABSENT)?;
        resource_id::parse(id).map(|_| Handle::Absent(id))
    }
}

/// `id` when it is a valid container app ID.
fn app(id: &str) -> Option<&str> {
    let (kind, _) = resource_id::parse(id)?;
    (kind == Type::ContainerApp).then_some(id)
}

/// A Resource Manager response: its status, monitor headers and body
/// (`None` when it could not be read whole within [`BODY_MAX`]).
struct Reply {
    status: u16,
    async_operation: Option<String>,
    location: Option<String>,
    body: Option<Vec<u8>>,
}

impl Reply {
    /// The body as a JSON object.
    fn json(&self) -> Option<Value> {
        serde_json::from_slice::<Value>(self.body.as_deref()?)
            .ok()
            .filter(Value::is_object)
    }

    /// ARM's `error.code`, sanitized.
    fn code(&self) -> Option<String> {
        sanitize(self.json()?.get("error")?.get("code")?.as_str()?)
    }
}

/// How a mutating request ended: a 2xx was accepted and a definitive 4xx
/// took no effect, including a 429, which ARM throttles before the request
/// reaches the resource provider; anything else (no or an unreadable
/// response, 3xx, 408, 5xx) is unknown.
fn accepted(reply: Option<Reply>) -> Result<Reply, ArmError> {
    let reply = reply
        .filter(|reply| reply.body.is_some())
        .ok_or(ArmError::Unknown)?;
    match reply.status {
        200..=299 => Ok(reply),
        408 => Err(ArmError::Unknown),
        429 => Err(refused(THROTTLED)),
        status @ 400..=499 => Err(ArmError::Refused(
            reply.code().unwrap_or_else(|| format!("http_{status}")),
        )),
        _ => Err(ArmError::Unknown),
    }
}

/// `body` with our owner tag: refused unless it is an object whose tags,
/// if any, are an object without the reserved key in any case.
fn tagged(body: &Value, owner: &str) -> Result<Vec<u8>, ArmError> {
    let mut body = body.clone();
    let tags = body
        .as_object_mut()
        .and_then(|body| {
            body.entry("tags")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
        })
        .ok_or_else(|| refused("body_invalid"))?;
    if tags.keys().any(|key| key.eq_ignore_ascii_case(OWNER_TAG)) {
        return Err(refused("owner_tag_reserved"));
    }
    tags.insert(OWNER_TAG.into(), Value::String(owner.into()));
    Ok(body.to_string().into_bytes())
}

/// The owner tag of a live resource, its key matched ASCII
/// case-insensitively; malformed or ambiguous tags are unknown.
fn owner_tag(found: &Value) -> Result<Option<String>, ArmError> {
    let tags = match found.get("tags") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(tags)) => tags,
        Some(_) => return Err(ArmError::Unknown),
    };
    let mut owners = tags
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(OWNER_TAG))
        .map(|(_, owner)| owner);
    match (owners.next(), owners.next()) {
        (None, _) => Ok(None),
        (Some(Value::String(owner)), None) => Ok(Some(owner.clone())),
        _ => Err(ArmError::Unknown),
    }
}

/// Refused unless the live resource carries exactly our owner tag.
fn owned(found: &Value, owner: &str) -> Result<(), ArmError> {
    match owner_tag(found) {
        Ok(Some(tag)) if tag == owner => Ok(()),
        Ok(_) => Err(refused("resource_ownership_conflict")),
        Err(_) => Err(refused("precheck_unparseable")),
    }
}

/// One listed revision with its `active` flag, `None` when malformed. It is
/// active while flagged or still running replicas; a missing replica count
/// is malformed, never zero. Its digest is the app's (`shadow`) with this
/// revision's template.
fn revision(item: &Value, shadow: &mut Value, body: Option<&Value>) -> Option<(Revision, bool)> {
    let name = item.get("name")?.as_str()?;
    let safe = |c: u8| c.is_ascii_alphanumeric() || c == b'-';
    if name.is_empty() || name.len() > NAME_MAX || !name.bytes().all(safe) {
        return None;
    }
    let properties = item.get("properties")?;
    let flagged = properties.get("active")?.as_bool()?;
    let count = properties.get("replicas")?;
    let replicas = count.as_u64().or_else(|| {
        count
            .as_f64()
            .filter(|n| *n >= 0.0 && n.fract() == 0.0)
            .map(|n| n as u64)
    })?;
    let template = properties.get("template").filter(|t| t.is_object())?;
    let config_digest = match body {
        Some(body) => {
            shadow
                .as_object_mut()?
                .entry("properties")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()?
                .insert("template".into(), template.clone());
            digest::observed_digest(body, shadow)
        }
        None => UNMAPPED.into(),
    };
    let revision = Revision {
        name: name.into(),
        active: flagged || replicas > 0,
        config_digest,
    };
    Some((revision, flagged))
}

/// A resource's `properties.provisioningState`, empty when absent.
fn state(found: &Value) -> &str {
    found
        .pointer("/properties/provisioningState")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// An ARM operation or provisioning state: `Failed` only for ARM's own
/// terminal failures; anything not terminal is still running.
fn terminal(state: &str, code: Option<String>) -> Poll {
    if state.eq_ignore_ascii_case("Succeeded") {
        Poll::Succeeded
    } else if state.eq_ignore_ascii_case("Failed") || state.eq_ignore_ascii_case("Canceled") {
        Poll::Failed(code.unwrap_or_else(|| state.into()))
    } else {
        Poll::Running
    }
}

/// An error code as `aca.rs` keeps one: ASCII letters, digits and `._-`,
/// at most [`CODE_MAX`] of them; `None` when nothing is left.
fn sanitize(code: &str) -> Option<String> {
    let code: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
        .take(CODE_MAX)
        .collect();
    (!code.is_empty()).then_some(code)
}

fn refused(code: &str) -> ArmError {
    ArmError::Refused(code.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread::JoinHandle;

    const SCOPE: &str = "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg-1/providers/Microsoft.App";
    const OWNER: &str = "vlite-tenant:t-1";
    const IMAGE: &str = "example.azurecr.io/vlite:1";
    const CLIENT_ID: &str = "01234567-89ab-cdef-0123-456789abcdef";
    /// The management token request the fake identity endpoint expects.
    const TOKEN: &str =
        "GET /msi/token?api-version=2019-08-01&resource=https%3A%2F%2Fmanagement.azure.com%2F";

    // --------------------------------------------------------- fake ARM

    /// One scripted exchange: the expected `METHOD target` and the raw
    /// response; `None` drops the connection unanswered.
    struct Step {
        line: String,
        answer: Option<String>,
    }

    /// A request as the fake received it (header names lowercased).
    struct Seen {
        line: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers.get(name).map(String::as_str)
        }
    }

    /// A loopback HTTP/1.1 server (ARM and the platform identity endpoint)
    /// answering its script in order, one request per connection; a request
    /// off the script gets a 500.
    struct Fake {
        base: String,
        script: Vec<String>,
        thread: JoinHandle<Vec<Seen>>,
    }

    impl Fake {
        /// Stops the server; asserts it saw exactly the scripted requests.
        fn finish(self) -> Vec<Seen> {
            TcpStream::connect(self.base.trim_start_matches("http://")).unwrap();
            let seen = self.thread.join().unwrap();
            let lines: Vec<&str> = seen.iter().map(|s| s.line.as_str()).collect();
            assert_eq!(lines, self.script);
            seen
        }
    }

    fn bind() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        (listener, base)
    }

    fn serve(listener: TcpListener, steps: Vec<Step>) -> Fake {
        let base = format!("http://{}", listener.local_addr().unwrap());
        let script = steps.iter().map(|step| step.line.clone()).collect();
        let thread = std::thread::spawn(move || {
            let mut steps = steps.into_iter();
            let mut seen = Vec::new();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let line = line
                    .trim_end()
                    .rsplit_once(' ')
                    .map_or("", |(line, _)| line)
                    .to_owned();
                let mut headers = BTreeMap::new();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
                    }
                }
                let length = headers
                    .get("content-length")
                    .map_or(0, |n| n.parse().unwrap());
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let answer = match steps.next() {
                    Some(step) if step.line == line => step.answer,
                    _ => Some(reply(500, &[], "")),
                };
                seen.push(Seen {
                    line,
                    headers,
                    body,
                });
                if let Some(answer) = answer {
                    let _ = stream.write_all(answer.as_bytes());
                }
            }
            seen
        });
        Fake {
            base,
            script,
            thread,
        }
    }

    /// A raw response that closes its connection.
    fn reply(status: u16, headers: &[(&str, &str)], body: &str) -> String {
        let mut response = format!("HTTP/1.1 {status} Fake\r\nconnection: close\r\n");
        for (name, value) in headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        if status != 204 {
            response.push_str(&format!(
                "content-type: application/json\r\ncontent-length: {}\r\n",
                body.len()
            ));
        }
        response.push_str("\r\n");
        response.push_str(body);
        response
    }

    fn ok(body: Value) -> String {
        reply(200, &[], &body.to_string())
    }

    fn status(code: u16) -> String {
        reply(code, &[], "")
    }

    fn arm_error(code: u16, error: &str) -> String {
        let body = json!({"error": {"code": error, "message": "details"}});
        reply(code, &[], &body.to_string())
    }

    fn on(line: impl Into<String>, answer: String) -> Step {
        Step {
            line: line.into(),
            answer: Some(answer),
        }
    }

    fn dropped(line: impl Into<String>) -> Step {
        Step {
            line: line.into(),
            answer: None,
        }
    }

    fn token() -> Step {
        on(
            TOKEN,
            ok(json!({"access_token": "arm-token", "expires_on": "99999999999"})),
        )
    }

    /// `METHOD {path}?api-version=…` as ARM receives it.
    fn line(method: &str, path: &str) -> String {
        format!("{method} {path}?api-version={API_VERSION}")
    }

    // --------------------------------------------------------- fixtures

    fn app(name: &str) -> String {
        format!("{SCOPE}/containerApps/{name}")
    }

    fn template(image: &str) -> Value {
        json!({
            "containers": [
                {"name": "vlite", "image": image, "resources": {"cpu": 0.5, "memory": "1Gi"}}
            ],
            "scale": {"minReplicas": 1, "maxReplicas": 1}
        })
    }

    /// The profile body of app-1.
    fn desired(image: &str) -> Value {
        json!({
            "location": "westus2",
            "tags": {"team": "data"},
            "properties": {
                "environmentId": format!("{SCOPE}/managedEnvironments/env-1"),
                "configuration": {"activeRevisionsMode": "Single"},
                "template": template(image)
            }
        })
    }

    /// `desired(image)` as ARM answers it: a display location, a recased
    /// ID, a float integer, server-filled fields and maybe an owner tag.
    fn found(image: &str, owner: Option<&str>) -> Value {
        let mut echo = desired(image);
        echo["id"] = json!(app("app-1"));
        echo["name"] = json!("app-1");
        echo["location"] = json!("West US 2");
        echo["systemData"] = json!({"createdBy": "someone@example.com"});
        let properties = &mut echo["properties"];
        properties["provisioningState"] = json!("Succeeded");
        properties["environmentId"] =
            json!(format!("{SCOPE}/managedEnvironments/env-1").to_ascii_uppercase());
        properties["latestRevisionName"] = json!("app-1--r2");
        properties["template"]["containers"][0]["resources"]["ephemeralStorage"] = json!("2Gi");
        properties["template"]["scale"]["minReplicas"] = json!(1.0);
        if let Some(owner) = owner {
            echo["tags"]["vlite-tenant"] = json!(owner);
        }
        echo
    }

    /// A revision list entry of app-1.
    fn entry(name: &str, active: bool, replicas: u64, image: &str) -> Value {
        let weight = if active { 100 } else { 0 };
        json!({
            "id": format!("{}/revisions/{name}", app("app-1")),
            "name": name,
            "type": "Microsoft.App/containerApps/revisions",
            "properties": {
                "active": active,
                "replicas": replicas,
                "trafficWeight": weight,
                "template": template(image)
            }
        })
    }

    fn page(items: Vec<Value>, next: Option<&str>) -> String {
        let mut page = json!({ "value": items });
        if let Some(next) = next {
            page["nextLink"] = json!(next);
        }
        ok(page)
    }

    fn adapter(base: &str, bodies: Vec<(String, Value)>, client_id: Option<&str>) -> ArmClient {
        let identity = IdentitySource::Platform {
            endpoint: format!("{base}/msi/token"),
            header: "hdr-secret".into(),
        };
        let bodies = bodies.into_iter().collect();
        ArmClient::for_test(base, bodies, identity, client_id.map(str::to_owned)).unwrap()
    }

    fn mapped(id: &str) -> Vec<(String, Value)> {
        vec![(id.to_owned(), desired(IMAGE))]
    }

    fn call(id: &str, verb: Verb) -> Call {
        Call {
            resource: id.to_owned(),
            owner: OWNER.to_owned(),
            verb,
        }
    }

    fn create() -> Verb {
        Verb::Put {
            config_digest: digest::config_digest(&desired(IMAGE)),
        }
    }

    fn update() -> Verb {
        Verb::Update {
            config_digest: digest::config_digest(&desired(IMAGE)),
        }
    }

    /// What a write of app-1 sends: the profile body plus our owner tag.
    fn sent() -> Value {
        let mut body = desired(IMAGE);
        body["tags"]["vlite-tenant"] = json!(OWNER);
        body
    }

    // ------------------------------------------------------------ begin

    #[test]
    fn a_create_puts_the_tagged_body_once_and_follows_its_async_operation() {
        let (listener, base) = bind();
        let id = app("app-1");
        let path = format!(
            "/subscriptions/00000000-0000-0000-0000-000000000000/providers/Microsoft.App/locations/westus2/containerappOperationStatuses/op-1?api-version={API_VERSION}"
        );
        let operation = format!("{base}{path}");
        let mut pending = found(IMAGE, Some(OWNER));
        pending["properties"]["provisioningState"] = json!("InProgress");
        let fake = serve(
            listener,
            vec![
                token(),
                on(line("GET", &id), status(404)),
                on(
                    line("PUT", &id),
                    reply(
                        201,
                        &[("azure-asyncoperation", &operation), ("retry-after", "5")],
                        &pending.to_string(),
                    ),
                ),
                on(format!("GET {path}"), ok(json!({"status": "InProgress"}))),
                on(format!("GET {path}"), ok(json!({"status": "Succeeded"}))),
            ],
        );
        let arm = adapter(&base, mapped(&id), None);
        let handle = arm.begin(&call(&id, create())).unwrap();
        assert_eq!(handle, format!("async:{operation} resource:{id}"));
        assert!(text(&handle));
        assert_eq!(arm.poll(&handle), Ok(Poll::Running));
        assert_eq!(arm.poll(&handle), Ok(Poll::Succeeded));
        let seen = fake.finish();
        // The token is for Resource Manager, from the platform endpoint.
        assert_eq!(
            seen[0].line.to_ascii_lowercase(),
            "get /msi/token?api-version=2019-08-01&resource=https%3a%2f%2fmanagement.azure.com%2f"
        );
        assert_eq!(seen[0].header("x-identity-header"), Some("hdr-secret"));
        assert_eq!(seen[0].header("authorization"), None);
        for request in &seen[1..] {
            assert_eq!(
                request.header("authorization"),
                Some("Bearer arm-token"),
                "{}",
                request.line
            );
            assert!(
                request.line.contains("api-version=2026-07-01"),
                "{}",
                request.line
            );
        }
        assert_eq!(seen[1].header("if-none-match"), None);
        assert_eq!(seen[2].header("if-none-match"), Some("*"));
        assert_eq!(seen[2].header("content-type"), Some("application/json"));
        assert_eq!(
            serde_json::from_slice::<Value>(&seen[2].body).unwrap(),
            sent()
        );
    }

    #[test]
    fn local_checks_refuse_before_any_request() {
        let (listener, base) = bind();
        let fake = serve(listener, vec![]);
        let id = app("app-1");
        let env = format!("{SCOPE}/managedEnvironments/env-1");
        let storage = format!("{env}/storages/share-1");
        let singletons = [
            app("vlite-app"),
            format!("{SCOPE}/managedEnvironments/VLITE-ENV"),
            format!("{env}/storages/vlite-data"),
        ];
        let mut bodies = mapped(&id);
        for other in singletons.iter().chain([&env, &storage]) {
            bodies.push((other.clone(), desired(IMAGE)));
        }
        let arm = adapter(&base, bodies, None);
        let invalid = [
            String::new(),
            "app-1".into(),
            format!("{id}/revisions"),
            format!("{id}?x=1"),
            format!("{SCOPE}/containerApps/a/../app-1"),
            id.replace("Microsoft.App", "Microsoft.Web"),
            id.replace("app-1", "App-1"),
        ];
        for verb in [create(), update(), Verb::Delete, Verb::Stop] {
            let begin = |id: &str| arm.begin(&call(id, verb.clone()));
            for id in &singletons {
                assert_eq!(begin(id), Err(refused("singleton_refused")), "{id}");
            }
            assert_eq!(begin(&env), Err(refused("resource_type_unsupported")));
            assert_eq!(begin(&storage), Err(refused("resource_type_unsupported")));
            assert_eq!(begin(&app("app-2")), Err(refused("resource_unmapped")));
            for id in &invalid {
                assert_eq!(begin(id), Err(refused("resource_id_invalid")), "{id}");
            }
        }
        // A write sends exactly the mapped body the driver asked for.
        let other = digest::config_digest(&desired("example.azurecr.io/other:2"));
        let put = Verb::Put {
            config_digest: other.clone(),
        };
        let update = Verb::Update {
            config_digest: other,
        };
        for verb in [put, update] {
            assert_eq!(
                arm.begin(&call(&id, verb)),
                Err(refused("config_digest_mismatch"))
            );
        }
        let tagged = |tags: Value| {
            let mut body = desired(IMAGE);
            body["tags"] = tags;
            body
        };
        for (body, code) in [
            (tagged(json!({"VLITE-TENANT": OWNER})), "owner_tag_reserved"),
            (
                tagged(json!({"team": "data", "Vlite-Tenant": "x"})),
                "owner_tag_reserved",
            ),
            (tagged(Value::Null), "body_invalid"),
            (tagged(json!("team")), "body_invalid"),
            (tagged(json!([])), "body_invalid"),
            (json!(["not", "an", "object"]), "body_invalid"),
        ] {
            let arm = adapter(&base, vec![(id.clone(), body.clone())], None);
            let config_digest = digest::config_digest(&body);
            let put = Verb::Put {
                config_digest: config_digest.clone(),
            };
            assert_eq!(arm.begin(&call(&id, put)), Err(refused(code)), "{body}");
            let update = Verb::Update { config_digest };
            assert_eq!(arm.begin(&call(&id, update)), Err(refused(code)), "{body}");
        }
        assert!(fake.finish().is_empty());
    }

    #[test]
    fn failed_prechecks_refuse_and_an_existing_resource_is_never_adopted() {
        let (listener, base) = bind();
        let id = app("app-1");
        let fake = serve(
            listener,
            vec![
                on(TOKEN, status(500)),
                token(),
                on(line("GET", &id), status(500)),
                on(line("GET", &id), reply(200, &[], "not json")),
                on(line("GET", &id), reply(200, &[], "[]")),
                dropped(line("GET", &id)),
                on(
                    line("GET", &id),
                    reply(302, &[("location", &format!("{base}/elsewhere"))], ""),
                ),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(line("GET", &id), ok(found(IMAGE, None))),
            ],
        );
        let arm = adapter(&base, mapped(&id), None);
        let begin = || arm.begin(&call(&id, create()));
        assert_eq!(begin(), Err(refused("token_unavailable")));
        assert_eq!(begin(), Err(refused("precheck_http_500")));
        assert_eq!(begin(), Err(refused("precheck_unparseable")));
        assert_eq!(begin(), Err(refused("precheck_unparseable")));
        assert_eq!(begin(), Err(refused("precheck_unavailable")));
        assert_eq!(begin(), Err(refused("precheck_http_302")));
        // Never adopted or overwritten, ours or not.
        assert_eq!(begin(), Err(refused("resource_exists")));
        assert_eq!(begin(), Err(refused("resource_exists")));
        fake.finish();
    }

    #[test]
    fn write_responses_are_refused_only_when_definitive() {
        let (listener, base) = bind();
        let id = app("app-1");
        let truncated =
            "HTTP/1.1 400 Fake\r\nconnection: close\r\ncontent-length: 64\r\n\r\n{\"error\":"
                .to_owned();
        let long = "A".repeat(200);
        let resource = Ok(format!("resource:{id}"));
        let cases = [
            (
                arm_error(400, "InvalidParameterValue"),
                Err(refused("InvalidParameterValue")),
            ),
            (
                arm_error(409, "ContainerAppConflict"),
                Err(refused("ContainerAppConflict")),
            ),
            (
                arm_error(412, "PreconditionFailed"),
                Err(refused("PreconditionFailed")),
            ),
            (arm_error(422, "bad code/\u{7}!"), Err(refused("badcode"))),
            (arm_error(400, &long), Err(refused(&long[..128]))),
            (reply(403, &[], ""), Err(refused("http_403"))),
            (reply(404, &[], "{}"), Err(refused("http_404"))),
            // Throttled before reaching the provider: no effect.
            (
                arm_error(429, "SubscriptionRequestsThrottled"),
                Err(refused("TooManyRequests")),
            ),
            // An unknown outcome may have taken effect: watched by readback.
            (truncated, resource.clone()),
            (arm_error(408, "RequestTimeout"), resource.clone()),
            (arm_error(500, "InternalServerError"), resource.clone()),
            (arm_error(503, "ServiceUnavailable"), resource.clone()),
            (
                reply(307, &[("location", &format!("{base}/elsewhere"))], ""),
                resource.clone(),
            ),
        ];
        let mut steps = vec![token()];
        for (answer, _) in &cases {
            steps.push(on(line("GET", &id), status(404)));
            steps.push(on(line("PUT", &id), answer.clone()));
        }
        steps.push(on(line("GET", &id), status(404)));
        steps.push(dropped(line("PUT", &id)));
        let fake = serve(listener, steps);
        let arm = adapter(&base, mapped(&id), None);
        for (answer, outcome) in cases {
            assert_eq!(arm.begin(&call(&id, create())), outcome, "{answer}");
        }
        assert_eq!(arm.begin(&call(&id, create())), resource);
        fake.finish();
    }

    #[test]
    fn monitor_urls_off_the_base_are_never_called() {
        let (elsewhere, other) = bind();
        let other_fake = serve(elsewhere, vec![]);
        let (listener, base) = bind();
        let id = app("app-1");
        let host = base.trim_start_matches("http://");
        let foreign = format!("{other}/operations/op-1?api-version={API_VERSION}");
        let userinfo = format!("http://user:secret@{host}/operations/op-1");
        let https = format!("https://{host}/operations/op-1");
        let long = format!("{base}/operations/{}", "a".repeat(2100));
        let located = format!("{base}/operations/op-2?api-version={API_VERSION}");
        let fallback = format!("resource:{id}");
        // Persistable alone, but not with its fallback.
        let room = 2048 - "async:".len() - format!("{base}/operations/").len();
        let tight = format!("{base}/operations/{}", "b".repeat(room));
        assert!(text(&format!("async:{tight}")));
        let fake = serve(
            listener,
            vec![
                token(),
                on(line("GET", &id), status(404)),
                on(
                    line("PUT", &id),
                    reply(
                        201,
                        &[("azure-asyncoperation", &foreign), ("location", &userinfo)],
                        "",
                    ),
                ),
                on(line("GET", &id), status(404)),
                on(
                    line("PUT", &id),
                    reply(
                        200,
                        &[
                            ("azure-asyncoperation", &https),
                            ("location", "/operations/op-2"),
                        ],
                        "",
                    ),
                ),
                on(line("GET", &id), status(404)),
                on(
                    line("PUT", &id),
                    reply(
                        201,
                        &[("azure-asyncoperation", &long), ("location", &located)],
                        "",
                    ),
                ),
                on(line("GET", &id), status(404)),
                on(
                    line("PUT", &id),
                    reply(
                        201,
                        &[("azure-asyncoperation", &tight), ("location", &tight)],
                        "",
                    ),
                ),
            ],
        );
        let arm = adapter(&base, mapped(&id), None);
        assert_eq!(arm.begin(&call(&id, create())), Ok(fallback.clone()));
        assert_eq!(arm.begin(&call(&id, create())), Ok(fallback.clone()));
        // A handle too long to persist falls back to the next monitor.
        assert_eq!(
            arm.begin(&call(&id, create())),
            Ok(format!("location:{located} {fallback}"))
        );
        assert_eq!(arm.begin(&call(&id, create())), Ok(fallback.clone()));
        // Persisted handles off the base stay unknown and are never called.
        for handle in [
            format!("async:{foreign} {fallback}"),
            format!("location:{foreign} {fallback}"),
            format!("async:{userinfo} {fallback}"),
            format!("location:{https} {fallback}"),
        ] {
            assert_eq!(arm.poll(&handle), Err(ArmError::Unknown), "{handle}");
        }
        fake.finish();
        assert!(other_fake.finish().is_empty());
    }

    #[test]
    fn an_update_rewrites_only_our_live_app() {
        let (listener, base) = bind();
        let id = app("app-1");
        let mut recased = found(IMAGE, None);
        recased["tags"]["VLite-Tenant"] = json!(OWNER);
        let mut numeric = found(IMAGE, None);
        numeric["tags"]["vlite-tenant"] = json!(1);
        let fake = serve(
            listener,
            vec![
                on(
                    format!("{TOKEN}&client_id={CLIENT_ID}"),
                    ok(json!({"access_token": "arm-token", "expires_on": "99999999999"})),
                ),
                on(line("GET", &id), status(404)),
                on(line("GET", &id), ok(found(IMAGE, None))),
                on(line("GET", &id), ok(found(IMAGE, Some("vlite-tenant:t-2")))),
                on(line("GET", &id), ok(found(IMAGE, Some("VLITE-TENANT:T-1")))),
                on(line("GET", &id), ok(numeric)),
                on(line("GET", &id), ok(recased)),
                on(line("PUT", &id), status(200)),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(line("PUT", &id), arm_error(429, "TooManyRequests")),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(line("PUT", &id), arm_error(500, "InternalServerError")),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                dropped(line("PUT", &id)),
            ],
        );
        // Profile IDs match ASCII case-insensitively.
        let bodies = vec![(id.to_ascii_uppercase(), desired(IMAGE))];
        let arm = adapter(&base, bodies, Some(CLIENT_ID));
        let begin = || arm.begin(&call(&id, update()));
        let handle = format!("update:{id}@{}", digest::config_digest(&desired(IMAGE)));
        assert_eq!(begin(), Err(refused("resource_missing")));
        assert_eq!(begin(), Err(refused("resource_ownership_conflict")));
        assert_eq!(begin(), Err(refused("resource_ownership_conflict")));
        assert_eq!(begin(), Err(refused("resource_ownership_conflict")));
        assert_eq!(begin(), Err(refused("precheck_unparseable")));
        assert_eq!(begin(), Ok(handle.clone()));
        assert_eq!(begin(), Err(refused("TooManyRequests")));
        // An unknown outcome is watched by the same readback.
        assert_eq!(begin(), Ok(handle.clone()));
        assert_eq!(begin(), Ok(handle));
        let seen = fake.finish();
        assert_eq!(seen[7].header("if-none-match"), None);
        assert_eq!(seen[7].header("authorization"), Some("Bearer arm-token"));
        assert_eq!(
            serde_json::from_slice::<Value>(&seen[7].body).unwrap(),
            sent()
        );
    }

    #[test]
    fn a_delete_removes_only_our_app_and_watches_it_go() {
        let (listener, base) = bind();
        let id = app("app-1");
        let path = format!(
            "/subscriptions/00000000-0000-0000-0000-000000000000/providers/Microsoft.App/locations/westus2/containerappOperationResults/op-3?api-version={API_VERSION}"
        );
        let located = format!("{base}{path}");
        let fake = serve(
            listener,
            vec![
                token(),
                on(line("GET", &id), status(404)),
                on(line("GET", &id), ok(found(IMAGE, Some("vlite-tenant:t-2")))),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(
                    line("DELETE", &id),
                    reply(202, &[("location", &located), ("retry-after", "1")], ""),
                ),
                on(format!("GET {path}"), status(202)),
                on(format!("GET {path}"), status(204)),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(line("DELETE", &id), status(204)),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(line("GET", &id), status(404)),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(line("DELETE", &id), arm_error(404, "ResourceNotFound")),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(line("DELETE", &id), arm_error(500, "InternalServerError")),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                dropped(line("DELETE", &id)),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(line("DELETE", &id), arm_error(409, "Conflict")),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(line("DELETE", &id), arm_error(429, "TooManyRequests")),
            ],
        );
        let arm = adapter(&base, mapped(&id), None);
        let absent = format!("absent:{id}");
        let begin = || arm.begin(&call(&id, Verb::Delete));
        // Already gone: nothing is sent.
        assert_eq!(begin(), Ok(absent.clone()));
        assert_eq!(begin(), Err(refused("resource_ownership_conflict")));
        let handle = begin().unwrap();
        assert_eq!(handle, format!("location:{located} {absent}"));
        assert_eq!(arm.poll(&handle), Ok(Poll::Running));
        assert_eq!(arm.poll(&handle), Ok(Poll::Succeeded));
        assert_eq!(begin(), Ok(absent.clone()));
        assert_eq!(arm.poll(&absent), Ok(Poll::Running));
        assert_eq!(arm.poll(&absent), Ok(Poll::Succeeded));
        assert_eq!(begin(), Ok(absent.clone()));
        // An unknown outcome is watched until the app is gone.
        assert_eq!(begin(), Ok(absent.clone()));
        assert_eq!(begin(), Ok(absent));
        assert_eq!(begin(), Err(refused("Conflict")));
        // Throttled, it took no effect: refused, never watched as absent.
        assert_eq!(begin(), Err(refused("TooManyRequests")));
        fake.finish();
    }

    #[test]
    fn a_stop_deactivates_the_active_revisions_and_waits_for_none() {
        let (listener, base) = bind();
        let id = app("app-1");
        let list = format!("{id}/revisions");
        let fake = serve(
            listener,
            vec![
                token(),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(
                    line("GET", &list),
                    page(
                        vec![
                            entry("app-1--r1", true, 1, IMAGE),
                            entry("app-1--r2", false, 0, IMAGE),
                            entry("app-1--r3", false, 1, IMAGE),
                        ],
                        None,
                    ),
                ),
                on(
                    line("POST", &format!("{list}/app-1--r1/deactivate")),
                    status(200),
                ),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(
                    line("GET", &list),
                    page(
                        vec![
                            entry("app-1--r1", false, 0, IMAGE),
                            entry("app-1--r2", false, 0, IMAGE),
                            entry("app-1--r3", false, 1, IMAGE),
                        ],
                        None,
                    ),
                ),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(
                    line("GET", &list),
                    page(vec![entry("app-1--r1", false, 0, IMAGE)], None),
                ),
                on(line("GET", &id), status(404)),
            ],
        );
        let arm = adapter(&base, mapped(&id), None);
        let handle = arm.begin(&call(&id, Verb::Stop)).unwrap();
        assert_eq!(handle, format!("stop:{id}"));
        // r3 is deactivated but still drains a replica.
        assert_eq!(arm.poll(&handle), Ok(Poll::Running));
        assert_eq!(arm.poll(&handle), Ok(Poll::Succeeded));
        // A deleted app has no active revision either.
        assert_eq!(arm.poll(&handle), Ok(Poll::Succeeded));
        let seen = fake.finish();
        assert_eq!(seen[3].header("content-length"), Some("0"));
        assert_eq!(seen[3].header("authorization"), Some("Bearer arm-token"));
    }

    #[test]
    fn a_stop_is_refused_only_before_any_deactivation() {
        let (listener, base) = bind();
        let id = app("app-1");
        let list = format!("{id}/revisions");
        let both = || {
            page(
                vec![
                    entry("app-1--r1", true, 1, IMAGE),
                    entry("app-1--r2", true, 1, IMAGE),
                ],
                None,
            )
        };
        let three = || {
            page(
                vec![
                    entry("app-1--r1", true, 1, IMAGE),
                    entry("app-1--r2", true, 1, IMAGE),
                    entry("app-1--r3", true, 1, IMAGE),
                ],
                None,
            )
        };
        let first = line("POST", &format!("{list}/app-1--r1/deactivate"));
        let second = line("POST", &format!("{list}/app-1--r2/deactivate"));
        let third = line("POST", &format!("{list}/app-1--r3/deactivate"));
        let ours = || on(line("GET", &id), ok(found(IMAGE, Some(OWNER))));
        let fake = serve(
            listener,
            vec![
                token(),
                ours(),
                on(line("GET", &list), both()),
                on(first.clone(), arm_error(400, "RevisionCannotBeDeactivated")),
                on(second.clone(), arm_error(409, "Conflict")),
                // r3 is still attempted after r2 failed.
                ours(),
                on(line("GET", &list), three()),
                on(first.clone(), status(200)),
                on(second.clone(), arm_error(409, "Conflict")),
                on(third.clone(), status(200)),
                ours(),
                on(line("GET", &list), both()),
                dropped(first.clone()),
                on(second.clone(), arm_error(409, "Conflict")),
                ours(),
                on(line("GET", &list), both()),
                on(first.clone(), status(200)),
                dropped(second.clone()),
                ours(),
                on(line("GET", &list), both()),
                dropped(first.clone()),
                dropped(second.clone()),
                ours(),
                on(line("GET", &list), both()),
                on(first, arm_error(429, "TooManyRequests")),
                on(second, arm_error(429, "TooManyRequests")),
                ours(),
                on(line("GET", &list), status(500)),
                on(line("GET", &id), ok(found(IMAGE, Some("vlite-tenant:t-2")))),
                on(line("GET", &id), status(404)),
            ],
        );
        let arm = adapter(&base, mapped(&id), None);
        let begin = || arm.begin(&call(&id, Verb::Stop));
        // Every deactivation refused: no effect, the first refusal reported.
        assert_eq!(begin(), Err(refused("RevisionCannotBeDeactivated")));
        // r1 (and r3) deactivated, r2 refused: the stop definitely failed.
        let failed = format!("stop-failed:{id}@Conflict");
        assert_eq!(begin(), Ok(failed.clone()));
        assert_eq!(arm.poll(&failed), Ok(Poll::Failed("Conflict".into())));
        // r1 may have been deactivated: not refused either.
        assert_eq!(begin(), Ok(failed));
        // Unknown outcomes alone are watched until none is active.
        assert_eq!(begin(), Ok(format!("stop:{id}")));
        assert_eq!(begin(), Ok(format!("stop:{id}")));
        assert_eq!(begin(), Err(refused("TooManyRequests")));
        assert_eq!(begin(), Err(refused("revision_inventory_incomplete")));
        assert_eq!(begin(), Err(refused("resource_ownership_conflict")));
        assert_eq!(begin(), Ok(format!("stop:{id}")));
        fake.finish();
    }

    #[test]
    fn a_missing_replica_count_never_proves_a_stop() {
        let (listener, base) = bind();
        let id = app("app-1");
        let list = format!("{id}/revisions");
        let ours = || on(line("GET", &id), ok(found(IMAGE, Some(OWNER))));
        let listed = |items: Vec<Value>| on(line("GET", &list), page(items, None));
        // A revision whose live replica count ARM left out.
        let uncounted = |name: &str, active: bool| {
            let mut item = entry(name, active, 0, IMAGE);
            item["properties"]
                .as_object_mut()
                .unwrap()
                .remove("replicas");
            item
        };
        let fake = serve(
            listener,
            vec![
                token(),
                ours(),
                listed(vec![uncounted("app-1--r1", false)]),
                ours(),
                listed(vec![uncounted("app-1--r1", false)]),
                ours(),
                listed(vec![
                    entry("app-1--r2", true, 1, IMAGE),
                    uncounted("app-1--r1", false),
                ]),
                ours(),
                listed(vec![uncounted("app-1--r2", true)]),
                ours(),
                listed(vec![entry("app-1--r1", false, 1, IMAGE)]),
                ours(),
                listed(vec![entry("app-1--r1", false, 0, IMAGE)]),
                on(line("GET", &id), status(404)),
            ],
        );
        let arm = adapter(&base, mapped(&id), None);
        let stop = format!("stop:{id}");
        // Deactivated, but whether it still drains replicas is unknown.
        assert_eq!(arm.poll(&stop), Err(ArmError::Unknown));
        assert_eq!(
            arm.read(&id).unwrap().unwrap().revisions,
            Revisions::Incomplete
        );
        // Refused before any deactivation is sent, flagged or not.
        let begin = || arm.begin(&call(&id, Verb::Stop));
        assert_eq!(begin(), Err(refused("revision_inventory_incomplete")));
        assert_eq!(begin(), Err(refused("revision_inventory_incomplete")));
        // Explicit counts, and a deleted app, still decide the same handle.
        assert_eq!(arm.poll(&stop), Ok(Poll::Running));
        assert_eq!(arm.poll(&stop), Ok(Poll::Succeeded));
        assert_eq!(arm.poll(&stop), Ok(Poll::Succeeded));
        fake.finish();
    }

    // ------------------------------------------------------------- read

    #[test]
    fn a_read_reports_owner_digest_state_and_every_revision() {
        let (listener, base) = bind();
        let id = app("app-1");
        let list = format!("{id}/revisions");
        let next = format!("{base}{list}?api-version={API_VERSION}&$skiptoken=page-2");
        let unmapped = app("app-2");
        let mut pending = found(IMAGE, Some(OWNER));
        pending["properties"]["provisioningState"] = json!("InProgress");
        let fake = serve(
            listener,
            vec![
                token(),
                on(line("GET", &id), status(404)),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(
                    line("GET", &list),
                    page(
                        vec![entry("app-1--r1", false, 0, "example.azurecr.io/vlite:0")],
                        Some(&next),
                    ),
                ),
                on(
                    format!("GET {list}?api-version={API_VERSION}&$skiptoken=page-2"),
                    page(vec![entry("app-1--r2", true, 1, IMAGE)], None),
                ),
                on(
                    line("GET", &id),
                    ok(found("example.azurecr.io/other:2", Some(OWNER))),
                ),
                on(line("GET", &list), page(vec![], None)),
                on(line("GET", &unmapped), ok(found(IMAGE, None))),
                on(
                    line("GET", &format!("{unmapped}/revisions")),
                    page(vec![entry("app-2--r1", true, 0, IMAGE)], None),
                ),
                on(line("GET", &id), ok(pending)),
                on(line("GET", &list), page(vec![], None)),
            ],
        );
        let arm = adapter(&base, mapped(&id), None);
        let digest = digest::config_digest(&desired(IMAGE));
        assert_eq!(arm.read(&id), Ok(None));
        // ARM's echo, with its extra fields, matches the profile body.
        let observed = arm.read(&id).unwrap().unwrap();
        assert_eq!(observed.owner.as_deref(), Some(OWNER));
        assert_eq!(observed.config_digest, digest);
        assert!(observed.succeeded);
        assert_eq!(
            observed.revisions,
            Revisions::Complete(vec![
                Revision {
                    name: "app-1--r1".into(),
                    active: false,
                    config_digest: digest::config_digest(&desired("example.azurecr.io/vlite:0")),
                },
                Revision {
                    name: "app-1--r2".into(),
                    active: true,
                    config_digest: digest.clone(),
                },
            ])
        );
        // A differing desired field changes the digest.
        let observed = arm.read(&id).unwrap().unwrap();
        assert_ne!(observed.config_digest, digest);
        assert_eq!(observed.revisions, Revisions::Complete(vec![]));
        // An unmapped resource never matches anything.
        let observed = arm.read(&unmapped).unwrap().unwrap();
        assert_eq!(observed.owner, None);
        assert_eq!(observed.config_digest, "unmapped");
        assert_eq!(
            observed.revisions,
            Revisions::Complete(vec![Revision {
                name: "app-2--r1".into(),
                active: true,
                config_digest: "unmapped".into(),
            }])
        );
        // Not provisioned yet.
        let observed = arm.read(&id).unwrap().unwrap();
        assert!(!observed.succeeded);
        assert_eq!(observed.config_digest, digest);
        // An invalid ID is never requested.
        assert_eq!(arm.read("/subscriptions/x"), Err(ArmError::Unknown));
        assert_eq!(arm.read(&format!("{id}/revisions")), Err(ArmError::Unknown));
        fake.finish();
    }

    #[test]
    fn the_owner_is_the_reserved_tag_in_any_case() {
        let (listener, base) = bind();
        let env = format!("{SCOPE}/managedEnvironments/env-1");
        let tagged = |tags: Value| {
            ok(json!({"tags": tags, "properties": {"provisioningState": "Succeeded"}}))
        };
        let fake = serve(
            listener,
            vec![
                token(),
                on(line("GET", &env), tagged(json!({"VLITE-TENANT": OWNER}))),
                on(line("GET", &env), tagged(json!({"team": "data"}))),
                on(line("GET", &env), tagged(Value::Null)),
                on(line("GET", &env), ok(json!({"properties": {}}))),
                on(line("GET", &env), tagged(json!({"vlite-tenant": 7}))),
                on(
                    line("GET", &env),
                    tagged(json!({"vlite-tenant": OWNER, "Vlite-Tenant": OWNER})),
                ),
                on(line("GET", &env), tagged(json!(["vlite-tenant"]))),
                on(line("GET", &env), status(500)),
                on(line("GET", &env), reply(200, &[], "[]")),
                dropped(line("GET", &env)),
            ],
        );
        let arm = adapter(&base, vec![], None);
        let owner = || arm.read(&env).map(|found| found.unwrap().owner);
        assert_eq!(owner(), Ok(Some(OWNER.to_owned())));
        assert_eq!(owner(), Ok(None));
        assert_eq!(owner(), Ok(None));
        assert_eq!(owner(), Ok(None));
        for _ in 0..6 {
            assert_eq!(owner(), Err(ArmError::Unknown));
        }
        fake.finish();
    }

    #[test]
    fn an_unprovable_inventory_is_incomplete() {
        let (elsewhere, other) = bind();
        let other_fake = serve(elsewhere, vec![]);
        let (listener, base) = bind();
        let id = app("app-1");
        let list = format!("{id}/revisions");
        let skip = |n: usize| format!("{list}?api-version={API_VERSION}&$skiptoken={n}");
        let ours = || on(line("GET", &id), ok(found(IMAGE, Some(OWNER))));
        let mut steps = vec![token(), ours()];
        // More than 20 pages: exactly 20 are read.
        steps.push(on(
            line("GET", &list),
            page(vec![], Some(&format!("{base}{}", skip(1)))),
        ));
        for n in 1..20 {
            let next = format!("{base}{}", skip(n + 1));
            steps.push(on(format!("GET {}", skip(n)), page(vec![], Some(&next))));
        }
        let template = template(IMAGE);
        let malformed = [
            json!({"properties": {"active": true, "template": template}}),
            json!({"name": "", "properties": {"active": true, "template": template}}),
            json!({"name": "a/b", "properties": {"active": true, "template": template}}),
            json!({"name": "a?b", "properties": {"active": true, "template": template}}),
            json!({"name": "r".repeat(129), "properties": {"active": true, "template": template}}),
            json!({"name": "r1", "properties": {"template": template}}),
            json!({"name": "r1", "properties": {"active": "true", "template": template}}),
            json!({"name": "r1", "properties": {"active": false, "replicas": -1, "template": template}}),
            json!({"name": "r1", "properties": {"active": false, "replicas": "1", "template": template}}),
            json!({"name": "r1", "properties": {"active": false, "replicas": null, "template": template}}),
            json!({"name": "r1", "properties": {"active": false, "replicas": 1.5, "template": template}}),
            // A missing replica count is unknown, never zero.
            json!({"name": "r1", "properties": {"active": false, "template": template}}),
            json!({"name": "r1", "properties": {"active": true, "template": template}}),
            json!({"name": "r1", "properties": {"active": true}}),
            json!({"name": "r1", "properties": {"active": true, "template": "t"}}),
            json!({"name": "r1"}),
            json!("r1"),
        ];
        let answers = [
            page(
                vec![
                    entry("app-1--r1", true, 1, IMAGE),
                    entry("APP-1--R1", false, 0, IMAGE),
                ],
                None,
            ),
            ok(json!({ "nextLink": null })),
            ok(json!({"value": {}})),
            ok(json!({"value": [], "nextLink": 7})),
            ok(json!({"value": [], "nextLink": "not a url"})),
            page(
                vec![],
                Some(&format!("{other}{list}?api-version={API_VERSION}")),
            ),
            reply(200, &[], "not json"),
            status(500),
            status(404),
        ];
        for answer in malformed
            .iter()
            .map(|item| page(vec![item.clone()], None))
            .chain(answers.iter().cloned())
        {
            steps.push(ours());
            steps.push(on(line("GET", &list), answer));
        }
        // A replica count spelled as an integral float still counts.
        let mut float = entry("app-1--r1", false, 0, IMAGE);
        float["properties"]["replicas"] = json!(2.0);
        steps.push(ours());
        steps.push(on(line("GET", &list), page(vec![float], None)));
        let fake = serve(listener, steps);
        let arm = adapter(&base, mapped(&id), None);
        let revisions = || arm.read(&id).unwrap().unwrap().revisions;
        assert_eq!(revisions(), Revisions::Incomplete);
        for case in malformed.iter().map(Value::to_string).chain(answers) {
            assert_eq!(revisions(), Revisions::Incomplete, "{case}");
        }
        assert_eq!(
            revisions(),
            Revisions::Complete(vec![Revision {
                name: "app-1--r1".into(),
                active: true,
                config_digest: digest::config_digest(&desired(IMAGE)),
            }])
        );
        fake.finish();
        assert!(other_fake.finish().is_empty());
    }

    #[test]
    fn storages_and_environments_have_no_revisions() {
        let (listener, base) = bind();
        let env = format!("{SCOPE}/managedEnvironments/env-1");
        let storage = format!("{env}/storages/share-1");
        let share = json!({"properties": {"azureFile": {
            "accountName": "account", "shareName": "share", "accessMode": "ReadWrite"
        }}});
        let environment = json!({"location": "westus2"});
        let mut stored = share.clone();
        stored["id"] = json!(storage);
        stored["name"] = json!("share-1");
        let state = |state: &str| {
            ok(json!({"location": "West US 2", "properties": {"provisioningState": state}}))
        };
        let fake = serve(
            listener,
            vec![
                token(),
                on(line("GET", &storage), ok(stored)),
                on(line("GET", &env), state("Succeeded")),
                on(line("GET", &env), state("Waiting")),
                on(line("GET", &storage), status(404)),
            ],
        );
        let bodies = vec![
            (storage.clone(), share.clone()),
            (env.clone(), environment.clone()),
        ];
        let arm = adapter(&base, bodies, None);
        let observed = |body: &Value, succeeded: bool| {
            Ok(Some(Observed {
                owner: None,
                config_digest: digest::config_digest(body),
                succeeded,
                revisions: Revisions::NotApplicable,
            }))
        };
        // A storage has no provisioning state: present is succeeded.
        assert_eq!(arm.read(&storage), observed(&share, true));
        assert_eq!(arm.read(&env), observed(&environment, true));
        assert_eq!(arm.read(&env), observed(&environment, false));
        assert_eq!(arm.read(&storage), Ok(None));
        fake.finish();
    }

    // ------------------------------------------------------------- poll

    #[test]
    fn polls_end_only_on_terminal_arm_states() {
        let (listener, base) = bind();
        let id = app("app-1");
        let storage = format!("{SCOPE}/managedEnvironments/env-1/storages/share-1");
        let path = format!("/operations/op-1?api-version={API_VERSION}");
        let operation = format!("GET {path}");
        let state = |state: &str| {
            let mut echo = found(IMAGE, Some(OWNER));
            echo["properties"]["provisioningState"] = json!(state);
            ok(echo)
        };
        let failed = json!({"status": "Failed", "error": {"code": "ContainerAppOperationError", "message": "m"}});
        let fake = serve(
            listener,
            vec![
                token(),
                on(&operation, ok(failed)),
                on(&operation, ok(json!({"status": "Canceled"}))),
                on(&operation, ok(json!({"status": "Deleting"}))),
                on(&operation, ok(json!({"state": "Succeeded"}))),
                on(&operation, reply(200, &[], "not json")),
                on(&operation, arm_error(404, "NotFound")),
                on(line("GET", &id), status(500)),
                on(&operation, arm_error(500, "InternalServerError")),
                on(line("GET", &id), state("Succeeded")),
                on(&operation, status(202)),
                on(line("GET", &id), state("InProgress")),
                on(&operation, arm_error(400, "InvalidRequest")),
                on(&operation, arm_error(409, "Conflict")),
                on(&operation, arm_error(422, "Unprocessable")),
                on(&operation, reply(400, &[], "")),
                on(line("GET", &id), status(500)),
                on(&operation, arm_error(401, "InvalidAuthenticationToken")),
                on(line("GET", &id), state("InProgress")),
                on(&operation, arm_error(404, "NotFound")),
                on(line("GET", &id), status(500)),
                on(&operation, arm_error(429, "TooManyRequests")),
                on(line("GET", &id), state("Failed")),
                on(&operation, arm_error(500, "InternalServerError")),
                on(line("GET", &id), state("Succeeded")),
                dropped(&operation),
                on(&operation, status(202)),
                on(&operation, status(204)),
                on(&operation, ok(found(IMAGE, Some(OWNER)))),
                on(&operation, reply(201, &[], "")),
                on(line("GET", &id), state("Succeeded")),
                on(line("GET", &id), state("InProgress")),
                on(line("GET", &id), state("Failed")),
                on(line("GET", &id), state("Canceled")),
                on(line("GET", &id), status(404)),
                on(line("GET", &id), status(500)),
                on(line("GET", &storage), ok(json!({"properties": {}}))),
                on(line("GET", &id), status(500)),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(line("GET", &format!("{id}/revisions")), status(500)),
                on(line("GET", &id), status(500)),
            ],
        );
        let arm = adapter(&base, mapped(&id), None);
        let resource = format!("resource:{id}");
        let asynchronous = format!("async:{base}{path} {resource}");
        let location = format!("location:{base}{path} {resource}");
        let failed = |code: &str| Ok(Poll::Failed(code.into()));
        // Azure-AsyncOperation: only ARM's terminal states end the wait.
        assert_eq!(
            arm.poll(&asynchronous),
            failed("ContainerAppOperationError")
        );
        assert_eq!(arm.poll(&asynchronous), failed("Canceled"));
        assert_eq!(arm.poll(&asynchronous), Ok(Poll::Running));
        for _ in 0..2 {
            assert_eq!(arm.poll(&asynchronous), Err(ArmError::Unknown));
        }
        // Any other reply is replaced by the resource readback.
        assert_eq!(arm.poll(&asynchronous), Err(ArmError::Unknown));
        assert_eq!(arm.poll(&asynchronous), Ok(Poll::Succeeded));
        assert_eq!(arm.poll(&asynchronous), Ok(Poll::Running));
        // Location: a definitive ARM error fails; anything else unclear is
        // replaced by the resource readback.
        assert_eq!(arm.poll(&location), failed("InvalidRequest"));
        assert_eq!(arm.poll(&location), failed("Conflict"));
        assert_eq!(arm.poll(&location), failed("Unprocessable"));
        assert_eq!(arm.poll(&location), Err(ArmError::Unknown));
        assert_eq!(arm.poll(&location), Ok(Poll::Running));
        assert_eq!(arm.poll(&location), Err(ArmError::Unknown));
        assert_eq!(arm.poll(&location), failed("Failed"));
        assert_eq!(arm.poll(&location), Ok(Poll::Succeeded));
        // No reply at all is unknown.
        assert_eq!(arm.poll(&location), Err(ArmError::Unknown));
        assert_eq!(arm.poll(&location), Ok(Poll::Running));
        for _ in 0..3 {
            assert_eq!(arm.poll(&location), Ok(Poll::Succeeded));
        }
        // The resource's own provisioning state.
        assert_eq!(arm.poll(&resource), Ok(Poll::Succeeded));
        assert_eq!(arm.poll(&resource), Ok(Poll::Running));
        assert_eq!(arm.poll(&resource), failed("Failed"));
        assert_eq!(arm.poll(&resource), failed("Canceled"));
        for _ in 0..2 {
            assert_eq!(arm.poll(&resource), Err(ArmError::Unknown));
        }
        assert_eq!(
            arm.poll(&format!("resource:{storage}")),
            Ok(Poll::Succeeded)
        );
        assert_eq!(arm.poll(&format!("absent:{id}")), Err(ArmError::Unknown));
        // A stop is proven only by a complete inventory.
        assert_eq!(arm.poll(&format!("stop:{id}")), Err(ArmError::Unknown));
        assert_eq!(arm.poll(&format!("stop:{id}")), Err(ArmError::Unknown));
        fake.finish();
    }

    #[test]
    fn malformed_handles_are_unknown_without_a_request() {
        let (listener, base) = bind();
        let fake = serve(listener, vec![]);
        let id = app("app-1");
        let arm = adapter(&base, mapped(&id), None);
        let monitor = format!("{base}/operations/x");
        let storage = format!("{SCOPE}/managedEnvironments/env-1/storages/share-1");
        for handle in [
            String::new(),
            "async:".into(),
            "async:not a url".into(),
            format!("location:ftp{}", base.trim_start_matches("http")),
            format!("async:{}", base.replace("127.0.0.1", "localhost")),
            // A monitor needs a fallback that is itself a final handle.
            format!("async:{monitor}"),
            format!("location:{monitor} "),
            format!("async:{monitor} resource:app-1"),
            format!("async:{monitor} location:{monitor} resource:{id}"),
            format!("location:{monitor} stop:{id}"),
            format!("async:{monitor} resource:{id} x"),
            "resource:app-1".into(),
            format!("absent:{id}/x"),
            format!("stop:{SCOPE}/managedEnvironments/env-1"),
            format!("update:{storage}"),
            format!("update:{id} "),
            format!("update:{id}"),
            format!("update:{id}@"),
            format!("update:{id}@abc"),
            format!("update:{id}@{}", "A".repeat(64)),
            format!("update:{id}@{}@{}", "a".repeat(64), "a".repeat(64)),
            format!("update:{storage}@{}", "a".repeat(64)),
            format!("stop-failed:{id}"),
            format!("stop-failed:{id}@"),
            format!("stop-failed:{id}@bad code"),
            format!("stop-failed:{storage}@Conflict"),
            format!("location:{monitor} stop-failed:{id}@Conflict"),
            format!("async:{monitor} update:{id}@{} x", "a".repeat(64)),
            format!("unknown:{id}"),
            format!("RESOURCE:{id}"),
            id.clone(),
        ] {
            assert_eq!(arm.poll(&handle), Err(ArmError::Unknown), "{handle}");
        }
        assert!(fake.finish().is_empty());
    }

    #[test]
    fn an_expired_monitor_falls_back_to_its_handle() {
        let (listener, base) = bind();
        let id = app("app-1");
        let path = format!("/operations/op-1?api-version={API_VERSION}");
        let operation = format!("GET {path}");
        let fake = serve(
            listener,
            vec![
                token(),
                on(&operation, arm_error(404, "NotFound")),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(&operation, status(410)),
                on(line("GET", &id), status(404)),
                on(&operation, arm_error(404, "NotFound")),
                on(line("GET", &id), ok(found("other", Some(OWNER)))),
            ],
        );
        let arm = adapter(&base, mapped(&id), None);
        let monitor = format!("{base}{path}");
        assert_eq!(
            arm.poll(&format!("async:{monitor} resource:{id}")),
            Ok(Poll::Succeeded)
        );
        assert_eq!(
            arm.poll(&format!("location:{monitor} absent:{id}")),
            Ok(Poll::Succeeded)
        );
        // An update's fallback still wants the desired configuration.
        let sent = digest::config_digest(&desired(IMAGE));
        assert_eq!(
            arm.poll(&format!("async:{monitor} update:{id}@{sent}")),
            Ok(Poll::Running)
        );
        fake.finish();
    }

    #[test]
    fn an_update_succeeds_only_on_the_desired_configuration() {
        let (listener, base) = bind();
        let id = app("app-1");
        let stale = |image: &str, state: &str| {
            let mut echo = found(image, Some(OWNER));
            echo["properties"]["provisioningState"] = json!(state);
            ok(echo)
        };
        let fake = serve(
            listener,
            vec![
                token(),
                on(line("GET", &id), stale("other", "Succeeded")),
                on(line("GET", &id), stale("other", "Failed")),
                on(line("GET", &id), stale("other", "Canceled")),
                on(line("GET", &id), stale(IMAGE, "Failed")),
                on(line("GET", &id), stale(IMAGE, "Canceled")),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(line("GET", &id), status(404)),
                // The digest sent, not the profile's current body, decides.
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
            ],
        );
        let arm = adapter(&base, mapped(&id), None);
        let update = format!("update:{id}@{}", digest::config_digest(&desired(IMAGE)));
        // Any terminal state of the previous configuration is still settling.
        for _ in 0..3 {
            assert_eq!(arm.poll(&update), Ok(Poll::Running));
        }
        assert_eq!(arm.poll(&update), Ok(Poll::Failed("Failed".into())));
        assert_eq!(arm.poll(&update), Ok(Poll::Failed("Canceled".into())));
        assert_eq!(arm.poll(&update), Ok(Poll::Succeeded));
        assert_eq!(arm.poll(&update), Err(ArmError::Unknown));
        let older = format!("update:{id}@{}", digest::config_digest(&desired("other")));
        assert_eq!(arm.poll(&older), Ok(Poll::Running));
        // Without a profile body to project onto nothing is called.
        let other = format!(
            "update:{}@{}",
            app("app-2"),
            digest::config_digest(&desired(IMAGE))
        );
        assert_eq!(arm.poll(&other), Err(ArmError::Unknown));
        fake.finish();
    }

    #[test]
    fn every_handle_round_trips() {
        let (listener, base) = bind();
        let fake = serve(listener, vec![]);
        let id = app("app-1");
        let storage = format!("{SCOPE}/managedEnvironments/env-1/storages/share-1");
        let arm = adapter(&base, mapped(&id), None);
        let digest = digest::config_digest(&desired(IMAGE));
        let monitor = Url::parse(&format!("{base}/operations/op-1?api-version=1")).unwrap();
        let fallbacks = [
            Handle::Resource(&id, Type::ContainerApp),
            Handle::Resource(&storage, Type::Storage),
            Handle::Update(&id, &digest),
            Handle::Absent(&id),
            Handle::Absent(&storage),
        ];
        let mut handles = vec![
            Handle::Stop(&id),
            Handle::StopFailed(&id, "Conflict"),
            Handle::StopFailed(&id, "http_400"),
        ];
        for fallback in &fallbacks {
            handles.push(fallback.clone());
            handles.push(Handle::Async(monitor.clone(), Box::new(fallback.clone())));
            handles.push(Handle::Location(
                monitor.clone(),
                Box::new(fallback.clone()),
            ));
        }
        for handle in handles {
            let persisted = handle.persisted();
            assert_eq!(arm.handle(&persisted), Some(handle), "{persisted}");
        }
        assert!(fake.finish().is_empty());
    }

    #[test]
    fn an_over_long_monitor_persists_only_its_fallback() {
        let (listener, base) = bind();
        let id = app("app-1");
        // Persistable alone, but not with its fallback.
        let room = 2048 - "async:".len() - format!("{base}/operations/").len();
        let tight = format!("{base}/operations/{}", "c".repeat(room));
        let headers = [
            ("azure-asyncoperation", tight.as_str()),
            ("location", tight.as_str()),
        ];
        let fake = serve(
            listener,
            vec![
                token(),
                on(line("GET", &id), status(404)),
                on(line("PUT", &id), reply(201, &headers, "")),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
                on(line("GET", &id), ok(found("other", Some(OWNER)))),
                on(line("PUT", &id), reply(200, &headers, "")),
                on(line("GET", &id), ok(found(IMAGE, Some(OWNER)))),
            ],
        );
        let arm = adapter(&base, mapped(&id), None);
        let resource = format!("resource:{id}");
        assert_eq!(arm.begin(&call(&id, create())), Ok(resource.clone()));
        assert_eq!(arm.poll(&resource), Ok(Poll::Succeeded));
        let handle = format!("update:{id}@{}", digest::config_digest(&desired(IMAGE)));
        assert_eq!(arm.begin(&call(&id, update())), Ok(handle.clone()));
        assert_eq!(arm.poll(&handle), Ok(Poll::Succeeded));
        fake.finish();
    }

    // ------------------------------------------------------ construction

    #[test]
    fn the_production_adapter_targets_the_public_endpoint_only() {
        let arm = ArmClient::new(BTreeMap::new(), IdentitySource::Imds, None).unwrap();
        assert_eq!(arm.base.as_str(), "https://management.azure.com/");
        let id = app("app-1");
        assert_eq!(
            arm.url(&id, "").as_str(),
            format!("https://management.azure.com{id}?api-version=2026-07-01")
        );
        let under = |url: &str| arm.under(&Url::parse(url).unwrap());
        assert!(under(
            "https://management.azure.com/subscriptions/x?api-version=1"
        ));
        assert!(under("https://MANAGEMENT.azure.com:443/x"));
        assert!(!under("http://management.azure.com/x"));
        assert!(!under("https://management.azure.com:444/x"));
        assert!(!under("https://management.azure.com.example.org/x"));
        assert!(!under("https://user@management.azure.com/x"));
        assert!(!under("https://example.org/https://management.azure.com/"));
        // A base path bounds the path too.
        let identity = IdentitySource::Imds;
        let arm = ArmClient::for_test("http://127.0.0.1:9/arm/", BTreeMap::new(), identity, None)
            .unwrap();
        assert_eq!(
            arm.url(&id, "/revisions").as_str(),
            format!("http://127.0.0.1:9/arm{id}/revisions?api-version=2026-07-01")
        );
        let under = |url: &str| arm.under(&Url::parse(url).unwrap());
        assert!(under("http://127.0.0.1:9/arm"));
        assert!(under("http://127.0.0.1:9/arm/x"));
        assert!(!under("http://127.0.0.1:9/army/x"));
        assert!(!under("http://127.0.0.1:9/arm/../x"));
        // An unusable endpoint or ambiguous profile IDs are refused.
        for base in ["not a url", "mailto:ops@example.org"] {
            let error = ArmClient::for_test(base, BTreeMap::new(), IdentitySource::Imds, None)
                .err()
                .unwrap();
            assert_eq!(error.status.as_u16(), 422);
        }
        let bodies = BTreeMap::from([
            (id.clone(), json!({})),
            (id.to_ascii_uppercase(), json!({})),
        ]);
        let error = ArmClient::new(bodies, IdentitySource::Imds, None)
            .err()
            .unwrap();
        assert_eq!(error.status.as_u16(), 422);
    }
}
