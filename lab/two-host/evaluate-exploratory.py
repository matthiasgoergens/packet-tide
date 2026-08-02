#!/usr/bin/env python3
"""Validate and summarize a preregistered exploratory two-host matrix."""

from __future__ import annotations

import json
import math
import random
import statistics
import sys
from collections import defaultdict
from pathlib import Path


HEX_SHA256 = set("0123456789abcdef")


def sha256_digest(value: object, label: str) -> str:
    if not isinstance(value, str) or len(value) != 64 or any(c not in HEX_SHA256 for c in value):
        raise ValueError(f"{label} is not a canonical SHA-256 digest")
    return value


def treatment_transport(treatment: str) -> str:
    return "tcp4" if treatment.startswith("tcp4") else "tcp" if treatment.startswith("tcp") else "udp"


def treatment_cc(treatment: str) -> str | None:
    return treatment.rsplit("-", 1)[1] if treatment.startswith("tcp") and "-" in treatment else None


def validate_preregistration(value: object) -> tuple[dict, tuple[str, ...], dict[str, dict], dict[str, dict]]:
    if not isinstance(value, dict) or value.get("schema") != 1:
        raise ValueError("unsupported preregistration schema")
    if value.get("study_kind") != "exploratory":
        raise ValueError("preregistration is not an exploratory study")
    treatments_raw = value.get("treatments")
    if (
        not isinstance(treatments_raw, list)
        or not treatments_raw
        or any(not isinstance(item, str) for item in treatments_raw)
        or len(set(treatments_raw)) != len(treatments_raw)
        or "udp" not in treatments_raw
    ):
        raise ValueError("invalid or duplicate treatments")
    treatments = tuple(treatments_raw)
    scenarios_raw = value.get("scenarios")
    if not isinstance(scenarios_raw, list) or not scenarios_raw:
        raise ValueError("scenarios must be a nonempty list")
    scenarios: dict[str, dict] = {}
    for scenario in scenarios_raw:
        if not isinstance(scenario, dict) or not isinstance(scenario.get("name"), str):
            raise ValueError("invalid scenario")
        name = scenario["name"]
        if name in scenarios:
            raise ValueError(f"duplicate scenario {name}")
        scenarios[name] = scenario
    blocks = int(value.get("blocks_per_scenario", 0))
    if blocks < 1:
        raise ValueError("blocks_per_scenario must be positive")
    plan_raw = value.get("plan")
    if not isinstance(plan_raw, list) or len(plan_raw) != blocks * len(scenarios):
        raise ValueError("plan cardinality does not match scenarios and blocks")
    plan: dict[str, dict] = {}
    indices: set[int] = set()
    scenario_blocks: dict[str, set[int]] = defaultdict(set)
    seed = int(value["randomization_seed"])
    for entry in plan_raw:
        if not isinstance(entry, dict):
            raise ValueError("invalid plan entry")
        block_id = entry.get("block_id")
        scenario = entry.get("scenario")
        index = int(entry.get("block_index", -1))
        scenario_block = int(entry.get("scenario_block", -1))
        order = entry.get("order")
        if (
            not isinstance(block_id, str)
            or block_id in plan
            or scenario not in scenarios
            or index in indices
            or not isinstance(order, list)
            or len(order) != len(treatments)
            or set(order) != set(treatments)
            or int(entry.get("netem_seed", -1)) != seed + index
        ):
            raise ValueError(f"invalid plan entry {block_id!r}")
        plan[block_id] = entry
        indices.add(index)
        scenario_blocks[scenario].add(scenario_block)
    if (
        indices != set(range(len(plan)))
        or set(scenario_blocks) != set(scenarios)
        or any(values != set(range(blocks)) for values in scenario_blocks.values())
    ):
        raise ValueError("plan indices or per-scenario block indices are incomplete")
    return value, treatments, scenarios, plan


def finite_positive(value: object) -> float:
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise ValueError("measurement is not finite and positive")
    return number


