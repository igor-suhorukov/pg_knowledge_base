#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# pg_knowledge_base's legs on the host (docs/design.md §7): the images, and
# each leg in a container of its own (docker/compose.yml), its results in a
# run directory of the cache.  Nothing is built, installed or run against a
# server on the host; it needs docker with Compose v2 and BuildKit.
#
#   run.sh images        the builder and dev images, from Rust's and
#                        PostgreSQL 19's official images; the legs build
#                        them when they are not there
#   run.sh fetch         cargo fetch for the engine, in the builder's
#                        container with the network: the crates, and
#                        oxigraph's repository at the commit
#                        engine/Cargo.toml names, into the cache;
#                        engine/Cargo.lock updated when the manifest has
#                        changed.  engine, enginetest and oxigraph run it
#                        first when the cache lacks what the two name
#   run.sh engine        the engine library, ~/.cache/pg_accel/pg_knowledge_base/engine
#   run.sh enginetest    the engine's own unit tests (no server)
#   run.sh build         the extension built and installed in a dev container
#   run.sh checks        the C ABI's two declarations agree (checks/abi.py)
#   run.sh suite         the regression suite (test/regress), then
#                        test/nopreload on the server restarted without
#                        the module preloaded
#   run.sh r2rml         the W3C RDB2RDF test cases (R2RML and the Direct
#                        Mapping), Ontop's copy of them at the pinned commit
#                        fetched into the cache
#   run.sh sparql        the W3C SPARQL 1.1 query and update test cases,
#                        from w3c/rdf-tests at the commit oxigraph's
#                        testsuite pins
#   run.sh endpoint      the SPARQL endpoint: the protocol, logins, TLS,
#                        sessions, cancels, from local and remote clients
#   run.sh oxigraph      oxigraph's own CLI and server, with its RocksDB,
#                        from the engine's commit of oxigraph: the bench
#                        leg's other side (~/.cache/pg_accel/pg_knowledge_base/engine/lib/oxigraph)
#   run.sh otel-agent    pg_otel_probe's agent from the pinned commit of its
#                        repository (~/.cache/pg_accel/pg_knowledge_base/engine/lib/pg_otel_probe_agent);
#                        run only when named
#   run.sh otel          pg_knowledge_base in pg_otel_probe's traces: the
#                        probe built from the same commit, its agent's IPC
#                        files read back (test/otel); run only when named,
#                        or by all with KB_OTEL=1
#   run.sh bench         storage, load and query times against oxigraph's
#                        own store and server (test/bench), the oxigraph leg
#                        run first when its CLI is not of the engine's
#                        commit; KB_BENCH_PRODUCTS sets the dataset's scale
#   run.sh repro         a scratch script: KB_RUN's repro.sql on a server
#                        of its own
#   run.sh shell         a dev container's shell, the server running
#   run.sh all           checks, engine, suite, r2rml, sparql, endpoint; the
#                        otel legs only with KB_OTEL=1
#
# Settings, from the environment:
#   KB_CACHE         the runs: ~/.cache/pg_accel/pg_knowledge_base
#   KB_ENGINE_BUILD  the engine's build tree and libraries: $KB_CACHE/engine
#   KB_CARGO_HOME    cargo's caches, oxigraph's repository among them:
#                    $KB_CACHE/cargo
#   KB_CPUS          each container's CPUs: 0, as many as there are
#   KB_PG_TAG        the official PostgreSQL image's tag the dev image is
#                    built on: 19rc1-trixie
#   KB_UID, KB_GID   the user the builder's containers run as, and the uid
#                    postgres has in the dev image: the host's user
#   KB_TESTS         the suite's tests of test/regress, all by default;
#                    test/nopreload runs only with all of them
#   KB_ONTOP_COMMIT  the commit of github.com/ontop/ontop whose W3C RDB2RDF
#                    test cases the r2rml leg runs
#   KB_ONTOP_SRC     a checkout of Ontop's to read them from instead
#   KB_OTEL          1: all runs the otel legs too (default 0: they run only
#                    when named)
#   KB_OTEL_PROBE_REPO   pg_opentelemetry_probe's repository:
#                        ~/dev/projects/oss_databases/github/pg_opentelemetry_probe
#   KB_OTEL_PROBE_COMMIT the commit the otel legs build, exported by git
#                        archive into the cache
#   KB_RDF_TESTS     w3c/rdf-tests' sparql/ at the commit oxigraph's
#                    testsuite pins (KB_RDF_TESTS_COMMIT): fetched into the
#                    cache by the sparql leg
#   KB_RUN           repro: the run directory that holds repro.sql
set -u -o pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$here/.." && pwd)"
COMPOSE="$ROOT/docker/compose.yml"
export KB_CACHE="${KB_CACHE:-$HOME/.cache/pg_accel/pg_knowledge_base}"
export KB_ENGINE_BUILD="${KB_ENGINE_BUILD:-$KB_CACHE/engine}"
export KB_CARGO_HOME="${KB_CARGO_HOME:-$KB_CACHE/cargo}"
export KB_CPUS="${KB_CPUS:-0}"
export KB_UID="${KB_UID:-$(id -u)}" KB_GID="${KB_GID:-$(id -g)}"
export KB_PG_TAG="${KB_PG_TAG:-19rc1-trixie}"
KB_RDF_TESTS_COMMIT="${KB_RDF_TESTS_COMMIT:-d5d930b03ee6b63b388d0bb28fdb29ee78a59ec0}"
export KB_RDF_TESTS="${KB_RDF_TESTS:-$KB_CACHE/rdf-tests}"
KB_ONTOP_COMMIT="${KB_ONTOP_COMMIT:-964dbb79431fb958fce9de0784424f7c0e35f9bf}"
KB_ONTOP="${KB_ONTOP_SRC:-$KB_CACHE/ontop}"
KB_OTEL_PROBE_REPO="${KB_OTEL_PROBE_REPO:-$HOME/dev/projects/oss_databases/github/pg_opentelemetry_probe}"
KB_OTEL_PROBE_COMMIT="${KB_OTEL_PROBE_COMMIT:-38a52ca36bf9747049e80af469f926a5db899a17}"
export KB_OTEL_PROBE_SRC="${KB_OTEL_PROBE_SRC:-$KB_CACHE/pg_otel_probe/$KB_OTEL_PROBE_COMMIT}"
mkdir -p "$KB_CACHE/runs" "$KB_CACHE/empty" "$KB_ENGINE_BUILD/lib" "$KB_CARGO_HOME"

