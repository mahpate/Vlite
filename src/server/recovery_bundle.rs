//! Sanitized recovery bundle captured with each cloud backup set
//! (feature-backup-ha ITEM-003/004).
//!
//! The bundle is the typed recovery configuration: tenants, dashboard account
//! identities and roles, the scoped-credential inventory, and the names of the
//! secrets a restore target must provision. It never holds a secret value, a
//! password or credential hash, a session, a filesystem path or host identity.
//! Import is rekey-only: accounts arrive disabled with unusable passwords,
//! credentials are reported for re-issue (the restored lineage rejects the old
//! ones) and webhook subscriptions are reported for re-creation.
//!
//! Every configuration is certified as one cross-store whole (unique
//! identities, every tenant reference resolves) at capture, parse and import,
//! and the bundle names the database object and the admitted policy it was
//! captured with, so a restore cannot pair it with another set's copy.
use super::*;
use serde::Serialize;
use std::{collections::HashSet, path::Path as FilePath};

pub(super) const SCHEMA: &str = "vlite.recovery-bundle/1";
pub(super) const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;

/// State that is never captured; a restore provisions or re-issues it.
pub(super) const EXCLUDED: [&str; 10] = [
    "sessions",
    "password_hashes",
    "credential_secret_hashes",
    "webhook_urls_and_signing_secrets",
    "bearer_tokens",
    "tls_material",
    "audit_log",
    "provider_configuration",
    "filesystem_paths",
    "host_identity",
];

/// Profile refusals: database shapes a cloud backup set cannot yet carry
/// with a complete verify/drill/restore path. Returned as `(code, message)`.
pub(super) fn refusal(
    encrypted: bool,
    memory_bindings: bool,
) -> Option<(&'static str, &'static str)> {
    if encrypted {
        return Some((
            "encrypted_cloud_backup_unqualified",
            "cloud backup of an encrypted database is not supported yet: the isolated restore drill has no keyed path and key custody is outside this milestone",
        ));
    }
    if memory_bindings {
        return Some((
            "memory_reservations_present",
            "cloud backup is unavailable while memory reservations are served",
        ));
    }
    None
}

/// One secret the restore target must provision. Only the name and whether
/// the source had it configured are recorded, never the value or its path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SecretReference {
    pub name: String,
    pub configured: bool,
    pub action: String,
}

/// The captured configuration; compared before and after the database copy.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub tenants: Vec<super::tenants::Tenant>,
    pub accounts: Vec<super::users::RecoveryAccount>,
    /// Credential identities and grants (no secret hash); reported for
    /// re-issue, never restored.
    pub credentials: Vec<super::credentials::RecoveryCredential>,
    pub webhook_subscriptions: usize,
    pub secret_references: Vec<SecretReference>,
    pub excluded: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Bundle {
    pub schema: String,
    pub created_unix_ms: u64,
    pub installation_id: String,
    /// Hex lineage of the captured database, if it has one.
    pub source_lineage: Option<String>,
    /// SHA-256 of the database object captured with this configuration.
    pub database_sha256: String,
    /// The admitted policy the set was captured under; equals the manifest's.
    pub policy: super::cloud_backup::PolicyCapture,
    /// Always true: the restore re-keys every credential and account.
    pub rekey_only: bool,
    pub config: Config,
}

fn reference(name: &str, configured: bool, action: &str) -> SecretReference {
    SecretReference {
        name: name.into(),
        configured,
        action: action.into(),
    }
}

