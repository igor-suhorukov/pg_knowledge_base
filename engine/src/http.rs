// SPDX-License-Identifier: Apache-2.0
//! The SPARQL 1.1 protocol and graph store protocol (docs/design.md §5.3),
//! as oxigraph's server answers them (oxigraph/cli/src/main.rs,
//! handle_request): the same paths -- /query, /update, /sparql, /store --
//! the same parameters, the same content negotiation, the same statuses.
//! The session (src/endpoint/session.c) reads the request, logs its client
//! in and opens the transaction http_prepare() asks for; http_execute()
//! answers it through the host's respond() and write().
//!
//! A port of oxigraph's server code to the session's host: Copyright (c)
//! 2018 Oxigraph developers, MIT OR Apache-2.0, used here under Apache-2.0.

use std::collections::{HashMap, HashSet};

use oxiri::{Iri, IriRef};
use oxrdf::{BlankNode, GraphName, NamedNode, NamedOrBlankNode, Triple};
use oxrdf::vocab::rdf;
use oxrdfio::{RdfFormat, RdfParser, RdfSerializer};
use oxstr::OxString;
use sparesults::QueryResultsFormat;

use crate::error::{EResult, EngineError};
use crate::ffi::*;
use crate::host::Host;
use crate::key;
use crate::load;
use crate::query::{self, DatasetOptions};
use crate::update::{self, UpdateOptions};
use crate::writer::StoreWriter;

/// The largest SPARQL body oxigraph's server takes; data for the graph
/// store is limited by the caller's max_body alone.
const MAX_SPARQL_BODY_SIZE: usize = 1_048_576;

#[derive(Clone)]
struct Request {
    method: String,
    path: String,
    query: String,
    accept: String,
    content_type: Option<String>,
    base_iri: String,
    body: Vec<u8>,
    read_only: bool,
    union_default_graph: bool,
    with_mappings: bool,
}

#[derive(Clone)]
enum Target {
    Default,
    Named(NamedNode),
}

enum Operation {
    Page,
    ServiceDescription { query: bool, update: bool },
    Query {
        query: String,
        options: DatasetOptions,
    },
    Update {
        update: String,
        options: DatasetOptions,
    },
    StoreGet(Option<Target>),
    StoreHead(Option<Target>),
    StorePut(Option<Target>, RdfFormat),
    StorePost(Option<Target>, RdfFormat),
    StoreDelete(Option<Target>),
}

pub struct KbHttp {
    request: Request,
    operation: Operation,
    activity: String,
    lenient: bool,
    /// A query evaluated with spareval's statistics, for a trace
    explain: bool,
    /// What the request ran, once it has: for kb_http_trace()
    trace: Option<query::Trace>,
}

fn bad_request(message: impl Into<String>) -> EngineError {
    EngineError::invalid(message).with_http(400)
}

fn unsupported_media_type(content_type: &str) -> EngineError {
    EngineError::invalid(format!("No supported content Content-Type given: {content_type}"))
        .with_http(415)
}

fn read_only() -> EngineError {
    EngineError::invalid("The server is read-only").with_http(403)
}

fn not_found(message: impl Into<String>) -> EngineError {
    EngineError::not_found(message).with_http(404)
}

struct RequestParams {
    params: HashMap<String, Vec<String>>,
}

impl RequestParams {
    fn parse(data: &[u8]) -> Self {
        let mut params: HashMap<String, Vec<String>> = HashMap::new();
        for (k, v) in form_urlencoded::parse(data) {
            params.entry(k.into_owned()).or_default().push(v.into_owned());
        }
        Self { params }
    }

    fn from_url(request: &Request) -> Self {
        Self::parse(request.query.as_bytes())
    }

    fn from_url_and_body(request: &Request) -> EResult<Self> {
        let mut params = Self::from_url(request).params;
        let existing: HashSet<String> = params.keys().cloned().collect();
        if request.body.len() > MAX_SPARQL_BODY_SIZE {
            return Err(bad_request(format!(
                "SPARQL body payloads are limited to {MAX_SPARQL_BODY_SIZE} bytes"
            )));
        }
        for (k, v) in form_urlencoded::parse(&request.body) {
            if existing.contains(k.as_ref()) {
                return Err(bad_request(format!(
                    "'{k}' cannot be set both in the body and the URL query"
                )));
            }
            params.entry(k.into_owned()).or_default().push(v.into_owned());
        }
        Ok(Self { params })
    }

