#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 TRANSPORT FILE_BYTES RATE_MBIT RTT_MS LOSS_PERCENT SEED" >&2
  exit 2
fi

TRANSPORT=$1
FILE_BYTES=$2
RATE_MBIT=$3
RTT_MS=$4
LOSS_PERCENT=$5
SEED=$6
QUEUE_PACKETS=${TSU_QUEUE_PACKETS:-10000}
SEND_RATE_MBIT=${TSU_SEND_RATE_MBIT:-$RATE_MBIT}
FORWARD_JITTER_MS=${TSU_FORWARD_JITTER_MS:-0}
FORWARD_DUPLICATE_PERCENT=${TSU_FORWARD_DUPLICATE_PERCENT:-0}
FORWARD_REORDER_PERCENT=${TSU_FORWARD_REORDER_PERCENT:-0}
UDP_PAYLOAD_BYTES=${TSU_UDP_PAYLOAD_BYTES:-1172}
FEEDBACK_INTERVAL_MS=${TSU_FEEDBACK_INTERVAL_MS:-50}

PROGRAM_TRANSPORT=$TRANSPORT
TCP_CC=''
case $TRANSPORT in
  udp) ;;
  tcp) PROGRAM_TRANSPORT=tcp ;;
  tcp-cubic) PROGRAM_TRANSPORT=tcp; TCP_CC=cubic ;;
  tcp-bbr) PROGRAM_TRANSPORT=tcp; TCP_CC=bbr ;;
  tcp4) PROGRAM_TRANSPORT=tcp4 ;;
  tcp4-cubic) PROGRAM_TRANSPORT=tcp4; TCP_CC=cubic ;;
  tcp4-bbr) PROGRAM_TRANSPORT=tcp4; TCP_CC=bbr ;;
  rsync) PROGRAM_TRANSPORT=rsync ;;
  rsync-cubic) PROGRAM_TRANSPORT=rsync; TCP_CC=cubic ;;
  rsync-bbr) PROGRAM_TRANSPORT=rsync; TCP_CC=bbr ;;
  *) echo "invalid transport: $TRANSPORT" >&2; exit 2 ;;
esac
[[ $FILE_BYTES =~ ^[0-9]+$ ]] || { echo "invalid file size" >&2; exit 2; }

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BINARY="$ROOT/target/release/packet-tide"
DATA=/tmp/packet-tide-lab/data
RESULTS=/tmp/packet-tide-lab/results
AUTH_KEY=${TSU_AUTH_KEY_FILE:-$DATA/auth.key}
SOURCE="$DATA/source-$FILE_BYTES-seed1.bin"
OUTPUT="$DATA/output-$TRANSPORT.bin"
RECEIVER_LOG="$RESULTS/receiver-$TRANSPORT.log"
RECEIVER_SUMMARY="$RESULTS/receiver-summary-$TRANSPORT.json"
SENDER_JSON="$RESULTS/sender-$TRANSPORT.json"
RSYNC_CONFIG="$DATA/rsyncd.conf"
FORWARD_QDISC="$RESULTS/qdisc-forward-$TRANSPORT.json"
REVERSE_QDISC="$RESULTS/qdisc-reverse-$TRANSPORT.json"
HOST_BEFORE="$RESULTS/host-before-$TRANSPORT.json"
HOST_AFTER="$RESULTS/host-after-$TRANSPORT.json"
BLOCK_ID=${TSU_BLOCK_ID:-standalone-$FILE_BYTES-$RATE_MBIT-$RTT_MS-$LOSS_PERCENT-$SEED}
BLOCK_ORDER=${TSU_BLOCK_ORDER:-0}
TREATMENT_ORDER=${TSU_TREATMENT_ORDER:-0}
RANDOMIZATION_SEED=${TSU_RANDOMIZATION_SEED:-0}
EXPECTED_TREATMENTS=${TSU_EXPECTED_TREATMENTS:-$TRANSPORT}
RESULT_JSON="$RESULTS/result-$BLOCK_ID-$TRANSPORT.json"

mkdir -p "$DATA" "$RESULTS"
if [[ ! -f $AUTH_KEY ]]; then
  "$BINARY" keygen --out "$AUTH_KEY"
