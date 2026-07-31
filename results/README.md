# Benchmark results

The final preregistered integrated-binary result is summarized in
[`2026-08-01-current-binary-bridge.md`](2026-08-01-current-binary-bridge.md), with
the requirement-by-requirement audit in
[`2026-08-01-goal-completion-audit.md`](2026-08-01-goal-completion-audit.md).
The integrated `f6bc9a7` binary passed clean-path parity, modern-TCP speedup,
repair-overhead, and correctness gates across 35 accepted randomized blocks.

Results in `raw/initial` are exploratory smoke-test measurements from `spider`, not
a publishable benchmark dataset. Every recorded transfer passed final SHA-256 and
byte-for-byte verification.

The dated `initial-20260716`, `refine-20260716`, and `crossover-20260716`
directories contain the 162 verified transfers from the first controlled matrix.
See [2026-07-16-baseline.md](2026-07-16-baseline.md) for the consolidated report.

The initial environment was:

- one Linux host with sender, router, and receiver network namespaces;
- 100 Mbit/s netem rate;
- one run per scenario rather than repeated randomized trials;
- CUBIC as the available non-Reno TCP controller;
- sparse zero-filled files on tmpfs;
- normal virtual-interface offloads because `ethtool` was not installed;
- all workloads run with `nice -n 10 ionice -c2 -n7`.

Selected observations:

| File | RTT | Loss | TCP | UDP | Notes |
| --- | ---: | ---: | ---: | ---: | --- |
| 1 MiB | 20 ms | 0% | 54.5 Mbit/s | 61.8 Mbit/s | Startup dominated |
| 10 MiB | 20 ms | 0% | 88.9 Mbit/s | 89.4 Mbit/s | Effectively tied |
| 10 MiB | 20 ms | 0.1% | 88.9 Mbit/s | 89.3 Mbit/s | UDP repaired 7 packets |
| 10 MiB | 100 ms | 1% | 2.39 Mbit/s | 67.8 Mbit/s | Preliminary stress case |

The 100 ms/1% UDP value uses seed 5 after adding an RTT-based repair cooldown. Its
forward netem path dropped 86 of 9,113 observed packets, while the sender issued 157
repairs. This remaining repair amplification is an optimization target.

The seed-4 UDP result predates that cooldown and is retained as a debugging artifact;
its 483 repairs should not be compared as the current implementation.

These results demonstrate correctness and justify a fuller experiment. They do not
yet establish a performance claim because there was only one repetition, TCP BBR
was not enabled, packet offloads were not controlled, and host/queue counters were
not captured into each JSON record.
