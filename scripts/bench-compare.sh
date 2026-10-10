#!/usr/bin/env bash
# Unattended benchmark runner: golem-db (MDBX) at one or two revisions vs.
# this engine at an old and a new revision, on the golem-db conformance
# harness, plus an object-cache sweep and a reader/producer workload. See
# docs/benchmarking.md.
#
# Usage:
#   scripts/bench-compare.sh [--all | --quick | --scenarios LIST] [--runs N]
#       [--out DIR] [--new-rev REV] [--old-rev REV] [--golem-rev REV]
#       [--golem-old-rev REV] [--golem-new-rev REV] [--engines LIST]
#       [--resume DIR] [--max-wait-load MIN] [--load-threshold X]
#       [--cpus LIST] [--timeout MIN] [--full-large] [--no-cache-sweep]
#       [--no-readers] [--dry-run]
#
# Exit codes: 0 all runs succeeded; 1 some runs failed (summary still
# written); 2 prepare/build failed or bad usage; 3 another instance holds the
# lock.

set -uo pipefail

# ---------------------------------------------------------------- defaults --
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO="${BENCH_REPO:-/home/ubuntu/arkiv-network/arkiv-state-db-experiment}"
WORK="${BENCH_WORK:-/home/ubuntu/arkiv-bench}"
CONF_URL="https://github.com/golemfactory/golem-db-conformance-testing.git"
CONF_REV="686ddfc"
CONF_LOCAL_SEED="${BENCH_CONF_SEED:-/tmp/claude-1000/-home-ubuntu-arkiv-network-arkiv-state-db-experiment/450179c7-8b6e-4354-9e17-57fd9bab9836/scratchpad/conf/repo}"

MODE="all"
SCENARIO_FILTER=""
RUNS=5
RUNS_SET=0
OUT=""
NEW_REV=""
OLD_REV="e230fb3"
GOLEM_REV=""
GOLEM_OLD_REV=""
GOLEM_NEW_REV=""
GOLEM_SHA=""
GOLEM_OLD_SHA=""
GOLEM_NEW_SHA=""
ENGINES=""
RESUME=""
MAX_WAIT_LOAD=120
MAX_WAIT_SET=0
LOAD_THRESHOLD="1.5"
CPUS="2-9"
RUN_TIMEOUT_MIN=60
FULL_LARGE=0
CACHE_SWEEP=1
READERS=1
DRY_RUN=0
JOBS="${CARGO_BUILD_JOBS:-4}"
MIN_FREE_GB=30

export PATH="$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:${PATH:-}"
export PYTHONDONTWRITEBYTECODE=1

usage() { sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --all) MODE="all" ;;
    --quick) MODE="quick" ;;
    --scenarios) MODE="list"; SCENARIO_FILTER="${2:-}"; shift ;;
    --runs) RUNS="${2:-}"; RUNS_SET=1; shift ;;
    --out) OUT="${2:-}"; shift ;;
    --new-rev) NEW_REV="${2:-}"; shift ;;
    --old-rev) OLD_REV="${2:-}"; shift ;;
    --golem-rev) GOLEM_REV="${2:-}"; shift ;;
    --golem-old-rev) GOLEM_OLD_REV="${2:-}"; shift ;;
    --golem-new-rev) GOLEM_NEW_REV="${2:-}"; shift ;;
    --engines) ENGINES="${2:-}"; shift ;;
    --resume) RESUME="${2:-}"; shift ;;
    --max-wait-load) MAX_WAIT_LOAD="${2:-}"; MAX_WAIT_SET=1; shift ;;
    --load-threshold) LOAD_THRESHOLD="${2:-}"; shift ;;
    --cpus) CPUS="${2:-}"; shift ;;
    --timeout) RUN_TIMEOUT_MIN="${2:-}"; shift ;;
    --full-large) FULL_LARGE=1 ;;
    --no-cache-sweep) CACHE_SWEEP=0 ;;
    --no-readers) READERS=0 ;;
    --dry-run) DRY_RUN=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

case "$RUNS" in ''|*[!0-9]*) echo "--runs needs a number" >&2; exit 2 ;; esac
case "$MAX_WAIT_LOAD" in ''|*[!0-9]*) echo "--max-wait-load needs minutes" >&2; exit 2 ;; esac
case "$RUN_TIMEOUT_MIN" in ''|*[!0-9]*) echo "--timeout needs minutes" >&2; exit 2 ;; esac

# --resume: reuse the original configuration (revisions, mode, runs).
if [ -n "$RESUME" ]; then
  if [ ! -f "$RESUME/config.env" ]; then
    echo "--resume: $RESUME/config.env not found" >&2; exit 2
  fi
  saved_wait="$MAX_WAIT_LOAD"; saved_wait_set="$MAX_WAIT_SET"; saved_dry="$DRY_RUN"
  # shellcheck disable=SC1091
  . "$RESUME/config.env"
  OUT="$RESUME"
  DRY_RUN="$saved_dry"
  if [ "$saved_wait_set" = 1 ]; then MAX_WAIT_LOAD="$saved_wait"; fi
fi

if [ -n "$GOLEM_REV" ] && { [ -n "$GOLEM_OLD_REV" ] || [ -n "$GOLEM_NEW_REV" ]; }; then
  echo "--golem-rev cannot be combined with --golem-old-rev/--golem-new-rev" >&2; exit 2
fi

# Engines. Two golem revisions give golem-old and golem-new; otherwise golem.
if [ -n "$GOLEM_OLD_REV" ] || [ -n "$GOLEM_NEW_REV" ]; then
  TWO_GOLEM=1; KNOWN_ENGINES="golem-old golem-new arkiv-old arkiv-new arkiv-mem"
else
  TWO_GOLEM=0; KNOWN_ENGINES="golem arkiv-old arkiv-new arkiv-mem"
fi
[ -n "$ENGINES" ] || ENGINES="$KNOWN_ENGINES"
ENGINES="$(echo "$ENGINES" | tr ',' ' ' | xargs)"
for e in $ENGINES; do
  case " $KNOWN_ENGINES " in *" $e "*) ;; *)
    echo "--engines: unknown engine '$e' (known: $KNOWN_ENGINES)" >&2; exit 2 ;;
  esac
