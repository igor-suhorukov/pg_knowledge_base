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
    /// Compiled mappings, by name, with what they were compiled for: kept
    /// while their revision, the catalog's version and the search_path stay
    static COMPILED: RefCell<HashMap<String, (Version, Rc<Compiled>)>> = RefCell::new(HashMap::new());
}

/// The enabled mappings -- their names, revisions and bases -- and what a
/// compiled one is for besides: the catalog's version (kb.catalog_version, one
/// more after each DDL statement that may change a table's columns), the
/// search_path that resolves the names of its SQL, kb.r2rml_cache, the
/// database.  The documents only a mapping compiled anew needs.
const MAPPINGS_SQL: &str = "SELECT m.name, m.revision::text, coalesce(m.base_iri, ''), v.version::text, \
     current_setting('search_path'), coalesce(current_setting('kb.r2rml_cache', true), 'on'), d.oid::text \
     FROM kb.mappings m CROSS JOIN kb.catalog_version v JOIN pg_database d ON d.datname = current_database() \
     WHERE m.enabled ORDER BY m.name";

/// What a compiled mapping is for: its revision and base, the catalog's
/// version, the search_path
#[derive(Clone, PartialEq, Eq)]
struct Version {
    revision: i64,
    base_iri: Option<String>,
    catalog: i64,
    search_path: String,
}

/// The directory of a database's cached compiled mappings, under the data
/// directory -- the backend's working directory -- where pg_basebackup copies
/// none of pg_stat_tmp's files
fn cache_dir(database: &str) -> std::path::PathBuf {
    std::path::Path::new("pg_stat_tmp/pg_knowledge_base").join(database)
}

/// A hash of a mapping's name, which its cache files begin with
fn name_hash(name: &str) -> String {
    use siphasher::sip::SipHasher24;
    use std::hash::Hasher;
    let mut h = SipHasher24::new();
    h.write(name.as_bytes());
    format!("{:016x}", h.finish())
}

/// A mapping's cache file: a hash of its name, its revision, the catalog's
/// version, and a hash of its base and the search_path
fn cache_file(name: &str, v: &Version) -> String {
    use siphasher::sip::SipHasher24;
    use std::hash::Hasher;
    let mut k = SipHasher24::new();
    k.write(v.base_iri.as_deref().unwrap_or("").as_bytes());
    k.write_u8(0);
    k.write(v.search_path.as_bytes());
    format!("{}-{}-{}-{:016x}.bin", name_hash(name), v.revision, v.catalog, k.finish())
}

/// A file's name hash, revision and catalog's version (cache_file())
fn file_parts(file: &str) -> Option<(&str, i64, i64)> {
    let mut parts = file.strip_suffix(".bin")?.split('-');
    let name = parts.next()?;
    let revision = parts.next()?.parse().ok()?;
    let catalog = parts.next()?.parse().ok()?;
    Some((name, revision, catalog))
}

/// A mapping compiled for v, from the cache when a file has it
fn read_cached(dir: &std::path::Path, name: &str, v: &Version) -> Option<Compiled> {
    let bytes = std::fs::read(dir.join(cache_file(name, v))).ok()?;
    Compiled::decode(&bytes).filter(|c| c.name == name && c.revision == v.revision)
}

