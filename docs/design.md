# pg_knowledge_base: oxigraph's SPARQL in PostgreSQL 19

| | |
|---|---|
| PostgreSQL 19 | 19rc1, unmodified: the official image `postgres:19rc1-trixie`, PGDG's packages `19~rc1-1.pgdg13+1` on Debian trixie, without assertions; the measurements of §6 were made before, on `REL_19_STABLE` at `0f433a6b357` (19beta3) built with assertions |
| oxigraph | [`igor-suhorukov/oxigraph`](https://github.com/igor-suhorukov/oxigraph) at `f0a06d83d90` (2026-10-19, `0.6.0-dev`; branch `sparql-optimizer-statistics`: oxigraph's `main` at `3e53f53f`, then a cost-based join reordering on dataset statistics): spargebra, sparopt, spareval, sparesults, oxrdf, oxrdfio, oxsdatatypes, git dependencies of `engine/Cargo.toml` that cargo fetches; for the bench, its CLI and server with RocksDB, built from cargo's checkout of the same commit |
| Ontop | [`ontop/ontop`](https://github.com/ontop/ontop) at `964dbb7` (2026-10-15): the W3C RDB2RDF test cases it carries (`test/rdb2rdf-compliance`), fetched by `test/run.sh` |
| W3C rdf-tests | `d5d930b`, the commit oxigraph's testsuite pins: the SPARQL 1.1 query and update test cases |
| pg_opentelemetry_probe | `github/pg_opentelemetry_probe` at `38a52ca` (2026-10-16): its extension API (`include/pg_otel_probe_api.h`, copied), and the probe and its agent built from a `git archive` of that commit for the `otel` leg |
| Rust | 1.99 (`rust:1.99-trixie`) |

**How statements are marked.** A statement with a file reference is what that file does. *Unverified* marks what no test here exercises.

## 0. Summary

- **SPARQL 1.1 in SQL.** `kb.sparql(query)` returns a query's solutions as rows, its columns matched to the query's variables by name, each value converted to its column's type (`AS t(name text, age int, born date, p jsonb)`); `kb.sparql_json(query)` returns what oxigraph's server answers, SPARQL 1.1 Query Results JSON, or JSON-LD for CONSTRUCT and DESCRIBE; `kb.sparql_text(query, format)` any format oxigraph writes; `kb.sparql_update(update)` runs an update in the caller's transaction (§2.3--§2.5).
- **oxigraph's evaluator, in the backend.** spargebra parses, sparopt optimizes and spareval evaluates, as in oxigraph's own store, in an engine library, `libkb_engine.so`, built from oxigraph's crates behind a C ABI (`include/kb_engine.h`) and loaded by `dlopen()` at a backend's first call. The extension proper is C, built by PGXS. The engine starts no thread and reaches PostgreSQL only through the callbacks of a call, which never let a PostgreSQL error unwind through Rust (§2.2).
- **The store is PostgreSQL's tables.** `kb.quads` holds a quad as four 16-byte term keys in six btree orders, oxigraph's nine; `kb.terms` the text of the terms whose keys are hashes. A key is computed from its term alone -- small values inline, other terms a 120-bit SipHash -- so a query's constants and the terms R2RML makes are keys without a lookup. The engine reads the store by index-only scans it drives itself, in the caller's snapshot; the store has PostgreSQL's MVCC, WAL, replication -- logical too, kb.quads' replica identity its unique index --, backups, `pg_dump` and locks (§3).
- **R2RML mappings are virtual graphs.** `kb.add_mapping(name, turtle)` checks a mapping against its tables and stores it; from then on every SPARQL query reads the store and the mapped tables as one dataset. A triple pattern becomes SQL over a mapping's logical table with the pattern's bound terms as conditions -- a bound IRI inverted through its template, a literal cast to its column's type -- run through SPI as the querying role, with that role's privileges and row-level security, read only. `kb.direct_mapping()` writes the W3C Direct Mapping of tables as an R2RML mapping (§4).
- **A SPARQL 1.1 endpoint with vexec_flight's architecture.** An acceptor background worker listens on `kb.endpoint_port` and starts a session worker for each connection, which takes the socket and owns it as a backend does: HTTP/1.1 with keep-alive, TLS of its own, logins by `pg_hba.conf` and the role's password through HTTP's basic authentication, each request a transaction. The paths, parameters, content negotiation and statuses are oxigraph's server's, its protocol code ported into the engine: the SPARQL 1.1 protocol, the graph store protocol, the service description -- each below a database's name: `/<database>/query`, `/<database>/update`, `/<database>/sparql`, `/<database>/store` (§5).
- **Traces, through pg_otel_probe.** With the probe preloaded beside the module, the probe's span of a statement that runs SPARQL carries it -- the operation, the text with its literals replaced, the plan with each node's results and time, the rows, the engine's time and memory, the store's scans and writes, R2RML's SQL -- through the probe's API for other extensions; the endpoint's requests become statements the probe traces, children of their callers' spans (§2.8).
- **Conformance.** The W3C SPARQL 1.1 query and update test cases: 990 pass, 11 differ as oxigraph's own store differs from them (§3.1), 6 are skipped as oxigraph's testsuite skips them. The W3C RDB2RDF test cases of R2RML and the Direct Mapping: 95 of 95 (§7).
- **A table access method for the store is not worth building now** (§3.5). The store's bytes are in its indexes, which a table access method does not change; heap gives MVCC, WAL, replication and VACUUM that a table AM would have to rebuild; and at 3.6 million triples the store's reads were as fast as oxigraph's server's, its lookups and scans faster (§6). The gaps are density -- 473 bytes a quad against RocksDB's 95, 392 after a bulk load -- which prefix-compressed keys in an index AM, or narrower keys, close more cheaply, and loads, which the bulk mode narrows (§2.6).
- **Every build and every test runs in Docker containers** (§7).

## 1. Decisions and rules

1. **oxigraph is the dependency; its crates are linked, not copied.** `engine/Cargo.toml` names them as git dependencies on one commit of oxigraph's repository, every `rev` the same; cargo fetches it with its submodules, and `engine/Cargo.lock` pins it. No build reads a local checkout of oxigraph, where others' work is under way. Two pieces are ports of oxigraph's code, kept to its behaviour: the update evaluator (`engine/src/update.rs`, from `lib/oxigraph/src/sparql/update.rs`) and the endpoint's protocol (`engine/src/http.rs`, from `cli/src/main.rs`).
2. **An unmodified PostgreSQL 19 core**, and only what `REL_19_STABLE` exports.
3. **A PostgreSQL error never unwinds through Rust, and a panic never through C** (§2.2).
4. **The engine runs on the backend's thread and starts none.** spareval is synchronous, and every callback is the backend's own code.
5. **SQL's semantics around SPARQL.** A query reads the caller's snapshot; an update writes in the caller's transaction; the store's privileges are its tables'; a mapping's SQL runs as the querying role.
6. **oxigraph's store's answers where SPARQL leaves room:** literals kept as values (§3.1), the default graph the store's own, CLEAR keeping named graphs, each loaded document's blank nodes its own.
7. **vexec_flight's process model for the endpoint**: its acceptor, `pg_hba.conf` copy and login check reused, HTTP/1.1 in place of HTTP/2 and gRPC (§5).
8. **Docker containers only, from public sources.** The host runs `docker` and `git`, and nothing else of the build or the tests; the default build needs no local checkout and no private image, and runs on GitHub's runners (§7).

## 2. The extension

### 2.1 The pieces

| Path | What it is |
|---|---|
| `src/module.c` | `_PG_init`: the settings (§2.7); the endpoint's shared table and acceptor when preloaded |
| `src/engine.c` | the engine library loaded on first use, its ABI checked, its errors raised as PostgreSQL's |
| `src/call.c` | one call of the engine: the host it is given, its callbacks' kept errors, its cursors, snapshot, output and error context |
| `src/store.c` | the store's scans and writes (§3) |
| `src/sql.c` | R2RML's SQL: logical tables described, rows read, values' lexical forms; the enabled mappings (§4.3) |
| `src/convert.c` | RDF terms as the column types of `kb.sparql()`'s rows (§2.4) |
| `src/trace.c` | the SPARQL of a statement on pg_otel_probe's span of it (§2.8) |
| `src/functions.c` | the SQL functions (§2.3) |
| `src/endpoint/` | the endpoint (§5): `acceptor.c`, `settings.c`, `login.c`, `hba.c`, `session.c`, `http.c`, `tls.c` |
| `sql/pg_knowledge_base--0.1.sql` | the tables, functions and views |
| `include/kb_engine.h` | the engine library's C ABI |
| `include/pg_otel_probe_api.h` | pg_otel_probe's API for other extensions, a copy of the probe's at `38a52ca` |
| `engine/` | the engine library: `lib.rs` (the entry points), `ffi.rs`, `host.rs` (the callbacks), `key.rs` (term keys), `terms.rs`, `dataset.rs`, `query.rs`, `update.rs`, `writer.rs`, `load.rs`, `format.rs`, `http.rs` (the protocol), `alloc.rs` (counted memory), `trace.rs` (texts and plans for traces), `r2rml/` |
| `checks/abi.py` | the ABI's two declarations compared, member for member and constant for constant |
| `test/` | the legs (§7) |

### 2.2 The engine library

`libkb_engine.so` is a Rust `cdylib` over oxigraph's crates (`engine/Cargo.toml`). The extension loads it with `dlopen(RTLD_NOW | RTLD_LOCAL)` in a backend or a worker at its first call, never in the postmaster, and asks `kb_engine_api(KB_ABI_VERSION)` for its table of entry points; a library built for another ABI is refused (`src/engine.c`). `kb.engine_library` names another build.

**A call.** Each SQL function, and each request of the endpoint, is one call of the engine (`src/call.c`). The extension fills a `KbHost`, a table of callbacks -- the store's scans (`quads_open`, `quads_next`, `quads_close`), its dictionary (`terms_get`), its named graphs, its writes, R2RML's SQL (`sql_describe`, `sql_open`, `sql_next`, `sql_close`), the enabled mappings, the output (`write`, `respond`), `check_interrupts` -- and passes it to an entry point. A callback runs its PostgreSQL code under `PG_TRY` (`kb_call_guard()`): an error is copied into the caller's memory and flushed, the call is marked failed, and the callback returns `KB_HOST_ERROR`. The engine then returns `KB_ERR_HOST` from its entry point, unwinding only its own frames, and the extension re-throws the kept error, whose abort releases what the failed callback held -- buffer pins, locks, scans -- through the resource owners, as for any error. After a failure every callback fails at once and closes do nothing. The engine's own errors come back as a code, an HTTP status and a message, raised with the code's SQLSTATE. Every error of a call carries its context -- `SPARQL query "..."`, `SPARQL update "..."`, `R2RML mapping "..."` -- PostgreSQL's errors inside callbacks too. Every entry point catches panics (`catch_unwind`, `panic = "unwind"`) and returns `KB_ERR_PANIC`.

**Snapshots.** A call reads the active snapshot. A call that may write -- an update, a load -- reads through a copy of it, whose command id it advances before a read that follows a write, as SPI's read-write execution does between statements: an update's later operations see its earlier ones' writes.

**Memory.** What the engine allocates -- a join's hash table, a sort, a group -- is in no memory context, and `work_mem` does not bound it. The library's global allocator counts it (`engine/src/alloc.rs`), and past `kb.engine_memory_limit` (1 GB) a call fails with SQLSTATE 53200 (`out_of_memory`), HTTP 503 at the endpoint, and frees what it held.

**Interrupts.** A cancel, `statement_timeout` and `pg_terminate_backend()` reach the engine at its callbacks and through PostgreSQL's `InterruptPending` flag, which the engine reads at each row it makes and each quad it reads (`Host::poll()`), calling `check_interrupts` once it is set; the memory limit is checked there too. spareval's own loops over what it has materialized -- a cartesian product's inner side, a sort's input -- call nothing, and see a cancel at their next row or quad: at worst one pass over a materialized input (§8).

### 2.3 The SQL functions

| Function | |
|---|---|
| `kb.sparql(query, base_iri, default_graphs, named_graphs) RETURNS SETOF record` | a query's results as rows (§2.4): SELECT's solutions; ASK's one row, its column `result`; CONSTRUCT's and DESCRIBE's triples as `subject, predicate, object` |
| `kb.sparql_json(query, base_iri, default_graphs, named_graphs) RETURNS jsonb` | the results as oxigraph's server answers them: SPARQL 1.1 Query Results JSON, JSON-LD for graphs |
| `kb.sparql_text(query, format, base_iri, default_graphs, named_graphs) RETURNS text` | the results in a format: `json`, `xml`, `csv`, `tsv`; `turtle`, `ntriples`, `nquads`, `trig`, `rdfxml`, `jsonld`, `n3`; or a media type |
| `kb.sparql_explain(query, base_iri) RETURNS jsonb` | spareval's plan, the query run to its end, with each node's statistics |
| `kb.sparql_update(update, base_iri, using_graphs, using_named_graphs) RETURNS void` | an update, in the caller's transaction (§2.5) |
| `kb.load(data, format, graph, base_iri, lenient, bulk) RETURNS bigint` | a document into the store (§2.6); `bulk`, kb.quads' indexes built again around it |
| `kb.load_file(path, format, graph, base_iri, lenient, bulk) RETURNS bigint` | a server file into the store, for roles with `pg_read_server_files` |
| `kb.dump(format, graph, with_mappings) RETURNS text` | the store, or one graph of it, as a document; R2RML's triples too when asked |
| `kb.term(key) RETURNS text`, `kb.term_key(term) RETURNS uuid` | a key's term in N-Triples' syntax; a term's key |
| `kb.triples` | a view of the quads as N-Triples terms, the graph NULL for the default graph |
| `kb.stats(exact) RETURNS jsonb`, `kb.vacuum_terms() RETURNS bigint` | the store's sizes; the dictionary's rows no quad uses any more, deleted |
| `kb.add_mapping(name, document, base_iri, replace)`, `kb.check_mapping(name, document, base_iri)`, `kb.enable_mapping(name, enabled)`, `kb.drop_mapping(name, if_exists)` | R2RML's mappings (§4.1) |
| `kb.direct_mapping(tables regclass[], base_iri) RETURNS text` | the W3C Direct Mapping of tables, as an R2RML document for `kb.add_mapping()` |
| `kb.engine_version() RETURNS text` | the library's version |
| `kb.endpoint_request() RETURNS void` | an endpoint request, as the statement the endpoint's session runs where pg_otel_probe is loaded (§2.8); elsewhere it refuses |

`default_graphs` and `named_graphs` are the protocol's `default-graph-uri` and `named-graph-uri`, and replace the query's own dataset, as at oxigraph's server; `using_graphs` and `using_named_graphs` are `using-graph-uri` and `using-named-graph-uri`.

### 2.4 Queries

A query is parsed, optimized and evaluated as oxigraph's store does it (`engine/src/query.rs`), over the dataset of §3.2, lazily: `kb.sparql()` takes the solutions a row at a time into the function's tuplestore; `kb.sparql_json()` and `kb.sparql_text()` serialize them by sparesults and oxrdfio into one value, at most 1 GB.

**Rows.** A column takes the variable of its name, or else the one whose name differs only in case; a column that names no variable is an error that lists the query's variables, and a variable that no column names is left out. An unbound variable is NULL. A value is converted to its column's type (`src/convert.c`):

| Column type | Value |
|---|---|
| `text`, `varchar`, `char`, `name` | a literal's lexical form, an IRI, a blank node's `_:label` |
| `json`, `jsonb` | the term as SPARQL's JSON results write it: `{"type", "value", "datatype", "xml:lang"}` |
| numbers, `boolean`, `date`, `time`, `timestamp`, `timestamptz` | the literal's value by the type's input function; XML Schema's `INF` and `-INF`, a date's time zone and a negative duration adapted |
| `interval` | an `xsd:duration` |
| `bytea` | an `xsd:hexBinary`'s or `xsd:base64Binary`'s bytes, any other value's UTF-8 |
| any other type | its input function over the lexical form |

A value the type does not take is an error that names the column: an IRI or `"abc"` in an `int` column.

**The dataset.** The default graph is the store's default graph, as in oxigraph's store; with `kb.union_default_graph = on` it is the union of every graph, the default graph's too (oxigraph's `union_default_graph`). `FROM` and `FROM NAMED`, or the functions' graph arrays, replace it. R2RML's graphs are read beside the store's unless `kb.with_mappings = off` (§4.3). A query's `BASE` or the function's `base_iri` resolves its relative IRIs.

### 2.5 Updates

An update is a statement of the caller's transaction (`engine/src/update.rs`): `ROLLBACK` takes it back, `ROLLBACK TO SAVEPOINT` its part after the savepoint, and an error in any of its operations the whole update, as a statement's error does. The operations run in turn, each reading what the ones before it wrote; a DELETE/INSERT evaluates its WHERE first, then deletes, then inserts. As in oxigraph's store: the blank nodes of INSERT DATA are fresh, one for each label of the request; CLEAR keeps a named graph, DROP removes it; CREATE fails for a graph that exists, and DROP and CLEAR for one that does not, unless SILENT. LOAD reads only `file:` IRIs, from the server's file system, and only for roles with `pg_read_server_files`; another IRI is refused, or skipped by LOAD SILENT. An update writes the store, never R2RML's tables. In a read-only transaction `kb.sparql_update()`, `kb.load()` and `kb.load_file()` are refused, as INSERT is.

### 2.6 Loads and dumps

`kb.load()` reads a document by oxigraph's parsers (`engine/src/load.rs`): Turtle, N-Triples, N-Quads, TriG, RDF/XML, JSON-LD, N3. `graph` names the graph of its triples -- a dataset's named graphs stay theirs -- `base_iri` resolves its relative IRIs, and `lenient` lets invalid IRIs and language tags through, as oxigraph's `--lenient`. Its blank nodes become fresh ones, as oxigraph's store keeps the documents it loads apart. The quads go to the store in batches of `kb.load_batch` through SPI, in the caller's transaction. A syntax error loads nothing. `kb.load_file()` reads a server file, its format from its extension unless named. `kb.dump()` writes every graph in a dataset format, or one graph (`'default'` the default graph); with `with_mappings` R2RML's triples too.

**Bulk loads.** `kb.load(..., bulk => true)` and `kb.load_file(..., bulk => true)` put a document into the store as `CREATE INDEX` builds an index (`src/store.c`, `kb_bulk_begin()`, `kb_bulk_finish()`). kb.quads is locked, ACCESS EXCLUSIVE, and its indexes dropped, their definitions kept -- an administrator's own indexes' too; one a constraint holds stays and is kept up. The document's quads go to a temporary table without an index, then into kb.quads by one `INSERT ... SELECT DISTINCT ... WHERE NOT EXISTS`, each quad once and none the store holds already, the sort or hash given `maintenance_work_mem`; and the indexes are built again from their definitions: sorted, their leaves filled to the fillfactor, by parallel workers when the table is large. The replica identity goes back to the index built again -- dropped, it left kb.quads none, as `NOTHING` would (§3.3). All of it is the caller's transaction's: an error or a rollback leaves the indexes and the quads as they were. Only kb.quads' owner may, as only it may drop the indexes. The cost is the whole store's -- the indexes are built over every quad, old and new, and kb.quads stays locked until the transaction ends, other sessions' queries waiting -- so it is a mode for a store's first load, and for loads large against the store. `VACUUM (ANALYZE) kb.quads` after it, as after any large load, sets the visibility map that the index-only scans read.

### 2.7 Settings

| Setting | Default | Context | What |
|---|---|---|---|
| `kb.engine_library` | `$libdir/libkb_engine.so` | superuser | the engine library |
| `kb.union_default_graph` | off | user | SPARQL's default graph is the union of every graph |
| `kb.with_mappings` | on | user | SPARQL reads R2RML's virtual graphs beside the store |
| `kb.load_batch` | 8192 | user | the quads one write of a load or an update carries |
| `kb.engine_memory_limit` | 1 GB | superuser | the memory the engine may hold in a backend; 0, no limit |
| `kb.term_cache_size` | 65536 | user | the dictionary's terms a backend keeps |
| `kb.trace_query_text` | inherit | superuser | the SPARQL text and plan labels on pg_otel_probe's spans: `inherit` (pg_otel_probe.query_text's `off` and `raw`, else `normalized`), `off`, `normalized` (literals as `?`), `raw` (§2.8) |
| `kb.trace_plan` | on | superuser | plans on the spans, a traced query evaluated with spareval's statistics |
| `kb.endpoint_listen_addresses` | empty | sighup | where the endpoint listens; empty, the default, listens on none: the endpoint's switch, which a reload turns |
| `kb.endpoint_port` | 7878 | postmaster | the endpoint's port |
| `kb.endpoint_ssl_cert_file`, `kb.endpoint_ssl_key_file` | the server's `ssl_cert_file`, `ssl_key_file` | sighup | TLS; without them plain HTTP only, which `hostssl` lines do not match |
| `kb.endpoint_max_sessions` | 3 | postmaster | connections at once, each a background worker counted in `max_worker_processes` with the acceptor: the default fits PostgreSQL's default of 8, which more sessions need raised |
| `kb.endpoint_idle_session_timeout` | 1 min | sighup | a connection idle this long between requests closes; 0, never |
| `kb.endpoint_anonymous_role` | empty | sighup | the role of a request without credentials, which `pg_hba.conf` must trust |
| `kb.endpoint_read_only` | off | sighup | updates and the graph store's writes refused |
| `kb.endpoint_max_body` | 128 MB | sighup | the largest request body read |
| `kb.endpoint_cors` | off | sighup | CORS headers for any origin |

