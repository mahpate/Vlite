//! Identity-based collection access for the server layer (crate-private).
//!
//! `server::guard` resolves a caller-supplied name exactly once, applies the
//! request's grants and reservations, and hands the handler the collection ID.
//! These accessors take that ID, so no handler looks a name up a second time.
//! A second lookup of the unqualified name after the guard had resolved the
//! tenant-qualified one was the defect fixed in 0.7.0 (named and sparse search,
//! `PATCH .../metadata`). An ID that no longer names a live collection answers
//! `CollectionNotFound`, as a name dropped in between does, and never reaches
//! `Database::state`, which would panic on it.

use crate::{
    CollectionMut, CollectionRef, CollectionSettings, Database, Error, Result, SettingsPersistence,
};

impl Database {
    /// `Ok` when `id` names a live collection of a healthy database.
    pub(crate) fn live_collection(&self, id: u16) -> Result<()> {
        self.ensure_healthy()?;
        if self.collections.contains_key(&id) {
            Ok(())
        } else {
            Err(Error::CollectionNotFound)
        }
    }

    /// `collection` for an ID the server layer has already resolved.
    pub(crate) fn collection_by_id(&self, id: u16) -> Result<CollectionRef<'_>> {
        self.live_collection(id)?;
        Ok(CollectionRef { db: self, id })
    }

    /// `collection_mut` for an ID the server layer has already resolved.
    pub(crate) fn collection_mut_by_id(&mut self, id: u16) -> Result<CollectionMut<'_>> {
        self.live_collection(id)?;
        self.collection_mut_in(id)
    }

    /// `drop_collection` for an ID the server layer has already resolved. It
    /// refuses in the order `drop_collection` does: the database's state and
    /// format, the default collection, then the lookup.
    pub(crate) fn drop_collection_by_id(&mut self, id: u16) -> Result<()> {
        self.ensure_collections_droppable()?;
        let default = self
            .collections
            .get(&id)
            .is_some_and(|state| state.name == "default");
        if default {
            return Err(Error::DefaultCollectionImmutable);
        }
        self.live_collection(id)?;
        self.drop_live_collection(id)
    }

    /// `set_collection_settings` for an ID the server layer has already
    /// resolved.
    pub(crate) fn set_settings_by_id(
        &mut self,
        id: u16,
        settings: CollectionSettings,
    ) -> Result<SettingsPersistence> {
        self.live_collection(id)?;
        self.set_live_collection_settings(id, settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CollectionSpec, CreateOptions, FormatVersion};

    fn database() -> Database {
        let mut db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
        db.create_collection(
            "alpha",
            CollectionSpec {
                dimensions: 2,
                settings: CollectionSettings::default(),
            },
        )
        .unwrap();
        db
    }

    #[test]
    fn a_stale_or_unknown_id_is_not_found_and_never_panics() {
        let mut db = database();
        let alpha = db.resolve("alpha").unwrap();
        assert!(db.collection_by_id(alpha).is_ok());
        db.drop_collection("alpha").unwrap();
        for id in [alpha, 999] {
            assert!(matches!(
                db.collection_by_id(id),
                Err(Error::CollectionNotFound)
            ));
            assert!(matches!(
                db.collection_mut_by_id(id),
                Err(Error::CollectionNotFound)
            ));
            assert!(matches!(
                db.drop_collection_by_id(id),
                Err(Error::CollectionNotFound)
            ));
            assert!(matches!(
                db.set_settings_by_id(id, CollectionSettings::default()),
                Err(Error::CollectionNotFound)
            ));
        }
    }

    #[test]
    fn accessors_act_on_the_collection_the_id_names() {
        let mut db = database();
        let alpha = db.resolve("alpha").unwrap();
        db.collection_mut_by_id(alpha)
            .unwrap()
            .put(1, &[1.0, 0.0], b"a")
            .unwrap();
        assert_eq!(db.collection_by_id(alpha).unwrap().len(), 1);
        assert_eq!(db.len(), 0, "the default collection is untouched");
        assert!(matches!(
            db.drop_collection_by_id(alpha),
            Err(Error::CollectionNotEmpty)
        ));
        let settings = CollectionSettings {
            rerank_multiplier: 7,
            ..CollectionSettings::default()
        };
        db.set_settings_by_id(alpha, settings).unwrap();
        assert_eq!(db.state(alpha).settings.rerank_multiplier, 7);
        assert_eq!(db.state(0).settings.rerank_multiplier, 4);
        db.collection_mut_by_id(alpha).unwrap().delete(1).unwrap();
        db.drop_collection_by_id(alpha).unwrap();
        assert!(matches!(
            db.resolve("alpha"),
            Err(Error::CollectionNotFound)
        ));
    }

    #[test]
    fn the_default_collection_cannot_be_dropped_by_id() {
        let mut db = database();
        assert!(matches!(
            db.drop_collection_by_id(0),
            Err(Error::DefaultCollectionImmutable)
        ));
        assert!(matches!(
            db.drop_collection("default"),
            Err(Error::DefaultCollectionImmutable)
        ));
    }

    /// A drop by name and a drop by ID make the same checks in the same
    /// order, so a caller gets one answer for either.
    #[test]
    fn a_drop_by_id_refuses_in_the_order_a_drop_by_name_does() {
        fn refusal(db: &mut Database, name: &str, id: u16) -> Error {
            let by_name = db.drop_collection(name).expect_err("a drop by name");
            let by_id = db.drop_collection_by_id(id).expect_err("a drop by ID");
            assert_eq!(
                std::mem::discriminant(&by_name),
                std::mem::discriminant(&by_id),
                "{name}: by name `{by_name}`, by ID `{by_id}`"
            );
            by_name
        }
        let mut db = database();
        let alpha = db.resolve("alpha").unwrap();
        db.collection_mut_by_id(alpha)
            .unwrap()
            .put(1, &[1.0, 0.0], b"a")
            .unwrap();
        // The default collection, a collection that holds a record, and none.
        let targets = [("default", 0), ("alpha", alpha), ("absent", 999)];
        assert!(matches!(
            refusal(&mut db, "default", 0),
            Error::DefaultCollectionImmutable
        ));
        assert!(matches!(
            refusal(&mut db, "alpha", alpha),
            Error::CollectionNotEmpty
        ));
        assert!(matches!(
            refusal(&mut db, "absent", 999),
            Error::CollectionNotFound
        ));
        // A database that refuses everything answers before the default
        // collection and the lookup are looked at.
        db.poisoned = true;
        for (name, id) in targets {
            assert!(
                matches!(refusal(&mut db, name, id), Error::Poisoned),
                "{name}"
            );
        }
        // A format without collections answers there too, after the health
        // check.
        let directory = tempfile::tempdir().unwrap();
        let mut v3 =
            Database::create_with_format(directory.path().join("v3.vlite"), 2, FormatVersion::V3)
                .unwrap();
        for (name, id) in targets {
            assert!(
                matches!(refusal(&mut v3, name, id), Error::CollectionsRequireUpgrade),
                "{name}"
            );
        }
        v3.poisoned = true;
        for (name, id) in targets {
            assert!(
                matches!(refusal(&mut v3, name, id), Error::Poisoned),
                "{name}"
            );
        }
    }

    #[test]
    fn a_hidden_name_is_not_found_before_any_lookup() {
        let db = database();
        let all = |_: &str| true;
        let none = |_: &str| false;
        assert!(db.resolve_visible("alpha", &all).is_ok());
        assert!(matches!(
            db.resolve_visible("alpha", &none),
            Err(Error::CollectionNotFound)
        ));
        // A hidden name is masked even when it is malformed, so the answer
        // does not depend on the name's validity.
        assert!(matches!(
            db.resolve_visible("not a name", &none),
            Err(Error::CollectionNotFound)
        ));
        assert!(matches!(
            db.resolve_visible("not a name", &all),
            Err(Error::InvalidCollectionName)
        ));
    }
}
