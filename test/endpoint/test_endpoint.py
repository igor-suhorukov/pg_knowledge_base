# SPDX-License-Identifier: Apache-2.0
"""The SPARQL endpoint's leg (docs/design.md §5, §7): its clients' view of
it, against a server test/endpoint.sh configured -- the endpoint on every
address, TLS with a certificate of the leg's, pg_hba.conf lines by role,
address and TLS.

    test_endpoint.py <run directory>

Each check prints a line; the leg fails when one does.
"""
import base64
import json
import os
import re
import socket
import ssl
import subprocess
import sys
import threading
import time
import traceback
from urllib.parse import quote

import psycopg
import requests
from SPARQLWrapper import JSON, SPARQLWrapper

PORT = 7878
LOCAL = f"http://127.0.0.1:{PORT}"
DB = "/postgres"            # every path is a database's: /<database>/query
OTHER_IP = socket.gethostbyname(socket.gethostname())
OTHER = f"http://{OTHER_IP}:{PORT}"
TLS = f"https://127.0.0.1:{PORT}"
SUPER = ("postgres", "x")
ALICE = ("alice", "alice-secret")

results = []


def check(name, condition, detail=""):
    results.append((name, bool(condition)))
    print(f"{'ok  ' if condition else 'FAIL'} {name}{'' if condition else ': ' + str(detail)}", flush=True)
    return condition


def case(fn):
    """A group of checks: an exception fails it, and the others go on."""
    def run():
        try:
            fn()
        except Exception:
            check(fn.__name__, False, traceback.format_exc())
    run.__name__ = fn.__name__
    return run


def sql(query, *args, db="postgres"):
    with psycopg.connect(host="/tmp", dbname=db, user="postgres", autocommit=True) as conn:
        cur = conn.execute(query, args or None)
        return cur.fetchall() if cur.description else None


def q(query, auth=SUPER, base=LOCAL, accept=None, path=f"{DB}/query", **kw):
    headers = {"Accept": accept} if accept else {}
    return requests.get(f"{base}{path}", params={"query": query}, auth=auth, headers=headers,
                        timeout=60, **kw)


def update(text, auth=SUPER, base=LOCAL, path=f"{DB}/update"):
    return requests.post(f"{base}{path}", data=text.encode(), auth=auth, timeout=60,
                         headers={"Content-Type": "application/sparql-update"})


def raw(request_bytes, host="127.0.0.1", read_all=True, timeout=10):
    """A request written as bytes on a fresh socket; the response's bytes."""
    s = socket.create_connection((host, PORT), timeout=timeout)
    s.sendall(request_bytes)
    chunks = []
    try:
        while True:
            data = s.recv(65536)
            if not data:
                break
            chunks.append(data)
            if not read_all:
                break
    except socket.timeout:
        pass
    s.close()
    return b"".join(chunks)


def basic(user, password):
    return "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()


@case
def setup():
    sql("CREATE EXTENSION IF NOT EXISTS pg_knowledge_base")
    sql("""SELECT kb.load($$
        @prefix ex: <http://example.com/> .
        ex:alice ex:name "Alice" ; ex:age 30 ; ex:knows ex:bob .
        ex:bob ex:name "Bob" ; ex:age 25 .
        $$, 'turtle')""")
    sql("SELECT kb.load('<http://example.com/carol> <http://example.com/name> \"Carol\" .', "
        "'ntriples', 'http://example.com/g1')")
    for role, password in [("alice", "alice-secret"), ("bob", "bob-secret"),
                           ("carol", "carol-secret"), ("mallory", "m")]:
        sql(f"CREATE ROLE {role} LOGIN PASSWORD '{password}'")
    sql("CREATE ROLE anon LOGIN")
    sql("CREATE ROLE dave PASSWORD 'dave-secret'")      # no LOGIN
    for role in ("alice", "bob", "carol", "anon"):
        sql(f"GRANT USAGE ON SCHEMA kb TO {role}")
        sql(f"GRANT SELECT ON kb.quads, kb.terms, kb.graphs, kb.mappings TO {role}")
    sql("GRANT INSERT, DELETE ON kb.quads, kb.terms, kb.graphs TO alice")
    sql("CREATE DATABASE other")
    sql("REVOKE CONNECT ON DATABASE other FROM PUBLIC")
    check("setup", True)


# ---------------------------------------------------------------------
# The protocol, as oxigraph's server speaks it
# ---------------------------------------------------------------------

