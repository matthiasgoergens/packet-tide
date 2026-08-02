#!/usr/bin/env bash
set -euo pipefail

binary=${1:-target/debug/packet-tide}
work=$(mktemp -d "${TMPDIR:-/tmp}/packet-tide-rate-control.XXXXXX")
receiver=

cleanup() {
  status=$?
  if [[ -n $receiver ]]; then
    kill -KILL "$receiver" 2>/dev/null || true
    wait "$receiver" 2>/dev/null || true
  fi
  if (( status != 0 )); then
    for log in "$work"/*.json "$work"/*.err; do
      [[ -f $log ]] || continue
      printf '==> %s <==\n' "$log" >&2
      sed -n '1,160p' "$log" >&2
    done
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM

"$binary" keygen --out "$work/key"
dd if=/dev/zero of="$work/source" bs=1M count=2 status=none
base_port=$((28000 + ($$ % 16000)))

"$binary" receive \
  --listen "127.0.0.1:$base_port" \
  --udp "127.0.0.1:$((base_port + 1))" \
  --out "$work/output" \
  --key-file "$work/key" >"$work/receiver.json" 2>"$work/receiver.err" &
receiver=$!
sleep 0.2

"$binary" send \
  --connect "127.0.0.1:$base_port" \
  --udp-target "127.0.0.1:$((base_port + 1))" \
  --file "$work/source" \
  --transport udp \
  --min-rate-mbps 5 \
  --max-rate-mbps 50 \
  --feedback-interval-ms 50 \
  --key-file "$work/key" >"$work/sender.json"
wait "$receiver"
receiver=
cmp "$work/source" "$work/output"

python3 - "$work/sender.json" <<'PY'
import json
import sys

sender = json.load(open(sys.argv[1], encoding="utf-8"))
controller = sender["rate_controller"]
assert sender["transport"] == "udp"
assert controller["mode"] == "auto"
assert controller["initial_rate_mbps"] == 5
assert controller["minimum_rate_mbps"] == 5
assert controller["maximum_rate_mbps"] == 50
assert controller["decisions"]
assert any(item["decision"] == "increase" for item in controller["decisions"])
assert all(5 <= item["new_rate_mbps"] <= 50 for item in controller["decisions"])
PY

echo "automatic rate-control transfer checks passed"
