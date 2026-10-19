// SPDX-License-Identifier: Apache-2.0
//! An R2RML mapping document read into model.rs's structures, and checked
//! as R2RML's conformance asks (https://www.w3.org/TR/r2rml/): what is not a
//! conforming mapping is refused here, before any table is read.

use std::collections::HashMap;

use oxrdf::vocab::rdf;
use oxrdf::{Graph, Literal, NamedNode, NamedOrBlankNode, NamedOrBlankNodeRef, Term};
use oxrdfio::{RdfFormat, RdfParser};

use crate::error::{EResult, EngineError};
use crate::r2rml::model::*;

pub const RR: &str = "http://www.w3.org/ns/r2rml#";

fn rr(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{RR}{local}"))
}

fn nonconforming(message: impl Into<String>) -> EngineError {
    EngineError::mapping(format!("non-conforming R2RML mapping: {}", message.into()))
}

/// The SQL versions R2RML's community listed (https://www.w3.org/2001/sw/wiki/RDB2RDF/SQL_Version_IRIs)
const SQL_VERSIONS: &[&str] = &[
    "SQL2008",
    "Oracle",
    "MySQL",
    "MSSQLServer",
    "HSQLDB",
    "PostgreSQL",
    "DB2",
    "Informix",
    "Ingres",
    "Progress",
    "SybaseASE",
    "SybaseSQLAnywhere",
    "Virtuoso",
    "Firebird",
];

struct Reader<'a> {
    graph: &'a Graph,
}

fn node_text(node: &NamedOrBlankNode) -> String {
    match node {
        NamedOrBlankNode::NamedNode(n) => {
            format!("<{}>", n.as_str().strip_prefix(NO_BASE).unwrap_or(n.as_str()))
        }
        NamedOrBlankNode::BlankNode(b) => format!("_:{}", b.as_str()),
    }
}

