// SPDX-License-Identifier: Apache-2.0
//! R2RML's virtual quads for a pattern (docs/design.md §4.3).
//!
//! For each unit of each mapping that can make a quad of the pattern -- its
//! constant predicate the pattern's, or a predicate it makes from a column
//! -- one SQL query over its logical table, or over its table and its
//! parent's joined: the columns its terms read, rows without NULLs in them,
//! and the conditions the pattern's bound terms give.  A bound IRI is
//! matched against a template and gives its columns' values ("http://ex/emp/
//! {id}" and <http://ex/emp/7> give id = '7'), a bound literal its column's
//! value, cast to the column's type so that the table's indexes serve, a
//! bound blank node of the mapping the label it was made from (blank_node()),
//! as a literal's value or a template's captures.  The
//! conditions only narrow: each row's terms are made as R2RML makes them
//! (§11) and kept only when their keys are the pattern's, so a condition
//! that cannot be derived -- a template whose separators a value may hold --
//! is left out, never guessed.

use std::iter::{empty, once};
use std::rc::Rc;
use std::str::FromStr;

use oxrdf::{BlankNode, Literal, NamedNode, Term};
use oxsdatatypes::{Date, DateTime, Decimal, Double, Time};
use percent_encoding::percent_decode_str;
use rustc_hash::FxHashSet;
use spareval::InternalQuad;

use crate::error::{EResult, EngineError};
use crate::ffi::*;
use crate::host::Host;
use crate::key;
use crate::r2rml::compile::*;
use crate::r2rml::model::TermType;
use crate::r2rml::QuadIter;
use crate::terms::{self, Tk};

/// The XML Schema datatype of a natural RDF literal of a SQL class (R2RML
/// §10.2); None: a plain literal.
fn natural_datatype(class: i32) -> Option<&'static str> {
    Some(match class {
        KB_SQL_INTEGER => "http://www.w3.org/2001/XMLSchema#integer",
        KB_SQL_DECIMAL => "http://www.w3.org/2001/XMLSchema#decimal",
        KB_SQL_FLOAT => "http://www.w3.org/2001/XMLSchema#double",
        KB_SQL_BOOLEAN => "http://www.w3.org/2001/XMLSchema#boolean",
        KB_SQL_DATE => "http://www.w3.org/2001/XMLSchema#date",
        KB_SQL_TIME => "http://www.w3.org/2001/XMLSchema#time",
        KB_SQL_TIMESTAMP => "http://www.w3.org/2001/XMLSchema#dateTime",
        KB_SQL_BINARY => "http://www.w3.org/2001/XMLSchema#hexBinary",
        _ => return None,
    })
}

/// A value's natural lexical form, canonical where XML Schema says
/// (R2RML §10.2): the form a template writes.
fn natural_lexical(value: &str, class: i32) -> String {
    match class {
        KB_SQL_DECIMAL => Decimal::from_str(value).map_or_else(|_| value.to_owned(), |d| d.to_string()),
        KB_SQL_FLOAT => Double::from_str(value).map_or_else(|_| value.to_owned(), |d| d.to_string()),
        _ => value.to_owned(),
    }
}

/// RFC 3987's iunreserved: what an IRI-safe string keeps (R2RML §7.3)
fn iunreserved(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '-' | '.' | '_' | '~')
        || matches!(c as u32,
            0xA0..=0xD7FF | 0xF900..=0xFDCF | 0xFDF0..=0xFFEF
            | 0x10000..=0x1FFFD | 0x20000..=0x2FFFD | 0x30000..=0x3FFFD
            | 0x40000..=0x4FFFD | 0x50000..=0x5FFFD | 0x60000..=0x6FFFD
            | 0x70000..=0x7FFFD | 0x80000..=0x8FFFD | 0x90000..=0x9FFFD
            | 0xA0000..=0xAFFFD | 0xB0000..=0xBFFFD | 0xC0000..=0xCFFFD
            | 0xD0000..=0xDFFFD | 0xE1000..=0xEFFFD)
}

