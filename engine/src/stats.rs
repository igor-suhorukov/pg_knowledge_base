// SPDX-License-Identifier: Apache-2.0
//! The optimizer's estimates (docs/design.md §2.4): sparopt orders a query's
//! joins, and chooses between nested loops and hash joins, by the quads it
//! expects of each pattern, which spareval asks the dataset for
//! (QueryableDataset's internal_quad_count_estimate and the others,
//! dataset.rs).  The store's quads are counted, up to STORE_COUNT, then
//! estimated by PostgreSQL's planner; the mappings' are estimated by the
//! plans of their SQL (r2rml/eval.rs), and a lookup of a query that reads
//! them costs LOOKUP_ROWS (internal_lookup_cost_estimate).  kb.statistics =
//! off leaves sparopt to the query's shape.

use std::sync::atomic::{AtomicBool, Ordering};

use rustc_hash::FxHashSet;

use crate::error::EResult;
use crate::host::{Graphs, Host};
use crate::key::{self, DEFAULT_GRAPH, Key};

static ENABLED: AtomicBool = AtomicBool::new(true);

/// kb.statistics, as the call's configuration has it
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The store's quads an estimate counts; past them, the planner's estimate
const STORE_COUNT: u64 = 10_000;

/// The rows a scan of a mapping's SQL reads in the time of one lookup of a
/// bound term -- a query planned and run, some 0.7 ms, against some 3 us a
/// row: what a nested loop pays for each row of its left side when the query
/// reads the mappings, sparopt's lookup cost.  A lookup of the store alone is
/// an index scan, which sparopt prices as one row.
pub const LOOKUP_ROWS: f64 = 256.0;

/// A scan of kb.quads as SQL conditions, its keys as parameters
fn conditions(s: Option<&Key>, p: Option<&Key>, o: Option<&Key>, graphs: Graphs<'_>, params: &mut Vec<String>) -> String {
    let mut out = Vec::new();
    for (column, k) in [("s", s), ("p", p), ("o", o)] {
        if let Some(k) = k {
            params.push(key::key_text(k));
            out.push(format!("{column} = ${}::uuid", params.len()));
        }
    }
    match graphs {
        Graphs::One(g) => {
            params.push(key::key_text(g));
            out.push(format!("g = ${}::uuid", params.len()));
        }
        Graphs::Named => out.push(format!("g <> '{}'::uuid", key::key_text(&DEFAULT_GRAPH))),
        Graphs::All => (),
    }
    if out.is_empty() {
        "true".to_owned()
    } else {
        out.join(" AND ")
    }
}

/// The store's quads of a pattern
pub fn store_quads(host: &Host, s: Option<&Key>, p: Option<&Key>, o: Option<&Key>, graphs: Graphs<'_>) -> EResult<u64> {
    let mut n = 0;
    for quad in host.quads(s, p, o, graphs)? {
        quad?;
        n += 1;
        if n >= STORE_COUNT {
            let mut params = Vec::new();
            let sql = format!("SELECT 1 FROM kb.quads WHERE {}", conditions(s, p, o, graphs, &mut params));
            return Ok(n.max(host.sql_estimate(&sql, &params)? as u64));
        }
    }
    Ok(n)
}

/// A predicate's quads in the store, and their distinct subjects and objects
pub fn store_predicate(host: &Host, p: &Key, graphs: Graphs<'_>) -> EResult<(u64, u64, u64)> {
    let mut subjects: FxHashSet<Key> = FxHashSet::default();
    let mut objects: FxHashSet<Key> = FxHashSet::default();
    let mut n = 0;
    for quad in host.quads(None, Some(p), None, graphs)? {
        let q = quad?;
        subjects.insert(q[0]);
        objects.insert(q[2]);
        n += 1;
        if n >= STORE_COUNT {
            let mut params = Vec::new();
            let cond = conditions(None, Some(p), None, graphs, &mut params);
            let triples = host.sql_estimate(&format!("SELECT 1 FROM kb.quads WHERE {cond}"), &params)?;
            let s = host.sql_estimate(&format!("SELECT DISTINCT s FROM kb.quads WHERE {cond}"), &params)?;
            let o = host.sql_estimate(&format!("SELECT DISTINCT o FROM kb.quads WHERE {cond}"), &params)?;
            return Ok((
                n.max(triples as u64),
                (subjects.len() as u64).max(s as u64),
                (objects.len() as u64).max(o as u64),
            ));
        }
    }
    Ok((n, subjects.len() as u64, objects.len() as u64))
}

/// The store's distinct predicates, by the planner
pub fn store_predicates(host: &Host, graphs: Graphs<'_>) -> EResult<u64> {
    let mut params = Vec::new();
    let sql = format!("SELECT DISTINCT p FROM kb.quads WHERE {}", conditions(None, None, None, graphs, &mut params));
    Ok(host.sql_estimate(&sql, &params)? as u64)
}

/// The store's named graphs
pub fn store_graphs(host: &Host) -> EResult<u64> {
    let mut n = 0;
    for g in host.graphs()? {
        g?;
        n += 1;
        if n >= STORE_COUNT {
            return Ok(n.max(host.sql_estimate("SELECT 1 FROM kb.graphs", &[])? as u64));
        }
    }
    Ok(n)
}