impl Reader<'_> {
    fn values(&self, subject: &NamedOrBlankNode, predicate: &NamedNode) -> Vec<Term> {
        self.graph
            .objects_for_subject_predicate(NamedOrBlankNodeRef::from(subject), predicate)
            .collect()
    }

    fn at_most_one(
        &self,
        subject: &NamedOrBlankNode,
        predicate: &NamedNode,
    ) -> EResult<Option<Term>> {
        let mut values = self.values(subject, predicate);
        if values.len() > 1 {
            return Err(nonconforming(format!(
                "{} has more than one {}",
                node_text(subject),
                predicate
            )));
        }
        Ok(values.pop())
    }

    fn string(&self, subject: &NamedOrBlankNode, predicate: &NamedNode) -> EResult<Option<String>> {
        match self.at_most_one(subject, predicate)? {
            None => Ok(None),
            Some(Term::Literal(l)) if l.language().is_none() => Ok(Some(l.value().to_owned())),
            Some(other) => Err(nonconforming(format!(
                "the {} of {} is {}, not a string",
                predicate,
                node_text(subject),
                other
            ))),
        }
    }

    fn resource(term: Term, what: &str) -> EResult<NamedOrBlankNode> {
        match term {
            Term::NamedNode(n) => Ok(n.into()),
            Term::BlankNode(b) => Ok(b.into()),
            Term::Literal(l) => Err(nonconforming(format!("{what} is the literal {l}"))),
        }
    }

    /// A term map: its node's rr:constant, rr:column or rr:template, and
    /// its term type, language and datatype.
    fn term_map(&self, node: &NamedOrBlankNode, role: Role) -> EResult<TermMap> {
        let constant = self.at_most_one(node, &rr("constant"))?;
        let column = self.string(node, &rr("column"))?;
        let template = self.string(node, &rr("template"))?;
        let value = match (constant, column, template) {
            (Some(c), None, None) => TermMapValue::Constant(c),
            (None, Some(c), None) => TermMapValue::Column(c),
            (None, None, Some(t)) => TermMapValue::Template(
                Template::parse(&t).map_err(nonconforming)?,
            ),
            (None, None, None) => {
                return Err(nonconforming(format!(
                    "the {} {} has none of rr:constant, rr:column, rr:template",
                    role.name(),
                    node_text(node)
                )));
            }
            _ => {
                return Err(nonconforming(format!(
                    "the {} {} has more than one of rr:constant, rr:column, rr:template",
                    role.name(),
                    node_text(node)
                )));
            }
        };
        let language = match self.at_most_one(node, &rr("language"))? {
            None => None,
            Some(Term::Literal(l)) if l.language().is_none() => {
                let tag = l.value();
                let checked = Literal::new_language_tagged_literal("", tag.to_owned()).map_err(|e| {
                    nonconforming(format!("\"{tag}\" is not a language tag: {e}"))
                })?;
                // a valid tag's language is ISO 639's, of two or three letters
                // (BCP 47 §2.2.1), or private (x-) or grandfathered (i-): the
                // grammar alone takes "english"
                let primary = tag.split('-').next().unwrap_or("");
                if !(primary.len() == 2 || primary.len() == 3 || primary.eq_ignore_ascii_case("x")
                    || primary.eq_ignore_ascii_case("i"))
                {
                    return Err(nonconforming(format!("\"{tag}\" is not a valid language tag")));
                }
                checked.language().map(str::to_owned)
            }
            Some(other) => {
                return Err(nonconforming(format!("rr:language {other} is not a string")));
            }
        };
        let datatype = match self.at_most_one(node, &rr("datatype"))? {
            None => None,
            Some(Term::NamedNode(n)) => Some(n),
            Some(other) => {
                return Err(nonconforming(format!("rr:datatype {other} is not an IRI")));
            }
        };
        let declared = match self.at_most_one(node, &rr("termType"))? {
            None => None,
            Some(Term::NamedNode(n)) if n == rr("IRI") => Some(TermType::Iri),
            Some(Term::NamedNode(n)) if n == rr("BlankNode") => Some(TermType::BlankNode),
            Some(Term::NamedNode(n)) if n == rr("Literal") => Some(TermType::Literal),
            Some(other) => {
                return Err(nonconforming(format!("rr:termType {other} is not one of R2RML's")));
            }
        };
        // R2RML §7.4: the default term type
        let term_type = match (&value, declared) {
            (TermMapValue::Constant(c), declared) => {
                let own = match c {
                    Term::NamedNode(_) => TermType::Iri,
                    Term::BlankNode(_) => {
                        return Err(nonconforming("a constant blank node"));
                    }
                    Term::Literal(_) => TermType::Literal,
                };
                if declared.is_some_and(|d| d != own) {
                    return Err(nonconforming(format!(
                        "the constant {c} is not of its term map's rr:termType"
                    )));
                }
                own
            }
            (_, Some(t)) => t,
            (TermMapValue::Column(_), None) if role == Role::Object => TermType::Literal,
            (_, None) if role == Role::Object && (language.is_some() || datatype.is_some()) => {
                TermType::Literal
            }
            _ => TermType::Iri,
        };
        let allowed = match role {
            Role::Subject => matches!(term_type, TermType::Iri | TermType::BlankNode),
            Role::Predicate | Role::Graph => term_type == TermType::Iri,
            Role::Object => true,
        };
        if !allowed {
            return Err(nonconforming(format!(
                "a {} of rr:termType {:?}",
                role.name(),
                term_type
            )));
        }
        if (language.is_some() || datatype.is_some()) && term_type != TermType::Literal {
            return Err(nonconforming(
                "rr:language or rr:datatype on a term map that makes no literal",
            ));
        }
        if language.is_some() && datatype.is_some() {
            return Err(nonconforming("both rr:language and rr:datatype on one term map"));
        }
        if matches!(value, TermMapValue::Constant(_)) && (language.is_some() || datatype.is_some()) {
            return Err(nonconforming("rr:language or rr:datatype on a constant term map"));
        }
        Ok(TermMap {
            value,
            term_type,
            language,
            datatype,
        })
    }

    /// A term map given as its node (rr:xMap) or as a constant (rr:x).
    fn maps(
        &self,
        node: &NamedOrBlankNode,
        map: &str,
        shortcut: &str,
        role: Role,
    ) -> EResult<Vec<TermMap>> {
        let mut maps = Vec::new();
        for value in self.values(node, &rr(map)) {
            let n = Self::resource(value, &format!("rr:{map}"))?;
            maps.push(self.term_map(&n, role)?);
        }
        for value in self.values(node, &rr(shortcut)) {
            maps.push(constant_map(value, role)?);
        }
        Ok(maps)
    }

    fn graph_maps(&self, node: &NamedOrBlankNode) -> EResult<Vec<GraphMap>> {
        Ok(self
            .maps(node, "graphMap", "graph", Role::Graph)?
            .into_iter()
            .map(|m| match &m.value {
                TermMapValue::Constant(Term::NamedNode(n)) if *n == rr("defaultGraph") => {
                    GraphMap::Default
                }
                _ => GraphMap::Map(m),
            })
            .collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Subject,
    Predicate,
    Object,
    Graph,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Subject => "subject map",
            Role::Predicate => "predicate map",
            Role::Object => "object map",
            Role::Graph => "graph map",
        }
    }
}