die() { echo "run.sh: $*" >&2; exit 1; }

# What the containers mount at /rdf-tests, /ontop and /otelprobe: each
# directory once it holds what the legs read, an empty one until then, so
# that docker makes no directory of its own
mount_of() { if [ -e "$1/$2" ]; then echo "$1"; else echo "$KB_CACHE/empty"; fi; }
mounts() {
	KB_RDF_TESTS_MOUNT="$(mount_of "$KB_RDF_TESTS" sparql)"
	KB_ONTOP_MOUNT="$(mount_of "$KB_ONTOP" test/rdb2rdf-compliance)"
	KB_OTEL_MOUNT="$(mount_of "$KB_OTEL_PROBE_SRC" Makefile)"
	export KB_RDF_TESTS_MOUNT KB_ONTOP_MOUNT KB_OTEL_MOUNT
}
mounts

# git_sparse <dir> <repository> <commit> <path>: the commit's <path>, fetched
# once into <dir>, which is made beside it and then moved in place
git_sparse() {
	local dir="$1" url="$2" commit="$3" path="$4"
	[ -e "$dir/$path" ] && return 0
	echo "== $url $commit: $path into $dir"
	rm -rf "$dir.tmp" && mkdir -p "$dir.tmp" &&
		git -C "$dir.tmp" init -q &&
		git -C "$dir.tmp" remote add origin "$url" &&
		git -C "$dir.tmp" sparse-checkout set "$path" &&
		git -C "$dir.tmp" fetch -q --depth 1 --filter=blob:none origin "$commit" &&
		git -C "$dir.tmp" checkout -q FETCH_HEAD || die "cannot fetch $path of $url at $commit"
	rmdir "$dir" 2>/dev/null
	[ -e "$dir" ] && die "cannot fetch into $dir: something else is there"
	mv "$dir.tmp" "$dir" || die "cannot move $dir.tmp to $dir"
	mounts
}

