use super::*;
use serde_json::Value;

fn props(values: &[(&str, PropertyValue)]) -> Properties {
    values
        .iter()
        .map(|(name, value)| ((*name).into(), value.clone()))
        .collect()
}

fn table(rows: &[(u64, Properties)]) -> Table {
    Table::build(
        &rows
            .iter()
            .map(|(id, values)| (*id, values))
            .collect::<Vec<_>>(),
    )
    .unwrap()
}

fn equal_bits(actual: &Properties, expected: &Properties) {
    assert_eq!(
        properties::encode(actual).unwrap(),
        properties::encode(expected).unwrap()
    );
}

fn hex(text: &str) -> Vec<u8> {
    let text: String = text.split_whitespace().collect();
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

fn golden() -> Vec<u8> {
    // Independent envelope: 3 IDs, one "w" sparse column at rows 0,2.
    // ZigZag(-1)=1, ZigZag(2)=4, width=3, LSB-first payload=0x21.
    hex(include_str!(
        "../../../../tests/fixtures/compact/graph-column-integers.hex"
    ))
}

#[test]
fn independent_integer_hex_decodes_and_reencodes_exactly() {
    let bytes = golden();
    let decoded = Table::decode(&bytes).unwrap();
    for (id, expected) in [(0, Some(-1)), (7, None), (9, Some(2))] {
        let expected = expected.map_or_else(Properties::new, |n| {
            props(&[("w", PropertyValue::Integer(n))])
        });
        equal_bits(
            &decoded
                .project(id, &PropertyProjection::All)
                .unwrap()
                .unwrap(),
            &expected,
        );
    }
    assert_eq!(decoded.encode().unwrap(), bytes);
    assert_eq!(
        table(&[
            (0, props(&[("w", PropertyValue::Integer(-1))])),
            (7, Properties::new()),
            (9, props(&[("w", PropertyValue::Integer(2))])),
        ])
        .encode()
        .unwrap(),
        bytes
    );
}

#[test]
fn independent_float_and_raw_string_hex_pin_bits_and_encoding_choice() {
    let bytes = hex(include_str!(
        "../../../../tests/fixtures/compact/graph-column-floats.hex"
    ));
    let decoded = Table::decode(&bytes).unwrap();
    for (id, bits) in [(0, 0), (1, 0x8000_0000_0000_0000)] {
        equal_bits(
            &decoded
                .project(id, &PropertyProjection::All)
                .unwrap()
                .unwrap(),
            &props(&[("f", PropertyValue::Float(f64::from_bits(bits)))]),
        );
    }
    assert_eq!(decoded.encode().unwrap(), bytes);
    let bytes = hex(include_str!(
        "../../../../tests/fixtures/compact/graph-column-raw-string.hex"
    ));
    let decoded = Table::decode(&bytes).unwrap();
    equal_bits(
        &decoded
            .project(42, &PropertyProjection::All)
            .unwrap()
            .unwrap(),
        &props(&[("s", PropertyValue::String(String::new()))]),
    );
    assert_eq!(decoded.columns[0].values.kind, 0);
    assert_eq!(decoded.encode().unwrap(), bytes);
}

fn fixture_properties(values: &Value) -> Properties {
    let mut result = Properties::new();
    for value in values.as_array().unwrap() {
        let bits =
            |key| f64::from_bits(u64::from_str_radix(value[key].as_str().unwrap(), 16).unwrap());
        let property = match value["type"].as_str().unwrap() {
            "String" => PropertyValue::String(value["value"].as_str().unwrap().into()),
            "Integer" => PropertyValue::Integer(value["value"].as_i64().unwrap()),
            "Float" => PropertyValue::Float(bits("bits")),
            "Boolean" => PropertyValue::Boolean(value["value"].as_bool().unwrap()),
            "GeoPoint" => PropertyValue::GeoPoint {
                latitude: bits("latitude_bits"),
                longitude: bits("longitude_bits"),
            },
            _ => panic!("unknown independent fixture type"),
        };
        result.insert(value["name"].as_str().unwrap().into(), property);
    }
    result
}

#[test]
fn retained_independent_property_goldens_and_projections_match_bits() {
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/compact/graph-property-cases.json"
    ))
    .unwrap();
    for case in fixtures["cases"].as_array().unwrap() {
        for kind in ["nodes", "edges"] {
            let mut rows: Vec<_> = case["input"][kind]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| {
                    (
                        row["id"].as_u64().unwrap(),
                        fixture_properties(&row["properties"]),
                    )
                })
                .collect();
            rows.sort_by_key(|row| row.0);
            let encoded = table(&rows).encode().unwrap();
            let decoded = Table::decode(&encoded).unwrap();
            for expected in case["expected"][kind].as_array().unwrap() {
                let id = expected["id"].as_u64().unwrap();
                let values = fixture_properties(&expected["properties"]);
                equal_bits(
                    &decoded
                        .project(id, &PropertyProjection::All)
                        .unwrap()
                        .unwrap(),
                    &values,
                );
                assert!(
                    decoded
                        .project(id, &PropertyProjection::None)
                        .unwrap()
                        .unwrap()
                        .is_empty()
                );
                for name in values.keys().chain([&"absent".to_owned()]) {
                    let projection = PropertyProjection::Only(BTreeSet::from([name.clone()]));
                    let expected: Properties = values
                        .iter()
                        .filter(|(key, _)| *key == name)
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    equal_bits(
                        &decoded.project(id, &projection).unwrap().unwrap(),
                        &expected,
                    );
                }
            }
        }
    }
}

