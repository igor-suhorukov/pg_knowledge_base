# SPDX-License-Identifier: Apache-2.0
"""The W3C SPARQL 1.0 and 1.1 test cases (w3c/rdf-tests at the commit
oxigraph's testsuite pins), run against the extension's store (docs/design.md
§7): oxigraph's own evaluator passes them over its in-memory dataset, so what
fails here fails in the store -- its keys, its scans, its graphs, its writes.

For each query evaluation test: the store emptied, the test's data loaded
(kb.load, its default graph and named graphs), the query run (kb.sparql_text,
SPARQL's JSON results or N-Triples), the results compared with the expected
ones: solutions as multisets with blank nodes mapped, graphs isomorphic,
typed literals by value -- the store keeps a literal of a datatype oxigraph
knows as its value, as oxigraph's store does (engine/src/key.rs).  For each
update evaluation test: the data loaded, kb.sparql_update, the store dumped and
compared with the expected dataset.  Syntax tests: the query or update parsed
-- run on the empty store -- or refused with SQLSTATE 42601.

    run_w3c.py <rdf-tests checkout> <run directory> [test name...]
"""
import datetime
import decimal
import json
import logging
import os
import sys
import traceback

import psycopg
import rdflib
from rdflib import RDF, RDFS, XSD, BNode, Graph, Literal, URIRef
from rdflib.collection import Collection
from rdflib.compare import isomorphic

MF = rdflib.Namespace("http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#")
QT = rdflib.Namespace("http://www.w3.org/2001/sw/DataAccess/tests/test-query#")
UT = rdflib.Namespace("http://www.w3.org/2009/sparql/tests/test-update#")
RS = rdflib.Namespace("http://www.w3.org/2001/sw/DataAccess/tests/result-set#")
WEB = "https://w3c.github.io/rdf-tests/"
SOL = rdflib.Namespace("urn:kb:test:solution#")

MANIFESTS = [
    "sparql/sparql10/manifest-syntax.ttl",
    "sparql/sparql10/manifest-evaluation.ttl",
    "sparql/sparql11/manifest-sparql11-query.ttl",
    "sparql/sparql11/manifest-sparql11-update.ttl",
    "sparql/sparql11/json-res/manifest.ttl",
    "sparql/sparql11/csv-tsv-res/manifest.ttl",
]

# what oxigraph's own testsuite leaves out (testsuite/tests/sparql.rs), and why
OXIGRAPH_SKIPS = {
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/open-world/manifest#date-2":
        "oxigraph: XSD 1.1 equality on dates",
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/reduced/manifest#reduced-1":
        "oxigraph: relies on naive iteration of the input file",
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/reduced/manifest#reduced-2":
        "oxigraph: relies on naive iteration of the input file",
    "http://www.w3.org/2009/sparql/docs/tests/data-sparql11/csv-tsv-res/manifest#csv01":
        "oxigraph: CSVResultFormatTest not run",
    "http://www.w3.org/2009/sparql/docs/tests/data-sparql11/csv-tsv-res/manifest#csv02":
        "oxigraph: CSVResultFormatTest not run",
    "http://www.w3.org/2009/sparql/docs/tests/data-sparql11/csv-tsv-res/manifest#csv03":
        "oxigraph: CSVResultFormatTest not run",
}

