use super::super::error::GraphError;
use super::super::language::{LanguageError, LanguageErrorKind};
use super::super::model::PropertyValue;
use super::{
    Assignment, Creation, FieldTarget, MAX_PARAMETER_BYTES, MAX_PARAMETER_NAME_BYTES,
    MAX_PARAMETERS, Mutation, Name, ParameterKind, ParameterUse, Plan, Predicate, Removal,
    ReturnClause, ReturnItem, ReturnValue, Statement, error,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub(crate) struct ParameterEntry {
    pub(crate) name: String,
    pub(crate) value: PropertyValue,
    /// Byte offset in the caller's parameter envelope, not in query text.
    pub(crate) position: usize,
}

#[derive(Debug)]
pub(crate) enum PlanError {
    Query(LanguageError),
    InvalidMetadata(GraphError),
}

impl From<LanguageError> for PlanError {
    fn from(value: LanguageError) -> Self {
        Self::Query(value)
    }
}

pub(crate) fn plan(text: &str, parameters: &[ParameterEntry]) -> Result<Plan, PlanError> {
    let parsed = super::parser::parse(text)?;
    validate_statement(&parsed.statement)?;
    let bound = bind_parameters(text.len(), parameters, &parsed.parameter_uses)?;
    Ok(Plan {
        statement: parsed.statement,
        parameters: bound,
    })
}

fn bind_parameters(
    eof: usize,
    parameters: &[ParameterEntry],
    uses: &[ParameterUse],
) -> Result<BTreeMap<String, PropertyValue>, PlanError> {
    let mut names = BTreeSet::new();
    for parameter in parameters {
        if !names.insert(parameter.name.as_str()) {
            return Err(error(
                LanguageErrorKind::Parameter,
                parameter.position,
                "duplicate parameter name in parameter envelope",
            )
            .into());
        }
    }
    if parameters.len() > MAX_PARAMETERS {
        return Err(error(
            LanguageErrorKind::Limit,
            eof,
            format!("parameter count exceeds {MAX_PARAMETERS}"),
        )
        .into());
    }
    let mut metadata_bytes = 4usize;
    for parameter in parameters {
        if parameter.name.len() > MAX_PARAMETER_NAME_BYTES {
            return Err(error(
                LanguageErrorKind::Limit,
                eof,
                format!("parameter name exceeds {MAX_PARAMETER_NAME_BYTES} bytes"),
            )
            .into());
        }
        if !valid_parameter_name(&parameter.name) {
            return Err(error(
                LanguageErrorKind::Parameter,
                parameter.position,
                "parameter names must be ASCII bare identifiers",
            )
            .into());
        }
        super::super::properties::validate_value(&parameter.value)
            .map_err(PlanError::InvalidMetadata)?;
        metadata_bytes = metadata_bytes.saturating_add(parameter_size(parameter));
    }
    if metadata_bytes > MAX_PARAMETER_BYTES {
        return Err(error(
            LanguageErrorKind::Limit,
            eof,
            format!("parameter metadata exceeds {MAX_PARAMETER_BYTES} bytes"),
        )
        .into());
    }

    for usage in uses {
        let Some(parameter) = parameters
            .iter()
            .find(|item| item.name == usage.reference.name)
        else {
            return Err(error(
                LanguageErrorKind::Parameter,
                usage.reference.position,
                "missing parameter",
            )
            .into());
        };
        if !matches_parameter_kind(&parameter.value, usage.kind) {
            return Err(error(
                LanguageErrorKind::Parameter,
                usage.reference.position,
                match usage.kind {
                    ParameterKind::Any => "parameter has an unsupported value type",
                    ParameterKind::String => "parameter must be a string",
                    ParameterKind::Integer => "parameter must be an integer",
                    ParameterKind::NonNegativeInteger => "parameter must be a non-negative integer",
                },
            )
            .into());
        }
    }

    for parameter in parameters {
        if !uses
            .iter()
            .any(|usage| usage.reference.name == parameter.name)
        {
            return Err(error(
                LanguageErrorKind::Parameter,
                eof,
                "unused parameter in parameter envelope",
            )
            .into());
        }
    }

    Ok(parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), parameter.value.clone()))
        .collect())
}

