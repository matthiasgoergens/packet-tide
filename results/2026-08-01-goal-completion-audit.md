# v0.1 single-file transfer goal — completion audit

This audit ties every required property to evidence from the same integrated
source revision, `f6bc9a7`. Later commits add only benchmark tooling, evidence,
documentation, and licensing; `git diff f6bc9a7 -- src/main.rs src/resume.rs` is
empty.

| Requirement | Authoritative evidence | Decision |
| --- | --- | --- |
| Verified single-file correctness | All 144 bridge transfers verified. All 12 final adverse-network transfers verified under loss, jitter, duplication, reordering, combined impairment, and oversubscription. | Pass |
| Clean-path parity | 12 accepted blocks; UDP/best-TCP estimate 1.0028, upper one-sided 95% bound 1.0048 against the 1.05 limit. | Pass |
| Modern-TCP speedup on a provisioned high-BDP lossy path | 12 accepted 100 ms/0.3% blocks; best-TCP/UDP estimate 1.4050, lower bound 1.3753 against the 1.25 requirement. Controls are CUBIC, BBR, and four-stream CUBIC. | Pass |
| Repair overhead | 11 accepted 100 ms/1% blocks; mean IP-layer overhead 5.028%, upper bound 5.066% against the 10% limit. | Pass |
| Bounded memory | Receipt bitmaps are capped at 64 MiB per endpoint, object size at 536,870,912 chunks, and repair/cooldown structures at 65,536 entries. The queue-limit unit test passes; a 128 MiB receive/resume run used 3,160 KiB peak RSS. | Pass |
| Resume | The 128 MiB crash regression killed both endpoints, reused all 20,368 durable chunks, verified the final file, and then recognized the completed object with zero UDP datagrams. | Pass |
| Adverse-network behavior | Two randomized blocks in each of six stress conditions all completed. Adaptive grace reduced reorder repairs by 99.66% and combined-impairment repairs by 91.36%; 125 Mbit/s pacing into a shallow 100 Mbit/s path completed predictably. | Pass |
| Packet granularity | Router captures observed zero IPv4 packets over the 1,500-byte MTU for integrated UDP, CUBIC, BBR, and four-stream CUBIC. | Pass |
| Reproducible benchmark | Plans and matrices were committed before observations; schedules, netem seeds, realized designs, quality records, host/qdisc telemetry, source and binary hashes, raw results, and deterministic analysis are retained. Workloads used randomized complete blocks and idle gates on `spider`. | Pass |
| Open source | The complete source and harness are present under the explicit MIT license. | Pass |

The performance claim is intentionally scoped: one verified 16 MiB object on the
declared 100 Mbit/s, 20/100 ms, independently lossy emulated paths. It does not
claim universal superiority on congested shared networks, every file size, every
loss process, physical WANs, or real storage. Within the stated provisioned
high-BDP/random-loss scope, the same integrated resumable binary satisfies every
v0.1 decision rule.

Primary evidence:

- `results/2026-08-01-current-binary-bridge.md`
- `results/2026-08-01-current-binary-bridge-analysis.json`
- `results/2026-07-31-adverse-network-repair-fix.md`
- `results/raw/resume/f6bc9a7/result.json`
- `results/2026-08-01-packet-size-validation.md`
