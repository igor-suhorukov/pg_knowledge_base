// SPDX-License-Identifier: Apache-2.0
//! Basic graph patterns over R2RML's tables as one SQL query (docs/design.md
//! §4.4).
//!
//! Before spareval evaluates a query, each of its basic graph patterns is
//! cut: the triple patterns the mappings answer -- a constant predicate, or
//! IRIs a FILTER restricts a variable one to, terms whose columns the SQL can
//! give, and no more of the store's quads than STORE_ROWS, which go into the
//! SQL as VALUES -- that share variables become one SQL query, a SERVICE the
//! engine answers itself (SERVICE_PREFIX and a number), and spareval
//! evaluates the rest, and joins, as before.  A
//! pattern's SQL is the UNION ALL of what its units' queries read (eval.rs),
//! each SELECT giving, for a variable two patterns share, a tag and a key --
//! 'I' and an IRI column's value, a blank node's mapping and label, a
//! literal's kind and lexical form -- on which PostgreSQL joins the patterns,
//! with its own plans and indexes.  The rows' terms are made as eval.rs makes
//! them and checked against the patterns' constants and against each other,
//! so the keys only have to never miss a match: a variable two patterns share
//! whose terms no key tells -- an IRI made by a template, a literal of a
//! datatype a lexical form does not fix -- keeps its pattern out.
//!
//! The SERVICE takes besides the OPTIONALs of basic graph patterns after a
//! part, as LEFT JOINs whose conditions are exact; the EXISTS of the FILTERs
//! and BINDs over it, as columns of its rows; and what those FILTERs ask of
//! its core's variables, as conditions (Restriction).  Its SQL is planned
//! without nested loops: PostgreSQL has no statistics of the keys' columns.
//!
//! A property path of the default graph joined to a basic graph pattern goes
//! into the core too, when the triple patterns of the part bind both its
//! ends, or they are constants: a join of the two on the pairs the path
//! links, which SQL follows from the terms of one end -- the edges of each
//! link its units' and the store's, keyed; a sequence a join of its steps;
//! an alternative their UNION; one or more steps a recursive CTE; zero
//! steps the end's own terms (Steps, PathSql).  spareval would follow it
//! for each solution, or for every node of the graph.  One end may be the
//! path's alone when the path is no sequence: the SQL then gives its term,
//! by its tag and its key (key_term()).  A sequence, a link and an inverse
//! link at a path's top the parser has made triple patterns already, through
//! a fresh variable.
//!
//! kb.r2rml_pushdown = off evaluates every pattern apart, as before.

use std::collections::HashMap;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use oxrdf::vocab::xsd;
use oxrdf::{Literal, NamedNode, Term, Variable};
use oxsdatatypes::Integer;
use rustc_hash::FxHashSet;
use siphasher::sip128::{Hasher128, SipHasher24};
use std::hash::Hasher;
use spareval::{QueryEvaluationError, QueryEvaluator, QuerySolutionIter, ServiceHandler};
use spargebra::Query;
use spargebra::algebra::{PropertyPathExpression, QueryExpression};
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};

use crate::error::{EResult, EngineError};
use crate::ffi::{KB_SQL_BOOLEAN, KB_SQL_HASH_JOINS, KB_SQL_INTEGER, KB_SQL_STRING};
use crate::host::{Graphs, Host};
use crate::key::{self, DEFAULT_GRAPH, Key};
use crate::r2rml::compile::{CGraph, CMap, CPart, CValue, ColRef, Compiled};
use crate::r2rml::eval::{self, Bind, Context, GraphChoice, Row, VirtualSet};
use crate::r2rml::model::TermType;
use crate::terms;

/// The SERVICE names of the SQL queries a query was given: this, then a number
pub const SERVICE_PREFIX: &str = "urn:x-pg-knowledge-base:r2rml:";

/// The store's quads a pattern may match and still go to SQL, as VALUES
const STORE_ROWS: usize = 2000;

/// The store's quads read for a pattern whose variables FILTERs restrict, of
/// which no more than STORE_ROWS may meet the restrictions
const STORE_PROBE: usize = 20_000;

/// The IRIs a FILTER may restrict a variable predicate to, for its pattern to
/// go to SQL as the union of theirs
const MAX_PREDICATES: usize = 64;

static ENABLED: AtomicBool = AtomicBool::new(true);

/// kb.r2rml_pushdown, as the call's configuration has it
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

// A pattern's positions: subject, predicate, object, and its graph, which a
// pattern of the default graph has none of
const S: usize = 0;
const P: usize = 1;
const O: usize = 2;
const G: usize = 3;
const ROLES: [usize; 4] = [S, P, O, G];

#[derive(Clone, Debug, PartialEq)]
enum Pos {
    Const(Term),
    Var(Variable),
}

/// A unit of a mapping that may make quads of a pattern, with the graph map it
/// makes them in
#[derive(Clone)]
struct Branch {
    compiled: Rc<Compiled>,
    unit: usize,
    choice: GraphChoice,
}

impl Branch {
    fn ctx(&self) -> Context<'_> {
        Context {
            compiled: &self.compiled,
            unit: &self.compiled.units[self.unit],
        }
    }

    /// The term map of a position: None for the default graph's
    fn map(&self, role: usize) -> Option<&CMap> {
        let unit = &self.compiled.units[self.unit];
        match role {
            S => Some(&unit.subject),
            P => Some(&unit.predicate),
            O => Some(&unit.object),
            _ => match self.choice {
                GraphChoice::Map(i) => match &unit.graphs[i] {
                    CGraph::Map(m) => Some(m),
                    CGraph::Default => None,
                },
                GraphChoice::Default => None,
            },
        }
    }
}

/// A triple pattern the SQL can answer, or a property path between two
/// terms of its part's core
struct Pattern {
    source: Source,
    /// subject, predicate, object, graph; the graph None in the default graph,
    /// the predicate None for a path
    pos: [Option<Pos>; 4],
    branches: Vec<Branch>,
    /// The store's quads that match it, as terms; the graph None for the
    /// default graph
    store: Vec<[Option<Term>; 4]>,
    /// A path's steps: the pattern makes no term of its own, it joins its ends
    /// that the core's triple patterns bind (anchored())
    path: Option<Steps>,
}

/// What a pattern of a part is in the query
#[derive(Clone)]
enum Source {
    Triple(TriplePattern),
    Path(TermPattern, PropertyPathExpression, TermPattern),
}

/// A property path as SQL follows it, the way it goes: each link the edges
/// its units and the store's quads in the default graph make, from the
/// subject to the object, or back when inverse
#[derive(Clone)]
enum Steps {
    Link {
        predicate: NamedNode,
        branches: Vec<Branch>,
        /// the store's quads' subjects and objects
        store: Vec<[Term; 2]>,
        inverse: bool,
    },
    Sequence(Box<Steps>, Box<Steps>),
    Alternative(Box<Steps>, Box<Steps>),
    OneOrMore(Box<Steps>),
    ZeroOrMore(Box<Steps>),
    ZeroOrOne(Box<Steps>),
}

impl Steps {
    /// The same pairs, the other way
    fn inverted(&self) -> Steps {
        let b = |x: &Steps| Box::new(x.inverted());
        match self {
            Steps::Link {
                predicate,
                branches,
                store,
                inverse,
            } => Steps::Link {
                predicate: predicate.clone(),
                branches: branches.clone(),
                store: store.clone(),
                inverse: !inverse,
            },
            Steps::Sequence(x, y) => Steps::Sequence(b(y), b(x)),
            Steps::Alternative(x, y) => Steps::Alternative(b(x), b(y)),
            Steps::OneOrMore(x) => Steps::OneOrMore(b(x)),
            Steps::ZeroOrMore(x) => Steps::ZeroOrMore(b(x)),
            Steps::ZeroOrOne(x) => Steps::ZeroOrOne(b(x)),
        }
    }

    /// Whether path_sql() follows the steps, from the terms of a seed or from
    /// any: zero steps need the seed's, which the steps a closure repeats have
    /// not
    fn followable(&self, seeded: bool) -> bool {
        match self {
            Steps::Link { .. } => true,
            Steps::Sequence(x, y) => x.followable(seeded) && y.followable(true),
            Steps::Alternative(x, y) => x.followable(seeded) && y.followable(seeded),
            Steps::OneOrMore(x) => x.followable(seeded) && x.followable(false),
            Steps::ZeroOrMore(x) => seeded && x.followable(true) && x.followable(false),
            Steps::ZeroOrOne(x) => seeded && x.followable(true),
        }
    }

    /// The links' units, each once
    fn branches<'s>(&'s self, out: &mut Vec<&'s Branch>) {
        match self {
            Steps::Link { branches, .. } => out.extend(branches.iter()),
            Steps::Sequence(x, y) | Steps::Alternative(x, y) => {
                x.branches(out);
                y.branches(out);
            }
            Steps::OneOrMore(x) | Steps::ZeroOrMore(x) | Steps::ZeroOrOne(x) => x.branches(out),
        }
    }
}

impl Source {
    /// The pattern again, as spareval would evaluate it
    fn expression(self) -> QueryExpression {
        match self {
            Source::Triple(tp) => QueryExpression::Bgp { patterns: vec![tp] },
            Source::Path(subject, path, object) => QueryExpression::Path { subject, path, object },
        }
    }
}

/// Whether the ends of a path pattern are constants, or variables a triple
/// pattern of the part binds as a subject or an object -- nodes of the
/// graph, whose zero steps spareval gives as the SQL does -- but for one, the
/// path's own, when the path is not a sequence: then its pairs are distinct,
/// whichever end spareval follows it from, where a sequence gives a pair
/// once for each way
fn anchored(p: &Pattern, part: &[Pattern]) -> bool {
    let bound = |r: usize| match &p.pos[r] {
        Some(Pos::Var(v)) => part.iter().any(|q| {
            q.path.is_none() && [S, O].iter().any(|&x| matches!(&q.pos[x], Some(Pos::Var(w)) if w == v))
        }),
        _ => true,
    };
    match (bound(S), bound(O)) {
        (true, true) => true,
        (false, false) => false,
        _ => !matches!(p.path, Some(Steps::Sequence(..))) && p.pos[S] != p.pos[O],
    }
}

impl Pattern {
    fn variables(&self) -> impl Iterator<Item = (usize, &Variable)> {
        self.pos.iter().enumerate().filter_map(|(r, p)| match p {
            Some(Pos::Var(v)) => Some((r, v)),
            _ => None,
        })
    }

    fn roles(&self) -> impl Iterator<Item = usize> + '_ {
        ROLES.into_iter().filter(|r| self.pos[*r].is_some())
    }
}

/// What a SERVICE of the rewrite answers: connected patterns of a basic
/// graph pattern, the OPTIONALs that follow it, each a basic graph pattern
/// joined to the part on variables the part binds, and the EXISTS of the
/// FILTERs and BINDs over it
struct Part {
    core: Vec<Pattern>,
    optionals: Vec<Vec<Pattern>>,
    /// What FILTERs over the part ask of the core's variables (Rewriter's
    /// restrict)
    restrictions: Vec<(Variable, Restriction)>,
    /// EXISTS { P } of an expression over the part's solutions: a variable
    /// the part binds to true when its core's row has a match of P, and P's
    /// patterns (Rewriter::exists())
    exists: Vec<(Variable, Vec<Pattern>)>,
}

/// A condition of a FILTER the SQL also checks on a variable of a part's
/// core, to read fewer rows; spareval still evaluates the FILTER
#[derive(Clone, Debug)]
enum Restriction {
    /// STRSTARTS(STR(?v), "p"): an IRI, or a string, that begins with p
    Prefix(String),
    /// ?v = <iri>, sameTerm(?v, <iri>), ?v IN (<iri>, ...): one of these
    OneOf(Vec<Term>),
}

/// The restrictions the conjuncts of a FILTER's expression give
fn restrictions(e: &spargebra::algebra::Expression, out: &mut Vec<(Variable, Restriction)>) {
    use spargebra::algebra::Expression as X;
    use spargebra::vocab::sparql;
    let variable = |e: &X| match e {
        X::Variable(v) => Some(v.clone()),
        X::FunctionCall(f, a) if *f == sparql::STR && a.len() == 1 => match &a[0] {
            X::Variable(v) => Some(v.clone()),
            _ => None,
        },
        _ => None,
    };
    match e {
        X::And(a, b) => {
            restrictions(a, out);
            restrictions(b, out);
        }
        X::FunctionCall(f, args) if *f == sparql::STRSTARTS && args.len() == 2 => {
            let prefix = match &args[1] {
                X::Literal(l) if l.language().is_none() && *l.datatype() == xsd::STRING => Some(l.value().to_owned()),
                X::FunctionCall(g, a) if *g == sparql::STR && a.len() == 1 => match &a[0] {
                    X::NamedNode(n) => Some(n.as_str().to_owned()),
                    _ => None,
                },
                _ => None,
            };
            if let (Some(v), Some(p)) = (variable(&args[0]), prefix) {
                out.push((v, Restriction::Prefix(p)));
            }
        }
        X::FunctionCall(f, args)
            if (*f == sparql::EQUALS || *f == sparql::SAME_TERM || *f == sparql::RDF_TERM_EQUAL) && args.len() == 2 =>
        {
            if let (X::Variable(v), X::NamedNode(n)) | (X::NamedNode(n), X::Variable(v)) = (&args[0], &args[1]) {
                out.push((v.clone(), Restriction::OneOf(vec![n.clone().into()])));
            }
        }
        X::In(a, list) => {
            if let X::Variable(v) = a.as_ref() {
                let iris: Option<Vec<Term>> = list
                    .iter()
                    .map(|e| match e {
                        X::NamedNode(n) => Some(n.clone().into()),
                        _ => None,
                    })
                    .collect();
                if let Some(iris) = iris {
                    out.push((v.clone(), Restriction::OneOf(iris)));
                }
            }
        }
        _ => (),
    }
}

/// The SQL of a part, and how its rows make the solutions
pub struct Plan {
    sql: String,
    params: Vec<String>,
    variables: Arc<[Variable]>,
    /// The part's patterns, the core's first, each with the number of its
    /// group: 0 the core, n the n-th OPTIONAL
    patterns: Vec<(usize, Pattern)>,
    /// For each pattern: the column of its branch's number, and, for each
    /// position, the first of its value columns and how many there are
    columns: Vec<(usize, [(usize, usize); 4])>,
    /// For each EXISTS: its column, 't' or NULL, and its variable's place
    exists: Vec<(usize, usize)>,
    /// For each path's end no pattern of the core binds: the columns of its
    /// tag and its key, and its variable's place
    ends: Vec<(usize, usize, usize)>,
    /// The mappings, whose blank nodes an end's key may be of
    mappings: Vec<Rc<Compiled>>,
}

// ---------------------------------------------------------------------
// The rewrite
// ---------------------------------------------------------------------

/// The query with its basic graph patterns cut, the parts SQL answers made
/// SERVICEs, and the plans of those, in the order of their numbers.  The
/// caller gives each to spareval through handlers().
pub fn rewrite(query: &mut Query, host: &Host, vset: &VirtualSet) -> EResult<Vec<Rc<Plan>>> {
    let expression = match query {
        Query::Select(q) => &mut q.expression,
        Query::Construct(q) => &mut q.expression,
        Query::Describe(q) => &mut q.pattern,
        Query::Ask(q) => &mut q.expression,
    };
    let e = flatten(std::mem::replace(expression, QueryExpression::Bgp { patterns: Vec::new() }));
    let mut rewriter = Rewriter {
        host: *host,
        vset,
        parts: Vec::new(),
        restrict: Vec::new(),
        taken: used(&e),
        fresh: 0,
    };
    *expression = rewriter.expression(e)?;
    if crate::stats::enabled() {
        let caps = rewriter
            .parts
            .iter()
            .map(|(part, _)| rows_at_most(part, host))
            .collect::<EResult<Vec<_>>>()?;
        cap(expression, &caps);
    }
    rewriter.parts.into_iter().map(|(part, _)| plan(part, host).map(Rc::new)).collect()
}

