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

use crate::memory::binding::{Binding, BindingSet, ROOT_TENANT};
use std::collections::{BTreeMap, BTreeSet};

/// The namespace of a stored name: graphs and collections are separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Resource {
    Graph,
    Collection,
}

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

    fn owner(&self, resource: Resource, stored: &str) -> Option<usize> {
        self.owners.get(&(resource, stored.to_owned())).copied()
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

    pub(super) fn is_empty(&self) -> bool {
        self.set.bindings.is_empty()
    }

    pub(super) fn reserved(&self) -> &Reserved {
        &self.reserved
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
                },
                Grant {
                    principal: PLAIN.into(),
                    scopes: vec![scope],
                    raw_access: false,
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
}
