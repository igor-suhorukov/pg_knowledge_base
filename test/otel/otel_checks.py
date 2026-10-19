# SPDX-License-Identifier: Apache-2.0
"""The otel leg's checks (docs/design.md §2.8): SPARQL through SQL and through
the endpoint, with pg_otel_probe preloaded beside pg_knowledge_base, and the
spans read back from the agent's Arrow IPC files -- each statement's kb.*
attributes, the nested statements under it, the endpoint's requests as
statements of their own, children of their callers' spans.

    otel_checks.py WORKDIR
"""
import glob
import os
import sys
import time
import traceback

import psycopg
import pyarrow as pa
import pyarrow.ipc as ipc
import requests

WORK = sys.argv[1]
IPC = os.path.join(WORK, "ipc")
ENDPOINT = "http://127.0.0.1:7878/postgres"
SUPER = ("postgres", "x")
EX = "http://example.com/"

results = []


def check(name, condition, detail=""):
    results.append((name, bool(condition)))
    print(f"{'ok  ' if condition else 'FAIL'} {name}{'' if condition else ': ' + str(detail)}", flush=True)
    return condition


def connect(app):
    return psycopg.connect(host="/tmp", dbname="postgres", user="postgres", autocommit=True,
                           application_name=app)


def sql(query, *args, app="kb-otel"):
    with connect(app) as conn:
        cur = conn.execute(query, args or None)
        return cur.fetchall() if cur.description else None


def plain(table):
    """Timestamps as int64 nanoseconds, as the probe's own legs read them"""
    cols = [c.cast(pa.int64()) if pa.types.is_timestamp(f.type) else c
            for f, c in zip(table.schema, table.columns)]
    return pa.table(cols, names=table.schema.names)


def spans():
    """Every span of the agent's IPC files, as dicts; ids as hex."""
    rows = []
    for f in sorted(glob.glob(f"{IPC}/traces-*.arrows")):
        with pa.memory_map(f) as src:
            reader = ipc.open_stream(src)
            batches = []
            while True:
                try:
                    batches.append(reader.read_next_batch())
                except StopIteration:
                    break
                except pa.ArrowInvalid:
                    break       # a file still being written
            if batches:
                rows.extend(plain(pa.Table.from_batches(batches, reader.schema)).to_pylist())
    for r in rows:
        for k in ("trace_id", "span_id", "parent_span_id", "statement_span_id"):
            if isinstance(r.get(k), bytes):
                r[k] = r[k].hex()
        r["kb"] = attrs(r)
    return rows


def attrs(row):
    """A span's extension attributes, by key, typed"""
    out = {}
    for a in row.get("extension_attributes") or []:
        out[a["key"]] = {0: a["int_value"], 1: a["double_value"], 2: a["string_value"],
                         3: a["bool_value"]}.get(a["type"])
    return out


def statements(rows, app):
    return [r for r in rows if r.get("application_name") == app and r["span_type"] in (0, 2)
            and r.get("toplevel")]


def eventually(fn, seconds=40):
    deadline = time.time() + seconds
    while True:
        value = fn()
        if value or time.time() > deadline:
            return value
        time.sleep(0.5)