/// The rows a part's SQL may give, for sparopt, which estimates a SERVICE by
/// its inner pattern -- the join of the core's patterns by the mappings'
/// averages, without the restrictions of the FILTERs the SQL applies: the
/// fewest PostgreSQL's plans expect of one of its core's patterns, with its
/// restrictions.  A join on a variable gives no more rows than its most
/// selective side has, but for the values of a variable several rows of the
/// other side share.
fn rows_at_most(part: &Part, host: &Host) -> EResult<Option<u64>> {
    let mut mappings: Vec<Rc<Compiled>> = Vec::new();
    for p in &part.core {
        for b in &p.branches {
            if !mappings.iter().any(|x| Rc::ptr_eq(x, &b.compiled)) {
                mappings.push(Rc::clone(&b.compiled));
            }
        }
    }
    let mut least: Option<f64> = None;
    for p in part.core.iter().filter(|p| p.path.is_none()) {
        let mut params = Vec::new();
        let union = pattern_sql(p, &[], &[0; 4], &part.restrictions, &mappings, &mut params)?;
        let rows = host.sql_estimate(&format!("SELECT 1 FROM ({union}) AS kb_e"), &params)?;
        least = Some(least.map_or(rows, |l: f64| l.min(rows)));
    }
    Ok(least.map(|r| r.ceil().max(1.0) as u64))
}

/// Each SERVICE of the rewrite in e, its inner pattern in a Slice of the rows
/// its SQL may give: sparopt's estimate of it
fn cap(e: &mut QueryExpression, caps: &[Option<u64>]) {
    use QueryExpression as E;
    if let Some(n) = service_number(e) {
        if let (Some(Some(limit)), E::Service { inner, .. }) = (caps.get(n), &mut *e) {
            let old = std::mem::replace(inner.as_mut(), E::Bgp { patterns: Vec::new() });
            **inner = E::Slice {
                inner: Box::new(old),
                offset: 0,
                limit: Some(*limit),
            };
        }
        return;
    }
    match e {
        E::Join { left, right }
        | E::LeftJoin { left, right, .. }
        | E::Lateral { left, right }
        | E::Union { left, right }
        | E::Minus { left, right } => {
            cap(left, caps);
            cap(right, caps);
        }
        E::Filter { inner, .. }
        | E::Graph { inner, .. }
        | E::Extend { inner, .. }
        | E::OrderBy { inner, .. }
        | E::Project { inner, .. }
        | E::Distinct { inner }
        | E::Reduced { inner }
        | E::Slice { inner, .. }
        | E::Group { inner, .. }
        | E::Service { inner, .. } => cap(inner, caps),
        E::Bgp { .. } | E::Path { .. } | E::Values { .. } => (),
    }
}

/// The expression with each group's joins flattened as join() does them
fn flatten(e: QueryExpression) -> QueryExpression {
    use QueryExpression as E;
    let f = |x: Box<E>| Box::new(flatten(*x));
    match e {
        E::Join { left, right } => join(flatten(*left), flatten(*right)),
        E::LeftJoin {
            left,
            right,
            expression,
        } => left_join(flatten(*left), flatten(*right), expression),
        E::Lateral { left, right } => E::Lateral {
            left: f(left),
            right: f(right),
        },
        E::Union { left, right } => E::Union {
            left: f(left),
            right: f(right),
        },
        E::Minus { left, right } => E::Minus {
            left: f(left),
            right: f(right),
        },
        E::Filter { expr, inner } => E::Filter { expr, inner: f(inner) },
        E::Graph { name, inner } => E::Graph { name, inner: f(inner) },
        E::Extend {
            inner,
            variable,
            expression,
        } => E::Extend {
            inner: f(inner),
            variable,
            expression,
        },
        E::OrderBy { inner, expression } => E::OrderBy {
            inner: f(inner),
            expression,
        },
        E::Project { inner, variables } => E::Project {
            inner: f(inner),
            variables,
        },
        E::Distinct { inner } => E::Distinct { inner: f(inner) },
        E::Reduced { inner } => E::Reduced { inner: f(inner) },
        E::Slice { inner, offset, limit } => E::Slice {
            inner: f(inner),
            offset,
            limit,
        },
        E::Group {
            inner,
            variables,
            aggregates,
        } => E::Group {
            inner: f(inner),
            variables,
            aggregates,
        },
        other @ (E::Bgp { .. } | E::Path { .. } | E::Values { .. } | E::Service { .. }) => other,
    }
}

/// The evaluator with a handler for each plan's SERVICE
pub fn handlers(mut evaluator: QueryEvaluator, host: &Host, plans: &[Rc<Plan>]) -> QueryEvaluator {
    for (i, plan) in plans.iter().enumerate() {
        evaluator = evaluator.with_service_handler(
            NamedNode::new_unchecked(format!("{SERVICE_PREFIX}{i}")),
            Handler(OneThread {
                host: *host,
                plan: Rc::clone(plan),
            }),
        );
    }
    evaluator
}

struct Rewriter<'a> {
    host: Host,
    vset: &'a VirtualSet,
    parts: Vec<(Part, Option<NamedNodePattern>)>,
    /// What the FILTERs over the expression being rewritten ask of its
    /// variables, while its solutions keep their values to them
    restrict: Vec<(Variable, Restriction)>,
    /// The query's variables, and the number of the next one made
    taken: FxHashSet<Variable>,
    fresh: usize,
}

/// The number of a SERVICE of the rewrite
fn service_number(e: &QueryExpression) -> Option<usize> {
    match e {
        QueryExpression::Service {
            name: NamedNodePattern::NamedNode(n),
            ..
        } => n.as_str().strip_prefix(SERVICE_PREFIX)?.parse().ok(),
        _ => None,
    }
}

/// SERVICE n of the rewrite, of inner
fn service(n: usize, inner: QueryExpression) -> QueryExpression {
    QueryExpression::Service {
        name: NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!("{SERVICE_PREFIX}{n}"))),
        inner: Box::new(inner),
        silent: false,
    }
}

/// The triple patterns of e when it is a basic graph pattern of the graph
/// given
fn bgp_of<'e>(e: &'e QueryExpression, graph: Option<&NamedNodePattern>) -> Option<&'e [TriplePattern]> {
    match (e, graph) {
        (QueryExpression::Bgp { patterns }, None) => Some(patterns),
        (QueryExpression::Graph { name, inner }, Some(g)) if name == g => match inner.as_ref() {
            QueryExpression::Bgp { patterns } => Some(patterns),
            _ => None,
        },
        _ => None,
    }
}

/// e's graph and triple patterns when it is a basic graph pattern
fn bgp_in(e: &QueryExpression) -> Option<(Option<NamedNodePattern>, Vec<TriplePattern>)> {
    match e {
        QueryExpression::Bgp { patterns } => Some((None, patterns.clone())),
        QueryExpression::Graph { name, inner } => match inner.as_ref() {
            QueryExpression::Bgp { patterns } => Some((Some(name.clone()), patterns.clone())),
            _ => None,
        },
        _ => None,
    }
}

fn triple_variables(triples: &[TriplePattern], graph: Option<&NamedNodePattern>) -> FxHashSet<Variable> {
    let mut out = FxHashSet::default();
    for tp in triples {
        for t in [&tp.subject, &tp.object] {
            if let TermPattern::Variable(v) = t {
                out.insert(v.clone());
            }
        }
        if let NamedNodePattern::Variable(v) = &tp.predicate {
            out.insert(v.clone());
        }
    }
    if let Some(NamedNodePattern::Variable(v)) = graph {
        out.insert(v.clone());
    }
    out
}

/// Whether a group of patterns joins a core in SQL: the variables it shares
/// with the core, and within itself, have keys on both sides
fn joins_core(core: &[Pattern], group: &[Pattern]) -> bool {
    let core_vars: FxHashSet<&Variable> = core.iter().flat_map(|p| p.variables().map(|(_, v)| v)).collect();
    let group_vars: FxHashSet<&Variable> = group.iter().flat_map(|p| p.variables().map(|(_, v)| v)).collect();
    let within = shared_variables(group.iter());
    group
        .iter()
        .all(|p| p.variables().all(|(r, v)| !(core_vars.contains(v) || within.contains(v)) || keyable(p, r)))
        && core
            .iter()
            .all(|p| p.variables().all(|(r, v)| !group_vars.contains(v) || keyable(p, r)))
}

/// Each EXISTS of an expression given to f, outside of the patterns of its
/// EXISTS
fn exists_mut(
    e: &mut spargebra::algebra::Expression,
    f: &mut impl FnMut(&mut spargebra::algebra::Expression) -> EResult<()>,
) -> EResult<()> {
    use spargebra::algebra::Expression as X;
    match e {
        X::Exists(_) => f(e),
        X::Or(a, b) | X::And(a, b) => {
            exists_mut(a, f)?;
            exists_mut(b, f)
        }
        X::In(a, list) => {
            exists_mut(a, f)?;
            list.iter_mut().try_for_each(|x| exists_mut(x, f))
        }
        X::If(a, b, c) => {
            exists_mut(a, f)?;
            exists_mut(b, f)?;
            exists_mut(c, f)
        }
        X::Coalesce(list) | X::FunctionCall(_, list) => list.iter_mut().try_for_each(|x| exists_mut(x, f)),
        X::NamedNode(_) | X::Literal(_) | X::Variable(_) | X::Bound(_) => Ok(()),
    }
}

/// SERVICE n of e, along the paths Rewriter::serving() takes, its inner made
/// to bind m too: spareval tells what a SERVICE binds from its inner, and
/// would take BOUND(?m) for false
fn mark(e: &mut QueryExpression, n: usize, m: &Variable) -> bool {
    if service_number(e) == Some(n) {
        if let QueryExpression::Service { inner, .. } = e {
            let left = std::mem::replace(inner.as_mut(), QueryExpression::Bgp { patterns: Vec::new() });
            **inner = QueryExpression::LeftJoin {
                left: Box::new(left),
                right: Box::new(QueryExpression::Extend {
                    inner: empty(),
                    variable: m.clone(),
                    expression: spargebra::algebra::Expression::Literal(Literal::from(true)),
                }),
                expression: None,
            };
        }
        return true;
    }
    match e {
        QueryExpression::Join { left, right } => mark(left, n, m) || mark(right, n, m),
        QueryExpression::LeftJoin { left, .. }
        | QueryExpression::Filter { inner: left, .. }
        | QueryExpression::Extend { inner: left, .. } => mark(left, n, m),
        _ => false,
    }
}

/// Whether the operands of a join's tree are a basic graph pattern and paths,
/// of the default graph (Rewriter::bgp())
fn joins_paths(left: &QueryExpression, right: &QueryExpression) -> bool {
    fn walk(e: &QueryExpression, bgp: &mut bool, path: &mut bool) {
        match e {
            QueryExpression::Join { left, right } => {
                walk(left, bgp, path);
                walk(right, bgp, path);
            }
            QueryExpression::Bgp { .. } => *bgp = true,
            QueryExpression::Path { .. } => *path = true,
            _ => (),
        }
    }
    let (mut bgp, mut path) = (false, false);
    walk(left, &mut bgp, &mut path);
    walk(right, &mut bgp, &mut path);
    bgp && path
}

/// The operands of a tree of joins, and the tree again
fn operands(e: QueryExpression, out: &mut Vec<QueryExpression>) {
    match e {
        QueryExpression::Join { left, right } => {
            operands(*left, out);
            operands(*right, out);
        }
        other => out.push(other),
    }
}

fn join_all(mut ops: Vec<QueryExpression>) -> QueryExpression {
    let first = ops.remove(0);
    ops.into_iter().fold(first, |l, r| QueryExpression::Join {
        left: Box::new(l),
        right: Box::new(r),
    })
}

/// Whether the SQL's conditions for a pattern's constants are the whole
/// match: a column's value, or every capture of a template -- so that the
/// rows of an OPTIONAL need no check that could turn a match into none
fn exact(p: &Pattern) -> bool {
    p.branches.iter().all(|b| {
        let ctx = b.ctx();
        ROLES.iter().all(|&r| match (&p.pos[r], b.map(r)) {
            (Some(Pos::Const(t)), Some(m)) => match &m.value {
                CValue::Constant(..) | CValue::Column(_) => true,
                CValue::Template(parts) => {
                    let n = parts.iter().filter(|x| matches!(x, CPart::Column(_))).count();
                    match ctx.bind(m, t) {
                        Bind::Columns(c) => c.len() == n,
                        Bind::Any => n == 0,
                        Bind::Never => true,
                    }
                }
            },
            _ => true,
        })
    })
}