# where the store answers otherwise than oxigraph's in-memory dataset, by
# design: it keeps a literal of a datatype oxigraph knows as its value, as
# oxigraph's own store does, so "01"^^xsd:integer is "1"^^xsd:integer -- a
# pattern matches both, sameTerm() holds, STR() and DISTINCT see one value
CANONICAL = "the store keeps literals as values, as oxigraph's store does"
EXPECTED_DIFFERENCES = {
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/open-world/manifest#open-eq-01": CANONICAL,
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/expr-builtin/manifest#dawg-str-1": CANONICAL,
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/expr-builtin/manifest#dawg-str-2": CANONICAL,
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/expr-builtin/manifest#sameTerm-simple": CANONICAL,
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/expr-builtin/manifest#sameTerm-eq": CANONICAL,
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/expr-builtin/manifest#sameTerm-not-eq": CANONICAL,
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/expr-equals/manifest#eq-graph-1": CANONICAL,
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/expr-equals/manifest#eq-graph-2": CANONICAL,
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/distinct/manifest#distinct-1": CANONICAL,
    "http://www.w3.org/2001/sw/DataAccess/tests/data-r2/distinct/manifest#distinct-9": CANONICAL,
    # spareval's BNODE(str) is the blank node named str, one node for every
    # solution with that str, where SPARQL 1.1 asks a node a solution;
    # oxigraph's testsuite maps blank nodes solution by solution and does not
    # see it, this runner maps them across the results
    "http://www.w3.org/2009/sparql/docs/tests/data-sparql11/functions/manifest#bnode01":
        "spareval: BNODE(str) one node for every solution",
}

INTEGERS = {XSD[t] for t in ("integer", "int", "long", "short", "byte", "unsignedLong",
                             "unsignedInt", "unsignedShort", "unsignedByte",
                             "nonNegativeInteger", "nonPositiveInteger",
                             "positiveInteger", "negativeInteger")}


def local(url):
    if not str(url).startswith(WEB):
        raise ValueError(f"not a test file: {url}")
    return os.path.join(ROOT, str(url)[len(WEB):])


def read(url):
    with open(local(url), encoding="utf-8") as f:
        return f.read()


def rdf_format(url):
    url = str(url)
    if url.endswith(".rdf"):
        return "rdfxml", "xml"
    if url.endswith(".nt"):
        return "ntriples", "nt"
    if url.endswith(".nq"):
        return "nquads", "nquads"
    if url.endswith(".trig"):
        return "trig", "trig"
    return "turtle", "turtle"


def normal(term):
    """A literal by its value, as the store's keys compare them."""
    if not isinstance(term, Literal):
        return term
    if term.language:
        return Literal(str(term), lang=term.language.lower())
    dt, lex = term.datatype, str(term)
    if dt is None or dt == XSD.string:
        return Literal(lex)
    try:
        if dt in INTEGERS:
            return Literal(str(int(lex.strip())), datatype=XSD.integer)
        if dt == XSD.decimal:
            d = decimal.Decimal(lex.strip()).normalize()
            return Literal(format(d, "f"), datatype=XSD.decimal)
        if dt in (XSD.double, XSD.float):
            v = float(lex.strip().replace("INF", "inf"))
            return Literal(repr(v), datatype=dt)
        if dt == XSD.boolean:
            if lex.strip() in ("true", "1"):
                return Literal("true", datatype=dt)
            if lex.strip() in ("false", "0"):
                return Literal("false", datatype=dt)
        if dt == XSD.dateTime:
            v = datetime.datetime.fromisoformat(lex.strip().replace("Z", "+00:00"))
            return Literal(v.isoformat(), datatype=dt)
    except (ValueError, decimal.InvalidOperation, OverflowError):
        pass
    return term


def solutions_graph(variables, rows):
    """Solutions as an RDF graph -- a blank node a solution -- so that graph
    isomorphism maps blank nodes and counts duplicates."""
    g = Graph()
    for row in rows:
        node = BNode()
        g.add((node, RDF.type, SOL.Solution))
        for var, value in row.items():
            g.add((node, SOL[var], normal(value)))
    return g


def json_term(t):
    if t["type"] == "uri":
        return URIRef(t["value"])
    if t["type"] == "bnode":
        return BNode(t["value"])
    if "xml:lang" in t:
        return Literal(t["value"], lang=t["xml:lang"])
    if "datatype" in t:
        return Literal(t["value"], datatype=URIRef(t["datatype"]))
    return Literal(t["value"])


def results_from_json(text):
    data = json.loads(text)
    if "boolean" in data:
        return ("boolean", data["boolean"])
    variables = data["head"].get("vars", [])
    rows = [{k: json_term(v) for k, v in b.items()} for b in data["results"]["bindings"]]
    return ("solutions", set(variables), rows)


