# Local patches to chromiacast 0.3.1

Vendored from crates.io (MIT) and wired in through `[patch.crates-io]` in the top-level `Cargo.toml`.
Upstream candidates; drop the vendor copy once they land.

1. **`OfferBuilder::android_rtp_hack(bool)`** (`src/offer.rs`): payload types 127 (audio) / 96 (video),
   as Chrome sends by default (openscreen `SenderSession::Configuration::use_android_rtp_hack = true`);
   some Android TV receivers need them. Test: `android_rtp_hack_tests`.
2. **Leading Sender Report** (`src/sender/event_loop.rs`, `StreamState::awaiting_receiver_report`):
   until the first Receiver Report gives an RTT, send an SR before each frame's RTP packets, as
   openscreen's `SenderImpl::EnqueueFrame` does. Without it the receiver drops every packet that
   arrives before its first SR ("arrived before the first Sender Report"), i.e. the first ~300 ms
   of video and a burst of retransmits. Test: `sender_report_leads_first_frame_until_receiver_report`.
3. **Test vectors** only: `crypto.rs` `known_answer` (AES-CTR checked against `openssl enc`) and
   `rtcp.rs` `openscreen_vectors` (the 19 packets in `testdata/openscreen_rtcp/`, copied from
   openscreen's `compound_rtcp_parser_fuzzer_seeds`, i.e. what its receiver-side RTCP builder emits).