impl Rewriter<'_> {
    /// e rewritten.  A FILTER's restrictions go down to the basic graph
    /// patterns whose solutions give its variables their values: through
    /// joins, UNIONs, BINDs and FILTERs, and the left sides of OPTIONALs,
    /// MINUSes and LATERALs.
    fn expression(&mut self, e: QueryExpression) -> EResult<QueryExpression> {
        let b = |x: QueryExpression| Box::new(x);
        Ok(match e {
            QueryExpression::Bgp { patterns } => self.bgp(patterns, Vec::new(), None)?,
            QueryExpression::Graph { name, inner } => match *inner {
                QueryExpression::Bgp { patterns } => self.bgp(patterns, Vec::new(), Some(name))?,
                // a graph's other patterns are evaluated as before
                other => QueryExpression::Graph {
                    name,
                    inner: b(other),
                },
            },
            QueryExpression::Join { left, right } if !joins_paths(&left, &right) => QueryExpression::Join {
                left: b(self.expression(*left)?),
                right: b(self.expression(*right)?),
            },
            QueryExpression::Join { left, right } => {
                let mut ops = Vec::new();
                operands(QueryExpression::Join { left, right }, &mut ops);
                // the group's basic graph pattern and the paths joined to it
                let mut triples = Vec::new();
                let mut paths = Vec::new();
                let mut others = Vec::new();
                for op in ops {
                    match op {
                        QueryExpression::Bgp { patterns } => triples.extend(patterns),
                        QueryExpression::Path { subject, path, object } => paths.push((subject, path, object)),
                        other => others.push(other),
                    }
                }
                let mut out = vec![self.bgp(triples, paths, None)?];
                for op in others {
                    out.push(self.expression(op)?);
                }
                join_all(out)
            }
            QueryExpression::LeftJoin {
                left,
                right,
                expression,
            } => {
                let left = self.expression(*left)?;
                let restrict = self.restrict.clone();
                self.apart(|s| {
                    Ok(match s.optional(left, *right, expression, &restrict)? {
                        Ok(e) => e,
                        Err((left, right, expression)) => QueryExpression::LeftJoin {
                            left: b(left),
                            right: b(s.expression(right)?),
                            expression,
                        },
                    })
                })?
            }
            QueryExpression::Lateral { left, right } => QueryExpression::Lateral {
                left: b(self.expression(*left)?),
                right: b(self.apart(|s| s.expression(*right))?),
            },
            QueryExpression::Filter { expr, inner } => {
                let n = self.restrict.len();
                restrictions(&expr, &mut self.restrict);
                let result = self.expression(*inner).and_then(|mut inner| {
                    let expr = self.exists(expr, &mut inner)?;
                    Ok(QueryExpression::Filter { expr, inner: b(inner) })
                });
                self.restrict.truncate(n);
                result?
            }
            QueryExpression::Union { left, right } => QueryExpression::Union {
                left: b(self.expression(*left)?),
                right: b(self.expression(*right)?),
            },
            // the BIND's variable is none of its inner's
            QueryExpression::Extend {
                inner,
                variable,
                expression,
            } => {
                let mut inner = self.expression(*inner)?;
                let expression = self.exists(expression, &mut inner)?;
                QueryExpression::Extend {
                    inner: b(inner),
                    variable,
                    expression,
                }
            }
            QueryExpression::Minus { left, right } => QueryExpression::Minus {
                left: b(self.expression(*left)?),
                right: b(self.apart(|s| s.expression(*right))?),
            },
            QueryExpression::OrderBy { inner, expression } => QueryExpression::OrderBy {
                inner: b(self.apart(|s| s.expression(*inner))?),
                expression,
            },
            QueryExpression::Project { inner, variables } => QueryExpression::Project {
                inner: b(self.apart(|s| s.expression(*inner))?),
                variables,
            },
            QueryExpression::Distinct { inner } => QueryExpression::Distinct {
                inner: b(self.apart(|s| s.expression(*inner))?),
            },
            QueryExpression::Reduced { inner } => QueryExpression::Reduced {
                inner: b(self.apart(|s| s.expression(*inner))?),
            },
            QueryExpression::Slice {
                inner,
                offset,
                limit,
            } => QueryExpression::Slice {
                inner: b(self.apart(|s| s.expression(*inner))?),
                offset,
                limit,
            },
            QueryExpression::Group {
                inner,
                variables,
                aggregates,
            } => QueryExpression::Group {
                inner: b(self.apart(|s| s.expression(*inner))?),
                variables,
                aggregates,
            },
            other @ (QueryExpression::Path { .. }
            | QueryExpression::Values { .. }
            | QueryExpression::Service { .. }) => other,
        })
    }

    /// f with no FILTER's restrictions: for an expression whose solutions
    /// are not those the FILTERs are on
    fn apart<T>(&mut self, f: impl FnOnce(&mut Self) -> EResult<T>) -> EResult<T> {
        let outer = std::mem::take(&mut self.restrict);
        let result = f(self);
        self.restrict = outer;
        result
    }

    /// A basic graph pattern, in the default graph or in GRAPH name, and the
    /// paths of the default graph joined to it: its parts SQL answers as
    /// SERVICEs, joined to what is left of it.  GRAPH distributes over a basic
    /// graph pattern's join, so each part keeps the GRAPH inside its SERVICE.
    /// A path goes into a part whose triple patterns bind its ends, or one of
    /// them (anchored()); the others are spareval's.
    fn bgp(
        &mut self,
        patterns: Vec<TriplePattern>,
        paths: Vec<(TermPattern, PropertyPathExpression, TermPattern)>,
        graph: Option<NamedNodePattern>,
    ) -> EResult<QueryExpression> {
        let mut analysed = Vec::new();
        let mut rest = Vec::new();
        for tp in patterns {
            match self.pattern(&tp, graph.as_ref(), &self.restrict)? {
                Some(p) => analysed.push(p),
                None => rest.push(tp),
            }
        }
        let mut rest_paths: Vec<QueryExpression> = Vec::new();
        for (subject, path, object) in paths {
            match self.path_pattern(&subject, &path, &object, graph.as_ref())? {
                Some(p) => analysed.push(p),
                None => rest_paths.push(QueryExpression::Path { subject, path, object }),
            }
        }
        let (pushed, left) = loop {
            let (pushed, left) = parts(analysed);
            let loose: Vec<(usize, usize)> = pushed
                .iter()
                .enumerate()
                .flat_map(|(i, part)| {
                    part.iter()
                        .enumerate()
                        .filter(|(_, p)| p.path.is_some() && !anchored(p, part))
                        .map(move |(j, _)| (i, j))
                })
                .collect();
            if loose.is_empty() {
                break (pushed, left);
            }
            // the paths whose ends their parts do not bind are spareval's, the
            // others' parts are made again
            analysed = Vec::new();
            for (i, part) in pushed.into_iter().enumerate() {
                for (j, p) in part.into_iter().enumerate() {
                    if loose.contains(&(i, j)) {
                        rest_paths.push(p.source.expression());
                    } else {
                        analysed.push(p);
                    }
                }
            }
            analysed.extend(left);
        };
        for p in left {
            match p.source {
                Source::Triple(tp) => rest.push(tp),
                source @ Source::Path(..) => rest_paths.push(source.expression()),
            }
        }
        let within = |inner: QueryExpression| match &graph {
            Some(name) => QueryExpression::Graph {
                name: name.clone(),
                inner: Box::new(inner),
            },
            None => inner,
        };
        let mut result: Option<QueryExpression> = None;
        for part in pushed {
            let mut triples: Vec<TriplePattern> = Vec::new();
            let mut inner_paths: Vec<QueryExpression> = Vec::new();
            for p in &part {
                match &p.source {
                    Source::Triple(tp) => triples.push(tp.clone()),
                    source @ Source::Path(..) => inner_paths.push(source.clone().expression()),
                }
            }
            let mut inner = vec![QueryExpression::Bgp { patterns: triples }];
            inner.extend(inner_paths);
            let restrict = self.restrict.clone();
            let n = self.new_part(part, graph.clone(), &restrict);
            let service = service(n, within(join_all(inner)));
            result = Some(match result {
                None => service,
                Some(r) => QueryExpression::Join {
                    left: Box::new(r),
                    right: Box::new(service),
                },
            });
        }
        let mut out: Vec<QueryExpression> = result.into_iter().collect();
        if !rest.is_empty() || out.is_empty() {
            out.push(within(QueryExpression::Bgp { patterns: rest }));
        }
        out.extend(rest_paths.into_iter().map(within));
        Ok(join_all(out))
    }

    /// A property path of the default graph as the SQL can follow it, from
    /// either end: None in a graph of its own, with a literal subject, a
    /// negated property set, a link whose units do not key their subjects
    /// and objects, more than STORE_ROWS of the store's quads of a link, or
    /// zero steps a closure would repeat (Steps::followable()).
    fn path_pattern(
        &self,
        subject: &TermPattern,
        path: &PropertyPathExpression,
        object: &TermPattern,
        graph: Option<&NamedNodePattern>,
    ) -> EResult<Option<Pattern>> {
        if graph.is_some() {
            return Ok(None);
        }
        let term = |t: &TermPattern| -> Option<Pos> {
            match t {
                TermPattern::NamedNode(n) => Some(Pos::Const(n.clone().into())),
                TermPattern::Literal(l) => Some(Pos::Const(l.clone().into())),
                TermPattern::Variable(v) => Some(Pos::Var(v.clone())),
                #[allow(unreachable_patterns)]
                _ => None,
            }
        };
        let (Some(s), Some(o)) = (term(subject), term(object)) else {
            return Ok(None);
        };
        if matches!(&s, Pos::Const(Term::Literal(_))) {
            return Ok(None);
        }
        for end in [&s, &o] {
            if let Pos::Const(t) = end {
                if term_key(t, &[]).is_none() {
                    return Ok(None);
                }
            }
        }
        let Some(steps) = self.steps(path, false)? else {
            return Ok(None);
        };
        if !(steps.followable(true) && steps.inverted().followable(true)) {
            return Ok(None);
        }
        Ok(Some(Pattern {
            source: Source::Path(subject.clone(), path.clone(), object.clone()),
            pos: [Some(s), None, Some(o), None],
            branches: Vec::new(),
            store: Vec::new(),
            path: Some(steps),
        }))
    }

    /// A path's steps, inverse the other way; None for one the SQL cannot
    /// follow (path_pattern())
    fn steps(&self, path: &PropertyPathExpression, inverse: bool) -> EResult<Option<Steps>> {
        use PropertyPathExpression as X;
        let b = |x: Steps| Box::new(x);
        Ok(Some(match path {
            X::Link(p) => match self.link(p, inverse)? {
                Some(link) => link,
                None => return Ok(None),
            },
            X::Inv(x) => match self.steps(x, !inverse)? {
                Some(x) => x,
                None => return Ok(None),
            },
            X::Seq(x, y) => {
                let (Some(x), Some(y)) = (self.steps(x, inverse)?, self.steps(y, inverse)?) else {
                    return Ok(None);
                };
                if inverse { Steps::Sequence(b(y), b(x)) } else { Steps::Sequence(b(x), b(y)) }
            }
            X::Alt(x, y) => {
                let (Some(x), Some(y)) = (self.steps(x, inverse)?, self.steps(y, inverse)?) else {
                    return Ok(None);
                };
                Steps::Alternative(b(x), b(y))
            }
            X::OneOrMorePath(x) | X::ZeroOrMorePath(x) | X::ZeroOrOnePath(x) => {
                let Some(inner) = self.steps(x, inverse)? else {
                    return Ok(None);
                };
                match path {
                    X::OneOrMorePath(_) => Steps::OneOrMore(b(inner)),
                    X::ZeroOrMorePath(_) => Steps::ZeroOrMore(b(inner)),
                    _ => Steps::ZeroOrOne(b(inner)),
                }
            }
            X::Nps(_) => return Ok(None),
        }))
    }

    /// A link's edges in the default graph: the units that make triples of
    /// its predicate, with keys for their subjects and objects, and the
    /// store's quads of it; None when a unit has no key, or the store more
    /// than STORE_ROWS quads
    fn link(&self, predicate: &NamedNode, inverse: bool) -> EResult<Option<Steps>> {
        let term = Term::from(predicate.clone());
        let pkey = key::term_key(term.as_ref()).0;
        let mut found: Vec<[Key; 4]> = Vec::new();
        for quad in self.host.quads(None, Some(&pkey), None, Graphs::One(&DEFAULT_GRAPH))? {
            found.push(quad?);
            if found.len() > STORE_ROWS {
                return Ok(None);
            }
        }
        terms::prefetch(&self.host, found.iter().flat_map(|q| [q[S], q[O]]))?;
        let mut store = Vec::with_capacity(found.len());
        for q in &found {
            let pair = [terms::term_of(&self.host, &q[S])?, terms::term_of(&self.host, &q[O])?];
            if pair.iter().any(|t| term_key(t, &self.vset.mappings).is_none()) {
                return Ok(None);
            }
            store.push(pair);
        }
        let mut branches = Vec::new();
        for (compiled, unit, choice) in self.vset.candidates(Some(&pkey), Some(None)) {
            let branch = Branch { compiled, unit, choice };
            let ctx = branch.ctx();
            let unit = &branch.compiled.units[branch.unit];
            if matches!(ctx.bind(&unit.predicate, &term), Bind::Never) {
                continue;
            }
            if map_key(&ctx, &unit.subject).is_none() || map_key(&ctx, &unit.object).is_none() {
                return Ok(None);
            }
            branches.push(branch);
        }
        Ok(Some(Steps::Link {
            predicate: predicate.clone(),
            branches,
            store,
            inverse,
        }))
    }

    /// LeftJoin(left, right) with right, a basic graph pattern, given to the
    /// SQL of a SERVICE of left as a LEFT JOIN: when the SERVICE is an operand
    /// of left's joins -- one of the rewrite's, or a basic graph pattern the
    /// SQL answers whole, made one -- right shares variables with its core
    /// and with no earlier OPTIONAL of it but through the core, uses no
    /// variable of the other operands the core does not bind, is connected,
    /// and its patterns' SQL conditions are exact.  Err gives the operands
    /// back.  restrict: the FILTERs' over left.
    #[allow(clippy::type_complexity)]
    fn optional(
        &mut self,
        left: QueryExpression,
        right: QueryExpression,
        expression: Option<spargebra::algebra::Expression>,
        restrict: &[(Variable, Restriction)],
    ) -> EResult<Result<QueryExpression, (QueryExpression, QueryExpression, Option<spargebra::algebra::Expression>)>> {
        let back = |left, right, expression| Ok(Err((left, right, expression)));
        if expression.is_some() {
            return back(left, right, expression);
        }
        let Some((graph, triples)) = bgp_in(&right) else {
            return back(left, right, expression);
        };
        let mut ops = Vec::new();
        operands(left, &mut ops);
        let mut group = Vec::new();
        for tp in &triples {
            match self.pattern(tp, graph.as_ref(), &[])? {
                Some(p) if exact(&p) => group.push(p),
                _ => return back(join_all(ops), right, expression),
            }
        }
        if components(&group).len() != 1 {
            return back(join_all(ops), right, expression);
        }
        // the part: a SERVICE among the operands, or else a basic graph
        // pattern there, made one if the OPTIONAL goes into it
        let mut target: Option<(usize, Result<usize, Vec<Pattern>>)> = ops.iter().enumerate().find_map(|(i, e)| {
            let n = service_number(e)?;
            (self.parts[n].1 == graph).then_some((i, Ok(n)))
        });
        if target.is_none() {
            for (i, e) in ops.iter().enumerate() {
                let Some(core_triples) = bgp_of(e, graph.as_ref()) else { continue };
                if let Some(core) = self.core(core_triples, graph.as_ref(), restrict)? {
                    target = Some((i, Err(core)));
                    break;
                }
            }
        }
        let Some((at, target)) = target else {
            return back(join_all(ops), right, expression);
        };
        let core_patterns: &[Pattern] = match &target {
            Ok(n) => &self.parts[*n].0.core,
            Err(core) => core,
        };
        let core: FxHashSet<Variable> = core_patterns.iter().flat_map(|p| p.variables().map(|(_, v)| v.clone())).collect();
        let earlier: FxHashSet<Variable> = match &target {
            Ok(n) => self.parts[*n]
                .0
                .optionals
                .iter()
                .flatten()
                .flat_map(|p| p.variables().map(|(_, v)| v.clone()))
                .filter(|v| !core.contains(v))
                .collect(),
            Err(_) => FxHashSet::default(),
        };
        let others: FxHashSet<Variable> = ops
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != at)
            .flat_map(|(_, e)| used(e))
            .collect();
        let vars: FxHashSet<Variable> = group.iter().flat_map(|p| p.variables().map(|(_, v)| v.clone())).collect();
        let ok = vars.iter().any(|v| core.contains(v))
            && vars.iter().all(|v| !earlier.contains(v))
            && vars.iter().all(|v| !others.contains(v) || core.contains(v))
            && joins_core(core_patterns, &group);
        if !ok {
            return back(join_all(ops), right, expression);
        }
        let n = match target {
            Ok(n) => n,
            Err(core) => {
                let n = self.new_part(core, graph.clone(), restrict);
                let op = std::mem::replace(&mut ops[at], QueryExpression::Bgp { patterns: Vec::new() });
                ops[at] = service(n, op);
                n
            }
        };
        self.parts[n].0.optionals.push(group);
        let QueryExpression::Service { inner, .. } = &mut ops[at] else {
            return Err(EngineError::internal("a SERVICE of the rewrite is not one"));
        };
        let left = std::mem::replace(inner.as_mut(), QueryExpression::Bgp { patterns: Vec::new() });
        **inner = QueryExpression::LeftJoin {
            left: Box::new(left),
            right: Box::new(right),
            expression: None,
        };
        Ok(Ok(join_all(ops)))
    }

    /// A part of the core given, its number: the core's FILTERs' restrictions
    /// on its variables kept
    fn new_part(&mut self, core: Vec<Pattern>, graph: Option<NamedNodePattern>, restrict: &[(Variable, Restriction)]) -> usize {
        let restrictions = restrict
            .iter()
            .filter(|(v, _)| core.iter().any(|p| p.variables().any(|(_, x)| x == v)))
            .cloned()
            .collect();
        self.parts.push((
            Part {
                core,
                optionals: Vec::new(),
                restrictions,
                exists: Vec::new(),
            },
            graph,
        ));
        self.parts.len() - 1
    }

    /// A basic graph pattern's triple patterns as a part's core: None unless
    /// the SQL answers each, and they are connected, with keys for the
    /// variables they share
    fn core(
        &self,
        triples: &[TriplePattern],
        graph: Option<&NamedNodePattern>,
        restrict: &[(Variable, Restriction)],
    ) -> EResult<Option<Vec<Pattern>>> {
        if triples.is_empty() {
            return Ok(None);
        }
        let mut core = Vec::with_capacity(triples.len());
        for tp in triples {
            let Some(p) = self.pattern(tp, graph, restrict)? else {
                return Ok(None);
            };
            core.push(p);
        }
        let shared = shared_variables(core.iter());
        let keyed = core
            .iter()
            .all(|p| p.variables().all(|(r, v)| !shared.contains(v) || keyable(p, r)));
        Ok((components(&core).len() == 1 && keyed).then_some(core))
    }

    /// The part, in e's graph, whose core binds every variable of need and
    /// that accept takes: one of the SERVICEs whose solutions keep their
    /// values to e's -- through joins, the left sides of OPTIONALs, FILTERs
    /// and BINDs -- or else a basic graph pattern there the SQL answers whole,
    /// made one
    fn serving(
        &mut self,
        e: &mut QueryExpression,
        graph: &Option<NamedNodePattern>,
        need: &FxHashSet<Variable>,
        accept: &dyn Fn(&[Pattern]) -> bool,
    ) -> EResult<Option<usize>> {
        if let Some(n) = service_number(e) {
            let (part, g) = &self.parts[n];
            let core: FxHashSet<&Variable> = part.core.iter().flat_map(|p| p.variables().map(|(_, v)| v)).collect();
            return Ok((g == graph && need.iter().all(|v| core.contains(v)) && accept(&part.core)).then_some(n));
        }
        match e {
            QueryExpression::Join { left, right } => {
                if let Some(n) = self.serving(left, graph, need, accept)? {
                    return Ok(Some(n));
                }
                self.serving(right, graph, need, accept)
            }
            QueryExpression::LeftJoin { left, .. }
            | QueryExpression::Filter { inner: left, .. }
            | QueryExpression::Extend { inner: left, .. } => self.serving(left, graph, need, accept),
            other => {
                let Some(triples) = bgp_of(other, graph.as_ref()) else {
                    return Ok(None);
                };
                let vars = triple_variables(triples, graph.as_ref());
                if !need.iter().all(|v| vars.contains(v)) {
                    return Ok(None);
                }
                let restrict = self.restrict.clone();
                let Some(core) = self.core(triples, graph.as_ref(), &restrict)? else {
                    return Ok(None);
                };
                if !accept(&core) {
                    return Ok(None);
                }
                let n = self.new_part(core, graph.clone(), &restrict);
                let inner = std::mem::replace(other, QueryExpression::Bgp { patterns: Vec::new() });
                *other = service(n, inner);
                Ok(Some(n))
            }
        }
    }

    /// e with each EXISTS that a SERVICE of inner answers made BOUND(?m) of a
    /// variable the SERVICE binds when its row has a match (Part's exists):
    /// EXISTS { P } of a basic graph pattern P of the SERVICE's graph whose
    /// patterns the SQL answers exactly, whose variables inner binds are all
    /// the core's -- EXISTS' substitution of them then a join on them -- with
    /// keys for what P shares, with the core or within itself.  spareval
    /// would evaluate P for each solution.
    fn exists(&mut self, mut e: spargebra::algebra::Expression, inner: &mut QueryExpression) -> EResult<spargebra::algebra::Expression> {
        let scope = used(inner);
        exists_mut(&mut e, &mut |x| self.decorrelate(x, inner, &scope))?;
        Ok(e)
    }

    fn decorrelate(
        &mut self,
        x: &mut spargebra::algebra::Expression,
        inner: &mut QueryExpression,
        scope: &FxHashSet<Variable>,
    ) -> EResult<()> {
        let spargebra::algebra::Expression::Exists(pattern) = x else {
            return Ok(());
        };
        let Some((graph, triples)) = bgp_in(pattern) else {
            return Ok(());
        };
        let mut group = Vec::new();
        for tp in &triples {
            match self.pattern(tp, graph.as_ref(), &[])? {
                Some(p) if exact(&p) => group.push(p),
                _ => return Ok(()),
            }
        }
        let need: FxHashSet<Variable> = group
            .iter()
            .flat_map(|p| p.variables().map(|(_, v)| v.clone()))
            .filter(|v| scope.contains(v))
            .collect();
        if need.is_empty() {
            return Ok(());
        }
        let found = {
            let accept = |core: &[Pattern]| joins_core(core, &group);
            self.serving(inner, &graph, &need, &accept)?
        };
        let Some(n) = found else {
            return Ok(());
        };
        let m = self.fresh();
        self.parts[n].0.exists.push((m.clone(), group));
        mark(inner, n, &m);
        *x = spargebra::algebra::Expression::Bound(m);
        Ok(())
    }

    /// A variable the query has none of
    fn fresh(&mut self) -> Variable {
        loop {
            let v = Variable::new_unchecked(format!("kb_exists_{}", self.fresh));
            self.fresh += 1;
            if !self.taken.contains(&v) {
                return v;
            }
        }
    }

    /// A triple pattern as the SQL can answer it; None when it cannot: a
    /// variable predicate no FILTER restricts to IRIs, a literal subject, more
    /// than STORE_ROWS of the store's quads that meet the restrictions on its
    /// variables, a column whose value the SQL cannot give as the host does.
    fn pattern(
        &self,
        tp: &TriplePattern,
        graph: Option<&NamedNodePattern>,
        restrict: &[(Variable, Restriction)],
    ) -> EResult<Option<Pattern>> {
        // its predicates: its own, or the IRIs FILTERs restrict its variable
        // to -- ?p IN (...), ?p = <iri> -- the union of their patterns
        let (predicate, predicates): (Pos, Vec<NamedNode>) = match &tp.predicate {
            NamedNodePattern::NamedNode(p) => (Pos::Const(p.clone().into()), vec![p.clone()]),
            NamedNodePattern::Variable(v) => {
                let mut list: Option<Vec<NamedNode>> = None;
                for (_, r) in restrict.iter().filter(|(x, _)| x == v) {
                    let Restriction::OneOf(terms) = r else { continue };
                    let iris: Vec<NamedNode> = terms
                        .iter()
                        .filter_map(|t| match t {
                            Term::NamedNode(n) => Some(n.clone()),
                            _ => None,
                        })
                        .collect();
                    list = Some(match list {
                        None => iris,
                        Some(before) => before.into_iter().filter(|n| iris.contains(n)).collect(),
                    });
                }
                match list {
                    Some(mut list) if list.len() <= MAX_PREDICATES => {
                        list.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
                        list.dedup();
                        (Pos::Var(v.clone()), list)
                    }
                    _ => return Ok(None),
                }
            }
        };
        let term = |t: &TermPattern| -> Option<Pos> {
            match t {
                TermPattern::NamedNode(n) => Some(Pos::Const(n.clone().into())),
                TermPattern::Literal(l) => Some(Pos::Const(l.clone().into())),
                TermPattern::Variable(v) => Some(Pos::Var(v.clone())),
                #[allow(unreachable_patterns)]
                _ => None,
            }
        };
        let (Some(s), Some(o)) = (term(&tp.subject), term(&tp.object)) else {
            return Ok(None);
        };
        if matches!(&s, Pos::Const(Term::Literal(_))) {
            return Ok(None);
        }
        let g = graph.map(|g| match g {
            NamedNodePattern::NamedNode(n) => Pos::Const(n.clone().into()),
            NamedNodePattern::Variable(v) => Pos::Var(v.clone()),
        });
        let pos = [Some(s), Some(predicate), Some(o), g];
        let predicate_keys: Vec<Key> = predicates
            .iter()
            .map(|p| key::term_key(Term::from(p.clone()).as_ref()).0)
            .collect();

        // the store's quads of the pattern: a few go into the SQL
        let key_of = |p: &Option<Pos>| match p {
            Some(Pos::Const(t)) => Some(key::term_key(t.as_ref()).0),
            _ => None,
        };
        let (ks, ko) = (key_of(&pos[S]), key_of(&pos[O]));
        let kg = key_of(&pos[G]);
        let graphs = match (&pos[G], &kg) {
            (None, _) => Graphs::One(&DEFAULT_GRAPH),
            (Some(_), Some(k)) => Graphs::One(k),
            (Some(_), None) => Graphs::Named,
        };
        let checks: Vec<(usize, &Restriction)> = ROLES
            .into_iter()
            .filter_map(|r| match &pos[r] {
                Some(Pos::Var(v)) => Some((r, v)),
                _ => None,
            })
            .flat_map(|(r, v)| restrict.iter().filter(move |(x, _)| x == v).map(move |(_, x)| (r, x)))
            .collect();
        let limit = if checks.is_empty() { STORE_ROWS } else { STORE_PROBE };
        let mut found: Vec<[Key; 4]> = Vec::new();
        for kp in &predicate_keys {
            for quad in self.host.quads(ks.as_ref(), Some(kp), ko.as_ref(), graphs)? {
                found.push(quad?);
                if found.len() > limit {
                    return Ok(None);
                }
            }
        }
        if !checks.is_empty() {
            terms::prefetch(&self.host, found.iter().flat_map(|q| checks.iter().map(|(r, _)| q[*r])))?;
            let mut kept = Vec::new();
            for q in found {
                let mut meets = true;
                for (r, x) in &checks {
                    if key::is_default_graph(&q[*r]) || !satisfies(&terms::term_of(&self.host, &q[*r])?, x) {
                        meets = false;
                        break;
                    }
                }
                if meets {
                    kept.push(q);
                }
            }
            if kept.len() > STORE_ROWS {
                return Ok(None);
            }
            found = kept;
        }
        terms::prefetch(&self.host, found.iter().flat_map(|q| q.iter().copied()))?;
        let mut store = Vec::with_capacity(found.len());
        for q in &found {
            let mut t: [Option<Term>; 4] = [None, None, None, None];
            for r in ROLES {
                if r == G && key::is_default_graph(&q[G]) {
                    continue;
                }
                t[r] = Some(terms::term_of(&self.host, &q[r])?);
            }
            store.push(t);
        }

        // the units that may make quads of it, of each of its predicates once
        let graph_name = match (&pos[G], &kg) {
            (None, _) => Some(None),
            (Some(_), Some(k)) => Some(Some(k)),
            (Some(_), None) => None,
        };
        let mut branches: Vec<Branch> = Vec::new();
        let candidates = predicate_keys
            .iter()
            .flat_map(|pkey| self.vset.candidates(Some(pkey), graph_name));
        for (compiled, unit, choice) in candidates {
            if branches.iter().any(|b| {
                Rc::ptr_eq(&b.compiled, &compiled)
                    && b.unit == unit
                    && match (b.choice, choice) {
                        (GraphChoice::Default, GraphChoice::Default) => true,
                        (GraphChoice::Map(x), GraphChoice::Map(y)) => x == y,
                        _ => false,
                    }
            }) {
                continue;
            }
            let branch = Branch {
                compiled,
                unit,
                choice,
            };
            let ctx = branch.ctx();
            let mut never = false;
            for r in ROLES {
                if let (Some(Pos::Const(t)), Some(m)) = (&pos[r], branch.map(r)) {
                    if matches!(ctx.bind(m, t), Bind::Never) {
                        never = true;
                    }
                }
            }
            if never {
                continue;
            }
            for r in ROLES {
                if let Some(m) = branch.map(r) {
                    if m.columns().iter().any(|c| eval::value_kind(&ctx, *c).is_none()) {
                        return Ok(None);
                    }
                }
            }
            branches.push(branch);
        }
        Ok(Some(Pattern {
            source: Source::Triple(tp.clone()),
            pos,
            branches,
            store,
            path: None,
        }))
    }
}

