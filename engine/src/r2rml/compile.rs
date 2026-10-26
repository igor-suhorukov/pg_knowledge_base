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
use oxrdf::{BlankNode, Literal, NamedNode, Term};

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

    let mut h = SipHasher24::new();
    h.write(name.as_bytes());
    Ok(Compiled::new(
        name.to_owned(),
        revision,
        base,
        format!("r{:016x}x", h.finish()),
        tables,
        units,
    ))
}

impl Compiled {
    fn new(
        name: String,
        revision: i64,
        base: Option<Iri<String>>,
        bnode_prefix: String,
        tables: Vec<CTable>,
        units: Vec<Unit>,
    ) -> Self {
        let mut by_predicate: HashMap<Key, Vec<usize>> = HashMap::new();
        let mut other_predicates = Vec::new();
        for (i, unit) in units.iter().enumerate() {
            match unit.predicate.constant_key() {
                Some(k) => by_predicate.entry(*k).or_default().push(i),
                None => other_predicates.push(i),
            }
        }
        Compiled {
            name,
            revision,
            base,
            bnode_prefix,
            tables,
            units,
            by_predicate,
            other_predicates,
        }
    }

    /// The compiled mapping as bytes, which decode() reads back: what a cache
    /// of compiled mappings keeps between sessions (mod.rs)
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::with_capacity(4096));
        w.0.extend_from_slice(CACHE_MAGIC);
        w.str(&self.name);
        w.0.extend_from_slice(&self.revision.to_le_bytes());
        w.opt_str(self.base.as_ref().map(Iri::as_str));
        w.str(&self.bnode_prefix);
        w.len(self.tables.len());
        for t in &self.tables {
            w.str(&t.sql);
            w.len(t.columns.len());
            for c in &t.columns {
                w.str(&c.name);
                w.0.extend_from_slice(&c.type_oid.to_le_bytes());
                w.0.extend_from_slice(&c.sql_class.to_le_bytes());
                w.str(&c.type_name);
            }
        }
        w.len(self.units.len());
        for u in &self.units {
            w.len(u.child);
            match &u.parent {
                None => w.0.push(0),
                Some((table, joins)) => {
                    w.0.push(1);
                    w.len(*table);
                    w.len(joins.len());
                    for (c, p) in joins {
                        w.len(*c);
                        w.len(*p);
                    }
                }
            }
            w.map(&u.subject);
            w.map(&u.predicate);
            w.map(&u.object);
            w.len(u.graphs.len());
            for g in &u.graphs {
                match g {
                    CGraph::Default => w.0.push(0),
                    CGraph::Map(m) => {
                        w.0.push(1);
                        w.map(m);
                    }
                }
            }
        }
        w.0
    }

    /// A compiled mapping from encode()'s bytes; None for others
    pub fn decode(bytes: &[u8]) -> Option<Compiled> {
        let mut r = Reader { b: bytes, at: 0 };
        if r.take(CACHE_MAGIC.len())? != CACHE_MAGIC {
            return None;
        }
        let name = r.string()?;
        let revision = i64::from_le_bytes(r.take(8)?.try_into().ok()?);
        let base = match r.opt_string()? {
            Some(b) => Some(Iri::parse(b).ok()?),
            None => None,
        };
        let bnode_prefix = r.string()?;
        let mut tables = Vec::new();
        for _ in 0..r.len()? {
            let sql = r.string()?;
            let mut columns = Vec::new();
            for _ in 0..r.len()? {
                columns.push(Column {
                    name: r.string()?,
                    type_oid: u32::from_le_bytes(r.take(4)?.try_into().ok()?),
                    sql_class: i32::from_le_bytes(r.take(4)?.try_into().ok()?),
                    type_name: r.string()?,
                });
            }
            tables.push(CTable { sql, columns });
        }
        let mut units = Vec::new();
        for _ in 0..r.len()? {
            let child = r.len()?;
            let parent = match r.byte()? {
                0 => None,
                _ => {
                    let table = r.len()?;
                    let mut joins = Vec::new();
                    for _ in 0..r.len()? {
                        joins.push((r.len()?, r.len()?));
                    }
                    Some((table, joins))
                }
            };
            let subject = r.map()?;
            let predicate = r.map()?;
            let object = r.map()?;
            let mut graphs = Vec::new();
            for _ in 0..r.len()? {
                graphs.push(match r.byte()? {
                    0 => CGraph::Default,
                    _ => CGraph::Map(r.map()?),
                });
            }
            units.push(Unit {
                child,
                parent,
                subject,
                predicate,
                object,
                graphs,
            });
        }
        (r.at == bytes.len()).then(|| Compiled::new(name, revision, base, bnode_prefix, tables, units))
    }
}

