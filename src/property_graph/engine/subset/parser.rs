use super::super::language::{LanguageError, LanguageErrorKind};
use super::lexer::{Kind, Token, lex};
use super::{
    Anchor, AnchorKind, Assignment, Compare, Creation, CreationEdge, CreationNode, Direction,
    FieldTarget, MAX_HOPS, MAX_PAREN_DEPTH, MAX_PREDICATES, Mutation, Name, NodePattern,
    ParameterKind, ParameterRef, ParameterUse, Pattern, Predicate, PropertyPair,
    RelationshipPattern, Removal, ReturnClause, ReturnItem, ReturnValue, SortDirection, SortItem,
    Statement, error, syntax, unsupported,
};

const RESERVED: &[&str] = &[
    "MATCH",
    "WHERE",
    "RETURN",
    "ORDER",
    "BY",
    "SKIP",
    "LIMIT",
    "AS",
    "OF",
    "KEY",
    "ID",
    "CREATE",
    "MERGE",
    "SET",
    "REMOVE",
    "DELETE",
    "DETACH",
    "AND",
    "OR",
    "NOT",
    "IS",
    "NULL",
    "ASC",
    "DESC",
    "OPTIONAL",
    "WITH",
    "UNWIND",
    "CALL",
    "YIELD",
    "UNION",
    "ALL",
    "DISTINCT",
    "FOREACH",
    "LOAD",
    "CSV",
    "USE",
    "INSERT",
    "UPDATE",
    "DROP",
    "ALTER",
    "INDEX",
    "CONSTRAINT",
    "EXPLAIN",
    "PROFILE",
    "SHORTEST",
    "PATH",
    "SEARCH",
    "NEAR",
    "IN",
    "TRUE",
    "FALSE",
    "EXISTS",
];

const UNSUPPORTED_CLAUSES: &[&str] = &[
    "OPTIONAL",
    "WITH",
    "UNWIND",
    "CALL",
    "YIELD",
    "UNION",
    "FOREACH",
    "LOAD",
    "CSV",
    "USE",
    "INSERT",
    "UPDATE",
    "DROP",
    "ALTER",
    "INDEX",
    "CONSTRAINT",
    "EXPLAIN",
    "PROFILE",
    "SHORTEST",
    "SEARCH",
    "NEAR",
    "DISTINCT",
    "IN",
    "EXISTS",
];

const RESERVED_FUNCTIONS: &[&str] = &["ID", "ALL"];

pub(super) struct Parsed {
    pub(super) statement: Statement,
    pub(super) parameter_uses: Vec<ParameterUse>,
}

pub(super) fn parse(text: &str) -> Result<Parsed, LanguageError> {
    let tokens = lex(text)?;
    let mut parser = Parser {
        tokens,
        at: 0,
        parameter_uses: Vec::new(),
        predicate_count: 0,
        parenthesis_depth: 0,
        match_clause_seen: false,
    };
    let statement = parser.statement()?;
    if !parser.is_end() {
        return Err(parser.unexpected("the end of the statement"));
    }
    Ok(Parsed {
        statement,
        parameter_uses: parser.parameter_uses,
    })
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    parameter_uses: Vec<ParameterUse>,
    predicate_count: usize,
    parenthesis_depth: usize,
    match_clause_seen: bool,
}

impl Parser {
    fn token(&self) -> &Token {
        &self.tokens[self.at]
    }

    fn kind_at(&self, offset: usize) -> &Kind {
        &self.tokens[(self.at + offset).min(self.tokens.len() - 1)].kind
    }

    fn position(&self) -> usize {
        self.token().position
    }

    fn is_end(&self) -> bool {
        self.token().kind == Kind::End
    }

    fn bump(&mut self) -> Token {
        let token = self.tokens[self.at].clone();
        if self.at + 1 < self.tokens.len() {
            self.at += 1;
        }
        token
    }

