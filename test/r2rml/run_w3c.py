# SPDX-License-Identifier: Apache-2.0
"""The W3C RDB2RDF test cases (https://www.w3.org/2001/sw/rdb2rdf/test-cases/),
as Ontop's checkout carries them (test/rdb2rdf-compliance/src/test/resources),
run against the extension (docs/design.md §7): for each test database a
PostgreSQL database made from its create.sql; for each R2RML test its mapping
added with kb.add_mapping() and the virtual graphs dumped with kb.dump(), in a
transaction rolled back; for each Direct Mapping test the tables' mapping
made by kb.direct_mapping().  The output is compared with the expected
dataset as RDF datasets: graphs isomorphic, typed literals compared by value,
as the store keeps them (engine/src/key.rs).

    run_w3c.py <resources directory> <run directory> [test identifier...]
"""
import datetime
import decimal
import os
import re
import sys
import traceback

import psycopg
import rdflib
from rdflib import XSD, BNode, Dataset, Graph, Literal, URIRef
from rdflib.compare import isomorphic

TEST = rdflib.Namespace("http://purl.org/NET/rdb2rdf-test#")
DCTERMS = rdflib.Namespace("http://purl.org/dc/elements/1.1/")
BASE = "http://example.com/base/"

INTEGERS = {XSD[t] for t in ("integer", "int", "long", "short", "byte", "unsignedLong",
                             "unsignedInt", "unsignedShort", "unsignedByte",
                             "nonNegativeInteger", "nonPositiveInteger",
                             "positiveInteger", "negativeInteger")}


def pg_dialect(sql):
    """create.sql in PostgreSQL's dialect: VARBINARY is bytea, X'..' a bytea literal."""
    sql = re.sub(r"\bVARBINARY\s*\(\s*\d+\s*\)", "BYTEA", sql, flags=re.I)
    sql = re.sub(r"\bX'([0-9A-Fa-f]*)'", lambda m: "'\\x%s'::bytea" % m.group(1), sql)
    return sql


def normal(term):
    """A literal by its value, as the store's keys compare them."""
    if not isinstance(term, Literal) or term.datatype is None:
        return term
    dt, lex = term.datatype, str(term)
    try:
        if dt in INTEGERS:
            return Literal(str(int(lex)), datatype=XSD.integer)
        if dt == XSD.decimal:
            d = decimal.Decimal(lex).normalize()
            return Literal(format(d, "f"), datatype=XSD.decimal)
        if dt in (XSD.double, XSD.float):
            return Literal(repr(float(lex.replace("INF", "inf"))), datatype=dt)
        if dt == XSD.boolean:
            return Literal("true" if lex in ("true", "1") else "false", datatype=dt)
        if dt == XSD.dateTime:
            v = datetime.datetime.fromisoformat(lex.replace("Z", "+00:00"))
            return Literal(v.isoformat(), datatype=dt)
        if dt == XSD.date:
            return Literal(datetime.date.fromisoformat(lex[:10]).isoformat() + lex[10:], datatype=dt)
        if dt == XSD.hexBinary:
            return Literal(lex.upper(), datatype=dt)
    except (ValueError, decimal.InvalidOperation):
        pass
    return term


def graphs_of(text, fmt):
    """A dataset's graphs, by name (None the default graph), literals normalized."""
    ds = Dataset()
    if text.strip():
        ds.parse(data=text, format=fmt, publicID=BASE)
    out = {}
    for s, p, o, g in ds.quads((None, None, None, None)):
        name = None if g is None or g == ds.default_graph.identifier or \
            str(g) == "urn:x-rdflib:default" else str(g)
        out.setdefault(name, Graph()).add((s, p, normal(o)))
    return out


def same_dataset(expected, actual):
    names = set(expected) | set(actual)
    for name in names:
        e = expected.get(name, Graph())
        a = actual.get(name, Graph())
        if len(e) != len(a) or not isomorphic(e, a):
            return False, name, e, a
    return True, None, None, None


def connect(dbname):
    return psycopg.connect(host="/tmp", dbname=dbname, user="postgres", autocommit=True)


