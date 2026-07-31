#!/usr/bin/env python3
"""Analyze the predeclared 2026-07-31 confirmatory benchmark matrix.

Usage:
    analyze-confirmatory.py OUTPUT.json SESSION_RESULT_DIR [SESSION_RESULT_DIR ...]

Each result directory is one session stratum.  Only blocks explicitly marked
``accepted`` in that directory's block-quality.jsonl are candidates.  A candidate
needs exactly udp, tcp-cubic, tcp-bbr, and tcp4-cubic results before it contributes
to an estimand.
"""

import json
import math
import random
import statistics
import sys
from collections import defaultdict
from pathlib import Path
from typing import Callable


REQUIRED_TREATMENTS = frozenset(("udp", "tcp-cubic", "tcp-bbr", "tcp4-cubic"))
MIN_BLOCKS = 10
BOOTSTRAP_REPLICATES = 10_000
BOOTSTRAP_SEED = 0

# (name, file bytes, rate Mbit/s, RTT ms, forward loss percent)
CONDITIONS = (
    ("clean_guardrail", 16 * 1024 * 1024, 100.0, 20.0, 0.0),
    ("primary_advantage", 16 * 1024 * 1024, 100.0, 100.0, 0.3),
    ("repair_guardrail", 16 * 1024 * 1024, 100.0, 100.0, 1.0),
)


def number(value: object) -> float:
    """Convert JSON numbers while rejecting missing or non-finite values."""
    result = float(value)
    if not math.isfinite(result):
        raise ValueError("value is not finite")
    return result


def condition_name(result: dict) -> str | None:
    scenario = result.get("scenario", {})
    try:
        file_bytes = int(scenario["file_bytes"])
        rate = number(scenario["rate_mbit"])
        rtt = number(scenario["rtt_ms"])
        loss = number(scenario["loss_percent"])
        reverse_loss = number(scenario.get("reverse_loss_percent", 0))
    except (KeyError, TypeError, ValueError):
        return None
    if reverse_loss != 0:
        return None
    for name, expected_bytes, expected_rate, expected_rtt, expected_loss in CONDITIONS:
        if (
            file_bytes == expected_bytes
            and math.isclose(rate, expected_rate, abs_tol=1e-9)
            and math.isclose(rtt, expected_rtt, abs_tol=1e-9)
            and math.isclose(loss, expected_loss, abs_tol=1e-9)
        ):
            return name
    return None


def quality_statuses(result_dir: Path) -> dict[str, str]:
    """Return final conservative quality status for each recorded block."""
    path = result_dir / "block-quality.jsonl"
    statuses: dict[str, str] = {}
    if not path.exists():
        return statuses
    for line_number, raw in enumerate(path.read_text().splitlines(), 1):
        if not raw.strip():
            continue
        try:
            record = json.loads(raw)
            block_id = record["block_id"]
            status = record["status"]
        except (json.JSONDecodeError, KeyError, TypeError) as error:
            raise ValueError(f"{path}:{line_number}: invalid quality record") from error
        if not isinstance(block_id, str) or not isinstance(status, str):
            raise ValueError(f"{path}:{line_number}: invalid quality record")
        # Re-evaluating a block must never turn a previously quarantined record into
        # an accepted one during analysis.
        if statuses.get(block_id) != "quarantined":
            statuses[block_id] = "accepted" if status == "accepted" else "quarantined"
    return statuses


def percentile(sorted_values: list[float], probability: float) -> float:
    """Nearest-rank quantile, with deterministic inclusive endpoints."""
    index = max(0, min(len(sorted_values) - 1, math.ceil(probability * len(sorted_values)) - 1))
    return sorted_values[index]


