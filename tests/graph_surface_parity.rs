//! REG-008 surface parity (feature-agent-graph EPIC-001 ITEM-002/ITEM-003).
//!
//! Compares the current build's served `/openapi.json` bytes (modulo the
//! exact, reviewed additions in `APPROVED_OPENAPI_ADDITIONS`) and `vlite-mcp`
//! responses (read-only and writable) on a vector-only file against the golden
//! captures produced from baseline commit `347b01d` by
//! `python scripts\graph_baseline.py fixtures`, except for the package version
//! string, which release preparation bumps (see `MASKED_VERSION`). Requires
//! `server` and `mcp`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use vlite::Database;
use vlite::server::{Service, router};

fn fixture(name: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("graph_baseline")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .replace("\r\n", "\n")
}

/// Reviewed post-baseline OpenAPI additions, each matched byte-for-byte (LF)
/// at a line boundary; any other difference from the `347b01d` capture
/// fails REG-008. The `347b01d` capture already contains every change
/// published before it (the `x-durable-group-commit` block, the
/// `AutoMaintenancePolicy` retry fields and `MaintenanceStatus.rebuild.phase`).
/// Approved since: the tenant lifecycle routes (usage, settings, suspend,
/// resume), the dashboard account routes (`/v1/auth/*`, `/v1/admin/users*`)
/// and their `Tenant`, `TenantUsage`, `CapabilityIdentity`, `UserAccount`,
/// `AuthSession` and `AuthSignedIn` schemas; they are new whole lines and
/// leave existing lines untouched. v1.2.20 adds the graph-build-only
/// `graphs` and `graph_access` account fields to those three account lines.
/// Extend this list only for reviewed, intentional additions, or re-pin the
/// baseline with `python scripts\graph_baseline.py fixtures --rev <sha>`.
const APPROVED_OPENAPI_ADDITIONS: &[&str] = &[
    concat!(
        r##"    "/v1/admin/tenants/{id}/usage":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string","pattern":"^[a-f0-9]{32}$"}}],"get":{"summary":"Admin only: live usage of one tenant","description":"Point-in-time snapshot: tenant collections (unqualified names) with live vector counts, the aggregate against max_vectors with remaining headroom, and bound credential counts by state.","responses":{"200":{"description":"Tenant usage","content":{"application/json":{"schema":{"$ref":"#/components/schemas/TenantUsage"}}}},"404":{"description":"tenant_not_found: unknown or deleted tenant"},"409":{"description":"tenants_disabled"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/admin/tenants/{id}/settings":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string","pattern":"^[a-f0-9]{32}$"}}],"put":{"summary":"Admin only: rename a tenant or change its aggregate vector limit","description":"Partial update: send name, max_vectors or both; max_vectors null removes the limit. A limit below current usage is accepted and blocks further growth until usage drops. Durable and audited (tenant_updated).","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"minProperties":1,"properties":{"name":{"type":"string","minLength":1,"maxLength":64,"description":"1..64 UTF-8 bytes without control characters; not unique"},"max_vectors":{"type":["integer","null"],"minimum":0}}}}}},"responses":{"200":{"description":"Updated tenant","content":{"application/json":{"schema":{"type":"object","required":["tenant"],"properties":{"tenant":{"$ref":"#/components/schemas/Tenant"}}}}}},"404":{"description":"tenant_not_found: unknown or deleted tenant"},"409":{"description":"tenants_disabled"},"422":{"description":"invalid_input: empty update, invalid name or invalid limit"},"503":{"description":"tenants_commit_indeterminate: the change is visible but its durability is unconfirmed; list or read usage before retrying"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/admin/tenants/{id}/suspend":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string","pattern":"^[a-f0-9]{32}$"}}],"post":{"summary":"Admin only: suspend a tenant","description":"Tenant-bound credentials are refused with 403 tenant_suspended on REST and gRPC (PERMISSION_DENIED) until the tenant is resumed, and new tenant credentials are refused with 409 tenant_suspended. Collections, vectors and credentials are kept; administrators can still read and clean up tenant collections. Idempotent; durable and audited (tenant_suspended). Servers that predate suspension refuse to start while any tenant is suspended.","responses":{"200":{"description":"Suspended tenant","content":{"application/json":{"schema":{"type":"object","required":["tenant"],"properties":{"tenant":{"$ref":"#/components/schemas/Tenant"}}}}}},"404":{"description":"tenant_not_found: unknown or deleted tenant"},"409":{"description":"tenants_disabled"},"503":{"description":"tenants_commit_indeterminate: the change is visible but its durability is unconfirmed; list or read usage before retrying"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/admin/tenants/{id}/resume":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string","pattern":"^[a-f0-9]{32}$"}}],"post":{"summary":"Admin only: resume a suspended tenant","description":"Restores access for unexpired, unrevoked tenant credentials. Idempotent; durable and audited (tenant_resumed).","responses":{"200":{"description":"Active tenant","content":{"application/json":{"schema":{"type":"object","required":["tenant"],"properties":{"tenant":{"$ref":"#/components/schemas/Tenant"}}}}}},"404":{"description":"tenant_not_found: unknown or deleted tenant"},"409":{"description":"tenants_disabled"},"503":{"description":"tenants_commit_indeterminate: the change is visible but its durability is unconfirmed; list or read usage before retrying"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/auth/config":{"get":{"security":[],"summary":"Public: which dashboard sign-in methods are available","responses":{"200":{"description":"Sign-in configuration","content":{"application/json":{"schema":{"type":"object","required":["mode","password_login","reason","setup_required"],"properties":{"mode":{"enum":["open","protected"],"description":"open means no admin token is configured"},"password_login":{"type":"boolean"},"reason":{"enum":["accounts_disabled","insecure_transport",null],"description":"Why password sign-in is unavailable"},"setup_required":{"type":"boolean","description":"No accounts exist yet: sign in with the admin token and create the first super admin"}}}}}}}}},"##,
        "\n",
        r##"    "/v1/auth/login":{"post":{"security":[],"summary":"Public: sign in with a username and password","description":"Starts a server session carried in an HttpOnly, SameSite=Strict cookie (__Host-vlite_session over HTTPS, otherwise vlite_session). Sessions live in memory, so a restart signs everyone out. Needs HTTPS, a loopback-only listener or VLITE_TRUSTED_PROXY=1, and the X-VLite-Request header; cross-site requests are refused. Unknown usernames and wrong passwords fail the same way; repeated failures lock the username (and, behind a trusted proxy, the client IP) with exponential backoff up to 15 minutes.","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"required":["username","password"],"properties":{"username":{"type":"string","maxLength":64,"description":"Case-insensitive"},"password":{"type":"string","maxLength":128}}}}}},"responses":{"200":{"description":"Signed in","headers":{"Set-Cookie":{"description":"Session cookie","schema":{"type":"string"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/AuthSignedIn"}}}},"401":{"description":"invalid_credentials"},"403":{"description":"account_disabled, insecure_transport, csrf_rejected or tenant_suspended"},"409":{"description":"accounts_disabled (needs an admin token and VLITE_ADMIN_DIR) or accounts_require_upgrade"},"429":{"description":"too_many_attempts","headers":{"Retry-After":{"description":"Seconds until the next attempt is accepted","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"503":{"description":"busy (too many concurrent password checks) or audit_unavailable"}}}},"##,
        "\n",
        r##"    "/v1/auth/session":{"get":{"summary":"Who is signed in, and how","description":"Works for session cookies, bearer tokens and open mode, including while a password change is pending.","responses":{"200":{"description":"Current session","content":{"application/json":{"schema":{"$ref":"#/components/schemas/AuthSession"}}}},"401":{"description":"Missing or invalid token, or session_expired (the response also clears the session cookie)"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/auth/logout":{"post":{"summary":"Sign out: end this session and clear the cookie","description":"Token callers get signed_out false. Cookie requests must send X-VLite-Request.","responses":{"200":{"description":"Signed out","headers":{"Set-Cookie":{"description":"Expires both session cookie names","schema":{"type":"string"}}},"content":{"application/json":{"schema":{"type":"object","required":["signed_out"],"properties":{"signed_out":{"type":"boolean"}}}}}},"401":{"description":"Missing or invalid token, or session_expired"},"403":{"description":"csrf_rejected"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/auth/password":{"post":{"summary":"Change your own password","description":"Session cookie only. Ends every session of the account and starts a new one for this browser. Passwords are 15 to 128 characters (passphrases welcome); the username, common words and simple sequences do not count toward the length. Durable and audited (user_password_changed).","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"required":["current_password","new_password"],"properties":{"current_password":{"type":"string"},"new_password":{"type":"string","minLength":15,"maxLength":128}}}}}},"responses":{"200":{"description":"Password changed; new session cookie","headers":{"Set-Cookie":{"description":"Session cookie","schema":{"type":"string"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/AuthSignedIn"}}}},"401":{"description":"session_expired"},"403":{"description":"wrong_password or csrf_rejected"},"409":{"description":"not_signed_in (token or open mode) or account_changed"},"422":{"description":"weak_password or password_unchanged"},"429":{"description":"too_many_attempts","headers":{"Retry-After":{"description":"Seconds until the next attempt is accepted","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"503":{"description":"accounts_unavailable (nothing changed), accounts_commit_indeterminate (in effect but durability unconfirmed; list accounts before retrying), audit_unavailable or busy"}}}},"##,
        "\n",
        r##"    "/v1/admin/users":{"get":{"summary":"Admin only: list dashboard accounts","responses":{"200":{"description":"Accounts sorted by username","content":{"application/json":{"schema":{"type":"object","required":["items","capacity"],"properties":{"items":{"type":"array","items":{"$ref":"#/components/schemas/UserAccount"}},"capacity":{"const":1024}}}}}},"409":{"description":"accounts_disabled"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}},"post":{"summary":"Admin only: create a dashboard account","description":"Super admins have full admin rights and no tenant. Editors (read, search, write, import, index and collection management) and viewers (read and search) may be confined to one tenant and need a format v4+ database. must_change_password defaults to true. Durable and audited (user_created); passwords are never logged or returned.","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"required":["username","password","role"],"properties":{"username":{"type":"string","minLength":3,"maxLength":64,"description":"Letters, digits, '.', '_', '-' and '@', starting with a letter or digit; stored lowercased"},"password":{"type":"string","minLength":15,"maxLength":128},"role":{"enum":["super_admin","editor","viewer"]},"tenant_id":{"type":["string","null"],"pattern":"^[a-f0-9]{32}$"},"must_change_password":{"type":"boolean","default":true},"disabled":{"type":"boolean","default":false},"graphs":{"type":["array","null"],"maxItems":64,"items":{"type":"string","pattern":"^(\\*|[a-z][a-z0-9_-]{0,62})$"},"description":"Experimental graph builds only; other builds reject the field. Graphs the account may use: null = role default (every graph in its tenant scope), [] = none, or 1-64 names (or [\"*\"]). Tenanted accounts use tenant-local names of at most 28 bytes. Not allowed for super_admin."},"graph_access":{"enum":["none","read","write",null],"description":"Experimental graph builds only; other builds reject the field. Caps graph operations below the role default (editor write, viewer read); null = role default. Viewers cannot have write; not allowed for super_admin."}}}}}},"responses":{"201":{"description":"Created account","content":{"application/json":{"schema":{"type":"object","required":["user"],"properties":{"user":{"$ref":"#/components/schemas/UserAccount"}}}}}},"409":{"description":"username_taken, accounts_full (1024 accounts), accounts_disabled or accounts_require_upgrade"},"422":{"description":"invalid_username, weak_password, tenant_not_found or invalid_input"},"503":{"description":"accounts_unavailable (nothing changed), accounts_commit_indeterminate (in effect but durability unconfirmed; list accounts before retrying), audit_unavailable or busy"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/admin/users/{id}":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string","pattern":"^[a-f0-9]{32}$"}}],"put":{"summary":"Admin only: change an account's username, role, tenant or status","description":"Partial update; tenant_id null removes the tenant. Any effective change ends the account's sessions. Your own account cannot be changed here (cannot_modify_self). Durable and audited (user_updated, with the changed fields).","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"minProperties":1,"properties":{"username":{"type":"string","minLength":3,"maxLength":64},"role":{"enum":["super_admin","editor","viewer"]},"tenant_id":{"type":["string","null"],"pattern":"^[a-f0-9]{32}$"},"disabled":{"type":"boolean"},"must_change_password":{"type":"boolean"},"graphs":{"type":["array","null"],"maxItems":64,"items":{"type":"string","pattern":"^(\\*|[a-z][a-z0-9_-]{0,62})$"},"description":"Experimental graph builds only; other builds reject the field. Graphs the account may use: null resets to the role default (every graph in its tenant scope), [] = none, or 1-64 names (or [\"*\"]). Tenanted accounts use tenant-local names of at most 28 bytes. Not allowed for super_admin."},"graph_access":{"enum":["none","read","write",null],"description":"Experimental graph builds only; other builds reject the field. Caps graph operations below the role default (editor write, viewer read); null resets to the role default. Viewers cannot have write; not allowed for super_admin."}}}}}},"responses":{"200":{"description":"Updated account and the fields that changed (empty when nothing changed)","content":{"application/json":{"schema":{"type":"object","required":["user","changed"],"properties":{"user":{"$ref":"#/components/schemas/UserAccount"},"changed":{"type":"array","items":{"enum":["username","role","tenant_id","disabled","must_change_password","graphs","graph_access"]}}}}}}},"404":{"description":"user_not_found"},"409":{"description":"cannot_modify_self, username_taken, accounts_disabled or accounts_require_upgrade"},"422":{"description":"invalid_username, tenant_not_found or invalid_input"},"503":{"description":"accounts_unavailable (nothing changed), accounts_commit_indeterminate (in effect but durability unconfirmed; list accounts before retrying), audit_unavailable or busy"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}},"delete":{"summary":"Admin only: delete an account and end its sessions","description":"Your own account cannot be deleted (cannot_modify_self). Durable and audited (user_deleted).","responses":{"200":{"description":"deleted true","content":{"application/json":{"schema":{"type":"object","required":["deleted"],"properties":{"deleted":{"const":true}}}}}},"404":{"description":"user_not_found"},"409":{"description":"cannot_modify_self or accounts_disabled"},"503":{"description":"accounts_unavailable (nothing changed), accounts_commit_indeterminate (in effect but durability unconfirmed; list accounts before retrying), audit_unavailable or busy"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/admin/users/{id}/password":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string","pattern":"^[a-f0-9]{32}$"}}],"post":{"summary":"Admin only: set a new password for an account","description":"Ends the account's sessions and clears its sign-in lock. must_change_password defaults to true, so the user chooses their own password at next sign-in; share the temporary one out of band. Durable and audited (user_password_reset); passwords are never logged.","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"required":["password"],"properties":{"password":{"type":"string","minLength":15,"maxLength":128},"must_change_password":{"type":"boolean","default":true}}}}}},"responses":{"200":{"description":"Updated account","content":{"application/json":{"schema":{"type":"object","required":["user"],"properties":{"user":{"$ref":"#/components/schemas/UserAccount"}}}}}},"404":{"description":"user_not_found"},"409":{"description":"cannot_modify_self or accounts_disabled"},"422":{"description":"weak_password or invalid_input"},"503":{"description":"accounts_unavailable (nothing changed), accounts_commit_indeterminate (in effect but durability unconfirmed; list accounts before retrying), audit_unavailable or busy"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
    ),
    concat!(
        r##"      "Tenant":{"type":"object","required":["id","name","max_vectors","state"],"properties":{"id":{"type":"string","pattern":"^[a-f0-9]{32}$"},"name":{"type":"string"},"max_vectors":{"type":["integer","null"],"description":"Aggregate live-vector limit across tenant collections; null is unlimited"},"deleted":{"type":"boolean"},"state":{"enum":["active","suspended"]},"suspended_unix_ms":{"type":["integer","null"]},"collections":{"type":"integer","description":"List responses only: collections owned by the tenant"},"vectors":{"type":"integer","description":"List responses only: live vectors across tenant collections"},"users":{"type":"integer","description":"List responses only: dashboard accounts bound to the tenant"}}},"##,
        "\n",
        r##"      "TenantUsage":{"type":"object","required":["tenant","collections","vectors","max_vectors","remaining_vectors","over_quota","credentials"],"properties":{"tenant":{"$ref":"#/components/schemas/Tenant"},"collections":{"type":"array","items":{"type":"object","required":["name","physical_name","vectors"],"properties":{"name":{"type":"string"},"physical_name":{"type":"string","description":"t<tenant-id>__<name>"},"vectors":{"type":"integer"}}}},"vectors":{"type":"integer"},"max_vectors":{"type":["integer","null"]},"remaining_vectors":{"type":["integer","null"],"description":"Saturates at 0; null when unlimited"},"over_quota":{"type":"boolean","description":"True when usage exceeds a lowered limit"},"credentials":{"type":"object","required":["active","expired","revoked"],"properties":{"active":{"type":"integer"},"expired":{"type":"integer"},"revoked":{"type":"integer"}}}}},"##,
        "\n",
        r##"      "CapabilityIdentity":{"type":"object","description":"Returned as identity by GET /v1/capabilities for display only; authorization uses role and scope. Labels never contain secrets.","required":["kind","label"],"properties":{"kind":{"enum":["super_admin","open","read_only","scoped","user"],"description":"open means no admin token is configured and every caller is admin; user means a dashboard password session"},"label":{"type":"string"},"credential_id":{"type":"string"},"credential_name":{"type":"string"},"tenant_id":{"type":["string","null"]},"tenant_name":{"type":["string","null"]},"user_id":{"type":"string"},"username":{"type":"string"},"account_role":{"enum":["super_admin","editor","viewer"]},"must_change_password":{"type":"boolean"}}},"##,
        "\n",
        r##"      "UserAccount":{"type":"object","description":"A dashboard account; password hashes are never returned.","required":["id","username","role","role_label","tenant_id","tenant_name","disabled","must_change_password","created_unix_ms","updated_unix_ms"],"properties":{"id":{"type":"string","pattern":"^[a-f0-9]{32}$"},"username":{"type":"string"},"role":{"enum":["super_admin","editor","viewer"]},"role_label":{"type":"string"},"tenant_id":{"type":["string","null"]},"tenant_name":{"type":["string","null"]},"disabled":{"type":"boolean"},"must_change_password":{"type":"boolean"},"created_unix_ms":{"type":"integer"},"updated_unix_ms":{"type":"integer"},"active_sessions":{"type":"integer","description":"List responses only"},"last_login_unix_ms":{"type":["integer","null"],"description":"List responses only; since the server started"},"graphs":{"type":["array","null"],"maxItems":64,"items":{"type":"string","pattern":"^(\\*|[a-z][a-z0-9_-]{0,62})$"},"description":"Experimental graph builds only. Stored knob: graphs the account may use: null = role default (every graph in its tenant scope), [] = none, or 1-64 names (or [\"*\"]). Tenanted accounts use tenant-local names of at most 28 bytes."},"graph_access":{"enum":["none","read","write",null],"description":"Experimental graph builds only. Stored knob that caps graph operations below the role default (editor write, viewer read); null = role default."}}},"##,
        "\n",
        r##"      "AuthSession":{"type":"object","required":["method","role","user","expires_unix_ms","idle_timeout_seconds","must_change_password","accounts","setup_required"],"properties":{"method":{"enum":["password","token","open"]},"role":{"enum":["admin","read_only","scoped"]},"user":{"oneOf":[{"$ref":"#/components/schemas/UserAccount"},{"type":"null"}]},"expires_unix_ms":{"type":["integer","null"],"description":"Absolute session end; null for tokens and open mode"},"idle_timeout_seconds":{"type":["integer","null"]},"must_change_password":{"type":"boolean"},"accounts":{"type":"boolean","description":"Password sign-in is configured"},"setup_required":{"type":"boolean","description":"No accounts exist yet"}}},"##,
        "\n",
        r##"      "AuthSignedIn":{"type":"object","required":["user","expires_unix_ms","idle_timeout_seconds","must_change_password"],"properties":{"user":{"$ref":"#/components/schemas/UserAccount"},"expires_unix_ms":{"type":"integer"},"idle_timeout_seconds":{"type":"integer"},"must_change_password":{"type":"boolean","description":"While true, everything except /v1/auth/session, /v1/auth/logout, /v1/auth/password and GET /v1/capabilities returns 403 password_change_required"}}},"##,
        "\n",
    ),
];

