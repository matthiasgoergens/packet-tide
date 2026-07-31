# Adaptive repair policy validation — 2026-07-31

The original adverse-network suite at `aae99cf` exposed a repair storm when
packets were reordered. Revision `f6bc9a7` adds an explicit original/repair packet
kind and activates a negotiated hole-reporting grace only after a unique original
arrives below the receive frontier. Both revisions ran the same two-block
randomized schedule, scenario parameters, and netem seeds on `spider`.

All 12 final transfers passed SHA-256 verification and byte comparison. Twelve
release-mode Rust tests also passed. Because changed packet sequences consume the
netem PRNG differently, this is a paired configuration/seed comparison rather than
an identical packet-loss realization.

| Condition | Repair change | Elapsed-time change | Offered IP-byte change |
| --- | ---: | ---: | ---: |
| 3% loss | +1.64% | +7.21% | +0.17% |
| 20 ms jitter + 0.3% loss | 0.00% | -0.22% | +0.08% |
| 5% duplication + 0.3% loss | 0.00% | +0.44% | +0.08% |
| 10% reordering + 0.3% loss | -99.66% | -34.72% | -46.04% |
| Combined loss, jitter, duplication, and reordering | -91.36% | -28.73% | -43.26% |
| 125 Mbit/s into 100 Mbit/s with a 128-packet queue | -38.47% | -34.26% | -14.59% |

The one-byte packet-kind addition accounts for most of the roughly 0.08% wire-byte
increase in the jitter and duplication controls. The pure-loss result retains
essentially the same repair volume but pays a 7.2% elapsed-time cost; this is the
remaining price of detecting reorder safely. The pathological result is removed:
the two reorder runs fall from 24,424 total repairs to 83, while the combined runs
fall from 25,578 to 2,210.

The oversubscribed sender still completes rather than collapsing or hanging. It
requires 4,980–5,756 repairs after 5,499–5,994 forward queue drops, so fixed pacing
above a shallow bottleneck remains predictably wasteful and is not presented as an
efficient operating point.

The 128 MiB crash/resume regression was rerun against the same final wire format.
It reused 20,368 durable chunks, reconstructed a byte-identical file, recorded
3,160 KiB receiver peak RSS, and returned completion with zero datagrams on a
subsequent reconnect. The resume-map magic was bumped so a partial file created
with the older 1,182-byte payload cannot be mistaken for the new 1,181-byte layout.

The machine-readable paired comparison is
`results/2026-07-31-stress-comparison.json`. Final raw stress and resume evidence is
under `results/raw/stress/f6bc9a7/` and `results/raw/resume/f6bc9a7/`.
