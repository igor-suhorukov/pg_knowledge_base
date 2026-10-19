// SPDX-License-Identifier: Apache-2.0
//! libkb_engine.so: oxigraph's SPARQL stack behind the C ABI of
//! include/kb_engine.h (docs/design.md §2.2).  The one exported symbol is
//! kb_engine_api(), the table of entry points; every entry point catches
//! what the engine returns or panics with and gives it back as a status.

mod alloc;
mod dataset;
mod error;
mod ffi;
mod format;
mod host;
mod http;
mod key;
mod load;
mod query;
mod r2rml;
mod terms;
mod trace;
mod update;
mod writer;

use std::ffi::{CString, c_char};
use std::sync::LazyLock;

use oxrdf::{GraphName, NamedNode, Term};

use crate::error::EngineError;
use crate::ffi::*;
use crate::host::{Host, MappingRow};
use crate::query::{DatasetOptions, KbQuery};

const ABI_VERSION: u32 = 1;

static LIBRARY_VERSION: LazyLock<CString> = LazyLock::new(|| {
    CString::new(format!(
        "kb_engine {} (oxigraph {})",
        env!("CARGO_PKG_VERSION"),
        OXIGRAPH_VERSION
    ))
    .unwrap_or_default()
});

const OXIGRAPH_VERSION: &str = "0.6.0-dev";

#[repr(C)]
pub struct KbApi {
    pub size: u32,
    pub abi_version: u32,
    pub version: unsafe extern "C" fn() -> *const c_char,
    pub free_string: unsafe extern "C" fn(*mut c_char),
    pub configure: unsafe extern "C" fn(*const KbConfig),
    pub memory_used: unsafe extern "C" fn() -> i64,
    pub query_open: unsafe extern "C" fn(
        *const KbHost,
        *const KbQuerySpec,
        *mut *mut KbQuery,
        *mut KbError,
    ) -> i32,
    pub query_kind: unsafe extern "C" fn(*mut KbQuery) -> i32,
    pub query_variables: unsafe extern "C" fn(*mut KbQuery, *mut i32, *mut *const KbStr) -> i32,
    pub query_next:
        unsafe extern "C" fn(*mut KbQuery, *mut *const KbTerm, *mut bool, *mut KbError) -> i32,
    pub query_write: unsafe extern "C" fn(*mut KbQuery, *const c_char, *mut KbError) -> i32,
    pub query_explain: unsafe extern "C" fn(*mut KbQuery, *mut *mut c_char, *mut KbError) -> i32,
    pub query_close: unsafe extern "C" fn(*mut KbQuery),
    pub update: unsafe extern "C" fn(*const KbHost, *const KbUpdateSpec, *mut KbError) -> i32,
    pub load:
        unsafe extern "C" fn(*const KbHost, *const KbLoadSpec, *mut i64, *mut KbError) -> i32,
    pub dump: unsafe extern "C" fn(*const KbHost, *const KbDumpSpec, *mut KbError) -> i32,
    pub term_text: unsafe extern "C" fn(
        *const KbHost,
        *const KbKey,
        *mut *mut c_char,
        *mut KbError,
    ) -> i32,
    pub term_parse: unsafe extern "C" fn(
        *const c_char,
        *mut KbKey,
        *mut KbTerm,
        *mut bool,
        *mut KbError,
    ) -> i32,
    pub mapping_check: unsafe extern "C" fn(
        *const KbHost,
        *const KbMappingRow,
        *mut *mut c_char,
        *mut KbError,
    ) -> i32,
    pub direct_mapping: unsafe extern "C" fn(
        *const c_char,
        *const c_char,
        *mut *mut c_char,
        *mut KbError,
    ) -> i32,
    pub http_prepare: unsafe extern "C" fn(
        *const KbHttpRequest,
        *mut *mut http::KbHttp,
        *mut KbHttpInfo,
        *mut KbError,
    ) -> i32,
    pub http_execute: unsafe extern "C" fn(*const KbHost, *mut http::KbHttp, *mut KbError) -> i32,
    pub http_close: unsafe extern "C" fn(*mut http::KbHttp),
    pub query_trace: unsafe extern "C" fn(*mut KbQuery, i32, bool, *mut KbTrace, *mut KbError) -> i32,
    pub http_explain: unsafe extern "C" fn(*mut http::KbHttp, bool),
    pub http_trace:
        unsafe extern "C" fn(*mut http::KbHttp, i32, bool, *mut KbTrace, *mut KbError) -> i32,
    pub text_trace: unsafe extern "C" fn(KbStr, i32, bool, *mut *mut c_char, *mut KbError) -> i32,
    pub memory_peak: unsafe extern "C" fn(bool) -> i64,
}

