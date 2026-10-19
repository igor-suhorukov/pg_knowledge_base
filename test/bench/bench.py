# SPDX-License-Identifier: Apache-2.0
"""The bench leg's driver (docs/design.md §6), run by test/bench.sh with the
server up and the endpoint on 127.0.0.1:7878:

  1. the dataset made (generate.py) as N-Triples;
  2. loaded into the store by kb.load_file(), one transaction, then VACUUM
     (ANALYZE) -- the index-only scans want the visibility map -- with the
     time, the WAL written, and each relation's size;
  3. loaded into oxigraph's own store by its CLI's bulk loader (RocksDB),
     with the time and the size of its directory;
  4. each query through oxigraph's server (serve-read-only on
     127.0.0.1:7979), through the store's endpoint, and through SQL
     (kb.sparql_text()): JSON results, or N-Triples for graphs, the same
     client on a kept-alive connection, a warm-up then KB_BENCH_RUNS timed
     runs; the results' sizes compared, so that a time is never of a wrong
     answer.  And one more row, "connect": ASK {} on a new connection each
     time, which the endpoint serves by a new session (a background worker
     started, connected to the database).

    bench.py WORKDIR PRODUCTS RUNS

Results: WORKDIR/bench/results.json and results.md.
"""
import json
import os
import statistics
import subprocess
import sys
import time

import psycopg
import requests

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import generate  # noqa: E402

NS = generate.NS
V = generate.V
OXIGRAPH = os.environ.get("KB_OXIGRAPH_CLI", "/engine/oxigraph")
PG_URL = "http://127.0.0.1:7878/postgres/query"
OX_URL = "http://127.0.0.1:7979/query"
AUTH = ("postgres", "x")

PREFIXES = f"""PREFIX v: <{V}>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
PREFIX dc: <http://purl.org/dc/elements/1.1/>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
"""


def queries(info):
    p = f"<{NS}product/{info['reviewed_product']}>"
    return [
        ("lookup", "one product's triples",
         f"SELECT ?p ?o WHERE {{ {p} ?p ?o }}"),
        ("star", "products with a feature, a filter, sorted, 10",
         f"""SELECT ?product ?label WHERE {{
  ?product v:productFeature <{NS}feature/17> ; rdfs:label ?label ; v:numeric1 ?n1 .
  FILTER(?n1 > 1000) }} ORDER BY ?label LIMIT 10"""),
        ("reviews", "a product's reviews, their reviewers, a rating",
         f"""SELECT ?title ?name ?r1 WHERE {{
  ?review v:reviewFor {p} ; v:reviewer ?person ; dc:title ?title .
  ?person foaf:name ?name OPTIONAL {{ ?review v:rating1 ?r1 }} }}"""),
        ("offers", "a product's cheapest offers, their vendors' countries",
         f"""SELECT ?offer ?price ?country WHERE {{
  ?offer v:product {p} ; v:vendor ?vendor ; v:price ?price . ?vendor v:country ?country
}} ORDER BY ?price LIMIT 10"""),
        ("recent", "a product's reviews by date, 5",
         f"""SELECT ?title ?date WHERE {{ ?r v:reviewFor {p} ; dc:title ?title ; v:reviewDate ?date }}
ORDER BY DESC(?date) LIMIT 5"""),
        ("ask", "ASK: a product's feature, by its label",
         f"""ASK {{ {p} v:productFeature ?f . ?f rdfs:label ?l FILTER(STRSTARTS(?l, "feature")) }}"""),
        ("describe", "DESCRIBE a product",
         f"DESCRIBE {p}"),
        ("path", "products of a type and its subtypes (subClassOf*)",
         f"""SELECT (COUNT(?product) AS ?n) WHERE {{ ?t rdfs:subClassOf* <{NS}type/1> . ?product a ?t }}"""),
        ("group-offers", "every offer, by its vendor's country",
         """SELECT ?country (COUNT(?offer) AS ?n) (AVG(?price) AS ?avg) WHERE {
  ?offer v:vendor ?vendor ; v:price ?price . ?vendor v:country ?country
} GROUP BY ?country ORDER BY DESC(?n) ?country"""),
        ("group-reviews", "every review, by its reviewer's country",
         """SELECT ?country (COUNT(?review) AS ?n) WHERE {
  ?review v:reviewer ?person . ?person v:country ?country } GROUP BY ?country ORDER BY ?country"""),
        ("range", "offers of a month under 100, by dateTime and decimal",
         """SELECT (COUNT(?offer) AS ?n) WHERE { ?offer v:validFrom ?from ; v:price ?price
  FILTER(?from >= "2007-01-01T00:00:00"^^xsd:dateTime && ?from < "2007-02-01T00:00:00"^^xsd:dateTime
         && ?price < 100) }"""),
        ("text", "labels that contain a word (every label decoded)",
         """SELECT ?s ?label WHERE { ?s rdfs:label ?label FILTER(CONTAINS(?label, "zebra")) }"""),
        ("count", "COUNT(*) of the default graph",
         "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }"),
        ("predicates", "DISTINCT predicates (a full scan)",
         "SELECT DISTINCT ?p WHERE { ?s ?p ?o }"),
    ]


