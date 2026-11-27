//! ITEM-010 seed engine and create-only receipt (DD-001/DD-003), offline.
use super::seed::{self, Encryption, SeedConfig};
use super::*;
use crate::{Database, EncryptionKey};
use std::path::Path;

const OP: &str = "0123456789abcdef0123456789abcdef";
const EMPTY: &[u8] = br#"{"mode":"empty"}"#;

fn config(dimensions: usize) -> SeedConfig {
    SeedConfig::new(dimensions, Encryption::None, EMPTY).unwrap()
}

fn encrypted(dimensions: usize) -> SeedConfig {
    SeedConfig::new(
        dimensions,
        Encryption::SecretReference("tenant-database-key".into()),
        EMPTY,
    )
    .unwrap()
}

fn key(byte: u8) -> EncryptionKey {
    EncryptionKey::from_bytes([byte; 32])
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn database(dir: &Path) -> std::path::PathBuf {
    dir.join(format!("{OP}.vlite"))
}

fn receipt(dir: &Path) -> std::path::PathBuf {
    dir.join(format!("{OP}.seed-receipt.json"))
}

#[test]
fn an_empty_seed_is_pinned_and_receipted() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(3);
    let made = seed::seed(dir.path(), OP, &config, None).unwrap();

    let bytes = std::fs::read(database(dir.path())).unwrap();
    assert_eq!(made.database_digest, sha256_hex(&bytes));
    assert_eq!(made.operation_id, OP);
    assert_eq!(made.configuration_digest, config.digest());
    assert!(is_hex(&made.configuration_digest, 64));
    let db = Database::open(database(dir.path())).unwrap();
    assert_eq!(db.format_version(), 5);
    assert_eq!(db.lineage(), Some(made.lineage));
    assert_ne!(made.lineage, 0);
    assert_eq!(db.dimensions(), 3);
    assert!(!db.has_commit_timestamps());
    assert!(!db.is_encrypted());
    assert!(db.is_empty());
    drop(db);

    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(receipt(dir.path())).unwrap()).unwrap();
    assert_eq!(stored["schema"], "vlite.tenant-seed-receipt/1");
    assert_eq!(stored["operation_id"], OP);
    assert_eq!(stored["configuration_digest"], config.digest());
    assert_eq!(stored["lineage"], made.lineage);
    assert_eq!(stored["database_digest"], made.database_digest);
    assert_eq!(
        made.seed(),
        Seed {
            lineage: made.lineage,
            digest: made.database_digest.clone()
        }
    );
    assert_eq!(seed::verify(dir.path(), OP, &config, None).unwrap(), made);
}

#[test]
fn a_rerun_returns_the_same_receipt_without_rewriting() {
    let dir = tempfile::tempdir().unwrap();
    let first = seed::seed(dir.path(), OP, &config(3), None).unwrap();
    let bytes = std::fs::read(database(dir.path())).unwrap();
    let stored = std::fs::read(receipt(dir.path())).unwrap();
    let again = seed::seed(dir.path(), OP, &config(3), None).unwrap();
    assert_eq!(again, first);
    assert_eq!(std::fs::read(database(dir.path())).unwrap(), bytes);
    assert_eq!(std::fs::read(receipt(dir.path())).unwrap(), stored);
}

#[test]
fn only_the_strict_empty_manifest_is_accepted() {
    for manifest in [
        br#"{"mode":"copy"}"#.as_slice(),
        br#"{"mode":"empty","source":"x"}"#,
        br#"{"mode":"empty","mode":"empty"}"#,
        br#"{}"#,
        br#"[]"#,
        br#"{"mode":"EMPTY"}"#,
        br#"{"dimension":"from-approved-profile"}"#,
        b"",
    ] {
        let error = SeedConfig::new(3, Encryption::None, manifest).unwrap_err();
        assert_eq!(error.code, "seed_configuration_unsupported", "{manifest:?}");
    }
    // Insignificant whitespace is still the same manifest.
    SeedConfig::new(3, Encryption::None, b" { \"mode\" : \"empty\" }\n").unwrap();
}