/// Names the server writes into the admin directory. Anything else may be
/// security state this bundle does not carry, so capture refuses rather than
/// silently leaving it behind. `lost+found` is a filesystem artifact.
fn recognized(name: &str, directory: bool) -> bool {
    if directory {
        return matches!(name, "cloud-backup" | "compaction" | "lost+found");
    }
    let generated = |name: &str| {
        let name = name.strip_suffix(".manifest.json").unwrap_or(name);
        admin::generated(name, "backup-") || admin::generated(name, "restore-")
    };
    matches!(
        name,
        "users.json"
            | "tenants.json"
            | "credentials.json"
            | "webhooks.json"
            | "webhooks.json.tmp"
            | "webhooks.cursor"
            | "webhooks.cursor.tmp"
            | "audit.jsonl"
            | "audit.1.jsonl"
            | "next-startup"
    ) || name.starts_with(".tmp")
        || generated(name)
        || name
            .strip_prefix("disk-ann-")
            .and_then(|n| n.strip_suffix(".dann"))
            .is_some_and(|n| n.parse::<u16>().is_ok())
}

/// Refuses an admin directory with an entry the server did not write. Only
/// the count is reported; symbolic links are never recognized.
fn check_directory(directory: &FilePath) -> Result<(), ApiError> {
    let mut unknown = 0usize;
    for entry in std::fs::read_dir(directory).map_err(internal)? {
        let entry = entry.map_err(internal)?;
        let kind = entry.file_type().map_err(internal)?;
        let known = (kind.is_file() || kind.is_dir())
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| recognized(name, kind.is_dir()));
        unknown += usize::from(!known);
    }
    if unknown == 0 {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::CONFLICT,
            "recovery_config_unrecognized",
            format!(
                "the admin directory holds {unknown} entries the server did not write; move them out so no security state is left out of the backup"
            ),
        ))
    }
}

/// Cross-store certification: every entry is well-formed, identities are
/// unique, and every account and credential tenant resolves within the same
/// configuration (deleted tenants stay as tombstones, so they resolve).
/// Messages never echo a value.
pub(super) fn certify(c: &Config) -> Result<(), &'static str> {
    if c.tenants.len() > MAX_ENTRIES
        || c.accounts.len() > MAX_ENTRIES
        || c.credentials.len() > 64 * MAX_ENTRIES
        || c.secret_references.len() > 64
    {
        return Err("the recovery configuration has too many entries");
    }
    if !super::tenants::valid_store(&c.tenants) {
        return Err("a tenant entry is invalid or duplicated");
    }
    let tenants: HashSet<&str> = c.tenants.iter().map(|t| t.id.as_str()).collect();
    let resolves = |t: &Option<String>| t.as_deref().is_none_or(|id| tenants.contains(id));
    let mut usernames = HashSet::new();
    for account in &c.accounts {
        if account.check().is_err() || !usernames.insert(account.username.as_str()) {
            return Err("an account entry is invalid or duplicated");
        }
        if !resolves(&account.tenant_id) {
            return Err("an account names a tenant the configuration does not hold");
        }
    }
    let mut ids = HashSet::new();
    for credential in &c.credentials {
        credential.check()?;
        if !ids.insert(credential.id.as_str()) {
            return Err("a credential ID is duplicated");
        }
        if !resolves(&credential.tenant_id) {
            return Err("a credential names a tenant the configuration does not hold");
        }
    }
    let mut names = HashSet::new();
    for r in &c.secret_references {
        let valid = (1..=64).contains(&r.name.len())
            && r.name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            && r.action.len() <= 256
            && !r.action.chars().any(char::is_control);
        if !valid || !names.insert(r.name.as_str()) {
            return Err("a secret reference is invalid or duplicated");
        }
    }
    if c.excluded.iter().map(String::as_str).ne(EXCLUDED) {
        return Err("the recovery configuration has an unexpected exclusion list");
    }
    Ok(())
}

fn inconsistent(message: &'static str) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "recovery_config_inconsistent",
        format!("the live recovery configuration is inconsistent: {message}"),
    )
}

