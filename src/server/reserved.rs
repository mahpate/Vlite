//! Reserved-resource registry and memory binding registry (crate-private).
//!
//! Contract: `docs/projects/roadmap/gap1-adapter-mapping.md` section 6 and
//! `serving-memory-contract.md` section 6. A memory binding reserves its
//! stored graph and evidence collection. Raw routes (graph, record, search,
//! export, feed, attachment) ask [`Reserved`] before any lookup: a reserved
//! resource is reachable only by a credential holding `raw_access: true` on
//! that binding, in addition to the ordinary underlying grants the route
//! already checks. Lists and feeds drop reserved entries the caller cannot
//! reach. An empty registry (no bindings file) answers "not reserved" for
//! everything, so existing behaviour is unchanged.
//!
//! All names here are stored names: tenant-qualified `t{tenant}__{name}` for
//! tenant resources, exactly as `credentials::Grant::qualify` builds them.
#![allow(
    dead_code,
    reason = "interface for the raw-route guards and memory routes, wired by later slices"
)]

use crate::memory::binding::{Binding, BindingSet, ROOT_TENANT, Recorded};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::Duration;

/// The namespace of a stored name: graphs and collections are separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Resource {
    Graph,
    Collection,
}

/// The owner of a reservation that no entry of the file owns: a record in the
/// database that the file does not name (design note 1.4 of the 0.7.1 memory
/// follow-ups). Nobody holds raw access to it.
const UNOWNED: usize = usize::MAX;

/// Resources reserved by memory bindings, and who may reach them raw.
#[derive(Debug, Default)]
pub(super) struct Reserved {
    /// Stored resource to the index of its owning binding.
    owners: BTreeMap<(Resource, String), usize>,
    /// `(binding index, credential ID)` pairs granted `raw_access`.
    raw: BTreeSet<(usize, String)>,
}

impl Reserved {
    fn new(set: &BindingSet) -> Self {
        let mut reserved = Self::default();
        for (index, binding) in set.bindings.iter().enumerate() {
            for (graph, stored) in binding.storage() {
                let resource = if graph {
                    Resource::Graph
                } else {
                    Resource::Collection
                };
                reserved.owners.insert((resource, stored), index);
            }
            for grant in binding.grants.iter().filter(|grant| grant.raw_access) {
                reserved.raw.insert((index, grant.principal.clone()));
            }
        }
        reserved
    }

    /// No binding is configured: every guard is a no-op.
    pub(super) fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    /// Whether `stored` is reserved by a binding (active or retired).
    #[cfg(test)]
    pub(super) fn is_reserved(&self, resource: Resource, stored: &str) -> bool {
        self.owner(resource, stored).is_some()
    }

    /// Whether a raw route may touch `stored` for `credential`: always for an
    /// unreserved resource; for a reserved one only when `credential` is an
    /// API credential ID granted `raw_access` on the owning binding. `None`
    /// (admin token, local admin mode or a signed-in user session) never has
    /// raw access to a reserved resource. Callers still apply the route's
    /// ordinary grants; this check never widens them.
    pub(super) fn raw_allowed(
        &self,
        credential: Option<&str>,
        resource: Resource,
        stored: &str,
    ) -> bool {
        match self.owner(resource, stored) {
            None => true,
            Some(index) => credential.is_some_and(|id| self.raw.contains(&(index, id.to_owned()))),
        }
    }

    /// Drops the entries of a list or feed page that `credential` may not
    /// reach raw, keeping order. Cursors advance over the unfiltered page.
    pub(super) fn retain_visible<T>(
        &self,
        credential: Option<&str>,
        resource: Resource,
        items: &mut Vec<T>,
        name: impl Fn(&T) -> &str,
    ) {
        if !self.is_empty() {
            items.retain(|item| self.raw_allowed(credential, resource, name(item)));
        }
    }

