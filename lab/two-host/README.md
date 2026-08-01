# Two-host release benchmark

`run-matrix.py` reproduces the v0.1 comparison on two independent SSH-reachable
Linux machines. It stages one exact static release binary and one key on both
hosts, verifies distinct `/etc/machine-id` values, randomizes `udp`, `tcp`, and
`tcp4` within paired blocks, waits for both machines to be reasonably idle, and
verifies every output by SHA-256.

No namespace is prepared manually. Each treatment gets ephemeral Podman
containers. Network impairment is attached only to the sender container's
`eth0`; the script never changes a physical host qdisc or NIC offload setting.
The container requires `CAP_NET_ADMIN`, while the host does not need a global
network change. The default matrix preregisters 12 blocks each of:

- clean: 16 MiB, 100 Mbit/s, 20 ms added RTT, 0% forward loss;
- lossy: 16 MiB, 100 Mbit/s, 100 ms added RTT, 1% forward loss.

The reverse path is not impaired. The physical path adds its own latency and
loss, which the result provenance must be interpreted alongside. Use dedicated
ports or adjust `--base-port` if 24000 onward is unavailable.

```sh
python3 lab/two-host/run-matrix.py \
  --sender SSH_ALIAS_1 --receiver SSH_ALIAS_2 \
  --receiver-address ADDRESS_REACHABLE_FROM_SENDER \
  --binary dist/tsunami-udp-v0.1.0-x86_64-unknown-linux-musl/tsunami-udp \
  --key-file /secure/path/benchmark.key \
  --results results/v0.1-two-host

python3 lab/two-host/evaluate-release.py results/v0.1-two-host
```

Use `--dry-run` to inspect the randomized matrix without contacting either host.
`--allow-same-host-smoke` exists only to validate orchestration and cannot support
the independent-machine performance claim.

The evaluator is the release gate, not just a plotting script. It fails unless
there are at least ten complete randomized blocks per condition, every transfer
and digest verifies, all records use one exact binary, and the machine-ID hashes
prove distinct endpoints. Its preregistered one-sided bootstrap gates are:

- clean UDP/best-TCP elapsed-time upper bound at most 1.05;
- lossy best-TCP/UDP elapsed-time lower bound at least 1.25.

Same-host smoke output is deliberately rejected even when its timing looks good.
