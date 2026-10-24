use bytes::{BufMut, Bytes, BytesMut};
use rand::Rng;
use std::time::Duration;

#[cfg(test)]
use crate::constants::MAX_RTP_PACKET_SIZE_IPV4;
use crate::constants::{
    ADAPTIVE_LATENCY_EXT_SIZE, RTP_BASE_HEADER_SIZE, RTP_MAX_HEADER_SIZE, RTP_REQUIRED_FIRST_BYTE,
};
use crate::frame::FrameId;
use crate::sync_source::SyncSource;

#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub data: Bytes,
}

/// Mutable state for RTP packetization of a single stream.
pub struct RtpPacketizer {
    sync_source: SyncSource,
    payload_type: u8,
    sequence_number: u16,
    max_payload_size: usize,
}

impl RtpPacketizer {
    pub fn new(sync_source: SyncSource, payload_type: u8, max_packet_size: usize) -> Self {
        let sequence_number = rand::thread_rng().gen();
        Self {
            sync_source,
            payload_type,
            sequence_number,
            max_payload_size: max_packet_size.saturating_sub(RTP_MAX_HEADER_SIZE),
        }
    }

    /// Split an encrypted frame into RTP packets.
    pub fn packetize(
        &mut self,
        frame_id: FrameId,
        is_key_frame: bool,
        referenced_frame_id: FrameId,
        rtp_timestamp: u32,
        new_playout_delay: Option<Duration>,
        payload: &[u8],
    ) -> Vec<RtpPacket> {
        let num_packets = if payload.is_empty() {
            1
        } else {
            payload.len().div_ceil(self.max_payload_size)
        };
        let max_packet_id = (num_packets - 1) as u16;

        let mut packets = Vec::with_capacity(num_packets);

        for packet_id in 0..num_packets {
            let offset = packet_id * self.max_payload_size;
            let end = (offset + self.max_payload_size).min(payload.len());
            let chunk = &payload[offset..end];
            let is_last = packet_id == num_packets - 1;

            let include_ext = packet_id == 0 && new_playout_delay.is_some();
            let ext_count: u8 = if include_ext { 1 } else { 0 };
            let header_size = RTP_BASE_HEADER_SIZE
                + if include_ext {
                    ADAPTIVE_LATENCY_EXT_SIZE
                } else {
                    0
                };

            let mut buf = BytesMut::with_capacity(header_size + chunk.len());

            // Standard RTP header (12 bytes)
            buf.put_u8(RTP_REQUIRED_FIRST_BYTE);
            let marker_and_payload_type =
                if is_last { 0x80 } else { 0x00 } | (self.payload_type & 0x7F);
            buf.put_u8(marker_and_payload_type);
            buf.put_u16(self.sequence_number);
            buf.put_u32(rtp_timestamp);
            buf.put_u32(self.sync_source);

            // Cast header
            let mut cast_byte = ext_count & 0x3F;
            if is_key_frame {
                cast_byte |= 0x80;
            }
            cast_byte |= 0x40; // R bit: always include reference frame ID
            buf.put_u8(cast_byte);
            buf.put_u8(frame_id.truncate());
            buf.put_u16(packet_id as u16);
            buf.put_u16(max_packet_id);
            buf.put_u8(referenced_frame_id.truncate());

            // Adaptive latency extension
            if include_ext {
                let delay_ms = new_playout_delay.unwrap().as_millis() as u16;
                // Extension header: type (upper 6 bits) | size (lower 10 bits)
                // Type = 1, size = 2
                let ext_header: u16 = (1 << 10) | 2;
                buf.put_u16(ext_header);
                buf.put_u16(delay_ms);
            }

            // Payload
            buf.put_slice(chunk);

            self.sequence_number = self.sequence_number.wrapping_add(1);
            packets.push(RtpPacket { data: buf.freeze() });
        }

        packets
    }
}

/// Identify whether a raw UDP packet is RTP or RTCP and extract its sync source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    Rtp(SyncSource),
    Rtcp(SyncSource),
    Unknown,
}

