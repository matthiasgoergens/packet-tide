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
import re
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
SUPPORTED_TREATMENTS = (
    "udp",
    "tcp",
    "tcp-cubic",
    "tcp-bbr",
    "tcp4",
    "tcp4-cubic",
    "tcp4-bbr",
)


def treatment_transport(treatment: str) -> str:
    return "tcp4" if treatment.startswith("tcp4") else "tcp" if treatment.startswith("tcp") else "udp"


def treatment_cc(treatment: str) -> str | None:
    return treatment.rsplit("-", 1)[1] if "-" in treatment else None


def load_scenarios(path: Path | None) -> tuple[dict[str, object], ...]:
    if path is None:
        return DEFAULT_SCENARIOS
    decoded = json.loads(path.read_text())
    if not isinstance(decoded, list) or not decoded:
        raise SystemExit("--scenario-file must contain a nonempty JSON array")
    scenarios: list[dict[str, object]] = []
    names: set[str] = set()
    required = {"name", "file_bytes", "rate_mbit", "rtt_ms", "loss_percent"}
    for index, scenario in enumerate(decoded):
        if not isinstance(scenario, dict) or set(scenario) != required:
            raise SystemExit(f"scenario {index} must contain exactly {sorted(required)}")
        name = scenario["name"]
        if (
            not isinstance(name, str)
            or re.fullmatch(r"[a-z0-9][a-z0-9-]{0,62}", name) is None
            or name in names
        ):
            raise SystemExit(f"scenario {index} has an empty or duplicate name")
        names.add(name)
        try:
            file_bytes = int(scenario["file_bytes"])
            rate_mbit = float(scenario["rate_mbit"])
            rtt_ms = float(scenario["rtt_ms"])
            loss_percent = float(scenario["loss_percent"])
        except (TypeError, ValueError) as error:
            raise SystemExit(f"scenario {name!r} contains a nonnumeric value") from error
        if file_bytes <= 0 or rate_mbit <= 0 or rtt_ms < 0 or not 0 <= loss_percent < 100:
            raise SystemExit(f"scenario {name!r} contains an out-of-range value")
        scenarios.append(
            {
                "name": name,
                "file_bytes": file_bytes,
                "rate_mbit": rate_mbit,
                "rtt_ms": rtt_ms,
                "loss_percent": loss_percent,
            }
        )
    return tuple(scenarios)


def build_plan(
    scenarios: tuple[dict[str, object], ...],
    blocks: int,
    treatments: tuple[str, ...],
    seed: int,
) -> list[dict[str, object]]:
    rng = random.Random(seed)
    bases = {scenario["name"]: rng.sample(treatments, len(treatments)) for scenario in scenarios}
    plan: list[dict[str, object]] = []
    block_index = 0
    for scenario_block in range(blocks):
        scenario_order = list(scenarios)
        rng.shuffle(scenario_order)
        for scenario in scenario_order:
            base = bases[scenario["name"]]
            rotation = scenario_block % len(treatments)
            order = base[rotation:] + base[:rotation]
            plan.append(
                {
                    "scenario": scenario["name"],
                    "scenario_block": scenario_block,
                    "block_index": block_index,
                    "block_id": f"two-host-{scenario['name']}-{block_index}",
                    "order": order,
                    "netem_seed": seed + block_index,
                }
            )
            block_index += 1
    return plan


def run(command: list[str], *, input_text: str | None = None, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, input=input_text, text=True, capture_output=True, check=check)


def write_json_atomic(path: Path, value: object) -> None:
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


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


def container_identity(
    host: str, runtime: str, image: str, jump: str | None = None
) -> dict[str, str]:
    script = f"""
{quote(runtime)} --version
{quote(runtime)} image inspect {quote(image)} --format '{{{{.Digest}}}} {{{{.Id}}}}'
"""
    lines = remote(host, script, jump=jump).stdout.splitlines()
    if len(lines) != 2 or len(lines[1].split()) != 2:
        raise RuntimeError("container image must already exist with a stable digest")
    digest, image_id = lines[1].split()
    return {"runtime_version": lines[0], "image": image, "digest": digest, "image_id": image_id}


