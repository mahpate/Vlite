//! A bounded, read-only query grammar; never evaluates code or writes records.
use crate::{
    CollectionRef, Condition, Error, FacetCount, Metadata, MetadataValue, Result, SearchGroup,
    SearchHit,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub position: usize,
    pub message: String,
}
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "query byte {}: {}", self.position, self.message)
    }
}
impl std::error::Error for ParseError {}
#[derive(Clone, Debug)]
struct Token {
    text: String,
    quoted: bool,
    pos: usize,
}
fn lex(input: &str) -> std::result::Result<Vec<Token>, ParseError> {
    if input.len() > 16_384 {
        return Err(ParseError {
            position: 16_384,
            message: "query exceeds 16384 bytes".into(),
        });
    }
    let mut chars = input.char_indices().peekable();
    let mut tokens = Vec::new();
    while let Some((pos, c)) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c == '"' {
            let mut text = String::new();
            let mut closed = false;
            while let Some((i, c)) = chars.next() {
                if c == '"' {
                    closed = true;
                    break;
                }
                if c == '\\' {
                    let (_, next) = chars.next().ok_or(ParseError {
                        position: i,
                        message: "unfinished escape".into(),
                    })?;
                    match next {
                        '"' | '\\' => text.push(next),
                        'n' => text.push('\n'),
                        't' => text.push('\t'),
                        _ => {
                            return Err(ParseError {
                                position: i,
                                message: "unsupported escape".into(),
                            });
                        }
                    }
                } else {
                    text.push(c);
                }
            }
            if !closed {
                return Err(ParseError {
                    position: pos,
                    message: "unterminated string".into(),
                });
            }
            tokens.push(Token {
                text,
                quoted: true,
                pos,
            });
        } else if "(),=<>".contains(c) {
            let mut text = c.to_string();
            if "<>".contains(c) && chars.peek().is_some_and(|(_, c)| *c == '=') {
                chars.next();
                text.push('=');
            }
            tokens.push(Token {
                text,
                quoted: false,
                pos,
            });
        } else {
            let mut text = c.to_string();
            while chars
                .peek()
                .is_some_and(|(_, c)| !c.is_whitespace() && !"(),=<>\";".contains(*c))
            {
                text.push(chars.next().expect("peeked character").1);
            }
            if c == ';' {
                return Err(ParseError {
                    position: pos,
                    message: "statements are not supported".into(),
                });
            }
            tokens.push(Token {
                text,
                quoted: false,
                pos,
            });
        }
    }
    Ok(tokens)
}
#[derive(Clone, Debug)]
enum Predicate {
    Compare(String, String, MetadataValue),
    In(String, Vec<MetadataValue>),
    Exists(String),
    Geo(Condition),
}
#[derive(Clone, Debug)]
pub struct ParsedQuery {
    name: Option<String>,
    parameter: Option<String>,
    text: Option<String>,
    version: Option<String>,
    predicates: Vec<Predicate>,
    group: Option<String>,
    facet: Option<String>,
    order: Option<(String, bool)>,
    limit: usize,
}
#[derive(Clone, Debug, Default)]
pub struct QueryOutput {
    pub hits: Vec<SearchHit>,
    pub groups: Vec<SearchGroup>,
    pub facets: Vec<FacetCount>,
}
struct Parser {
    tokens: Vec<Token>,
    i: usize,
    end: usize,
}
impl Parser {
    fn error(&self, message: &str) -> ParseError {
        ParseError {
            position: self.tokens.get(self.i).map_or(self.end, |t| t.pos),
            message: message.into(),
        }
    }
    fn eat(&mut self, s: &str) -> bool {
        if self
            .tokens
            .get(self.i)
            .is_some_and(|t| !t.quoted && t.text.eq_ignore_ascii_case(s))
        {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn need(&mut self, s: &str) -> std::result::Result<(), ParseError> {
        if self.eat(s) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {s}")))
        }
    }
    fn token(&mut self) -> std::result::Result<Token, ParseError> {
        let t = self
            .tokens
            .get(self.i)
            .cloned()
            .ok_or_else(|| self.error("unexpected end"))?;
        self.i += 1;
        Ok(t)
    }
    fn ident(&mut self) -> std::result::Result<String, ParseError> {
        let t = self.token()?;
        if t.quoted
            || t.text.is_empty()
            || t.text.len() > 256
            || !t
                .text
                .chars()
                .all(|c| c.is_alphanumeric() || "_.-".contains(c))
        {
            return Err(ParseError {
                position: t.pos,
                message: "expected identifier".into(),
            });
        }
        Ok(t.text)
    }
    fn value(&mut self) -> std::result::Result<MetadataValue, ParseError> {
        let t = self.token()?;
        if t.quoted {
            return Ok(MetadataValue::String(t.text));
        }
        if t.text.eq_ignore_ascii_case("true") {
            return Ok(MetadataValue::Boolean(true));
        }
        if t.text.eq_ignore_ascii_case("false") {
            return Ok(MetadataValue::Boolean(false));
        }
        if let Ok(i) = t.text.parse::<i64>() {
            return Ok(MetadataValue::Integer(i));
        }
        if let Ok(f) = t.text.parse::<f64>()
            && f.is_finite()
        {
            return Ok(MetadataValue::Float(f));
        }
        Err(ParseError {
            position: t.pos,
            message: "expected quoted string, boolean or finite number".into(),
        })
    }
    fn number(&mut self) -> std::result::Result<f64, ParseError> {
        match self.value()? {
            MetadataValue::Integer(x) => Ok(x as f64),
            MetadataValue::Float(x) => Ok(x),
            _ => Err(self.error("expected number")),
        }
    }
    fn predicate(&mut self) -> std::result::Result<Predicate, ParseError> {
        if self.eat("EXISTS") {
            return Ok(Predicate::Exists(self.ident()?));
        }
        if self.eat("GEO") {
            self.need("(")?;
            let field = self.ident()?;
            self.need(",")?;
            let latitude = self.number()?;
            self.need(",")?;
            let longitude = self.number()?;
            self.need(",")?;
            let radius_meters = self.number()?;
            self.need(")")?;
            let c = Condition::GeoRadius {
                field,
                latitude,
                longitude,
                radius_meters,
            };
            crate::metadata::validate_filter(std::slice::from_ref(&c))
                .map_err(|_| self.error("invalid geo radius"))?;
            return Ok(Predicate::Geo(c));
        }
        let field = self.ident()?;
        if self.eat("EXISTS") {
            return Ok(Predicate::Exists(field));
        }
        if self.eat("IN") {
            self.need("(")?;
            let mut values = vec![self.value()?];
            while self.eat(",") {
                if values.len() >= 64 {
                    return Err(self.error("IN accepts at most 64 values"));
                }
                values.push(self.value()?);
            }
            self.need(")")?;
            return Ok(Predicate::In(field, values));
        }
        let op = self.token()?;
        if op.quoted || !["=", "<", "<=", ">", ">="].contains(&op.text.as_str()) {
            return Err(ParseError {
                position: op.pos,
                message: "expected =, <, <=, >, >= or IN".into(),
            });
        }
        Ok(Predicate::Compare(field, op.text, self.value()?))
    }
}
pub fn parse(input: &str) -> std::result::Result<ParsedQuery, ParseError> {
    let mut p = Parser {
        tokens: lex(input)?,
        i: 0,
        end: input.len(),
    };
    p.need("SEARCH")?;
    let mut q = ParsedQuery {
        name: None,
        parameter: None,
        text: None,
        version: None,
        predicates: Vec::new(),
        group: None,
        facet: None,
        order: None,
        limit: 10,
    };
    let first = p.ident()?;
    if first.eq_ignore_ascii_case("TEXT") && p.eat("MATCH") {
        let t = p.token()?;
        if !t.quoted {
            return Err(p.error("expected quoted search text"));
        }
        q.text = Some(t.text);
    } else {
        q.name = Some(first);
        p.need("NEAR")?;
        let t = p.token()?;
        if t.quoted || !t.text.starts_with('$') || t.text.len() < 2 {
            return Err(ParseError {
                position: t.pos,
                message: "expected $vector parameter".into(),
            });
        }
        q.parameter = Some(t.text[1..].into());
    }
    if p.eat("VERSION") {
        q.version = Some(p.ident()?);
    }
    if p.eat("TEXT") {
        let t = p.token()?;
        if !t.quoted {
            return Err(p.error("expected quoted search text"));
        }
        q.text = Some(t.text);
    }
    if p.eat("K") {
        let t = p.token()?;
        q.limit = t.text.parse().map_err(|_| ParseError {
            position: t.pos,
            message: "expected unsigned limit".into(),
        })?;
    }
    if p.eat("WHERE") {
        loop {
            if q.predicates.len() >= 64 {
                return Err(p.error("at most 64 predicates"));
            }
            q.predicates.push(p.predicate()?);
            if !p.eat("AND") {
                break;
            }
        }
    }
    if p.eat("GROUP") {
        p.need("BY")?;
        q.group = Some(p.ident()?);
    }
    if p.eat("FACET") {
        p.eat("BY");
        q.facet = Some(p.ident()?);
    }
    if p.eat("ORDER") {
        p.need("BY")?;
        let field = p.ident()?;
        let desc = p.eat("DESC");
        if !desc {
            p.eat("ASC");
        }
        q.order = Some((field, desc));
    }
    if p.eat("LIMIT") {
        let t = p.token()?;
        q.limit = t.text.parse().map_err(|_| ParseError {
            position: t.pos,
            message: "expected unsigned limit".into(),
        })?;
    }
    if q.limit > 1000 {
        return Err(p.error("limit must be 0..=1000"));
    }
    if p.i != p.tokens.len() {
        return Err(p.error("unexpected token or unsupported clause"));
    }
    Ok(q)
}
fn compare(a: &MetadataValue, b: &MetadataValue) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (MetadataValue::Integer(a), MetadataValue::Integer(b)) => Some(a.cmp(b)),
        (MetadataValue::Float(a), MetadataValue::Float(b)) => a.partial_cmp(b),
        (MetadataValue::String(a), MetadataValue::String(b)) => Some(a.cmp(b)),
        (MetadataValue::Boolean(a), MetadataValue::Boolean(b)) => Some(a.cmp(b)),
        _ => None,
    }
}
impl ParsedQuery {
    fn matches(&self, m: &Metadata) -> bool {
        self.predicates.iter().all(|p| match p {
            Predicate::Exists(f) => m.contains_key(f),
            Predicate::In(f, vs) => m.get(f).is_some_and(|v| vs.contains(v)),
            Predicate::Geo(c) => crate::metadata::matches(m, std::slice::from_ref(c)),
            Predicate::Compare(f, op, b) => m.get(f).is_some_and(|a| {
                if op == "=" {
                    a == b
                } else {
                    compare(a, b).is_some_and(|o| match op.as_str() {
                        "<" => o.is_lt(),
                        "<=" => o.is_le(),
                        ">" => o.is_gt(),
                        ">=" => o.is_ge(),
                        _ => false,
                    })
                }
            }),
        })
    }
    pub fn execute(
        &self,
        collection: CollectionRef<'_>,
        parameters: &BTreeMap<String, Vec<f32>>,
    ) -> Result<QueryOutput> {
        collection.db.ensure_healthy()?;
        let state = collection.db.state(collection.id);
        let depth = state.records.len();
        let mut hits = if let Some(n) = &self.name {
            let vector = parameters
                .get(self.parameter.as_deref().unwrap_or(""))
                .ok_or(Error::InvalidQuery("missing vector parameter"))?;
            if n == "default" {
                if self.version.is_some() {
                    return Err(Error::InvalidQuery(
                        "the legacy default vector has no model versions",
                    ));
                }
                collection.search(vector, depth)?
            } else {
                collection.search_named(n, self.version.as_deref(), vector, depth)?
            }
        } else {
            Vec::new()
        };
        hits.retain(|h| {
            collection
                .get(h.id)
                .ok()
                .flatten()
                .is_some_and(|r| self.matches(&r.metadata))
        });
        if let Some(text) = &self.text {
            let status = state.text_index.status();
            if !status.available {
                return Err(Error::IndexNotReady);
            }
            if status.declared_fields == 0 {
                return Err(Error::InvalidQuery("text fields must be declared"));
            }
            let terms: BTreeSet<_> = crate::text_index::tokenize(text).into_iter().collect();
            let lexical: Vec<_> = state
                .text_index
                .search(&terms, depth, |id| {
                    collection
                        .get(id)
                        .ok()
                        .flatten()
                        .is_some_and(|r| self.matches(&r.metadata))
                })
                .into_iter()
                .map(|(id, score)| SearchHit {
                    id,
                    distance: -score,
                })
                .collect();
            if self.name.is_some() {
                let fusion = crate::hybrid::fuse_ranked(
                    &hits,
                    &lexical,
                    1000,
                    crate::hybrid::FusionOptions::default(),
                )?;
                hits = fusion
                    .into_iter()
                    .map(|h| SearchHit {
                        id: h.id,
                        distance: -h.score,
                    })
                    .collect();
            } else {
                hits = lexical;
            }
        }
        hits.retain(|h| {
            collection
                .get(h.id)
                .ok()
                .flatten()
                .is_some_and(|r| self.matches(&r.metadata))
        });
        if let Some((field, desc)) = &self.order {
            hits.sort_by(|a, b| {
                let order = if field == "distance" {
                    a.distance.total_cmp(&b.distance)
                } else if field == "id" {
                    a.id.cmp(&b.id)
                } else {
                    let a = state.records[&a.id].metadata.get(field);
                    let b = state.records[&b.id].metadata.get(field);
                    match (a, b) {
                        (Some(a), Some(b)) => compare(a, b).unwrap_or(std::cmp::Ordering::Equal),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        _ => std::cmp::Ordering::Equal,
                    }
                };
                let order = if *desc { order.reverse() } else { order };
                order.then(a.id.cmp(&b.id))
            });
        }
        let mut out = QueryOutput::default();
        if let Some(field) = &self.facet {
            for h in &hits {
                if let Some(value) = state.records[&h.id].metadata.get(field) {
                    if let Some(f) = out.facets.iter_mut().find(|f| f.value == *value) {
                        f.count += 1;
                    } else {
                        out.facets.push(FacetCount {
                            value: value.clone(),
                            count: 1,
                        });
                    }
                }
            }
        }
        if let Some(field) = &self.group {
            for h in &hits {
                if let Some(value) = state.records[&h.id].metadata.get(field) {
                    if let Some(g) = out.groups.iter_mut().find(|g| g.value == *value) {
                        g.hits.push(h.clone());
                    } else if out.groups.len() < self.limit {
                        out.groups.push(SearchGroup {
                            value: value.clone(),
                            hits: vec![h.clone()],
                        });
                    }
                }
            }
        }
        hits.truncate(self.limit);
        out.hits = hits;
        Ok(out)
    }
}
