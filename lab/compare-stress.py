#!/usr/bin/env python3
import json
import re
import statistics
import sys
from collections import defaultdict
from pathlib import Path


BLOCK = re.compile(r"stress-b(\d+)-(.+)-s(\d+)$")


def load(directory: Path) -> dict[tuple[int, str, int], dict]:
    results = {}
    for path in sorted(directory.glob("result-*.json")):
        result = json.loads(path.read_text())
        match = BLOCK.fullmatch(result["design"]["block_id"])
        if not match:
            raise ValueError(f"{path}: unexpected block ID")
        key = (int(match[1]), match[2], int(match[3]))
        if key in results:
            raise ValueError(f"{directory}: duplicate block {key}")
        results[key] = result
    return results


def relative_change(before: float, after: float) -> float:
    return after / before - 1.0


def main() -> None:
    if len(sys.argv) != 4:
        raise SystemExit("usage: compare-stress.py OUTPUT.json BEFORE_DIR AFTER_DIR")
    output_path, before_dir, after_dir = map(Path, sys.argv[1:])
    before = load(before_dir)
    after = load(after_dir)
    if before.keys() != after.keys():
        raise ValueError("stress result sets do not contain identical blocks")
    if not all(result.get("verified") is True for result in (*before.values(), *after.values())):
        raise ValueError("unverified result in stress comparison")

    by_case: dict[str, list[tuple[dict, dict]]] = defaultdict(list)
    for key in sorted(before):
        by_case[key[1]].append((before[key], after[key]))
    cases = {}
    for case, pairs in sorted(by_case.items()):
        before_repairs = [item[0]["repairs"] for item in pairs]
        after_repairs = [item[1]["repairs"] for item in pairs]
        before_elapsed = [item[0]["elapsed_ms"] for item in pairs]
        after_elapsed = [item[1]["elapsed_ms"] for item in pairs]
        before_bytes = [item[0]["udp_ip_bytes_offered"] for item in pairs]
        after_bytes = [item[1]["udp_ip_bytes_offered"] for item in pairs]
        cases[case] = {
            "blocks": len(pairs),
            "repairs": {
                "before": before_repairs,
                "after": after_repairs,
                "relative_change": relative_change(sum(before_repairs), sum(after_repairs)),
            },
            "elapsed_ms": {
                "before": before_elapsed,
                "after": after_elapsed,
                "relative_change": relative_change(sum(before_elapsed), sum(after_elapsed)),
            },
            "udp_ip_bytes_offered": {
                "before": before_bytes,
                "after": after_bytes,
                "relative_change": relative_change(sum(before_bytes), sum(after_bytes)),
            },
            "paired_elapsed_ratio_median": statistics.median(
                new / old for old, new in zip(before_elapsed, after_elapsed)
            ),
        }
    output = {
        "verified": True,
        "before": str(before_dir),
        "after": str(after_dir),
        "paired_blocks": len(before),
        "cases": cases,
    }
    output_path.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
    print(output_path)


if __name__ == "__main__":
    main()