/// The join of two operands of a group, after what sparopt does before its
/// join reordering (lift_out_of_join): an OPTIONAL or a FILTER of one operand
/// lifted over the join when the other cannot tell, so that the basic graph
/// patterns of a group meet, and merge into one.
fn join(l: QueryExpression, r: QueryExpression) -> QueryExpression {
    use QueryExpression as E;
    match (l, r) {
        (E::Bgp { patterns: mut a }, E::Bgp { patterns: b }) => {
            a.extend(b);
            E::Bgp { patterns: a }
        }
        (E::LeftJoin { left, right, expression }, y) if liftable(&left, &right, expression.as_ref(), &y) => {
            E::LeftJoin {
                left: Box::new(join(*left, y)),
                right,
                expression,
            }
        }
        (y, E::LeftJoin { left, right, expression }) if liftable(&left, &right, expression.as_ref(), &y) => {
            E::LeftJoin {
                left: Box::new(join(y, *left)),
                right,
                expression,
            }
        }
        (E::Filter { expr, inner }, y) if filter_liftable(&expr, &inner) => E::Filter {
            expr,
            inner: Box::new(join(*inner, y)),
        },
        (
            E::Extend {
                inner,
                variable,
                expression,
            },
            y,
        ) if extend_liftable(&inner, &variable, &expression, &y) => E::Extend {
            inner: Box::new(join(*inner, y)),
            variable,
            expression,
        },
        (
            y,
            E::Extend {
                inner,
                variable,
                expression,
            },
        ) if extend_liftable(&inner, &variable, &expression, &y) => E::Extend {
            inner: Box::new(join(y, *inner)),
            variable,
            expression,
        },
        (y, E::Filter { expr, inner }) if filter_liftable(&expr, &inner) => E::Filter {
            expr,
            inner: Box::new(join(y, *inner)),
        },
        (E::Join { left, right }, y @ E::Bgp { .. }) => match (*left, *right) {
            (x, z @ E::Bgp { .. }) => E::Join {
                left: Box::new(x),
                right: Box::new(join(z, y)),
            },
            (x @ E::Bgp { .. }, z) => E::Join {
                left: Box::new(join(x, y)),
                right: Box::new(z),
            },
            (x, z) => E::Join {
                left: Box::new(E::Join {
                    left: Box::new(x),
                    right: Box::new(z),
                }),
                right: Box::new(y),
            },
        },
        (l, r) => E::Join {
            left: Box::new(l),
            right: Box::new(r),
        },
    }
}

/// LeftJoin(l, r, e), with a BIND of l lifted over it as join() lifts one
/// over a join: so that an OPTIONAL after a BIND meets the basic graph
/// pattern before it
fn left_join(l: QueryExpression, r: QueryExpression, e: Option<spargebra::algebra::Expression>) -> QueryExpression {
    use QueryExpression as E;
    match l {
        E::Extend {
            inner,
            variable,
            expression,
        } if extend_liftable(
            &inner,
            &variable,
            &expression,
            &E::LeftJoin {
                left: empty(),
                right: Box::new(r.clone()),
                expression: e.clone(),
            },
        ) =>
        {
            E::Extend {
                inner: Box::new(left_join(*inner, r, e)),
                variable,
                expression,
            }
        }
        l => E::LeftJoin {
            left: Box::new(l),
            right: Box::new(r),
            expression: e,
        },
    }
}

fn empty() -> Box<QueryExpression> {
    Box::new(QueryExpression::Bgp { patterns: Vec::new() })
}