    fn contains(&self, key: &str) -> bool {
        self.params.contains_key(key)
    }

    fn remove_single(&mut self, key: &str) -> EResult<Option<String>> {
        let values = self.remove_all(key);
        if values.len() > 1 {
            return Err(bad_request(format!(
                "The URL query parameter {key} must be set at most once"
            )));
        }
        Ok(values.into_iter().next())
    }

    fn remove_required_exactly_once(&mut self, key: &str) -> EResult<String> {
        self.remove_single(key)?
            .ok_or_else(|| bad_request(format!("The URL query parameter {key} must be set")))
    }

    fn remove_all(&mut self, key: &str) -> Vec<String> {
        self.params.remove(key).unwrap_or_default()
    }
}

fn string_body(request: &Request) -> EResult<String> {
    if request.body.len() > MAX_SPARQL_BODY_SIZE {
        return Err(bad_request(format!(
            "SPARQL body payloads are limited to {MAX_SPARQL_BODY_SIZE} bytes, found {} bytes",
            request.body.len()
        )));
    }
    String::from_utf8(request.body.clone())
        .map_err(|e| bad_request(format!("Invalid UTF-8 body: {e}")))
}

fn content_type(request: &Request) -> EResult<String> {
    request
        .content_type
        .clone()
        .ok_or_else(|| bad_request("No Content-Type given"))
}

fn query_operation(
    request: &Request,
    mut args: RequestParams,
    body: Option<String>,
) -> EResult<Operation> {
    let default_graphs = args.remove_all("default-graph-uri");
    let named_graphs = args.remove_all("named-graph-uri");
    let mut union = args.contains("union-default-graph");
    if default_graphs.is_empty() && named_graphs.is_empty() {
        union |= request.union_default_graph;
    } else if union {
        return Err(bad_request(
            "default-graph-uri or named-graph-uri and union-default-graph should not be set at the same time",
        ));
    }
    let query = if let Some(body) = body {
        if args.contains("query") {
            return Err(bad_request(
                "The query cannot be set both in the URL query parameters and the request body",
            ));
        }
        body
    } else {
        args.remove_required_exactly_once("query")?
    };
    Ok(Operation::Query {
        query,
        options: DatasetOptions {
            default_graphs,
            named_graphs,
            union_default_graph: union,
            with_mappings: request.with_mappings,
        },
    })
}

fn update_operation(
    request: &Request,
    mut args: RequestParams,
    body: Option<String>,
) -> EResult<Operation> {
    if request.read_only {
        return Err(read_only());
    }
    let default_graphs = args.remove_all("using-graph-uri");
    let named_graphs = args.remove_all("using-named-graph-uri");
    let mut union = args.contains("using-union-graph");
    if default_graphs.is_empty() && named_graphs.is_empty() {
        union |= request.union_default_graph;
    } else if union {
        return Err(bad_request(
            "using-graph-uri or using-named-graph-uri and using-union-graph should not be set at the same time",
        ));
    }
    let update = if let Some(body) = body {
        if args.contains("update") {
            return Err(bad_request(
                "The update cannot be set both in the URL update parameters and the request body",
            ));
        }
        body
    } else {
        args.remove_required_exactly_once("update")?
    };
    Ok(Operation::Update {
        update,
        options: DatasetOptions {
            default_graphs,
            named_graphs,
            union_default_graph: union,
            with_mappings: request.with_mappings,
        },
    })
}

fn resolve_with_base(request: &Request, url: &str) -> EResult<NamedNode> {
    let iri = IriRef::parse(url).map_err(|e| bad_request(e.to_string()))?;
    Ok(if iri.is_absolute() {
        NamedNode::new_unchecked(OxString::new_owned(iri.into_inner()))
    } else {
        NamedNode::new_unchecked(
            Iri::parse(request.base_iri.as_str())
                .map_err(|e| bad_request(e.to_string()))?
                .resolve(&iri)
                .map_err(|e| bad_request(e.to_string()))?
                .into_inner(),
        )
    })
}

