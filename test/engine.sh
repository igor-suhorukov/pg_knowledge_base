#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# The engine library, built in the builder container (docker/compose.yml,
# services "engine" and "engine-fetch"): engine/ is copied out of the
# read-only tree into /build/src, built against the oxigraph crates cargo
# fetched from the commit engine/Cargo.toml names, and the library left at
# /build/lib/libkb_engine.so.
#
#   engine.sh fetch   cargo fetch, the step that reaches the network: the
#                     crates, and oxigraph's repository at the commit with
#                     its submodules, into /cargo; the manifest and the
#                     lockfile it fetched for left in /cargo/kb_engine, for
#                     test/run.sh to see what the cache holds and to take
#                     the lockfile into the tree
#   engine.sh build   cargo build --offline --locked (the default)
#   engine.sh test    cargo test --offline --locked: the engine's own tests,
#                     which need no server
#   engine.sh otel-agent-fetch, engine.sh otel-agent
#                     pg_otel_probe's agent, from its snapshot at /otelprobe,
#                     for the otel leg: /build/lib/pg_otel_probe_agent
#   engine.sh oxigraph-fetch, engine.sh oxigraph
#                     oxigraph's own CLI and server with its RocksDB, built
#                     from cargo's checkout of oxigraph's repository at the
#                     engine's commit, for the bench leg: /build/lib/oxigraph,
#                     and the source it was built from in
#                     /build/lib/oxigraph.source
#
# engine/Cargo.lock is the lockfile: build and test take it as it is, and
# only fetch resolves it again, when engine/Cargo.toml has changed.
set -euo pipefail

step="${1:-build}"
src=/build/src/engine
profile="${KB_ENGINE_PROFILE:-release}"
# cargo's jobs: KB_ENGINE_JOBS, or the CPUs up to 24
jobs="${KB_ENGINE_JOBS:-$(( $(nproc) < 24 ? $(nproc) : 24 ))}"

rm -rf "$src"
mkdir -p "$src" /build/lib
cp -a /src/engine/. "$src/"
cd "$src"

# oxigraph's source as the lockfile records it: git+<repository>?rev=<commit>#<commit>
oxigraph_source() {
	grep -m 1 -o 'git+https://[^"]*' Cargo.lock
}

# The root of oxigraph's workspace in cargo's checkout of its repository,
# found from the engine's oxrdf
oxigraph_tree() {
	local manifest
	manifest="$(cargo metadata --offline --locked --format-version 1 |
		grep -o '"manifest_path":"[^"]*/git/checkouts/[^"]*/oxrdf/Cargo.toml"' | head -n 1 | cut -d '"' -f 4)"
	[ -n "$manifest" ] || { echo "engine.sh: no checkout of oxigraph's repository (test/run.sh fetch)" >&2; exit 1; }
	dirname "$(cargo locate-project --workspace --message-format plain --manifest-path "$manifest")"
}

case "$step" in
	fetch)
		cargo fetch
		mkdir -p /cargo/kb_engine
		cp Cargo.toml Cargo.lock /cargo/kb_engine/
		echo "engine.sh: fetched $(oxigraph_source)"
		;;
	build)
		cargo build --offline --locked --profile "$profile" -j "$jobs"
		dir="$profile"
		[ "$profile" = dev ] && dir=debug
		cp "/build/target/$dir/libkb_engine.so" /build/lib/libkb_engine.so.tmp
		mv /build/lib/libkb_engine.so.tmp /build/lib/libkb_engine.so
		ls -la /build/lib/libkb_engine.so
		;;
	test)
		cargo test --offline --locked --profile "$profile" -j "$jobs" --lib -- --test-threads=4
		;;
	otel-agent-fetch)
		cargo fetch --locked --manifest-path /otelprobe/agent/Cargo.toml
		;;
	otel-agent)
		CARGO_TARGET_DIR=/build/otel-agent-target cargo build --offline --locked --release \
			--manifest-path /otelprobe/agent/Cargo.toml -j "$jobs"
		cp /build/otel-agent-target/release/pg_otel_probe_agent /build/lib/pg_otel_probe_agent.tmp
		mv /build/lib/pg_otel_probe_agent.tmp /build/lib/pg_otel_probe_agent
		ls -la /build/lib/pg_otel_probe_agent
		;;
	oxigraph-fetch)
		cargo fetch --locked --manifest-path "$(oxigraph_tree)/Cargo.toml"
		;;
	oxigraph)
		tree="$(oxigraph_tree)"
		# The target directory is of one commit: cargo names the workspace's
		# artifacts by their paths in the workspace, the same at every commit,
		# and does not look again at the files of its own checkouts, so that
		# another commit's artifacts would pass for this one's.
		if [ "$(cat /build/oxigraph-target/source 2>/dev/null)" != "$(oxigraph_source)" ]; then
			rm -rf /build/oxigraph-target
			mkdir -p /build/oxigraph-target
			oxigraph_source >/build/oxigraph-target/source
		fi
		CARGO_TARGET_DIR=/build/oxigraph-target cargo build --offline --locked --release \
			--manifest-path "$tree/Cargo.toml" -p oxigraph-cli -j "$jobs"
		cp /build/oxigraph-target/release/oxigraph /build/lib/oxigraph.tmp
		mv /build/lib/oxigraph.tmp /build/lib/oxigraph
		oxigraph_source >/build/lib/oxigraph.source
		ls -la /build/lib/oxigraph
		echo "engine.sh: oxigraph from $(cat /build/lib/oxigraph.source)"
		;;
	*)
		echo "engine.sh: unknown step $step" >&2
		exit 2
		;;
esac