# w3c/rdf-tests' sparql/ at the commit oxigraph's testsuite pins
rdf_tests_checkout() {
	git_sparse "$KB_RDF_TESTS" https://github.com/w3c/rdf-tests.git "$KB_RDF_TESTS_COMMIT" sparql
}
# Ontop's copy of the W3C RDB2RDF test cases at the pinned commit, or in a
# checkout of Ontop's (KB_ONTOP_SRC), which is only read
ontop_checkout() {
	if [ -n "${KB_ONTOP_SRC:-}" ]; then
		[ -d "$KB_ONTOP_SRC/test/rdb2rdf-compliance" ] ||
			die "KB_ONTOP_SRC: no test/rdb2rdf-compliance in $KB_ONTOP_SRC"
		return 0
	fi
	git_sparse "$KB_ONTOP" https://github.com/ontop/ontop.git "$KB_ONTOP_COMMIT" \
		test/rdb2rdf-compliance/src/test/resources
}

# Whether the cargo cache holds what engine/Cargo.toml and engine/Cargo.lock
# name: test/engine.sh fetch leaves there the two it fetched for
engine_fetched() {
	cmp -s "$ROOT/engine/Cargo.toml" "$KB_CARGO_HOME/kb_engine/Cargo.toml" &&
		cmp -s "$ROOT/engine/Cargo.lock" "$KB_CARGO_HOME/kb_engine/Cargo.lock"
}
# Whether oxigraph's CLI is built, and from the commit engine/Cargo.lock
# names
oxigraph_built() {
	[ -x "$KB_ENGINE_BUILD/lib/oxigraph" ] &&
		[ "$(cat "$KB_ENGINE_BUILD/lib/oxigraph.source" 2>/dev/null)" = \
			"$(grep -m 1 -o 'git+https://[^"]*' "$ROOT/engine/Cargo.lock")" ]
}
# Whether the otel legs can run: the probe's snapshot made, or its
# repository there, with the commit, to make it from.  Nothing else of the
# project needs the probe -- the extension builds with its copy of the
# probe's header, and runs without the probe -- and no leg runs them
# unless named, or KB_OTEL=1 asks all to.
otel_available() {
	[ -f "$KB_OTEL_PROBE_SRC/Makefile" ] ||
		git -C "$KB_OTEL_PROBE_REPO" cat-file -e "$KB_OTEL_PROBE_COMMIT^{commit}" 2>/dev/null
}

# pg_opentelemetry_probe at the pinned commit, exported once into the cache
otel_probe_snapshot() {
	[ -f "$KB_OTEL_PROBE_SRC/Makefile" ] && return 0
	otel_available || die "no pg_opentelemetry_probe: no snapshot at $KB_OTEL_PROBE_SRC, and no commit $KB_OTEL_PROBE_COMMIT in $KB_OTEL_PROBE_REPO (KB_OTEL_PROBE_REPO)"
	echo "== pg_opentelemetry_probe $KB_OTEL_PROBE_COMMIT into $KB_OTEL_PROBE_SRC"
	rm -rf "$KB_OTEL_PROBE_SRC.tmp" && mkdir -p "$KB_OTEL_PROBE_SRC.tmp"
	git -C "$KB_OTEL_PROBE_REPO" archive "$KB_OTEL_PROBE_COMMIT" | tar -x -C "$KB_OTEL_PROBE_SRC.tmp" \
		|| die "cannot export pg_opentelemetry_probe $KB_OTEL_PROBE_COMMIT from $KB_OTEL_PROBE_REPO"
	rmdir "$KB_OTEL_PROBE_SRC" 2>/dev/null || true
	mv "$KB_OTEL_PROBE_SRC.tmp" "$KB_OTEL_PROBE_SRC"
	mounts
}
compose() { docker compose -f "$COMPOSE" "$@"; }

# The images, built when they are not there
need_images() {
	docker image inspect pg_knowledge_base/engine-builder:rust1.99 "pg_knowledge_base/dev:$KB_PG_TAG" \
		>/dev/null 2>&1 && return 0
	echo "== images: not there, built"
	compose --profile build build kb-engine-builder kb-dev-vanilla || die "cannot build the images"
}

