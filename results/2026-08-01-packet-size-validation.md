# Packet-size validation — integrated binary

Linux `AF_PACKET` captures on the router egress verified packet granularity for
the integrated `f6bc9a7` binary before its bridge campaign. Each treatment sent a
verified 16 MiB file over the 1,500-byte emulated path.

| Treatment | Captured IPv4 packets | Maximum IPv4 length | Oversized packets |
| --- | ---: | ---: | ---: |
| UDP | 14,332 | 1,228 bytes | 0 |
| TCP CUBIC | 17,412 | 1,500 bytes | 0 |
| TCP BBR | 17,412 | 1,500 bytes | 0 |
| Four-stream TCP CUBIC | 18,591 | 1,500 bytes | 0 |

The UDP capture contains 14,205 full data datagrams at 1,228 IP bytes. TCP data
segments appear at the path MTU rather than as GSO/GRO super-packets. This closes
the packet-aggregation ambiguity documented in `lab/README.md`: netem loss acts on
packets no larger than the declared path MTU for every compared transport.

Raw per-length histograms are retained in
`results/raw/packet-size/f6bc9a7/`.
