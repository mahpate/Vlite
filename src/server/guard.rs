//! The per-request guard: the only code that turns a name from a request into
//! a collection or graph a handler may use (crate-private).
//!
//! A [`RequestScope`] is built once per request from the authenticated role and
//! the service's reservations (`Service::scope`). It is the only constructor of
//! [`GuardedCollection`] and [`GuardedGraph`], whose fields are private to this
//! module, so a handler that holds one knows that the name it came from passed
//! every check the request is subject to, in this order:
//!
//! 1. the tenant qualification of the name (`t{tenant}__{name}`);
//! 2. the credential's name grant (`403 forbidden_scope`), so an out-of-scope
//!    caller cannot probe which names exist;
//! 3. the reservation of the name by a memory binding, which answers exactly
//!    like a name that does not exist unless the credential holds `raw_access`
//!    on the owning binding;
//! 4. the lookup of the name (`404`);
//! 5. the credential's binding to that collection's ID and lineage (`403`).
//!
//! Handles carry the collection ID. The ID-based accessors of the database
//! (`Database::collection_by_id` and the like) take it, so a handler never
//! resolves a name a second time: resolving the unqualified name again after
//! the tenant-qualified one had been checked was the defect class of the 0.7.0
//! metadata, named-search and sparse-search fixes. `tests/server_name_lookup_scan.rs`
//! fails when a handler outside this module looks a name up by itself.
//!
//! The reservation predicate of the core planners (`Database::resolve_visible`,
//! `write_batch_visible`, `retrieve_visible`) is also this scope's
//! ([`RequestScope::visible`]), so a collection named in a request body is
//! masked by the same rule as one named in the path.

use super::*;
use crate::{
    CollectionInfo, CollectionMut, CollectionRef, CollectionSettings, CollectionSpec,
    SettingsPersistence,
    kv::{KvStore, KvView},
    property_graph::{GraphInfo, GraphMut, GraphRef},
};
use std::sync::{OnceLock, atomic::AtomicBool};

tokio::task_local! {
    /// Set by the request pipeline for the handler it runs, so that the guard
    /// can tell the pipeline when it hid a reserved resource from a credential,
    /// and so that every guard of the request decides with the same grants.
    static HIDDEN: Arc<RequestContext>;
}

/// What the guards of one request share.
#[derive(Default)]
pub(super) struct RequestContext {
    /// Whether the guard hid a reserved resource from the credential.
    hidden: AtomicBool,
    /// The bindings view the request decides with, once it consulted one.
    pinned: OnceLock<reserved::Pin>,
}

/// Runs the handler of one request and reports, next to its output, whether
/// the guard answered a credential's request for a reserved resource it may
/// not reach like a request for a missing one (a hidden attempt). The pipeline
/// counts and audits it, without the name, once the request is done.
pub(super) async fn track_hidden<F: std::future::Future>(future: F) -> (F::Output, bool) {
    let context = Arc::new(RequestContext::default());
    let output = HIDDEN.scope(context.clone(), future).await;
    let flag = &context.hidden;
    (output, flag.load(Ordering::Relaxed))
}

/// The context of the request running on this task, to be carried into the
/// blocking worker that runs a database closure for it.
pub(super) fn current() -> Option<Arc<RequestContext>> {
    HIDDEN.try_with(Arc::clone).ok()
}

/// Runs `operation` as part of the request that owns `context`, so a guard
/// built inside a blocking worker notes into the same request and decides
/// with the same grants.
pub(super) fn within<T>(context: Option<Arc<RequestContext>>, operation: impl FnOnce() -> T) -> T {
    match context {
        Some(context) => HIDDEN.sync_scope(context, operation),
        None => operation(),
    }
}

/// The bindings that grant-dependent decisions use. Inside a request, the
/// view the request first consulted, pinned for the rest of it (every scope,
/// memory route and worker of the request), so one request never decides
/// with two versions of the grants; a request that never consults them pins
/// nothing. Outside a request, the view in force.
pub(super) fn bindings(served: &reserved::Served) -> Arc<reserved::Bindings> {
    HIDDEN
        .try_with(|context| context.pinned.get_or_init(|| served.pin()).bindings())
        .unwrap_or_else(|_| served.current())
}

/// What one request may reach: the authenticated role and the reservations of
/// the service it was made to.
#[derive(Clone)]
pub(super) struct RequestScope {
    role: admin::Role,
    bindings: Arc<reserved::Bindings>,
    /// The pipeline's context, when the request runs under it.
    hidden: Option<Arc<RequestContext>>,
}

