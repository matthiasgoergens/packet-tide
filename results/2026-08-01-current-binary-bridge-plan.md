# Predeclared current-binary bridge benchmark — 2026-08-01

## Purpose and frozen artifact

The original confirmatory campaign measures transport performance at source
revision `390332b`. Resume and adaptive repair were subsequently added at source
revision `f6bc9a7`. This bridge study directly tests whether the integrated current
implementation retains the original performance claim rather than extrapolating
between revisions.

The Linux release executable is frozen before observations at SHA-256
`6c24aec8284a907eac279ac12ea6aa3602058125eee82e7db6e360a48601dce5`.
Its relevant source hashes are:

- `src/main.rs`: `9634cb1d8b95cfd0ca9f2f74869811ac98a8a55570a784e028ff41aae06f4696`
- `src/resume.rs`: `c366041c19338b5a0e5b2a7faf94e89d68e9ce23e2fcfbe1238234952f4d5340`

The executable and source hashes were checked on `spider` before this plan was
committed. The artifact must be checked again immediately before the campaign.

## Design

The conditions, treatments, estimands, correctness requirements, quality gates,
and decision thresholds are exactly those in the predeclared 2026-07-31 plan:

- clean guardrail: 16 MiB, 100 Mbit/s, 20 ms RTT, 0% loss;
- primary advantage: 16 MiB, 100 Mbit/s, 100 ms RTT, 0.3% forward loss;
- repair guardrail: 16 MiB, 100 Mbit/s, 100 ms RTT, 1% forward loss;
- treatments: current UDP, CUBIC, BBR, and four-stream CUBIC;
- clean upper one-sided 95% bound for UDP/best-TCP must be at most 1.05;
- primary lower one-sided 95% bound for best-TCP/UDP must be at least 1.25;
- repair-overhead upper one-sided 95% bound must be at most 10%; and
- every accepted UDP output must pass SHA-256 and byte comparison.

There are 12 new independently seeded blocks per condition, using netem seeds
5101 through 5112. `lab/matrix-current-bridge.txt` is randomized with schedule
seed `73201`. The randomized complete-block harness keeps all four treatments
close together, balances treatment position with cyclic Latin squares, randomizes
block order, and enforces the same idle/pressure rules. At least 10 accepted,
complete blocks are required per condition. Interrupted or contaminated blocks
are retained and quarantined without replacement after outcomes are observed.

All 36 blocks form one campaign stratum. The final deterministic analysis uses
the existing 10,000-replicate block bootstrap with PRNG seed zero. This study is
an independent direct test of the current binary; its measurements are not pooled
with the `390332b` campaign.

## Interpretation

All three statistical rules and correctness must pass to establish that the
resumable/adaptive binary itself meets the scoped v0.1 performance goal. Failure
or insufficient accepted blocks is reported as failure or inconclusive rather
than repaired by incorporating results from the older binary.
