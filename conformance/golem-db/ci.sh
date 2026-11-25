#!/usr/bin/env bash
# CI driver for the golem-db conformance kit; also usable locally.
#
# The crate under test is ../../golem-api (golem-db's API on the arkiv
# engine). It pins golem-db `main` as a git dependency. This script fetches
# the test suites that are written against golem-db's API, points them at
# that crate by renaming one dependency, and checks their results.
#
#   ci.sh setup <golem-db-ref> [<harness-ref>]
#       Check out golem-db at <golem-db-ref> (a full commit SHA or a branch)
#       into work/golemdb, for its crates/conformance, and point that crate
#       at golem-api. With <harness-ref>, also check out
#       golem-db-conformance-testing into work/harness, apply
#       external-harness.patch and point its two crates at golem-api. Fails
#       clearly when a step no longer applies.
#   ci.sh track <golem-db-ref>
#       Rewrite golem-api/Cargo.toml to build against golem-db at
#       <golem-db-ref> instead of the pinned revision. For runs that follow
#       upstream; it leaves the working tree modified.
#   ci.sh kit
#       golem-api's own checks: rustfmt, clippy and its tests, which compare
#       it with golem-db's engine call by call.
#   ci.sh in-repo
#       golem-db's crates/conformance: on golem-db's own engine through
#       golem-api (nothing may fail), then on the arkiv engine, checked
#       against expected-failures/in-repo.txt.
#   ci.sh external
#       The external harness's conformance crate, spec suite and SQLite
#       oracle: on golem-db's engine (expected-failures/external-golemdb.txt)
#       and on the arkiv engine (expected-failures/external.txt).
#   ci.sh smoke
#       The external harness's load tool on small scenarios, on golem-db and
#       on both arkiv backends. Only the exit status matters; this is not a
#       benchmark.
#
# CONFORMANCE_COUNTS=any skips the comparison of how many tests ran and were
# ignored (for runs that follow upstream branches, where tests are added).
set -euo pipefail

KIT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$KIT/../.." && pwd)"
API="$REPO/golem-api"
# Checkouts and build output, ignored by git.
WORK="$KIT/work"
GOLEM_DB_URL=https://github.com/Arkiv-Network/golemdb.git
HARNESS_URL=https://github.com/golemfactory/golem-db-conformance-testing.git
export CARGO_TARGET_DIR="$WORK/target"

# Tests run and ignored at the pinned revisions (.github/workflows/golem-conformance.yml).
IN_REPO_RUN=100
IN_REPO_IGNORED=8
EXTERNAL_RUN=186 # 184 spec, 2 oracle
EXTERNAL_IGNORED=15
if [ "${CONFORMANCE_COUNTS:-pinned}" = any ]; then
    IN_REPO_RUN=any IN_REPO_IGNORED=any EXTERNAL_RUN=any EXTERNAL_IGNORED=any
fi

SMOKE_SCENARIOS=(scenarios/block-size/2-blocks-of-100.toml scenarios/commit-growth.toml "$KIT/scenarios-scaled/queries-small.toml")
SMOKE_BACKENDS=(golemdb arkiv arkiv-memory)

die() {
    echo "::error::$*" >&2
    exit 1
}

checkout() { # <dir> <url> <ref>: a clean checkout of a commit or branch tip
    [ -d "$1/.git" ] || git init --quiet "$1"
    git -C "$1" fetch --quiet "$2" "$3" ||
        die "cannot fetch $3 from $2 (a pinned commit that upstream dropped must be re-pinned)"
    git -C "$1" -c advice.detachedHead=false checkout --quiet --force FETCH_HEAD
    git -C "$1" clean --quiet -fd
}

# Replaces the one line `$2` of file `$1` with a dependency on golem-api.
use_kit_crate() {
    local file="$1" line="$2"
    grep -qxF "$line" "$file" ||
        die "$file no longer has the line '$line'; the kit must be ported"
    local kit="golemdb-api = { package = \"arkiv-golem-api\", path = \"$API\" }"
    sed -i "s|^$(printf '%s' "$line" | sed 's/[][\.*^$]/\\&/g')\$|$kit|" "$file"
    grep -qxF "$kit" "$file" || die "could not rewrite $file"
}

need() { # <dir> <what>
    [ -e "$1" ] || die "$2 is missing: run '$0 setup' first (see README.md)"
}

