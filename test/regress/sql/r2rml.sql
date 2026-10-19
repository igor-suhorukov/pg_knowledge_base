-- R2RML: mappings of tables, read as virtual graphs beside the store
-- (docs/design.md §4)
SELECT kb.sparql_update('CLEAR ALL');
CREATE SCHEMA hr;
CREATE TABLE hr.dept (id int PRIMARY KEY, name text NOT NULL, city varchar(20));
CREATE TABLE hr.emp (
    id int PRIMARY KEY, name text NOT NULL, salary numeric(10, 2), rating float8,
    active boolean, hired date, updated timestamptz, badge bytea, code char(4),
    dept int REFERENCES hr.dept, manager int REFERENCES hr.emp, email text, notes text);
INSERT INTO hr.dept VALUES (1, 'R&D', 'Berlin'), (2, 'Sales', NULL);
INSERT INTO hr.emp VALUES
 (7, 'Alice Smith', 5000.50, 4.5, true, '2020-01-15', '2026-10-19 10:00:00+03', '\x0aff', 'AB', 1, NULL, 'alice@example.com', 'first/hire'),
 (8, 'Bob', 4000, 3.25, false, '2021-06-01', NULL, NULL, 'X', 2, 7, NULL, NULL),
 (9, 'Carol', 7000, NULL, true, '2019-03-03', '2026-01-01 00:00:00+00', NULL, NULL, 1, 7, 'carol@example.com', 'a/b c');

