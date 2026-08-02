#!/usr/bin/env bash
set -euo pipefail

binary=${1:-target/debug/packet-tide}
work=$(mktemp -d "${TMPDIR:-/tmp}/packet-tide-telemetry.XXXXXX")
receiver=
proxy=

cleanup() {
  status=$?
  for pid in "$receiver" "$proxy"; do
    if [[ -n $pid ]]; then
      kill -KILL "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  if (( status != 0 )); then
    shopt -s nullglob
    for log in "$work"/*.json "$work"/*.err; do
      if [[ -f $log ]]; then
        printf '==> %s <==\n' "$log" >&2
        sed -n '1,160p' "$log" >&2
      fi
    done
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM

"$binary" keygen --out "$work/key"
dd if=/dev/zero of="$work/source" bs=1M count=1 status=none
base_port=$((24000 + ($$ % 16000)))

"$binary" receive \
  --listen "127.0.0.1:$base_port" \
  --udp "127.0.0.1:$((base_port + 1))" \
  --out "$work/output" \
  --key-file "$work/key" >"$work/receiver.json" 2>"$work/receiver.err" &
receiver=$!
python3 lab/udp-fault-proxy.py \
  --listen-port "$((base_port + 2))" \
  --target-port "$((base_port + 1))" >"$work/proxy.json" &
proxy=$!
sleep 0.2

"$binary" send \
  --connect "127.0.0.1:$base_port" \
  --udp-target "127.0.0.1:$((base_port + 2))" \
  --file "$work/source" \
  --transport udp \
  --rate-mbps 10 \
  --udp-payload-bytes 512 \
  --feedback-interval-ms 200 \
  --key-file "$work/key" >"$work/sender.json"
wait "$receiver"
receiver=
wait "$proxy"
proxy=
cmp "$work/source" "$work/output"

base_port=$((base_port + 10))
"$binary" receive \
  --listen "127.0.0.1:$base_port" \
  --udp "127.0.0.1:$((base_port + 1))" \
  --out "$work/output" \
  --key-file "$work/key" >"$work/retry-receiver.json" 2>"$work/retry-receiver.err" &
receiver=$!
sleep 0.2
"$binary" send \
  --connect "127.0.0.1:$base_port" \
  --udp-target "127.0.0.1:$((base_port + 1))" \
  --file "$work/source" \
  --transport udp \
  --rate-mbps 50 \
  --udp-payload-bytes 512 \
  --feedback-interval-ms 200 \
  --key-file "$work/key" >"$work/retry-sender.json"
wait "$receiver"
receiver=

python3 - "$work/sender.json" "$work/receiver.json" "$work/proxy.json" \
  "$work/retry-sender.json" "$work/retry-receiver.json" <<'PY'
import json
import sys

sender, receiver, proxy, retry_sender, retry_receiver = [
    json.load(open(path, encoding="utf-8")) for path in sys.argv[1:]
]
expected_chunks = (sender["bytes"] + sender["udp_payload_bytes"] - 1) // sender["udp_payload_bytes"]
fields = (
    "received_chunks",
    "frontier_chunks",
    "accepted_datagrams",
    "valid_datagrams",
    "duplicate_datagrams",
    "invalid_datagrams",
    "repair_datagrams",
    "socket_drops",
    "reports",
)
for field in fields:
    assert sender[f"receiver_{field}"] == receiver[field], field
assert sender["schema_version"] == receiver["schema_version"] == 1
assert sender["udp_payload_bytes"] == receiver["udp_payload_bytes"] == 512
assert sender["feedback_interval_ms"] == receiver["feedback_interval_ms"] == 200
assert sender["rate_controller"]["mode"] == "fixed"
assert sender["rate_controller"]["initial_rate_mbps"] == 10
assert sender["rate_controller"]["final_rate_mbps"] == 10
assert sender["rate_controller"]["decisions"] == []
expected_periodic_reports = sender["elapsed_ms"] / sender["feedback_interval_ms"]
assert receiver["reports"] >= max(2, expected_periodic_reports * 0.5)
assert receiver["reports"] <= expected_periodic_reports * 2 + 3
assert sender["receiver_received_chunks"] == expected_chunks
assert sender["receiver_accepted_datagrams"] == expected_chunks
assert sender["datagrams"] == proxy["received"]
assert sender["receiver_valid_datagrams"] == proxy["forwarded"] - proxy["oversized"]
assert sender["receiver_duplicate_datagrams"] == proxy["duplicated"]
assert sender["receiver_invalid_datagrams"] == proxy["oversized"] == 1
assert sender["repairs"] > 0
assert sender["receiver_repair_datagrams"] > 0
assert proxy["dropped"] > 0
assert proxy["duplicated"] > 0
assert retry_sender["datagrams"] == 0
assert retry_sender["udp_payload_bytes"] == retry_receiver["udp_payload_bytes"] == 512
assert retry_sender["feedback_interval_ms"] == retry_receiver["feedback_interval_ms"] == 200
assert retry_sender["rate_controller"]["mode"] == "fixed"
assert retry_sender["rate_controller"]["final_rate_mbps"] == 50
assert retry_sender["resumed_chunks"] == expected_chunks
for field in fields:
    assert retry_sender[f"receiver_{field}"] == retry_receiver[field], f"retry {field}"
assert retry_sender["receiver_received_chunks"] == expected_chunks
assert retry_sender["receiver_accepted_datagrams"] == 0
assert retry_sender["receiver_valid_datagrams"] == 0
assert retry_sender["receiver_reports"] == 1
PY

echo "receiver telemetry reconciliation checks passed"
