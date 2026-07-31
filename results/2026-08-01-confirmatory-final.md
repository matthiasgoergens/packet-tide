# Confirmatory benchmark — final `390332b` result

The three-session study predeclared on 2026-07-31 establishes the scoped v0.1
performance claim for the frozen `390332b` implementation. Across the three
sessions, 34 randomized complete blocks passed the host-quality rules, two were
quarantined, and every accepted-block UDP output passed SHA-256 verification and
byte comparison.

| Criterion | Accepted blocks | Estimate | One-sided 95% bound | Threshold | Decision |
| --- | ---: | ---: | ---: | ---: | --- |
| Clean: UDP time / best TCP time | 11 | 1.0033 | upper 1.0055 | <= 1.05 | Pass |
| 0.3% loss: best TCP time / UDP time | 12 | 1.4088 | lower 1.3733 | >= 1.25 | Pass |
| 1% loss: UDP IP-layer overhead | 11 | 4.981% | upper 5.032% | <= 10% | Pass |

`best TCP` is the fastest completed result in each block among one CUBIC stream,
one BBR stream, and four CUBIC streams. Elapsed time includes verified receiver
completion. The final decision uses the predeclared 10,000-replicate,
session-stratified block bootstrap with PRNG seed zero.

## Session 3

Session 3 used schedule seed `73103` and the frozen Linux executable with SHA-256
`74eef3bbe3cb52a0ff838a091773889530378dfcc56e3c5f1d26e6d17f037a82`.
The continued measured interval ran from 2026-07-31 16:56:31 UTC through 17:19:18
UTC. Its 44 completed transfers all verified; 11 blocks were accepted.

The initial session-3 launch omitted the one-time namespace setup. It failed before
the opening block produced any treatment result. The harness retained that clean
block as quarantined (`campaign interrupted`, no treatments) rather than replacing
it. After namespace setup, `continuation-provenance.json` captured the same frozen
binary and the three live namespaces. `lab/continue-matrix.sh` then preserved the
realized design, skipped the recorded quarantine, and ran the remaining 11 blocks
without regenerating assignments. Both provenance captures and the quality record
are retained with the raw results.

## Scope

This result applies to the frozen pre-resume implementation and the exact emulated
conditions in the plan. The separately predeclared current-binary bridge directly
tests whether revision `f6bc9a7`, which integrates resume and adaptive repair,
retains these performance properties. No performance result is extrapolated from
one binary to the other.

Machine-readable final analysis is
`results/2026-08-01-confirmatory-final-analysis.json`; all session-3 result JSON,
design, quality, telemetry, and provenance files are in
`results/raw/confirmatory/session3-390332b/`.
