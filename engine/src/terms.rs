// SPDX-License-Identifier: Apache-2.0
//! The evaluator's terms (docs/design.md §3.2): a key, and the term itself
//! where the engine has it at hand -- a query's constant, a value computed,
//! a term an R2RML mapping made -- so that only the store's hashed keys go
//! to the dictionary, through a backend-wide cache.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::mem::take;
use std::rc::Rc;

use oxrdf::Term;
use rustc_hash::FxHashMap;

use crate::error::{EResult, EngineError};
use crate::host::Host;
use crate::key::{self, Key};

/// A term as the evaluator holds it: equal when the keys are.
#[derive(Clone)]
pub struct Tk {
    pub key: Key,
    pub term: Option<Rc<Term>>,
}

impl Tk {
    pub fn stored(key: Key) -> Self {
        Self { key, term: None }
    }

    /// A term's key, the term kept beside it when the key is a hash.
    pub fn from_term(term: Term) -> Self {
        let (key, row) = key::term_key(term.as_ref());
        Self {
            key,
            term: row.map(|_| Rc::new(term)),
        }
    }
}

impl PartialEq for Tk {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Tk {}

impl Hash for Tk {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        // the hash's own bits, or the inline value's
        state.write(&self.key);
    }
}

/// Hashed keys' terms, kept across queries: a key names one term for ever,
/// so the cache is never stale.  Two generations: a key found in the old one
/// moves to the young one; when the young one is full it becomes the old.
struct TermCache {
    young: FxHashMap<Key, Term>,
    old: FxHashMap<Key, Term>,
    capacity: usize,
}

impl TermCache {
    fn get(&mut self, key: &Key) -> Option<Term> {
        if let Some(term) = self.young.get(key) {
            return Some(term.clone());
        }
        let term = self.old.remove(key)?;
        self.put(*key, term.clone());
        Some(term)
    }

    fn put(&mut self, key: Key, term: Term) {
        if self.young.len() >= self.capacity {
            self.old = take(&mut self.young);
        }
        self.young.insert(key, term);
    }
}

thread_local! {
    static CACHE: RefCell<TermCache> = RefCell::new(TermCache {
        young: FxHashMap::default(),
        old: FxHashMap::default(),
        capacity: 1 << 16,
    });
}

pub fn set_cache_capacity(capacity: usize) {
    CACHE.with(|c| c.borrow_mut().capacity = capacity.max(1024));
}

/// A key's term: inline, cached, or the dictionary's.
pub fn term_of(host: &Host, key: &Key) -> EResult<Term> {
    if let Some(term) = key::inline_term(key) {
        return Ok(term);
    }
    if let Some(term) = CACHE.with(|c| c.borrow_mut().get(key)) {
        return Ok(term);
    }
    let rows = host.terms_get(std::slice::from_ref(key))?;
    let term = match rows.into_iter().next().flatten() {
        Some(row) => key::hashed_term(key, &row.value, &row.extra),
        None => None,
    }
    .ok_or_else(|| missing(key))?;
    CACHE.with(|c| c.borrow_mut().put(*key, term.clone()));
    Ok(term)
}

fn missing(key: &Key) -> EngineError {
    if key::is_default_graph(key) {
        EngineError::internal("the default graph has no term")
    } else {
        EngineError::internal(format!(
            "the term of key {} is not in kb.terms",
            key::key_text(key)
        ))
    }
}

/// Bring the dictionary's rows of many keys into the cache at once: one
/// call to the host for a batch, where term_of() would make one each.
pub fn prefetch(host: &Host, keys: impl IntoIterator<Item = Key>) -> EResult<()> {
    let mut wanted: Vec<Key> = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        keys.into_iter()
            .filter(|k| key::is_hashed(k) && c.get(k).is_none())
            .collect()
    });
    if wanted.is_empty() {
        return Ok(());
    }
    wanted.sort_unstable();
    wanted.dedup();
    let rows = host.terms_get(&wanted)?;
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        for (key, row) in wanted.iter().zip(rows) {
            if let Some(row) = row {
                if let Some(term) = key::hashed_term(key, &row.value, &row.extra) {
                    c.put(*key, term);
                }
            }
        }
    });
    Ok(())
}

/// The term an evaluator's term names.
pub fn externalize(host: &Host, tk: &Tk) -> EResult<Term> {
    match &tk.term {
        Some(term) => Ok(Term::clone(term)),
        None => term_of(host, &tk.key),
    }
}
