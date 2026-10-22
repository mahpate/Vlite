use vlite::changes::FeedCursor;
use vlite::{CreateOptions, Database, EncryptionKey, Error};

#[test]
fn in_memory_database_round_trips_through_its_byte_image() {
    let mut db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    db.put(1, &[1.0, 0.0], b"one").unwrap();
    db.put(2, &[0.0, 1.0], b"two").unwrap();
    db.delete(1).unwrap();
    let image = db.to_bytes().unwrap();
    db.close().unwrap();

    let reopened = Database::open_from_bytes(image.clone(), None).unwrap();
    assert_eq!(reopened.len(), 1);
    assert_eq!(reopened.search(&[0.0, 1.0], 1).unwrap()[0].id, 2);

    // A torn image is rejected rather than half-loaded.
    assert!(Database::open_from_bytes(image[..image.len() - 3].to_vec(), None).is_err());
    assert!(Database::open_from_bytes(b"not a database".to_vec(), None).is_err());
}

#[test]
fn in_memory_supports_encryption_feed_and_file_backed_has_no_image() {
    let key = EncryptionKey::from_bytes([9; 32]);
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            encryption: Some(EncryptionKey::from_bytes([9; 32])),
            ..CreateOptions::default()
        },
    )
    .unwrap();
    let start: FeedCursor = db.feed_cursor().unwrap();
    db.put(7, &[1.0, 2.0], b"secret-payload").unwrap();
    let image = db.to_bytes().unwrap();
    assert!(!image.windows(14).any(|w| w == b"secret-payload"));
    let batch = db.changes_after(&start, 10).unwrap();
    assert_eq!(batch.changes.len(), 1);
    drop(db);

    assert!(matches!(
        Database::open_from_bytes(image.clone(), None),
        Err(Error::EncryptionKeyRequired)
    ));
    let reopened = Database::open_from_bytes(image, Some(&key)).unwrap();
    assert_eq!(reopened.get(7).unwrap().unwrap().payload, b"secret-payload");

    let directory = tempfile::tempdir().unwrap();
    let on_disk = Database::create(directory.path().join("d.vlite"), 2).unwrap();
    assert!(on_disk.to_bytes().is_none());
}