unsafe extern "C" fn kb_version() -> *const c_char {
    LIBRARY_VERSION.as_ptr()
}

unsafe extern "C" fn kb_configure(config: *const KbConfig) {
    if config.is_null() {
        return;
    }
    let config = unsafe { &*config };
    if (config.size as usize) < std::mem::size_of::<KbConfig>() {
        return;
    }
    alloc::set_limit(config.memory_limit.max(0) as usize);
    host::set_interrupt_flag(config.interrupt_pending);
    if config.term_cache_entries > 0 {
        terms::set_cache_capacity(config.term_cache_entries as usize);
    }
}

unsafe extern "C" fn kb_memory_used() -> i64 {
    alloc::used() as i64
}

unsafe extern "C" fn kb_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

fn spec_options(
    ndefault: i32,
    defaults: *const KbStr,
    nnamed: i32,
    named: *const KbStr,
    union_default_graph: bool,
    with_mappings: bool,
) -> Result<DatasetOptions, EngineError> {
    Ok(DatasetOptions {
        default_graphs: unsafe { str_array(defaults, ndefault) }?,
        named_graphs: unsafe { str_array(named, nnamed) }?,
        union_default_graph,
        with_mappings,
    })
}

unsafe extern "C" fn kb_query_open(
    host: *const KbHost,
    spec: *const KbQuerySpec,
    out: *mut *mut KbQuery,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if spec.is_null() || out.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            *out = std::ptr::null_mut();
            let spec = &*spec;
            if (spec.size as usize) < std::mem::size_of::<KbQuerySpec>() {
                return Err(EngineError::invalid("the query spec is from an older ABI"));
            }
            let host = Host::new(host)?;
            let options = spec_options(
                spec.ndefault_graphs,
                spec.default_graphs,
                spec.nnamed_graphs,
                spec.named_graphs,
                spec.union_default_graph,
                spec.with_mappings,
            )?;
            let base = spec.base_iri.to_opt_string()?;
            let query = KbQuery::open(
                host,
                spec.query.as_str()?,
                base.as_deref(),
                &options,
                spec.explain,
            )?;
            *out = Box::into_raw(Box::new(query));
            Ok(())
        })
    }
}

unsafe extern "C" fn kb_query_kind(query: *mut KbQuery) -> i32 {
    if query.is_null() {
        return 0;
    }
    unsafe { (*query).kind() }
}

unsafe extern "C" fn kb_query_variables(
    query: *mut KbQuery,
    n: *mut i32,
    names: *mut *const KbStr,
) -> i32 {
    if query.is_null() || n.is_null() || names.is_null() {
        return error::KB_ERR_INVALID;
    }
    unsafe {
        let vars = (*query).variable_strs();
        *n = vars.len() as i32;
        *names = vars.as_ptr();
    }
    error::KB_OK
}

unsafe extern "C" fn kb_query_next(
    query: *mut KbQuery,
    values: *mut *const KbTerm,
    done: *mut bool,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if query.is_null() || values.is_null() || done.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            match (*query).next_row()? {
                Some(row) => {
                    *values = row.as_ptr();
                    *done = false;
                }
                None => {
                    *values = std::ptr::null();
                    *done = true;
                }
            }
            Ok(())
        })
    }
}

unsafe extern "C" fn kb_query_write(
    query: *mut KbQuery,
    format: *const c_char,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if query.is_null() || format.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            let format = std::ffi::CStr::from_ptr(format).to_str()?;
            (*query).write(format)
        })
    }
}

unsafe extern "C" fn kb_query_explain(
    query: *mut KbQuery,
    json: *mut *mut c_char,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if query.is_null() || json.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            *json = c_string(&(*query).explain()?);
            Ok(())
        })
    }
}

unsafe extern "C" fn kb_query_trace(
    query: *mut KbQuery,
    max_bytes: i32,
    raw: bool,
    out: *mut KbTrace,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if query.is_null() || out.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            trace::fill(&mut *out, Some(&(*query).trace()), max_bytes, raw)
        })
    }
}

