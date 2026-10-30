//! Bulk load of a graph snapshot (Speed P2b).
//!
//! A checkpoint image stores each graph as its canonical body stream
//! (`property_graph::frame::compacted_bodies`). Loading it builds the graph
//! directly — nodes and edges are inserted in ID order, adjacency lists are
//! sorted once, declared rules are checked once per graph — instead of
//! applying every entry with per-entry undo, adjacency insertion and schema
//! checks. The result equals replaying the same bodies with
//! [`GraphState::apply_at`]; anything a canonical writer cannot produce is
//! rejected.

use super::*;

/// How far the stream of the graph being loaded has progressed; entries
/// must not go back to an earlier phase.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Created,
    Schema,
    Nodes,
    Edges,
    Attachments,
}

const NON_CANONICAL: GraphError = GraphError::Corrupt("non-canonical graph snapshot");

impl GraphState {
    /// Builds the state described by canonical snapshot bodies restated at
    /// `sequence`: per graph in ascending ID order, `CreateGraph`, an
    /// optional `PutSchema`, nodes and then edges in strictly ascending ID
    /// order, attachments in ascending (node, collection) order and a
    /// closing `GraphHighWater`. One graph may span several bodies.
    pub fn from_snapshot(
        sequence: u64,
        bodies: impl IntoIterator<Item = GraphBody>,
    ) -> Result<Self, GraphError> {
        let mut state = Self::new();
        let mut open: Option<(Graph, Phase)> = None;
        for body in bodies {
            body.validate()?;
            state.last_sequence = sequence;
            let graph_id = body.graph_id;
            for entry in body.entries {
                match open.as_mut() {
                    None => {
                        open = Some((state.created(graph_id, sequence, entry)?, Phase::Created))
                    }
                    Some((graph, phase)) => {
                        if graph.id != graph_id {
                            return Err(NON_CANONICAL);
                        }
                        if let GraphEntry::GraphHighWater {
                            next_node_id,
                            next_edge_id,
                        } = entry
                        {
                            let (graph, _) = open.take().expect("graph is open");
                            state.close(graph, next_node_id, next_edge_id)?;
                        } else {
                            *phase = state.load_entry(graph, *phase, sequence, entry)?;
                        }
                    }
                }
            }
        }
        if open.is_some() {
            return Err(NON_CANONICAL);
        }
        Ok(state)
    }

    fn created(
        &self,
        graph_id: GraphId,
        sequence: u64,
        entry: GraphEntry,
    ) -> Result<Graph, GraphError> {
        let GraphEntry::CreateGraph { name } = entry else {
            return Err(NON_CANONICAL);
        };
        if self
            .graphs
            .last_key_value()
            .is_some_and(|(last, _)| *last >= graph_id)
        {
            return Err(GraphError::GraphIdUnavailable(graph_id));
        }
        if self.names.contains_key(&name) {
            return Err(GraphError::GraphNameExists(name));
        }
        Ok(Graph::new(graph_id, name, sequence))
    }

