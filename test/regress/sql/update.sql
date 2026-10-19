-- SPARQL 1.1 Update (docs/design.md §2.5), in the caller's transaction
SELECT kb.sparql_update('CLEAR ALL');

SELECT kb.sparql_update($$
PREFIX ex: <http://example.com/>
INSERT DATA {
  ex:a ex:v 1 ; ex:name "a" .
  ex:b ex:v 2 ; ex:name "b" .
  GRAPH ex:g1 { ex:c ex:v 3 }
}
$$);
SELECT subject, predicate, object, graph FROM kb.triples ORDER BY graph NULLS FIRST, subject, predicate;

-- DELETE/INSERT: the WHERE read before the writes
SELECT kb.sparql_update($$
PREFIX ex: <http://example.com/>
DELETE { ?s ex:v ?v } INSERT { ?s ex:v ?w } WHERE { ?s ex:v ?v BIND(?v * 10 AS ?w) }
$$);
SELECT * FROM kb.sparql('PREFIX ex: <http://example.com/> SELECT ?s ?v WHERE { ?s ex:v ?v } ORDER BY ?s') AS t(s text, v int);
-- WITH and USING
SELECT kb.sparql_update($$
PREFIX ex: <http://example.com/>
WITH ex:g1 INSERT { ?s ex:copied true } WHERE { ?s ex:v ?v }
$$);
SELECT kb.sparql_update($$
PREFIX ex: <http://example.com/>
INSERT { GRAPH ex:g2 { ?s ex:from-default ?v } } USING ex:g1 WHERE { ?s ex:v ?v }
$$);
SELECT subject, predicate, object, graph FROM kb.triples WHERE graph IS NOT NULL ORDER BY graph, subject, predicate;
-- the protocol's using graphs
SELECT kb.sparql_update('PREFIX ex: <http://example.com/> INSERT { ?s ex:seen 1 } WHERE { ?s ex:v ?v }',
                        NULL, ARRAY['http://example.com/g1']);
SELECT * FROM kb.sparql('PREFIX ex: <http://example.com/> SELECT ?s WHERE { ?s ex:seen 1 }') AS t(s text);

-- blank nodes of INSERT DATA: fresh ones, one for each label of the request
SELECT kb.sparql_update('INSERT DATA { _:x <http://example.com/p> 1 . _:x <http://example.com/q> 2 . _:y <http://example.com/p> 3 }');
SELECT * FROM kb.sparql('SELECT (COUNT(DISTINCT ?b) AS ?n) WHERE { ?b <http://example.com/p> ?v }') AS t(n int);
SELECT * FROM kb.sparql('SELECT ?v ?w WHERE { ?b <http://example.com/p> ?v ; <http://example.com/q> ?w }') AS t(v int, w int);

-- DELETE DATA, DELETE WHERE
SELECT kb.sparql_update('PREFIX ex: <http://example.com/> DELETE DATA { ex:a ex:name "a" }');
SELECT kb.sparql_update('PREFIX ex: <http://example.com/> DELETE WHERE { ?s ex:name ?n }');
SELECT count(*) FROM kb.sparql('SELECT ?s WHERE { ?s <http://example.com/name> ?n }') AS t(s text);

