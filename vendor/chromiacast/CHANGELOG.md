# Changelog

All notable changes to Chromiacast are documented in this file.

## 0.3.1 - 2026-09-03

This release makes sender statistics notifications follow receiver feedback:

- externally visible feedback changes schedule a one-shot notification after
  a 100 millisecond debounce window;
- closely spaced acknowledgments are coalesced into one notification with the
  latest sender state; and
- unchanged state no longer produces periodic application wakeups alongside
  RTCP reports.

## 0.3.0 - 2026-08-31

This release makes outbound Cast stream configuration checked before it reaches
the wire:

- `Resolution::new()` and `Framerate::new()` now reject zero components and
  their fields are read-only;
- `OfferBuilder::build()` now returns `Result<Offer, OfferError>`;
- offers reject empty stream sets, zero rates and channel counts, missing video
  resolutions, and target delays that cannot be represented exactly; and
- CI now tests the minimal crate and each independently selectable feature in
  addition to the complete feature set.

The constructor and builder return-type changes require consumers of 0.2.x to
handle configuration errors when updating to 0.3.0.

## 0.2.0 - 2026-08-30

This release adds:

- paced, observable Cast sender sessions with RTCP feedback, retransmission,
  picture-loss handling, and adaptive playout-delay updates;
- mDNS discovery that preserves stable receiver identities and scoped routes;
- authenticated Cast and bounded local setup product-information queries;
- optional Android TV pairing, persistent client identities, remote state,
  and key control; and
- negotiated integer and rational receiver frame-rate handling.

The crate continues to leave capture, encoding, media-framework integration,
bitrate selection, and local authorization policy to applications.

## 0.1.0 - 2026-08-30

The first public release provides:

- authenticated Google Cast control connections and receiver lifecycle
  management;
- codec-agnostic Cast Streaming negotiation, encrypted RTP transport, RTCP
  feedback, and synchronized audio and video sessions; and
- optional mDNS receiver discovery.

Capture, encoding, media-framework integration, bitrate selection, and local
authorization policy remain application responsibilities.
