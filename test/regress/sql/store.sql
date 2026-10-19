-- The store: loads, terms, graphs, dumps, transactions (docs/design.md §3)
CREATE EXTENSION pg_knowledge_base;

-- every format oxigraph reads
SELECT kb.load($$
@prefix ex: <http://example.com/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:a ex:name "Alpha" ; ex:count 3 ; ex:ratio 0.25 ; ex:size 1.5e3 ;
     ex:ok true ; ex:when "2026-10-19T05:28:00Z"^^xsd:dateTime ; ex:day "2026-10-19"^^xsd:date ;
     ex:label "alpha"@en, "άλφα"@el ; ex:long "a literal longer than fourteen bytes" ;
     ex:odd "x"^^ex:type ; ex:bad "abc"^^xsd:integer ; ex:padded "007"^^xsd:int .
$$);
SELECT kb.load('<http://example.com/b> <http://example.com/name> "Beta" .', 'ntriples');
SELECT kb.load('<http://example.com/c> <http://example.com/name> "Gamma" <http://example.com/g1> .', 'nquads');
SELECT kb.load('@prefix ex: <http://example.com/> . ex:g2 { ex:d ex:name "Delta" }', 'trig');
SELECT kb.load('<?xml version="1.0"?>
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:ex="http://example.com/">
  <rdf:Description rdf:about="http://example.com/e"><ex:name>Epsilon</ex:name></rdf:Description>
</rdf:RDF>', 'rdfxml');
SELECT kb.load('{"@id": "http://example.com/f", "http://example.com/name": "Phi"}', 'jsonld');
-- into a named graph, relative IRIs against a base
SELECT kb.load('<x> <name> "Relative" .', 'turtle', 'http://example.com/g3', 'http://example.com/');

-- literals of datatypes oxigraph knows are kept as values: "007"^^xsd:int is 7
SELECT subject, predicate, object, graph FROM kb.triples ORDER BY graph NULLS FIRST, subject, predicate, object;

-- a key's term, a term's key
SELECT kb.term(kb.term_key('<http://example.com/a>'));
SELECT kb.term(kb.term_key('"007"^^<http://www.w3.org/2001/XMLSchema#int>'));
SELECT kb.term_key('"1"^^<http://www.w3.org/2001/XMLSchema#integer>') =
       kb.term_key('"01"^^<http://www.w3.org/2001/XMLSchema#integer>') AS same_value_same_key;
SELECT kb.term_key('"short"') AS inline_string, kb.term_key('<http://example.com/a>') IS NOT NULL;
SELECT kb.term('00000000-0000-0000-0000-000000000000');
SELECT kb.term_key('not a term');

-- the dictionary holds hashed terms only: IRIs, long strings, odd literals
SELECT count(*) AS hashed_terms FROM kb.terms;
SELECT count(*) AS named_graphs FROM kb.graphs;

-- blank nodes: each load's are its own
SELECT kb.load('_:x <http://example.com/p> "one" .', 'ntriples');
SELECT kb.load('_:x <http://example.com/p> "two" .', 'ntriples');
SELECT count(DISTINCT s) AS two_blank_nodes FROM kb.quads WHERE p = kb.term_key('<http://example.com/p>');

-- a syntax error loads nothing
SELECT kb.load('<http://example.com/z> <http://example.com/p> .', 'ntriples');
SELECT kb.load('x', 'nosuchformat');

-- dumps
SELECT kb.dump('ntriples', 'http://example.com/g1');
SELECT kb.dump('nquads', 'http://example.com/g2');
SELECT kb.dump('turtle');
SELECT length(kb.dump('trig')) > 0 AS dumped;

-- the store is transactional
BEGIN;
SELECT kb.sparql_update('CLEAR ALL');
SELECT count(*) FROM kb.quads;
ROLLBACK;
SELECT count(*) > 0 AS still_there FROM kb.quads;

SELECT (kb.stats(true))->'quads' AS quads, (kb.stats(true))->'named_graphs' AS named_graphs;

-- a deleted quad leaves its terms, until kb.vacuum_terms()
SELECT kb.sparql_update('DELETE WHERE { <http://example.com/a> <http://example.com/long> ?o }');
SELECT kb.vacuum_terms() > 0 AS vacuumed;
SELECT count(*) FROM kb.terms WHERE value = 'a literal longer than fourteen bytes';
