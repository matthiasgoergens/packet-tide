#!/usr/bin/env python3
"""Run an authenticated, randomized transfer matrix across two Linux hosts.

The script creates and removes one Podman container per endpoint/treatment.  Any
netem qdisc lives only on the sender container's ephemeral eth0, never on a host
NIC.  The physical machines therefore need no manually prepared namespaces.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import shlex
import subprocess
import sys
import time
import uuid
from pathlib import Path


DEFAULT_SCENARIOS = (
    {"name": "clean", "file_bytes": 16_777_216, "rate_mbit": 100.0, "rtt_ms": 20.0, "loss_percent": 0.0},
    {"name": "lossy", "file_bytes": 16_777_216, "rate_mbit": 100.0, "rtt_ms": 100.0, "loss_percent": 1.0},
)
TREATMENTS = ("udp", "tcp", "tcp4")


def run(command: list[str], *, input_text: str | None = None, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, input=input_text, text=True, capture_output=True, check=check)


def remote(host: str, script: str, *, check: bool = True) -> subprocess.CompletedProcess[str]:
    return run(["ssh", host, "bash", "-s"], input_text="set -euo pipefail\n" + script, check=check)


def quote(value: object) -> str:
    return shlex.quote(str(value))


def identity(host: str) -> dict[str, str]:
    script = """
machine=$(cat /etc/machine-id)
printf '%s\\n' "$machine"
uname -srmo
printf '%s\\n' "$(nproc)"
"""
    lines = remote(host, script).stdout.splitlines()
    if len(lines) != 3:
        raise RuntimeError(f"could not identify {host}")
    return {"ssh": host, "machine_id_sha256": hashlib.sha256(lines[0].encode()).hexdigest(), "uname": lines[1], "cpus": lines[2]}


def normalized_load(host: str) -> float:
    output = remote(host, "read -r load _ </proc/loadavg\nprintf '%s %s\\n' \"$load\" \"$(nproc)\"\n").stdout
    load, cpus = output.split()
    return float(load) / int(cpus)


def wait_idle(hosts: tuple[str, str], ceiling: float, timeout: float) -> dict[str, float]:
    started = time.monotonic()
    while True:
        loads = {host: normalized_load(host) for host in hosts}
        if all(value <= ceiling for value in loads.values()):
            return loads
        if time.monotonic() - started >= timeout:
            raise TimeoutError(f"hosts did not become idle enough: {loads}")
        time.sleep(5)


def copy_to(local: Path, host: str, destination: str) -> None:
    run(["scp", "-q", str(local), f"{host}:{destination}"])


def stage(args: argparse.Namespace, run_id: str) -> tuple[str, str]:
    work = f"/tmp/tsunami-two-host/{run_id}"
    for host in (args.sender, args.receiver):
        remote(host, f"mkdir -p {quote(work)}\nchmod 700 {quote(work)}\n")
        copy_to(args.binary, host, f"{work}/tsunami-udp")
        copy_to(args.key_file, host, f"{work}/auth.key")
        remote(host, f"chmod 700 {quote(work + '/tsunami-udp')}\nchmod 600 {quote(work + '/auth.key')}\n")
    return work, hashlib.sha256(args.binary.read_bytes()).hexdigest()


def cleanup(host: str, runtime: str, run_id: str, work: str, keep: bool) -> None:
    script = f"""
ids=$({quote(runtime)} ps -aq --filter name={quote(run_id)})
if [[ -n $ids ]]; then
  nice -n 10 ionice -c2 -n7 {quote(runtime)} rm -f $ids >/dev/null 2>&1 || true
fi
"""
    if not keep:
        script += f"rm -rf {quote(work)}\n"
    remote(host, script, check=False)


def create_source(host: str, work: str, size: int) -> str:
    path = f"{work}/source-{size}.bin"
    script = f"""
