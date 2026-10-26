// SPDX-License-Identifier: Apache-2.0
//! The C ABI's structs, field for field as include/kb_engine.h declares
//! them, and the helpers every entry point uses.

use std::ffi::{CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::error::{EResult, EngineError, KB_ERR_PANIC, KB_OK};

pub const KB_HOST_OK: i32 = 0;
#[allow(dead_code)]
pub const KB_HOST_ERROR: i32 = 1;

#[repr(C)]
pub struct KbError {
    pub code: i32,
    pub http_status: i32,
    pub message: *mut c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KbKey {
    pub b: [u8; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KbStr {
    pub ptr: *const c_char,
    pub len: i64,
}

pub const KB_TERM_NONE: i32 = 0;
pub const KB_TERM_IRI: i32 = 1;
pub const KB_TERM_BNODE: i32 = 2;
pub const KB_TERM_LITERAL: i32 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KbTerm {
    pub kind: i32,
    pub value: KbStr,
    pub datatype: KbStr,
    pub lang: KbStr,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KbDictEntry {
    pub found: bool,
    pub value: KbStr,
    pub extra: KbStr,
}

pub const KB_GRAPHS_ONE: i32 = 0;
pub const KB_GRAPHS_NAMED: i32 = 1;
pub const KB_GRAPHS_ALL: i32 = 2;

pub const KB_WRITE_INSERT: i32 = 1;
pub const KB_WRITE_DELETE: i32 = 2;

pub const KB_CLEAR_GRAPH: i32 = 1;
pub const KB_CLEAR_DROP: i32 = 2;
pub const KB_CLEAR_NAMED: i32 = 3;
pub const KB_CLEAR_DROP_NAMED: i32 = 4;
pub const KB_CLEAR_ALL: i32 = 5;

#[allow(dead_code)] // the class of what is none of the others
pub const KB_SQL_STRING: i32 = 0;
pub const KB_SQL_INTEGER: i32 = 1;
pub const KB_SQL_DECIMAL: i32 = 2;
pub const KB_SQL_FLOAT: i32 = 3;
pub const KB_SQL_BOOLEAN: i32 = 4;
pub const KB_SQL_DATE: i32 = 5;
pub const KB_SQL_TIME: i32 = 6;
pub const KB_SQL_TIMESTAMP: i32 = 7;
pub const KB_SQL_BINARY: i32 = 8;

/// sql_open()'s options
pub const KB_SQL_GENERIC_PLAN: i32 = 1;
pub const KB_SQL_HASH_JOINS: i32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KbColumn {
    pub name: KbStr,
    pub type_oid: u32,
    pub sql_class: i32,
    pub type_name: KbStr,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KbSqlValue {
    pub isnull: bool,
    pub text: KbStr,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KbMappingRow {
    pub name: KbStr,
    pub document: KbStr,
    pub base_iri: KbStr,
    pub revision: i64,
}

#[repr(C)]
pub struct KbCursor {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KbHost {
    pub size: u32,
    pub ctx: *mut c_void,
    pub quads_open: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *const KbKey,
            *const KbKey,
            *const KbKey,
            *const KbKey,
            i32,
            *mut *mut KbCursor,
        ) -> i32,
    >,
    pub quads_next: Option<unsafe extern "C" fn(*mut KbCursor, *mut KbKey, i32, *mut i32) -> i32>,
    pub quads_close: Option<unsafe extern "C" fn(*mut KbCursor)>,
    pub terms_get:
        Option<unsafe extern "C" fn(*mut c_void, *const KbKey, i32, *mut KbDictEntry) -> i32>,
    pub graphs_open: Option<unsafe extern "C" fn(*mut c_void, *mut *mut KbCursor) -> i32>,
    pub graph_exists: Option<unsafe extern "C" fn(*mut c_void, *const KbKey, *mut bool) -> i32>,
    pub quads_write: Option<unsafe extern "C" fn(*mut c_void, i32, *const KbKey, i32) -> i32>,
    pub terms_write:
        Option<unsafe extern "C" fn(*mut c_void, *const KbKey, *const KbDictEntry, i32) -> i32>,
    pub graphs_write: Option<unsafe extern "C" fn(*mut c_void, i32, *const KbKey, i32) -> i32>,
    pub graph_clear: Option<unsafe extern "C" fn(*mut c_void, i32, *const KbKey) -> i32>,
    pub sql_describe: Option<
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut i32, *mut *const KbColumn) -> i32,
    >,
    pub sql_open: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            i32,
            *const KbStr,
            i32,
            *mut *mut KbCursor,
            *mut i32,
        ) -> i32,
    >,
    pub sql_next:
        Option<unsafe extern "C" fn(*mut KbCursor, i32, *mut *const KbSqlValue, *mut i32) -> i32>,
    pub sql_close: Option<unsafe extern "C" fn(*mut KbCursor)>,
    pub sql_estimate:
        Option<unsafe extern "C" fn(*mut c_void, *const c_char, i32, *const KbStr, *mut f64) -> i32>,
    pub mappings_get:
        Option<unsafe extern "C" fn(*mut c_void, *mut i32, *mut *const KbMappingRow) -> i32>,
    pub write: Option<unsafe extern "C" fn(*mut c_void, *const c_char, usize) -> i32>,
    pub respond:
        Option<unsafe extern "C" fn(*mut c_void, i32, *const c_char, *const c_char) -> i32>,
    pub check_interrupts: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
}

pub const KB_QUERY_SELECT: i32 = 1;
pub const KB_QUERY_ASK: i32 = 2;
pub const KB_QUERY_CONSTRUCT: i32 = 3;

/// A call's figures for a trace (include/kb_engine.h)
#[repr(C)]
pub struct KbTrace {
    pub size: u32,
    pub operation: [c_char; 16],
    pub rows: i64,
    pub plan_nodes: i32,
    pub plan: *mut c_char,
}

#[repr(C)]
pub struct KbConfig {
    pub size: u32,
    pub memory_limit: i64,
    pub term_cache_entries: i32,
    pub interrupt_pending: *const i32,
    pub r2rml_pushdown: bool,
    pub statistics: bool,
}

#[repr(C)]
pub struct KbQuerySpec {
    pub size: u32,
    pub query: KbStr,
    pub base_iri: KbStr,
    pub ndefault_graphs: i32,
    pub default_graphs: *const KbStr,
    pub nnamed_graphs: i32,
    pub named_graphs: *const KbStr,
    pub union_default_graph: bool,
    pub with_mappings: bool,
    pub explain: bool,
}

#[repr(C)]
pub struct KbUpdateSpec {
    pub size: u32,
    pub update: KbStr,
    pub base_iri: KbStr,
    pub nusing_graphs: i32,
    pub using_graphs: *const KbStr,
    pub nusing_named_graphs: i32,
    pub using_named_graphs: *const KbStr,
    pub union_default_graph: bool,
    pub with_mappings: bool,
    pub allow_load: bool,
}

#[repr(C)]
pub struct KbLoadSpec {
    pub size: u32,
    pub data: KbStr,
    pub path: KbStr,
    pub format: KbStr,
    pub base_iri: KbStr,
    pub graph: KbStr,
    pub lenient: bool,
    pub batch: i32,
}

#[repr(C)]
pub struct KbDumpSpec {
    pub size: u32,
    pub format: KbStr,
    pub graph: KbStr,
    pub default_graph: bool,
    pub with_mappings: bool,
}

#[repr(C)]
pub struct KbHttpRequest {
    pub size: u32,
    pub method: KbStr,
    pub path: KbStr,
    pub query: KbStr,
    pub accept: KbStr,
    pub content_type: KbStr,
    pub base_iri: KbStr,
    pub body: KbStr,
    pub read_only: bool,
    pub union_default_graph: bool,
    pub with_mappings: bool,
    pub max_body: i64,
}

#[allow(dead_code)] // what a KbHttpInfo is before http_prepare() fills it
pub const KB_HTTP_ERROR: i32 = 0;
pub const KB_HTTP_READ: i32 = 1;
pub const KB_HTTP_WRITE: i32 = 2;
pub const KB_HTTP_STATIC: i32 = 3;

#[repr(C)]
pub struct KbHttpInfo {
    pub kind: i32,
    pub activity: KbStr,
}

impl KbStr {
    pub const EMPTY: KbStr = KbStr {
        ptr: std::ptr::null(),
        len: 0,
    };

    pub fn from_str(s: &str) -> Self {
        KbStr {
            ptr: s.as_ptr().cast(),
            len: s.len() as i64,
        }
    }

    pub fn from_bytes(s: &[u8]) -> Self {
        KbStr {
            ptr: s.as_ptr().cast(),
            len: s.len() as i64,
        }
    }

    /// # Safety
    /// ptr is NULL, or addresses len bytes that outlive 'a.
    pub unsafe fn as_bytes<'a>(&self) -> &'a [u8] {
        if self.ptr.is_null() || self.len <= 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.ptr.cast(), self.len as usize) }
    }

    /// # Safety
    /// As [`KbStr::as_bytes`].
    pub unsafe fn as_str<'a>(&self) -> EResult<&'a str> {
        Ok(std::str::from_utf8(unsafe { self.as_bytes() })?)
    }

    /// # Safety
    /// As [`KbStr::as_bytes`].
    pub unsafe fn to_opt_string(&self) -> EResult<Option<String>> {
        let s = unsafe { self.as_str() }?;
        Ok(if s.is_empty() {
            None
        } else {
            Some(s.to_owned())
        })
    }
}

/// An array of n KbStr as strings.
///
/// # Safety
/// p addresses n KbStr, each valid as [`KbStr::as_bytes`] requires.
pub unsafe fn str_array(p: *const KbStr, n: i32) -> EResult<Vec<String>> {
    if p.is_null() || n <= 0 {
        return Ok(Vec::new());
    }
    let items = unsafe { std::slice::from_raw_parts(p, n as usize) };
    items
        .iter()
        .map(|s| Ok(unsafe { s.as_str() }?.to_owned()))
        .collect()
}

pub fn c_string(s: &str) -> *mut c_char {
    // a message with a NUL is cut there
    let s = s.split('\0').next().unwrap_or("");
    CString::new(s).map_or(std::ptr::null_mut(), CString::into_raw)
}

pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic without a message".to_owned()
    }
}

/// Run an entry point's body: its error, or a panic, into *err.
///
/// # Safety
/// err is NULL or a valid KbError.
pub unsafe fn guard<F: FnOnce() -> EResult<()>>(err: *mut KbError, f: F) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(f));
    let error = match result {
        Ok(Ok(())) => return KB_OK,
        Ok(Err(e)) => e,
        Err(payload) => EngineError::new(
            KB_ERR_PANIC,
            format!("the engine panicked: {}", panic_message(&*payload)),
        ),
    };
    if !err.is_null() {
        unsafe {
            (*err).code = error.code;
            (*err).http_status = error.http();
            (*err).message = c_string(&error.message);
        }
    }
    error.code
}
