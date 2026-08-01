#!/usr/bin/env python3
"""Conservative gate for the preregistered two-host v0.1 performance claim."""

from __future__ import annotations

import json
import math
import random
import statistics
import sys
from collections import defaultdict
from pathlib import Path


TREATMENTS = ("udp", "tcp", "tcp4")
REQUIRED_TREATMENTS = frozenset(TREATMENTS)
MIN_BLOCKS = 10
BOOTSTRAP_REPLICATES = 10_000
BOOTSTRAP_SEED = 0
CLEAN_MAX_UDP_OVER_BEST_TCP = 1.05
LOSSY_MIN_BEST_TCP_OVER_UDP = 1.25
EXPECTED = {
    "clean": (16_777_216, 100.0, 20.0, 0.0),
    "lossy": (16_777_216, 100.0, 100.0, 1.0),
}


def percentile(values: list[float], probability: float) -> float:
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, math.ceil(probability * len(ordered)) - 1))
    return ordered[index]


def geometric_mean(values: list[float]) -> float:
    return math.exp(statistics.fmean(math.log(value) for value in values))


def bootstrap(values: list[float]) -> tuple[float, float, float]:
    estimate = geometric_mean(values)
    rng = random.Random(BOOTSTRAP_SEED)
    draws = [
        geometric_mean([rng.choice(values) for _ in values])
        for _ in range(BOOTSTRAP_REPLICATES)
    ]
    return estimate, percentile(draws, 0.05), percentile(draws, 0.95)


def finite_positive(value: object) -> float:
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise ValueError("measurement is not finite and positive")
    return number


def scenario_name(result: dict) -> str | None:
    scenario = result.get("scenario", {})
    name = scenario.get("name")
    expected = EXPECTED.get(name)
    if expected is None:
        return None
    try:
        observed = (
            int(scenario["file_bytes"]),
            float(scenario["rate_mbit"]),
            float(scenario["rtt_ms"]),
            float(scenario["loss_percent"]),
        )
    except (KeyError, TypeError, ValueError):
        return None
    if observed[0] != expected[0] or any(
        not math.isclose(observed[index], expected[index], abs_tol=1e-9)
        for index in range(1, 4)
    ):
        return None
    return str(name)


