# Two-host randomized benchmarks

`run-matrix.py` reproduces the v0.1 comparison on two independent SSH-reachable
Linux machines. It stages one exact static release binary and one key on both
hosts, verifies distinct `/etc/machine-id` values, randomizes `udp`, `tcp`, and
`tcp4` within paired blocks, waits for both machines to be reasonably idle, and
verifies every output by SHA-256.

No namespace is prepared manually. Each treatment gets an ephemeral Podman
sender container. Network impairment is attached only to that container's
private routed interface; the script never changes a physical host qdisc or NIC
offload setting. GSO/GRO aggregation on the private interface is capped at the
1,500-byte MTU so netem meters comparable wire-sized packets for TCP and UDP.
The receiver runs host-native by default, so a Raspberry Pi does not need Podman.
The sender container requires `CAP_NET_ADMIN`, while neither host needs a global
network change. The default matrix preregisters 12 blocks each of:

- clean: 16 MiB, 100 Mbit/s, 20 ms added RTT, 0% forward loss;
- lossy: 16 MiB, 100 Mbit/s, 100 ms added RTT, 1% forward loss.

The reverse path is not impaired. The physical path adds its own latency and
loss, which the result provenance must be interpreted alongside. Use dedicated
ports or adjust `--base-port` if 24000 onward is unavailable.

```sh
python3 lab/two-host/run-matrix.py \
  --sender SSH_ALIAS_1 --receiver SSH_ALIAS_2 \
  --receiver-proxy-jump SSH_JUMP_HOST \
  --receiver-address ADDRESS_REACHABLE_FROM_SENDER \
  --binary dist/tsunami-udp-v0.1.0-x86_64-unknown-linux-musl/tsunami-udp \
  --receiver-binary dist/tsunami-udp-v0.1.0-aarch64-unknown-linux-musl/tsunami-udp \
  --key-file /secure/path/benchmark.key \
  --results results/v0.1-two-host

python3 lab/two-host/evaluate-release.py results/v0.1-two-host
```

Use `--dry-run` to inspect the randomized matrix without contacting either host.
`--allow-same-host-smoke` exists only to validate orchestration and cannot support
the independent-machine performance claim.

By default the UDP offered rate equals the emulated bottleneck rate.
`--udp-rate-mbit RATE` separates those values when testing explicit pacing
headroom. The selected value is recorded in the preregistration and every result;
it must be chosen before a confirmatory run, not adjusted from its observations.

If an idle-gate timeout interrupts a run between complete blocks, rerun the same
command with `--resume` and, if desired, a longer `--idle-timeout`. Continuation
reuses the original schedule and run ID, validates host and artifact provenance,
skips complete blocks, retains incomplete observations in an explicit quarantine
record, and writes a numbered continuation record. Native receivers are cleaned up by the exact
run-scoped executable path, so an interrupted SSH controller cannot leave a port
collision for the next continuation.

The evaluator is the release gate, not just a plotting script. It fails unless
there are at least ten complete randomized blocks per condition, every transfer
and digest verifies, all records use one exact binary, and the machine-ID hashes
prove distinct endpoints. Its preregistered one-sided bootstrap gates are:

- clean UDP/best-TCP elapsed-time upper bound at most 1.05;
- lossy best-TCP/UDP elapsed-time lower bound at least 1.25.

Same-host smoke output is deliberately rejected even when its timing looks good.
When the hosts have different architectures, the preregistration records one
artifact hash per endpoint and requires that exact pair for every treatment.
`--sender-proxy-jump` and `--receiver-proxy-jump` use OpenSSH ProxyJump for
endpoints that are only routed from another machine.

## Exploratory crossover matrix

`exploratory-v1.json` is a deliberately fractional sweep across 64 KiB–64 MiB
files, 20–100 ms RTT, and 0–1% random loss. Six complete blocks per cell compare
fixed-rate and automatic UDP with one- and four-stream CUBIC and BBR. Scenario rows are interleaved across
time; within each scenario, cyclic randomized treatment orders put every treatment
in every ordinal position exactly once. All treatments in a block share one
preregistered netem seed.

Before every treatment, both hosts must pass three consecutive idle samples. Each
sample retains normalized load plus Linux CPU, I/O, and memory pressure. A failed
or interrupted block is quarantined and never silently rerun after observations
exist. At least four complete blocks per scenario are required for the descriptive
summary.

```sh
python3 lab/two-host/run-matrix.py \
  --sender spider --receiver matthias@192.168.66.82 \
  --receiver-proxy-jump spider --receiver-address 192.168.66.82 \
  --binary /path/to/packet-tide-x86_64 \
  --receiver-binary /path/to/packet-tide-aarch64 \
  --key-file /secure/path/benchmark.key \
  --scenario-file lab/two-host/exploratory-v1.json \
  --study-kind exploratory --blocks 6 \
  --treatments udp,udp-auto,tcp-cubic,tcp-bbr,tcp4-cubic,tcp4-bbr \
  --results results/raw/exploratory-tsu4

python3 lab/two-host/evaluate-exploratory.py results/raw/exploratory-tsu4
```

The exploratory evaluator validates the frozen schedule, complete blocks, endpoint
and artifact identities, realized order, and file hashes. It reports medians,
median absolute deviation, paired UDP/baseline ratios, and deterministic two-sided
95% block-bootstrap intervals. Its classifications are descriptive only and are
not a confirmatory performance claim.