#[test]
fn unsupported_configuration_refuses_before_side_effects() {
    for dimensions in [0, crate::MAX_DIMENSIONS + 1] {
        let error = SeedConfig::new(dimensions, Encryption::None, EMPTY).unwrap_err();
        assert_eq!(error.code, "seed_configuration_unsupported");
    }
    for reference in ["", " key", "a\nb", &"k".repeat(257)] {
        let error =
            SeedConfig::new(3, Encryption::SecretReference(reference.into()), EMPTY).unwrap_err();
        assert_eq!(
            error.code, "seed_configuration_unsupported",
            "{reference:?}"
        );
    }
    SeedConfig::new(crate::MAX_DIMENSIONS, Encryption::None, EMPTY).unwrap();

    let dir = tempfile::tempdir().unwrap();
    // A key reference with no resolved key, or a key without a reference.
    let error = seed::seed(dir.path(), OP, &encrypted(3), None).unwrap_err();
    assert_eq!(error.code, "seed_key_unavailable");
    let error = seed::seed(dir.path(), OP, &config(3), Some(&key(7))).unwrap_err();
    assert_eq!(error.code, "seed_configuration_unsupported");
    // The staging path is explicit: never created, never escaped.
    let missing = dir.path().join("absent");
    let error = seed::seed(&missing, OP, &config(3), None).unwrap_err();
    assert_eq!(error.code, "seed_staging_unavailable");
    assert!(!missing.exists());
    for id in ["../escape", "0123456789ABCDEF0123456789ABCDEF", "abc"] {
        let error = seed::seed(dir.path(), id, &config(3), None).unwrap_err();
        assert_eq!(error.code, "operation_request_invalid", "{id}");
    }
    assert!(entries(dir.path()).is_empty());
}

#[test]
fn the_configuration_digest_binds_every_choice() {
    let base = config(3).digest();
    assert_ne!(config(4).digest(), base);
    assert_ne!(encrypted(3).digest(), base);
    let other = SeedConfig::new(3, Encryption::SecretReference("other-key".into()), EMPTY)
        .unwrap()
        .digest();
    assert_ne!(other, encrypted(3).digest());
    // Whitespace in the retained manifest is part of its content digest.
    let spaced = SeedConfig::new(3, Encryption::None, b"{ \"mode\":\"empty\" }").unwrap();
    assert_ne!(spaced.digest(), base);
    assert_eq!(config(3).digest(), base);
}

#[test]
fn an_encrypted_seed_needs_its_key_and_never_records_it() {
    let dir = tempfile::tempdir().unwrap();
    let config = encrypted(2);
    let made = seed::seed(dir.path(), OP, &config, Some(&key(7))).unwrap();
    let bytes = std::fs::read(database(dir.path())).unwrap();
    assert_eq!(made.database_digest, sha256_hex(&bytes));
    assert!(Database::open(database(dir.path())).is_err());
    let db = Database::open_encrypted(database(dir.path()), &key(7)).unwrap();
    assert!(db.is_encrypted());
    assert!(!db.has_commit_timestamps());
    assert_eq!(db.lineage(), Some(made.lineage));
    drop(db);

    let stored = std::fs::read(receipt(dir.path())).unwrap();
    assert!(!stored.windows(32).any(|w| w == [7u8; 32]));
    let text = String::from_utf8(stored.clone()).unwrap();
    assert!(!text.contains(&hex(&[7u8; 32])));
    assert!(!text.contains("tenant-database-key"));

    // A wrong or absent key cannot verify; nothing is rewritten or rekeyed.
    for key in [Some(key(8)), None] {
        let error = seed::seed(dir.path(), OP, &config, key.as_ref()).unwrap_err();
        assert!(
            matches!(
                error.code,
                "seed_artifact_unverified" | "seed_key_unavailable"
            ),
            "{}",
            error.code
        );
        assert_eq!(std::fs::read(database(dir.path())).unwrap(), bytes);
        assert_eq!(std::fs::read(receipt(dir.path())).unwrap(), stored);
    }
    let error = seed::verify(dir.path(), OP, &config, Some(&key(8))).unwrap_err();
    assert_eq!(error.code, "seed_artifact_unverified");
    assert_eq!(
        seed::verify(dir.path(), OP, &config, Some(&key(7))).unwrap(),
        made
    );
}

