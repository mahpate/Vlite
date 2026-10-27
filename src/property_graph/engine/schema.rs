//! Declared graph schemas (EPIC-008, REQ-030..REQ-033).
//!
//! A [`GraphSchema`] is stored per graph by a `PutSchema` entry (tag 12) and
//! replaces the previous schema; an empty schema removes every rule. Rules
//! apply only to declared labels, relationship types and properties:
//! undeclared ones stay open-world. After each entry of a batch is applied
//! the state checks the rules the entry can affect ([`check_entry`]), so a
//! violation fails the whole batch and nothing is written (FM-013).
//!
//! - Label properties: `type` (the value has the declared type), `required`
//!   (present) and `unique` (no other node with the label has an equal
//!   value).
//! - Relationship types: property `type`/`required`, `endpoints` (when pairs
//!   are declared, the source carries the first label and the destination
//!   the second label of at least one pair) and `unique_triple` (at most one
//!   edge per source, type and destination).
//!
//! Declared `Exact` and `Range` indexes, and the postings of unique
//! properties, follow the `src\metadata_index.rs` design: value → node ID
//! postings, with integer and float range maps. They are derived: rebuilt
//! from the nodes whenever the schema changes and on replay.
//!
//! Payload of the `PutSchema` entry (all lists strictly ascending):
//! `label_count u16`, per label `name_len u8`, `name`, `desc_len u16`,
//! `desc`, properties; `relationship_count u16`, per type `name_len u8`,
//! `name`, `desc_len u16`, `desc`, `multiplicity u8` (1 `Multi`,
//! 2 `UniqueTriple`), `endpoint_count u8`, per pair `len u8`, `source`,
//! `len u8`, `destination`, then properties. Properties: `count u8`, per
//! property `name_len u16`, `name`, `type u8` (1 String, 2 Integer, 3 Float,
//! 4 Boolean, 5 GeoPoint), `flags u8` (bit 0 required, bit 1 unique, bit 2
//! exact index, bit 3 range index; other bits zero, at most one index bit).

use super::codec::GraphEntry;
use super::error::GraphError;
use super::limits::{
    MAX_INDEXED_PROPERTIES, MAX_OBSERVED_SCHEMA_BUDGET, MAX_SCHEMA_DESCRIPTION_BYTES,
    MAX_SCHEMA_ENDPOINTS, MAX_SCHEMA_LABELS, MAX_SCHEMA_PROPERTIES, MAX_SCHEMA_RELATIONSHIPS,
};
use super::model::{
    Direction, GraphEdge, GraphNode, NodeId, Properties, PropertyCondition, PropertyValue,
    validate_label, validate_relationship_type,
};
use super::properties::validate_field_name;
use super::state::Graph;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

/// The type of a declared property: one `MetadataValue` variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PropertyType {
    String,
    Integer,
    Float,
    Boolean,
    GeoPoint,
}

impl PropertyType {
    /// The type of `value`.
    pub fn of(value: &PropertyValue) -> Self {
        match value {
            PropertyValue::String(_) => Self::String,
            PropertyValue::Integer(_) => Self::Integer,
            PropertyValue::Float(_) => Self::Float,
            PropertyValue::Boolean(_) => Self::Boolean,
            PropertyValue::GeoPoint { .. } => Self::GeoPoint,
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::String => 1,
            Self::Integer => 2,
            Self::Float => 3,
            Self::Boolean => 4,
            Self::GeoPoint => 5,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::String,
            2 => Self::Integer,
            3 => Self::Float,
            4 => Self::Boolean,
            5 => Self::GeoPoint,
            _ => return None,
        })
    }
}

/// A declared property index on (label, property) (REQ-033).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyIndexKind {
    /// Value → node ID postings, used by `Eq` conditions.
    Exact,
    /// Ordered numeric postings, used by `Range` conditions; `Integer` and
    /// `Float` properties only.
    Range,
}

/// One declared property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PropertyDefinition {
    pub value_type: PropertyType,
    pub required: bool,
    /// Label properties only.
    pub unique: bool,
    /// Label properties only.
    pub index: Option<PropertyIndexKind>,
}

impl PropertyDefinition {
    /// An optional, non-unique, unindexed property of `value_type`.
    pub fn new(value_type: PropertyType) -> Self {
        Self {
            value_type,
            required: false,
            unique: false,
            index: None,
        }
    }

    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    #[must_use]
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    #[must_use]
    pub fn index(mut self, kind: PropertyIndexKind) -> Self {
        self.index = Some(kind);
        self
    }

    /// Whether the property keeps value postings (unique or exact index).
    fn exact_postings(&self) -> bool {
        self.unique || self.index == Some(PropertyIndexKind::Exact)
    }

    fn range_postings(&self) -> bool {
        self.index == Some(PropertyIndexKind::Range)
    }

    fn flags(&self) -> u8 {
        u8::from(self.required)
            | (u8::from(self.unique) << 1)
            | match self.index {
                None => 0,
                Some(PropertyIndexKind::Exact) => 4,
                Some(PropertyIndexKind::Range) => 8,
            }
    }
}