def percentile(values: list[float], probability: float) -> float:
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, math.ceil(probability * len(ordered)) - 1))
    return ordered[index]


def median_interval(values: list[float], replicates: int, seed: int) -> tuple[float, float]:
    rng = random.Random(seed)
    draws = [statistics.median(rng.choices(values, k=len(values))) for _ in range(replicates)]
    return percentile(draws, 0.025), percentile(draws, 0.975)


def main() -> None:
    if len(sys.argv) not in (2, 3):
        raise SystemExit("usage: evaluate-exploratory.py RESULT_DIR [OUTPUT.json]")
    result_dir = Path(sys.argv[1])
    output = Path(sys.argv[2]) if len(sys.argv) == 3 else result_dir / "exploratory-summary.json"
    try:
        preregistration, treatments, scenarios, plan = validate_preregistration(
            json.loads((result_dir / "preregistration.json").read_text())
        )
        expected_hosts = preregistration["hosts"]
        expected_host_pair = (
            sha256_digest(expected_hosts["sender"]["machine_id_sha256"], "sender machine ID"),
            sha256_digest(expected_hosts["receiver"]["machine_id_sha256"], "receiver machine ID"),
        )
        expected_artifacts = preregistration["endpoint_binary_sha256"]
        expected_artifact_pair = (
            sha256_digest(expected_artifacts["sender"], "sender artifact"),
            sha256_digest(expected_artifacts["receiver"], "receiver artifact"),
        )
    except (FileNotFoundError, json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
        raise SystemExit(f"invalid preregistration: {error}") from error
    required_treatments = set(treatments)
    analysis = preregistration["analysis_plan"]
    replicates = int(analysis["bootstrap_replicates"])
    seed = int(analysis["bootstrap_seed"])
    failures: list[str] = []
    excluded_blocks: dict[str, str] = {}
    records: dict[str, list[dict]] = defaultdict(list)

    for path in sorted(result_dir.glob("quarantine-*.json")):
        try:
            exclusion = json.loads(path.read_text())
            block_id = str(exclusion["block_id"])
            reason = str(exclusion["reason"])
        except (json.JSONDecodeError, KeyError, TypeError) as error:
            failures.append(f"{path.name}: invalid quarantine record ({error})")
            continue
        if block_id not in plan or block_id in excluded_blocks:
            failures.append(f"{path.name}: unknown or duplicate excluded block")
            continue
        excluded_blocks[block_id] = reason

    for path in sorted(result_dir.glob("result-*.json")):
        try:
            result = json.loads(path.read_text())
            block_id = str(result["design"]["block_id"])
            treatment = str(result.get("treatment", result["transport"]))
        except (json.JSONDecodeError, KeyError, TypeError) as error:
            failures.append(f"{path.name}: invalid result ({error})")
            continue
        if block_id not in plan:
            failures.append(f"{path.name}: block absent from preregistration")
            continue
        if treatment not in required_treatments:
            failures.append(f"{path.name}: unexpected treatment {treatment}")
            continue
        if result.get("verified") is not True or result.get("source_sha256") != result.get("output_sha256"):
            failures.append(f"{path.name}: transfer or digest was not verified")
            continue
        try:
            sha256_digest(result.get("source_sha256"), "source hash")
            artifacts = result["endpoint_binary_sha256"]
            artifact_pair = (
                sha256_digest(artifacts["sender"], "sender artifact"),
                sha256_digest(artifacts["receiver"], "receiver artifact"),
            )
            if artifact_pair != expected_artifact_pair:
                raise ValueError("endpoint artifacts differ from preregistration")
            if sha256_digest(result.get("binary_sha256"), "executed sender artifact") != artifact_pair[0]:
                raise ValueError("executed binary differs from staged sender artifact")
        except (KeyError, TypeError, ValueError) as error:
            failures.append(f"{path.name}: invalid artifact provenance ({error})")
            continue
        records[block_id].append(result)

    ratios: dict[str, dict[str, list[float]]] = {
        name: {treatment: [] for treatment in treatments if treatment != "udp"}
        for name in scenarios
    }
    raw: dict[str, dict[str, list[dict[str, float]]]] = {
        name: {treatment: [] for treatment in treatments} for name in scenarios
    }
    complete_blocks: dict[str, int] = defaultdict(int)
    host_pairs: set[tuple[str, str]] = set()
    artifact_pairs: set[tuple[str, str]] = set()

    for block_id, expected in plan.items():
        if block_id in excluded_blocks:
            if records.get(block_id):
                failures.append(f"{block_id}: both published and excluded")
            continue
        block = records.get(block_id, [])
        by_treatment = {item.get("treatment", item["transport"]): item for item in block}
        if len(by_treatment) != len(block) or set(by_treatment) != required_treatments:
            failures.append(f"{block_id}: incomplete or duplicate randomized block")
            continue
        observed_order = tuple(
            item.get("treatment", item["transport"])
            for item in sorted(block, key=lambda value: value["design"]["treatment_order"])
        )
        if observed_order != tuple(expected["order"]):
            failures.append(f"{block_id}: realized order differs from preregistration")
            continue
        scenario = str(expected["scenario"])
        if scenario not in scenarios or any(item.get("scenario") != scenarios[scenario] for item in block):
            failures.append(f"{block_id}: scenario provenance mismatch")
            continue
        design_failures = []
        for ordinal, treatment in enumerate(expected["order"]):
            design = by_treatment[treatment].get("design", {})
            expected_design = {
                "block_id": block_id,
                "block_index": expected["block_index"],
                "block_order": expected["block_index"],
                "scenario_block": expected["scenario_block"],
                "treatment_order": ordinal,
                "netem_seed": expected["netem_seed"],
                "randomization_seed": preregistration["randomization_seed"],
                "expected_treatments": list(treatments),
            }
            if any(design.get(key) != value for key, value in expected_design.items()):
                design_failures.append(treatment)
            item = by_treatment[treatment]
            expected_cc = treatment_cc(treatment)
            if (
                item.get("wire_transport") != treatment_transport(treatment)
                or item.get("tcp_congestion_control_requested") != expected_cc
                or item.get("tcp_congestion_control_actual") != expected_cc
                or item.get("netem_seed") != expected["netem_seed"]
            ):
                design_failures.append(treatment)
            controller = item.get("rate_controller")
            if treatment == "udp-auto":
                if not isinstance(controller, dict) or controller.get("mode") != "auto":
                    design_failures.append(treatment)
            elif treatment == "udp":
                if not isinstance(controller, dict) or controller.get("mode") != "fixed":
                    design_failures.append(treatment)
            elif controller is not None:
                design_failures.append(treatment)
        if design_failures:
            failures.append(f"{block_id}: treatment or design provenance mismatch")
            continue
        try:
            elapsed = {
                treatment: finite_positive(item["elapsed_ms"])
                for treatment, item in by_treatment.items()
            }
            goodput = {
                treatment: finite_positive(item["goodput_mbps"])
                for treatment, item in by_treatment.items()
            }
            for item in block:
                hosts = item["hosts"]
                host_pair = (
                    sha256_digest(hosts["sender"]["machine_id_sha256"], "sender machine ID"),
                    sha256_digest(hosts["receiver"]["machine_id_sha256"], "receiver machine ID"),
                )
                if host_pair != expected_host_pair:
                    raise ValueError("host pair differs from preregistration")
                host_pairs.add(host_pair)
                hashes = item["endpoint_binary_sha256"]
                artifact_pairs.add(
                    (sha256_digest(hashes["sender"], "sender artifact"), sha256_digest(hashes["receiver"], "receiver artifact"))
                )
                admission = item["admission_load_before_treatment"]
                if set(admission) != {expected_hosts["sender"]["ssh"], expected_hosts["receiver"]["ssh"]}:
                    raise ValueError("idle admission hosts differ from preregistration")
                minimum_samples = int(preregistration["idle_gate"]["consecutive_samples"])
                ceiling = float(preregistration["idle_gate"]["maximum_normalized_load_1m"])
                for samples in admission.values():
                    if not isinstance(samples, list) or len(samples) < minimum_samples:
                        raise ValueError("too few idle admission samples")
                    for sample in samples:
                        load = float(sample["normalized_load_1m"])
                        pressures = [float(sample[key]) for key in ("psi_cpu_some_avg10", "psi_io_some_avg10", "psi_memory_some_avg10")]
                        if not math.isfinite(load) or load < 0 or load > ceiling or any(not math.isfinite(value) or value < 0 for value in pressures):
                            raise ValueError("idle admission sample violates the preregistered gate")
        except (KeyError, TypeError, ValueError) as error:
            failures.append(f"{block_id}: invalid measurement or provenance ({error})")
            continue
        complete_blocks[scenario] += 1
        for treatment in treatments:
            raw[scenario][treatment].append(
                {"elapsed_ms": elapsed[treatment], "goodput_mbps": goodput[treatment]}
            )
            if treatment != "udp":
                ratios[scenario][treatment].append(elapsed["udp"] / elapsed[treatment])

    minimum_blocks = int(analysis["minimum_complete_blocks_per_scenario"])
    for scenario in scenarios:
        if complete_blocks[scenario] < minimum_blocks:
            failures.append(
                f"{scenario}: {complete_blocks[scenario]} complete blocks; require {minimum_blocks}"
            )
    if expected_host_pair[0] == expected_host_pair[1] or host_pairs != {expected_host_pair}:
        failures.append("results do not use one consistent pair of distinct machines")
    if artifact_pairs != {expected_artifact_pair}:
        failures.append("results do not use one consistent endpoint artifact pair")

    summaries: dict[str, dict] = {}
    for scenario in scenarios:
        treatment_summaries: dict[str, dict] = {}
        for treatment in treatments:
            values = raw[scenario][treatment]
            elapsed_values = [item["elapsed_ms"] for item in values]
            goodput_values = [item["goodput_mbps"] for item in values]
            treatment_summaries[treatment] = {
                "observations": len(values),
                "median_elapsed_ms": statistics.median(elapsed_values) if values else None,
                "elapsed_mad_ms": statistics.median(
                    [abs(value - statistics.median(elapsed_values)) for value in elapsed_values]
                ) if values else None,
                "median_goodput_mbps": statistics.median(goodput_values) if values else None,
            }
        paired: dict[str, dict] = {}
        for treatment, values in ratios[scenario].items():
            lower, upper = median_interval(values, replicates, seed) if values else (None, None)
            estimate = statistics.median(values) if values else None
            paired[treatment] = {
                "estimand": "median(udp_elapsed / baseline_elapsed)",
                "observations": len(values),
                "estimate": estimate,
                "bootstrap_95pct_interval": [lower, upper],
                "classification": (
                    "udp_faster" if upper is not None and upper < 1
                    else "udp_slower" if lower is not None and lower > 1
                    else "uncertain_or_tied"
                ),
            }
        summaries[scenario] = {
            "scenario": scenarios[scenario],
            "complete_blocks": complete_blocks[scenario],
            "treatments": treatment_summaries,
            "paired_udp_ratios": paired,
        }

    evidence = {
        "study_kind": "exploratory",
        "decision": "descriptive_only" if not failures else "invalid",
        "confirmatory_claim": None,
        "analysis_plan": analysis,
        "host_pairs": [list(pair) for pair in sorted(host_pairs)],
        "endpoint_binary_sha256": (
            {"sender": next(iter(artifact_pairs))[0], "receiver": next(iter(artifact_pairs))[1]}
            if len(artifact_pairs) == 1 else None
        ),
        "excluded_blocks": excluded_blocks,
        "scenarios": summaries,
        "failures": sorted(set(failures)),
    }
    output.write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    print(output)
    if failures:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
