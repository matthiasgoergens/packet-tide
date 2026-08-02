#!/usr/bin/env bash
set -euo pipefail

binary=${1:-target/debug/packet-tide}
timeout_ms=${IDLE_TIMEOUT_MS:-500}
work=$(mktemp -d "${TMPDIR:-/tmp}/packet-tide-liveness.XXXXXX")
receiver=
sender=

cleanup() {
  status=$?
  for pid in "$receiver" "$sender"; do
    if [[ -n $pid ]]; then
      kill -KILL "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  if (( status != 0 )); then
    for log in "$work"/*.log; do
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

wait_bounded() {
  local pid=$1
  local attempts=${2:-60}
  for ((attempt = 0; attempt < attempts; attempt++)); do
    if ! kill -0 "$pid" 2>/dev/null; then
      return 0
    fi
    sleep 0.05
  done
  return 1
}

"$binary" keygen --out "$work/key"
dd if=/dev/zero of="$work/source" bs=1M count=5 status=none
base_port=$((20000 + ($$ % 20000)))

# A sender that remains connected but stops producing authenticated heartbeats
# must not leave the receiver alive indefinitely. Its durable receipt state must
# remain usable for a later resume.
"$binary" receive \
  --listen "127.0.0.1:$base_port" \
  --udp "127.0.0.1:$((base_port + 1))" \
  --out "$work/out-silent-sender" \
  --key-file "$work/key" \
  --idle-timeout-ms "$timeout_ms" >"$work/receiver-silent-sender.log" 2>&1 &
receiver=$!
sleep 0.2
"$binary" send \
  --connect "127.0.0.1:$base_port" \
  --udp-target "127.0.0.1:$((base_port + 1))" \
  --file "$work/source" \
  --transport udp \
  --rate-mbps 1 \
  --key-file "$work/key" \
  --idle-timeout-ms "$timeout_ms" >"$work/sender-paused.log" 2>&1 &
sender=$!
sleep 1
kill -STOP "$sender"
wait_bounded "$receiver" || {
  echo "receiver did not stop after sender became silent" >&2
  exit 1
}
if wait "$receiver"; then
  echo "receiver unexpectedly succeeded after sender became silent" >&2
  exit 1
fi
receiver=
kill -KILL "$sender" 2>/dev/null || true
wait "$sender" 2>/dev/null || true
sender=
test -f "$work/out-silent-sender.part"
test -f "$work/out-silent-sender.part.map"

# The sender checks feedback during the original-data pass, not just after END.
# Pausing the receiver therefore stops a deliberately slow transfer promptly.
base_port=$((base_port + 10))
"$binary" receive \
  --listen "127.0.0.1:$base_port" \
  --udp "127.0.0.1:$((base_port + 1))" \
  --out "$work/out-silent-receiver" \
  --key-file "$work/key" \
  --idle-timeout-ms "$timeout_ms" >"$work/receiver-paused.log" 2>&1 &
receiver=$!
sleep 0.2
"$binary" send \
  --connect "127.0.0.1:$base_port" \
  --udp-target "127.0.0.1:$((base_port + 1))" \
  --file "$work/source" \
  --transport udp \
  --rate-mbps 1 \
  --key-file "$work/key" \
  --idle-timeout-ms "$timeout_ms" >"$work/sender-silent-receiver.log" 2>&1 &
sender=$!
sleep 1
kill -STOP "$receiver"
wait_bounded "$sender" || {
  echo "sender did not stop after receiver became silent" >&2
  exit 1
}
if wait "$sender"; then
  echo "sender unexpectedly succeeded after receiver became silent" >&2
  exit 1
fi
sender=
kill -KILL "$receiver" 2>/dev/null || true
wait "$receiver" 2>/dev/null || true
receiver=

echo "authenticated-control liveness checks passed"
