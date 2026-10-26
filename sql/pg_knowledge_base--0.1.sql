-- SPDX-License-Identifier: Apache-2.0
-- pg_knowledge_base 0.1 (docs/design.md): the store, the SQL functions,
-- R2RML's mappings.

\echo Use "CREATE EXTENSION pg_knowledge_base" to load this file. \quit

-- ---------------------------------------------------------------------
-- The store (docs/design.md §3)
-- ---------------------------------------------------------------------

-- A quad is four term keys (engine/src/key.rs); the default graph's key is
-- all zeros.  Six orders, as oxigraph's nine: three over every quad, and
-- three over the named graphs' quads alone.
CREATE TABLE kb.quads (
    g uuid NOT NULL,
    s uuid NOT NULL,
    p uuid NOT NULL,
    o uuid NOT NULL
);
COMMENT ON TABLE kb.quads IS
    'The RDF store''s quads, as term keys: kb.term(key) is a key''s term, kb.triples shows them';
CREATE UNIQUE INDEX quads_gspo ON kb.quads (g, s, p, o);
CREATE INDEX quads_gpos ON kb.quads (g, p, o, s);
CREATE INDEX quads_gosp ON kb.quads (g, o, s, p);
CREATE INDEX quads_spog ON kb.quads (s, p, o, g)
    WHERE g <> '00000000-0000-0000-0000-000000000000';
CREATE INDEX quads_posg ON kb.quads (p, o, s, g)
    WHERE g <> '00000000-0000-0000-0000-000000000000';
CREATE INDEX quads_ospg ON kb.quads (o, s, p, g)
    WHERE g <> '00000000-0000-0000-0000-000000000000';
-- A quad's replica identity is its four keys, which the unique index
-- holds: logical replication then publishes kb.quads' deletes, and a
-- subscriber finds the rows they delete.  A bulk load gives it back to the
-- index it builds again (docs/design.md §3.3).
ALTER TABLE kb.quads REPLICA IDENTITY USING INDEX quads_gspo;

-- The dictionary: the term of each hashed key (IRIs, long strings, ...);
-- a small value's key holds the value itself and has no row.
CREATE TABLE kb.terms (
    key uuid PRIMARY KEY,
    value text NOT NULL,
    extra text NOT NULL DEFAULT ''
);
COMMENT ON TABLE kb.terms IS
    'The terms of hashed keys: an IRI, a blank node''s label, a literal''s value; extra: its datatype or language';

-- The named graphs, empty ones too (CREATE GRAPH)
CREATE TABLE kb.graphs (
    key uuid PRIMARY KEY
);
COMMENT ON TABLE kb.graphs IS 'The store''s named graphs, as term keys';

-- ---------------------------------------------------------------------
-- R2RML's mappings (docs/design.md §4)
-- ---------------------------------------------------------------------

CREATE SEQUENCE kb.mapping_revision;
CREATE TABLE kb.mappings (
    name text PRIMARY KEY,
    document text NOT NULL,
    base_iri text,
    enabled boolean NOT NULL DEFAULT true,
    revision bigint NOT NULL DEFAULT nextval('kb.mapping_revision'),
    summary jsonb,
    created timestamptz NOT NULL DEFAULT now()
);
COMMENT ON TABLE kb.mappings IS
    'R2RML mappings of tables to RDF, read as virtual graphs beside the store''s; kb.add_mapping() adds one';

-- The catalog's version (docs/design.md §4.2): one more at the end of each
-- DDL statement but those on temporary objects, and those that leave the
-- columns of tables, views and types as they were -- what the compiled
-- mappings of a backend, and the files of pg_stat_tmp/pg_knowledge_base, are
-- keyed by besides their revisions.  A statement's change shows when its
-- transaction commits; transactions that change the catalog at once wait for
-- each other on its row.  It starts at the time it is made, in microseconds:
-- the extension made again never meets the versions of files it left.
CREATE TABLE kb.catalog_version (version bigint NOT NULL);
INSERT INTO kb.catalog_version VALUES ((extract(epoch FROM clock_timestamp()) * 1000000)::bigint);
COMMENT ON TABLE kb.catalog_version IS
    'The version of the catalog that R2RML''s compiled mappings are keyed by; DDL makes it one more (kb.catalog_changed())';
-- every role that queries reads it: a count of DDL statements, nothing more
GRANT SELECT ON kb.catalog_version TO PUBLIC;

