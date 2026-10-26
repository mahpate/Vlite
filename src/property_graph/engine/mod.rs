//! Pure property-graph engine shared by `vlite` and the `vlite-graph` harness.
//!
//! This source lives inside the root `vlite` package (`src/**`) so that
//! `cargo package -p vlite` ships it without any crate dependency. It compiles
//! in two places from the same files:
//!
//! - as `vlite::property_graph::engine` (crate-private) in every `vlite`
//!   build, and
//! - as the root module of the unpublished, workspace-only `vlite-graph`
//!   crate (`crates/vlite-graph`), which includes this file by `#[path]` so the
//!   engine is built and unit-tested in isolation.
//!
//! All internal references are module-relative (`super::`), never `crate::`.
//!
//! Boundary rules:
//!
//! - Only `std` is used: no `vlite` types, no third-party dependencies, no
//!   file, network or thread I/O, and no `unsafe` code. Persistence,
//!   encryption, locking and commits remain in `vlite`.
//!
//! Implemented here: the graph data model ([`model`]), the strict bounded
//! graph-entry codec for the closed entry tags ([`codec`]), the property encoding ([`properties`],
//! byte-identical to VLite metadata), limits and errors, and the in-memory
//! named-graph state with atomic batch application and invariant checks
//! ([`GraphState`]), and key-addressed mutation planning ([`Planner`]). Attachment record checks against vector collections live
//! in the `vlite` adapter (`property_graph::frame`).
//!
//! Bounded reads (EPIC-005): one query IR ([`GraphQuery`]) and one executor
//! ([`execute`]) for get, `find_nodes`, `neighbors`, breadth-first traversal,
//! shortest paths and `explain`, with read limits and ceilings
//! ([`GraphLimits`]), deterministic ordering, truncation reporting, opaque
//! page cursors and property conditions with VLite metadata filter semantics.
//! The clock, cancellation flag and view sequence are passed per call in an
//! [`ExecutionContext`]. Not implemented: hybrid retrieval.
//!
//! Schemas (EPIC-008): optional declared label and relationship rules
//! enforced on every write, declared exact/range property indexes used by
//! `find_nodes`, `ensure_edge` for unique triples, and a budgeted observed
//! schema summary ([`schema`]).
//!
//! Memory diagnostics (ITEM-021): [`GraphState::release_components`] drops a
//! state one owned component at a time so an allocation-counting benchmark
//! (`examples/graph_memory.rs`) can attribute its heap bytes exactly.

#![forbid(unsafe_code)]

pub mod codec;
mod cursor;
mod error;
mod filter;
pub mod language;
pub mod limits;
mod meter;
pub mod model;
mod mutate;
pub mod properties;
mod query;
pub mod schema;
mod state;
#[cfg(test)]
mod testkit;
mod traverse;

pub use codec::{GraphBody, GraphEntry, decode_body, encode_body};
pub use error::GraphError;
pub use filter::{NodeFilter, conditions_match};
pub use limits::GraphLimits;
pub use meter::ExecutionContext;
pub use model::{
    Direction, EdgeId, GraphEdge, GraphId, GraphNode, NodeId, Precondition, Properties,
    PropertyCondition, PropertyValue,
};
pub use mutate::{EdgeSelector, GraphMutation, MutationOutcome, Planner};
pub use query::{
    EdgeView, Explanation, GraphPath, GraphQuery, IndexChoice, NodeSelector, NodeView, PathOutcome,
    PropertyProjection, QueryOp, QueryOutput, QueryResult, QueryStats, TraversedNode,
    TruncationReason, execute,
};
pub use schema::{
    GraphSchema, LabelDefinition, Multiplicity, ObservedEndpoint, ObservedLabel, ObservedProperty,
    ObservedRelationship, ObservedSchema, PropertyDefinition, PropertyIndexKind, PropertyType,
    RelationshipDefinition, SchemaOrigin,
};
pub use state::{Adjacency, Graph, GraphState, MemoryComponent};