@case
def queries():
    r = q("SELECT ?n WHERE { ?s <http://example.com/name> ?n } ORDER BY ?n")
    check("GET /query: 200", r.status_code == 200, r.status_code)
    check("GET /query: SPARQL JSON by default",
          r.headers.get("Content-Type", "").startswith("application/sparql-results+json"), r.headers)
    names = [b["n"]["value"] for b in r.json()["results"]["bindings"]]
    check("GET /query: the default graph's names", names == ["Alice", "Bob"], names)
    check("a whole response has its Content-Length", "Content-Length" in r.headers, r.headers)

    r = requests.post(f"{LOCAL}{DB}/query", auth=SUPER, data="ASK { ?s ?p ?o }",
                      headers={"Content-Type": "application/sparql-query",
                               "Accept": "application/sparql-results+xml"})
    check("POST sparql-query, XML", r.status_code == 200 and "<boolean>true</boolean>" in r.text, r.text)

    r = requests.post(f"{LOCAL}{DB}/query", auth=SUPER,
                      data={"query": "SELECT ?n WHERE { ?s <http://example.com/name> ?n } ORDER BY ?n"},
                      headers={"Accept": "text/csv"})
    check("POST form, CSV", r.status_code == 200 and r.text.replace("\r", "") == "n\nAlice\nBob\n", repr(r.text))

    r = q("SELECT ?n WHERE { ?s <http://example.com/name> ?n } ORDER BY ?n", accept="text/tab-separated-values")
    check("TSV", r.status_code == 200 and r.text.splitlines()[1] == '"Alice"', repr(r.text))

    r = q("SELECT * WHERE { ?s ?p ?o }", accept="image/png")
    check("an Accept no format answers: 406", r.status_code == 406, r.status_code)

    r = q("CONSTRUCT { ?s <http://example.com/n> ?n } WHERE { ?s <http://example.com/name> ?n }",
          accept="text/turtle")
    check("CONSTRUCT as Turtle", r.status_code == 200 and r.headers["Content-Type"].startswith("text/turtle")
          and "Alice" in r.text, r.text)
    r = q("DESCRIBE <http://example.com/alice>", accept="application/ld+json")
    check("DESCRIBE as JSON-LD", r.status_code == 200 and json.loads(r.text)[0]["@id"] == "http://example.com/alice",
          r.text)

    r = requests.get(f"{LOCAL}{DB}/query", auth=SUPER,
                     params=[("query", "SELECT ?n WHERE { ?s <http://example.com/name> ?n }"),
                             ("default-graph-uri", "http://example.com/g1")])
    names = [b["n"]["value"] for b in r.json()["results"]["bindings"]]
    check("default-graph-uri", names == ["Carol"], names)
    r = requests.get(f"{LOCAL}{DB}/query", auth=SUPER,
                     params=[("query", "SELECT ?n WHERE { ?s <http://example.com/name> ?n }"),
                             ("union-default-graph", "")])
    check("union-default-graph", len(r.json()["results"]["bindings"]) == 3, r.text)
    r = requests.get(f"{LOCAL}{DB}/query", auth=SUPER,
                     params=[("query", "SELECT ?g WHERE { GRAPH ?g { } }"),
                             ("named-graph-uri", "http://example.com/g1")])
    check("named-graph-uri", r.json()["results"]["bindings"][0]["g"]["value"] == "http://example.com/g1", r.text)

    r = requests.post(f"{LOCAL}{DB}/query?query=ASK{{}}", auth=SUPER, data="ASK {}",
                      headers={"Content-Type": "application/sparql-query"})
    check("a query in the URL and the body: 400", r.status_code == 400, r.status_code)
    r = requests.post(f"{LOCAL}{DB}/query", auth=SUPER, data="ASK {}", headers={"Content-Type": "text/plain"})
    check("an unsupported Content-Type: 415", r.status_code == 415, r.status_code)
    r = q("SELECT * WHERE {")
    check("a syntax error: 400, SQLSTATE 42601", r.status_code == 400 and r.headers.get("X-SQLSTATE") == "42601",
          (r.status_code, r.headers))
    r = requests.get(f"{LOCAL}/nothing/here", auth=SUPER)
    check("an unknown path: 404", r.status_code == 404, r.status_code)

    r = requests.get(f"{LOCAL}{DB}/query", auth=SUPER, headers={"Accept": "text/turtle"})
    check("the service description", r.status_code == 200 and "sparql-service-description" in r.text, r.text)
    r = requests.get(f"{LOCAL}{DB}/", auth=SUPER)
    check("the page", r.status_code == 200 and r.headers["Content-Type"].startswith("text/html"), r.status_code)