The endpoint and traces need `shared_preload_libraries = 'pg_knowledge_base'`; the SQL functions do not. A backend that loads the module itself -- at `CREATE EXTENSION`, or at its first call -- defines every setting but the postmaster's, `kb.endpoint_port` and `kb.endpoint_max_sessions`: those can be defined only while the postmaster loads shared_preload_libraries (`init_custom_variable()` exits with FATAL later), and are not, as pg_prewarm leaves `pg_prewarm.autoprewarm` undefined; such a backend has no endpoint (`test/nopreload`). **`kb.endpoint_listen_addresses` switches the endpoint on and off**, and is its only switch, which a reload turns (`ALTER SYSTEM SET kb.endpoint_listen_addresses = ''; SELECT pg_reload_conf();`): empty, the default, the endpoint listens on no port and starts no request, while the module serves SQL and traces. The acceptor runs whenever the module is preloaded, the endpoint on or off -- one background worker, counted in `max_worker_processes` -- so that a reload can switch on an endpoint the server started without (§5.1); a session ends the request it is serving, and closes (§5.2). A list that does not split is refused by the setting's check; one none of whose addresses can be listened on leaves the endpoint off, logged, and is tried again every 5 s. `kb.endpoint_port` and `kb.endpoint_max_sessions` stay the server start's: the port names the UNIX-domain socket between the acceptor and the sessions, and the sessions' table is in shared memory.