/// A compiled mapping into the cache: written aside and renamed, so that a
/// reader sees a whole file or none, and its files of earlier revisions and
/// versions removed.  The cache is only one: an error writes nothing.
fn write_cached(dir: &std::path::Path, compiled: &Compiled, v: &Version) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let file = cache_file(&compiled.name, v);
    let tmp = dir.join(format!(".{}.{file}", std::process::id()));
    if std::fs::write(&tmp, compiled.encode()).is_err() || std::fs::rename(&tmp, dir.join(&file)).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    let Some((hash, revision, catalog)) = file_parts(&file) else { return };
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let other = entry.file_name();
        let Some(other) = other.to_str() else { continue };
        if let Some((h, r, c)) = file_parts(other) {
            if h == hash && (r < revision || c < catalog) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// The files of mappings no longer enabled, and the leftovers of writes an
/// error broke, an hour old, removed
fn sweep(dir: &std::path::Path, names: &[&str]) {
    let hashes: Vec<String> = names.iter().map(|n| name_hash(n)).collect();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let file = entry.file_name();
        let Some(file) = file.to_str() else { continue };
        let stale = match file_parts(file) {
            Some((hash, ..)) => !hashes.iter().any(|h| h == hash),
            None => {
                file.starts_with('.')
                    && entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age.as_secs() > 3600)
            }
        };
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn compile_row(host: &Host, row: &MappingRow) -> EResult<Compiled> {
    let (graph, base) = parse::parse_document(&row.document, row.base_iri.as_deref())?;
    let mapping = parse::read_mapping(&graph)?;
    compile::compile(host, &row.name, row.revision, &mapping, base.as_deref())
}

/// A mapping's document, for a mapping compiled anew
fn document(host: &Host, name: &str) -> EResult<String> {
    for row in host.sql_open("SELECT document FROM kb.mappings WHERE name = $1", &[name.to_owned()], 0)? {
        if let Some(Some(document)) = row?.into_iter().next() {
            return Ok(document);
        }
    }
    Err(crate::error::EngineError::internal("a mapping kb.mappings no longer has"))
}

/// The enabled mappings, compiled; None when there are none.  A backend
/// keeps them; with kb.r2rml_cache, a mapping it has not is read from the
/// file another backend wrote of it, or compiled and written there.
pub fn virtual_graphs(host: &Host) -> EResult<Option<Rc<VirtualGraphs>>> {
    let mut listed: Vec<(String, Version)> = Vec::new();
    let mut cache: Option<std::path::PathBuf> = None;
    for row in host.sql_open(MAPPINGS_SQL, &[], 0)? {
        let row = row?;
        let get = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
        if listed.is_empty() && get(5) == "on" {
            cache = Some(cache_dir(&get(6)));
        }
        let base_iri = get(2);
        listed.push((
            get(0),
            Version {
                revision: get(1).parse().unwrap_or_default(),
                base_iri: (!base_iri.is_empty()).then_some(base_iri),
                catalog: get(3).parse().unwrap_or_default(),
                search_path: get(4),
            },
        ));
    }
    if listed.is_empty() {
        COMPILED.with(|c| c.borrow_mut().clear());
        return Ok(None);
    }
    let mut mappings = Vec::with_capacity(listed.len());
    let mut written = false;
    for (name, version) in &listed {
        let kept = COMPILED.with(|c| {
            c.borrow()
                .get(name)
                .filter(|(v, _)| v == version)
                .map(|(_, m)| Rc::clone(m))
        });
        let compiled = match kept {
            Some(m) => m,
            None => {
                let read = cache.as_deref().and_then(|dir| read_cached(dir, name, version));
                let m = Rc::new(match read {
                    Some(m) => m,
                    None => {
                        let row = MappingRow {
                            name: name.clone(),
                            document: document(host, name)?,
                            base_iri: version.base_iri.clone(),
                            revision: version.revision,
                        };
                        let m = compile_row(host, &row)?;
                        if let Some(dir) = &cache {
                            write_cached(dir, &m, version);
                            written = true;
                        }
                        m
                    }
                });
                COMPILED.with(|c| c.borrow_mut().insert(name.clone(), (version.clone(), Rc::clone(&m))));
                m
            }
        };
        mappings.push(compiled);
    }
    if written {
        if let Some(dir) = &cache {
            sweep(dir, &listed.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>());
        }
    }
    COMPILED.with(|c| {
        c.borrow_mut()
            .retain(|name, _| listed.iter().any(|(n, _)| n == name))
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