fn store_target(request: &Request) -> EResult<Option<Target>> {
    if request.path == "/store" {
        let mut params = RequestParams::from_url(request);
        if let Some(graph) = params.remove_single("graph")? {
            if params.contains("default") {
                Err(bad_request(
                    "Both graph and default parameters should not be set at the same time",
                ))
            } else {
                Ok(Some(Target::Named(resolve_with_base(request, &graph)?)))
            }
        } else if params.contains("default") {
            Ok(Some(Target::Default))
        } else {
            Ok(None)
        }
    } else {
        Ok(Some(Target::Named(resolve_with_base(request, "")?)))
    }
}

fn rdf_body_format(request: &Request) -> EResult<RdfFormat> {
    let content_type = content_type(request)?;
    RdfFormat::from_media_type(&content_type).ok_or_else(|| unsupported_media_type(&content_type))
}

fn operation(request: &Request) -> EResult<Operation> {
    let path = request.path.as_str();
    let method = request.method.as_str();
    match (path, method) {
        ("/", "GET" | "HEAD") => Ok(Operation::Page),
        ("/query", "GET") => {
            if request.query.is_empty() {
                Ok(Operation::ServiceDescription {
                    query: true,
                    update: false,
                })
            } else {
                query_operation(request, RequestParams::from_url(request), None)
            }
        }
        ("/query", "POST" | "QUERY") => {
            let content_type = content_type(request)?;
            if content_type == "application/sparql-query" {
                query_operation(
                    request,
                    RequestParams::from_url(request),
                    Some(string_body(request)?),
                )
            } else if content_type == "application/x-www-form-urlencoded" {
                query_operation(request, RequestParams::from_url_and_body(request)?, None)
            } else {
                Err(unsupported_media_type(&content_type))
            }
        }
        ("/update", "GET") => {
            if request.read_only {
                return Err(read_only());
            }
            Ok(Operation::ServiceDescription {
                query: false,
                update: true,
            })
        }
        ("/update", "POST") => {
            if request.read_only {
                return Err(read_only());
            }
            let content_type = content_type(request)?;
            if content_type == "application/sparql-update" {
                update_operation(
                    request,
                    RequestParams::from_url(request),
                    Some(string_body(request)?),
                )
            } else if content_type == "application/x-www-form-urlencoded" {
                update_operation(request, RequestParams::from_url_and_body(request)?, None)
            } else {
                Err(unsupported_media_type(&content_type))
            }
        }
        ("/sparql", "GET") => {
            if request.query.is_empty() {
                Ok(Operation::ServiceDescription {
                    query: true,
                    update: !request.read_only,
                })
            } else {
                query_operation(request, RequestParams::from_url(request), None)
            }
        }
        ("/sparql", "POST" | "QUERY") => {
            let is_query = method == "QUERY";
            let content_type = content_type(request)?;
            if content_type == "application/sparql-query" {
                query_operation(
                    request,
                    RequestParams::from_url(request),
                    Some(string_body(request)?),
                )
            } else if content_type == "application/sparql-update" && !is_query {
                update_operation(
                    request,
                    RequestParams::from_url(request),
                    Some(string_body(request)?),
                )
            } else if content_type == "application/x-www-form-urlencoded" {
                let args = RequestParams::from_url_and_body(request)?;
                match (args.contains("query"), args.contains("update")) {
                    (true, true) => Err(bad_request(
                        "Both 'query' and 'update' cannot be set at the same time",
                    )),
                    (true, false) => query_operation(request, args, None),
                    (false, true) => {
                        if is_query {
                            return Err(bad_request(
                                "SPARQL updates are not compatible with the QUERY HTTP method",
                            ));
                        }
                        update_operation(request, args, None)
                    }
                    (false, false) => Err(bad_request(
                        "'query' or 'update' must be set to define the SPARQL operation to execute",
                    )),
                }
            } else {
                Err(unsupported_media_type(&content_type))
            }
        }
        (path, "GET") if path.starts_with("/store") => Ok(Operation::StoreGet(store_target(request)?)),
        (path, "HEAD") if path.starts_with("/store") => {
            Ok(Operation::StoreHead(store_target(request)?))
        }
        (path, "PUT") if path.starts_with("/store") => {
            if request.read_only {
                return Err(read_only());
            }
            Ok(Operation::StorePut(store_target(request)?, rdf_body_format(request)?))
        }
        (path, "POST") if path.starts_with("/store") => {
            if request.read_only {
                return Err(read_only());
            }
            Ok(Operation::StorePost(store_target(request)?, rdf_body_format(request)?))
        }
        (path, "DELETE") if path.starts_with("/store") => {
            if request.read_only {
                return Err(read_only());
            }
            Ok(Operation::StoreDelete(store_target(request)?))
        }
        _ => Err(not_found(format!(
            "{method} {path} is not supported by this server"
        ))),
    }
}