@case
def updates():
    r = update('INSERT DATA { <http://example.com/dan> <http://example.com/name> "Dan" }')
    check("POST /update: 204", r.status_code == 204, (r.status_code, r.text))
    r = q('ASK { <http://example.com/dan> ?p "Dan" }')
    check("the update is committed", r.json()["boolean"] is True, r.text)
    r = requests.post(f"{LOCAL}{DB}/update", auth=SUPER,
                      data={"update": 'DELETE DATA { <http://example.com/dan> <http://example.com/name> "Dan" }'})
    check("POST form update", r.status_code == 204, r.status_code)
    r = requests.post(f"{LOCAL}{DB}/sparql", auth=SUPER, data="ASK { <http://example.com/dan> ?p ?o }",
                      headers={"Content-Type": "application/sparql-query"})
    check("/sparql serves queries, and the delete is committed", r.json()["boolean"] is False, r.text)
    # an update that fails is rolled back whole
    r = update('INSERT DATA { <http://example.com/eve> <http://example.com/name> "Eve" } ; '
               'DROP GRAPH <http://example.com/no-such-graph>')
    check("a failing update: an error", r.status_code >= 400, r.status_code)
    r = q('ASK { <http://example.com/eve> ?p ?o }')
    check("a failing update writes nothing", r.json()["boolean"] is False, r.text)
    r = requests.post(f"{LOCAL}{DB}/query", auth=SUPER, data={"update": "CLEAR ALL"})
    check("an update on /query: refused", r.status_code == 400, r.status_code)


@case
def graph_store():
    r = requests.get(f"{LOCAL}{DB}/store?default", auth=SUPER, headers={"Accept": "application/n-triples"})
    check("GET the default graph", r.status_code == 200 and "Alice" in r.text and "Carol" not in r.text, r.text)
    r = requests.put(f"{LOCAL}{DB}/store?graph=http://example.com/g2", auth=SUPER,
                     data='<http://example.com/x> <http://example.com/p> "1" .',
                     headers={"Content-Type": "application/n-triples"})
    check("PUT a new graph: 201", r.status_code == 201, r.status_code)
    r = requests.put(f"{LOCAL}{DB}/store?graph=http://example.com/g2", auth=SUPER,
                     data='<http://example.com/x> <http://example.com/p> "2" .',
                     headers={"Content-Type": "application/n-triples"})
    check("PUT over a graph: 204", r.status_code == 204, r.status_code)
    r = requests.get(f"{LOCAL}{DB}/store?graph=http://example.com/g2", auth=SUPER,
                     headers={"Accept": "application/n-triples"})
    check("PUT replaced the graph", r.text.strip() == '<http://example.com/x> <http://example.com/p> "2" .', r.text)
    r = requests.post(f"{LOCAL}{DB}/store?graph=http://example.com/g2", auth=SUPER,
                      data='<http://example.com/y> <http://example.com/p> "3" .',
                      headers={"Content-Type": "application/n-triples"})
    check("POST to a graph: 204", r.status_code == 204, r.status_code)
    r = requests.head(f"{LOCAL}{DB}/store?graph=http://example.com/g2", auth=SUPER)
    check("HEAD a graph: 200", r.status_code == 200, r.status_code)
    r = requests.post(f"{LOCAL}{DB}/store", auth=SUPER, data='<http://example.com/z> <http://example.com/p> "4" .',
                      headers={"Content-Type": "application/n-triples"})
    check("POST triples to the store: a new graph, 201 and its Location",
          r.status_code == 201 and r.headers.get("Location", "").startswith(f"{LOCAL}{DB}/store/"), r.headers)
    r = requests.delete(f"{LOCAL}{DB}/store?graph=http://example.com/g2", auth=SUPER)
    check("DELETE a graph: 204", r.status_code == 204, r.status_code)
    r = requests.delete(f"{LOCAL}{DB}/store?graph=http://example.com/g2", auth=SUPER)
    check("DELETE a missing graph: 404", r.status_code == 404, r.status_code)
    r = requests.get(f"{LOCAL}{DB}/store", auth=SUPER, headers={"Accept": "application/n-quads"})
    check("GET the whole store as N-Quads", r.status_code == 200 and "<http://example.com/g1>" in r.text, r.text)
    r = requests.get(f"{LOCAL}{DB}/store", auth=SUPER, headers={"Accept": "text/turtle"})
    check("the whole store as Turtle: 400", r.status_code == 400, r.status_code)


