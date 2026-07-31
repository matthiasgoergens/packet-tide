#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 tcp|udp|rsync FILE_BYTES RATE_MBIT RTT_MS LOSS_PERCENT SEED" >&2
  exit 2
fi

TRANSPORT=$1
FILE_BYTES=$2
RATE_MBIT=$3
RTT_MS=$4
LOSS_PERCENT=$5
SEED=$6

[[ $TRANSPORT == tcp || $TRANSPORT == udp || $TRANSPORT == rsync ]] || { echo "invalid transport" >&2; exit 2; }
[[ $FILE_BYTES =~ ^[0-9]+$ ]] || { echo "invalid file size" >&2; exit 2; }

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BINARY="$ROOT/target/release/tsunami-udp"
DATA=/tmp/tsunami-udp-lab/data
RESULTS=/tmp/tsunami-udp-lab/results
SOURCE="$DATA/source-$FILE_BYTES.bin"
OUTPUT="$DATA/output-$TRANSPORT.bin"
RECEIVER_LOG="$RESULTS/receiver-$TRANSPORT.log"
SENDER_JSON="$RESULTS/sender-$TRANSPORT.json"
RESULT_JSON="$RESULTS/result-$TRANSPORT-$FILE_BYTES-$RATE_MBIT-$RTT_MS-$LOSS_PERCENT-$SEED.json"
RSYNC_CONFIG="$DATA/rsyncd.conf"
FORWARD_QDISC="$RESULTS/qdisc-forward-$TRANSPORT.json"
REVERSE_QDISC="$RESULTS/qdisc-reverse-$TRANSPORT.json"
HOST_BEFORE="$RESULTS/host-before-$TRANSPORT.json"
HOST_AFTER="$RESULTS/host-after-$TRANSPORT.json"
BLOCK_ID=${TSU_BLOCK_ID:-standalone-$FILE_BYTES-$RATE_MBIT-$RTT_MS-$LOSS_PERCENT-$SEED}
BLOCK_ORDER=${TSU_BLOCK_ORDER:-0}
TREATMENT_ORDER=${TSU_TREATMENT_ORDER:-0}
RANDOMIZATION_SEED=${TSU_RANDOMIZATION_SEED:-0}

mkdir -p "$DATA" "$RESULTS"
truncate -s "$FILE_BYTES" "$SOURCE"
rm -f "$OUTPUT" "$OUTPUT".part.* "$RECEIVER_LOG" "$SENDER_JSON"

"$ROOT/lab/configure-network.sh" "$RATE_MBIT" "$RTT_MS" "$LOSS_PERCENT" 10000 "$SEED"
REPAIR_COOLDOWN_MS=$(awk -v rtt="$RTT_MS" 'BEGIN { printf "%.0f", 2.0 * rtt + 50.0 }')
python3 "$ROOT/lab/host-snapshot.py" >"$HOST_BEFORE"

receiver_pid=''
cleanup_receiver() {
  if [[ -n $receiver_pid ]]; then
    kill "$receiver_pid" 2>/dev/null || true
    wait "$receiver_pid" 2>/dev/null || true
  fi
}
trap cleanup_receiver EXIT INT TERM

if [[ $TRANSPORT == rsync ]]; then
  cat >"$RSYNC_CONFIG" <<EOF