/// A SPARQL text for a trace: in at most max_bytes, its literals replaced
/// by '?' unless raw.
unsafe extern "C" fn kb_text_trace(
    text: KbStr,
    max_bytes: i32,
    raw: bool,
    out: *mut *mut c_char,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if out.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            let text = text.as_str()?;
            let text = if raw { text.to_owned() } else { trace::normalize(text) };
            *out = c_string(&trace::cut(text.trim(), max_bytes.max(16) as usize));
            Ok(())
        })
    }
}

unsafe extern "C" fn kb_memory_peak(reset: bool) -> i64 {
    alloc::peak(reset) as i64
}

unsafe extern "C" fn kb_query_close(query: *mut KbQuery) {
    if !query.is_null() {
        // a panic in a drop has nowhere to go but here
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(unsafe { Box::from_raw(query) });
        }));
    }
}

unsafe extern "C" fn kb_update(
    host: *const KbHost,
    spec: *const KbUpdateSpec,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if spec.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            let spec = &*spec;
            if (spec.size as usize) < std::mem::size_of::<KbUpdateSpec>() {
                return Err(EngineError::invalid("the update spec is from an older ABI"));
            }
            let host = Host::new(host)?;
            let options = update::UpdateOptions {
                base_iri: spec.base_iri.to_opt_string()?,
                using: spec_options(
                    spec.nusing_graphs,
                    spec.using_graphs,
                    spec.nusing_named_graphs,
                    spec.using_named_graphs,
                    spec.union_default_graph,
                    spec.with_mappings,
                )?,
                allow_load: spec.allow_load,
            };
            update::update(host, spec.update.as_str()?, &options)
        })
    }
}

fn graph_name(name: Option<String>) -> Result<GraphName, EngineError> {
    Ok(match name {
        Some(g) => NamedNode::new(g)?.into(),
        None => GraphName::DefaultGraph,
    })
}

unsafe extern "C" fn kb_load(
    host: *const KbHost,
    spec: *const KbLoadSpec,
    count: *mut i64,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if spec.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            let spec = &*spec;
            if (spec.size as usize) < std::mem::size_of::<KbLoadSpec>() {
                return Err(EngineError::invalid("the load spec is from an older ABI"));
            }
            let host = Host::new(host)?;
            let format_name = spec.format.to_opt_string()?;
            let format = format_name.as_deref().map(format::require_rdf_format).transpose()?;
            let options = load::LoadOptions {
                format,
                base_iri: spec.base_iri.to_opt_string()?,
                graph: graph_name(spec.graph.to_opt_string()?)?,
                lenient: spec.lenient,
                batch: if spec.batch > 0 { spec.batch as usize } else { 8192 },
                datasets: true,
            };
            let n = match spec.path.to_opt_string()? {
                Some(path) => load::load_file(host, &path, &options)?,
                None => {
                    let format = format.ok_or_else(|| {
                        EngineError::invalid("a document's format must be named")
                    })?;
                    load::load_bytes(host, spec.data.as_bytes(), format, &options)?
                }
            };
            if !count.is_null() {
                *count = n as i64;
            }
            Ok(())
        })
    }
}

unsafe extern "C" fn kb_dump(host: *const KbHost, spec: *const KbDumpSpec, err: *mut KbError) -> i32 {
    unsafe {
        guard(err, || {
            if spec.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            let spec = &*spec;
            if (spec.size as usize) < std::mem::size_of::<KbDumpSpec>() {
                return Err(EngineError::invalid("the dump spec is from an older ABI"));
            }
            let host = Host::new(host)?;
            let format = format::require_rdf_format(spec.format.as_str()?)?;
            let graph = if spec.default_graph {
                load::DumpGraph::Default
            } else {
                match spec.graph.to_opt_string()? {
                    Some(g) => load::DumpGraph::Named(NamedNode::new(g)?),
                    None => load::DumpGraph::All,
                }
            };
            load::dump(host, format, &graph, spec.with_mappings)
        })
    }
}

unsafe extern "C" fn kb_term_text(
    host: *const KbHost,
    key: *const KbKey,
    text: *mut *mut c_char,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if key.is_null() || text.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            *text = std::ptr::null_mut();
            let key = (*key).b;
            if key::is_default_graph(&key) {
                return Ok(());
            }
            let term = match key::inline_term(&key) {
                Some(term) => Some(term),
                None => {
                    let host = Host::new(host)?;
                    match host.terms_get(&[key])?.into_iter().next().flatten() {
                        Some(row) => key::hashed_term(&key, &row.value, &row.extra),
                        None => None,
                    }
                }
            };
            if let Some(term) = term {
                *text = c_string(&term.to_string());
            }
            Ok(())
        })
    }
}

