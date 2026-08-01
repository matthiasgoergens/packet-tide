#!/usr/bin/env python3
"""Run an authenticated, randomized transfer matrix across two Linux hosts.

The sender runs in an ephemeral Podman container whose private interface carries
the netem qdisc. The receiver can run host-native, so a small ARM Linux endpoint
does not need a container runtime. No host NIC or manually prepared namespace is
used.
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
from datetime import datetime, timezone
from pathlib import Path


DEFAULT_SCENARIOS = (
    {"name": "clean", "file_bytes": 16_777_216, "rate_mbit": 100.0, "rtt_ms": 20.0, "loss_percent": 0.0},
    {"name": "lossy", "file_bytes": 16_777_216, "rate_mbit": 100.0, "rtt_ms": 100.0, "loss_percent": 1.0},
)
TREATMENTS = ("udp", "tcp", "tcp4")


def run(command: list[str], *, input_text: str | None = None, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, input=input_text, text=True, capture_output=True, check=check)


def ssh_options(jump: str | None) -> list[str]:
    return [] if jump is None else ["-o", f"ProxyJump={jump}"]


def remote(
    host: str,
    script: str,
    *,
    check: bool = True,
    jump: str | None = None,
) -> subprocess.CompletedProcess[str]:
    return run(
        ["ssh", *ssh_options(jump), host, "bash", "-s"],
        input_text="set -euo pipefail\n" + script,
        check=check,
    )


def start_remote(host: str, script: str, jump: str | None = None) -> subprocess.Popen[str]:
    process = subprocess.Popen(
        ["ssh", *ssh_options(jump), host, "bash", "-s"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert process.stdin is not None
    process.stdin.write("set -euo pipefail\n" + script)
    process.stdin.close()
    process.stdin = None
    return process


def quote(value: object) -> str:
    return shlex.quote(str(value))


def identity(host: str, jump: str | None = None) -> dict[str, str]:
    script = """
machine=$(cat /etc/machine-id)
printf '%s\\n' "$machine"
uname -srmo
printf '%s\\n' "$(nproc)"
"""
    lines = remote(host, script, jump=jump).stdout.splitlines()
    if len(lines) != 3:
        raise RuntimeError(f"could not identify {host}")
    return {"ssh": host, "machine_id_sha256": hashlib.sha256(lines[0].encode()).hexdigest(), "uname": lines[1], "cpus": lines[2]}


def normalized_load(host: str, jump: str | None = None) -> float:
    output = remote(
        host,
        "read -r load _ </proc/loadavg\nprintf '%s %s\\n' \"$load\" \"$(nproc)\"\n",
        jump=jump,
    ).stdout
    load, cpus = output.split()
    return float(load) / int(cpus)


def wait_idle(
    hosts: tuple[tuple[str, str | None], tuple[str, str | None]],
    ceiling: float,
    timeout: float,
) -> dict[str, float]:
    started = time.monotonic()
    while True:
        loads = {host: normalized_load(host, jump) for host, jump in hosts}
        if all(value <= ceiling for value in loads.values()):
            return loads
        if time.monotonic() - started >= timeout:
            raise TimeoutError(f"hosts did not become idle enough: {loads}")
        time.sleep(5)


def copy_to(local: Path, host: str, destination: str, jump: str | None = None) -> None:
    run(["scp", "-q", *ssh_options(jump), str(local), f"{host}:{destination}"])


def stage(args: argparse.Namespace, run_id: str) -> tuple[str, dict[str, str]]:
    work = f"/tmp/tsunami-two-host/{run_id}"
    endpoint_binaries = {
        "sender": args.binary,
        "receiver": args.receiver_binary or args.binary,
    }
    for endpoint, host, jump in (
        ("sender", args.sender, args.sender_proxy_jump),
        ("receiver", args.receiver, args.receiver_proxy_jump),
    ):
        remote(host, f"mkdir -p {quote(work)}\nchmod 700 {quote(work)}\n", jump=jump)
        copy_to(endpoint_binaries[endpoint], host, f"{work}/tsunami-udp", jump)
        copy_to(args.key_file, host, f"{work}/auth.key", jump)
        remote(
            host,
            f"chmod 700 {quote(work + '/tsunami-udp')}\nchmod 600 {quote(work + '/auth.key')}\n",
            jump=jump,
        )
    hashes = {
        endpoint: hashlib.sha256(path.read_bytes()).hexdigest()
        for endpoint, path in endpoint_binaries.items()
    }
    return work, hashes


def cleanup(
    host: str,
    runtime: str,
    run_id: str,
    work: str,
    keep: bool,
    containers: bool,
    jump: str | None = None,
) -> None:
    script = ""
    if containers:
        script += f"""