fn constant_map(value: Term, role: Role) -> EResult<TermMap> {
    let term_type = match &value {
        Term::NamedNode(_) => TermType::Iri,
        Term::Literal(_) if role == Role::Object => TermType::Literal,
        other => {
            return Err(nonconforming(format!(
                "the constant {other} cannot be a {}",
                role.name()
            )));
        }
    };
    Ok(TermMap {
        value: TermMapValue::Constant(value),
        term_type,
        language: None,
        datatype: None,
    })
}

/// The base a document that has none is parsed against: the names it gives
/// its own resources (<#TriplesMap1>, as R2RML's examples write them)
/// resolve against it, while a relative IRI it gives a triple to make -- a
/// constant, a class, a datatype -- is refused (unresolved_terms()).
const NO_BASE: &str = "http://kb.invalid/mapping/";
const NO_BASE_ROOT: &str = "http://kb.invalid/";

/// The document parsed, and its base IRI: the caller's, or the document's
/// own @base.
pub fn parse_document(document: &str, base_iri: Option<&str>) -> EResult<(Graph, Option<String>)> {
    let parser = RdfParser::from_format(RdfFormat::Turtle).with_base_iri(base_iri.unwrap_or(NO_BASE))?;
    let mut reader = parser.for_slice(document);
    let mut graph = Graph::new();
    for triple in reader.by_ref() {
        let quad = triple.map_err(|e| nonconforming(format!("its Turtle does not parse: {e}")))?;
        graph.insert(oxrdf::Triple::from(quad));
    }
    let base = match base_iri {
        Some(base) => Some(base.to_owned()),
        None => reader.base_iri().filter(|b| *b != NO_BASE).map(str::to_owned),
    };
    unresolved_terms(&graph)?;
    Ok((graph, base))
}

/// A relative IRI the document's triples would hold, with no base to
/// resolve it against: refused.
fn unresolved_terms(graph: &Graph) -> EResult<()> {
    let made = ["constant", "subject", "predicate", "object", "graph", "class", "datatype"].map(rr);
    for t in graph {
        let Term::NamedNode(n) = &t.object else {
            continue;
        };
        if n.as_str().starts_with(NO_BASE_ROOT) && made.contains(&t.predicate) {
            let relative = n.as_str().strip_prefix(NO_BASE).unwrap_or(n.as_str());
            return Err(nonconforming(format!(
                "<{relative}>, the object of its {}, is a relative IRI, and the mapping has no base IRI \
                 to resolve it against: give it one, as kb.add_mapping()'s base_iri or as an @base",
                t.predicate
            )));
        }
    }
    Ok(())
}

