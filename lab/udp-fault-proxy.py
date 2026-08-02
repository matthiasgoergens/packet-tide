#!/usr/bin/env python3
"""Deterministic loopback UDP loss/duplication proxy for telemetry tests."""

from __future__ import annotations

import argparse
import json
import socket
import struct
import time


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen-port", type=int, required=True)
    parser.add_argument("--target-port", type=int, required=True)
    parser.add_argument("--idle-seconds", type=float, default=1.0)
    args = parser.parse_args()

    source = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    source.bind(("127.0.0.1", args.listen_port))
    source.settimeout(0.1)
    target = ("127.0.0.1", args.target_port)
    seen_originals: set[int] = set()
    received = forwarded = dropped = duplicated = 0
    last_packet: float | None = None

    while last_packet is None or time.monotonic() - last_packet < args.idle_seconds:
        try:
            packet, _ = source.recvfrom(65_535)
        except TimeoutError:
            continue
        last_packet = time.monotonic()
        received += 1
        if len(packet) < 10 or packet[0] != 3:
            source.sendto(packet, target)
            forwarded += 1
            continue
        sequence = struct.unpack("!Q", packet[1:9])[0]
        repair = packet[9] == 1
        first_original = not repair and sequence not in seen_originals
        if first_original:
            seen_originals.add(sequence)
        if first_original and sequence % 20 == 0:
            dropped += 1
            continue
        source.sendto(packet, target)
        forwarded += 1
        if first_original and sequence % 31 == 0:
            source.sendto(packet, target)
            forwarded += 1
            duplicated += 1

    print(
        json.dumps(
            {
                "received": received,
                "forwarded": forwarded,
                "dropped": dropped,
                "duplicated": duplicated,
            },
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
