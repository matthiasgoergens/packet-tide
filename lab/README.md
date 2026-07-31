# Linux network-emulation lab

The initial lab runs on `spider` inside three Linux network namespaces. It creates
only `tsu-bench-s`, `tsu-bench-r`, and `tsu-bench-d`, plus files below
`/tmp/tsunami-udp-lab`.

Run remote workloads at reduced CPU and I/O priority:

```sh
ssh spider 'cd /tmp/tsunami-udp-lab/project && \
  sudo nice -n 10 ionice -c2 -n7 ./lab/setup.sh'

ssh spider 'cd /tmp/tsunami-udp-lab/project && \
  sudo nice -n 10 ionice -c2 -n7 ./lab/run-one.sh udp 10485760 100 20 0.1 1'

ssh spider 'cd /tmp/tsunami-udp-lab/project && \
  sudo nice -n 10 ionice -c2 -n7 ./lab/run-one.sh rsync 10485760 100 20 0.1 1'

ssh spider 'cd /tmp/tsunami-udp-lab/project && \
  sudo nice -n 10 ionice -c2 -n7 ./lab/cleanup.sh'
```

`run-one.sh` transfers one sparse zero-filled file, verifies it byte-for-byte, and
writes a JSON result below `/tmp/tsunami-udp-lab/results`. The zero-filled input is
acceptable while compression is disabled; later datasets should include generated
incompressible content before compression experiments begin.

`run-matrix.sh` accepts rows of `FILE_BYTES RATE_MBIT RTT_MS LOSS_PERCENT SEED`,
runs UDP, the prototype's TCP baseline, and rsync 3.4+ for every row, then writes
`summary.json` and a paired `rbd-analysis.json`. Each scenario/seed row is one
randomized block: all three transports run back-to-back in a shuffled order. Block
order is also shuffled using the optional reproducible randomization seed, which is
independent of the netem seed. Randomized batches use all six transport permutations
once, balancing first/middle/last position over every six complete blocks. The
generated `design.tsv` records the realized assignment. Before each block, an idle
gate requires four acceptable samples over 30 seconds (`load/core <= 0.5`, CPU PSI
`some avg10 <= 5%`, and I/O PSI `some avg10 <= 1%`). Every treatment records UTC
timestamps, load average, memory/swap, cumulative CPU ticks, and Linux CPU, I/O, and
memory pressure before and after. A completed block that crosses a declared pressure
threshold is retained but marked in `block-quality.jsonl`; paired analysis excludes
it without silently rerunning it. `rbd-analysis.json` reports paired elapsed-time
ratios, geometric means, and a block-bootstrap 95% interval. The
rsync case uses daemon mode, `--whole-file`, `--no-compress`,
and a new destination. Its elapsed time includes rsync startup, transfer, receiver
flush, and byte-for-byte verification. The prototype's reported interval begins
after its control handshake and includes receiver flush and SHA-256 verification,
so compare rsync as an application baseline and the built-in TCP case as the
strict transport baseline.

Each result also captures the forward and reverse `tc -s -j` qdisc state. Forward
and reverse netem instances use adjacent rather than identical random seeds.

The current harness constrains GSO and GRO maximum sizes to the 1,500-byte path MTU
on all four ephemeral veth interfaces using `ip link`. This prevents netem from
dropping a large aggregate as one packet without installing anything on `spider`.
Packet captures should still confirm the intended packet granularity before the
loss results are treated as publishable.

When `tcpdump` is unavailable, `capture-packets.py` can run as root inside the
router namespace using Linux `AF_PACKET`. It reports observed IPv4-size counts and
fails the validation criterion if any IP packet exceeds the 1,500-byte path MTU.
With the namespaces already set up, `validate-packet-sizes.sh` performs this check
during a 16 MiB UDP transfer.