def results_from_file(url):
    """Expected results: SPARQL XML, JSON, CSV or TSV, or RDF -- a result set
    in the rs: vocabulary, or a graph."""
    url = str(url)
    ext = url.rsplit(".", 1)[-1]
    if ext in ("srx", "srj", "tsv", "csv"):
        fmt = {"srx": "xml", "srj": "json", "tsv": "tsv", "csv": "csv"}[ext]
        with open(local(url), "rb") as f:
            r = rdflib.query.Result.parse(f, format=fmt)
        if r.type == "ASK":
            return ("boolean", bool(r.askAnswer))
        variables = {str(v) for v in r.vars}
        rows = []
        for row in r.bindings:
            rows.append({str(k): v for k, v in row.items() if v is not None})
        return ("solutions", variables, rows)
    rdf, fmt = rdf_format(url)
    g = Graph()
    g.parse(local(url), format=fmt, publicID=url)
    rs = g.value(None, RDF.type, RS.ResultSet)
    if rs is None:
        return ("graph", g)
    boolean = g.value(rs, RS.boolean)
    if boolean is not None:
        return ("boolean", bool(boolean.toPython()))
    variables = {str(v) for v in g.objects(rs, RS.resultVariable)}
    rows = []
    for sol in g.objects(rs, RS.solution):
        row = {}
        for b in g.objects(sol, RS.binding):
            row[str(g.value(b, RS.variable))] = g.value(b, RS.value)
        rows.append(row)
    return ("solutions", variables, rows)


def same_results(expected, actual):
    if expected[0] != actual[0]:
        return False, f"expected {expected[0]}, got {actual[0]}"
    if expected[0] == "boolean":
        return expected[1] == actual[1], f"expected {expected[1]}, got {actual[1]}"
    if expected[0] == "graph":
        e = Graph()
        for s, p, o in expected[1]:
            e.add((s, p, normal(o)))
        a = Graph()
        for s, p, o in actual[1]:
            a.add((s, p, normal(o)))
        ok = len(e) == len(a) and isomorphic(e, a)
        return ok, "" if ok else f"graphs differ: expected {len(e)} triples, got {len(a)}\n{diff(e, a)}"
    _, ev, er = expected
    _, av, ar = actual
    if ev != av and len(er) > 0:
        return False, f"variables: expected {sorted(ev)}, got {sorted(av)}"
    if len(er) != len(ar):
        return False, f"expected {len(er)} solutions, got {len(ar)}\n{rows_diff(er, ar)}"
    ok = isomorphic(solutions_graph(ev, er), solutions_graph(av, ar))
    return ok, "" if ok else "solutions differ\n" + rows_diff(er, ar)


def show(row):
    return "{" + ", ".join(f"{k}={normal(v).n3()}" for k, v in sorted(row.items())) + "}"


def rows_diff(expected, actual):
    e = sorted(show(r) for r in expected)
    a = sorted(show(r) for r in actual)
    missing = [r for r in e if r not in a][:5]
    extra = [r for r in a if r not in e][:5]
    return "".join(f"\n  - {r}" for r in missing) + "".join(f"\n  + {r}" for r in extra)


def diff(e, a):
    missing = sorted(" ".join(x.n3() for x in t) for t in (e - a))[:5]
    extra = sorted(" ".join(x.n3() for x in t) for t in (a - e))[:5]
    return "".join(f"\n  - {t}" for t in missing) + "".join(f"\n  + {t}" for t in extra)


