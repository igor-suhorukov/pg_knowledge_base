// SPDX-License-Identifier: Apache-2.0
//! The host's callbacks (include/kb_engine.h, KbHost), as the engine calls
//! them: the store's scans and writes, SQL for R2RML, the output.
//!
//! A callback that fails has caught PostgreSQL's error and kept it; the
//! engine then returns EngineError::host() up to its entry point and calls
//! nothing more of the host's but closes, which the extension makes no-ops
//! once it holds an error.

use std::ffi::{CString, c_char};
use std::io;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::error::{EResult, EngineError};
use crate::ffi::*;
use crate::key::{DictRow, Key};

/// What a scan of the store reads when its pattern names no graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Graphs<'a> {
    One(&'a Key),
    Named,
    All,
}

#[derive(Clone, Copy)]
pub struct Host {
    raw: KbHost,
}

/// A SQL column, as sql_describe() gives it.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub type_oid: u32,
    pub sql_class: i32,
    pub type_name: String,
}

/// A mapping, as kb.mappings holds it.
#[derive(Debug, Clone)]
pub struct MappingRow {
    pub name: String,
    pub document: String,
    pub base_iri: Option<String>,
    pub revision: i64,
}

fn ok(rc: i32) -> EResult<()> {
    if rc == KB_HOST_OK {
        crate::alloc::check()
    } else {
        Err(EngineError::host())
    }
}

fn missing(what: &str) -> EngineError {
    EngineError::unsupported(format!("the caller offers no {what}"))
}

fn kb_key(key: &Key) -> KbKey {
    KbKey { b: *key }
}

fn keys_ptr(keys: &[Key]) -> *const KbKey {
    // Key and KbKey are the same 16 bytes
    keys.as_ptr().cast()
}

fn opt_key(key: Option<&Key>, slot: &mut KbKey) -> *const KbKey {
    match key {
        Some(key) => {
            *slot = kb_key(key);
            slot
        }
        None => std::ptr::null(),
    }
}

impl Host {
    /// # Safety
    /// host is a valid KbHost whose callbacks stay valid while the Host and
    /// what it opens are used.
    pub unsafe fn new(host: *const KbHost) -> EResult<Self> {
        if host.is_null() {
            return Err(EngineError::invalid("the host is NULL"));
        }
        let raw = unsafe { *host };
        if (raw.size as usize) < std::mem::size_of::<KbHost>() {
            return Err(EngineError::invalid("the host is from an older ABI"));
        }
        Ok(Self { raw })
    }

    pub fn quads(
        &self,
        s: Option<&Key>,
        p: Option<&Key>,
        o: Option<&Key>,
        graphs: Graphs<'_>,
    ) -> EResult<QuadCursor> {
        let open = self.raw.quads_open.ok_or_else(|| missing("store"))?;
        let (mut ks, mut kp, mut ko, mut kg) = (KbKey { b: [0; 16] }, KbKey { b: [0; 16] }, KbKey { b: [0; 16] }, KbKey { b: [0; 16] });
        let (g, mode) = match graphs {
            Graphs::One(g) => (Some(g), KB_GRAPHS_ONE),
            Graphs::Named => (None, KB_GRAPHS_NAMED),
            Graphs::All => (None, KB_GRAPHS_ALL),
        };
        let mut cursor = std::ptr::null_mut();
        ok(unsafe {
            open(
                self.raw.ctx,
                opt_key(s, &mut ks),
                opt_key(p, &mut kp),
                opt_key(o, &mut ko),
                opt_key(g, &mut kg),
                mode,
                &mut cursor,
            )
        })?;
        Ok(QuadCursor::new(*self, cursor))
    }

    /// The named graphs kb.graphs holds.
    pub fn graphs(&self) -> EResult<QuadCursor> {
        let open = self.raw.graphs_open.ok_or_else(|| missing("store"))?;
        let mut cursor = std::ptr::null_mut();
        ok(unsafe { open(self.raw.ctx, &mut cursor) })?;
        Ok(QuadCursor::new(*self, cursor))
    }

    pub fn graph_exists(&self, g: &Key) -> EResult<bool> {
        let f = self.raw.graph_exists.ok_or_else(|| missing("store"))?;
        let mut exists = false;
        let key = kb_key(g);
        ok(unsafe { f(self.raw.ctx, &key, &mut exists) })?;
        Ok(exists)
    }