    /// Adds one entry of an open graph and returns the phase it belongs to.
    fn load_entry(
        &mut self,
        graph: &mut Graph,
        phase: Phase,
        sequence: u64,
        entry: GraphEntry,
    ) -> Result<Phase, GraphError> {
        match entry {
            GraphEntry::PutSchema { schema } if phase == Phase::Created => {
                graph.schema = schema;
                Ok(Phase::Schema)
            }
            GraphEntry::PutNode {
                node_id,
                key,
                labels,
                properties,
            } if phase <= Phase::Nodes => {
                if graph
                    .nodes
                    .last_key_value()
                    .is_some_and(|(last, _)| *last >= node_id)
                {
                    return Err(GraphError::NodeIdUnavailable(node_id));
                }
                if graph.node_keys.contains_key(&key) {
                    return Err(GraphError::NodeKeyExists(key));
                }
                new_id(&mut graph.next_node_id, node_id.0)?;
                graph.insert_node(GraphNode {
                    id: node_id,
                    key,
                    labels,
                    properties,
                    revision: sequence,
                });
                Ok(Phase::Nodes)
            }
            GraphEntry::PutEdge {
                edge_id,
                source,
                destination,
                relationship,
                key,
                properties,
            } if phase <= Phase::Edges => {
                if graph
                    .edges
                    .last_key_value()
                    .is_some_and(|(last, _)| *last >= edge_id)
                {
                    return Err(GraphError::EdgeIdUnavailable(edge_id));
                }
                for endpoint in [source, destination] {
                    if !graph.nodes.contains_key(&endpoint) {
                        return Err(GraphError::NodeNotFound(endpoint));
                    }
                }
                if let Some(key) = &key
                    && graph.edge_keys.contains_key(key)
                {
                    return Err(GraphError::EdgeKeyExists(key.clone()));
                }
                new_id(&mut graph.next_edge_id, edge_id.0)?;
                graph.push_edge(GraphEdge {
                    id: edge_id,
                    key,
                    source,
                    relationship,
                    destination,
                    properties,
                    revision: sequence,
                });
                Ok(Phase::Edges)
            }
            GraphEntry::Attach {
                node_id,
                collection,
                record_id,
            } => {
                if !graph.nodes.contains_key(&node_id) {
                    return Err(GraphError::NodeNotFound(node_id));
                }
                let ascending = graph
                    .attachments
                    .last_key_value()
                    .is_none_or(|(last, map)| {
                        *last < node_id
                            || (*last == node_id
                                && map.last_key_value().is_some_and(|(c, _)| *c < collection))
                    });
                if !ascending {
                    return Err(GraphError::NodeAlreadyAttached {
                        node: node_id,
                        collection,
                    });
                }
                if let Some((graph, node)) = self.attachment_owners.get(&(collection, record_id)) {
                    return Err(GraphError::RecordAlreadyAttached {
                        collection,
                        record: record_id,
                        graph: *graph,
                        node: *node,
                    });
                }
                graph
                    .attachments
                    .entry(node_id)
                    .or_default()
                    .insert(collection, record_id);
                self.attachment_owners
                    .insert((collection, record_id), (graph.id, node_id));
                Ok(Phase::Attachments)
            }
            _ => Err(NON_CANONICAL),
        }
    }

    /// Completes a graph at its high-water entry: adjacency lists are
    /// sorted, declared rules checked and the graph becomes live.
    fn close(
        &mut self,
        mut graph: Graph,
        next_node_id: u64,
        next_edge_id: u64,
    ) -> Result<(), GraphError> {
        if next_node_id < graph.next_node_id || next_edge_id < graph.next_edge_id {
            return Err(GraphError::HighWaterDecrease);
        }
        graph.next_node_id = next_node_id;
        graph.next_edge_id = next_edge_id;
        for entries in graph
            .out_edges
            .values_mut()
            .chain(graph.in_edges.values_mut())
        {
            entries.sort_unstable();
        }
        if !graph.schema.is_empty() {
            schema::check_graph(&graph)?;
        }
        self.names.insert(graph.name.clone(), graph.id);
        self.graphs.insert(graph.id, graph);
        Ok(())
    }
}

