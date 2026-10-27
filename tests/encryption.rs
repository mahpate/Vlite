use std::fs;
use tempfile::tempdir;
use vlite::maintenance;
use vlite::{
    CreateOptions, Database, EncryptionKey, Error, FormatVersion, KdfParams, Metadata,
    MetadataValue,
};

fn key(byte: u8) -> EncryptionKey {
    EncryptionKey::from_bytes([byte; 32])
}

fn encrypted(path: &std::path::Path, key: EncryptionKey) -> Database {
    Database::create_with_options(
        path,
        3,
        CreateOptions {
            encryption: Some(key),
            ..CreateOptions::default()
        },
    )
    .unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn encrypted_round_trip_hides_plaintext_and_reopens() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("secret.vlite");
    let mut db = encrypted(&path, key(1));
    assert!(db.is_encrypted());
    let key_id = db.encryption_key_id().unwrap();
    db.put_with_metadata(
        7,
        &[1.0, 2.0, 3.0],
        b"top-secret-payload",
        &Metadata::from([(
            "owner".into(),
            MetadataValue::String("alice-confidential".into()),
        )]),
    )
    .unwrap();
    db.put(8, &[0.5, 0.25, 0.125], b"second").unwrap();
    assert!(db.delete(8).unwrap());
    db.close().unwrap();

    let bytes = fs::read(&path).unwrap();
    assert!(!contains(&bytes, b"top-secret-payload"));
    assert!(!contains(&bytes, b"alice-confidential"));

    let db = Database::open_encrypted(&path, &key(1)).unwrap();
    assert_eq!(db.encryption_key_id(), Some(key_id));
    assert_eq!(db.len(), 1);
    assert_eq!(db.get(7).unwrap().unwrap().payload, b"top-secret-payload");
    assert_eq!(db.search(&[1.0, 2.0, 3.0], 1).unwrap()[0].id, 7);
    db.close().unwrap();
    assert_eq!(
        maintenance::inspect_with_key(&path, &key(1))
            .unwrap()
            .commits,
        3
    );
}

#[test]
fn wrong_missing_and_unneeded_keys_give_clear_errors() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("secret.vlite");
    let mut db = encrypted(&path, key(1));
    db.put(1, &[1.0, 0.0, 0.0], b"x").unwrap();
    db.close().unwrap();

    assert!(matches!(
        Database::open(&path),
        Err(Error::EncryptionKeyRequired)
    ));
    assert!(matches!(
        Database::open_encrypted(&path, &key(2)),
        Err(Error::EncryptionKeyMismatch { .. })
    ));
    assert!(matches!(
        maintenance::inspect(&path),
        Err(Error::EncryptionKeyRequired)
    ));
    assert!(matches!(
        maintenance::compact(&path, directory.path().join("c.vlite")),
        Err(Error::EncryptionKeyRequired)
    ));

    let plain = directory.path().join("plain.vlite");
    Database::create_with_format(&plain, 3, FormatVersion::V3)
        .unwrap()
        .close()
        .unwrap();
    assert!(matches!(
        Database::open_encrypted(&plain, &key(1)),
        Err(Error::NotEncrypted)
    ));
}

#[test]
fn tampering_with_ciphertext_prefix_or_footer_is_detected() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("secret.vlite");
    let mut db = encrypted(&path, key(1));
    db.put(1, &[1.0, 0.0, 0.0], b"payload").unwrap();
    db.close().unwrap();
    let original = fs::read(&path).unwrap();
    // Flip each byte after the descriptor; every flip must be rejected
    // (checksum, authentication, or structural error), never accepted.
    for offset in (96..original.len()).step_by(3) {
        let mut damaged = original.clone();
        damaged[offset] ^= 0x01;
        let copy = directory.path().join("damaged.vlite");
        fs::write(&copy, &damaged).unwrap();
        assert!(
            Database::open_encrypted(&copy, &key(1)).is_err(),
            "flip at {offset} accepted"
        );
        fs::remove_file(&copy).unwrap();
    }
}