#[test]
fn presence_rank_boundaries_sparse_dense_and_missing_are_distinct() {
    for present in [
        vec![0, 63, 64, 65, 127, 128, 192],
        (0..193).filter(|n| n % 3 != 1).collect(),
    ] {
        let presence = Presence::build(193, &present);
        let (mode, bytes) = presence.encode();
        let decoded = Presence::decode(mode, &bytes, 193, present.len()).unwrap();
        for row in 0..194 {
            assert_eq!(decoded.rank(row), present.binary_search(&row).ok());
        }
        if present.len() == 7 {
            assert_eq!(mode, 0);
        } else {
            assert_eq!(mode, 1);
        }
    }
    let rows: Vec<_> = (0..193)
        .map(|id| {
            (
                id,
                if id % 3 == 1 {
                    Properties::new()
                } else {
                    props(&[("w", PropertyValue::Integer(id as i64))])
                },
            )
        })
        .collect();
    let stored = Table::decode(&table(&rows).encode().unwrap()).unwrap();
    for (id, expected) in rows {
        equal_bits(
            &stored
                .project(id, &PropertyProjection::All)
                .unwrap()
                .unwrap(),
            &expected,
        );
    }
    assert!(
        stored
            .project(194, &PropertyProjection::All)
            .unwrap()
            .is_none()
    );
}

#[test]
fn integer_extrema_cross_byte_packing_and_zero_width_are_exact() {
    for numbers in [
        vec![0, 0, 0],
        vec![-1, 0, 1, -2, 2, -3, 3, 4, -4, 63, -64, 64, 127, -128, 128],
        vec![
            i64::MIN,
            i64::MAX,
            9_007_199_254_740_993,
            -9_007_199_254_740_993,
        ],
    ] {
        let rows: Vec<_> = numbers
            .iter()
            .enumerate()
            .map(|(at, &n)| (at as u64, props(&[("i", PropertyValue::Integer(n))])))
            .collect();
        let stored = table(&rows);
        assert_eq!(stored.columns[0].values.kind, 1);
        if numbers.iter().all(|&n| n == 0) {
            assert_eq!(stored.columns[0].values.width, 0);
        }
        let decoded = Table::decode(&stored.encode().unwrap()).unwrap();
        for (id, expected) in rows {
            equal_bits(
                &decoded
                    .project(id, &PropertyProjection::All)
                    .unwrap()
                    .unwrap(),
                &expected,
            );
        }
    }
}