def kind(query):
    head = query.lstrip().split(None, 1)[0].upper()
    return "graph" if head in ("DESCRIBE", "CONSTRUCT") else "ask" if head == "ASK" else "select"


def size_of_answer(k, body):
    """Rows, triples or the boolean: what the three paths must agree on."""
    if k == "graph":
        return sum(1 for line in body.splitlines() if line.strip())
    data = json.loads(body)
    if k == "ask":
        return data["boolean"]
    return len(data["results"]["bindings"])


def http(client, url, query, k):
    accept = "application/n-triples" if k == "graph" else "application/sparql-results+json"
    start = time.perf_counter()
    r = client.post(url, data=PREFIXES + query, auth=AUTH,
                    headers={"Content-Type": "application/sparql-query", "Accept": accept}, timeout=3600)
    body = r.text
    elapsed = time.perf_counter() - start
    if r.status_code != 200:
        raise RuntimeError(f"{url}: {r.status_code} {body[:300]}")
    return elapsed, body


def sql(conn, query, k):
    fmt = "ntriples" if k == "graph" else "json"
    start = time.perf_counter()
    body = conn.execute("SELECT kb.sparql_text(%s, %s)", (PREFIXES + query, fmt)).fetchone()[0]
    return time.perf_counter() - start, body


def sql_connecting(query, k):
    start = time.perf_counter()
    with psycopg.connect(host="/tmp", dbname="postgres", user="postgres", autocommit=True) as conn:
        _, body = sql(conn, query, k)
    return time.perf_counter() - start, body


def store_sizes(conn):
    """The store's quads, terms and bytes, and each of its relations' bytes."""
    rels = conn.execute("""
        SELECT c.relname, pg_relation_size(c.oid) AS bytes
        FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'kb' AND c.relkind IN ('r', 'i') ORDER BY c.relkind DESC, c.relname""").fetchall()
    total = conn.execute("""
        SELECT sum(pg_total_relation_size(c.oid)) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'kb' AND c.relkind = 'r'""").fetchone()[0]
    quads, terms = conn.execute("SELECT (SELECT count(*) FROM kb.quads), (SELECT count(*) FROM kb.terms)").fetchone()
    return {"quads": quads, "terms": terms, "bytes": int(total), "relations": {name: int(b) for name, b in rels}}


def du(path):
    total = sst = 0
    for root, _, files in os.walk(path):
        for f in files:
            n = os.path.getsize(os.path.join(root, f))
            total += n
            if f.endswith(".sst"):
                sst += n
    return total, sst


def wait_for(url, seconds=60):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            requests.get(url, timeout=1)
            return
        except requests.RequestException:
            time.sleep(0.2)
    raise RuntimeError(f"{url} did not answer")


