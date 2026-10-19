#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# The otel leg, in a dev container (docs/design.md §2.8, §7): pg_otel_probe
# built from its snapshot at /otelprobe and preloaded beside the module, its
# agent (test/run.sh otel-agent, at /engine) writing the spans to Arrow IPC
# files, and test/otel/otel_checks.py, which runs SPARQL through SQL and the
# endpoint and reads the spans back.
set -euo pipefail

[ -f /otelprobe/Makefile ] || { echo "otel.sh: no pg_opentelemetry_probe at /otelprobe" >&2; exit 1; }
[ -x /engine/pg_otel_probe_agent ] || {
	echo "otel.sh: no agent at /engine/pg_otel_probe_agent (test/run.sh otel-agent)" >&2
	exit 1
}
PROBE_BUILD=/tmp/pg_otel_probe
rm -rf "$PROBE_BUILD"
mkdir -p "$PROBE_BUILD"
cp -a /otelprobe/. "$PROBE_BUILD/"
make -C "$PROBE_BUILD" -s -j"${MAKE_JOBS:-8}" AGENT=no PG_CONFIG=pg_config >"$KB_WORK/probe-build.log" 2>&1 || {
	cat "$KB_WORK/probe-build.log"
	exit 1
}
make -C "$PROBE_BUILD" -s AGENT=no PG_CONFIG=pg_config install >>"$KB_WORK/probe-build.log" 2>&1
echo "otel.sh: pg_otel_probe built and installed"

mkdir -p "$KB_WORK/ipc"
export KB_HBA="local all all trust
host all all 127.0.0.1/32 trust"
export KB_SETTINGS="shared_preload_libraries = 'pg_otel_probe, pg_knowledge_base'
pg_otel_probe.agent_path = '/engine/pg_otel_probe_agent'
pg_otel_probe.ipc_dir = '$KB_WORK/ipc'
pg_otel_probe.enable_otlp = off
kb.endpoint_listen_addresses = '127.0.0.1'
kb.endpoint_port = 7878
${KB_SETTINGS:-}"
. "$KB_SRC/test/server.sh"
/opt/kbtest/bin/python -I "$KB_SRC/test/otel/otel_checks.py" "$KB_WORK"