#[test]
fn authenticated_frame_prefix_cannot_be_rebound_to_other_ciphertext() {
    // Swap the ciphertext of two valid frames while keeping each prefix: the
    // AEAD associated data (prefix) must reject the splice even though each
    // frame's checksum is recomputed by the attacker.
    let directory = tempdir().unwrap();
    let path = directory.path().join("secret.vlite");
    let mut db = encrypted(&path, key(1));
    db.put(1, &[1.0, 0.0, 0.0], b"aaaa").unwrap();
    db.put(2, &[0.0, 1.0, 0.0], b"bbbb").unwrap();
    db.close().unwrap();
    let mut bytes = fs::read(&path).unwrap();
    let frame_len = (bytes.len() - 96) / 2;
    let (first, second) = (96, 96 + frame_len);
    // Exchange the sealed bodies (equal lengths), leaving prefixes/footers.
    let body = 36..frame_len - 16;
    let a: Vec<u8> = bytes[first + body.start..first + body.end].to_vec();
    let b: Vec<u8> = bytes[second + body.start..second + body.end].to_vec();
    bytes[first + body.start..first + body.end].copy_from_slice(&b);
    bytes[second + body.start..second + body.end].copy_from_slice(&a);
    let copy = directory.path().join("spliced.vlite");
    fs::write(&copy, &bytes).unwrap();
    assert!(Database::open_encrypted(&copy, &key(1)).is_err());
}

#[test]
fn passphrase_keys_round_trip_and_reject_wrong_passphrase() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("pass.vlite");
    let params = KdfParams {
        memory_kib: 64,
        iterations: 1,
        parallelism: 1,
    };
    let right = EncryptionKey::from_passphrase_with_params("correct horse", params).unwrap();
    let mut db = encrypted(&path, right.clone());
    db.put(1, &[1.0, 2.0, 3.0], b"p").unwrap();
    db.close().unwrap();
    // Parameters and salt come from the header; only the passphrase matters.
    let reopen = EncryptionKey::from_passphrase_with_params("correct horse", params).unwrap();
    assert_eq!(Database::open_encrypted(&path, &reopen).unwrap().len(), 1);
    let wrong = EncryptionKey::from_passphrase_with_params("battery staple", params).unwrap();
    assert!(matches!(
        Database::open_encrypted(&path, &wrong),
        Err(Error::EncryptionKeyMismatch { .. })
    ));
    // A raw key never unlocks a passphrase-protected file.
    assert!(matches!(
        Database::open_encrypted(&path, &key(9)),
        Err(Error::EncryptionKeyMismatch { .. })
    ));
    assert!(EncryptionKey::from_passphrase_with_params("", params).is_err());
    assert_eq!(format!("{right:?}"), "EncryptionKey(<redacted>)");
}

#[test]
fn key_rotation_is_a_verified_copy_out_and_never_in_place() {
    let directory = tempdir().unwrap();
    let old = directory.path().join("old.vlite");
    let new = directory.path().join("new.vlite");
    let mut db = encrypted(&old, key(1));
    for id in 0..20u64 {
        db.put(id, &[id as f32, 1.0, 0.0], format!("row-{id}").as_bytes())
            .unwrap();
    }
    db.delete(3).unwrap();
    db.close().unwrap();
    let before = fs::read(&old).unwrap();

    let report = maintenance::reencrypt(&old, Some(&key(1)), &new, Some(&key(2))).unwrap();
    assert_eq!(report.commits, 21);
    assert_eq!(fs::read(&old).unwrap(), before, "source must be untouched");
    assert!(matches!(
        Database::open_encrypted(&new, &key(1)),
        Err(Error::EncryptionKeyMismatch { .. })
    ));
    let rotated = Database::open_encrypted(&new, &key(2)).unwrap();
    assert_eq!(rotated.len(), 19);
    assert_eq!(rotated.get(5).unwrap().unwrap().payload, b"row-5");
    assert_ne!(
        rotated.encryption_key_id(),
        Database::open_encrypted(&old, &key(1))
            .unwrap()
            .encryption_key_id()
    );
    // Existing destinations are never replaced.
    assert!(maintenance::reencrypt(&old, Some(&key(1)), &new, Some(&key(2))).is_err());
}