SELECT pg_catalog.pg_extension_config_dump('kb.quads', '');
SELECT pg_catalog.pg_extension_config_dump('kb.terms', '');
SELECT pg_catalog.pg_extension_config_dump('kb.graphs', '');
SELECT pg_catalog.pg_extension_config_dump('kb.mappings', '');
SELECT pg_catalog.pg_extension_config_dump('kb.mapping_revision', '');

-- ---------------------------------------------------------------------
-- SPARQL
-- ---------------------------------------------------------------------

CREATE FUNCTION kb.sparql(query text, base_iri text DEFAULT NULL,
                          default_graphs text[] DEFAULT NULL, named_graphs text[] DEFAULT NULL)
RETURNS SETOF record
AS 'MODULE_PATHNAME', 'kb_sparql'
LANGUAGE C VOLATILE;
COMMENT ON FUNCTION kb.sparql(text, text, text[], text[]) IS
    'A SPARQL query''s solutions as rows: SELECT * FROM kb.sparql(''...'') AS t(var type, ...), columns matched to variables by name';

CREATE FUNCTION kb.sparql_json(query text, base_iri text DEFAULT NULL,
                               default_graphs text[] DEFAULT NULL, named_graphs text[] DEFAULT NULL)
RETURNS jsonb
AS 'MODULE_PATHNAME', 'kb_sparql_json'
LANGUAGE C VOLATILE;
COMMENT ON FUNCTION kb.sparql_json(text, text, text[], text[]) IS
    'A SPARQL query''s results as oxigraph''s server answers them: SPARQL 1.1 Query Results JSON, or JSON-LD for CONSTRUCT and DESCRIBE';

CREATE FUNCTION kb.sparql_text(query text, format text DEFAULT 'json', base_iri text DEFAULT NULL,
                               default_graphs text[] DEFAULT NULL, named_graphs text[] DEFAULT NULL)
RETURNS text
AS 'MODULE_PATHNAME', 'kb_sparql_text'
LANGUAGE C VOLATILE;
COMMENT ON FUNCTION kb.sparql_text(text, text, text, text[], text[]) IS
    'A SPARQL query''s results in a format: json, xml, csv, tsv; turtle, ntriples, nquads, trig, rdfxml, jsonld, n3; or a media type';

CREATE FUNCTION kb.sparql_explain(query text, base_iri text DEFAULT NULL)
RETURNS jsonb
AS 'MODULE_PATHNAME', 'kb_sparql_explain'
LANGUAGE C VOLATILE;
COMMENT ON FUNCTION kb.sparql_explain(text, text) IS
    'oxigraph''s plan of a SPARQL query, run to its end, with each node''s statistics';

CREATE FUNCTION kb.sparql_update("update" text, base_iri text DEFAULT NULL,
                                 using_graphs text[] DEFAULT NULL, using_named_graphs text[] DEFAULT NULL)
RETURNS void
AS 'MODULE_PATHNAME', 'kb_sparql_update'
LANGUAGE C VOLATILE;
COMMENT ON FUNCTION kb.sparql_update(text, text, text[], text[]) IS
    'A SPARQL 1.1 update, in the caller''s transaction';

CREATE FUNCTION kb.load(data text, format text DEFAULT 'turtle', graph text DEFAULT NULL,
                        base_iri text DEFAULT NULL, lenient boolean DEFAULT false,
                        bulk boolean DEFAULT false)
RETURNS bigint
AS 'MODULE_PATHNAME', 'kb_load'
LANGUAGE C VOLATILE;
COMMENT ON FUNCTION kb.load(text, text, text, text, boolean, boolean) IS
    'An RDF document into the store: its quads read; graph is its triples'' graph, NULL the default graph; bulk: kb.quads'' indexes dropped and built again around the load';

CREATE FUNCTION kb.load_file(path text, format text DEFAULT NULL, graph text DEFAULT NULL,
                             base_iri text DEFAULT NULL, lenient boolean DEFAULT false,
                             bulk boolean DEFAULT false)
RETURNS bigint
AS 'MODULE_PATHNAME', 'kb_load_file'
LANGUAGE C VOLATILE;
COMMENT ON FUNCTION kb.load_file(text, text, text, text, boolean, boolean) IS
    'A server file of RDF into the store (pg_read_server_files), its format from its extension unless named; bulk: kb.quads'' indexes dropped and built again around the load';

CREATE FUNCTION kb.dump(format text DEFAULT 'nquads', graph text DEFAULT NULL,
                        with_mappings boolean DEFAULT false)