/// Reads the live admin stores. `now` fixes credential states so two
/// captures of unchanged stores compare equal. The result is certified.
pub(super) fn capture(
    admin: &admin::AdminState,
    webhooks: &webhooks::Dispatcher,
    now: u64,
) -> Result<Config, ApiError> {
    if let Some(directory) = &admin.directory {
        check_directory(directory)?;
    }
    let tenants = match &admin.tenants {
        Some(store) => super::tenants::recovery_inventory(store)?,
        None => Vec::new(),
    };
    let accounts = match &admin.users {
        Some(users) => users.recovery_inventory()?,
        None => Vec::new(),
    };
    let credentials = match &admin.credentials {
        Some(store) => store.recovery_inventory(now)?,
        None => Vec::new(),
    };
    let webhook_subscriptions = webhooks.subscription_count()?;
    let (admin_token, read_only_token) = {
        let auth = admin.auth.lock().map_err(internal)?;
        (auth.admin.is_some(), auth.read_only.is_some())
    };
    let secret_references = vec![
        reference(
            "admin_token",
            admin_token,
            "provision a new administration token on the restore target",
        ),
        reference(
            "read_only_token",
            read_only_token,
            "provision a new read-only token if it is still required",
        ),
        reference(
            "dashboard_passwords",
            !accounts.is_empty(),
            "sign in with the fresh restore admin, then reset and enable each account",
        ),
        reference(
            "scoped_credentials",
            !credentials.is_empty(),
            "issue new credentials; the restored database lineage rejects the old ones",
        ),
        reference(
            "webhook_signing_secrets",
            webhook_subscriptions > 0,
            "recreate webhook subscriptions with new signing secrets",
        ),
    ];
    let config = Config {
        tenants,
        accounts,
        credentials,
        webhook_subscriptions,
        secret_references,
        excluded: EXCLUDED.iter().map(|s| (*s).to_owned()).collect(),
    };
    certify(&config).map_err(inconsistent)?;
    Ok(config)
}

/// The database copy and the configuration must describe the same moment:
/// a change between the two captures fails the set instead of mixing them.
/// Equal reads bracket the copy; they do not serialize it with admin writes,
/// so a change made and reverted between the two reads goes unseen. Import
/// is rekey-only, so such a configuration still cannot grant access.
pub(super) fn stable(before: &Config, after: &Config) -> Result<(), ApiError> {
    let a = serde_json::to_vec(before).map_err(internal)?;
    let b = serde_json::to_vec(after).map_err(internal)?;
    if a == b {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::CONFLICT,
            "recovery_config_unstable",
            "the recovery configuration changed while the backup was captured; retry",
        ))
    }
}

pub(super) fn bundle(
    config: Config,
    installation_id: &str,
    lineage: Option<u64>,
    database_sha256: &str,
    policy: super::cloud_backup::PolicyCapture,
    now: u64,
) -> Bundle {
    Bundle {
        schema: SCHEMA.into(),
        created_unix_ms: now,
        installation_id: installation_id.into(),
        source_lineage: lineage.map(|l| format!("{l:016x}")),
        database_sha256: database_sha256.into(),
        policy,
        rekey_only: true,
        config,
    }
}

/// Parses, validates and certifies a downloaded bundle. Errors are fixed
/// text: no parser diagnostic or bundle value is echoed.
pub(super) fn parse(bytes: &[u8]) -> Result<Bundle, &'static str> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err("the recovery bundle is too large");
    }
    let bundle: Bundle =
        serde_json::from_slice(bytes).map_err(|_| "the recovery bundle is invalid")?;
    if bundle.schema != SCHEMA {
        return Err("the recovery bundle schema is unsupported");
    }
    if !bundle.rekey_only {
        return Err("the recovery bundle is not rekey-only");
    }
    let sha = &bundle.database_sha256;
    if sha.len() != 64 || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("the recovery bundle names no valid database object");
    }
    certify(&bundle.config)?;
    Ok(bundle)
}

