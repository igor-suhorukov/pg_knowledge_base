-- SPARQL queries: forms, formats, rows, datasets (docs/design.md §2.3, §2.4)
SELECT kb.load($$
@prefix ex: <http://example.com/> .
@prefix foaf: <http://xmlns.com/foaf/0.1/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:alice a foaf:Person ; foaf:name "Alice" ; foaf:age 30 ; foaf:knows ex:bob, ex:carol ;
         ex:born "1996-05-04"^^xsd:date ; ex:height 1.68 ; ex:member true .
ex:bob a foaf:Person ; foaf:name "Bob"@en ; foaf:age 25 ; foaf:knows ex:carol .
ex:carol a foaf:Person ; foaf:name "Carol" ; foaf:age 35 .
$$);
SELECT kb.load('@prefix ex: <http://example.com/> . ex:dave ex:in ex:g1 .', 'turtle', 'http://example.com/g1');
SELECT kb.load('@prefix ex: <http://example.com/> . ex:erin ex:in ex:g2 .', 'turtle', 'http://example.com/g2');

-- rows: columns by the variables' names, typed
SELECT * FROM kb.sparql($$
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
SELECT ?name ?age WHERE { ?p a foaf:Person ; foaf:name ?name ; foaf:age ?age } ORDER BY ?age
$$) AS t(name text, age int);
-- in another order, a column for the full term, and one never bound
SELECT * FROM kb.sparql($$
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
SELECT ?p ?name ?nick WHERE { ?p foaf:name ?name OPTIONAL { ?p foaf:nick ?nick } } ORDER BY ?p
$$) AS t(name jsonb, p text, nick text);
SELECT * FROM kb.sparql($$
PREFIX ex: <http://example.com/>
SELECT ?born ?height ?member WHERE { ex:alice ex:born ?born ; ex:height ?height ; ex:member ?member }
$$) AS t(born date, height numeric, member boolean);
-- SPARQL's own computations
SELECT * FROM kb.sparql($$
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
SELECT (COUNT(?p) AS ?n) (AVG(?age) AS ?avg) (MAX(?age) AS ?max) (GROUP_CONCAT(?name; separator=", ") AS ?names)
WHERE { ?p foaf:age ?age ; foaf:name ?name }
$$) AS t(n bigint, avg numeric, max int, names text);
SELECT * FROM kb.sparql($$
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
SELECT ?a ?c WHERE { ?a foaf:knows+ ?c } ORDER BY ?a ?c
$$) AS t(a text, c text);
SELECT * FROM kb.sparql($$
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
SELECT ?p (STRLEN(?n) AS ?len) WHERE { ?p foaf:name ?n FILTER(LANG(?n) = "") MINUS { ?p foaf:age 35 } } ORDER BY ?p
$$) AS t(p text, len int);
SELECT * FROM kb.sparql($$
SELECT ?x ?y WHERE { VALUES (?x ?y) { (1 "one") (2 UNDEF) } BIND(?x * 10 AS ?z) FILTER(?z > 5) } ORDER BY ?x
$$) AS t(x int, y text);
-- ASK, CONSTRUCT and DESCRIBE as rows
SELECT * FROM kb.sparql('ASK { <http://example.com/alice> ?p ?o }') AS t(result boolean);
SELECT * FROM kb.sparql($$
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
CONSTRUCT { ?b foaf:knownBy ?a } WHERE { ?a foaf:knows ?b }
$$) AS t(subject text, predicate text, object text) ORDER BY 1, 3;

-- errors of the rows
SELECT * FROM kb.sparql('SELECT ?x WHERE { VALUES ?x { <http://example.com/x> } }') AS t(x int);
SELECT * FROM kb.sparql('SELECT ?x WHERE { VALUES ?x { "abc" } }') AS t(x int);
SELECT * FROM kb.sparql('SELECT ?x WHERE { VALUES ?x { 1 } }') AS t(y int);
SELECT * FROM kb.sparql('SELECT ?x WHERE { ?x }') AS t(x text);
SELECT * FROM kb.sparql('SELECT ?x WHERE { ?x ?p ?o }');

