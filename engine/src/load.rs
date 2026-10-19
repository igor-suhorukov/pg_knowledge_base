// SPDX-License-Identifier: Apache-2.0
//! RDF documents into the store, by oxigraph's parsers (oxrdfio), and the
//! store out of it, by its serializers (docs/design.md §2.6).

use std::fs::File;
use std::io::{BufReader, Read};

use oxrdf::{BlankNode, GraphName, NamedNode, NamedOrBlankNode, Quad, Term, Triple};
use oxrdfio::{RdfFormat, RdfParser, RdfSerializer};
use rustc_hash::FxHashMap;
use spareval::{InternalQuad, QueryableDataset};

use crate::dataset::KbDataset;
use crate::error::{EResult, EngineError};
use crate::format;
use crate::host::{Host, HostWriter};
use crate::key::fresh_blank_node;
use crate::query;
use crate::terms::{self, Tk};
use crate::writer::StoreWriter;

pub struct LoadOptions {
    /// None: from the file's extension
    pub format: Option<RdfFormat>,
    pub base_iri: Option<String>,
    /// The graph of the document's triples (a dataset's named graphs stay theirs)
    pub graph: GraphName,
    pub lenient: bool,
    pub batch: usize,
    /// The document may name graphs only when its format holds datasets
    pub datasets: bool,
}

fn parser(format: RdfFormat, options: &LoadOptions) -> EResult<RdfParser> {
    let mut parser = RdfParser::from_format(format).with_default_graph(options.graph.clone());
    if let Some(base) = &options.base_iri {
        parser = parser.with_base_iri(base)?;
    }
    if !options.datasets {
        parser = parser.without_named_graphs();
    }
    if options.lenient {
        parser = parser.lenient();
    }
    Ok(parser)
}

/// Each blank node of a document a fresh one of the engine's, as a store
/// keeps the documents it loads apart.
struct Renamer {
    names: FxHashMap<BlankNode, BlankNode>,
}

impl Renamer {
    fn node(&mut self, node: BlankNode) -> BlankNode {
        self.names.entry(node).or_insert_with(fresh_blank_node).clone()
    }

    fn quad(&mut self, quad: Quad) -> Quad {
        Quad {
            subject: match quad.subject {
                NamedOrBlankNode::BlankNode(b) => self.node(b).into(),
                s => s,
            },
            predicate: quad.predicate,
            object: match quad.object {
                Term::BlankNode(b) => self.node(b).into(),
                o => o,
            },
            graph_name: match quad.graph_name {
                GraphName::BlankNode(b) => self.node(b).into(),
                g => g,
            },
        }
    }
}

fn store_quads<E: Into<EngineError>>(
    host: Host,
    quads: impl Iterator<Item = Result<Quad, E>>,
    options: &LoadOptions,
) -> EResult<u64> {
    let mut writer = StoreWriter::new(host, options.batch);
    let mut renamer = Renamer {
        names: FxHashMap::default(),
    };
    let mut n: u64 = 0;
    for quad in quads {
        let quad = renamer.quad(quad.map_err(Into::into)?);
        writer.insert(quad.as_ref())?;
        n += 1;
        if n % 16384 == 0 {
            host.check_interrupts()?;
        }
    }
    writer.flush()?;
    Ok(n)
}

pub fn load_bytes(host: Host, data: &[u8], format: RdfFormat, options: &LoadOptions) -> EResult<u64> {
    let parser = parser(format, options)?;
    store_quads(
        host,
        parser.for_slice(data).map(|q| q.map_err(|e| EngineError::syntax(e.to_string()))),
        options,
    )
}

pub fn load_reader(
    host: Host,
    reader: impl Read,
    format: RdfFormat,
    options: &LoadOptions,
) -> EResult<u64> {
    let parser = parser(format, options)?;
    store_quads(host, parser.for_reader(reader), options)
}

pub fn load_file(host: Host, path: &str, options: &LoadOptions) -> EResult<u64> {
    let format = match options.format {
        Some(f) => f,
        None => {
            let extension = std::path::Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            format::rdf_format(extension).ok_or_else(|| {
                EngineError::invalid(format!(
                    "the format of \"{path}\" is not known from its extension: name it"
                ))
            })?
        }
    };
    let file = File::open(path)
        .map_err(|e| EngineError::not_found(format!("could not open \"{path}\": {e}")))?;
    load_reader(host, BufReader::with_capacity(1 << 20, file), format, options)
}

fn externalize_batch(host: &Host, batch: &[InternalQuad<Tk>]) -> EResult<Vec<Quad>> {
    terms::prefetch(
        host,
        batch.iter().flat_map(|q| {
            [
                Some(q.subject.key),
                Some(q.predicate.key),
                Some(q.object.key),
                q.graph_name.as_ref().map(|g| g.key),
            ]
            .into_iter()
            .flatten()
        }),
    )?;
    batch
        .iter()
        .map(|q| {
            let subject = match terms::externalize(host, &q.subject)? {
                Term::NamedNode(n) => NamedOrBlankNode::from(n),
                Term::BlankNode(b) => b.into(),
                Term::Literal(_) => return Err(EngineError::internal("a literal subject in the store")),
            };
            let predicate = match terms::externalize(host, &q.predicate)? {
                Term::NamedNode(n) => n,
                _ => return Err(EngineError::internal("a predicate that is not an IRI in the store")),
            };
            let object = terms::externalize(host, &q.object)?;
            let graph_name = match &q.graph_name {
                None => GraphName::DefaultGraph,
                Some(g) => match terms::externalize(host, g)? {
                    Term::NamedNode(n) => n.into(),
                    Term::BlankNode(b) => b.into(),
                    Term::Literal(_) => return Err(EngineError::internal("a literal graph name in the store")),
                },
            };
            Ok(Quad {
                subject,
                predicate,
                object,
                graph_name,
            })
        })
        .collect()
}

pub enum DumpGraph {
    All,
    Default,
    Named(NamedNode),
}

/// The store -- with R2RML's graphs when asked -- serialized through the
/// host's output.
pub fn dump(host: Host, format: RdfFormat, graph: &DumpGraph, with_mappings: bool) -> EResult<()> {
    if matches!(graph, DumpGraph::All) && !format.supports_datasets() {
        return Err(EngineError::invalid(format!(
            "{} holds no named graphs: dump one graph, or use N-Quads or TriG",
            format.name()
        )));
    }
    let dataset: KbDataset = query::dataset(host, with_mappings)?;
    let mut out = HostWriter::new(host);
    let mut serializer = RdfSerializer::from_format(format).for_writer(&mut out);
    let patterns: Vec<Option<Option<Tk>>> = match graph {
        DumpGraph::All => vec![Some(None), None],
        DumpGraph::Default => vec![Some(None)],
        DumpGraph::Named(g) => vec![Some(Some(Tk::from_term(g.clone().into())))],
    };
    let as_triples = !matches!(graph, DumpGraph::All);
    for pattern in patterns {
        let mut quads = dataset.internal_quads_for_pattern(
            None,
            None,
            None,
            pattern.as_ref().map(|g| g.as_ref()),
        );
        loop {
            let mut batch = Vec::with_capacity(4096);
            for q in quads.by_ref().take(4096) {
                batch.push(q?);
            }
            if batch.is_empty() {
                break;
            }
            for quad in externalize_batch(&host, &batch)? {
                if as_triples {
                    serializer.serialize_triple(&Triple::from(quad))?;
                } else {
                    serializer.serialize_quad(&quad)?;
                }
            }
            host.check_interrupts()?;
        }
    }
    serializer.finish()?;
    std::io::Write::flush(&mut out)?;
    Ok(())
}