done
[ -n "$ENGINES" ] || { echo "--engines: empty list" >&2; exit 2; }
has_engine() { case " $ENGINES " in *" $1 "*) return 0 ;; esac; return 1; }

if [ "$MODE" = "quick" ]; then
  [ "$RUNS_SET" = 1 ] || RUNS=1
  [ "$MAX_WAIT_SET" = 1 ] || MAX_WAIT_LOAD=0
fi

[ -n "$OUT" ] || OUT="$WORK/results/$(date -u +%Y%m%dT%H%M%SZ)"

# ----------------------------------------------------------------- logging --
LOG=""
STATUS_FILE=""
PHASE="init"
PROGRESS="0/0"
FAILURES=0
START_EPOCH="$(date +%s)"

log() {
  local line
  line="$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"
  if [ -n "$LOG" ]; then echo "$line" >>"$LOG"; fi
  echo "$line" >&2
}

status() {
  [ -n "$STATUS_FILE" ] || return 0
  [ $# -gt 0 ] && PHASE="$1"
  {
    echo "phase=$PHASE"
    echo "progress=$PROGRESS"
    echo "failures=$FAILURES"
    echo "started=$(date -u -d "@$START_EPOCH" +%Y-%m-%dT%H:%M:%SZ)"
    echo "updated=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "pid=$$"
    if [ -n "${EXIT_CODE:-}" ]; then echo "exit_code=$EXIT_CODE"; fi
  } >"$STATUS_FILE.tmp" 2>/dev/null
  mv -f "$STATUS_FILE.tmp" "$STATUS_FILE" 2>/dev/null || true
}

EXIT_CODE=""
CURRENT_DB=""
finish() {
  local rc=$?
  [ -n "$EXIT_CODE" ] || EXIT_CODE=$rc
  if [ -n "$CURRENT_DB" ] && [ -d "$CURRENT_DB" ]; then rm -rf "$CURRENT_DB"; fi
  status "${PHASE}"
  log "exit code $EXIT_CODE (phase $PHASE)"
}

die() {
  log "FATAL: $*"
  EXIT_CODE=2
  status "failed:$PHASE"
  exit 2
}

# ------------------------------------------------------------ scenarios ----
# id|path relative to the harness root
ALL_SCENARIOS=(
  "index-layout/1-no-index|scenarios/index-layout/1-no-index.toml"
  "index-layout/2-same-term|scenarios/index-layout/2-same-term.toml"
  "index-layout/3-16-terms|scenarios/index-layout/3-16-terms.toml"
  "index-layout/4-unique-terms|scenarios/index-layout/4-unique-terms.toml"
  "index-layout/5-16-attributes|scenarios/index-layout/5-16-attributes.toml"
  "block-size/1-blocks-of-10|scenarios/block-size/1-blocks-of-10.toml"
  "block-size/2-blocks-of-100|scenarios/block-size/2-blocks-of-100.toml"
  "block-size/3-blocks-of-1000|scenarios/block-size/3-blocks-of-1000.toml"
  "block-size/4-blocks-of-5000|scenarios/block-size/4-blocks-of-5000.toml"
  "commit-growth|scenarios/commit-growth.toml"
  "concurrent-writers|scenarios/concurrent-writers.toml"
  "scaled/large-2m-small|scenarios-scaled/large-2m-small.toml"
  "scaled/large-realistic-small|scenarios-scaled/large-realistic-small.toml"
)
FULL_LARGE_SCENARIOS=(
  "large/large-2m|scenarios-full/large-2m.toml"
  "large/large-realistic|scenarios-full/large-realistic.toml"
)
MEM_SCENARIOS=" commit-growth block-size/2-blocks-of-100 scaled/large-2m-small "
SWEEP_SCENARIOS=" commit-growth scaled/large-2m-small "
SWEEP_MIBS="8 32 128"
SWEEP_RUNS=3
READER_BLOCKS="100 1000"
READER_THREADS="0 1 4 8"
READER_RUNS=3
READER_OPS=50000
READER_PRELOAD=20000

if [ "$MODE" = "quick" ]; then
  SCENARIO_FILTER="commit-growth,block-size/1"
  SWEEP_MIBS="32"
  SWEEP_RUNS=1
  READER_BLOCKS="100"
  READER_THREADS="0 2"
  READER_RUNS=1
  READER_OPS=5000
  READER_PRELOAD=5000
fi

SELECTED=()
select_scenarios() {
  local entry id item
  local pool=("${ALL_SCENARIOS[@]}")
  if [ "$FULL_LARGE" = 1 ]; then pool+=("${FULL_LARGE_SCENARIOS[@]}"); fi
  for entry in "${pool[@]}"; do
    id="${entry%%|*}"
    if [ -z "$SCENARIO_FILTER" ]; then
      SELECTED+=("$entry"); continue
    fi
    local IFS=','
    for item in $SCENARIO_FILTER; do
      [ -n "$item" ] || continue
      if [ "$id" = "$item" ] || [ "${id#"$item"}" != "$id" ]; then
        SELECTED+=("$entry"); break
      fi
    done
  done
  # --full-large together with a filter: also allow large/* ids by name.
  if [ "${#SELECTED[@]}" -eq 0 ]; then
    die "no scenarios match '$SCENARIO_FILTER'"
  fi
}

scenario_path() { # id -> relative path
  local entry
  for entry in "${ALL_SCENARIOS[@]}" "${FULL_LARGE_SCENARIOS[@]}"; do
    if [ "${entry%%|*}" = "$1" ]; then echo "${entry#*|}"; return 0; fi
  done
  return 1
}

# --------------------------------------------------------------- git helpers --
git_with_token() {
  # Runs git with the workspace GitHub token as an HTTP header, passed through
  # GIT_CONFIG_* environment variables (never argv, never logged).
  local token=""
  local envfile="/home/ubuntu/arkiv-network/.env"
  if [ -r "$envfile" ]; then
    token="$(sed -n 's/^GITHUB_TOKEN=//p' "$envfile" | head -n1 | tr -d '"'"'"'\r')"
  fi
  [ -n "$token" ] || return 1
  local header
  header="AUTHORIZATION: basic $(printf 'x-access-token:%s' "$token" | base64 -w0)"
  # timeout cannot run a shell function, so it wraps git directly here.
  GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0="http.https://github.com/.extraheader" \
    GIT_CONFIG_VALUE_0="$header" GIT_TERMINAL_PROMPT=0 timeout 120 git "$@"
}

resolve_revs() {
  if [ -z "$NEW_REV" ]; then
    log "fetching origin/main in $REPO (optional)"
    if GIT_TERMINAL_PROMPT=0 timeout 120 git -C "$REPO" fetch -q origin main >>"$LOG" 2>&1 \
       || git_with_token -C "$REPO" fetch -q origin main >>"$LOG" 2>&1; then
      NEW_REV="origin/main"
    else
      log "WARN: fetch failed; using local main"
      NEW_REV="main"
    fi
  fi
  OLD_SHA="$(git -C "$REPO" rev-parse --verify -q "$OLD_REV^{commit}")" || die "old rev $OLD_REV not found in $REPO"
  NEW_SHA="$(git -C "$REPO" rev-parse --verify -q "$NEW_REV^{commit}")" || die "new rev $NEW_REV not found in $REPO"
  OLD_SHORT="${OLD_SHA:0:12}"
  NEW_SHORT="${NEW_SHA:0:12}"
  log "old rev $OLD_REV = $OLD_SHA; new rev $NEW_REV = $NEW_SHA"
}

# ----------------------------------------------------------------- prepare --
export_rev() { # sha -> $WORK/src/arkiv-<short>
  local sha="$1" short="${1:0:12}"
  local dir="$WORK/src/arkiv-$short"
  if [ -f "$dir/.export-ok" ]; then
    log "export $short: reusing $dir"
  else
    log "export $short: git archive into $dir"
    rm -rf "$dir" && mkdir -p "$dir" || die "cannot create $dir"
    git -C "$REPO" archive --format=tar "$sha" | tar -x -C "$dir" \
      || die "git archive of $sha failed"
    echo "$sha" >"$dir/.export-ok"
  fi
  # The reader workload example, when this revision does not carry it.
  local ex="crates/arkiv-db-redb/examples/bench_readers.rs"
  if [ ! -f "$dir/$ex" ] || [ -f "$dir/.bench-readers-injected" ]; then
    if [ -f "$SCRIPT_REPO/$ex" ]; then
      if ! cmp -s "$SCRIPT_REPO/$ex" "$dir/$ex"; then
        cp "$SCRIPT_REPO/$ex" "$dir/$ex" && touch "$dir/.bench-readers-injected"
        log "export $short: injected $ex from $SCRIPT_REPO"
      fi
    fi
  fi
}

prepare_conformance() {
  local dst="$WORK/src/golem-db-conformance-testing"
  if [ -f "$dst/.copy-ok" ]; then
    log "conformance repo: reusing $dst"
  else
    rm -rf "$dst"
    if [ -d "$CONF_LOCAL_SEED/.git" ] && [ -d "$CONF_LOCAL_SEED/golem-db" ]; then
      log "conformance repo: copying local clone $CONF_LOCAL_SEED"
      mkdir -p "$dst"
      (cd "$CONF_LOCAL_SEED" && tar -c --exclude=./target --exclude=./golem-db/target \
         --exclude=./results . ) | (cd "$dst" && tar -x) || die "copy of $CONF_LOCAL_SEED failed"
    else
      log "conformance repo: cloning $CONF_URL"
      timeout 1800 git clone -q --recurse-submodules "$CONF_URL" "$dst" >>"$LOG" 2>&1 \
        || die "clone of $CONF_URL failed"
    fi
    local head
    head="$(git -C "$dst" rev-parse HEAD 2>/dev/null)"
    if [ "${head:0:7}" != "$CONF_REV" ]; then
      log "conformance repo: checkout $CONF_REV (was ${head:0:12})"
      git -C "$dst" checkout -q "$CONF_REV" >>"$LOG" 2>&1 || die "checkout $CONF_REV failed"
    fi
    git -C "$dst" submodule update --init -q >>"$LOG" 2>&1 || die "submodule update failed"
    git -C "$dst" reset -q --hard >>"$LOG" 2>&1
    git -C "$dst" submodule foreach -q git reset -q --hard >>"$LOG" 2>&1
    git -C "$dst" rev-parse HEAD >"$dst/.copy-ok"
  fi
  # Seed the shared target dir with the old clone's build (registry deps).
  if [ ! -d "$WORK/target" ] && [ -d "$CONF_LOCAL_SEED/target/release" ]; then
    log "seeding $WORK/target from $CONF_LOCAL_SEED/target"
    cp -a "$CONF_LOCAL_SEED/target" "$WORK/target" 2>>"$LOG" || rm -rf "$WORK/target"
  fi
}

# golem-db revisions resolve in the conformance copy's golem-db submodule;
# a revision that is missing there is fetched from origin once.
CONF_DIR="$WORK/src/golem-db-conformance-testing"
golem_resolve() { # rev -> sha on stdout
  local rev="$1" g="$CONF_DIR/golem-db" sha
  if sha="$(git -C "$g" rev-parse --verify -q "$rev^{commit}" 2>/dev/null)"; then echo "$sha"; return 0; fi
  log "golem-db: $rev not found locally; fetching origin"
  GIT_TERMINAL_PROMPT=0 timeout 120 git -C "$g" fetch -q origin >>"$LOG" 2>&1 \
    || git_with_token -C "$g" fetch -q origin >>"$LOG" 2>&1 || true
  git -C "$g" rev-parse --verify -q "$rev^{commit}" 2>/dev/null
}

golem_pinned() { # the harness's submodule commit
  git -C "$CONF_DIR" ls-tree HEAD golem-db 2>/dev/null | awk '{print $3}'
}

resolve_golem_revs() {
  local pinned
  pinned="$(golem_pinned)"
  [ -n "$pinned" ] || die "cannot read the golem-db submodule commit of $CONF_DIR"
  if [ "$TWO_GOLEM" = 1 ]; then
    [ -n "$GOLEM_OLD_REV" ] || GOLEM_OLD_REV="$pinned"
    [ -n "$GOLEM_NEW_REV" ] || GOLEM_NEW_REV="$pinned"
    GOLEM_OLD_SHA="$(golem_resolve "$GOLEM_OLD_REV")" || die "golem-db rev $GOLEM_OLD_REV not found"
    GOLEM_NEW_SHA="$(golem_resolve "$GOLEM_NEW_REV")" || die "golem-db rev $GOLEM_NEW_REV not found"
    log "golem-old $GOLEM_OLD_REV = $GOLEM_OLD_SHA; golem-new $GOLEM_NEW_REV = $GOLEM_NEW_SHA"
  else
    [ -n "$GOLEM_REV" ] || GOLEM_REV="$pinned"
    GOLEM_SHA="$(golem_resolve "$GOLEM_REV")" || die "golem-db rev $GOLEM_REV not found"
    log "golem $GOLEM_REV = $GOLEM_SHA"
  fi
}

# golem-db commit for an engine's harness. The arkiv engines are built with
# the newest configured golem-db (golem-new, or golem).
engine_golem_sha() {
  case "$1" in
    golem-old) echo "$GOLEM_OLD_SHA" ;;
    golem-new) echo "$GOLEM_NEW_SHA" ;;
    golem) echo "$GOLEM_SHA" ;;
    *) if [ "$TWO_GOLEM" = 1 ]; then echo "$GOLEM_NEW_SHA"; else echo "$GOLEM_SHA"; fi ;;
  esac
}