ids=$({quote(runtime)} ps -aq --filter name={quote(run_id)})
if [[ -n $ids ]]; then
  nice -n 10 ionice -c2 -n7 {quote(runtime)} rm -f $ids >/dev/null 2>&1 || true
fi
"""
    if not keep:
        script += f"rm -rf {quote(work)}\n"
    remote(host, script, check=False, jump=jump)


def create_source(host: str, work: str, size: int, jump: str | None = None) -> str:
    path = f"{work}/source-{size}.bin"
    script = f"""
if [[ ! -f {quote(path)} || $(stat -c %s {quote(path)}) -ne {size} ]]; then
  nice -n 10 ionice -c2 -n7 dd if=/dev/zero of={quote(path)} bs=1048576 count={size // 1048576} status=none
fi
sha256sum {quote(path)} | awk '{{print $1}}'
"""
    return remote(host, script, jump=jump).stdout.strip()


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
    binary_hashes: dict[str, str],
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
    remote(
        args.receiver,
        f"rm -f {quote(output)} {quote(output + '.part')} {quote(output + '.part.map')} {quote(receiver_log)}\n",
        jump=args.receiver_proxy_jump,
    )
    receiver_process: subprocess.Popen[str] | None = None
    if args.receiver_mode == "native":
        receiver_process = start_remote(
            args.receiver,
            f"""
exec nice -n 10 ionice -c2 -n7 {quote(work + '/tsunami-udp')} receive \
  --listen 0.0.0.0:{control_port} --udp 0.0.0.0:{udp_port} \
  --out {quote(output)} --key-file {quote(work + '/auth.key')}
""",
            args.receiver_proxy_jump,
        )
    else:
        receiver_script = f"""
rm -f {quote(output)} {quote(output + '.part')} {quote(output + '.part.map')} {quote(receiver_log)}
nice -n 10 ionice -c2 -n7 {runtime} run -d --name {quote(receiver_name)} \
  -p {control_port}:9000/tcp -p {udp_port}:9001/udp \
  -v {quote(work)}:/work:z {image} \
  /work/tsunami-udp receive --listen 0.0.0.0:9000 --udp 0.0.0.0:9001 \
  --out /work/{Path(output).name} --key-file /work/auth.key
"""
        remote(args.receiver, receiver_script, jump=args.receiver_proxy_jump)
    ready = remote(
        args.receiver,
        f"""
for _ in {{1..100}}; do
  if ss -ltn | grep -q ':{control_port} '; then exit 0; fi
  sleep 0.05
done
{f'{runtime} logs {quote(receiver_name)} 2>&1 || true' if args.receiver_mode == 'container' else 'true'}
exit 1
""",
        check=False,
        jump=args.receiver_proxy_jump,
    )
    if ready.returncode != 0:
        raise RuntimeError(f"receiver did not listen on port {control_port}:\n{ready.stdout}\n{ready.stderr}")

    loss = float(scenario["loss_percent"])
    netem_loss = "" if loss == 0 else f" loss random {loss}%"
    seed = args.seed + block_index * 10 + treatment_index
    sender_rate_mbit = (
        args.udp_rate_mbit
        if treatment == "udp" and args.udp_rate_mbit is not None
        else scenario["rate_mbit"]
    )
    sender_script = f"""
nice -n 10 ionice -c2 -n7 {runtime} run --name {quote(sender_name)} --cap-add NET_ADMIN \
  -v {quote(work)}:/work:z {image} sh -lc \
  {quote(f'iface=$(ip route show default | head -n1 | cut -d " " -f5); test -n "$iface"; ip link set dev "$iface" gso_max_size 1500 gso_ipv4_max_size 1500 gro_max_size 1500 gro_ipv4_max_size 1500; tc qdisc replace dev "$iface" root netem limit 10000 delay {scenario["rtt_ms"]}ms{netem_loss} rate {scenario["rate_mbit"]}mbit seed {seed} && exec /work/tsunami-udp send --connect {args.receiver_address}:{control_port} --udp-target {args.receiver_address}:{udp_port} --file /work/source-{scenario["file_bytes"]}.bin --transport {treatment} --rate-mbps {sender_rate_mbit} --repair-cooldown-ms {2 * float(scenario["rtt_ms"]) + 50:.0f} --key-file /work/auth.key')}
"""
    sent = remote(
        args.sender,
        sender_script,
        check=False,
        jump=args.sender_proxy_jump,
    )
    if sent.returncode != 0:
        if receiver_process is not None:
            receiver_process.terminate()
            try:
                receiver_stdout, receiver_stderr = receiver_process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                receiver_process.kill()
                receiver_stdout, receiver_stderr = receiver_process.communicate()
            logs = receiver_stdout + receiver_stderr
        else:
            logs = remote(
                args.receiver,
                f"{runtime} logs {quote(receiver_name)} 2>&1 || true\n",
                check=False,
                jump=args.receiver_proxy_jump,
            ).stdout
            remote(
                args.receiver,
                f"nice -n 10 ionice -c2 -n7 {runtime} rm -f {quote(receiver_name)} >/dev/null 2>&1 || true\n",
                check=False,
                jump=args.receiver_proxy_jump,
            )
        raise RuntimeError(f"{treatment} sender failed ({sent.returncode}):\n{sent.stderr}\n{logs}")
    if receiver_process is not None:
        try:
            receiver_stdout, receiver_stderr = receiver_process.communicate(timeout=180)
        except subprocess.TimeoutExpired as error:
            receiver_process.terminate()
            raise RuntimeError(f"{treatment} native receiver timed out") from error
        receiver_returncode = receiver_process.returncode
        receiver_status = str(receiver_returncode)
        logs = receiver_stdout + receiver_stderr
        receiver_failed = receiver_returncode != 0
    else:
        receiver_done = remote(
            args.receiver,
            f"timeout 180s nice -n 10 ionice -c2 -n7 {runtime} wait {quote(receiver_name)}\n",
            check=False,
            jump=args.receiver_proxy_jump,
        )
        logs = remote(
            args.receiver,
            f"{runtime} logs {quote(receiver_name)} 2>&1 || true\n",
            check=False,
            jump=args.receiver_proxy_jump,
        ).stdout
        receiver_status = receiver_done.stdout.strip()
        receiver_failed = receiver_done.returncode != 0 or receiver_status != "0"
    Path(args.results).mkdir(parents=True, exist_ok=True)
    (Path(args.results) / f"receiver-{block_index}-{treatment}.log").write_text(logs)
    if receiver_failed:
        raise RuntimeError(
            f"{treatment} failed (sender={sent.returncode}, receiver={receiver_status!r}):\n{sent.stderr}\n{logs}"
        )
    sender_json = json.loads(sent.stdout.strip().splitlines()[-1])
    output_hash = remote(
        args.receiver,
        f"sha256sum {quote(output)} | awk '{{print $1}}'\n",
        jump=args.receiver_proxy_jump,
    ).stdout.strip()
    if output_hash != source_hash:
        raise RuntimeError(f"hash mismatch for {treatment}: {source_hash} != {output_hash}")
    remote(
        args.sender,
        f"nice -n 10 ionice -c2 -n7 {runtime} rm -f {quote(sender_name)} >/dev/null\n",
        check=False,
        jump=args.sender_proxy_jump,
    )
    if args.receiver_mode == "container":
        remote(
            args.receiver,
            f"nice -n 10 ionice -c2 -n7 {runtime} rm -f {quote(receiver_name)} >/dev/null\n",
            check=False,
            jump=args.receiver_proxy_jump,
        )
    sender_json.update(
        {
            "verified": True,
            "binary_sha256": binary_hashes["sender"],
            "endpoint_binary_sha256": binary_hashes,
            "source_sha256": source_hash,
            "output_sha256": output_hash,
            "scenario": scenario,
            "sender_rate_mbit": sender_rate_mbit,
            "design": {
                "block_id": f"two-host-{scenario['name']}-{block_index}",
                "block_order": block_index,
                "treatment_order": treatment_index,
                "randomization_seed": args.seed,
                "expected_treatments": list(TREATMENTS),
            },
            "hosts": host_info,
            "isolation": f"ephemeral sender container with netem on its private interface; {args.receiver_mode} receiver",
        }
    )
    return sender_json


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--sender", required=True, help="SSH host for the sending Linux machine")
    parser.add_argument("--receiver", required=True, help="SSH host for the receiving Linux machine")
    parser.add_argument("--sender-proxy-jump", help="optional SSH jump host for the sender")
    parser.add_argument("--receiver-proxy-jump", help="optional SSH jump host for the receiver")
    parser.add_argument("--receiver-address", required=True, help="receiver address reachable from the sender container")
    parser.add_argument("--binary", required=True, type=Path, help="static Linux sender release binary")
    parser.add_argument("--receiver-binary", type=Path, help="receiver-architecture release binary; defaults to --binary")
    parser.add_argument("--key-file", required=True, type=Path, help="32-byte v0.1 PSK")
    parser.add_argument("--results", type=Path, default=Path("two-host-results"))
    parser.add_argument("--blocks", type=int, default=12)
    parser.add_argument("--seed", type=int, default=5101)
    parser.add_argument("--base-port", type=int, default=24000)
    parser.add_argument("--max-normalized-load", type=float, default=0.75)
    parser.add_argument(
        "--udp-rate-mbit",
        type=float,
        help="optional UDP offered rate, independent of the emulated bottleneck rate",
    )
    parser.add_argument("--idle-timeout", type=float, default=300.0)
    parser.add_argument("--runtime", default="podman")
    parser.add_argument("--image", default="docker.io/library/archlinux:base-devel")
    parser.add_argument("--receiver-mode", choices=("native", "container"), default="native")
    parser.add_argument("--keep-remote", action="store_true")
    parser.add_argument("--allow-same-host-smoke", action="store_true")
    parser.add_argument("--smoke", action="store_true", help="run one 1 MiB block per scenario")
    parser.add_argument("--resume", action="store_true", help="continue only complete blocks from an interrupted preregistered run")
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
    if args.udp_rate_mbit is not None and args.udp_rate_mbit <= 0:
        raise SystemExit("--udp-rate-mbit must be positive")
    if args.dry_run:
        rng = random.Random(args.seed)
        print(json.dumps([
            {"scenario": scenario, "block": block, "order": rng.sample(TREATMENTS, len(TREATMENTS))}
            for scenario in scenarios for block in range(args.blocks)
        ], indent=2))
        return
    if not args.binary.is_file() or not args.key_file.is_file():
        raise SystemExit("--binary and --key-file must be files")
    if args.receiver_binary is not None and not args.receiver_binary.is_file():
        raise SystemExit("--receiver-binary must be a file")
    if len(args.key_file.read_bytes()) != 32:
        raise SystemExit("--key-file must contain exactly 32 bytes")
    args.results.mkdir(parents=True, exist_ok=True)
    preregistration_path = args.results / "preregistration.json"
    if args.resume and not preregistration_path.is_file():
        raise SystemExit(f"missing preregistration for --resume: {preregistration_path}")
    if not args.resume and any(args.results.iterdir()):
        raise SystemExit(f"results directory must be empty: {args.results}")
    host_info = {
        "sender": identity(args.sender, args.sender_proxy_jump),
        "receiver": identity(args.receiver, args.receiver_proxy_jump),
    }
    if (
        host_info["sender"]["machine_id_sha256"] == host_info["receiver"]["machine_id_sha256"]
        and not args.allow_same_host_smoke
    ):
        raise SystemExit("sender and receiver are the same Linux machine; use two independent hosts")
    previous_preregistration = (
        json.loads(preregistration_path.read_text()) if args.resume else None
    )
    run_id = (
        str(previous_preregistration["run_id"])
        if previous_preregistration is not None
        else uuid.uuid4().hex[:10]
    )
    work, binary_hashes = stage(args, run_id)
    rng = random.Random(args.seed)
    plan = []
    block_index = 0
    for scenario in scenarios:
        for scenario_block in range(args.blocks):
            plan.append(
                {
                    "scenario": scenario["name"],
                    "scenario_block": scenario_block,
                    "block_id": f"two-host-{scenario['name']}-{block_index}",
                    "order": rng.sample(TREATMENTS, len(TREATMENTS)),
                }
            )
            block_index += 1
    preregistration = {
        "schema": 1,
        "run_id": run_id,
        "endpoint_binary_sha256": binary_hashes,
        "hosts": host_info,
        "scenarios": list(scenarios),
        "blocks_per_scenario": args.blocks,
        "treatments": list(TREATMENTS),
        "randomization_seed": args.seed,
        "udp_sender_rate_mbit": args.udp_rate_mbit,
        "sender_private_interface_offload_cap_bytes": 1500,
        "plan": plan,
        "release_gates": {
            "minimum_complete_blocks_per_scenario": 10,
            "clean_udp_over_best_tcp_upper_95pct_max": 1.05,
            "lossy_best_tcp_over_udp_lower_95pct_min": 1.25,
        },
    }
    if previous_preregistration is not None:
        comparable_keys = (
            "run_id",
            "endpoint_binary_sha256",
            "hosts",
            "scenarios",
            "blocks_per_scenario",
            "treatments",
            "randomization_seed",
            "udp_sender_rate_mbit",
            "sender_private_interface_offload_cap_bytes",
            "plan",
            "release_gates",
        )
        mismatches = [
            key
            for key in comparable_keys
            if previous_preregistration.get(key) != preregistration.get(key)
        ]
        if mismatches:
            raise SystemExit(f"resume provenance mismatch: {', '.join(mismatches)}")
        continuation_index = len(list(args.results.glob("continuation-*.json"))) + 1
        (args.results / f"continuation-{continuation_index}.json").write_text(
            json.dumps(
                {
                    "continued_at": datetime.now(timezone.utc).isoformat(),
                    "reason": "idle gate timeout between complete randomized blocks",
                    "max_normalized_load": args.max_normalized_load,
                    "idle_timeout_seconds": args.idle_timeout,
                    "existing_results": len(list(args.results.glob("result-*.json"))),
                },
                indent=2,
            )
            + "\n"
        )
    else:
        preregistration_path.write_text(json.dumps(preregistration, indent=2) + "\n")
    try:
        block_index = 0
        for scenario in scenarios:
            source_hash = create_source(
                args.sender,
                work,
                int(scenario["file_bytes"]),
                args.sender_proxy_jump,
            )
            for scenario_block in range(args.blocks):
                block_id = f"two-host-{scenario['name']}-{block_index}"
                existing_paths = {
                    treatment: args.results / f"result-{block_id}-{treatment}.json"
                    for treatment in TREATMENTS
                }
                existing = {
                    treatment: path
                    for treatment, path in existing_paths.items()
                    if path.is_file()
                }
                if existing:
                    if len(existing) != len(TREATMENTS):
                        raise RuntimeError(
                            f"cannot resume partial randomized block {block_id}: {sorted(existing)}"
                        )
                    for treatment, path in existing.items():
                        result = json.loads(path.read_text())
                        if (
                            not result.get("verified")
                            or result.get("transport") != treatment
                            or result.get("design", {}).get("block_id") != block_id
                            or result.get("endpoint_binary_sha256") != binary_hashes
                        ):
                            raise RuntimeError(f"invalid completed result in {path}")
                    print(f"skipping complete block {block_id}", flush=True)
                    block_index += 1
                    continue
                loads = wait_idle(
                    (
                        (args.sender, args.sender_proxy_jump),
                        (args.receiver, args.receiver_proxy_jump),
                    ),
                    args.max_normalized_load,
                    args.idle_timeout,
                )
                order = plan[block_index]["order"]
                for treatment_index, treatment in enumerate(order):
                    result = run_treatment(
                        args, work, run_id, block_index, treatment_index, scenario,
                        treatment, source_hash, host_info, binary_hashes,
                    )
                    result["design"]["scenario_block"] = scenario_block
                    result["host_load_before_block"] = loads
                    path = args.results / f"result-{result['design']['block_id']}-{treatment}.json"
                    path.write_text(json.dumps(result, indent=2) + "\n")
                    print(path, flush=True)
                block_index += 1
    finally:
        cleanup(
            args.sender,
            args.runtime,
            run_id,
            work,
            args.keep_remote,
            True,
            args.sender_proxy_jump,
        )
        cleanup(
            args.receiver,
            args.runtime,
            run_id,
            work,
            args.keep_remote,
            args.receiver_mode == "container",
            args.receiver_proxy_jump,
        )


if __name__ == "__main__":
    main()
