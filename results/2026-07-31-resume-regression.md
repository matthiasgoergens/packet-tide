# UDP resume and memory regression — 2026-07-31

Commit `10fa9b4` was built in release mode on `spider` and tested at reduced CPU
and I/O priority. Nine Rust unit tests passed, including durable receipt-map
round-tripping, metadata mismatch reset, torn-map rejection, and the hard repair
queue limit.

The end-to-end regression generated a deterministic 128 MiB file and started a
50 Mbit/s loopback UDP transfer. After five seconds it sent `SIGKILL` to both test
endpoints. The last atomic checkpoint contained 20,381 of 113,552 chunks. Fresh
processes then negotiated a new session ID, skipped exactly those durable chunks,
transferred the remainder, and produced a byte-identical output.

| Measurement | Result |
| --- | ---: |
| Durable chunks reused | 20,381 / 113,552 (17.95%) |
| Resumed UDP datagrams | 93,171 |
| Resumed IP-layer bytes offered | 114,413,252 |
| Estimated fresh IP-layer bytes | 139,441,120 |
| Avoided IP-layer bytes | 25,027,868 (17.95%) |
| Resume elapsed time | 4.683 s |
| Receiver peak RSS during resume | 2,912 KiB |
| Final SHA-256 and byte comparison | Pass |

After installation, the same sender command was run a third time. The receiver
recognized the existing size-and-hash-matching destination and returned completion
with zero UDP datagrams and zero offered UDP bytes. This exercises recovery from a
lost completion observation after the destination was atomically installed.

The implementation imposes an explicit 64 MiB receipt-bitmap ceiling at both
endpoints (about 591 GiB with the current 1,182-byte payload) and caps both sender
repair structures at 65,536 chunk entries. The 2.8 MiB observed RSS is evidence for
this 128 MiB case, while the code-enforced caps and unit tests establish the
absolute data-structure limits. More fault injection is still required around each
individual sync/rename boundary, and concurrent writers to one destination are not
yet supported.

Machine-readable output and `/usr/bin/time -v` telemetry are retained in
`results/raw/resume/10fa9b4/`. The tested binary SHA-256 was
`b15d39c6a705aad63b20bdc87be7985f34cbf1f8a0f0858f9848d67ac282d171`.