/// A declared label.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LabelDefinition {
    /// At most 1 KiB of UTF-8.
    pub description: String,
    pub properties: BTreeMap<String, PropertyDefinition>,
}

impl LabelDefinition {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            properties: BTreeMap::new(),
        }
    }

    /// Declares (or replaces) one property.
    #[must_use]
    pub fn property(mut self, name: impl Into<String>, definition: PropertyDefinition) -> Self {
        self.properties.insert(name.into(), definition);
        self
    }
}

/// How many edges of one type may connect the same ordered node pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Multiplicity {
    #[default]
    Multi,
    /// At most one edge per (source, type, destination); enables `ensure_edge`.
    UniqueTriple,
}

/// A declared relationship type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RelationshipDefinition {
    /// At most 1 KiB of UTF-8.
    pub description: String,
    /// Allowed (source label, destination label) pairs; empty allows any.
    pub endpoints: BTreeSet<(String, String)>,
    /// Neither unique nor indexed.
    pub properties: BTreeMap<String, PropertyDefinition>,
    pub multiplicity: Multiplicity,
}

impl RelationshipDefinition {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            ..Self::default()
        }
    }

    /// Allows edges from a node with `source` to a node with `destination`.
    #[must_use]
    pub fn endpoint(mut self, source: impl Into<String>, destination: impl Into<String>) -> Self {
        self.endpoints.insert((source.into(), destination.into()));
        self
    }

    #[must_use]
    pub fn multiplicity(mut self, multiplicity: Multiplicity) -> Self {
        self.multiplicity = multiplicity;
        self
    }

    /// Declares (or replaces) one property.
    #[must_use]
    pub fn property(mut self, name: impl Into<String>, definition: PropertyDefinition) -> Self {
        self.properties.insert(name.into(), definition);
        self
    }
}

/// The declared schema of one graph. The default (empty) schema declares
/// nothing and enforces nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphSchema {
    pub labels: BTreeMap<String, LabelDefinition>,
    pub relationships: BTreeMap<String, RelationshipDefinition>,
}

fn invalid(reason: &'static str) -> GraphError {
    GraphError::InvalidSchema(reason)
}

fn validate_description(description: &str) -> Result<(), GraphError> {
    if description.len() > MAX_SCHEMA_DESCRIPTION_BYTES {
        return Err(invalid("descriptions are at most 1024 bytes"));
    }
    Ok(())
}

fn validate_properties(
    properties: &BTreeMap<String, PropertyDefinition>,
    relationship: bool,
) -> Result<usize, GraphError> {
    if properties.len() > MAX_SCHEMA_PROPERTIES {
        return Err(invalid("a definition declares at most 64 properties"));
    }
    let mut postings = 0;
    for (name, definition) in properties {
        validate_field_name(name).map_err(|_| invalid("invalid property name"))?;
        if definition.range_postings()
            && !matches!(
                definition.value_type,
                PropertyType::Integer | PropertyType::Float
            )
        {
            return Err(invalid("range indexes require integer or float properties"));
        }
        if definition.unique || definition.index.is_some() {
            if relationship {
                return Err(invalid(
                    "relationship properties cannot be unique or indexed",
                ));
            }
            postings += 1;
        }
    }
    Ok(postings)
}

#[cfg(feature = "json")]
impl PropertyType {
    /// The JSON name used by every tool surface (`"string"`, `"geopoint"`, ...).
    #[doc(hidden)]
    pub fn json_name(self) -> &'static str {
        match self {
            PropertyType::String => "string",
            PropertyType::Integer => "integer",
            PropertyType::Float => "float",
            PropertyType::Boolean => "boolean",
            PropertyType::GeoPoint => "geopoint",
        }
    }
}

