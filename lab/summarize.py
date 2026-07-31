#!/usr/bin/env python3
import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: summarize.py RESULT_DIR")

    result_dir = Path(sys.argv[1])
    groups: dict[tuple[int, float, float, float], dict[str, list[dict]]] = defaultdict(
        lambda: defaultdict(list)
    )
    for path in sorted(result_dir.glob("result-*.json")):
        result = json.loads(path.read_text())
        scenario = result["scenario"]
        key = (
            scenario["file_bytes"],
            scenario["rate_mbit"],
            scenario["rtt_ms"],
            scenario["loss_percent"],
        )
        groups[key][result["transport"]].append(result)

    summary = []
    for key, transports in sorted(groups.items()):
        row = {
            "file_bytes": key[0],
            "rate_mbit": key[1],
            "rtt_ms": key[2],
            "loss_percent": key[3],
            "transports": {},
        }
        for name, results in sorted(transports.items()):
            goodputs = [item["goodput_mbps"] for item in results]
            elapsed = [item["elapsed_ms"] for item in results]
            row["transports"][name] = {
                "runs": len(results),
                "median_goodput_mbps": statistics.median(goodputs),
                "min_goodput_mbps": min(goodputs),
                "max_goodput_mbps": max(goodputs),
                "median_elapsed_ms": statistics.median(elapsed),
                "median_repairs": statistics.median(
                    item.get("repairs", 0) for item in results
                ),
            }
        summary.append(row)

    output = result_dir / "summary.json"
    output.write_text(json.dumps(summary, indent=2) + "\n")
    print(output)


if __name__ == "__main__":
    main()
