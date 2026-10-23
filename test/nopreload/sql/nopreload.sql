-- The SQL functions on a server that does not preload the module
-- (docs/design.md §2.7): CREATE EXTENSION loads it into this backend,
-- which defines the settings a backend may, and leaves the postmaster's
-- undefined
SHOW shared_preload_libraries;
CREATE EXTENSION pg_knowledge_base;
SELECT name, context FROM pg_settings WHERE name LIKE 'kb.%' ORDER BY name;
SHOW kb.endpoint_port;
SELECT kb.load($$
<http://example.com/a> <http://example.com/p> "1" .
<http://example.com/b> <http://example.com/p> "a literal longer than fourteen bytes" .
$$, 'ntriples');
SELECT * FROM kb.sparql('SELECT ?s ?o WHERE { ?s <http://example.com/p> ?o } ORDER BY ?s') AS t(s text, o text);

-- a new backend loads it at its first call
\c
SELECT kb.sparql_update('INSERT DATA { <http://example.com/c> <http://example.com/p> 3 }');
SELECT kb.load('<http://example.com/d> <http://example.com/p> "4" .', 'ntriples', bulk => true);
SELECT * FROM kb.sparql('SELECT ?s ?o WHERE { ?s <http://example.com/p> ?o } ORDER BY ?s') AS t(s text, o text);
-- and a parallel worker, which loads the libraries its leader has loaded
SET debug_parallel_query = on;
SELECT count(*) FROM kb.quads;
RESET debug_parallel_query;

-- R2RML
CREATE TABLE emp (id int PRIMARY KEY, name text NOT NULL);
INSERT INTO emp VALUES (7, 'Alice');
SELECT kb.add_mapping('emp', $$
@prefix rr: <http://www.w3.org/ns/r2rml#> .
<#E> rr:logicalTable [ rr:tableName "emp" ] ;
  rr:subjectMap [ rr:template "http://example.com/emp/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate <http://example.com/name> ; rr:objectMap [ rr:column "name" ] ] .
$$) IS NOT NULL AS added;
SELECT * FROM kb.sparql('SELECT ?n WHERE { <http://example.com/emp/7> <http://example.com/name> ?n }') AS t(n text);

-- no endpoint runs: its requests' function refuses
SELECT kb.endpoint_request();