fn iri_safe(value: &str, out: &mut String) {
    for c in value.chars() {
        if iunreserved(c) {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
}

/// A mapping's blank node's own label: its prefix, then the label made from a
/// row -- ASCII letters and digits as they are, any other byte "-" and two
/// hexadecimal digits -- a label N-Triples takes, that blank_id_label() reads
/// back.
fn blank_id(prefix: &str, label: &str) -> String {
    let mut id = String::with_capacity(prefix.len() + label.len() * 2);
    id.push_str(prefix);
    for b in label.bytes() {
        if b.is_ascii_alphanumeric() {
            id.push(char::from(b));
        } else {
            id.push_str(&format!("-{b:02x}"));
        }
    }
    id
}

pub(crate) fn blank_id_label(prefix: &str, id: &str) -> Option<String> {
    let rest = id.strip_prefix(prefix)?;
    let mut bytes = Vec::with_capacity(rest.len());
    let mut it = rest.bytes();
    while let Some(b) = it.next() {
        if b == b'-' {
            let hex = [it.next()?, it.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else if b.is_ascii_alphanumeric() {
            bytes.push(b);
        } else {
            return None;
        }
    }
    String::from_utf8(bytes).ok()
}

pub(crate) fn data_error(message: impl Into<String>) -> EngineError {
    EngineError::evaluation(format!("R2RML data error: {}", message.into()))
}

/// The row a unit reads: values by their ColRef's place in the select list
pub struct Row<'a> {
    pub values: &'a [Option<String>],
    pub place: &'a [(ColRef, usize)],
}

impl Row<'_> {
    fn get(&self, col: ColRef) -> Option<&str> {
        let i = self.place.iter().find(|(c, _)| *c == col)?.1;
        self.values.get(i)?.as_deref()
    }
}

pub struct Context<'a> {
    pub compiled: &'a Compiled,
    pub unit: &'a Unit,
}

impl Context<'_> {
    pub(crate) fn class(&self, col: ColRef) -> i32 {
        let table = match col.side {
            Side::Child => self.unit.child,
            Side::Parent => self.unit.parent.as_ref().map_or(self.unit.child, |p| p.0),
        };
        self.compiled.tables[table].columns[col.index].sql_class
    }

    pub(crate) fn column(&self, col: ColRef) -> &crate::host::Column {
        let table = match col.side {
            Side::Child => self.unit.child,
            Side::Parent => self.unit.parent.as_ref().map_or(self.unit.child, |p| p.0),
        };
        &self.compiled.tables[table].columns[col.index]
    }

    fn resolve(&self, iri: String) -> EResult<NamedNode> {
        if let Ok(n) = NamedNode::new(iri.clone()) {
            return Ok(n);
        }
        if let (Some(base), Ok(relative)) = (&self.compiled.base, oxiri::IriRef::parse(iri.as_str())) {
            if let Ok(resolved) = base.resolve(&relative) {
                if let Ok(n) = NamedNode::new(resolved.into_inner()) {
                    return Ok(n);
                }
            }
        }
        Err(data_error(format!("\"{iri}\" is not an IRI")))
    }

    /// The blank node of a label: the same label in one mapping is one blank
    /// node.  Its own label is the mapping's prefix and the label -- ASCII
    /// letters and digits as they are, any other byte as "-" and two
    /// hexadecimal digits -- so that a bound blank node gives back its
    /// columns' values (bind()).
    pub fn blank_node(&self, label: &str) -> BlankNode {
        BlankNode::new_unchecked(blank_id(&self.compiled.bnode_prefix, label))
    }

    /// The label a blank node of this mapping was made from; None for any
    /// other blank node, which the mapping cannot make.
    fn blank_label(&self, node: &BlankNode) -> Option<String> {
        blank_id_label(&self.compiled.bnode_prefix, node.as_str())
    }

    /// A term map's term over a row (R2RML §11); None where a column it
    /// reads is NULL.
    pub fn generate(&self, map: &CMap, row: &Row<'_>) -> EResult<Option<Term>> {
        let text = match &map.value {
            CValue::Constant(t, _) => return Ok(Some(t.clone())),
            CValue::Column(c) => {
                let Some(v) = row.get(*c) else {
                    return Ok(None);
                };
                if map.term_type == TermType::Literal {
                    return Ok(Some(self.literal(map, v, Some(self.class(*c)))));
                }
                if map.term_type == TermType::Iri {
                    return Ok(Some(self.resolve(v.to_owned())?.into()));
                }
                natural_lexical(v, self.class(*c))
            }
            CValue::Template(parts) => {
                let mut s = String::new();
                for part in parts {
                    match part {
                        CPart::Text(t) => s.push_str(t),
                        CPart::Column(c) => {
                            let Some(v) = row.get(*c) else {
                                return Ok(None);
                            };
                            let v = natural_lexical(v, self.class(*c));
                            if map.term_type == TermType::Iri {
                                iri_safe(&v, &mut s);
                            } else {
                                s.push_str(&v);
                            }
                        }
                    }
                }
                if map.term_type == TermType::Literal {
                    return Ok(Some(self.literal(map, &s, None)));
                }
                if map.term_type == TermType::Iri {
                    return Ok(Some(self.resolve(s)?.into()));
                }
                s
            }
        };
        Ok(Some(self.blank_node(&text).into()))
    }

    fn literal(&self, map: &CMap, value: &str, class: Option<i32>) -> Term {
        if let Some(language) = &map.language {
            return Literal::new_language_tagged_literal_unchecked(value.to_owned(), language.clone()).into();
        }
        if let Some(datatype) = &map.datatype {
            return Literal::new_typed_literal(value.to_owned(), datatype.clone()).into();
        }
        match class.and_then(natural_datatype) {
            Some(dt) => Literal::new_typed_literal(value.to_owned(), NamedNode::new_unchecked(dt)).into(),
            None => Literal::new_simple_literal(value.to_owned()).into(),
        }
    }
}

// ---------------------------------------------------------------------
// The conditions a bound term gives
// ---------------------------------------------------------------------

/// What a bound term asks of a term map
pub(crate) enum Bind {
    /// the map can never make it
    Never,
    /// nothing to ask of the SQL: a constant that is it, or a value no
    /// condition is derived for -- the rows' terms are checked
    Any,
    /// each column one of its values
    Columns(Vec<(ColRef, Vec<String>)>),
}

fn term_type_of(term: &Term) -> TermType {
    match term {
        Term::NamedNode(_) => TermType::Iri,
        Term::BlankNode(_) => TermType::BlankNode,
        Term::Literal(_) => TermType::Literal,
    }
}

/// Captures of a template matched against a string: a column's value, or
/// None where the match cannot tell it; None when the string cannot be one
/// the template makes.
fn match_template(parts: &[CPart], s: &str, encoded: bool) -> Option<Vec<(ColRef, Option<String>)>> {
    let mut captures = Vec::new();
    let mut pos = 0;
    let mut i = 0;
    while i < parts.len() {
        match &parts[i] {
            CPart::Text(t) => {
                if !s[pos..].starts_with(t.as_str()) {
                    return None;
                }
                pos += t.len();
            }
            CPart::Column(c) => match parts.get(i + 1) {
                None => {
                    captures.push((*c, Some(s[pos..].to_owned())));
                    pos = s.len();
                }
                Some(CPart::Text(next)) => {
                    let first = next.chars().next()?;
                    if encoded && !iunreserved(first) && first != '%' {
                        let end = s[pos..].find(first)?;
                        captures.push((*c, Some(s[pos..pos + end].to_owned())));
                        pos += end;
                    } else {
                        // a value may hold the separator: what follows is not
                        // known, and the rows' terms are checked instead
                        captures.push((*c, None));
                        for p in &parts[i + 1..] {
                            if let CPart::Column(c) = p {
                                captures.push((*c, None));
                            }
                        }
                        if let Some(CPart::Text(last)) = parts.last() {
                            if !s[pos..].ends_with(last.as_str()) {
                                return None;
                            }
                        }
                        return Some(captures);
                    }
                }
                Some(CPart::Column(_)) => {
                    for p in &parts[i..] {
                        if let CPart::Column(c) = p {
                            captures.push((*c, None));
                        }
                    }
                    return Some(captures);
                }
            },
        }
        i += 1;
    }
    if pos != s.len() {
        return None;
    }
    if encoded {
        for (_, v) in &mut captures {
            if let Some(value) = v {
                *v = Some(percent_decode_str(value).decode_utf8().ok()?.into_owned());
            }
        }
    }
    Some(captures)
}

