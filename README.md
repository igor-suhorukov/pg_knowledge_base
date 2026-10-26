# pg_knowledge_base

[oxigraph](https://github.com/oxigraph/oxigraph)'s SPARQL in PostgreSQL 19:
an RDF store in PostgreSQL's tables, SPARQL 1.1 queries and updates from SQL
-- as rows, or as the JSON oxigraph's server answers -- R2RML mappings that
read PostgreSQL's own tables as RDF beside the store, and a SPARQL 1.1
endpoint inside the server, which answers as oxigraph's server does and
logs its clients in as PostgreSQL logs a backend's in.

It works on an unmodified PostgreSQL 19 core.  oxigraph's code -- its
parser, optimizer and evaluator, its parsers and serializers of RDF and of
results -- runs in an engine library built from oxigraph's Rust crates
(`engine/`), behind a C ABI (`include/kb_engine.h`); the extension proper
is C, built by PGXS.  The endpoint has the architecture:
an acceptor, and a background worker for each connection that owns its socket 
as a backend does.

The design, what each piece does, and whether the store should be a table
access method, are in [docs/design.md](docs/design.md).

## Status

As of 2026-10-23, built and tested against:

| | |
|---|---|
| PostgreSQL | 19rc1, the official image `postgres:19rc1-trixie` (PGDG's `19~rc1-1.pgdg13+1`), unmodified |
| oxigraph | [`igor-suhorukov/oxigraph`](https://github.com/igor-suhorukov/oxigraph) at `31cbce3f133` (`0.6.0-dev`, branch `sparql-optimizer-statistics`), its crates git dependencies of `engine/Cargo.toml` |
| Rust | 1.99 |

- Every leg of `test/run.sh` passes: `checks` (the ABI's two
  declarations agree), `engine`,
  `enginetest`, `suite` (7 files: store, sparql, update, r2rml, security,
  bulk, replication; then nopreload, on a server that does not preload the
  module), `r2rml`, `sparql`, `endpoint`, and, named, `otel`; the default
  ones also as a GitHub runner runs them (`.github/workflows/build.yml`).
- The W3C SPARQL 1.1 query and update test cases: 990 pass, none fails; 11
  differ as oxigraph's own store differs from them -- it keeps literals as
  values, `"01"^^xsd:integer` as `1` -- and 6 are skipped, as oxigraph's
  testsuite skips them.
- The W3C RDB2RDF test cases of R2RML and the Direct Mapping, from Ontop's
  checkout: 95 of 95.
- The endpoint: 88 checks of the protocol, the graph store, a second
  database's paths, logins by `pg_hba.conf` line, TLS, sessions, cancels,
  from a local and a remote client, and 8 of its limits.
- Traces: 25 checks of the spans pg_otel_probe (`38a52ca`) writes, read
  back from its agent's Arrow IPC files.

## What it does

- **`kb.sparql()`** returns a query's solutions as rows, each column the
  variable of its name, converted to its type: `text`, numbers, `boolean`,
  dates and timestamps, `interval`, `bytea`, `jsonb` for the whole term.
- **`kb.sparql_json()`** returns the results as oxigraph's server answers
  them: SPARQL 1.1 Query Results JSON, JSON-LD for CONSTRUCT and DESCRIBE;
  `kb.sparql_text()` in any format, XML, CSV, TSV, Turtle, N-Triples and
  the others.
- **`kb.sparql_update()`** runs a SPARQL 1.1 update in the caller's
  transaction: it commits and rolls back with it.
- **The store is PostgreSQL's tables**, `kb.quads` and its six btree
  orders, `kb.terms`, `kb.graphs`: MVCC, WAL, replication, backups,
  `pg_dump` and privileges are PostgreSQL's.  Logical replication too:
  `kb.quads`' replica identity is its unique index, so a publication
  carries its deletes.  `kb.load()` and
  `kb.load_file()` read Turtle, N-Triples, N-Quads, TriG, RDF/XML, JSON-LD
  and N3.
- **R2RML.** `kb.add_mapping()` declares how tables are RDF; every query
  then reads the mapped tables beside the store, as SQL the pattern's bound
  terms narrow, run as the querying role with its privileges and row-level
  security; a basic graph pattern over them, with its OPTIONALs, EXISTS and
  simple FILTERs, is one SQL query.  The optimizer's estimates of the
  patterns are the store's counts and the plans of the mappings' SQL.
  `kb.direct_mapping()` writes the W3C Direct Mapping of tables.
- **Traces.** With pg_otel_probe (pg_opentelemetry_probe) preloaded beside
  it, the probe's span of a statement that runs SPARQL
  carries the query's operation, its text with the literals replaced, its
  plan with each node's results and time, the rows, the engine's time and
  memory, the store's scans and writes and R2RML's SQL; the endpoint's
  requests become statements the probe traces, children of the callers'
  spans.
- **The endpoint** serves the SPARQL 1.1 protocol and the graph store
  protocol on oxigraph's paths below each database's name --
  `/<database>/query`, `/<database>/update`, `/<database>/sparql`,
  `/<database>/store` -- over HTTP/1.1 and TLS; each request is a transaction, logged in by
  `pg_hba.conf` and the role's password, seen in `pg_stat_activity`, ended
  by `pg_cancel_backend()`.

## Using it

The SQL functions need no preloading: a backend loads the module at
`CREATE EXTENSION`, or at its first call.  The endpoint and the traces need
`shared_preload_libraries = 'pg_knowledge_base'`.

```sql
CREATE EXTENSION pg_knowledge_base;   -- in the schema kb

SELECT kb.load($$
@prefix ex: <http://example.com/> .
@prefix foaf: <http://xmlns.com/foaf/0.1/> .
ex:alice a foaf:Person ; foaf:name "Alice" ; foaf:age 30 ; foaf:knows ex:bob .
ex:bob a foaf:Person ; foaf:name "Bob"@en ; foaf:age 25 .
$$);

SELECT * FROM kb.sparql($$
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
SELECT ?name ?age WHERE { ?p a foaf:Person ; foaf:name ?name ; foaf:age ?age } ORDER BY ?age
$$) AS t(name text, age int);
--  name  | age
-- -------+-----
--  Bob   |  25
--  Alice |  30

SELECT kb.sparql_json('PREFIX foaf: <http://xmlns.com/foaf/0.1/> SELECT ?n WHERE { ?p foaf:name ?n } ORDER BY ?n');
-- {"head": {"vars": ["n"]}, "results": {"bindings": [
--   {"n": {"type": "literal", "value": "Alice"}},
--   {"n": {"type": "literal", "value": "Bob", "xml:lang": "en"}}]}}

SELECT kb.sparql_update('PREFIX foaf: <http://xmlns.com/foaf/0.1/>
                         DELETE { ?p foaf:age ?a } INSERT { ?p foaf:age ?b }
                         WHERE { ?p foaf:age ?a BIND(?a + 1 AS ?b) }');
```

A large document loads faster in bulk -- `kb.load_file('/data/big.nt',
bulk => true)`: kb.quads' indexes dropped and built again around it, in the
caller's transaction, kb.quads locked meanwhile; for a store's first load
and for loads large against the store (docs/design.md §2.6).

A column may be any type the value converts to, or `jsonb` for the term
itself; ASK gives one column, `result`, and CONSTRUCT and DESCRIBE three,
`subject`, `predicate`, `object`.  The results join tables as any set
returning function does:

```sql
-- salaries (name text, salary int): an ordinary table
SELECT s.name, s.salary, t.age
FROM salaries s
JOIN kb.sparql('SELECT ?name ?age WHERE { ?p <http://xmlns.com/foaf/0.1/name> ?name ;
                                             <http://xmlns.com/foaf/0.1/age> ?age }')
     AS t(name text, age int) USING (name);
```

### R2RML

```sql
CREATE TABLE emp (id int PRIMARY KEY, name text NOT NULL, salary numeric(10, 2));
INSERT INTO emp VALUES (7, 'Alice Smith', 5000.50), (9, 'Carol', 7000);

SELECT kb.add_mapping('hr', $$
@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix foaf: <http://xmlns.com/foaf/0.1/> .
@prefix ex: <http://example.com/hr#> .
<#Emp> rr:logicalTable [ rr:tableName "emp" ] ;
  rr:subjectMap [ rr:template "http://example.com/emp/{id}" ; rr:class foaf:Person ] ;
  rr:predicateObjectMap [ rr:predicate foaf:name ; rr:objectMap [ rr:column "name" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:salary ; rr:objectMap [ rr:column "salary" ] ] .
$$);

-- the store's triples and the table's rows in one query
SELECT kb.load('<http://example.com/emp/7> <http://xmlns.com/foaf/0.1/knows> <http://example.com/emp/9> .', 'ntriples');
SELECT * FROM kb.sparql($$
PREFIX foaf: <http://xmlns.com/foaf/0.1/> PREFIX ex: <http://example.com/hr#>
SELECT ?a ?b ?salary WHERE { ?x foaf:knows ?y . ?x foaf:name ?a . ?y foaf:name ?b ; ex:salary ?salary }
$$) AS t(a text, b text, salary numeric);
--       a      |   b   | salary
-- -------------+-------+--------
--  Alice Smith | Carol |   7000

-- the W3C Direct Mapping of tables, as a mapping
SELECT kb.add_mapping('direct', kb.direct_mapping(ARRAY['emp'::regclass], 'http://example.com/db/'));
```

A pattern's bound terms become the SQL's conditions:
`<http://example.com/emp/7> foaf:name ?n` reads the row whose `id` is 7,
through the table's index.  Patterns that share a variable whose terms a
column gives -- an IRI of a column, a blank node, a string -- are joined by
PostgreSQL in one SQL query, with their OPTIONALs and EXISTS
(`kb.r2rml_pushdown`, [docs/design.md §4.4](docs/design.md#44-basic-graph-patterns-in-one-sql-query));
an IRI a template makes, as `http://example.com/emp/{id}` here, keeps its
pattern to a query of its own.
`kb.check_mapping()` checks a mapping without storing it,
`kb.enable_mapping()` and `kb.drop_mapping()` switch one off and remove
it, `SET kb.with_mappings = off` reads the store alone.  A mapping's SQL
runs as the role that queries; only roles that may write `kb.mappings` add
mappings.  A backend compiles the mappings once, until a mapping or the
catalog changes -- DDL makes `kb.catalog_version` one more -- and reads
those another backend compiled from `pg_stat_tmp/pg_knowledge_base`
(`kb.r2rml_cache`, [docs/design.md §4.2](docs/design.md#42-compilation)).

### The endpoint

```
# postgresql.conf
shared_preload_libraries = 'pg_knowledge_base'
kb.endpoint_listen_addresses = '*'
kb.endpoint_port = 7878
kb.endpoint_ssl_cert_file = 'server.crt'  # TLS; the server's own by default
kb.endpoint_ssl_key_file = 'server.key'

# pg_hba.conf: the endpoint's logins, as a backend's
hostssl all all 10.0.0.0/8 scram-sha-256
```

The endpoint is off unless `kb.endpoint_listen_addresses` names an address.
Empty, its default, it listens on no port, and the module, preloaded,
serves the SQL functions and the traces alone.  The list is the endpoint's
switch -- there is no other -- and a reload turns it, without a restart:

```sql
ALTER SYSTEM SET kb.endpoint_listen_addresses = '';   -- off
SELECT pg_reload_conf();
ALTER SYSTEM RESET kb.endpoint_listen_addresses;      -- on again, as postgresql.conf has it
SELECT pg_reload_conf();
```

Switched off, the endpoint takes no new connection and starts no request:
a request being served ends as it would have, and its connection closes;
an idle connection closes at once.  The acceptor, a background worker,
runs whenever the module is preloaded, the endpoint on or off.

```sh
curl -u alice:secret -H 'Accept: application/sparql-results+json' \
     --data-urlencode 'query=SELECT * WHERE { ?s ?p ?o } LIMIT 3' https://db:7878/mydb/query
curl -u alice:secret --data-urlencode 'update=INSERT DATA { <http://example.com/s> <http://example.com/p> 1 }' \
     https://db:7878/mydb/update
curl -u alice:secret -X PUT -H 'Content-Type: text/turtle' --data-binary @data.ttl \
     'https://db:7878/mydb/store?graph=http://example.com/g'
```

Every path is below a database's name, percent-encoded:

| Path | |
|---|---|
| `/<database>/query` | SPARQL queries; GET without `query`, the service description |
| `/<database>/update` | SPARQL updates |
| `/<database>/sparql` | both |
| `/<database>/store`, `?default`, `?graph=<iri>`, `/<database>/store/...` | the graph store protocol |
| `/<database>/` | a page with a query form (`/<database>` is sent there) |

There is no path without a database: `/` answers 404, and a client that
asks oxigraph's `/query` asks for a database named `query`.  A connection
serves one database: a request for another on it is answered 421, for the
client to ask on a new connection.  Any SPARQL 1.1 client works against a
database's `/query` and `/update`.  A request without credentials runs as
`kb.endpoint_anonymous_role`, when that names a role `pg_hba.conf` trusts;
`kb.endpoint_read_only` refuses writes.  Each connection is a background
worker: `kb.endpoint_max_sessions` of them at once, 3 by default, counted in
`max_worker_processes` with the acceptor -- more sessions need it raised
from PostgreSQL's default of 8.  The settings are in
[docs/design.md §2.7](docs/design.md#27-settings).

### Traces

```
# postgresql.conf, beside pg_otel_probe's own settings
shared_preload_libraries = 'pg_otel_probe, pg_knowledge_base'
kb.trace_query_text = inherit   # off, normalized (literals as ?), raw; inherit: as pg_otel_probe.query_text
kb.trace_plan = on              # plans, a traced query evaluated with statistics
```

A statement's span then carries `kb.sparql.calls`, `kb.sparql.operation`,
`kb.sparql.text`, `kb.sparql.plan` -- spareval's plan, a node a line:

```
Project(?name, ?age) rows=2 time=0.257ms
  Sort(ASC(?age)) rows=2 time=0.246ms
    Filter((?age > ?^^<http://www.w3.org/2001/XMLSchema#integer>)) rows=2 time=0.231ms
      Lateral rows=3 time=0.218ms
        QuadPattern(?p <http://example.com/name> ?name) rows=3 time=0.155ms
        QuadPattern(?p <http://example.com/age> ?age) rows=3 time=0.037ms
```

-- `kb.sparql.rows`, `kb.sparql.duration_ms`, `kb.engine.memory_peak_bytes`,
`kb.store.*`, `kb.r2rml.*`, `kb.load.*`, and on an endpoint request
`kb.http.request.method`, `kb.http.route`,
`kb.http.response.status_code`.  An endpoint request with a W3C
`traceparent` header joins its caller's trace, its R2RML queries and its
commit as its span's children (docs/design.md §2.8).

## A table access method?

No, not now (docs/design.md §3.5).  The engine drives its scans of the
store's btree indexes itself, so a table access method would give SPARQL
nothing of the planner's, and would change only the heap -- a fifth of the
store's bytes -- while it took on MVCC, WAL, replication and VACUUM, which
heap gives for nothing.  At 3.6 million triples the store answered as fast
as oxigraph's server or faster -- 4 to 8 times on queries that are nested
loops of lookups (docs/design.md §6).  The gaps are elsewhere:

| 3,577,116 triples | pg_knowledge_base | oxigraph (RocksDB) |
|---|---:|---:|
| on disk | 1,613 MB, 473 B a quad | 324 MB, 95 B a triple |
| load | 43.8 s, one transaction | 1.5 s, a bulk loader on 16 threads |
| bulk load (`bulk => true`) | 19.1 s, 392 B a quad | -- |
| a lookup (one product's triples) | 0.65 ms (SQL: 0.14 ms) | 0.54 ms |
| an aggregate over 200,000 offers | 1.17 s | 9.76 s |

Density is mostly in the indexes, where prefix compression would close it:
indexes built rather than grown first -- `bulk => true`, or `REINDEX TABLE
kb.quads` after a large load: 392 bytes a quad -- then narrower keys or an
index access method with prefix-compressed leaves.

## Limits

- Literals of XML Schema's datatypes are kept as values, as oxigraph's
  store keeps them: their lexical forms are not.
- R2RML's basic graph patterns go to SQL whole, with their OPTIONALs,
  EXISTS and simple FILTERs; a variable predicate, a property path,
  aggregates and ORDER BY are evaluated after the rows are read, not pushed
  into SQL as Ontop does.  SPARQL updates do not write the tables.
- What the engine allocates is bounded by `kb.engine_memory_limit`, not by
  `work_mem`; a cancel or the limit is seen at the next row or quad, which
  a cartesian product under a sort may hold back for one pass over its
  inner side.
- SERVICE is not served.  The endpoint speaks HTTP/1.1 and HTTP's basic
  authentication only.

More in [docs/design.md §8](docs/design.md#8-limits).

## Building and testing

```sh
test/run.sh images        # the builder and dev images
test/run.sh fetch         # cargo's fetch: the crates, and oxigraph's repository at its commit
test/run.sh engine        # the engine library, ~/.cache/pg_accel/pg_knowledge_base/engine
test/run.sh all           # checks, engine, suite, r2rml, sparql, endpoint
test/run.sh otel          # traces through pg_otel_probe, built from its pinned commit:
                          # only when named, or by all with KB_OTEL=1
test/run.sh bench         # the store against oxigraph's RocksDB and server (KB_BENCH_PRODUCTS)
```

Every leg runs in a container of `docker/compose.yml`, its results in
`~/.cache/pg_accel/pg_knowledge_base/runs`.  The extension builds and runs
without pg_otel_probe -- its API header is copied in `include/`, and the
probe is looked up by name at run time -- so only the `otel` legs need the
probe's repository (`KB_OTEL_PROBE_REPO`), and they run only when named, or
by `all` with `KB_OTEL=1`.

oxigraph's crates are git dependencies of `engine/Cargo.toml` on one commit
of oxigraph's repository, now the fork `igor-suhorukov/oxigraph`; to move
to another commit, change the seven `rev`s together.  `test/run.sh fetch`
fetches the commit with its submodules -- 625 MB in cargo's cache, once --
and takes the lockfile into the tree when cargo resolved it again;
`engine`, `enginetest` and `oxigraph` run it first when the cache lacks
what `engine/Cargo.toml` and `engine/Cargo.lock` name, and then build
offline from the lockfile as it is.  No build reads a local checkout of
oxigraph.  oxigraph's published crates (0.5.12 on crates.io) are older
than the API the engine is written against: the move to them waits for
0.6.  The extension needs OpenSSL; the engine, Rust 1.99.

The default build needs public sources only: the dev image is PostgreSQL
19's official image (`postgres:19rc1-trixie`, `KB_PG_TAG`) with PGDG's PGXS
of the server's own version; the builder, Rust's; oxigraph's crates,
w3c/rdf-tests and Ontop's W3C RDB2RDF test cases come from GitHub at pinned
commits, the test clients from PyPI.  The legs build the images when they
are not there, and run as the host's user.  `.github/workflows/build.yml`
runs it on GitHub's runners: `images`, `all`, `enginetest`.

```sh
make PG_CONFIG=/usr/lib/postgresql/19/bin/pg_config install
make PG_CONFIG=/usr/lib/postgresql/19/bin/pg_config install-engine \
     KB_ENGINE=~/.cache/pg_accel/pg_knowledge_base/engine/lib/libkb_engine.so
```

## Licence

pg_knowledge_base's files are Apache-2.0.  `src/endpoint/hba.c` carries
copies of PostgreSQL's `pg_hba.conf` matching, under the PostgreSQL
Licence.  `engine/src/update.rs` and `engine/src/http.rs` are ports of
oxigraph's update evaluator and server (MIT OR Apache-2.0, used under
Apache-2.0); oxigraph's crates are linked, not copied.
