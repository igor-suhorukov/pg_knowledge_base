// SPDX-License-Identifier: Apache-2.0
//! SPARQL 1.1 Update (docs/design.md §2.5), as oxigraph's store evaluates
//! it (oxigraph/lib/oxigraph/src/sparql/update.rs): each operation in turn,
//! reading what the ones before it wrote; DELETE/INSERT's WHERE evaluated
//! first, then its deletions, then its insertions.  The writes are the
//! caller's transaction's: a ROLLBACK takes them back.  R2RML's virtual
//! graphs are read, never written.
//!
//! A port of oxigraph's evaluator to this store's writer: Copyright (c) 2018
//! Oxigraph developers, MIT OR Apache-2.0, used here under Apache-2.0.

use oxiri::Iri;
use oxrdf::{BlankNode, GraphName as OxGraphName, NamedOrBlankNode, Quad as OxQuad, Term as OxTerm};
use oxstr::OxString;
use rustc_hash::FxHashMap;
use spareval::{DeleteInsertQuad, QueryDatasetSpecification};
use spargebra::SparqlParser;
use spargebra::algebra::GraphTarget;
use spargebra::term::{GraphName, GroundQuad, GroundTerm, Quad, Term};
use spargebra::update::{
    ClearOperation, CreateOperation, DeleteDataOperation, DeleteInsertOperation, DropOperation,
    GraphUpdateOperation, InsertDataOperation, LoadOperation,
};

use crate::error::{EResult, EngineError};
use crate::ffi::*;
use crate::host::Host;
use crate::key::{self, fresh_blank_node};
use crate::load;
use crate::query::{self, DatasetOptions};
use crate::writer::StoreWriter;

pub struct UpdateOptions {
    pub base_iri: Option<String>,
    pub using: DatasetOptions,
    pub allow_load: bool,
}

pub fn update(host: Host, text: &str, options: &UpdateOptions) -> EResult<()> {
    let mut parser = SparqlParser::new();
    if let Some(base) = &options.base_iri {
        parser = parser.with_base_iri(base)?;
    }
    let update = parser.parse_update(text)?;
    let mut evaluator = Evaluator {
        host,
        writer: StoreWriter::new(host, 8192),
        base_iri: update.base_iri.clone(),
        options,
    };
    for operation in &update.operations {
        evaluator.eval(operation)?;
        evaluator.writer.flush()?;
        host.check_interrupts()?;
    }
    Ok(())
}

struct Evaluator<'a> {
    host: Host,
    writer: StoreWriter,
    base_iri: Option<Iri<OxString>>,
    options: &'a UpdateOptions,
}

