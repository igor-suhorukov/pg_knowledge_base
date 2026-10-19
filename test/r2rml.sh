#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# The W3C RDB2RDF test cases, in a dev container (docs/design.md §7): a
# server with the module preloaded (server.sh), and test/r2rml/run_w3c.py
# over Ontop's copy of the test cases, mounted at /ontop.  KB_TESTS names
# test identifiers (R2RMLTC0002a ...) to run alone.
set -euo pipefail

. "$KB_SRC/test/server.sh"
resources=/ontop/test/rdb2rdf-compliance/src/test/resources
[ -d "$resources" ] || { echo "r2rml.sh: no test cases at $resources (KB_ONTOP_SRC)" >&2; exit 1; }
/opt/kbtest/bin/python -I "$KB_SRC/test/r2rml/run_w3c.py" "$resources" "$KB_WORK" ${KB_TESTS:-}