/// `/openapi.json` (`info.version`) and the MCP `initialize` result
/// (`serverInfo.version`) report the package version, which release
/// preparation bumps. That one value is masked on both sides and the served
/// one must equal `CARGO_PKG_VERSION`; every other byte must still match the
/// capture, so a version bump never needs a re-pin.
const MASKED_VERSION: &str = "<package-version>";
const OPENAPI_VERSION_ANCHOR: &str = "\"title\": \"VLite REST API\",\n    \"version\": \"";

fn is_release_version(text: &str) -> bool {
    let core = text.split_once('-').map_or(text, |(core, _)| core);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Masks `info.version`; returns the masked text and the version it held.
fn mask_openapi_version(text: &str) -> Result<(String, String), String> {
    if text.matches(OPENAPI_VERSION_ANCHOR).count() != 1 {
        return Err("OpenAPI info.version must appear exactly once after the title".to_string());
    }
    let start = text.find(OPENAPI_VERSION_ANCHOR).unwrap() + OPENAPI_VERSION_ANCHOR.len();
    let length = text[start..]
        .find('"')
        .ok_or_else(|| "unterminated OpenAPI info.version".to_string())?;
    let version = &text[start..start + length];
    if !is_release_version(version) {
        return Err(format!(
            "OpenAPI info.version {version:?} is not a release version"
        ));
    }
    Ok((
        format!(
            "{}{MASKED_VERSION}{}",
            &text[..start],
            &text[start + length..]
        ),
        version.to_string(),
    ))
}

/// Masks `serverInfo.version` of an `initialize` result and returns it; other
/// responses and non-release values are left untouched (`None`).
fn mask_mcp_version(response: &mut Value) -> Option<String> {
    let slot = response.pointer_mut("/result/serverInfo/version")?;
    let version = slot.as_str()?.to_string();
    if !is_release_version(&version) {
        return None;
    }
    *slot = Value::String(MASKED_VERSION.to_string());
    Some(version)
}

/// Served OpenAPI must equal the baseline once each approved addition (if
/// present, at most once, on whole lines) is removed and the package version
/// is masked. Returns the served package version, or the first differing line
/// on failure.
fn openapi_matches_baseline(
    served: &str,
    golden: &str,
    additions: &[&str],
) -> Result<String, String> {
    let mut remainder = served.to_string();
    for addition in additions {
        match remainder.matches(addition).count() {
            0 => {}
            1 => {
                let start = remainder.find(addition).unwrap();
                if start == 0 || !remainder[..start].ends_with('\n') {
                    return Err("approved OpenAPI addition is not on whole lines".to_string());
                }
                remainder.replace_range(start..start + addition.len(), "");
            }
            count => return Err(format!("approved OpenAPI addition appears {count} times")),
        }
    }
    let (remainder, version) = mask_openapi_version(&remainder)?;
    let (golden, _) = mask_openapi_version(golden)?;
    if remainder == golden {
        return Ok(version);
    }
    let line = remainder
        .lines()
        .zip(golden.lines())
        .position(|(a, b)| a != b)
        .map_or_else(|| "length".to_string(), |index| (index + 1).to_string());
    Err(format!(
        "served /openapi.json differs from the baseline capture beyond the approved additions \
         (first difference at line {line} after removing them)"
    ))
}

/// A test-local stand-in for a reviewed addition; it exercises the allowlist
/// mechanism and is never part of `APPROVED_OPENAPI_ADDITIONS`.
const SYNTHETIC_ADDITION: &str = concat!(
    "  \"x-synthetic-addition\": {\n",
    "    \"enabled_by_default\": false\n",
    "  },\n",
);

fn golden_with_synthetic_addition() -> String {
    let golden = fixture("openapi.json");
    let anchor = "  \"servers\": [{\"url\": \"http://127.0.0.1:8080\"}],\n";
    assert_eq!(golden.matches(anchor).count(), 1);
    golden.replacen(anchor, &format!("{anchor}{SYNTHETIC_ADDITION}"), 1)
}

#[test]
fn reg008_openapi_allowlist_accepts_baseline_and_approved_additions() {
    let golden = fixture("openapi.json");
    openapi_matches_baseline(&golden, &golden, &[]).unwrap();
    openapi_matches_baseline(&golden, &golden, &[SYNTHETIC_ADDITION]).unwrap();
    let extended = golden_with_synthetic_addition();
    serde_json::from_str::<Value>(&extended).expect("synthetic addition keeps valid JSON");
    openapi_matches_baseline(&extended, &golden, &[SYNTHETIC_ADDITION]).unwrap();
    assert!(
        openapi_matches_baseline(&extended, &golden, APPROVED_OPENAPI_ADDITIONS).is_err(),
        "an addition that is not approved must fail REG-008"
    );
}

#[test]
fn reg008_openapi_allowlist_rejects_unapproved_drift() {
    let golden = fixture("openapi.json");
    let extended = golden_with_synthetic_addition();
    let addition = SYNTHETIC_ADDITION;
    // A schema change elsewhere, alongside the approved addition.
    let first_integer = extended.find("\"type\":\"integer\"").unwrap();
    let mut schema_drift = extended.clone();
    schema_drift.replace_range(
        first_integer..first_integer + "\"type\":\"integer\"".len(),
        "\"type\":\"string\"",
    );
    // An edit inside the approved block itself.
    let edited_block = extended.replacen(
        "\"enabled_by_default\": false",
        "\"enabled_by_default\": true",
        1,
    );
    // A different, unapproved additive top-level extension.
    let other_addition = extended.replacen(
        addition,
        &format!("{addition}  \"x-unapproved\": {{}},\n"),
        1,
    );
    // The approved block duplicated.
    let duplicated = extended.replacen(addition, &format!("{addition}{addition}"), 1);
    // A schema change without the approved addition.
    let bare_drift = golden.replacen("\"type\":\"integer\"", "\"type\":\"number\"", 1);
    for (name, served) in [
        ("schema drift", schema_drift),
        ("edited approved block", edited_block),
        ("unapproved addition", other_addition),
        ("duplicated approved block", duplicated),
        ("drift without addition", bare_drift),
    ] {
        assert_ne!(served, extended, "{name}: mutation must apply");
        assert!(
            openapi_matches_baseline(&served, &golden, &[addition]).is_err(),
            "{name}: must fail REG-008"
        );
    }
}

#[test]
fn reg008_package_version_is_masked_but_nothing_else() {
    let golden = fixture("openapi.json");
    let (_, baseline_version) = mask_openapi_version(&golden).unwrap();
    let anchor = format!("{OPENAPI_VERSION_ANCHOR}{baseline_version}\"");
    let with_version = |version: &str| {
        golden.replacen(&anchor, &format!("{OPENAPI_VERSION_ANCHOR}{version}\""), 1)
    };
    let bumped = with_version("9.8.7");
    assert_ne!(bumped, golden, "version bump must apply");
    assert_eq!(
        openapi_matches_baseline(&bumped, &golden, &[]),
        Ok("9.8.7".to_string())
    );
    assert_eq!(
        openapi_matches_baseline(&with_version("0.5.0-rc.1"), &golden, &[]),
        Ok("0.5.0-rc.1".to_string())
    );
    for (name, served) in [
        (
            "description drift",
            bumped.replacen(
                "\"description\": \"Experimental API",
                "\"description\": \"Stable API",
                1,
            ),
        ),
        (
            "schema drift",
            bumped.replacen("\"type\":\"integer\"", "\"type\":\"number\"", 1),
        ),
        ("version anchor duplicated", format!("{bumped}{bumped}")),
        (
            "version anchor missing",
            bumped.replacen("\"title\": \"VLite REST API\",\n", "", 1),
        ),
        ("non-release version", with_version("latest")),
        ("literal mask text", with_version(MASKED_VERSION)),
    ] {
        assert_ne!(served, bumped, "{name}: mutation must apply");
        assert!(
            openapi_matches_baseline(&served, &golden, &[]).is_err(),
            "{name}: must fail REG-008"
        );
    }
}

#[test]
fn reg008_mcp_version_is_masked_but_nothing_else() {
    let transcript: Vec<Value> = serde_json::from_str(&fixture("mcp_read_only.json")).unwrap();
    assert_eq!(
        transcript[0]["id"], 1,
        "response 1 is the initialize result"
    );
    let mut golden = transcript[0].clone();
    let mut bumped = golden.clone();
    bumped["result"]["serverInfo"]["version"] = Value::String("9.8.7".to_string());
    assert_eq!(mask_mcp_version(&mut bumped).as_deref(), Some("9.8.7"));
    assert!(mask_mcp_version(&mut golden).is_some());
    assert_eq!(bumped, golden, "only the version differed");
    let mut not_a_version = transcript[0].clone();
    not_a_version["result"]["serverInfo"]["version"] = Value::String("latest".to_string());
    assert_eq!(mask_mcp_version(&mut not_a_version), None);
    assert_eq!(not_a_version["result"]["serverInfo"]["version"], "latest");
    let mut tools_list = transcript[1].clone();
    assert_eq!(mask_mcp_version(&mut tools_list), None);
    assert_eq!(tools_list, transcript[1]);
}

#[tokio::test]
async fn reg008_served_openapi_matches_baseline_except_approved_additions() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create(directory.path().join("openapi.vlite"), 2).unwrap();
    let response = router(Service::new(db))
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let served = String::from_utf8(bytes.to_vec())
        .unwrap()
        .replace("\r\n", "\n");
    // EPIC-011: graph builds merge whole graph path lines (RD-013); every
    // other served line must still match the baseline.
    #[cfg(feature = "graph")]
    let served: String = served
        .split_inclusive('\n')
        .filter(|line| {
            !line.starts_with("    \"/v1/graphs") && !line.starts_with("    \"/v1/admin/graph/")
        })
        .collect();
    let version = openapi_matches_baseline(
        &served,
        &fixture("openapi.json"),
        APPROVED_OPENAPI_ADDITIONS,
    )
    .unwrap_or_else(|message| panic!("{message}"));
    assert_eq!(version, env!("CARGO_PKG_VERSION"), "served OpenAPI version");
}