# arkiv commit for an engine's harness. golem engines use the new revision.
engine_arkiv_sha() {
  if [ "$1" = "arkiv-old" ]; then echo "$OLD_SHA"; else echo "$NEW_SHA"; fi
}

harness_id() { # arkiv sha, golem sha -> <arkiv12>-g<golem12>
  echo "${1:0:12}-g${2:0:12}"
}

kit_hash() { # export dir -> hash of its kit
  (cd "$1/conformance/golem-db" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)
}

build_harness() { # arkiv sha, golem sha
  local sha="$1" gsha="$2"
  local short
  short="$(harness_id "$sha" "$gsha")"
  local src="$WORK/src/arkiv-${sha:0:12}"
  local h="$WORK/harness/h-$short"
  local bin="$WORK/bin/harness-$short"
  local stamp
  stamp="sha=$sha kit=$(kit_hash "$src") conf=$CONF_REV golem=$gsha"
  if [ -x "$bin" ] && [ -f "$h/.bench-stamp" ] && [ "$(cat "$h/.bench-stamp")" = "$stamp" ]; then
    log "harness $short: up to date ($bin)"
    return 0
  fi
  log "harness $short: preparing $h"
  rm -rf "$h" && mkdir -p "$h" || die "cannot create $h"
  (cd "$CONF_DIR" && tar -c --exclude=./target \
     --exclude=./golem-db/target .) | (cd "$h" && tar -x) || die "copy of harness failed"
  # The copy carries the submodule's git dir (.git/modules/golem-db).
  git -C "$h/golem-db" checkout -q --detach "$gsha" >>"$LOG" 2>&1 \
    || die "harness $short: checkout of golem-db $gsha failed"
  git -C "$h/golem-db" reset -q --hard >>"$LOG" 2>&1
  [ "$(git -C "$h/golem-db" rev-parse HEAD 2>/dev/null)" = "$gsha" ] \
    || die "harness $short: golem-db is not at $gsha"
  # Cargo keys path dependencies by their path relative to the workspace, so
  # every harness copy shares golem-db's units in the one target dir, and it
  # trusts mtimes. tar keeps old mtimes: mark the sources new so a build never
  # reuses another golem-db revision's objects.
  find "$h/golem-db" -path "$h/golem-db/.git" -prune -o -type f -exec touch {} + 2>>"$LOG"
  local kit="$src/conformance/golem-db"
  (cd "$h" && git apply "$kit/harness.patch") >>"$LOG" 2>&1 || die "harness $short: git apply harness.patch failed"
  cp "$kit"/src/*.rs "$h/crates/harness/src/" || die "harness $short: copying kit sources failed"
  cp -r "$kit/scenarios-scaled" "$h/" || die "harness $short: copying scenarios-scaled failed"
  sed -i "s|\.\./\.\./\.\./arkiv-state-db-experiment|$src|g" "$h/crates/harness/Cargo.toml"
  grep -q "$src/crates/arkiv-db" "$h/crates/harness/Cargo.toml" || die "harness $short: path rewrite failed"
  mkdir -p "$h/scenarios-full"
  local f
  for f in "$h"/scenarios/large/*.toml; do
    [ -f "$f" ] || continue
    sed '/^profile_at/d' "$f" >"$h/scenarios-full/$(basename "$f")"
  done
  if [ -f "$WORK/src/harness-Cargo.lock.pinned" ]; then
    cp "$WORK/src/harness-Cargo.lock.pinned" "$h/Cargo.lock"
  fi
  local blog="$OUT/prepare/build-harness-$short.log"
  log "harness $short: cargo build --release (jobs $JOBS), log $blog"
  status "prepare:build-harness-$short"
  if ! (cd "$h" && CARGO_BUILD_JOBS="$JOBS" CARGO_TARGET_DIR="$WORK/target" \
          nice -n 10 cargo build --release -p golemdb-harness) >"$blog" 2>&1; then
    log "harness $short: online build failed; retrying --offline"
    (cd "$h" && CARGO_BUILD_JOBS="$JOBS" CARGO_TARGET_DIR="$WORK/target" \
        nice -n 10 cargo build --release --offline -p golemdb-harness) >>"$blog" 2>&1 \
      || die "harness $short: build failed (see $blog)"
  fi
  mkdir -p "$WORK/bin"
  cp "$WORK/target/release/golemdb-harness" "$bin.tmp" && mv -f "$bin.tmp" "$bin" \
    || die "harness $short: copying binary failed"
  [ -f "$WORK/src/harness-Cargo.lock.pinned" ] || cp "$h/Cargo.lock" "$WORK/src/harness-Cargo.lock.pinned"
  echo "$stamp" >"$h/.bench-stamp"
  log "harness $short: built $bin"
}

READERS_OK_OLD=0
READERS_OK_NEW=0
build_readers() { # sha -> sets READERS_OK_*; never fatal
  local sha="$1" short="${1:0:12}"
  local src="$WORK/src/arkiv-$short"
  local bin="$WORK/bin/bench_readers-$short"
  local ex="$src/crates/arkiv-db-redb/examples/bench_readers.rs"
  [ -f "$ex" ] || { log "readers $short: example missing"; return 1; }
  local stamp
  stamp="sha=$sha ex=$(sha256sum "$ex" | cut -c1-16)"
  if [ -x "$bin" ] && [ -f "$bin.stamp" ] && [ "$(cat "$bin.stamp")" = "$stamp" ]; then
    log "readers $short: up to date ($bin)"
    return 0
  fi
  local blog="$OUT/prepare/build-readers-$short.log"
  log "readers $short: building bench_readers, log $blog"
  status "prepare:build-readers-$short"
  if (cd "$src" && CARGO_BUILD_JOBS="$JOBS" CARGO_TARGET_DIR="$WORK/target-arkiv" \
        nice -n 10 cargo build --release -p arkiv-db-redb --example bench_readers) >"$blog" 2>&1 \
     || (cd "$src" && CARGO_BUILD_JOBS="$JOBS" CARGO_TARGET_DIR="$WORK/target-arkiv" \
        nice -n 10 cargo build --release --offline -p arkiv-db-redb --example bench_readers) >>"$blog" 2>&1; then
    mkdir -p "$WORK/bin"
    cp "$WORK/target-arkiv/release/examples/bench_readers" "$bin.tmp" && mv -f "$bin.tmp" "$bin"
    echo "$stamp" >"$bin.stamp"
    log "readers $short: built $bin"
    return 0
  fi
  log "WARN: readers $short: bench_readers does not build at this revision (see $blog); skipping its reader runs"
  return 1
}

# Engines whose reader workload runs (arkiv-old/arkiv-new among ENGINES).
reader_engines() {
  local e out=""
  for e in arkiv-old arkiv-new; do has_engine "$e" && out="$out $e"; done
  echo "${out# }"
}

prepare() {
  status "prepare"
  mkdir -p "$WORK/src" "$WORK/harness" "$WORK/bin" "$WORK/db" "$OUT/prepare" || die "mkdir failed"
  local e pair pairs="" a g
  for e in $ENGINES; do
    a="$(engine_arkiv_sha "$e")"; g="$(engine_golem_sha "$e")"
    case " $pairs " in *" $a:$g "*) ;; *) pairs="$pairs $a:$g" ;; esac
  done
  export_rev "$NEW_SHA"
  if has_engine arkiv-old && [ "$OLD_SHA" != "$NEW_SHA" ]; then export_rev "$OLD_SHA"; fi
  for pair in $pairs; do
    build_harness "${pair%%:*}" "${pair#*:}"
  done
  if [ "$READERS" = 1 ]; then
    if has_engine arkiv-new && build_readers "$NEW_SHA"; then READERS_OK_NEW=1; fi
    if has_engine arkiv-old; then
      if [ "$NEW_SHA" = "$OLD_SHA" ] && has_engine arkiv-new; then READERS_OK_OLD=$READERS_OK_NEW
      elif build_readers "$OLD_SHA"; then READERS_OK_OLD=1; fi
    fi
  fi
  local hn
  hn="$WORK/harness/h-$(harness_id "$NEW_SHA" "$(engine_golem_sha arkiv-new)")"
  {
    echo "old_rev=$OLD_REV"; echo "old_sha=$OLD_SHA"
    echo "new_rev=$NEW_REV"; echo "new_sha=$NEW_SHA"
    echo "engines=$ENGINES"
    if [ "$TWO_GOLEM" = 1 ]; then
      echo "golem_old_rev=$GOLEM_OLD_REV"; echo "golem_old_sha=$GOLEM_OLD_SHA"
      echo "golem_new_rev=$GOLEM_NEW_REV"; echo "golem_new_sha=$GOLEM_NEW_SHA"
    else
      echo "golem_rev=$GOLEM_REV"; echo "golem_sha=$GOLEM_SHA"
    fi
    echo "golem_pinned_sha=$(golem_pinned)"
    echo "conformance_rev=$(cat "$CONF_DIR/.copy-ok" 2>/dev/null)"
    echo "harness_rustc=$(cd "$hn" 2>/dev/null && rustc -V 2>/dev/null)"
    echo "harness_cargo=$(cd "$hn" 2>/dev/null && cargo -V 2>/dev/null)"
    echo "arkiv_rustc=$(cd "$WORK/src/arkiv-$NEW_SHORT" && rustc -V 2>/dev/null)"
    echo "redb_version=$(grep -A1 '^name = "redb"$' "$hn/Cargo.lock" 2>/dev/null | sed -n 's/^version = "\(.*\)"/\1/p' | tr '\n' ' ')"
    echo "readers_old=$READERS_OK_OLD"; echo "readers_new=$READERS_OK_NEW"
    echo "old_bench_readers_injected=$([ -f "$WORK/src/arkiv-$OLD_SHORT/.bench-readers-injected" ] && echo yes || echo no)"
    echo "new_bench_readers_injected=$([ -f "$WORK/src/arkiv-$NEW_SHORT/.bench-readers-injected" ] && echo yes || echo no)"
  } >"$OUT/meta/revs.txt"
  log "prepare done"
}

# -------------------------------------------------------------------- plan --
# plan.tsv columns: variant scenario engine run
PLAN_LINES=()
shuffled() { # seed words...
  python3 -c 'import random,sys; r=random.Random(int(sys.argv[1])); l=sys.argv[2:]; r.shuffle(l); print(" ".join(l))' "$@"
}

build_plan() {
  local run i entry id engines e mib
  PLAN_LINES=()
  for ((run = 1; run <= RUNS; run++)); do
    i=0
    for entry in "${SELECTED[@]}"; do
      id="${entry%%|*}"
      engines=""
      for e in $ENGINES; do
        if [ "$e" = "arkiv-mem" ]; then
          case "$MEM_SCENARIOS" in *" $id "*) ;; *) continue ;; esac
        fi
        engines="$engines $e"
      done
      [ -n "$engines" ] || { i=$((i + 1)); continue; }
      # shellcheck disable=SC2086
      for e in $(shuffled $((run * 1000 + i)) $engines); do
        PLAN_LINES+=("main	$id	$e	$run")
      done
      i=$((i + 1))
    done
  done
  if [ "$CACHE_SWEEP" = 1 ] && has_engine arkiv-new; then
    for ((run = 1; run <= SWEEP_RUNS; run++)); do
      for entry in "${SELECTED[@]}"; do
        id="${entry%%|*}"
        case "$SWEEP_SCENARIOS" in *" $id "*) ;; *) continue ;; esac
        for mib in $(shuffled $((run * 7919)) $SWEEP_MIBS); do
          PLAN_LINES+=("cache-$mib	$id	arkiv-new	$run")
        done
      done
    done
  fi
  if [ "$READERS" = 1 ] && [ -n "$(reader_engines)" ]; then
    local b t
    for ((run = 1; run <= READER_RUNS; run++)); do
      for b in $READER_BLOCKS; do
        for t in $READER_THREADS; do
          # shellcheck disable=SC2046
          for e in $(shuffled $((run * 31 + b + t)) $(reader_engines)); do
            PLAN_LINES+=("readers	readers/b$b-r$t	$e	$run")
          done
        done
      done
    done
  fi
}

# --------------------------------------------------------------- commands --
# Fills CMD (array) and ENVV (array of VAR=value) for a job.
CMD=()
ENVV=()
job_command() { # variant scenario engine run dbdir
  local variant="$1" id="$2" engine="$3" dbdir="$5"
  local short h rel
  CMD=(); ENVV=()
  if [ "$variant" = "readers" ]; then
    local spec="${id#readers/}" b t
    b="${spec%%-*}"; b="${b#b}"; t="${spec##*-r}"
    if [ "$engine" = "arkiv-old" ]; then short="$OLD_SHORT"; else short="$NEW_SHORT"; fi
    CMD=("$WORK/bin/bench_readers-$short" --dir "$dbdir" --readers "$t" --block-ops "$b"
         --ops "$READER_OPS" --preload "$READER_PRELOAD")
    return 0
  fi
  rel="$(scenario_path "$id")" || return 1
  short="$(harness_id "$(engine_arkiv_sha "$engine")" "$(engine_golem_sha "$engine")")"
  h="$WORK/harness/h-$short"
  ENVV=("GOLEMDB_DB_DIR=$dbdir")
  case "$engine" in
    golem|golem-old|golem-new) ;;
    arkiv-old|arkiv-new) ENVV+=("GOLEMDB_BACKEND=arkiv") ;;
    arkiv-mem) ENVV+=("GOLEMDB_BACKEND=arkiv-memory") ;;
  esac
  case "$variant" in
    cache-*)
      local mib="${variant#cache-}"
      ENVV+=("ARKIV_OBJECT_CACHE_MIB=$mib" "ARKIV_OBJECT_CACHE_ENTRIES=$((mib / 8 * 16384))")
      ;;
  esac
  CMD=("$WORK/bin/harness-$short" "$h/$rel")
}

job_key() { # variant scenario engine run
  echo "$1__${2//\//_}__$3__r$4"
}

job_timeout_min() { # scenario
  case "$1" in large/*) echo $((RUN_TIMEOUT_MIN > 360 ? RUN_TIMEOUT_MIN : 360)) ;; *) echo "$RUN_TIMEOUT_MIN" ;; esac
}

print_plan() {
  local line variant id engine run key dbdir
  echo "# bench-compare plan ($(date -u +%Y-%m-%dT%H:%M:%SZ))"
  echo "# out=$OUT work=$WORK repo=$REPO"
  echo "# old=$OLD_REV (${OLD_SHA:-?}) new=$NEW_REV (${NEW_SHA:-?}) conformance=$CONF_REV"
  if [ "$TWO_GOLEM" = 1 ]; then
    echo "# golem-old=$GOLEM_OLD_REV (${GOLEM_OLD_SHA:-?}) golem-new=$GOLEM_NEW_REV (${GOLEM_NEW_SHA:-?}) engines=$ENGINES"
  else
    echo "# golem=${GOLEM_REV:-pinned} (${GOLEM_SHA:-?}) engines=$ENGINES"
  fi
  echo "# mode=$MODE runs=$RUNS cpus=$CPUS timeout=${RUN_TIMEOUT_MIN}m load-gate<$LOAD_THRESHOLD up to ${MAX_WAIT_LOAD}m full-large=$FULL_LARGE"
  echo "# prepare: copy/clone conformance@$CONF_REV -> $WORK/src/golem-db-conformance-testing;"
  echo "#   git archive old/new -> $WORK/src/arkiv-<sha>; harness copies $WORK/harness/h-<sha>-g<golem sha>"
  echo "#   with each arkiv rev's kit and golem-db checked out at the engine's golem rev;"
  echo "#   cargo build --release -p golemdb-harness (CARGO_BUILD_JOBS=$JOBS, CARGO_TARGET_DIR=$WORK/target);"
  echo "#   cargo build --release -p arkiv-db-redb --example bench_readers (CARGO_TARGET_DIR=$WORK/target-arkiv)"
  echo "# each job: sync; sudo -n drop_caches; cd \$OUT/runs/<key>; env ... timeout -k 60 <T>m taskset -c $CPUS /usr/bin/time -v <cmd>; du -sb db; rm -rf db"
  echo "# jobs: ${#PLAN_LINES[@]}"
  for line in "${PLAN_LINES[@]}"; do
    IFS=$'\t' read -r variant id engine run <<<"$line"
    key="$(job_key "$variant" "$id" "$engine" "$run")"
    dbdir="$WORK/db/$key"
    job_command "$variant" "$id" "$engine" "$run" "$dbdir" || { echo "$key: UNKNOWN SCENARIO"; continue; }
    printf '%s\t%s %s\n' "$key" "${ENVV[*]:-}" "${CMD[*]}"
  done
}

# ------------------------------------------------------------- measurement --
SUDO_OK=0
drop_caches() {
  sync
  if [ "$SUDO_OK" = 1 ]; then
    echo 3 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null 2>&1 || true
  fi
}

write_meta() { # file
  {
    echo "== date"; date -u +%Y-%m-%dT%H:%M:%SZ
    echo "== loadavg"; cat /proc/loadavg
    echo "== uptime"; uptime
    echo "== top10 by cpu"; ps -eo pid,user,pcpu,pmem,rss,etime,comm --sort=-pcpu | head -n 11
    echo "== free -m"; free -m
    echo "== df"; df -h "$WORK" / 2>/dev/null
    echo "== uname"; uname -a
    echo "== revs"; cat "$OUT/meta/revs.txt" 2>/dev/null
    echo "== cpus"; echo "taskset $CPUS; nproc $(nproc)"
  } >"$1" 2>&1
}

load5() { awk '{print $2}' /proc/loadavg; }

load_gate() {
  status "load-gate"
  local waited=0 l
  l="$(load5)"
  if [ "$MAX_WAIT_LOAD" -le 0 ]; then
    echo "load gate skipped (max wait 0); 5-min load $l" >"$OUT/meta/load-gate.txt"
    log "load gate skipped; 5-min load $l"
    return 0
  fi
  while :; do
    l="$(load5)"
    if awk -v l="$l" -v t="$LOAD_THRESHOLD" 'BEGIN{exit !(l < t)}'; then
      echo "load gate passed after ${waited} min; 5-min load $l < $LOAD_THRESHOLD" >"$OUT/meta/load-gate.txt"
      log "load gate passed after ${waited} min (load $l)"
      return 0
    fi
    if [ "$waited" -ge "$MAX_WAIT_LOAD" ]; then
      echo "load gate timed out after ${waited} min; 5-min load $l >= $LOAD_THRESHOLD; proceeding anyway" >"$OUT/meta/load-gate.txt"
      log "WARN: load gate timed out after ${waited} min (load $l); proceeding"
      write_meta "$OUT/meta/load-gate-timeout-machine.txt"
      return 0
    fi
    log "load gate: 5-min load $l >= $LOAD_THRESHOLD; waiting (${waited}/${MAX_WAIT_LOAD} min)"
    sleep 60
    waited=$((waited + 1))
  done
}

run_job() { # variant scenario engine run index total
  local variant="$1" id="$2" engine="$3" run="$4" idx="$5" total="$6"
  local key rundir dbdir rc tmin avail_gb
  key="$(job_key "$variant" "$id" "$engine" "$run")"
  rundir="$OUT/runs/$key"
  PROGRESS="$idx/$total"
  if [ -f "$rundir/done" ]; then
    log "[$idx/$total] $key: already done, skipping"
    return 0
  fi
  status "measure:$key"
  rm -rf "$rundir"; mkdir -p "$rundir"
  dbdir="$WORK/db/$key"
  rm -rf "$dbdir"
  if ! job_command "$variant" "$id" "$engine" "$run" "$dbdir"; then
    log "[$idx/$total] $key: unknown scenario"; echo "unknown scenario" >"$rundir/failed"; FAILURES=$((FAILURES + 1)); return 1
  fi
  if [ ! -x "${CMD[0]}" ]; then
    log "[$idx/$total] $key: binary ${CMD[0]} missing; skipped"
    echo "binary missing" >"$rundir/failed"; echo 127 >"$rundir/exit"
    FAILURES=$((FAILURES + 1)); return 1
  fi
  avail_gb=$(( $(df -Pk "$WORK" | awk 'NR==2{print $4}') / 1048576 ))
  if [ "$avail_gb" -lt "$MIN_FREE_GB" ]; then
    log "[$idx/$total] $key: only ${avail_gb} GiB free on $WORK (< $MIN_FREE_GB); skipped"
    echo "low disk: ${avail_gb} GiB" >"$rundir/failed"; echo 28 >"$rundir/exit"
    FAILURES=$((FAILURES + 1)); return 1
  fi
  mkdir -p "$dbdir"
  CURRENT_DB="$dbdir"
  tmin="$(job_timeout_min "$id")"
  python3 -c 'import json,sys; print(json.dumps(dict(zip(["variant","scenario","engine","run","key","timeout_min"], sys.argv[1:]))))' \
    "$variant" "$id" "$engine" "$run" "$key" "$tmin" >"$rundir/job.json"
  printf '%s\n' "${ENVV[@]}" >"$rundir/env.txt"
  printf '%q ' env "${ENVV[@]}" timeout -k 60 "${tmin}m" taskset -c "$CPUS" /usr/bin/time -v -o "$rundir/time.txt" "${CMD[@]}" >"$rundir/cmd.txt"
  echo >>"$rundir/cmd.txt"
  write_meta "$rundir/meta.txt"
  drop_caches
  log "[$idx/$total] $key: start (load $(load5))"
  local t0 t1
  t0="$(date +%s.%N)"
  (cd "$rundir" && exec env "${ENVV[@]}" timeout -k 60 "${tmin}m" taskset -c "$CPUS" \
     /usr/bin/time -v -o "$rundir/time.txt" "${CMD[@]}") >"$rundir/out.log" 2>&1 </dev/null
  rc=$?
  t1="$(date +%s.%N)"
  echo "$rc" >"$rundir/exit"
  awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.3f\n", b - a}' >"$rundir/wall_secs"
  du -sb "$dbdir" 2>/dev/null | awk '{print $1}' >"$rundir/disk_bytes"
  rm -rf "$dbdir"
  CURRENT_DB=""
  if [ "$rc" -eq 0 ]; then
    touch "$rundir/done"
    rm -f "$rundir/failed"
  else
    if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then echo "timeout after ${tmin} min (rc $rc)" >"$rundir/failed"
    else echo "exit code $rc" >"$rundir/failed"; fi
    FAILURES=$((FAILURES + 1))
  fi
  python3 -I "$SCRIPT_DIR/bench/parse_run.py" "$rundir" >>"$OUT/results.jsonl" 2>>"$LOG" || log "WARN: parse of $key failed"
  log "[$idx/$total] $key: rc=$rc $(tail -n1 "$OUT/results.jsonl" 2>/dev/null | python3 -c 'import json,sys
try:
  d=json.loads(sys.stdin.read()); print("ops/s=%s secs=%s" % (d.get("ops_per_s") or d.get("producer_ops_per_s"), d.get("wall_secs")))
except Exception: print("")' 2>/dev/null)"
  return 0
}

# --------------------------------------------------------------------- main --
select_scenarios

if [ "$DRY_RUN" = 1 ]; then
  if [ -z "${OLD_SHA:-}" ]; then
    OLD_SHA="$(git -C "$REPO" rev-parse --verify -q "$OLD_REV^{commit}" || echo "?")"
    if [ -z "$NEW_REV" ]; then NEW_REV="origin/main"; fi
    NEW_SHA="$(git -C "$REPO" rev-parse --verify -q "$NEW_REV^{commit}" || echo "?")"
  fi
  OLD_SHORT="${OLD_SHA:0:12}"; NEW_SHORT="${NEW_SHA:0:12}"
  if [ -z "$GOLEM_SHA$GOLEM_OLD_SHA$GOLEM_NEW_SHA" ]; then
    # Resolve only from what is already on disk; a dry run fetches nothing.
    p="$(golem_pinned)"
    lr() { git -C "$CONF_DIR/golem-db" rev-parse --verify -q "$1^{commit}" 2>/dev/null || echo "?"; }
    if [ "$TWO_GOLEM" = 1 ]; then
      GOLEM_OLD_SHA="$(lr "${GOLEM_OLD_REV:-$p}")"; GOLEM_NEW_SHA="$(lr "${GOLEM_NEW_REV:-$p}")"
    else
      GOLEM_SHA="$(lr "${GOLEM_REV:-$p}")"
    fi
  fi
  build_plan
  print_plan
  exit 0
fi

mkdir -p "$OUT/meta" "$OUT/runs" "$OUT/prepare" || { echo "cannot create $OUT" >&2; exit 2; }
LOG="$OUT/runner.log"
STATUS_FILE="$OUT/STATUS"
mkdir -p "$WORK"
exec 9>"$WORK/.bench-compare.lock"
if ! flock -n 9; then
  log "another bench-compare instance holds $WORK/.bench-compare.lock; exiting"
  EXIT_CODE=3; status "locked"; exit 3
fi
trap finish EXIT
trap 'log "interrupted"; EXIT_CODE=130; exit 130' INT TERM HUP

log "bench-compare start: mode=$MODE runs=$RUNS out=$OUT resume=${RESUME:-no} pid=$$"
write_meta "$OUT/meta/machine-start.txt"

status "prepare"
mkdir -p "$WORK/src" || die "mkdir $WORK/src failed"
prepare_conformance
if [ -z "$RESUME" ]; then
  resolve_revs
  resolve_golem_revs
else
  log "resume: revisions from config.env: old $OLD_SHA new $NEW_SHA golem ${GOLEM_SHA:-$GOLEM_OLD_SHA $GOLEM_NEW_SHA}"
  OLD_SHORT="${OLD_SHA:0:12}"; NEW_SHORT="${NEW_SHA:0:12}"
fi
{
  printf 'MODE=%q\nSCENARIO_FILTER=%q\nRUNS=%q\nOLD_REV=%q\nNEW_REV=%q\nOLD_SHA=%q\nNEW_SHA=%q\n' \
    "$MODE" "$SCENARIO_FILTER" "$RUNS" "$OLD_REV" "$NEW_REV" "$OLD_SHA" "$NEW_SHA"
  printf 'GOLEM_REV=%q\nGOLEM_OLD_REV=%q\nGOLEM_NEW_REV=%q\nGOLEM_SHA=%q\nGOLEM_OLD_SHA=%q\nGOLEM_NEW_SHA=%q\nENGINES=%q\n' \
    "$GOLEM_REV" "$GOLEM_OLD_REV" "$GOLEM_NEW_REV" "$GOLEM_SHA" "$GOLEM_OLD_SHA" "$GOLEM_NEW_SHA" "$ENGINES"
  printf 'MAX_WAIT_LOAD=%q\nLOAD_THRESHOLD=%q\nCPUS=%q\nRUN_TIMEOUT_MIN=%q\nFULL_LARGE=%q\nCACHE_SWEEP=%q\nREADERS=%q\n' \
    "$MAX_WAIT_LOAD" "$LOAD_THRESHOLD" "$CPUS" "$RUN_TIMEOUT_MIN" "$FULL_LARGE" "$CACHE_SWEEP" "$READERS"
} >"$OUT/config.env"

if sudo -n true 2>/dev/null; then SUDO_OK=1; log "sudo -n available: dropping page cache before each run"
else log "sudo -n unavailable: page cache not dropped"; fi

prepare

build_plan
print_plan >"$OUT/plan.txt"
log "plan: ${#PLAN_LINES[@]} jobs (see $OUT/plan.txt)"

load_gate

total=${#PLAN_LINES[@]}
idx=0
for line in "${PLAN_LINES[@]}"; do
  idx=$((idx + 1))
  IFS=$'\t' read -r variant id engine run <<<"$line"
  if [ "$variant" = "readers" ]; then
    if { [ "$engine" = "arkiv-old" ] && [ "$READERS_OK_OLD" != 1 ]; } || \
       { [ "$engine" = "arkiv-new" ] && [ "$READERS_OK_NEW" != 1 ]; }; then
      log "[$idx/$total] readers $id $engine: bench_readers unavailable at this rev; skipped"
      continue
    fi
  fi
  run_job "$variant" "$id" "$engine" "$run" "$idx" "$total"
done

status "summary"
write_meta "$OUT/meta/machine-end.txt"
if python3 -I "$SCRIPT_DIR/bench/summarize.py" "$OUT" >>"$LOG" 2>&1; then
  log "summary written: $OUT/summary.md"
else
  log "WARN: summary generation failed"
fi

PROGRESS="$total/$total"
if [ "$FAILURES" -gt 0 ]; then
  log "finished with $FAILURES failed run(s)"
  EXIT_CODE=1; status "done-with-failures"; exit 1
fi
EXIT_CODE=0
status "done"
exit 0
