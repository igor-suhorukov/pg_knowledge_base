-- R2RML: basic graph patterns over mappings as one SQL query
-- (docs/design.md §4.4), with kb.r2rml_pushdown; each query's solutions
-- the same with it off, when every pattern is read apart
SELECT kb.sparql_update('CLEAR ALL');
CREATE SCHEMA pd;
CREATE TABLE pd.person (iri text PRIMARY KEY, name text NOT NULL, age int, city text, boss text, tag text);
CREATE TABLE pd.city (iri text PRIMARY KEY, label text NOT NULL, country text);
CREATE TABLE pd.typed (iri text NOT NULL, type text NOT NULL);
INSERT INTO pd.person VALUES
 ('http://example.com/p/1', 'Ann', 30, 'http://example.com/c/berlin', NULL, 'a'),
 ('http://example.com/p/2', 'Ben', 40, 'http://example.com/c/paris', 'http://example.com/p/1', 'b'),
 ('http://example.com/p/3', 'Cid', NULL, NULL, 'http://example.com/p/1', NULL),
 ('http://example.com/q/4', 'Dee', 30, 'http://example.com/c/berlin', 'http://example.com/p/2', 'a');
INSERT INTO pd.city VALUES
 ('http://example.com/c/berlin', 'Berlin', 'DE'), ('http://example.com/c/paris', 'Paris', 'FR');
INSERT INTO pd.typed VALUES
 ('http://example.com/p/1', 'http://example.com/T/Admin'), ('http://example.com/p/2', 'http://example.com/T/User'),
 ('http://example.com/q/4', 'http://example.com/T/Admin'), ('http://example.com/c/berlin', 'http://example.org/Capital');