fn parameter_size(parameter: &ParameterEntry) -> usize {
    let value_size = match &parameter.value {
        PropertyValue::String(value) => 1 + 4 + value.len(),
        PropertyValue::Integer(_) | PropertyValue::Float(_) => 1 + 8,
        PropertyValue::Boolean(_) => 2,
        PropertyValue::GeoPoint { .. } => 1 + 16,
    };
    2 + parameter.name.len() + value_size
}

fn valid_parameter_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn matches_parameter_kind(value: &PropertyValue, kind: ParameterKind) -> bool {
    match kind {
        ParameterKind::Any => true,
        ParameterKind::String => matches!(value, PropertyValue::String(_)),
        ParameterKind::Integer => matches!(value, PropertyValue::Integer(_)),
        ParameterKind::NonNegativeInteger => {
            matches!(value, PropertyValue::Integer(value) if *value >= 0)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindingKind {
    Node,
    Edge,
    Path,
}

#[derive(Clone, Copy)]
struct Binding {
    kind: BindingKind,
}

fn validate_statement(statement: &Statement) -> Result<(), LanguageError> {
    let mut bindings = BTreeMap::<String, Binding>::new();
    if !statement.mutations.is_empty()
        && let Some((relationship, _)) = statement
            .pattern
            .as_ref()
            .and_then(|pattern| pattern.relationship.as_ref())
        && let Some(position) = relationship.hop_position
    {
        return Err(super::unsupported(
            position,
            "write MATCH does not accept variable-length relationships",
        ));
    }
    if let Some(pattern) = &statement.pattern {
        validate_node_pattern(&pattern.start)?;
        if let Some(variable) = &pattern.start.variable {
            declare(&mut bindings, variable, BindingKind::Node)?;
        }
        if let Some((relationship, end)) = &pattern.relationship {
            if pattern.start.anchor.is_none() {
                return Err(super::syntax(
                    relationship.position,
                    "relationship MATCH requires a KEY or ID anchor on the first node",
                ));
            }
            validate_relationship_pattern(relationship)?;
            validate_node_pattern(end)?;
            if let Some(hop_position) = relationship.hop_position {
                if relationship.variable.is_some() {
                    return Err(super::unsupported(
                        hop_position,
                        "variable-length relationships cannot bind a relationship variable",
                    ));
                }
                if relationship.anchor.is_some() {
                    return Err(super::unsupported(
                        hop_position,
                        "relationship anchors cannot be combined with variable-length paths",
                    ));
                }
            }
            if let Some(variable) = &end.variable
                && !pattern
                    .start
                    .variable
                    .as_ref()
                    .is_some_and(|start| start.text == variable.text)
            {
                declare(&mut bindings, variable, BindingKind::Node)?;
            }
            if let Some(variable) = &relationship.variable {
                declare(&mut bindings, variable, BindingKind::Edge)?;
            }
        }
        if let Some(path) = &pattern.path_variable {
            if pattern.relationship.is_none() {
                return Err(super::syntax(
                    path.position,
                    "a path variable requires a relationship pattern",
                ));
            }
            declare(&mut bindings, path, BindingKind::Path)?;
        }
    }

    if let Some(predicate) = &statement.predicate {
        validate_predicate(predicate, &bindings)?;
    }
    for mutation in &statement.mutations {
        validate_mutation(mutation, &mut bindings)?;
    }
    if let Some(returning) = &statement.returning {
        validate_return(returning, &bindings)?;
    }
    Ok(())
}

fn declare(
    bindings: &mut BTreeMap<String, Binding>,
    name: &Name,
    kind: BindingKind,
) -> Result<(), LanguageError> {
    if bindings.contains_key(&name.text) {
        return Err(super::syntax(
            name.position,
            "variable declarations must be unique",
        ));
    }
    bindings.insert(name.text.clone(), Binding { kind });
    Ok(())
}

fn validate_node_pattern(node: &super::NodePattern) -> Result<(), LanguageError> {
    let mut labels = BTreeSet::new();
    for label in &node.labels {
        if !labels.insert(label.text.as_str()) {
            return Err(super::syntax(
                label.position,
                "a label cannot occur twice in one node pattern",
            ));
        }
    }
    validate_unique_properties(&node.properties)
}

fn validate_relationship_pattern(
    relationship: &super::RelationshipPattern,
) -> Result<(), LanguageError> {
    let mut types = BTreeSet::new();
    for relationship_type in &relationship.types {
        if !types.insert(relationship_type.text.as_str()) {
            return Err(super::syntax(
                relationship_type.position,
                "a relationship type cannot occur twice in one pattern",
            ));
        }
    }
    validate_unique_properties(&relationship.properties)
}

fn validate_unique_properties(properties: &[super::PropertyPair]) -> Result<(), LanguageError> {
    let mut seen = BTreeSet::new();
    for property in properties {
        if !seen.insert(property.name.text.as_str()) {
            return Err(super::syntax(
                property.name.position,
                "a property cannot occur twice in one map",
            ));
        }
    }
    Ok(())
}

fn lookup_binding<'a>(
    bindings: &'a BTreeMap<String, Binding>,
    variable: &Name,
) -> Result<&'a Binding, LanguageError> {
    bindings
        .get(&variable.text)
        .ok_or_else(|| super::syntax(variable.position, "unknown variable"))
}

fn validate_field_target(
    target: &FieldTarget,
    bindings: &BTreeMap<String, Binding>,
) -> Result<BindingKind, LanguageError> {
    let binding = lookup_binding(bindings, &target.variable)?;
    if binding.kind == BindingKind::Path {
        return Err(super::syntax(
            target.variable.position,
            "property fields require a node or relationship variable",
        ));
    }
    Ok(binding.kind)
}

fn validate_predicate(
    predicate: &Predicate,
    bindings: &BTreeMap<String, Binding>,
) -> Result<(), LanguageError> {
    match predicate {
        Predicate::And(terms) | Predicate::Or(terms) => {
            for term in terms {
                validate_predicate(term, bindings)?;
            }
        }
        Predicate::Not(term) => validate_predicate(term, bindings)?,
        Predicate::Compare {
            variable, field, ..
        }
        | Predicate::IsNull {
            variable, field, ..
        } => {
            validate_field_target(
                &FieldTarget {
                    variable: variable.clone(),
                    field: field.clone(),
                },
                bindings,
            )?;
        }
    }
    Ok(())
}

fn validate_mutation(
    mutation: &Mutation,
    bindings: &mut BTreeMap<String, Binding>,
) -> Result<(), LanguageError> {
    match mutation {
        Mutation::Create { creations, .. } => {
            for creation in creations {
                validate_creation(creation, bindings)?;
            }
        }
        Mutation::Merge { creation, .. } => validate_creation(creation, bindings)?,
        Mutation::Set { assignments, .. } => {
            let mut seen = BTreeSet::new();
            for assignment in assignments {
                match assignment {
                    Assignment::Property { target, .. } => {
                        validate_field_target(target, bindings)?;
                        if !seen.insert((
                            target.variable.text.as_str(),
                            "property",
                            target.field.text.as_str(),
                        )) {
                            return Err(super::syntax(
                                target.field.position,
                                "a property cannot occur twice in one assignment list",
                            ));
                        }
                    }
                    Assignment::Label { variable, label } => {
                        require_node(bindings, variable)?;
                        if !seen.insert((variable.text.as_str(), "label", label.text.as_str())) {
                            return Err(super::syntax(
                                label.position,
                                "a label cannot occur twice in one assignment list",
                            ));
                        }
                    }
                }
            }
        }
        Mutation::Remove { removals, .. } => {
            let mut seen = BTreeSet::new();
            for removal in removals {
                match removal {
                    Removal::Property(target) => {
                        validate_field_target(target, bindings)?;
                        if !seen.insert((
                            target.variable.text.as_str(),
                            "property",
                            target.field.text.as_str(),
                        )) {
                            return Err(super::syntax(
                                target.field.position,
                                "a property cannot occur twice in one removal list",
                            ));
                        }
                    }
                    Removal::Label { variable, label } => {
                        require_node(bindings, variable)?;
                        if !seen.insert((variable.text.as_str(), "label", label.text.as_str())) {
                            return Err(super::syntax(
                                label.position,
                                "a label cannot occur twice in one removal list",
                            ));
                        }
                    }
                }
            }
        }
        Mutation::Delete {
            variables, detach, ..
        } => {
            for variable in variables {
                let kind = lookup_binding(bindings, variable)?.kind;
                if *detach && kind != BindingKind::Node {
                    return Err(super::syntax(
                        variable.position,
                        "DETACH DELETE accepts only node variables",
                    ));
                }
                if !matches!(kind, BindingKind::Node | BindingKind::Edge) {
                    return Err(super::syntax(
                        variable.position,
                        "DELETE accepts only node or relationship variables",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_creation(
    creation: &Creation,
    bindings: &mut BTreeMap<String, Binding>,
) -> Result<(), LanguageError> {
    match creation {
        Creation::Node(node) => {
            validate_name_list(
                &node.labels,
                "a label cannot occur twice in one node pattern",
            )?;
            validate_unique_properties(&node.properties)?;
            declare(bindings, &node.variable, BindingKind::Node)
        }
        Creation::Edge(edge) => {
            require_node(bindings, &edge.source)?;
            require_node(bindings, &edge.destination)?;
            validate_unique_properties(&edge.properties)?;
            declare(bindings, &edge.variable, BindingKind::Edge)
        }
    }
}

fn validate_name_list(names: &[Name], duplicate_reason: &'static str) -> Result<(), LanguageError> {
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name.text.as_str()) {
            return Err(super::syntax(name.position, duplicate_reason));
        }
    }
    Ok(())
}

fn require_node(
    bindings: &BTreeMap<String, Binding>,
    variable: &Name,
) -> Result<(), LanguageError> {
    if lookup_binding(bindings, variable)?.kind == BindingKind::Node {
        Ok(())
    } else {
        Err(super::syntax(
            variable.position,
            "this operation requires a node variable",
        ))
    }
}

fn validate_return(
    returning: &ReturnClause,
    bindings: &BTreeMap<String, Binding>,
) -> Result<(), LanguageError> {
    let mut columns = BTreeSet::<String>::new();
    let mut scalar_aliases = BTreeSet::new();
    for item in &returning.items {
        let output_name = validate_return_item(item, bindings)?;
        if !columns.insert(output_name) {
            return Err(super::syntax(
                item.alias
                    .as_ref()
                    .map_or(item.position, |alias| alias.position),
                "RETURN column names must be unique",
            ));
        }
        if matches!(item.value, ReturnValue::Field(_))
            && let Some(alias) = &item.alias
        {
            scalar_aliases.insert(alias.text.as_str());
        }
    }
    for sort in &returning.order_by {
        if !scalar_aliases.contains(sort.name.text.as_str()) {
            return Err(super::syntax(
                sort.name.position,
                "ORDER BY must name a projected scalar column, and projected fields require an alias",
            ));
        }
    }
    Ok(())
}

fn validate_return_item(
    item: &ReturnItem,
    bindings: &BTreeMap<String, Binding>,
) -> Result<String, LanguageError> {
    let natural_name = match &item.value {
        ReturnValue::Variable(variable) => {
            lookup_binding(bindings, variable)?;
            variable.text.clone()
        }
        ReturnValue::Field(target) => {
            validate_field_target(target, bindings)?;
            format!("{}.{}", target.variable.text, target.field.text)
        }
    };
    Ok(item
        .alias
        .as_ref()
        .map_or(natural_name, |alias| alias.text.clone()))
}

#[cfg(test)]
mod tests {
    use super::super::super::model::Properties;
    use super::*;

    #[test]
    fn parameter_size_matches_encoded_metadata_for_each_value_type() {
        let cases = [
            ("string", PropertyValue::String("é".into())),
            ("integer", PropertyValue::Integer(-7)),
            ("float", PropertyValue::Float(1.5)),
            ("boolean", PropertyValue::Boolean(true)),
            (
                "point",
                PropertyValue::GeoPoint {
                    latitude: 47.6,
                    longitude: -122.3,
                },
            ),
        ];
        for (name, value) in cases {
            let parameter = ParameterEntry {
                name: name.into(),
                value: value.clone(),
                position: 0,
            };
            let properties = Properties::from([(name.to_owned(), value)]);
            let encoded_length = super::super::super::properties::validate(&properties).unwrap();
            assert_eq!(
                parameter_size(&parameter),
                encoded_length - 4,
                "value type for {name}"
            );
        }
    }
}