### 2.8 Traces

With pg_opentelemetry_probe -- pg_otel_probe -- preloaded beside the module (`shared_preload_libraries = 'pg_otel_probe, pg_knowledge_base'`, in either order), the probe's span of a statement that runs SPARQL carries it (`src/trace.c`). It goes through the probe's API for other extensions (`include/pg_otel_probe_api.h`, the probe's PLAN.md §4.7.4): a statement reporter, which the probe calls in ExecutorEnd and, registered abort-safe, on a failed statement's error path; and its context functions. Neither module links the other, and the module builds without the probe's sources -- the API is a header of inline functions and rendezvous variables found by name, copied here; without the probe the module keeps only the executor's statements under way, and the probe is unchanged.

| Attribute | |
|---|---|
| `kb.sparql.calls` | the statement's SPARQL calls: `kb.sparql()` and the other functions, loads, an endpoint request |
| `kb.sparql.operation` | SELECT, ASK, CONSTRUCT, DESCRIBE, UPDATE or LOAD: the longest call's |
| `kb.sparql.text` | its text, at most 1 kB, its literals replaced by `?` (`kb.trace_query_text`) |
| `kb.sparql.plan`, `kb.sparql.plan_nodes` | its plan, spareval's explanation as `kb.sparql_explain()` gives it: a node a line, children indented, each with its results and time; at most 2 kB, the nodes past them counted |
| `kb.sparql.rows`, `kb.sparql.duration_ms` | the calls' rows, booleans or triples, and the engine's time |
| `kb.engine.memory_peak_bytes` | the most the engine held during a call |
| `kb.store.scans`, `kb.store.quads_read`, `kb.store.terms_looked_up` | the store's index scans, the quads they gave, the dictionary's lookups |
| `kb.store.quads_written`, `kb.store.quads_deleted`, `kb.store.terms_written` | the store's writes |
| `kb.r2rml.queries`, `kb.r2rml.rows` | R2RML's SQL queries, and the rows they read |
| `kb.load.quads`, `kb.load.bulk` | a load's quads, and whether it was a bulk load (§2.6) |
| `kb.http.request.method`, `kb.http.route`, `kb.http.response.status_code` | an endpoint request's |

A span of the `otel` leg (§7), `SELECT * FROM kb.sparql($1) AS t(name text, age int)`:

```
kb.sparql.calls = 1
kb.sparql.operation = SELECT
kb.sparql.text = PREFIX ex: <http://example.com/> SELECT ?name ?age WHERE { ?p ex:name ?name ; ex:age ?age FILTER(?age > ?) } ORDER BY ?age
kb.sparql.plan =
  Project(?name, ?age) rows=2 time=0.257ms
    Sort(ASC(?age)) rows=2 time=0.246ms
      Filter((?age > ?^^<http://www.w3.org/2001/XMLSchema#integer>)) rows=2 time=0.231ms
        Lateral rows=3 time=0.218ms
          QuadPattern(?p <http://example.com/name> ?name) rows=3 time=0.155ms
          QuadPattern(?p <http://example.com/age> ?age) rows=3 time=0.037ms
kb.sparql.rows = 2, kb.sparql.duration_ms = 1.11, kb.engine.memory_peak_bytes = 83320
kb.store.scans = 4, kb.store.quads_read = 6
```

**Which statement a call is of.** The probe calls a reporter with a statement's QueryDesc; a function is called with no pointer to its statement. So ExecutorRun and ExecutorFinish keep the statements under way, and a call belongs to the innermost: the SELECT that calls `kb.sparql()`, a statement of a PL/pgSQL function that does, the endpoint's statement of a request. A call is traced when the probe has a span for that statement (its context function `outgoing()`), and its figures go to a record of the statement, which ExecutorEnd drops once the probe has read it -- whichever module's hook runs first, the probe reads before the executor ends -- and the end of the transaction, or of the subtransaction that made it, drops with the rest. A statement's several calls -- `kb.sparql()` in a LATERAL, in a function called per row -- are added up; the longest names the operation, the text and the plan.

**Plans and texts.** A traced query is evaluated with spareval's statistics -- a timer around every node's every row -- for its plan; `kb.trace_plan = off` spares that, and an untraced statement never pays it. The text and the plan's labels have their literals replaced by `?` unless `kb.trace_query_text` is `raw`; by default the setting follows `pg_otel_probe.query_text`, whose `off` it honours too. The probe gives an extension's attributes 4 kB a span, shared with other extensions': the text gets at most 1 kB, the plan 2 kB.

**The endpoint's requests.** With the probe loaded, a request runs as a statement of its own, `SELECT kb.endpoint_request()`, through SPI in the request's transaction (`src/endpoint/session.c`): the probe traces it as a backend's statement -- a SERVER span, starting at the request's start, which the session now sets as postgres.c sets a message's (pg_stat_activity's `query_start` shows it too) -- a child of the caller's span when the request carries W3C `traceparent` and `tracestate` headers, which the session gives the probe's `set_parent()` for the transaction, as vexec_flight gives it a call's gRPC metadata. The request's SQL -- R2RML's queries, the store's writes -- and its commit are that span's children. Without the probe a request runs as it did, without the statement.