def source_identity(scenario_file: Path | None) -> dict[str, object]:
    revision = run(["git", "rev-parse", "HEAD"]).stdout.strip()
    tracked_dirty = (
        run(["git", "diff", "--quiet"], check=False).returncode != 0
        or run(["git", "diff", "--cached", "--quiet"], check=False).returncode != 0
    )
    harness = Path(__file__).resolve()
    return {
        "git_commit": revision,
        "git_tracked_worktree_dirty": tracked_dirty,
        "harness_sha256": hashlib.sha256(harness.read_bytes()).hexdigest(),
        "scenario_file": str(scenario_file) if scenario_file is not None else None,
        "scenario_file_sha256": (
            hashlib.sha256(scenario_file.read_bytes()).hexdigest()
            if scenario_file is not None else None
        ),
    }


def available_congestion_controls(host: str, jump: str | None = None) -> list[str]:
    return remote(
        host, "sysctl -n net.ipv4.tcp_available_congestion_control\n", jump=jump
    ).stdout.split()


def pressure_snapshot(host: str, jump: str | None = None) -> dict[str, object]:
    output = remote(
        host,
        """read -r load _ </proc/loadavg
pressure() {
  if [[ -r /proc/pressure/$1 ]]; then
    awk '/^some / {for (i=1;i<=NF;i++) if ($i ~ /^avg10=/) {split($i,a,"="); print a[2]}}' "/proc/pressure/$1"
  else
    printf '0\\n'
  fi
}
cpu=$(pressure cpu)
io=$(pressure io)
memory=$(pressure memory)
printf '%s %s %s %s %s\\n' "$load" "$(nproc)" "${cpu:-0}" "${io:-0}" "${memory:-0}"
""",
        jump=jump,
    ).stdout
    load, cpus, cpu, io, memory = output.split()
    return {
        "captured_at": datetime.now(timezone.utc).isoformat(),
        "normalized_load_1m": float(load) / int(cpus),
        "psi_cpu_some_avg10": float(cpu),
        "psi_io_some_avg10": float(io),
        "psi_memory_some_avg10": float(memory),
    }