// ---------------------------------------------------------------------
// Content negotiation, as oxigraph's server does it
// ---------------------------------------------------------------------

fn rdf_content_negotiation(accept: &str) -> EResult<RdfFormat> {
    content_negotiation(
        accept,
        RdfFormat::from_media_type,
        RdfFormat::NQuads,
        &[
            RdfFormat::NQuads,
            RdfFormat::TriG,
            RdfFormat::JsonLd {
                profile: oxrdfio::JsonLdProfileSet::empty(),
            },
            RdfFormat::Turtle,
            RdfFormat::NTriples,
            RdfFormat::RdfXml,
            RdfFormat::N3,
        ],
        RdfFormat::media_type,
        "application/n-quads or text/turtle",
    )
}

fn query_results_content_negotiation(accept: &str) -> EResult<QueryResultsFormat> {
    content_negotiation(
        accept,
        |media_type| {
            let format = QueryResultsFormat::from_media_type(media_type)?;
            for parameter in media_type.split(';').skip(1) {
                if let Some((name, value)) = parameter.split_once('=') {
                    if name.trim().eq_ignore_ascii_case("charset")
                        && !value.trim().trim_matches('"').eq_ignore_ascii_case("utf-8")
                    {
                        return None;
                    }
                }
            }
            Some(format)
        },
        QueryResultsFormat::Json,
        &[
            QueryResultsFormat::Json,
            QueryResultsFormat::Xml,
            QueryResultsFormat::Csv,
            QueryResultsFormat::Tsv,
        ],
        QueryResultsFormat::media_type,
        "application/sparql-results+json or text/tsv",
    )
}

fn content_negotiation<F: Copy + Eq>(
    header: &str,
    parse: impl Fn(&str) -> Option<F>,
    default: F,
    supported: &[F],
    media_type: impl Fn(F) -> &'static str,
    example: &str,
) -> EResult<F> {
    if header.is_empty() {
        return Ok(default);
    }
    let mut candidates: Vec<(F, ((i32, usize), f32))> = supported
        .iter()
        .map(|format| (*format, ((0, 0), 0_f32)))
        .collect();
    for mut possible in header.split(',') {
        let mut score = 1.;
        if let Some((possible_type, last_parameter)) = possible.rsplit_once(';') {
            if let Some((name, value)) = last_parameter.split_once('=') {
                if name.trim().eq_ignore_ascii_case("q") {
                    score = value.trim().parse::<f32>().map_err(|_| {
                        bad_request(format!("Invalid Accept media type score: {value}"))
                    })?;
                    possible = possible_type;
                }
            }
        }
        let (possible_base, possible_sub) = possible
            .split_once(';')
            .unwrap_or((possible, ""))
            .0
            .split_once('/')
            .ok_or_else(|| bad_request(format!("Invalid media type: '{possible}'")))?;
        let possible_base = possible_base.trim();
        let possible_sub = possible_sub.trim();

        let (format, specificity) = if possible_base == "*" && possible_sub == "*" {
            (None, 0)
        } else if possible_sub == "*" {
            (None, 1)
        } else {
            (parse(possible), 2)
        };
        if let Some(format) = format {
            if !candidates.iter().any(|(candidate, _)| *candidate == format) {
                candidates.push((format, ((0, 0), 0.)));
            }
        }
        for (candidate, candidate_score) in &mut candidates {
            let candidate_media_type = media_type(*candidate);
            let candidate_base = candidate_media_type.split_once('/').map_or("", |s| s.0);
            if format == Some(*candidate)
                || (possible_sub == "*" && (possible_base == "*" || possible_base == candidate_base))
            {
                if let Some(parameter_count) =
                    matching_media_type_parameters(possible, candidate_media_type)
                {
                    let score = ((specificity, parameter_count), score);
                    if score > *candidate_score {
                        *candidate_score = score;
                    }
                }
            }
        }
    }

    let mut result = None;
    let mut result_score = 0_f32;
    for (format, (_, score)) in candidates {
        if score > result_score {
            result = Some(format);
            result_score = score;
        }
    }
    result.ok_or_else(|| {
        EngineError::invalid(format!(
            "The accept header does not provide any accepted format like {example}"
        ))
        .with_http(406)
    })
}

