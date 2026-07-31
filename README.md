# tsunami-udp

An experimental file-transfer tool that uses a small control path and paced UDP
for bulk data. The long-term direction includes fountain codes, but the first
implementation deliberately uses a much simpler selective-repeat protocol.

The project is intended as an open-source, modern spiritual successor to the
original Tsunami UDP approach. It is currently an independent reimplementation,
not an official continuation or source fork of the historical project.

## First milestone: plain UDP

The sender divides one file into fixed-size chunks and sends each chunk as an
independently identified UDP datagram. It does not wait for an acknowledgement
after every datagram.

The receiver periodically reports which chunks are still missing. The sender
retransmits those chunks while continuing to send unsent data. Once the receiver
has every chunk and has verified the complete file, it sends a completion message
that is acknowledged reliably.

The initial implementation should have:

- one file per transfer;
- a session ID and chunk number in every datagram;
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
encryption, and fountain coding are later milestones.

Integration with rsync is also deferred. Once the standalone transport is correct
and benchmarked, we can evaluate whether to expose it as a separate transport for
rsync, maintain a fork, or propose changes upstream. The transport should first
prove useful without coupling its early design to rsync internals or compatibility.

See [DESIGN.md](DESIGN.md) for the proposed wire behavior and roadmap, and
[BENCHMARKS.md](BENCHMARKS.md) for the TCP comparison methodology.

## Prototype

The current Rust prototype implements one-shot TCP and resumable selective-repeat
UDP file transfer with SHA-256 completion verification. Interrupted UDP transfers
retain a stable `.part` file and durable `.part.map` receipt checkpoint beside the
destination. Repeating the same command resumes when size and hash match; a
different object safely starts over. It is intentionally a benchmark vehicle
rather than a stable public protocol.

```sh
cargo build --release

target/release/tsunami-udp receive \
  --listen 0.0.0.0:9000 --udp 0.0.0.0:9001 --out received.bin

target/release/tsunami-udp send \
  --connect RECEIVER:9000 --udp-target RECEIVER:9001 \
  --file source.bin --transport udp --rate-mbps 100
```

The receiver syncs data before atomically publishing each receipt checkpoint.
Consequently, a crash can cause a few chunks to be retransmitted but cannot make a
restart trust data that was not durable. Receipt bitmaps are capped at 64 MiB per
endpoint (about a 590.5 GiB file with the current payload), and sender repair queues
are capped at 65,536 chunks. `lab/test-resume.sh` kills both endpoints mid-transfer,
resumes with a new session, verifies the output, and checks a completion retry.

The Linux namespace harness and its safety boundaries are described in
[lab/README.md](lab/README.md). Exploratory measurements are retained in
[results/README.md](results/README.md).

## License

This project is open source under the [MIT License](LICENSE).