#[test]
fn string_dictionary_mixed_raw_and_raw_smaller_choices_are_explicit() {
    let mixed = table(&[
        (0, props(&[("m", PropertyValue::Integer(1))])),
        (1, props(&[("m", PropertyValue::String("1".into()))])),
    ]);
    assert_eq!(mixed.columns[0].values.kind, 0);
    let small = table(&[(0, props(&[("s", PropertyValue::String("".into()))]))]);
    assert_eq!(
        small.columns[0].values.kind, 0,
        "dictionary overhead must count"
    );
    let rows: Vec<_> = (0..128)
        .map(|id| {
            (
                id,
                props(&[
                    (
                        "s",
                        PropertyValue::String(
                            ["zebra", "alpha", "\u{96ea}", ""][id as usize % 4].into(),
                        ),
                    ),
                    ("b", PropertyValue::Boolean(id % 2 == 0)),
                ]),
            )
        })
        .collect();
    let stored = table(&rows);
    assert_eq!(stored.columns[0].values.kind, 3);
    assert_eq!(stored.columns[1].values.kind, 5);
    let decoded = Table::decode(&stored.encode().unwrap()).unwrap();
    for (id, expected) in rows {
        equal_bits(
            &decoded
                .project(id, &PropertyProjection::All)
                .unwrap()
                .unwrap(),
            &expected,
        );
    }
}

#[test]
fn float_and_geo_signed_zero_subnormal_bits_survive_columns() {
    let rows: Vec<_> = [
        0,
        1,
        0x8000_0000_0000_0000,
        0x8000_0000_0000_0001,
        0x7fef_ffff_ffff_ffff,
    ]
    .into_iter()
    .enumerate()
    .map(|(at, bits)| {
        (
            at as u64,
            props(&[
                ("f", PropertyValue::Float(f64::from_bits(bits))),
                (
                    "g",
                    PropertyValue::GeoPoint {
                        latitude: -0.0,
                        longitude: f64::from_bits(1),
                    },
                ),
            ]),
        )
    })
    .collect();
    let stored = table(&rows);
    assert_eq!(stored.columns[0].values.kind, 2);
    assert_eq!(stored.columns[1].values.kind, 4);
    let decoded = Table::decode(&stored.encode().unwrap()).unwrap();
    for (id, expected) in rows {
        equal_bits(
            &decoded
                .project(id, &PropertyProjection::All)
                .unwrap()
                .unwrap(),
            &expected,
        );
    }
}