fn matching_media_type_parameters(media_range: &str, candidate: &str) -> Option<usize> {
    let mut count = 0;
    for parameter in media_range.split(';').skip(1) {
        let (name, value) = parameter.split_once('=')?;
        let name = name.trim();
        let value = value.trim().trim_matches('"');
        let candidate_value = candidate.split(';').skip(1).find_map(|parameter| {
            let (candidate_name, candidate_value) = parameter.split_once('=')?;
            candidate_name
                .trim()
                .eq_ignore_ascii_case(name)
                .then_some(candidate_value.trim().trim_matches('"'))
        });
        let matches = if let Some(candidate_value) = candidate_value {
            if name.eq_ignore_ascii_case("charset") {
                value.eq_ignore_ascii_case(candidate_value)
            } else {
                value == candidate_value
            }
        } else if name.eq_ignore_ascii_case("charset") {
            ["ascii", "utf8", "utf-8"]
                .iter()
                .any(|candidate| value.eq_ignore_ascii_case(candidate))
        } else {
            false
        };
        if !matches {
            return None;
        }
        count += 1;
    }
    Some(count)
}

// ---------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------

const SD: &str = "http://www.w3.org/ns/sparql-service-description#";

fn sd(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{SD}{local}"))
}

fn service_description(
    format: RdfFormat,
    query: bool,
    update: bool,
    union_default_graph: bool,
    endpoint: &str,
) -> EResult<Vec<u8>> {
    let root = BlankNode::default();
    let mut graph = vec![Triple::new(root.clone(), rdf::TYPE, sd("Service"))];
    graph.push(Triple::new(
        root.clone(),
        sd("endpoint"),
        NamedNode::new_unchecked(match format {
            RdfFormat::Turtle | RdfFormat::TriG | RdfFormat::N3 | RdfFormat::JsonLd { .. } | RdfFormat::RdfXml => {
                String::new()
            }
            _ => endpoint.to_owned(),
        }),
    ));
    if query {
        graph.push(Triple::new(root.clone(), sd("supportedLanguage"), sd("SPARQL10Query")));
        graph.push(Triple::new(root.clone(), sd("supportedLanguage"), sd("SPARQL11Query")));
    }
    if update {
        graph.push(Triple::new(root.clone(), sd("supportedLanguage"), sd("SPARQL11Update")));
    }
    if query {
        for format in [
            QueryResultsFormat::Json,
            QueryResultsFormat::Xml,
            QueryResultsFormat::Csv,
            QueryResultsFormat::Tsv,
        ] {
            graph.push(Triple::new(
                root.clone(),
                sd("resultFormat"),
                NamedNode::new_unchecked(format.iri()),
            ));
        }
        for format in [
            RdfFormat::NTriples,
            RdfFormat::NQuads,
            RdfFormat::Turtle,
            RdfFormat::TriG,
            RdfFormat::N3,
            RdfFormat::RdfXml,
        ] {
            graph.push(Triple::new(
                root.clone(),
                sd("resultFormat"),
                NamedNode::new_unchecked(format.iri()),
            ));
        }
    }
    graph.push(Triple::new(root.clone(), sd("feature"), sd("EmptyGraphs")));
    if union_default_graph {
        graph.push(Triple::new(root.clone(), sd("feature"), sd("UnionDefaultGraph")));
    }
    graph.push(Triple::new(
        root,
        sd("defaultEntailmentRegime"),
        NamedNode::new_unchecked("http://www.w3.org/ns/entailment/Simple"),
    ));
    let mut serializer = RdfSerializer::from_format(format)
        .with_prefix("sd", SD)
        .map_err(|e| EngineError::internal(e.to_string()))?
        .for_writer(Vec::new());
    for t in &graph {
        serializer.serialize_triple(t)?;
    }
    Ok(serializer.finish()?)
}

