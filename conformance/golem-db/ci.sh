#!/usr/bin/env bash
# CI driver for the golem-db conformance kit; also usable locally.
#
#   ci.sh setup <harness-ref> <golem-db-ref>
#       Clone golem-db-conformance-testing next to this repository (as
#       ../golem-db-conformance-testing), check out <harness-ref> with its
#       golem-db submodule at <golem-db-ref> (commit or branch), and apply
#       the kit. Fails clearly if the patch no longer applies.
#   ci.sh tests
#       Run the ported golem-db tests with the default retention and with
#       ARKIV_STATE_RETENTION=0, checked against expected-failures.txt.
#   ci.sh smoke
#       Build the release harness and run small scenarios on all three
#       backends. Only the exit status matters; this is not a benchmark.
#
# HARNESS_DIR overrides the harness location.
set -euo pipefail

KIT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$KIT/../.." && pwd)"
HARNESS_DIR="${HARNESS_DIR:-$(dirname "$REPO")/golem-db-conformance-testing}"
HARNESS_URL=https://github.com/golemfactory/golem-db-conformance-testing.git
GOLEM_DB_URL=https://github.com/golemfactory/golem-db.git
EXPECTED_TOTAL=21
SMOKE_SCENARIOS=(scenarios/block-size/2-blocks-of-100.toml scenarios/commit-growth.toml)
SMOKE_BACKENDS=(golemdb arkiv arkiv-memory)

fetch_ref() { # <dir> <url> <ref>: check out a commit or branch tip
    git -C "$1" fetch --quiet "$2" "$3"
    git -C "$1" -c advice.detachedHead=false checkout --quiet FETCH_HEAD
}

setup() {
    local harness_ref="$1" golem_ref="$2"
    if [ "$(basename "$REPO")" != arkiv-state-db-experiment ]; then
        echo "error: the harness patch expects this repository at ../arkiv-state-db-experiment (found $REPO)" >&2
        exit 1
    fi
    if [ ! -d "$HARNESS_DIR/.git" ]; then
        git init --quiet "$HARNESS_DIR"
    fi
    fetch_ref "$HARNESS_DIR" "$HARNESS_URL" "$harness_ref"
    git -C "$HARNESS_DIR" submodule update --init --quiet golem-db
    fetch_ref "$HARNESS_DIR/golem-db" "$GOLEM_DB_URL" "$golem_ref"
    echo "harness:  $(git -C "$HARNESS_DIR" log -1 --format='%H %s')"
    echo "golem-db: $(git -C "$HARNESS_DIR/golem-db" log -1 --format='%H %s')"

    cd "$HARNESS_DIR"
    if ! git apply --check "$KIT/harness.patch"; then
        echo "::error::conformance/golem-db/harness.patch no longer applies to harness $harness_ref; the kit must be ported" >&2
        exit 1
    fi
    git apply "$KIT/harness.patch"
    cp "$KIT/src/arkiv.rs" "$KIT/src/golem_tests.rs" crates/harness/src/
    cp -r "$KIT/scenarios-scaled" .
}

tests() {
    cd "$HARNESS_DIR"
    cargo test -p golemdb-harness golem_tests --no-run
    local status=0 retention
    for retention in "" 0; do
        echo "::group::ported tests, ARKIV_STATE_RETENTION='${retention}'"
        ARKIV_STATE_RETENTION="$retention" python3 "$KIT/check-tests.py" \
            --expected-failures "$KIT/expected-failures.txt" \
            --expected-total "$EXPECTED_TOTAL" -- \
            cargo test -p golemdb-harness golem_tests --no-fail-fast -- --test-threads=1 \
            || status=1
        echo "::endgroup::"
        [ "$status" = 0 ] || echo "::error::ported golem-db tests diverged from expected-failures.txt (ARKIV_STATE_RETENTION='${retention}')"
    done
    return "$status"
}

smoke() {
    cd "$HARNESS_DIR"
    cargo build --release -p golemdb-harness
    local db_dir backend scenario
    db_dir="$(mktemp -d)"
    for backend in "${SMOKE_BACKENDS[@]}"; do
        for scenario in "${SMOKE_SCENARIOS[@]}"; do
            echo "::group::$backend $scenario"
            GOLEMDB_BACKEND="$backend" GOLEMDB_DB_DIR="$db_dir" cargo load "$scenario"
            echo "::endgroup::"
        done
    done
    rm -rf "$db_dir"
}

case "${1:-}" in
    setup) shift; [ $# = 2 ] || { echo "usage: $0 setup <harness-ref> <golem-db-ref>" >&2; exit 2; }; setup "$@" ;;
    tests) tests ;;
    smoke) smoke ;;
    *) echo "usage: $0 setup <harness-ref> <golem-db-ref> | tests | smoke" >&2; exit 2 ;;
esac