    /// The dictionary rows of keys, None where it has none.
    pub fn terms_get(&self, keys: &[Key]) -> EResult<Vec<Option<DictRow>>> {
        let f = self.raw.terms_get.ok_or_else(|| missing("store"))?;
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(1024) {
            let mut entries = vec![
                KbDictEntry {
                    found: false,
                    value: KbStr::EMPTY,
                    extra: KbStr::EMPTY,
                };
                chunk.len()
            ];
            ok(unsafe {
                f(
                    self.raw.ctx,
                    keys_ptr(chunk),
                    chunk.len() as i32,
                    entries.as_mut_ptr(),
                )
            })?;
            for entry in &entries {
                out.push(if entry.found {
                    Some(DictRow {
                        value: unsafe { entry.value.as_str() }?.to_owned(),
                        extra: unsafe { entry.extra.as_str() }?.to_owned(),
                    })
                } else {
                    None
                });
            }
        }
        Ok(out)
    }

    pub fn quads_write(&self, op: i32, quads: &[[Key; 4]]) -> EResult<()> {
        let f = self.raw.quads_write.ok_or_else(|| missing("store writes"))?;
        for chunk in quads.chunks(4096) {
            ok(unsafe { f(self.raw.ctx, op, chunk.as_ptr().cast(), chunk.len() as i32) })?;
        }
        Ok(())
    }

    pub fn terms_write(&self, keys: &[Key], rows: &[DictRow]) -> EResult<()> {
        let f = self.raw.terms_write.ok_or_else(|| missing("store writes"))?;
        for (keys, rows) in keys.chunks(4096).zip(rows.chunks(4096)) {
            let entries: Vec<KbDictEntry> = rows
                .iter()
                .map(|r| KbDictEntry {
                    found: true,
                    value: KbStr::from_str(&r.value),
                    extra: KbStr::from_str(&r.extra),
                })
                .collect();
            ok(unsafe {
                f(
                    self.raw.ctx,
                    keys_ptr(keys),
                    entries.as_ptr(),
                    keys.len() as i32,
                )
            })?;
        }
        Ok(())
    }

    pub fn graphs_write(&self, op: i32, keys: &[Key]) -> EResult<()> {
        let f = self.raw.graphs_write.ok_or_else(|| missing("store writes"))?;
        for chunk in keys.chunks(4096) {
            ok(unsafe { f(self.raw.ctx, op, keys_ptr(chunk), chunk.len() as i32) })?;
        }
        Ok(())
    }

    pub fn graph_clear(&self, what: i32, g: Option<&Key>) -> EResult<()> {
        let f = self.raw.graph_clear.ok_or_else(|| missing("store writes"))?;
        let mut slot = KbKey { b: [0; 16] };
        ok(unsafe { f(self.raw.ctx, what, opt_key(g, &mut slot)) })
    }

    pub fn sql_describe(&self, sql: &str) -> EResult<Vec<Column>> {
        let f = self.raw.sql_describe.ok_or_else(|| missing("SQL"))?;
        let sql = CString::new(sql).map_err(|_| EngineError::invalid("a SQL text with a NUL"))?;
        let mut n = 0;
        let mut columns: *const KbColumn = std::ptr::null();
        ok(unsafe { f(self.raw.ctx, sql.as_ptr(), &mut n, &mut columns) })?;
        if n <= 0 || columns.is_null() {
            return Ok(Vec::new());
        }
        let columns = unsafe { std::slice::from_raw_parts(columns, n as usize) };
        columns
            .iter()
            .map(|c| {
                Ok(Column {
                    name: unsafe { c.name.as_str() }?.to_owned(),
                    type_oid: c.type_oid,
                    sql_class: c.sql_class,
                    type_name: unsafe { c.type_name.as_str() }?.to_owned(),
                })
            })
            .collect()
    }

    /// The rows PostgreSQL's plan of a query with these parameters expects;
    /// the query is planned, not run
    pub fn sql_estimate(&self, sql: &str, params: &[String]) -> EResult<f64> {
        let f = self.raw.sql_estimate.ok_or_else(|| missing("SQL"))?;
        let sql = CString::new(sql).map_err(|_| EngineError::invalid("a SQL text with a NUL"))?;
        let params: Vec<KbStr> = params.iter().map(|p| KbStr::from_str(p)).collect();
        let mut rows = 0.0;
        ok(unsafe { f(self.raw.ctx, sql.as_ptr(), params.len() as i32, params.as_ptr(), &mut rows) })?;
        Ok(rows)
    }

