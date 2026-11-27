#!/bin/bash
# Write-volume test bed for a full node at production size
# (docs/state-write-volume.md): a second full node on a fresh
# `snapshot install`, syncing the same blocks from a local full node.
#
#   state-write-volume.sh run TAG BLOCKS [VAR=VALUE...]
#       install, sync BLOCKS blocks, kill -9, restart, sync 300 more,
#       clean stop, restart
#   state-write-volume.sh torture TAG KILLS HEAD_MINUTES [VAR=VALUE...]
#       install, sync to the head while killing the node KILLS times at
#       random moments, compare a block with the source and with the
#       chain's producer, then measure HEAD_MINUTES at the head
#
# Environment:
#   WV_DIR     work directory (run directory, results); must have room for
#              a copy of the database
#   WV_ENV     a file to source that sets PINS (array: --chain-id,
#              --run-id, --source-id, --genesis-hash, --proposer),
#              SNAPSHOTS (directory with a `latest` snapshot) and PEER (the
#              producer's public URL)
#   WV_SOURCE  the local full node to sync from (default
#              http://127.0.0.1:9510); it must keep every body
#   BIN        the arkiv-proposer binary under test
#   NODE_ARGS  extra node flags, for example "--state-durable-every 64"
# VAR=VALUE arguments are passed to the node's environment. The script
# drops the page cache (sudo) before each start and never sends a
# transaction. Do not edit it while a run executes it.
set -uo pipefail
T=${WV_DIR:?set WV_DIR}
. "${WV_ENV:?set WV_ENV}"
BIN=${BIN:?set BIN}
RUNDIR=$T/run
URL=http://127.0.0.1:9520
MET=http://127.0.0.1:9620/metrics
SRC=${WV_SOURCE:-http://127.0.0.1:9510}
SNAP=$(readlink -f "$SNAPSHOTS/latest")
mkdir -p "$T/out"

drop() { sync; sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'; }
height() { curl -s -m ${2:-5} "$1/signed/v1/status" | python3 -c 'import json,sys; print(json.load(sys.stdin)["head"]["height"])' 2>/dev/null; }
# The status call waits for the node lock, which a cold first import can hold for seconds.
sure_height() { local h; while :; do h=$(height "$1" 60); [ -n "$h" ] && { echo "$h"; return; }; sleep 0.5; done; }
metric() { awk -v m="$1" '$1==m{print $2}' "$2"; }
sample() { # sample PID -> "epoch height write_bytes read_bytes sc_count sc_sum hc_count hc_sum rss_kb"
  local pid=$1 m=$T/out/.metrics
  curl -s -m 5 $MET > $m
  echo "$(date +%s.%N | cut -c1-14) $(sure_height $URL) $(awk '/^write_bytes/{print $2}' /proc/$pid/io) $(awk '/^read_bytes/{print $2}' /proc/$pid/io) \
$(metric 'rogue_one_db_operation_duration_seconds_count{database="state",layer="store",operation="commit"}' $m) \
$(metric 'rogue_one_db_operation_duration_seconds_sum{database="state",layer="store",operation="commit"}' $m) \
$(metric 'rogue_one_db_operation_duration_seconds_count{database="history",layer="store",operation="commit"}' $m) \
$(metric 'rogue_one_db_operation_duration_seconds_sum{database="history",layer="store",operation="commit"}' $m) \
$(awk '/^VmRSS/{print $2}' /proc/$pid/status)"
}
start() { # start LOG VAR=VALUE... ; sets PID, UP (seconds until the status endpoint answers)
  local log=$1; shift
  local t0=$(date +%s.%N)
  local seen=$(wc -l < "$log")
  env "$@" "$BIN" full --run-dir "$RUNDIR" --listen 127.0.0.1:9520 --metrics-listen 127.0.0.1:9620 \
    --peer $SRC "${PINS[@]}" --state-retention 256 --body-retention archive --read-concurrency 64 ${NODE_ARGS:-} \
    >> "$log" 2>&1 &
  PID=$!
  while :; do
    if ! kill -0 $PID 2>/dev/null; then echo "node exited during start"; UP=-1; return 1; fi
    h=$(height $URL); [ -n "$h" ] && break
    sleep 0.2
  done
  UP=$(python3 -c "import time,sys; print(round(time.time()-$t0,1))")
  # The head the node opened at (its own status line, printed before it syncs)
  # and the blocks it replayed from its history to get there.
  FIRST=$(tail -n +$((seen+1)) "$log" | grep '"apiVersion"' | tail -1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["head"]["height"])')
  REPLAYED=$(tail -n +$((seen+1)) "$log" | grep '"event":"history_replay"' | tail -1 | python3 -c 'import json,sys; l=sys.stdin.read().strip(); print(json.loads(l)["blocks"] if l else 0)')
  OPENED=$(tail -n +$((seen+1)) "$log" | grep -E '"event":"store_open"' | tail -1)
  return 0
}
wait_height() { # wait_height TARGET
  while :; do h=$(height $URL); [ -n "$h" ] && [ "$h" -ge "$1" ] && break; kill -0 $PID 2>/dev/null || { echo "node died"; return 1; }; sleep 0.5; done
}
diff_line() { python3 - "$@" <<'PY'
import sys
name=sys.argv[1]; a=sys.argv[2].split(); b=sys.argv[3].split()
f=lambda x: float(x) if x not in ('','None') else 0.0
a=list(map(f,a)); b=list(map(f,b))
dt=b[0]-a[0]; blocks=b[1]-a[1]
wr=(b[2]-a[2])/2**20; rd=(b[3]-a[3])/2**20
sc=b[4]-a[4]; ss=b[5]-a[5]; hc=b[6]-a[6]; hs=b[7]-a[7]
print(f"{name}: blocks={blocks:.0f} seconds={dt:.1f} blocks_per_s={blocks/dt:.1f} written_mib={wr:.0f} mib_per_block={wr/max(blocks,1):.3f} "
      f"read_mib={rd:.0f} state_commit_ms={1000*ss/max(sc,1):.2f} history_commit_ms={1000*hs/max(hc,1):.2f} history_commits_per_block={hc/max(blocks,1):.2f} rss_mib={b[8]/1024:.0f}")
PY
}

case ${1:-} in
run)
  tag=$2; blocks=$3; shift 3
  out=$T/out/$tag; mkdir -p $out; log=$out/node.log; : > $log
  exec > >(tee $out/result.txt) 2>&1
  echo "== $tag  env: $*  args: ${NODE_ARGS:-}  bin: $(basename $BIN)  $(date -u +%FT%TZ)"
  pkill -x "$(basename $BIN)" 2>/dev/null; sleep 1
  rm -rf $RUNDIR; mkdir -p $RUNDIR
  t0=$(date +%s)
  "$BIN" snapshot install --from "$SNAP" --run-dir "$RUNDIR" "${PINS[@]}" --peer $SRC > $out/install.json 2> $out/install.log || { echo "install failed"; tail -3 $out/install.log; exit 1; }
  echo "install_seconds=$(( $(date +%s)-t0 ))"
  drop
  start $log "$@" || exit 1
  echo "first_start: up_seconds=$UP head=$FIRST"
  s0=$(sample $PID); base=$(echo $s0 | cut -d' ' -f2); echo "s0: $s0"
  wait_height $((base+blocks)) || exit 1
  s1=$(sample $PID)
  diff_line sync "$s0" "$s1"
  # hard kill in the middle of the sync
  sleep 3; hk=$(sure_height $URL); kill -9 $PID; wait $PID 2>/dev/null
  echo "killed: head_before_kill=$hk"
  drop
  start $log "$@" || { echo "restart after kill failed"; tail -5 $log; exit 1; }
  [ -n "$OPENED" ] && echo "$OPENED"
  echo "after_kill: up_seconds=$UP head_at_open=$FIRST replayed_from_history=$REPLAYED state_blocks_behind=$((hk-FIRST+REPLAYED)) blocks_lost=$((hk-FIRST))"
  grep -E '"event":"history_replay_failed"' $log | tail -2
  s2=$(sample $PID); base=$(echo $s2 | cut -d' ' -f2)
  wait_height $((base+300)) || exit 1
  s3=$(sample $PID)
  diff_line resync "$s2" "$s3"
  # clean stop and start
  hs=$(sure_height $URL); t0=$(date +%s.%N); kill -TERM $PID; wait $PID 2>/dev/null
  echo "clean_stop: seconds=$(python3 -c "import time; print(round(time.time()-$t0,1))") head_before_stop=$hs"
  drop
  start $log "$@" || { echo "restart after clean stop failed"; tail -5 $log; exit 1; }
  [ -n "$OPENED" ] && echo "$OPENED"
  echo "after_clean_stop: up_seconds=$UP head_at_open=$FIRST blocks_lost=$((hs-FIRST))"
  sleep 5; h=$(sure_height $URL)
  echo "final: head=$h; node $(curl -s -m 5 $URL/signed/v1/status | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["head"]["hash"], d["health"])')"
  kill -TERM $PID; wait $PID 2>/dev/null
  ls -la $RUNDIR | grep redb
  echo "== done $(date -u +%FT%TZ)"
  ;;
torture)
  tag=$2; kills=$3; minutes=$4; shift 4
  out=$T/out/$tag; mkdir -p $out; log=$out/node.log; : > $log
  exec > >(tee $out/result.txt) 2>&1
  echo "== $tag  env: $*  args: ${NODE_ARGS:-}  bin: $(basename $BIN)  $(date -u +%FT%TZ)"
  pkill -x "$(basename $BIN)" 2>/dev/null; sleep 1
  rm -rf $RUNDIR; mkdir -p $RUNDIR
  "$BIN" snapshot install --from "$SNAP" --run-dir "$RUNDIR" "${PINS[@]}" --peer $SRC > $out/install.json 2> $out/install.log || { echo "install failed"; exit 1; }
  drop
  start $log "$@" || exit 1
  echo "first_start: up_seconds=$UP head=$FIRST"
  s0=$(sample $PID); t0=$(date +%s)
  total_replayed=0; worst_up=0
  for k in $(seq 1 $kills); do
    sleep $(( (RANDOM % 20) + 4 )).$(( RANDOM % 10 ))
    hk=$(sure_height $URL); kill -9 $PID; wait $PID 2>/dev/null
    start $log "$@" || { echo "restart $k failed"; tail -5 $log | cut -c1-300; exit 1; }
    total_replayed=$((total_replayed+REPLAYED))
    worst_up=$(python3 -c "print(max($worst_up,$UP))")
    echo "kill $k: head_before_kill=$hk up_seconds=$UP head_at_open=$FIRST replayed_from_history=$REPLAYED blocks_lost=$((hk-FIRST))"
    [ $((hk-FIRST)) -gt 0 ] && { echo "FAIL: blocks lost"; exit 1; }
  done
  # catch up
  while :; do a=$(sure_height $URL); b=$(sure_height $SRC); [ $((b-a)) -le 1 ] && break; kill -0 $PID 2>/dev/null || { echo "node died"; tail -3 $log | cut -c1-300; exit 1; }; sleep 2; done
  s1=$(sample $PID)
  diff_line "catch_up(with $kills kills)" "$s0" "$s1"
  echo "kills=$kills total_replayed=$total_replayed worst_up_seconds=$worst_up"
  h=$(sure_height $URL)
  mine=$(curl -s -m 20 $URL/signed/v1/feed/blocks/$h | sha256sum | cut -c1-16)
  theirs=$(curl -s -m 20 $SRC/signed/v1/feed/blocks/$h | sha256sum | cut -c1-16)
  producer=$(curl -s -m 20 $PEER/signed/v1/feed/blocks/$h | sha256sum | cut -c1-16)
  echo "block $h: test=$mine source=$theirs producer=$producer $([ "$mine" = "$theirs" ] && [ "$mine" = "$producer" ] && echo MATCH || echo MISMATCH)"
  curl -s -m 20 $URL/signed/v1/status | python3 -c 'import json,sys; d=json.load(sys.stdin); print("status:", d["head"]["height"], d["head"]["hash"], d["head"]["stateRoot"], d["health"])'
  # at the head
  s2=$(sample $PID); sleep $((minutes*60)); s3=$(sample $PID)
  diff_line "at_head(${minutes}min)" "$s2" "$s3"
  hs=$(sure_height $URL); kill -TERM $PID; wait $PID 2>/dev/null
  start $log "$@" || exit 1
  echo "after_clean_stop: up_seconds=$UP head_at_open=$FIRST replayed_from_history=$REPLAYED blocks_lost=$((hs-FIRST))"
  kill -TERM $PID; wait $PID 2>/dev/null
  ls -la $RUNDIR | grep redb
  echo "== done $(date -u +%FT%TZ)"
  ;;
*) sed -n 2,26p "$0"; exit 2;;
esac