RETURNS text
AS 'MODULE_PATHNAME', 'kb_dump'
LANGUAGE C VOLATILE;
COMMENT ON FUNCTION kb.dump(text, text, boolean) IS
    'The store as a document: every graph, or one (''default'' the default graph), R2RML''s graphs too when asked';

CREATE FUNCTION kb.term(key uuid)
RETURNS text
AS 'MODULE_PATHNAME', 'kb_term'
LANGUAGE C STABLE STRICT;
COMMENT ON FUNCTION kb.term(uuid) IS 'A key''s term, in N-Triples'' syntax';

CREATE FUNCTION kb.term_key(term text)
RETURNS uuid
AS 'MODULE_PATHNAME', 'kb_term_key'
LANGUAGE C IMMUTABLE STRICT;
COMMENT ON FUNCTION kb.term_key(text) IS 'A term''s key, the term in N-Triples'' syntax';

-- The SPARQL endpoint's request, run as a statement where pg_otel_probe is
-- loaded, so that the probe traces it (src/endpoint/session.c); outside an
-- endpoint session it refuses
CREATE FUNCTION kb.endpoint_request()
RETURNS void
AS 'MODULE_PATHNAME', 'kb_endpoint_request'
LANGUAGE C VOLATILE;

CREATE FUNCTION kb.engine_version()
RETURNS text
AS 'MODULE_PATHNAME', 'kb_engine_version'
LANGUAGE C STABLE;

CREATE VIEW kb.triples AS
SELECT kb.term(s) AS subject, kb.term(p) AS predicate, kb.term(o) AS object,
       CASE WHEN g = '00000000-0000-0000-0000-000000000000' THEN NULL ELSE kb.term(g) END AS graph
FROM kb.quads;
COMMENT ON VIEW kb.triples IS 'The store''s quads as terms in N-Triples'' syntax; graph NULL for the default graph';

-- The store's sizes; exact counts read the tables, the others are estimates
CREATE FUNCTION kb.stats(exact boolean DEFAULT false)
RETURNS jsonb
LANGUAGE sql STABLE
AS $$
SELECT jsonb_build_object(
    'quads', CASE WHEN exact THEN (SELECT count(*) FROM kb.quads)
                  ELSE (SELECT greatest(reltuples, 0)::bigint FROM pg_class WHERE oid = 'kb.quads'::regclass) END,
    'terms', CASE WHEN exact THEN (SELECT count(*) FROM kb.terms)
                  ELSE (SELECT greatest(reltuples, 0)::bigint FROM pg_class WHERE oid = 'kb.terms'::regclass) END,
    'named_graphs', (SELECT count(*) FROM kb.graphs),
    'mappings', (SELECT count(*) FROM kb.mappings WHERE enabled),
    'bytes', jsonb_build_object(
        'quads_heap', pg_relation_size('kb.quads'),
        'quads_indexes', pg_indexes_size('kb.quads'),
        'terms', pg_total_relation_size('kb.terms'),
        'graphs', pg_total_relation_size('kb.graphs')))
$$;

-- Dictionary rows no quad refers to any more: a deleted quad leaves its
-- terms, as oxigraph's store does
CREATE FUNCTION kb.vacuum_terms()
RETURNS bigint
LANGUAGE sql VOLATILE
AS $$
WITH gone AS (
    DELETE FROM kb.terms t
    WHERE NOT EXISTS (SELECT 1 FROM kb.quads q WHERE q.s = t.key)
      AND NOT EXISTS (SELECT 1 FROM kb.quads q WHERE q.p = t.key)
      AND NOT EXISTS (SELECT 1 FROM kb.quads q WHERE q.o = t.key)
      AND NOT EXISTS (SELECT 1 FROM kb.quads q WHERE q.g = t.key)
      AND NOT EXISTS (SELECT 1 FROM kb.graphs n WHERE n.key = t.key)
    RETURNING 1)
SELECT count(*) FROM gone
$$;

-- ---------------------------------------------------------------------
-- R2RML
-- ---------------------------------------------------------------------

CREATE FUNCTION kb.check_mapping(name text, document text, base_iri text DEFAULT NULL)
RETURNS jsonb
AS 'MODULE_PATHNAME', 'kb_check_mapping'
LANGUAGE C VOLATILE;
COMMENT ON FUNCTION kb.check_mapping(text, text, text) IS
    'An R2RML mapping parsed and checked against the tables it names, as a JSON summary; nothing stored';