#[cfg(feature = "json")]
fn property_definitions_json(
    properties: &BTreeMap<String, PropertyDefinition>,
) -> serde_json::Value {
    properties
        .iter()
        .map(|(name, definition)| {
            let index = definition.index.map(|kind| match kind {
                PropertyIndexKind::Exact => "exact",
                PropertyIndexKind::Range => "range",
            });
            (
                name.clone(),
                serde_json::json!({
                    "type": definition.value_type.json_name(),
                    "required": definition.required,
                    "unique": definition.unique,
                    "index": index,
                }),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into()
}

#[cfg(feature = "json")]
impl GraphSchema {
    /// The declared schema in the `PUT /v1/graphs/{graph}/schema` request
    /// shape (round-trips through every tool surface).
    #[doc(hidden)]
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::{Map, Value, json};
        let labels: Map<String, Value> = self
            .labels
            .iter()
            .map(|(name, label)| {
                (
                    name.clone(),
                    json!({
                        "description": label.description,
                        "properties": property_definitions_json(&label.properties),
                    }),
                )
            })
            .collect();
        let relationships: Map<String, Value> = self
            .relationships
            .iter()
            .map(|(name, relationship)| {
                let endpoints: Value = relationship
                    .endpoints
                    .iter()
                    .map(|(source, destination)| {
                        json!({"source": source, "destination": destination})
                    })
                    .collect();
                let multiplicity = match relationship.multiplicity {
                    Multiplicity::Multi => "multi",
                    Multiplicity::UniqueTriple => "unique_triple",
                };
                (
                    name.clone(),
                    json!({
                        "description": relationship.description,
                        "endpoints": endpoints,
                        "multiplicity": multiplicity,
                        "properties": property_definitions_json(&relationship.properties),
                    }),
                )
            })
            .collect();
        json!({"labels": labels, "relationships": relationships})
    }
}

impl GraphSchema {
    /// Declares (or replaces) one label.
    #[must_use]
    pub fn label(mut self, name: impl Into<String>, definition: LabelDefinition) -> Self {
        self.labels.insert(name.into(), definition);
        self
    }

    /// Declares (or replaces) one relationship type.
    #[must_use]
    pub fn relationship(
        mut self,
        name: impl Into<String>,
        definition: RelationshipDefinition,
    ) -> Self {
        self.relationships.insert(name.into(), definition);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.labels.is_empty() && self.relationships.is_empty()
    }

    /// Checks every rule that does not depend on graph data.
    pub fn validate(&self) -> Result<(), GraphError> {
        if self.labels.len() > MAX_SCHEMA_LABELS {
            return Err(invalid("a schema declares at most 256 labels"));
        }
        if self.relationships.len() > MAX_SCHEMA_RELATIONSHIPS {
            return Err(invalid("a schema declares at most 256 relationship types"));
        }
        let mut postings = 0;
        for (name, label) in &self.labels {
            validate_label(name).map_err(|_| invalid("invalid label name"))?;
            validate_description(&label.description)?;
            postings += validate_properties(&label.properties, false)?;
        }
        if postings > MAX_INDEXED_PROPERTIES {
            return Err(invalid(
                "a schema has at most 32 unique or indexed properties",
            ));
        }
        for (name, relationship) in &self.relationships {
            validate_relationship_type(name)
                .map_err(|_| invalid("invalid relationship type name"))?;
            validate_description(&relationship.description)?;
            if relationship.endpoints.len() > MAX_SCHEMA_ENDPOINTS {
                return Err(invalid("a relationship type declares at most 64 endpoints"));
            }
            for (source, destination) in &relationship.endpoints {
                validate_label(source).map_err(|_| invalid("invalid endpoint label"))?;
                validate_label(destination).map_err(|_| invalid("invalid endpoint label"))?;
            }
            validate_properties(&relationship.properties, true)?;
        }
        Ok(())
    }

    /// Exact encoded payload length of a validated schema.
    pub(super) fn encoded_len(&self) -> usize {
        fn properties_len(properties: &BTreeMap<String, PropertyDefinition>) -> usize {
            1 + properties
                .keys()
                .map(|name| 2 + name.len() + 2)
                .sum::<usize>()
        }
        let labels: usize = self
            .labels
            .iter()
            .map(|(name, label)| {
                1 + name.len() + 2 + label.description.len() + properties_len(&label.properties)
            })
            .sum();
        let relationships: usize = self
            .relationships
            .iter()
            .map(|(name, relationship)| {
                1 + name.len()
                    + 2
                    + relationship.description.len()
                    + 1
                    + 1
                    + relationship
                        .endpoints
                        .iter()
                        .map(|(source, destination)| 2 + source.len() + destination.len())
                        .sum::<usize>()
                    + properties_len(&relationship.properties)
            })
            .sum();
        2 + labels + 2 + relationships
    }

    /// Appends the payload of a validated schema.
    pub(super) fn encode_into(&self, bytes: &mut Vec<u8>) {
        fn put_properties(properties: &BTreeMap<String, PropertyDefinition>, bytes: &mut Vec<u8>) {
            bytes.push(properties.len() as u8);
            for (name, definition) in properties {
                bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
                bytes.extend_from_slice(name.as_bytes());
                bytes.push(definition.value_type.code());
                bytes.push(definition.flags());
            }
        }
        fn put_text(text: &str, bytes: &mut Vec<u8>) {
            bytes.extend_from_slice(&(text.len() as u16).to_le_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        fn put_name(name: &str, bytes: &mut Vec<u8>) {
            bytes.push(name.len() as u8);
            bytes.extend_from_slice(name.as_bytes());
        }
        bytes.extend_from_slice(&(self.labels.len() as u16).to_le_bytes());
        for (name, label) in &self.labels {
            put_name(name, bytes);
            put_text(&label.description, bytes);
            put_properties(&label.properties, bytes);
        }
        bytes.extend_from_slice(&(self.relationships.len() as u16).to_le_bytes());
        for (name, relationship) in &self.relationships {
            put_name(name, bytes);
            put_text(&relationship.description, bytes);
            bytes.push(match relationship.multiplicity {
                Multiplicity::Multi => 1,
                Multiplicity::UniqueTriple => 2,
            });
            bytes.push(relationship.endpoints.len() as u8);
            for (source, destination) in &relationship.endpoints {
                put_name(source, bytes);
                put_name(destination, bytes);
            }
            put_properties(&relationship.properties, bytes);
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], GraphError> {
        let (value, rest) = self
            .bytes
            .split_at_checked(n)
            .ok_or(GraphError::Corrupt("truncated schema payload"))?;
        self.bytes = rest;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8, GraphError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, GraphError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }
    fn string(&mut self, len: usize) -> Result<String, GraphError> {
        std::str::from_utf8(self.take(len)?)
            .map(str::to_owned)
            .map_err(|_| GraphError::Corrupt("invalid UTF-8"))
    }
    fn name(&mut self) -> Result<String, GraphError> {
        let len = self.u8()? as usize;
        self.string(len)
    }
    fn text(&mut self) -> Result<String, GraphError> {
        let len = self.u16()? as usize;
        self.string(len)
    }
    fn properties(&mut self) -> Result<BTreeMap<String, PropertyDefinition>, GraphError> {
        let count = self.u8()? as usize;
        let mut properties = BTreeMap::new();
        for _ in 0..count {
            let name = self.text()?;
            let value_type = PropertyType::from_code(self.u8()?)
                .ok_or(GraphError::Corrupt("unknown schema property type"))?;
            let flags = self.u8()?;
            let index = match flags & 12 {
                0 => None,
                4 => Some(PropertyIndexKind::Exact),
                8 => Some(PropertyIndexKind::Range),
                _ => return Err(GraphError::Corrupt("schema property has two indexes")),
            };
            if flags & !15 != 0 {
                return Err(GraphError::Corrupt("unknown schema property flags"));
            }
            ascending(properties.last_key_value().map(|(k, _)| k), &name)?;
            properties.insert(
                name,
                PropertyDefinition {
                    value_type,
                    required: flags & 1 != 0,
                    unique: flags & 2 != 0,
                    index,
                },
            );
        }
        Ok(properties)
    }
}

fn ascending<T: Ord>(last: Option<&T>, next: &T) -> Result<(), GraphError> {
    if last.is_some_and(|last| last >= next) {
        Err(GraphError::Corrupt(
            "schema names must be strictly ascending",
        ))
    } else {
        Ok(())
    }
}

/// Decodes one `PutSchema` payload, which must be consumed exactly. The
/// caller validates the result.
pub(super) fn decode(payload: &[u8]) -> Result<GraphSchema, GraphError> {
    let mut r = Reader { bytes: payload };
    let mut schema = GraphSchema::default();
    for _ in 0..r.u16()? {
        let name = r.name()?;
        ascending(schema.labels.last_key_value().map(|(k, _)| k), &name)?;
        let description = r.text()?;
        let properties = r.properties()?;
        schema.labels.insert(
            name,
            LabelDefinition {
                description,
                properties,
            },
        );
    }
    for _ in 0..r.u16()? {
        let name = r.name()?;
        ascending(schema.relationships.last_key_value().map(|(k, _)| k), &name)?;
        let description = r.text()?;
        let multiplicity = match r.u8()? {
            1 => Multiplicity::Multi,
            2 => Multiplicity::UniqueTriple,
            _ => return Err(GraphError::Corrupt("unknown multiplicity")),
        };
        let mut endpoints = BTreeSet::new();
        for _ in 0..r.u8()? {
            let pair = (r.name()?, r.name()?);
            ascending(endpoints.last(), &pair)?;
            endpoints.insert(pair);
        }
        let properties = r.properties()?;
        schema.relationships.insert(
            name,
            RelationshipDefinition {
                description,
                endpoints,
                properties,
                multiplicity,
            },
        );
    }
    if !r.bytes.is_empty() {
        return Err(GraphError::Corrupt("trailing schema payload bytes"));
    }
    Ok(schema)
}

/// `f64` ordered by `total_cmp` after `-0.0` is normalized to `0.0`, so it
/// orders and compares exactly like `partial_cmp` on non-NaN values.
#[derive(Clone, Copy, Debug)]
pub(super) struct OrdF64(f64);

impl OrdF64 {
    fn new(value: f64) -> Self {
        Self(if value == 0.0 { 0.0 } else { value })
    }
}

impl PartialEq for OrdF64 {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for OrdF64 {}

impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// A property value keyed so that key equality is `PropertyValue` equality.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum EqKey {
    String(String),
    Integer(i64),
    Float(OrdF64),
    Boolean(bool),
    GeoPoint(OrdF64, OrdF64),
}

impl EqKey {
    fn new(value: &PropertyValue) -> Self {
        match value {
            PropertyValue::String(value) => Self::String(value.clone()),
            PropertyValue::Integer(value) => Self::Integer(*value),
            PropertyValue::Float(value) => Self::Float(OrdF64::new(*value)),
            PropertyValue::Boolean(value) => Self::Boolean(*value),
            PropertyValue::GeoPoint {
                latitude,
                longitude,
            } => Self::GeoPoint(OrdF64::new(*latitude), OrdF64::new(*longitude)),
        }
    }
}

/// Postings of one (label, property): node IDs by equal value (unique or
/// exact-indexed properties) and by integer or float value (range-indexed
/// properties). Empty sets are pruned.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PropertyIndex {
    values: BTreeMap<EqKey, BTreeSet<NodeId>>,
    ints: BTreeMap<i64, BTreeSet<NodeId>>,
    floats: BTreeMap<OrdF64, BTreeSet<NodeId>>,
}

fn post<K: Ord>(map: &mut BTreeMap<K, BTreeSet<NodeId>>, key: K, id: NodeId, insert: bool) {
    if insert {
        map.entry(key).or_default().insert(id);
    } else if let Some(set) = map.get_mut(&key) {
        set.remove(&id);
        if set.is_empty() {
            map.remove(&key);
        }
    }
}

fn range_bound<T: Copy>(bound: Option<T>) -> Bound<T> {
    bound.map_or(Bound::Unbounded, Bound::Included)
}

impl PropertyIndex {
    fn is_empty(&self) -> bool {
        self.values.is_empty() && self.ints.is_empty() && self.floats.is_empty()
    }

    fn update(&mut self, definition: &PropertyDefinition, value: &PropertyValue, id: NodeId) {
        self.post(definition, value, id, true);
    }

    fn post(
        &mut self,
        definition: &PropertyDefinition,
        value: &PropertyValue,
        id: NodeId,
        add: bool,
    ) {
        if definition.exact_postings() {
            post(&mut self.values, EqKey::new(value), id, add);
        }
        if definition.range_postings() {
            match value {
                PropertyValue::Integer(value) => post(&mut self.ints, *value, id, add),
                PropertyValue::Float(value) => {
                    post(&mut self.floats, OrdF64::new(*value), id, add);
                }
                _ => {}
            }
        }
    }

    /// Nodes whose value equals `value`.
    pub(super) fn equal(&self, value: &PropertyValue) -> Option<&BTreeSet<NodeId>> {
        self.values.get(&EqKey::new(value))
    }

    /// Node IDs whose value is within the inclusive bounds (validated: the
    /// same numeric type, `min <= max`), ascending.
    pub(super) fn range(
        &self,
        min: Option<&PropertyValue>,
        max: Option<&PropertyValue>,
    ) -> BTreeSet<NodeId> {
        let float = |value: Option<&PropertyValue>| match value {
            Some(PropertyValue::Float(value)) => Some(OrdF64::new(*value)),
            _ => None,
        };
        let int = |value: Option<&PropertyValue>| match value {
            Some(PropertyValue::Integer(value)) => Some(*value),
            _ => None,
        };
        let is_float = matches!(min.or(max), Some(PropertyValue::Float(_)));
        if is_float {
            self.floats
                .range((range_bound(float(min)), range_bound(float(max))))
                .flat_map(|(_, ids)| ids.iter().copied())
                .collect()
        } else {
            self.ints
                .range((range_bound(int(min)), range_bound(int(max))))
                .flat_map(|(_, ids)| ids.iter().copied())
                .collect()
        }
    }
}

/// Adds (`add`) or removes `node`'s postings in every index of `schema`.
pub(super) fn index_node(
    schema: &GraphSchema,
    indexes: &mut BTreeMap<(String, String), PropertyIndex>,
    node: &GraphNode,
    add: bool,
) {
    if schema.labels.is_empty() {
        return;
    }
    for label in &node.labels {
        let Some(definition) = schema.labels.get(label) else {
            continue;
        };
        for (field, property) in &definition.properties {
            if !(property.exact_postings() || property.range_postings()) {
                continue;
            }
            let Some(value) = node.properties.get(field) else {
                continue;
            };
            let key = (label.clone(), field.clone());
            if add {
                indexes
                    .entry(key)
                    .or_default()
                    .update(property, value, node.id);
            } else if let Some(index) = indexes.get_mut(&key) {
                index.post(property, value, node.id, false);
                if index.is_empty() {
                    indexes.remove(&key);
                }
            }
        }
    }
}

/// The index `find_nodes` uses for `label` and `conditions`: the first `Eq`
/// condition on a property with value postings, or the first `Range`
/// condition on a range-indexed property.
pub(super) fn usable_index<'c>(
    schema: &GraphSchema,
    label: &str,
    conditions: &'c [PropertyCondition],
) -> Option<&'c PropertyCondition> {
    let definition = schema.labels.get(label)?;
    conditions.iter().find(|condition| match condition {
        PropertyCondition::Eq { field, .. } => definition
            .properties
            .get(field)
            .is_some_and(PropertyDefinition::exact_postings),
        PropertyCondition::Range { field, .. } => definition
            .properties
            .get(field)
            .is_some_and(PropertyDefinition::range_postings),
        PropertyCondition::GeoRadius { .. } => false,
    })
}

fn violation(rule: &'static str, element: &str, property: Option<&str>) -> GraphError {
    GraphError::SchemaViolation {
        rule,
        element: element.to_owned(),
        property: property.map(str::to_owned),
    }
}

/// `type` and `required` rules of `properties` against `declared`.
fn check_properties(
    element: &str,
    declared: &BTreeMap<String, PropertyDefinition>,
    properties: &Properties,
    mut unique: impl FnMut(&str, &PropertyDefinition, &PropertyValue) -> bool,
) -> Result<(), GraphError> {
    for (field, definition) in declared {
        match properties.get(field) {
            Some(value) => {
                if PropertyType::of(value) != definition.value_type {
                    return Err(violation("type", element, Some(field)));
                }
                if definition.unique && !unique(field, definition, value) {
                    return Err(violation("unique", element, Some(field)));
                }
            }
            None if definition.required => {
                return Err(violation("required", element, Some(field)));
            }
            None => {}
        }
    }
    Ok(())
}

fn check_node(graph: &Graph, node: &GraphNode) -> Result<(), GraphError> {
    let schema = graph.schema();
    for label in &node.labels {
        let Some(definition) = schema.labels.get(label) else {
            continue;
        };
        check_properties(
            label,
            &definition.properties,
            &node.properties,
            |field, _, value| {
                graph
                    .property_index(label, field)
                    .and_then(|index| index.equal(value))
                    .is_none_or(|ids| ids.len() <= 1)
            },
        )?;
    }
    Ok(())
}

fn check_endpoints(graph: &Graph, edge: &GraphEdge) -> Result<(), GraphError> {
    let Some(definition) = graph.schema().relationships.get(&edge.relationship) else {
        return Ok(());
    };
    if definition.endpoints.is_empty() {
        return Ok(());
    }
    let labels = |id| graph.node(id).map(|node: &GraphNode| &node.labels);
    let (Some(source), Some(destination)) = (labels(edge.source), labels(edge.destination)) else {
        return Err(GraphError::InvariantViolation("dangling edge"));
    };
    if definition
        .endpoints
        .iter()
        .any(|(s, d)| source.contains(s) && destination.contains(d))
    {
        Ok(())
    } else {
        Err(violation("endpoints", &edge.relationship, None))
    }
}

fn check_edge(graph: &Graph, edge: &GraphEdge, structure: bool) -> Result<(), GraphError> {
    let Some(definition) = graph.schema().relationships.get(&edge.relationship) else {
        return Ok(());
    };
    check_properties(
        &edge.relationship,
        &definition.properties,
        &edge.properties,
        |_, _, _| true,
    )?;
    if structure {
        check_endpoints(graph, edge)?;
        if definition.multiplicity == Multiplicity::UniqueTriple
            && graph
                .edges_between(edge.source, &edge.relationship, edge.destination)
                .nth(1)
                .is_some()
        {
            return Err(violation("unique_triple", &edge.relationship, None));
        }
    }
    Ok(())
}

/// Checks the declared rules `entry`, just applied to `graph`, can affect.
pub(super) fn check_entry(graph: &Graph, entry: &GraphEntry) -> Result<(), GraphError> {
    match entry {
        GraphEntry::PutNode { node_id, .. } | GraphEntry::PatchNode { node_id, .. } => {
            let node = graph
                .node(*node_id)
                .ok_or(GraphError::InvariantViolation("applied node is missing"))?;
            check_node(graph, node)?;
            for id in graph.edges_of(*node_id, Direction::Both) {
                if let Some(edge) = graph.edge(id) {
                    check_endpoints(graph, edge)?;
                }
            }
            Ok(())
        }
        GraphEntry::PutEdge { edge_id, .. } | GraphEntry::PatchEdge { edge_id, .. } => {
            let edge = graph
                .edge(*edge_id)
                .ok_or(GraphError::InvariantViolation("applied edge is missing"))?;
            check_edge(graph, edge, matches!(entry, GraphEntry::PutEdge { .. }))
        }
        GraphEntry::PutSchema { .. } => {
            graph.nodes().try_for_each(|node| check_node(graph, node))?;
            graph
                .edges()
                .try_for_each(|edge| check_edge(graph, edge, true))
        }
        _ => Ok(()),
    }
}

/// Whether an item of a schema summary is declared, observed or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaOrigin {
    Declared,
    Observed,
    Both,
}

impl SchemaOrigin {
    fn of(declared: bool, count: u64) -> Self {
        match (declared, count > 0) {
            (true, true) => Self::Both,
            (true, false) => Self::Declared,
            _ => Self::Observed,
        }
    }
}

/// A property of a label or relationship type in a summary.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObservedProperty {
    pub name: String,
    pub origin: SchemaOrigin,
    /// Scanned entities with the property.
    pub count: u64,
    /// Observed value types, ascending; the declared type when unobserved.
    pub types: Vec<PropertyType>,
}