def main() -> None:
    if len(sys.argv) not in (2, 3):
        raise SystemExit("usage: evaluate-release.py RESULT_DIR [OUTPUT.json]")
    result_dir = Path(sys.argv[1])
    output = Path(sys.argv[2]) if len(sys.argv) == 3 else result_dir / "release-evidence.json"
    failures: list[str] = []
    blocks: dict[str, list[dict]] = defaultdict(list)
    binary_hashes: set[str] = set()
    endpoint_binary_pairs: set[tuple[str, str]] = set()
    host_pairs: set[tuple[str, str]] = set()
    randomization_seeds: set[int] = set()

    preregistration_path = result_dir / "preregistration.json"
    preregistration: dict = {}
    if not preregistration_path.is_file():
        failures.append("missing preregistration.json")
    else:
        try:
            preregistration = json.loads(preregistration_path.read_text())
        except (json.JSONDecodeError, TypeError) as error:
            failures.append(f"invalid preregistration.json ({error})")
    expected_scenarios = [
        {
            "name": name,
            "file_bytes": values[0],
            "rate_mbit": values[1],
            "rtt_ms": values[2],
            "loss_percent": values[3],
        }
        for name, values in EXPECTED.items()
    ]
    expected_gates = {
        "minimum_complete_blocks_per_scenario": MIN_BLOCKS,
        "clean_udp_over_best_tcp_upper_95pct_max": CLEAN_MAX_UDP_OVER_BEST_TCP,
        "lossy_best_tcp_over_udp_lower_95pct_min": LOSSY_MIN_BEST_TCP_OVER_UDP,
    }
    if preregistration:
        if preregistration.get("schema") != 1:
            failures.append("unsupported preregistration schema")
        if preregistration.get("scenarios") != expected_scenarios:
            failures.append("preregistered scenarios differ from the release matrix")
        if preregistration.get("blocks_per_scenario") != 12:
            failures.append("release matrix must preregister exactly 12 blocks per scenario")
        if preregistration.get("treatments") != list(TREATMENTS):
            failures.append("preregistered treatments or their canonical order changed")
        if preregistration.get("release_gates") != expected_gates:
            failures.append("preregistered release gates changed")

    paths = sorted(result_dir.glob("result-*.json"))
    if not paths:
        failures.append("no result records")
    for path in paths:
        try:
            result = json.loads(path.read_text())
            design = result["design"]
            block_id = design["block_id"]
            transport = result["transport"]
            sender_id = result["hosts"]["sender"]["machine_id_sha256"]
            receiver_id = result["hosts"]["receiver"]["machine_id_sha256"]
            binary_hash = result["binary_sha256"]
            endpoint_hashes = result["endpoint_binary_sha256"]
            endpoint_pair = (str(endpoint_hashes["sender"]), str(endpoint_hashes["receiver"]))
            randomization_seed = int(design["randomization_seed"])
        except (json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
            failures.append(f"{path.name}: invalid result structure ({error})")
            continue
        if sender_id == receiver_id:
            failures.append(f"{path.name}: sender and receiver are the same machine")
        host_pairs.add((str(sender_id), str(receiver_id)))
        binary_hashes.add(str(binary_hash))
        endpoint_binary_pairs.add(endpoint_pair)
        randomization_seeds.add(randomization_seed)
        if transport not in REQUIRED_TREATMENTS:
            failures.append(f"{path.name}: unexpected treatment {transport!r}")
        if result.get("verified") is not True:
            failures.append(f"{path.name}: transfer was not verified")
        if result.get("source_sha256") != result.get("output_sha256"):
            failures.append(f"{path.name}: source/output digest mismatch")
        blocks[str(block_id)].append(result)

    if len(host_pairs) != 1:
        failures.append(f"expected one consistent ordered host pair, observed {len(host_pairs)}")
    if len(binary_hashes) != 1:
        failures.append(f"expected one exact sender binary, observed {len(binary_hashes)} hashes")
    if len(endpoint_binary_pairs) != 1:
        failures.append(
            f"expected one consistent sender/receiver artifact pair, observed {len(endpoint_binary_pairs)}"
        )
    if len(randomization_seeds) != 1:
        failures.append(f"expected one randomization seed, observed {len(randomization_seeds)}")

    expected_orders: dict[tuple[str, int], tuple[str, ...]] = {}
    expected_block_ids: dict[tuple[str, int], str] = {}
    if preregistration:
        try:
            for entry in preregistration["plan"]:
                key = (str(entry["scenario"]), int(entry["scenario_block"]))
                if key in expected_orders:
                    raise ValueError(f"duplicate plan entry {key}")
                expected_orders[key] = tuple(entry["order"])
                expected_block_ids[key] = str(entry["block_id"])
        except (KeyError, TypeError, ValueError) as error:
            failures.append(f"invalid preregistered block plan ({error})")
        if len(expected_orders) != 24:
            failures.append(f"preregistered plan has {len(expected_orders)} blocks; expected 24")

    ratios: dict[str, list[float]] = defaultdict(list)
    scenario_blocks: dict[str, int] = defaultdict(int)
    for block_id, results in sorted(blocks.items()):
        by_transport: dict[str, dict] = {}
        for result in results:
            transport = result.get("transport")
            if transport in by_transport:
                failures.append(f"{block_id}: duplicate treatment {transport}")
            by_transport[transport] = result
        if set(by_transport) != REQUIRED_TREATMENTS:
            failures.append(f"{block_id}: incomplete treatments {sorted(by_transport)}")
            continue
        names = {scenario_name(result) for result in results}
        if len(names) != 1 or None in names:
            failures.append(f"{block_id}: mismatched or non-preregistered scenario")
            continue
        orders = {result.get("design", {}).get("treatment_order") for result in results}
        if orders != {0, 1, 2}:
            failures.append(f"{block_id}: treatment order is not a complete randomized permutation")
            continue
        try:
            scenario_block_values = {
                int(result["design"]["scenario_block"]) for result in results
            }
        except (KeyError, TypeError, ValueError):
            failures.append(f"{block_id}: missing scenario-block index")
            continue
        if len(scenario_block_values) != 1:
            failures.append(f"{block_id}: mismatched scenario-block index")
            continue
        if any(set(result.get("design", {}).get("expected_treatments", [])) != REQUIRED_TREATMENTS for result in results):
            failures.append(f"{block_id}: expected-treatment declaration changed")
            continue
        try:
            elapsed = {name: finite_positive(result["elapsed_ms"]) for name, result in by_transport.items()}
        except (KeyError, TypeError, ValueError) as error:
            failures.append(f"{block_id}: invalid elapsed measurement ({error})")
            continue
        best_tcp = min(elapsed["tcp"], elapsed["tcp4"])
        name = names.pop()
        scenario_block = scenario_block_values.pop()
        observed_order = tuple(
            result["transport"]
            for result in sorted(results, key=lambda item: item["design"]["treatment_order"])
        )
        expected_order = expected_orders.get((name, scenario_block))
        if expected_order is None:
            failures.append(f"{block_id}: block is absent from preregistered plan")
            continue
        if expected_block_ids.get((name, scenario_block)) != block_id:
            failures.append(f"{block_id}: block ID differs from preregistered plan")
            continue
        if observed_order != expected_order:
            failures.append(
                f"{block_id}: observed order {observed_order} does not match seeded order {expected_order}"
            )
            continue
        scenario_blocks[name] += 1
        ratios[name].append(
            elapsed["udp"] / best_tcp if name == "clean" else best_tcp / elapsed["udp"]
        )

    conditions: dict[str, dict] = {}
    for name in EXPECTED:
        values = ratios[name]
        if len(values) < MIN_BLOCKS:
            failures.append(f"{name}: only {len(values)} complete blocks; require {MIN_BLOCKS}")
        if values:
            estimate, lower, upper = bootstrap(values)
        else:
            estimate = lower = upper = None
        if name == "clean":
            passed = len(values) >= MIN_BLOCKS and upper is not None and upper <= CLEAN_MAX_UDP_OVER_BEST_TCP
            conditions[name] = {
                "estimand": "geometric_mean(udp_time / best_tcp_time)",
                "complete_blocks": len(values),
                "estimate": estimate,
                "one_sided_95pct_upper": upper,
                "threshold_max": CLEAN_MAX_UDP_OVER_BEST_TCP,
                "decision": "pass" if passed else "fail",
            }
            if len(values) >= MIN_BLOCKS and not passed:
                failures.append(f"clean: upper bound {upper:.6f} exceeds {CLEAN_MAX_UDP_OVER_BEST_TCP}")
        else:
            passed = len(values) >= MIN_BLOCKS and lower is not None and lower >= LOSSY_MIN_BEST_TCP_OVER_UDP
            conditions[name] = {
                "estimand": "geometric_mean(best_tcp_time / udp_time)",
                "complete_blocks": len(values),
                "estimate": estimate,
                "one_sided_95pct_lower": lower,
                "threshold_min": LOSSY_MIN_BEST_TCP_OVER_UDP,
                "decision": "pass" if passed else "fail",
            }
            if len(values) >= MIN_BLOCKS and not passed:
                failures.append(f"lossy: lower bound {lower:.6f} is below {LOSSY_MIN_BEST_TCP_OVER_UDP}")

    evidence = {
        "claim": "authenticated v0.1 clean-path parity and lossy-path advantage between two independent Linux machines",
        "decision": "pass" if not failures else "not_established",
        "requirements": {
            "minimum_complete_blocks_per_condition": MIN_BLOCKS,
            "required_treatments": sorted(REQUIRED_TREATMENTS),
            "bootstrap_replicates": BOOTSTRAP_REPLICATES,
            "bootstrap_seed": BOOTSTRAP_SEED,
            "same_endpoint_artifacts_all_treatments": len(endpoint_binary_pairs) == 1,
            "distinct_linux_machine_ids": bool(host_pairs) and all(left != right for left, right in host_pairs),
        },
        "sender_binary_sha256": next(iter(binary_hashes)) if len(binary_hashes) == 1 else None,
        "endpoint_binary_sha256": (
            {"sender": next(iter(endpoint_binary_pairs))[0], "receiver": next(iter(endpoint_binary_pairs))[1]}
            if len(endpoint_binary_pairs) == 1
            else None
        ),
        "host_pairs": sorted([list(pair) for pair in host_pairs]),
        "conditions": conditions,
        "failures": sorted(set(failures)),
    }
    if preregistration:
        if preregistration.get("endpoint_binary_sha256") != evidence["endpoint_binary_sha256"]:
            failures.append("measured endpoint artifacts differ from preregistered artifacts")
        preregistered_hosts = preregistration.get("hosts", {})
        try:
            preregistered_pair = (
                preregistered_hosts["sender"]["machine_id_sha256"],
                preregistered_hosts["receiver"]["machine_id_sha256"],
            )
            if host_pairs != {preregistered_pair}:
                failures.append("measured hosts differ from preregistered hosts")
        except (KeyError, TypeError):
            failures.append("preregistration is missing host identities")
    evidence["failures"] = sorted(set(failures))
    evidence["decision"] = "pass" if not failures else "not_established"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    print(output)
    if failures:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