impl RequestScope {
    /// The only constructor: the reservations always come from the service,
    /// so a scope without them cannot be written.
    pub(super) fn new(service: &Service, role: admin::Role) -> Self {
        Self {
            role,
            bindings: bindings(&service.memory_bindings),
            hidden: HIDDEN.try_with(Arc::clone).ok(),
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(role: admin::Role, bindings: Arc<reserved::Bindings>) -> Self {
        Self {
            role,
            bindings,
            hidden: HIDDEN.try_with(Arc::clone).ok(),
        }
    }

    /// The answer for a reserved collection the caller may not reach: the one
    /// for a missing collection. A credential that asked is noted.
    pub(super) fn hidden_collection(&self) -> ApiError {
        self.note_hidden();
        Error::CollectionNotFound.into()
    }

    /// Only credentials are noted: the administrator and the read-only token
    /// are not probing. The note carries no name.
    fn note_hidden(&self) {
        if matches!(self.role, admin::Role::Scoped(_))
            && let Some(context) = &self.hidden
        {
            context.hidden.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn role(&self) -> &admin::Role {
        &self.role
    }

    /// The stored form of a name the caller addresses: `t{tenant}__{name}`
    /// for a tenanted principal, the name itself otherwise.
    pub(super) fn qualify(&self, name: &str) -> String {
        tenants::qualify(&self.role, name)
    }

    /// The name a caller addresses for a stored name.
    pub(super) fn unqualify(&self, stored: &str) -> String {
        tenants::unqualify(&self.role, stored)
    }

    /// The credential allowed raw access to reserved resources: an API
    /// credential only. Administrators, read-only tokens and signed-in
    /// sessions never have it.
    fn raw(&self) -> Option<&str> {
        raw_credential(&self.role)
    }

    /// Whether this principal's grant (if it has one) allows `op`.
    pub(super) fn allows(&self, op: credentials::Op) -> bool {
        match &self.role {
            admin::Role::Scoped(grant) => grant.allows(op),
            admin::Role::Admin | admin::Role::ReadOnly => true,
        }
    }

    /// Whether the grant (if any) names the stored collection, before any
    /// lookup. A name outside the grant is `403`, never "not found".
    pub(super) fn names_covered(&self, stored: &str) -> bool {
        match &self.role {
            admin::Role::Scoped(grant) => grant.names(stored),
            admin::Role::Admin | admin::Role::ReadOnly => true,
        }
    }

    /// Whether the stored collection is visible to this principal: not
    /// reserved by a memory binding, or reserved and held raw.
    pub(super) fn collection_visible(&self, stored: &str) -> bool {
        self.bindings
            .reserved()
            .raw_allowed(self.raw(), reserved::Resource::Collection, stored)
    }

    /// Whether the stored graph is visible to this principal.
    pub(super) fn graph_visible(&self, stored: &str) -> bool {
        self.bindings
            .reserved()
            .raw_allowed(self.raw(), reserved::Resource::Graph, stored)
    }

    /// [`Self::collection_visible`] as the predicate the core planners take.
    pub(super) fn visible(&self) -> impl Fn(&str) -> bool + '_ {
        move |stored| self.collection_visible(stored)
    }

    /// Whether this principal may see an entry (a list item, an import job, a
    /// metrics row, a slow-query record) about the collection `stored`, which
    /// has ID `id` in the database of `lineage`: its grant binds that ID, name
    /// and lineage, and no reservation hides the name. Every listing that is
    /// not a name lookup decides with this one rule.
    pub(super) fn entry_visible(&self, lineage: Option<u64>, id: u16, stored: &str) -> bool {
        self.collection_visible(stored)
            && match &self.role {
                admin::Role::Scoped(grant) => grant.covers(lineage, id, stored),
                admin::Role::Admin | admin::Role::ReadOnly => true,
            }
    }

    /// The attach rule the core planners take next to [`Self::visible`]:
    /// whether a record of a collection may be attached to a node of a graph.
    /// A reserved graph takes only its own binding's evidence collection, for
    /// every principal, raw holders included.
    pub(super) fn attachable(&self) -> impl Fn(&str, &str) -> bool + '_ {
        move |graph, collection| {
            self.bindings
                .reserved()
                .attachment_allowed(graph, collection)
        }
    }

    /// Drops the graph entries of a list this principal may not reach.
    pub(super) fn retain_visible_graphs<T>(&self, items: &mut Vec<T>, name: impl Fn(&T) -> &str) {
        self.bindings
            .reserved()
            .retain_visible(self.raw(), reserved::Resource::Graph, items, name);
    }

    /// Drops the attach and detach operations of a change this principal may
    /// not reach, and reports whether the change still has anything to show.
    fn retain_change_operations(&self, kind: &mut crate::changes::ChangeKind) -> bool {
        self.bindings
            .reserved()
            .retain_change_operations(self.raw(), kind)
    }

    /// The one rule for an event of the change feed, whichever transport or
    /// subscriber carries it. Its collection must be named by the grant and not
    /// reserved, its graph (if any) must be covered by the grant and not
    /// reserved, and the attach and detach operations of a reserved collection
    /// are dropped from it. Reports whether anything is left to show. The REST
    /// route, the gRPC streams and the webhook dispatcher all apply this rule,
    /// so a route-level refusal is never the only thing between a tenanted
    /// credential and another namespace's events.
    pub(super) fn retain_feed_change(&self, change: &mut crate::changes::Change) -> bool {
        // Graph lifecycle and batch events carry no collection name; their
        // graph is checked against the graph grant below.
        (change.collection.is_empty() || self.names_covered(&change.collection))
            && self.collection_visible(&change.collection)
            && graph::feed_graph(&change.kind).is_none_or(|name| {
                graph::feed_visible(&self.role, name) && self.graph_visible(name)
            })
            && self.retain_change_operations(&mut change.kind)
    }

    /// The collection a request addresses by name, guarded in the order of
    /// the module documentation. `target` is the name as the caller spelled
    /// it; a tenanted principal reaches its own namespace only.
    pub(super) fn collection(
        &self,
        db: &Database,
        target: &Target,
    ) -> Result<GuardedCollection, ApiError> {
        db.ensure_healthy()?;
        self.guard_collection(db, self.qualify(target.name()))
    }

    /// A collection a request body names (a graph batch, a retrieval pivot or
    /// evidence collection). Such names are checked exactly as stored, never
    /// tenant-qualified: an attachment names a collection, not a graph, so
    /// qualifying it would check a different collection. The checks and their
    /// order are those of [`Self::collection`].
    pub(super) fn body_collection(
        &self,
        db: &Database,
        stored: &str,
    ) -> Result<GuardedCollection, ApiError> {
        self.guard_collection(db, stored.to_owned())
    }

    fn guard_collection(
        &self,
        db: &Database,
        stored: String,
    ) -> Result<GuardedCollection, ApiError> {
        if !self.names_covered(&stored) {
            return Err(forbidden_scope());
        }
        if !self.collection_visible(&stored) {
            return Err(self.hidden_collection());
        }
        let id = db.resolve(&stored)?;
        if let admin::Role::Scoped(grant) = &self.role
            && !grant.covers(db.lineage(), id, &stored)
        {
            return Err(forbidden_scope());
        }
        Ok(GuardedCollection {
            id,
            stored: stored.into_boxed_str(),
        })
    }

    /// Creates a collection under the caller's name. A name this principal
    /// may not see answers like an occupied ordinary name: that collision
    /// cannot be hidden.
    pub(super) fn create_collection(
        &self,
        db: &mut Database,
        name: &str,
        spec: CollectionSpec,
    ) -> Result<CollectionInfo, ApiError> {
        let stored = self.qualify(name);
        let hidden = !self.collection_visible(&stored);
        if hidden {
            self.note_hidden();
        }
        Ok(db.create_collection_occupied(&stored, spec, hidden)?)
    }

    /// The collections this principal may list, named as it addresses them.
    pub(super) fn list_collections(&self, db: &Database) -> Result<Vec<CollectionInfo>, ApiError> {
        let lineage = db.lineage();
        let mut items = db
            .collections()?
            .into_iter()
            .filter(|info| self.entry_visible(lineage, info.id, &info.name))
            .collect::<Vec<_>>();
        for item in &mut items {
            item.name = self.unqualify(&item.name);
        }
        Ok(items)
    }

    /// The graph a request addresses by name. Only the name grant is checked
    /// here, before anything is looked up (`403` for a name outside the
    /// grant); a reserved graph this principal may not reach yields a handle
    /// that behaves like a graph that does not exist, at the stage the route
    /// would have found it missing.
    pub(super) fn guarded_graph(&self, name: &str) -> Result<GuardedGraph, ApiError> {
        let stored = self.qualify(name);
        if matches!(&self.role, admin::Role::Scoped(grant) if !grant.graph_allowed(&stored)) {
            return Err(forbidden_scope());
        }
        let hidden = !self.graph_visible(&stored);
        if hidden {
            self.note_hidden();
        }
        Ok(GuardedGraph {
            stored: stored.into_boxed_str(),
            hidden,
        })
    }

    /// The graphs this principal may list, in the database's order.
    pub(super) fn list_graphs(&self, db: &Database) -> Result<Vec<GraphInfo>, ApiError> {
        let mut graphs = db
            .list_graphs()?
            .into_iter()
            .filter(|graph| match &self.role {
                admin::Role::Scoped(grant) => grant.graph_allowed(&graph.name),
                admin::Role::Admin | admin::Role::ReadOnly => true,
            })
            .collect::<Vec<_>>();
        self.retain_visible_graphs(&mut graphs, |graph| graph.name.as_str());
        Ok(graphs)
    }
}

/// A collection that passed the guard for one request. Only
/// [`RequestScope`] constructs it, and it holds the ID, so nothing downstream
/// looks the name up again. The ID is meaningful for as long as the database
/// lock the guard ran under is held; the accessors check it again.
pub(super) struct GuardedCollection {
    id: u16,
    stored: Box<str>,
}

impl GuardedCollection {
    pub(super) fn id(&self) -> u16 {
        self.id
    }

    /// The stored (tenant-qualified) name.
    pub(super) fn stored(&self) -> &str {
        &self.stored
    }

    pub(super) fn read<'a>(&self, db: &'a Database) -> Result<CollectionRef<'a>, ApiError> {
        Ok(db.collection_by_id(self.id)?)
    }