class Store:
    def __init__(self, conn):
        self.conn = conn

    def reset(self):
        self.conn.execute("TRUNCATE kb.quads, kb.terms, kb.graphs")

    def load(self, url, graph=None):
        rdf, _ = rdf_format(url)
        self.conn.execute("SELECT kb.load(%s, %s, %s, %s)",
                          (read(url), rdf, None if graph is None else str(graph), str(url)))

    def create_graph(self, name):
        self.conn.execute("SELECT kb.sparql_update(%s)", (f"CREATE SILENT GRAPH <{name}>",))

    def dataset(self):
        text = self.conn.execute("SELECT kb.dump('nquads')").fetchone()[0]
        ds = rdflib.Dataset()
        if text.strip():
            ds.parse(data=text, format="nquads")
        graphs = {}
        for s, p, o, g in ds.quads((None, None, None, None)):
            name = None if g is None or str(g) == "urn:x-rdflib:default" else str(g)
            graphs.setdefault(name, Graph()).add((s, p, normal(o)))
        return graphs


def manifest_entries(path):
    """A manifest's tests, its includes followed: (test, its graph)."""
    url = WEB + path
    g = Graph()
    g.parse(local(url), format="turtle", publicID=url)
    out = []
    for m in g.subjects(RDF.type, MF.Manifest):
        for lst in g.objects(m, MF.include):
            for inc in Collection(g, lst):
                out.extend(manifest_entries(str(inc)[len(WEB):]))
        for lst in g.objects(m, MF.entries):
            for test in Collection(g, lst):
                out.append((test, g))
    return out


def run_query_test(store, conn, test, g):
    action = g.value(test, MF.action)
    query_url = g.value(action, QT.query)
    store.reset()
    data = g.value(action, QT.data)
    if data is not None:
        store.load(data)
    for gd in g.objects(action, QT.graphData):
        store.load(gd, graph=gd)
    query = read(query_url)
    result_url = g.value(test, MF.result)
    expected = results_from_file(result_url)
    kind = expected[0]
    fmt = "ntriples" if kind == "graph" else "json"
    text = conn.execute("SELECT kb.sparql_text(%s, %s, %s)", (query, fmt, str(query_url))).fetchone()[0]
    if fmt == "json":
        actual = results_from_json(text)
    else:
        ag = Graph()
        if text.strip():
            ag.parse(data=text, format="nt")
        actual = ("graph", ag)
    return same_results(expected, actual)


def graph_data(g, node, pred):
    """ut:data and ut:graphData of an action or result: (file, graph name)"""
    out = []
    for d in g.objects(node, UT.data):
        out.append((d, None))
    for gd in g.objects(node, UT.graphData):
        graph = g.value(gd, UT.graph)
        label = g.value(gd, RDFS.label)
        out.append((graph, URIRef(str(label)) if label is not None else graph))
    return out


def run_update_test(store, conn, test, g):
    action = g.value(test, MF.action)
    request = g.value(action, UT.request)
    store.reset()
    for f, name in graph_data(g, action, UT.data):
        store.load(f, graph=name)
        if name is not None:
            store.create_graph(name)
    conn.execute("SELECT kb.sparql_update(%s, %s)", (read(request), str(request)))
    result = g.value(test, MF.result)
    expected = {}
    for f, name in graph_data(g, result, UT.data):
        rdf, fmt = rdf_format(f)
        eg = Graph()
        eg.parse(local(f), format=fmt, publicID=str(f))
        key = None if name is None else str(name)
        target = expected.setdefault(key, Graph())
        for s, p, o in eg:
            target.add((s, p, normal(o)))
    actual = store.dataset()
    for name in set(expected) | set(actual):
        e = expected.get(name, Graph())
        a = actual.get(name, Graph())
        if len(e) != len(a) or not isomorphic(e, a):
            return False, f"graph {name or 'default'}: expected {len(e)} triples, got {len(a)}\n{diff(e, a)}"
    return True, ""


