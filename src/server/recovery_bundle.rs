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
use super::*;
use serde::Serialize;
use std::path::Path as FilePath;

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
    /// Public credential views (no secret hash); reported, never restored.
    pub credentials: Vec<Value>,
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

/// Reads the live admin stores. `now` fixes credential states so two
/// captures of unchanged stores compare equal.
pub(super) fn capture(
    admin: &admin::AdminState,
    webhooks: &webhooks::Dispatcher,
    now: u64,
) -> Result<Config, ApiError> {
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
    Ok(Config {
        tenants,
        accounts,
        credentials,
        webhook_subscriptions,
        secret_references,
        excluded: EXCLUDED.iter().map(|s| (*s).to_owned()).collect(),
    })
}

/// The database copy and the configuration must describe the same moment:
/// a change between the two captures fails the set instead of mixing them.
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
    now: u64,
) -> Bundle {
    Bundle {
        schema: SCHEMA.into(),
        created_unix_ms: now,
        installation_id: installation_id.into(),
        source_lineage: lineage.map(|l| format!("{l:016x}")),
        rekey_only: true,
        config,
    }
}

/// Parses and validates a downloaded bundle.
pub(super) fn parse(bytes: &[u8]) -> Result<Bundle, String> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err("recovery bundle is too large".into());
    }
    let bundle: Bundle =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid recovery bundle: {e}"))?;
    if bundle.schema != SCHEMA {
        return Err("unsupported recovery bundle schema".into());
    }
    if !bundle.rekey_only {
        return Err("recovery bundle is not rekey-only".into());
    }
    let c = &bundle.config;
    if c.tenants.len() > MAX_ENTRIES
        || c.accounts.len() > MAX_ENTRIES
        || c.credentials.len() > 64 * MAX_ENTRIES
    {
        return Err("recovery bundle has too many entries".into());
    }
    for account in &c.accounts {
        account.check()?;
    }
    Ok(bundle)
}

/// Rekey-only import into a new, isolated admin directory: refuses any
/// existing admin store, writes the accounts (all disabled, unusable
/// passwords, plus the optional fresh super admin) and the tenants, and
/// returns the operator report. Credentials and webhooks are never written.
pub(super) fn import(
    bundle: &Bundle,
    directory: &FilePath,
    fresh_admin: Option<(&str, &str)>,
) -> std::io::Result<Value> {
    let meta = std::fs::symlink_metadata(directory)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "the restore admin directory must be a real directory",
        ));
    }
    for name in [
        "users.json",
        "tenants.json",
        "credentials.json",
        "webhooks.json",
        "next-startup",
    ] {
        if std::fs::symlink_metadata(directory.join(name)).is_ok() {
            return Err(std::io::Error::other(format!(
                "the restore admin directory already has {name}; use a new, empty directory"
            )));
        }
    }
    let c = &bundle.config;
    let accounts = super::users::rekey_import(directory, &c.accounts, fresh_admin)?;
    if let Err(error) = super::tenants::recovery_import(directory, &c.tenants) {
        let _ = std::fs::remove_file(directory.join("users.json"));
        return Err(error);
    }
    let reissue: Vec<Value> = c
        .credentials
        .iter()
        .map(|v| json!({"id":v["id"],"name":v["name"],"tenant_id":v["tenant_id"],"state":v["state"]}))
        .collect();
    Ok(json!({
        "schema": bundle.schema,
        "rekey_only": true,
        "source_lineage": bundle.source_lineage,
        "tenants": c.tenants.len(),
        "accounts_imported_disabled": c.accounts.len(),
        "accounts_written": accounts,
        "fresh_admin": fresh_admin.map(|(name, _)| name.trim().to_ascii_lowercase()),
        "credentials_to_reissue": reissue,
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
            credentials: vec![
                json!({"id":"abcdefghijkl","name":"ci","tenant_id":null,"state":"active"}),
            ],
            webhook_subscriptions: 2,
            secret_references: vec![reference("admin_token", true, "provision")],
            excluded: EXCLUDED.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn bundle_round_trips_without_secret_material() {
        let b = bundle(sample(), "0123456789abcdef", Some(0xfeed), 42);
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
    }

    #[test]
    fn parse_rejects_unknown_fields_wrong_schema_and_non_rekey_bundles() {
        let b = bundle(sample(), "id", None, 1);
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
        let b = bundle(sample(), "id", Some(7), 1);
        let report = import(
            &b,
            dir.path(),
            Some(("Restore-Admin", "correct horse battery staple")),
        )
        .unwrap();
        assert_eq!(report["accounts_imported_disabled"], 2);
        assert_eq!(report["accounts_written"], 3);
        assert_eq!(report["fresh_admin"], "restore-admin");
        assert_eq!(report["credentials_to_reissue"][0]["id"], "abcdefghijkl");
        assert_eq!(report["webhook_subscriptions_to_recreate"], 2);
        assert!(!dir.path().join("credentials.json").exists());
        assert!(!dir.path().join("webhooks.json").exists());
        let tenants = super::super::tenants::Store::load(dir.path()).unwrap();
        assert_eq!(tenants.name(&"c".repeat(32)).as_deref(), Some("Contoso"));
        assert!(import(&b, dir.path(), None).is_err());
    }

    #[test]
    fn import_refuses_a_directory_with_existing_admin_state() {
        for existing in ["credentials.json", "webhooks.json", "next-startup"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(existing), b"{}").unwrap();
            let b = bundle(sample(), "id", None, 1);
            assert!(import(&b, dir.path(), None).is_err(), "{existing}");
            assert!(!dir.path().join("users.json").exists());
        }
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
