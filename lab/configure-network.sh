#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 5 || $# -gt 6 ]]; then
  echo "usage: $0 RATE_MBIT RTT_MS FORWARD_LOSS_PERCENT QUEUE_PACKETS SEED [REVERSE_LOSS_PERCENT]" >&2
  exit 2
fi

RATE_MBIT=$1
RTT_MS=$2
LOSS_PERCENT=$3
QUEUE_PACKETS=$4
SEED=$5
REVERSE_LOSS_PERCENT=${6:-$LOSS_PERCENT}
ROUTER_NS=tsu-bench-r

number='^[0-9]+([.][0-9]+)?$'
integer='^[0-9]+$'
[[ $RATE_MBIT =~ $number ]] || { echo "invalid rate" >&2; exit 2; }
[[ $RTT_MS =~ $number ]] || { echo "invalid RTT" >&2; exit 2; }
[[ $LOSS_PERCENT =~ $number ]] || { echo "invalid loss" >&2; exit 2; }
[[ $REVERSE_LOSS_PERCENT =~ $number ]] || { echo "invalid reverse loss" >&2; exit 2; }
[[ $QUEUE_PACKETS =~ $integer ]] || { echo "invalid queue size" >&2; exit 2; }
[[ $SEED =~ $integer ]] || { echo "invalid seed" >&2; exit 2; }

HALF_DELAY_MS=$(awk -v rtt="$RTT_MS" 'BEGIN { printf "%.3f", rtt / 2.0 }')

for interface in tsu-left0 tsu-right0; do
  ip netns exec "$ROUTER_NS" tc qdisc delete dev "$interface" root 2>/dev/null || true
  INTERFACE_SEED=$SEED
  INTERFACE_LOSS=$LOSS_PERCENT
  if [[ $interface == tsu-left0 ]]; then
    INTERFACE_SEED=$((SEED + 1))
    INTERFACE_LOSS=$REVERSE_LOSS_PERCENT
  fi
  ip netns exec "$ROUTER_NS" tc qdisc add dev "$interface" root netem \
    limit "$QUEUE_PACKETS" \
    delay "${HALF_DELAY_MS}ms" \
    loss random "${INTERFACE_LOSS}%" \
    rate "${RATE_MBIT}mbit" \
    seed "$INTERFACE_SEED"
done
