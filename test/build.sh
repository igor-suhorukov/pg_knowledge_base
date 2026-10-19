#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# The extension built by PGXS in a copy of the read-only tree, and
# installed into the container's server with the engine library
# (docs/design.md §7).  Sourced by the legs; the copy is at $KB_BUILD.
set -euo pipefail

KB_BUILD="${KB_BUILD:-/tmp/pg_knowledge_base}"
rm -rf "$KB_BUILD"
mkdir -p "$KB_BUILD"
cp -a "$KB_SRC/." "$KB_BUILD/"
rm -rf "$KB_BUILD/engine/target"
make -C "$KB_BUILD" -s -k -j"${MAKE_JOBS:-8}" PG_CONFIG=pg_config COPT="-Werror" >"$KB_WORK/build.log" 2>&1 || {
	cat "$KB_WORK/build.log"
	exit 1
}
make -C "$KB_BUILD" -s install PG_CONFIG=pg_config >>"$KB_WORK/build.log" 2>&1
if [ ! -f "$KB_ENGINE" ]; then
	echo "build.sh: no engine library at $KB_ENGINE: test/run.sh engine" >&2
	exit 1
fi
make -C "$KB_BUILD" -s install-engine PG_CONFIG=pg_config KB_ENGINE="$KB_ENGINE" >>"$KB_WORK/build.log" 2>&1
echo "build.sh: built and installed"
