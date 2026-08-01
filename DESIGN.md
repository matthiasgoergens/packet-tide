# Protocol design notes

## Rationale: transfer objects, not byte streams

TCP presents an ordered, reliable byte stream with no inherent message or object
boundaries. That is a useful general abstraction, but it is stronger than a file
transfer requires.

A file transfer already has a finite object model:

- a known total length;
- independently addressable byte ranges;
- an explicit beginning and end;
- a final integrity condition;
- no requirement that bytes become available to the application in send order.

If chunk 50 arrives before chunk 49, the receiver can write chunk 50 immediately
and remember that chunk 49 is absent. There is no protocol-level reason to delay
all later data behind that gap. Success means that every required range is present
and the completed object verifies, not that the network delivered one continuous
in-order stream.

The baseline therefore implements reliability at the object level: packets carry
file offsets or chunk numbers, the receiver reports holes, and the sender repairs
those holes. Ordering is metadata rather than a delivery constraint.

This does not make TCP intrinsically unsuitable for file transfer, and modern TCP
stacks can buffer out-of-order segments efficiently. The design claim is narrower:
exposing the file's actual completion semantics gives the transfer protocol freedom
to avoid stream-level head-of-line blocking, choose its own pacing and repair
policy, and later substitute forward-error correction for explicit retransmission.

## Goals

The immediate goal is to establish a measurable UDP baseline before introducing
forward-error correction. The protocol should be simple enough to debug with a
packet capture and robust enough to transfer a large file despite packet loss,
duplication, and reordering.

The first version does not attempt to be fair to competing traffic. It still
paces packets because sending faster than the path or receiver can accept causes
self-inflicted loss and can reduce useful throughput.

## Project positioning

This is intended to be an open-source, modern spiritual successor to Tsunami UDP:
the same broad split between reliable control and high-rate UDP data, rebuilt
around an explicit finite-object model, current security expectations, measurable
pacing, resumability, and modern operating-system networking facilities.

Until the historical project's naming and license provenance have been reviewed,
the implementation should be described as independent rather than as an official
continuation or source fork. Reusing ideas and observable protocol patterns does
not require importing the old implementation.

Rsync integration is a possible later delivery mechanism, not part of the initial
transport design. After the baseline is correct and benchmarked, investigate three
options:

1. invoke the new transport beneath an otherwise separate synchronization layer;
2. maintain an rsync fork with a selectable bulk-data transport;
3. propose a narrowly scoped upstream rsync extension.

That investigation should account for rsync's process model, protocol negotiation,
rolling-checksum/delta pipeline, compatibility requirements, and maintainer appetite.
It should not constrain the first single-file protocol.

## Baseline architecture

Use two logical channels:

1. A reliable control connection negotiates the transfer, carries receiver
   reports, and confirms completion or cancellation.
2. UDP carries the file data. Data packets may be lost, duplicated, or reordered.

TCP is a reasonable control transport for the first version. Its traffic volume
is tiny compared with the UDP data stream, and it removes ambiguity around setup
and final completion.

### Transfer setup and authentication

Version 0.1 requires a shared 256-bit key file. The receiver first supplies a
fresh random challenge; both peers authenticate a transcript containing fresh
nonces and the complete transfer metadata. Direction-separated session keys are
derived from that transcript. Authentication completes before the receiver
creates directories, output files, or receipt maps. There is no insecure TSU1
compatibility mode.

The sender proposes:

- random session ID;
- file name or destination-relative path;
- file size;
- whole-file cryptographic hash;
- UDP payload size;
- total chunk count;
- initial pacing rate.
- a missing-hole grace interval derived from the expected feedback round trip.

The receiver accepts the session, creates a temporary output file, allocates a
chunk-receipt bitmap, and tells the sender which UDP address and port to use.

For UDP resume, immutable size, SHA-256, payload size, and chunk count identify the
object while every connection gets a fresh random session ID. Before sending new
data, the receiver streams nonzero words from its durable receipt bitmap over the
control connection. The sender skips those chunks; UDP packets from an old session
remain harmless because their session ID no longer matches.

### UDP data packet

Each data packet contains at least:

- protocol version and packet type;
- session ID;
- chunk number;
- packet kind (original or repair);
- payload length;
- payload bytes.

The fixed header and payload are covered by a 128-bit truncated HMAC-SHA256 tag
under a per-transfer UDP key. The receiver verifies the tag before writing data
or changing its receipt bitmap. TCP4 data-lane greetings and every control
message are also authenticated; control messages carry direction-local sequence
numbers so replay and reordering fail closed.

The byte offset is derived from `chunk_number * negotiated_payload_size`. The
final chunk may be shorter. A default datagram size near 1,200 bytes avoids IP
fragmentation across typical paths; "big" should mean close to the safe path MTU,
not a maximum-size 65,507-byte UDP payload.

The UDP checksum remains useful for accidental corruption, while the packet MAC
provides active-attack integrity. The final whole-file hash is authenticated as
part of the setup transcript.

### Receiver state

The receiver writes valid chunks directly into their offsets in a temporary file
and marks them in a bitmap. Duplicate chunks are ignored. It does not retain the
whole file in memory.

