# Adverse-network correctness baseline — 2026-07-31

Commit `aae99cf` ran two randomized complete blocks over six adverse network
conditions on `spider`. Every one of the 12 deterministic 16 MiB transfers passed
SHA-256 verification and byte comparison. The suite used reduced CPU and I/O
priority, host idle gates, forward-only impairments, recorded netem seeds, and
automatic namespace cleanup.

| Condition | Elapsed time, ms | Repairs | Forward qdisc drops | Result |
| --- | ---: | ---: | ---: | --- |
| 3% loss | 1,816 / 1,929 | 826 / 825 | 475 / 474 | Pass |
| 20 ms jitter + 0.3% loss | 1,706 / 1,708 | 41 / 41 | 41 / 41 | Pass |
| 5% duplication + 0.3% loss | 1,635 / 1,635 | 36 / 36 | 38 / 38 | Pass |
| 10% reordering + 0.3% loss | 2,696 / 2,695 | 11,957 / 12,467 | 78 / 72 | Pass |
| Combined 3% loss, 20 ms jitter, 2% duplication, 10% reordering | 2,859 / 2,902 | 12,785 / 12,793 | 852 / 937 | Pass |
| Sender at 125 Mbit/s into 100 Mbit/s, 128-packet queue | 3,118 / 3,004 | 8,660 / 8,789 | 8,602 / 8,550 | Pass |

Correctness is established for these seeded observations, but the reordering rows
expose a repair-policy defect. The receiver reports every hole below the newest
sequence every 50 ms. Reordering lets a newer packet move the frontier before
ordinary delayed packets arrive, so the sender retransmits most of the file even
though netem dropped only 72–78 packets. The next experiment will rerun these exact
seeds after delaying hole eligibility by a negotiated time grace.

Raw JSON, realized order, qdisc/host telemetry, and provenance are retained in
`results/raw/stress/aae99cf/`.
