#!/usr/bin/env bash
set -euo pipefail

SENDER_NS=tsu-bench-s
ROUTER_NS=tsu-bench-r
RECEIVER_NS=tsu-bench-d

cleanup() {
  ip netns delete "$SENDER_NS" 2>/dev/null || true
  ip netns delete "$ROUTER_NS" 2>/dev/null || true
  ip netns delete "$RECEIVER_NS" 2>/dev/null || true
}

cleanup
trap cleanup ERR INT TERM

ip netns add "$SENDER_NS"
ip netns add "$ROUTER_NS"
ip netns add "$RECEIVER_NS"

ip link add tsu-s0 type veth peer name tsu-left0
ip link add tsu-d0 type veth peer name tsu-right0
ip link set tsu-s0 netns "$SENDER_NS"
ip link set tsu-left0 netns "$ROUTER_NS"
ip link set tsu-d0 netns "$RECEIVER_NS"
ip link set tsu-right0 netns "$ROUTER_NS"

ip -n "$SENDER_NS" address add 10.210.1.2/24 dev tsu-s0
ip -n "$ROUTER_NS" address add 10.210.1.1/24 dev tsu-left0
ip -n "$ROUTER_NS" address add 10.210.2.1/24 dev tsu-right0
ip -n "$RECEIVER_NS" address add 10.210.2.2/24 dev tsu-d0

for namespace in "$SENDER_NS" "$ROUTER_NS" "$RECEIVER_NS"; do
  ip -n "$namespace" link set lo up
done
ip -n "$SENDER_NS" link set tsu-s0 up
ip -n "$ROUTER_NS" link set tsu-left0 up
ip -n "$ROUTER_NS" link set tsu-right0 up
ip -n "$RECEIVER_NS" link set tsu-d0 up

# Keep virtual offload aggregation at the path MTU so netem loss acts on
# wire-sized packets rather than a large GSO/GRO super-packet. This uses only
# iproute2 and changes ephemeral veth devices, avoiding host package changes.
ip -n "$SENDER_NS" link set dev tsu-s0 \
  gso_max_size 1500 gso_ipv4_max_size 1500 \
  gro_max_size 1500 gro_ipv4_max_size 1500
ip -n "$ROUTER_NS" link set dev tsu-left0 \
  gso_max_size 1500 gso_ipv4_max_size 1500 \
  gro_max_size 1500 gro_ipv4_max_size 1500
ip -n "$ROUTER_NS" link set dev tsu-right0 \
  gso_max_size 1500 gso_ipv4_max_size 1500 \
  gro_max_size 1500 gro_ipv4_max_size 1500
ip -n "$RECEIVER_NS" link set dev tsu-d0 \
  gso_max_size 1500 gso_ipv4_max_size 1500 \
  gro_max_size 1500 gro_ipv4_max_size 1500

ip -n "$SENDER_NS" route add default via 10.210.1.1
ip -n "$RECEIVER_NS" route add default via 10.210.2.1
ip netns exec "$ROUTER_NS" sysctl -q -w net.ipv4.ip_forward=1

trap - ERR INT TERM