-- IRIs from columns, so that SQL can join on them; a blank node made by a
-- template; a string in a language
SELECT kb.add_mapping('pd', $$
@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.com/v#> .
<#P> rr:logicalTable [ rr:tableName "pd.person" ] ;
  rr:subjectMap [ rr:column "iri" ; rr:class ex:Person ] ;
  rr:predicateObjectMap [ rr:predicate ex:name ; rr:objectMap [ rr:column "name" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:age ; rr:objectMap [ rr:column "age" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:city ; rr:objectMap [ rr:column "city" ; rr:termType rr:IRI ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:boss ; rr:objectMap [ rr:column "boss" ; rr:termType rr:IRI ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:tag ; rr:objectMap [ rr:column "tag" ; rr:language "en" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:card ; rr:objectMap [ rr:template "card {iri}" ; rr:termType rr:BlankNode ] ] .
<#Card> rr:logicalTable [ rr:tableName "pd.person" ] ;
  rr:subjectMap [ rr:template "card {iri}" ; rr:termType rr:BlankNode ] ;
  rr:predicateObjectMap [ rr:predicate ex:holder ; rr:objectMap [ rr:column "name" ] ] .
<#C> rr:logicalTable [ rr:tableName "pd.city" ] ;
  rr:subjectMap [ rr:column "iri" ] ;
  rr:predicateObjectMap [ rr:predicate ex:label ; rr:objectMap [ rr:column "label" ] ] ;
  rr:predicateObjectMap [ rr:predicate ex:country ; rr:objectMap [ rr:column "country" ] ] .
<#T> rr:logicalTable [ rr:tableName "pd.typed" ] ;
  rr:subjectMap [ rr:column "iri" ] ;
  rr:predicateObjectMap [ rr:predicate <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> ;
                          rr:objectMap [ rr:column "type" ; rr:termType rr:IRI ] ] .
$$);
-- and a few quads of the store's
SELECT kb.load($$
@prefix ex: <http://example.com/v#> .
<http://example.com/p/3> ex:name "Cid" ; ex:city <http://example.com/c/paris> .
<http://example.com/c/paris> ex:label "Paris"@fr .
<http://example.com/p/5> ex:name "Eve" ; ex:boss <http://example.com/q/4> .
$$);

-- a query's solutions with kb.r2rml_pushdown and without, the same; and its
-- plan's operators, in order -- a SERVICE of the SQL as "SQL"
CREATE FUNCTION pd_check(query text) RETURNS text LANGUAGE plpgsql AS $f$
DECLARE
    pushed text;
    apart text;
    plan text;
BEGIN
    PERFORM set_config('kb.r2rml_pushdown', 'on', true);
    SELECT string_agg(b::text, E'\n' ORDER BY b::text) INTO pushed
      FROM jsonb_array_elements(kb.sparql_json(query) -> 'results' -> 'bindings') AS b;
    SELECT string_agg(CASE WHEN n LIKE 'Service(<urn:x-pg-knowledge-base:r2rml:%' THEN 'SQL'
                           ELSE regexp_replace(n, '\(.*$', '') END, ' ' ORDER BY i) INTO plan
      FROM jsonb_path_query(kb.sparql_explain(query), 'strict $.**.name') WITH ORDINALITY AS x(j, i),
           LATERAL (SELECT j #>> '{}') AS y(n);
    PERFORM set_config('kb.r2rml_pushdown', 'off', true);
    SELECT string_agg(b::text, E'\n' ORDER BY b::text) INTO apart
      FROM jsonb_array_elements(kb.sparql_json(query) -> 'results' -> 'bindings') AS b;
    PERFORM set_config('kb.r2rml_pushdown', 'on', true);
    RETURN format('%s: %s', CASE WHEN pushed IS NOT DISTINCT FROM apart THEN 'same' ELSE 'DIFFERENT' END, plan);
END
$f$;

-- a join on IRIs, the store's quads among the rows as VALUES
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?n ?l WHERE { ?p ex:name ?n ; ex:city ?c . ?c ex:label ?l }');
SELECT * FROM kb.sparql('PREFIX ex: <http://example.com/v#>
SELECT ?n ?l WHERE { ?p ex:name ?n ; ex:city ?c . ?c ex:label ?l } ORDER BY ?n ?l') AS t(n text, l text);
-- OPTIONALs as LEFT JOINs of the SQL, a core of one pattern too
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?n ?b WHERE { ?p ex:name ?n . OPTIONAL { ?p ex:boss ?x . ?x ex:name ?b } }');
SELECT * FROM kb.sparql('PREFIX ex: <http://example.com/v#>
SELECT ?n ?b WHERE { ?p ex:name ?n . OPTIONAL { ?p ex:boss ?x . ?x ex:name ?b } } ORDER BY ?n ?b') AS t(n text, b text);
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?p ?n ?a WHERE { ?p a ex:Person . OPTIONAL { ?p ex:name ?n } OPTIONAL { ?p ex:age ?a } }');
-- a pattern after an OPTIONAL, with a variable only the OPTIONAL binds:
-- spareval joins it
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?n ?c WHERE { ?p ex:name ?n . OPTIONAL { ?p ex:city ?c } ?c ex:country ?k }');
-- FILTERs the SQL checks too: a prefix, IN, =
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?p ?t WHERE { ?p ex:name ?n ; a ?t . FILTER(STRSTARTS(STR(?t), "http://example.com/T/")) }');
SELECT * FROM kb.sparql('PREFIX ex: <http://example.com/v#>
SELECT ?p ?t WHERE { ?p ex:name ?n ; a ?t . FILTER(STRSTARTS(STR(?t), "http://example.com/T/")) } ORDER BY ?p')
  AS t(p text, t text);
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?p WHERE { ?p ex:city ?c ; ex:name ?n . FILTER(?c IN (<http://example.com/c/paris>)) }');
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?p WHERE { ?p ex:city ?c ; ex:name ?n . FILTER(?c = <http://example.com/c/berlin> && STRSTARTS(?n, "D")) }');
-- a variable predicate a FILTER restricts to IRIs: the union of their patterns
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?n ?prop ?v WHERE { ?p ex:name ?n ; ?prop ?v . FILTER(?prop IN (ex:age, ex:city, ex:boss)) }');
SELECT * FROM kb.sparql('PREFIX ex: <http://example.com/v#>
SELECT ?n ?prop ?v WHERE { ?p ex:name ?n ; ?prop ?v . FILTER(?prop IN (ex:age, ex:city)) } ORDER BY ?n ?prop ?v')
  AS t(n text, prop text, v text);
-- a BIND between two basic graph patterns, lifted over their join
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?n ?k ?l WHERE { ?p ex:name ?n . BIND(STRLEN(?n) AS ?k) ?p ex:city ?c . ?c ex:label ?l }');
-- EXISTS and NOT EXISTS as columns of the SQL's rows
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?n ?admin WHERE { ?p ex:name ?n . BIND(IF(EXISTS { ?p a <http://example.com/T/Admin> }, "yes", "no") AS ?admin) }');
SELECT * FROM kb.sparql('PREFIX ex: <http://example.com/v#>
SELECT ?n ?admin WHERE { ?p ex:name ?n . BIND(IF(EXISTS { ?p a <http://example.com/T/Admin> }, "yes", "no") AS ?admin) }
ORDER BY ?n') AS t(n text, admin text);
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?n WHERE { ?p ex:name ?n . FILTER NOT EXISTS { ?p ex:boss ?b . ?b ex:age 30 } }');
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?n WHERE { ?p ex:name ?n ; ex:age ?a . FILTER(?a > 20 && EXISTS { ?p ex:city ?c . ?c ex:country "DE" }) }');
-- an EXISTS with a variable an OPTIONAL binds: spareval's
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?n WHERE { ?p ex:name ?n . OPTIONAL { ?p ex:boss ?b } FILTER(!EXISTS { ?b ex:name "Ann" }) }');
-- blank nodes of a mapping, joined by their labels
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?n ?h WHERE { ?p ex:name ?n ; ex:card ?c . ?c ex:holder ?h }');
-- strings in a language, integers
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?a ?b WHERE { ?a ex:tag ?t . ?b ex:tag ?t . FILTER(?a != ?b) }');
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?a ?b WHERE { ?a ex:age ?x . ?b ex:age ?x . FILTER(STR(?a) < STR(?b)) }');
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?p WHERE { ?p ex:age 30 ; ex:name ?n }');
-- a graph's patterns, and a variable twice in one pattern
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?g ?n WHERE { GRAPH ?g { ?p ex:name ?n ; ex:city ?c } }');
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?p WHERE { ?p ex:boss ?p ; ex:name ?n }');
-- the rows two mappings, or a mapping and the store, both make are one
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?p ?n WHERE { ?p ex:name ?n ; ex:boss ?b }');
SELECT * FROM kb.sparql('PREFIX ex: <http://example.com/v#>
SELECT ?p ?n WHERE { ?p ex:name ?n ; ex:boss ?b } ORDER BY ?p') AS t(p text, n text);

-- the optimizer's estimates, the store's and the mappings', change plans,
-- not solutions
SET kb.statistics = off;
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?n ?b WHERE { ?p ex:name ?n . OPTIONAL { ?p ex:boss ?x . ?x ex:name ?b } ?p ?any ?v }');
RESET kb.statistics;
SELECT pd_check('PREFIX ex: <http://example.com/v#>
SELECT ?n ?b WHERE { ?p ex:name ?n . OPTIONAL { ?p ex:boss ?x . ?x ex:name ?b } ?p ?any ?v }');

DROP FUNCTION pd_check(text);
SELECT kb.drop_mapping('pd');
SELECT kb.sparql_update('CLEAR ALL');
DROP SCHEMA pd CASCADE;