def main():
    sql("CREATE EXTENSION IF NOT EXISTS pg_otel_probe")
    sql("CREATE EXTENSION IF NOT EXISTS pg_knowledge_base")
    agent = eventually(lambda: (sql("SELECT value FROM otel_probe.stats() WHERE name = 'ring.consumers'")
                                or [[0]])[0][0] >= 1, 30)
    check("the probe's agent runs", agent)

    # the workload, a statement an application name
    sql(f"""SELECT kb.load($$
        @prefix ex: <{EX}> .
        ex:alice ex:name "Alice" ; ex:age 30 ; ex:knows ex:bob .
        ex:bob ex:name "Bob" ; ex:age 25 .
        ex:carol ex:name "Carol" ; ex:age 35 .
        $$)""", app="kb-otel-load")
    sql("SELECT kb.load($$<http://example.com/x> <http://example.com/p> \"1\" .\n"
        "<http://example.com/y> <http://example.com/p> \"2\" .$$, 'ntriples', bulk => true)",
        app="kb-otel-bulk")
    select = f"""PREFIX ex: <{EX}>
        SELECT ?name ?age WHERE {{ ?p ex:name ?name ; ex:age ?age FILTER(?age > 26) }} ORDER BY ?age"""
    sql("SELECT * FROM kb.sparql(%s) AS t(name text, age int)", select, app="kb-otel-select")
    sql("CREATE TABLE emp (id int PRIMARY KEY, name text NOT NULL)")
    sql("INSERT INTO emp VALUES (7, 'Dave'), (8, 'Erin')")
    sql(f"""SELECT kb.add_mapping('emp', $$
        @prefix rr: <http://www.w3.org/ns/r2rml#> .
        <#E> rr:logicalTable [ rr:tableName "emp" ] ;
          rr:subjectMap [ rr:template "{EX}emp/{{id}}" ] ;
          rr:predicateObjectMap [ rr:predicate <{EX}name> ; rr:objectMap [ rr:column "name" ] ] .
        $$)""")
    sql("SELECT * FROM kb.sparql(%s) AS t(n text)",
        f"SELECT ?n WHERE {{ <{EX}emp/7> <{EX}name> ?n }}", app="kb-otel-r2rml")
    sql("SELECT kb.sparql_update(%s)",
        f"PREFIX ex: <{EX}> INSERT DATA {{ ex:dan ex:name \"Dan\" ; ex:age 40 }}", app="kb-otel-update")
    try:
        sql("SELECT kb.sparql_json('SELECT ?x WHERE { ?x }')", app="kb-otel-error")
    except psycopg.Error:
        pass
    with connect("kb-otel-raw") as conn:
        conn.execute("SET kb.trace_query_text = raw")
        conn.execute("SELECT * FROM kb.sparql(%s) AS t(name text, age int)", (select,))
    with connect("kb-otel-noplan") as conn:
        conn.execute("SET kb.trace_plan = off")
        conn.execute("SELECT kb.sparql_json(%s)", (select,))
    tid, sid = "4bf92f3577b34da6a3ce929d0e0e4736", "00f067aa0ba902b7"
    r = requests.get(f"{ENDPOINT}/query", params={"query": select}, auth=SUPER,
                     headers={"traceparent": f"00-{tid}-{sid}-01", "Accept": "application/sparql-results+json"})
    check("the endpoint's query answers", r.status_code == 200 and len(r.json()["results"]["bindings"]) == 3,
          (r.status_code, r.text[:200]))
    utid, usid = "0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331"
    r = requests.post(f"{ENDPOINT}/update", data=f"INSERT DATA {{ <{EX}fay> <{EX}name> \"Fay\" }}",
                      auth=SUPER, headers={"traceparent": f"00-{utid}-{usid}-01",
                                           "Content-Type": "application/sparql-update"})
    check("the endpoint's update answers", r.status_code == 204, (r.status_code, r.text[:200]))
    sql("SELECT otel_probe.flush()")

    wanted = ["kb-otel-load", "kb-otel-bulk", "kb-otel-select", "kb-otel-r2rml", "kb-otel-update",
              "kb-otel-error", "kb-otel-raw", "kb-otel-noplan"]

    def complete():
        rows = spans()
        have = all(any("kb.sparql.calls" in s["kb"] for s in statements(rows, app)) for app in wanted)
        endpoint = [r for r in rows if r["trace_id"] in (tid, utid) and "kb.sparql.calls" in r["kb"]]
        return rows if have and len(endpoint) >= 2 else None

    rows = eventually(complete) or spans()

    def one(app):
        found = [s for s in statements(rows, app) if "kb.sparql.calls" in s["kb"]]
        return found[0] if found else None

    s = one("kb-otel-select")
    a = s["kb"] if s else {}
    check("a kb.sparql() statement: its span carries the call", a.get("kb.sparql.calls") == 1, a)
    check("its operation", a.get("kb.sparql.operation") == "SELECT", a)
    check("its text, the literals replaced", "FILTER(?age > ?)" in (a.get("kb.sparql.text") or "")
          and "26" not in (a.get("kb.sparql.text") or ""), a.get("kb.sparql.text"))
    plan = a.get("kb.sparql.plan") or ""
    check("its plan, each node with its results and time",
          "QuadPattern" in plan and "rows=" in plan and "time=" in plan and (a.get("kb.sparql.plan_nodes") or 0) > 1,
          plan)
    check("its rows", a.get("kb.sparql.rows") == 2, a.get("kb.sparql.rows"))
    check("the store's scans and quads", (a.get("kb.store.scans") or 0) >= 1 and (a.get("kb.store.quads_read") or 0) >= 1,
          a)
    check("the engine's time and memory", (a.get("kb.sparql.duration_ms") or 0) > 0
          and (a.get("kb.engine.memory_peak_bytes") or 0) > 0, a)

    s = one("kb-otel-r2rml")
    a = s["kb"] if s else {}
    check("an R2RML query: its SQL counted", (a.get("kb.r2rml.queries") or 0) >= 1 and (a.get("kb.r2rml.rows") or 0) >= 1,
          a)
    nested = [r for r in rows if s and r.get("parent_span_id") == s["span_id"] and r["span_type"] == 0]
    check("its SQL statements, children of the statement's span", nested,
          [(r["name"], r.get("query_text")) for r in rows if s and r.get("statement_span_id") == s["span_id"]][:5])

    s = one("kb-otel-update")
    a = s["kb"] if s else {}
    check("an update", a.get("kb.sparql.operation") == "UPDATE" and (a.get("kb.store.quads_written") or 0) >= 2, a)

    s = one("kb-otel-load")
    a = s["kb"] if s else {}
    check("a load", a.get("kb.sparql.operation") == "LOAD" and a.get("kb.load.quads") == 7, a)
    s = one("kb-otel-bulk")
    a = s["kb"] if s else {}
    check("a bulk load", a.get("kb.load.bulk") is True and a.get("kb.load.quads") == 2, a)
    builds = [r for r in rows if s and r.get("parent_span_id") == s["span_id"]
              and (r.get("command_tag") or "").startswith("CREATE INDEX")]
    check("its index builds, children of its span", len(builds) >= 3,
          [(r["name"], r.get("command_tag")) for r in rows if s and r.get("parent_span_id") == s["span_id"]][:8])

    s = one("kb-otel-error")
    a = s["kb"] if s else {}
    check("a failed statement: the call, on the error path", a.get("kb.sparql.calls") == 1
          and s.get("status_code") == 2 and s.get("error_sqlstate") == "42601",
          (a, s and s.get("status_code"), s and s.get("error_sqlstate")))

    s = one("kb-otel-raw")
    a = s["kb"] if s else {}
    check("kb.trace_query_text = raw: the literals kept", "FILTER(?age > 26)" in (a.get("kb.sparql.text") or ""),
          a.get("kb.sparql.text"))
    s = one("kb-otel-noplan")
    a = s["kb"] if s else {}
    check("kb.trace_plan = off: no plan", "kb.sparql.plan" not in a and a.get("kb.sparql.operation") == "SELECT", a)

    e = [r for r in rows if r["trace_id"] == tid and "kb.sparql.calls" in r["kb"]]
    s = e[0] if e else None
    a = s["kb"] if s else {}
    check("an endpoint query: a span of its trace, a child of the caller's span",
          s is not None and s.get("parent_span_id") == sid and s.get("parent_is_remote") is True,
          s and (s.get("parent_span_id"), s.get("parent_is_remote"), s.get("name")))
    check("the request on it", a.get("kb.http.request.method") == "GET" and a.get("kb.http.route") == "/query"
          and a.get("kb.http.response.status_code") == 200, a)
    check("its query, plan and rows", a.get("kb.sparql.operation") == "SELECT" and "QuadPattern" in (a.get("kb.sparql.plan") or "")
          and a.get("kb.sparql.rows") == 3, a)
    check("the request's statement", s is not None and (s.get("query_text") or "").startswith("SELECT kb.endpoint_request()"),
          s and s.get("query_text"))
    e = [r for r in rows if r["trace_id"] == utid and "kb.sparql.calls" in r["kb"]]
    a = e[0]["kb"] if e else {}
    check("an endpoint update", a.get("kb.sparql.operation") == "UPDATE" and a.get("kb.http.route") == "/update"
          and a.get("kb.http.response.status_code") == 204 and (a.get("kb.store.quads_written") or 0) >= 1, a)
    commits = [r for r in rows if r["trace_id"] == utid and r["span_type"] == 3]
    check("its commit, in its trace", commits, [(r["name"], r["span_type"]) for r in rows if r["trace_id"] == utid])


if __name__ == "__main__":
    try:
        main()
    except Exception:
        check("the checks ran", False, traceback.format_exc())
    failed = [n for n, ok in results if not ok]
    print(f"\n{len(results) - len(failed)} of {len(results)} otel checks pass")
    sys.exit(1 if failed else 0)
