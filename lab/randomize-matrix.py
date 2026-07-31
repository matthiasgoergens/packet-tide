#!/usr/bin/env python3
import csv
import itertools
import random
import sys
from pathlib import Path


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: randomize-matrix.py MATRIX_FILE RANDOMIZATION_SEED")

    source = Path(sys.argv[1])
    randomization_seed = int(sys.argv[2])
    rows: list[tuple[str, str, str, str, str]] = []
    for line_number, raw_line in enumerate(source.read_text().splitlines(), 1):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        fields = line.split()
        if len(fields) != 5:
            raise SystemExit(f"{source}:{line_number}: expected five fields")
        rows.append(tuple(fields))

    rng = random.Random(randomization_seed)
    rng.shuffle(rows)
    permutation_batches: list[tuple[str, str, str]] = []
    all_permutations = list(itertools.permutations(("udp", "tcp", "rsync")))
    while len(permutation_batches) < len(rows):
        batch = all_permutations.copy()
        rng.shuffle(batch)
        permutation_batches.extend(batch)
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
        file_bytes, rate_mbit, rtt_ms, loss_percent, seed = row
        transports = permutation_batches[block_order - 1]
        block_id = f"b-{file_bytes}-{rate_mbit}-{rtt_ms}-{loss_percent}-{seed}"
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
