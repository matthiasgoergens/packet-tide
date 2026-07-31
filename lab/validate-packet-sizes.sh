#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
CAPTURE=/tmp/tsunami-udp-lab/results/packet-size-validation.json

capture_pid=''
stop_capture() {
  if [[ -n $capture_pid ]]; then
    kill "$capture_pid" 2>/dev/null || true
    wait "$capture_pid" 2>/dev/null || true
  fi
}
trap stop_capture EXIT INT TERM

ip netns exec tsu-bench-r \
  python3 "$ROOT/lab/capture-packets.py" tsu-right0 --seconds 5 \
  >"$CAPTURE" &
capture_pid=$!

"$ROOT/lab/run-one.sh" udp 16777216 100 20 0 88881 >/dev/null
wait "$capture_pid"
capture_pid=''

jq -e '.oversized_ipv4_packets == 0 and .max_ipv4_length <= 1500' "$CAPTURE" >/dev/null
cat "$CAPTURE"