def stratified_bootstrap(
    values_by_session: dict[str, list[float]], estimator: Callable[[list[float]], float]
) -> tuple[float, float, float]:
    """Return estimate and one-sided 95% lower/upper bootstrap bounds."""
    values = [value for session in sorted(values_by_session) for value in values_by_session[session]]
    estimate = estimator(values)
    rng = random.Random(BOOTSTRAP_SEED)
    draws: list[float] = []
    for _ in range(BOOTSTRAP_REPLICATES):
        sample = [
            rng.choice(session_values)
            for session in sorted(values_by_session)
            for _ in range(len(values_by_session[session]))
            for session_values in (values_by_session[session],)
        ]
        draws.append(estimator(sample))
    draws.sort()
    return estimate, percentile(draws, 0.05), percentile(draws, 0.95)


def geometric_mean(values: list[float]) -> float:
    return math.exp(statistics.fmean(math.log(value) for value in values))


def decision(eligible_blocks: int, bound: float | None, threshold: float, direction: str) -> str:
    if eligible_blocks < MIN_BLOCKS:
        return "inconclusive_insufficient_accepted_blocks"
    if direction == "upper":
        return "pass" if bound is not None and bound <= threshold else "fail"
    return "pass" if bound is not None and bound >= threshold else "fail"