    /// Drops the attach and detach operations of a graph batch that name a
    /// collection `credential` may not reach raw, and reports whether the
    /// change still has anything to show.
    pub(super) fn retain_change_operations(
        &self,
        credential: Option<&str>,
        kind: &mut crate::changes::ChangeKind,
    ) -> bool {
        use crate::{changes::ChangeKind, property_graph::GraphChange};
        if self.is_empty() {
            return true;
        }
        if let ChangeKind::GraphBatch { operations, .. } = kind {
            operations.retain(|operation| match operation {
                GraphChange::Attached { collection, .. }
                | GraphChange::Detached { collection, .. } => {
                    self.raw_allowed(credential, Resource::Collection, collection)
                }
                _ => true,
            });
            return !operations.is_empty();
        }
        true
    }

    /// Whether a record of `collection` may be attached to a node of `graph`
    /// through a public route. A reserved graph takes only records of the
    /// collection its own binding reserves; any other graph takes any
    /// collection (a reserved collection is masked by [`Self::raw_allowed`]).
    /// Memory writes do not go through this rule.
    pub(super) fn attachment_allowed(&self, graph: &str, collection: &str) -> bool {
        match self.owner(Resource::Graph, graph) {
            None => true,
            Some(owner) => self.owner(Resource::Collection, collection) == Some(owner),
        }
    }

    fn owner(&self, resource: Resource, stored: &str) -> Option<usize> {
        self.owners.get(&(resource, stored.to_owned())).copied()
    }

    /// Reserves `stored` with no owning entry; an entry's own reservation wins.
    fn reserve_unowned(&mut self, resource: Resource, stored: String) {
        self.owners.entry((resource, stored)).or_insert(UNOWNED);
    }

    /// Whether every resource that `other` reserves is reserved here too,
    /// whoever owns it. Which credentials hold raw access to a resource is not
    /// compared.
    pub(super) fn covers(&self, other: &Self) -> bool {
        other
            .owners
            .keys()
            .all(|resource| self.owners.contains_key(resource))
    }
}

/// The validated binding set read at startup and its reservations.
#[derive(Debug, Default)]
pub(super) struct Bindings {
    set: BindingSet,
    reserved: Reserved,
}

impl Bindings {
    /// `set` must already be validated (`binding::parse` validates).
    pub(super) fn new(set: BindingSet) -> Self {
        let reserved = Reserved::new(&set);
        Self { set, reserved }
    }

    /// Whether nothing is reserved: no entry is configured and no record in
    /// the database stays reserved without one (the aggregate guards ask this).
    pub(super) fn is_empty(&self) -> bool {
        self.reserved.is_empty()
    }

    /// Whether the file names at least one binding: only then are the memory
    /// routes served and the startup checks of the file run.
    pub(super) fn is_configured(&self) -> bool {
        !self.set.bindings.is_empty()
    }

    pub(super) fn set(&self) -> &BindingSet {
        &self.set
    }

    /// The same bindings with the graphs and evidence collections of
    /// `orphans` reserved as well, with no owner and so no raw access.
    pub(super) fn with_orphans(&self, orphans: &[Recorded]) -> Self {
        let mut reserved = Reserved::new(&self.set);
        for record in orphans {
            for (graph, stored) in record.storage() {
                let resource = if graph {
                    Resource::Graph
                } else {
                    Resource::Collection
                };
                reserved.reserve_unowned(resource, stored);
            }
        }
        Self {
            set: self.set.clone(),
            reserved,
        }
    }

    pub(super) fn reserved(&self) -> &Reserved {
        &self.reserved
    }

    /// Whether everything `other` reserves is reserved here too; owners and
    /// raw-access grants are not compared (`Reserved::covers`).
    pub(super) fn covers(&self, other: &Self) -> bool {
        self.reserved.covers(&other.reserved)
    }

