#!/usr/bin/env python3
import csv
import random
import sys
from collections import defaultdict
from pathlib import Path


def main() -> None:
    if len(sys.argv) != 4:
        raise SystemExit(
            "usage: randomize-matrix.py MATRIX_FILE RANDOMIZATION_SEED TREATMENTS_CSV"
        )

    source = Path(sys.argv[1])
    randomization_seed = int(sys.argv[2])
    treatments = sys.argv[3].split(",")
    if len(treatments) != len(set(treatments)) or "udp" not in treatments:
        raise SystemExit("treatments must be unique and include udp")
    rows: list[dict[str, object]] = []
    for line_number, raw_line in enumerate(source.read_text().splitlines(), 1):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        fields = line.split()
        if len(fields) != 5:
            raise SystemExit(f"{source}:{line_number}: expected five fields")
        rows.append({"source_line": line_number, "fields": tuple(fields)})

    rng = random.Random(randomization_seed)
    by_condition: dict[tuple[str, str, str, str], list[dict[str, object]]] = defaultdict(list)
    for row in rows:
        fields = row["fields"]
        assert isinstance(fields, tuple)
        by_condition[fields[:4]].append(row)
    for condition_rows in by_condition.values():
        rng.shuffle(condition_rows)
        orders: list[tuple[str, ...]] = []
        while len(orders) < len(condition_rows):
            base = treatments.copy()
            rng.shuffle(base)
            rotations = [tuple(base[offset:] + base[:offset]) for offset in range(len(base))]
            rng.shuffle(rotations)
            orders.extend(rotations)
        for row, order in zip(condition_rows, orders):
            row["transport_order"] = order
    rng.shuffle(rows)
    writer = csv.writer(sys.stdout, delimiter="\t", lineterminator="\n")
    writer.writerow(
        [
            "block_order",
            "block_id",
            "file_bytes",
            "rate_mbit",
            "rtt_ms",
            "loss_percent",
            "seed",
            "transport_order",
        ]
    )
    for block_order, row in enumerate(rows, 1):
        fields = row["fields"]
        transports = row["transport_order"]
        source_line = row["source_line"]
        assert isinstance(fields, tuple) and isinstance(transports, tuple)
        file_bytes, rate_mbit, rtt_ms, loss_percent, seed = fields
        block_id = (
            f"b{source_line}-{file_bytes}-{rate_mbit}-{rtt_ms}-{loss_percent}-{seed}"
        )
        writer.writerow(
            [
                block_order,
                block_id,
                file_bytes,
                rate_mbit,
                rtt_ms,
                loss_percent,
                seed,
                ",".join(transports),
            ]
        )


if __name__ == "__main__":
    main()