#[test]
fn encrypt_and_decrypt_conversions_preserve_the_log() {
    let directory = tempdir().unwrap();
    let plain = directory.path().join("plain.vlite");
    let secret = directory.path().join("secret.vlite");
    let back = directory.path().join("back.vlite");
    let mut db = Database::create_with_format(&plain, 3, FormatVersion::V5).unwrap();
    db.put(1, &[1.0, 2.0, 3.0], b"alpha").unwrap();
    db.put(2, &[3.0, 2.0, 1.0], b"beta").unwrap();
    db.close().unwrap();
    maintenance::reencrypt(&plain, None, &secret, Some(&key(5))).unwrap();
    assert!(!contains(&fs::read(&secret).unwrap(), b"alpha"));
    maintenance::reencrypt(&secret, Some(&key(5)), &back, None).unwrap();
    let db = Database::open(&back).unwrap();
    assert_eq!(db.len(), 2);
    assert_eq!(db.get(2).unwrap().unwrap().payload, b"beta");
    // Pre-v5 sources cannot be encrypted without upgrading first.
    let v3 = directory.path().join("v3.vlite");
    Database::create_with_format(&v3, 3, FormatVersion::V3)
        .unwrap()
        .close()
        .unwrap();
    assert!(
        maintenance::reencrypt(&v3, None, directory.path().join("x.vlite"), Some(&key(5))).is_err()
    );
}

#[test]
fn backup_restore_compact_and_snapshots_respect_encryption() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("secret.vlite");
    let mut db = encrypted(&path, key(1));
    db.put(1, &[1.0, 0.0, 0.0], b"payload").unwrap();
    db.put(1, &[0.0, 1.0, 0.0], b"payload2").unwrap();
    // Sidecars hold plaintext vectors, so they are refused.
    assert!(matches!(
        db.save_ann_snapshot(directory.path().join("ann.snap")),
        Err(Error::SidecarUnavailable(_))
    ));
    db.close().unwrap();

    let backup = directory.path().join("backup.vlite");
    maintenance::backup_with_key(&path, &backup, &key(1)).unwrap();
    assert_eq!(fs::read(&backup).unwrap(), fs::read(&path).unwrap());
    assert!(maintenance::backup(&path, directory.path().join("b2.vlite")).is_err());
    let restored = directory.path().join("restored.vlite");
    maintenance::restore_with_key(&backup, &restored, &key(1)).unwrap();
    assert_eq!(
        Database::open_encrypted(&restored, &key(1)).unwrap().len(),
        1
    );
    let compacted = directory.path().join("compacted.vlite");
    let report = maintenance::compact_with_key(&path, &compacted, &key(1)).unwrap();
    assert_eq!(report.output.live_records, 1);
    assert!(!contains(&fs::read(&compacted).unwrap(), b"payload2"));
    assert!(maintenance::verify_with_key(&compacted, &key(1)).is_ok());
}

#[test]
fn encryption_requires_v5_and_plain_files_are_unchanged() {
    let directory = tempdir().unwrap();
    let error = Database::create_with_options(
        directory.path().join("v4.vlite"),
        3,
        CreateOptions {
            format: Some(FormatVersion::V4),
            encryption: Some(key(1)),
            ..CreateOptions::default()
        },
    );
    assert!(matches!(error, Err(Error::InvalidEncryptionOptions(_))));

    // A v3 file written today is byte-identical to the legacy layout.
    let path = directory.path().join("plain.vlite");
    let mut db = Database::create_with_format(&path, 3, FormatVersion::V3).unwrap();
    db.put(1, &[1.0, 2.0, 3.0], b"x").unwrap();
    db.close().unwrap();
    let bytes = fs::read(&path).unwrap();
    assert_eq!(&bytes[..8], b"VLITEDB\0");
    assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 3);
    assert_eq!(bytes.len(), 32 + 36 + 12 + 1 + 16);
}

#[test]
fn crash_truncation_of_an_encrypted_log_is_reported_and_repairable() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("secret.vlite");
    let mut db = encrypted(&path, key(1));
    db.put(1, &[1.0, 0.0, 0.0], b"first").unwrap();
    db.put(2, &[0.0, 1.0, 0.0], b"second").unwrap();
    db.close().unwrap();
    let full = fs::read(&path).unwrap();
    let truncated = &full[..full.len() - 10];
    fs::write(&path, truncated).unwrap();
    assert!(matches!(
        Database::open_encrypted(&path, &key(1)),
        Err(Error::IncompleteTail { .. })
    ));
    let report = Database::discard_incomplete_tail_encrypted(&path, &key(1)).unwrap();
    assert_eq!(report.retained_commits, 1);
    assert_eq!(Database::open_encrypted(&path, &key(1)).unwrap().len(), 1);
}
