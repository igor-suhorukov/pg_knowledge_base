#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# A server for a leg (docs/design.md §7), sourced: the extension built and
# installed, a cluster made in the run's directory with the module
# preloaded, started, and stopped when the leg exits.  PGHOST and PGPORT are
# set for psql.  KB_SETTINGS adds lines to postgresql.conf; KB_HBA, when
# set, replaces pg_hba.conf.
. "$KB_SRC/test/build.sh"
PGDATA="$KB_WORK/data"
export PGHOST=/tmp PGPORT=5432 PGDATABASE=postgres PGDATA
rm -rf "$PGDATA"
initdb -D "$PGDATA" -A trust -E UTF8 --locale=C.UTF-8 >"$KB_WORK/initdb.log" 2>&1
cat >>"$PGDATA/postgresql.conf" <<CONF
shared_preload_libraries = 'pg_knowledge_base'
unix_socket_directories = '/tmp'
listen_addresses = '${KB_LISTEN:-}'
max_connections = 40
max_worker_processes = 40
log_min_messages = warning
lc_messages = 'C'
${KB_SETTINGS:-}
CONF
if [ -n "${KB_HBA:-}" ]; then
	printf '%s\n' "$KB_HBA" >"$PGDATA/pg_hba.conf"
fi
pg_ctl -D "$PGDATA" -l "$KB_WORK/server.log" -w start >/dev/null
trap 'pg_ctl -D "$PGDATA" -m fast -w stop >/dev/null 2>&1 || true' EXIT

# pg_regress over a directory of sql/ and expected/, the tests named
regress() {
	local dir="$1"; shift
	local name
	name="$(basename "$dir")"
	mkdir -p "$KB_WORK/$name"
	cp -a "$dir/." "$KB_WORK/$name/"
	createdb "kb_$name"
	"$(dirname "$(pg_config --pgxs)")/../test/regress/pg_regress" \
		--inputdir="$KB_WORK/$name" --outputdir="$KB_WORK/$name" \
		--use-existing --dbname="kb_$name" "$@"
}
