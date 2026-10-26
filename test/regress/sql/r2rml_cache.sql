-- R2RML's compiled mappings (docs/design.md §4.2): a backend keeps them while
-- their revisions and the catalog's version stay; with kb.r2rml_cache, the
-- files of pg_stat_tmp/pg_knowledge_base keep them between backends
SELECT kb.sparql_update('CLEAR ALL');
CREATE SCHEMA rc;
CREATE TABLE rc.item (iri text PRIMARY KEY, code int NOT NULL);
INSERT INTO rc.item VALUES ('http://example.com/i/1', 1), ('http://example.com/i/2', 2);
SELECT kb.add_mapping('rc', $$
@prefix rr: <http://www.w3.org/ns/r2rml#> .
<#I> rr:logicalTable [ rr:tableName "rc.item" ] ;
  rr:subjectMap [ rr:column "iri" ] ;
  rr:predicateObjectMap [ rr:predicate <http://example.com/code> ; rr:objectMap [ rr:column "code" ] ] .
$$) IS NOT NULL AS added;
-- the cache files of this database
CREATE FUNCTION rc_files() RETURNS bigint LANGUAGE sql AS $f$
    SELECT count(*) FROM pg_ls_dir('pg_stat_tmp/pg_knowledge_base/' ||
        (SELECT oid FROM pg_database WHERE datname = current_database()), true, false) AS f
    WHERE f LIKE '%.bin'
$f$;

-- a query compiles the mapping and writes its file, the files of mappings
-- dropped before removed
SELECT * FROM kb.sparql('SELECT ?s ?c (DATATYPE(?c) AS ?d) WHERE { ?s <http://example.com/code> ?c } ORDER BY ?s')
  AS t(s text, c text, d text);
SELECT rc_files();
-- a new backend reads it
\c
SELECT * FROM kb.sparql('SELECT ?s ?c (DATATYPE(?c) AS ?d) WHERE { ?s <http://example.com/code> ?c } ORDER BY ?s')
  AS t(s text, c text, d text);
SELECT rc_files();

-- a temporary table and an index leave the catalog's version as it was; a
-- column of another type makes it one more, and the backend compiles the
-- mapping anew: its literals of the new type, one file still
SELECT version AS v0 FROM kb.catalog_version \gset
CREATE TEMP TABLE scratch (a int);
CREATE INDEX ON rc.item (code);
SELECT version = :v0 AS same FROM kb.catalog_version;
ALTER TABLE rc.item ALTER COLUMN code TYPE text;
SELECT version = :v0 + 1 AS one_more FROM kb.catalog_version;
SELECT * FROM kb.sparql('SELECT ?s ?c (DATATYPE(?c) AS ?d) WHERE { ?s <http://example.com/code> ?c } ORDER BY ?s')
  AS t(s text, c text, d text);
SELECT rc_files();
-- a dropped table, through sql_drop, one more too
SELECT version AS v1 FROM kb.catalog_version \gset
CREATE TABLE rc.gone (a int);
DROP TABLE rc.gone;
SELECT version = :v1 + 2 AS two_more FROM kb.catalog_version;

-- off, a backend compiles each mapping itself
\c
SET kb.r2rml_cache = off;
SELECT * FROM kb.sparql('SELECT ?s ?c WHERE { ?s <http://example.com/code> ?c } ORDER BY ?s') AS t(s text, c text);
RESET kb.r2rml_cache;

DROP FUNCTION rc_files();
SELECT kb.drop_mapping('rc');
DROP SCHEMA rc CASCADE;
