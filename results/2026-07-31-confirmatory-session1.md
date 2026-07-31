# Confirmatory benchmark — session 1 interim report

Session 1 ran the committed `390332b` protocol and study design on `spider` from
2026-07-31 15:11:30 UTC through 15:34:42 UTC. The 48 transfers completed in 23
minutes 12 seconds at reduced CPU and I/O priority. All output files passed the
protocol's SHA-256 check and the harness's byte comparison.

This is an interim look, not the predeclared decision. Each condition has only
three or four accepted blocks; the plan requires at least ten.

| Criterion | Accepted blocks | Session-1 estimate | One-sided 95% bound | Threshold | Formal decision |
| --- | ---: | ---: | ---: | ---: | --- |
| Clean: UDP time / best TCP time | 4 | 1.0072 | upper 1.0099 | <= 1.05 | Inconclusive: insufficient blocks |
| 0.3% loss: best TCP time / UDP time | 4 | 1.3715 | lower 1.3148 | >= 1.25 | Inconclusive: insufficient blocks |
| 1% loss: UDP IP-layer overhead | 3 | 5.046% | upper 5.126% | <= 10% | Inconclusive: insufficient blocks |

The interim estimates are on the favorable side of all three thresholds. BBR
was the relevant TCP baseline on both lossy conditions. At 0.3% loss its paired
geometric-mean time was 1.372 times UDP's; at 1% it was 1.488 times UDP's over the
three accepted blocks. On the clean path, UDP was about 0.72% slower than the
fastest TCP result per block.

Eleven blocks were accepted. The fourth 1% block was retained but quarantined
because host I/O PSI was 2.97% after its single-stream CUBIC treatment, exceeding
the predeclared 1% threshold. It was not rerun or used in the interim estimands.

The session provenance records Linux 7.1.5-arch1-2.1, iproute2 7.1.0, rsync
3.4.4, the loaded in-tree BBR module, source commit `390332b`, and release binary
SHA-256 `74eef3bbe3cb52a0ff838a091773889530378dfcc56e3c5f1d26e6d17f037a82`.
The root provenance capture could not see the user's Rust toolchain through
sudo's secure path; a post-run read-only check recorded rustc and Cargo 1.91.0.
The harness removed all three temporary network namespaces at completion.

Raw results, realized treatment order, quality decisions, and machine provenance
are in `results/raw/confirmatory/session1-390332b/`. The machine-readable interim
decision output is `results/2026-07-31-confirmatory-session1-analysis.json`.
