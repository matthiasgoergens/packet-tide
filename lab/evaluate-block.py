#!/usr/bin/env python3
import json
import sys
from pathlib import Path


MAX_LOAD_PER_CPU = 0.5
MAX_CPU_PSI = 5.0
MAX_IO_PSI = 1.0


def psi(snapshot: dict, resource: str) -> float:
    return snapshot.get("pressure", {}).get(resource, {}).get("some", {}).get("avg10", 0.0)


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: evaluate-block.py RESULT_DIR BLOCK_ID")

    result_dir = Path(sys.argv[1])
    block_id = sys.argv[2]
    results = []
    for path in sorted(result_dir.glob("result-*.json")):
        result = json.loads(path.read_text())
        if result.get("design", {}).get("block_id") == block_id:
            results.append(result)

    reasons = []
    transports = {result["transport"] for result in results}
    if transports != {"udp", "tcp", "rsync"}:
        reasons.append(f"incomplete treatments: {sorted(transports)}")
    for result in results:
        transport = result["transport"]
        if not result.get("verified"):
            reasons.append(f"{transport}: verification failed")
        for phase in ("before", "after"):
            snapshot = result.get("host", {}).get(phase)
            if not snapshot:
                reasons.append(f"{transport}: missing {phase} host snapshot")
                continue
            cpu_count = snapshot.get("cpu_count") or 1
            load_per_cpu = snapshot["load"]["one"] / cpu_count
            cpu_psi = psi(snapshot, "cpu")
            io_psi = psi(snapshot, "io")
            if load_per_cpu > MAX_LOAD_PER_CPU:
                reasons.append(f"{transport} {phase}: load/core {load_per_cpu:.3f}")
            if cpu_psi > MAX_CPU_PSI:
                reasons.append(f"{transport} {phase}: CPU PSI {cpu_psi:.2f}")
            if io_psi > MAX_IO_PSI:
                reasons.append(f"{transport} {phase}: I/O PSI {io_psi:.2f}")

    record = {
        "block_id": block_id,
        "status": "quarantined" if reasons else "accepted",
        "reasons": reasons,
    }
    with (result_dir / "block-quality.jsonl").open("a") as stream:
        stream.write(json.dumps(record, separators=(",", ":")) + "\n")
    print(json.dumps(record, separators=(",", ":")))


if __name__ == "__main__":
    main()