def main() -> None:
    if len(sys.argv) < 3:
        raise SystemExit(
            "usage: analyze-confirmatory.py OUTPUT.json SESSION_RESULT_DIR [SESSION_RESULT_DIR ...]"
        )

    output_path = Path(sys.argv[1])
    session_dirs = [Path(argument) for argument in sys.argv[2:]]
    blocks: dict[tuple[str, str], list[dict]] = defaultdict(list)
    statuses: dict[tuple[str, str], str] = {}
    for session_dir in session_dirs:
        if not session_dir.is_dir():
            raise SystemExit(f"not a result directory: {session_dir}")
        session = str(session_dir.resolve())
        for block_id, status in quality_statuses(session_dir).items():
            statuses[(session, block_id)] = status
        for path in sorted(session_dir.glob("result-*.json")):
            try:
                result = json.loads(path.read_text())
                block_id = result["design"]["block_id"]
                transport = result["transport"]
            except (json.JSONDecodeError, KeyError, TypeError) as error:
                raise ValueError(f"{path}: invalid result record") from error
            if not isinstance(block_id, str) or not isinstance(transport, str):
                raise ValueError(f"{path}: invalid result record")
            blocks[(session, block_id)].append(result)

    records: dict[str, list[dict]] = {name: [] for name, *_ in CONDITIONS}
    rejected: list[dict] = []
    udp_correctness_failures: list[dict] = []
    accepted_status_blocks = 0
    quarantined_blocks = sum(status == "quarantined" for status in statuses.values())

    for key, status in sorted(statuses.items()):
        session, block_id = key
        if status != "accepted":
            continue
        accepted_status_blocks += 1
        results = blocks.get(key, [])
        by_transport: dict[str, dict] = {}
        duplicate_transports: set[str] = set()
        for result in results:
            transport = result.get("transport")
            if transport in by_transport:
                duplicate_transports.add(str(transport))
            else:
                by_transport[transport] = result

        udp_result = by_transport.get("udp")
        if udp_result is not None and udp_result.get("verified") is not True:
            udp_correctness_failures.append({"session": session, "block_id": block_id})

        if set(by_transport) != REQUIRED_TREATMENTS or duplicate_transports:
            rejected.append(
                {
                    "session": session,
                    "block_id": block_id,
                    "reason": "incomplete_or_duplicate_treatments",
                    "observed_treatments": sorted(str(item) for item in by_transport),
                }
            )
            continue
        names = {condition_name(result) for result in by_transport.values()}
        if len(names) != 1 or None in names:
            rejected.append({"session": session, "block_id": block_id, "reason": "unexpected_or_mismatched_condition"})
            continue
        if any(result.get("verified") is not True for result in by_transport.values()):
            rejected.append({"session": session, "block_id": block_id, "reason": "verification_failure"})
            continue
        try:
            elapsed = {name: number(result["elapsed_ms"]) for name, result in by_transport.items()}
            if any(value <= 0 for value in elapsed.values()):
                raise ValueError("non-positive elapsed time")
            source_bytes = int(by_transport["udp"]["scenario"]["file_bytes"])
            offered = number(by_transport["udp"]["udp_ip_bytes_offered"])
            if source_bytes <= 0 or offered < 0:
                raise ValueError("invalid byte count")
        except (KeyError, TypeError, ValueError) as error:
            rejected.append({"session": session, "block_id": block_id, "reason": f"invalid_measurement: {error}"})
            continue
        best_tcp_time = min(elapsed[name] for name in REQUIRED_TREATMENTS if name != "udp")
        name = names.pop()
        records[name].append(
            {
                "session": session,
                "block_id": block_id,
                "udp_over_best_tcp": elapsed["udp"] / best_tcp_time,
                "best_tcp_over_udp": best_tcp_time / elapsed["udp"],
                "udp_overhead": offered / source_bytes - 1.0,
            }
        )

    output_conditions: dict[str, dict] = {}
    for name, *_ in CONDITIONS:
        condition_records = records[name]
        by_session: dict[str, list[dict]] = defaultdict(list)
        for record in condition_records:
            by_session[record["session"]].append(record)
        common = {
            "accepted_complete_blocks": len(condition_records),
            "sessions": {session: len(values) for session, values in sorted(by_session.items())},
            "minimum_blocks_required": MIN_BLOCKS,
        }
        if name == "clean_guardrail":
            values = {session: [r["udp_over_best_tcp"] for r in items] for session, items in by_session.items()}
            estimate, lower, upper = stratified_bootstrap(values, geometric_mean) if values else (None, None, None)
            output_conditions[name] = common | {
                "estimand": "geometric_mean(udp_time / best_tcp_time)",
                "estimate": estimate, "one_sided_95pct": {"lower": lower, "upper": upper},
                "threshold": 1.05, "decision": decision(len(condition_records), upper, 1.05, "upper"),
            }
        elif name == "primary_advantage":
            values = {session: [r["best_tcp_over_udp"] for r in items] for session, items in by_session.items()}
            estimate, lower, upper = stratified_bootstrap(values, geometric_mean) if values else (None, None, None)
            output_conditions[name] = common | {
                "estimand": "geometric_mean(best_tcp_time / udp_time)",
                "estimate": estimate, "one_sided_95pct": {"lower": lower, "upper": upper},
                "threshold": 1.25, "decision": decision(len(condition_records), lower, 1.25, "lower"),
            }
        else:
            values = {session: [r["udp_overhead"] for r in items] for session, items in by_session.items()}
            estimate, lower, upper = stratified_bootstrap(values, statistics.fmean) if values else (None, None, None)
            output_conditions[name] = common | {
                "estimand": "mean(udp_ip_bytes_offered / source_bytes - 1)",
                "estimate": estimate, "one_sided_95pct": {"lower": lower, "upper": upper},
                "threshold": 0.10, "decision": decision(len(condition_records), upper, 0.10, "upper"),
            }

    primary_decisions = [entry["decision"] for entry in output_conditions.values()]
    reliability = "fail" if udp_correctness_failures else "pass"
    goal = "pass" if reliability == "pass" and all(item == "pass" for item in primary_decisions) else "not_established"
    output = {
        "analysis": "2026-07-31-confirmatory-plan",
        "bootstrap": {"replicates": BOOTSTRAP_REPLICATES, "seed": BOOTSTRAP_SEED, "stratification": "session", "confidence": "one-sided 95%"},
        "quality": {"accepted_status_blocks": accepted_status_blocks, "quarantined_blocks": quarantined_blocks, "excluded_accepted_blocks": rejected},
        "correctness": {"udp_accepted_block_failures": udp_correctness_failures, "decision": reliability},
        "conditions": output_conditions,
        "v0_1_performance_goal": goal,
    }
    output_path.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
    print(output_path)


if __name__ == "__main__":
    main()