@case
def databases():
    """Every path of the endpoint's is below a database's name: /<database>/query,
    /<database>/update, /<database>/sparql, /<database>/store, /<database>/ --
    the name percent-encoded, as "kb two" is; none without one."""
    sql('CREATE DATABASE "kb two"')
    sql("CREATE EXTENSION pg_knowledge_base", db="kb two")
    sql("SELECT kb.load('<http://example.com/zed> <http://example.com/name> \"Zed\" .', 'ntriples')",
        db="kb two")
    db = f"{LOCAL}/kb%20two"
    query = "SELECT ?n WHERE { ?s <http://example.com/name> ?n }"

    def names(r):
        return sorted(b["n"]["value"] for b in r.json()["results"]["bindings"])

    def count(where, database="postgres"):
        return sql(f"SELECT count(*) FROM kb.triples WHERE {where}", db=database)[0][0]

    r = requests.get(f"{db}/query", params={"query": query}, auth=SUPER)
    check("/<database>/query: the database's store", r.status_code == 200 and names(r) == ["Zed"],
          (r.status_code, r.text[:200]))
    r = requests.get(f"{LOCAL}{DB}/query", params={"query": query}, auth=SUPER)
    check("/postgres/query: another database's store", r.status_code == 200 and "Zed" not in names(r),
          (r.status_code, r.text[:200]))
    r = update('INSERT DATA { <http://example.com/yan> <http://example.com/name> "Yan" }', path="/kb%20two/update")
    check("/<database>/update: the database's store written, not another's",
          r.status_code == 204 and count("object = '\"Yan\"'", "kb two") == 1 and count("object = '\"Yan\"'") == 0,
          (r.status_code, r.text[:200]))
    r = requests.post(f"{db}/sparql", data={"query": query}, auth=SUPER)
    check("/<database>/sparql: a query", r.status_code == 200 and names(r) == ["Yan", "Zed"],
          (r.status_code, r.text[:200]))
    r = requests.post(f"{db}/sparql", auth=SUPER,
                      data={"update": 'DELETE DATA { <http://example.com/yan> <http://example.com/name> "Yan" }'})
    check("/<database>/sparql: an update", r.status_code == 204 and count("object = '\"Yan\"'", "kb two") == 0,
          r.status_code)
    turtle = '<http://example.com/s> <http://example.com/p> "in g9" .'
    r = requests.put(f"{db}/store", params={"graph": "http://example.com/g9"}, data=turtle.encode(), auth=SUPER,
                     headers={"Content-Type": "text/turtle"})
    r2 = requests.get(f"{db}/store", params={"graph": "http://example.com/g9"}, auth=SUPER,
                      headers={"Accept": "application/n-triples"})
    check("/<database>/store?graph=: PUT and GET a graph of the database's",
          r.status_code == 201 and r2.status_code == 200 and "in g9" in r2.text
          and count("graph = '<http://example.com/g9>'", "kb two") == 1
          and count("graph = '<http://example.com/g9>'") == 0, (r.status_code, r2.status_code, r2.text[:200]))
    r = requests.post(f"{db}/store", data=turtle.encode(), auth=SUPER, headers={"Content-Type": "text/turtle"})
    location = r.headers.get("Location", "")
    check("POST /<database>/store: the new graph's Location keeps the database's path",
          r.status_code == 201 and location.startswith(f"{db}/store/"), (r.status_code, location))
    r = requests.get(location, auth=SUPER, headers={"Accept": "application/n-triples"})
    check("the new graph read at its Location", r.status_code == 200 and "in g9" in r.text,
          (r.status_code, r.text[:200]))
    r = requests.delete(f"{db}/store", params={"graph": "http://example.com/g9"}, auth=SUPER)
    check("/<database>/store: DELETE", r.status_code == 204 and count("graph = '<http://example.com/g9>'", "kb two") == 0,
          r.status_code)
    r = requests.get(f"{db}/query", auth=SUPER, headers={"Accept": "application/n-triples"})
    check("/<database>/query: the service description names the database's endpoint",
          r.status_code == 200 and f"<{db}/query>" in r.text, r.text[:300])
    r = requests.get(f"{db}/", auth=SUPER)
    check("/<database>/: the page", r.status_code == 200 and r.headers["Content-Type"].startswith("text/html"),
          r.status_code)
    r = requests.get(db, auth=SUPER, allow_redirects=False)
    check("/<database>: sent to /<database>/, the page's relative links the database's",
          r.status_code == 308 and r.headers.get("Location") == "/kb%20two/", (r.status_code, r.headers))
    r = requests.get(f"{db}/nothing", auth=None)
    check("a path below a database's that the endpoint does not serve: 404, before any login",
          r.status_code == 404, r.status_code)
    r = requests.get(f"{LOCAL}/{'x' * 70}/query", params={"query": "ASK {}"}, auth=SUPER)
    check("a first segment longer than a database's name: 400", r.status_code == 400, r.status_code)
    # no path without a database: oxigraph's /query names the database "query"
    r = requests.get(f"{LOCAL}/", auth=None)
    check("/: 404, before any login, naming the paths", r.status_code == 404 and "/<database>/query" in r.text,
          (r.status_code, r.text))
    r = requests.post(f"{LOCAL}/query", data="ASK {}", auth=SUPER, headers={"Content-Type": "application/sparql-query"})
    check("/query: the database \"query\", which is not",
          r.status_code == 404 and 'database "query" does not exist' in r.text, (r.status_code, r.text))
    sql("CREATE DATABASE store")
    sql("CREATE EXTENSION pg_knowledge_base", db="store")
    r = requests.get(f"{LOCAL}/store/query", params={"query": "ASK {}"}, auth=SUPER)
    check("a database named as a path, store: /store/query", r.status_code == 200 and r.json()["boolean"] is True,
          (r.status_code, r.text[:200]))
    s = requests.Session()
    r1 = s.get(f"{db}/query", params={"query": "ASK {}"}, auth=SUPER)
    r2 = s.get(f"{LOCAL}{DB}/query", params={"query": "ASK {}"}, auth=SUPER)
    check("another database on the connection: 421", r1.status_code == 200 and r2.status_code == 421,
          (r1.status_code, r2.status_code))
    s.close()


