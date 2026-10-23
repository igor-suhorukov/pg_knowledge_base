-- Logical replication (docs/design.md §3.3): kb.quads' replica identity
-- is its unique index, so a publication carries its deletes, and a
-- subscriber finds the rows they delete -- after a bulk load too, which
-- builds the index again.  The subscriber is a database of this cluster,
-- whose subscription takes a slot made beforehand, as one to its own
-- cluster must; the suite's server has wal_level = logical (suite.sh).
SELECT kb.sparql_update('CLEAR ALL');
SELECT kb.load($$
<http://example.com/a> <http://example.com/p> "1" .
<http://example.com/b> <http://example.com/p> "a literal longer than fourteen bytes" .
$$, 'ntriples');
CREATE PUBLICATION kb_pub FOR TABLE kb.quads, kb.terms, kb.graphs;
SELECT slot_name FROM pg_create_logical_replication_slot('kb_sub', 'pgoutput');
SELECT format('host=%s port=%s dbname=%s', current_setting('unix_socket_directories'),
              current_setting('port'), current_database()) AS conninfo \gset
\set publisher :DBNAME
CREATE DATABASE kb_replica;
\c kb_replica
CREATE EXTENSION pg_knowledge_base;
CREATE SUBSCRIPTION kb_sub CONNECTION :'conninfo' PUBLICATION kb_pub
    WITH (create_slot = false, slot_name = 'kb_sub');
-- the tables copied
DO $$
BEGIN
    FOR i IN 1..600 LOOP
        EXIT WHEN (SELECT count(*) FILTER (WHERE srsubstate = 'r') = 3 FROM pg_subscription_rel);
        PERFORM pg_sleep(0.1);
    END LOOP;
END $$;
SELECT * FROM kb.sparql('SELECT ?s ?o WHERE { ?s <http://example.com/p> ?o } ORDER BY ?s') AS t(s text, o text);

-- a bulk load, then a SPARQL update's delete and insert
\c :publisher
SELECT kb.load('<http://example.com/c> <http://example.com/p> "3" .', 'ntriples', bulk => true);
SELECT kb.sparql_update($$
DELETE DATA { <http://example.com/a> <http://example.com/p> "1" } ;
INSERT DATA { <http://example.com/d> <http://example.com/p> "4" }
$$);
\c kb_replica
DO $$
BEGIN
    FOR i IN 1..600 LOOP
        EXIT WHEN (SELECT result FROM kb.sparql('ASK { <http://example.com/d> <http://example.com/p> "4" }')
                   AS t(result boolean));
        PERFORM pg_sleep(0.1);
    END LOOP;
END $$;
SELECT * FROM kb.sparql('SELECT ?s ?o WHERE { ?s <http://example.com/p> ?o } ORDER BY ?s') AS t(s text, o text);

DROP SUBSCRIPTION kb_sub;
\c :publisher
DROP PUBLICATION kb_pub;
DROP DATABASE kb_replica WITH (FORCE);
SELECT kb.sparql_update('CLEAR ALL');
