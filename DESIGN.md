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

### Transfer setup

The sender proposes:

- random session ID;
- file name or destination-relative path;
- file size;
- whole-file cryptographic hash;
- UDP payload size;
- total chunk count;
- initial pacing rate.

The receiver accepts the session, creates a temporary output file, allocates a
chunk-receipt bitmap, and tells the sender which UDP address and port to use.

### UDP data packet

Each data packet contains at least:

- protocol version and packet type;
- session ID;
- chunk number;
- payload length;
- payload bytes.

The byte offset is derived from `chunk_number * negotiated_payload_size`. The
final chunk may be shorter. A default datagram size near 1,200 bytes avoids IP
fragmentation across typical paths; "big" should mean close to the safe path MTU,
not a maximum-size 65,507-byte UDP payload.

The first version may rely on the UDP checksum plus final whole-file hash rather
than adding a checksum to every chunk. Per-packet authentication belongs with the
later security work.

### Receiver state

The receiver writes valid chunks directly into their offsets in a temporary file
and marks them in a bitmap. Duplicate chunks are ignored. It does not retain the
whole file in memory.

At a configurable interval, initially 50-100 ms, the receiver sends a status
report over the control connection. A report contains:

- highest chunk number observed;
- total distinct chunks received;
- missing chunk ranges below the observed frontier;
- optionally the receiver's measured data rate and local socket-drop counters.

Ranges keep reports compact. If a report would become too large, it can be split
across messages or represented as bitmap windows. Reports are cumulative enough
that losing or superseding one never makes correctness depend on it.

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
- Plain UDP data is neither encrypted nor authenticated.
- NAT traversal, path migration, directory trees, metadata preservation, sparse
  files, and rsync-style reuse are out of scope initially.

These are acceptable because the baseline provides the correctness and benchmark
reference needed to decide whether fountain coding is actually beneficial.

## Later milestones

1. Transfer directory manifests and preserve metadata.
2. Reuse existing destination data through content-addressed or content-defined
   chunks.
3. Add automatic rate adaptation while retaining a fixed-rate override.
4. Authenticate and encrypt control and data traffic.
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
