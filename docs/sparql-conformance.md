# W3C SPARQL conformance: the 17 tests, and the way to 1007

A report, with recommendations: what keeps the W3C SPARQL 1.1 test cases
from passing in full, and what each would take. Written 2026-10-19; nothing
in it is built.

| | |
|---|---|
| The tests | w3c/rdf-tests at `d5d930b`, the commit oxigraph's testsuite pins: SPARQL 1.0 syntax and evaluation, SPARQL 1.1 query and update, the JSON and the CSV/TSV results (`test/sparql/run_w3c.py`) |
| oxigraph | `1bf3a924c9f` (`0.6.0-dev`); the line numbers below are of that commit. The engine is built since from `f0a06d83d90` of the fork `igor-suhorukov/oxigraph`, where the files cited are the same and the counts too (`runs/20261019T092544-sparql`) |
| pg_knowledge_base | the tree of 2026-10-19 (docs/design.md) |

**How statements are marked.** A statement with a file reference is what
that file does. *Unverified* marks what was not checked.

## 0. Summary

- **Today: 990 pass, 0 fail, 11 differ as expected, 6 skipped**, of 1007
  (`test/run.sh sparql`). oxigraph's own testsuite leaves out or ignores
  the same 17 tests: 1007 of 1007 would claim more than oxigraph does.
- **The 17 fall in four groups** (§1--§4): five the runner does not run,
  though the engine answers them right; ten the store's canonical literals
  keep from passing, by design; two of spareval's, oxigraph's evaluator.
- **The way to 1007**, from the cheapest (§5): the runner (995), a fix of
  date equality in oxigraph (996), a store mode that keeps literals' lexical
  forms (1006), a fix of `BNODE(str)` in oxigraph (1007).

| Group | Tests | Cause | Where the fix is |
|---|---|---|---|
| 1 | `reduced-1`, `reduced-2`, `csv01`, `csv02`, `csv03` | the runner skips them, as oxigraph's testsuite does | `test/sparql/run_w3c.py` |
| 2 | `open-eq-01`, `dawg-str-1`, `dawg-str-2`, `sameTerm-simple`, `sameTerm-eq`, `sameTerm-not-eq`, `eq-graph-1`, `eq-graph-2`, `distinct-1`, `distinct-9` | the store keeps a literal of a datatype oxigraph knows as its value, as oxigraph's store does | `engine/src/key.rs`: a store mode |
| 3 | `date-2` | spareval's `=` on dates with and without a time zone | oxigraph: `oxsdatatypes`, `spareval` |
| 4 | `bnode01` | spareval's `BNODE(str)`, one node for every solution | oxigraph: `spareval` |

## 1. Five tests the runner does not run

`run_w3c.py` skips what oxigraph's testsuite skips (`OXIGRAPH_SKIPS`). The
queries of these five were run against the store by hand, in a scratch run,
and their answers compared with the expected ones: the engine answers all
five right.

- **`reduced-1`, `reduced-2`.** Their manifest entries say
  `mf:resultCardinality mf:LaxCardinality`: a REDUCED query may keep any
  number of a solution's duplicates. The store's answers are the expected
  solutions with the duplicates gone -- `reduced-1` the rows `(x1, "abc")`
  and `(x2, "abc")` where the file has `(x1, "abc")` twice. `reduced-2`'s
  file, of RDF 1.0's days, tells `"abc"` from `"abc"^^xsd:string`; in RDF
  1.1 they are one term, and the store's six values are its nine as RDF 1.1
  reads them.
- **`csv01`, `csv02`, `csv03`** are of type `mf:CSVResultFormatTest`, which
  the runner does not run. The store's CSV (`kb.sparql_text(q, 'csv')`)
  holds the expected values, the quoting of `"4,4"` and the form `1.0E6`
  included; it differs from the expected files only where SPARQL leaves room:
  the order of `SELECT *`'s columns (`o,p,s` against `s,p,o`), the order of
  the rows, and the labels of blank nodes (`_:77cc…` against `_:a`).

**Recommendation.** In `run_w3c.py`: for `mf:LaxCardinality`, compare the
solutions as sets; for `mf:CSVResultFormatTest`, run the query to CSV and
compare its columns by their names, its rows as a multiset, its blank nodes
by a one-to-one renaming. No risk; little work. **995 of 1007.**

## 2. Ten tests the store's canonical literals keep from passing

**The cause.** A term's key is computed from its term alone, and a literal
of a datatype oxigraph knows is kept as its value (`engine/src/key.rs`,
docs/design.md §3.1): `"01"^^xsd:integer` is `"1"^^xsd:integer`,
`"007"^^xsd:int` is `"7"^^xsd:integer`, `"2006-08-23+00:00"^^xsd:date` is
`"2006-08-23Z"^^xsd:date`. oxigraph's own store does the same: its encoder
reads every type derived from `xsd:integer` as an `IntegerLiteral`
(`lib/oxigraph/src/storage/numeric_encoder.rs:604-613`). These ten tests ask
for a literal kept as written -- `sameTerm()` false between `"01"` and
`"1"`, `STR()` giving `"01"`, `DISTINCT` keeping both.