    /// A query's rows; options: KB_SQL_GENERIC_PLAN for a query run again
    /// and again with other parameters, planned once for any;
    /// KB_SQL_HASH_JOINS for one whose joins PostgreSQL cannot estimate
    pub fn sql_open(&self, sql: &str, params: &[String], options: i32) -> EResult<SqlCursor> {
        let f = self.raw.sql_open.ok_or_else(|| missing("SQL"))?;
        let sql = CString::new(sql).map_err(|_| EngineError::invalid("a SQL text with a NUL"))?;
        let params: Vec<KbStr> = params.iter().map(|p| KbStr::from_str(p)).collect();
        let mut cursor = std::ptr::null_mut();
        let mut ncolumns = 0;
        ok(unsafe {
            f(
                self.raw.ctx,
                sql.as_ptr(),
                params.len() as i32,
                params.as_ptr(),
                options,
                &mut cursor,
                &mut ncolumns,
            )
        })?;
        Ok(SqlCursor {
            host: *self,
            cursor,
            ncolumns: ncolumns.max(0) as usize,
            rows: Vec::new(),
            pos: 0,
            done: false,
        })
    }

    pub fn has_sql(&self) -> bool {
        self.raw.sql_open.is_some() && self.raw.mappings_get.is_some()
    }

    pub fn mappings(&self) -> EResult<Vec<MappingRow>> {
        let Some(f) = self.raw.mappings_get else {
            return Ok(Vec::new());
        };
        let mut n = 0;
        let mut rows: *const KbMappingRow = std::ptr::null();
        ok(unsafe { f(self.raw.ctx, &mut n, &mut rows) })?;
        if n <= 0 || rows.is_null() {
            return Ok(Vec::new());
        }
        let rows = unsafe { std::slice::from_raw_parts(rows, n as usize) };
        rows.iter()
            .map(|r| {
                Ok(MappingRow {
                    name: unsafe { r.name.as_str() }?.to_owned(),
                    document: unsafe { r.document.as_str() }?.to_owned(),
                    base_iri: unsafe { r.base_iri.to_opt_string() }?,
                    revision: r.revision,
                })
            })
            .collect()
    }

    pub fn write(&self, data: &[u8]) -> EResult<()> {
        let f = self.raw.write.ok_or_else(|| missing("output"))?;
        if data.is_empty() {
            return Ok(());
        }
        ok(unsafe { f(self.raw.ctx, data.as_ptr().cast::<c_char>(), data.len()) })
    }

    pub fn respond(&self, status: i32, content_type: &str, headers: &str) -> EResult<()> {
        let f = self.raw.respond.ok_or_else(|| missing("HTTP response"))?;
        let content_type = CString::new(content_type).unwrap_or_default();
        let headers = CString::new(headers).unwrap_or_default();
        ok(unsafe { f(self.raw.ctx, status, content_type.as_ptr(), headers.as_ptr()) })
    }

    pub fn check_interrupts(&self) -> EResult<()> {
        match self.raw.check_interrupts {
            Some(f) => ok(unsafe { f(self.raw.ctx) }),
            None => Ok(()),
        }
    }

    /// At each row made and each quad read: an interrupt the host has
    /// pending served, and the memory limit checked -- two reads, until one
    /// is due.  spareval's own loops over what it has materialized (a
    /// cartesian product's inner side, a sort's input) call nothing, and
    /// see a cancel at their next row or quad.
    pub fn poll(&self) -> EResult<()> {
        let flag = INTERRUPT_PENDING.load(Ordering::Relaxed);
        if !flag.is_null() && unsafe { std::ptr::read_volatile(flag) } != 0 {
            self.check_interrupts()?;
        }
        crate::alloc::check()
    }
}

/// The host's flag of a pending interrupt (KbConfig.interrupt_pending)
static INTERRUPT_PENDING: AtomicPtr<i32> = AtomicPtr::new(std::ptr::null_mut());

pub fn set_interrupt_flag(flag: *const i32) {
    INTERRUPT_PENDING.store(flag.cast_mut(), Ordering::Relaxed);
}

/// A scan of the store, its quads s, p, o, g a batch at a time; closed when
/// dropped.
pub struct QuadCursor {
    host: Host,
    cursor: *mut KbCursor,
    buf: Vec<[Key; 4]>,
    pos: usize,
    done: bool,
}

const QUAD_BATCH: usize = 256;

impl QuadCursor {
    fn new(host: Host, cursor: *mut KbCursor) -> Self {
        Self {
            host,
            cursor,
            buf: Vec::new(),
            pos: 0,
            done: cursor.is_null(),
        }
    }