fn template_is_absolute(parts: &[CPart]) -> bool {
    match parts.first() {
        Some(CPart::Text(t)) => {
            let scheme_end = t.find(':');
            scheme_end.is_some_and(|i| {
                i > 0
                    && t[..i]
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            })
        }
        _ => false,
    }
}

impl Context<'_> {
    pub(crate) fn bind(&self, map: &CMap, term: &Term) -> Bind {
        if let CValue::Constant(_, k) = &map.value {
            return if *k == key::term_key(term.as_ref()).0 {
                Bind::Any
            } else {
                Bind::Never
            };
        }
        if term_type_of(term) != map.term_type {
            return Bind::Never;
        }
        match (&map.value, term) {
            (CValue::Column(c), Term::NamedNode(n)) => {
                let mut values = vec![n.as_str().to_owned()];
                if let Some(base) = &self.compiled.base {
                    if let Some(rest) = n.as_str().strip_prefix(base.as_str()) {
                        values.push(rest.to_owned());
                    }
                }
                Bind::Columns(vec![(*c, values)])
            }
            (CValue::Column(c), Term::Literal(l)) => {
                let class = self.class(*c);
                let expected_datatype = if let Some(language) = &map.language {
                    if l.language() != Some(language.as_str()) {
                        return Bind::Never;
                    }
                    None
                } else if let Some(dt) = &map.datatype {
                    Some(dt.as_str().to_owned())
                } else {
                    Some(
                        natural_datatype(class)
                            .unwrap_or("http://www.w3.org/2001/XMLSchema#string")
                            .to_owned(),
                    )
                };
                if map.language.is_none() && l.language().is_some() {
                    return Bind::Never;
                }
                if let Some(dt) = expected_datatype {
                    // the key's canonical datatype: xsd:int is xsd:integer
                    let made = key::literal_key(
                        Literal::new_typed_literal(l.value().to_owned(), NamedNode::new_unchecked(dt)).as_ref(),
                    )
                    .0;
                    if made != key::literal_key(l.as_ref()).0 {
                        return Bind::Never;
                    }
                }
                Bind::Columns(vec![(*c, vec![l.value().to_owned()])])
            }
            (CValue::Template(parts), Term::NamedNode(n)) => {
                let iri = n.as_str();
                let mut candidates = Vec::new();
                if template_is_absolute(parts) {
                    candidates.push(iri);
                } else if let Some(base) = &self.compiled.base {
                    if let Some(rest) = iri.strip_prefix(base.as_str()) {
                        candidates.push(rest);
                    }
                    candidates.push(iri);
                } else {
                    candidates.push(iri);
                }
                for candidate in candidates {
                    if let Some(captures) = match_template(parts, candidate, true) {
                        let columns: Vec<(ColRef, Vec<String>)> = captures
                            .into_iter()
                            .filter_map(|(c, v)| v.map(|v| (c, vec![v])))
                            .collect();
                        return if columns.is_empty() {
                            Bind::Any
                        } else {
                            Bind::Columns(columns)
                        };
                    }
                }
                Bind::Never
            }
            (CValue::Template(parts), Term::Literal(l)) => {
                if l.language() != map.language.as_deref() {
                    return Bind::Never;
                }
                match match_template(parts, l.value(), false) {
                    None => Bind::Never,
                    Some(captures) => {
                        let columns: Vec<(ColRef, Vec<String>)> = captures
                            .into_iter()
                            .filter_map(|(c, v)| v.map(|v| (c, vec![v])))
                            .collect();
                        if columns.is_empty() {
                            Bind::Any
                        } else {
                            Bind::Columns(columns)
                        }
                    }
                }
            }
            // a blank node of this mapping gives back the label it was made
            // from: a column's value, or a template's captures as a literal's
            // would; another mapping's or the store's is never this one's
            (CValue::Column(c), Term::BlankNode(b)) => match self.blank_label(b) {
                Some(label) => Bind::Columns(vec![(*c, vec![label])]),
                None => Bind::Never,
            },
            (CValue::Template(parts), Term::BlankNode(b)) => {
                let Some(label) = self.blank_label(b) else {
                    return Bind::Never;
                };
                match match_template(parts, &label, false) {
                    None => Bind::Never,
                    Some(captures) => {
                        let columns: Vec<(ColRef, Vec<String>)> = captures
                            .into_iter()
                            .filter_map(|(c, v)| v.map(|v| (c, vec![v])))
                            .collect();
                        if columns.is_empty() {
                            Bind::Any
                        } else {
                            Bind::Columns(columns)
                        }
                    }
                }
            }
            _ => Bind::Any,
        }
    }

    /// A value as the column's type takes it in SQL, or None when it cannot
    /// be one of the column's values.
    pub(crate) fn sql_value(&self, col: ColRef, value: &str) -> Option<(String, &'static str)> {
        let column = self.column(col);
        Some(match column.sql_class {
            KB_SQL_INTEGER => {
                let v: i64 = value.trim().parse().ok()?;
                let fits = match column.type_oid {
                    21 => i16::try_from(v).is_ok(),
                    23 => i32::try_from(v).is_ok(),
                    26 => u32::try_from(v).is_ok(),
                    _ => true,
                };
                if !fits {
                    return None;
                }
                (v.to_string(), "typed")
            }
            KB_SQL_DECIMAL => (Decimal::from_str(value).ok()?.to_string(), "typed"),
            KB_SQL_FLOAT => {
                let v = match value {
                    "INF" => "Infinity".to_owned(),
                    "-INF" => "-Infinity".to_owned(),
                    "NaN" => "NaN".to_owned(),
                    v => {
                        let _: f64 = v.parse().ok()?;
                        v.to_owned()
                    }
                };
                (v, "typed")
            }
            KB_SQL_BOOLEAN => match value {
                "true" | "1" => ("true".to_owned(), "typed"),
                "false" | "0" => ("false".to_owned(), "typed"),
                _ => return None,
            },
            KB_SQL_DATE => {
                Date::from_str(value).ok()?;
                (value.trim_end_matches('Z').to_owned(), "typed")
            }
            KB_SQL_TIME => {
                Time::from_str(value).ok()?;
                (value.to_owned(), "typed")
            }
            KB_SQL_TIMESTAMP => {
                DateTime::from_str(value).ok()?;
                (value.to_owned(), "typed")
            }
            KB_SQL_BINARY => {
                if value.len() % 2 != 0 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
                (value.to_owned(), "hex")
            }
            _ => (value.to_owned(), "text"),
        })
    }
}

pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// ---------------------------------------------------------------------
// Columns as text: what a UNION ALL of units' queries gives
// ---------------------------------------------------------------------

/// What the SQL gives of a column as text: its value as the host's lexical()
/// writes it (src/sql.c), or, for floating point, its bits, which
/// float_value() writes as the host would
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Value {
    Text,
    Float4,
    Float8,
}

/// How the SQL gives a column's value: None for a type it cannot give as the
/// host gives it (a date, a time, bytes, a domain, ...)
pub(crate) fn value_kind(ctx: &Context<'_>, col: ColRef) -> Option<Value> {
    Some(match ctx.column(col).type_oid {
        700 => Value::Float4,
        701 => Value::Float8,
        // bool, int8, int2, int4, oid, name, text, varchar, numeric, uuid
        16 | 20 | 21 | 23 | 26 | 19 | 25 | 1043 | 1700 | 2950 => Value::Text,
        _ => return None,
    })
}

pub(crate) fn value_sql(ctx: &Context<'_>, col: ColRef) -> String {
    let lhs = column_sql(ctx, col);
    match ctx.column(col).type_oid {
        16 => format!("CASE WHEN {lhs} THEN 'true' WHEN NOT {lhs} THEN 'false' END"),
        700 => format!("encode(float4send({lhs}), 'hex')"),
        701 => format!("encode(float8send({lhs}), 'hex')"),
        _ => format!("{lhs}::text"),
    }
}

/// A float's bits as the host writes the value (src/sql.c, lexical())
pub(crate) fn float_value(hex: &str, single: bool) -> Option<String> {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    let (nan, inf, text) = if single {
        let f = f32::from_be_bytes(bytes.try_into().ok()?);
        (f.is_nan(), f.is_infinite().then_some(f > 0.), f.to_string())
    } else {
        let f = f64::from_be_bytes(bytes.try_into().ok()?);
        (f.is_nan(), f.is_infinite().then_some(f > 0.), f.to_string())
    };
    Some(match (nan, inf) {
        (true, _) => "NaN".to_owned(),
        (_, Some(true)) => "INF".to_owned(),
        (_, Some(false)) => "-INF".to_owned(),
        _ => text,
    })
}

// ---------------------------------------------------------------------
// A unit's query
// ---------------------------------------------------------------------

/// The SQL of a unit over the bound positions' conditions; None when a
/// condition can never hold.
struct Query {
    sql: String,
    params: Vec<String>,
    place: Vec<(ColRef, usize)>,
}

pub(crate) fn column_sql(ctx: &Context<'_>, col: ColRef) -> String {
    let alias = match col.side {
        Side::Child => "kb_c",
        Side::Parent => "kb_p",
    };
    format!("{alias}.{}", quote_ident(&ctx.column(col).name))
}

/// The conditions of a unit's rows: its maps' columns not NULL, and the
/// bound terms' values, as parameters appended to params; None when a
/// condition can never hold.
pub(crate) fn conditions(
    ctx: &Context<'_>,
    binds: &[&Bind],
    not_null: &[&CMap],
    params: &mut Vec<String>,
) -> Option<Vec<String>> {
    let mut conds: Vec<String> = Vec::new();
    for map in not_null {
        for c in map.columns() {
            let s = format!("{} IS NOT NULL", column_sql(ctx, c));
            if !conds.contains(&s) {
                conds.push(s);
            }
        }
    }
    for bind in binds {
        match bind {
            Bind::Never => return None,
            Bind::Any => (),
            Bind::Columns(columns) => {
                for (col, values) in columns {
                    let mut alternatives = Vec::new();
                    for value in values {
                        let Some((v, how)) = ctx.sql_value(*col, value) else {
                            continue;
                        };
                        params.push(v);
                        let n = params.len();
                        let column = ctx.column(*col);
                        let lhs = column_sql(ctx, *col);
                        alternatives.push(match how {
                            "hex" => format!("{lhs} = decode(${n}, 'hex')"),
                            "typed" => format!("{lhs} = ${n}::{}", column.type_name),
                            _ => match column.type_oid {
                                25 => format!("{lhs} = ${n}"),
                                1043 => format!("{lhs} = ${n}::varchar"),
                                1042 => format!("{lhs} = ${n}::bpchar"),
                                19 => format!("{lhs} = ${n}::name"),
                                _ => format!("{lhs}::text = ${n}"),
                            },
                        });
                    }
                    if alternatives.is_empty() {
                        return None;
                    }
                    conds.push(if alternatives.len() == 1 {
                        alternatives.pop().unwrap_or_default()
                    } else {
                        format!("({})", alternatives.join(" OR "))
                    });
                }
            }
        }
    }
    Some(conds)
}

/// A unit's FROM: its logical table, as kb_c, joined to its parent's, as
/// kb_p, for a referencing object map with join conditions.
pub(crate) fn from_sql(ctx: &Context<'_>) -> String {
    let unit = ctx.unit;
    let tables = &ctx.compiled.tables;
    let mut from = format!("({}) AS kb_c", tables[unit.child].sql);
    if let Some((parent, joins)) = &unit.parent {
        from.push_str(&format!(" JOIN ({}) AS kb_p ON ", tables[*parent].sql));
        let on: Vec<String> = joins
            .iter()
            .map(|(c, p)| {
                format!(
                    "kb_c.{} = kb_p.{}",
                    quote_ident(&tables[unit.child].columns[*c].name),
                    quote_ident(&tables[*parent].columns[*p].name)
                )
            })
            .collect();
        from.push_str(&on.join(" AND "));
    }
    from
}

