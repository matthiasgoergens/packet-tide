#!/usr/bin/env python3
import argparse
import collections
import json
import socket
import time


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("interface")
    parser.add_argument("--seconds", type=float, default=5.0)
    parser.add_argument("--max-packets", type=int, default=250_000)
    parser.add_argument("--path-mtu", type=int, default=1500)
    args = parser.parse_args()

    capture = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(0x0003))
    capture.bind((args.interface, 0))
    capture.settimeout(0.2)
    deadline = time.monotonic() + args.seconds
    ip_lengths: collections.Counter[int] = collections.Counter()
    protocols: collections.Counter[str] = collections.Counter()
    udp_ip_lengths: collections.Counter[int] = collections.Counter()
    frames = 0
    oversized_ip_packets = 0

    while time.monotonic() < deadline and frames < args.max_packets:
        try:
            frame = capture.recv(65_535)
        except TimeoutError:
            continue
        frames += 1
        if len(frame) < 34 or int.from_bytes(frame[12:14], "big") != 0x0800:
            continue
        ip_length = int.from_bytes(frame[16:18], "big")
        ip_lengths[ip_length] += 1
        if ip_length > args.path_mtu:
            oversized_ip_packets += 1
        protocol = frame[23]
        protocols[{6: "tcp", 17: "udp"}.get(protocol, str(protocol))] += 1
        if protocol == 17:
            udp_ip_lengths[ip_length] += 1

    result = {
        "interface": args.interface,
        "frames": frames,
        "ipv4_packets": sum(ip_lengths.values()),
        "max_ipv4_length": max(ip_lengths, default=0),
        "max_udp_ipv4_length": max(udp_ip_lengths, default=0),
        "path_mtu": args.path_mtu,
        "oversized_ipv4_packets": oversized_ip_packets,
        "protocols": protocols,
        "ipv4_length_counts": dict(sorted(ip_lengths.items())),
    }
    print(json.dumps(result, separators=(",", ":")))


if __name__ == "__main__":
    main()
