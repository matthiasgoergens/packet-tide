#!/usr/bin/env python3
import os
import random
import sys
from pathlib import Path


def main() -> None:
    if len(sys.argv) != 4:
        raise SystemExit("usage: generate-data.py PATH SIZE SEED")
    path = Path(sys.argv[1])
    size = int(sys.argv[2])
    seed = int(sys.argv[3])
    temporary = path.with_name(path.name + f".tmp.{os.getpid()}")
    rng = random.Random(seed)
    remaining = size
    with temporary.open("wb") as stream:
        while remaining:
            count = min(1024 * 1024, remaining)
            stream.write(rng.randbytes(count))
            remaining -= count
    temporary.replace(path)


if __name__ == "__main__":
    main()