fn build_query(
    ctx: &Context<'_>,
    binds: &[&Bind],
    needed: &[&CMap],
    not_null: &[&CMap],
    limit: Option<u32>,
    distinct: bool,
) -> Option<Query> {
    let mut select: Vec<ColRef> = Vec::new();
    for map in needed {
        for c in map.columns() {
            if !select.contains(&c) {
                select.push(c);
            }
        }
    }
    let place: Vec<(ColRef, usize)> = select.iter().enumerate().map(|(i, c)| (*c, i)).collect();
    let mut params: Vec<String> = Vec::new();
    let conds = conditions(ctx, binds, not_null, &mut params)?;
    let from = from_sql(ctx);
    let columns: Vec<String> = if select.is_empty() {
        vec!["1".to_owned()]
    } else {
        select.iter().map(|c| column_sql(ctx, *c)).collect()
    };
    let mut sql = format!(
        "SELECT {}{} FROM {from}",
        if distinct { "DISTINCT " } else { "" },
        columns.join(", ")
    );
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    if let Some(n) = limit {
        sql.push_str(&format!(" LIMIT {n}"));
    }
    Some(Query { sql, params, place })
}

/// A pattern's bound term, as a term: the evaluator's own, or the store's
/// through the dictionary.
fn bound_term(host: &Host, tk: Option<&Tk>) -> EResult<Option<Term>> {
    match tk {
        None => Ok(None),
        Some(tk) => Ok(Some(terms::externalize(host, tk)?)),
    }
}

pub(crate) fn tk_of(term: Term) -> Tk {
    Tk::from_term(term)
}

#[derive(Clone, Copy)]
pub(crate) enum GraphChoice {
    Default,
    Map(usize),
}

struct Task {
    compiled: Rc<Compiled>,
    unit: usize,
    choice: GraphChoice,
}

pub struct VirtualSet {
    pub mappings: Vec<Rc<Compiled>>,
}

/// The units of a pattern the plans of whose SQL an estimate asks for, at
/// most; past them the mappings give none, and sparopt falls back to its
/// heuristics -- a variable predicate has thousands of units
const ESTIMATED_UNITS: usize = 256;

/// The rows a scan of a mapping's SQL reads in the time of one lookup of a
/// bound term -- a query planned and run, some 0.7 ms, against some 3 us a
/// row -- which sparopt prices as one row: a predicate's distinct subjects
/// and objects are reported this many times fewer, so that its lookups cost
/// what they do, and a join reads a pattern whole rather than look up more
/// than a few hundredth of its rows
const LOOKUP_ROWS: f64 = 256.0;

impl VirtualSet {
    fn tasks(&self, predicate: Option<&Tk>, graph_name: Option<Option<&Tk>>) -> Vec<Task> {
        self.candidates(predicate.map(|p| &p.key), graph_name.map(|g| g.map(|g| &g.key)))
            .into_iter()
            .map(|(compiled, unit, choice)| Task {
                compiled,
                unit,
                choice,
            })
            .collect()
    }

    /// The quads the mappings may make of a pattern, as the plans of the
    /// units' queries expect them (stats.rs); None past ESTIMATED_UNITS
    pub fn estimate_quads(
        &self,
        host: &Host,
        subject: Option<&Tk>,
        predicate: Option<&Tk>,
        object: Option<&Tk>,
        graph_name: Option<Option<&Tk>>,
    ) -> EResult<Option<f64>> {
        let tasks = self.tasks(predicate, graph_name);
        if tasks.len() > ESTIMATED_UNITS {
            return Ok(None);
        }
        let terms = [
            bound_term(host, subject)?,
            bound_term(host, predicate)?,
            bound_term(host, object)?,
            bound_term(host, graph_name.flatten())?,
        ];
        let mut rows = 0.0;
        for task in &tasks {
            let ctx = task.ctx();
            let binds = task.binds(&terms);
            let bind_refs: Vec<&Bind> = binds.iter().collect();
            let not_null = [&ctx.unit.subject, &ctx.unit.predicate, &ctx.unit.object];
            if let Some(query) = build_query(&ctx, &bind_refs, &task.maps(), &not_null, None, false) {
                rows += host.sql_estimate(&query.sql, &query.params)?;
            }
        }
        Ok(Some(rows))
    }

    /// A predicate's triples the mappings may make, and their distinct
    /// subjects and objects, as the plans of the units' queries expect them;
    /// None past ESTIMATED_UNITS
    pub fn estimate_predicate(
        &self,
        host: &Host,
        predicate: &Tk,
        graph_name: Option<Option<&Tk>>,
    ) -> EResult<Option<(f64, f64, f64)>> {
        let tasks = self.tasks(Some(predicate), graph_name);
        if tasks.len() > ESTIMATED_UNITS {
            return Ok(None);
        }
        let terms = [None, bound_term(host, Some(predicate))?, None, bound_term(host, graph_name.flatten())?];
        let (mut triples, mut subjects, mut objects) = (0.0, 0.0, 0.0);
        for task in &tasks {
            let ctx = task.ctx();
            let binds = task.binds(&terms);
            let bind_refs: Vec<&Bind> = binds.iter().collect();
            let not_null = [&ctx.unit.subject, &ctx.unit.predicate, &ctx.unit.object];
            let Some(all) = build_query(&ctx, &bind_refs, &task.maps(), &not_null, None, false) else {
                continue;
            };
            triples += host.sql_estimate(&all.sql, &all.params)?;
            for (map, sum) in [(&ctx.unit.subject, &mut subjects), (&ctx.unit.object, &mut objects)] {
                if let Some(q) = build_query(&ctx, &bind_refs, &[map], &not_null, None, true) {
                    *sum += host.sql_estimate(&q.sql, &q.params)?;
                }
            }
        }
        Ok(Some((triples, (subjects / LOOKUP_ROWS).max(1.0), (objects / LOOKUP_ROWS).max(1.0))))
    }

    /// The predicates the mappings may make: their constants, once, and one
    /// for each predicate map of another kind
    pub fn predicate_count(&self) -> u64 {
        let mut constants: FxHashSet<&crate::key::Key> = FxHashSet::default();
        let mut others = 0u64;
        for compiled in &self.mappings {
            constants.extend(compiled.by_predicate.keys());
            others += compiled.other_predicates.len() as u64;
        }
        constants.len() as u64 + others
    }