What the probe sees of the module without any of this: the SQL a call runs through SPI -- R2RML's queries, the store's writes, a bulk load's index builds -- are nested statements of the probe's, with spans and plan nodes of their own.

## 3. The store

`kb.quads (g, s, p, o uuid)` holds the quads as term keys; the default graph's key is all zeros. Its six btree indexes are oxigraph's nine orders in PostgreSQL's terms: `(g, s, p, o)` -- unique, and kb.quads' replica identity (§3.3) --, `(g, p, o, s)` and `(g, o, s, p)` over every quad, and `(s, p, o, g)`, `(p, o, s, g)` and `(o, s, p, g)` over the named graphs' quads alone, partial `WHERE g <> zeros`, as oxigraph keeps the default graph's triples in three orders and the named graphs' quads in six. `kb.terms (key, value, extra)` is the dictionary of hashed keys; `kb.graphs (key)` holds the named graphs, empty ones too. `kb.stats()` gives the sizes.

### 3.1 Terms and keys

A key is 16 bytes computed from its term alone (`engine/src/key.rs`), its first byte the term's kind:

| First byte | Term | The other 15 bytes |
|---|---|---|
| `00` | the default graph | zeros |
| `01` | an IRI | a 120-bit SipHash-2-4 of it; `kb.terms` holds the IRI |
| `02` | a blank node the engine named: a hexadecimal number below 2^120 | the number |
| `03`, `04` | another blank node: a label of 14 bytes or less, or longer | the label inline, or its hash |
| `05`, `06` | a simple literal: 14 bytes or less, or longer | the string inline, or its hash |
| `07`, `08` | a language-tagged literal: value and tag in 13 bytes, or longer | both inline, or the hash |
| `09` | a literal of another datatype, or a value too large to inline | the hash of its canonical form |
| `0A`--`19` | `xsd:boolean`, `float`, `double`, `integer` (and the types derived from it), `decimal`, `dateTime`, `time`, `date`, `gYearMonth`, `gYear`, `gMonthDay`, `gDay`, `gMonth`, `duration`, `yearMonthDuration`, `dayTimeDuration` | the value as oxsdatatypes encodes it, shortened |

Equal terms have equal keys and nothing else does, but for a hash collision -- some 2^-60 for a billion terms. So the evaluator joins on keys, and a key computed for a query's constant, a value the evaluator computes or a term an R2RML row makes matches the store's without a lookup; the dictionary only turns keys back into terms.

**Literals are values**, as oxigraph's store keeps them (`lib/oxigraph/src/storage/numeric_encoder.rs`): `"01"^^xsd:integer` is `"1"^^xsd:integer`, `"007"^^xsd:int` is `"7"^^xsd:integer`, `1.50` is `1.5`. That is oxigraph's behaviour, and it is why 10 W3C test cases differ -- `open-eq-01`, `dawg-str-1`, `dawg-str-2`, `sameTerm-simple`, `sameTerm-eq`, `sameTerm-not-eq`, `eq-graph-1`, `eq-graph-2`, `distinct-1`, `distinct-9` -- which ask for a literal's lexical form to be kept. The eleventh, `bnode01`, is spareval's: `BNODE(str)` gives one blank node for every solution, where SPARQL asks for one per solution; oxigraph's test runner compares results in a way that does not catch it. What passing these, and the six skipped, would take is in [sparql-conformance.md](sparql-conformance.md).

### 3.2 Reads

