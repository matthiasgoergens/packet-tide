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

`run-one.sh` transfers one deterministic incompressible file, verifies it by
SHA-256 and byte comparison, and writes a uniquely block-identified JSON result
below `/tmp/tsunami-udp-lab/results`.

`run-matrix.sh` accepts rows of `FILE_BYTES RATE_MBIT RTT_MS LOSS_PERCENT SEED`,
runs the configured CSV treatment list for every row, then writes `summary.json`
and a paired `rbd-analysis.json`. Each scenario/seed row is one randomized block:
all treatments run close together in a shuffled order. Block order is also shuffled
using the optional reproducible randomization seed, which is independent of the
netem seed. Cyclic Latin-square batches balance every treatment across ordinal
positions within each condition. The
generated `design.tsv` records the realized assignment. Before each block, an idle
gate requires four acceptable samples over 30 seconds (`load/core <= 0.5`, CPU PSI
`some avg10 <= 5%`, and I/O PSI `some avg10 <= 1%`), with a shorter gate before
each treatment. Every treatment records UTC
timestamps, load average, memory/swap, cumulative CPU ticks, and Linux CPU, I/O, and
memory pressure before and after. A completed block that crosses a declared pressure
threshold is retained but marked in `block-quality.jsonl`; paired analysis excludes
it without silently rerunning it. `rbd-analysis.json` reports paired elapsed-time
ratios, geometric means, and a block-bootstrap 95% interval. The
session directory also contains `provenance.json`, recording the kernel, BBR
module, tool versions, executable and source hashes, and ephemeral link state.
Set `TSU_SOURCE_COMMIT` when invoking `run-matrix.sh` to bind that manifest to the
committed source revision.

The confirmatory study uses schedule seeds `73101`, `73102`, and `73103` for its
three session matrices. After all sessions, run:

```sh
python3 lab/analyze-confirmatory.py results/confirmatory-analysis.json \
  SESSION1_RESULT_DIR SESSION2_RESULT_DIR SESSION3_RESULT_DIR
```

The rsync case uses a prestarted daemon, `--whole-file`, `--no-compress`, and a new
destination. Its elapsed time includes client invocation and protocol setup,
transfer, receiver flush, and byte-for-byte verification, but not daemon startup.
The prototype's reported interval begins
after its control handshake and includes receiver flush and SHA-256 verification,
so compare rsync as an application baseline and the built-in TCP case as the
strict transport baseline.

Each result also captures the forward and reverse `tc -s -j` qdisc state. Current
confirmatory scenarios inject random loss only on the forward data path; the reverse
feedback/ACK path retains the configured delay and rate without random loss.
Reusing a netem seed across treatments gives them the same stochastic
configuration, not an identical packet-loss realization, because their packet
sequences differ.

The current harness constrains GSO and GRO maximum sizes to the 1,500-byte path MTU
on all four ephemeral veth interfaces using `ip link`. This prevents netem from
dropping a large aggregate as one packet without installing anything on `spider`.
Packet captures should still confirm the intended packet granularity before the
loss results are treated as publishable.

When `tcpdump` is unavailable, `capture-packets.py` can run as root inside the
router namespace using Linux `AF_PACKET`. It reports observed IPv4-size counts and
fails the validation criterion if any IP packet exceeds the 1,500-byte path MTU.
With the namespaces already set up, `validate-packet-sizes.sh` performs this check
during 16 MiB UDP, CUBIC, BBR, and four-stream CUBIC transfers.

## Resume regression

`test-resume.sh` is a loopback correctness test independent of the confirmatory
performance matrix. It generates a deterministic 128 MiB file, kills the UDP
sender and receiver after five seconds, restarts both endpoints, verifies that the
sender skips exactly the durable checkpointed chunks, compares the completed file,
and reconnects once more to verify zero-datagram completion recovery. It records
receiver peak RSS with `/usr/bin/time -v`.

On `spider`, run the whole test at reduced priority:

```sh
nice -n 10 ionice -c2 -n7 bash lab/test-resume.sh
```