fi
if [[ ! -f $SOURCE ]] || [[ $(stat -c %s "$SOURCE") -ne $FILE_BYTES ]]; then
  python3 "$ROOT/lab/generate-data.py" "$SOURCE" "$FILE_BYTES" 1
fi
rm -f "$OUTPUT" "$OUTPUT".part "$OUTPUT".part.* "$RECEIVER_LOG" "$RECEIVER_SUMMARY" "$SENDER_JSON"

"$ROOT/lab/configure-network.sh" \
  "$RATE_MBIT" "$RTT_MS" "$LOSS_PERCENT" "$QUEUE_PACKETS" "$SEED" 0 \
  "$FORWARD_JITTER_MS" "$FORWARD_DUPLICATE_PERCENT" "$FORWARD_REORDER_PERCENT"
if [[ -n $TCP_CC ]]; then
  ip netns exec tsu-bench-s sysctl -q -w net.ipv4.tcp_congestion_control="$TCP_CC"
  ip netns exec tsu-bench-d sysctl -q -w net.ipv4.tcp_congestion_control="$TCP_CC"
fi
ACTUAL_TCP_CC=$(ip netns exec tsu-bench-s sysctl -n net.ipv4.tcp_congestion_control)
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

if [[ $PROGRAM_TRANSPORT == rsync ]]; then
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

  timeout --foreground 180s ip netns exec tsu-bench-d \
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
  printf 'null\n' >"$RECEIVER_SUMMARY"
else
  timeout --foreground 180s ip netns exec tsu-bench-d \
    "$BINARY" receive \
    --listen 10.210.2.2:9000 \
    --udp 10.210.2.2:9001 \
    --out "$OUTPUT" --key-file "$AUTH_KEY" >"$RECEIVER_LOG" 2>&1 &
  receiver_pid=$!

  for _ in {1..50}; do
    if ip netns exec tsu-bench-d ss -ltn | grep -q '10.210.2.2:9000'; then
      break
    fi
    sleep 0.02
  done

  timeout --foreground 180s ip netns exec tsu-bench-s \
    "$BINARY" send \
    --connect 10.210.2.2:9000 \
    --udp-target 10.210.2.2:9001 \
    --file "$SOURCE" \
    --transport "$PROGRAM_TRANSPORT" \
    --rate-mbps "$SEND_RATE_MBIT" \
    --repair-cooldown-ms "$REPAIR_COOLDOWN_MS" \
    --udp-payload-bytes "$UDP_PAYLOAD_BYTES" \
    --feedback-interval-ms "$FEEDBACK_INTERVAL_MS" \
    --key-file "$AUTH_KEY" >"$SENDER_JSON"

  wait "$receiver_pid"
  receiver_pid=''
  cmp --silent "$SOURCE" "$OUTPUT"
  tail -n 1 "$RECEIVER_LOG" >"$RECEIVER_SUMMARY"
  jq -e '.schema_version == 1 and .role == "receiver"' "$RECEIVER_SUMMARY" >/dev/null
  jq -e --slurpfile receiver "$RECEIVER_SUMMARY" '
    ($receiver[0]) as $r |
    .schema_version == 1 and
    .role == "sender" and
    .udp_payload_bytes == $r.udp_payload_bytes and
    .feedback_interval_ms == $r.feedback_interval_ms and
    .receiver_received_chunks == $r.received_chunks and
    .receiver_frontier_chunks == $r.frontier_chunks and
    .receiver_accepted_datagrams == $r.accepted_datagrams and
    .receiver_valid_datagrams == $r.valid_datagrams and
    .receiver_duplicate_datagrams == $r.duplicate_datagrams and
    .receiver_invalid_datagrams == $r.invalid_datagrams and
    .receiver_repair_datagrams == $r.repair_datagrams and
    .receiver_socket_drops == $r.socket_drops and
    .receiver_reports == $r.reports
  ' "$SENDER_JSON" >/dev/null
  if [[ $PROGRAM_TRANSPORT == udp ]]; then
    expected_chunks=$(((FILE_BYTES + UDP_PAYLOAD_BYTES - 1) / UDP_PAYLOAD_BYTES))
    jq -e --argjson expected_chunks "$expected_chunks" \
      --argjson payload_bytes "$UDP_PAYLOAD_BYTES" \
      --argjson feedback_interval_ms "$FEEDBACK_INTERVAL_MS" '
      .schema_version == 1 and
      .role == "sender" and
      .receiver_received_chunks == $expected_chunks and
      .receiver_frontier_chunks == $expected_chunks and
      .receiver_accepted_datagrams == $expected_chunks and
      .udp_payload_bytes == $payload_bytes and
      .feedback_interval_ms == $feedback_interval_ms and
      .receiver_valid_datagrams == (.receiver_accepted_datagrams + .receiver_duplicate_datagrams) and
      .datagrams >= .receiver_valid_datagrams and
      .repairs >= .receiver_repair_datagrams and
      .receiver_reports >= 1
    ' "$SENDER_JSON" >/dev/null
  fi