A pattern's scan takes the index whose leading columns the pattern binds most of (`src/store.c`), and reads the quads from its tuples, visiting the heap only for a page the visibility map does not show all-visible, as `IndexOnlyNext()` does; under SERIALIZABLE it takes the predicate locks an index scan takes. The indexes are found by their columns, not their names: an administrator may drop the three partial ones -- the `g`-first ones then serve named graphs too, by btree's skip scan -- or add others. Reads are the caller's snapshot's, and SELECT on `kb.quads` is checked once a call.

The engine's terms (`engine/src/terms.rs`) are a key and, where the engine has it at hand -- a constant, a computed value, an R2RML term -- the term itself; only the store's hashed keys go to the dictionary, a batch of them at a time, through a backend-wide cache of `kb.term_cache_size` terms. A key's term never changes, so the cache outlives transactions.

The dataset (`engine/src/dataset.rs`) is spareval's `QueryableDataset`, the boundary oxigraph offers for a store of one's own: the store's quads and R2RML's virtual ones (§4.3) for each pattern, a quad that both make given once.

### 3.3 Writes

Writes go through SPI as statements over arrays of keys (`src/store.c`, `engine/src/writer.rs`): `INSERT ... SELECT * FROM unnest(...) ON CONFLICT DO NOTHING` into `kb.quads`, `kb.terms` and `kb.graphs`, and `DELETE FROM kb.quads ... USING unnest(...)`, in batches of `kb.load_batch` quads. They are WAL-logged, replicated, locked, checked and triggered as any write; two transactions that insert one quad meet at the unique index, as two INSERTs do. Logical replication carries them too: kb.quads' replica identity is its unique index, `quads_gspo` -- the four keys, `REPLICA IDENTITY USING INDEX` -- without which a publication of kb.quads would refuse its deletes, and a subscriber would not find the rows they delete; kb.terms, kb.graphs and kb.mappings have primary keys. A subscriber creates the extension, and subscribes to those four tables and to the mapped ones (`test/regress/sql/replication.sql`). A deleted quad leaves its terms in the dictionary, as oxigraph's store does, until `kb.vacuum_terms()` deletes the rows no quad uses.

### 3.4 Privileges and row-level security

The store's privileges are its tables': SELECT on `kb.quads`, `kb.terms`, `kb.graphs` and `kb.mappings` to query; INSERT and DELETE on the first three to update and load. Row-level security on `kb.quads` is refused at the first scan: the engine's scans bypass the executor's policies. R2RML's tables keep theirs (§4.3).

### 3.5 A table access method?

The question asked was whether oxigraph's storage should be a table access method. It should not, now.

**What the store needs** is ordered permutations of quads, read by prefix ranges -- what oxigraph keeps in RocksDB's column families -- and a dictionary. PostgreSQL's btree indexes are such permutations, and index-only scans read them without the heap. The engine drives the scans itself (§3.2): SPARQL never goes through PostgreSQL's executor, so a table access method would bring the planner nothing; it would change only the bytes on disk and how SQL reads `kb.quads`.

**Where the bytes are.** At 3,577,116 quads of the bench's dataset (§6), all in the default graph:

| | bytes a quad |
|---|---:|
| `kb.quads`, the heap: a 64-byte row, a 24-byte tuple header, a 4-byte line pointer | 93 |
| three btree indexes (`gspo`, `gpos`, `gosp`): a 64-byte key, an 8-byte index tuple header, a line pointer, leaves about 70% full after inserts in hash order | 3 × 113 |
| `kb.terms` and its primary key | 40 |
| the store | 473 |
| the store after a bulk load (§2.6), its indexes built rather than grown | 392 |
| oxigraph's RocksDB directory, the same triples | 95 |

The heap is a fifth of it; the indexes, nearly three quarters. RocksDB's advantage is prefix compression of sorted keys -- consecutive keys share their first terms -- and LZ4 over its blocks; PostgreSQL's btree has neither (deduplication merges only equal keys). A table access method replaces the heap and leaves the indexes as they are.

**What a table access method would cost.** Heap gives the store, for nothing: MVCC visibility and the visibility map that the index-only scans use, WAL and crash recovery, physical and logical replication, `pg_basebackup`, `pg_upgrade`, VACUUM, locks, constraints, triggers and `pg_dump`. A table access method must provide each of them itself -- WAL through generic WAL records or a custom resource manager that every standby must load. lance_tam's experience is the measure: its datasets' files are not WAL-logged, and a physical standby has no copy of a local one.

**What closes the gap instead**, from the cheapest:

1. **Indexes built rather than grown** -- built: `bulk => true` (§2.6). A btree built by `CREATE INDEX` fills its leaves to 90%: after a bulk load of the same 3,577,116 quads each index takes 86 bytes a quad instead of 113, and the store 392 bytes a quad instead of 473; the load took 19.1 s instead of 48.2, and wrote 1.45 GB of WAL instead of 2.63 (§6). `REINDEX TABLE kb.quads` after a large load gives the same sizes.
2. **Narrower keys.** Eight-byte term ids from a dictionary halve the rows and the index tuples, at the price of a dictionary lookup for every term a load writes and a query names, and of the keys R2RML's terms get without one (§3.1). A 64-bit hash collides too often at a billion terms to replace the dictionary.
3. **An index access method with prefix-compressed leaves** over `(g, s, p, o)` and its permutations, heap kept for MVCC: the three indexes from 3 × 113 bytes a quad to perhaps 3 × 30, the store to about half. WAL-logged by generic WAL records, its pages PostgreSQL's.
4. **A table access method that owns the permutations** -- an LSM of sorted, compressed runs inside PostgreSQL, the engine reading it as it reads the indexes now -- reaching RocksDB's density, at the price of everything heap gives for nothing, above.

The store's access is behind the `KbHost` callbacks (`quads_open`, `quads_next`, `quads_close`, the writes), so any of these replaces `src/store.c`'s side alone. And density, not speed, is the gap: through shared buffers the store's reads were as fast as oxigraph's server's or faster (§6); loads are the other gap, which a bulk path, not a table access method, closes.

## 4. R2RML

### 4.1 Mappings

A mapping is an R2RML document in Turtle, stored in `kb.mappings (name, document, base_iri, enabled, revision, summary, created)` by `kb.add_mapping()`, which checks it first (`kb.check_mapping()`): parsed, checked as R2RML's conformance asks -- one logical table and one subject map a triples map, a term map's one of `rr:constant`, `rr:column`, `rr:template`, term types, language tags, datatypes, join conditions, `rr:sqlVersion` (`engine/src/r2rml/parse.rs`) -- and compiled against its tables (§4.2). The check returns a JSON summary: its logical tables' SQL, its predicates, its graphs. `kb.enable_mapping()` and `kb.drop_mapping()` turn it off and remove it; `replace => true` replaces one. Each change takes a new revision, and a backend compiles a mapping once a revision.

The mapping's base IRI is `base_iri`, or the document's own `@base`; a template or a column that makes a relative IRI is resolved against it. Without either, the names a mapping gives its own resources -- `<#TriplesMap1>`, as R2RML's examples write them -- still resolve, but a relative IRI it gives a triple -- a constant, a class, a datatype -- is refused, and a relative IRI a row makes is a data error, as R2RML calls it, that fails the query.

Only roles that may write `kb.mappings` add mappings -- by default, the extension's owner. A mapping's SQL runs with the privileges of whoever queries (§4.3), so its author can make every querying role run what its `rr:sqlQuery` calls, as a view's author can: writing `kb.mappings` is for trusted roles.

### 4.2 Compilation

A mapping is compiled against its tables (`engine/src/r2rml/compile.rs`): each logical table -- `rr:tableName`, or `rr:sqlQuery` as a subquery -- is described by preparing its SQL (`sql_describe`); each column a term map names is found among its result columns as PostgreSQL resolves an identifier (a delimited `"Name"` exactly, any other folded to lower case); and the mapping is cut into units, a unit one triple template -- a subject, a predicate and an object over one row, or over a row and its parent's row for a referencing object map -- with the graphs its triples go to. A logical table whose SQL writes or locks rows -- `DELETE ... RETURNING`, a data-modifying WITH, `FOR UPDATE` -- is refused, as are column names it repeats.