#[test]
fn a_missing_receipt_preserves_the_database_and_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    seed::seed(dir.path(), OP, &config(3), None).unwrap();
    let bytes = std::fs::read(database(dir.path())).unwrap();
    std::fs::remove_file(receipt(dir.path())).unwrap();
    for _ in 0..2 {
        let error = seed::seed(dir.path(), OP, &config(3), None).unwrap_err();
        assert_eq!(error.code, "seed_artifact_unverified");
        assert_eq!(std::fs::read(database(dir.path())).unwrap(), bytes);
        assert!(!receipt(dir.path()).exists());
    }
    let error = seed::verify(dir.path(), OP, &config(3), None).unwrap_err();
    assert_eq!(error.code, "seed_artifact_unverified");
}

#[test]
fn a_stray_or_missing_database_is_never_adopted_or_recreated() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(database(dir.path()), b"not a seed").unwrap();
    let error = seed::seed(dir.path(), OP, &config(3), None).unwrap_err();
    assert_eq!(error.code, "seed_artifact_unverified");
    assert_eq!(std::fs::read(database(dir.path())).unwrap(), b"not a seed");
    assert!(!receipt(dir.path()).exists());

    // A receipt whose database is gone is not re-seeded under it.
    let dir = tempfile::tempdir().unwrap();
    seed::seed(dir.path(), OP, &config(3), None).unwrap();
    let stored = std::fs::read(receipt(dir.path())).unwrap();
    std::fs::remove_file(database(dir.path())).unwrap();
    let error = seed::seed(dir.path(), OP, &config(3), None).unwrap_err();
    assert_eq!(error.code, "seed_artifact_unverified");
    assert!(!database(dir.path()).exists());
    assert_eq!(std::fs::read(receipt(dir.path())).unwrap(), stored);
}

#[test]
fn a_mismatched_or_partial_artifact_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    seed::seed(dir.path(), OP, &config(3), None).unwrap();
    let bytes = std::fs::read(database(dir.path())).unwrap();
    let stored = std::fs::read(receipt(dir.path())).unwrap();

    // Another configuration for the same operation is never re-seeded.
    let error = seed::seed(dir.path(), OP, &config(4), None).unwrap_err();
    assert_eq!(error.code, "seed_artifact_unverified");

    // Changed database bytes.
    let mut changed = bytes.clone();
    changed.push(0);
    std::fs::write(database(dir.path()), &changed).unwrap();
    let error = seed::seed(dir.path(), OP, &config(3), None).unwrap_err();
    assert_eq!(error.code, "seed_artifact_unverified");
    assert_eq!(std::fs::read(database(dir.path())).unwrap(), changed);
    std::fs::write(database(dir.path()), &bytes).unwrap();

    // A truncated, duplicated-key or rebound receipt.
    let text = String::from_utf8(stored.clone()).unwrap();
    let duplicated = text.replacen('{', &format!("{{\"operation_id\":\"{OP}\","), 1);
    let rebound = text.replace(OP, "fedcba9876543210fedcba9876543210");
    let extra = text.replacen('{', "{\"extra\":1,", 1);
    for receipt_bytes in [
        stored[..stored.len() / 2].to_vec(),
        duplicated.into_bytes(),
        rebound.into_bytes(),
        extra.into_bytes(),
    ] {
        std::fs::write(receipt(dir.path()), &receipt_bytes).unwrap();
        let error = seed::seed(dir.path(), OP, &config(3), None).unwrap_err();
        assert_eq!(error.code, "seed_artifact_unverified");
        assert_eq!(std::fs::read(receipt(dir.path())).unwrap(), receipt_bytes);
        assert_eq!(std::fs::read(database(dir.path())).unwrap(), bytes);
    }
    std::fs::write(receipt(dir.path()), &stored).unwrap();
    seed::verify(dir.path(), OP, &config(3), None).unwrap();
}

