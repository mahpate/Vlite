mod vector_support;
use vector_support::*;
use vlite::Database;
#[test]
fn checkpointed_batches_keep_old_reads_and_switch_atomically() {
    let f = Fixture::new();
    let mut db = f.db();
    seed(&mut db);
    {
        let mut c = db.collection_mut("default").unwrap();
        c.begin_reembed("text", "v2").unwrap();
        assert!(c.begin_reembed("text", "v3").is_err());
        assert!(c.finish_reembed("text").is_err());
        let pending = c.view().reembed_pending("text", 2).unwrap();
        assert_eq!(pending.iter().map(|x| x.0).collect::<Vec<_>>(), [1, 2]);
        c.apply_reembed_batch("text", &[(1, vec![9.0, 1.0]), (2, vec![8.0, 1.0])])
            .unwrap();
        assert_eq!(
            c.view().search_named("text", None, &[1.0, 1.0], 1).unwrap()[0].id,
            1
        );
        assert_eq!(
            c.view().reembed_status("text").unwrap().unwrap().cursor,
            Some(2)
        );
    }
    db.close().unwrap();
    let mut db = Database::open(&f.path).unwrap();
    db.put(0, &[0.0, 1.0], b"new low ID").unwrap();
    {
        let mut c = db.collection_mut("default").unwrap();
        let pending = c.view().reembed_pending("text", 10).unwrap();
        assert_eq!(pending.iter().map(|x| x.0).collect::<Vec<_>>(), [0, 3, 4]);
        let before = std::fs::metadata(&f.path).unwrap().len();
        assert!(
            c.apply_reembed_batch("text", &[(3, vec![1.0, 2.0]), (4, vec![1.0])])
                .is_err()
        );
        assert_eq!(std::fs::metadata(&f.path).unwrap().len(), before);
        c.apply_reembed_batch(
            "text",
            &[
                (0, vec![20.0, 1.0]),
                (3, vec![7.0, 1.0]),
                (4, vec![6.0, 1.0]),
            ],
        )
        .unwrap();
        c.finish_reembed("text").unwrap();
        assert_eq!(
            c.view().search_named("text", None, &[6.0, 1.0], 1).unwrap()[0].id,
            4
        );
        assert_eq!(
            c.view()
                .search_named("text", Some("v1"), &[1.0, 1.0], 1)
                .unwrap()[0]
                .id,
            1
        );
    }
    db.close().unwrap();
    let db = Database::open(&f.path).unwrap();
    assert_eq!(
        db.collection("default").unwrap().vector_specs().unwrap()["text"].active_version,
        "v2"
    );
    assert!(
        db.collection("default")
            .unwrap()
            .reembed_status("text")
            .unwrap()
            .is_none()
    );
}
#[test]
fn zero_history_protects_staged_target_and_compaction_keeps_checkpoint() {
    let f = Fixture::new();
    let out = Fixture::new();
    let mut db = f.db();
    seed(&mut db);
    {
        let mut c = db.collection_mut("default").unwrap();
        c.prune_vector_history("text", 0).unwrap();
        c.begin_reembed("text", "v2").unwrap();
        c.apply_reembed_batch("text", &[(1, vec![9.0, 1.0])])
            .unwrap();
        assert_eq!(
            c.view().record_vectors(1).unwrap().unwrap().dense["text"].len(),
            2
        );
    }
    db.close().unwrap();
    vlite::maintenance::compact(&f.path, &out.path).unwrap();
    let db = Database::open(&out.path).unwrap();
    let c = db.collection("default").unwrap();
    assert_eq!(c.reembed_status("text").unwrap().unwrap().cursor, Some(1));
    assert_eq!(c.reembed_pending("text", 10).unwrap().len(), 3);
}
#[test]
fn asynchronous_batches_check_source_revisions_and_metadata_requeues_staging() {
    use vlite::{Error, Metadata, vectors::ReembedUpdate};
    let f = Fixture::new();
    let mut db = f.db();
    seed(&mut db);
    db.collection_mut("default")
        .unwrap()
        .begin_reembed("text", "v2")
        .unwrap();
    let first = db
        .collection("default")
        .unwrap()
        .reembed_pending_checked("text", 1)
        .unwrap()
        .remove(0);
    db.update_metadata(first.id, &Metadata::new()).unwrap();
    let mut c = db.collection_mut("default").unwrap();
    assert!(matches!(
        c.apply_reembed_batch_checked(
            "text",
            &[ReembedUpdate {
                id: first.id,
                revision: first.revision,
                vector: vec![8.0, 1.0]
            }]
        ),
        Err(Error::RevisionMismatch { .. })
    ));
    let fresh = c
        .view()
        .reembed_pending_checked("text", 1)
        .unwrap()
        .remove(0);
    c.apply_reembed_batch_checked(
        "text",
        &[ReembedUpdate {
            id: fresh.id,
            revision: fresh.revision,
            vector: vec![8.0, 1.0],
        }],
    )
    .unwrap();
    assert_eq!(c.view().reembed_pending("text", 10).unwrap().len(), 3);
    db.update_metadata(first.id, &Metadata::new()).unwrap();
    assert_eq!(
        db.collection("default")
            .unwrap()
            .reembed_pending("text", 10)
            .unwrap()
            .len(),
        4
    );
    db.close().unwrap();
    let db = Database::open(&f.path).unwrap();
    assert_eq!(
        db.collection("default")
            .unwrap()
            .reembed_pending("text", 10)
            .unwrap()
            .len(),
        4
    );
}