// The parsed term of the last term_parse(), whose strings it returned.
thread_local! {
    static PARSED: std::cell::RefCell<Option<Term>> = const { std::cell::RefCell::new(None) };
}

unsafe extern "C" fn kb_term_parse(
    text: *const c_char,
    key: *mut KbKey,
    term: *mut KbTerm,
    hashed: *mut bool,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if text.is_null() || key.is_null() || term.is_null() || hashed.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            let text = std::ffi::CStr::from_ptr(text).to_str()?;
            let parsed: Term = text.trim().parse().map_err(|e| {
                EngineError::syntax(format!("\"{text}\" is not a term in N-Triples' syntax: {e}"))
            })?;
            let (k, row) = key::term_key(parsed.as_ref());
            (*key).b = k;
            *hashed = row.is_some();
            PARSED.with(|p| {
                let mut p = p.borrow_mut();
                *p = Some(parsed);
                let t = p.as_ref().expect("just set");
                *term = match t {
                    Term::NamedNode(n) => KbTerm {
                        kind: KB_TERM_IRI,
                        value: KbStr::from_str(n.as_str()),
                        datatype: KbStr::EMPTY,
                        lang: KbStr::EMPTY,
                    },
                    Term::BlankNode(b) => KbTerm {
                        kind: KB_TERM_BNODE,
                        value: KbStr::from_str(b.as_str()),
                        datatype: KbStr::EMPTY,
                        lang: KbStr::EMPTY,
                    },
                    Term::Literal(l) => KbTerm {
                        kind: KB_TERM_LITERAL,
                        value: KbStr::from_str(l.value()),
                        datatype: KbStr::from_str(l.datatype().as_str()),
                        lang: l.language().map_or(KbStr::EMPTY, KbStr::from_str),
                    },
                };
            });
            Ok(())
        })
    }
}

unsafe extern "C" fn kb_mapping_check(
    host: *const KbHost,
    mapping: *const KbMappingRow,
    summary: *mut *mut c_char,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if mapping.is_null() || summary.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            let host = Host::new(host)?;
            let m = &*mapping;
            let row = MappingRow {
                name: m.name.as_str()?.to_owned(),
                document: m.document.as_str()?.to_owned(),
                base_iri: m.base_iri.to_opt_string()?,
                revision: m.revision,
            };
            *summary = c_string(&r2rml::check_mapping(&host, &row)?);
            Ok(())
        })
    }
}

unsafe extern "C" fn kb_direct_mapping(
    tables_json: *const c_char,
    base_iri: *const c_char,
    document: *mut *mut c_char,
    err: *mut KbError,
) -> i32 {
    unsafe {
        guard(err, || {
            if tables_json.is_null() || base_iri.is_null() || document.is_null() {
                return Err(EngineError::invalid("a NULL argument"));
            }
            let tables = std::ffi::CStr::from_ptr(tables_json).to_str()?;
            let base = std::ffi::CStr::from_ptr(base_iri).to_str()?;
            *document = c_string(&r2rml::direct_mapping(tables, base)?);
            Ok(())
        })
    }
}

static API: KbApi = KbApi {
    size: std::mem::size_of::<KbApi>() as u32,
    abi_version: ABI_VERSION,
    version: kb_version,
    free_string: kb_free_string,
    configure: kb_configure,
    memory_used: kb_memory_used,
    query_open: kb_query_open,
    query_kind: kb_query_kind,
    query_variables: kb_query_variables,
    query_next: kb_query_next,
    query_write: kb_query_write,
    query_explain: kb_query_explain,
    query_close: kb_query_close,
    update: kb_update,
    load: kb_load,
    dump: kb_dump,
    term_text: kb_term_text,
    term_parse: kb_term_parse,
    mapping_check: kb_mapping_check,
    direct_mapping: kb_direct_mapping,
    http_prepare: http::kb_http_prepare,
    http_execute: http::kb_http_execute,
    http_close: http::kb_http_close,
    query_trace: kb_query_trace,
    http_explain: http::kb_http_explain,
    http_trace: http::kb_http_trace,
    text_trace: kb_text_trace,
    memory_peak: kb_memory_peak,
};

/// The table of entry points, for the ABI version the caller was built
/// against; NULL for another.
#[unsafe(no_mangle)]
pub extern "C" fn kb_engine_api(abi_version: u32) -> *const KbApi {
    if abi_version != ABI_VERSION {
        return std::ptr::null();
    }
    &API
}