The prototype checkpoints this state beside the destination as a stable `.part`
file and an atomically replaced `.part.map`. Checkpoint ordering is data
`fdatasync`, then map write and `fsync`, then map rename. A crash before the rename
leaves the previous valid map and merely causes redundant retransmission. Exact
metadata and map length are validated on restart; a mismatched or torn map is not
trusted. Checkpoints occur at most once per second, while live missing reports can
remain more frequent.

Memory has explicit ceilings. Each endpoint rejects objects requiring more than a
64 MiB receipt bitmap: 536,870,912 chunks, or about 590.5 GiB with the current
1,181-byte payload. The sender's queued-repair set and cooldown cache are each
capped at 65,536 chunk entries. Repeated cumulative reports refill a capped queue,
so dropping excess advisory entries affects latency rather than correctness.

At a configurable interval, initially 50-100 ms, the receiver sends a status
report over the control connection. A report contains:

- highest chunk number observed;
- total distinct chunks received;
- missing chunk ranges below the observed frontier;
- optionally the receiver's measured data rate and local socket-drop counters.

Ranges keep reports compact. If a report would become too large, it can be split
across messages or represented as bitmap windows. Reports are cumulative enough
that losing or superseding one never makes correctness depend on it.

The newest observed sequence is not immediately eligible for a missing report.
The receiver retains a small time history of the frontier. Reports initially use
the newest frontier at the normal 50 ms reporting cadence. If a new unique original
packet arrives below the frontier, the receiver has observed path reordering and
activates the longer negotiated grace. An explicit packet-kind field ensures late
repair packets are not misclassified as path reordering. After the sender's `END`, the receiver
likewise waits out the active grace
before declaring tail holes. This prevents ordinary reordering from being mistaken
for loss without always charging genuine loss the full delay. The baseline uses
the sender's existing `2 * RTT + 50 ms` repair cooldown to deduplicate repairs and
half of that value (never below one report interval) as the maximum hole grace.

### Sender behavior

The sender uses a token-bucket or equivalent monotonic-clock pacer. A fixed
`--rate` is sufficient for the baseline. Sending in uncontrolled bursts is not.

During the initial pass, the sender prioritizes unsent chunks. It also maintains
a retransmission queue populated from receiver reports. Retransmissions should be
interleaved with new data so that an early missing chunk does not wait until the
entire first pass finishes.

Receiver reports can be slightly stale. Retransmitting the same chunk more than
once is harmless, although the sender should deduplicate its retransmission queue
to avoid wasting substantial bandwidth.

After the initial pass, the sender sends only reported missing chunks. If status
reports stop arriving, it pauses or substantially reduces data transmission and
uses the reliable control connection to determine whether the receiver still
exists.

### Completion

When every bitmap entry is set, the receiver syncs and hashes the temporary file.
If the hash matches, it atomically moves the file into place and sends `COMPLETE`
over the reliable control connection. The sender acknowledges completion, and
both sides may then release session state.

If verification fails, the receiver must not claim success. Initially it may
request a complete retry; finer corruption localization can be added later.

If the receiver has already atomically installed a destination with matching size
and SHA-256, a reconnect returns `COMPLETE` without sending UDP data. This covers a
receiver or network failure after installation but before the sender observed the
final completion message.

Completion must not be a single unreliable UDP message. A lost final message
must not leave one side transmitting forever or the other side unsure whether the
transfer succeeded.

## Pacing

The baseline exposes an explicit fixed rate, for example `--rate 800mbps`. Useful
experiments should also include an unlimited mode to demonstrate the effects of
sender, switch, and receiver queue overflow.

Automatic rate selection comes later. A deliberately aggressive controller can
increase its rate while observed receive throughput follows and reduce it when
additional sending produces only loss, delayed feedback, or receiver-side drops.
This is about avoiding self-inflicted waste even when fairness is not a goal.

## Known limitations of the baseline

- Missing reports can become expensive at high loss rates.
- Retransmissions require sender bookkeeping and at least one feedback round trip.
- A fixed chunk bitmap scales with file size, though one bit per roughly 1,200
  bytes is manageable for ordinary files.
- File contents and metadata are authenticated but not encrypted.
- The PSK protocol provides no forward secrecy and one key identifies a peer,
  not an individual user.
- Dropping or flooding traffic can still deny service; authentication does not
  make UDP available under attack.
- NAT traversal, path migration, directory trees, metadata preservation, sparse
  files, and rsync-style reuse are out of scope initially.

These are acceptable because the baseline provides the correctness and benchmark
reference needed to decide whether fountain coding is actually beneficial.

## Later milestones

1. Transfer directory manifests and preserve metadata.
2. Reuse existing destination data through content-addressed or content-defined
   chunks.
3. Add automatic rate adaptation while retaining a fixed-rate override.
4. Optionally add confidentiality and forward secrecy using an audited secure
   transport rather than extending the MAC-only construction ad hoc.
5. Replace selective retransmission, optionally, with systematic fountain coding:
   send original symbols first, then repair symbols.
6. Bound fountain-code memory and decoding cost by using independent generations,
   roughly 4-32 MiB each.
7. Let the receiver declare each generation decodable, while retaining a reliable
   final completion handshake and whole-file verification.
8. Evaluate rsync integration only after standalone transport benchmarks establish
   where the new protocol helps and where it does not.

Fountain codes address loss recovery; they do not remove the need for pacing.
They should be judged against this baseline under controlled latency and loss,
including clean LAN conditions where coding overhead may not pay for itself.