    fn is_keyword(&self, keyword: &str) -> bool {
        matches!(&self.token().kind, Kind::Word(word) if word.eq_ignore_ascii_case(keyword))
    }

    fn is_match_clause_start(&self) -> bool {
        self.is_keyword("MATCH")
            && (self.kind_at(1) == &Kind::Symbol("(")
                || (Self::is_identifier_kind(self.kind_at(1))
                    && self.kind_at(2) == &Kind::Symbol("=")))
    }

    fn eat_keyword(&mut self, keyword: &str) -> Option<usize> {
        self.is_keyword(keyword).then(|| self.bump().position)
    }

    fn is_symbol(&self, symbol: &str) -> bool {
        matches!(&self.token().kind, Kind::Symbol(actual) if *actual == symbol)
    }

    fn eat_symbol(&mut self, symbol: &str) -> Option<usize> {
        self.is_symbol(symbol).then(|| self.bump().position)
    }

    fn is_mutation(&self) -> bool {
        ["CREATE", "MERGE", "SET", "REMOVE", "DELETE", "DETACH"]
            .iter()
            .any(|keyword| self.is_keyword(keyword))
    }

    fn second_match_error(&self) -> LanguageError {
        unsupported(
            self.position(),
            "a second MATCH clause is not supported by vlite-cypher-1",
        )
    }

    fn is_identifier_kind(kind: &Kind) -> bool {
        matches!(kind, Kind::Word(_) | Kind::Quoted(_))
    }

    fn unexpected(&self, expected: &str) -> LanguageError {
        let token = self.token();
        if self.match_clause_seen && self.is_keyword("MATCH") {
            return self.second_match_error();
        }
        let unreserved_function = Self::is_identifier_kind(&token.kind)
            && !matches!(&token.kind, Kind::Word(word) if Self::bare_reserved(word));
        let reserved_function = matches!(&token.kind, Kind::Word(word)
            if RESERVED_FUNCTIONS.iter().any(|name| word.eq_ignore_ascii_case(name)));
        if (unreserved_function || reserved_function) && self.kind_at(1) == &Kind::Symbol("(") {
            return unsupported(token.position, "function invocation is not supported");
        }
        if let Kind::Word(word) = &token.kind {
            if UNSUPPORTED_CLAUSES
                .iter()
                .any(|reserved| word.eq_ignore_ascii_case(reserved))
            {
                return unsupported(
                    token.position,
                    format!(
                        "{} is not supported by vlite-cypher-1",
                        word.to_ascii_uppercase()
                    ),
                );
            }
            if ["TRUE", "FALSE", "NULL"]
                .iter()
                .any(|reserved| word.eq_ignore_ascii_case(reserved))
            {
                return unsupported(
                    token.position,
                    "literal values are not supported; bind data with a typed parameter",
                );
            }
        }
        if let Kind::Unsigned(_) = token.kind {
            return unsupported(
                token.position,
                "numeric literals are not supported; bind data with a typed parameter",
            );
        }
        if matches!(
            token.kind,
            Kind::Symbol("[") | Kind::Symbol("{") | Kind::Symbol("*")
        ) {
            return unsupported(
                token.position,
                "lists, nested maps and arithmetic expressions are not supported",
            );
        }
        if matches!(token.kind, Kind::Symbol("-")) {
            return unsupported(
                token.position,
                "arithmetic and negative literals are not supported; bind typed values",
            );
        }
        syntax(
            token.position,
            format!("expected {expected}, found {}", self.describe(&token.kind)),
        )
    }

