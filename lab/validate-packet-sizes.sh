#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
RESULTS=/tmp/packet-tide-lab/results

capture_pid=''
stop_capture() {
  if [[ -n $capture_pid ]]; then
    kill "$capture_pid" 2>/dev/null || true
    wait "$capture_pid" 2>/dev/null || true
  fi
}
trap stop_capture EXIT INT TERM

for transport in udp tcp-cubic tcp-bbr tcp4-cubic; do
  path_mtu=1500
  udp_payload_bytes=1172
  if [[ $transport == udp ]]; then
    path_mtu=1280
    udp_payload_bytes=1224
  fi
  for link in \
    'tsu-bench-s tsu-s0' \
    'tsu-bench-r tsu-left0' \
    'tsu-bench-r tsu-right0' \
    'tsu-bench-d tsu-d0'; do
    read -r namespace interface <<<"$link"
    ip -n "$namespace" link set dev "$interface" mtu "$path_mtu"
  done
  capture="$RESULTS/packet-size-validation-$transport.json"
  ip netns exec tsu-bench-r \
    python3 "$ROOT/lab/capture-packets.py" tsu-right0 --seconds 5 --path-mtu "$path_mtu" \
    >"$capture" &
  capture_pid=$!

  TSU_UDP_PAYLOAD_BYTES=$udp_payload_bytes \
    "$ROOT/lab/run-one.sh" "$transport" 16777216 100 20 0 88881 >/dev/null
  wait "$capture_pid"
  capture_pid=''

  jq -e \
    --argjson path_mtu "$path_mtu" \
    --argjson expected_udp_ip_bytes "$((udp_payload_bytes + 56))" \
    --argjson expect_udp "$([[ $transport == udp ]] && echo true || echo false)" \
    '.oversized_ipv4_packets == 0 and
     .path_mtu == $path_mtu and
     .max_ipv4_length <= $path_mtu and
     (if $expect_udp then
        .protocols.udp > 0 and .max_udp_ipv4_length == $expected_udp_ip_bytes
      else
        .protocols.udp == null or .max_udp_ipv4_length <= $expected_udp_ip_bytes
      end)' \
    "$capture" >/dev/null
  cat "$capture"
done