A column's value makes its term as R2RML says (§10, §11 of the recommendation): a template's values IRI-safe, percent-encoded, for an IRI; a literal's natural RDF datatype from its SQL type, its lexical form canonical (`src/sql.c`, `engine/src/r2rml/eval.rs`):

| SQL type | Natural datatype |
|---|---|
| `smallint`, `integer`, `bigint` | `xsd:integer` |
| `numeric` | `xsd:decimal` |
| `real`, `double precision` | `xsd:double` |
| `boolean` | `xsd:boolean` |
| `date` | `xsd:date` |
| `time`, `timetz` | `xsd:time` |
| `timestamp`, `timestamptz` (in UTC) | `xsd:dateTime` |
| `bytea` | `xsd:hexBinary` |
| any other | a plain literal of its text |

A value's text does not depend on the session's settings -- DateStyle, TimeZone, IntervalStyle, `bytea_output`, `extra_float_digits` -- so a row makes the same terms in every session. A blank node of a mapping is a key from the mapping's name and its label, the same in every query.

`kb.direct_mapping(tables, base_iri)` writes the W3C Direct Mapping of tables as an R2RML document (`engine/src/r2rml/direct.rs`): a row an IRI from its table and its primary key's values, or a blank node from its `tableoid` and `ctid` without one; its table its class; a column a predicate; a foreign key a reference to the row it names.

### 4.3 Evaluation

For a triple pattern, each unit of each enabled mapping that can make a quad of it -- its constant predicate the pattern's, or a predicate made from a column -- gives one SQL query (`engine/src/r2rml/eval.rs`): the columns its terms read, of its logical table or of its table joined to its parent's, the rows without NULL in them, and the conditions the pattern's bound terms give. A bound IRI is matched against a template and gives its columns' values (`http://ex/emp/{id}` and `<http://ex/emp/7>` give `id = '7'`); a bound literal gives its column's value, cast to the column's type so that the table's indexes serve; a value that does not cast, an IRI that does not match the template, gives no query at all. The conditions only narrow: each row's terms are made as R2RML makes them and kept only when their keys are the pattern's, so a condition that cannot be derived -- a template whose separators a value may hold -- is left out, never guessed. Plans are kept per backend by their SQL's text (`src/sql.c`), so a pattern probed once for each binding of a nested loop is planned once.

The SQL runs through SPI as the querying role, read only, in the caller's snapshot: the tables' privileges, column privileges and row-level security apply as to any query of theirs. A quad that the store and a mapping, or two mappings, make is in the dataset once. The graph a unit's triples go to is a named graph of the dataset; `GRAPH ?g {}` lists the mappings' graphs beside the store's. SPARQL updates never write the tables.

### 4.4 What is not pushed down

Each pattern is its own SQL query: joins between patterns, FILTERs other than the bound terms, aggregates, ORDER BY and LIMIT are spareval's, over the rows the queries return. A pattern with an unbound subject and object reads its whole table. Ontop rewrites a whole SPARQL query into one SQL query; this design keeps oxigraph's evaluator and the store in one dataset instead, and a basic graph pattern's patterns over one logical table, joined in one SQL query -- Ontop's self-join elimination -- is the next step. Not built.

## 5. The endpoint

The endpoint is vexec_flight's (`pg_accel/modules/vexec_flight`) with HTTP/1.1 in place of HTTP/2 and gRPC: the acceptor and the login check are its, the names changed.

### 5.1 The acceptor

The acceptor (`src/endpoint/acceptor.c`) is a background worker without threads that listens on `kb.endpoint_listen_addresses` and `kb.endpoint_port`. It runs whenever the module is preloaded, and a reload switches the endpoint (§2.7): between two turns of its loop, outside its wait set, it closes the sockets of the list before and listens on the new one -- on none, the list empty, and the endpoint is off -- binding the same port again (`ListenServerPort()` sets `SO_REUSEADDR`). It never exits for that: a worker that exits with 0 is unregistered, and could not listen again (PG19:`doc/src/sgml/bgworker.sgml:253-261`). Off, it goes on checking logins and handing sockets over, for the connections it took before. For each connection it reserves a slot of the session table, asks the postmaster for a session worker (`RegisterDynamicBackgroundWorker()`) with the slot and a token, and hands the worker the socket over a UNIX-domain socket (`SCM_RIGHTS`) once the worker shows the token. The session then owns the socket as a backend does. A connection that cannot have a session -- `kb.endpoint_max_sessions` are open, or no worker slot is free -- is answered 503, and its socket kept open a moment so that the client reads the answer.

### 5.2 Logins and sessions

The first request of a connection carries its credentials: HTTP's basic authentication, or none when `kb.endpoint_anonymous_role` names a role. The database is the path's first segment: `/<database>/query` (§5.3). The acceptor checks the login as PostgreSQL checks a backend's (`login.c`, `hba.c`): `pg_hba.conf`'s lines matched with the client's address, its TLS, its database and role, by a copy of PostgreSQL's matching code over the acceptor's own copy of the lines; the password against the stored secret (`trust`, `reject`, `password`, `md5` and `scram-sha-256` lines; a `scram-sha-256` line takes only a SCRAM secret); `rolcanlogin`, `rolvaliduntil`, `datallowconn`, CONNECT, the role's and the database's connection limits. A refused login is answered 401 -- with a challenge where credentials may help -- or 403, and the connection stays open. The session then connects to that role and database (`BackgroundWorkerInitializeConnectionByOid()`), with `MyProcPort` describing its client, so that `pg_stat_activity` shows the client's address and `pg_stat_ssl` its TLS. A later request on the connection must name the same database and user: another is answered 421, and the connection closed for the client to ask on a new one.

Each request is a transaction of its own (`src/endpoint/session.c`), read only for a query or a read of the graph store: permissions, row-level security on R2RML's tables and settings -- a role's and a database's, `statement_timeout` among them -- behave as for a backend's statement, and `pg_stat_activity` shows the request's query. The transaction commits before the response ends: an update the client sees answered is committed. Where pg_otel_probe is loaded, the request runs as a statement of its own, which the probe traces, a child of the caller's span (§2.8). `pg_cancel_backend()` ends the request under way; `pg_terminate_backend()` ends the session; a connection idle for `kb.endpoint_idle_session_timeout` closes. **Switched off** by a reload, the session starts no request: it reads the reload between requests -- one that comes while a request is served waits for the request's end, which is the request's as it would have been -- and closes its connection, at once when the connection is idle; a connection the acceptor took before the switch, whose session starts after it, has its first request answered 503. A session's TLS (`tls.c`) is an OpenSSL context of its own, from `kb.endpoint_ssl_cert_file` and `kb.endpoint_ssl_key_file`, that selects ALPN `http/1.1`: PostgreSQL's own context refuses clients that do not offer PostgreSQL's ALPN name.

### 5.3 HTTP and the protocol

HTTP/1.1 (`src/endpoint/http.c`): keep-alive, pipelined requests, bodies whole or chunked, `Expect: 100-continue`, bodies up to `kb.endpoint_max_body`. A response's head waits for the first 64 kB of its body: a result that fits goes out with its `Content-Length`, and an error found before then -- most are found at once -- is answered with its own status; past them the body goes out chunked, and an error can only cut the connection before the last chunk.

What a request asks and its answer are oxigraph's server's (`engine/src/http.rs`, from `cli/src/main.rs`), its paths below a database's name, percent-encoded (`src/endpoint/session.c`, `split_path()`, `answer_path()`):

| Path | Methods | |
|---|---|---|
| `/<database>/` | GET, HEAD | a page with a query form |
| `/<database>/query` | GET with `query`, POST (`application/sparql-query` or a form), QUERY | a query; GET without `query`: the service description |
| `/<database>/update` | POST (`application/sparql-update` or a form) | an update; GET: the service description |
| `/<database>/sparql` | GET, POST, QUERY | a query or an update |
| `/<database>/store`, `?default`, `?graph=<iri>`, `/<database>/store/...` | GET, HEAD, PUT, POST, DELETE | the graph store protocol; a path under `/<database>/store/` names its graph directly |