def wait_idle(
    hosts: tuple[tuple[str, str | None], tuple[str, str | None]],
    ceiling: float,
    timeout: float,
    consecutive_samples: int,
    sample_seconds: float,
) -> dict[str, list[dict[str, object]]]:
    started = time.monotonic()
    accepted: dict[str, list[dict[str, object]]] = {host: [] for host, _ in hosts}
    while True:
        snapshots = {host: pressure_snapshot(host, jump) for host, jump in hosts}
        if all(value["normalized_load_1m"] <= ceiling for value in snapshots.values()):
            for host, snapshot in snapshots.items():
                accepted[host].append(snapshot)
            if all(len(values) >= consecutive_samples for values in accepted.values()):
                return accepted
        else:
            accepted = {host: [] for host, _ in hosts}
        if time.monotonic() - started >= timeout:
            raise TimeoutError(f"hosts did not remain idle enough: {snapshots}")
        time.sleep(sample_seconds)


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
        if endpoint == "receiver" and args.receiver_mode == "native":
            remote(
                host,
                f"pkill -TERM -f -- {quote('^' + work + '/packet-tide receive ')} 2>/dev/null || true\n",
                check=False,
                jump=jump,
            )
        remote(host, f"mkdir -p {quote(work)}\nchmod 700 {quote(work)}\n", jump=jump)
        copy_to(endpoint_binaries[endpoint], host, f"{work}/packet-tide", jump)
        copy_to(args.key_file, host, f"{work}/auth.key", jump)
        remote(
            host,
            f"chmod 700 {quote(work + '/packet-tide')}\nchmod 600 {quote(work + '/auth.key')}\n",
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
    else:
        script += f"pkill -TERM -f -- {quote('^' + work + '/packet-tide receive ')} 2>/dev/null || true\n"
    if not keep:
        script += f"rm -rf {quote(work)}\n"
    remote(host, script, check=False, jump=jump)


def create_source(host: str, work: str, size: int, jump: str | None = None) -> str:
    path = f"{work}/source-{size}.bin"
    script = f"""
if [[ ! -f {quote(path)} || $(stat -c %s {quote(path)}) -ne {size} ]]; then
  nice -n 10 ionice -c2 -n7 truncate -s {size} {quote(path)}
fi
test $(stat -c %s {quote(path)}) -eq {size}
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
    network_seed: int,
    source_hash: str,
    host_info: dict[str, dict[str, str]],
    binary_hashes: dict[str, str],
) -> dict[str, object]:
    suffix = f"{run_id}-{block_index}-{treatment_index}"
    receiver_name = f"tsu-r-{suffix}"
    sender_name = f"tsu-s-{suffix}"
    control_port = args.base_port + (block_index * len(args.treatments) + treatment_index) * 2
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
exec nice -n 10 ionice -c2 -n7 {quote(work + '/packet-tide')} receive \
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
  /work/packet-tide receive --listen 0.0.0.0:9000 --udp 0.0.0.0:9001 \
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
        if receiver_process is not None:
            receiver_process.terminate()
            try:
                receiver_process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                receiver_process.kill()
                receiver_process.communicate()
        raise RuntimeError(f"receiver did not listen on port {control_port}:\n{ready.stdout}\n{ready.stderr}")

    loss = float(scenario["loss_percent"])
    netem_loss = "" if loss == 0 else f" loss random {loss}%"
    seed = network_seed
    sender_rate_mbit = (
        args.udp_rate_mbit
        if treatment == "udp" and args.udp_rate_mbit is not None
        else scenario["rate_mbit"]
    )
    program_transport = treatment_transport(treatment)
    congestion_control = treatment_cc(treatment)
    set_congestion_control = (
        ""
        if congestion_control is None
        else (
            f"sysctl -q -w net.ipv4.tcp_congestion_control={congestion_control} && "
            f'test "$(sysctl -n net.ipv4.tcp_congestion_control)" = {congestion_control} && '
        )
    )
    sender_script = f"""
nice -n 10 ionice -c2 -n7 {runtime} run --name {quote(sender_name)} --cap-add NET_ADMIN \
  -v {quote(work)}:/work:z {image} sh -lc \
  {quote(f'iface=$(ip route show default | head -n1 | cut -d " " -f5); test -n "$iface"; ip link set dev "$iface" gso_max_size 1500 gso_ipv4_max_size 1500 gro_max_size 1500 gro_ipv4_max_size 1500; tc qdisc replace dev "$iface" root netem limit 10000 delay {scenario["rtt_ms"]}ms{netem_loss} rate {scenario["rate_mbit"]}mbit seed {seed} && {set_congestion_control}exec /work/packet-tide send --connect {args.receiver_address}:{control_port} --udp-target {args.receiver_address}:{udp_port} --file /work/source-{scenario["file_bytes"]}.bin --transport {program_transport} --rate-mbps {sender_rate_mbit} --repair-cooldown-ms {2 * float(scenario["rtt_ms"]) + 50:.0f} --udp-payload-bytes {args.udp_payload_bytes} --feedback-interval-ms {args.feedback_interval_ms} --key-file /work/auth.key')}
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
    receiver_summaries = []
    for line in logs.splitlines():
        try:
            candidate = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (
            isinstance(candidate, dict)
            and candidate.get("schema_version") == 1
            and candidate.get("role") == "receiver"
        ):
            receiver_summaries.append(candidate)
    receiver_summary = receiver_summaries[-1] if receiver_summaries else None
    if sender_json.get("schema_version") == 1 and receiver_summary is None:
        raise RuntimeError(f"{treatment} receiver did not emit a schema-versioned summary:\n{logs}")
    if program_transport == "udp" and sender_json.get("schema_version") == 1:
        expected_chunks = (
            scenario["file_bytes"] + args.udp_payload_bytes - 1
        ) // args.udp_payload_bytes
        expected = {
            "received_chunks": sender_json.get("receiver_received_chunks"),
            "frontier_chunks": sender_json.get("receiver_frontier_chunks"),
            "accepted_datagrams": sender_json.get("receiver_accepted_datagrams"),
            "valid_datagrams": sender_json.get("receiver_valid_datagrams"),
            "duplicate_datagrams": sender_json.get("receiver_duplicate_datagrams"),
            "invalid_datagrams": sender_json.get("receiver_invalid_datagrams"),
            "repair_datagrams": sender_json.get("receiver_repair_datagrams"),
            "socket_drops": sender_json.get("receiver_socket_drops"),
            "reports": sender_json.get("receiver_reports"),
        }
        if any(receiver_summary.get(key) != value for key, value in expected.items()):
            raise RuntimeError(
                f"{treatment} sender and receiver telemetry disagree: {expected} != {receiver_summary}"
            )
        if (
            sender_json.get("udp_payload_bytes") != args.udp_payload_bytes
            or sender_json.get("feedback_interval_ms") != args.feedback_interval_ms
            or receiver_summary.get("udp_payload_bytes") != args.udp_payload_bytes
            or receiver_summary.get("feedback_interval_ms") != args.feedback_interval_ms
            or expected["received_chunks"] != expected_chunks
            or expected["frontier_chunks"] != expected_chunks
            or expected["accepted_datagrams"] != expected_chunks
            or expected["valid_datagrams"]
            != expected["accepted_datagrams"] + expected["duplicate_datagrams"]
            or sender_json["datagrams"] < expected["valid_datagrams"]
            or sender_json["repairs"] < expected["repair_datagrams"]
            or expected["reports"] < 1
        ):
            raise RuntimeError(f"{treatment} receiver telemetry failed reconciliation: {expected}")
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
    wire_transport = sender_json["transport"]
    sender_json.update(
        {
            "transport": treatment,
            "wire_transport": wire_transport,
            "verified": True,
            "binary_sha256": binary_hashes["sender"],
            "endpoint_binary_sha256": binary_hashes,
            "source_sha256": source_hash,
            "output_sha256": output_hash,
            "receiver_summary": receiver_summary,
            "scenario": scenario,
            "treatment": treatment,
            "tcp_congestion_control_requested": congestion_control,
            "tcp_congestion_control_actual": congestion_control,
            "sender_rate_mbit": sender_rate_mbit,
            "netem_seed": seed,
            "design": {
                "block_id": f"two-host-{scenario['name']}-{block_index}",
                "block_index": block_index,
                "block_order": block_index,
                "treatment_order": treatment_index,
                "netem_seed": seed,
                "randomization_seed": args.seed,
                "expected_treatments": list(args.treatments),
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
    parser.add_argument("--scenario-file", type=Path, help="JSON array of exploratory scenarios")
    parser.add_argument(
        "--study-kind",
        choices=("confirmatory", "exploratory"),
        default="confirmatory",
    )
    parser.add_argument(
        "--treatments",
        default=",".join(TREATMENTS),
        help="comma-separated randomized treatments",
    )
    parser.add_argument("--blocks", type=int, default=12)
    parser.add_argument("--seed", type=int, default=5101)
    parser.add_argument("--base-port", type=int, default=24000)
    parser.add_argument("--max-normalized-load", type=float, default=0.75)
    parser.add_argument("--idle-consecutive-samples", type=int, default=3)
    parser.add_argument("--idle-sample-seconds", type=float, default=2.0)
    parser.add_argument(
        "--udp-rate-mbit",
        type=float,
        help="optional UDP offered rate, independent of the emulated bottleneck rate",
    )
    parser.add_argument("--udp-payload-bytes", type=int, default=1172)
    parser.add_argument("--feedback-interval-ms", type=int, default=50)
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
    scenarios = load_scenarios(args.scenario_file)
    args.treatments = tuple(args.treatments.split(","))
    if (
        not args.treatments
        or len(set(args.treatments)) != len(args.treatments)
        or any(value not in SUPPORTED_TREATMENTS for value in args.treatments)
        or "udp" not in args.treatments
    ):
        raise SystemExit(
            f"--treatments must be unique supported values including udp: {SUPPORTED_TREATMENTS}"
        )
    if args.smoke:
        args.blocks = 1
        scenarios = tuple({**scenario, "file_bytes": 1_048_576} for scenario in scenarios)
    required_ports = len(scenarios) * args.blocks * len(args.treatments) * 2
    if args.base_port < 1024 or args.base_port + required_ports - 1 > 65_535:
        raise SystemExit("the selected matrix does not fit in the available TCP/UDP port range")
    if args.blocks < 2 and not (args.smoke or args.allow_same_host_smoke):
        raise SystemExit("--blocks must be at least 2")
    if args.udp_rate_mbit is not None and args.udp_rate_mbit <= 0:
        raise SystemExit("--udp-rate-mbit must be positive")
    if args.idle_consecutive_samples < 1 or args.idle_sample_seconds <= 0:
        raise SystemExit("idle sampling values must be positive")
    if not 256 <= args.udp_payload_bytes <= 1424:
        raise SystemExit("--udp-payload-bytes must be between 256 and 1424")
    if not 10 <= args.feedback_interval_ms <= 10_000:
        raise SystemExit("--feedback-interval-ms must be between 10 and 10000")
    if args.dry_run:
        print(json.dumps(build_plan(scenarios, args.blocks, args.treatments, args.seed), indent=2))
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
    container_info = container_identity(
        args.sender, args.runtime, args.image, args.sender_proxy_jump
    )
    source_info = source_identity(args.scenario_file)
    available_cc = available_congestion_controls(args.sender, args.sender_proxy_jump)
    requested_cc = {value for value in map(treatment_cc, args.treatments) if value is not None}
    missing_cc = requested_cc - set(available_cc)
    if missing_cc:
        raise SystemExit(f"requested unavailable congestion controls: {sorted(missing_cc)}")
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
    plan = build_plan(scenarios, args.blocks, args.treatments, args.seed)
    preregistration = {
        "schema": 1,
        "study_kind": args.study_kind,
        "smoke_mode": args.smoke,
        "run_id": run_id,
        "endpoint_binary_sha256": binary_hashes,
        "hosts": host_info,
        "container": container_info,
        "source": source_info,
        "available_tcp_congestion_control": available_cc,
        "scenarios": list(scenarios),
        "blocks_per_scenario": args.blocks,
        "treatments": list(args.treatments),
        "randomization_seed": args.seed,
        "udp_sender_rate_mbit": args.udp_rate_mbit,
        "udp_payload_bytes": args.udp_payload_bytes,
        "feedback_interval_ms": args.feedback_interval_ms,
        "idle_gate": {
            "maximum_normalized_load_1m": args.max_normalized_load,
            "consecutive_samples": args.idle_consecutive_samples,
            "sample_seconds": args.idle_sample_seconds,
            "scope": "before every treatment",
            "recorded_pressure": ["cpu.some.avg10", "io.some.avg10", "memory.some.avg10"],
        },
        "sender_private_interface_offload_cap_bytes": 1500,
        "plan": plan,
        "release_gates": {
            "minimum_complete_blocks_per_scenario": 10,
            "clean_udp_over_best_tcp_upper_95pct_max": 1.05,
            "lossy_best_tcp_over_udp_lower_95pct_min": 1.25,
        },
        "analysis_plan": {
            "exclusions": [
                "incomplete randomized block",
                "failed file or digest verification",
                "host load above the preregistered idle gate before a block",
            ],
            "summaries": [
                "median elapsed time and goodput by scenario and treatment",
                "paired UDP-to-each-baseline elapsed-time ratios",
                "median absolute deviation and 95% block-bootstrap intervals",
            ],
            "bootstrap_replicates": 10000,
            "bootstrap_seed": 0,
            "minimum_complete_blocks_per_scenario": 4,
            "decision_rule": (
                "exploratory crossover mapping only; no confirmatory pass/fail claim"
                if args.study_kind == "exploratory"
                else "apply the separately frozen release gates"
            ),
        },
    }
    if previous_preregistration is not None:
        comparable_keys = (
            "run_id",
            "study_kind",
            "smoke_mode",
            "endpoint_binary_sha256",
            "hosts",
            "container",
            "source",
            "available_tcp_congestion_control",
            "scenarios",
            "blocks_per_scenario",
            "treatments",
            "randomization_seed",
            "udp_sender_rate_mbit",
            "udp_payload_bytes",
            "feedback_interval_ms",
            "idle_gate",
            "sender_private_interface_offload_cap_bytes",
            "plan",
            "release_gates",
            "analysis_plan",
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
        scenarios_by_name = {scenario["name"]: scenario for scenario in scenarios}
        source_hashes: dict[int, str] = {}
        new_quarantines = 0
        for entry in plan:
            block_index = int(entry["block_index"])
            scenario_block = int(entry["scenario_block"])
            scenario = scenarios_by_name[entry["scenario"]]
            block_id = str(entry["block_id"])
            quarantine_path = args.results / f"quarantine-{block_id}.json"
            journal_path = args.results / f"in-progress-{block_id}.json"
            if journal_path.is_file() and not quarantine_path.is_file():
                journal = json.loads(journal_path.read_text())
                journal.update(
                    {
                        "excluded_at": datetime.now(timezone.utc).isoformat(),
                        "reason": "controller stopped while this block was in progress",
                    }
                )
                write_json_atomic(quarantine_path, journal)
                journal_path.unlink()
            if quarantine_path.is_file():
                print(f"skipping preregistered excluded block {block_id}", flush=True)
                continue
            file_bytes = int(scenario["file_bytes"])
            if file_bytes not in source_hashes:
                source_hashes[file_bytes] = create_source(
                    args.sender, work, file_bytes, args.sender_proxy_jump
                )
            source_hash = source_hashes[file_bytes]
            existing_paths = {
                treatment: args.results / f"result-{block_id}-{treatment}.json"
                for treatment in args.treatments
            }
            existing = {
                treatment: path for treatment, path in existing_paths.items() if path.is_file()
            }
            if existing:
                if len(existing) != len(args.treatments):
                    raise RuntimeError(
                        f"partial published randomized block {block_id}: {sorted(existing)}"
                    )
                for treatment, path in existing.items():
                    result = json.loads(path.read_text())
                    if (
                        result.get("verified") is not True
                        or result.get("treatment", result.get("transport")) != treatment
                        or result.get("design", {}).get("block_id") != block_id
                        or result.get("endpoint_binary_sha256") != binary_hashes
                        or result.get("source_sha256") != result.get("output_sha256")
                    ):
                        raise RuntimeError(f"invalid completed result in {path}")
                print(f"skipping complete block {block_id}", flush=True)
                continue
            block_results: list[tuple[str, dict[str, object]]] = []
            journal: dict[str, object] = {
                "block_id": block_id,
                "started_at": datetime.now(timezone.utc).isoformat(),
                "completed_treatments": [],
                "observations": [],
            }
            write_json_atomic(journal_path, journal)
            try:
                for treatment_index, treatment in enumerate(entry["order"]):
                    loads = wait_idle(
                        (
                            (args.sender, args.sender_proxy_jump),
                            (args.receiver, args.receiver_proxy_jump),
                        ),
                        args.max_normalized_load,
                        args.idle_timeout,
                        args.idle_consecutive_samples,
                        args.idle_sample_seconds,
                    )
                    result = run_treatment(
                        args, work, run_id, block_index, treatment_index, scenario,
                        treatment, int(entry["netem_seed"]), source_hash, host_info, binary_hashes,
                    )
                    result["design"]["scenario_block"] = scenario_block
                    result["admission_load_before_treatment"] = loads
                    block_results.append((treatment, result))
                    journal["completed_treatments"] = [name for name, _ in block_results]
                    journal["observations"] = [value for _, value in block_results]
                    write_json_atomic(journal_path, journal)
                for treatment, result in block_results:
                    path = existing_paths[treatment]
                    path.write_text(json.dumps(result, indent=2) + "\n")
                    print(path, flush=True)
                journal_path.unlink()
            except BaseException as error:
                journal.update(
                    {
                        "excluded_at": datetime.now(timezone.utc).isoformat(),
                        "reason": str(error) or type(error).__name__,
                    }
                )
                write_json_atomic(quarantine_path, journal)
                journal_path.unlink(missing_ok=True)
                new_quarantines += 1
                print(f"quarantined incomplete block {block_id}: {error}", file=sys.stderr)
                if isinstance(error, (KeyboardInterrupt, SystemExit)):
                    raise
        if new_quarantines:
            raise RuntimeError(f"{new_quarantines} new randomized blocks were quarantined")
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