@case
def framing():
    # a result past 64 kB goes out chunked, and whole
    sql("SELECT kb.sparql_update('INSERT { ?s <http://example.com/i> ?i } WHERE { "
        "VALUES ?i { " + " ".join(str(i) for i in range(5000)) + " } "
        "BIND(IRI(CONCAT(\"http://example.com/item/\", STR(?i))) AS ?s) }')")
    r = q("SELECT ?s ?i WHERE { ?s <http://example.com/i> ?i }")
    check("a large result: chunked", r.headers.get("Transfer-Encoding") == "chunked", r.headers)
    check("a large result: whole", len(r.json()["results"]["bindings"]) == 5000,
          len(r.json()["results"]["bindings"]))
    # Expect: 100-continue, and a chunked request body
    body = "ASK { " + " " * 100000 + " ?s ?p ?o }"
    out = subprocess.run(["curl", "-s", "-u", "postgres:x", "-H", "Expect: 100-continue",
                          "-H", "Content-Type: application/sparql-query", "--data-binary", "@-",
                          f"{LOCAL}{DB}/query"], input=body.encode(), capture_output=True)
    check("Expect: 100-continue", b'"boolean":true' in out.stdout, out)
    out = subprocess.run(["curl", "-s", "-u", "postgres:x", "-H", "Transfer-Encoding: chunked",
                          "-H", "Content-Type: application/sparql-query", "--data-binary", "@-",
                          f"{LOCAL}{DB}/query"], input=b"ASK { ?s ?p ?o }", capture_output=True)
    check("a chunked request body", b'"boolean":true' in out.stdout, out)
    # keep-alive: requests on one connection, HTTP/1.0 closes
    s = requests.Session()
    for i in range(5):
        r = s.get(f"{LOCAL}{DB}/query", params={"query": "ASK {}"}, auth=SUPER)
    check("keep-alive: five requests on a session", r.status_code == 200, r.status_code)
    out = raw(b"GET /postgres/query?query=ASK%7B%7D HTTP/1.0\r\nAuthorization: " + basic("postgres", "x").encode()
              + b"\r\n\r\n")
    check("HTTP/1.0: the response, and the connection closed",
          out.startswith(b"HTTP/1.1 200") and b"Connection: close" in out, out[:200])
    out = raw(b"GARBAGE\r\n\r\n")
    check("a malformed request: 400", out.startswith(b"HTTP/1.1 400"), out[:100])
    two = (b"GET /postgres/query?query=ASK%7B%7D HTTP/1.1\r\nHost: x\r\nAuthorization: " + basic("postgres", "x").encode()
           + b"\r\n\r\n") * 2
    out = raw(two, read_all=True, timeout=3)
    check("pipelined requests: two responses", out.count(b"HTTP/1.1 200") == 2, out[:300])


# ---------------------------------------------------------------------
# Logins, as PostgreSQL's
# ---------------------------------------------------------------------

