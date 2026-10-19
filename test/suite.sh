#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# The regression suite, in a dev container (docs/design.md §7): a server
# with the module preloaded (server.sh), and pg_regress over test/regress.
#
#   suite.sh          the suite (KB_TESTS: the tests, all by default)
#   suite.sh build    the build alone
#   suite.sh shell    a shell, the server running
set -euo pipefail

mode="${1:-suite}"
if [ "$mode" = build ]; then
	. "$KB_SRC/test/build.sh"
	exit 0
fi
. "$KB_SRC/test/server.sh"
if [ "$mode" = shell ]; then
	echo "server running; psql connects"
	bash -i
	exit 0
fi
tests="${KB_TESTS:-$(sed -n 's/^test: //p' "$KB_SRC/test/regress/schedule" | tr '\n' ' ')}"
regress "$KB_SRC/test/regress" $tests