    fn describe(&self, kind: &Kind) -> String {
        match kind {
            Kind::Word(_) => "word".into(),
            Kind::Quoted(_) => "quoted name".into(),
            Kind::Parameter(_) => "parameter".into(),
            Kind::Unsigned(_) => "number".into(),
            Kind::Symbol(_) => "symbol".into(),
            Kind::End => "end of input".into(),
        }
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<usize, LanguageError> {
        self.eat_keyword(keyword)
            .ok_or_else(|| self.unexpected(keyword))
    }

    fn expect_symbol(&mut self, symbol: &str) -> Result<usize, LanguageError> {
        self.eat_symbol(symbol)
            .ok_or_else(|| self.unexpected(&format!("{symbol:?}")))
    }

    fn bare_reserved(word: &str) -> bool {
        RESERVED
            .iter()
            .any(|reserved| word.eq_ignore_ascii_case(reserved))
    }

    fn name(&mut self, expected: &str) -> Result<Name, LanguageError> {
        let token = self.token().clone();
        let (text, quoted) = match token.kind {
            Kind::Word(text) => (text, false),
            Kind::Quoted(text) => (text, true),
            _ => return Err(self.unexpected(expected)),
        };
        self.bump();
        Ok(Name {
            text,
            position: token.position,
            quoted,
        })
    }

    fn identifier(&mut self) -> Result<Name, LanguageError> {
        if self.match_clause_seen && self.is_match_clause_start() {
            return Err(self.second_match_error());
        }
        if let Kind::Word(word) = &self.token().kind
            && ["TRUE", "FALSE", "NULL"]
                .iter()
                .any(|reserved| word.eq_ignore_ascii_case(reserved))
        {
            return Err(unsupported(
                self.position(),
                "literal values are not supported; bind data with a typed parameter",
            ));
        }
        if let Kind::Word(word) = &self.token().kind
            && ["EXISTS", "COUNT"]
                .iter()
                .any(|name| word.eq_ignore_ascii_case(name))
            && self.kind_at(1) == &Kind::Symbol("{")
        {
            return Err(unsupported(self.position(), "subqueries are not supported"));
        }
        if let Kind::Word(word) = &self.token().kind
            && UNSUPPORTED_CLAUSES
                .iter()
                .any(|reserved| word.eq_ignore_ascii_case(reserved))
        {
            return Err(unsupported(
                self.position(),
                format!(
                    "{} is not supported by vlite-cypher-1",
                    word.to_ascii_uppercase()
                ),
            ));
        }
        let unreserved_function = Self::is_identifier_kind(&self.token().kind)
            && !matches!(&self.token().kind, Kind::Word(word) if Self::bare_reserved(word));
        let reserved_function = matches!(&self.token().kind, Kind::Word(word)
            if RESERVED_FUNCTIONS.iter().any(|name| word.eq_ignore_ascii_case(name)));
        if (unreserved_function || reserved_function) && self.kind_at(1) == &Kind::Symbol("(") {
            return Err(unsupported(
                self.position(),
                "function invocation is not supported",
            ));
        }
        let name = self.name("an identifier")?;
        if !name.quoted && Self::bare_reserved(&name.text) {
            return Err(syntax(
                name.position,
                "reserved words must be backquoted when used as variables or aliases",
            ));
        }
        Ok(name)
    }

    fn parameter(&mut self, expected: ParameterKind) -> Result<ParameterRef, LanguageError> {
        let token = self.token().clone();
        let Kind::Parameter(name) = token.kind else {
            if let Kind::Word(word) = &token.kind
                && ["TRUE", "FALSE", "NULL"]
                    .iter()
                    .any(|kw| word.eq_ignore_ascii_case(kw))
            {
                return Err(unsupported(
                    token.position,
                    "literal values are not supported; bind data with a typed parameter",
                ));
            }
            if let Kind::Unsigned(_) = token.kind {
                return Err(unsupported(
                    token.position,
                    "numeric literals are not supported; bind data with a typed parameter",
                ));
            }
            let unreserved_function = Self::is_identifier_kind(&token.kind)
                && !matches!(&token.kind, Kind::Word(word) if Self::bare_reserved(word));
            let reserved_function = matches!(&token.kind, Kind::Word(word)
                if RESERVED_FUNCTIONS.iter().any(|name| word.eq_ignore_ascii_case(name)));
            if (unreserved_function || reserved_function) && self.kind_at(1) == &Kind::Symbol("(") {
                return Err(unsupported(
                    token.position,
                    "function invocation is not supported",
                ));
            }
            return Err(self.unexpected("a typed parameter"));
        };
        self.bump();
        let reference = ParameterRef {
            name,
            position: token.position,
        };
        self.parameter_uses.push(ParameterUse {
            reference: reference.clone(),
            kind: expected,
        });
        Ok(reference)
    }

    fn statement(&mut self) -> Result<Statement, LanguageError> {
        let pattern = if self.eat_keyword("MATCH").is_some() {
            let pattern = self.pattern()?;
            self.match_clause_seen = true;
            Some(pattern)
        } else {
            None
        };
        let as_of_position = if self.is_keyword("AS") {
            Some(self.position())
        } else {
            None
        };
        let as_of = if as_of_position.is_some() {
            self.bump();
            self.expect_keyword("OF")?;
            Some(self.parameter(ParameterKind::Integer)?)
        } else {
            None
        };
        if pattern.is_none() && self.is_keyword("WHERE") {
            return Err(syntax(self.position(), "WHERE requires a preceding MATCH"));
        }
        if pattern.is_some() && self.is_keyword("MATCH") {
            return Err(self.second_match_error());
        }
        let predicate = if self.eat_keyword("WHERE").is_some() {
            Some(self.disjunction()?)
        } else {
            None
        };

        if !self.is_mutation() {
            let Some(pattern) = pattern else {
                return Err(self.unexpected("MATCH or a write clause"));
            };
            self.expect_keyword("RETURN")?;
            let returning = Some(self.return_clause()?);
            return Ok(Statement {
                pattern: Some(pattern),
                as_of,
                predicate,
                mutations: Vec::new(),
                returning,
            });
        }

        if let Some(position) = as_of_position {
            return Err(unsupported(position, "AS OF is read-only"));
        }
        let mut mutations = Vec::new();
        while self.is_mutation() {
            mutations.push(self.mutation()?);
        }
        let returning = if self.eat_keyword("RETURN").is_some() {
            Some(self.return_clause()?)
        } else {
            None
        };
        Ok(Statement {
            pattern,
            as_of: None,
            predicate,
            mutations,
            returning,
        })
    }

    fn pattern(&mut self) -> Result<Pattern, LanguageError> {
        let position = self.position();
        let path_variable =
            if Self::is_identifier_kind(self.kind_at(0)) && self.kind_at(1) == &Kind::Symbol("=") {
                let variable = self.identifier()?;
                self.expect_symbol("=")?;
                Some(variable)
            } else {
                None
            };
        let start = self.node_pattern()?;
        let relationship = if self.is_symbol("-") || self.is_symbol("<-") {
            Some((self.relationship_pattern()?, self.node_pattern()?))
        } else {
            None
        };
        if self.is_symbol(",") {
            return Err(unsupported(
                self.position(),
                "MATCH accepts exactly one pattern",
            ));
        }
        if self.is_symbol("-") || self.is_symbol("<-") {
            return Err(unsupported(
                self.position(),
                "chained relationships are not supported",
            ));
        }
        Ok(Pattern {
            path_variable,
            start,
            relationship,
            position,
        })
    }

    fn node_pattern(&mut self) -> Result<NodePattern, LanguageError> {
        let position = self.expect_symbol("(")?;
        let variable = Some(self.identifier()?);
        let mut labels = Vec::new();
        while self.eat_symbol(":").is_some() {
            labels.push(self.name("a label name")?);
        }
        let anchor = self.anchor()?;
        let properties = self.properties(true)?;
        self.expect_symbol(")")?;
        Ok(NodePattern {
            variable,
            labels,
            anchor,
            properties,
            position,
        })
    }

    fn relationship_pattern(&mut self) -> Result<RelationshipPattern, LanguageError> {
        let position = self.position();
        let incoming = if self.eat_symbol("<-").is_some() {
            true
        } else {
            self.expect_symbol("-")?;
            false
        };
        self.expect_symbol("[")?;
        let variable = if Self::is_identifier_kind(&self.token().kind)
            && !self.is_symbol(":")
            && !self.is_symbol("]")
            && !self.is_symbol("*")
            && !self.is_symbol("{")
            && !matches!(&self.token().kind, Kind::Word(word) if ["KEY", "ID"].iter().any(|kw| word.eq_ignore_ascii_case(kw)))
        {
            Some(self.identifier()?)
        } else {
            None
        };
        let mut types = Vec::new();
        if self.eat_symbol(":").is_some() {
            types.push(self.name("a relationship type")?);
            while self.eat_symbol("|").is_some() {
                self.eat_symbol(":");
                types.push(self.name("a relationship type")?);
            }
        }
        let anchor = self.anchor()?;
        let (max_hops, hop_position) = if let Some(star) = self.eat_symbol("*") {
            let minimum = match &self.token().kind {
                Kind::Unsigned(value) => value.clone(),
                _ => {
                    return Err(unsupported(
                        star,
                        "only relationship bounds of the form *1..N are supported",
                    ));
                }
            };
            self.bump();
            if minimum != "1" {
                return Err(unsupported(
                    star,
                    "only relationship bounds of the form *1..N are supported",
                ));
            }
            if self.eat_symbol("..").is_none() {
                return Err(unsupported(
                    star,
                    "only relationship bounds of the form *1..N are supported",
                ));
            }
            let maximum_position = self.position();
            let maximum = match &self.token().kind {
                Kind::Unsigned(value) => value.clone(),
                _ => {
                    return Err(unsupported(
                        star,
                        "only relationship bounds of the form *1..N are supported",
                    ));
                }
            };
            self.bump();
            if maximum == "0" || (maximum.len() > 1 && maximum.starts_with('0')) {
                return Err(syntax(
                    maximum_position,
                    "hop bounds must be positive decimals without leading zeroes",
                ));
            }
            let numeric = maximum.parse::<u64>().unwrap_or(u64::MAX);
            if numeric == 0 {
                return Err(syntax(
                    maximum_position,
                    "hop bounds must be greater than zero",
                ));
            }
            if numeric > MAX_HOPS || maximum.len() > MAX_HOPS.to_string().len() {
                return Err(error(
                    LanguageErrorKind::Limit,
                    maximum_position,
                    format!("hop bound exceeds the dialect maximum of {MAX_HOPS}"),
                ));
            }
            (Some(numeric), Some(star))
        } else {
            (None, None)
        };
        let properties = self.properties(true)?;
        self.expect_symbol("]")?;
        let direction = if incoming {
            self.expect_symbol("-")?;
            Direction::In
        } else if self.eat_symbol("->").is_some() {
            Direction::Out
        } else {
            self.expect_symbol("-")?;
            Direction::Both
        };
        Ok(RelationshipPattern {
            variable,
            types,
            anchor,
            max_hops,
            hop_position,
            properties,
            direction,
            position,
        })
    }

    fn anchor(&mut self) -> Result<Option<Anchor>, LanguageError> {
        let (kind, position) = if let Some(position) = self.eat_keyword("KEY") {
            (AnchorKind::Key, position)
        } else if let Some(position) = self.eat_keyword("ID") {
            (AnchorKind::Id, position)
        } else {
            return Ok(None);
        };
        let parameter = self.parameter(match kind {
            AnchorKind::Key => ParameterKind::String,
            AnchorKind::Id => ParameterKind::NonNegativeInteger,
        })?;
        Ok(Some(Anchor {
            kind,
            parameter,
            position,
        }))
    }

    fn properties(&mut self, predicate_map: bool) -> Result<Vec<PropertyPair>, LanguageError> {
        if self.eat_symbol("{").is_none() {
            return Ok(Vec::new());
        }
        let mut properties = Vec::new();
        if self.eat_symbol("}").is_some() {
            return Ok(properties);
        }
        loop {
            let name = self.name("a property name")?;
            self.expect_symbol(":")?;
            let parameter = self.parameter(ParameterKind::Any)?;
            if predicate_map {
                self.count_predicate(name.position)?;
            }
            properties.push(PropertyPair { name, parameter });
            if self.eat_symbol("}").is_some() {
                break;
            }
            self.expect_symbol(",")?;
        }
        Ok(properties)
    }

    fn count_predicate(&mut self, position: usize) -> Result<(), LanguageError> {
        self.predicate_count += 1;
        if self.predicate_count > MAX_PREDICATES {
            return Err(error(
                LanguageErrorKind::Limit,
                position,
                format!("predicate count exceeds {MAX_PREDICATES}"),
            ));
        }
        Ok(())
    }

    fn disjunction(&mut self) -> Result<Predicate, LanguageError> {
        let mut terms = vec![self.conjunction()?];
        while self.eat_keyword("OR").is_some() {
            terms.push(self.conjunction()?);
        }
        Ok(if terms.len() == 1 {
            terms.remove(0)
        } else {
            Predicate::Or(terms)
        })
    }

    fn conjunction(&mut self) -> Result<Predicate, LanguageError> {
        let mut terms = vec![self.negation()?];
        while self.eat_keyword("AND").is_some() {
            terms.push(self.negation()?);
        }
        Ok(if terms.len() == 1 {
            terms.remove(0)
        } else {
            Predicate::And(terms)
        })
    }

    fn negation(&mut self) -> Result<Predicate, LanguageError> {
        if self.eat_keyword("NOT").is_some() {
            if self.is_keyword("NOT") {
                return Err(syntax(
                    self.position(),
                    "NOT may prefix only one predicate; use parentheses to nest negation",
                ));
            }
            return Ok(Predicate::Not(Box::new(self.negation()?)));
        }
        self.atomic_predicate()
    }

    fn atomic_predicate(&mut self) -> Result<Predicate, LanguageError> {
        if let Some(position) = self.eat_symbol("(") {
            self.parenthesis_depth += 1;
            if self.parenthesis_depth > MAX_PAREN_DEPTH {
                return Err(error(
                    LanguageErrorKind::Limit,
                    position,
                    format!("predicate nesting exceeds {MAX_PAREN_DEPTH}"),
                ));
            }
            let predicate = self.disjunction()?;
            self.expect_symbol(")")?;
            self.parenthesis_depth -= 1;
            return Ok(predicate);
        }
        let position = self.position();
        let variable = self.identifier()?;
        self.expect_symbol(".")?;
        let field = self.name("a property name")?;
        let predicate = if self.eat_symbol("=").is_some() {
            let parameter = self.parameter(ParameterKind::Any)?;
            Predicate::Compare {
                variable,
                field,
                operation: Compare::Equal,
                parameter,
            }
        } else if self.eat_symbol("<>").is_some() {
            let parameter = self.parameter(ParameterKind::Any)?;
            Predicate::Compare {
                variable,
                field,
                operation: Compare::NotEqual,
                parameter,
            }
        } else if self.eat_symbol("<=").is_some() {
            let parameter = self.parameter(ParameterKind::Any)?;
            Predicate::Compare {
                variable,
                field,
                operation: Compare::LessEqual,
                parameter,
            }
        } else if self.eat_symbol(">=").is_some() {
            let parameter = self.parameter(ParameterKind::Any)?;
            Predicate::Compare {
                variable,
                field,
                operation: Compare::GreaterEqual,
                parameter,
            }
        } else if self.eat_symbol("<").is_some() {
            let parameter = self.parameter(ParameterKind::Any)?;
            Predicate::Compare {
                variable,
                field,
                operation: Compare::Less,
                parameter,
            }
        } else if self.eat_symbol(">").is_some() {
            let parameter = self.parameter(ParameterKind::Any)?;
            Predicate::Compare {
                variable,
                field,
                operation: Compare::Greater,
                parameter,
            }
        } else if self.eat_keyword("IS").is_some() {
            let negated = self.eat_keyword("NOT").is_some();
            self.expect_keyword("NULL")?;
            Predicate::IsNull {
                variable,
                field,
                negated,
            }
        } else if self.is_keyword("IN") {
            return Err(unsupported(
                self.position(),
                "IN predicates are not supported",
            ));
        } else {
            return Err(syntax(
                self.position(),
                "expected a comparison operator or IS [NOT] NULL",
            ));
        };
        self.count_predicate(position)?;
        Ok(predicate)
    }

    fn mutation(&mut self) -> Result<Mutation, LanguageError> {
        if let Some(position) = self.eat_keyword("CREATE") {
            let mut creations = vec![self.creation(false)?];
            while self.eat_symbol(",").is_some() {
                creations.push(self.creation(false)?);
            }
            return Ok(Mutation::Create {
                creations,
                position,
            });
        }
        if let Some(position) = self.eat_keyword("MERGE") {
            let creation = self.creation(true)?;
            if matches!(&creation, Creation::Edge(edge) if edge.key.is_none()) {
                return Err(syntax(position, "MERGE relationships require KEY"));
            }
            return Ok(Mutation::Merge { creation, position });
        }
        if let Some(position) = self.eat_keyword("SET") {
            let mut assignments = vec![self.assignment()?];
            while self.eat_symbol(",").is_some() {
                assignments.push(self.assignment()?);
            }
            return Ok(Mutation::Set {
                assignments,
                position,
            });
        }
        if let Some(position) = self.eat_keyword("REMOVE") {
            let mut removals = vec![self.removal()?];
            while self.eat_symbol(",").is_some() {
                removals.push(self.removal()?);
            }
            return Ok(Mutation::Remove { removals, position });
        }
        let detach = self.eat_keyword("DETACH").is_some();
        let position = self.expect_keyword("DELETE")?;
        let mut variables = vec![self.identifier()?];
        while self.eat_symbol(",").is_some() {
            variables.push(self.identifier()?);
        }
        Ok(Mutation::Delete {
            variables,
            detach,
            position,
        })
    }

    fn creation(&mut self, merge: bool) -> Result<Creation, LanguageError> {
        if !self.is_symbol("(") {
            return Err(self.unexpected("a node or relationship creation"));
        }
        let is_edge = Self::is_identifier_kind(self.kind_at(1))
            && self.kind_at(2) == &Kind::Symbol(")")
            && self.kind_at(3) == &Kind::Symbol("-");
        if is_edge {
            return self.creation_edge(merge).map(Creation::Edge);
        }
        self.creation_node().map(Creation::Node)
    }

    fn creation_node(&mut self) -> Result<CreationNode, LanguageError> {
        let position = self.expect_symbol("(")?;
        let variable = self.identifier()?;
        let mut labels = Vec::new();
        while self.eat_symbol(":").is_some() {
            labels.push(self.name("a label name")?);
        }
        let key = if self.eat_keyword("KEY").is_some() {
            self.parameter(ParameterKind::String)?
        } else {
            return Err(self.unexpected("KEY and a string parameter"));
        };
        let properties = self.properties(false)?;
        self.expect_symbol(")")?;
        Ok(CreationNode {
            variable,
            labels,
            key,
            properties,
            position,
        })
    }

    fn creation_edge(&mut self, merge: bool) -> Result<CreationEdge, LanguageError> {
        let position = self.expect_symbol("(")?;
        let source = self.identifier()?;
        self.expect_symbol(")")?;
        self.expect_symbol("-")?;
        self.expect_symbol("[")?;
        let variable = self.identifier()?;
        self.expect_symbol(":")?;
        let relationship_type = self.name("a relationship type")?;
        let key = if self.eat_keyword("KEY").is_some() {
            Some(self.parameter(ParameterKind::String)?)
        } else {
            None
        };
        if merge && key.is_none() {
            return Err(syntax(
                self.position(),
                "MERGE relationships require an explicit KEY parameter",
            ));
        }
        let properties = self.properties(false)?;
        self.expect_symbol("]")?;
        self.expect_symbol("->")?;
        self.expect_symbol("(")?;
        let destination = self.identifier()?;
        self.expect_symbol(")")?;
        Ok(CreationEdge {
            variable,
            source,
            relationship_type,
            key,
            properties,
            destination,
            position,
        })
    }

    fn field_target_or_label(
        &mut self,
    ) -> Result<Result<FieldTarget, (Name, Name)>, LanguageError> {
        let variable = self.identifier()?;
        if self.eat_symbol(".").is_some() {
            let field = self.name("a property name")?;
            Ok(Ok(FieldTarget { variable, field }))
        } else if self.eat_symbol(":").is_some() {
            let label = self.name("a label name")?;
            Ok(Err((variable, label)))
        } else {
            Err(self.unexpected("a property field or label"))
        }
    }

    fn assignment(&mut self) -> Result<Assignment, LanguageError> {
        match self.field_target_or_label()? {
            Ok(target) => {
                self.expect_symbol("=")?;
                let parameter = self.parameter(ParameterKind::Any)?;
                Ok(Assignment::Property { target, parameter })
            }
            Err((variable, label)) => Ok(Assignment::Label { variable, label }),
        }
    }

    fn removal(&mut self) -> Result<Removal, LanguageError> {
        match self.field_target_or_label()? {
            Ok(target) => Ok(Removal::Property(target)),
            Err((variable, label)) => Ok(Removal::Label { variable, label }),
        }
    }

    fn return_clause(&mut self) -> Result<ReturnClause, LanguageError> {
        let position = self.tokens[self.at - 1].position;
        if self.is_symbol("*") {
            return Err(unsupported(
                self.position(),
                "RETURN * is not supported; name each projected variable or field",
            ));
        }
        let mut items = vec![self.return_item()?];
        while self.eat_symbol(",").is_some() {
            items.push(self.return_item()?);
        }
        let mut order_by = Vec::new();
        if self.eat_keyword("ORDER").is_some() {
            self.expect_keyword("BY")?;
            order_by.push(self.sort_item()?);
            while self.eat_symbol(",").is_some() {
                order_by.push(self.sort_item()?);
            }
        }
        let skip = if self.eat_keyword("SKIP").is_some() {
            Some(self.parameter(ParameterKind::NonNegativeInteger)?)
        } else {
            None
        };
        let limit = if self.eat_keyword("LIMIT").is_some() {
            Some(self.parameter(ParameterKind::NonNegativeInteger)?)
        } else {
            None
        };
        Ok(ReturnClause {
            items,
            order_by,
            skip,
            limit,
            position,
        })
    }

    fn return_item(&mut self) -> Result<ReturnItem, LanguageError> {
        let position = self.position();
        let variable = self.identifier()?;
        let value = if self.eat_symbol(".").is_some() {
            let field = self.name("a property name")?;
            ReturnValue::Field(FieldTarget { variable, field })
        } else {
            ReturnValue::Variable(variable)
        };
        let alias = if self.eat_keyword("AS").is_some() {
            Some(self.identifier()?)
        } else {
            None
        };
        Ok(ReturnItem {
            value,
            alias,
            position,
        })
    }

    fn sort_item(&mut self) -> Result<SortItem, LanguageError> {
        let name = self.identifier()?;
        let direction = if self.eat_keyword("DESC").is_some() {
            SortDirection::Descending
        } else {
            self.eat_keyword("ASC");
            SortDirection::Ascending
        };
        Ok(SortItem { name, direction })
    }
}