@case
def logins():
    r = q("ASK {}", auth=None)
    check("no credentials, no anonymous role: 401 with a challenge",
          r.status_code == 401 and r.headers.get("WWW-Authenticate", "").startswith("Basic"), r.headers)
    r = q("ASK {}", auth=("alice", "wrong"))
    check("a wrong password: 401", r.status_code == 401, r.status_code)
    r = q("ASK { ?s ?p ?o }", auth=ALICE)
    check("scram-sha-256: in", r.status_code == 200 and r.json()["boolean"] is True, (r.status_code, r.text))
    r = q("ASK {}", auth=("dave", "dave-secret"))
    check("a role without LOGIN: refused", r.status_code in (401, 403), r.status_code)
    r = q("ASK {}", auth=("mallory", "m"))
    check("a reject line: 403", r.status_code == 403, r.status_code)
    r = q("ASK {}", auth=("nobody", "x"))
    check("a role that is not: 401", r.status_code == 401, r.status_code)
    r = q("ASK {}", auth=SUPER, path="/nosuchdb/query")
    check("a database that is not: 404", r.status_code == 404, r.status_code)
    r = q("ASK {}", auth=ALICE, path="/other/query")
    check("no CONNECT on the database: 403", r.status_code == 403, r.status_code)
    r = q("ASK {}", auth=("carol", "carol-secret"))
    check("a hostssl line, over plain HTTP: refused", r.status_code in (401, 403), r.status_code)
    r = requests.get(f"{TLS}{DB}/query", params={"query": "ASK { ?s ?p ?o }"}, auth=("carol", "carol-secret"),
                     verify=False)
    check("a hostssl line, over TLS: in", r.status_code == 200 and r.json()["boolean"] is True,
          (r.status_code, r.text))
    # the client's address: bob may log in from 127.0.0.1 only
    r = q("ASK {}", auth=("bob", "bob-secret"))
    check("an address line: in from 127.0.0.1", r.status_code == 200, r.status_code)
    r = q("ASK {}", auth=("bob", "bob-secret"), base=OTHER)
    check(f"an address line: refused from {OTHER_IP}", r.status_code in (401, 403), r.status_code)
    # one connection, one role and one database
    s = requests.Session()
    r1 = s.get(f"{LOCAL}{DB}/query", params={"query": "ASK {}"}, auth=SUPER)
    r2 = s.get(f"{LOCAL}{DB}/query", params={"query": "ASK {}"}, auth=ALICE)
    check("another role on the connection: 421", r1.status_code == 200 and r2.status_code == 421,
          (r1.status_code, r2.status_code))
    s = requests.Session()
    r1 = s.get(f"{LOCAL}{DB}/query", params={"query": "ASK {}"}, auth=SUPER)
    r2 = s.get(f"{LOCAL}{DB}/query", params={"query": "ASK {}"}, auth=("postgres", "other-password"))
    check("other credentials of the role on the connection: 421", r2.status_code == 421, r2.status_code)


@case
def privileges():
    r = update('INSERT DATA { <http://example.com/f> <http://example.com/p> "f" }', auth=("bob", "bob-secret"))
    check("an update without INSERT on the store: 403, 42501",
          r.status_code == 403 and r.headers.get("X-SQLSTATE") == "42501", (r.status_code, r.headers, r.text))
    r = update('INSERT DATA { <http://example.com/f> <http://example.com/p> "f" }', auth=ALICE)
    check("an update with INSERT: 204", r.status_code == 204, (r.status_code, r.text))
    sql("REVOKE SELECT ON kb.quads FROM bob")
    r = q("ASK { ?s ?p ?o }", auth=("bob", "bob-secret"))
    check("a query without SELECT on the store: 403", r.status_code == 403, (r.status_code, r.text))
    sql("GRANT SELECT ON kb.quads TO bob")


@case
def cancel_and_timeout():
    result = {}

    def slow():
        result["r"] = q("SELECT (COUNT(*) AS ?n) WHERE { ?a <http://example.com/i> ?x . ?b <http://example.com/i> ?y }",
                        auth=ALICE)

    t = threading.Thread(target=slow)
    t.start()
    pid = None
    for _ in range(100):
        rows = sql("SELECT pid FROM pg_stat_activity WHERE backend_type = 'pg_knowledge_base session' "
                   "AND state = 'active' AND usename = 'alice'")
        if rows:
            pid = rows[0][0]
            break
        time.sleep(0.05)
    if pid:
        row = sql("SELECT usename, host(client_addr), query, application_name FROM pg_stat_activity "
                  "WHERE pid = %s", pid)[0]
        check("a running request in pg_stat_activity: its role, address, SPARQL",
              row[0] == "alice" and row[1] == "127.0.0.1" and "COUNT(*)" in row[2]
              and row[3] == "pg_knowledge_base endpoint", row)
        sql("SELECT pg_cancel_backend(%s)", pid)
    else:
        check("a running request in pg_stat_activity", False, "not seen")
    t.join()
    r = result.get("r")
    check("pg_cancel_backend(): 503, 57014", r is not None and r.status_code == 503
          and r.headers.get("X-SQLSTATE") == "57014", r is not None and (r.status_code, r.headers, r.text))
    sql("ALTER ROLE alice SET statement_timeout = '200ms'")
    r = q("SELECT (COUNT(*) AS ?n) WHERE { ?a <http://example.com/i> ?x . ?b <http://example.com/i> ?y }",
          auth=ALICE)
    check("the role's statement_timeout: 503, 57014", r.status_code == 503 and r.headers.get("X-SQLSTATE") == "57014",
          (r.status_code, r.text))
    sql("ALTER ROLE alice RESET statement_timeout")


