# Predeclared confirmatory benchmark plan — 2026-07-31

## Claim under test

For verified single-file delivery over a provisioned 100 Mbit/s path, paced UDP
selective repair is non-inferior to the best tested TCP baseline on a clean,
low-RTT path and at least 25% faster on a high-RTT path with independent random
loss, while adding no more than 10% bulk-path wire overhead at 1% loss.

## Treatments

- `udp`: selective repair paced at 100 Mbit/s including IPv4 and UDP headers.
- `tcp-cubic`: one TCP stream using CUBIC.
- `tcp-bbr`: one TCP stream using the kernel's BBR module.
- `tcp4-cubic`: four CUBIC streams carrying disjoint contiguous ranges.

Rsync remains a secondary application benchmark and is not part of the strict
transport decision.

## Conditions

All use a deterministic incompressible 16 MiB file, 100 Mbit/s bottleneck,
1,500-byte MTU, zero reverse-path random loss, tmpfs storage, and a 10,000-packet
netem queue.

| Condition | RTT | Forward loss | Purpose |
| --- | ---: | ---: | --- |
| Clean guardrail | 20 ms | 0% | UDP must remain within 5% of best TCP. |
| Primary advantage | 100 ms | 0.3% | UDP must beat best TCP by at least 25%. |
| Repair guardrail | 100 ms | 1% | UDP bulk-path overhead must remain at or below 10%. |

There are 12 blocks per condition, split into three sessions of four blocks.
Within each session and condition, a randomized cyclic Latin square places every
treatment once in every ordinal position. Session block order is randomized using
a schedule seed independent of netem seeds: `73101`, `73102`, and `73103` for
sessions one through three. A block is one condition/netem seed with all four
treatments run close together. Sharing a netem seed fixes the stochastic
configuration but does not produce an identical loss realization, because each
transport emits a different packet sequence.

## Quality rules

- The host must pass the declared load and PSI idle gate before a block and a
  shorter gate before every treatment.
- Host and qdisc telemetry are retained before/after each treatment.
- Interrupted, incomplete, verification-failing, or pressure-contaminated blocks
  are retained and quarantined, never silently replaced after outcomes are seen.
- At least 10 accepted blocks are required per condition; otherwise it is
  inconclusive.
- All UDP outputs in accepted blocks must pass SHA-256 and byte comparison. Any
  accepted-block UDP correctness failure fails the reliability criterion.

## Estimands and decisions

For each block, `best_tcp_time` is the minimum elapsed time of `tcp-cubic`,
`tcp-bbr`, and `tcp4-cubic`. Elapsed time begins after the shared control READY and
ends after receiver flush, SHA-256 verification, atomic installation, and the
sender's receipt of COMPLETE.

Paired block ratios are bootstrapped with session stratification:

1. Clean non-inferiority passes only if the upper one-sided 95% bound for
   `udp_time / best_tcp_time` is at most 1.05.
2. The performance claim passes only if the lower one-sided 95% bound for
   `best_tcp_time / udp_time` is at least 1.25 in the 100 ms/0.3% condition.
3. Repair efficiency passes only if the upper one-sided 95% bound for
   `udp_ip_bytes_offered / source_bytes - 1` is at most 0.10 in the 1% condition.

Timing-ratio point estimates and bootstrap replicates use the geometric mean;
overhead uses the arithmetic mean. The deterministic analysis uses 10,000
bootstrap replicates with PRNG seed zero.

`udp_ip_bytes_offered` includes every original and repair UDP datagram plus IPv4
and UDP headers, including datagrams later dropped by netem. TCP control traffic
is reported separately via qdisc counters and is excluded from this bulk-path
overhead estimand.

Failure of any decision rule means the stated v0.1 performance goal is not yet
established. The matrix and rules are committed before the first confirmatory
session is run.
