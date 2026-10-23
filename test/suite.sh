#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# The regression suite, in a dev container (docs/design.md §7): a server
# with the module preloaded (server.sh), and pg_regress over test/regress;
# then the server restarted without the module preloaded, and pg_regress
# over test/nopreload.
#
#   suite.sh          the suite (KB_TESTS: test/regress's tests, all by
#                     default; test/nopreload runs only with all of them)
#   suite.sh build    the build alone
#   suite.sh shell    a shell, the server running
set -euo pipefail

mode="${1:-suite}"
if [ "$mode" = build ]; then
	. "$KB_SRC/test/build.sh"
	exit 0
fi
# the replication test's publication needs logical decoding
KB_SETTINGS="wal_level = logical
${KB_SETTINGS:-}"
. "$KB_SRC/test/server.sh"
if [ "$mode" = shell ]; then
	echo "server running; psql connects"
	bash -i
	exit 0
fi
tests="${KB_TESTS:-$(sed -n 's/^test: //p' "$KB_SRC/test/regress/schedule" | tr '\n' ' ')}"
regress "$KB_SRC/test/regress" $tests

# The SQL functions on a server that does not preload the module
# (docs/design.md §2.7): the same server restarted without it, which
# CREATE EXTENSION then loads into a backend
if [ -z "${KB_TESTS:-}" ]; then
	echo "shared_preload_libraries = ''" >>"$PGDATA/postgresql.conf"
	pg_ctl -D "$PGDATA" -l "$KB_WORK/server.log" -w restart >/dev/null
	regress "$KB_SRC/test/nopreload" nopreload
fi