#[test]
fn seeds_of_different_operations_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let other = "fedcba9876543210fedcba9876543210";
    let a = seed::seed(dir.path(), OP, &config(3), None).unwrap();
    let b = seed::seed(dir.path(), other, &config(3), None).unwrap();
    assert_ne!(a.lineage, b.lineage);
    assert_ne!(a.database_digest, b.database_digest);
    assert_eq!(entries(dir.path()).len(), 4);
}

// ------------------------------------- profile seed source and composition

/// design-tenant-provisioning ITEM-010 offline composition: a create's
/// seed configuration comes from its retained writer profile (DD-001), and
/// `seed::seed_create` seeds it and journals the receipt's seed through
/// `Operations::record_seed`. The key is caller-supplied; no secret
/// reference is resolved here.
mod create {
    use super::*;
    use crate::server::cloud_backup::fake::Memory;
    use serde_json::{Value, json};
    use std::sync::Arc;

    const ADMIN: &str = "admin-aaaaaaaa";
    const IDEMPOTENCY: &str = "onboard-contoso-2026-11-26";
    const ATTEMPT: &str = "worker-a";
    const SECRET: &str = "tenant-db-key@4";
    const WRITER: &str = "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg-1/providers/Microsoft.App/containerApps/tenant-a-writer";

    fn profile(seed_configuration: Option<Value>) -> Vec<u8> {
        let mut document = json!({"resources": [{
            "id": WRITER,
            "ownership": "owned",
            "body": {"location": "westus2", "properties": {
                "environmentId": "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg-1/providers/Microsoft.App/managedEnvironments/vlite-env",
                "configuration": {"activeRevisionsMode": "Single"},
                "template": {
                    "containers": [{"name": "vlite", "image": "example.azurecr.io/vlite@sha256:0000000000000000000000000000000000000000000000000000000000000000", "resources": {"cpu": 0.5, "memory": "1Gi"}}],
                    "scale": {"minReplicas": 1, "maxReplicas": 1}
                }
            }}
        }]});
        if let Some(configuration) = seed_configuration {
            document["seed_configuration"] = configuration;
        }
        serde_json::to_vec(&document).unwrap()
    }

    fn plain(dimensions: usize) -> Value {
        json!({"dimensions": dimensions, "encryption": {"mode": "none"}})
    }

    fn referenced(secret: &str) -> Value {
        json!({"dimensions": 3, "encryption": {"mode": "secret_reference", "secretRef": secret}})
    }

    /// An accepted create retaining `profile` and `manifest`.
    fn accepted(profile: &[u8], manifest: &[u8]) -> (Operations, String) {
        let ops = Operations::new(Arc::new(Memory::default()), "provisioning");
        let payload = Payload::Create {
            name: "contoso".into(),
            max_vectors: Some(1000),
            writer_profile: sha256_hex(profile),
            initialization: sha256_hex(manifest),
            resources: vec![WRITER.into()],
            secret_versions: vec!["archive-sas@3".into(), SECRET.into()],
        };
        let contents = [profile.to_vec(), manifest.to_vec()];
        let intent = ops
            .accept(ADMIN, IDEMPOTENCY, &payload, &contents, 1_700_000_000_000)
            .unwrap()
            .intent;
        (ops, intent.operation_id)
    }

    /// Accepted and claimed by `ATTEMPT`.
    fn claimed(profile: &[u8], manifest: &[u8]) -> (Operations, String) {
        let (ops, op) = accepted(profile, manifest);
        ops.claim(&op, ATTEMPT, None).unwrap();
        (ops, op)
    }

    fn from_profile(profile: &[u8], manifest: &[u8]) -> Result<SeedConfig, ApiError> {
        let (ops, op) = accepted(profile, manifest);
        SeedConfig::from_loaded(&ops.load(&op).unwrap())
    }

    fn files(dir: &Path, op: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        (
            dir.join(format!("{op}.vlite")),
            dir.join(format!("{op}.seed-receipt.json")),
        )
    }

    #[test]
    fn the_retained_profile_is_the_seed_configuration_source() {
        let config = from_profile(&profile(Some(plain(3))), EMPTY).unwrap();
        assert_eq!(config.digest(), self::config(3).digest());
        let config = from_profile(&profile(Some(referenced(SECRET))), EMPTY).unwrap();
        let expected =
            SeedConfig::new(3, Encryption::SecretReference(SECRET.into()), EMPTY).unwrap();
        assert_eq!(config.digest(), expected.digest());
    }

