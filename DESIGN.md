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
- receiver feedback interval;
- total chunk count;
- initial pacing rate.
- a missing-hole grace interval derived from the expected feedback round trip.

TSU4 adds exact authenticated payload/cadence negotiation to TSU3's
`PING`/`PONG`, `CANCEL`, and `COMPLETE_ACK` control messages. Both endpoints impose a configurable control-idle deadline (30 seconds
by default), so a peer that remains connected but silent cannot retain transfer
state indefinitely. The sender checks feedback failure throughout the initial
UDP pass as well as during repair-only operation.

The receiver validates the offered payload (256–1,424 bytes) and feedback cadence
(10–10,000 ms), then echoes the exact values in its authenticated `READY`. It
never silently clamps an experiment parameter. The receiver then creates a
temporary output file and allocates a chunk-receipt bitmap. If the destination is
already complete, its authenticated telemetry plus `COMPLETE` is the acceptance;
there is no redundant `READY` exchange.

For UDP resume, immutable size, SHA-256, payload size, and chunk count identify the
object while every connection gets a fresh random session ID and derived packet
key. Before sending new
data, the receiver streams nonzero words from its durable receipt bitmap over the
control connection. The sender skips those chunks; UDP packets from an old session
remain harmless because their authentication tag does not verify under the new
session key.

### UDP data packet

Each data packet contains at least:

- protocol version and packet type;
- session binding (implicit in the per-transfer packet key in v0.1);
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
final chunk may be shorter. The 1,172-byte default produces a 1,200-byte UDP
payload including Packet Tide's 28-byte header, or a 1,228-byte IPv4 packet. The
1,424-byte conservative maximum fits IPv6 within a 1,500-byte path MTU; paths with
smaller MTUs require a smaller configured payload.

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
64 MiB receipt bitmap: 536,870,912 chunks, or about 586 GiB with the current
1,172-byte payload. The sender's queued-repair set and cooldown cache are each
capped at 65,536 chunk entries. Repeated cumulative reports refill a capped queue,
so dropping excess advisory entries affects latency rather than correctness.

At a configurable interval, initially 50-100 ms, the receiver sends a status
report over the control connection. A report contains:

- highest chunk frontier observed;
- total distinct chunks received, including durable resume state;
- session counts for accepted, valid, duplicate, invalid, and repair datagrams;
- missing chunk ranges below the observed frontier;
- the Linux UDP socket-drop counter when `/proc` exposes it, otherwise an explicit
  unsupported marker.

TSU4 encodes these as canonical unsigned decimal fields in every authenticated
`M` report. The final exact snapshot is sent immediately before `COMPLETE` and is
therefore covered by the same ordered, direction-specific control authentication.
Invalid or unauthenticated UDP datagrams may increment only the diagnostic invalid
counter; they never advance object progress or authenticated peer liveness.

Ranges keep reports compact. If a report would become too large, it can be split
across messages or represented as bitmap windows. Reports are cumulative enough
that losing or superseding one never makes correctness depend on it.

The newest observed sequence is not immediately eligible for a missing report.
The receiver retains a small time history of the frontier. Reports initially use
the newest frontier at the negotiated reporting cadence. If a new unique original
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

The sender answers `COMPLETE` with `COMPLETE_ACK`. The receiver does not report a
clean session shutdown until that acknowledgement arrives; its wait is bounded by
the control-idle deadline. If the acknowledgement or connection is lost after
installation, reconnecting for the same immutable object returns `COMPLETE` again
and repeats the acknowledgement exchange without retransmitting data.

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

UDP uses bounded automatic pacing by default. The controller increases its rate
while authenticated useful receive throughput follows and reduces it when extra
sending produces stalled progress, duplicate work, or receiver-side socket drops.
An explicit `--rate-mbps` retains the fixed-rate baseline for experiments.

## Directory manifest and installation policy

The directory protocol begins with a canonical `PTM1` manifest. Paths are raw
Unix path bytes encoded as lowercase hexadecimal, so locale and Unicode
normalization cannot change their identity. Entries are strictly byte-sorted and
unique. Every non-root parent must appear earlier as a directory. Absolute paths,
empty components, `.` and `..`, NUL bytes, paths over 4,096 bytes, symlinks, and
non-regular filesystem objects fail closed. The manifest is bounded to one million
entries and carries a SHA-256 digest over its canonical header and entries.

The manifest records regular-file size and SHA-256 plus the nine ordinary permission
bits and nanosecond modification time for files and directories. Ownership, ACLs,
extended attributes, hard-link identity, sparse extents, devices, sockets, FIFOs,
and symlinks are not preserved in the first directory version. Set-user-ID,
set-group-ID, and sticky bits are deliberately stripped. The receiver must
not follow destination symlinks while validating or installing paths.

The intended receiver policy is replace-whole-tree, not merge. It validates the
complete authenticated manifest before creating anything, receives files beneath
a sibling staging directory named from the manifest hash, verifies every object,
applies file metadata, then directory metadata from deepest to shallowest, syncs,
and atomically renames the staging tree into place. An existing destination is a
conflict and fails unless a future explicit replacement policy is selected.
Interrupted transfers retain only the hash-bound staging tree and per-file receipt
maps; reconnecting with the same manifest resumes it, while a different manifest
cannot reuse it.

## Content-defined reuse

Content reuse is an optional UDP setup phase. When enabled, the sender binds the
chunk count and hash of a canonical `CDC1` manifest into the authenticated
handshake, then sends the manifest over the sequenced control channel. The
receiver caps it at one million entries and 64 MiB and checks canonical offsets,
lengths, hashes, total size, and the authenticated manifest digest.

Boundaries use a deterministic rolling gear hash with a 16 KiB minimum, 64 KiB
target, and 256 KiB maximum. Each content chunk carries SHA-256. The receiver
scans an explicitly selected local candidate without following symlinks, indexes
bounded `(length, hash)` identities, reads matching bytes again, and verifies
their digest before writing them at the new target offsets.

Only fixed-size UDP ranges completely covered by verified content chunks enter
the durable receipt bitmap. The receiver synchronizes the partial file and
checkpoints that bitmap before sending authenticated `REUSED`, `H`, and `GO`
messages. Existing resume receipts and newly reused ranges then share the same
bounded repair protocol.

An absent or unrelated candidate yields zero reusable ranges and the normal
whole-file UDP pass. Reuse defaults off unless both endpoints opt in. The final
authenticated whole-object SHA-256 remains authoritative; no local inventory or
receipt claim can install an object that fails that check.

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
- NAT traversal, path migration, sparse files, and full rsync semantics remain
  out of scope.

These are acceptable because the baseline provides the correctness and benchmark
reference needed to decide whether fountain coding is actually beneficial.

## Later milestones

1. Optionally add confidentiality and forward secrecy using an audited secure
   transport rather than extending the MAC-only construction ad hoc.
2. Replace selective retransmission, optionally, with systematic fountain coding:
   send original symbols first, then repair symbols.
3. Bound fountain-code memory and decoding cost by using independent generations,
   roughly 4-32 MiB each.
4. Let the receiver declare each generation decodable, while retaining a reliable
   final completion handshake and whole-file verification.
5. Evaluate rsync integration only after standalone transport benchmarks establish
   where the new protocol helps and where it does not.

Fountain codes address loss recovery; they do not remove the need for pacing.
They should be judged against this baseline under controlled latency and loss,
including clean LAN conditions where coding overhead may not pay for itself.
The selected bounded RaptorQ profile and its integration gates are recorded in
[`docs/FOUNTAIN.md`](docs/FOUNTAIN.md).