const PAGE: &str = include_str!("page.html");

fn graph_exists(host: &Host, target: &Target) -> EResult<bool> {
    Ok(match target {
        Target::Default => true,
        Target::Named(g) => host.graph_exists(&key::iri_key(g.as_str()))?,
    })
}

fn assert_graph_exists(host: &Host, target: &Target) -> EResult<()> {
    if graph_exists(host, target)? {
        Ok(())
    } else {
        Err(not_found(format!(
            "The graph {} does not exists",
            match target {
                Target::Default => GraphName::DefaultGraph,
                Target::Named(g) => g.clone().into(),
            }
        )))
    }
}

fn target_graph(target: &Target) -> GraphName {
    match target {
        Target::Default => GraphName::DefaultGraph,
        Target::Named(g) => g.clone().into(),
    }
}

/// The request's body into the store, as oxigraph's web_load_graph() and
/// web_load_dataset() load it.
fn load_body(
    host: Host,
    request: &Request,
    format: RdfFormat,
    graph: Option<&GraphName>,
    lenient: bool,
) -> EResult<()> {
    let mut parser = RdfParser::from_format(format);
    if let Some(graph) = graph {
        parser = parser.without_named_graphs().with_default_graph(graph.clone());
        if let GraphName::NamedNode(g) = graph {
            parser = parser
                .with_base_iri(g.as_str())
                .map_err(|e| bad_request(e.to_string()))?;
        }
    }
    if lenient {
        parser = parser.lenient();
    }
    let mut writer = StoreWriter::new(host, 8192);
    let mut renames: HashMap<BlankNode, BlankNode> = HashMap::new();
    for quad in parser.for_slice(&request.body) {
        let quad = quad.map_err(|e| bad_request(e.to_string()))?;
        let mut rename = |b: BlankNode| -> BlankNode {
            renames.entry(b).or_insert_with(key::fresh_blank_node).clone()
        };
        let quad = oxrdf::Quad {
            subject: match quad.subject {
                NamedOrBlankNode::BlankNode(b) => rename(b).into(),
                s => s,
            },
            predicate: quad.predicate,
            object: match quad.object {
                oxrdf::Term::BlankNode(b) => rename(b).into(),
                o => o,
            },
            graph_name: match quad.graph_name {
                GraphName::BlankNode(b) => rename(b).into(),
                g => g,
            },
        };
        writer.insert(quad.as_ref())?;
    }
    writer.flush()
}

