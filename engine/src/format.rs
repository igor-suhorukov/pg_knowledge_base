// SPDX-License-Identifier: Apache-2.0
//! Formats by name, file extension or media type: SPARQL's results formats
//! for SELECT and ASK, RDF's for graphs and datasets.

use oxrdfio::RdfFormat;
use sparesults::QueryResultsFormat;

use crate::error::{EResult, EngineError};

pub fn results_format(name: &str) -> Option<QueryResultsFormat> {
    let name = name.trim();
    match name.to_ascii_lowercase().as_str() {
        "json" | "srj" | "sparql-results+json" => return Some(QueryResultsFormat::Json),
        "xml" | "srx" | "sparql-results+xml" => return Some(QueryResultsFormat::Xml),
        "csv" => return Some(QueryResultsFormat::Csv),
        "tsv" => return Some(QueryResultsFormat::Tsv),
        _ => (),
    }
    if name.contains('/') {
        QueryResultsFormat::from_media_type(name)
    } else {
        QueryResultsFormat::from_extension(name)
    }
}

pub fn rdf_format(name: &str) -> Option<RdfFormat> {
    let name = name.trim();
    match name.to_ascii_lowercase().replace(['-', '_', '/'], "").as_str() {
        "turtle" | "ttl" => return Some(RdfFormat::Turtle),
        "ntriples" | "nt" => return Some(RdfFormat::NTriples),
        "nquads" | "nq" => return Some(RdfFormat::NQuads),
        "trig" => return Some(RdfFormat::TriG),
        "rdfxml" | "rdf" | "xml" => return Some(RdfFormat::RdfXml),
        "n3" => return Some(RdfFormat::N3),
        "jsonld" | "json" => {
            return Some(RdfFormat::JsonLd {
                profile: oxjsonld_profile(),
            });
        }
        _ => (),
    }
    if name.contains('/') {
        RdfFormat::from_media_type(name)
    } else {
        RdfFormat::from_extension(name)
    }
}

fn oxjsonld_profile() -> oxrdfio::JsonLdProfileSet {
    oxrdfio::JsonLdProfileSet::empty()
}

pub fn require_results_format(name: &str) -> EResult<QueryResultsFormat> {
    results_format(name).ok_or_else(|| {
        EngineError::invalid(format!(
            "unknown query results format \"{name}\": json, xml, csv, tsv, or a media type"
        ))
    })
}

pub fn require_rdf_format(name: &str) -> EResult<RdfFormat> {
    rdf_format(name).ok_or_else(|| {
        EngineError::invalid(format!(
            "unknown RDF format \"{name}\": turtle, ntriples, nquads, trig, rdfxml, jsonld, n3, or a media type"
        ))
    })
}