**No path without a database.** The first segment is always a database's name, so that a URL names its database and nothing else does; a database may have any name, `query` and `store` too. `/` is answered 404 with the paths' form, and a client of oxigraph's server that asks `/query` asks the database `query`. `/<database>` without the slash is sent to `/<database>/` (308), where the page's relative links are the database's; a path below a database's that is none of these is answered 404 before any login; a name longer than a database's may be, 400. The request's URL, prefix and all, is the base of its relative IRIs and the name of a graph the graph store names directly: a graph `POST /<database>/store` makes is `<.../<database>/store/<id>>`, its `Location` the database's.

The parameters are the protocol's -- `default-graph-uri`, `named-graph-uri`, `using-graph-uri`, `using-named-graph-uri` -- and oxigraph's `union-default-graph`; the content negotiation and the formats are oxigraph's. `kb.endpoint_read_only` refuses updates and the graph store's writes with 403.

An error's status is the engine's for its own errors (400 for a syntax error, 404 for a graph that is not there, 406, 415, 503 past the memory limit) and, for PostgreSQL's, its SQLSTATE's: 403 for a privilege or a read-only transaction, 409 for serialization failures, deadlocks and constraints, 503 for a cancel, a shutdown or insufficient resources, 400 for data and syntax errors, 500 otherwise. The body is the error's message, detail and hint, and the `X-SQLSTATE` header its code.

## 6. Measurements

`test/run.sh bench` (`test/bench/`): a dataset in the shape of the Berlin SPARQL Benchmark's e-commerce data -- products, their types in a hierarchy, features, producers, offers, vendors, reviews, reviewers -- made from a seed, loaded into the store by `kb.load_file()` and into oxigraph's own store by its CLI's bulk loader (RocksDB, the same commit), then 14 queries through oxigraph's server (`serve-read-only`), through the store's endpoint, and through SQL (`kb.sparql_text()`), the same client on kept-alive connections; each answer compared across the three, so that no time is of a wrong answer.

One run on the build machine (32 CPUs, otherwise idle: load average 0.5), 2026-10-19: 50,000 products, 3,577,116 triples, 475 MB of N-Triples; the median of 5 runs after a warm-up (`~/.cache/pg_accel/pg_knowledge_base/runs/20261019T075837-bench`), the engine and oxigraph's CLI built from oxigraph's `1bf3a924c9f`, before the move to `f0a06d83d90` and its join reordering (§9). The server, the one of that time: `REL_19_STABLE` at `0f433a6b357`, built with assertions. It has `shared_buffers = 4GB`, which holds the whole store; oxigraph's has its defaults, which its CLI does not change: RocksDB's 32 MB block cache, over blocks of 4 kB compressed by LZ4.

| | pg_knowledge_base | oxigraph (RocksDB) |
|---|---:|---:|
| load | 43.8 s (81,600 quads/s), one transaction, then VACUUM (ANALYZE) 0.3 s | 1.5 s (2,409,000 triples/s), the bulk loader's 16 threads |
| bulk load (`bulk => true`, §2.6), in a database of its own (`runs/20261019T081104-bench`) | 19.1 s (187,700 quads/s), the same 3,577,116 quads; then VACUUM (ANALYZE) 0.3 s | -- |
| on disk | 1,613 MB, 473 B a quad, and 2,627 MB of WAL written | 324 MB, 95 B a triple |
| on disk after a bulk load | 1,338 MB, 392 B a quad, and 1,453 MB of WAL written | -- |

| Query | | oxigraph, ms | endpoint, ms | SQL, ms | Answer |
|---|---|---:|---:|---:|---:|
| lookup | one product's triples | 0.54 | 0.65 | 0.14 | 17 |
| star | products with a feature, a filter, sorted, 10 | 14.9 | 2.81 | 2.58 | 10 |
| reviews | a product's reviews, their reviewers, a rating | 0.50 | 0.30 | 0.13 | 2 |
| offers | a product's cheapest offers, their vendors' countries | 0.52 | 0.29 | 0.14 | 4 |
| recent | a product's reviews by date, 5 | 0.33 | 0.28 | 0.12 | 2 |
| ask | a product's feature, by its label | 0.27 | 0.25 | 0.10 | true |
| describe | DESCRIBE a product | 0.45 | 0.33 | 0.09 | 17 |
| path | products of a type and its subtypes (`subClassOf*`) | 3.17 | 1.89 | 1.51 | 1 |
| group-offers | every offer, by its vendor's country | 9,764 | 1,174 | 1,203 | 20 |
| group-reviews | every review, by its reviewer's country | 1,347 | 239 | 237 | 20 |
| range | offers of a month under 100, by dateTime and decimal | 267 | 66 | 66 | 1 |
| text | labels that contain a word | 31.1 | 12.0 | 10.4 | 286 |
| count | COUNT(*) of the default graph | 678 | 477 | 481 | 1 |
| predicates | DISTINCT predicates, a full scan | 857 | 655 | 643 | 33 |
| connect | `ASK {}` on a new connection | 1.15 | 2.81 | 2.21 | true |

- **Reads.** Both sides evaluate with spareval; the difference is the dataset's: a lookup in btree pages that shared buffers hold, against a RocksDB seek that decompresses a block from outside its cache. Point queries take a few tenths of a millisecond at either server and about a tenth through SQL. The queries whose plans are nested loops of lookups (`Lateral` in `kb.sparql_explain()`) -- the star, the two aggregates, the range -- are 4 to 8 times faster on the store; the full scans 1.3 to 1.4 times. With a block cache that held its 324 MB, oxigraph would answer faster than here (*unverified*: its CLI has no setting for one).
- **A new connection** costs the endpoint 2.8 ms, a background worker started and connected, as a backend's fork and connection do (2.2 ms through libpq here); oxigraph's server, a thread, 1.2 ms. On a kept-alive connection the endpoint answers within a few tenths of a millisecond of SQL.
- **Loads** are the store's weak side: one transaction in one backend, three index insertions a quad and 2.6 GB of WAL, 30 times slower than oxigraph's bulk loader writing sorted files on 16 threads. A bulk load (§2.6) is 2.5 times faster, with 45% less WAL and a sixth less disk; oxigraph's is still 11 times faster.
- **Density**: 473 bytes a quad against 95, five times (§3.5).

An earlier run at 5,000 products (360,031 triples, `runs/20261019T071517-bench`), while other work ran on the machine, gave the same sizes a quad (475 and 101) and times of the same order.

## 7. Tests and containers

`test/run.sh` drives every leg in a container of the compose project `docker/compose.yml`; results go to `~/.cache/pg_accel/pg_knowledge_base/runs/<time>-<leg>`.

