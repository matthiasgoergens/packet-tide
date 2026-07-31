# Current integrated-binary bridge — final result

The preregistered bridge study establishes that the integrated resumable/adaptive
`f6bc9a7` binary retains the scoped v0.1 performance claim. The exact Linux
executable SHA-256 was
`6c24aec8284a907eac279ac12ea6aa3602058125eee82e7db6e360a48601dce5`;
its `src/main.rs` and `src/resume.rs` hashes match the predeclared plan, and those
source files remain unchanged at the repository head.

| Criterion | Accepted blocks | Estimate | One-sided 95% bound | Threshold | Decision |
| --- | ---: | ---: | ---: | ---: | --- |
| Clean: UDP time / best TCP time | 12 | 1.0028 | upper 1.0048 | <= 1.05 | Pass |
| 0.3% loss: best TCP time / UDP time | 12 | 1.4050 | lower 1.3753 | >= 1.25 | Pass |
| 1% loss: UDP IP-layer overhead | 11 | 5.028% | upper 5.066% | <= 10% | Pass |

All 144 transfers completed and passed SHA-256 and byte comparison. Thirty-five
randomized complete blocks passed the quality rules. One 1%-loss block was retained
but quarantined because host I/O PSI was 1.80% after its CUBIC treatment, above the
predeclared 1% limit. No block was replaced.

The campaign used schedule seed `73201`, netem seeds 5101 through 5112, a 16 MiB
deterministic incompressible file, a 100 Mbit/s path, and the same CUBIC, BBR, and
four-stream CUBIC controls as the original campaign. It ran at reduced CPU and I/O
priority from 2026-07-31 17:23:07 UTC through 18:35:08 UTC. The deterministic
analysis uses the predeclared 10,000 block-bootstrap replicates and PRNG seed zero.

Across all observations, median integrated UDP completion was 1.472 s clean,
1.634 s at 0.3% loss, and 1.645 s at 1% loss. These descriptive medians do not
replace the paired decision estimands above.

Machine-readable analysis is
`results/2026-08-01-current-binary-bridge-analysis.json`. The realized order,
quality decisions, provenance, telemetry, and all 144 raw result records are in
`results/raw/confirmatory/current-bridge-f6bc9a7/`.
