// SPDX-License-Identifier: Apache-2.0
//! A mapping compiled against its tables (docs/design.md §4.2): each
//! logical table described by the host -- its columns, their types -- each
//! column a mapping names found among them as PostgreSQL resolves an
//! identifier, and the mapping cut into units, a unit one triple template
//! -- a subject, a predicate, an object over one row, or over a row and its
//! parent's -- with the graphs its triples go to.

use std::collections::HashMap;
use std::hash::Hasher;

use oxiri::Iri;
use siphasher::sip::SipHasher24;
use oxrdf::vocab::rdf;
use oxrdf::{NamedNode, Term};

use crate::error::{EResult, EngineError};
use crate::host::{Column, Host};
use crate::key::{self, Key};
use crate::r2rml::model::*;

fn nonconforming(message: impl Into<String>) -> EngineError {
    EngineError::mapping(format!("non-conforming R2RML mapping: {}", message.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Child,
    Parent,
}

/// A column of a unit's row: its side and its index among its table's columns
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColRef {
    pub side: Side,
    pub index: usize,
}

#[derive(Debug, Clone)]
pub enum CPart {
    Text(String),
    Column(ColRef),
}

#[derive(Debug, Clone)]
pub enum CValue {
    Constant(Term, Key),
    Column(ColRef),
    Template(Vec<CPart>),
}

#[derive(Debug, Clone)]
pub struct CMap {
    pub value: CValue,
    pub term_type: TermType,
    pub language: Option<String>,
    pub datatype: Option<NamedNode>,
}

impl CMap {
    pub fn columns(&self) -> Vec<ColRef> {
        match &self.value {
            CValue::Constant(..) => Vec::new(),
            CValue::Column(c) => vec![*c],
            CValue::Template(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    CPart::Column(c) => Some(*c),
                    CPart::Text(_) => None,
                })
                .collect(),
        }
    }

    pub fn constant_key(&self) -> Option<&Key> {
        match &self.value {
            CValue::Constant(_, k) => Some(k),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum CGraph {
    Default,
    Map(CMap),
}

#[derive(Debug, Clone)]
pub struct CTable {
    pub sql: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone)]
pub struct Unit {
    pub child: usize,
    /// The parent's table and the join's columns, child's and parent's
    pub parent: Option<(usize, Vec<(usize, usize)>)>,
    pub subject: CMap,
    pub predicate: CMap,
    pub object: CMap,
    /// None: the default graph only
    pub graphs: Vec<CGraph>,
}

#[derive(Debug)]
pub struct Compiled {
    pub name: String,
    pub revision: i64,
    pub base: Option<Iri<String>>,
    /// What the labels of the mapping's blank nodes begin with: "r", a hash
    /// of the mapping's name in 16 hexadecimal digits, "x" (eval.rs,
    /// blank_node())
    pub bnode_prefix: String,
    pub tables: Vec<CTable>,
    pub units: Vec<Unit>,
    /// The units by their constant predicate's key
    pub by_predicate: HashMap<Key, Vec<usize>>,
    /// The units whose predicate is no constant
    pub other_predicates: Vec<usize>,
}

/// A column a mapping names, as PostgreSQL resolves the identifier: a
/// delimited one ("Name") as it is, any other folded to lower case.
fn resolve_column(name: &str, columns: &[Column], table: &str) -> EResult<usize> {
    let wanted = if name.len() >= 2 && name.starts_with('"') && name.ends_with('"') {
        name[1..name.len() - 1].replace("\"\"", "\"")
    } else {
        name.to_ascii_lowercase()
    };
    let found: Vec<usize> = columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name == wanted)
        .map(|(i, _)| i)
        .collect();
    match found.as_slice() {
        [one] => Ok(*one),
        [] => Err(nonconforming(format!(
            "the column {name} is not one of {table}'s: {}",
            columns
                .iter()
                .map(|c| format!("\"{}\"", c.name))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        _ => Err(nonconforming(format!(
            "the column {name} is more than one of {table}'s"
        ))),
    }
}

struct Compiler<'a> {
    tables: &'a [CTable],
    names: &'a [String],
}

impl Compiler<'_> {
    fn map(&self, map: &TermMap, side: Side, table: usize) -> EResult<CMap> {
        let col = |name: &str| -> EResult<ColRef> {
            Ok(ColRef {
                side,
                index: resolve_column(name, &self.tables[table].columns, &self.names[table])?,
            })
        };
        let value = match &map.value {
            TermMapValue::Constant(t) => CValue::Constant(t.clone(), key::term_key(t.as_ref()).0),
            TermMapValue::Column(c) => CValue::Column(col(c)?),
            TermMapValue::Template(t) => CValue::Template(
                t.parts
                    .iter()
                    .map(|p| {
                        Ok(match p {
                            TemplatePart::Text(s) => CPart::Text(s.clone()),
                            TemplatePart::Column(c) => CPart::Column(col(c)?),
                        })
                    })
                    .collect::<EResult<Vec<_>>>()?,
            ),
        };
        Ok(CMap {
            value,
            term_type: map.term_type,
            language: map.language.clone(),
            datatype: map.datatype.clone(),
        })
    }

    fn graphs(&self, graphs: &[GraphMap], table: usize) -> EResult<Vec<CGraph>> {
        graphs
            .iter()
            .map(|g| {
                Ok(match g {
                    GraphMap::Default => CGraph::Default,
                    GraphMap::Map(m) => CGraph::Map(self.map(m, Side::Child, table)?),
                })
            })
            .collect()
    }
}

fn constant(term: Term) -> CMap {
    let key = key::term_key(term.as_ref()).0;
    let term_type = match &term {
        Term::Literal(_) => TermType::Literal,
        Term::BlankNode(_) => TermType::BlankNode,
        Term::NamedNode(_) => TermType::Iri,
    };
    CMap {
        value: CValue::Constant(term, key),
        term_type,
        language: None,
        datatype: None,
    }
}

/// The mapping's logical tables described through the host, its columns
/// resolved, its units made.
pub fn compile(
    host: &Host,
    name: &str,
    revision: i64,
    mapping: &Mapping,
    base: Option<&str>,
) -> EResult<Compiled> {
    let base = match base {
        Some(b) => Some(Iri::parse(b.to_owned()).map_err(|e| {
            nonconforming(format!("the base IRI <{b}> is not an IRI: {e}"))
        })?),
        None => None,
    };
    // each triples map's table, described once per distinct SQL
    let mut tables: Vec<CTable> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut table_of: Vec<usize> = Vec::new();
    let mut by_sql: HashMap<String, usize> = HashMap::new();
    for tm in &mapping.triples_maps {
        let sql = tm.table.effective_sql();
        let index = match by_sql.get(&sql) {
            Some(&i) => i,
            None => {
                let columns = host.sql_describe(&sql)?;
                let mut seen = std::collections::HashSet::new();
                for c in &columns {
                    if !seen.insert(c.name.as_str()) {
                        return Err(nonconforming(format!(
                            "the logical table of {} has two columns named \"{}\"",
                            tm.id, c.name
                        )));
                    }
                }
                tables.push(CTable {
                    sql: sql.clone(),
                    columns,
                });
                names.push(match &tm.table {
                    LogicalTable::Table(t) => t.clone(),
                    LogicalTable::Query(_) => format!("the rr:sqlQuery of {}", tm.id),
                });
                by_sql.insert(sql, tables.len() - 1);
                tables.len() - 1
            }
        };
        table_of.push(index);
    }

    let compiler = Compiler {
        tables: &tables,
        names: &names,
    };
    let mut units = Vec::new();
    for (i, tm) in mapping.triples_maps.iter().enumerate() {
        let child = table_of[i];
        let subject = compiler.map(&tm.subject, Side::Child, child)?;
        let subject_graphs = compiler.graphs(&tm.graphs, child)?;
        for class in &tm.classes {
            units.push(Unit {
                child,
                parent: None,
                subject: subject.clone(),
                predicate: constant(Term::from(rdf::TYPE.clone())),
                object: constant(class.clone().into()),
                graphs: subject_graphs.clone(),
            });
        }
        for pom in &tm.predicate_object_maps {
            let mut graphs = subject_graphs.clone();
            graphs.extend(compiler.graphs(&pom.graphs, child)?);
            for predicate in &pom.predicates {
                let predicate = compiler.map(predicate, Side::Child, child)?;
                for object in &pom.objects {
                    let (object, parent) = match object {
                        ObjectMap::Term(m) => (compiler.map(m, Side::Child, child)?, None),
                        ObjectMap::Ref { parent, joins } => {
                            let parent_table = table_of[*parent];
                            let parent_subject = &mapping.triples_maps[*parent].subject;
                            if joins.is_empty() {
                                // the same rows: the parent's subject over the child's row
                                (compiler.map(parent_subject, Side::Child, child)?, None)
                            } else {
                                let joins = joins
                                    .iter()
                                    .map(|(c, p)| {
                                        Ok((
                                            resolve_column(c, &tables[child].columns, &names[child])?,
                                            resolve_column(
                                                p,
                                                &tables[parent_table].columns,
                                                &names[parent_table],
                                            )?,
                                        ))
                                    })
                                    .collect::<EResult<Vec<_>>>()?;
                                (
                                    compiler.map(parent_subject, Side::Parent, parent_table)?,
                                    Some((parent_table, joins)),
                                )
                            }
                        }
                    };
                    units.push(Unit {
                        child,
                        parent,
                        subject: subject.clone(),
                        predicate: predicate.clone(),
                        object,
                        graphs: graphs.clone(),
                    });
                }
            }
        }
    }

    let mut by_predicate: HashMap<Key, Vec<usize>> = HashMap::new();
    let mut other_predicates = Vec::new();
    for (i, unit) in units.iter().enumerate() {
        match unit.predicate.constant_key() {
            Some(k) => by_predicate.entry(*k).or_default().push(i),
            None => other_predicates.push(i),
        }
    }
    let mut h = SipHasher24::new();
    h.write(name.as_bytes());
    Ok(Compiled {
        name: name.to_owned(),
        revision,
        base,
        bnode_prefix: format!("r{:016x}x", h.finish()),
        tables,
        units,
        by_predicate,
        other_predicates,
    })
}