impl Graph {
    /// [`Graph::insert_edge`] without keeping adjacency lists sorted; the
    /// caller sorts them once all edges are in.
    fn push_edge(&mut self, edge: GraphEdge) {
        if let Some(key) = &edge.key {
            self.edge_keys.insert(key.clone(), edge.id);
        }
        let relationship = match self.relationships.get_key_value(&edge.relationship) {
            Some((symbol, _)) => Arc::clone(symbol),
            None => Arc::new(edge.relationship.clone()),
        };
        *self
            .relationships
            .entry(Arc::clone(&relationship))
            .or_default() += 1;
        let entry = (relationship, edge.id);
        self.out_edges
            .entry(edge.source)
            .or_default()
            .push(entry.clone());
        self.in_edges
            .entry(edge.destination)
            .or_default()
            .push(entry);
        self.edges.insert(edge.id, edge);
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::model::PropertyValue;
    use super::super::super::schema::{
        LabelDefinition, PropertyDefinition, PropertyIndexKind, PropertyType,
    };
    use super::super::super::testkit::{hub_graph, int, props, random_graph, set};
    use super::*;

    /// The canonical snapshot of `state` in bodies of at most `chunk`
    /// entries, in the order the checkpoint image writer emits them.
    fn snapshot_bodies(state: &GraphState, chunk: usize) -> Vec<GraphBody> {
        let mut bodies = Vec::new();
        for graph in state.graphs() {
            let mut entries = vec![GraphEntry::CreateGraph {
                name: graph.name().into(),
            }];
            if !graph.schema().is_empty() {
                entries.push(GraphEntry::PutSchema {
                    schema: graph.schema().clone(),
                });
            }
            entries.extend(graph.nodes().map(|node| GraphEntry::PutNode {
                node_id: node.id,
                key: node.key.clone(),
                labels: node.labels.clone(),
                properties: node.properties.clone(),
            }));
            entries.extend(graph.edges().map(|edge| GraphEntry::PutEdge {
                edge_id: edge.id,
                source: edge.source,
                destination: edge.destination,
                relationship: edge.relationship.clone(),
                key: edge.key.clone(),
                properties: edge.properties.clone(),
            }));
            for node in graph.nodes() {
                entries.extend(graph.attachments(node.id).map(|(collection, record_id)| {
                    GraphEntry::Attach {
                        node_id: node.id,
                        collection,
                        record_id,
                    }
                }));
            }
            entries.push(GraphEntry::GraphHighWater {
                next_node_id: graph.next_node_id(),
                next_edge_id: graph.next_edge_id(),
            });
            bodies.extend(entries.chunks(chunk).map(|entries| GraphBody {
                graph_id: graph.id(),
                entries: entries.to_vec(),
            }));
        }
        bodies
    }

    fn replayed(sequence: u64, bodies: &[GraphBody]) -> GraphState {
        let mut state = GraphState::new();
        for body in bodies {
            state.apply_at(sequence, body, true).unwrap();
        }
        state
    }

    fn person(id: u64, key: &str, age: i64) -> GraphEntry {
        GraphEntry::PutNode {
            node_id: NodeId(id),
            key: key.into(),
            labels: set(&["Person"]),
            properties: props(&[
                ("age", int(age)),
                ("email", PropertyValue::String(format!("{key}@example.com"))),
            ]),
        }
    }

    fn edge(id: u64, source: u64, relationship: &str, destination: u64) -> GraphEntry {
        GraphEntry::PutEdge {
            edge_id: EdgeId(id),
            source: NodeId(source),
            destination: NodeId(destination),
            relationship: relationship.into(),
            key: Some(format!("e{id}")),
            properties: Properties::new(),
        }
    }

    /// Graph 1 (`people`) with a schema (unique and range-indexed
    /// properties), several relationship types, gaps in its IDs and
    /// attachments in two collections.
    fn add_schema_graph(state: &mut GraphState) {
        let schema = GraphSchema::default().label(
            "Person",
            LabelDefinition::new("a person")
                .property(
                    "age",
                    PropertyDefinition::new(PropertyType::Integer).index(PropertyIndexKind::Range),
                )
                .property(
                    "email",
                    PropertyDefinition::new(PropertyType::String)
                        .required()
                        .unique(),
                ),
        );
        let body = GraphBody {
            graph_id: GraphId(1),
            entries: vec![
                GraphEntry::CreateGraph {
                    name: "people".into(),
                },
                GraphEntry::PutSchema { schema },
                person(0, "ann", 31),
                person(2, "bob", 27),
                person(5, "cid", 31),
                edge(0, 0, "KNOWS", 2),
                edge(3, 2, "KNOWS", 5),
                edge(4, 0, "LIKES", 5),
                edge(9, 5, "KNOWS", 0),
                edge(10, 0, "KNOWS", 0),
                GraphEntry::Attach {
                    node_id: NodeId(0),
                    collection: 0,
                    record_id: 7,
                },
                GraphEntry::Attach {
                    node_id: NodeId(0),
                    collection: 3,
                    record_id: 7,
                },
                GraphEntry::Attach {
                    node_id: NodeId(5),
                    collection: 0,
                    record_id: 8,
                },
                GraphEntry::GraphHighWater {
                    next_node_id: 9,
                    next_edge_id: 12,
                },
            ],
        };
        let sequence = state.last_sequence() + 1;
        state.apply(sequence, &body).unwrap();
    }

    #[test]
    fn snapshot_load_equals_replaying_the_same_bodies() {
        let mut states: Vec<GraphState> = (0..24).map(|seed| random_graph(seed).state).collect();
        states.push(hub_graph(40, 3000).state);
        let mut with_schema = random_graph(99).state;
        add_schema_graph(&mut with_schema);
        states.push(with_schema);
        let mut only_schema = GraphState::new();
        add_schema_graph(&mut only_schema);
        states.push(only_schema);
        states.push(GraphState::new());
        for state in states {
            for chunk in [1, 3, MAX_BATCH_ENTRIES_FOR_TESTS] {
                let bodies = snapshot_bodies(&state, chunk);
                let floor = state.last_sequence() + 5;
                let expected = replayed(floor, &bodies);
                let built = GraphState::from_snapshot(floor, bodies).unwrap();
                assert_eq!(built, expected);
                built.check_invariants().unwrap();
                assert_eq!(built.last_sequence(), expected.last_sequence());
            }
        }
    }

    const MAX_BATCH_ENTRIES_FOR_TESTS: usize = super::super::super::limits::MAX_BATCH_ENTRIES;

    #[test]
    fn snapshot_rejects_inconsistent_or_non_canonical_bodies() {
        let mut state = GraphState::new();
        add_schema_graph(&mut state);
        let canonical = snapshot_bodies(&state, usize::MAX);
        let entries = canonical[0].entries.clone();
        let body = |entries: Vec<GraphEntry>| {
            vec![GraphBody {
                graph_id: GraphId(1),
                entries,
            }]
        };
        let replace = |index: usize, entry: GraphEntry| {
            let mut changed = entries.clone();
            changed[index] = entry;
            body(changed)
        };
        let remove = |index: usize| {
            let mut changed = entries.clone();
            changed.remove(index);
            body(changed)
        };
        let insert = |index: usize, entry: GraphEntry| {
            let mut changed = entries.clone();
            changed.insert(index, entry);
            body(changed)
        };
        let high_water = entries.len() - 1;
        let cases: Vec<(&str, Vec<GraphBody>)> = vec![
            ("no create", remove(0)),
            ("no high water", remove(high_water)),
            ("node IDs not ascending", replace(3, person(0, "bob", 27))),
            ("duplicate node key", replace(3, person(3, "ann", 27))),
            (
                "schema violation",
                insert(5, person(6, "dan", 1))
                    .into_iter()
                    .map(|mut body| {
                        if let GraphEntry::PutNode { properties, .. } = &mut body.entries[5] {
                            properties.remove("email");
                        }
                        body
                    })
                    .collect(),
            ),
            (
                "unique property repeated",
                insert(
                    5,
                    GraphEntry::PutNode {
                        node_id: NodeId(6),
                        key: "dan".into(),
                        labels: set(&["Person"]),
                        properties: props(&[(
                            "email",
                            PropertyValue::String("ann@example.com".into()),
                        )]),
                    },
                ),
            ),
            ("edge to a missing node", replace(5, edge(0, 0, "KNOWS", 4))),
            ("edge IDs not ascending", replace(6, edge(0, 2, "KNOWS", 5))),
            ("node after edges", insert(6, person(7, "eve", 3))),
            (
                "record attached twice",
                replace(
                    11,
                    GraphEntry::Attach {
                        node_id: NodeId(2),
                        collection: 0,
                        record_id: 7,
                    },
                ),
            ),
            (
                "node attached twice to one collection",
                insert(
                    11,
                    GraphEntry::Attach {
                        node_id: NodeId(0),
                        collection: 0,
                        record_id: 99,
                    },
                ),
            ),
            (
                "attachment of a missing node",
                replace(
                    12,
                    GraphEntry::Attach {
                        node_id: NodeId(4),
                        collection: 0,
                        record_id: 8,
                    },
                ),
            ),
            (
                "high water below a node",
                replace(
                    high_water,
                    GraphEntry::GraphHighWater {
                        next_node_id: 5,
                        next_edge_id: 12,
                    },
                ),
            ),
            (
                "high water below an edge",
                replace(
                    high_water,
                    GraphEntry::GraphHighWater {
                        next_node_id: 9,
                        next_edge_id: 10,
                    },
                ),
            ),
            (
                "mutation entry",
                insert(5, GraphEntry::DeleteNode { node_id: NodeId(2) }),
            ),
            (
                "entry after high water",
                insert(high_water + 1, person(20, "zed", 1)),
            ),
            ("schema after nodes", insert(3, entries[1].clone())),
        ];
        for (name, bodies) in cases {
            assert!(
                GraphState::from_snapshot(50, bodies).is_err(),
                "accepted: {name}"
            );
        }
        // Graphs must ascend and use distinct names.
        let mut twice = canonical.clone();
        twice.extend(canonical.clone());
        assert!(GraphState::from_snapshot(50, twice).is_err());
        let mut renamed = canonical.clone();
        renamed[0].graph_id = GraphId(0);
        let mut descending = canonical.clone();
        descending.extend(renamed);
        assert!(GraphState::from_snapshot(50, descending).is_err());
        assert_eq!(
            GraphState::from_snapshot(50, canonical).unwrap(),
            replayed(50, &snapshot_bodies(&state, usize::MAX))
        );
    }
}
