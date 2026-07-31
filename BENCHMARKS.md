# TCP baseline and benchmark plan

## Question

Is the UDP object-transfer protocol faster than a well-implemented TCP file
transfer, and under which network and host conditions?

## Initial scope

The initial benchmark transfers exactly one immutable regular file from one sender
to one receiver. Timing ends only after the receiver has all bytes and verifies the
expected file hash.

The initial benchmark excludes:

- directory traversal and manifests;
- file metadata and permission preservation;
- rolling checksums, delta detection, and reuse of destination data;
- multiple files or concurrent transfer scheduling;
- compression;
- deduplication;
- restart/resume across a terminated session;
- fountain coding;
- encryption, except where a later comparison explicitly enables equal security on
  both transports.

Temporary-file creation, data writes, final hashing, and atomic placement remain in
scope because they are required to claim that a file was transferred completely.
Transport-only memory-backed runs may additionally isolate the network and protocol
cost, but they are reported separately and are not called completed file transfers.

This is an empirical question. Removing ordered-stream semantics does not by
itself guarantee higher goodput. TCP selective acknowledgements retain received
out-of-order data, and a complete file still requires every missing range. The
candidate advantages of this protocol are instead:

- starting immediately at a configured known-good rate;
- not treating every random packet loss as evidence of congestion;
- scheduling repairs without coupling them to an ordered application stream;
- expressing completion, verification, and resume at the object/block level;
- later substituting repair symbols for explicit retransmissions.

## Hypotheses

### Where TCP should win or tie

- Clean, low-latency LANs where one TCP flow already reaches line rate.
- CPU-limited transfers, especially before UDP segmentation/receive offload and
  batched system calls are implemented.
- Storage-limited receivers, because TCP flow control already supplies effective
  backpressure.
- Small files where setup, hashing, and final verification dominate transfer time.
- Congested shared links where an aggressive fixed UDP rate mostly creates its own
  loss.
- Tests against model-based TCP congestion control such as BBR on random-loss
  paths; this is expected to be a much stronger baseline than loss-based CUBIC.

### Where UDP selective repair may win

- High-bandwidth, high-RTT paths when the sender already knows a safe target rate.
- Random non-congestion loss, particularly with loss-based TCP congestion control.
- Moderate-size transfers on large-bandwidth-delay-product paths where TCP startup
  has not reached the available rate before much of the file is sent.
- Dedicated, provisioned, or rate-limited paths where fixed pacing is appropriate.
- Workloads that benefit from independently verified blocks, object-level resume,
  or future multi-source delivery.

### Uncertain cases

- Bursty loss: periodic missing reports may create a long repair tail.
- Very high packet rates: the winner may be determined by kernel/NIC offloads rather
  than protocol semantics.
- Wi-Fi and mobile paths: random loss favors decoupled repair, but rapidly changing
  capacity favors a good adaptive controller.
- Multiple simultaneous files: object-aware scheduling may help, but this is outside
  the first single-file milestone.

## Baselines

Use the same program, file-reading path, write path, buffer allocation strategy,
hash algorithm, and completion definition for both custom transports wherever
possible.

1. Single TCP connection using the platform default congestion controller.
2. Single TCP connection using CUBIC on Linux.
3. Single TCP connection using BBR where available, recording the exact version.
4. Four parallel TCP connections, with disjoint file ranges.
5. Paced selective-repeat UDP at a configured target rate.
6. Eventually, adaptive-rate UDP and fountain-coded UDP.

Run `iperf3` separately to characterize the path, but do not use its UDP sender as
the application benchmark: offered UDP rate is not verified file goodput.

Rsync should be measured in a later suite. A whole-file rsync run mixes transport,
process startup, remote scanning, checksums, and delta computation, so it cannot
answer the initial transport question cleanly.

## Primary metric

Measure application goodput:

```text
verified source bytes / time from accepted transfer to verified completion
```

Success requires matching the expected final hash. A sender's offered rate and a
receiver's raw datagram rate are not successful throughput.

Also record:

- wall-clock completion time;
- bytes and packets placed on the wire;
- original versus repair/retransmitted bytes;
- packet loss, duplication, and late/stale repairs;
- CPU time per endpoint and peak resident memory;
- receiver socket drops;
- disk read/write throughput;
- time to first data, end of initial pass, and end of repair tail;
- TCP retransmissions, congestion-window behavior, and congestion controller;
- UDP configured rate and actual pacing distribution.

## Controlled matrix

Every measured run transfers one complete file. Across runs, vary both the file and
the emulated network path.

Begin with generated incompressible files in memory-backed storage, followed by
NVMe-backed runs. Test one variable at a time before combining adverse conditions.

| Dimension | Initial values |
| --- | --- |
| Bottleneck rate | 10 Mbit/s, 100 Mbit/s, 1 Gbit/s, 10 Gbit/s |
| RTT | 0.2 ms, 5 ms, 20 ms, 100 ms, 250 ms |
| Independent packet loss | 0%, 0.001%, 0.01%, 0.1%, 1%, 3% |
| Burst loss | none, short and long Gilbert-Elliott bursts |
| Jitter | none, low, and high relative to base RTT |
| Reordering/duplication | none, then targeted stress cases |
| Queue | shallow, bandwidth-delay-product sized, deep |
| File size | 1 KiB, 1 MiB, 10 MiB, 100 MiB, 1 GiB, 10 GiB |
| Storage | memory-backed, local NVMe, deliberately throttled |

