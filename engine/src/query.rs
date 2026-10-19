// SPDX-License-Identifier: Apache-2.0
//! SPARQL queries (docs/design.md §2.4): parsed by spargebra, optimized by
//! sparopt and evaluated by spareval over the dataset of dataset.rs, as
//! oxigraph's own store evaluates them; their results a row at a time for
//! the tabular function, or serialized by sparesults and oxrdfio.

use std::sync::Arc;

use oxrdf::{GraphName, Literal, NamedNode, NamedOrBlankNode, Term, Triple, Variable};
use oxrdfio::RdfSerializer;
use sparesults::{QueryResultsSerializer, QuerySolution};
use spareval::{
    QueryDatasetSpecification, QueryEvaluator, QueryExplanation, QueryResults, QuerySolutionIter,
    QueryTripleIter,
};
use spargebra::{Query, SparqlParser};

use crate::dataset::KbDataset;
use crate::error::{EResult, EngineError};
use crate::ffi::*;
use crate::format;
use crate::host::{Host, HostWriter};
use crate::r2rml;

/// What a query reads, from KbQuerySpec or an HTTP request.
#[derive(Default, Clone)]
pub struct DatasetOptions {
    pub default_graphs: Vec<String>,
    pub named_graphs: Vec<String>,
    pub union_default_graph: bool,
    pub with_mappings: bool,
}

impl DatasetOptions {
    /// The protocol's dataset over the query's own FROM and FROM NAMED, as
    /// oxigraph's server applies it.
    pub fn apply(&self, dataset: &mut QueryDatasetSpecification) -> EResult<()> {
        if self.union_default_graph && self.default_graphs.is_empty() && self.named_graphs.is_empty()
        {
            dataset.set_default_graph_as_union();
        } else if !self.default_graphs.is_empty() || !self.named_graphs.is_empty() {
            dataset.set_default_graph(
                self.default_graphs
                    .iter()
                    .map(|g| Ok(GraphName::from(NamedNode::new(g.clone())?)))
                    .collect::<Result<Vec<_>, oxiri::IriParseError>>()?,
            );
            dataset.set_available_named_graphs(
                self.named_graphs
                    .iter()
                    .map(|g| Ok(NamedOrBlankNode::from(NamedNode::new(g.clone())?)))
                    .collect::<Result<Vec<_>, oxiri::IriParseError>>()?,
            );
        }
        Ok(())
    }
}

/// The evaluator every query and update uses: oxigraph's, as its store
/// configures it.
pub fn evaluator() -> QueryEvaluator {
    QueryEvaluator::new()
}

pub fn parse_query(text: &str, base_iri: Option<&str>) -> EResult<Query> {
    let mut parser = SparqlParser::new();
    if let Some(base) = base_iri {
        parser = parser.with_base_iri(base)?;
    }
    Ok(parser.parse_query(text)?)
}

/// The dataset of a query or update: the store, and R2RML's graphs when asked.
pub fn dataset(host: Host, with_mappings: bool) -> EResult<KbDataset> {
    let virt = if with_mappings && host.has_sql() {
        r2rml::virtual_graphs(&host)?
    } else {
        None
    };
    Ok(KbDataset::new(host, virt))
}