/// A label in a summary.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObservedLabel {
    pub name: String,
    pub origin: SchemaOrigin,
    /// Scanned nodes with the label.
    pub count: u64,
    pub properties: Vec<ObservedProperty>,
}

/// A (source label, destination label) pair of a relationship type.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObservedEndpoint {
    pub source: String,
    pub destination: String,
    pub origin: SchemaOrigin,
    /// Scanned edges whose source has `source` and destination has
    /// `destination`.
    pub count: u64,
}

/// A relationship type in a summary.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObservedRelationship {
    pub name: String,
    pub origin: SchemaOrigin,
    /// Scanned edges of the type.
    pub count: u64,
    pub endpoints: Vec<ObservedEndpoint>,
    pub properties: Vec<ObservedProperty>,
}

/// REQ-031: the declared schema merged with what a bounded scan observed.
/// Nodes are scanned in ID order, then edges in ID order, until the budget
/// is spent; `complete` is false when entities were left unscanned.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObservedSchema {
    pub complete: bool,
    pub entities_scanned: u64,
    /// Ascending by name.
    pub labels: Vec<ObservedLabel>,
    /// Ascending by name.
    pub relationships: Vec<ObservedRelationship>,
}

#[derive(Default)]
struct Tally {
    count: u64,
    properties: BTreeMap<String, (u64, BTreeSet<PropertyType>)>,
}