impl KbHttp {
    fn execute(&mut self, host: Host) -> EResult<()> {
        let request = &self.request;
        match &self.operation {
            Operation::Page => {
                host.respond(200, "text/html; charset=utf-8", "")?;
                if request.method != "HEAD" {
                    host.write(PAGE.as_bytes())?;
                }
                Ok(())
            }
            Operation::ServiceDescription { query, update } => {
                let format = rdf_content_negotiation(&request.accept)?;
                let endpoint = format!("{}", request.base_iri);
                let body = service_description(
                    format,
                    *query,
                    *update,
                    request.union_default_graph,
                    &endpoint,
                )?;
                host.respond(200, format.media_type(), "")?;
                host.write(&body)
            }
            Operation::Query { query, options } => {
                let mut q =
                    query::KbQuery::open(host, query, Some(&request.base_iri), options, self.explain)?;
                let written = match q.kind() {
                    KB_QUERY_SELECT | KB_QUERY_ASK => query_results_content_negotiation(&request.accept)
                        .and_then(|format| {
                            host.respond(200, format.media_type(), "")?;
                            q.write(format.media_type())
                        }),
                    _ => rdf_content_negotiation(&request.accept).and_then(|format| {
                        host.respond(200, format.media_type(), "")?;
                        q.write(format.media_type())
                    }),
                };
                self.trace = Some(q.trace());
                written
            }
            Operation::Update { update, options } => {
                self.trace = Some(query::Trace {
                    operation: "UPDATE",
                    ..Default::default()
                });
                update::update(
                    host,
                    update,
                    &UpdateOptions {
                        base_iri: Some(request.base_iri.clone()),
                        using: options.clone(),
                        allow_load: false,
                    },
                )?;
                host.respond(204, "", "")
            }
            Operation::StoreGet(target) => {
                let format = rdf_content_negotiation(&request.accept)?;
                match target {
                    Some(target) => {
                        assert_graph_exists(&host, target)?;
                        host.respond(200, format.media_type(), "")?;
                        let graph = match target {
                            Target::Default => load::DumpGraph::Default,
                            Target::Named(g) => load::DumpGraph::Named(g.clone()),
                        };
                        load::dump(host, format, &graph, request.with_mappings)
                    }
                    None => {
                        if !format.supports_datasets() {
                            return Err(bad_request(format!(
                                "It is not possible to serialize the full RDF dataset using {format} that does not support named graphs"
                            )));
                        }
                        host.respond(200, format.media_type(), "")?;
                        load::dump(host, format, &load::DumpGraph::All, request.with_mappings)
                    }
                }
            }
            Operation::StoreHead(target) => {
                if let Some(target) = target {
                    assert_graph_exists(&host, target)?;
                }
                host.respond(200, "", "")
            }
            Operation::StorePut(target, format) => match target {
                Some(target) => {
                    let new = match target {
                        Target::Named(g) => {
                            let k = key::iri_key(g.as_str());
                            if host.graph_exists(&k)? {
                                host.graph_clear(KB_CLEAR_GRAPH, Some(&k))?;
                                false
                            } else {
                                let mut w = StoreWriter::new(host, 16);
                                w.insert_graph(g.as_ref().into());
                                w.flush()?;
                                true
                            }
                        }
                        Target::Default => {
                            host.graph_clear(KB_CLEAR_GRAPH, Some(&key::DEFAULT_GRAPH))?;
                            false
                        }
                    };
                    load_body(host, request, *format, Some(&target_graph(target)), self.lenient)?;
                    host.respond(if new { 201 } else { 204 }, "", "")
                }
                None => {
                    host.graph_clear(KB_CLEAR_ALL, None)?;
                    load_body(host, request, *format, None, self.lenient)?;
                    host.respond(204, "", "")
                }
            },
            Operation::StoreDelete(target) => {
                match target {
                    Some(Target::Default) => {
                        host.graph_clear(KB_CLEAR_GRAPH, Some(&key::DEFAULT_GRAPH))?
                    }
                    Some(Target::Named(g)) => {
                        let k = key::iri_key(g.as_str());
                        if host.graph_exists(&k)? {
                            host.graph_clear(KB_CLEAR_DROP, Some(&k))?;
                        } else {
                            return Err(not_found(format!("The graph {g} does not exists")));
                        }
                    }
                    None => host.graph_clear(KB_CLEAR_ALL, None)?,
                }
                host.respond(204, "", "")
            }
            Operation::StorePost(target, format) => match target {
                Some(target) => {
                    let new = !graph_exists(&host, target)?;
                    if let (true, Target::Named(g)) = (new, target) {
                        let mut w = StoreWriter::new(host, 16);
                        w.insert_graph(g.as_ref().into());
                        w.flush()?;
                    }
                    load_body(host, request, *format, Some(&target_graph(target)), self.lenient)?;
                    host.respond(if new { 201 } else { 204 }, "", "")
                }
                None => {
                    if format.supports_datasets() {
                        load_body(host, request, *format, None, self.lenient)?;
                        host.respond(204, "", "")
                    } else {
                        // relative to the request's own URL, so that a
                        // database's prefix (/<database>/store) is kept
                        let graph = resolve_with_base(
                            request,
                            &format!("store/{:x}", rand::random::<u128>()),
                        )?;
                        let mut w = StoreWriter::new(host, 16);
                        w.insert_graph(graph.as_ref().into());
                        w.flush()?;
                        load_body(
                            host,
                            request,
                            *format,
                            Some(&graph.clone().into()),
                            self.lenient,
                        )?;
                        host.respond(201, "", &format!("Location: {}\r\n", graph.as_str()))
                    }
                }
            },
        }
    }
}