Absolute size is easy to understand, but size relative to the bandwidth-delay
product (BDP) is often more explanatory:

```text
BDP bytes = bottleneck bits/second * RTT seconds / 8
```

For selected paths, also test files near `0.1 * BDP`, `1 * BDP`, `10 * BDP`, and
`100 * BDP`. Files smaller than one BDP primarily exercise setup and startup. Very
large multiples exercise steady-state transfer and loss repair.

Do not initially execute the full Cartesian product. Use staged sweeps:

1. Clean-path sweep across file size, bottleneck rate, and RTT.
2. Independent-loss sweep on a few representative rate/RTT pairs.
3. Burst-loss, jitter, reordering, and duplication stress tests.
4. Queue-overflow tests with the UDP rate below, at, and above path capacity.
5. Repeat representative cases on real storage and physical networks.

The early results should be used to add more samples around crossover points where
TCP and UDP exchange the lead.

## Simulation topology

Use one Linux kernel with three network namespaces for the first reproducible lab:

```text
sender namespace                router namespace                receiver namespace
  sender0 <---- veth pair ----> left0     right0 <---- veth pair ----> receiver0
  transfer client               forwarding + tc                 transfer server
```

The router namespace is the only path between endpoints. Apply rate, delay, loss,
jitter, duplication, reordering, and queue limits to its two egress interfaces.
Configure the two directions independently so the data path and feedback path can
have different properties. Placing forward-path impairment on the router interface
toward the receiver also avoids the TCP Small Queues problem documented for netem
tests that shape only at the sender.

Linux network namespaces and `veth` devices provide the isolation; containers are
optional process/package wrappers, not the network simulator. The harness can run
programs with `ip netns exec` and configure links with `tc`. It requires root or the
specific network-administration capabilities needed to create namespaces, veth
devices, routes, and queue disciplines.

On a non-Linux development host, run this entire topology inside one lightweight
Linux VM. Three separate VMs add scheduling and virtual-switch noise without helping
the initial transport comparison. Add multi-VM, multi-kernel, or physical-host tests
later when validating portability and realistic NIC behavior.

### Queue disciplines

For simple functional tests, netem can directly apply rate, delay, and loss. For
more controlled bandwidth and queue experiments, compose:

- HTB or TBF for the bottleneck rate;
- netem for propagation delay and stochastic impairments;
- an explicit queue limit or later an AQM discipline for queue experiments.

Record the complete `tc` configuration and counters before and after every run.
Use a fixed netem seed whenever supported.

### Virtual offloads

Virtual links can carry aggregated GSO/GRO packets much larger than the path MTU.
If netem drops one aggregate, it may accidentally simulate a large packet-loss burst
rather than one network packet. For protocol-correctness and loss-model tests,
disable TSO, GSO, GRO, and related aggregation on the veth path, then verify packet
sizes with a capture.

This intentionally makes the namespace lab unsuitable for final CPU-efficiency
claims. Run separate host-performance tests with normal offloads enabled, followed
eventually by two physical machines and a third Linux router/bridge or hardware
impairment device.

### Harness inputs and outputs

Represent every scenario as machine-readable data containing:

- file size and deterministic content seed;
- forward and reverse rate, delay, jitter, loss model, and queue parameters;
- protocol and TCP congestion controller;
- UDP pacing rate;
- repetition number and network random seed.

The harness creates the topology, applies one scenario, runs the sender and receiver,
verifies the output hash, collects endpoint and `tc` counters, and writes one JSON
result. Cleanup must be safe after success, failure, or interruption.

Use fixed random seeds and at least five repetitions per cell. Randomize test order,
report medians and dispersion, and retain every raw result rather than only summary
charts.

## Fair comparison rules

- Both protocols use the same path MTU and file payload.
- Both include final hash verification in completion time.
- Neither includes connection establishment unless both do.
- Socket and system buffer tuning is disclosed for both.
- The UDP sender is capped at the emulated bottleneck rate, then separately tested
  above and below that rate.
- Tests distinguish random injected loss from queue-overflow congestion loss.
- TCP receive/send windows must be large enough for the path bandwidth-delay
  product; an accidentally restricted TCP window is not a UDP victory.
- Report results for default TCP, CUBIC, BBR, and parallel TCP separately.
- A test is invalid if either endpoint is unintentionally CPU- or disk-limited,
  unless that host bottleneck is the condition being studied.

## Decision criteria

The first protocol milestone succeeds if it:

1. matches single-stream TCP on a clean LAN within a reasonable CPU budget;
2. clearly beats CUBIC on at least some reproducible high-RTT/random-loss regimes;
3. identifies whether any advantage remains against BBR and parallel TCP;
4. degrades predictably rather than collapsing when paced slightly above capacity;
5. completes reliably under loss, duplication, reordering, and lost/stale reports.

If it cannot beat modern TCP in controlled transport-only tests, the object model
may still be valuable for resumability, synchronization, multicast, multi-source
transfer, or fountain coding. Those benefits must be claimed separately from raw
unicast speed.