pid file = $DATA/rsyncd.pid
use chroot = no
log file = $DATA/rsyncd.log
[sink]
path = $DATA
read only = no
uid = $(id -u)
gid = $(id -g)
EOF

  timeout --foreground 60s ip netns exec tsu-bench-d \
    rsync --daemon --no-detach --config="$RSYNC_CONFIG" --port=8873 \
    >"$RECEIVER_LOG" 2>&1 &
  receiver_pid=$!

  for _ in {1..50}; do
    if ip netns exec tsu-bench-d ss -ltn | grep -q '10.210.2.2:8873'; then
      break
    fi
    sleep 0.02
  done

  START_NS=$(date +%s%N)
  timeout --foreground 180s ip netns exec tsu-bench-s rsync \
    --whole-file --no-compress --inplace \
    --port=8873 "$SOURCE" "rsync://10.210.2.2/sink/$(basename "$OUTPUT")"
  sync "$OUTPUT"
  cmp --silent "$SOURCE" "$OUTPUT"
  END_NS=$(date +%s%N)
  ELAPSED_NS=$((END_NS - START_NS))
  awk \
    -v bytes="$FILE_BYTES" \
    -v elapsed_ns="$ELAPSED_NS" \
    'BEGIN {
      elapsed_ms = elapsed_ns / 1000000.0
      goodput = bytes * 8.0 / (elapsed_ns / 1000000000.0) / 1000000.0
      printf "{\"transport\":\"rsync\",\"bytes\":%s,\"elapsed_ms\":%.3f,\"goodput_mbps\":%.3f,\"datagrams\":0,\"repairs\":0}\n", bytes, elapsed_ms, goodput
    }' >"$SENDER_JSON"

  kill "$receiver_pid" 2>/dev/null || true
  wait "$receiver_pid" 2>/dev/null || true
  receiver_pid=''
else
  timeout --foreground 60s ip netns exec tsu-bench-d \
    "$BINARY" receive \
    --listen 10.210.2.2:9000 \
    --udp 10.210.2.2:9001 \
    --out "$OUTPUT" >"$RECEIVER_LOG" 2>&1 &
  receiver_pid=$!

  for _ in {1..50}; do
    if ip netns exec tsu-bench-d ss -ltn | grep -q '10.210.2.2:9000'; then
      break
    fi
    sleep 0.02
  done

  timeout --foreground 60s ip netns exec tsu-bench-s \
    "$BINARY" send \
    --connect 10.210.2.2:9000 \
    --udp-target 10.210.2.2:9001 \
    --file "$SOURCE" \
    --transport "$TRANSPORT" \
    --rate-mbps "$RATE_MBIT" \
    --repair-cooldown-ms "$REPAIR_COOLDOWN_MS" >"$SENDER_JSON"

  wait "$receiver_pid"
  receiver_pid=''
  cmp --silent "$SOURCE" "$OUTPUT"
fi

ip netns exec tsu-bench-r tc -s -j qdisc show dev tsu-right0 >"$FORWARD_QDISC"
ip netns exec tsu-bench-r tc -s -j qdisc show dev tsu-left0 >"$REVERSE_QDISC"
python3 "$ROOT/lab/host-snapshot.py" >"$HOST_AFTER"

jq -c \
  --argjson file_bytes "$FILE_BYTES" \
  --argjson rate_mbit "$RATE_MBIT" \
  --argjson rtt_ms "$RTT_MS" \
  --argjson loss_percent "$LOSS_PERCENT" \
  --argjson seed "$SEED" \
  --arg block_id "$BLOCK_ID" \
  --argjson block_order "$BLOCK_ORDER" \
  --argjson treatment_order "$TREATMENT_ORDER" \
  --argjson randomization_seed "$RANDOMIZATION_SEED" \
  --slurpfile forward_qdisc "$FORWARD_QDISC" \
  --slurpfile reverse_qdisc "$REVERSE_QDISC" \
  --slurpfile host_before "$HOST_BEFORE" \
  --slurpfile host_after "$HOST_AFTER" \
  '. + {
    scenario: {
      file_bytes: $file_bytes,
      rate_mbit: $rate_mbit,
      rtt_ms: $rtt_ms,
      loss_percent: $loss_percent,
      seed: $seed
    },
    network: {
      forward_qdisc: $forward_qdisc[0],
      reverse_qdisc: $reverse_qdisc[0]
    },
    design: {
      block_id: $block_id,
      block_order: $block_order,
      treatment_order: $treatment_order,
      randomization_seed: $randomization_seed
    },
    host: {
      before: $host_before[0],
      after: $host_after[0]
    },
    verified: true
  }' "$SENDER_JSON" >"$RESULT_JSON.tmp"
mv "$RESULT_JSON.tmp" "$RESULT_JSON"
rm -f "$FORWARD_QDISC" "$REVERSE_QDISC" "$HOST_BEFORE" "$HOST_AFTER"
cat "$RESULT_JSON"