fn state() -> GraphState {
    let mut state = GraphState::new();
    state.set_temporal(true).unwrap();
    let schema = GraphSchema::default().label(
        "Entity",
        LabelDefinition::new("columns")
            .property(
                "n",
                PropertyDefinition::new(PropertyType::Integer)
                    .required()
                    .unique()
                    .index(PropertyIndexKind::Range),
            )
            .property(
                "group",
                PropertyDefinition::new(PropertyType::String).index(PropertyIndexKind::Exact),
            ),
    );
    let mut entries = vec![
        GraphEntry::CreateGraph {
            name: "columnar".into(),
        },
        GraphEntry::PutSchema { schema },
    ];
    for (id, key) in [(0, "zulu"), (7, "alpha"), (19, "middle")] {
        entries.push(GraphEntry::PutNode {
            node_id: NodeId(id),
            key: key.into(),
            labels: BTreeSet::from(["Entity".into()]),
            properties: props(&[
                ("n", PropertyValue::Integer(id as i64)),
                ("group", PropertyValue::String("g".into())),
            ]),
        });
    }
    for (id, from, to, key) in [(0, 0, 0, "zulu"), (7, 0, 7, "edge"), (19, 7, 0, "other")] {
        entries.push(GraphEntry::PutEdge {
            edge_id: EdgeId(id),
            source: NodeId(from),
            destination: NodeId(to),
            relationship: "R".into(),
            key: Some(key.into()),
            properties: props(&[
                ("f", PropertyValue::Float(-0.0)),
                ("w", PropertyValue::Integer(id as i64)),
            ]),
        });
    }
    entries.extend([
        GraphEntry::Attach {
            node_id: NodeId(0),
            collection: 1,
            record_id: 5,
        },
        GraphEntry::Attach {
            node_id: NodeId(0),
            collection: 2,
            record_id: 71,
        },
        GraphEntry::Attach {
            node_id: NodeId(7),
            collection: 1,
            record_id: 99,
        },
        GraphEntry::NodeExpiry {
            node_id: NodeId(0),
            expires_at_ms: Some(0),
        },
        GraphEntry::EdgeExpiry {
            edge_id: EdgeId(7),
            expires_at_ms: Some(10),
        },
        GraphEntry::Temporal(TemporalEntry::PutEdge {
            edge_id: EdgeId(30),
            revision: 0,
            source: NodeId(0),
            destination: NodeId(19),
            relationship: "T".into(),
            key: Some("temporal".into()),
            segments: vec![
                Segment {
                    valid_from: None,
                    valid_to: Some(0),
                    properties: props(&[("s", PropertyValue::String("before".into()))]),
                },
                Segment {
                    valid_from: Some(3),
                    valid_to: None,
                    properties: props(&[("s", PropertyValue::String("after".into()))]),
                },
            ],
        }),
    ]);
    state
        .apply(
            1,
            &GraphBody {
                graph_id: GraphId(0),
                entries,
            },
        )
        .unwrap();
    state
}

