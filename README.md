# Packet Tide

[![CI](https://github.com/matthiasgoergens/packet-tide/actions/workflows/ci.yml/badge.svg)](https://github.com/matthiasgoergens/packet-tide/actions/workflows/ci.yml)

An authenticated whole-file transfer tool that uses a small control path and
paced UDP for bulk data. The long-term direction includes fountain codes, but the first
implementation deliberately uses a much simpler selective-repeat protocol.

The project is intended as an open-source, modern spiritual successor to the
original Tsunami UDP approach. It is currently an independent reimplementation,
not an official continuation or source fork of the historical project.

## First milestone: authenticated selective-repeat UDP

The sender divides one file into fixed-size chunks and sends each chunk as an
independently identified UDP datagram. It does not wait for an acknowledgement
after every datagram.

The receiver periodically reports which chunks are still missing. The sender
retransmits those chunks while continuing to send unsent data. Once the receiver
has every chunk and has verified the complete file, it sends a completion message
that is acknowledged reliably.

The initial implementation should have:

- one file per transfer;
- a session-bound authentication tag and chunk number in every datagram;
- payloads sized to avoid IP fragmentation (about 1,200 bytes by default);
- automatic receiver-feedback rate control with an exact fixed-rate override;
- periodic missing-range reports rather than per-packet acknowledgements;
- retransmission of missing chunks;
- bounded receiver memory and direct writes to a temporary file;
- a cryptographic hash of the completed file;
- retryable completion and cancellation messages;
- transfer statistics useful for comparing pacing rates.

The first performance comparison is likewise limited to one complete immutable
file. It intentionally excludes directory synchronization, delta reuse, multiple
files, compression, and restart/resume so that it measures the transports rather
than unrelated synchronization work.

Encryption and fountain coding are later milestones. Current development also
contains opt-in directory transfer and content-defined reuse; both retain a full
transfer fallback. Version 0.1 authenticates
peers and traffic but deliberately does not encrypt file contents or metadata.

Integration with rsync is also deferred. Once the standalone transport is correct
and benchmarked, we can evaluate whether to expose it as a separate transport for
rsync, maintain a fork, or propose changes upstream. The transport should first
prove useful without coupling its early design to rsync internals or compatibility.

See [DESIGN.md](DESIGN.md) for the proposed wire behavior and roadmap, and
[BENCHMARKS.md](BENCHMARKS.md) for the TCP comparison methodology.

## Install and run

The Rust 1.88+ implementation supports Linux. Install from a source checkout:

```sh
cargo install --locked --path .
packet-tide --version
```

The historical v0.1.0 release and its TSU2 wire protocol use the original
`tsunami-udp` executable and artifact names. Current development is Packet Tide
0.2.0-alpha.1 and uses the incompatible TSU4 protocol; mixed versions fail closed.

Tagged releases publish static Linux archives for x86-64 and ARM64, a clean
Cargo source package, and `SHA256SUMS`. After downloading an archive and the
checksum file from the [Packet Tide releases page](https://github.com/matthiasgoergens/packet-tide/releases):

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf packet-tide-VERSION-TARGET.tar.gz
sudo install -m 0755 packet-tide-VERSION-TARGET/packet-tide /usr/local/bin/
packet-tide --version
```

See [release/README.md](release/README.md) for the exact artifact names,
source-package installation, and maintainer checks.

Generate a 256-bit shared key once, then copy it to the other endpoint through a
secure channel. `keygen` creates a new mode-0600 file and refuses to overwrite an
existing path. Both sides reject keys that are not exactly 32 raw bytes or are
accessible by group/other users.

```sh
packet-tide keygen --out transfer.key

packet-tide receive \
  --listen 0.0.0.0:9000 --udp 0.0.0.0:9001 --out received.bin \
  --key-file transfer.key --idle-timeout-ms 30000

packet-tide send \
  --connect RECEIVER:9000 --udp-target RECEIVER:9001 \
  --file source.bin --transport udp \
  --udp-payload-bytes 1172 --feedback-interval-ms 50 \
  --key-file transfer.key --idle-timeout-ms 30000
```

Allow inbound TCP 9000 and UDP 9001 at the receiver. `tcp` and `tcp4` are also
available as benchmark transports. UDP defaults to automatic pacing between 10
Mbit/s and 10 Gbit/s; `--min-rate-mbps` and `--max-rate-mbps` bound that search.
The controller raises its pace when authenticated useful receive throughput follows
the offered load and backs off on stalls, duplicate work, or receiver socket drops.
Aggregate decision counts and up to 4,096 individual decisions are included in
the sender's JSON result, keeping telemetry bounded for very long transfers.

For a reproducible fixed-rate experiment, pass `--rate-mbps 100`. That option is
an override: it disables adaptation and cannot be combined with the automatic
rate bounds. Existing benchmark commands therefore retain their exact fixed-rate
semantics.

Both endpoints default to a 30-second authenticated-control idle timeout. During
UDP transfer the sender emits `PING` heartbeats and the receiver answers `PONG`;
missing reports also prove receiver liveness. A local UDP failure sends a
best-effort authenticated `CANCEL`, while silence or connection loss aborts within
the configured timeout and leaves the receiver's resumable partial object intact.
Values from 500 milliseconds through one hour are accepted.

### Directory trees

Directory transfer uses a separate authenticated manifest port and reuses one TCP
data listener plus one UDP socket for each regular file in canonical order:

```sh
packet-tide receive-dir \
  --listen 0.0.0.0:8999 --data-listen 0.0.0.0:9000 \
  --udp 0.0.0.0:9001 --out received-tree --key-file transfer.key

packet-tide send-dir \
  --connect RECEIVER:8999 --data-connect RECEIVER:9000 \
  --udp-target RECEIVER:9001 --root source-tree --key-file transfer.key
```

Only regular files and directories are accepted. Paths are transmitted as raw
Unix bytes in a canonical, SHA-256-bound manifest; absolute paths, traversal,
symlinks, and special files fail closed. Ordinary `rwx` permission bits and
nanosecond modification times are preserved. Ownership, ACLs, extended attributes,
hard-link identity, sparse extents, and special permission bits are not preserved.

The receiver never merges into an existing destination. It resumes only beneath
a sibling staging tree bound to the exact manifest hash, verifies every file,
applies metadata, and atomically renames the complete tree into place. Thus an
interruption cannot expose a partially populated destination tree. Single-file
`send` and `receive` remain available as the underlying primitive.

### Reusing content already at the receiver

For a receiver that already has an older or similar file, enable the bounded
content-defined inventory on the sender and name the local candidate on the
receiver:

```sh
packet-tide receive \
  --listen 0.0.0.0:9000 --udp 0.0.0.0:9001 --out received-new.bin \
  --reuse-from received-old.bin --key-file transfer.key

packet-tide send \
  --connect RECEIVER:9000 --udp-target RECEIVER:9001 \
  --file source-new.bin --transport udp --reuse-chunks true \
  --key-file transfer.key
```

The sender authenticates a content-defined chunk manifest. The receiver scans its
candidate without following symlinks, verifies each matching chunk with SHA-256,
copies it into the partial object, makes it durable, and reports only fully
covered UDP chunks as already present. Insertions and local edits can therefore
preserve reuse beyond the changed region.

Reuse defaults off. If either option is absent, the ordinary complete-file
transfer runs. A missing or unrelated candidate also sends all bytes. Regardless
of reuse, the receiver verifies the authenticated whole-file SHA-256 before
atomically installing the result. Sender JSON includes `reused_bytes` and
`reused_chunks`; `lab/test-reuse.sh` covers unchanged, edited, inserted,
truncated, unrelated-candidate, and fallback cases.

Directory transfer supports `--reuse-chunks true` on `send-dir` and
`--reuse-from OLD_TREE` on `receive-dir`. Candidates are resolved by their
manifest-relative raw path; missing files transfer in full, while symlinks and
unsafe parent components fail closed.

The sender offers the UDP file-data payload and receiver feedback interval in the
authenticated handshake. Defaults are 1,172 bytes and 50 ms; accepted bounds are
256–1,424 bytes and 10–10,000 ms. The receiver either accepts those exact values
or rejects the transfer—there is no silent clamping. The 1,424-byte maximum keeps
an IPv6 UDP datagram within a 1,500-byte path MTU; use a smaller value when the
path MTU is smaller. Both negotiated values appear in each endpoint's JSON summary.

Successful endpoints each emit one schema-versioned JSON summary. UDP sender
summaries include the receiver's final authenticated progress and datagram
counters; receiver summaries contain the matching local snapshot. On Linux the
summary also reports the socket's cumulative kernel drop count when available.
The benchmark harness reconciles these summaries before accepting a run.

The v0.1 release uses TSU2. Current development uses the incompatible TSU4 wire
protocol, which adds bounded liveness, authenticated cancellation, heartbeats,
an explicit completion acknowledgement, and exact transfer-parameter negotiation.
Both use a fresh mutual PSK
challenge-response handshake,
direction-specific sequenced HMAC-SHA256 control messages, authenticated TCP4
lane greetings, and a 128-bit truncated HMAC-SHA256 tag on every UDP datagram.
It rejects the earlier unauthenticated TSU1 wire format rather than downgrading.
The final SHA-256 verifies the complete object. This protects integrity,
authenticity, and replay boundaries; it does not provide confidentiality,
forward secrecy, resistance to traffic analysis, or availability against an
attacker that drops or floods traffic.

Interrupted UDP transfers
retain a stable `.part` file and durable `.part.map` receipt checkpoint beside the
destination. Repeating the same command resumes when size and hash match; a
different object safely starts over. Resume maps are protocol-versioned and are
never shared across TSU1, TSU2, TSU3, and TSU4. Payload size is part of the durable
map identity, so changing it starts with an empty receipt map even when size, hash,
and the resulting chunk count happen to match.

The receiver syncs data before atomically publishing each receipt checkpoint.
Consequently, a crash can cause a few chunks to be retransmitted but cannot make a
restart trust data that was not durable. Receipt bitmaps are capped at 64 MiB per
endpoint (about a 586 GiB file with the current payload), and sender repair queues
are capped at 65,536 chunks. `lab/test-resume.sh` kills both endpoints mid-transfer,
resumes with a new session, verifies the output, and checks a completion retry.

The Linux namespace harness and its safety boundaries are described in
[lab/README.md](lab/README.md). Exploratory measurements are retained in
[results/README.md](results/README.md).

## License

This project is open source under the [MIT License](LICENSE).