-- A mapping, checked and stored: queries read its virtual graphs from then
-- on.  Its SQL runs as the querying role, with that role's privileges and
-- row-level security, so only roles that may write kb.mappings may add one.
CREATE FUNCTION kb.add_mapping(name text, document text, base_iri text DEFAULT NULL,
                               replace boolean DEFAULT false)
RETURNS jsonb
LANGUAGE plpgsql VOLATILE
AS $$
DECLARE
    checked jsonb := kb.check_mapping(name, document, base_iri);
BEGIN
    IF replace THEN
        INSERT INTO kb.mappings AS m (name, document, base_iri, summary)
        VALUES (add_mapping.name, add_mapping.document, add_mapping.base_iri, checked)
        ON CONFLICT ON CONSTRAINT mappings_pkey DO UPDATE
            SET document = EXCLUDED.document, base_iri = EXCLUDED.base_iri,
                summary = EXCLUDED.summary, enabled = true,
                revision = nextval('kb.mapping_revision');
    ELSE
        INSERT INTO kb.mappings (name, document, base_iri, summary)
        VALUES (add_mapping.name, add_mapping.document, add_mapping.base_iri, checked);
    END IF;
    RETURN checked;
END
$$;

CREATE FUNCTION kb.drop_mapping(name text, if_exists boolean DEFAULT false)
RETURNS void
LANGUAGE plpgsql VOLATILE
AS $$
BEGIN
    DELETE FROM kb.mappings m WHERE m.name = drop_mapping.name;
    IF NOT FOUND AND NOT if_exists THEN
        RAISE EXCEPTION 'mapping "%" does not exist', drop_mapping.name
            USING ERRCODE = 'undefined_object';
    END IF;
END
$$;

CREATE FUNCTION kb.enable_mapping(name text, enabled boolean DEFAULT true)
RETURNS void
LANGUAGE plpgsql VOLATILE
AS $$
BEGIN
    UPDATE kb.mappings m SET enabled = enable_mapping.enabled,
                             revision = nextval('kb.mapping_revision')
    WHERE m.name = enable_mapping.name;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'mapping "%" does not exist', enable_mapping.name
            USING ERRCODE = 'undefined_object';
    END IF;
END
$$;

CREATE FUNCTION kb.mappings_changed()
RETURNS trigger
AS 'MODULE_PATHNAME', 'kb_mappings_changed_trigger'
LANGUAGE C;
CREATE TRIGGER mappings_changed AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON kb.mappings
    FOR EACH STATEMENT EXECUTE FUNCTION kb.mappings_changed();

CREATE FUNCTION kb.direct_mapping_json(tables text, base_iri text)
RETURNS text
AS 'MODULE_PATHNAME', 'kb_direct_mapping_json'
LANGUAGE C IMMUTABLE STRICT;

-- The class of a SQL type for R2RML's natural literals (src/sql.c, sql_class)
CREATE FUNCTION kb.sql_class(type regtype)
RETURNS int
LANGUAGE sql IMMUTABLE STRICT
AS $$
SELECT CASE (SELECT coalesce(nullif(typbasetype, 0), oid) FROM pg_type WHERE oid = type)
    WHEN 'int2'::regtype THEN 1 WHEN 'int4'::regtype THEN 1 WHEN 'int8'::regtype THEN 1
    WHEN 'oid'::regtype THEN 1
    WHEN 'numeric'::regtype THEN 2
    WHEN 'float4'::regtype THEN 3 WHEN 'float8'::regtype THEN 3
    WHEN 'bool'::regtype THEN 4
    WHEN 'date'::regtype THEN 5
    WHEN 'time'::regtype THEN 6 WHEN 'timetz'::regtype THEN 6
    WHEN 'timestamp'::regtype THEN 7 WHEN 'timestamptz'::regtype THEN 7
    WHEN 'bytea'::regtype THEN 8
    ELSE 0 END
$$;