impl Evaluator<'_> {
    fn eval(&mut self, operation: &GraphUpdateOperation) -> EResult<()> {
        match operation {
            GraphUpdateOperation::InsertData(op) => self.eval_insert_data(op),
            GraphUpdateOperation::DeleteData(op) => self.eval_delete_data(op),
            GraphUpdateOperation::DeleteInsert(op) => self.eval_delete_insert(op),
            GraphUpdateOperation::Load(op) => self.eval_load(op),
            GraphUpdateOperation::Clear(op) => self.eval_clear(op),
            GraphUpdateOperation::Create(op) => self.eval_create(op),
            GraphUpdateOperation::Drop(op) => self.eval_drop(op),
        }
    }

    fn eval_insert_data(&mut self, operation: &InsertDataOperation) -> EResult<()> {
        let mut bnodes = FxHashMap::default();
        for quad in &operation.data {
            let quad = convert_quad(quad, &mut bnodes);
            self.writer.insert(quad.as_ref())?;
        }
        Ok(())
    }

    fn eval_delete_data(&mut self, operation: &DeleteDataOperation) -> EResult<()> {
        for quad in &operation.data {
            let quad = convert_ground_quad(quad);
            self.writer.delete(quad.as_ref())?;
        }
        Ok(())
    }

    fn eval_delete_insert(&mut self, operation: &DeleteInsertOperation) -> EResult<()> {
        let evaluator = query::evaluator();
        let mut prepared = evaluator.prepare_delete_insert(operation, self.base_iri.as_ref());
        if operation.using.is_none() {
            // the protocol's using-graph-uri, using-named-graph-uri and the
            // union default graph, where the update names no USING
            let mut using = QueryDatasetSpecification::new();
            self.options.using.apply(&mut using)?;
            *prepared.dataset_mut() = using;
        }
        let dataset = query::dataset(self.host, self.options.using.with_mappings)?;
        let mutations = prepared
            .execute(dataset)?
            .collect::<Result<Vec<_>, _>>()?;
        for mutation in &mutations {
            if let DeleteInsertQuad::Delete(quad) = mutation {
                self.writer.delete(quad.as_ref())?;
            }
        }
        self.writer.flush_deletes()?;
        for mutation in &mutations {
            if let DeleteInsertQuad::Insert(quad) = mutation {
                self.writer.insert(quad.as_ref())?;
            }
        }
        Ok(())
    }

    fn eval_load(&mut self, operation: &LoadOperation) -> EResult<()> {
        let result = self.load(operation);
        match result {
            Err(e) if operation.silent && !e.is_host() => Ok(()),
            r => r,
        }
    }

    fn load(&mut self, operation: &LoadOperation) -> EResult<()> {
        let source = operation.source.as_str();
        let Some(path) = source.strip_prefix("file://") else {
            return Err(EngineError::unsupported(format!(
                "LOAD reads file: IRIs only, not <{source}>: load other documents with kb.load()"
            )));
        };
        if !self.options.allow_load {
            return Err(EngineError::invalid(format!(
                "LOAD <{source}> reads a server file, which needs the privileges of pg_read_server_files"
            )));
        }
        let path = percent_encoding::percent_decode_str(path)
            .decode_utf8()
            .map_err(|e| EngineError::invalid(format!("LOAD <{source}>: {e}")))?;
        let graph = match &operation.destination {
            GraphName::NamedNode(g) => {
                // the graph is there once loaded into, even from an empty document
                self.writer.insert_graph(g.as_ref().into());
                OxGraphName::NamedNode(g.clone())
            }
            GraphName::DefaultGraph => OxGraphName::DefaultGraph,
        };
        self.writer.flush()?;
        let spec = load::LoadOptions {
            format: None,
            base_iri: Some(source.to_owned()),
            graph,
            lenient: false,
            batch: 8192,
            datasets: false,
        };
        load::load_file(self.host, &path, &spec)?;
        Ok(())
    }

    fn graph_exists(&self, graph: &spargebra::term::NamedNode) -> EResult<bool> {
        self.host.graph_exists(&key::iri_key(graph.as_str()))
    }

    fn eval_create(&mut self, operation: &CreateOperation) -> EResult<()> {
        if self.graph_exists(&operation.graph)? {
            if operation.silent {
                Ok(())
            } else {
                Err(EngineError::exists(format!(
                    "The graph {} already exists",
                    operation.graph
                )))
            }
        } else {
            self.writer.insert_graph(operation.graph.as_ref().into());
            Ok(())
        }
    }

    fn eval_clear(&mut self, operation: &ClearOperation) -> EResult<()> {
        self.writer.flush()?;
        match &operation.graph {
            GraphTarget::NamedNode(graph) => {
                if self.graph_exists(graph)? {
                    self.host
                        .graph_clear(KB_CLEAR_GRAPH, Some(&key::iri_key(graph.as_str())))
                } else if operation.silent {
                    Ok(())
                } else {
                    Err(EngineError::not_found(format!(
                        "The graph {graph} does not exist"
                    )))
                }
            }
            GraphTarget::DefaultGraph => {
                self.host.graph_clear(KB_CLEAR_GRAPH, Some(&key::DEFAULT_GRAPH))
            }
            GraphTarget::NamedGraphs => self.host.graph_clear(KB_CLEAR_NAMED, None),
            GraphTarget::AllGraphs => {
                self.host.graph_clear(KB_CLEAR_GRAPH, Some(&key::DEFAULT_GRAPH))?;
                self.host.graph_clear(KB_CLEAR_NAMED, None)
            }
        }
    }

    fn eval_drop(&mut self, operation: &DropOperation) -> EResult<()> {
        self.writer.flush()?;
        match &operation.graph {
            GraphTarget::NamedNode(graph) => {
                if self.graph_exists(graph)? {
                    self.host
                        .graph_clear(KB_CLEAR_DROP, Some(&key::iri_key(graph.as_str())))
                } else if operation.silent {
                    Ok(())
                } else {
                    Err(EngineError::not_found(format!(
                        "The graph {graph} does not exist"
                    )))
                }
            }
            GraphTarget::DefaultGraph => {
                self.host.graph_clear(KB_CLEAR_GRAPH, Some(&key::DEFAULT_GRAPH))
            }
            GraphTarget::NamedGraphs => self.host.graph_clear(KB_CLEAR_DROP_NAMED, None),
            GraphTarget::AllGraphs => self.host.graph_clear(KB_CLEAR_ALL, None),
        }
    }
}

fn convert_blank_node(node: &BlankNode, bnodes: &mut FxHashMap<BlankNode, BlankNode>) -> BlankNode {
    bnodes
        .entry(node.clone())
        .or_insert_with(fresh_blank_node)
        .clone()
}

fn convert_quad(quad: &Quad, bnodes: &mut FxHashMap<BlankNode, BlankNode>) -> OxQuad {
    OxQuad {
        subject: match &quad.subject {
            NamedOrBlankNode::NamedNode(subject) => subject.clone().into(),
            NamedOrBlankNode::BlankNode(subject) => convert_blank_node(subject, bnodes).into(),
        },
        predicate: quad.predicate.clone(),
        object: match &quad.object {
            Term::NamedNode(object) => object.clone().into(),
            Term::BlankNode(object) => convert_blank_node(object, bnodes).into(),
            Term::Literal(object) => OxTerm::from(object.clone()),
        },
        graph_name: match &quad.graph_name {
            GraphName::NamedNode(graph_name) => graph_name.clone().into(),
            GraphName::DefaultGraph => OxGraphName::DefaultGraph,
        },
    }
}

fn convert_ground_quad(quad: &GroundQuad) -> OxQuad {
    OxQuad {
        subject: quad.subject.clone().into(),
        predicate: quad.predicate.clone(),
        object: match &quad.object {
            GroundTerm::NamedNode(object) => object.clone().into(),
            GroundTerm::Literal(object) => object.clone().into(),
        },
        graph_name: match &quad.graph_name {
            GraphName::NamedNode(graph_name) => graph_name.clone().into(),
            GraphName::DefaultGraph => OxGraphName::DefaultGraph,
        },
    }
}
