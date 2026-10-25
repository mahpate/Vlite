//! Isolated build and test harness for the VLite property-graph engine.
//!
//! The engine source lives in the root `vlite` package at
//! `src/property_graph/engine/` so that `cargo package -p vlite` ships it
//! without any crate dependency (`vlite` never depends on this crate). This
//! unpublished, workspace-only crate includes the very same files by path, so
//! `cargo test -p vlite-graph` builds and tests the pure engine with no `vlite`
//! types, no dependencies, no I/O and no `unsafe` code. Its API carries no
//! stability promise.

#![forbid(unsafe_code)]

#[path = "../../../src/property_graph/engine/mod.rs"]
mod engine;

pub use engine::*;