/// What a cached compiled mapping begins with, its format's version last
const CACHE_MAGIC: &[u8] = b"kb-r2rml-compiled-1";

struct Writer(Vec<u8>);

impl Writer {
    fn len(&mut self, n: usize) {
        self.0.extend_from_slice(&(n as u32).to_le_bytes());
    }

    fn str(&mut self, s: &str) {
        self.len(s.len());
        self.0.extend_from_slice(s.as_bytes());
    }

    fn opt_str(&mut self, s: Option<&str>) {
        match s {
            None => self.0.push(0),
            Some(s) => {
                self.0.push(1);
                self.str(s);
            }
        }
    }

    fn col(&mut self, c: ColRef) {
        self.0.push(match c.side {
            Side::Child => 0,
            Side::Parent => 1,
        });
        self.len(c.index);
    }

    fn term(&mut self, t: &Term) {
        match t {
            Term::NamedNode(n) => {
                self.0.push(0);
                self.str(n.as_str());
            }
            Term::BlankNode(b) => {
                self.0.push(1);
                self.str(b.as_str());
            }
            Term::Literal(l) => match l.language() {
                Some(language) => {
                    self.0.push(2);
                    self.str(l.value());
                    self.str(language);
                }
                None => {
                    self.0.push(3);
                    self.str(l.value());
                    self.str(l.datatype().as_str());
                }
            },
        }
    }

    fn map(&mut self, m: &CMap) {
        match &m.value {
            CValue::Constant(t, k) => {
                self.0.push(0);
                self.term(t);
                self.0.extend_from_slice(k);
            }
            CValue::Column(c) => {
                self.0.push(1);
                self.col(*c);
            }
            CValue::Template(parts) => {
                self.0.push(2);
                self.len(parts.len());
                for p in parts {
                    match p {
                        CPart::Text(s) => {
                            self.0.push(0);
                            self.str(s);
                        }
                        CPart::Column(c) => {
                            self.0.push(1);
                            self.col(*c);
                        }
                    }
                }
            }
        }
        self.0.push(match m.term_type {
            TermType::Iri => 0,
            TermType::BlankNode => 1,
            TermType::Literal => 2,
        });
        self.opt_str(m.language.as_deref());
        self.opt_str(m.datatype.as_ref().map(NamedNode::as_str));
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let bytes = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(bytes)
    }