setup() {
    local golem_ref="$1" harness_ref="${2:-}"
    mkdir -p "$WORK"
    checkout "$WORK/golemdb" "$GOLEM_DB_URL" "$golem_ref"
    echo "golem-db: $(git -C "$WORK/golemdb" log -1 --format='%H %s')"
    if [ -f "$WORK/golemdb/crates/conformance/Cargo.toml" ]; then
        use_kit_crate "$WORK/golemdb/crates/conformance/Cargo.toml" "golemdb-api.workspace = true"
    else
        echo "golem-db $golem_ref has no crates/conformance: only 'kit', 'external' and 'smoke' apply"
    fi

    rm -rf "$WORK/harness"
    if [ -n "$harness_ref" ]; then
        checkout "$WORK/harness" "$HARNESS_URL" "$harness_ref"
        echo "harness:  $(git -C "$WORK/harness" log -1 --format='%H %s')"
        git -C "$WORK/harness" apply --check "$KIT/external-harness.patch" ||
            die "conformance/golem-db/external-harness.patch no longer applies to harness $harness_ref; the kit must be ported"
        git -C "$WORK/harness" apply "$KIT/external-harness.patch"
        local crate
        for crate in conformance harness; do
            use_kit_crate "$WORK/harness/crates/$crate/Cargo.toml" \
                'golemdb-api = { path = "../../golem-db/crates/api" }'
        done
    fi
}

# Points every golem-db dependency of golem-api at one revision. Cargo then
# re-resolves those packages and keeps the rest of Cargo.lock.
track() {
    local rev
    rev="$(git ls-remote "$GOLEM_DB_URL" "$1" | cut -f1 | head -n1)"
    [ -n "$rev" ] || rev="$1" # already a commit
    grep -q 'git = "https://github.com/Arkiv-Network/golemdb", rev = "' "$API/Cargo.toml" ||
        die "golem-api/Cargo.toml no longer pins golem-db by rev; the kit must be ported"
    sed -i "s|\\(git = \"https://github.com/Arkiv-Network/golemdb\", rev = \"\\)[0-9a-f]*\"|\\1$rev\"|" "$API/Cargo.toml"
    echo "golem-api now builds against golem-db $rev"
}

kit() {
    cd "$API"
    cargo fmt -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test
}

# check <name> <backend> <expected-failures> <run> <ignored> -- <command...>
check() {
    local name="$1" backend="$2" list="$3" run="$4" ignored="$5"
    shift 6
    echo "::group::$name"
    local status=0
    GOLEMDB_BACKEND="$backend" python3 "$KIT/check-tests.py" --name "$name" \
        --expected-failures "$KIT/expected-failures/$list" \
        --expected-run "$run" --expected-ignored "$ignored" -- "$@" || status=1
    echo "::endgroup::"
    [ "$status" = 0 ] || echo "::error::$name diverged from expected-failures/$list"
    return "$status"
}

in_repo() {
    need "$WORK/golemdb/crates/conformance" "golem-db's crates/conformance"
    cd "$WORK/golemdb"
    local test=(cargo test -p golemdb-conformance --no-fail-fast)
    "${test[@]}" --no-run
    local status=0
    check "golem-db crates/conformance on golem-db (control)" golemdb none.txt \
        "$IN_REPO_RUN" "$IN_REPO_IGNORED" -- "${test[@]}" || status=1
    check "golem-db crates/conformance on arkiv" arkiv in-repo.txt \
        "$IN_REPO_RUN" "$IN_REPO_IGNORED" -- "${test[@]}" || status=1
    return "$status"
}

external() {
    need "$WORK/harness" "the external harness checkout"
    cd "$WORK/harness"
    local test=(cargo test -p golemdb-conformance --no-fail-fast)
    "${test[@]}" --no-run
    local status=0
    check "external harness on golem-db (control)" golemdb external-golemdb.txt \
        "$EXTERNAL_RUN" "$EXTERNAL_IGNORED" -- "${test[@]}" || status=1
    check "external harness on arkiv" arkiv external.txt \
        "$EXTERNAL_RUN" "$EXTERNAL_IGNORED" -- "${test[@]}" || status=1
    return "$status"
}

smoke() {
    need "$WORK/harness" "the external harness checkout"
    cd "$WORK/harness"
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

usage() {
    echo "usage: $0 setup <golem-db-ref> [<harness-ref>] | track <golem-db-ref> | kit | in-repo | external | smoke" >&2
    exit 2
}

case "${1:-}" in
    setup)
        shift
        [ $# = 1 ] || [ $# = 2 ] || usage
        setup "$@"
        ;;
    track)
        [ $# = 2 ] || usage
        track "$2"
        ;;
    kit) kit ;;
    in-repo) in_repo ;;
    external) external ;;
    smoke) smoke ;;
    *) usage ;;
esac