-- graphs: CREATE, ADD, COPY, MOVE, CLEAR, DROP
SELECT kb.sparql_update('CREATE GRAPH <http://example.com/empty>');
SELECT * FROM kb.sparql('SELECT ?g WHERE { GRAPH ?g { } } ORDER BY ?g') AS t(g text);
SELECT kb.sparql_update('CREATE GRAPH <http://example.com/empty>');
SELECT kb.sparql_update('CREATE SILENT GRAPH <http://example.com/empty>');
SELECT kb.sparql_update('COPY <http://example.com/g1> TO <http://example.com/g3>');
SELECT kb.sparql_update('ADD <http://example.com/g2> TO <http://example.com/g3>');
SELECT kb.sparql_update('MOVE <http://example.com/g3> TO <http://example.com/g4>');
SELECT graph, count(*) FROM kb.triples WHERE graph IS NOT NULL GROUP BY graph ORDER BY graph;
SELECT kb.sparql_update('CLEAR GRAPH <http://example.com/g4>');
SELECT kb.sparql_update('DROP GRAPH <http://example.com/empty>');
SELECT kb.sparql_update('DROP GRAPH <http://example.com/nothing>');
SELECT kb.sparql_update('DROP SILENT GRAPH <http://example.com/nothing>');
SELECT * FROM kb.sparql('SELECT ?g WHERE { GRAPH ?g { } } ORDER BY ?g') AS t(g text);
SELECT kb.sparql_update('CLEAR DEFAULT');
SELECT count(*) FROM kb.quads WHERE g = '00000000-0000-0000-0000-000000000000';
SELECT kb.sparql_update('DROP NAMED');
SELECT count(*) AS quads, (SELECT count(*) FROM kb.graphs) AS graphs FROM kb.quads;

-- an update is the caller's transaction's: a later error takes it back
SELECT kb.sparql_update('INSERT DATA { <http://example.com/x> <http://example.com/p> 1 }');
BEGIN;
SELECT kb.sparql_update('INSERT DATA { <http://example.com/y> <http://example.com/p> 2 }');
SAVEPOINT s;
SELECT kb.sparql_update('INSERT DATA { <http://example.com/z> <http://example.com/p> 3 }');
ROLLBACK TO SAVEPOINT s;
SELECT * FROM kb.sparql('SELECT ?s WHERE { ?s <http://example.com/p> ?v } ORDER BY ?s') AS t(s text);
COMMIT;
SELECT kb.sparql_update('INSERT DATA { <http://example.com/w> <http://example.com/p> 4 } ; DROP GRAPH <http://example.com/none>');
SELECT * FROM kb.sparql('SELECT ?s WHERE { ?s <http://example.com/p> ?v } ORDER BY ?s') AS t(s text);

-- a read-only transaction writes nothing
BEGIN TRANSACTION READ ONLY;
SELECT kb.sparql_update('INSERT DATA { <http://example.com/r> <http://example.com/p> 5 }');
ROLLBACK;
BEGIN TRANSACTION READ ONLY;
SELECT kb.load('<http://example.com/r> <http://example.com/p> "5" .', 'ntriples');
ROLLBACK;

-- LOAD reads file: IRIs of the server's, with pg_read_server_files' privileges
\! printf '<http://example.com/from-file> <http://example.com/p> "loaded" .\n' > /tmp/kb_load_test.nt
SELECT kb.sparql_update('LOAD <file:///tmp/kb_load_test.nt> INTO GRAPH <http://example.com/loaded>');
SELECT * FROM kb.sparql('SELECT ?o WHERE { GRAPH <http://example.com/loaded> { ?s ?p ?o } }') AS t(o text);
-- LOAD takes a document's triples, not its named graphs, as oxigraph's does
\! printf '<http://example.com/q> <http://example.com/p> "in a graph" <http://example.com/named> .\n' > /tmp/kb_load_test.nq
SELECT kb.sparql_update('LOAD <file:///tmp/kb_load_test.nq> INTO GRAPH <http://example.com/loaded>');
SELECT kb.sparql_update('LOAD <http://example.com/remote.ttl>');
SELECT kb.sparql_update('LOAD SILENT <http://example.com/remote.ttl>');
SELECT kb.load_file('/tmp/kb_load_test.nt', NULL, 'http://example.com/file2');
SELECT count(*) FROM kb.sparql('SELECT ?s WHERE { GRAPH <http://example.com/file2> { ?s ?p ?o } }') AS t(s text);
CREATE ROLE kb_unprivileged;
GRANT USAGE ON SCHEMA kb TO kb_unprivileged;
SET ROLE kb_unprivileged;
SELECT kb.load_file('/tmp/kb_load_test.nt');
SELECT kb.sparql_update('LOAD <file:///tmp/kb_load_test.nt>');
RESET ROLE;
DROP OWNED BY kb_unprivileged;
DROP ROLE kb_unprivileged;