    /// The named graphs the mappings may make: their constants, and the most
    /// that one graph map of a column or a template gives, by its query's plan
    /// -- the graphs of a table's units are the same graphs
    pub fn estimate_graphs(&self, host: &Host) -> EResult<f64> {
        let mut constants: FxHashSet<crate::key::Key> = FxHashSet::default();
        let mut planned: FxHashSet<String> = FxHashSet::default();
        let mut most: f64 = 0.0;
        for compiled in &self.mappings {
            for unit in &compiled.units {
                let ctx = Context { compiled, unit };
                for g in &unit.graphs {
                    let CGraph::Map(m) = g else { continue };
                    if let Some(k) = m.constant_key() {
                        constants.insert(*k);
                        continue;
                    }
                    let Some(query) = build_query(&ctx, &[], &[m], &[m], None, true) else {
                        continue;
                    };
                    if planned.insert(query.sql.clone()) {
                        most = most.max(host.sql_estimate(&query.sql, &query.params)?);
                    }
                }
            }
        }
        Ok(constants.len() as f64 + most)
    }

    /// The units that may make a quad of a pattern, by its predicate's key
    /// and its graph -- Some(None) the default graph, Some(Some(key)) a
    /// named graph, None any named graph -- each with the graph map it
    /// makes the quad in.
    pub(crate) fn candidates(
        &self,
        predicate: Option<&crate::key::Key>,
        graph_name: Option<Option<&crate::key::Key>>,
    ) -> Vec<(Rc<Compiled>, usize, GraphChoice)> {
        let mut out = Vec::new();
        for compiled in &self.mappings {
            let units: Vec<usize> = match predicate {
                Some(p) => compiled
                    .by_predicate
                    .get(p)
                    .into_iter()
                    .flatten()
                    .chain(compiled.other_predicates.iter())
                    .copied()
                    .collect(),
                None => (0..compiled.units.len()).collect(),
            };
            for u in units {
                let unit = &compiled.units[u];
                let choices: Vec<GraphChoice> = match graph_name {
                    Some(None) => {
                        if unit.graphs.is_empty()
                            || unit.graphs.iter().any(|g| matches!(g, CGraph::Default))
                        {
                            vec![GraphChoice::Default]
                        } else {
                            Vec::new()
                        }
                    }
                    Some(Some(g)) => unit
                        .graphs
                        .iter()
                        .enumerate()
                        .filter(|(_, cg)| match cg {
                            CGraph::Map(m) => m.constant_key().is_none_or(|k| k == g),
                            CGraph::Default => false,
                        })
                        .map(|(i, _)| GraphChoice::Map(i))
                        .collect(),
                    None => unit
                        .graphs
                        .iter()
                        .enumerate()
                        .filter(|(_, cg)| matches!(cg, CGraph::Map(_)))
                        .map(|(i, _)| GraphChoice::Map(i))
                        .collect(),
                };
                for choice in choices {
                    out.push((Rc::clone(compiled), u, choice));
                }
            }
        }
        out
    }

    pub fn quads_for_pattern(
        &self,
        host: &Host,
        subject: Option<&Tk>,
        predicate: Option<&Tk>,
        object: Option<&Tk>,
        graph_name: Option<Option<&Tk>>,
    ) -> (QuadIter, usize) {
        // a subject that is a literal, a predicate that is no IRI: nothing
        if subject.is_some_and(|s| !key::is_resource(&s.key))
            || predicate.is_some_and(|p| !key::is_iri(&p.key))
            || graph_name.flatten().is_some_and(|g| !key::is_iri(&g.key))
        {
            return (Box::new(empty()), 0);
        }
        let tasks = self.tasks(predicate, graph_name);
        let n = tasks.len();
        if n == 0 {
            return (Box::new(empty()), 0);
        }
        let host = *host;
        let pattern = [
            subject.cloned(),
            predicate.cloned(),
            object.cloned(),
            graph_name.flatten().cloned(),
        ];
        let terms = match (|| -> EResult<[Option<Term>; 4]> {
            Ok([
                bound_term(&host, pattern[0].as_ref())?,
                bound_term(&host, pattern[1].as_ref())?,
                bound_term(&host, pattern[2].as_ref())?,
                bound_term(&host, pattern[3].as_ref())?,
            ])
        })() {
            Ok(t) => Rc::new(t),
            Err(e) => return (Box::new(once(Err(e))), n),
        };
        // the units whose columns the SQL gives as text go in queries of
        // BATCH_UNITS each, a UNION ALL of theirs; any other unit is a query
        // of its own
        let (batch, single): (Vec<Task>, Vec<Task>) = tasks.into_iter().partition(Task::batchable);
        let (batch, single) = if batch.len() > 1 {
            (batch, single)
        } else {
            (Vec::new(), batch.into_iter().chain(single).collect())
        };
        let (b_pattern, b_terms) = (pattern.clone(), Rc::clone(&terms));
        let mut chunks: Vec<Vec<Task>> = Vec::new();
        let mut batch = batch.into_iter().peekable();
        while batch.peek().is_some() {
            chunks.push(batch.by_ref().take(BATCH_UNITS).collect());
        }
        // each run when first asked, as the units' own queries are
        let batched = chunks.into_iter().flat_map(move |chunk| -> QuadIter {
            match run_batch(&host, chunk, &b_pattern, &b_terms) {
                Ok(it) => it,
                Err(e) => Box::new(once(Err(e))),
            }
        });
        let iter = batched.chain(single.into_iter().flat_map(move |task| -> QuadIter {
            match run_task(&host, &task, &pattern, &terms) {
                Ok(it) => it,
                Err(e) => Box::new(once(Err(e))),
            }
        }));
        (Box::new(iter), n)
    }
}

impl Task {
    fn ctx(&self) -> Context<'_> {
        Context {
            compiled: &self.compiled,
            unit: &self.compiled.units[self.unit],
        }
    }

    fn graph_map(&self) -> Option<&CMap> {
        match self.choice {
            GraphChoice::Map(i) => match &self.compiled.units[self.unit].graphs[i] {
                CGraph::Map(m) => Some(m),
                CGraph::Default => None,
            },
            GraphChoice::Default => None,
        }
    }

    /// The maps of its subject, predicate, object and graph
    fn maps(&self) -> Vec<&CMap> {
        let unit = &self.compiled.units[self.unit];
        let mut maps = vec![&unit.subject, &unit.predicate, &unit.object];
        maps.extend(self.graph_map());
        maps
    }

    /// What the pattern's bound terms ask of its maps
    fn binds(&self, terms: &[Option<Term>; 4]) -> Vec<Bind> {
        let ctx = self.ctx();
        let maps = [
            Some(&ctx.unit.subject),
            Some(&ctx.unit.predicate),
            Some(&ctx.unit.object),
            self.graph_map(),
        ];
        maps.iter()
            .zip(terms.iter())
            .filter_map(|(m, t)| Some(ctx.bind((*m)?, t.as_ref()?)))
            .collect()
    }

    /// Whether the SQL gives every column it reads as text (value_kind())
    fn batchable(&self) -> bool {
        let ctx = self.ctx();
        self.maps()
            .iter()
            .all(|m| m.columns().iter().all(|c| value_kind(&ctx, *c).is_some()))
    }

    /// The columns its maps read, each once, in their order
    fn columns(&self) -> Vec<ColRef> {
        let mut select: Vec<ColRef> = Vec::new();
        for map in self.maps() {
            for c in map.columns() {
                if !select.contains(&c) {
                    select.push(c);
                }
            }
        }
        select
    }
}