-- The W3C Direct Mapping of tables, as an R2RML document, for
-- kb.add_mapping(): a row's IRI from its primary key, a blank node without
-- one, a predicate a column, a reference a foreign key
CREATE FUNCTION kb.direct_mapping(tables regclass[], base_iri text DEFAULT 'http://example.com/base/')
RETURNS text
LANGUAGE sql STABLE
AS $$
SELECT kb.direct_mapping_json(coalesce((
    SELECT jsonb_agg(jsonb_build_object(
        'name', format('%I.%I', n.nspname, c.relname),
        'table', c.relname,
        'kind', c.relkind,
        'columns', (
            SELECT coalesce(jsonb_agg(jsonb_build_object(
                       'name', a.attname,
                       'type', format_type(a.atttypid, a.atttypmod),
                       'class', kb.sql_class(a.atttypid)) ORDER BY a.attnum), '[]')
            FROM pg_attribute a
            WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped),
        'primary_key', (
            SELECT coalesce(jsonb_agg(a.attname ORDER BY k.ord), '[]')
            FROM pg_constraint pk
            CROSS JOIN LATERAL unnest(pk.conkey) WITH ORDINALITY AS k (attnum, ord)
            JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum
            WHERE pk.conrelid = c.oid AND pk.contype = 'p'),
        'foreign_keys', (
            SELECT coalesce(jsonb_agg(jsonb_build_object(
                       'columns', (SELECT jsonb_agg(a.attname ORDER BY k.ord)
                                   FROM unnest(fk.conkey) WITH ORDINALITY AS k (attnum, ord)
                                   JOIN pg_attribute a ON a.attrelid = fk.conrelid AND a.attnum = k.attnum),
                       'ref_name', format('%I.%I', rn.nspname, rc.relname),
                       'ref_table', rc.relname,
                       'ref_columns', (SELECT jsonb_agg(a.attname ORDER BY k.ord)
                                       FROM unnest(fk.confkey) WITH ORDINALITY AS k (attnum, ord)
                                       JOIN pg_attribute a ON a.attrelid = fk.confrelid AND a.attnum = k.attnum))
                   ORDER BY fk.conname), '[]')
            FROM pg_constraint fk
            JOIN pg_class rc ON rc.oid = fk.confrelid
            JOIN pg_namespace rn ON rn.oid = rc.relnamespace
            WHERE fk.conrelid = c.oid AND fk.contype = 'f'))
        ORDER BY t.ord)
    FROM unnest(tables) WITH ORDINALITY AS t (rel, ord)
    JOIN pg_class c ON c.oid = t.rel
    JOIN pg_namespace n ON n.oid = c.relnamespace), '[]')::text, base_iri)
$$;

-- A DDL statement's end: the catalog's version one more, but for temporary
-- objects and for statements that leave the columns of tables, views and
-- types as they were.  Its owner's, so that any role's DDL may write it.
CREATE FUNCTION kb.catalog_changed()
RETURNS event_trigger
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_EVENT = 'sql_drop' THEN
        IF NOT EXISTS (
            SELECT 1 FROM pg_event_trigger_dropped_objects() AS o
            WHERE NOT o.is_temporary
              AND o.object_type NOT IN ('index', 'trigger', 'policy', 'statistics object', 'rule',
                                        'event trigger', 'publication', 'publication relation',
                                        'publication namespace', 'subscription')) THEN
            RETURN;
        END IF;
    ELSIF NOT EXISTS (
        SELECT 1 FROM pg_event_trigger_ddl_commands() AS c
        WHERE (c.schema_name IS NULL OR c.schema_name NOT LIKE 'pg\_temp%')
          AND c.command_tag NOT IN ('COMMENT', 'GRANT', 'REVOKE', 'SECURITY LABEL', 'ALTER DEFAULT PRIVILEGES',
                                    'CREATE INDEX', 'ALTER INDEX', 'REINDEX', 'CREATE STATISTICS',
                                    'ALTER STATISTICS', 'REFRESH MATERIALIZED VIEW', 'CREATE TRIGGER',
                                    'ALTER TRIGGER', 'CREATE POLICY', 'ALTER POLICY', 'CREATE PUBLICATION',
                                    'ALTER PUBLICATION', 'CREATE SUBSCRIPTION', 'ALTER SUBSCRIPTION')) THEN
        RETURN;
    END IF;
    UPDATE kb.catalog_version SET version = version + 1;
END
$$;
COMMENT ON FUNCTION kb.catalog_changed() IS
    'An event trigger''s: DDL that may change the columns R2RML''s mappings read makes kb.catalog_version one more';
CREATE EVENT TRIGGER kb_catalog_changed ON ddl_command_end EXECUTE FUNCTION kb.catalog_changed();
CREATE EVENT TRIGGER kb_catalog_dropped ON sql_drop EXECUTE FUNCTION kb.catalog_changed();