enum State {
    Solutions(QuerySolutionIter<'static>),
    Boolean(Option<bool>),
    Graph(QueryTripleIter<'static>),
    Done,
}

enum Current {
    Nothing,
    Solution(QuerySolution),
    Triple(Triple),
    Boolean(Term),
}

pub struct KbQuery {
    host: Host,
    kind: i32,
    variables: Vec<String>,
    variable_strs: Vec<KbStr>,
    state: State,
    current: Current,
    row: Vec<KbTerm>,
    explanation: Option<QueryExplanation>,
    /// SELECT, ASK, CONSTRUCT or DESCRIBE, for traces
    operation: &'static str,
    /// The rows, booleans or triples given so far
    rows: i64,
}

const NONE_TERM: KbTerm = KbTerm {
    kind: KB_TERM_NONE,
    value: KbStr::EMPTY,
    datatype: KbStr::EMPTY,
    lang: KbStr::EMPTY,
};

fn kb_term(term: Option<&Term>) -> KbTerm {
    match term {
        None => NONE_TERM,
        Some(Term::NamedNode(n)) => KbTerm {
            kind: KB_TERM_IRI,
            value: KbStr::from_str(n.as_str()),
            datatype: KbStr::EMPTY,
            lang: KbStr::EMPTY,
        },
        Some(Term::BlankNode(b)) => KbTerm {
            kind: KB_TERM_BNODE,
            value: KbStr::from_str(b.as_str()),
            datatype: KbStr::EMPTY,
            lang: KbStr::EMPTY,
        },
        Some(Term::Literal(l)) => KbTerm {
            kind: KB_TERM_LITERAL,
            value: KbStr::from_str(l.value()),
            datatype: KbStr::from_str(l.datatype().as_str()),
            lang: l.language().map_or(KbStr::EMPTY, KbStr::from_str),
        },
    }
}

impl KbQuery {
    pub fn open(
        host: Host,
        text: &str,
        base_iri: Option<&str>,
        options: &DatasetOptions,
        explain: bool,
    ) -> EResult<Self> {
        let query = parse_query(text, base_iri)?;
        let operation = match &query {
            Query::Select { .. } => "SELECT",
            Query::Ask { .. } => "ASK",
            Query::Construct { .. } => "CONSTRUCT",
            Query::Describe { .. } => "DESCRIBE",
        };
        let evaluator = if explain {
            evaluator().compute_statistics()
        } else {
            evaluator()
        };
        let mut prepared = evaluator.prepare(&query);
        options.apply(prepared.dataset_mut())?;
        let dataset = dataset(host, options.with_mappings)?;
        let (results, explanation) = if explain {
            let (results, explanation) = prepared.explain(dataset);
            (results?, Some(explanation))
        } else {
            (prepared.execute(dataset)?, None)
        };
        let (kind, variables, state) = match results {
            QueryResults::Solutions(solutions) => (
                KB_QUERY_SELECT,
                solutions
                    .variables()
                    .iter()
                    .map(|v| v.as_str().to_owned())
                    .collect(),
                State::Solutions(solutions),
            ),
            QueryResults::Boolean(value) => (
                KB_QUERY_ASK,
                vec!["result".to_owned()],
                State::Boolean(Some(value)),
            ),
            QueryResults::Graph(triples) => (
                KB_QUERY_CONSTRUCT,
                vec![
                    "subject".to_owned(),
                    "predicate".to_owned(),
                    "object".to_owned(),
                ],
                State::Graph(triples),
            ),
        };
        let variable_strs = Vec::with_capacity(variables.len());
        let mut q = Self {
            host,
            kind,
            variables,
            variable_strs,
            state,
            current: Current::Nothing,
            row: Vec::new(),
            explanation,
            operation,
            rows: 0,
        };
        q.variable_strs = q.variables.iter().map(|v| KbStr::from_str(v)).collect();
        Ok(q)
    }

    pub fn kind(&self) -> i32 {
        self.kind
    }

    pub fn variable_strs(&self) -> &[KbStr] {
        &self.variable_strs
    }

    /// The next row, its terms valid until the next call; None at the end.
    pub fn next_row(&mut self) -> EResult<Option<&[KbTerm]>> {
        self.row.clear();
        self.host.poll()?;
        self.current = match &mut self.state {
            State::Solutions(solutions) => match solutions.next() {
                Some(solution) => Current::Solution(solution?),
                None => Current::Nothing,
            },
            State::Boolean(value) => match value.take() {
                Some(v) => Current::Boolean(Literal::from(v).into()),
                None => Current::Nothing,
            },
            State::Graph(triples) => match triples.next() {
                Some(triple) => Current::Triple(triple?),
                None => Current::Nothing,
            },
            State::Done => Current::Nothing,
        };
        match &self.current {
            Current::Nothing => {
                self.state = State::Done;
                return Ok(None);
            }
            Current::Solution(solution) => {
                for value in solution.values() {
                    self.row.push(kb_term(value.as_ref()));
                }
            }
            Current::Triple(triple) => {
                self.row.push(match &triple.subject {
                    NamedOrBlankNode::NamedNode(n) => KbTerm {
                        kind: KB_TERM_IRI,
                        value: KbStr::from_str(n.as_str()),
                        datatype: KbStr::EMPTY,
                        lang: KbStr::EMPTY,
                    },
                    NamedOrBlankNode::BlankNode(b) => KbTerm {
                        kind: KB_TERM_BNODE,
                        value: KbStr::from_str(b.as_str()),
                        datatype: KbStr::EMPTY,
                        lang: KbStr::EMPTY,
                    },
                });
                self.row.push(KbTerm {
                    kind: KB_TERM_IRI,
                    value: KbStr::from_str(triple.predicate.as_str()),
                    datatype: KbStr::EMPTY,
                    lang: KbStr::EMPTY,
                });
                self.row.push(kb_term(Some(&triple.object)));
            }
            Current::Boolean(term) => self.row.push(kb_term(Some(term))),
        }
        self.rows += 1;
        Ok(Some(&self.row))
    }

    /// The results serialized in a format through the host's output.
    pub fn write(&mut self, format_name: &str) -> EResult<()> {
        let state = std::mem::replace(&mut self.state, State::Done);
        let mut out = HostWriter::new(self.host);
        match state {
            State::Solutions(solutions) => {
                let format = format::require_results_format(format_name)?;
                write_solutions(&self.host, &mut out, format, solutions, &mut self.rows)?;
            }
            State::Boolean(value) => {
                let format = format::require_results_format(format_name)?;
                QueryResultsSerializer::from_format(format)
                    .serialize_boolean_to_writer(&mut out, value.unwrap_or(false))?;
                self.rows += 1;
            }
            State::Graph(triples) => {
                let format = format::require_rdf_format(format_name)?;
                write_triples(&self.host, &mut out, format, triples, &mut self.rows)?;
            }
            State::Done => (),
        }
        std::io::Write::flush(&mut out)?;
        Ok(())
    }

    pub fn explain(&self) -> EResult<String> {
        let Some(explanation) = &self.explanation else {
            return Err(EngineError::invalid("the query was not opened to be explained"));
        };
        let mut out = Vec::new();
        explanation.write_in_json(&mut out)?;
        String::from_utf8(out).map_err(|e| EngineError::internal(e.to_string()))
    }

    /// What a trace takes of the query (trace.rs): its operation, its rows
    /// so far, and, opened to be explained, its plan's JSON.
    pub fn trace(&self) -> Trace {
        let plan = self.explanation.as_ref().and_then(|e| {
            let mut out = Vec::new();
            e.write_in_json(&mut out).ok().map(|()| out)
        });
        Trace {
            operation: self.operation,
            rows: self.rows,
            plan,
        }
    }
}

/// A call's figures for a trace: what kb_query_trace() and kb_http_trace()
/// give the extension.
#[derive(Default, Clone)]
pub struct Trace {
    pub operation: &'static str,
    pub rows: i64,
    /// spareval's explanation, as JSON
    pub plan: Option<Vec<u8>>,
}

pub fn write_solutions(
    host: &Host,
    out: &mut HostWriter,
    format: sparesults::QueryResultsFormat,
    solutions: QuerySolutionIter<'static>,
    rows: &mut i64,
) -> EResult<()> {
    let variables: Arc<[Variable]> = solutions.variables().into();
    let mut serializer = QueryResultsSerializer::from_format(format)
        .serialize_solutions_to_writer(&mut *out, variables.to_vec())?;
    for solution in solutions {
        let solution = solution?;
        serializer.serialize(solution.iter())?;
        *rows += 1;
        host.poll()?;
    }
    serializer.finish()?;
    Ok(())
}

pub fn write_triples(
    host: &Host,
    out: &mut HostWriter,
    format: oxrdfio::RdfFormat,
    triples: QueryTripleIter<'static>,
    rows: &mut i64,
) -> EResult<()> {
    let mut serializer = RdfSerializer::from_format(format).for_writer(&mut *out);
    for triple in triples {
        serializer.serialize_triple(&triple?)?;
        *rows += 1;
        host.poll()?;
    }
    serializer.finish()?;
    Ok(())
}
