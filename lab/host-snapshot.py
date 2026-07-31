#!/usr/bin/env python3
import datetime
import json
import os
from pathlib import Path


def pressure(resource: str) -> dict[str, dict[str, float | int]]:
    result: dict[str, dict[str, float | int]] = {}
    path = Path("/proc/pressure") / resource
    if not path.exists():
        return result
    for line in path.read_text().splitlines():
        label, *values = line.split()
        parsed: dict[str, float | int] = {}
        for value in values:
            key, raw = value.split("=", 1)
            parsed[key] = int(raw) if key == "total" else float(raw)
        result[label] = parsed
    return result


def memory() -> dict[str, int]:
    wanted = {"MemAvailable", "MemFree", "SwapFree", "SwapTotal"}
    result: dict[str, int] = {}
    for line in Path("/proc/meminfo").read_text().splitlines():
        key, value = line.split(":", 1)
        if key in wanted:
            result[f"{key}_kib"] = int(value.strip().split()[0])
    return result


def cpu_totals() -> dict[str, int]:
    fields = Path("/proc/stat").read_text().splitlines()[0].split()
    names = (
        "user",
        "nice",
        "system",
        "idle",
        "iowait",
        "irq",
        "softirq",
        "steal",
        "guest",
        "guest_nice",
    )
    return {name: int(value) for name, value in zip(names, fields[1:])}


def main() -> None:
    load1, load5, load15 = os.getloadavg()
    snapshot = {
        "timestamp_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "cpu_count": os.cpu_count(),
        "load": {"one": load1, "five": load5, "fifteen": load15},
        "memory": memory(),
        "cpu_ticks": cpu_totals(),
        "pressure": {
            resource: pressure(resource) for resource in ("cpu", "io", "memory")
        },
    }
    print(json.dumps(snapshot, separators=(",", ":")))


if __name__ == "__main__":
    main()
