-- Bulk loads (docs/design.md §2.6): kb.quads' indexes dropped, the quads
-- staged and put in once each, the indexes built again
SELECT kb.sparql_update('CLEAR ALL');
SELECT indexname, indexdef FROM pg_indexes WHERE schemaname = 'kb' AND tablename = 'quads' ORDER BY 1;
-- the replica identity: the unique index (docs/design.md §3.3)
SELECT c.relreplident, i.indexrelid::regclass AS replica_identity
FROM pg_class c LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisreplident
WHERE c.oid = 'kb.quads'::regclass;
-- an index of the administrator's is built again too
CREATE INDEX quads_extra ON kb.quads (o, s) WHERE g = '00000000-0000-0000-0000-000000000000';

-- a triple twice in the document, and one the store holds already: each once
SELECT kb.load('<http://example.com/held> <http://example.com/p> "1" .', 'ntriples');
SELECT kb.load($$
<http://example.com/a> <http://example.com/p> "1" .
<http://example.com/a> <http://example.com/p> "1" .
<http://example.com/held> <http://example.com/p> "1" .
<http://example.com/b> <http://example.com/p> "a literal longer than fourteen bytes" <http://example.com/g> .
$$, 'nquads', bulk => true);
SELECT subject, predicate, object, graph FROM kb.triples ORDER BY graph NULLS FIRST, subject;
SELECT count(*) AS named_graphs FROM kb.graphs;
-- the same indexes, valid, and queries read through them
SELECT indexname, indexdef FROM pg_indexes WHERE schemaname = 'kb' AND tablename = 'quads' ORDER BY 1;
SELECT count(*) FILTER (WHERE indisvalid AND indisready) AS valid, count(*) AS indexes
FROM pg_index WHERE indrelid = 'kb.quads'::regclass;
-- the replica identity given back to the unique index built again
SELECT c.relreplident, i.indexrelid::regclass AS replica_identity
FROM pg_class c LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisreplident
WHERE c.oid = 'kb.quads'::regclass;
SELECT * FROM kb.sparql('SELECT ?s WHERE { ?s <http://example.com/p> "1" } ORDER BY ?s') AS t(s text);
SELECT * FROM kb.sparql('SELECT ?o WHERE { GRAPH <http://example.com/g> { ?s ?p ?o } }') AS t(o text);
-- the unique index holds: a quad loaded again is not twice
SELECT kb.load('<http://example.com/a> <http://example.com/p> "1" .', 'ntriples');
SELECT count(*) FROM kb.quads;
-- a graph and a base, as for any load
SELECT kb.load('<x> <http://example.com/p> "2" .', 'turtle', 'http://example.com/g2', 'http://example.com/', bulk => true);
SELECT subject, object, graph FROM kb.triples WHERE graph = '<http://example.com/g2>';

-- the caller's transaction's: rolled back, the quads and the indexes are as they were
BEGIN;
SELECT kb.load('<http://example.com/c> <http://example.com/p> "3" .', 'ntriples', bulk => true);
SELECT count(*) FROM kb.quads;
ROLLBACK;
SELECT count(*) AS quads, (SELECT count(*) FROM pg_indexes WHERE schemaname = 'kb' AND tablename = 'quads') AS indexes
FROM kb.quads;
-- a syntax error loads nothing, and leaves the indexes
SELECT kb.load('<http://example.com/d> <http://example.com/p> .', 'ntriples', bulk => true);
SELECT count(*) AS quads, (SELECT count(*) FROM pg_indexes WHERE schemaname = 'kb' AND tablename = 'quads') AS indexes
FROM kb.quads;
-- two in one transaction
BEGIN;
SELECT kb.load('<http://example.com/e> <http://example.com/p> "5" .', 'ntriples', bulk => true);
SELECT kb.load('<http://example.com/f> <http://example.com/p> "6" .', 'ntriples', bulk => true);
COMMIT;
SELECT count(*) AS quads, (SELECT count(*) FROM pg_indexes WHERE schemaname = 'kb' AND tablename = 'quads') AS indexes
FROM kb.quads;

-- a server file
\! printf '<http://example.com/from-file> <http://example.com/p> "7" .\n<http://example.com/from-file> <http://example.com/p> "7" .\n' > /tmp/kb_bulk_test.nt
SELECT kb.load_file('/tmp/kb_bulk_test.nt', bulk => true);
SELECT count(*) FROM kb.quads WHERE s = kb.term_key('<http://example.com/from-file>');

-- only kb.quads' owner drops and builds its indexes; a read-only transaction loads nothing
CREATE ROLE kb_loader;
GRANT USAGE ON SCHEMA kb TO kb_loader;
GRANT SELECT, INSERT, DELETE ON kb.quads, kb.terms, kb.graphs TO kb_loader;
SET ROLE kb_loader;
SELECT kb.load('<http://example.com/g> <http://example.com/p> "8" .', 'ntriples', bulk => true);
SELECT kb.load('<http://example.com/g> <http://example.com/p> "8" .', 'ntriples');
RESET ROLE;
DROP OWNED BY kb_loader;
DROP ROLE kb_loader;
BEGIN TRANSACTION READ ONLY;
SELECT kb.load('<http://example.com/h> <http://example.com/p> "9" .', 'ntriples', bulk => true);
ROLLBACK;
DROP INDEX kb.quads_extra;