/// Whether Join(LeftJoin(a, b, e), y) is LeftJoin(Join(a, y), b, e): y uses no
/// variable b or e use that a does not bind in each of its solutions
fn liftable(
    a: &QueryExpression,
    b: &QueryExpression,
    e: Option<&spargebra::algebra::Expression>,
    y: &QueryExpression,
) -> bool {
    let optional = used(&QueryExpression::LeftJoin {
        left: empty(),
        right: Box::new(b.clone()),
        expression: e.cloned(),
    });
    let bound = certain(a);
    used(y).iter().all(|v| !optional.contains(v) || bound.contains(v))
}

/// Whether Join(Filter(e, a), y) is Filter(e, Join(a, y)): every variable e
/// uses is one a binds in each of its solutions
fn filter_liftable(e: &spargebra::algebra::Expression, a: &QueryExpression) -> bool {
    let bound = certain(a);
    used(&QueryExpression::Filter {
        expr: e.clone(),
        inner: empty(),
    })
    .iter()
    .all(|v| bound.contains(v))
}

/// Whether Join(Extend(a, v, x), y) is Extend(Join(a, y), v, x): y does not
/// use v, and x uses no variable of y that a does not bind in each of its
/// solutions
fn extend_liftable(
    a: &QueryExpression,
    v: &Variable,
    x: &spargebra::algebra::Expression,
    y: &QueryExpression,
) -> bool {
    let of_y = used(y);
    if of_y.contains(v) {
        return false;
    }
    let bound = certain(a);
    used(&QueryExpression::Extend {
        inner: empty(),
        variable: v.clone(),
        expression: x.clone(),
    })
    .iter()
    .all(|w| w == v || !of_y.contains(w) || bound.contains(w))
}

/// The variables an expression uses anywhere, its FILTERs' and BINDs' too
fn used(e: &QueryExpression) -> FxHashSet<Variable> {
    let mut out = FxHashSet::default();
    let e = sparopt::algebra::QueryExpression::from(e);
    e.lookup_used_variables(&mut |v| {
        out.insert(v.clone());
    });
    out
}

/// The variables an expression binds in each of its solutions, as far as its
/// shape tells
fn certain(e: &QueryExpression) -> FxHashSet<Variable> {
    let mut out = FxHashSet::default();
    match e {
        QueryExpression::Bgp { patterns } => {
            for tp in patterns {
                for t in [&tp.subject, &tp.object] {
                    if let TermPattern::Variable(v) = t {
                        out.insert(v.clone());
                    }
                }
                if let NamedNodePattern::Variable(v) = &tp.predicate {
                    out.insert(v.clone());
                }
            }
        }
        QueryExpression::Join { left, right } => {
            out = certain(left);
            out.extend(certain(right));
        }
        QueryExpression::LeftJoin { left, .. }
        | QueryExpression::Filter { inner: left, .. }
        | QueryExpression::Extend { inner: left, .. } => {
            out = certain(left);
        }
        QueryExpression::Graph { name, inner } => {
            out = certain(inner);
            if let NamedNodePattern::Variable(v) = name {
                out.insert(v.clone());
            }
        }
        _ => (),
    }
    out
}

/// The patterns that go to SQL, in connected parts of two or more, and those
/// left: a part's variables two patterns share must have keys, or the
/// patterns that do not give one leave the part.
fn parts(mut patterns: Vec<Pattern>) -> (Vec<Vec<Pattern>>, Vec<Pattern>) {
    let mut left = Vec::new();
    loop {
        let components = components(&patterns);
        // the patterns of a part whose shared variables some branch cannot key
        let mut out: Vec<usize> = Vec::new();
        for comp in &components {
            if comp.len() < 2 {
                continue;
            }
            let shared = shared_variables(comp.iter().map(|&i| &patterns[i]));
            for &i in comp {
                let p = &patterns[i];
                if p.variables().any(|(r, v)| shared.contains(v) && !keyable(p, r)) {
                    out.push(i);
                }
            }
        }
        if out.is_empty() {
            let mut slots: Vec<Option<Pattern>> = patterns.into_iter().map(Some).collect();
            let mut parts = Vec::new();
            for comp in components {
                if comp.len() < 2 {
                    left.extend(comp.into_iter().filter_map(|i| slots[i].take()));
                } else {
                    parts.push(comp.into_iter().filter_map(|i| slots[i].take()).collect());
                }
            }
            return (parts, left);
        }
        out.sort_unstable();
        for i in out.into_iter().rev() {
            left.push(patterns.remove(i));
        }
    }
}

/// The patterns connected by shared variables, each part in an order where
/// every pattern but the first shares a variable with one before it
fn components(patterns: &[Pattern]) -> Vec<Vec<usize>> {
    let mut seen = vec![false; patterns.len()];
    let mut out = Vec::new();
    for start in 0..patterns.len() {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        let mut comp = vec![start];
        let mut i = 0;
        while i < comp.len() {
            let here: Vec<&Variable> = patterns[comp[i]].variables().map(|(_, v)| v).collect();
            for (j, p) in patterns.iter().enumerate() {
                if !seen[j] && p.variables().any(|(_, v)| here.contains(&v)) {
                    seen[j] = true;
                    comp.push(j);
                }
            }
            i += 1;
        }
        out.push(comp);
    }
    out
}

fn shared_variables<'a>(patterns: impl Iterator<Item = &'a Pattern>) -> FxHashSet<Variable> {
    let mut count: HashMap<&Variable, usize> = HashMap::new();
    for p in patterns {
        for (_, v) in p.variables() {
            *count.entry(v).or_default() += 1;
        }
    }
    count.into_iter().filter(|(_, n)| *n > 1).map(|(v, _)| v.clone()).collect()
}

/// Whether every term a position may take has a key
fn keyable(p: &Pattern, role: usize) -> bool {
    p.branches.iter().all(|b| match b.map(role) {
        Some(m) => map_key(&b.ctx(), m).is_some(),
        None => true,
    }) && p.store.iter().all(|q| q[role].as_ref().is_none_or(|t| term_key(t, &[]).is_some()))
}

// ---------------------------------------------------------------------
// Keys and values
// ---------------------------------------------------------------------

/// The tag and the key of a literal: a plain string, a string in a
/// language, an integer and a boolean as their canonical lexical forms; None
/// for any other, whose lexical forms may differ for one value
fn literal_key(l: &Literal) -> Option<(String, String)> {
    if let Some(language) = l.language() {
        return Some((format!("L@{}", language.to_ascii_lowercase()), l.value().to_owned()));
    }
    let dt = l.datatype();
    if *dt == xsd::STRING {
        return Some(("Ls".to_owned(), l.value().to_owned()));
    }
    if *dt == xsd::BOOLEAN {
        return match l.value() {
            "true" | "1" => Some(("Lb".to_owned(), "true".to_owned())),
            "false" | "0" => Some(("Lb".to_owned(), "false".to_owned())),
            _ => None,
        };
    }
    // xsd:integer, and the datatypes the key keeps as one (xsd:int, ...)
    let i = Integer::from_str(l.value()).ok()?;
    let canonical = Literal::new_typed_literal(i.to_string(), xsd::INTEGER);
    (key::literal_key(canonical.as_ref()).0 == key::literal_key(l.as_ref()).0)
        .then(|| ("Li".to_owned(), i.to_string()))
}