-- JSON, as oxigraph's server answers
SELECT kb.sparql_json('PREFIX foaf: <http://xmlns.com/foaf/0.1/> SELECT ?n WHERE { ?p foaf:name ?n } ORDER BY ?n');
SELECT kb.sparql_json('ASK { ?s ?p "nothing" }');
SELECT jsonb_array_length(kb.sparql_json('DESCRIBE <http://example.com/carol>'));
SELECT kb.sparql_json('SELECT ?n WHERE { ?p <http://xmlns.com/foaf/0.1/name> ?n } ORDER BY ?n') -> 'results' -> 'bindings' -> 0;

-- the other formats
SELECT kb.sparql_text('PREFIX foaf: <http://xmlns.com/foaf/0.1/> SELECT ?n ?age WHERE { ?p foaf:name ?n ; foaf:age ?age } ORDER BY ?age', 'csv');
SELECT kb.sparql_text('PREFIX foaf: <http://xmlns.com/foaf/0.1/> SELECT ?n ?age WHERE { ?p foaf:name ?n ; foaf:age ?age } ORDER BY ?age', 'tsv');
SELECT kb.sparql_text('SELECT ?n WHERE { <http://example.com/carol> <http://xmlns.com/foaf/0.1/name> ?n }', 'xml');
SELECT kb.sparql_text('PREFIX foaf: <http://xmlns.com/foaf/0.1/> CONSTRUCT { ?p foaf:name ?n } WHERE { ?p foaf:name ?n FILTER(?p = <http://example.com/carol>) }', 'turtle');
SELECT kb.sparql_text('CONSTRUCT WHERE { <http://example.com/carol> ?p ?o }', 'application/n-triples');
SELECT kb.sparql_text('ASK {}', 'nosuchformat');

-- datasets: graphs, FROM, the protocol's default and named graphs
SELECT * FROM kb.sparql('SELECT ?g ?s WHERE { GRAPH ?g { ?s ?p ?o } } ORDER BY ?g ?s') AS t(g text, s text);
SELECT * FROM kb.sparql('SELECT ?s FROM <http://example.com/g1> WHERE { ?s ?p ?o } ORDER BY ?s') AS t(s text);
SELECT * FROM kb.sparql('SELECT ?s WHERE { ?s ?p ?o } ORDER BY ?s', NULL, ARRAY['http://example.com/g2']) AS t(s text);
SELECT * FROM kb.sparql('SELECT ?g WHERE { GRAPH ?g { } }', NULL, NULL, ARRAY['http://example.com/g1']) AS t(g text);
SET kb.union_default_graph = on;
SELECT count(*) FROM kb.sparql('SELECT ?s WHERE { ?s <http://example.com/in> ?o }') AS t(s text);
RESET kb.union_default_graph;
SELECT count(*) FROM kb.sparql('SELECT ?s WHERE { ?s <http://example.com/in> ?o }') AS t(s text);
SELECT * FROM kb.sparql('BASE <http://example.com/> SELECT ?n WHERE { <carol> <http://xmlns.com/foaf/0.1/name> ?n }') AS t(n text);
SELECT * FROM kb.sparql('SELECT ?n WHERE { <carol> <http://xmlns.com/foaf/0.1/name> ?n }', 'http://example.com/') AS t(n text);

-- the plan, with each node's statistics
SELECT jsonb_path_query(kb.sparql_explain('SELECT * WHERE { ?s a ?t } LIMIT 2'), '$.plan.name');

-- inside SQL: a function, a join with a table
CREATE FUNCTION people_older_than(n int) RETURNS TABLE (name text, age int) LANGUAGE sql AS $f$
    SELECT * FROM kb.sparql(format($q$
        PREFIX foaf: <http://xmlns.com/foaf/0.1/>
        SELECT ?name ?age WHERE { ?p foaf:name ?name ; foaf:age ?age FILTER(?age > %s) } ORDER BY ?age
    $q$, n)) AS t(name text, age int)
$f$;
SELECT * FROM people_older_than(26);
CREATE TABLE salaries (name text, salary int);
INSERT INTO salaries VALUES ('Alice', 5000), ('Carol', 7000);
SELECT s.name, s.salary, t.age
FROM salaries s
JOIN kb.sparql('SELECT ?name ?age WHERE { ?p <http://xmlns.com/foaf/0.1/name> ?name ; <http://xmlns.com/foaf/0.1/age> ?age }')
     AS t(name text, age int) USING (name)
ORDER BY s.name;