@case
def tls():
    r = requests.get(f"{TLS}{DB}/query", params={"query": "ASK {}"}, auth=SUPER, verify=False)
    check("TLS", r.status_code == 200, r.status_code)
    s = requests.Session()
    s.verify = False
    for _ in range(3):
        r = s.get(f"{TLS}{DB}/query", params={"query": "ASK {}"}, auth=SUPER)
    check("TLS, keep-alive", r.status_code == 200, r.status_code)
    rows = []
    done = threading.Event()

    def watch():
        while not done.is_set():
            got = sql("SELECT s.ssl, s.version FROM pg_stat_ssl s JOIN pg_stat_activity a USING (pid) "
                      "WHERE a.backend_type = 'pg_knowledge_base session' AND s.ssl")
            if got:
                rows.extend(got)
                return
            time.sleep(0.02)

    w = threading.Thread(target=watch)
    w.start()
    s2 = requests.Session()
    s2.verify = False
    s2.get(f"{TLS}{DB}/query", params={"query": "ASK {}"}, auth=SUPER)
    time.sleep(0.5)
    done.set()
    w.join()
    check("a TLS session in pg_stat_ssl", bool(rows) and rows[0][0] is True, rows)


@case
def clients():
    sparql = SPARQLWrapper(f"{LOCAL}{DB}/query")
    sparql.setCredentials(*SUPER)
    sparql.setQuery("SELECT ?n WHERE { ?s <http://example.com/name> ?n } ORDER BY ?n")
    sparql.setReturnFormat(JSON)
    names = [b["n"]["value"] for b in sparql.query().convert()["results"]["bindings"]]
    check("SPARQLWrapper", names[:2] == ["Alice", "Bob"], names)


# ---------------------------------------------------------------------
# Switched off and on by a reload of kb.endpoint_listen_addresses
# ---------------------------------------------------------------------

COUNT = "SELECT (COUNT(*) AS ?n) WHERE { ?a <http://example.com/i> ?x . ?b <http://example.com/i> ?y }"


def get_request(query, auth=SUPER):
    return (f"GET /postgres/query?query={quote(query)} HTTP/1.1\r\nHost: x\r\n"
            f"Accept: application/sparql-results+json\r\nAuthorization: {basic(*auth)}\r\n\r\n").encode()


def set_listen(value):
    """kb.endpoint_listen_addresses set, or reset to postgresql.conf's (None), and reloaded."""
    if value is None:
        sql("ALTER SYSTEM RESET kb.endpoint_listen_addresses")
    else:
        sql(f"ALTER SYSTEM SET kb.endpoint_listen_addresses = '{value}'")
    sql("SELECT pg_reload_conf()")


def refused(timeout=10):
    """Whether a new connection is refused, within timeout seconds."""
    end = time.time() + timeout
    while time.time() < end:
        try:
            socket.create_connection(("127.0.0.1", PORT), timeout=2).close()
        except ConnectionRefusedError:
            return True
        time.sleep(0.1)
    return False


def answering(timeout=10):
    """Whether a query is answered 200, within timeout seconds."""
    end = time.time() + timeout
    while time.time() < end:
        try:
            if q("ASK {}").status_code == 200:
                return True
        except requests.ConnectionError:
            pass
        time.sleep(0.1)
    return False


def acceptors():
    return sql("SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'pg_knowledge_base acceptor'")[0][0]


@case
def switched_by_reload():
    idle = socket.create_connection(("127.0.0.1", PORT), timeout=10)
    idle.sendall(get_request("ASK {}"))
    check("before the switch: a keep-alive connection served", idle.recv(65536).startswith(b"HTTP/1.1 200"))
    slow = {}

    def run():
        started = time.time()
        slow["response"] = raw(get_request(COUNT), timeout=60)
        slow["seconds"] = time.time() - started

    t = threading.Thread(target=run)
    t.start()
    being_served = False
    for _ in range(250):
        if sql("SELECT 1 FROM pg_stat_activity WHERE backend_type = 'pg_knowledge_base session' "
               "AND state = 'active' AND query LIKE '%COUNT(*)%'"):
            being_served = True
            break
        time.sleep(0.02)
    set_listen("")
    check("a reload emptying kb.endpoint_listen_addresses: new connections refused", refused())
    idle.settimeout(5)
    try:
        closed = idle.recv(1) == b""
    except (socket.timeout, ConnectionResetError):
        closed = False
    idle.close()
    check("switched off: an idle keep-alive connection closed", closed)
    t.join()
    response = slow.get("response", b"")
    check("switched off: the request being served ends -- 200, its result whole -- and its connection closes",
          being_served and response.startswith(b"HTTP/1.1 200") and b"25000000" in response
          and slow.get("seconds", 60) < 50, (being_served, response[:160], slow.get("seconds")))
    check("switched off: the acceptor runs on", acceptors() == 1, acceptors())
    set_listen(None)
    check("a reload naming addresses again: switched on", answering())
    try:
        sql("ALTER SYSTEM SET kb.endpoint_listen_addresses = '\"unterminated'")
        check("a list of bad syntax refused", False, "accepted")
    except psycopg.Error as e:
        check("a list of bad syntax refused", e.sqlstate == "22023", (e.sqlstate, e))
    set_listen("192.0.2.1")         # TEST-NET-1: no interface of the container's has it
    check("an address that cannot be listened on: off, the acceptor running on",
          refused() and acceptors() == 1, acceptors())
    set_listen(None)
    check("then a reload naming one that can: switched on", answering())


