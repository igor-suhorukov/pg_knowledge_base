#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# The W3C SPARQL test cases, in a dev container (docs/design.md §7): a
# server with the module preloaded (server.sh), and test/sparql/run_w3c.py
# over w3c/rdf-tests at the commit oxigraph's testsuite pins, mounted at
# /rdf-tests.  KB_TESTS names tests (parts of their IRIs) to run alone.
set -euo pipefail

. "$KB_SRC/test/server.sh"
[ -d /rdf-tests/sparql ] || { echo "sparql.sh: no w3c/rdf-tests at /rdf-tests (test/run.sh fetches it)" >&2; exit 1; }
/opt/kbtest/bin/python -I "$KB_SRC/test/sparql/run_w3c.py" /rdf-tests "$KB_WORK" ${KB_TESTS:-}