def run(resources, work, only):
    results = []
    admin = connect("postgres")
    for d in sorted(os.listdir(resources)):
        path = os.path.join(resources, d)
        manifest = os.path.join(path, "manifest.ttl")
        if not os.path.isfile(manifest):
            continue
        m = Graph()
        m.parse(manifest, format="turtle")
        db = f"r2rml_{d.lower()}"
        admin.execute(f'DROP DATABASE IF EXISTS "{db}"')
        admin.execute(f'CREATE DATABASE "{db}"')
        conn = connect(db)
        conn.execute("CREATE EXTENSION pg_knowledge_base")
        sql = None
        for s in m.subjects(rdflib.RDF.type, TEST.DataBase):
            sql = str(m.value(s, TEST.sqlScriptFile))
        with open(os.path.join(path, sql), encoding="utf-8") as f:
            conn.execute(pg_dialect(f.read()))
        for kind in (TEST.R2RML, TEST.DirectMapping):
            for tc in m.subjects(rdflib.RDF.type, kind):
                ident = str(m.value(tc, DCTERMS.identifier))
                if only and ident not in only:
                    continue
                expected_output = str(m.value(tc, TEST.hasExpectedOutput)).lower() == "true"
                output = m.value(tc, TEST.output)
                status, detail = "pass", ""
                try:
                    with conn.transaction(force_rollback=True):
                        if kind == TEST.R2RML:
                            with open(os.path.join(path, str(m.value(tc, TEST.mappingDocument))),
                                      encoding="utf-8") as f:
                                document = f.read()
                        else:
                            document = conn.execute(
                                "SELECT kb.direct_mapping(array_agg(c.oid::regclass ORDER BY c.relname), %s) "
                                "FROM pg_class c WHERE c.relnamespace = 'public'::regnamespace "
                                "AND c.relkind = 'r'", (BASE,)).fetchone()[0]
                        conn.execute("SELECT kb.add_mapping('test', %s, %s)", (document, BASE))
                        dump = conn.execute("SELECT kb.dump('nquads', NULL, true)").fetchone()[0]
                    if not expected_output:
                        status, detail = "FAIL", "expected an error, the mapping ran"
                    else:
                        with open(os.path.join(path, str(output)), encoding="utf-8") as f:
                            text = f.read()
                        fmt = "nquads" if str(output).endswith(".nq") else "turtle"
                        expected = graphs_of(text, fmt)
                        actual = graphs_of(dump, "nquads")
                        ok, name, e, a = same_dataset(expected, actual)
                        if not ok:
                            status = "FAIL"
                            missing = sorted(" ".join(x.n3() for x in t) for t in (e - a))
                            extra = sorted(" ".join(x.n3() for x in t) for t in (a - e))
                            detail = (f"graph {name or 'default'}: expected {len(e)} triples, got {len(a)}"
                                      + "".join(f"\n  - {t}" for t in missing[:6])
                                      + "".join(f"\n  + {t}" for t in extra[:6]))
                            with open(os.path.join(work, f"{ident}.actual.nq"), "w", encoding="utf-8") as f:
                                f.write(dump)
                except psycopg.Error as e:
                    if expected_output:
                        status, detail = "FAIL", f"{type(e).__name__}: {str(e).strip()}"
                    else:
                        status, detail = "pass", f"refused: {str(e).strip().splitlines()[0]}"
                except Exception:
                    status, detail = "FAIL", traceback.format_exc()
                results.append((ident, status, detail))
                print(f"{status:5} {ident:24} {detail.splitlines()[0] if detail else ''}", flush=True)
                if status == "FAIL" and detail.count("\n"):
                    print("      " + "\n      ".join(detail.splitlines()[1:]), flush=True)
        conn.close()
    admin.close()
    passed = sum(1 for _, s, _ in results if s == "pass")
    print(f"\n{passed} of {len(results)} W3C RDB2RDF test cases pass")
    with open(os.path.join(work, "r2rml-results.tsv"), "w", encoding="utf-8") as f:
        for ident, status, detail in results:
            f.write(f"{ident}\t{status}\t{(detail.splitlines() or [''])[0]}\n")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(run(sys.argv[1], sys.argv[2], set(sys.argv[3:])))
