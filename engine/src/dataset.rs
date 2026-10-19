// SPDX-License-Identifier: Apache-2.0
//! The dataset a query reads (docs/design.md §3.2): the store's quads,
//! through the host's index scans, and R2RML's virtual graphs, through SQL,
//! as one RDF dataset -- spareval's QueryableDataset, the boundary oxigraph
//! offers for a store of one's own.

use std::iter::{empty, once};
use std::rc::Rc;

use oxrdf::Term;
use oxsdatatypes::Boolean;
use rustc_hash::FxHashSet;
use spareval::{ExpressionTerm, InternalQuad, InternalTriple, QueryableDataset};

use crate::error::{EResult, EngineError};
use crate::host::{Graphs, Host};
use crate::key::{self, DEFAULT_GRAPH, Key};
use crate::r2rml::VirtualGraphs;
use crate::terms::{self, Tk};

pub struct DatasetInner {
    pub host: Host,
    /// R2RML's virtual graphs, when the query reads them
    pub virt: Option<Rc<VirtualGraphs>>,
}

#[derive(Clone)]
pub struct KbDataset(pub Rc<DatasetInner>);

impl KbDataset {
    pub fn new(host: Host, virt: Option<Rc<VirtualGraphs>>) -> Self {
        Self(Rc::new(DatasetInner { host, virt }))
    }
}

type QuadIter = Box<dyn Iterator<Item = EResult<InternalQuad<Tk>>>>;

fn stored_quad(q: [Key; 4]) -> InternalQuad<Tk> {
    InternalQuad {
        subject: Tk::stored(q[0]),
        predicate: Tk::stored(q[1]),
        object: Tk::stored(q[2]),
        graph_name: if key::is_default_graph(&q[3]) {
            None
        } else {
            Some(Tk::stored(q[3]))
        },
    }
}

fn quad_keys(q: &InternalQuad<Tk>) -> [Key; 4] {
    [
        q.subject.key,
        q.predicate.key,
        q.object.key,
        q.graph_name.as_ref().map_or(DEFAULT_GRAPH, |g| g.key),
    ]
}

/// The store's quads for a pattern; nothing where a bound term cannot be in
/// its place.
fn native_quads(
    host: &Host,
    subject: Option<&Tk>,
    predicate: Option<&Tk>,
    object: Option<&Tk>,
    graph_name: Option<Option<&Tk>>,
) -> QuadIter {
    if subject.is_some_and(|s| !key::is_resource(&s.key))
        || predicate.is_some_and(|p| !key::is_iri(&p.key))
        || graph_name.flatten().is_some_and(|g| !key::is_resource(&g.key))
    {
        return Box::new(empty());
    }
    let graphs = match graph_name {
        Some(None) => Graphs::One(&DEFAULT_GRAPH),
        Some(Some(g)) => Graphs::One(&g.key),
        None => Graphs::Named,
    };
    match host.quads(
        subject.map(|t| &t.key),
        predicate.map(|t| &t.key),
        object.map(|t| &t.key),
        graphs,
    ) {
        Ok(cursor) => Box::new(cursor.map(|q| q.map(stored_quad))),
        Err(e) => Box::new(once(Err(e))),
    }
}