fn mcp_transcript(mode: &str) -> Vec<Value> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.put(1, &[0.0, 0.0], b"origin").unwrap();
    db.put(2, &[1.0, 1.0], b"near").unwrap();
    db.put(3, &[2.0, 2.0], b"far").unwrap();
    db.close().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_vlite-mcp"))
        .args(["--db", path.to_str().unwrap(), &format!("--{mode}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(fixture("mcp_requests.jsonl").as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn reg008_mcp_tools_list_and_outputs_match_baseline() {
    for (mode, golden) in [
        ("read-only", "mcp_read_only.json"),
        ("writable", "mcp_writable.json"),
    ] {
        let expected: Vec<Value> = serde_json::from_str(&fixture(golden)).unwrap();
        let actual = mcp_transcript(mode);
        assert_eq!(actual.len(), expected.len(), "{mode}: response count");
        for (actual, expected) in actual.iter().zip(&expected) {
            let (mut actual, mut expected) = (actual.clone(), expected.clone());
            let served_version = mask_mcp_version(&mut actual);
            let golden_version = mask_mcp_version(&mut expected);
            assert_eq!(
                served_version.is_some(),
                golden_version.is_some(),
                "{mode}: MCP response id {} serverInfo.version presence",
                expected["id"]
            );
            if let Some(version) = served_version {
                assert_eq!(
                    version,
                    env!("CARGO_PKG_VERSION"),
                    "{mode}: MCP server version"
                );
            }
            assert_eq!(
                actual, expected,
                "{mode}: MCP response id {} differs from baseline",
                expected["id"]
            );
        }
    }
}
