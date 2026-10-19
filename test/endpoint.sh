#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# The SPARQL endpoint's leg, in a dev container (docs/design.md §7): a
# server with the endpoint on every address and TLS with a certificate made
# here, pg_hba.conf's lines by role, address and TLS, and the clients of
# test/endpoint/test_endpoint.py -- switched off and on again by reloads
# among them -- then the server restarted with the limits' settings for the
# limits phase, and once more with kb.endpoint_listen_addresses empty, the
# endpoint off from the start, for the off phase.
set -euo pipefail

openssl req -x509 -newkey rsa:2048 -nodes -subj /CN=localhost -days 2 \
	-keyout /tmp/endpoint.key -out /tmp/endpoint.crt >/dev/null 2>&1
chmod 600 /tmp/endpoint.key
export KB_HBA="local all all trust
host all postgres 0.0.0.0/0 trust
host all anon 127.0.0.1/32 trust
host all mallory 127.0.0.1/32 reject
host all bob 127.0.0.1/32 scram-sha-256
host all bob 0.0.0.0/0 reject
hostssl all carol 127.0.0.1/32 scram-sha-256
hostnossl all carol 0.0.0.0/0 reject
host all all 0.0.0.0/0 scram-sha-256"
export KB_SETTINGS="kb.endpoint_listen_addresses = '*'
kb.endpoint_port = 7878
kb.endpoint_ssl_cert_file = '/tmp/endpoint.crt'
kb.endpoint_ssl_key_file = '/tmp/endpoint.key'
password_encryption = 'scram-sha-256'
${KB_SETTINGS:-}"
. "$KB_SRC/test/server.sh"
rc=0
/opt/kbtest/bin/python -I "$KB_SRC/test/endpoint/test_endpoint.py" "$KB_WORK" main || rc=1

cat >>"$PGDATA/postgresql.conf" <<CONF
kb.endpoint_max_sessions = 2
kb.endpoint_anonymous_role = 'anon'
kb.endpoint_read_only = on
kb.endpoint_cors = on
CONF
pg_ctl -D "$PGDATA" -m fast -w restart -l "$KB_WORK/server.log" >/dev/null
/opt/kbtest/bin/python -I "$KB_SRC/test/endpoint/test_endpoint.py" "$KB_WORK" limits || rc=1

cat >>"$PGDATA/postgresql.conf" <<CONF
kb.endpoint_listen_addresses = ''
CONF
pg_ctl -D "$PGDATA" -m fast -w restart -l "$KB_WORK/server.log" >/dev/null
/opt/kbtest/bin/python -I "$KB_SRC/test/endpoint/test_endpoint.py" "$KB_WORK" off || rc=1
exit $rc
