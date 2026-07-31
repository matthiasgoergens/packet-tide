#!/usr/bin/env python3
import datetime
import hashlib
import json
import os
import platform
import subprocess
import sys
from pathlib import Path


def command(*args: str) -> str | None:
    try:
        return subprocess.run(
            args, check=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
        ).stdout.strip()
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None


def sha256(path: Path) -> str | None:
    if not path.is_file():
        return None
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: capture-provenance.py OUTPUT_JSON")

    output = Path(sys.argv[1])
    root = Path(__file__).resolve().parent.parent
    sources = [root / "Cargo.lock", root / "Cargo.toml", root / "src/main.rs"]
    sources.extend(sorted((root / "lab").glob("*")))
    manifest = {
        "captured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "source_commit": os.environ.get("TSU_SOURCE_COMMIT"),
        "platform": platform.platform(),
        "kernel": command("uname", "-a"),
        "tools": {
            "rustc": command("rustc", "--version"),
            "cargo": command("cargo", "--version"),
            "iproute2": command("ip", "-Version"),
            "tc": command("tc", "-Version"),
            "rsync": (command("rsync", "--version") or "").splitlines()[:1],
        },
        "tcp": {
            "available_congestion_control": command(
                "sysctl", "-n", "net.ipv4.tcp_available_congestion_control"
            ),
            "default_congestion_control": command(
                "sysctl", "-n", "net.ipv4.tcp_congestion_control"
            ),
            "bbr_module": command("modinfo", "tcp_bbr"),
        },
        "binary": {
            "path": str(root / "target/release/tsunami-udp"),
            "sha256": sha256(root / "target/release/tsunami-udp"),
        },
        "source_sha256": {
            str(path.relative_to(root)): sha256(path)
            for path in sources
            if path.is_file()
        },
        "network_namespaces": command("ip", "netns", "list"),
        "sender_link": command("ip", "-d", "-s", "-n", "tsu-bench-s", "link", "show"),
        "router_link": command("ip", "-d", "-s", "-n", "tsu-bench-r", "link", "show"),
        "receiver_link": command(
            "ip", "-d", "-s", "-n", "tsu-bench-d", "link", "show"
        ),
        "sender_qdisc": command(
            "ip", "netns", "exec", "tsu-bench-s", "tc", "-s", "qdisc", "show"
        ),
        "router_qdisc": command(
            "ip", "netns", "exec", "tsu-bench-r", "tc", "-s", "qdisc", "show"
        ),
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(output)


if __name__ == "__main__":
    main()