    fn byte(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn len(&mut self) -> Option<usize> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize)
    }

    fn string(&mut self) -> Option<String> {
        let n = self.len()?;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }

    fn opt_string(&mut self) -> Option<Option<String>> {
        Some(match self.byte()? {
            0 => None,
            _ => Some(self.string()?),
        })
    }

    fn col(&mut self) -> Option<ColRef> {
        let side = match self.byte()? {
            0 => Side::Child,
            _ => Side::Parent,
        };
        Some(ColRef {
            side,
            index: self.len()?,
        })
    }

    fn term(&mut self) -> Option<Term> {
        Some(match self.byte()? {
            0 => NamedNode::new_unchecked(self.string()?).into(),
            1 => BlankNode::new_unchecked(self.string()?).into(),
            2 => {
                let value = self.string()?;
                Literal::new_language_tagged_literal_unchecked(value, self.string()?).into()
            }
            _ => {
                let value = self.string()?;
                Literal::new_typed_literal(value, NamedNode::new_unchecked(self.string()?)).into()
            }
        })
    }

    fn map(&mut self) -> Option<CMap> {
        let value = match self.byte()? {
            0 => {
                let t = self.term()?;
                let k: Key = self.take(16)?.try_into().ok()?;
                CValue::Constant(t, k)
            }
            1 => CValue::Column(self.col()?),
            _ => {
                let mut parts = Vec::new();
                for _ in 0..self.len()? {
                    parts.push(match self.byte()? {
                        0 => CPart::Text(self.string()?),
                        _ => CPart::Column(self.col()?),
                    });
                }
                CValue::Template(parts)
            }
        };
        let term_type = match self.byte()? {
            0 => TermType::Iri,
            1 => TermType::BlankNode,
            _ => TermType::Literal,
        };
        let language = self.opt_string()?;
        let datatype = self.opt_string()?.map(NamedNode::new_unchecked);
        Some(CMap {
            value,
            term_type,
            language,
            datatype,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(side: Side, index: usize) -> ColRef {
        ColRef { side, index }
    }

    #[test]
    fn compiled_mappings_encode_and_decode() {
        let column = |name: &str, type_oid: u32| Column {
            name: name.to_owned(),
            type_oid,
            sql_class: 1,
            type_name: "text".to_owned(),
        };
        let tables = vec![
            CTable {
                sql: "SELECT * FROM t".to_owned(),
                columns: vec![column("iri", 25), column("name", 25), column("age", 23)],
            },
            CTable {
                sql: "SELECT iri, value FROM t__values WHERE property = 'p'".to_owned(),
                columns: vec![column("iri", 25), column("value", 25)],
            },
        ];
        let iri = CMap {
            value: CValue::Column(col(Side::Child, 0)),
            term_type: TermType::Iri,
            language: None,
            datatype: None,
        };
        let units = vec![
            Unit {
                child: 0,
                parent: None,
                subject: iri.clone(),
                predicate: constant(NamedNode::new_unchecked("http://example.com/name").into()),
                object: CMap {
                    value: CValue::Column(col(Side::Child, 1)),
                    term_type: TermType::Literal,
                    language: Some("ru".to_owned()),
                    datatype: None,
                },
                graphs: vec![
                    CGraph::Default,
                    CGraph::Map(constant(NamedNode::new_unchecked("http://example.com/g").into())),
                ],
            },
            Unit {
                child: 0,
                parent: Some((1, vec![(0, 0)])),
                subject: CMap {
                    value: CValue::Template(vec![
                        CPart::Text("card ".to_owned()),
                        CPart::Column(col(Side::Child, 0)),
                    ]),
                    term_type: TermType::BlankNode,
                    language: None,
                    datatype: None,
                },
                predicate: CMap {
                    value: CValue::Column(col(Side::Parent, 1)),
                    term_type: TermType::Iri,
                    language: None,
                    datatype: None,
                },
                object: CMap {
                    value: CValue::Column(col(Side::Child, 2)),
                    term_type: TermType::Literal,
                    language: None,
                    datatype: Some(NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#int")),
                },
                graphs: Vec::new(),
            },
            Unit {
                child: 1,
                parent: None,
                subject: iri,
                predicate: constant(NamedNode::new_unchecked("http://example.com/p").into()),
                object: constant(Literal::new_typed_literal("7", NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer")).into()),
                graphs: Vec::new(),
            },
        ];
        let compiled = Compiled::new(
            "m".to_owned(),
            42,
            Some(Iri::parse("http://example.com/base/".to_owned()).unwrap()),
            "r0123456789abcdefx".to_owned(),
            tables,
            units,
        );
        let bytes = compiled.encode();
        let back = Compiled::decode(&bytes).expect("decoded");
        assert_eq!(format!("{:?}", back.units), format!("{:?}", compiled.units));
        assert_eq!(format!("{:?}", back.tables), format!("{:?}", compiled.tables));
        assert_eq!(back.name, "m");
        assert_eq!(back.revision, 42);
        assert_eq!(back.base.as_ref().map(Iri::as_str), Some("http://example.com/base/"));
        assert_eq!(back.bnode_prefix, compiled.bnode_prefix);
        assert_eq!(back.other_predicates, vec![1]);
        assert_eq!(back.by_predicate.len(), 2);
        // a truncated or a foreign file is none
        assert!(Compiled::decode(&bytes[..bytes.len() - 1]).is_none());
        assert!(Compiled::decode(b"something else").is_none());
    }
}