def run_syntax_test(conn, test, g, update, positive):
    url = g.value(test, MF.action)
    text = read(url)
    try:
        with conn.transaction(force_rollback=True):
            if update:
                conn.execute("SELECT kb.sparql_update(%s, %s)", (text, str(url)))
            else:
                conn.execute("SELECT kb.sparql_text(%s, 'json', %s)", (text, str(url)))
        return (True, "") if positive else (False, "parsed, and it should not")
    except psycopg.Error as e:
        if positive:
            # a positive syntax test may fail to run for other reasons: not a syntax error
            if e.sqlstate == "42601":
                return False, str(e).strip().splitlines()[0]
            return True, f"parsed; ran with {e.sqlstate}"
        return (e.sqlstate == "42601"), f"{e.sqlstate}: {str(e).strip().splitlines()[0]}"


def run(root, work, only):
    global ROOT
    # rdflib logs each ill-typed literal of the tests' data ("xyz"^^xsd:integer)
    logging.getLogger("rdflib").setLevel(logging.CRITICAL)
    ROOT = root
    conn = psycopg.connect(host="/tmp", dbname="postgres", user="postgres", autocommit=True)
    conn.execute("DROP DATABASE IF EXISTS sparql_w3c")
    conn.execute("CREATE DATABASE sparql_w3c")
    conn.close()
    conn = psycopg.connect(host="/tmp", dbname="sparql_w3c", user="postgres", autocommit=True)
    conn.execute("CREATE EXTENSION pg_knowledge_base")
    conn.execute("SET statement_timeout = '30s'")
    store = Store(conn)
    results = []
    seen = set()
    for path in MANIFESTS:
        for test, g in manifest_entries(path):
            if test in seen:
                continue
            seen.add(test)
            name = str(test)
            if only and not any(o in name for o in only):
                continue
            types = set(g.objects(test, RDF.type))
            if name in OXIGRAPH_SKIPS:
                results.append((name, "skip", OXIGRAPH_SKIPS[name]))
                continue
            try:
                if MF.QueryEvaluationTest in types:
                    ok, detail = run_query_test(store, conn, test, g)
                elif MF.UpdateEvaluationTest in types:
                    ok, detail = run_update_test(store, conn, test, g)
                elif types & {MF.PositiveSyntaxTest, MF.PositiveSyntaxTest11}:
                    ok, detail = run_syntax_test(conn, test, g, False, True)
                elif types & {MF.NegativeSyntaxTest, MF.NegativeSyntaxTest11}:
                    ok, detail = run_syntax_test(conn, test, g, False, False)
                elif MF.PositiveUpdateSyntaxTest11 in types:
                    ok, detail = run_syntax_test(conn, test, g, True, True)
                elif MF.NegativeUpdateSyntaxTest11 in types:
                    ok, detail = run_syntax_test(conn, test, g, True, False)
                else:
                    results.append((name, "skip", "type: " + ", ".join(sorted(str(t).split("#")[-1] for t in types))))
                    continue
            except psycopg.Error as e:
                ok, detail = False, f"{e.sqlstate}: {str(e).strip()}"
            except Exception:
                ok, detail = False, traceback.format_exc()
            if name in EXPECTED_DIFFERENCES:
                results.append((name, "pass" if ok else "differs", EXPECTED_DIFFERENCES[name]))
                continue
            results.append((name, "pass" if ok else "FAIL", detail))
            if not ok:
                print(f"FAIL {name}\n     " + "\n     ".join(detail.splitlines()), flush=True)
    passed = sum(1 for _, s, _ in results if s == "pass")
    failed = sum(1 for _, s, _ in results if s == "FAIL")
    skipped = sum(1 for _, s, _ in results if s == "skip")
    differs = sum(1 for _, s, _ in results if s == "differs")
    for name, status, detail in results:
        if status == "differs":
            print(f"differs {name}: {detail}")
    print(f"\n{passed} passed, {failed} failed, {differs} differ as expected, {skipped} skipped, "
          f"of {len(results)} W3C SPARQL tests")
    with open(os.path.join(work, "sparql-results.tsv"), "w", encoding="utf-8") as f:
        for name, status, detail in results:
            f.write(f"{name}\t{status}\t{(detail.splitlines() or [''])[0]}\n")
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(run(sys.argv[1], sys.argv[2], set(sys.argv[3:])))