    /// The binding named `binding` in the caller's tenant (`None` is the
    /// untenanted `@root`). The caller never names a tenant.
    pub(super) fn resolve(&self, tenant: Option<&str>, binding: &str) -> Option<&Binding> {
        self.set.find(tenant.unwrap_or(ROOT_TENANT), binding)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &Binding> {
        self.set.bindings.iter()
    }
}

/// How long a reload waits for the requests that consulted superseded grants.
pub(super) const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

const DRAIN_POLL: Duration = Duration::from_millis(10);

/// The lifetime of one installed view: every request that consulted the view
/// holds its epoch until the request ends.
#[derive(Debug, Default)]
pub(super) struct Epoch;

/// The view one request decides with, and the epoch that marks it in use.
#[derive(Clone, Debug)]
pub(super) struct Pin {
    bindings: Arc<Bindings>,
    _epoch: Arc<Epoch>,
}

impl Pin {
    pub(super) fn bindings(&self) -> Arc<Bindings> {
        self.bindings.clone()
    }
}

/// The bindings a router serves. A reload of the bindings file replaces the
/// view in force with one whose grants differ (`memory/reload.rs`); the
/// bindings, their resources and the reservations never change while serving.
#[derive(Debug, Default)]
pub(super) struct Served {
    /// The configured bindings file, which a reload reads again.
    source: Option<PathBuf>,
    current: RwLock<(Arc<Bindings>, Arc<Epoch>)>,
    /// The epochs of superseded views, until no request holds one.
    retired: Mutex<Vec<Weak<Epoch>>>,
    reloading: Arc<tokio::sync::Mutex<()>>,
}

impl Served {
    pub(super) fn new(bindings: Arc<Bindings>, source: Option<PathBuf>) -> Self {
        Self {
            source,
            current: RwLock::new((bindings, Arc::new(Epoch))),
            retired: Mutex::default(),
            reloading: Arc::default(),
        }
    }

    pub(super) fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// The view in force, unpinned: for code outside a request and for every
    /// decision that does not depend on grants.
    pub(super) fn current(&self) -> Arc<Bindings> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .clone()
    }

    /// The view in force, marked in use until the pin is dropped.
    pub(super) fn pin(&self) -> Pin {
        let current = self.current.read().unwrap_or_else(PoisonError::into_inner);
        Pin {
            bindings: current.0.clone(),
            _epoch: current.1.clone(),
        }
    }