if [[ ! -f {quote(path)} || $(stat -c %s {quote(path)}) -ne {size} ]]; then
  nice -n 10 ionice -c2 -n7 dd if=/dev/zero of={quote(path)} bs=1048576 count={size // 1048576} status=none
fi
sha256sum {quote(path)} | awk '{{print $1}}'
"""
    return remote(host, script).stdout.strip()


def run_treatment(
    args: argparse.Namespace,
    work: str,
    run_id: str,
    block_index: int,
    treatment_index: int,
    scenario: dict[str, object],
    treatment: str,
    source_hash: str,
    host_info: dict[str, dict[str, str]],
    binary_hash: str,
) -> dict[str, object]:
    suffix = f"{run_id}-{block_index}-{treatment_index}"
    receiver_name = f"tsu-r-{suffix}"
    sender_name = f"tsu-s-{suffix}"
    control_port = args.base_port + (block_index * len(TREATMENTS) + treatment_index) * 2
    udp_port = control_port + 1
    output = f"{work}/output-{block_index}-{treatment}.bin"
    receiver_log = f"{work}/receiver-{block_index}-{treatment}.log"
    runtime = quote(args.runtime)
    image = quote(args.image)
    receiver_script = f"""
rm -f {quote(output)} {quote(output + '.part')} {quote(output + '.part.map')} {quote(receiver_log)}
nice -n 10 ionice -c2 -n7 {runtime} run -d --name {quote(receiver_name)} \
  -p {control_port}:9000/tcp -p {udp_port}:9001/udp \
  -v {quote(work)}:/work:z {image} \
  /work/tsunami-udp receive --listen 0.0.0.0:9000 --udp 0.0.0.0:9001 \
  --out /work/{Path(output).name} --key-file /work/auth.key
"""
    remote(args.receiver, receiver_script)
    ready = remote(
        args.receiver,
        f"""
for _ in {{1..100}}; do
  if ss -ltn | grep -q ':{control_port} '; then exit 0; fi
  sleep 0.05
done
{runtime} logs {quote(receiver_name)} 2>&1 || true
exit 1
""",
        check=False,
    )
    if ready.returncode != 0:
        raise RuntimeError(f"receiver did not listen on port {control_port}:\n{ready.stdout}\n{ready.stderr}")

    loss = float(scenario["loss_percent"])
    netem_loss = "" if loss == 0 else f" loss random {loss}%"
    seed = args.seed + block_index * 10 + treatment_index
    sender_script = f"""
nice -n 10 ionice -c2 -n7 {runtime} run --name {quote(sender_name)} --cap-add NET_ADMIN \
  -v {quote(work)}:/work:z {image} sh -lc \
  {quote(f'iface=$(ip route show default | head -n1 | cut -d " " -f5); test -n "$iface"; tc qdisc replace dev "$iface" root netem limit 10000 delay {scenario["rtt_ms"]}ms{netem_loss} rate {scenario["rate_mbit"]}mbit seed {seed} && exec /work/tsunami-udp send --connect {args.receiver_address}:{control_port} --udp-target {args.receiver_address}:{udp_port} --file /work/source-{scenario["file_bytes"]}.bin --transport {treatment} --rate-mbps {scenario["rate_mbit"]} --repair-cooldown-ms {2 * float(scenario["rtt_ms"]) + 50:.0f} --key-file /work/auth.key')}
"""
    sent = remote(args.sender, sender_script, check=False)
    if sent.returncode != 0:
        logs = remote(
            args.receiver,
            f"{runtime} logs {quote(receiver_name)} 2>&1 || true\n",
            check=False,
        ).stdout
        remote(
            args.receiver,
            f"nice -n 10 ionice -c2 -n7 {runtime} rm -f {quote(receiver_name)} >/dev/null 2>&1 || true\n",
            check=False,
        )
        raise RuntimeError(f"{treatment} sender failed ({sent.returncode}):\n{sent.stderr}\n{logs}")
    receiver_done = remote(
        args.receiver,
        f"timeout 180s nice -n 10 ionice -c2 -n7 {runtime} wait {quote(receiver_name)}\n",
        check=False,
    )
    logs = remote(args.receiver, f"{runtime} logs {quote(receiver_name)} 2>&1 || true\n", check=False).stdout
    Path(args.results).mkdir(parents=True, exist_ok=True)
    (Path(args.results) / f"receiver-{block_index}-{treatment}.log").write_text(logs)
    receiver_status = receiver_done.stdout.strip()
    if receiver_done.returncode != 0 or receiver_status != "0":
        raise RuntimeError(
            f"{treatment} failed (sender={sent.returncode}, receiver={receiver_status!r}):\n{sent.stderr}\n{logs}"
        )
    sender_json = json.loads(sent.stdout.strip().splitlines()[-1])
    output_hash = remote(args.receiver, f"sha256sum {quote(output)} | awk '{{print $1}}'\n").stdout.strip()
    if output_hash != source_hash:
        raise RuntimeError(f"hash mismatch for {treatment}: {source_hash} != {output_hash}")
    remote(args.sender, f"nice -n 10 ionice -c2 -n7 {runtime} rm -f {quote(sender_name)} >/dev/null\n", check=False)
    remote(args.receiver, f"nice -n 10 ionice -c2 -n7 {runtime} rm -f {quote(receiver_name)} >/dev/null\n", check=False)
    sender_json.update(
        {
            "verified": True,
            "binary_sha256": binary_hash,
            "source_sha256": source_hash,
            "output_sha256": output_hash,
            "scenario": scenario,
            "design": {
                "block_id": f"two-host-{scenario['name']}-{block_index}",
                "block_order": block_index,
                "treatment_order": treatment_index,
                "randomization_seed": args.seed,
                "expected_treatments": list(TREATMENTS),
            },
            "hosts": host_info,
            "isolation": "ephemeral sender/receiver containers; netem on sender container eth0 only",
        }
    )
    return sender_json


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--sender", required=True, help="SSH host for the sending Linux machine")
    parser.add_argument("--receiver", required=True, help="SSH host for the receiving Linux machine")
    parser.add_argument("--receiver-address", required=True, help="receiver address reachable from the sender container")
    parser.add_argument("--binary", required=True, type=Path, help="static Linux release binary")
    parser.add_argument("--key-file", required=True, type=Path, help="32-byte v0.1 PSK")
    parser.add_argument("--results", type=Path, default=Path("two-host-results"))
    parser.add_argument("--blocks", type=int, default=12)
    parser.add_argument("--seed", type=int, default=5101)
    parser.add_argument("--base-port", type=int, default=24000)
    parser.add_argument("--max-normalized-load", type=float, default=0.75)
    parser.add_argument("--idle-timeout", type=float, default=300.0)
    parser.add_argument("--runtime", default="podman")
    parser.add_argument("--image", default="docker.io/library/archlinux:base-devel")
    parser.add_argument("--keep-remote", action="store_true")
    parser.add_argument("--allow-same-host-smoke", action="store_true")
    parser.add_argument("--smoke", action="store_true", help="run one 1 MiB block per scenario")
    parser.add_argument("--dry-run", action="store_true")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    scenarios = DEFAULT_SCENARIOS
    if args.smoke:
        args.blocks = 1
        scenarios = tuple({**scenario, "file_bytes": 1_048_576} for scenario in DEFAULT_SCENARIOS)
    if args.blocks < 2 and not (args.smoke or args.allow_same_host_smoke):
        raise SystemExit("--blocks must be at least 2")
    if args.dry_run:
        rng = random.Random(args.seed)
        print(json.dumps([
            {"scenario": scenario, "block": block, "order": rng.sample(TREATMENTS, len(TREATMENTS))}
            for scenario in scenarios for block in range(args.blocks)
        ], indent=2))
        return
    if not args.binary.is_file() or not args.key_file.is_file():
        raise SystemExit("--binary and --key-file must be files")
    if len(args.key_file.read_bytes()) != 32:
        raise SystemExit("--key-file must contain exactly 32 bytes")
    host_info = {"sender": identity(args.sender), "receiver": identity(args.receiver)}
    if (
        host_info["sender"]["machine_id_sha256"] == host_info["receiver"]["machine_id_sha256"]
        and not args.allow_same_host_smoke
    ):
        raise SystemExit("sender and receiver are the same Linux machine; use two independent hosts")
    run_id = uuid.uuid4().hex[:10]
    work, binary_hash = stage(args, run_id)
    try:
        rng = random.Random(args.seed)
        block_index = 0
        for scenario in scenarios:
            source_hash = create_source(args.sender, work, int(scenario["file_bytes"]))
            for scenario_block in range(args.blocks):
                loads = wait_idle((args.sender, args.receiver), args.max_normalized_load, args.idle_timeout)
                order = rng.sample(TREATMENTS, len(TREATMENTS))
                for treatment_index, treatment in enumerate(order):
                    result = run_treatment(
                        args, work, run_id, block_index, treatment_index, scenario,
                        treatment, source_hash, host_info, binary_hash,
                    )
                    result["design"]["scenario_block"] = scenario_block
                    result["host_load_before_block"] = loads
                    path = args.results / f"result-{result['design']['block_id']}-{treatment}.json"
                    path.write_text(json.dumps(result, indent=2) + "\n")
                    print(path, flush=True)
                block_index += 1
    finally:
        cleanup(args.sender, args.runtime, run_id, work, args.keep_remote)
        cleanup(args.receiver, args.runtime, run_id, work, args.keep_remote)


if __name__ == "__main__":
    main()