    pub(super) fn write<'a>(&self, db: &'a mut Database) -> Result<CollectionMut<'a>, ApiError> {
        Ok(db.collection_mut_by_id(self.id)?)
    }

    pub(super) fn kv_view<'a>(&self, db: &'a Database) -> Result<KvView<'a>, ApiError> {
        Ok(db.kv_view_by_id(self.id)?)
    }

    pub(super) fn kv_store<'a>(&self, db: &'a mut Database) -> Result<KvStore<'a>, ApiError> {
        Ok(db.kv_store_by_id(self.id)?)
    }

    pub(super) fn info(&self, db: &Database) -> Result<CollectionInfo, ApiError> {
        db.live_collection(self.id)?;
        Ok(db.info(self.id))
    }

    pub(super) fn drop_collection(&self, db: &mut Database) -> Result<(), ApiError> {
        Ok(db.drop_collection_by_id(self.id)?)
    }

    pub(super) fn set_settings(
        &self,
        db: &mut Database,
        settings: CollectionSettings,
    ) -> Result<SettingsPersistence, ApiError> {
        Ok(db.set_settings_by_id(self.id, settings)?)
    }

    pub(super) fn build_disk_ann(
        &self,
        db: &Database,
        path: &std::path::Path,
        config: crate::HnswConfig,
        metric: crate::DistanceMetric,
    ) -> Result<crate::DiskAnnSource, ApiError> {
        Ok(db.build_disk_ann_by_id(self.id, path, config, metric)?)
    }
}

/// A graph that passed the name guard for one request. A reserved graph this
/// principal may not reach is "hidden": every operation answers as the same
/// operation does for a graph that does not exist.
pub(super) struct GuardedGraph {
    stored: Box<str>,
    hidden: bool,
}

impl GuardedGraph {
    #[cfg(test)]
    pub(super) fn is_hidden(&self) -> bool {
        self.hidden
    }