/// The quad a row of a unit makes, if it is one of the pattern's: None when
/// a column it reads is NULL or a bound position's key is not the pattern's
fn make_quad(
    ctx: &Context<'_>,
    graph_map: Option<&CMap>,
    r: &Row<'_>,
    pattern: &[Option<Tk>; 4],
) -> EResult<Option<InternalQuad<Tk>>> {
    let unit = ctx.unit;
    let Some(s) = ctx.generate(&unit.subject, r)? else {
        return Ok(None);
    };
    let Some(p) = ctx.generate(&unit.predicate, r)? else {
        return Ok(None);
    };
    let Some(o) = ctx.generate(&unit.object, r)? else {
        return Ok(None);
    };
    if matches!(s, Term::Literal(_)) {
        return Err(data_error(format!("the subject {s} is a literal")));
    }
    if !matches!(p, Term::NamedNode(_)) {
        return Err(data_error(format!("the predicate {p} is not an IRI")));
    }
    let g = match graph_map {
        Some(m) => match ctx.generate(m, r)? {
            Some(g @ Term::NamedNode(_)) => Some(g),
            Some(other) => {
                return Err(data_error(format!("the graph {other} is not an IRI")));
            }
            None => return Ok(None),
        },
        None => None,
    };
    let quad = InternalQuad {
        subject: tk_of(s),
        predicate: tk_of(p),
        object: tk_of(o),
        graph_name: g.map(tk_of),
    };
    // the pattern's bound positions, by key
    let keys = [
        Some(&quad.subject.key),
        Some(&quad.predicate.key),
        Some(&quad.object.key),
        quad.graph_name.as_ref().map(|g| &g.key),
    ];
    for (bound, made) in pattern.iter().zip(keys) {
        if let Some(b) = bound {
            if Some(&b.key) != made {
                return Ok(None);
            }
        }
    }
    Ok(Some(quad))
}

/// One unit's query, run, its rows made into quads and checked against the
/// pattern.
fn run_task(
    host: &Host,
    task: &Task,
    pattern: &[Option<Tk>; 4],
    terms: &Rc<[Option<Term>; 4]>,
) -> EResult<QuadIter> {
    let ctx = task.ctx();
    let unit = ctx.unit;
    let binds = task.binds(terms);
    let needed = task.maps();
    let not_null = [&unit.subject, &unit.predicate, &unit.object];
    let bind_refs: Vec<&Bind> = binds.iter().collect();
    let Some(query) = build_query(&ctx, &bind_refs, &needed, &not_null, None, false) else {
        return Ok(Box::new(empty()));
    };
    let cursor = host.sql_open(&query.sql, &query.params, KB_SQL_GENERIC_PLAN)?;
    let task = Task {
        compiled: Rc::clone(&task.compiled),
        unit: task.unit,
        choice: task.choice,
    };
    let place = query.place;
    let pattern = pattern.clone();
    Ok(Box::new(cursor.filter_map(move |row| {
        let row = match row {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };
        let r = Row {
            values: &row,
            place: &place,
        };
        make_quad(&task.ctx(), task.graph_map(), &r, &pattern).transpose()
    })))
}

/// The units of a pattern one query reads at most: PostgreSQL plans a UNION
/// ALL in a time that grows faster than its branches -- 179 in some 4 ms,
/// 5,000 in some 45 s -- and the plans of a pattern's queries stay in the
/// backend's cache, MAX_CACHED_PLANS of src/sql.c, when they are fewer
const BATCH_UNITS: usize = 64;

/// Many units' queries as one, a UNION ALL: each SELECT gives the number of
/// its unit and its columns as text (value_sql()), as many as the widest
/// unit's, NULL past its own; the rows made into quads as run_task() makes
/// them.
fn run_batch(
    host: &Host,
    tasks: Vec<Task>,
    pattern: &[Option<Tk>; 4],
    terms: &Rc<[Option<Term>; 4]>,
) -> EResult<QuadIter> {
    let mut params: Vec<String> = Vec::new();
    let mut ready = Vec::new();
    for (i, task) in tasks.iter().enumerate() {
        let ctx = task.ctx();
        let binds = task.binds(terms);
        let bind_refs: Vec<&Bind> = binds.iter().collect();
        let not_null = [&ctx.unit.subject, &ctx.unit.predicate, &ctx.unit.object];
        if let Some(conds) = conditions(&ctx, &bind_refs, &not_null, &mut params) {
            ready.push((i, conds));
        }
    }
    let columns: Vec<Vec<ColRef>> = tasks.iter().map(Task::columns).collect();
    let width = ready.iter().map(|(i, _)| columns[*i].len()).max().unwrap_or(0);
    let mut selects = Vec::with_capacity(ready.len());
    for (i, conds) in &ready {
        let ctx = tasks[*i].ctx();
        let mut out = vec![format!("{i} AS kb_t")];
        for k in 0..width {
            out.push(match columns[*i].get(k) {
                Some(c) => format!("{} AS kb_v{k}", value_sql(&ctx, *c)),
                None => format!("NULL::text AS kb_v{k}"),
            });
        }
        let mut sql = format!("SELECT {} FROM {}", out.join(", "), from_sql(&ctx));
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        selects.push(sql);
    }
    if selects.is_empty() {
        return Ok(Box::new(empty()));
    }
    let cursor = host.sql_open(&selects.join(" UNION ALL "), &params, KB_SQL_GENERIC_PLAN)?;
    let pattern = pattern.clone();
    Ok(Box::new(cursor.filter_map(move |row| {
        let row = match row {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };
        let made = (|| -> EResult<Option<InternalQuad<Tk>>> {
            let i: usize = row
                .first()
                .and_then(|v| v.as_deref())
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| EngineError::internal("a unit's number the SQL gave is not one"))?;
            let task = &tasks[i];
            let ctx = task.ctx();
            let cols = &columns[i];
            let mut values: Vec<Option<String>> = Vec::with_capacity(cols.len());
            for (k, c) in cols.iter().enumerate() {
                let v = row.get(k + 1).cloned().flatten();
                values.push(match (v, value_kind(&ctx, *c)) {
                    (Some(v), Some(Value::Float4)) => Some(float_value(&v, true).ok_or_else(bad_float)?),
                    (Some(v), Some(Value::Float8)) => Some(float_value(&v, false).ok_or_else(bad_float)?),
                    (v, _) => v,
                });
            }
            let place: Vec<(ColRef, usize)> = cols.iter().enumerate().map(|(k, c)| (*c, k)).collect();
            make_quad(
                &ctx,
                task.graph_map(),
                &Row {
                    values: &values,
                    place: &place,
                },
                &pattern,
            )
        })();
        made.transpose()
    })))
}