pub fn classify_packet(data: &[u8]) -> PacketType {
    if data.len() < 8 {
        return PacketType::Unknown;
    }

    let first_byte = data[0];
    let second_byte = data[1];

    if first_byte == RTP_REQUIRED_FIRST_BYTE {
        let pt = second_byte & 0x7F;
        if (96..=127).contains(&pt) && data.len() >= 12 {
            let sync_source = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
            return PacketType::Rtp(sync_source);
        }
    }

    // RTCP: version 2, packet type 200-207
    if (first_byte & 0xC0) == 0x80 {
        let packet_type = second_byte;
        if (200..=207).contains(&packet_type) && data.len() >= 8 {
            let sync_source = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
            return PacketType::Rtcp(sync_source);
        }
    }

    PacketType::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_packetizer() -> RtpPacketizer {
        RtpPacketizer::new(12345, 101, MAX_RTP_PACKET_SIZE_IPV4)
    }

    #[test]
    fn single_packet_frame() {
        let mut p = make_packetizer();
        let payload = vec![0xAA; 100];
        let packets = p.packetize(FrameId(0), true, FrameId(0), 90000, None, &payload);
        assert_eq!(packets.len(), 1);

        let data = &packets[0].data;
        assert_eq!(data[0], RTP_REQUIRED_FIRST_BYTE);
        assert_eq!(data[1] & 0x80, 0x80); // marker bit set (last packet)
        assert_eq!(data[1] & 0x7F, 101); // payload type

        // SSRC
        let sync_source = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        assert_eq!(sync_source, 12345);

        // Cast header
        assert_eq!(data[12] & 0x80, 0x80); // K bit (key frame)
        assert_eq!(data[12] & 0x40, 0x40); // R bit (has ref FID)
        assert_eq!(data[13], 0); // frame ID

        // Packet ID = 0
        let pid = u16::from_be_bytes([data[14], data[15]]);
        assert_eq!(pid, 0);

        // Max packet ID = 0
        let max_pid = u16::from_be_bytes([data[16], data[17]]);
        assert_eq!(max_pid, 0);

        // Referenced frame ID
        assert_eq!(data[18], 0);

        // Payload starts at byte 19
        assert_eq!(&data[19..], &payload[..]);
    }

    #[test]
    fn multi_packet_frame() {
        let mut p = make_packetizer();
        let max_payload = MAX_RTP_PACKET_SIZE_IPV4 - RTP_MAX_HEADER_SIZE;
        let payload = vec![0xBB; max_payload * 3 + 50]; // ~3.03 packets
        let packets = p.packetize(FrameId(5), false, FrameId(4), 180000, None, &payload);
        assert_eq!(packets.len(), 4);

        // First packet: not marked
        assert_eq!(packets[0].data[1] & 0x80, 0x00);
        assert_eq!(packets[0].data[12] & 0x80, 0x00); // not key frame

        // Last packet: marker bit set
        assert_eq!(packets[3].data[1] & 0x80, 0x80);

        // All packets have correct max_packet_id
        for pkt in &packets {
            let max_pid = u16::from_be_bytes([pkt.data[16], pkt.data[17]]);
            assert_eq!(max_pid, 3);
        }
    }

    #[test]
    fn sequence_numbers_increment() {
        let mut p = make_packetizer();
        let payload = vec![0; 100];

        let pkts1 = p.packetize(FrameId(0), true, FrameId(0), 0, None, &payload);
        let seq1 = u16::from_be_bytes([pkts1[0].data[2], pkts1[0].data[3]]);

        let pkts2 = p.packetize(FrameId(1), false, FrameId(0), 3000, None, &payload);
        let seq2 = u16::from_be_bytes([pkts2[0].data[2], pkts2[0].data[3]]);

        assert_eq!(seq2, seq1.wrapping_add(1));
    }

    #[test]
    fn adaptive_latency_extension() {
        let mut p = make_packetizer();
        let payload = vec![0; 100];
        let packets = p.packetize(
            FrameId(0),
            true,
            FrameId(0),
            0,
            Some(Duration::from_millis(500)),
            &payload,
        );

        let data = &packets[0].data;
        let ext_count = data[12] & 0x3F;
        assert_eq!(ext_count, 1);

        // Extension at byte 19 (after ref FID)
        let ext_header = u16::from_be_bytes([data[19], data[20]]);
        let ext_type = ext_header >> 10;
        let ext_size = ext_header & 0x3FF;
        assert_eq!(ext_type, 1);
        assert_eq!(ext_size, 2);

        let delay = u16::from_be_bytes([data[21], data[22]]);
        assert_eq!(delay, 500);

        // Payload follows extension
        assert_eq!(&data[23..], &payload[..]);
    }

    #[test]
    fn empty_payload() {
        let mut p = make_packetizer();
        let packets = p.packetize(FrameId(0), true, FrameId(0), 0, None, &[]);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].data.len(), RTP_BASE_HEADER_SIZE);
    }

    #[test]
    fn classify_rtp_packet() {
        let mut p = make_packetizer();
        let packets = p.packetize(FrameId(0), true, FrameId(0), 0, None, &[0; 10]);
        let result = classify_packet(&packets[0].data);
        assert_eq!(result, PacketType::Rtp(12345));
    }

    #[test]
    fn classify_too_short() {
        assert_eq!(classify_packet(&[0; 4]), PacketType::Unknown);
    }

    #[test]
    fn reassemble_payload() {
        let mut p = make_packetizer();
        let original = vec![0x42; 5000];
        let packets = p.packetize(FrameId(0), true, FrameId(0), 0, None, &original);

        let mut reassembled = Vec::new();
        for pkt in &packets {
            reassembled.extend_from_slice(&pkt.data[RTP_BASE_HEADER_SIZE..]);
        }
        assert_eq!(reassembled, original);
    }
}
