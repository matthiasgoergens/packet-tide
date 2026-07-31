#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
RESULTS=/tmp/tsunami-udp-lab/results

capture_pid=''
stop_capture() {
  if [[ -n $capture_pid ]]; then
    kill "$capture_pid" 2>/dev/null || true
    wait "$capture_pid" 2>/dev/null || true
  fi
}
trap stop_capture EXIT INT TERM

for transport in udp tcp-cubic tcp-bbr tcp4-cubic; do
  capture="$RESULTS/packet-size-validation-$transport.json"
  ip netns exec tsu-bench-r \
    python3 "$ROOT/lab/capture-packets.py" tsu-right0 --seconds 5 \
    >"$capture" &
  capture_pid=$!

  "$ROOT/lab/run-one.sh" "$transport" 16777216 100 20 0 88881 >/dev/null
  wait "$capture_pid"
  capture_pid=''

  jq -e \
    '.oversized_ipv4_packets == 0 and .max_ipv4_length <= 1500' \
    "$capture" >/dev/null
  cat "$capture"
done