pub(crate) fn bad_float() -> EngineError {
    EngineError::internal("a floating point value's bits the SQL gave are not readable")
}

impl VirtualSet {
    /// The named graphs the mappings make: their constants, and the values
    /// of their graph maps' columns.
    pub fn named_graphs(&self, host: &Host) -> EResult<Vec<Tk>> {
        let mut out: Vec<Tk> = Vec::new();
        for compiled in &self.mappings {
            for unit in &compiled.units {
                let ctx = Context {
                    compiled,
                    unit,
                };
                for g in &unit.graphs {
                    let CGraph::Map(m) = g else { continue };
                    if let CValue::Constant(t, _) = &m.value {
                        out.push(tk_of(t.clone()));
                        continue;
                    }
                    let Some(query) = build_query(&ctx, &[], &[m], &[m], None, true) else {
                        continue;
                    };
                    for row in host.sql_open(&query.sql, &query.params, 0)? {
                        let row = row?;
                        let r = Row {
                            values: &row,
                            place: &query.place,
                        };
                        if let Some(t) = ctx.generate(m, &r)? {
                            out.push(tk_of(t));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn contains_graph(&self, host: &Host, graph: &Tk) -> EResult<bool> {
        let Some(term) = bound_term(host, Some(graph))? else {
            return Ok(false);
        };
        for compiled in &self.mappings {
            for unit in &compiled.units {
                let ctx = Context {
                    compiled,
                    unit,
                };
                for g in &unit.graphs {
                    let CGraph::Map(m) = g else { continue };
                    let bind = ctx.bind(m, &term);
                    if matches!(bind, Bind::Never) {
                        continue;
                    }
                    if let CValue::Constant(..) = &m.value {
                        return Ok(true);
                    }
                    let Some(query) = build_query(&ctx, &[&bind], &[m], &[m], Some(1000), true)
                    else {
                        continue;
                    };
                    // a graph's test, for each solution of a GRAPH ?g
                    for row in host.sql_open(&query.sql, &query.params, KB_SQL_GENERIC_PLAN)? {
                        let row = row?;
                        let r = Row {
                            values: &row,
                            place: &query.place,
                        };
                        if let Some(t) = ctx.generate(m, &r)? {
                            if key::term_key(t.as_ref()).0 == graph.key {
                                return Ok(true);
                            }
                        }
                    }
                }
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(i: usize) -> ColRef {
        ColRef {
            side: Side::Child,
            index: i,
        }
    }

    #[test]
    fn templates_match() {
        let parts = vec![
            CPart::Text("http://ex/emp/".into()),
            CPart::Column(c(0)),
            CPart::Text("/".into()),
            CPart::Column(c(1)),
        ];
        let m = match_template(&parts, "http://ex/emp/7/Bob%20Smith", true).unwrap();
        assert_eq!(m, vec![(c(0), Some("7".into())), (c(1), Some("Bob Smith".into()))]);
        assert!(match_template(&parts, "http://ex/dept/7/x", true).is_none());
        // a separator a value may hold: no condition for what follows
        let parts = vec![
            CPart::Text("http://ex/".into()),
            CPart::Column(c(0)),
            CPart::Text("-".into()),
            CPart::Column(c(1)),
        ];
        let m = match_template(&parts, "http://ex/a-b-c", true).unwrap();
        assert_eq!(m, vec![(c(0), None), (c(1), None)]);
    }

    #[test]
    fn floats() {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(float_value(&hex(&1.5f64.to_be_bytes()), false).as_deref(), Some("1.5"));
        assert_eq!(float_value(&hex(&0.1f32.to_be_bytes()), true).as_deref(), Some("0.1"));
        assert_eq!(float_value(&hex(&f64::INFINITY.to_be_bytes()), false).as_deref(), Some("INF"));
        assert_eq!(float_value(&hex(&f64::NEG_INFINITY.to_be_bytes()), false).as_deref(), Some("-INF"));
        assert_eq!(float_value(&hex(&f64::NAN.to_be_bytes()), false).as_deref(), Some("NaN"));
        assert_eq!(float_value("zz", false), None);
    }

    #[test]
    fn blank_labels() {
        let prefix = "r0123456789abcdefx";
        for label in ["dc931584c844f7352b6e17b547c6ecf4", "emp 7", "a-b_c.d", "\u{42f}\u{440}\u{43e}\u{441}\u{43b}\u{430}\u{432}/\u{2116}5", ""] {
            let id = blank_id(prefix, label);
            assert!(id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "{id}");
            assert!(oxrdf::BlankNode::new(id.clone()).is_ok(), "{id}");
            assert_eq!(blank_id_label(prefix, &id).as_deref(), Some(label));
        }
        // another mapping's, or the store's
        assert_eq!(blank_id_label(prefix, "r0123456789abcdeex7"), None);
        assert_eq!(blank_id_label(prefix, "b1"), None);
        assert_eq!(blank_id_label(prefix, &format!("{prefix}-4")), None);
    }
}