# ---------------------------------------------------------------------
# The limits phase: the server restarted with kb.endpoint_max_sessions = 2,
# kb.endpoint_anonymous_role = 'anon', kb.endpoint_read_only, kb.endpoint_cors
# ---------------------------------------------------------------------

@case
def anonymous_read_only_cors():
    r = q("ASK { ?s ?p ?o }", auth=None)
    check("the anonymous role, trusted: in", r.status_code == 200 and r.json()["boolean"] is True,
          (r.status_code, r.text))
    r = update('INSERT DATA { <http://example.com/g> <http://example.com/p> "g" }')
    check("kb.endpoint_read_only: an update refused, 403", r.status_code == 403, r.status_code)
    check("kb.endpoint_cors: Access-Control-Allow-Origin", r.headers.get("Access-Control-Allow-Origin") == "*",
          r.headers)
    r = requests.options(f"{LOCAL}{DB}/query", headers={"Origin": "http://elsewhere",
                                                    "Access-Control-Request-Method": "POST"})
    check("a CORS preflight: 204", r.status_code == 204 and "POST" in r.headers.get(
        "Access-Control-Allow-Methods", ""), (r.status_code, r.headers))
    r = q("ASK {}", auth=("alice", "wrong"))
    check("credentials, wrong, with an anonymous role: still 401", r.status_code == 401, r.status_code)


@case
def session_limit():
    request = (b"GET /postgres/query?query=ASK%7B%7D HTTP/1.1\r\nHost: x\r\nAuthorization: "
               + basic("postgres", "x").encode() + b"\r\n\r\n")
    held = []
    for _ in range(2):
        s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        s.sendall(request)
        s.recv(65536)
        held.append(s)
    third = raw(request, timeout=5)
    check("kb.endpoint_max_sessions: the third connection answered 503",
          third.startswith(b"HTTP/1.1 503"), third[:120])
    held[0].close()
    time.sleep(0.5)
    again = raw(request, timeout=5)
    check("a session freed: a connection served again", again.startswith(b"HTTP/1.1 200"), again[:120])
    for s in held[1:]:
        s.close()


@case
def idle_timeout():
    sql("ALTER SYSTEM SET kb.endpoint_idle_session_timeout = '1s'")
    sql("SELECT pg_reload_conf()")
    time.sleep(0.5)
    request = (b"GET /postgres/query?query=ASK%7B%7D HTTP/1.1\r\nHost: x\r\nAuthorization: "
               + basic("postgres", "x").encode() + b"\r\n\r\n")
    s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
    s.sendall(request)
    s.recv(65536)
    time.sleep(2.5)
    s.settimeout(3)
    try:
        closed = s.recv(1) == b""
    except (socket.timeout, ConnectionResetError):
        closed = False
    check("kb.endpoint_idle_session_timeout closes an idle connection", closed)
    s.close()
    sql("ALTER SYSTEM RESET kb.endpoint_idle_session_timeout")
    sql("SELECT pg_reload_conf()")


# ---------------------------------------------------------------------
# The off phase: the server restarted with kb.endpoint_listen_addresses
# empty, the endpoint off from the start
# ---------------------------------------------------------------------

@case
def started_off():
    check("started with the list empty: connections refused", refused(timeout=2))
    check("started with the list empty: the acceptor runs", acceptors() == 1, acceptors())
    set_listen("127.0.0.1")
    check("a reload naming an address: switched on", answering())
    set_listen(None)
    check("a reload emptying it again: switched off", refused())


def main(work, phase):
    steps = {
        "main": (setup, queries, updates, graph_store, databases, framing, logins, privileges,
                 cancel_and_timeout, tls, clients, switched_by_reload),
        "limits": (anonymous_read_only_cors, session_limit, idle_timeout),
        "off": (started_off,),
    }[phase]
    for step in steps:
        step()
    failed = [n for n, ok in results if not ok]
    print(f"\n{len(results) - len(failed)} of {len(results)} endpoint checks pass")
    with open(os.path.join(work, f"endpoint-{phase}-results.txt"), "w") as f:
        for n, ok in results:
            f.write(f"{'ok' if ok else 'FAIL'}\t{n}\n")
    return 1 if failed else 0


if __name__ == "__main__":
    import urllib3
    urllib3.disable_warnings()
    sys.exit(main(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "main"))
