# Confirmatory benchmark — session 2 interim report

Session 2 ran the frozen `390332b` implementation and preregistered schedule seed
`73102` on `spider` from 2026-07-31 16:26:28 UTC through 16:49:08 UTC. Its 48
transfers completed in 22 minutes 39 seconds at reduced CPU and I/O priority. All
12 blocks passed the predeclared host-quality rules, and every output passed
SHA-256 verification and byte comparison.

Combined with session 1, the interim preregistered analysis is:

| Criterion | Accepted blocks | Two-session estimate | One-sided 95% bound | Threshold | Formal decision |
| --- | ---: | ---: | ---: | ---: | --- |
| Clean: UDP time / best TCP time | 8 | 1.0038 | upper 1.0064 | <= 1.05 | Inconclusive: insufficient blocks |
| 0.3% loss: best TCP time / UDP time | 8 | 1.3677 | lower 1.3377 | >= 1.25 | Inconclusive: insufficient blocks |
| 1% loss: UDP IP-layer overhead | 7 | 4.994% | upper 5.065% | <= 10% | Inconclusive: insufficient blocks |

The estimates remain on the favorable side of all three thresholds and are stable
relative to session 1. Formal decisions remain locked as inconclusive because the
plan requires at least ten accepted blocks per condition. Session 3 can raise the
counts to 12 clean, 12 primary, and 11 repair blocks if its blocks pass quality
checks.

Raw results, realized order, quality records, and provenance are retained in
`results/raw/confirmatory/session2-390332b/`. Machine-readable combined analysis is
`results/2026-08-01-confirmatory-session1-2-analysis.json`.