impl Tally {
    fn add(&mut self, properties: &Properties) {
        self.count += 1;
        for (name, value) in properties {
            let entry = self.properties.entry(name.clone()).or_default();
            entry.0 += 1;
            entry.1.insert(PropertyType::of(value));
        }
    }

    fn summarize(
        &self,
        declared: Option<&BTreeMap<String, PropertyDefinition>>,
    ) -> Vec<ObservedProperty> {
        let empty = BTreeMap::new();
        let declared = declared.unwrap_or(&empty);
        let names: BTreeSet<&String> = self.properties.keys().chain(declared.keys()).collect();
        names
            .into_iter()
            .map(|name| {
                let (count, types) = self.properties.get(name).cloned().unwrap_or_default();
                let definition = declared.get(name);
                let types = if count == 0 {
                    definition.map(|d| d.value_type).into_iter().collect()
                } else {
                    types.into_iter().collect()
                };
                ObservedProperty {
                    name: name.clone(),
                    origin: SchemaOrigin::of(definition.is_some(), count),
                    count,
                    types,
                }
            })
            .collect()
    }
}

impl Graph {
    /// The REQ-031 summary, scanning at most `budget` entities (PD-011).
    pub fn observed_schema(&self, budget: u64) -> Result<ObservedSchema, GraphError> {
        if budget == 0 {
            return Err(GraphError::InvalidQuery(
                "the observed schema budget must be at least 1",
            ));
        }
        if budget > MAX_OBSERVED_SCHEMA_BUDGET {
            return Err(GraphError::LimitExceedsCeiling {
                limit: "observed_schema_budget",
                requested: budget,
                ceiling: MAX_OBSERVED_SCHEMA_BUDGET,
            });
        }
        let total = (self.node_count() as u64).saturating_add(self.edge_count() as u64);
        let mut labels: BTreeMap<&str, Tally> = BTreeMap::new();
        let mut relationships: BTreeMap<&str, Tally> = BTreeMap::new();
        let mut endpoints: BTreeMap<(&str, &str, &str), u64> = BTreeMap::new();
        let mut scanned = 0u64;
        for node in self.nodes().take(budget as usize) {
            scanned += 1;
            for label in &node.labels {
                labels.entry(label).or_default().add(&node.properties);
            }
        }
        for edge in self.edges().take((budget - scanned) as usize) {
            scanned += 1;
            relationships
                .entry(&edge.relationship)
                .or_default()
                .add(&edge.properties);
            if let (Some(source), Some(destination)) =
                (self.node(edge.source), self.node(edge.destination))
            {
                for s in &source.labels {
                    for d in &destination.labels {
                        *endpoints.entry((&edge.relationship, s, d)).or_default() += 1;
                    }
                }
            }
        }
        let schema = self.schema();
        let empty = Tally::default();
        let label_names: BTreeSet<&str> = labels
            .keys()
            .copied()
            .chain(schema.labels.keys().map(String::as_str))
            .collect();
        let labels = label_names
            .into_iter()
            .map(|name| {
                let tally = labels.get(name).unwrap_or(&empty);
                let declared = schema.labels.get(name);
                ObservedLabel {
                    name: name.to_owned(),
                    origin: SchemaOrigin::of(declared.is_some(), tally.count),
                    count: tally.count,
                    properties: tally.summarize(declared.map(|d| &d.properties)),
                }
            })
            .collect();
        let relationship_names: BTreeSet<&str> = relationships
            .keys()
            .copied()
            .chain(schema.relationships.keys().map(String::as_str))
            .collect();
        let relationships = relationship_names
            .into_iter()
            .map(|name| {
                let tally = relationships.get(name).unwrap_or(&empty);
                let declared = schema.relationships.get(name);
                let mut pairs: BTreeMap<(&str, &str), (bool, u64)> = endpoints
                    .range((name, "", "")..)
                    .take_while(|((kind, _, _), _)| *kind == name)
                    .map(|((_, s, d), count)| ((*s, *d), (false, *count)))
                    .collect();
                for (s, d) in declared.iter().flat_map(|d| &d.endpoints) {
                    pairs.entry((s.as_str(), d.as_str())).or_default().0 = true;
                }
                ObservedRelationship {
                    name: name.to_owned(),
                    origin: SchemaOrigin::of(declared.is_some(), tally.count),
                    count: tally.count,
                    endpoints: pairs
                        .into_iter()
                        .map(
                            |((source, destination), (declared, count))| ObservedEndpoint {
                                source: source.to_owned(),
                                destination: destination.to_owned(),
                                origin: SchemaOrigin::of(declared, count),
                                count,
                            },
                        )
                        .collect(),
                    properties: tally.summarize(declared.map(|d| &d.properties)),
                }
            })
            .collect();
        Ok(ObservedSchema {
            complete: scanned == total,
            entities_scanned: scanned,
            labels,
            relationships,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GraphSchema {
        GraphSchema::default()
            .label(
                "Person",
                LabelDefinition::new("people")
                    .property(
                        "name",
                        PropertyDefinition::new(PropertyType::String)
                            .required()
                            .unique(),
                    )
                    .property(
                        "age",
                        PropertyDefinition::new(PropertyType::Integer)
                            .index(PropertyIndexKind::Range),
                    ),
            )
            .relationship(
                "KNOWS",
                RelationshipDefinition::new("")
                    .endpoint("Person", "Person")
                    .multiplicity(Multiplicity::UniqueTriple)
                    .property("since", PropertyDefinition::new(PropertyType::Float)),
            )
    }

    #[test]
    fn schemas_round_trip_canonically() {
        for schema in [GraphSchema::default(), sample()] {
            schema.validate().unwrap();
            let mut bytes = Vec::new();
            schema.encode_into(&mut bytes);
            assert_eq!(bytes.len(), schema.encoded_len());
            assert_eq!(decode(&bytes).unwrap(), schema);
            for cut in 0..bytes.len() {
                assert!(decode(&bytes[..cut]).is_err(), "truncated at {cut}");
            }
            let mut long = bytes.clone();
            long.push(0);
            assert!(decode(&long).is_err());
        }
    }

    #[test]
    fn invalid_codes_and_orders_are_corrupt() {
        let mut bytes = Vec::new();
        sample().encode_into(&mut bytes);
        // Person: count u16, name, desc, prop count, "age" entry.
        let age_type = 2 + 1 + 6 + 2 + 6 + 1 + 2 + 3;
        for (offset, value) in [(age_type, 9), (age_type + 1, 12), (age_type + 1, 16)] {
            let mut bad = bytes.clone();
            bad[offset] = value;
            assert!(matches!(decode(&bad), Err(GraphError::Corrupt(_))));
        }
        let unordered = GraphSchema::default()
            .label("A", LabelDefinition::new(""))
            .label("B", LabelDefinition::new(""));
        let mut bytes = Vec::new();
        unordered.encode_into(&mut bytes);
        bytes.swap(3, 8);
        assert!(matches!(decode(&bytes), Err(GraphError::Corrupt(_))));
    }

    #[test]
    fn validation_bounds_definitions() {
        let range_on_string = GraphSchema::default().label(
            "L",
            LabelDefinition::new("").property(
                "p",
                PropertyDefinition::new(PropertyType::String).index(PropertyIndexKind::Range),
            ),
        );
        let unique_edge_property = GraphSchema::default().relationship(
            "R",
            RelationshipDefinition::new("")
                .property("p", PropertyDefinition::new(PropertyType::String).unique()),
        );
        let bad_label = GraphSchema::default().label("bad label", LabelDefinition::new(""));
        let bad_endpoint = GraphSchema::default()
            .relationship("R", RelationshipDefinition::new("").endpoint("", "L"));
        let long = GraphSchema::default().label("L", LabelDefinition::new("x".repeat(1025)));
        let mut many = LabelDefinition::new("");
        for i in 0..33 {
            many = many.property(
                format!("p{i}"),
                PropertyDefinition::new(PropertyType::String).index(PropertyIndexKind::Exact),
            );
        }
        let too_many_indexes = GraphSchema::default().label("L", many);
        for schema in [
            range_on_string,
            unique_edge_property,
            bad_label,
            bad_endpoint,
            long,
            too_many_indexes,
        ] {
            assert!(matches!(
                schema.validate(),
                Err(GraphError::InvalidSchema(_))
            ));
        }
        let ok = GraphSchema::default().label("L", LabelDefinition::new("x".repeat(1024)));
        ok.validate().unwrap();
    }

    #[test]
    fn float_keys_treat_signed_zeros_as_equal() {
        assert_eq!(OrdF64::new(-0.0), OrdF64::new(0.0));
        assert_eq!(
            EqKey::new(&PropertyValue::Float(-0.0)),
            EqKey::new(&PropertyValue::Float(0.0))
        );
        assert!(OrdF64::new(-1.0) < OrdF64::new(-0.0));
    }
}
