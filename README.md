# tsunami-udp

[![CI](https://github.com/matthiasgoergens/packet-tide/actions/workflows/ci.yml/badge.svg)](https://github.com/matthiasgoergens/packet-tide/actions/workflows/ci.yml)

An experimental file-transfer tool that uses a small control path and paced UDP
for bulk data. The long-term direction includes fountain codes, but the first
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
- configurable token-bucket pacing;
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

Directory synchronization, content-defined chunk reuse, automatic rate control,
encryption, and fountain coding are later milestones. Version 0.1 authenticates
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
tsunami-udp --version
```

Tagged releases publish static Linux archives for x86-64 and ARM64, a clean
Cargo source package, and `SHA256SUMS`. After downloading an archive and the
checksum file from the [Packet Tide releases page](https://github.com/matthiasgoergens/packet-tide/releases):

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf tsunami-udp-VERSION-TARGET.tar.gz
sudo install -m 0755 tsunami-udp-VERSION-TARGET/tsunami-udp /usr/local/bin/
tsunami-udp --version
```

See [release/README.md](release/README.md) for the exact artifact names,
source-package installation, and maintainer checks.

Generate a 256-bit shared key once, then copy it to the other endpoint through a
secure channel. `keygen` creates a new mode-0600 file and refuses to overwrite an
existing path. Both sides reject keys that are not exactly 32 raw bytes or are
accessible by group/other users.

```sh
tsunami-udp keygen --out transfer.key

tsunami-udp receive \
  --listen 0.0.0.0:9000 --udp 0.0.0.0:9001 --out received.bin \
  --key-file transfer.key --idle-timeout-ms 30000

tsunami-udp send \
  --connect RECEIVER:9000 --udp-target RECEIVER:9001 \
  --file source.bin --transport udp --rate-mbps 100 \
  --key-file transfer.key --idle-timeout-ms 30000
```

Allow inbound TCP 9000 and UDP 9001 at the receiver. `tcp` and `tcp4` are also
available as benchmark transports. The fixed UDP rate is intentionally not a
congestion controller: start below the expected path capacity and increase it
carefully, because an excessive rate creates self-inflicted loss and may trigger
network policing.

Both endpoints default to a 30-second authenticated-control idle timeout. During
UDP transfer the sender emits `PING` heartbeats and the receiver answers `PONG`;
missing reports also prove receiver liveness. A local UDP failure sends a
best-effort authenticated `CANCEL`, while silence or connection loss aborts within
the configured timeout and leaves the receiver's resumable partial object intact.
Values from 500 milliseconds through one hour are accepted.

The v0.1 release uses TSU2. Current development uses the incompatible TSU3 wire
protocol, which adds bounded liveness, authenticated cancellation, heartbeats,
and an explicit completion acknowledgement. Both use a fresh mutual PSK
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
never shared across TSU1, TSU2, and TSU3.

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
