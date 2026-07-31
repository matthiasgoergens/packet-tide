#!/usr/bin/env python3
import argparse
import os
import time
from pathlib import Path


def psi_avg10(resource: str) -> float:
    path = Path("/proc/pressure") / resource
    if not path.exists():
        return 0.0
    for line in path.read_text().splitlines():
        if line.startswith("some "):
            for field in line.split()[1:]:
                key, value = field.split("=", 1)
                if key == "avg10":
                    return float(value)
    return 0.0


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--max-load-per-cpu", type=float, default=0.5)
    parser.add_argument("--max-cpu-psi", type=float, default=5.0)
    parser.add_argument("--max-io-psi", type=float, default=1.0)
    parser.add_argument("--samples", type=int, default=4)
    parser.add_argument("--interval", type=float, default=10.0)
    parser.add_argument("--timeout", type=float, default=3600.0)
    args = parser.parse_args()

    cpu_count = os.cpu_count() or 1
    deadline = time.monotonic() + args.timeout
    consecutive = 0
    while time.monotonic() < deadline:
        load_per_cpu = os.getloadavg()[0] / cpu_count
        cpu_psi = psi_avg10("cpu")
        io_psi = psi_avg10("io")
        idle = (
            load_per_cpu <= args.max_load_per_cpu
            and cpu_psi <= args.max_cpu_psi
            and io_psi <= args.max_io_psi
        )
        if idle:
            consecutive += 1
            if consecutive >= args.samples:
                return
        else:
            consecutive = 0
            print(
                f"waiting for idle host: load/cpu={load_per_cpu:.3f} "
                f"cpu.psi={cpu_psi:.2f} io.psi={io_psi:.2f}",
                flush=True,
            )
        time.sleep(args.interval)
    raise SystemExit("host did not remain idle before timeout")


if __name__ == "__main__":
    main()
