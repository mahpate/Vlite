//! Private syntax and planning for the versioned embedded query subset.

use super::language::{LanguageError, LanguageErrorKind};
use super::model::PropertyValue;
use std::collections::BTreeMap;

pub(crate) mod lexer;
pub(crate) mod parser;
pub(crate) mod plan;

#[allow(unused_imports)] // The standalone graph harness has no embedded-query adapter.
pub(crate) use plan::{ParameterEntry, plan};

pub(crate) const MAX_PREDICATES: usize = 64;
pub(crate) const MAX_PAREN_DEPTH: usize = 16;
pub(crate) const MAX_PARAMETERS: usize = 64;
pub(crate) const MAX_PARAMETER_BYTES: usize = 65_536;
pub(crate) const MAX_PARAMETER_NAME_BYTES: usize = 256;
pub(crate) const MAX_HOPS: u64 = 4;

pub(crate) fn error(
    kind: LanguageErrorKind,
    position: usize,
    message: impl Into<String>,
) -> LanguageError {
    LanguageError {
        kind,
        position,
        message: message.into(),
    }
}

pub(crate) fn syntax(position: usize, message: impl Into<String>) -> LanguageError {
    error(LanguageErrorKind::Syntax, position, message)
}

pub(crate) fn unsupported(position: usize, message: impl Into<String>) -> LanguageError {
    error(LanguageErrorKind::Unsupported, position, message)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Name {
    pub(crate) text: String,
    pub(crate) position: usize,
    pub(crate) quoted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParameterRef {
    pub(crate) name: String,
    pub(crate) position: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ParameterKind {
    Any,
    String,
    Integer,
    NonNegativeInteger,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParameterUse {
    pub(crate) reference: ParameterRef,
    pub(crate) kind: ParameterKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnchorKind {
    Key,
    Id,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Anchor {
    pub(crate) kind: AnchorKind,
    pub(crate) parameter: ParameterRef,
    pub(crate) position: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PropertyPair {
    pub(crate) name: Name,
    pub(crate) parameter: ParameterRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NodePattern {
    pub(crate) variable: Option<Name>,
    pub(crate) labels: Vec<Name>,
    pub(crate) anchor: Option<Anchor>,
    pub(crate) properties: Vec<PropertyPair>,
    pub(crate) position: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Out,
    In,
    Both,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelationshipPattern {
    pub(crate) variable: Option<Name>,
    pub(crate) types: Vec<Name>,
    pub(crate) anchor: Option<Anchor>,
    pub(crate) max_hops: Option<u64>,
    pub(crate) hop_position: Option<usize>,
    pub(crate) properties: Vec<PropertyPair>,
    pub(crate) direction: Direction,
    pub(crate) position: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pattern {
    pub(crate) path_variable: Option<Name>,
    pub(crate) start: NodePattern,
    pub(crate) relationship: Option<(RelationshipPattern, NodePattern)>,
    pub(crate) position: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Compare {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Predicate {
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    Not(Box<Predicate>),
    Compare {
        variable: Name,
        field: Name,
        operation: Compare,
        parameter: ParameterRef,
    },
    IsNull {
        variable: Name,
        field: Name,
        negated: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreationNode {
    pub(crate) variable: Name,
    pub(crate) labels: Vec<Name>,
    pub(crate) key: ParameterRef,
    pub(crate) properties: Vec<PropertyPair>,
    pub(crate) position: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreationEdge {
    pub(crate) variable: Name,
    pub(crate) source: Name,
    pub(crate) relationship_type: Name,
    pub(crate) key: Option<ParameterRef>,
    pub(crate) properties: Vec<PropertyPair>,
    pub(crate) destination: Name,
    pub(crate) position: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Creation {
    Node(CreationNode),
    Edge(CreationEdge),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FieldTarget {
    pub(crate) variable: Name,
    pub(crate) field: Name,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Assignment {
    Property {
        target: FieldTarget,
        parameter: ParameterRef,
    },
    Label {
        variable: Name,
        label: Name,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Removal {
    Property(FieldTarget),
    Label { variable: Name, label: Name },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mutation {
    Create {
        creations: Vec<Creation>,
        position: usize,
    },
    Merge {
        creation: Creation,
        position: usize,
    },
    Set {
        assignments: Vec<Assignment>,
        position: usize,
    },
    Remove {
        removals: Vec<Removal>,
        position: usize,
    },
    Delete {
        variables: Vec<Name>,
        detach: bool,
        position: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReturnValue {
    Variable(Name),
    Field(FieldTarget),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReturnItem {
    pub(crate) value: ReturnValue,
    pub(crate) alias: Option<Name>,
    pub(crate) position: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SortItem {
    pub(crate) name: Name,
    pub(crate) direction: SortDirection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReturnClause {
    pub(crate) items: Vec<ReturnItem>,
    pub(crate) order_by: Vec<SortItem>,
    pub(crate) skip: Option<ParameterRef>,
    pub(crate) limit: Option<ParameterRef>,
    pub(crate) position: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Statement {
    pub(crate) pattern: Option<Pattern>,
    pub(crate) as_of: Option<ParameterRef>,
    pub(crate) predicate: Option<Predicate>,
    pub(crate) mutations: Vec<Mutation>,
    pub(crate) returning: Option<ReturnClause>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Plan {
    pub(crate) statement: Statement,
    pub(crate) parameters: BTreeMap<String, PropertyValue>,
}
