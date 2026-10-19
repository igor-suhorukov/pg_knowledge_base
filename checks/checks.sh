#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# The repository's checks, in a container (test/run.sh checks): the C ABI's
# two declarations agree, and the tree's text is English -- no Cyrillic in
# any file.
set -euo pipefail
python3 -I "$KB_SRC/checks/abi.py" "$KB_SRC"
if grep -rlP '[\x{0400}-\x{04FF}]' --exclude-dir=.git "$KB_SRC"; then
	echo "checks: the tree's text is English" >&2
	exit 1
fi