# leg <name> <service> <command...>: its run's directory under the cache,
# its output kept there too.
leg() {
	local name="$1"; shift
	need_images
	local run="$KB_CACHE/runs/$(date +%Y%m%dT%H%M%S)-$name"
	mkdir -p "$run"
	chmod 777 "$run"
	echo "== $name: $run"
	KB_WORK="$run" compose --profile run run --rm -T "$@" 2>&1 \
		| grep -v -E '^ (Container|Network|Volume) ' | tee "$run/output.log"
	local rc=${PIPESTATUS[0]}
	echo "== $name: $( [ "$rc" = 0 ] && echo passed || echo "FAILED ($rc)")"
	return "$rc"
}

case "${1:-}" in
	images)
		compose --profile build build kb-engine-builder kb-dev-vanilla ;;
	fetch)
		leg fetch engine-fetch /src/test/engine.sh fetch || exit
		# the lockfile cargo resolved, into the tree
		if ! cmp -s "$KB_CARGO_HOME/kb_engine/Cargo.lock" "$ROOT/engine/Cargo.lock"; then
			cp "$KB_CARGO_HOME/kb_engine/Cargo.lock" "$ROOT/engine/Cargo.lock"
			echo "== engine/Cargo.lock: updated"
		fi ;;
	engine)
		engine_fetched || "$0" fetch || exit
		leg engine engine /src/test/engine.sh build ;;
	enginetest)
		engine_fetched || "$0" fetch || exit
		leg enginetest engine /src/test/engine.sh test ;;
	build)
		leg build vanilla /src/test/suite.sh build ;;
	checks)
		leg checks vanilla /src/checks/checks.sh ;;
	suite)
		leg suite vanilla /src/test/suite.sh ;;
	r2rml)
		ontop_checkout
		leg r2rml vanilla /src/test/r2rml.sh ;;
	sparql)
		rdf_tests_checkout
		leg sparql vanilla /src/test/sparql.sh ;;
	endpoint)
		leg endpoint vanilla /src/test/endpoint.sh ;;
	oxigraph)
		engine_fetched || "$0" fetch || exit
		leg oxigraph-fetch engine-fetch /src/test/engine.sh oxigraph-fetch &&
			leg oxigraph engine /src/test/engine.sh oxigraph ;;
	otel-agent)
		otel_probe_snapshot
		leg otel-agent-fetch engine-fetch /src/test/engine.sh otel-agent-fetch &&
			leg otel-agent engine /src/test/engine.sh otel-agent ;;
	otel)
		otel_probe_snapshot
		[ -x "$KB_ENGINE_BUILD/lib/pg_otel_probe_agent" ] || "$0" otel-agent || die "no pg_otel_probe agent"
		leg otel vanilla /src/test/otel.sh ;;
	bench)
		oxigraph_built || "$0" oxigraph || die "no oxigraph CLI for the bench"
		leg bench vanilla /src/test/bench.sh ;;
	repro)
		[ -n "${KB_RUN:-}" ] && [ -f "$KB_RUN/repro.sql" ] || die "KB_RUN: a directory with repro.sql"
		need_images
		KB_WORK="$KB_RUN" compose --profile run run --rm -T vanilla /src/test/repro.sh ;;
	shell)
		need_images
		mkdir -p "$KB_CACHE/runs/shell" && chmod 777 "$KB_CACHE/runs/shell"
		KB_WORK="$KB_CACHE/runs/shell" compose --profile run run --rm vanilla /src/test/suite.sh shell ;;
	all)
		"$0" checks && "$0" engine && "$0" suite && "$0" r2rml && "$0" sparql && "$0" endpoint &&
			if [ "${KB_OTEL:-0}" = 1 ]; then
				"$0" otel
			else
				echo "== otel: not run by default: KB_OTEL=1 test/run.sh all, or test/run.sh otel"
			fi ;;
	*)
		sed -n '3,/^set /p' "$0" | sed 's/^# \{0,1\}//' | head -n -1
		exit 2 ;;
esac