fn request_from(raw: &KbHttpRequest) -> EResult<Request> {
    let content_type = unsafe { raw.content_type.as_str() }?;
    Ok(Request {
        method: unsafe { raw.method.as_str() }?.to_owned(),
        path: unsafe { raw.path.as_str() }?.to_owned(),
        query: unsafe { raw.query.as_str() }?.to_owned(),
        accept: unsafe { raw.accept.as_str() }?.to_owned(),
        content_type: if content_type.is_empty() {
            None
        } else {
            Some(
                content_type
                    .split_once(';')
                    .map_or(content_type, |(b, _)| b)
                    .trim()
                    .to_ascii_lowercase(),
            )
        },
        base_iri: unsafe { raw.base_iri.as_str() }?.to_owned(),
        body: unsafe { raw.body.as_bytes() }.to_vec(),
        read_only: raw.read_only,
        union_default_graph: raw.union_default_graph,
        with_mappings: raw.with_mappings,
    })
}

pub unsafe extern "C" fn kb_http_prepare(
    raw: *const KbHttpRequest,
    out: *mut *mut KbHttp,
    info: *mut KbHttpInfo,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if raw.is_null() || out.is_null() || info.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            *out = std::ptr::null_mut();
            let raw = &*raw;
            if (raw.size as usize) < std::mem::size_of::<KbHttpRequest>() {
                return Err(EngineError::invalid("the request is from an older ABI"));
            }
            let request = request_from(raw)?;
            let operation = operation(&request)?;
            let (kind, activity) = match &operation {
                Operation::Page | Operation::ServiceDescription { .. } => (KB_HTTP_STATIC, String::new()),
                Operation::Query { query, .. } => (KB_HTTP_READ, query.clone()),
                Operation::Update { update, .. } => (KB_HTTP_WRITE, update.clone()),
                Operation::StoreGet(_) | Operation::StoreHead(_) => (
                    KB_HTTP_READ,
                    format!("{} {}", request.method, request.path),
                ),
                Operation::StorePut(..) | Operation::StorePost(..) | Operation::StoreDelete(_) => (
                    KB_HTTP_WRITE,
                    format!("{} {}?{}", request.method, request.path, request.query),
                ),
            };
            let lenient = RequestParams::from_url(&request).contains("lenient");
            let http = Box::new(KbHttp {
                request,
                operation,
                activity,
                lenient,
                explain: false,
                trace: None,
            });
            (*info).kind = kind;
            (*info).activity = KbStr::from_str(&http.activity);
            *out = Box::into_raw(http);
            Ok(())
        })
    }
}

pub unsafe extern "C" fn kb_http_execute(
    host: *const KbHost,
    http: *mut KbHttp,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if http.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            let host = Host::new(host)?;
            (*http).execute(host)
        })
    }
}

/// A query of the request evaluated with spareval's statistics, for its plan
/// in a trace; before http_execute().
pub unsafe extern "C" fn kb_http_explain(http: *mut KbHttp, on: bool) {
    if !http.is_null() {
        unsafe { (*http).explain = on };
    }
}

/// What the request ran, for a trace (src/trace.c): a query's operation,
/// rows and plan, an update's operation; nothing for a page, a service
/// description or the graph store.
pub unsafe extern "C" fn kb_http_trace(
    http: *mut KbHttp,
    max_bytes: i32,
    raw: bool,
    out: *mut KbTrace,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if http.is_null() || out.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            crate::trace::fill(&mut *out, (*http).trace.as_ref(), max_bytes, raw)
        })
    }
}

pub unsafe extern "C" fn kb_http_close(http: *mut KbHttp) {
    if !http.is_null() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(unsafe { Box::from_raw(http) });
        }));
    }
}