    fn refill(&mut self) -> EResult<()> {
        let Some(next) = self.host.raw.quads_next else {
            return Err(missing("store"));
        };
        self.buf.clear();
        self.buf.resize(QUAD_BATCH, [[0; 16]; 4]);
        let mut n = 0;
        let rc = unsafe {
            next(
                self.cursor,
                self.buf.as_mut_ptr().cast(),
                QUAD_BATCH as i32,
                &mut n,
            )
        };
        if rc != KB_HOST_OK {
            self.buf.clear();
            self.done = true;
            return Err(EngineError::host());
        }
        crate::alloc::check()?;
        let n = (n.max(0) as usize).min(QUAD_BATCH);
        self.buf.truncate(n);
        self.pos = 0;
        if n == 0 {
            self.done = true;
            self.close();
        }
        Ok(())
    }

    fn close(&mut self) {
        if !self.cursor.is_null() {
            if let Some(close) = self.host.raw.quads_close {
                unsafe { close(self.cursor) };
            }
            self.cursor = std::ptr::null_mut();
        }
    }
}

impl Iterator for QuadCursor {
    type Item = EResult<[Key; 4]>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos < self.buf.len() {
                if let Err(e) = self.host.poll() {
                    self.buf.clear();
                    self.pos = 0;
                    self.done = true;
                    return Some(Err(e));
                }
                let quad = self.buf[self.pos];
                self.pos += 1;
                return Some(Ok(quad));
            }
            if self.done {
                return None;
            }
            if let Err(e) = self.refill() {
                return Some(Err(e));
            }
        }
    }
}

impl Drop for QuadCursor {
    fn drop(&mut self) {
        self.close();
    }
}

/// A SQL query's rows, a batch at a time; closed when dropped.
pub struct SqlCursor {
    host: Host,
    cursor: *mut KbCursor,
    ncolumns: usize,
    rows: Vec<Vec<Option<String>>>,
    pos: usize,
    done: bool,
}

const SQL_BATCH: i32 = 512;

impl SqlCursor {
    pub fn ncolumns(&self) -> usize {
        self.ncolumns
    }

    fn refill(&mut self) -> EResult<()> {
        let Some(next) = self.host.raw.sql_next else {
            return Err(missing("SQL"));
        };
        let mut values: *const KbSqlValue = std::ptr::null();
        let mut nrows = 0;
        let rc = unsafe { next(self.cursor, SQL_BATCH, &mut values, &mut nrows) };
        if rc != KB_HOST_OK {
            self.done = true;
            return Err(EngineError::host());
        }
        crate::alloc::check()?;
        self.rows.clear();
        self.pos = 0;
        let nrows = nrows.max(0) as usize;
        if nrows == 0 || values.is_null() {
            self.done = true;
            self.close();
            return Ok(());
        }
        let values = unsafe { std::slice::from_raw_parts(values, nrows * self.ncolumns) };
        for row in values.chunks(self.ncolumns.max(1)) {
            self.rows.push(
                row.iter()
                    .map(|v| {
                        if v.isnull {
                            Ok(None)
                        } else {
                            Ok(Some(unsafe { v.text.as_str() }?.to_owned()))
                        }
                    })
                    .collect::<EResult<Vec<_>>>()?,
            );
        }
        Ok(())
    }

    fn close(&mut self) {
        if !self.cursor.is_null() {
            if let Some(close) = self.host.raw.sql_close {
                unsafe { close(self.cursor) };
            }
            self.cursor = std::ptr::null_mut();
        }
    }
}

impl Iterator for SqlCursor {
    type Item = EResult<Vec<Option<String>>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos < self.rows.len() {
                if let Err(e) = self.host.poll() {
                    self.rows.clear();
                    self.pos = 0;
                    self.done = true;
                    return Some(Err(e));
                }
                let row = std::mem::take(&mut self.rows[self.pos]);
                self.pos += 1;
                return Some(Ok(row));
            }
            if self.done || self.cursor.is_null() {
                return None;
            }
            if let Err(e) = self.refill() {
                return Some(Err(e));
            }
        }
    }
}

impl Drop for SqlCursor {
    fn drop(&mut self) {
        self.close();
    }
}

/// The host's output as a writer, 64 kB at a time.
pub struct HostWriter {
    host: Host,
    buf: Vec<u8>,
    pub written: u64,
}

const WRITE_CHUNK: usize = 64 * 1024;

impl HostWriter {
    pub fn new(host: Host) -> Self {
        Self {
            host,
            buf: Vec::with_capacity(WRITE_CHUNK),
            written: 0,
        }
    }

    fn drain(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            self.host.write(&self.buf)?;
            self.written += self.buf.len() as u64;
            self.buf.clear();
        }
        Ok(())
    }
}

impl io::Write for HostWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= WRITE_CHUNK {
            self.drain()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.drain()
    }
}
