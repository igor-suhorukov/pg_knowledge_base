-- Privileges, row-level security, limits (docs/design.md §3.4, §2.2)
SELECT kb.sparql_update('CLEAR ALL');
SELECT kb.load('<http://example.com/a> <http://example.com/p> 1 .');

-- the store's tables' privileges are SPARQL's
CREATE ROLE kb_nobody;
SET ROLE kb_nobody;
SELECT * FROM kb.sparql('SELECT * WHERE { ?s ?p ?o }') AS t(s text);
RESET ROLE;
GRANT USAGE ON SCHEMA kb TO kb_nobody;
GRANT SELECT ON kb.quads, kb.terms, kb.graphs, kb.mappings TO kb_nobody;
SET ROLE kb_nobody;
SELECT * FROM kb.sparql('SELECT ?s WHERE { ?s ?p ?o }') AS t(s text);
SELECT kb.sparql_update('INSERT DATA { <http://example.com/b> <http://example.com/p> 2 }');
SELECT kb.load('<http://example.com/b> <http://example.com/p> 2 .');
SELECT kb.add_mapping('m', '@prefix rr: <http://www.w3.org/ns/r2rml#> . <#T> rr:logicalTable [ rr:tableName "pg_class" ] ; rr:subjectMap [ rr:template "http://x/{oid}" ] .');
RESET ROLE;

-- the store's scans bypass the executor: row-level security on them is refused
ALTER TABLE kb.quads ENABLE ROW LEVEL SECURITY;
CREATE POLICY everything ON kb.quads USING (true);
SET ROLE kb_nobody;
SELECT * FROM kb.sparql('SELECT ?s WHERE { ?s ?p ?o }') AS t(s text);
RESET ROLE;
DROP POLICY everything ON kb.quads;
ALTER TABLE kb.quads DISABLE ROW LEVEL SECURITY;
DROP OWNED BY kb_nobody;
DROP ROLE kb_nobody;

-- the engine's memory: a query past kb.engine_memory_limit fails, and frees it
SELECT kb.sparql_update($$
INSERT { ?s <http://example.com/i> ?i } WHERE {
  VALUES ?a { 0 1 2 3 4 5 6 7 8 9 } VALUES ?b { 0 1 2 3 4 5 6 7 8 9 } VALUES ?c { 0 1 2 3 4 5 6 7 8 9 }
  BIND(?a * 100 + ?b * 10 + ?c AS ?i) BIND(IRI(CONCAT("http://example.com/n/", STR(?i))) AS ?s) }
$$);
SET kb.engine_memory_limit = '1MB';
SELECT count(*) FROM kb.sparql($$
SELECT ?x ?y WHERE { ?a <http://example.com/i> ?x . ?b <http://example.com/i> ?y } ORDER BY ?x ?y
$$) AS t(x int, y int);
-- its SQLSTATE is out_of_memory's
DO $do$
BEGIN
    PERFORM count(*) FROM kb.sparql($$
        SELECT ?x ?y WHERE { ?a <http://example.com/i> ?x . ?b <http://example.com/i> ?y } ORDER BY ?x ?y
    $$) AS t(x int, y int);
EXCEPTION WHEN out_of_memory THEN
    RAISE NOTICE 'out_of_memory (%)', SQLSTATE;
END
$do$;
RESET kb.engine_memory_limit;
SELECT count(*) FROM kb.sparql($$
SELECT ?x ?y WHERE { ?a <http://example.com/i> ?x . ?b <http://example.com/i> ?y FILTER(?x < 10) } ORDER BY ?x ?y
$$) AS t(x int, y int);

-- a cancel, or statement_timeout, ends a query inside the engine
SET statement_timeout = '100ms';
SELECT count(*) FROM kb.sparql($$
SELECT ?x WHERE { ?a <http://example.com/i> ?x . ?b <http://example.com/i> ?y . ?c <http://example.com/i> ?z }
$$) AS t(x int);
RESET statement_timeout;
