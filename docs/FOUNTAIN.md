# Bounded fountain prototype

Status: codec foundation only; not yet selectable on the transfer wire.

## Codec choice

Use systematic RaptorQ as specified by [RFC 6330](https://www.rfc-editor.org/rfc/rfc6330.html),
via the pinned Apache-2.0 [`raptorq` 1.8.1 crate](https://docs.rs/raptorq/1.8.1/raptorq/).
Do not introduce a bespoke XOR or LT-code variant. RaptorQ supplies the original
source symbols first and can then generate repair symbols without a predetermined
upper count.

Version 1.8.1 is intentionally used instead of 2.x: the latter's x86-64 AVX-512
implementation does not compile on Packet Tide's Rust 1.88 minimum toolchain.

## Bounded Packet Tide profile

- One independently decodable source block per generation.
- Nominal generation size: 4 MiB; configurable later up to a hard 32 MiB cap.
- A final generation may be smaller than 4 MiB and must be nonempty.
- One 1,168-byte RaptorQ symbol plus its four-byte RFC payload ID fits the
  existing 1,172-byte Packet Tide UDP payload and 1,200-byte datagram budget.
- One source block and one sub-block, with eight-byte symbol alignment.
- Serialized object transmission information is 12 authenticated bytes.
- Every symbol packet remains covered by Packet Tide's session MAC.
- A decoded generation is written only at its authenticated file offset.
- The authenticated whole-file SHA-256 and reliable `COMPLETE` exchange remain
  the final success condition.

The 32 MiB cap bounds source data retained by either codec invocation. The exact
peak working-memory multiplier of the chosen implementation still needs measuring
on x86-64 and ARM64 before wire integration is considered complete. The receiver
must process generations independently and release a decoder after checkpointing
that generation; it must never construct a decoder for the entire file.

## Planned wire experiment

Fountain mode will remain an explicit alternative to selective repair. For each
generation the sender transmits systematic source symbols once, followed by repair
symbols. The receiver reports `DECODED generation` after successful recovery, or
periodically reports how many additional symbols it has accepted. The sender may
continue producing new repair ESIs until `DECODED`, subject to the same bounded
pacer and authenticated-control idle deadline as baseline UDP.

The experiment must compare:

- clean-path encoding overhead;
- independent random loss;
- burst loss;
- duplication and reordering;
- sender and receiver CPU;
- peak resident memory;
- offered IP bytes and elapsed time;
- selective-repair UDP under the identical randomized block.

No default changes until those results show a useful region. A decoding failure,
timeout, or unsupported profile must fail closed rather than silently claiming a
complete generation.