**The fix: a store mode that keeps lexical forms.** A literal gets an inline
key only when its lexical form is its value's canonical one and its datatype
the primitive one; any other literal gets a hashed key of its lexical form
and its datatype, as a literal of an unknown datatype does now. Equal terms
still get equal keys, and nothing else does. Expressions keep their value
semantics -- spareval reads a typed literal's value when it compares or
computes -- so `FILTER(?x = 1)` still holds of `"01"^^xsd:integer`.

What it changes:

- **The store's format.** Keys are the store's format: the mode is a
  property of a store, chosen when it is made and kept in it, never a
  session's setting; a store in the other mode is loaded again.
- **Answers, beside oxigraph's.** A pattern `?x ex:p 1` no longer matches
  `"01"^^xsd:integer`, and `"7"^^xsd:int` keeps its datatype -- as RDF
  defines terms, and otherwise than oxigraph's store answers.
- **Space.** A literal not in canonical form takes a dictionary row. They
  are rare in data; R2RML's natural literals and the values the evaluator
  computes are canonical already.

**Recommendation.** The mode as an option of a store, oxigraph's behaviour
staying the default; the W3C leg run in the strict mode, the regression
suite in both. **1005 of 1007** with §1, in that mode.

## 3. `date-2`: date equality in oxigraph

**The cause.** `FILTER(?v != "2006-08-23"^^xsd:date)` over dates with and
without a time zone. The test expects a comparison that is indeterminate --
a date with a time zone against one without, within 14 hours -- to be an
error, which drops the row: it expects `d4`, `d5`, `dt1`. The store answers
`d2`, `d3`, `d4`, `d5`, `dt1`. In oxsdatatypes, equality of such a pair is
`false` (`lib/oxsdatatypes/src/date_time.rs:1559-1562`, under
`// TODO: implicit timezone`), while the ordering already treats the 14
hours as indeterminate (`date_time.rs:1571-1580`); spareval's `=` on dates
takes the equality (`lib/spareval/src/expression.rs:2373-2376`).

**The fix.** In spareval's `equals()` for date and time values, the
ordering's answer: `None` -- indeterminate -- an error. A few lines, in
oxigraph. **996 of 1007** with §1.

## 4. `bnode01`: `BNODE(str)` in oxigraph

**The cause.** spareval's `BNODE(str)` is the blank node named `str`
(`lib/spareval/src/expression.rs:821-833`, `BlankNode::new(id)`): the same
node for every solution with that string -- and the same as a blank node of
the data named so. SPARQL 1.1 asks for a node distinct from the dataset's,
the same within one solution and another for each solution (§17.4.2.9).
oxigraph's testsuite compares results solution by solution and does not see
it; `run_w3c.py` compares the results whole.

**The fix.** A map from strings to fresh blank nodes, shared by a query's
`BNODE()` calls and started again at each solution. spareval's expression
evaluators do not see where one solution ends and the next begins, so the
change reaches how BIND and SELECT's expressions are evaluated: more than a
few lines, in oxigraph. **1007 of 1007** with §1--§3.

## 5. Recommendations

| Step | What | Where | Then |
|---|---|---|---|
| 1 | `LaxCardinality` and the CSV tests compared | the runner | 995 |
| 2 | Date equality by the ordering | a pull request to oxigraph | 996 |
| 3 | A store mode that keeps lexical forms | `engine/src/key.rs`, the suite | 1006, in that mode |
| 4 | `BNODE(str)` a node a solution | a pull request to oxigraph | 1007 |

After the four, `OXIGRAPH_SKIPS` and `EXPECTED_DIFFERENCES` in `run_w3c.py`
are empty.

**oxigraph's fixes upstream.** §3 and §4 are oxigraph's, and are best made
there, as pull requests; they would end oxigraph's own skips too. The
engine takes them in by moving the `rev`s of `engine/Cargo.toml`: to
oxigraph's commit once merged, or before that to a commit of the fork it is
built from, as it takes the optimizer's work now. The checkout at
`~/dev/projects/oxigraph` holds others' work under way: a fix is made on a
branch or a worktree of its own, never in that tree. Patches kept here --
a copy of oxigraph's crates under `[patch]` -- would depart from
docs/design.md's first rule, that oxigraph's crates are linked and not
copied.

**The test cases' own commit.** Later revisions of w3c/rdf-tests may have
brought some of these expectations to RDF 1.1 -- `reduced-2`'s, for one
(*unverified*); a move of `KB_RDF_TESTS_COMMIT` is worth checking against
this report.