| Leg | | Result, 2026-10-23 |
|---|---|---|
| `images` | `pg_knowledge_base/engine-builder:rust1.99` (Rust on trixie, clang for RocksDB's bindings) and `pg_knowledge_base/dev:19rc1-trixie` (the official `postgres:19rc1-trixie`, with PGDG's `postgresql-server-dev-19` of the server's version, OpenSSL, GSSAPI's headers, Python's clients: requests, rdflib, SPARQLWrapper, psycopg, pyarrow; postgres with the host user's uid); the legs build them when they are not there | |
| `fetch`, `engine` | cargo's fetch, in the builder's container with the network: the crates, and oxigraph's repository at the commit with its submodules (625 MB in cargo's cache, once), `engine/Cargo.lock` taken into the tree when cargo resolves it again; then the engine library built offline from the lockfile as it is (`--locked`) into `~/.cache/pg_accel/pg_knowledge_base/engine/lib`. `engine`, `enginetest` and `oxigraph` fetch first when the cache lacks what the manifest and the lockfile name | passes, no warnings |
| `enginetest` | the engine's unit tests: keys, templates, traces' texts and plans | 8 of 8 |
| `checks` | the ABI's two declarations agree (`checks/abi.py`), and no file of the tree has Cyrillic text | passes |
| `suite` | the regression suite, `test/regress`: store, sparql, update, r2rml, security, bulk, replication -- the server with `wal_level = logical` --; then `test/nopreload`, the server restarted without the module preloaded | 7 of 7, then 1 of 1 |
| `r2rml` | the W3C RDB2RDF test cases, Ontop's copy at its pinned commit fetched from GitHub, each in a database of its own, the graphs compared by isomorphism (rdflib) | 95 of 95 |
| `sparql` | the W3C SPARQL 1.1 query and update test cases at the commit of rdf-tests oxigraph's testsuite pins, results compared by isomorphism | 990 pass, 0 fail, 11 differ as expected (§3.1), 6 skipped ([sparql-conformance.md](sparql-conformance.md)) |
| `endpoint` | the endpoint from a local and a remote client: the protocol, the graph store, a second database's paths, logins by `pg_hba.conf` line, TLS, sessions, cancels, terminations, switched off and on by reloads -- an idle connection closed, a request being served ended whole, a list of bad syntax refused, an address that cannot be listened on -- then the limits' settings, then the server started with the endpoint off and switched on by a reload | 97 of 97, then 8 of 8, then 4 of 4 |
| `oxigraph` | oxigraph's CLI and server with RocksDB, built offline from cargo's checkout of the engine's commit; `bench` builds them again when they are of another commit | builds |
| `otel-agent`, `otel` | pg_otel_probe's agent, then the probe itself, from the pinned commit of its repository, preloaded beside the module; SPARQL through SQL and the endpoint, and the spans read back from the agent's Arrow IPC files (`test/otel/otel_checks.py`); the only legs that need the probe's repository; they run only when named, or by `all` with `KB_OTEL=1` | 25 of 25 |
| `bench` | §6 | §6 |
| `repro`, `shell` | a scratch script on a server of its own; a shell, the server running | |

**On GitHub's runners**, `.github/workflows/build.yml` runs `images`, `all` and `enginetest`, with cargo's caches kept between runs, and keeps the legs' logs. The default build reads no local checkout and no private image: Rust's and PostgreSQL 19's official images, PGDG's and Debian's packages, oxigraph's crates, w3c/rdf-tests and Ontop's test cases from GitHub at pinned commits, the clients from PyPI. Run as a runner would -- a Docker daemon of its own without images or build cache, a fresh clone and home, uid 1001, 4 CPUs and 16 GB in all -- the three passed in 425 s on 2026-10-23: the images in 184 s, the legs in 228 s with cargo's fetch (695 MB) and the engine built from nothing, the engine's tests in 13 s; the runner held at most 6.8 GB of memory, the page cache included, and 5.8 GB of disk. Once PGDG's repository has moved past the server's version, `postgresql-server-dev-19` comes from PGDG's archive, the CA certificates installed first: on a base whose PGDG source no longer has the version, the dev image built so and the legs passed.

## 8. Limits

- **Literals are values** (§3.1): the lexical forms of the datatypes oxigraph knows are not kept.
- **A cancel or the memory limit is seen at the next row, quad or callback.** spareval's loops over what it has materialized call nothing in between: a cartesian product of large inputs under a sort holds the cancel for one pass over its inner side, and the memory past the limit for what that pass allocates (§2.2).
- **Deduplication holds memory.** A pattern that both the store and a mapping can answer, and a union default graph, keep the keys of the quads they have given until the pattern's end.
- `kb.sparql_json()` and `kb.sparql_text()` build their whole value: at most 1 GB; `kb.sparql()` and the endpoint stream.
- **R2RML is read pattern by pattern** (§4.4), and SPARQL updates do not write the mapped tables.
- **SERVICE is not served**: the backend makes no requests of other servers.
- **No statistics.** sparopt at this commit orders joins by costs that a dataset's estimates feed, through spareval's `QueryableDataset` (a pattern's quad count; a predicate's triples, distinct subjects and objects); the store gives none yet, and the optimizer falls back to its heuristics, the query's shape.
- **The endpoint** speaks HTTP/1.1 only; its passwords travel in HTTP's basic authentication, in the clear without TLS; a connection is one database and one role; each connection is a background worker.
- **Row-level security on `kb.quads`** is refused (§3.4).
- Each backend has its own term cache and its own compiled mappings and plans.
- **A bulk load locks kb.quads** for the rest of its transaction, and builds the indexes over the whole store (§2.6).
- **A SPARQL plan is a span's attribute, not spans of its own** (§2.8): the probe offers extensions attributes, not spans; and a statement's several calls keep the longest one's text and plan.

## 9. Decisions

Decided (2026-10-19 and 2026-10-23), within the request and by the user's own:

- **The endpoint's paths, parameters and answers are oxigraph's server's, below a database's name, and there is no path without one**: `/<database>/query` (decided by the user, 2026-10-19).
- **Literals as values, the default graph the store's own, CLEAR keeping named graphs**: oxigraph's store's answers (§1.6).
- **The store is heap and btree, not a table access method** (§3.5).
- **A bulk load mode**, kb.quads' indexes dropped and built again around a load (§2.6), asked for by the user.
- **kb.quads' replica identity is its unique index** (asked for by the user, 2026-10-23): `REPLICA IDENTITY USING INDEX quads_gspo`, the four keys, so that logical replication carries kb.quads' deletes (§3.3). Not a primary key, whose index a bulk load would keep up quad by quad instead of building it (§2.6), nor `FULL`, which PostgreSQL's documentation keeps as a fallback for tables without a key; a bulk load gives the identity back to the index it builds again.
- **Traces through pg_otel_probe's published API alone** (§2.8), asked for by the user: nothing of the probe changed.
- **A reload switches the endpoint on and off**, through `kb.endpoint_listen_addresses` alone, with no setting of its own (decided by the user, 2026-10-19): the acceptor runs whenever the module is preloaded; a session switched off ends the request it serves, then closes (§2.7, §5.1, §5.2).
- **The default build from public sources, on GitHub's runners** (asked for by the user, 2026-10-19): PostgreSQL 19's official image in place of pg_accel's, the test cases fetched from GitHub; the otel legs, which need pg_opentelemetry_probe, run only when named. The tree's text is English, which `checks` keeps.
- **oxigraph from the user's fork, fetched by cargo**: `igor-suhorukov/oxigraph` at `f0a06d83d90` (decided by the user, 2026-10-19), in place of a snapshot of a local checkout. oxigraph's published crates, 0.5.12 on crates.io, are older than the API the engine is written against -- oxstr's `OxString` in oxrdf's and spareval's signatures, among others -- so the move to them waits for 0.6: the seven `rev`s of `engine/Cargo.toml` replaced by a `version`.

Left to the user:

1. **R2RML pushdown beyond a pattern** (§4.4): a basic graph pattern over one logical table as one SQL query, then joins, FILTERs and aggregates, as Ontop rewrites them.
2. **The store's density** (§3.5): after the bulk mode, narrower keys or an index access method with prefix-compressed leaves; a table access method only if those do not suffice.
3. **SERVICE**: federated queries from the backend, through an HTTP client the engine would have to carry.
4. **A SPARQL plan's nodes as spans** of their own, as the probe makes PostgreSQL's: an API for an extension's own spans, which the probe would have to offer.
5. **W3C SPARQL conformance to 1007 of 1007** ([sparql-conformance.md](sparql-conformance.md)): the runner's lax cardinality and CSV tests, a store mode that keeps literals' lexical forms, two fixes of spareval's in oxigraph.
6. **The store's statistics for sparopt's join order** (§8): `KbDataset`'s estimates (`engine/src/dataset.rs`) -- a pattern's quads counted in an index up to a cap, a predicate's triples and distinct subjects and objects from `pg_statistic` or a summary of its own -- for the costs the fork's optimizer computes.
7. **A remote.** The tree is a git repository since 2026-10-19, with no remote: `.github/workflows/build.yml` runs once it is pushed to GitHub.