impl QueryableDataset<'static> for KbDataset {
    type InternalTerm = Tk;
    type Error = EngineError;

    fn internal_quads_for_pattern(
        &self,
        subject: Option<&Tk>,
        predicate: Option<&Tk>,
        object: Option<&Tk>,
        graph_name: Option<Option<&Tk>>,
    ) -> impl Iterator<Item = EResult<InternalQuad<Tk>>> + use<> {
        let inner = &self.0;
        let native = native_quads(&inner.host, subject, predicate, object, graph_name);
        let Some(virt) = &inner.virt else {
            return native;
        };
        let (virtual_quads, sources) =
            virt.quads_for_pattern(&inner.host, subject, predicate, object, graph_name);
        if sources == 0 {
            return native;
        }
        // a quad two sources make is in the dataset once
        let mut seen = FxHashSet::default();
        Box::new(native.chain(virtual_quads).filter(move |q| match q {
            Ok(q) => seen.insert(quad_keys(q)),
            Err(_) => true,
        })) as QuadIter
    }

    /// Triples of a merge of graphs.  None, the union default graph, is
    /// every graph's -- the default graph's too -- as oxigraph's store reads
    /// it (oxigraph/lib/oxigraph/src/storage, triples_for_pattern), where
    /// spareval's own default for a dataset is the named graphs alone.
    fn internal_triples_for_pattern(
        &self,
        subject: Option<&Tk>,
        predicate: Option<&Tk>,
        object: Option<&Tk>,
        graph_names: Option<&[Option<Tk>]>,
    ) -> impl Iterator<Item = EResult<InternalTriple<Tk>>> + use<> {
        let quads: QuadIter = match graph_names {
            Some(graphs) => {
                let iters: Vec<QuadIter> = graphs
                    .iter()
                    .map(|g| {
                        Box::new(self.internal_quads_for_pattern(
                            subject,
                            predicate,
                            object,
                            Some(g.as_ref()),
                        )) as QuadIter
                    })
                    .collect();
                Box::new(iters.into_iter().flatten())
            }
            None => Box::new(
                self.internal_quads_for_pattern(subject, predicate, object, Some(None))
                    .chain(self.internal_quads_for_pattern(subject, predicate, object, None)),
            ),
        };
        let mut seen = FxHashSet::default();
        quads.map(|q| q.map(InternalTriple::from)).filter(move |t| match t {
            Ok(t) => seen.insert([t.subject.key, t.predicate.key, t.object.key]),
            Err(_) => true,
        })
    }

    fn internal_named_graphs(&self) -> impl Iterator<Item = EResult<Tk>> + use<> {
        let inner = Rc::clone(&self.0);
        let mut graphs: Vec<EResult<Tk>> = Vec::new();
        let mut seen = FxHashSet::default();
        match inner.host.graphs() {
            Ok(cursor) => {
                for q in cursor {
                    match q {
                        Ok(q) => {
                            if seen.insert(q[3]) {
                                graphs.push(Ok(Tk::stored(q[3])));
                            }
                        }
                        Err(e) => {
                            graphs.push(Err(e));
                            break;
                        }
                    }
                }
            }
            Err(e) => graphs.push(Err(e)),
        }
        if let Some(virt) = &inner.virt {
            match virt.named_graphs(&inner.host) {
                Ok(names) => {
                    for g in names {
                        if seen.insert(g.key) {
                            graphs.push(Ok(g));
                        }
                    }
                }
                Err(e) => graphs.push(Err(e)),
            }
        }
        graphs.into_iter()
    }

    fn contains_internal_graph_name(&self, graph_name: &Tk) -> EResult<bool> {
        if !key::is_resource(&graph_name.key) {
            return Ok(false);
        }
        if self.0.host.graph_exists(&graph_name.key)? {
            return Ok(true);
        }
        match &self.0.virt {
            Some(virt) => virt.contains_graph(&self.0.host, graph_name),
            None => Ok(false),
        }
    }

    fn internalize_term(&self, term: Term) -> EResult<Tk> {
        Ok(Tk::from_term(term))
    }

    fn externalize_term(&self, term: Tk) -> EResult<Term> {
        terms::externalize(&self.0.host, &term)
    }

    fn externalize_expression_term(&self, term: Tk) -> EResult<ExpressionTerm> {
        if let Some(value) = key::inline_expression_term(&term.key) {
            return Ok(value);
        }
        Ok(terms::externalize(&self.0.host, &term)?.into())
    }

    fn internalize_expression_term(&self, term: ExpressionTerm) -> EResult<Tk> {
        if let Some(key) = key::expression_term_key(&term) {
            return Ok(Tk::stored(key));
        }
        Ok(Tk::from_term(term.into()))
    }

    fn internal_term_effective_boolean_value(&self, term: Tk) -> EResult<Option<bool>> {
        Ok(match term.key[0] {
            key::TAG_STRING_HASH => Some(true), // longer than an inline string: not empty
            key::TAG_STRING_SMALL
            | key::TAG_BOOLEAN
            | key::TAG_FLOAT
            | key::TAG_DOUBLE
            | key::TAG_INTEGER
            | key::TAG_DECIMAL => match key::inline_expression_term(&term.key) {
                Some(ExpressionTerm::StringLiteral(v)) => Some(!v.is_empty()),
                Some(ExpressionTerm::BooleanLiteral(v)) => Some(v.into()),
                Some(ExpressionTerm::FloatLiteral(v)) => Some(Boolean::from(v).into()),
                Some(ExpressionTerm::DoubleLiteral(v)) => Some(Boolean::from(v).into()),
                Some(ExpressionTerm::IntegerLiteral(v)) => Some(Boolean::from(v).into()),
                Some(ExpressionTerm::DecimalLiteral(v)) => Some(Boolean::from(v).into()),
                _ => None,
            },
            _ => None,
        })
    }
}
