#!/usr/bin/env python3
import json
import math
import random
import statistics
import sys
from collections import defaultdict
from pathlib import Path


def bootstrap_geometric_mean(values: list[float]) -> tuple[float, float]:
    if len(values) < 2:
        value = math.exp(statistics.fmean(math.log(item) for item in values))
        return value, value
    rng = random.Random(0)
    estimates = []
    for _ in range(10_000):
        sample = [rng.choice(values) for _ in values]
        estimates.append(math.exp(statistics.fmean(math.log(item) for item in sample)))
    estimates.sort()
    return estimates[249], estimates[9749]


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: analyze-rbd.py RESULT_DIR")

    result_dir = Path(sys.argv[1])
    quarantined = set()
    quality_path = result_dir / "block-quality.jsonl"
    if quality_path.exists():
        for line in quality_path.read_text().splitlines():
            record = json.loads(line)
            if record["status"] != "accepted":
                quarantined.add(record["block_id"])
    blocks: dict[str, dict[str, dict]] = defaultdict(dict)
    scenarios: dict[str, tuple[int, float, float, float]] = {}
    for path in sorted(result_dir.glob("result-*.json")):
        result = json.loads(path.read_text())
        design = result.get("design")
        if not design:
            continue
        block_id = design["block_id"]
        blocks[block_id][result["transport"]] = result
        scenario = result["scenario"]
        scenarios[block_id] = (
            scenario["file_bytes"],
            scenario["rate_mbit"],
            scenario["rtt_ms"],
            scenario["loss_percent"],
        )

    grouped: dict[tuple[int, float, float, float], dict[str, list[float]]] = defaultdict(
        lambda: defaultdict(list)
    )
    for block_id, treatments in blocks.items():
        if block_id in quarantined or "udp" not in treatments:
            continue
        udp_elapsed = treatments["udp"]["elapsed_ms"]
        for baseline in ("tcp", "rsync"):
            if baseline in treatments:
                grouped[scenarios[block_id]][baseline].append(
                    treatments[baseline]["elapsed_ms"] / udp_elapsed
                )

    analysis = []
    for scenario, comparisons in sorted(grouped.items()):
        row = {
            "file_bytes": scenario[0],
            "rate_mbit": scenario[1],
            "rtt_ms": scenario[2],
            "loss_percent": scenario[3],
            "udp_speedup_over": {},
        }
        for baseline, ratios in sorted(comparisons.items()):
            ci_low, ci_high = bootstrap_geometric_mean(ratios)
            row["udp_speedup_over"][baseline] = {
                "blocks": len(ratios),
                "geometric_mean": math.exp(
                    statistics.fmean(math.log(value) for value in ratios)
                ),
                "bootstrap_95pct_ci": [ci_low, ci_high],
                "median": statistics.median(ratios),
                "min": min(ratios),
                "max": max(ratios),
            }
        analysis.append(row)

    output = result_dir / "rbd-analysis.json"
    output.write_text(
        json.dumps(
            {"quarantined_blocks": len(quarantined), "scenarios": analysis}, indent=2
        )
        + "\n"
    )
    print(output)


if __name__ == "__main__":
    main()
