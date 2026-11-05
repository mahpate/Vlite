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
    format,
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

    /// `drop_collection` for an ID the server layer has already resolved.
    pub(crate) fn drop_collection_by_id(&mut self, id: u16) -> Result<()> {
        self.ensure_all_resident()?;
        if self.version < format::V4 {
            return Err(Error::CollectionsRequireUpgrade);
        }
        self.live_collection(id)?;
        if self.state(id).name == "default" {
            return Err(Error::DefaultCollectionImmutable);
        }
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
    use crate::{CollectionSpec, CreateOptions};

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