def main():
    work, products, runs = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    bench = os.path.join(work, "bench")
    os.makedirs(bench, exist_ok=True)
    data = os.path.join(bench, "data.nt")
    results = {"products": products, "runs": runs, "cpus": os.cpu_count()}

    t = time.perf_counter()
    info = generate.generate(products, data)
    results["dataset"] = info
    results["dataset"]["bytes"] = os.path.getsize(data)
    print(f"dataset: {info['triples']} triples, {os.path.getsize(data) >> 20} MB of N-Triples "
          f"({time.perf_counter() - t:.1f} s to make)", flush=True)

    # the store
    conn = psycopg.connect(host="/tmp", dbname="postgres", user="postgres", autocommit=True)
    conn.execute("CREATE EXTENSION IF NOT EXISTS pg_knowledge_base")
    lsn0 = conn.execute("SELECT pg_current_wal_lsn()").fetchone()[0]
    t = time.perf_counter()
    loaded = conn.execute("SELECT kb.load_file(%s, 'ntriples')", (data,)).fetchone()[0]
    load_s = time.perf_counter() - t
    wal = conn.execute("SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), %s)", (lsn0,)).fetchone()[0]
    t = time.perf_counter()
    conn.execute("VACUUM (ANALYZE) kb.quads")
    conn.execute("VACUUM (ANALYZE) kb.terms")
    vacuum_s = time.perf_counter() - t
    results["store"] = {"loaded": loaded, "load_s": load_s, "vacuum_s": vacuum_s, "wal_bytes": int(wal),
                        **store_sizes(conn)}
    st = results["store"]
    print(f"store: {loaded} quads loaded in {load_s:.1f} s ({loaded / load_s:,.0f} q/s), VACUUM {vacuum_s:.1f} s, "
          f"{int(wal) >> 20} MB of WAL, {st['bytes'] >> 20} MB ({st['bytes'] / st['quads']:.0f} B/quad), "
          f"{st['terms']} terms", flush=True)

    # the same document by a bulk load, in a database of its own, dropped
    # before the queries so that its pages leave shared buffers
    conn.execute("DROP DATABASE IF EXISTS kb_bulk")
    conn.execute("CREATE DATABASE kb_bulk")
    with psycopg.connect(host="/tmp", dbname="kb_bulk", user="postgres", autocommit=True) as bconn:
        bconn.execute("CREATE EXTENSION pg_knowledge_base")
        lsn0 = bconn.execute("SELECT pg_current_wal_lsn()").fetchone()[0]
        t = time.perf_counter()
        bconn.execute("SELECT kb.load_file(%s, 'ntriples', bulk => true)", (data,))
        bulk_s = time.perf_counter() - t
        bulk_wal = bconn.execute("SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), %s)", (lsn0,)).fetchone()[0]
        t = time.perf_counter()
        bconn.execute("VACUUM (ANALYZE) kb.quads")
        bconn.execute("VACUUM (ANALYZE) kb.terms")
        bulk_vacuum_s = time.perf_counter() - t
        results["store_bulk"] = {"load_s": bulk_s, "vacuum_s": bulk_vacuum_s, "wal_bytes": int(bulk_wal),
                                 **store_sizes(bconn)}
    conn.execute("DROP DATABASE kb_bulk")
    sb = results["store_bulk"]
    print(f"store, bulk: {sb['quads']} quads loaded in {bulk_s:.1f} s ({sb['quads'] / bulk_s:,.0f} q/s), "
          f"VACUUM {bulk_vacuum_s:.1f} s, {sb['wal_bytes'] >> 20} MB of WAL, {sb['bytes'] >> 20} MB "
          f"({sb['bytes'] / sb['quads']:.0f} B/quad){'' if sb['quads'] == st['quads'] else '  QUADS DIFFER'}",
          flush=True)

    # oxigraph's store
    ox_dir = os.path.join(bench, "oxigraph")
    subprocess.run(["rm", "-rf", ox_dir], check=True)
    t = time.perf_counter()
    subprocess.run([OXIGRAPH, "load", "--location", ox_dir, "--file", data], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    ox_load_s = time.perf_counter() - t
    ox_total, ox_sst = du(ox_dir)
    results["oxigraph"] = {"load_s": ox_load_s, "bytes": ox_total, "sst_bytes": ox_sst}
    print(f"oxigraph: loaded in {ox_load_s:.1f} s ({info['triples'] / ox_load_s:,.0f} t/s, bulk loader), "
          f"{ox_total >> 20} MB ({ox_total / info['triples']:.0f} B/triple)", flush=True)

    server = subprocess.Popen([OXIGRAPH, "serve-read-only", "--location", ox_dir, "--bind", "127.0.0.1:7979"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait_for("http://127.0.0.1:7979/")
        wait_for("http://127.0.0.1:7878/postgres/")
        results["queries"] = []
        ox, pg = requests.Session(), requests.Session()
        cases = [(name, what, query, False) for name, what, query in queries(info)]
        cases.append(("connect", "ASK {} on a new connection", "ASK {}", True))
        for name, what, query, fresh in cases:
            k = kind(query)
            row = {"name": name, "what": what, "query": query}
            for path, fn in (("oxigraph", lambda: http(requests if fresh else ox, OX_URL, query, k)),
                             ("pg_http", lambda: http(requests if fresh else pg, PG_URL, query, k)),
                             ("pg_sql", lambda: sql_connecting(query, k) if fresh else sql(conn, query, k))):
                _, body = fn()  # warm-up
                times = [fn()[0] for _ in range(runs)]
                row[path] = {"median_ms": statistics.median(times) * 1000, "min_ms": min(times) * 1000,
                             "answer": size_of_answer(k, body)}
            row["agree"] = row["oxigraph"]["answer"] == row["pg_http"]["answer"] == row["pg_sql"]["answer"]
            results["queries"].append(row)
            print(f"{name:14} oxigraph {row['oxigraph']['median_ms']:9.2f} ms   endpoint {row['pg_http']['median_ms']:9.2f} ms"
                  f"   sql {row['pg_sql']['median_ms']:9.2f} ms   answer {row['oxigraph']['answer']}"
                  f"{'' if row['agree'] else '  DIFFERS: ' + str((row['pg_http']['answer'], row['pg_sql']['answer']))}",
                  flush=True)
    finally:
        server.terminate()
        server.wait()

    with open(os.path.join(bench, "results.json"), "w") as f:
        json.dump(results, f, indent=2)
    with open(os.path.join(bench, "results.md"), "w") as f:
        f.write(report(results))
    print(report(results))
    agree = all(q["agree"] for q in results["queries"])
    return 0 if agree and results["store_bulk"]["quads"] == results["store"]["quads"] else 1


def report(r):
    d, s, o, b = r["dataset"], r["store"], r["oxigraph"], r["store_bulk"]
    mb = lambda b: f"{b / (1 << 20):,.0f} MB"  # noqa: E731
    lines = [
        f"# Bench: {d['triples']:,} triples ({r['products']:,} products), {r['cpus']} CPUs",
        "",
        "| | pg_knowledge_base | oxigraph (RocksDB) |",
        "|---|---:|---:|",
        f"| load | {s['load_s']:.1f} s ({s['loaded'] / s['load_s']:,.0f} quads/s), one transaction"
        f" | {o['load_s']:.1f} s ({d['triples'] / o['load_s']:,.0f} triples/s), bulk loader |",
        f"| after the load | VACUUM (ANALYZE) {s['vacuum_s']:.1f} s | -- |",
        f"| on disk | {mb(s['bytes'])} ({s['bytes'] / s['quads']:.0f} B/quad) + {mb(s['wal_bytes'])} of WAL"
        f" | {mb(o['bytes'])} ({o['bytes'] / d['triples']:.0f} B/triple) |",
        f"| bulk load (`bulk => true`) | {b['load_s']:.1f} s ({b['quads'] / b['load_s']:,.0f} quads/s), then VACUUM"
        f" (ANALYZE) {b['vacuum_s']:.1f} s | -- |",
        f"| on disk after it | {mb(b['bytes'])} ({b['bytes'] / b['quads']:.0f} B/quad) + {mb(b['wal_bytes'])} of WAL"
        f" | -- |",
        "",
        "The store's relations:",
        "",
        "| relation | size | B/quad | B/quad, bulk-loaded |",
        "|---|---:|---:|---:|",
    ]
    for name, size in s["relations"].items():
        if size:
            bulk_size = b["relations"].get(name, 0)
            lines.append(f"| kb.{name} | {mb(size)} | {size / s['quads']:.1f} | {bulk_size / b['quads']:.1f} |")
    lines += [
        "",
        f"Queries: the median of {r['runs']} runs after a warm-up, in ms, on a kept-alive connection but for"
        " \"connect\"; the answer is the rows, the triples or the boolean.",
        "",
        "| query | | oxigraph | endpoint | SQL | answer |",
        "|---|---|---:|---:|---:|---:|",
    ]
    for q in r["queries"]:
        lines.append(f"| {q['name']} | {q['what']} | {q['oxigraph']['median_ms']:.2f} | {q['pg_http']['median_ms']:.2f}"
                     f" | {q['pg_sql']['median_ms']:.2f} | {q['oxigraph']['answer']}"
                     f"{'' if q['agree'] else ' (differs)'} |")
    return "\n".join(lines) + "\n"


if __name__ == "__main__":
    sys.exit(main())
