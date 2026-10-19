// SPDX-License-Identifier: Apache-2.0
//! An R2RML mapping (https://www.w3.org/TR/r2rml/), as parse.rs reads it
//! from its document.

use oxrdf::{NamedNode, Term};

#[derive(Debug, Clone)]
pub struct Mapping {
    pub triples_maps: Vec<TriplesMap>,
}

#[derive(Debug, Clone)]
pub struct TriplesMap {
    /// The resource that names it, for messages
    pub id: String,
    pub table: LogicalTable,
    pub subject: TermMap,
    pub classes: Vec<NamedNode>,
    /// The subject map's graph maps
    pub graphs: Vec<GraphMap>,
    pub predicate_object_maps: Vec<PredicateObjectMap>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalTable {
    /// rr:tableName: a table or view's name, in SQL's syntax
    Table(String),
    /// rr:sqlQuery: a SELECT
    Query(String),
}

impl LogicalTable {
    /// R2RML's effective SQL query (§5)
    pub fn effective_sql(&self) -> String {
        match self {
            LogicalTable::Table(name) => format!("SELECT * FROM {name}"),
            LogicalTable::Query(sql) => sql.trim().trim_end_matches(';').trim().to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PredicateObjectMap {
    pub predicates: Vec<TermMap>,
    pub objects: Vec<ObjectMap>,
    pub graphs: Vec<GraphMap>,
}

#[derive(Debug, Clone)]
pub enum ObjectMap {
    Term(TermMap),
    /// rr:parentTriplesMap: the parent's subject, over the rows the join conditions match
    Ref {
        parent: usize,
        joins: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermType {
    Iri,
    BlankNode,
    Literal,
}

#[derive(Debug, Clone)]
pub enum TermMapValue {
    Constant(Term),
    Column(String),
    Template(Template),
}

#[derive(Debug, Clone)]
pub struct TermMap {
    pub value: TermMapValue,
    pub term_type: TermType,
    pub language: Option<String>,
    pub datatype: Option<NamedNode>,
}

#[derive(Debug, Clone)]
pub enum GraphMap {
    /// rr:defaultGraph
    Default,
    Map(TermMap),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplatePart {
    Text(String),
    Column(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub parts: Vec<TemplatePart>,
}

impl Template {
    /// A string template: "{" and "}" around a column's name, "\" escaping
    /// either (R2RML §7.4).
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(e @ ('{' | '}' | '\\')) => text.push(e),
                    Some(other) => {
                        text.push('\\');
                        text.push(other);
                    }
                    None => return Err(format!("the template \"{s}\" ends with \"\\\"")),
                },
                '{' => {
                    if !text.is_empty() {
                        parts.push(TemplatePart::Text(std::mem::take(&mut text)));
                    }
                    let mut column = String::new();
                    let mut closed = false;
                    while let Some(c) = chars.next() {
                        match c {
                            '\\' => match chars.next() {
                                Some(e) => column.push(e),
                                None => break,
                            },
                            '}' => {
                                closed = true;
                                break;
                            }
                            '{' => {
                                return Err(format!("the template \"{s}\" nests a \"{{\""));
                            }
                            c => column.push(c),
                        }
                    }
                    if !closed {
                        return Err(format!("the template \"{s}\" leaves a \"{{\" open"));
                    }
                    if column.is_empty() {
                        return Err(format!("the template \"{s}\" names an empty column"));
                    }
                    parts.push(TemplatePart::Column(column));
                }
                '}' => return Err(format!("the template \"{s}\" closes a \"}}\" it did not open")),
                c => text.push(c),
            }
        }
        if !text.is_empty() {
            parts.push(TemplatePart::Text(text));
        }
        Ok(Self { parts })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates() {
        let t = Template::parse("http://example.com/{\"ID\"}/{Name}-x").unwrap();
        assert_eq!(
            t.parts,
            vec![
                TemplatePart::Text("http://example.com/".into()),
                TemplatePart::Column("\"ID\"".into()),
                TemplatePart::Text("/".into()),
                TemplatePart::Column("Name".into()),
                TemplatePart::Text("-x".into()),
            ]
        );
        let t = Template::parse("a\\{b\\}{c}").unwrap();
        assert_eq!(
            t.parts,
            vec![
                TemplatePart::Text("a{b}".into()),
                TemplatePart::Column("c".into())
            ]
        );
        assert!(Template::parse("{a").is_err());
        assert!(Template::parse("a}").is_err());
        assert!(Template::parse("{}").is_err());
    }
}
