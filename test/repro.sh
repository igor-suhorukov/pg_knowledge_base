#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# A scratch leg: the suite's server, a script of psql's against it
# (KB_RUN's repro.sql, or KB_SCRIPT), its output on the leg's.
set -euo pipefail
. "$KB_SRC/test/server.sh"
psql -X -e -f "$KB_WORK/${KB_SCRIPT:-repro.sql}" 2>&1 || true