    #[test]
    fn a_missing_or_unsupported_profile_seed_configuration_is_refused() {
        let unsupported = [
            (profile(None), EMPTY.to_vec()),
            (profile(Some(plain(0))), EMPTY.to_vec()),
            (profile(Some(plain(crate::MAX_DIMENSIONS + 1))), EMPTY.to_vec()),
            // The reference must name a secret version the intent retains.
            (profile(Some(referenced("other-key@1"))), EMPTY.to_vec()),
            (profile(Some(referenced("has space"))), EMPTY.to_vec()),
            (profile(Some(plain(3))), br#"{"mode":"copy"}"#.to_vec()),
            (profile(Some(plain(3))), br#"{"mode":"empty","extra":1}"#.to_vec()),
        ];
        for (profile, manifest) in unsupported {
            let error = from_profile(&profile, &manifest).unwrap_err();
            assert_eq!(error.code, "seed_configuration_unsupported", "{}", error.message);
        }
        for configuration in [
            Value::Null,
            json!({"dimensions": 3}),
            json!({"dimensions": 3, "encryption": {"mode": "none"}, "extra": 1}),
            json!({"dimensions": 3, "encryption": {"mode": "none", "secretRef": SECRET}}),
            json!({"dimensions": 3, "encryption": {"mode": "secret_reference"}}),
            json!({"dimensions": 3, "encryption": {"mode": "key_bytes", "key": "00"}}),
        ] {
            let error = from_profile(&profile(Some(configuration.clone())), EMPTY).unwrap_err();
            assert_eq!(error.code, "operation_request_invalid", "{configuration}");
        }
    }

    #[test]
    fn a_create_is_seeded_from_its_profile_and_its_receipt_journaled() {
        let dir = tempfile::tempdir().unwrap();
        let (ops, op) = claimed(&profile(Some(plain(3))), EMPTY);
        let made = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), None).unwrap();
        assert_eq!(made.operation_id, op);
        assert_eq!(made.configuration_digest, config(3).digest());
        assert_eq!(recorded_seed(&ops.journal(&op).unwrap()), Some(made.seed()));

        // A rerun returns the same receipt and changes nothing.
        let (database, _) = files(dir.path(), &op);
        let bytes = std::fs::read(&database).unwrap();
        let journal = ops.journal(&op).unwrap();
        let again = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), None).unwrap();
        assert_eq!(again, made);
        assert_eq!(std::fs::read(&database).unwrap(), bytes);
        assert_eq!(ops.journal(&op).unwrap(), journal);
    }

    #[test]
    fn an_encrypted_create_needs_the_supplied_key() {
        let dir = tempfile::tempdir().unwrap();
        let (ops, op) = claimed(&profile(Some(referenced(SECRET))), EMPTY);
        let error = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), None).unwrap_err();
        assert_eq!(error.code, "seed_key_unavailable");
        assert!(entries(dir.path()).is_empty());
        let made = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), Some(&key(7))).unwrap();
        assert_eq!(recorded_seed(&ops.journal(&op).unwrap()), Some(made.seed()));
        let error = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), Some(&key(8))).unwrap_err();
        assert_eq!(error.code, "seed_artifact_unverified");
    }

    #[test]
    fn a_refused_create_leaves_no_artifact_or_seed() {
        let dir = tempfile::tempdir().unwrap();
        // Unclaimed, then claimed by another attempt.
        let (ops, op) = accepted(&profile(Some(plain(3))), EMPTY);
        let error = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), None).unwrap_err();
        assert_eq!(error.code, "actuator_not_owner");
        ops.claim(&op, "worker-b", None).unwrap();
        let error = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), None).unwrap_err();
        assert_eq!(error.code, "actuator_not_owner");
        assert!(entries(dir.path()).is_empty());
        assert_eq!(recorded_seed(&ops.journal(&op).unwrap()), None);

        // An unsupported profile.
        let (ops, op) = claimed(&profile(None), EMPTY);
        let error = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), None).unwrap_err();
        assert_eq!(error.code, "seed_configuration_unsupported");
        assert!(entries(dir.path()).is_empty());
        assert_eq!(recorded_seed(&ops.journal(&op).unwrap()), None);
    }

    #[test]
    fn a_crash_after_the_durable_receipt_is_journaled_on_rerun() {
        // The worker seeded and wrote the receipt, then crashed before it
        // journaled the seed.
        let dir = tempfile::tempdir().unwrap();
        let (ops, op) = claimed(&profile(Some(plain(3))), EMPTY);
        let receipt = seed::seed(dir.path(), &op, &config(3), None).unwrap();
        assert_eq!(recorded_seed(&ops.journal(&op).unwrap()), None);
        let made = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), None).unwrap();
        assert_eq!(made, receipt);
        assert_eq!(recorded_seed(&ops.journal(&op).unwrap()), Some(receipt.seed()));
    }

    #[test]
    fn a_crash_before_the_durable_receipt_is_preserved_and_refused() {
        // DD-003: a database without a durable receipt is never adopted,
        // re-seeded or journaled; operator recovery is not designed.
        let dir = tempfile::tempdir().unwrap();
        let (ops, op) = claimed(&profile(Some(plain(3))), EMPTY);
        seed::seed(dir.path(), &op, &config(3), None).unwrap();
        let (database, receipt) = files(dir.path(), &op);
        std::fs::remove_file(&receipt).unwrap();
        let bytes = std::fs::read(&database).unwrap();
        for _ in 0..2 {
            let error = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), None).unwrap_err();
            assert_eq!(error.code, "seed_artifact_unverified");
            assert_eq!(std::fs::read(&database).unwrap(), bytes);
            assert!(!receipt.exists());
            assert_eq!(recorded_seed(&ops.journal(&op).unwrap()), None);
        }
    }

    #[test]
    fn a_journaled_seed_is_never_reseeded() {
        let dir = tempfile::tempdir().unwrap();
        let (ops, op) = claimed(&profile(Some(plain(3))), EMPTY);
        let made = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), None).unwrap();
        let journal = ops.journal(&op).unwrap();

        // Lost artifacts, or another staging directory, are refused
        // without creating a replacement database.
        let (database, receipt) = files(dir.path(), &op);
        std::fs::remove_file(&database).unwrap();
        std::fs::remove_file(&receipt).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        for staging in [dir.path(), elsewhere.path()] {
            let error = seed::seed_create(&ops, &op, ATTEMPT, staging, None).unwrap_err();
            assert_eq!(error.code, "seed_artifact_unverified");
            assert!(entries(staging).is_empty());
        }
        assert_eq!(ops.journal(&op).unwrap(), journal);
        assert_eq!(recorded_seed(&journal), Some(made.seed()));
    }

    #[test]
    fn a_seed_step_of_unknown_outcome_is_never_reseeded() {
        // The worker journaled a seed step and crashed before settling it.
        let dir = tempfile::tempdir().unwrap();
        let (ops, op) = claimed(&profile(Some(plain(3))), EMPTY);
        let lost = sha256_hex(b"a seed whose artifacts are lost");
        ops.record(&op, ATTEMPT, &seed_action(&lost)).unwrap();
        let journal = ops.journal(&op).unwrap();
        let error = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), None).unwrap_err();
        assert_eq!(error.code, "seed_artifact_unverified");
        assert!(entries(dir.path()).is_empty());
        assert_eq!(ops.journal(&op).unwrap(), journal);

        // Its own verified artifacts settle it.
        let dir = tempfile::tempdir().unwrap();
        let (ops, op) = claimed(&profile(Some(plain(3))), EMPTY);
        let receipt = seed::seed(dir.path(), &op, &config(3), None).unwrap();
        ops.record(&op, ATTEMPT, &seed_action(&receipt.database_digest)).unwrap();
        let made = seed::seed_create(&ops, &op, ATTEMPT, dir.path(), None).unwrap();
        assert_eq!(made, receipt);
        assert_eq!(recorded_seed(&ops.journal(&op).unwrap()), Some(receipt.seed()));
    }
}
