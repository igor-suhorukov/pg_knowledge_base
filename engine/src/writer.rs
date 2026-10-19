// SPDX-License-Identifier: Apache-2.0
//! Writes to the store (docs/design.md §3.3): quads as keys, the dictionary
//! rows of their hashed terms, the names of their named graphs, in batches
//! through the host, in the caller's transaction.

use oxrdf::{GraphNameRef, NamedOrBlankNodeRef, QuadRef, TermRef};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::error::EResult;
use crate::ffi::{KB_WRITE_DELETE, KB_WRITE_INSERT};
use crate::host::Host;
use crate::key::{self, DEFAULT_GRAPH, DictRow, Key};

pub struct StoreWriter {
    host: Host,
    batch: usize,
    inserts: Vec<[Key; 4]>,
    deletes: Vec<[Key; 4]>,
    terms: FxHashMap<Key, DictRow>,
    graphs: FxHashSet<Key>,
    /// Hashed keys written in this call: their rows need no second write.
    written: FxHashSet<Key>,
    pub inserted: u64,
    pub deleted: u64,
}

impl StoreWriter {
    pub fn new(host: Host, batch: usize) -> Self {
        Self {
            host,
            batch: batch.clamp(64, 1 << 20),
            inserts: Vec::new(),
            deletes: Vec::new(),
            terms: FxHashMap::default(),
            graphs: FxHashSet::default(),
            written: FxHashSet::default(),
            inserted: 0,
            deleted: 0,
        }
    }

    fn keep_term(&mut self, (key, row): (Key, Option<DictRow>)) -> Key {
        if let Some(row) = row {
            if !self.written.contains(&key) {
                self.terms.entry(key).or_insert(row);
            }
        }
        key
    }

    fn term_key(&mut self, term: TermRef<'_>) -> Key {
        self.keep_term(key::term_key(term))
    }

    fn resource_key(&mut self, node: NamedOrBlankNodeRef<'_>) -> Key {
        match node {
            NamedOrBlankNodeRef::NamedNode(n) => self.keep_term(key::named_node_key(n)),
            NamedOrBlankNodeRef::BlankNode(b) => self.keep_term(key::blank_node_key(b)),
        }
    }

    fn graph_key(&mut self, graph: GraphNameRef<'_>) -> Key {
        match graph {
            GraphNameRef::NamedNode(n) => {
                let k = self.keep_term(key::named_node_key(n));
                self.graphs.insert(k);
                k
            }
            GraphNameRef::BlankNode(b) => {
                let k = self.keep_term(key::blank_node_key(b));
                self.graphs.insert(k);
                k
            }
            GraphNameRef::DefaultGraph => DEFAULT_GRAPH,
        }
    }

    pub fn insert(&mut self, quad: QuadRef<'_>) -> EResult<()> {
        let s = self.resource_key(quad.subject);
        let p = self.keep_term(key::named_node_key(quad.predicate));
        let o = self.term_key(quad.object);
        let g = self.graph_key(quad.graph_name);
        self.inserts.push([s, p, o, g]);
        if self.inserts.len() >= self.batch {
            self.flush_inserts()?;
        }
        Ok(())
    }

    pub fn delete(&mut self, quad: QuadRef<'_>) -> EResult<()> {
        let s = key::term_key(quad.subject.into()).0;
        let p = key::named_node_key(quad.predicate).0;
        let o = key::term_key(quad.object).0;
        let g = match quad.graph_name {
            GraphNameRef::NamedNode(n) => key::named_node_key(n).0,
            GraphNameRef::BlankNode(b) => key::blank_node_key(b).0,
            GraphNameRef::DefaultGraph => DEFAULT_GRAPH,
        };
        self.deletes.push([s, p, o, g]);
        if self.deletes.len() >= self.batch {
            self.flush_deletes()?;
        }
        Ok(())
    }

    /// A named graph, empty or not (CREATE GRAPH, LOAD INTO an empty one).
    pub fn insert_graph(&mut self, graph: NamedOrBlankNodeRef<'_>) {
        let k = self.resource_key(graph);
        self.graphs.insert(k);
    }

    pub fn flush_deletes(&mut self) -> EResult<()> {
        if !self.deletes.is_empty() {
            self.host.quads_write(KB_WRITE_DELETE, &self.deletes)?;
            self.deleted += self.deletes.len() as u64;
            self.deletes.clear();
        }
        Ok(())
    }

    pub fn flush_inserts(&mut self) -> EResult<()> {
        if !self.terms.is_empty() {
            let (keys, rows): (Vec<Key>, Vec<DictRow>) = self.terms.drain().unzip();
            self.host.terms_write(&keys, &rows)?;
            if self.written.len() < (1 << 20) {
                self.written.extend(keys);
            }
        }
        if !self.graphs.is_empty() {
            let graphs: Vec<Key> = self.graphs.drain().collect();
            self.host.graphs_write(KB_WRITE_INSERT, &graphs)?;
        }
        if !self.inserts.is_empty() {
            self.host.quads_write(KB_WRITE_INSERT, &self.inserts)?;
            self.inserted += self.inserts.len() as u64;
            self.inserts.clear();
        }
        Ok(())
    }

    /// Deletions first, then insertions, as an operation's are applied.
    pub fn flush(&mut self) -> EResult<()> {
        self.flush_deletes()?;
        self.flush_inserts()
    }
}