fi

ip netns exec tsu-bench-r tc -s -j qdisc show dev tsu-right0 >"$FORWARD_QDISC"
ip netns exec tsu-bench-r tc -s -j qdisc show dev tsu-left0 >"$REVERSE_QDISC"
python3 "$ROOT/lab/host-snapshot.py" >"$HOST_AFTER"

jq -c \
  --argjson file_bytes "$FILE_BYTES" \
  --argjson rate_mbit "$RATE_MBIT" \
  --argjson rtt_ms "$RTT_MS" \
  --argjson loss_percent "$LOSS_PERCENT" \
  --argjson send_rate_mbit "$SEND_RATE_MBIT" \
  --argjson queue_packets "$QUEUE_PACKETS" \
  --argjson forward_jitter_ms "$FORWARD_JITTER_MS" \
  --argjson forward_duplicate_percent "$FORWARD_DUPLICATE_PERCENT" \
  --argjson forward_reorder_percent "$FORWARD_REORDER_PERCENT" \
  --argjson udp_payload_bytes "$UDP_PAYLOAD_BYTES" \
  --argjson feedback_interval_ms "$FEEDBACK_INTERVAL_MS" \
  --arg transport "$TRANSPORT" \
  --arg tcp_cc "$ACTUAL_TCP_CC" \
  --arg expected_treatments "$EXPECTED_TREATMENTS" \
  --argjson seed "$SEED" \
  --arg block_id "$BLOCK_ID" \
  --argjson block_order "$BLOCK_ORDER" \
  --argjson treatment_order "$TREATMENT_ORDER" \
  --argjson randomization_seed "$RANDOMIZATION_SEED" \
  --slurpfile forward_qdisc "$FORWARD_QDISC" \
  --slurpfile reverse_qdisc "$REVERSE_QDISC" \
  --slurpfile host_before "$HOST_BEFORE" \
  --slurpfile host_after "$HOST_AFTER" \
  --slurpfile receiver_summary "$RECEIVER_SUMMARY" \
  '. + {
    transport: $transport,
    tcp_congestion_control: $tcp_cc,
    scenario: {
      file_bytes: $file_bytes,
      rate_mbit: $rate_mbit,
      rtt_ms: $rtt_ms,
      loss_percent: $loss_percent,
      reverse_loss_percent: 0,
      send_rate_mbit: $send_rate_mbit,
      queue_packets: $queue_packets,
      forward_jitter_ms: $forward_jitter_ms,
      forward_duplicate_percent: $forward_duplicate_percent,
      forward_reorder_percent: $forward_reorder_percent,
      udp_payload_bytes: $udp_payload_bytes,
      feedback_interval_ms: $feedback_interval_ms,
      seed: $seed
    },
    network: {
      forward_qdisc: $forward_qdisc[0],
      reverse_qdisc: $reverse_qdisc[0],
      forward_bytes: ($forward_qdisc[0][0].bytes // null),
      reverse_bytes: ($reverse_qdisc[0][0].bytes // null)
    },
    design: {
      block_id: $block_id,
      block_order: $block_order,
      treatment_order: $treatment_order,
      randomization_seed: $randomization_seed,
      expected_treatments: ($expected_treatments | split(","))
    },
    host: {
      before: $host_before[0],
      after: $host_after[0]
    },
    receiver_summary: $receiver_summary[0],
    input: {pattern: "python-mt19937", seed: 1},
    verified: true
  }' "$SENDER_JSON" >"$RESULT_JSON.tmp"
mv "$RESULT_JSON.tmp" "$RESULT_JSON"
rm -f "$FORWARD_QDISC" "$REVERSE_QDISC" "$HOST_BEFORE" "$HOST_AFTER"
cat "$RESULT_JSON"