pub fn read_mapping(graph: &Graph) -> EResult<Mapping> {
    let reader = Reader { graph };
    // a triples map is a resource with a logical table, typed or not
    let mut ids: Vec<NamedOrBlankNode> = graph
        .triples_for_predicate(&rr("logicalTable"))
        .map(|t| t.subject)
        .collect();
    for typed in graph.subjects_for_predicate_object(&rdf::TYPE, &rr("TriplesMap")) {
        if !ids.contains(&typed) {
            ids.push(typed);
        }
    }
    ids.sort_by_key(node_text);
    ids.dedup();
    if ids.is_empty() {
        return Err(nonconforming("the document has no triples map"));
    }
    let index: HashMap<NamedOrBlankNode, usize> =
        ids.iter().enumerate().map(|(i, id)| (id.clone(), i)).collect();

    let mut triples_maps = Vec::with_capacity(ids.len());
    for id in &ids {
        let name = node_text(id);
        // the logical table
        let tables = reader.values(id, &rr("logicalTable"));
        if tables.len() != 1 {
            return Err(nonconforming(format!(
                "the triples map {name} has {} logical tables, not one",
                tables.len()
            )));
        }
        let table_node = Reader::resource(tables.into_iter().next().unwrap_or_else(|| unreachable!()), "rr:logicalTable")?;
        let table = match (
            reader.string(&table_node, &rr("tableName"))?,
            reader.string(&table_node, &rr("sqlQuery"))?,
        ) {
            (Some(t), None) => LogicalTable::Table(t),
            (None, Some(q)) => {
                for version in reader.values(&table_node, &rr("sqlVersion")) {
                    let known = match &version {
                        Term::NamedNode(n) => n
                            .as_str()
                            .strip_prefix(RR)
                            .is_some_and(|l| SQL_VERSIONS.contains(&l)),
                        _ => false,
                    };
                    if !known {
                        return Err(nonconforming(format!("an unknown rr:sqlVersion {version}")));
                    }
                }
                LogicalTable::Query(q)
            }
            (None, None) => {
                return Err(nonconforming(format!(
                    "the logical table of {name} has neither rr:tableName nor rr:sqlQuery"
                )));
            }
            (Some(_), Some(_)) => {
                return Err(nonconforming(format!(
                    "the logical table of {name} has both rr:tableName and rr:sqlQuery"
                )));
            }
        };

        // the subject map
        let mut subjects = Vec::new();
        for value in reader.values(id, &rr("subjectMap")) {
            subjects.push(Reader::resource(value, "rr:subjectMap")?);
        }
        let subject_constants = reader.values(id, &rr("subject"));
        if subjects.len() + subject_constants.len() != 1 {
            return Err(nonconforming(format!(
                "the triples map {name} has {} subject maps, not one",
                subjects.len() + subject_constants.len()
            )));
        }
        let (subject, classes, graphs) = if let Some(node) = subjects.first() {
            let subject = reader.term_map(node, Role::Subject)?;
            let mut classes = Vec::new();
            for class in reader.values(node, &rr("class")) {
                match class {
                    Term::NamedNode(n) => classes.push(n),
                    other => return Err(nonconforming(format!("rr:class {other} is not an IRI"))),
                }
            }
            (subject, classes, reader.graph_maps(node)?)
        } else {
            let constant = subject_constants.into_iter().next().unwrap_or_else(|| unreachable!());
            (constant_map(constant, Role::Subject)?, Vec::new(), Vec::new())
        };

        // the predicate-object maps
        let mut predicate_object_maps = Vec::new();
        for pom in reader.values(id, &rr("predicateObjectMap")) {
            let pom = Reader::resource(pom, "rr:predicateObjectMap")?;
            let predicates = reader.maps(&pom, "predicateMap", "predicate", Role::Predicate)?;
            if predicates.is_empty() {
                return Err(nonconforming(format!(
                    "a predicate-object map of {name} has no predicate map"
                )));
            }
            let mut objects = Vec::new();
            for value in reader.values(&pom, &rr("objectMap")) {
                let node = Reader::resource(value, "rr:objectMap")?;
                let parents = reader.values(&node, &rr("parentTriplesMap"));
                if parents.is_empty() {
                    objects.push(ObjectMap::Term(reader.term_map(&node, Role::Object)?));
                    continue;
                }
                if parents.len() > 1 {
                    return Err(nonconforming("a referencing object map with more than one parent"));
                }
                let parent = Reader::resource(parents.into_iter().next().unwrap_or_else(|| unreachable!()), "rr:parentTriplesMap")?;
                let Some(&parent) = index.get(&parent) else {
                    return Err(nonconforming(format!(
                        "rr:parentTriplesMap {} is no triples map",
                        node_text(&parent)
                    )));
                };
                let mut joins = Vec::new();
                for join in reader.values(&node, &rr("joinCondition")) {
                    let join = Reader::resource(join, "rr:joinCondition")?;
                    match (
                        reader.string(&join, &rr("child"))?,
                        reader.string(&join, &rr("parent"))?,
                    ) {
                        (Some(child), Some(parent)) => joins.push((child, parent)),
                        _ => {
                            return Err(nonconforming(
                                "a join condition needs one rr:child and one rr:parent",
                            ));
                        }
                    }
                }
                objects.push(ObjectMap::Ref { parent, joins });
            }
            for value in reader.values(&pom, &rr("object")) {
                objects.push(ObjectMap::Term(constant_map(value, Role::Object)?));
            }
            if objects.is_empty() {
                return Err(nonconforming(format!(
                    "a predicate-object map of {name} has no object map"
                )));
            }
            predicate_object_maps.push(PredicateObjectMap {
                predicates,
                objects,
                graphs: reader.graph_maps(&pom)?,
            });
        }
        triples_maps.push(TriplesMap {
            id: name,
            table,
            subject,
            classes,
            graphs,
            predicate_object_maps,
        });
    }

    // a referencing object map without a join condition reads its parent's
    // subject over its own rows: the two logical tables must be one
    for tm in &triples_maps {
        for pom in &tm.predicate_object_maps {
            for object in &pom.objects {
                if let ObjectMap::Ref { parent, joins } = object {
                    if joins.is_empty()
                        && triples_maps[*parent].table.effective_sql() != tm.table.effective_sql()
                    {
                        return Err(nonconforming(format!(
                            "a referencing object map of {} has no join condition, and its parent's logical table is another",
                            tm.id
                        )));
                    }
                }
            }
        }
    }
    Ok(Mapping { triples_maps })
}