    pub(super) fn read<'a>(&self, db: &'a Database) -> Result<GraphRef<'a>, ApiError> {
        if self.hidden {
            return Err(Error::GraphNotFound.into());
        }
        Ok(db.graph(&self.stored)?)
    }

    /// A read-only database refuses a write before it looks for the graph,
    /// whether or not the graph is hidden.
    pub(super) fn write<'a>(&self, db: &'a mut Database) -> Result<GraphMut<'a>, ApiError> {
        if self.hidden {
            return Err(if db.read_only {
                Error::ReadOnly
            } else {
                Error::GraphNotFound
            }
            .into());
        }
        Ok(db.graph_mut(&self.stored)?)
    }

    /// A hidden name answers like an occupied ordinary name.
    pub(super) fn create(&self, db: &mut Database) -> Result<GraphInfo, ApiError> {
        if self.hidden {
            return Err(Error::GraphExists.into());
        }
        Ok(db.create_graph(&self.stored)?)
    }

    pub(super) fn drop_graph(&self, db: &mut Database) -> Result<(), ApiError> {
        if self.hidden {
            return Err(Error::GraphNotFound.into());
        }
        Ok(db.drop_graph(&self.stored)?)
    }

    /// The names of the collections with a record attached to the live node
    /// `key`, which deleting the node removes; empty for a graph that is
    /// hidden or has no such node. The caller checks each name itself: it
    /// needs the reserved ones too, to refuse the deletion.
    pub(super) fn attachment_collections(&self, db: &Database, key: &str) -> Vec<String> {
        if self.hidden {
            Vec::new()
        } else {
            db.node_attachment_collections(&self.stored, key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{
        MemoryScope,
        binding::{Binding, BindingSet, BindingState, Grant as BindingGrant, ROOT_TENANT},
    };
    use std::time::Duration;

    const TENANT: &str = "0123456789abcdef0123456789abcdef";
    const RAW: &str = "rawrawrawraw";
    const PLAIN: &str = "plainplainpl";

    fn binding(tenant: &str, name: &str, graph: &str, evidence: &str) -> Binding {
        let scope = MemoryScope {
            tenant: tenant.into(),
            agent: "a".into(),
            user: None,
            session: None,
        };
        Binding {
            binding: name.into(),
            tenant: tenant.into(),
            generation: 1,
            state: BindingState::Active,
            graph: graph.into(),
            evidence_collection: Some(evidence.into()),
            receipt_window_ms: 1,
            grants: vec![
                BindingGrant {
                    principal: RAW.into(),
                    scopes: vec![scope.clone()],
                    raw_access: true,
                    read_scopes: Vec::new(),
                },
                BindingGrant {
                    principal: PLAIN.into(),
                    scopes: vec![scope],
                    raw_access: false,
                    read_scopes: Vec::new(),
                },
            ],
        }
    }

    fn bindings() -> Arc<reserved::Bindings> {
        let set = BindingSet {
            bindings: vec![
                binding(ROOT_TENANT, "notes", "notes_graph", "notes_ev"),
                binding(TENANT, "notes", "notes_graph", "notes_ev"),
            ],
        };
        assert_eq!(set.validate(), Ok(()));
        Arc::new(reserved::Bindings::new(set))
    }

    fn database() -> Database {
        let mut db = Database::create_in_memory(2, crate::CreateOptions::default()).unwrap();
        for prefix in [String::new(), format!("t{TENANT}__")] {
            for name in ["notes_ev", "ordinary"] {
                db.create_collection(
                    &format!("{prefix}{name}"),
                    CollectionSpec {
                        dimensions: 2,
                        settings: CollectionSettings::default(),
                    },
                )
                .unwrap();
            }
            for name in ["notes_graph", "ordinary_graph"] {
                db.create_graph(&format!("{prefix}{name}")).unwrap();
            }
        }
        db
    }

    fn grant(id: &str, tenant: Option<&str>, names: &[&str], db: &Database) -> admin::Role {
        let mut grant = credentials::Grant::for_user(
            id.to_owned(),
            id.to_owned(),
            tenant.map(str::to_owned),
            &[
                credentials::Op::Read,
                credentials::Op::Write,
                credentials::Op::GraphRead,
                credentials::Op::GraphWrite,
            ],
            db.lineage().unwrap(),
            u64::MAX,
        );
        grant.api_credential = true;
        if !names.is_empty() {
            grant.all = false;
            grant.collections = names
                .iter()
                .map(|name| {
                    let stored = grant.qualify(name);
                    let id = db.resolve(&stored).unwrap();
                    (stored, id)
                })
                .collect();
        }
        grant.graphs = vec!["*".to_owned()];
        admin::Role::Scoped(Arc::new(grant))
    }

    fn summary(error: ApiError) -> (u16, &'static str) {
        (error.status.as_u16(), error.code)
    }

    /// Everything a client sees of an error, message included.
    fn answer(error: ApiError) -> (StatusCode, &'static str, String) {
        (error.status, error.code, error.message)
    }

    #[test]
    fn a_reserved_name_answers_like_a_missing_one_unless_the_credential_holds_it_raw() {
        let db = database();
        let bindings = bindings();
        let principals = [
            ("admin", admin::Role::Admin, false),
            ("read_only", admin::Role::ReadOnly, false),
            ("plain", grant(PLAIN, None, &[], &db), false),
            ("raw", grant(RAW, None, &[], &db), true),
            (
                "tenant",
                grant("tenanttenan", Some(TENANT), &[], &db),
                false,
            ),
        ];
        for (actor, role, holds) in principals {
            let scope = RequestScope::for_test(role, bindings.clone());
            let reserved = scope.collection(&db, &Target::Named("notes_ev".into()));
            let missing = scope.collection(&db, &Target::Named("absent_ev".into()));
            if holds {
                assert_eq!(reserved.unwrap().stored(), "notes_ev", "{actor}");
            } else {
                assert_eq!(
                    answer(reserved.err().unwrap()),
                    answer(missing.err().unwrap()),
                    "{actor}"
                );
            }
        }
    }

    #[test]
    fn the_name_grant_is_checked_before_the_reservation_and_the_lookup() {
        let db = database();
        let bindings = bindings();
        // A credential bound to `ordinary` alone: a reserved name and a
        // missing name are both outside its grant, and both answer 403.
        let scope = RequestScope::for_test(grant(PLAIN, None, &["ordinary"], &db), bindings);
        for name in ["notes_ev", "absent_ev", "other"] {
            let answer = scope.collection(&db, &Target::Named(name.into()));
            assert_eq!(summary(answer.err().unwrap()), (403, "forbidden_scope"));
        }
        assert!(
            scope
                .collection(&db, &Target::Named("ordinary".into()))
                .is_ok()
        );
    }

    #[test]
    fn a_tenanted_principal_is_guarded_in_its_own_namespace() {
        let db = database();
        let bindings = bindings();
        let scope = RequestScope::for_test(
            grant("tenanttenan", Some(TENANT), &[], &db),
            bindings.clone(),
        );
        let handle = scope
            .collection(&db, &Target::Named("ordinary".into()))
            .unwrap();
        assert_eq!(handle.stored(), format!("t{TENANT}__ordinary"));
        assert_ne!(Some(handle.id()), db.resolve("ordinary").ok());
        assert_eq!(scope.qualify("ordinary"), format!("t{TENANT}__ordinary"));
        assert_eq!(scope.unqualify(handle.stored()), "ordinary");
        // The tenant's `notes_ev` is reserved by the tenant's own binding.
        let reserved = scope.collection(&db, &Target::Named("notes_ev".into()));
        assert_eq!(
            summary(reserved.err().unwrap()),
            (404, "collection_not_found")
        );
        // A body-named collection is checked exactly as stored: the stored
        // name of the tenant's collection, never its tenant-qualified form.
        assert!(
            scope
                .body_collection(&db, &format!("t{TENANT}__ordinary"))
                .is_ok()
        );
        let root = scope.body_collection(&db, "ordinary");
        assert_eq!(summary(root.err().unwrap()), (403, "forbidden_scope"));
    }

    #[test]
    fn a_handle_acts_on_the_collection_it_was_guarded_for() {
        let mut db = database();
        let bindings = bindings();
        let scope = RequestScope::for_test(grant("tenanttenan", Some(TENANT), &[], &db), bindings);
        let handle = scope
            .collection(&db, &Target::Named("ordinary".into()))
            .unwrap();
        handle
            .write(&mut db)
            .unwrap()
            .put(7, &[1.0, 0.0], b"t")
            .unwrap();
        assert_eq!(handle.read(&db).unwrap().len(), 1);
        let root = db.resolve("ordinary").unwrap();
        assert_eq!(db.state(root).records.len(), 0);
        assert_eq!(
            handle.info(&db).unwrap().name,
            format!("t{TENANT}__ordinary")
        );
    }

    #[test]
    fn a_stale_handle_is_not_found_after_its_collection_is_dropped() {
        let mut db = database();
        let scope = RequestScope::for_test(admin::Role::Admin, bindings());
        let handle = scope
            .collection(&db, &Target::Named("ordinary".into()))
            .unwrap();
        handle.drop_collection(&mut db).unwrap();
        assert_eq!(
            summary(handle.read(&db).err().unwrap()),
            (404, "collection_not_found")
        );
        assert_eq!(
            summary(handle.write(&mut db).err().unwrap()),
            (404, "collection_not_found")
        );
        assert_eq!(
            summary(handle.drop_collection(&mut db).err().unwrap()),
            (404, "collection_not_found")
        );
        assert_eq!(
            summary(handle.info(&db).err().unwrap()),
            (404, "collection_not_found")
        );
    }

    #[test]
    fn creating_under_a_reserved_name_answers_like_an_occupied_name() {
        let mut db = database();
        let bindings = bindings();
        let spec = || CollectionSpec {
            dimensions: 2,
            settings: CollectionSettings::default(),
        };
        let scope = RequestScope::for_test(admin::Role::Admin, bindings.clone());
        let reserved = scope.create_collection(&mut db, "notes_ev", spec());
        let occupied = scope.create_collection(&mut db, "ordinary", spec());
        assert_eq!(
            answer(reserved.err().unwrap()),
            answer(occupied.err().unwrap())
        );
        let raw = RequestScope::for_test(grant(RAW, None, &[], &db), bindings);
        assert_eq!(
            summary(
                raw.create_collection(&mut db, "notes_ev", spec())
                    .err()
                    .unwrap()
            ),
            (409, "collection_exists")
        );
        let made = scope.create_collection(&mut db, "fresh", spec()).unwrap();
        assert_eq!(made.name, "fresh");
    }

    #[test]
    fn lists_drop_unreachable_collections_and_graphs_and_unqualify_names() {
        let db = database();
        let bindings = bindings();
        let names = |scope: &RequestScope| {
            scope
                .list_collections(&db)
                .unwrap()
                .into_iter()
                .map(|info| info.name)
                .collect::<Vec<_>>()
        };
        let graphs = |scope: &RequestScope| {
            scope
                .list_graphs(&db)
                .unwrap()
                .into_iter()
                .map(|info| info.name)
                .collect::<Vec<_>>()
        };
        let admin = RequestScope::for_test(admin::Role::Admin, bindings.clone());
        assert_eq!(
            names(&admin),
            [
                "default",
                "ordinary",
                format!("t{TENANT}__ordinary").as_str()
            ]
        );
        assert_eq!(
            graphs(&admin),
            [
                "ordinary_graph",
                format!("t{TENANT}__ordinary_graph").as_str()
            ]
        );
        let raw = RequestScope::for_test(grant(RAW, None, &[], &db), bindings.clone());
        assert!(names(&raw).contains(&"notes_ev".to_owned()));
        assert!(graphs(&raw).contains(&"notes_graph".to_owned()));
        let tenant = RequestScope::for_test(grant("tenanttenan", Some(TENANT), &[], &db), bindings);
        assert_eq!(names(&tenant), ["ordinary"]);
        // Graph entries keep their stored names; the response unqualifies them.
        assert_eq!(graphs(&tenant), [format!("t{TENANT}__ordinary_graph")]);
    }

    #[test]
    fn a_hidden_graph_behaves_like_a_missing_one_at_every_stage() {
        let mut db = database();
        let bindings = bindings();
        let scope = RequestScope::for_test(admin::Role::Admin, bindings.clone());
        let hidden = scope.guarded_graph("notes_graph").unwrap();
        let missing = scope.guarded_graph("absent_graph").unwrap();
        assert!(hidden.is_hidden() && !missing.is_hidden());
        assert_eq!(
            answer(hidden.read(&db).err().unwrap()),
            answer(missing.read(&db).err().unwrap())
        );
        assert_eq!(
            answer(hidden.drop_graph(&mut db).err().unwrap()),
            answer(missing.drop_graph(&mut db).err().unwrap())
        );
        assert_eq!(
            answer(hidden.write(&mut db).err().unwrap()),
            answer(missing.write(&mut db).err().unwrap())
        );
        // A hidden name answers like an occupied name on create.
        let occupied = scope.guarded_graph("ordinary_graph").unwrap();
        assert_eq!(
            answer(hidden.create(&mut db).err().unwrap()),
            answer(occupied.create(&mut db).err().unwrap())
        );
        // A read-only database refuses a write before it looks for the graph,
        // with the same answer for a hidden and a missing graph.
        db.read_only = true;
        assert_eq!(
            answer(hidden.write(&mut db).err().unwrap()),
            answer(missing.write(&mut db).err().unwrap())
        );
        assert_eq!(
            answer(hidden.write(&mut db).err().unwrap()),
            answer(ApiError::from(Error::ReadOnly))
        );
        let raw = RequestScope::for_test(grant(RAW, None, &[], &db), bindings);
        let own = raw.guarded_graph("notes_graph").unwrap();
        assert!(!own.is_hidden());
        assert!(own.read(&db).is_ok());
    }

    #[test]
    fn a_graph_name_outside_the_grant_is_refused_before_anything_is_looked_up() {
        let db = database();
        let mut role = grant(PLAIN, None, &[], &db);
        if let admin::Role::Scoped(grant) = &mut role {
            let grant = Arc::get_mut(grant).unwrap();
            grant.graphs = vec!["ordinary_graph".to_owned()];
        }
        let scope = RequestScope::for_test(role, bindings());
        for name in ["notes_graph", "absent_graph"] {
            assert_eq!(
                summary(scope.guarded_graph(name).err().unwrap()),
                (403, "forbidden_scope")
            );
        }
        assert!(scope.guarded_graph("ordinary_graph").is_ok());
    }

    #[tokio::test]
    async fn a_credentials_hidden_attempt_is_noted_and_nothing_else_is() {
        let mut db = database();
        let bindings = bindings();
        let plain = grant(PLAIN, None, &[], &db);
        let raw = grant(RAW, None, &[], &db);
        let spec = || CollectionSpec {
            dimensions: 2,
            settings: CollectionSettings::default(),
        };
        // (who, what is named, whether the request is noted as hidden)
        let cases = [
            (
                "plain, reserved collection",
                plain.clone(),
                "notes_ev",
                true,
            ),
            (
                "plain, missing collection",
                plain.clone(),
                "absent_ev",
                false,
            ),
            (
                "plain, ordinary collection",
                plain.clone(),
                "ordinary",
                false,
            ),
            (
                "raw holder, reserved collection",
                raw.clone(),
                "notes_ev",
                false,
            ),
            (
                "administrator, reserved collection",
                admin::Role::Admin,
                "notes_ev",
                false,
            ),
            (
                "read-only token, reserved collection",
                admin::Role::ReadOnly,
                "notes_ev",
                false,
            ),
        ];
        for (who, role, name, expected) in cases {
            let (_, hidden) = track_hidden(async {
                let scope = RequestScope::for_test(role.clone(), bindings.clone());
                let _ = scope.collection(&db, &Target::Named(name.into()));
            })
            .await;
            assert_eq!(hidden, expected, "{who}");
        }
        // Creating under a reserved name and naming a reserved graph count too.
        let (_, hidden) = track_hidden(async {
            let scope = RequestScope::for_test(plain.clone(), bindings.clone());
            let _ = scope.create_collection(&mut db, "notes_ev", spec());
        })
        .await;
        assert!(hidden, "create under a reserved name");
        for (name, expected) in [("notes_graph", true), ("absent_graph", false)] {
            let (_, hidden) = track_hidden(async {
                let scope = RequestScope::for_test(plain.clone(), bindings.clone());
                let _ = scope.guarded_graph(name);
            })
            .await;
            assert_eq!(hidden, expected, "{name}");
        }
        // A listing that drops reserved entries is not an attempt.
        let (_, hidden) = track_hidden(async {
            let scope = RequestScope::for_test(plain.clone(), bindings.clone());
            let _ = scope.list_collections(&db);
            let _ = scope.list_graphs(&db);
        })
        .await;
        assert!(!hidden, "lists");
        // A scope built inside the blocking worker that runs a database
        // closure notes into the request that started the worker.
        let (_, hidden) = track_hidden(async {
            let flag = current();
            let plain = plain.clone();
            let bindings = bindings.clone();
            tokio::task::spawn_blocking(move || {
                within(flag, || {
                    let db = database();
                    let scope = RequestScope::for_test(plain, bindings);
                    let _ = scope.collection(&db, &Target::Named("notes_ev".into()));
                })
            })
            .await
            .unwrap();
        })
        .await;
        assert!(hidden, "blocking worker");
        // Outside any request there is nothing to note into.
        let scope = RequestScope::for_test(plain, bindings);
        assert!(
            scope
                .collection(&db, &Target::Named("notes_ev".into()))
                .is_err()
        );
    }

    /// The test bindings with every `raw_access` revoked: the same bindings
    /// and reservations under other grants.
    fn revoked() -> reserved::Bindings {
        let mut set = bindings().set().clone();
        for binding in &mut set.bindings {
            for grant in &mut binding.grants {
                grant.raw_access = false;
            }
        }
        reserved::Bindings::new(set)
    }

    #[tokio::test]
    async fn a_request_keeps_the_grants_it_first_consulted() {
        let served = Arc::new(reserved::Served::new(bindings(), None));
        let first = served.current();
        track_hidden(async {
            assert!(Arc::ptr_eq(&super::bindings(&served), &first));
            served.install(revoked());
            assert!(
                Arc::ptr_eq(&super::bindings(&served), &first),
                "same request"
            );
            assert!(!served.drain(Duration::ZERO).await, "the request holds it");
            // A blocking worker of the request reuses its pin.
            let context = current();
            let worker = served.clone();
            let seen =
                tokio::task::spawn_blocking(move || within(context, || super::bindings(&worker)))
                    .await
                    .unwrap();
            assert!(Arc::ptr_eq(&seen, &first), "blocking worker");
        })
        .await;
        assert!(served.drain(Duration::ZERO).await, "the request ended");
        let second = served.current();
        assert!(!Arc::ptr_eq(&second, &first));
        track_hidden(async {
            assert!(
                Arc::ptr_eq(&super::bindings(&served), &second),
                "new request"
            );
        })
        .await;
        // Outside any request: the view in force, unpinned.
        assert!(Arc::ptr_eq(&super::bindings(&served), &second));
        served.install(revoked());
        assert!(served.drain(Duration::ZERO).await);
    }

    #[tokio::test]
    async fn a_scope_keeps_its_requests_grants_across_an_install() {
        let db = database();
        let raw = grant(RAW, None, &[], &db);
        let service =
            Service::new(db).with_bindings(reserved::Bindings::new(bindings().set().clone()));
        let named = || Target::Named("notes_ev".into());
        let (kept, hidden) = track_hidden(async {
            let before = RequestScope::new(&service, raw.clone());
            service.memory_bindings.install(revoked());
            let after = RequestScope::new(&service, raw.clone());
            let owner = service.database.read().unwrap();
            let db = owner.as_ref().unwrap();
            [before, after].map(|scope| {
                scope
                    .collection(db, &named())
                    .map(|handle| handle.stored().to_owned())
            })
        })
        .await;
        for (when, result) in ["built before the install", "built after it"]
            .iter()
            .zip(kept)
        {
            assert_eq!(result.unwrap(), "notes_ev", "{when}");
        }
        assert!(!hidden);
        let (missing, hidden) = track_hidden(async {
            let scope = RequestScope::new(&service, raw.clone());
            let owner = service.database.read().unwrap();
            scope
                .collection(owner.as_ref().unwrap(), &named())
                .err()
                .map(summary)
        })
        .await;
        assert_eq!(missing, Some((404, "collection_not_found")), "next request");
        assert!(hidden);
    }

    #[test]
    fn the_visible_predicate_is_the_collection_rule() {
        let db = database();
        let bindings = bindings();
        let plain = RequestScope::for_test(grant(PLAIN, None, &[], &db), bindings.clone());
        let raw = RequestScope::for_test(grant(RAW, None, &[], &db), bindings.clone());
        let visible = plain.visible();
        assert!(!visible("notes_ev") && visible("ordinary") && visible("absent_ev"));
        assert!(raw.visible()("notes_ev"));
        // The core planners answer a hidden body-named collection like a
        // missing one.
        assert_eq!(
            summary(ApiError::from(
                db.resolve_visible("notes_ev", &plain.visible())
                    .unwrap_err()
            )),
            summary(ApiError::from(
                db.resolve_visible("absent_ev", &plain.visible())
                    .unwrap_err()
            ))
        );
        assert!(db.resolve_visible("notes_ev", &raw.visible()).is_ok());
    }

    #[test]
    fn an_entry_needs_the_grants_binding_and_no_reservation() {
        let db = database();
        let bindings = bindings();
        let lineage = db.lineage();
        let tenanted = format!("t{TENANT}__ordinary");
        let tenanted_reserved = format!("t{TENANT}__notes_ev");
        let scope = |role| RequestScope::for_test(role, bindings.clone());
        let admin = scope(admin::Role::Admin);
        let read_only = scope(admin::Role::ReadOnly);
        let raw = scope(grant(RAW, None, &[], &db));
        let plain = scope(grant(PLAIN, None, &[], &db));
        let tenant = scope(grant("tenanttenan", Some(TENANT), &[], &db));
        let bound = scope(grant(PLAIN, None, &["ordinary"], &db));
        let id = |name: &str| db.resolve(name).unwrap();
        // (who, scope, stored name, expected)
        let cases = [
            ("admin, ordinary", &admin, "ordinary", true),
            ("admin, reserved", &admin, "notes_ev", false),
            ("read-only, ordinary", &read_only, "ordinary", true),
            ("read-only, reserved", &read_only, "notes_ev", false),
            ("raw holder, reserved", &raw, "notes_ev", true),
            (
                "raw holder, tenant reserved",
                &raw,
                tenanted_reserved.as_str(),
                true,
            ),
            ("plain, ordinary", &plain, "ordinary", true),
            ("plain, tenant ordinary", &plain, tenanted.as_str(), true),
            ("plain, reserved", &plain, "notes_ev", false),
            (
                "plain, tenant reserved",
                &plain,
                tenanted_reserved.as_str(),
                false,
            ),
            ("tenant, own", &tenant, tenanted.as_str(), true),
            ("tenant, root name", &tenant, "ordinary", false),
            (
                "tenant, own reserved",
                &tenant,
                tenanted_reserved.as_str(),
                false,
            ),
            ("bound, its collection", &bound, "ordinary", true),
            ("bound, another", &bound, tenanted.as_str(), false),
        ];
        for (who, scope, name, expected) in cases {
            assert_eq!(
                scope.entry_visible(lineage, id(name), name),
                expected,
                "{who}"
            );
        }
        // A grant binds the collection's ID and the database's lineage, not
        // the name alone: a recreated collection or another database never
        // matches.
        let ordinary = id("ordinary");
        assert!(!bound.entry_visible(lineage, ordinary + 1, "ordinary"));
        assert!(!bound.entry_visible(None, ordinary, "ordinary"));
        assert!(!bound.entry_visible(lineage.map(|l| l ^ 1), ordinary, "ordinary"));
        assert!(!plain.entry_visible(lineage.map(|l| l ^ 1), ordinary, "ordinary"));
        // The administrator is bound to nothing.
        assert!(admin.entry_visible(None, ordinary, "ordinary"));
    }

    #[test]
    fn a_feed_event_is_kept_only_inside_the_namespace_and_outside_reservations() {
        use crate::changes::{Change, ChangeKind};

        let db = database();
        let bindings = bindings();
        let event = |collection: &str, kind: ChangeKind| Change {
            sequence: 1,
            ordinal: 0,
            collection: collection.to_owned(),
            committed_at_ms: None,
            kind,
        };
        let put = |collection: &str| event(collection, ChangeKind::Delete { id: 1 });
        let graph = |name: &str| {
            event(
                "",
                ChangeKind::GraphCreated {
                    graph: name.to_owned(),
                },
            )
        };
        let tenanted = |name: &str| format!("t{TENANT}__{name}");
        let kept = |scope: &RequestScope, mut change: Change| scope.retain_feed_change(&mut change);
        let scope = |role| RequestScope::for_test(role, bindings.clone());
        let admin = scope(admin::Role::Admin);
        let raw = scope(grant(RAW, None, &[], &db));
        let plain = scope(grant(PLAIN, None, &[], &db));
        let tenant = scope(grant("tenanttenan", Some(TENANT), &[], &db));
        // Reservations hide events from everyone but a raw holder, the
        // administrator included.
        for (who, scope, expected) in [("admin", &admin, false), ("plain", &plain, false)] {
            assert_eq!(kept(scope, put("notes_ev")), expected, "{who} collection");
            assert_eq!(kept(scope, graph("notes_graph")), expected, "{who} graph");
        }
        assert!(kept(&raw, put("notes_ev")) && kept(&raw, graph("notes_graph")));
        for scope in [&admin, &plain, &raw] {
            assert!(kept(scope, put("ordinary")) && kept(scope, graph("ordinary_graph")));
        }
        // A tenanted credential's wildcard stays inside its tenant even when
        // nothing above the handler refused the route: no root collection and
        // no root graph.
        assert!(kept(&tenant, put(&tenanted("ordinary"))));
        assert!(kept(&tenant, graph(&tenanted("ordinary_graph"))));
        assert!(!kept(&tenant, put("ordinary")));
        assert!(!kept(&tenant, graph("ordinary_graph")));
        assert!(!kept(&tenant, put(&tenanted("notes_ev"))));
        assert!(!kept(&tenant, graph(&tenanted("notes_graph"))));
        // A grant limited to named collections sees only those.
        let bound = scope(grant(PLAIN, None, &["ordinary"], &db));
        assert!(kept(&bound, put("ordinary")));
        assert!(!kept(&bound, put(&tenanted("ordinary"))));
    }

    #[test]
    fn the_slow_query_log_and_the_metrics_show_what_the_scope_shows() {
        let db = database();
        let bindings = bindings();
        let lineage = db.lineage().unwrap();
        let tenant_ordinary = format!("t{TENANT}__ordinary");
        let tenant_notes = format!("t{TENANT}__notes_ev");
        let entry = |name: &str| {
            json!({"lineage": format!("{lineage:016x}"),
                "collection_id": db.resolve(name).unwrap(), "collection": name})
        };
        let scope = |role| RequestScope::for_test(role, bindings.clone());
        let admin = scope(admin::Role::Admin);
        let raw = scope(grant(RAW, None, &[], &db));
        let plain = scope(grant(PLAIN, None, &[], &db));
        let tenant = scope(grant("tenanttenan", Some(TENANT), &[], &db));
        let bound = scope(grant(PLAIN, None, &["ordinary"], &db));
        let names = [
            "ordinary",
            "notes_ev",
            tenant_ordinary.as_str(),
            tenant_notes.as_str(),
        ];
        // The slow-query log: root ordinary, root reserved, tenant ordinary,
        // tenant reserved.
        for (who, scope, expected) in [
            ("admin", &admin, [true, false, true, false]),
            ("raw holder", &raw, [true, true, true, true]),
            ("plain", &plain, [true, false, true, false]),
            ("tenant", &tenant, [false, false, true, false]),
            ("bound", &bound, [true, false, false, false]),
        ] {
            let seen = names.map(|name| observability::slow_entry_visible(scope, &entry(name)));
            assert_eq!(seen, expected, "{who}: slow-query entries");
            // The metrics list the same collections.
            let listed: std::collections::BTreeSet<String> =
                observability::collection_metrics(&db, scope)
                    .iter()
                    .map(|row| row["name"].as_str().unwrap().to_owned())
                    .collect();
            for (name, shown) in names.iter().zip(expected) {
                assert_eq!(listed.contains(*name), shown, "{who}: metrics for {name}");
            }
        }
        // An entry that names no collection or no ID is shown to nobody, and
        // a credential bound to an ID and a lineage never sees another's.
        let ordinary = db.resolve("ordinary").unwrap();
        for scope in [&admin, &raw, &plain, &tenant, &bound] {
            for incomplete in [
                json!({"lineage": format!("{lineage:016x}"), "collection": "ordinary"}),
                json!({"lineage": format!("{lineage:016x}"), "collection_id": ordinary}),
                json!({"lineage": format!("{lineage:016x}"), "collection_id": 70000, "collection": "ordinary"}),
            ] {
                assert!(!observability::slow_entry_visible(scope, &incomplete));
            }
        }
        let stale = json!({"lineage": format!("{lineage:016x}"),
            "collection_id": ordinary + 1, "collection": "ordinary"});
        let foreign = json!({"lineage": format!("{:016x}", lineage ^ 1),
            "collection_id": ordinary, "collection": "ordinary"});
        let unlabelled = json!({"collection_id": ordinary, "collection": "ordinary"});
        for scope in [&bound, &plain, &tenant] {
            assert!(!observability::slow_entry_visible(scope, &foreign));
            assert!(!observability::slow_entry_visible(scope, &unlabelled));
        }
        assert!(!observability::slow_entry_visible(&bound, &stale));
    }

    /// The collection guard exactly as 0.7.0 wrote it (`open`): qualify, name
    /// grant, reservation, lookup, then the grant's ID and lineage binding.
    /// It is the reference the scope is compared with.
    fn reference_open(
        db: &Database,
        role: &admin::Role,
        target: &Target,
        bindings: &reserved::Bindings,
    ) -> Result<u16, ApiError> {
        db.ensure_healthy()?;
        let stored = tenants::qualify(role, target.name());
        if let admin::Role::Scoped(grant) = role
            && !grant.names(&stored)
        {
            return Err(forbidden_scope());
        }
        if !bindings.reserved().raw_allowed(
            raw_credential(role),
            reserved::Resource::Collection,
            &stored,
        ) {
            return Err(Error::CollectionNotFound.into());
        }
        let id = db.resolve(&stored)?;
        if let admin::Role::Scoped(grant) = role
            && !grant.covers(db.lineage(), id, &stored)
        {
            return Err(forbidden_scope());
        }
        Ok(id)
    }

    #[test]
    fn the_core_planners_refuse_an_ordinary_record_on_a_reserved_graph() {
        use crate::{Metadata, Mutation, property_graph::GraphOp};

        let ops = |collection: &str| {
            vec![
                GraphOp::CreateNode {
                    key: "n".into(),
                    labels: vec!["Doc".into()],
                    properties: Metadata::new(),
                },
                GraphOp::Vector {
                    collection: collection.into(),
                    mutation: Mutation::Put {
                        id: 1,
                        vector: vec![1.0, 0.0],
                        payload: Vec::new(),
                    },
                },
                GraphOp::Attach {
                    node: "n".into(),
                    collection: collection.into(),
                    record: 1,
                },
            ]
        };
        let mut db = database();
        let bindings = bindings();
        let raw = RequestScope::for_test(grant(RAW, None, &[], &db), bindings.clone());
        let plain = RequestScope::for_test(grant(PLAIN, None, &[], &db), bindings);
        let write = |db: &mut Database, scope: &RequestScope, graph: &str, collection: &str| {
            db.graph_mut(graph).unwrap().write_batch_visible(
                &ops(collection),
                &scope.visible(),
                &scope.attachable(),
            )
        };
        // The raw holder reaches both the graph and the collection, yet an
        // ordinary record does not belong on a reserved graph.
        let error = write(&mut db, &raw, "notes_graph", "ordinary").unwrap_err();
        assert!(
            matches!(error, Error::GraphRecordNotAttachable),
            "{error:?}"
        );
        assert_eq!(db.graph("notes_graph").unwrap().info().node_count, 0);
        write(&mut db, &raw, "notes_graph", "notes_ev").unwrap();
        // A graph nobody reserved takes ordinary records for every principal,
        // and the reserved evidence only for the raw holder.
        write(&mut db, &plain, "ordinary_graph", "ordinary").unwrap();
        let error = write(&mut db, &plain, "ordinary_graph", "notes_ev").unwrap_err();
        assert!(matches!(error, Error::CollectionNotFound), "{error:?}");
        let mut other = database();
        write(&mut other, &raw, "ordinary_graph", "notes_ev").unwrap();
    }

    #[test]
    fn the_scope_agrees_with_the_0_7_0_guard_for_every_principal_and_name() {
        let db = database();
        let bindings = bindings();
        let principals = [
            admin::Role::Admin,
            admin::Role::ReadOnly,
            grant(PLAIN, None, &[], &db),
            grant(RAW, None, &[], &db),
            grant(PLAIN, None, &["ordinary"], &db),
            grant("tenanttenan", Some(TENANT), &[], &db),
            grant(RAW, Some(TENANT), &[], &db),
        ];
        for role in principals {
            let scope = RequestScope::for_test(role.clone(), bindings.clone());
            for target in [
                Target::Legacy,
                Target::Named("ordinary".into()),
                Target::Named("notes_ev".into()),
                Target::Named("absent_ev".into()),
                Target::Named("Not A Name".into()),
                Target::Named(format!("t{TENANT}__notes_ev")),
            ] {
                let free = reference_open(&db, &role, &target, &bindings);
                let guarded = scope.collection(&db, &target);
                match (free, guarded) {
                    (Ok(id), Ok(handle)) => assert_eq!(id, handle.id()),
                    (Err(free), Err(guarded)) => {
                        assert_eq!(summary(free), summary(guarded), "{}", target.name());
                    }
                    (free, guarded) => panic!(
                        "{}: {:?} vs {:?}",
                        target.name(),
                        free.map_err(summary),
                        guarded.map(|handle| handle.id()).map_err(summary)
                    ),
                }
            }
        }
    }
}