SELECT kb.add_mapping('hr', $$
@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.com/hr#> .
@prefix foaf: <http://xmlns.com/foaf/0.1/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
<#Emp> rr:logicalTable [ rr:tableName "hr.emp" ] ;
  rr:subjectMap [ rr:template "http://example.com/emp/{id}" ; rr:class foaf:Person ] ;
  rr:predicateObjectMap [ rr:predicate foaf:name ; rr:objectMap [ rr:column "name" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:salary ; rr:objectMap [ rr:column "salary" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:rating ; rr:objectMap [ rr:column "rating" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:active ; rr:objectMap [ rr:column "active" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:hired ; rr:objectMap [ rr:column "hired" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:updated ; rr:objectMap [ rr:column "updated" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:badge ; rr:objectMap [ rr:column "badge" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:code ; rr:objectMap [ rr:column "code" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:note ; rr:objectMap [ rr:column "notes" ; rr:language "en" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:idString ; rr:objectMap [ rr:column "id" ; rr:datatype xsd:string ] ] ;
  rr:predicateObjectMap [ rr:predicate foaf:mbox ; rr:objectMap [ rr:template "mailto:{email}" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:page ; rr:objectMap [ rr:template "http://example.com/notes/{notes}" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:manager ; rr:objectMap [ rr:template "http://example.com/emp/{manager}" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:dept ;
      rr:objectMap [ rr:parentTriplesMap <#Dept> ; rr:joinCondition [ rr:child "dept" ; rr:parent "id" ] ] ] .
<#Dept> rr:logicalTable [ rr:sqlQuery "SELECT id, upper(name) AS name, city FROM hr.dept" ] ;
  rr:subjectMap [ rr:template "http://example.com/dept/{id}" ; rr:class ex:Department ;
                  rr:graph <http://example.com/graphs/org> ] ;
  rr:predicateObjectMap [ rr:predicate ex:label ; rr:objectMap [ rr:column "name" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:city ; rr:objectMap [ rr:column "city" ] ;
                          rr:graphMap [ rr:template "http://example.com/graphs/city/{city}" ] ] .
$$, 'http://example.com/base/');
SELECT name, enabled, summary -> 'triple_templates' AS templates, summary -> 'graphs' AS graphs FROM kb.mappings;

-- every virtual triple, as N-Quads
SELECT line FROM regexp_split_to_table(kb.dump('nquads', NULL, true), E'\n') AS line WHERE line <> '' ORDER BY line;

-- a pattern's bound terms narrow the SQL: the template's column, the literal's value
SELECT * FROM kb.sparql('SELECT ?n WHERE { <http://example.com/emp/7> <http://xmlns.com/foaf/0.1/name> ?n }') AS t(n text);
SELECT * FROM kb.sparql('SELECT ?e WHERE { ?e <http://xmlns.com/foaf/0.1/name> "Bob" }') AS t(e text);
SELECT * FROM kb.sparql('SELECT ?e WHERE { ?e <http://example.com/hr#salary> 7000.0 }') AS t(e text);
SELECT * FROM kb.sparql('SELECT ?e WHERE { ?e <http://example.com/hr#salary> 7000 }') AS t(e text);
SELECT * FROM kb.sparql('SELECT ?e WHERE { ?e <http://example.com/hr#hired> "2021-06-01"^^<http://www.w3.org/2001/XMLSchema#date> }') AS t(e text);
SELECT * FROM kb.sparql('SELECT ?e WHERE { ?e <http://example.com/hr#active> false }') AS t(e text);
SELECT * FROM kb.sparql('SELECT ?n WHERE { <http://example.com/emp/abc> <http://xmlns.com/foaf/0.1/name> ?n }') AS t(n text);
SELECT * FROM kb.sparql('SELECT ?n WHERE { <http://example.com/emp/99999999999> <http://xmlns.com/foaf/0.1/name> ?n }') AS t(n text);
SELECT * FROM kb.sparql('SELECT ?e WHERE { ?e <http://example.com/hr#page> <http://example.com/notes/a%2Fb%20c> }') AS t(e text);
SELECT * FROM kb.sparql('SELECT ?e WHERE { ?e <http://example.com/hr#manager> <http://example.com/emp/7> } ORDER BY ?e') AS t(e text);
-- a join with the parent's table, and a graph made from a column
SELECT * FROM kb.sparql($$
PREFIX ex: <http://example.com/hr#>
SELECT ?e ?label WHERE { ?e ex:dept ?d . GRAPH <http://example.com/graphs/org> { ?d ex:label ?label } } ORDER BY ?e
$$) AS t(e text, label text);
SELECT * FROM kb.sparql('SELECT ?g ?city WHERE { GRAPH ?g { ?d <http://example.com/hr#city> ?city } } ORDER BY ?g') AS t(g text, city text);
SELECT * FROM kb.sparql('SELECT ?g WHERE { GRAPH ?g { } } ORDER BY ?g') AS t(g text);

-- the store and the tables in one query
SELECT kb.load($$
@prefix foaf: <http://xmlns.com/foaf/0.1/> .
<http://example.com/emp/7> foaf:knows <http://example.com/emp/9> .
<http://example.com/emp/9> foaf:nick "cc" .
<http://example.com/emp/9> foaf:name "Carol" .
$$);
SELECT * FROM kb.sparql($$
PREFIX foaf: <http://xmlns.com/foaf/0.1/> PREFIX ex: <http://example.com/hr#>
SELECT ?a ?b ?nick ?salary WHERE {
  ?x foaf:knows ?y . ?x foaf:name ?a . ?y foaf:name ?b ; ex:salary ?salary .
  OPTIONAL { ?y foaf:nick ?nick } }
$$) AS t(a text, b text, nick text, salary numeric);
-- a triple both stored and mapped is in the dataset once
SELECT * FROM kb.sparql('SELECT (COUNT(*) AS ?n) WHERE { <http://example.com/emp/9> <http://xmlns.com/foaf/0.1/name> ?n }') AS t(n int);
SELECT * FROM kb.sparql('SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }') AS t(n int);
SET kb.with_mappings = off;
SELECT * FROM kb.sparql('SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }') AS t(n int);
RESET kb.with_mappings;

-- the tables change, the triples with them
UPDATE hr.emp SET name = 'Robert' WHERE id = 8;
SELECT * FROM kb.sparql('SELECT ?n WHERE { <http://example.com/emp/8> <http://xmlns.com/foaf/0.1/name> ?n }') AS t(n text);
-- updates write the store, never the tables
SELECT kb.sparql_update('DELETE DATA { <http://example.com/emp/8> <http://xmlns.com/foaf/0.1/name> "Robert" }');
SELECT * FROM kb.sparql('SELECT ?n WHERE { <http://example.com/emp/8> <http://xmlns.com/foaf/0.1/name> ?n }') AS t(n text);

-- the querying role's privileges and row-level security on the tables
CREATE ROLE kb_reader;
GRANT USAGE ON SCHEMA kb, hr TO kb_reader;
GRANT SELECT ON kb.quads, kb.terms, kb.graphs, kb.mappings TO kb_reader;
SET ROLE kb_reader;
SELECT * FROM kb.sparql('SELECT ?n WHERE { ?e <http://xmlns.com/foaf/0.1/name> ?n }') AS t(n text);
RESET ROLE;
GRANT SELECT ON hr.emp, hr.dept TO kb_reader;
ALTER TABLE hr.emp ENABLE ROW LEVEL SECURITY;
CREATE POLICY active_only ON hr.emp FOR SELECT TO kb_reader USING (active);
SET ROLE kb_reader;
SELECT * FROM kb.sparql('SELECT ?n WHERE { ?e <http://xmlns.com/foaf/0.1/name> ?n } ORDER BY ?n') AS t(n text);
RESET ROLE;
DROP POLICY active_only ON hr.emp;
ALTER TABLE hr.emp DISABLE ROW LEVEL SECURITY;

-- a mapping replaced, disabled, dropped
SELECT kb.add_mapping('hr', 'not turtle');
SELECT kb.add_mapping('depts', $$
@prefix rr: <http://www.w3.org/ns/r2rml#> .
<#D> rr:logicalTable [ rr:tableName "hr.dept" ] ;
  rr:subjectMap [ rr:template "http://example.com/d/{id}" ] ;
  rr:predicateObjectMap [ rr:predicate <http://example.com/name> ; rr:objectMap [ rr:column "name" ] ] .
$$);
SELECT * FROM kb.sparql('SELECT ?s ?n WHERE { ?s <http://example.com/name> ?n } ORDER BY ?s') AS t(s text, n text);
SELECT kb.enable_mapping('depts', false);
SELECT count(*) FROM kb.sparql('SELECT ?s WHERE { ?s <http://example.com/name> ?n }') AS t(s text);
SELECT kb.drop_mapping('depts');
SELECT kb.drop_mapping('depts');
SELECT kb.drop_mapping('depts', if_exists => true);

-- non-conforming mappings are refused
SELECT kb.check_mapping('bad', $$
@prefix rr: <http://www.w3.org/ns/r2rml#> .
<#T> rr:logicalTable [ rr:tableName "hr.emp" ] ;
  rr:subjectMap [ rr:template "http://example.com/{id}" ; rr:class <Employee> ] .
$$);
SELECT kb.check_mapping('bad', $$
@prefix rr: <http://www.w3.org/ns/r2rml#> .
<#T> rr:logicalTable [ rr:tableName "hr.emp" ] ;
  rr:subjectMap [ rr:template "http://example.com/{nosuchcolumn}" ] .
$$);
SELECT kb.check_mapping('bad', $$
@prefix rr: <http://www.w3.org/ns/r2rml#> .
<#T> rr:logicalTable [ rr:tableName "hr.emp" ] .
$$);
SELECT kb.check_mapping('bad', $$
@prefix rr: <http://www.w3.org/ns/r2rml#> .
<#T> rr:logicalTable [ rr:tableName "hr.emp" ] ;
  rr:subjectMap [ rr:column "name" ; rr:termType rr:Literal ] .
$$);
SELECT kb.check_mapping('bad', $$
@prefix rr: <http://www.w3.org/ns/r2rml#> .
<#T> rr:logicalTable [ rr:sqlQuery "DELETE FROM hr.emp RETURNING *" ] ;
  rr:subjectMap [ rr:template "http://example.com/{id}" ] .
$$);
SELECT kb.check_mapping('bad', $$
@prefix rr: <http://www.w3.org/ns/r2rml#> .
<#T> rr:logicalTable [ rr:sqlQuery "SELECT * FROM hr.emp FOR UPDATE" ] ;
  rr:subjectMap [ rr:template "http://example.com/{id}" ] .
$$);
SELECT count(*) FROM hr.emp;

-- the W3C Direct Mapping, as a mapping
SELECT kb.drop_mapping('hr');
SELECT kb.add_mapping('direct', kb.direct_mapping(ARRAY['hr.dept'::regclass], 'http://example.com/db/'));
SELECT * FROM kb.sparql('SELECT ?s ?p ?o WHERE { ?s ?p ?o } ORDER BY ?s ?p ?o') AS t(s text, p text, o text);
SELECT kb.drop_mapping('direct');
DROP OWNED BY kb_reader;
DROP ROLE kb_reader;