/// The tag and the key of a term of the store or of a query: an IRI itself, a
/// mapping's blank node its mapping and label, the store's its label
fn term_key(t: &Term, mappings: &[Rc<Compiled>]) -> Option<(String, String)> {
    match t {
        Term::NamedNode(n) => Some(("I".to_owned(), n.as_str().to_owned())),
        Term::BlankNode(b) => {
            for m in mappings {
                if let Some(label) = eval::blank_id_label(&m.bnode_prefix, b.as_str()) {
                    return Some((format!("B{}", m.bnode_prefix), label));
                }
            }
            Some(("S".to_owned(), b.as_str().to_owned()))
        }
        Term::Literal(l) => literal_key(l),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// The term of a tag and a key, as map_key() and term_key() make them -- the
/// term the store or a mapping's map has: an IRI, a mapping's blank node by
/// its label, the store's by its own, a literal by its canonical form
fn key_term(tag: &str, key: &str, mappings: &[Rc<Compiled>]) -> EResult<Term> {
    let bad = |what: &str| eval::data_error(format!("{key:?} is not {what}"));
    Ok(match tag {
        "I" => NamedNode::new(key.to_owned()).map_err(|_| bad("an IRI"))?.into(),
        "S" => oxrdf::BlankNode::new(key.to_owned()).map_err(|_| bad("a blank node's label"))?.into(),
        "Ls" => Literal::new_simple_literal(key.to_owned()).into(),
        "Li" => Literal::new_typed_literal(key.to_owned(), xsd::INTEGER).into(),
        "Lb" => Literal::new_typed_literal(key.to_owned(), xsd::BOOLEAN).into(),
        _ => {
            if let Some(language) = tag.strip_prefix("L@") {
                Literal::new_language_tagged_literal(key.to_owned(), language.to_owned())
                    .map_err(|_| bad("a string in a language"))?
                    .into()
            } else if let Some(prefix) = tag.strip_prefix('B') {
                let compiled = mappings
                    .iter()
                    .find(|m| m.bnode_prefix == prefix)
                    .ok_or_else(|| EngineError::internal("a blank node's key of no mapping of the part"))?;
                oxrdf::BlankNode::new_unchecked(eval::blank_id(&compiled.bnode_prefix, key)).into()
            } else {
                return Err(EngineError::internal("a key's tag the SQL does not make"));
            }
        }
    })
}

/// The tag and the key of a term map's terms, as SQL; None when no key tells
/// its terms apart.  Constants go in as parameters.
fn map_key(ctx: &Context<'_>, map: &CMap) -> Option<(KeySql, KeySql)> {
    let text_oid = |c: ColRef| matches!(ctx.column(c).type_oid, 19 | 25 | 1043);
    let label_oid = |c: ColRef| matches!(ctx.column(c).type_oid, 19 | 25 | 1043 | 20 | 21 | 23 | 26);
    match &map.value {
        // R2RML has no constant blank node (parse.rs)
        CValue::Constant(t, _) => {
            let (tag, key) = term_key(t, &[])?;
            Some((KeySql::Param(tag), KeySql::Param(key)))
        }
        CValue::Column(c) => {
            let col = eval::column_sql(ctx, *c);
            let class = ctx.class(*c);
            match map.term_type {
                TermType::Iri if ctx.compiled.base.is_none() && text_oid(*c) => {
                    Some((KeySql::Tag("I".to_owned()), KeySql::Sql(format!("{col}::text"))))
                }
                TermType::BlankNode if label_oid(*c) => Some((
                    KeySql::Tag(format!("B{}", ctx.compiled.bnode_prefix)),
                    KeySql::Sql(format!("{col}::text")),
                )),
                TermType::Literal => {
                    if let Some(language) = &map.language {
                        return text_oid(*c).then(|| {
                            (
                                KeySql::Tag(format!("L@{}", language.to_ascii_lowercase())),
                                KeySql::Sql(format!("{col}::text")),
                            )
                        });
                    }
                    let dt = map.datatype.as_ref().map(NamedNode::as_ref);
                    if class == KB_SQL_STRING && text_oid(*c) && dt.is_none_or(|d| d == xsd::STRING) {
                        Some((KeySql::Tag("Ls".to_owned()), KeySql::Sql(format!("{col}::text"))))
                    } else if class == KB_SQL_INTEGER && dt.is_none_or(|d| d == xsd::INTEGER) {
                        Some((KeySql::Tag("Li".to_owned()), KeySql::Sql(format!("{col}::text"))))
                    } else if class == KB_SQL_BOOLEAN && dt.is_none_or(|d| d == xsd::BOOLEAN) {
                        Some((KeySql::Tag("Lb".to_owned()), KeySql::Sql(eval::value_sql(ctx, *c))))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        CValue::Template(parts) if map.term_type == TermType::BlankNode => {
            let mut sql = Vec::new();
            for part in parts {
                match part {
                    CPart::Text(t) => sql.push(KeySql::Param(t.clone())),
                    CPart::Column(c) if label_oid(*c) => {
                        sql.push(KeySql::Sql(format!("{}::text", eval::column_sql(ctx, *c))));
                    }
                    CPart::Column(_) => return None,
                }
            }
            Some((
                KeySql::Tag(format!("B{}", ctx.compiled.bnode_prefix)),
                KeySql::Concat(sql),
            ))
        }
        CValue::Template(_) => None,
    }
}

/// A tag or a key in SQL: a constant tag, a parameter, an expression, or a
/// concatenation of these
enum KeySql {
    Tag(String),
    Param(String),
    Sql(String),
    Concat(Vec<KeySql>),
}

impl KeySql {
    fn write(&self, params: &mut Vec<String>) -> String {
        match self {
            // tags are ASCII letters, digits, '@' and '-'
            KeySql::Tag(t) => format!("'{t}'::text"),
            KeySql::Param(v) => {
                params.push(v.clone());
                format!("${}::text", params.len())
            }
            KeySql::Sql(s) => s.clone(),
            KeySql::Concat(parts) => {
                let parts: Vec<String> = parts.iter().map(|p| p.write(params)).collect();
                if parts.is_empty() {
                    "''::text".to_owned()
                } else {
                    format!("({})", parts.join(" || "))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// The SQL
// ---------------------------------------------------------------------

/// An item of a part's core, in its order: a triple pattern, or a path
#[derive(Clone, Copy)]
enum Item {
    Pattern(usize),
    Path(usize),
}

/// Where the core gives a variable its first term, in the order of its
/// items: a pattern's position, or a path's end
#[derive(Clone, Copy, PartialEq)]
enum Place {
    Pattern(usize, usize),
    Path(usize, usize),
}

impl Place {
    /// The relation's alias in the SQL, and the role of its tag and key
    /// columns (t{r}, k{r})
    fn alias(self) -> (String, usize) {
        match self {
            Place::Pattern(i, r) => (format!("q{i}"), r),
            Place::Path(j, r) => (format!("w{j}"), r),
        }
    }
}

/// The SQL of a part: each pattern's UNION ALL (pattern_sql()); the core's
/// joined on the shared variables' tags and keys, and to the pairs of each of
/// its paths (PathSql), in the core's order; each OPTIONAL's joined among
/// themselves and LEFT JOINed to the core on the variables they share with
/// it, and each EXISTS a column, the semi-join of its patterns on what they
/// share with the core.
fn plan(part: Part, host: &Host) -> EResult<Plan> {
    let restrict = part.restrictions;
    let exists = part.exists;
    // the core's items in its order, where each but the first shares a
    // variable with one before it (components())
    let mut paths: Vec<Pattern> = Vec::new();
    let mut patterns: Vec<(usize, Pattern)> = Vec::new();
    let mut items: Vec<Item> = Vec::new();
    for p in part.core {
        if p.path.is_some() {
            items.push(Item::Path(paths.len()));
            paths.push(p);
        } else {
            items.push(Item::Pattern(patterns.len()));
            patterns.push((0, p));
        }
    }
    for (g, group) in part.optionals.into_iter().enumerate() {
        patterns.extend(group.into_iter().map(|p| (g + 1, p)));
    }
    let mut variables: Vec<Variable> = Vec::new();
    // each variable's first place in the part's patterns, and in each group
    let mut first: HashMap<Variable, (usize, usize)> = HashMap::new();
    let mut first_in: HashMap<(usize, Variable), (usize, usize)> = HashMap::new();
    let shared = shared_variables(
        patterns
            .iter()
            .map(|(_, p)| p)
            .chain(paths.iter())
            .chain(exists.iter().flat_map(|(_, group)| group.iter())),
    );
    for (i, (g, p)) in patterns.iter().enumerate() {
        for (r, v) in p.variables() {
            if !first.contains_key(v) {
                first.insert(v.clone(), (i, r));
                variables.push(v.clone());
            }
            first_in.entry((*g, v.clone())).or_insert((i, r));
        }
    }
    // and in the core, where a path may give it first
    let mut core_first: HashMap<Variable, Place> = HashMap::new();
    for item in &items {
        match *item {
            Item::Pattern(i) => {
                for (r, v) in patterns[i].1.variables() {
                    core_first.entry(v.clone()).or_insert(Place::Pattern(i, r));
                }
            }
            Item::Path(j) => {
                for r in [S, O] {
                    if let Some(Pos::Var(v)) = &paths[j].pos[r] {
                        core_first.entry(v.clone()).or_insert(Place::Path(j, r));
                    }
                }
            }
        }
    }
    let mappings: Vec<Rc<Compiled>> = {
        let mut m: Vec<Rc<Compiled>> = Vec::new();
        let mut links = Vec::new();
        for steps in paths.iter().filter_map(|p| p.path.as_ref()) {
            steps.branches(&mut links);
        }
        for b in patterns
            .iter()
            .map(|(_, p)| p)
            .chain(exists.iter().flat_map(|(_, group)| group.iter()))
            .flat_map(|p| p.branches.iter())
            .chain(links)
        {
            if !m.iter().any(|x| Rc::ptr_eq(x, &b.compiled)) {
                m.push(Rc::clone(&b.compiled));
            }
        }
        m
    };
    // the tags each position of each pattern may give: where both sides of a
    // join of an OPTIONAL or an EXISTS, whose matches the rows' terms do not
    // check, give one, the same, the join is on the keys alone -- PostgreSQL
    // has no statistics of these columns, and each equality more makes its
    // estimate of the join's rows smaller, its plan a nested loop that reads
    // a pattern's whole UNION ALL again for each row
    let tag_sets: Vec<[FxHashSet<String>; 4]> = patterns
        .iter()
        .map(|(_, p)| ROLES.map(|r| tags(p, r, &mappings)))
        .collect();
    let exists_tags: Vec<Vec<[FxHashSet<String>; 4]>> = exists
        .iter()
        .map(|(_, group)| group.iter().map(|p| ROLES.map(|r| tags(p, r, &mappings))).collect())
        .collect();
    // a path's ends may have any tag: joins to them are on tags and keys
    let any: FxHashSet<String> = [String::new(), "?".to_owned()].into_iter().collect();
    let place = |at: Place| {
        let tags = match at {
            Place::Pattern(i, r) => &tag_sets[i][r],
            Place::Path(..) => &any,
        };
        let (alias, r) = at.alias();
        (alias, r, tags)
    };
    let mut params: Vec<String> = Vec::new();
    let mut froms: Vec<String> = Vec::new();
    // each OPTIONAL's joins, then the conditions that tie it to the core
    let mut groups: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    let mut selects: Vec<String> = Vec::new();
    let mut columns = Vec::new();
    let mut ncol = 0;
    // the core patterns' SQL, joined after in the core's order, and which a
    // path's seed reads again
    let mut unions: Vec<String> = Vec::new();
    for (i, (g, p)) in patterns.iter().enumerate() {
        let g = *g;
        // each position's value columns: as many as its widest map has, or
        // one for the store's quads
        let mut width = [0usize; 4];
        for r in p.roles() {
            width[r] = p
                .branches
                .iter()
                .map(|b| b.map(r).map_or(0, |m| m.columns().len()))
                .max()
                .unwrap_or(0)
                .max(usize::from(!p.store.is_empty()));
        }
        let keyed: Vec<usize> = p.variables().filter(|(_, v)| shared.contains(*v)).map(|(r, _)| r).collect();
        let restricted: &[(Variable, Restriction)] = if g == 0 { &restrict } else { &[] };
        let union = pattern_sql(p, &keyed, &width, restricted, &mappings, &mut params)?;
        let alias = format!("q{i}");
        if g == 0 {
            unions.push(union);
        } else {
            let link = |(r, (i0, r0)): (usize, (usize, usize))| {
                equal(&alias, r, &tag_sets[i][r], &format!("q{i0}"), r0, &tag_sets[i0][r0])
            };
            if groups.len() < g {
                groups.push((Vec::new(), Vec::new()));
            }
            let (joins, ties) = &mut groups[g - 1];
            // within the group: each variable's first place in it; to the
            // core: a variable's first place in the group and in the core
            let inner: Vec<String> = p
                .variables()
                .filter(|(_, v)| shared.contains(*v))
                .filter_map(|(r, v)| first_in.get(&(g, v.clone())).filter(|(i0, _)| *i0 < i).map(|&f| link((r, f))))
                .collect();
            for (r, v) in p.variables() {
                if first_in.get(&(g, v.clone())) == Some(&(i, r)) {
                    if let Some(&at) = core_first.get(v) {
                        let (a, r0, t0) = place(at);
                        ties.push(equal(&alias, r, &tag_sets[i][r], &a, r0, t0));
                    }
                }
            }
            joins.push(if joins.is_empty() {
                format!("({union}) AS {alias}")
            } else {
                format!("JOIN ({union}) AS {alias} ON {}", and(inner))
            });
        }
        // the columns of the result: the branch, then each position's values
        let b_col = ncol;
        selects.push(format!("{alias}.b"));
        ncol += 1;
        let mut cols = [(0usize, 0usize); 4];
        for r in p.roles() {
            cols[r] = (ncol, width[r]);
            for k in 0..width[r] {
                selects.push(format!("{alias}.v{r}_{k}"));
                ncol += 1;
            }
        }
        columns.push((b_col, cols));
    }
    // the core, in its order: a pattern joined on the keys alone of the
    // variables a pattern before gives -- the rows' terms are checked
    // (matched()), so that a key of two terms of different tags, a row too
    // many, does no harm -- and on tags and keys to a path's end, which no
    // term checks; a path's pairs from the terms of its more selective end
    // (path_join())
    let mut ctes: Vec<String> = Vec::new();
    for (k, item) in items.iter().enumerate() {
        let (alias, from) = match *item {
            Item::Pattern(i) => {
                let alias = format!("q{i}");
                let on: Vec<String> = patterns[i]
                    .1
                    .variables()
                    .filter(|(_, v)| shared.contains(*v))
                    .filter_map(|(r, v)| match core_first.get(v) {
                        Some(&Place::Pattern(i0, r0)) if i0 != i => Some(format!("{alias}.k{r} = q{i0}.k{r0}")),
                        Some(&Place::Path(j, r0)) => {
                            Some(format!("{alias}.t{r} = w{j}.t{r0} AND {alias}.k{r} = w{j}.k{r0}"))
                        }
                        _ => None,
                    })
                    .collect();
                (alias, (format!("({})", unions[i]), on))
            }
            Item::Path(j) => {
                let mut join = PathJoin {
                    host,
                    patterns: &patterns,
                    unions: &unions,
                    first: &first,
                    shared: &shared,
                    restrict: &restrict,
                    mappings: &mappings,
                    ctes: &mut ctes,
                    params: &mut params,
                };
                let (pairs, ties) = join.join(j, &paths[j], &core_first)?;
                (format!("w{j}"), (pairs, ties))
            }
        };
        let (relation, on) = from;
        froms.push(if k == 0 {
            format!("{relation} AS {alias}")
        } else {
            format!("JOIN {relation} AS {alias} ON {}", and(on))
        });
    }
    for (joins, ties) in groups {
        // a join in parentheses, a lone subquery without
        let group = if joins.len() == 1 { joins.join(" ") } else { format!("({})", joins.join(" ")) };
        froms.push(format!("LEFT JOIN {group} ON {}", and(ties)));
    }
    // each path's end that only the path gives: its tag and its key, of
    // which solution() makes its term
    let mut ends = Vec::new();
    for (j, p) in paths.iter().enumerate() {
        for r in [S, O] {
            let Some(Pos::Var(v)) = &p.pos[r] else { continue };
            let made = patterns.iter().any(|(g, q)| *g == 0 && q.variables().any(|(_, w)| w == v));
            if made || core_first.get(v) != Some(&Place::Path(j, r)) {
                continue;
            }
            let i = match variables.iter().position(|x| x == v) {
                Some(i) => i,
                None => {
                    variables.push(v.clone());
                    variables.len() - 1
                }
            };
            selects.push(format!("w{j}.t{r}"));
            selects.push(format!("w{j}.k{r}"));
            ends.push((ncol, ncol + 1, i));
            ncol += 2;
        }
    }
    // each EXISTS: a LEFT JOIN of the distinct keys its patterns' matches give
    // the variables they share with the core -- read once and joined by a
    // hash, where an EXISTS of the select list is a subquery for each row --
    // and 't' where the core's row has one; the keys are exact, as the SQL's
    // conditions are, and only they check the matches' terms
    let mut exists_columns = Vec::new();
    for (j, (m, group)) in exists.iter().enumerate() {
        let mut items = Vec::new();
        // within the group, the subquery's columns, and its ties to the core
        let mut conditions = Vec::new();
        let mut outs = Vec::new();
        let mut ties = Vec::new();
        let mut first_e: HashMap<&Variable, (usize, usize)> = HashMap::new();
        for (gi, p) in group.iter().enumerate() {
            let keyed: Vec<usize> = p.variables().filter(|(_, v)| shared.contains(*v)).map(|(r, _)| r).collect();
            let union = pattern_sql(p, &keyed, &[0; 4], &[], &mappings, &mut params)?;
            let alias = format!("e{j}_{gi}");
            for (r, v) in p.variables() {
                if !shared.contains(v) {
                    continue;
                }
                let here = &exists_tags[j][gi][r];
                if let Some(&(g0, r0)) = first_e.get(v) {
                    conditions.push(equal(&alias, r, here, &format!("e{j}_{g0}"), r0, &exists_tags[j][g0][r0]));
                    continue;
                }
                first_e.insert(v, (gi, r));
                if let Some(&at) = core_first.get(v) {
                    let c = outs.len();
                    outs.push(format!("{alias}.t{r} AS t{c}, {alias}.k{r} AS k{c}"));
                    let (a, r0, t0) = place(at);
                    ties.push(equal(&format!("x{j}"), c, here, &a, r0, t0));
                }
            }
            items.push(format!("({union}) AS {alias}"));
        }
        if outs.is_empty() {
            return Err(EngineError::internal("an EXISTS of the SQL shares no variable with its core"));
        }
        let within = if conditions.is_empty() { String::new() } else { format!(" WHERE {}", and(conditions)) };
        froms.push(format!(
            "LEFT JOIN (SELECT DISTINCT {} FROM {}{within}) AS x{j} ON {}",
            outs.join(", "),
            items.join(", "),
            and(ties)
        ));
        selects.push(format!("CASE WHEN x{j}.k0 IS NOT NULL THEN 't' END"));
        exists_columns.push((ncol, variables.len()));
        variables.push(m.clone());
        ncol += 1;
    }
    // a variable twice in one pattern of the core or of an OPTIONAL: the
    // rows' terms are checked
    let with = if ctes.is_empty() { String::new() } else { format!("WITH RECURSIVE {} ", ctes.join(", ")) };
    let sql = format!("{with}SELECT {} FROM {}", selects.join(", "), froms.join(" "));
    Ok(Plan {
        sql,
        params,
        variables: variables.into(),
        patterns,
        columns,
        exists: exists_columns,
        ends,
        mappings,
    })
}

/// What a path's join in the SQL of a part reads (plan())
struct PathJoin<'a> {
    host: &'a Host,
    patterns: &'a [(usize, Pattern)],
    /// the core patterns' SQL
    unions: &'a [String],
    first: &'a HashMap<Variable, (usize, usize)>,
    shared: &'a FxHashSet<Variable>,
    restrict: &'a [(Variable, Restriction)],
    mappings: &'a [Rc<Compiled>],
    ctes: &'a mut Vec<String>,
    params: &'a mut Vec<String>,
}

/// An end of a path: a constant's tag and key, a variable's first place in
/// the part's patterns, a core's (anchored()), or a variable only the path
/// binds
enum End {
    Constant(String, String),
    At(usize, usize),
    Free,
}

impl PathJoin<'_> {
    /// The relation of path j's pairs, its start's tag and key t0 and k0, its
    /// end's t2 and k2, and its join's conditions: each constant end its
    /// term, each variable end the variable's place before it in the core.
    /// The pairs are those from the terms of its seed -- a constant end, or
    /// the end whose pattern PostgreSQL expects the fewer rows of -- the path
    /// inverted when the seed is its object.
    fn join(&mut self, j: usize, p: &Pattern, core_first: &HashMap<Variable, Place>) -> EResult<(String, Vec<String>)> {
        let steps = p.path.as_ref().ok_or_else(|| EngineError::internal("a path pattern with no steps"))?;
        let mut ends: Vec<End> = Vec::with_capacity(2);
        for r in [S, O] {
            ends.push(match &p.pos[r] {
                Some(Pos::Const(t)) => {
                    let (tag, key) =
                        term_key(t, self.mappings).ok_or_else(|| EngineError::internal("a path's constant has no key"))?;
                    End::Constant(tag, key)
                }
                Some(Pos::Var(v)) => match self.first.get(v).filter(|(i0, _)| self.patterns[*i0].0 == 0) {
                    Some(&(i0, r0)) => End::At(i0, r0),
                    None => End::Free,
                },
                None => return Err(EngineError::internal("a path pattern with no end")),
            });
        }
        let back = match (&ends[0], &ends[1]) {
            (End::Free, End::Free) => return Err(EngineError::internal("a path with no end the core binds")),
            (End::Free, _) => true,
            (_, End::Free) => false,
            (start, end) => self.rows(end)? < self.rows(start)?,
        };
        let (seed_end, forward) = if back { (&ends[1], steps.inverted()) } else { (&ends[0], steps.clone()) };
        let seed = format!("kb_s{j}");
        let seed_sql = match seed_end {
            End::Constant(tag, key) => {
                self.params.push(tag.clone());
                self.params.push(key.clone());
                format!("SELECT ${}::text AS t, ${}::text AS k", self.params.len() - 1, self.params.len())
            }
            End::At(i0, r0) => format!("SELECT DISTINCT t{r0} AS t, k{r0} AS k FROM ({}) AS a", self.unions[*i0]),
            End::Free => return Err(EngineError::internal("a path's seed is its free end")),
        };
        self.ctes.push(format!("{seed} AS ({seed_sql})"));
        let mut builder = PathSql {
            ctes: &mut *self.ctes,
            params: &mut *self.params,
            mappings: self.mappings,
            prefix: format!("kb_p{j}"),
            count: 0,
            edges: HashMap::new(),
        };
        let pairs = builder
            .pairs(&forward, Some(&seed))?
            .ok_or_else(|| EngineError::internal("a path the SQL cannot follow"))?;
        let columns = if back {
            "o_t AS t0, o_k AS k0, s_t AS t2, s_k AS k2"
        } else {
            "s_t AS t0, s_k AS k0, o_t AS t2, o_k AS k2"
        };
        let alias = format!("w{j}");
        let mut on = Vec::new();
        for (end, r) in ends.iter().zip([S, O]) {
            match end {
                End::Constant(tag, key) => {
                    self.params.push(tag.clone());
                    self.params.push(key.clone());
                    on.push(format!(
                        "{alias}.t{r} = ${}::text AND {alias}.k{r} = ${}::text",
                        self.params.len() - 1,
                        self.params.len()
                    ));
                }
                End::At(..) | End::Free => {
                    let Some(Pos::Var(v)) = &p.pos[r] else { continue };
                    match core_first.get(v) {
                        Some(&Place::Path(j0, r0)) if j0 == j && r0 == r => (),
                        Some(&at) => {
                            let (a, r0) = at.alias();
                            on.push(format!("{alias}.t{r} = {a}.t{r0} AND {alias}.k{r} = {a}.k{r0}"));
                        }
                        None => return Err(EngineError::internal("a path's end with no place in the core")),
                    }
                }
            }
        }
        Ok((format!("(SELECT DISTINCT {columns} FROM {pairs})"), on))
    }

    /// The rows PostgreSQL expects of an end's pattern; none of a constant
    fn rows(&self, end: &End) -> EResult<f64> {
        match end {
            End::Constant(..) => Ok(0.0),
            End::Free => Ok(f64::INFINITY),
            End::At(i0, _) => {
                let (_, q) = &self.patterns[*i0];
                let keyed: Vec<usize> = q.variables().filter(|(_, v)| self.shared.contains(*v)).map(|(r, _)| r).collect();
                let mut ps = Vec::new();
                let union = pattern_sql(q, &keyed, &[0; 4], self.restrict, self.mappings, &mut ps)?;
                self.host.sql_estimate(&format!("SELECT 1 FROM ({union}) AS kb_e"), &ps)
            }
        }
    }
}

/// The tags the terms of a pattern's position may have, by its branches' maps
/// and its store quads
fn tags(p: &Pattern, role: usize, mappings: &[Rc<Compiled>]) -> FxHashSet<String> {
    let mut out = FxHashSet::default();
    for b in &p.branches {
        let Some(m) = b.map(role) else { continue };
        match map_key(&b.ctx(), m) {
            Some((KeySql::Tag(t) | KeySql::Param(t), _)) => {
                out.insert(t);
            }
            // a map with no key, or a tag SQL makes: none that is one
            _ => {
                out.insert(String::new());
                out.insert("?".to_owned());
            }
        }
    }
    for q in &p.store {
        if let Some((tag, _)) = q[role].as_ref().and_then(|t| term_key(t, mappings)) {
            out.insert(tag);
        }
    }
    out
}

/// The condition that two positions' terms are the same: their keys, and
/// their tags unless both give one, the same
fn equal(a: &str, ra: usize, tags_a: &FxHashSet<String>, b: &str, rb: usize, tags_b: &FxHashSet<String>) -> String {
    if tags_a.len() == 1 && tags_a == tags_b {
        format!("{a}.k{ra} = {b}.k{rb}")
    } else {
        format!("{a}.t{ra} = {b}.t{rb} AND {a}.k{ra} = {b}.k{rb}")
    }
}

/// The SQL of a path's pairs, as common table expressions: each a relation of
/// the tags and keys of a pair's start and end (s_t, s_k, o_t, o_k)
struct PathSql<'a> {
    ctes: &'a mut Vec<String>,
    params: &'a mut Vec<String>,
    mappings: &'a [Rc<Compiled>],
    prefix: String,
    count: usize,
    /// each link's edges, once
    edges: HashMap<*const Steps, String>,
}

impl PathSql<'_> {
    fn cte(&mut self, sql: String) -> String {
        let name = format!("{}_{}", self.prefix, self.count);
        self.count += 1;
        self.ctes.push(format!("{name} AS ({sql})"));
        name
    }

    /// The pairs the steps link, from the terms of a seed (a relation of t and
    /// k) or from any; None for zero steps without a seed, whose pairs are
    /// every node of the graph
    fn pairs(&mut self, steps: &Steps, seed: Option<&str>) -> EResult<Option<String>> {
        Ok(Some(match steps {
            Steps::Link { .. } => {
                let edges = match self.edges.get(&(steps as *const Steps)) {
                    Some(name) => name.clone(),
                    None => {
                        let sql = edges_sql(steps, self.mappings, self.params)?;
                        let name = self.cte(sql);
                        self.edges.insert(steps as *const Steps, name.clone());
                        name
                    }
                };
                match seed {
                    None => edges,
                    Some(seed) => self.cte(format!(
                        "SELECT e.s_t, e.s_k, e.o_t, e.o_k FROM {edges} AS e JOIN {seed} AS kb_s ON e.s_t = kb_s.t AND e.s_k = kb_s.k"
                    )),
                }
            }
            Steps::Sequence(x, y) => {
                let Some(first) = self.pairs(x, seed)? else { return Ok(None) };
                let middle = self.cte(format!("SELECT DISTINCT o_t AS t, o_k AS k FROM {first}"));
                let Some(second) = self.pairs(y, Some(&middle))? else { return Ok(None) };
                self.cte(format!(
                    "SELECT a.s_t, a.s_k, b.o_t, b.o_k FROM {first} AS a JOIN {second} AS b ON b.s_t = a.o_t AND b.s_k = a.o_k"
                ))
            }
            Steps::Alternative(x, y) => {
                let Some(a) = self.pairs(x, seed)? else { return Ok(None) };
                let Some(b) = self.pairs(y, seed)? else { return Ok(None) };
                self.cte(format!("SELECT s_t, s_k, o_t, o_k FROM {a} UNION SELECT s_t, s_k, o_t, o_k FROM {b}"))
            }
            Steps::OneOrMore(x) => match self.closure(x, seed)? {
                Some(c) => c,
                None => return Ok(None),
            },
            Steps::ZeroOrMore(x) | Steps::ZeroOrOne(x) => {
                let Some(seed) = seed else { return Ok(None) };
                let more = match steps {
                    Steps::ZeroOrMore(_) => self.closure(x, Some(seed))?,
                    _ => self.pairs(x, Some(seed))?,
                };
                let Some(more) = more else { return Ok(None) };
                self.cte(format!(
                    "SELECT t AS s_t, k AS s_k, t AS o_t, k AS o_k FROM {seed} UNION SELECT s_t, s_k, o_t, o_k FROM {more}"
                ))
            }
        }))
    }

    /// One or more steps: a recursive CTE, from the pairs of one step, each
    /// pair's end followed by a step more -- UNION stops at the pairs it has
    fn closure(&mut self, x: &Steps, seed: Option<&str>) -> EResult<Option<String>> {
        let Some(first) = self.pairs(x, seed)? else { return Ok(None) };
        let step = match seed {
            None => Some(first.clone()),
            Some(_) => self.pairs(x, None)?,
        };
        let Some(step) = step else { return Ok(None) };
        let name = format!("{}_{}", self.prefix, self.count);
        self.count += 1;
        self.ctes.push(format!(
            "{name}(s_t, s_k, o_t, o_k) AS (SELECT s_t, s_k, o_t, o_k FROM {first} UNION \
             SELECT c.s_t, c.s_k, e.o_t, e.o_k FROM {name} AS c JOIN {step} AS e ON e.s_t = c.o_t AND e.s_k = c.o_k)"
        ));
        Ok(Some(name))
    }
}

/// A link's edges: its units' subjects and objects, and its store quads', as
/// tags and keys -- the subject's first, or the object's when inverse
fn edges_sql(steps: &Steps, mappings: &[Rc<Compiled>], params: &mut Vec<String>) -> EResult<String> {
    let Steps::Link {
        predicate,
        branches,
        store,
        inverse,
    } = steps
    else {
        return Err(EngineError::internal("the edges of a path's steps that are not a link"));
    };
    let term = Term::from(predicate.clone());
    let mut selects = Vec::new();
    for b in branches {
        let ctx = b.ctx();
        let unit = &b.compiled.units[b.unit];
        let bind = ctx.bind(&unit.predicate, &term);
        let not_null = [&unit.subject, &unit.predicate, &unit.object];
        let Some(conds) = eval::conditions(&ctx, &[&bind], &not_null, params) else {
            continue;
        };
        let keys = |m: &CMap| map_key(&ctx, m).ok_or_else(|| EngineError::internal("a path's term map has no key"));
        let (mut start, mut end) = (keys(&unit.subject)?, keys(&unit.object)?);
        if *inverse {
            std::mem::swap(&mut start, &mut end);
        }
        let columns = [start.0.write(params), start.1.write(params), end.0.write(params), end.1.write(params)];
        let mut sql = format!(
            "SELECT {} AS s_t, {} AS s_k, {} AS o_t, {} AS o_k FROM {}",
            columns[0],
            columns[1],
            columns[2],
            columns[3],
            eval::from_sql(&ctx)
        );
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        selects.push(sql);
    }
    let mut rows = Vec::new();
    for pair in store {
        let (start, end) = if *inverse { (&pair[1], &pair[0]) } else { (&pair[0], &pair[1]) };
        let mut row = Vec::new();
        for t in [start, end] {
            let (tag, key) = term_key(t, mappings).ok_or_else(|| EngineError::internal("a path's store term has no key"))?;
            params.push(tag);
            row.push(format!("${}::text", params.len()));
            params.push(key);
            row.push(format!("${}::text", params.len()));
        }
        rows.push(format!("({})", row.join(", ")));
    }
    if !rows.is_empty() {
        selects.push(format!("SELECT * FROM (VALUES {}) AS kb_v(s_t, s_k, o_t, o_k)", rows.join(", ")));
    }
    if selects.is_empty() {
        selects.push("SELECT NULL::text AS s_t, NULL::text AS s_k, NULL::text AS o_t, NULL::text AS o_k WHERE false".to_owned());
    }
    Ok(selects.join(" UNION ALL "))
}

/// A pattern's SQL: the UNION ALL of its branches' SELECTs and of its store
/// quads' VALUES, each row the number of its branch (b), the tag and the key
/// of each position keyed (t{r}, k{r}), and width[r] value columns of each
/// position (v{r}_{k}).  The branches restrict rules out are left out, the
/// others' SQL checks it as it can.
fn pattern_sql(
    p: &Pattern,
    keyed: &[usize],
    width: &[usize; 4],
    restrict: &[(Variable, Restriction)],
    mappings: &[Rc<Compiled>],
    params: &mut Vec<String>,
) -> EResult<String> {
    let mut branches_sql = Vec::new();
    for (n, b) in p.branches.iter().enumerate() {
        let ctx = b.ctx();
        let mut binds = Vec::new();
        for r in ROLES {
            if let (Some(Pos::Const(t)), Some(m)) = (&p.pos[r], b.map(r)) {
                binds.push(ctx.bind(m, t));
            }
        }
        let unit = &b.compiled.units[b.unit];
        let mut not_null = vec![&unit.subject, &unit.predicate, &unit.object];
        not_null.extend(b.map(G));
        let bind_refs: Vec<&Bind> = binds.iter().collect();
        let Some(mut conds) = eval::conditions(&ctx, &bind_refs, &not_null, params) else {
            continue;
        };
        let mut never = false;
        for (r, v) in p.variables() {
            let Some(m) = b.map(r) else { continue };
            for (_, x) in restrict.iter().filter(|(rv, _)| rv == v) {
                match restriction_sql(&ctx, m, x, params) {
                    Err(()) => never = true,
                    Ok(Some(c)) => conds.push(c),
                    Ok(None) => (),
                }
            }
        }
        if never {
            continue;
        }
        let mut out = vec![format!("{n} AS b")];
        for &r in keyed {
            let (tag, key) = b
                .map(r)
                .and_then(|m| map_key(&ctx, m))
                .ok_or_else(|| EngineError::internal("a shared variable's term map has no key"))?;
            out.push(format!("{} AS t{r}", tag.write(params)));
            out.push(format!("{} AS k{r}", key.write(params)));
        }
        for r in p.roles() {
            let cols = b.map(r).map(CMap::columns).unwrap_or_default();
            for k in 0..width[r] {
                out.push(match cols.get(k) {
                    Some(c) => format!("{} AS v{r}_{k}", eval::value_sql(&ctx, *c)),
                    None => format!("NULL::text AS v{r}_{k}"),
                });
            }
        }
        let mut sql = format!("SELECT {} FROM {}", out.join(", "), eval::from_sql(&ctx));
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        branches_sql.push(sql);
    }
    if !p.store.is_empty() {
        // the store's quads, which met the restrictions in pattern(): the
        // number of the quad, and its keys
        let mut rows = Vec::new();
        for (n, q) in p.store.iter().enumerate() {
            let mut row = Vec::new();
            for &r in keyed {
                let (tag, key) = q[r]
                    .as_ref()
                    .and_then(|t| term_key(t, mappings))
                    .ok_or_else(|| EngineError::internal("a shared variable's store term has no key"))?;
                params.push(tag);
                row.push(format!("${}::text", params.len()));
                params.push(key);
                row.push(format!("${}::text", params.len()));
            }
            row.push(format!("{n}"));
            rows.push(format!("({})", row.join(", ")));
        }
        let mut names: Vec<String> = keyed.iter().flat_map(|r| [format!("t{r}"), format!("k{r}")]).collect();
        names.push("n".to_owned());
        let mut out = vec![format!("{} AS b", p.branches.len())];
        out.extend(keyed.iter().flat_map(|r| [format!("kb_v.t{r}"), format!("kb_v.k{r}")]));
        for r in p.roles() {
            for k in 0..width[r] {
                out.push(if k == 0 {
                    format!("kb_v.n::text AS v{r}_{k}")
                } else {
                    format!("NULL::text AS v{r}_{k}")
                });
            }
        }
        if !rows.is_empty() {
            branches_sql.push(format!(
                "SELECT {} FROM (VALUES {}) AS kb_v({})",
                out.join(", "),
                rows.join(", "),
                names.join(", ")
            ));
        }
    }
    if branches_sql.is_empty() {
        // no unit nor quad can match: the part has no solution
        let mut out = vec!["NULL::int AS b".to_owned()];
        out.extend(keyed.iter().flat_map(|r| [format!("NULL::text AS t{r}"), format!("NULL::text AS k{r}")]));
        for r in p.roles() {
            out.extend((0..width[r]).map(|k| format!("NULL::text AS v{r}_{k}")));
        }
        branches_sql.push(format!("SELECT {} WHERE false", out.join(", ")));
    }
    Ok(branches_sql.join(" UNION ALL "))
}

/// A restriction as a condition of a branch's map: Ok(None) when the SQL
/// cannot tell, Err when no term of the map meets it
fn restriction_sql(ctx: &Context<'_>, map: &CMap, r: &Restriction, params: &mut Vec<String>) -> Result<Option<String>, ()> {
    match r {
        Restriction::Prefix(prefix) => match &map.value {
            CValue::Constant(t, _) => match t {
                Term::BlankNode(_) => Err(()),
                t if satisfies(t, r) => Ok(None),
                _ => Err(()),
            },
            CValue::Column(c) => {
                let text = matches!(ctx.column(*c).type_oid, 19 | 25 | 1043);
                let string = match map.term_type {
                    TermType::Iri => ctx.compiled.base.is_none(),
                    TermType::Literal => {
                        map.language.is_some() || map.datatype.as_ref().is_none_or(|d| *d == xsd::STRING)
                    }
                    // STR() of a blank node is an error: the FILTER keeps none
                    TermType::BlankNode => return Err(()),
                };
                if !(text && string) {
                    return Ok(None);
                }
                params.push(prefix.clone());
                Ok(Some(format!("starts_with({}, ${}::text)", eval::column_sql(ctx, *c), params.len())))
            }
            CValue::Template(parts) => match (map.term_type, parts.first()) {
                (TermType::BlankNode, _) => Err(()),
                (TermType::Iri, Some(CPart::Text(t))) if ctx.compiled.base.is_none() || has_scheme(t) => {
                    if t.starts_with(prefix.as_str()) || prefix.starts_with(t.as_str()) {
                        Ok(None)
                    } else {
                        Err(())
                    }
                }
                _ => Ok(None),
            },
        },
        Restriction::OneOf(terms) => {
            let mut alternatives = Vec::new();
            for t in terms {
                match ctx.bind(map, t) {
                    Bind::Never => (),
                    Bind::Any => return Ok(None),
                    bind => {
                        if let Some(conds) = eval::conditions(ctx, &[&bind], &[], params) {
                            alternatives.push(format!("({})", and(conds)));
                        }
                    }
                }
            }
            if alternatives.is_empty() {
                Err(())
            } else {
                Ok(Some(format!("({})", alternatives.join(" OR "))))
            }
        }
    }
}

/// Whether a template's text begins with a scheme: what the template makes is
/// then an absolute IRI or an error, never resolved against the base
fn has_scheme(t: &str) -> bool {
    let Some((scheme, _)) = t.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Whether a term meets a restriction
fn satisfies(t: &Term, r: &Restriction) -> bool {
    match r {
        Restriction::Prefix(p) => match t {
            Term::NamedNode(n) => n.as_str().starts_with(p.as_str()),
            Term::Literal(l) => l.value().starts_with(p.as_str()),
            _ => false,
        },
        Restriction::OneOf(terms) => {
            let k = key::term_key(t.as_ref()).0;
            terms.iter().any(|x| key::term_key(x.as_ref()).0 == k)
        }
    }
}

fn and(conditions: Vec<String>) -> String {
    if conditions.is_empty() {
        "true".to_owned()
    } else {
        conditions.join(" AND ")
    }
}

// ---------------------------------------------------------------------
// The SERVICE
// ---------------------------------------------------------------------

/// What a handler holds.  spareval asks a handler to be Send and Sync; the
/// engine runs on the backend's thread and starts none (docs/design.md §1),
/// so neither the host nor the plan ever leaves it.
struct OneThread {
    host: Host,
    plan: Rc<Plan>,
}

unsafe impl Send for OneThread {}
unsafe impl Sync for OneThread {}

struct Handler(OneThread);

impl ServiceHandler for Handler {
    type Error = EngineError;

    fn handle(
        &self,
        _expression: &QueryExpression,
        _base_iri: Option<&oxiri::Iri<oxstr::OxString>>,
    ) -> Result<QuerySolutionIter<'static>, EngineError> {
        solutions(self.0.host, Rc::clone(&self.0.plan))
    }
}

/// The plan's SQL run, its rows made into solutions
fn solutions(host: Host, plan: Rc<Plan>) -> EResult<QuerySolutionIter<'static>> {
    // run once: planned for its parameters, and joined by hashes -- the keys'
    // columns have no statistics, and a nested loop on a wrong estimate reads
    // a pattern's UNION ALL again for each row
    let cursor = host.sql_open(&plan.sql, &plan.params, KB_SQL_HASH_JOINS)?;
    let variables = Arc::clone(&plan.variables);
    // a row's keys hashed to 128 bits: what tells the solutions apart for
    // the dedup, at a fifth of the keys' memory
    let mut seen: FxHashSet<u128> = FxHashSet::default();
    let iter = cursor.filter_map(move |row| {
        let row = match row {
            Ok(r) => r,
            Err(e) => return Some(Err(QueryEvaluationError::Service(Box::new(e)))),
        };
        match solution(&host, &plan, &row) {
            Ok(Some((keys, values))) => {
                let mut h = SipHasher24::new();
                for k in &keys {
                    h.write(k);
                }
                seen.insert(h.finish128().into()).then_some(Ok(values))
            }
            Ok(None) => None,
            Err(e) => Some(Err(QueryEvaluationError::Service(Box::new(e)))),
        }
    });
    Ok(QuerySolutionIter::from_tuples(variables, iter))
}

/// A row's solution, with its terms' keys; None when its terms do not make
/// one: a NULL, a constant or a shared variable of the core not matched.  An
/// OPTIONAL the row has no match of leaves its variables unbound; one whose
/// match the row's terms do not confirm is an error, the SQL's conditions for
/// it being exact (exact()).
#[allow(clippy::type_complexity)]
fn solution(host: &Host, plan: &Plan, row: &[Option<String>]) -> EResult<Option<(Vec<Key>, Vec<Option<Term>>)>> {
    host.poll()?;
    let mut keys: Vec<Option<Key>> = vec![None; plan.variables.len()];
    let mut values: Vec<Option<Term>> = vec![None; plan.variables.len()];
    // a path's end that no pattern makes: its term by its tag and its key
    for &(tag_col, key_col, i) in &plan.ends {
        let (Some(tag), Some(k)) = (
            row.get(tag_col).and_then(|v| v.as_deref()),
            row.get(key_col).and_then(|v| v.as_deref()),
        ) else {
            return Ok(None);
        };
        let t = key_term(tag, k, &plan.mappings)?;
        keys[i] = Some(key::term_key(t.as_ref()).0);
        values[i] = Some(t);
    }
    for ((g, p), (b_col, cols)) in plan.patterns.iter().zip(&plan.columns) {
        let Some(b) = row.get(*b_col).and_then(|v| v.as_deref()) else {
            if *g == 0 {
                return Ok(None);
            }
            continue;
        };
        let b: usize = b.parse().map_err(|_| EngineError::internal("a branch number is not one"))?;
        if !matched(plan, p, b, cols, row, &mut keys, &mut values)? {
            if *g == 0 {
                return Ok(None);
            }
            return Err(EngineError::internal("a row of an OPTIONAL the SQL joined does not match its pattern"));
        }
    }
    for &(col, i) in &plan.exists {
        if row.get(col).and_then(|v| v.as_deref()) == Some("t") {
            let t = Term::from(Literal::from(true));
            keys[i] = Some(key::term_key(t.as_ref()).0);
            values[i] = Some(t);
        }
    }
    Ok(Some((keys.into_iter().map(|k| k.unwrap_or(DEFAULT_GRAPH)).collect(), values)))
}

/// A pattern's terms in a row, by its branch b, checked against its
/// constants and the variables bound so far, which it binds
#[allow(clippy::too_many_arguments)]
fn matched(
    plan: &Plan,
    p: &Pattern,
    b: usize,
    cols: &[(usize, usize); 4],
    row: &[Option<String>],
    keys: &mut [Option<Key>],
    values: &mut [Option<Term>],
) -> EResult<bool> {
    {
        let quad: [Option<Term>; 4] = if b == p.branches.len() {
            let (c, _) = cols[p.roles().next().unwrap_or(S)];
            let Some(n) = row.get(c).and_then(|v| v.as_deref()).and_then(|v| v.parse::<usize>().ok()) else {
                return Ok(false);
            };
            p.store.get(n).cloned().ok_or_else(|| EngineError::internal("a store quad out of range"))?
        } else {
            let branch = p
                .branches
                .get(b)
                .ok_or_else(|| EngineError::internal("a branch out of range"))?;
            let ctx = branch.ctx();
            let mut quad: [Option<Term>; 4] = [None, None, None, None];
            for r in p.roles() {
                let Some(map) = branch.map(r) else { continue };
                let map_cols = map.columns();
                let (start, _) = cols[r];
                let mut vals: Vec<Option<String>> = Vec::with_capacity(map_cols.len());
                for (k, c) in map_cols.iter().enumerate() {
                    let v = row.get(start + k).cloned().flatten();
                    vals.push(match (v, eval::value_kind(&ctx, *c)) {
                        (Some(v), Some(eval::Value::Float4)) => Some(eval::float_value(&v, true).ok_or_else(eval::bad_float)?),
                        (Some(v), Some(eval::Value::Float8)) => Some(eval::float_value(&v, false).ok_or_else(eval::bad_float)?),
                        (v, _) => v,
                    });
                }
                let place: Vec<(ColRef, usize)> = map_cols.iter().enumerate().map(|(k, c)| (*c, k)).collect();
                let made = ctx.generate(
                    map,
                    &Row {
                        values: &vals,
                        place: &place,
                    },
                )?;
                let Some(made) = made else {
                    return Ok(false);
                };
                quad[r] = Some(made);
            }
            match &quad[S] {
                Some(Term::Literal(_)) => {
                    return Err(eval::data_error(format!(
                        "the subject {} is a literal",
                        quad[S].as_ref().map(Term::to_string).unwrap_or_default()
                    )));
                }
                None => return Ok(false),
                _ => (),
            }
            if !matches!(&quad[P], Some(Term::NamedNode(_))) {
                return Err(eval::data_error("the predicate is not an IRI"));
            }
            if let Some(g) = &quad[G] {
                if !matches!(g, Term::NamedNode(_)) {
                    return Err(eval::data_error(format!("the graph {g} is not an IRI")));
                }
            }
            quad
        };
        for r in p.roles() {
            let made = quad[r].as_ref();
            match &p.pos[r] {
                Some(Pos::Const(t)) => {
                    if made.map(|m| key::term_key(m.as_ref()).0) != Some(key::term_key(t.as_ref()).0) {
                        return Ok(false);
                    }
                }
                Some(Pos::Var(v)) => {
                    let Some(made) = made else { return Ok(false) };
                    let k = key::term_key(made.as_ref()).0;
                    let i = plan
                        .variables
                        .iter()
                        .position(|x| x == v)
                        .ok_or_else(|| EngineError::internal("a variable not in the plan"))?;
                    match keys[i] {
                        Some(existing) if existing != k => return Ok(false),
                        Some(_) => (),
                        None => {
                            keys[i] = Some(k);
                            values[i] = Some(made.clone());
                        }
                    }
                }
                None => (),
            }
        }
    }
    Ok(true)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_keys() {
        let int = |v: &str, dt: NamedNode| literal_key(&Literal::new_typed_literal(v.to_owned(), dt));
        assert_eq!(int("007", xsd::INTEGER), Some(("Li".into(), "7".into())));
        assert_eq!(int("7", xsd::INT), Some(("Li".into(), "7".into())));
        assert_eq!(int("1", xsd::BOOLEAN), Some(("Lb".into(), "true".into())));
        assert_eq!(int("1.50", xsd::DECIMAL), None);
        assert_eq!(
            literal_key(&Literal::new_language_tagged_literal_unchecked("a", "EN")),
            Some(("L@en".into(), "a".into()))
        );
        assert_eq!(literal_key(&Literal::new_simple_literal("a")), Some(("Ls".into(), "a".into())));
    }

    fn bgps(e: &QueryExpression, out: &mut Vec<usize>) {
        use QueryExpression as E;
        match e {
            E::Bgp { patterns } => out.push(patterns.len()),
            E::Join { left, right }
            | E::LeftJoin { left, right, .. }
            | E::Union { left, right }
            | E::Minus { left, right }
            | E::Lateral { left, right } => {
                bgps(left, out);
                bgps(right, out);
            }
            E::Filter { inner, .. }
            | E::Graph { inner, .. }
            | E::Extend { inner, .. }
            | E::OrderBy { inner, .. }
            | E::Project { inner, .. }
            | E::Distinct { inner }
            | E::Reduced { inner }
            | E::Slice { inner, .. }
            | E::Group { inner, .. }
            | E::Service { inner, .. } => bgps(inner, out),
            E::Path { .. } | E::Values { .. } => (),
        }
    }

    /// The kinds of an expression's nodes, and the sizes of its basic graph
    /// patterns
    fn shape(e: &QueryExpression) -> String {
        use QueryExpression as E;
        match e {
            E::Bgp { patterns } => format!("bgp{}", patterns.len()),
            E::Join { left, right } => format!("join({},{})", shape(left), shape(right)),
            E::LeftJoin { left, right, .. } => format!("leftjoin({},{})", shape(left), shape(right)),
            E::Filter { inner, .. } => format!("filter({})", shape(inner)),
            E::Extend { inner, .. } => format!("extend({})", shape(inner)),
            other => format!("{other}"),
        }
    }

    fn flattened(sparql: &str) -> (Vec<usize>, String) {
        let query = spargebra::SparqlParser::new().parse_query(sparql).unwrap();
        let Query::Select(select) = query else { panic!("not a SELECT") };
        let e = flatten(select.expression);
        let mut out = Vec::new();
        bgps(&e, &mut out);
        out.sort_unstable();
        (out, e.to_string())
    }

    #[test]
    fn groups_flatten() {
        // query 02 of Story-KG: the OPTIONAL between the two basic graph
        // patterns is lifted, and they merge; the OPTIONALs after them stay
        let (b, e) = flattened(
            "SELECT * WHERE { ?r a <x:R> ; <x:s> ?si . OPTIONAL { ?r <x:label> ?label } \
             ?a <x:body> ?r ; <x:target> ?t . OPTIONAL { ?t <x:line> ?line } FILTER(isIRI(?r)) }",
        );
        assert_eq!(b, vec![1, 1, 4], "{e}");
        let (b, e) = flattened(
            "PREFIX kgo: <https://example.org/story-kg/ontology#> \
             PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> \
             PREFIX schema: <http://schema.org/> PREFIX oa: <http://www.w3.org/ns/oa#> \
             SELECT * WHERE { ?r a kgo:Relationship ; rdf:subject ?si ; rdf:predicate ?predicate ; rdf:object ?oi . \
             ?si schema:name ?s . ?oi schema:name ?o . OPTIONAL { ?r kgo:relationLabel ?label } \
             ?a oa:hasBody ?r ; oa:hasTarget ?t . ?t oa:hasSelector ?sel . ?sel a oa:TextQuoteSelector ; oa:exact ?quote . }",
        );
        assert_eq!(b, vec![1, 11], "{e}");
        // ?label of the OPTIONAL is used after it: the OPTIONAL stays
        let (b, e) = flattened(
            "SELECT * WHERE { ?r a <x:R> . OPTIONAL { ?r <x:label> ?label } ?a <x:body> ?label }",
        );
        assert_eq!(b, vec![1, 1, 1], "{e}");
        // query 08: a BIND between two basic graph patterns, an OPTIONAL
        // after them -- the patterns merge, and the OPTIONAL follows them
        let (b, e) = flattened(
            "SELECT * WHERE { ?p a <x:P> ; <x:flow> ?f . ?f <x:source> ?a . ?a a ?at . \
             FILTER(STRSTARTS(STR(?at), \"x:\")) BIND(STRAFTER(STR(?at), \"#\") AS ?t) \
             ?f <x:target> ?b . ?b <x:name> ?to . OPTIONAL { ?f <x:cond> ?c } }",
        );
        assert_eq!(b, vec![1, 6], "{e}");
        let query = spargebra::SparqlParser::new()
            .parse_query(
                "SELECT * WHERE { ?p <x:flow> ?f . ?f <x:source> ?a . ?a a ?at . FILTER(isIRI(?at)) \
                 BIND(STR(?at) AS ?t) ?f <x:target> ?b . OPTIONAL { ?f <x:cond> ?c } }",
            )
            .unwrap();
        let Query::Select(select) = query else { panic!("not a SELECT") };
        let QueryExpression::Project { inner, .. } = flatten(select.expression) else { panic!("no projection") };
        assert_eq!(shape(&inner), "filter(extend(leftjoin(bgp4,bgp1)))", "{inner}");
        // the BIND's variable is used after it: it stays
        let (b, e) = flattened("SELECT * WHERE { ?a <x:p> ?x . BIND(?x AS ?y) ?b <x:q> ?y }");
        assert_eq!(b, vec![1, 1], "{e}");
        // the BIND's expression uses a variable only the patterns after it bind
        let (b, e) = flattened("SELECT * WHERE { ?a <x:p> ?x . BIND(?z AS ?y) ?b <x:q> ?z }");
        assert_eq!(b, vec![1, 1], "{e}");
    }

    #[test]
    fn schemes() {
        assert!(has_scheme("http://example.org/"));
        assert!(has_scheme("urn:x"));
        assert!(!has_scheme("a/b:c"));
        assert!(!has_scheme("/x"));
        assert!(!has_scheme("1x:y"));
    }
}