    /// Makes `bindings` the view in force. Requests that consulted the
    /// previous view keep it; [`Self::drain`] waits for them. Only a reload
    /// installs, under the database write lock.
    pub(super) fn install(&self, bindings: Bindings) {
        let (_, superseded) = std::mem::replace(
            &mut *self.current.write().unwrap_or_else(PoisonError::into_inner),
            (Arc::new(bindings), Arc::new(Epoch)),
        );
        self.retired
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::downgrade(&superseded));
    }

    /// Waits until no request holds a superseded view, for at most `timeout`,
    /// and says whether none does. No request is cut off.
    pub(super) async fn drain(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.drained() {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(DRAIN_POLL).await;
        }
        true
    }

    fn drained(&self) -> bool {
        let mut retired = self.retired.lock().unwrap_or_else(PoisonError::into_inner);
        retired.retain(|epoch| epoch.strong_count() > 0);
        retired.is_empty()
    }

    /// Whether no request holds a superseded view. Unlike [`Self::drain`] it
    /// prunes nothing, so status can ask it without changing the cell.
    pub(super) fn is_drained(&self) -> bool {
        self.retired
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .all(|epoch| epoch.strong_count() == 0)
    }

    /// The reload lock, unless another reload holds it.
    pub(super) fn try_reload(&self) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        self.reloading.clone().try_lock_owned().ok()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.current().is_empty()
    }

    pub(super) fn is_configured(&self) -> bool {
        self.current().is_configured()
    }

    /// [`Bindings::covers`] of the views in force. Both are copied out first:
    /// `other` may be this very cell.
    pub(super) fn covers(&self, other: &Self) -> bool {
        let (mine, theirs) = (self.current(), other.current());
        mine.covers(&theirs)
    }

    pub(super) fn with_orphans(&self, orphans: &[Recorded]) -> Bindings {
        self.current().with_orphans(orphans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryScope;
    use crate::memory::binding::{BindingState, Grant};

    const TENANT: &str = "0123456789abcdef0123456789abcdef";
    const RAW: &str = "rawrawrawraw";
    const PLAIN: &str = "plainplainpl";

    fn binding(tenant: &str, name: &str, graph: &str, evidence: Option<&str>) -> Binding {
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
            evidence_collection: evidence.map(Into::into),
            receipt_window_ms: 1,
            grants: vec![
                Grant {
                    principal: RAW.into(),
                    scopes: vec![scope.clone()],
                    raw_access: true,
                    read_scopes: Vec::new(),
                },
                Grant {
                    principal: PLAIN.into(),
                    scopes: vec![scope],
                    raw_access: false,
                    read_scopes: Vec::new(),
                },
            ],
        }
    }

    fn registry() -> Bindings {
        let mut retired = binding(ROOT_TENANT, "old", "old_graph", None);
        retired.state = BindingState::Retired { at_ms: 1 };
        retired.grants.clear();
        let set = BindingSet {
            bindings: vec![
                binding(ROOT_TENANT, "notes", "notes_graph", Some("notes_ev")),
                binding(TENANT, "notes", "g", Some("ev")),
                retired,
            ],
        };
        assert_eq!(set.validate(), Ok(()));
        Bindings::new(set)
    }

    #[test]
    fn an_empty_registry_reserves_nothing() {
        let bindings = Bindings::new(BindingSet::default());
        assert!(bindings.is_empty());
        let reserved = bindings.reserved();
        assert!(reserved.is_empty());
        assert!(!reserved.is_reserved(Resource::Graph, "notes_graph"));
        assert!(reserved.raw_allowed(None, Resource::Graph, "notes_graph"));
        let mut items = vec!["notes_graph", "x"];
        reserved.retain_visible(None, Resource::Graph, &mut items, |n| n);
        assert_eq!(items, ["notes_graph", "x"]);
        assert!(bindings.resolve(None, "notes").is_none());
    }

    #[test]
    fn a_registry_without_raw_grants_still_reserves() {
        let mut only = binding(ROOT_TENANT, "notes", "notes_graph", None);
        for grant in &mut only.grants {
            grant.raw_access = false;
        }
        let bindings = Bindings::new(BindingSet {
            bindings: vec![only],
        });
        let reserved = bindings.reserved();
        assert!(!reserved.is_empty());
        let mut items = vec!["notes_graph", "x"];
        reserved.retain_visible(Some(RAW), Resource::Graph, &mut items, |n| n);
        assert_eq!(items, ["x"]);
    }

    #[test]
    fn stored_names_are_reserved_per_namespace_and_tenant() {
        let bindings = registry();
        let reserved = bindings.reserved();
        assert!(!reserved.is_empty());
        for (resource, stored) in [
            (Resource::Graph, "notes_graph".to_owned()),
            (Resource::Collection, "notes_ev".to_owned()),
            (Resource::Graph, format!("t{TENANT}__g")),
            (Resource::Collection, format!("t{TENANT}__ev")),
            (Resource::Graph, "old_graph".to_owned()),
        ] {
            assert!(reserved.is_reserved(resource, &stored), "{stored}");
        }
        for (resource, stored) in [
            (Resource::Collection, "notes_graph"),
            (Resource::Graph, "notes_ev"),
            (Resource::Graph, "g"),
            (Resource::Collection, "ev"),
            (Resource::Graph, "notes"),
            (Resource::Graph, "notes_graph2"),
        ] {
            assert!(!reserved.is_reserved(resource, stored), "{stored}");
        }
    }

    #[test]
    fn raw_access_is_per_binding_and_credential_only() {
        let bindings = registry();
        let reserved = bindings.reserved();
        let tenant_graph = format!("t{TENANT}__g");
        assert!(reserved.raw_allowed(Some(RAW), Resource::Graph, "notes_graph"));
        assert!(reserved.raw_allowed(Some(RAW), Resource::Collection, "notes_ev"));
        assert!(reserved.raw_allowed(Some(RAW), Resource::Graph, &tenant_graph));
        assert!(!reserved.raw_allowed(Some(PLAIN), Resource::Graph, "notes_graph"));
        assert!(!reserved.raw_allowed(None, Resource::Graph, "notes_graph"));
        // A grant on another binding never reaches this one.
        assert!(!reserved.raw_allowed(Some(RAW), Resource::Graph, "old_graph"));
        // Unreserved resources are untouched by this check.
        assert!(reserved.raw_allowed(None, Resource::Graph, "g"));
        assert!(reserved.raw_allowed(Some(PLAIN), Resource::Collection, "other"));
    }

    #[test]
    fn a_reserved_graph_takes_only_its_own_bindings_collection() {
        let bindings = registry();
        let reserved = bindings.reserved();
        let tenant_graph = format!("t{TENANT}__g");
        let tenant_evidence = format!("t{TENANT}__ev");
        // The binding's own evidence collection, and nothing else, may be
        // attached to its graph.
        assert!(reserved.attachment_allowed("notes_graph", "notes_ev"));
        assert!(reserved.attachment_allowed(&tenant_graph, &tenant_evidence));
        assert!(!reserved.attachment_allowed("notes_graph", "ordinary"));
        assert!(!reserved.attachment_allowed("notes_graph", &tenant_evidence));
        assert!(!reserved.attachment_allowed(&tenant_graph, "notes_ev"));
        assert!(!reserved.attachment_allowed("old_graph", "notes_ev"));
        // A graph that is not reserved takes any collection.
        assert!(reserved.attachment_allowed("ordinary_graph", "ordinary"));
        assert!(reserved.attachment_allowed("ordinary_graph", "notes_ev"));
        assert!(
            Bindings::new(BindingSet::default())
                .reserved()
                .attachment_allowed("notes_graph", "ordinary")
        );
    }

    #[test]
    fn lists_keep_order_and_drop_unreachable_reserved_entries() {
        let bindings = registry();
        let reserved = bindings.reserved();
        let page = ["a", "notes_graph", "b", "old_graph", "notes_ev"];
        let mut plain = page.to_vec();
        reserved.retain_visible(Some(PLAIN), Resource::Graph, &mut plain, |n| n);
        assert_eq!(plain, ["a", "b", "notes_ev"]);
        let mut raw = page.to_vec();
        reserved.retain_visible(Some(RAW), Resource::Graph, &mut raw, |n| n);
        assert_eq!(raw, ["a", "notes_graph", "b", "notes_ev"]);
    }

    #[test]
    fn bindings_resolve_in_the_callers_tenant_only() {
        let bindings = registry();
        assert!(!bindings.is_empty());
        assert_eq!(bindings.iter().count(), 3);
        assert_eq!(
            bindings.resolve(None, "notes").map(|b| b.graph.as_str()),
            Some("notes_graph")
        );
        assert_eq!(
            bindings
                .resolve(Some(TENANT), "notes")
                .map(|b| b.graph.as_str()),
            Some("g")
        );
        assert!(bindings.resolve(Some(TENANT), "old").is_none());
        assert!(bindings.resolve(None, "missing").is_none());
    }

    fn recorded(tenant: &str, graph: &str, evidence: Option<&str>) -> Recorded {
        Recorded::of(&binding(tenant, "gone", graph, evidence))
    }

    #[test]
    fn an_orphan_reserves_its_graph_and_evidence_with_no_owner() {
        let bindings = Bindings::new(BindingSet::default()).with_orphans(&[recorded(
            ROOT_TENANT,
            "old_graph",
            Some("old_ev"),
        )]);
        assert!(!bindings.is_empty());
        assert!(!bindings.is_configured());
        assert_eq!(bindings.iter().count(), 0);
        assert!(bindings.resolve(None, "gone").is_none());
        let reserved = bindings.reserved();
        assert!(!reserved.is_empty());
        assert!(reserved.is_reserved(Resource::Graph, "old_graph"));
        assert!(reserved.is_reserved(Resource::Collection, "old_ev"));
        for credential in [None, Some(RAW), Some(PLAIN)] {
            assert!(!reserved.raw_allowed(credential, Resource::Graph, "old_graph"));
            assert!(!reserved.raw_allowed(credential, Resource::Collection, "old_ev"));
        }
        // The namespaces stay separate and unrelated names stay open.
        assert!(!reserved.is_reserved(Resource::Collection, "old_graph"));
        assert!(!reserved.is_reserved(Resource::Graph, "old_ev"));
        assert!(reserved.raw_allowed(None, Resource::Graph, "other"));
        let mut page = vec!["a", "old_graph", "b"];
        reserved.retain_visible(Some(RAW), Resource::Graph, &mut page, |n| n);
        assert_eq!(page, ["a", "b"]);
    }

    #[test]
    fn an_orphan_without_evidence_reserves_only_its_graph() {
        let bindings = Bindings::new(BindingSet::default()).with_orphans(&[recorded(
            ROOT_TENANT,
            "old_graph",
            None,
        )]);
        assert!(
            bindings
                .reserved()
                .is_reserved(Resource::Graph, "old_graph")
        );
        assert!(
            !bindings
                .reserved()
                .is_reserved(Resource::Collection, "old_graph")
        );
    }

    #[test]
    fn an_orphan_follows_the_tenant_qualified_names() {
        let bindings =
            Bindings::new(BindingSet::default()).with_orphans(&[recorded(TENANT, "g", Some("ev"))]);
        let reserved = bindings.reserved();
        assert!(reserved.is_reserved(Resource::Graph, &format!("t{TENANT}__g")));
        assert!(reserved.is_reserved(Resource::Collection, &format!("t{TENANT}__ev")));
        assert!(!reserved.is_reserved(Resource::Graph, "g"));
        assert!(!reserved.is_reserved(Resource::Collection, "ev"));
    }

    #[test]
    fn an_entrys_own_reservation_wins_over_an_orphan_of_the_same_name() {
        let bindings =
            registry().with_orphans(&[recorded(ROOT_TENANT, "notes_graph", Some("notes_ev"))]);
        assert!(bindings.is_configured());
        assert_eq!(bindings.iter().count(), 3);
        let reserved = bindings.reserved();
        assert!(reserved.raw_allowed(Some(RAW), Resource::Graph, "notes_graph"));
        assert!(reserved.raw_allowed(Some(RAW), Resource::Collection, "notes_ev"));
        assert!(!reserved.raw_allowed(Some(PLAIN), Resource::Graph, "notes_graph"));
    }

    #[test]
    fn a_registry_covers_what_another_one_reserves() {
        let none = Bindings::new(BindingSet::default());
        let notes = registry();
        let other = Bindings::new(BindingSet {
            bindings: vec![binding(
                ROOT_TENANT,
                "other",
                "other_graph",
                Some("other_ev"),
            )],
        });
        assert!(none.covers(&none));
        assert!(
            notes.covers(&none),
            "every registry covers one that reserves nothing"
        );
        assert!(
            !none.covers(&notes),
            "one that reserves nothing covers nothing else"
        );
        assert!(notes.covers(&notes));
        assert!(
            !notes.covers(&other) && !other.covers(&notes),
            "neither of two that reserve different resources covers the other"
        );
        // A record found in the database reserves its resources with no owner,
        // and that is cover enough; a partial overlap is not.
        let found = recorded(ROOT_TENANT, "other_graph", Some("other_ev"));
        let both = notes.with_orphans(std::slice::from_ref(&found));
        assert!(both.covers(&notes) && both.covers(&other));
        assert!(!notes.covers(&both) && !other.covers(&both));
        let half = notes.with_orphans(&[recorded(ROOT_TENANT, "other_graph", None)]);
        assert!(
            half.covers(&notes) && !half.covers(&other),
            "the evidence is not covered"
        );
    }

    /// The registry with every grant's `raw_access` flipped and one principal
    /// replaced: the same bindings and reservations under other grants.
    fn regranted() -> Bindings {
        let mut set = registry().set().clone();
        for binding in &mut set.bindings {
            for grant in &mut binding.grants {
                grant.raw_access = !grant.raw_access;
                if grant.principal == PLAIN {
                    grant.principal = "otherotherot".into();
                }
            }
        }
        assert_eq!(set.validate(), Ok(()));
        Bindings::new(set)
    }

    #[tokio::test]
    async fn an_install_swaps_the_view_and_a_drain_waits_for_its_pins() {
        let served = Served::new(Arc::new(registry()), None);
        assert!(served.source().is_none());
        let before = served.current();
        let pinned = served.pin();
        served.install(regranted());
        let after = served.current();
        assert!(!Arc::ptr_eq(&before, &after), "the install swaps the view");
        assert!(
            after
                .reserved()
                .raw_allowed(Some("otherotherot"), Resource::Graph, "notes_graph")
        );
        assert!(
            Arc::ptr_eq(&pinned.bindings(), &before),
            "a pin keeps its view"
        );
        assert!(
            !served.drain(Duration::from_millis(50)).await,
            "a pin of the superseded view is alive"
        );
        drop(pinned);
        assert!(served.drain(Duration::from_millis(50)).await);
        assert!(served.retired.lock().unwrap().is_empty(), "pruned");
        // An unpinned snapshot holds no epoch, so there is nothing to wait for.
        let snapshot = served.current();
        served.install(registry());
        assert!(served.drain(Duration::ZERO).await);
        assert!(Arc::ptr_eq(&snapshot, &after));
        // Whatever does not depend on grants reads the view in force.
        assert!(served.is_configured() && !served.is_empty());
        assert!(served.covers(&served));
        let unconfigured = Served::default();
        assert!(!unconfigured.is_configured() && unconfigured.is_empty());
        assert!(served.covers(&unconfigured) && !unconfigured.covers(&served));
    }

    /// TEST-A08 (feature-memory-preflight PRD): status asks whether the
    /// superseded views are unpinned without pruning them, so asking leaves
    /// the retired list as it was.
    #[test]
    fn is_drained_reports_the_pins_without_pruning() {
        let served = Served::new(Arc::new(registry()), None);
        assert!(served.is_drained(), "nothing superseded yet");
        let pinned = served.pin();
        served.install(regranted());
        assert!(
            !served.is_drained(),
            "a pin of the superseded view is alive"
        );
        assert_eq!(served.retired.lock().unwrap().len(), 1);
        drop(pinned);
        assert!(served.is_drained());
        assert_eq!(
            served.retired.lock().unwrap().len(),
            1,
            "asking prunes nothing"
        );
    }

    #[test]
    fn the_administrators_decisions_do_not_depend_on_grants() {
        let names = [
            "notes_graph".to_owned(),
            "notes_ev".to_owned(),
            "old_graph".to_owned(),
            format!("t{TENANT}__g"),
            format!("t{TENANT}__ev"),
            "ordinary".to_owned(),
            "g".to_owned(),
            "ev".to_owned(),
        ];
        let (old, new) = (registry(), regranted());
        let (before, after) = (old.reserved(), new.reserved());
        // The grants differ for a credential...
        assert_ne!(
            before.raw_allowed(Some(RAW), Resource::Graph, "notes_graph"),
            after.raw_allowed(Some(RAW), Resource::Graph, "notes_graph")
        );
        // ...and never for the administrator, who holds no credential ID.
        for resource in [Resource::Graph, Resource::Collection] {
            for name in &names {
                assert_eq!(
                    before.raw_allowed(None, resource, name),
                    after.raw_allowed(None, resource, name),
                    "{name}"
                );
            }
            let (mut seen, mut also) = (names.to_vec(), names.to_vec());
            before.retain_visible(None, resource, &mut seen, String::as_str);
            after.retain_visible(None, resource, &mut also, String::as_str);
            assert_eq!(seen, also);
        }
        for graph in &names {
            for collection in &names {
                assert_eq!(
                    before.attachment_allowed(graph, collection),
                    after.attachment_allowed(graph, collection),
                    "{graph} {collection}"
                );
            }
        }
    }

    #[test]
    fn no_orphans_leave_the_registry_as_it_was() {
        assert!(
            Bindings::new(BindingSet::default())
                .with_orphans(&[])
                .is_empty()
        );
        let before = registry();
        let after = registry().with_orphans(&[]);
        assert_eq!(after.iter().count(), before.iter().count());
        assert!(after.is_configured());
        for (resource, stored) in [
            (Resource::Graph, "notes_graph"),
            (Resource::Collection, "notes_ev"),
            (Resource::Graph, "old_graph"),
        ] {
            assert_eq!(
                after.reserved().is_reserved(resource, stored),
                before.reserved().is_reserved(resource, stored),
                "{stored}"
            );
        }
    }
}