fn selected(values: &Properties, projection: &PropertyProjection) -> Properties {
    values
        .iter()
        .filter(|(name, _)| match projection {
            PropertyProjection::All => true,
            PropertyProjection::None => false,
            PropertyProjection::Only(names) => names.contains(*name),
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

#[test]
fn owned_public_views_project_only_packed_fields_with_explicit_metadata() {
    let original = state();
    let graph = original.graph(GraphId(0)).unwrap();
    let source = GraphSource::from_graph(graph).unwrap();
    let packed = PackedFields::build(&source).unwrap();
    let projections = [
        PropertyProjection::All,
        PropertyProjection::None,
        PropertyProjection::Only(BTreeSet::from([
            "n".into(),
            "w".into(),
            "s".into(),
            "missing".into(),
        ])),
    ];
    for mut metadata in source.nodes.clone() {
        metadata.properties.clear();
        metadata.key = None;
        let resident = graph.node(NodeId(metadata.id)).unwrap();
        for projection in &projections {
            let actual = packed.physical_node_view(&metadata, projection).unwrap();
            let expected = NodeView {
                id: resident.id,
                key: resident.key.clone(),
                labels: resident.labels.clone(),
                properties: selected(&resident.properties, projection),
                revision: resident.revision,
                expires_at_ms: graph.node_expiry(resident.id),
            };
            equal_bits(&actual.properties, &expected.properties);
            assert_eq!(actual, expected);
        }
    }
    for mut metadata in source.edges.clone() {
        metadata.properties.clear();
        metadata.key = None;
        let resident = graph.edge(EdgeId(metadata.id)).unwrap();
        let segments = graph.segments(resident.id);
        let choices: Vec<_> = std::iter::once(None)
            .chain((0..segments.map_or(0, <[Segment]>::len)).map(Some))
            .collect();
        for segment in choices {
            for projection in &projections {
                let expected_properties =
                    segment.map_or(&resident.properties, |at| &segments.unwrap()[at].properties);
                let actual = packed
                    .physical_edge_view(&metadata, projection, segment)
                    .unwrap();
                let expected = EdgeView {
                    id: resident.id,
                    key: resident.key.clone(),
                    relationship: resident.relationship.clone(),
                    source: resident.source,
                    target: resident.destination,
                    source_key: graph.node(resident.source).unwrap().key.clone(),
                    target_key: graph.node(resident.destination).unwrap().key.clone(),
                    properties: selected(expected_properties, projection),
                    revision: resident.revision,
                    expires_at_ms: graph.edge_expiry(resident.id),
                };
                equal_bits(&actual.properties, &expected.properties);
                assert_eq!(actual, expected);
            }
        }
        assert!(
            packed
                .physical_edge_view(&metadata, &PropertyProjection::All, Some(99))
                .is_err()
        );
    }
}

#[test]
fn engine_fields_schema_indexes_keys_owners_and_temporal_round_trip() {
    let original = state();
    let source = GraphSource::from_graph(original.graph(GraphId(0)).unwrap()).unwrap();
    let packed = PackedFields::build(&source).unwrap();
    for key in ["zulu", "alpha", "middle", "missing", ""] {
        assert_eq!(
            packed.node_by_key(key),
            original
                .graph(GraphId(0))
                .unwrap()
                .node_by_key(key)
                .map(|n| n.id.0)
        );
    }
    for key in ["zulu", "edge", "other", "temporal", "missing"] {
        assert_eq!(
            packed.edge_by_key(key),
            original
                .graph(GraphId(0))
                .unwrap()
                .edge_by_key(key)
                .map(|e| e.id.0)
        );
    }
    assert_eq!(
        packed.attachments(0).unwrap().unwrap(),
        BTreeMap::from([(1, 5), (2, 71)])
    );
    assert_eq!(packed.owner(1, 5), Some(0));
    assert_eq!(packed.owner(2, 71), Some(0));
    assert_eq!(packed.owner(1, 99), Some(7));
    assert_eq!(packed.owner(2, 99), None);
    assert!(packed.attachments(8).unwrap().is_none());
    // Remove source fields before restoring: no decoded-field fallback is possible.
    let mut shape = source.clone();
    for node in &mut shape.nodes {
        node.properties.clear();
        node.key = None;
        node.attachments.clear();
    }
    for edge in &mut shape.edges {
        edge.properties.clear();
        edge.key = None;
        for segment in edge.segments.iter_mut().flatten() {
            segment.properties.clear();
        }
    }
    let restored = packed.restore(&shape).unwrap();
    assert_eq!(
        encode(&restored).unwrap(),
        encode(&source).unwrap(),
        "includes exact IEEE bits and all metadata"
    );
    let rebuilt = engine_state(&restored).unwrap();
    // Engine snapshot equality includes rebuilt schema property indexes.
    assert_eq!(rebuilt, original);
    let bad = GraphEntry::PutNode {
        node_id: NodeId(31),
        key: "duplicate".into(),
        labels: BTreeSet::from(["Entity".into()]),
        properties: props(&[("n", PropertyValue::Integer(7))]),
    };
    let body = GraphBody {
        graph_id: GraphId(0),
        entries: vec![bad],
    };
    assert_eq!(
        original.clone().apply(2, &body),
        rebuilt.clone().apply(2, &body)
    );
    assert!(rebuilt.clone().apply(2, &body).is_err());
    let mut wrong = shape.clone();
    wrong.sequence += 1;
    assert!(packed.restore(&wrong).is_err());
}

#[test]
fn malformed_columns_offsets_bits_and_headers_are_refused() {
    let bytes = golden();
    for cut in 0..bytes.len() {
        assert!(Table::decode(&bytes[..cut]).is_err(), "cut {cut}");
    }
    for (at, value) in [
        (0, 0),
        (8, 255),
        (12, 255),
        (24, 0),
        (43, 2),
        (44, 255),
        (48, 255),
        (52, 3),
        (60, 9),
        (61, 65),
        (62, 255),
        (66, 255),
    ] {
        let mut bad = bytes.clone();
        bad[at] = value;
        assert!(Table::decode(&bad).is_err(), "mutation at {at}");
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Table::decode(&trailing).is_err());
    assert!(Bytes::decode(&[0, 0, 0, 0, 2, 0, 0, 0, 1], 1).is_err());
    assert!(Bytes::decode(&[1, 0, 0, 0], 0).is_err());
    assert!(Presence::decode(1, &[0; 16], 1, 1).is_err());
    for value in [
        PropertyValue::Float(f64::NAN),
        PropertyValue::GeoPoint {
            latitude: 91.0,
            longitude: 0.0,
        },
    ] {
        assert!(Table::build(&[(0, &props(&[("bad", value)]))]).is_err());
    }
    assert!(Table::build(&[(0, &Properties::new()), (0, &Properties::new())]).is_err());
    assert!(raw_value(&[4, 2]).is_err());
    assert!(Values::decode(5, 33, &[0; 8], 1).is_err());
}

#[test]
fn malformed_dense_rank_dictionary_codes_and_property_limits_are_refused() {
    let ordinals: Vec<_> = (0..65).collect();
    let presence = Presence::build(65, &ordinals);
    let (mode, bytes) = presence.encode();
    assert_eq!(mode, 1);
    for at in [8, 16, 20, 24] {
        let mut bad = bytes.clone();
        bad[at] ^= 0x80;
        assert!(
            Presence::decode(mode, &bad, 65, 65).is_err(),
            "dense byte {at}"
        );
    }
    // At 64 rows, four sparse ordinals and a dense map both cost 16 bytes.
    assert_eq!(Presence::build(64, &[0, 1, 2, 3]).encode().0, 0);
    let repeated: Vec<_> = (0..32)
        .map(|id| (id, props(&[("s", PropertyValue::String("repeat".into()))])))
        .collect();
    let mut stored = table(&repeated);
    assert_eq!(stored.columns[0].values.kind, 5);
    // Corrupt the dictionary offset, not a row-owned String.
    stored.columns[0].values.data[12] = 255;
    assert!(Table::decode(&stored.encode().unwrap()).is_err());
    let texts = [
        PropertyValue::String("a".into()),
        PropertyValue::String("b".into()),
        PropertyValue::String("c".into()),
    ];
    let refs: Vec<_> = texts.iter().collect();
    let mut encoded = Values::build(&refs).unwrap();
    assert_eq!(encoded.kind, 5);
    let last = encoded.data.len() - 1;
    encoded.data[last] |= 3; // Code 3 is outside the three-entry dictionary.
    assert!(encoded.get(0).is_err());
    for invalid in [
        (0..65)
            .map(|n| (format!("p{n}"), PropertyValue::Boolean(true)))
            .collect(),
        props(&[("", PropertyValue::Integer(1))]),
        props(&[("too_long", PropertyValue::String("s".repeat(4097)))]),
        (0..17)
            .map(|n| (format!("p{n}"), PropertyValue::String("s".repeat(4096))))
            .collect(),
    ] {
        assert!(Table::build(&[(0, &invalid)]).is_err());
    }
}

#[test]
fn keyless_sources_namespace_separation_and_invalid_owners_use_existing_admission() {
    let mut source = GraphSource::from_graph(state().graph(GraphId(0)).unwrap()).unwrap();
    source.schema = GraphSchema::default();
    source.temporal_enabled = false;
    for edge in &mut source.edges {
        edge.segments = None;
    }
    source.nodes[1].key = None;
    let stored = PackedFields::build(&source).unwrap();
    assert_eq!(stored.node_by_key("zulu"), Some(0));
    assert_eq!(stored.edge_by_key("zulu"), Some(0));
    assert_eq!(stored.node_by_key("alpha"), None);
    assert_eq!(stored.node_by_key(""), None);
    assert!(stored.node_keys.get(1).unwrap().is_none());
    assert!(matches!(
        stored.physical_node_view(&source.nodes[1], &PropertyProjection::All),
        Err(CodecError::UnsupportedSource("keyless node view"))
    ));
    let mut duplicate = source.clone();
    duplicate.nodes[1].attachments.insert(2, 71);
    assert!(PackedFields::build(&duplicate).is_err());
    let mut duplicate = source.clone();
    duplicate.edges[1].key = Some("zulu".into());
    assert!(PackedFields::build(&duplicate).is_err());
    source.nodes.clear();
    source.edges.clear();
    let empty = PackedFields::build(&source).unwrap();
    assert_eq!(empty.node_by_key("zulu"), None);
    assert_eq!(empty.edge_by_key("zulu"), None);
    assert_eq!(
        encode(&empty.restore(&source).unwrap()).unwrap(),
        encode(&source).unwrap()
    );
}

#[test]
fn seeded_sparse_mixed_tables_match_independent_rows_and_projection() {
    for seed in [17u64, 941, 0x4d59_5df4_d0f3_3173] {
        let mut random = seed;
        let mut rows = Vec::new();
        for id in 0..257 {
            let mut values = Properties::new();
            for name in ["a", "prefix", "prefix_long", "\u{00e9}", "\u{8282}"] {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let value = match random % 7 {
                    0 => continue,
                    1 => PropertyValue::Integer(random as i64),
                    2 => PropertyValue::Float(f64::from_bits(random & 0x8000_0000_0000_0000)),
                    3 => PropertyValue::Boolean(random & 1 != 0),
                    4 => PropertyValue::GeoPoint {
                        latitude: -0.0,
                        longitude: (random % 180) as f64,
                    },
                    _ => PropertyValue::String(format!("s{}", random % 3)),
                };
                values.insert(name.into(), value);
            }
            rows.push((id * 17, values));
        }
        let stored = table(&rows);
        let bytes = stored.encode().unwrap();
        let decoded = Table::decode(&bytes).unwrap();
        assert_eq!(decoded.encode().unwrap(), bytes);
        for (id, expected) in &rows {
            equal_bits(
                &decoded
                    .project(*id, &PropertyProjection::All)
                    .unwrap()
                    .unwrap(),
                expected,
            );
            let names = BTreeSet::from(["a".into(), "\u{8282}".into(), "missing".into()]);
            let expected: Properties = expected
                .iter()
                .filter(|(name, _)| names.contains(*name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            equal_bits(
                &decoded
                    .project(*id, &PropertyProjection::Only(names))
                    .unwrap()
                    .unwrap(),
                &expected,
            );
        }
    }
}

#[test]
fn counted_fields_payload_is_informational_not_total_graph_residency() {
    let mut source = GraphSource::from_graph(state().graph(GraphId(0)).unwrap()).unwrap();
    source.schema = GraphSchema::default();
    source.temporal_enabled = false;
    source.next_node_id = 1000;
    source.next_edge_id = 10_000;
    source.nodes = (0..1000)
        .map(|id| SourceNode {
            id,
            key: Some(format!("n{id:07}")),
            labels: vec![],
            revision: 1,
            deadline: None,
            properties: Properties::new(),
            attachments: BTreeMap::new(),
        })
        .collect();
    source.edges = (0..10_000)
        .map(|id| SourceEdge {
            id,
            source: id % 1000,
            target: (id + 1) % 1000,
            relationship: "R".into(),
            key: None,
            revision: 1,
            deadline: None,
            properties: props(&[("w", PropertyValue::Integer((id % 100) as i64))]),
            segments: None,
        })
        .collect();
    let stored = PackedFields::build(&source).unwrap();
    println!(
        "informational_fields_only E=10000 N=1000 payload_bytes={} retained_bytes={} excludes_topology_schema_labels_deadlines_allocator_metadata_and_scratch",
        stored.payload_bytes().unwrap(),
        stored.retained_bytes()
    );
    assert!(stored.retained_bytes() >= stored.payload_bytes().unwrap());
    assert_eq!(stored.payload_bytes().unwrap(), 199_980);
    assert_eq!(stored.edges.columns[0].values.width, 8);
}
