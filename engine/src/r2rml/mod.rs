// SPDX-License-Identifier: Apache-2.0
//! R2RML (docs/design.md §4): mappings of PostgreSQL's tables to RDF
//! (https://www.w3.org/TR/r2rml/), read as virtual graphs beside the
//! store's -- no triple of theirs is stored: each pattern of a query reads
//! the tables through SQL as it is evaluated.
//!
//!	model.rs	a mapping
//!	parse.rs	a mapping read from its Turtle document, and checked
//!	compile.rs	a mapping compiled against its tables: units, one triple
//!				template each
//!	eval.rs		a pattern's virtual quads, through SQL
//!	pushdown.rs	patterns of a basic graph pattern the mappings answer, as
//!				one SQL query that joins them
//!	direct.rs	the W3C Direct Mapping of tables, as an R2RML document

pub mod compile;
pub mod direct;
pub mod eval;
pub mod model;
pub mod parse;
pub mod pushdown;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use spareval::InternalQuad;

use crate::error::EResult;
use crate::host::{Host, MappingRow};
use crate::r2rml::compile::{CGraph, CValue, Compiled};
use crate::r2rml::eval::VirtualSet;
use crate::terms::Tk;

pub type QuadIter = Box<dyn Iterator<Item = EResult<InternalQuad<Tk>>>>;

pub struct VirtualGraphs {
    set: VirtualSet,
}

impl VirtualGraphs {
    /// The virtual quads of a pattern, and how many units may make them.
    pub fn quads_for_pattern(
        &self,
        host: &Host,
        subject: Option<&Tk>,
        predicate: Option<&Tk>,
        object: Option<&Tk>,
        graph_name: Option<Option<&Tk>>,
    ) -> (QuadIter, usize) {
        self.set
            .quads_for_pattern(host, subject, predicate, object, graph_name)
    }

    pub fn named_graphs(&self, host: &Host) -> EResult<Vec<Tk>> {
        self.set.named_graphs(host)
    }

    pub fn contains_graph(&self, host: &Host, graph: &Tk) -> EResult<bool> {
        self.set.contains_graph(host, graph)
    }

    /// The compiled mappings, for pushdown.rs and the estimates of
    /// dataset.rs
    pub(crate) fn set(&self) -> &VirtualSet {
        &self.set
    }
}

thread_local! {
    /// Compiled mappings, by name, kept while their revision stays
    static COMPILED: RefCell<HashMap<String, Rc<Compiled>>> = RefCell::new(HashMap::new());
}

fn compile_row(host: &Host, row: &MappingRow) -> EResult<Compiled> {
    let (graph, base) = parse::parse_document(&row.document, row.base_iri.as_deref())?;
    let mapping = parse::read_mapping(&graph)?;
    compile::compile(host, &row.name, row.revision, &mapping, base.as_deref())
}

/// The enabled mappings, compiled; None when there are none.
pub fn virtual_graphs(host: &Host) -> EResult<Option<Rc<VirtualGraphs>>> {
    let rows = host.mappings()?;
    if rows.is_empty() {
        COMPILED.with(|c| c.borrow_mut().clear());
        return Ok(None);
    }
    let mut mappings = Vec::with_capacity(rows.len());
    for row in &rows {
        let cached = COMPILED.with(|c| {
            c.borrow()
                .get(&row.name)
                .filter(|m| m.revision == row.revision)
                .cloned()
        });
        let compiled = match cached {
            Some(m) => m,
            None => {
                let m = Rc::new(compile_row(host, row)?);
                COMPILED.with(|c| c.borrow_mut().insert(row.name.clone(), Rc::clone(&m)));
                m
            }
        };
        mappings.push(compiled);
    }
    COMPILED.with(|c| {
        c.borrow_mut()
            .retain(|name, _| rows.iter().any(|r| &r.name == name))
    });
    Ok(Some(Rc::new(VirtualGraphs {
        set: VirtualSet { mappings },
    })))
}

pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_list(items: impl IntoIterator<Item = String>) -> String {
    let items: Vec<String> = items.into_iter().map(|s| json_string(&s)).collect();
    format!("[{}]", items.join(", "))
}

/// A mapping checked against the tables it names: a JSON summary of it.
pub fn check_mapping(host: &Host, row: &MappingRow) -> EResult<String> {
    let compiled = compile_row(host, row)?;
    let mut predicates: Vec<String> = Vec::new();
    let mut graphs: Vec<String> = Vec::new();
    let mut templates = 0;
    for unit in &compiled.units {
        match &unit.predicate.value {
            CValue::Constant(t, _) => predicates.push(t.to_string()),
            _ => templates += 1,
        }
        for g in &unit.graphs {
            match g {
                CGraph::Default => graphs.push("default".to_owned()),
                CGraph::Map(m) => match &m.value {
                    CValue::Constant(t, _) => graphs.push(t.to_string()),
                    _ => graphs.push("(from columns)".to_owned()),
                },
            }
        }
    }
    predicates.sort();
    predicates.dedup();
    graphs.sort();
    graphs.dedup();
    Ok(format!(
        "{{\"name\": {}, \"base_iri\": {}, \"logical_tables\": {}, \"triple_templates\": {}, \"predicates\": {}, \"predicate_maps_from_columns\": {}, \"graphs\": {}}}",
        json_string(&compiled.name),
        compiled
            .base
            .as_ref()
            .map_or_else(|| "null".to_owned(), |b| json_string(b.as_str())),
        json_list(compiled.tables.iter().map(|t| t.sql.clone())),
        compiled.units.len(),
        json_list(predicates),
        templates,
        json_list(graphs),
    ))
}

/// The W3C Direct Mapping of tables, as an R2RML document.
pub fn direct_mapping(tables_json: &str, base_iri: &str) -> EResult<String> {
    direct::direct_mapping(tables_json, base_iri)
}
