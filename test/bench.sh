#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# The bench leg, in a dev container (docs/design.md §6): a server with the
# endpoint on 127.0.0.1 and room for the dataset (shared_buffers, no
# checkpoint during the load), oxigraph's CLI at /engine/oxigraph (test/run.sh
# oxigraph builds it), and test/bench/bench.py.
#
#   KB_BENCH_PRODUCTS  the dataset's scale: about 73 triples a product
#                      (50000: 3.6 million triples)
#   KB_BENCH_RUNS      the timed runs of each query (5)
set -euo pipefail

[ -x /engine/oxigraph ] || { echo "bench.sh: no oxigraph CLI at /engine/oxigraph (test/run.sh oxigraph)" >&2; exit 1; }
export KB_HBA="local all all trust
host all all 127.0.0.1/32 trust"
export KB_LISTEN=""
export KB_SETTINGS="kb.endpoint_listen_addresses = '127.0.0.1'
kb.endpoint_port = 7878
shared_buffers = '4GB'
max_wal_size = '32GB'
checkpoint_timeout = '1h'
maintenance_work_mem = '1GB'
${KB_SETTINGS:-}"
. "$KB_SRC/test/server.sh"
/opt/kbtest/bin/python -I "$KB_SRC/test/bench/bench.py" "$KB_WORK" "${KB_BENCH_PRODUCTS:-50000}" "${KB_BENCH_RUNS:-5}"