/// Rekey-only import into a new, empty admin directory: refuses a directory
/// with any entry at all, writes the accounts (all disabled, unusable
/// passwords, plus the required fresh super admin) and the tenants, re-reads
/// both stores as a server start would, and returns the operator report.
/// Credentials and webhooks are never written.
pub(super) fn import(
    bundle: &Bundle,
    directory: &FilePath,
    fresh_admin: (&str, &str),
) -> std::io::Result<Value> {
    certify(&bundle.config).map_err(std::io::Error::other)?;
    let meta = std::fs::symlink_metadata(directory)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "the restore admin directory must be a real directory",
        ));
    }
    if std::fs::read_dir(directory)?.next().is_some() {
        return Err(std::io::Error::other(
            "the restore admin directory is not empty; use a new, empty directory",
        ));
    }
    let c = &bundle.config;
    let accounts = super::users::rekey_import(directory, &c.accounts, fresh_admin)?;
    let written = super::tenants::recovery_import(directory, &c.tenants)
        .and_then(|()| super::users::verify_rekeyed(directory, fresh_admin));
    if let Err(error) = written {
        let _ = std::fs::remove_file(directory.join("users.json"));
        let _ = std::fs::remove_file(directory.join("tenants.json"));
        return Err(error);
    }
    Ok(json!({
        "schema": bundle.schema,
        "rekey_only": true,
        "source_lineage": bundle.source_lineage,
        "tenants": c.tenants.len(),
        "accounts_imported_disabled": c.accounts.len(),
        "accounts_written": accounts,
        "fresh_admin": fresh_admin.0.trim().to_ascii_lowercase(),
        "credentials_to_reissue": c.credentials,
        "webhook_subscriptions_to_recreate": c.webhook_subscriptions,
        "secret_references": c.secret_references,
        "excluded": c.excluded,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        let tenants: Vec<super::super::tenants::Tenant> = serde_json::from_value(json!([
            {"id": "c".repeat(32), "name": "Contoso", "max_vectors": 10}
        ]))
        .unwrap();
        let accounts: Vec<super::super::users::RecoveryAccount> = serde_json::from_value(json!([
            {"username":"ops","role":"super_admin","disabled":false},
            {"username":"reader","role":"viewer","tenant_id":"c".repeat(32),"disabled":true}
        ]))
        .unwrap();
        Config {
            tenants,
            accounts,
            credentials: serde_json::from_value(json!([{
                "id":"abcdefghijkl","name":"ci","tenant_id":null,"collections":["*"],
                "operations":["read"],"state":"active","expires_unix_ms":99
            }]))
            .unwrap(),
            webhook_subscriptions: 2,
            secret_references: vec![reference("admin_token", true, "provision")],
            excluded: EXCLUDED.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    const DB_SHA: &str = "5c";

    fn sealed(config: Config, lineage: Option<u64>) -> Bundle {
        let policy = super::super::cloud_backup::fake::policy_capture();
        bundle(config, "id", lineage, &DB_SHA.repeat(32), policy, 1)
    }

    fn reparse(value: &Value) -> Result<Bundle, &'static str> {
        parse(&serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn bundle_round_trips_without_secret_material() {
        let b = sealed(sample(), Some(0xfeed));
        let bytes = serde_json::to_vec(&b).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        for forbidden in [
            "$argon2",
            "secret_sha256",
            "\"password\"",
            "\"url\"",
            "\"token\"",
        ] {
            assert!(!text.contains(forbidden), "{forbidden}");
        }
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed.source_lineage.as_deref(), Some("000000000000feed"));
        assert!(parsed.rekey_only);
        assert_eq!(parsed.config.accounts.len(), 2);
        assert_eq!(parsed.database_sha256, DB_SHA.repeat(32));
        assert_eq!(
            parsed.policy,
            super::super::cloud_backup::fake::policy_capture()
        );
    }

    #[test]
    fn parse_rejects_unknown_fields_wrong_schema_and_non_rekey_bundles() {
        let b = sealed(sample(), None);
        let mut value = serde_json::to_value(&b).unwrap();
        value["config"]["accounts"][0]["password"] = json!("$argon2id$...");
        assert!(parse(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut value = serde_json::to_value(&b).unwrap();
        value["schema"] = json!("vlite.recovery-bundle/2");
        assert!(parse(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut value = serde_json::to_value(&b).unwrap();
        value["rekey_only"] = json!(false);
        assert!(parse(&serde_json::to_vec(&value).unwrap()).is_err());
        assert!(parse(&vec![b' '; MAX_BYTES as usize + 1]).is_err());
    }

    #[test]
    fn stability_check_detects_a_changed_configuration() {
        let a = sample();
        let mut b = sample();
        assert!(stable(&a, &b).is_ok());
        b.webhook_subscriptions = 3;
        assert_eq!(stable(&a, &b).unwrap_err().code, "recovery_config_unstable");
    }

    #[test]
    fn import_writes_loadable_rekeyed_stores_and_refuses_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let b = sealed(sample(), Some(7));
        let fresh = ("Restore-Admin", "correct horse battery staple");
        let report = import(&b, dir.path(), fresh).unwrap();
        assert_eq!(report["accounts_imported_disabled"], 2);
        assert_eq!(report["accounts_written"], 3);
        assert_eq!(report["fresh_admin"], "restore-admin");
        assert_eq!(report["credentials_to_reissue"][0]["id"], "abcdefghijkl");
        assert_eq!(report["webhook_subscriptions_to_recreate"], 2);
        assert!(!dir.path().join("credentials.json").exists());
        assert!(!dir.path().join("webhooks.json").exists());
        let tenants = super::super::tenants::Store::load(dir.path()).unwrap();
        assert_eq!(tenants.name(&"c".repeat(32)).as_deref(), Some("Contoso"));
        assert!(import(&b, dir.path(), fresh).is_err());
    }

    #[test]
    fn import_refuses_any_pre_existing_entry() {
        let fresh = ("restore-admin", "correct horse battery staple");
        for existing in [
            "credentials.json",
            "webhooks.json",
            "next-startup",
            "unrelated.txt",
            ".hidden",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(existing), b"{}").unwrap();
            let b = sealed(sample(), None);
            assert!(import(&b, dir.path(), fresh).is_err(), "{existing}");
            assert!(!dir.path().join("users.json").exists());
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        let b = sealed(sample(), None);
        assert!(import(&b, dir.path(), fresh).is_err());
    }

    #[test]
    fn a_failed_import_leaves_the_directory_empty() {
        // An imported account collides with the fresh admin: nothing stays.
        let dir = tempfile::tempdir().unwrap();
        let b = sealed(sample(), None);
        let error = import(&b, dir.path(), ("ops", "correct horse battery staple")).unwrap_err();
        assert!(error.to_string().contains("already used"), "{error}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_reference_to_a_missing_tenant_is_refused_by_parse_and_import() {
        let mut config = sample();
        config.tenants.clear();
        assert_eq!(
            certify(&config).unwrap_err(),
            "an account names a tenant the configuration does not hold"
        );
        let b = sealed(config, None);
        assert!(reparse(&serde_json::to_value(&b).unwrap()).is_err());
        let dir = tempfile::tempdir().unwrap();
        let fresh = ("restore-admin", "correct horse battery staple");
        assert!(import(&b, dir.path(), fresh).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        // The same for a tenant-bound credential.
        let mut config = sample();
        config.credentials[0].tenant_id = Some("d".repeat(32));
        assert_eq!(
            certify(&config).unwrap_err(),
            "a credential names a tenant the configuration does not hold"
        );
        config.credentials[0].tenant_id = Some("c".repeat(32));
        assert!(certify(&config).is_ok());
    }

    #[test]
    fn duplicates_and_malformed_entries_fail_certification() {
        let mut c = sample();
        c.tenants.push(c.tenants[0].clone());
        assert!(certify(&c).is_err());
        let mut c = sample();
        c.accounts.push(c.accounts[0].clone());
        assert!(certify(&c).is_err());
        let mut c = sample();
        c.credentials.push(c.credentials[0].clone());
        assert_eq!(certify(&c).unwrap_err(), "a credential ID is duplicated");
        let mut c = sample();
        c.secret_references.push(c.secret_references[0].clone());
        assert!(certify(&c).is_err());
        let mut c = sample();
        c.excluded.pop();
        assert!(certify(&c).is_err());
        for broken in [
            json!({"operations": []}),
            json!({"operations": ["no_such_operation"]}),
            json!({"state": "resurrected"}),
            json!({"id": "short"}),
            json!({"collections": []}),
            json!({"tenant_id": "not-hex"}),
        ] {
            let mut value = serde_json::to_value(&sample().credentials[0]).unwrap();
            for (k, v) in broken.as_object().unwrap() {
                value[k] = v.clone();
            }
            let mut c = sample();
            c.credentials = vec![serde_json::from_value(value).unwrap()];
            assert!(certify(&c).is_err(), "{broken}");
        }
    }

    #[test]
    fn a_credential_with_unknown_or_secret_fields_is_refused() {
        // RB-03: the credential entry is a closed shape, not free JSON.
        let b = sealed(sample(), None);
        for (field, value) in [
            ("secret_sha256", json!("ab".repeat(32))),
            ("secret", json!("vlite_live_secret")),
            ("lineage", json!("00000000000000aa")),
        ] {
            let mut v = serde_json::to_value(&b).unwrap();
            v["config"]["credentials"][0][field] = value;
            assert!(reparse(&v).is_err(), "{field}");
        }
    }

    #[test]
    fn parse_errors_do_not_echo_bundle_values() {
        // RB-06: no serde diagnostic reaches the caller.
        const MARK: &str = "SYNTHETIC-MARKER-7f3a";
        let b = sealed(sample(), None);
        let mut cases = Vec::new();
        let mut v = serde_json::to_value(&b).unwrap();
        v["config"][MARK] = json!(1);
        cases.push(v);
        let mut v = serde_json::to_value(&b).unwrap();
        v["config"]["accounts"][0]["role"] = json!(MARK);
        cases.push(v);
        let mut v = serde_json::to_value(&b).unwrap();
        v["config"]["credentials"][0]["state"] = json!(MARK);
        cases.push(v);
        let mut v = serde_json::to_value(&b).unwrap();
        v["database_sha256"] = json!(MARK);
        cases.push(v);
        for case in cases {
            let error = reparse(&case).err().unwrap();
            assert!(!error.contains(MARK), "{error}");
        }
        assert!(!parse(MARK.as_bytes()).err().unwrap().contains(MARK));
    }

    #[test]
    fn only_server_written_admin_entries_are_recognized() {
        for name in [
            "users.json",
            "tenants.json",
            "credentials.json",
            "webhooks.json",
            "webhooks.cursor.tmp",
            "audit.1.jsonl",
            "next-startup",
            ".tmpAbC123",
            "backup-1700000000000-1.vlite",
            "backup-1700000000000-1.vlite.manifest.json",
            "restore-1700000000000-2.vlite",
            "disk-ann-7.dann",
        ] {
            assert!(recognized(name, false), "{name}");
        }
        for name in ["cloud-backup", "compaction", "lost+found"] {
            assert!(recognized(name, true), "{name}");
        }
        for name in [
            "unrecognized-policy.json",
            "users.json.bak",
            "disk-ann-x.dann",
            "backup-../x.vlite",
            "tls.key",
        ] {
            assert!(!recognized(name, false), "{name}");
        }
        assert!(!recognized("users.json", true));
        assert!(!recognized("staging", true));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("users.json"), b"[]").unwrap();
        std::fs::create_dir(dir.path().join("cloud-backup")).unwrap();
        assert!(check_directory(dir.path()).is_ok());
        std::fs::write(dir.path().join("unrecognized-policy.json"), b"{}").unwrap();
        let error = check_directory(dir.path()).unwrap_err();
        assert_eq!(error.code, "recovery_config_unrecognized");
        assert!(!error.message.contains("unrecognized-policy"));
    }

    #[test]
    fn refusals_name_unsupported_profiles() {
        assert_eq!(
            refusal(true, false).unwrap().0,
            "encrypted_cloud_backup_unqualified"
        );
        assert_eq!(
            refusal(false, true).unwrap().0,
            "memory_reservations_present"
        );
        assert!(refusal(false, false).is_none());
    }
}
