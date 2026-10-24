use crate::constants::*;
use crate::frame::FrameId;
use crate::ntp::NtpTimestamp;
use crate::sync_source::SyncSource;

use bytes::{BufMut, BytesMut};

#[derive(Debug, Clone)]
pub struct ReceiverReport {
    pub receiver_sync_source: SyncSource,
    pub sync_source: SyncSource,
    pub fraction_lost: u8,
    pub cumulative_lost: i32,
    pub highest_sequence: u32,
    pub jitter: u32,
    pub last_sr_timestamp: u32,
    pub delay_since_last_sr: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct PacketNack {
    pub frame_id: FrameId,
    pub packet_id: u16,
}

#[derive(Debug, Clone)]
pub struct CastFeedback {
    pub receiver_sync_source: SyncSource,
    pub sender_sync_source: SyncSource,
    pub checkpoint_frame_id: u8,
    pub playout_delay_ms: u16,
    pub nacks: Vec<PacketNack>,
    pub ack_bitvector: Option<AckBitvector>,
}

#[derive(Debug, Clone)]
pub struct AckBitvector {
    pub feedback_count: u8,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct PictureLossIndication {
    pub receiver_sync_source: SyncSource,
    pub sender_sync_source: SyncSource,
}

#[derive(Debug, Clone)]
pub struct ReceiverReferenceTime {
    pub receiver_sync_source: SyncSource,
    pub ntp_timestamp: NtpTimestamp,
}

/// All feedback parsed from a single compound RTCP packet.
#[derive(Debug, Clone, Default)]
pub struct CompoundRtcpPacket {
    pub receiver_reports: Vec<ReceiverReport>,
    pub cast_feedbacks: Vec<CastFeedback>,
    pub picture_loss: Vec<PictureLossIndication>,
    pub receiver_reference_times: Vec<ReceiverReferenceTime>,
}

pub fn parse_compound(data: &[u8]) -> Result<CompoundRtcpPacket, ParseError> {
    let mut result = CompoundRtcpPacket::default();
    let mut offset = 0;

    while offset + 4 <= data.len() {
        let header = RtcpHeader::parse(&data[offset..])?;
        let packet_len = 4 + (header.length as usize) * 4;
        if offset + packet_len > data.len() {
            return Err(ParseError::Truncated);
        }

        let payload = &data[offset + 4..offset + packet_len];
        match header.packet_type {
            RTCP_RECEIVER_REPORT => {
                parse_receiver_report(payload, header.count, &mut result.receiver_reports)?;
            }
            RTCP_PAYLOAD_SPECIFIC => {
                parse_payload_specific(payload, header.count, &mut result)?;
            }
            RTCP_EXTENDED_REPORTS => {
                parse_extended_reports(payload, &mut result.receiver_reference_times)?;
            }
            _ => {}
        }

        offset += packet_len;
    }

    if offset != data.len() {
        return Err(ParseError::Truncated);
    }

    Ok(result)
}

struct RtcpHeader {
    count: u8,
    packet_type: u8,
    length: u16,
}

impl RtcpHeader {
    fn parse(data: &[u8]) -> Result<Self, ParseError> {
        if data.len() < 4 {
            return Err(ParseError::Truncated);
        }
        let version = (data[0] >> 6) & 0x03;
        if version != 2 {
            return Err(ParseError::BadVersion);
        }
        Ok(RtcpHeader {
            count: data[0] & 0x1F,
            packet_type: data[1],
            length: u16::from_be_bytes([data[2], data[3]]),
        })
    }
}

fn parse_receiver_report(
    payload: &[u8],
    report_count: u8,
    out: &mut Vec<ReceiverReport>,
) -> Result<(), ParseError> {
    if payload.len() < 4 {
        return Err(ParseError::Truncated);
    }
    let receiver_sync_source = read_u32(payload, 0);

    let mut offset = 4;
    for _ in 0..report_count {
        if offset + 24 > payload.len() {
            return Err(ParseError::Truncated);
        }
        out.push(ReceiverReport {
            receiver_sync_source,
            sync_source: read_u32(payload, offset),
            fraction_lost: payload[offset + 4],
            cumulative_lost: read_i24(payload, offset + 5),
            highest_sequence: read_u32(payload, offset + 8),
            jitter: read_u32(payload, offset + 12),
            last_sr_timestamp: read_u32(payload, offset + 16),
            delay_since_last_sr: read_u32(payload, offset + 20),
        });
        offset += 24;
    }
    Ok(())
}

fn parse_payload_specific(
    payload: &[u8],
    subtype: u8,
    out: &mut CompoundRtcpPacket,
) -> Result<(), ParseError> {
    match subtype {
        RTCP_SUBTYPE_PICTURE_LOSS => {
            if payload.len() < 8 {
                return Err(ParseError::Truncated);
            }
            out.picture_loss.push(PictureLossIndication {
                receiver_sync_source: read_u32(payload, 0),
                sender_sync_source: read_u32(payload, 4),
            });
        }
        RTCP_SUBTYPE_FEEDBACK => {
            parse_cast_feedback(payload, out)?;
        }
        _ => {}
    }
    Ok(())
}

fn parse_cast_feedback(payload: &[u8], out: &mut CompoundRtcpPacket) -> Result<(), ParseError> {
    if payload.len() < 16 {
        return Err(ParseError::Truncated);
    }

    let receiver_sync_source = read_u32(payload, 0);
    let sender_sync_source = read_u32(payload, 4);
    let magic = read_u32(payload, 8);
    if magic != CAST_FEEDBACK_MAGIC {
        return Err(ParseError::BadMagic);
    }

    let checkpoint_frame_id = payload[12];
    let loss_field_count = payload[13] as usize;
    let playout_delay_ms = u16::from_be_bytes([payload[14], payload[15]]);

    let mut nacks = Vec::new();
    let mut offset = 16;
    for _ in 0..loss_field_count {
        if offset + 4 > payload.len() {
            return Err(ParseError::Truncated);
        }
        let within_frame_id = payload[offset];
        let lost_packet_id = u16::from_be_bytes([payload[offset + 1], payload[offset + 2]]);
        let bitvector = payload[offset + 3];

        if lost_packet_id == ALL_PACKETS_LOST {
            nacks.push(PacketNack {
                frame_id: FrameId::expand(within_frame_id, FrameId(checkpoint_frame_id as i64)),
                packet_id: ALL_PACKETS_LOST,
            });
        } else {
            nacks.push(PacketNack {
                frame_id: FrameId::expand(within_frame_id, FrameId(checkpoint_frame_id as i64)),
                packet_id: lost_packet_id,
            });
            // Expand the bit vector: each bit represents the next packet
            for bit in 0..8u16 {
                if bitvector & (1 << bit) != 0 {
                    let packet_id = lost_packet_id
                        .checked_add(1 + bit)
                        .ok_or(ParseError::InvalidValue)?;
                    nacks.push(PacketNack {
                        frame_id: FrameId::expand(
                            within_frame_id,
                            FrameId(checkpoint_frame_id as i64),
                        ),
                        packet_id,
                    });
                }
            }
        }

        offset += 4;
    }

    let ack_bitvector = if offset + 4 <= payload.len() {
        let ack_magic = read_u32(payload, offset);
        if ack_magic == CAST_ACK_MAGIC && offset + 6 <= payload.len() {
            let feedback_count = payload[offset + 4];
            let num_octets = payload[offset + 5] as usize;
            offset += 6;
            if offset + num_octets <= payload.len() {
                let data = payload[offset..offset + num_octets].to_vec();
                Some(AckBitvector {
                    feedback_count,
                    data,
                })
            } else {
                return Err(ParseError::Truncated);
            }
        } else {
            None
        }
    } else {
        None
    };

    out.cast_feedbacks.push(CastFeedback {
        receiver_sync_source,
        sender_sync_source,
        checkpoint_frame_id,
        playout_delay_ms,
        nacks,
        ack_bitvector,
    });

    Ok(())
}

fn parse_extended_reports(
    payload: &[u8],
    out: &mut Vec<ReceiverReferenceTime>,
) -> Result<(), ParseError> {
    if payload.len() < 4 {
        return Err(ParseError::Truncated);
    }
    let receiver_sync_source = read_u32(payload, 0);

    let mut offset = 4;
    while offset + 4 <= payload.len() {
        let block_type = payload[offset];
        let block_length = u16::from_be_bytes([payload[offset + 2], payload[offset + 3]]) as usize;
        let block_data_len = block_length * 4;
        offset += 4;

        if offset + block_data_len > payload.len() {
            return Err(ParseError::Truncated);
        }

        if block_type == 4 && block_data_len >= 8 && offset + 8 <= payload.len() {
            let ntp_seconds = read_u32(payload, offset);
            let ntp_fraction = read_u32(payload, offset + 4);
            let ntp = NtpTimestamp(((ntp_seconds as u64) << 32) | ntp_fraction as u64);
            out.push(ReceiverReferenceTime {
                receiver_sync_source,
                ntp_timestamp: ntp,
            });
        }

        offset += block_data_len;
    }

    Ok(())
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn read_i24(data: &[u8], offset: usize) -> i32 {
    let value = (i32::from(data[offset]) << 16)
        | (i32::from(data[offset + 1]) << 8)
        | i32::from(data[offset + 2]);
    if value & 0x80_0000 != 0 {
        value | !0xff_ffff
    } else {
        value
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Truncated,
    BadVersion,
    BadMagic,
    InvalidValue,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Truncated => write!(f, "packet truncated"),
            ParseError::BadVersion => write!(f, "unsupported RTCP version"),
            ParseError::BadMagic => write!(f, "bad Cast feedback magic"),
            ParseError::InvalidValue => write!(f, "invalid RTCP field value"),
        }
    }
}

impl std::error::Error for ParseError {}

pub struct SenderReportBuilder;

impl SenderReportBuilder {
    /// Build a Sender Report RTCP packet.
    pub fn build(
        sync_source: SyncSource,
        ntp_timestamp: NtpTimestamp,
        rtp_timestamp: u32,
        packet_count: u32,
        octet_count: u32,
    ) -> BytesMut {
        let mut buf = BytesMut::with_capacity(28);
        // RTCP header: V=2, P=0, RC=0, PT=200, length=6 (24 bytes / 4 - 1)
        buf.put_u8(0x80);
        buf.put_u8(RTCP_SENDER_REPORT);
        buf.put_u16(6); // length in 32-bit words minus one
        buf.put_u32(sync_source);
        buf.put_u32(ntp_timestamp.upper_32());
        buf.put_u32(ntp_timestamp.fraction());
        buf.put_u32(rtp_timestamp);
        buf.put_u32(packet_count);
        buf.put_u32(octet_count);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_cast_feedback(
        receiver_sync_source: u32,
        sender_sync_source: u32,
        checkpoint: u8,
        playout_delay: u16,
        loss_fields: &[(u8, u16, u8)],
    ) -> Vec<u8> {
        let loss_count = loss_fields.len();
        let payload_words = 4 + loss_count; // 16 bytes header + 4 per loss field
        let mut buf = Vec::new();

        // RTCP header
        buf.push(0x80 | RTCP_SUBTYPE_FEEDBACK);
        buf.push(RTCP_PAYLOAD_SPECIFIC);
        let length = payload_words as u16;
        buf.extend_from_slice(&length.to_be_bytes());

        // Payload
        buf.extend_from_slice(&receiver_sync_source.to_be_bytes());
        buf.extend_from_slice(&sender_sync_source.to_be_bytes());
        buf.extend_from_slice(&CAST_FEEDBACK_MAGIC.to_be_bytes());
        buf.push(checkpoint);
        buf.push(loss_count as u8);
        buf.extend_from_slice(&playout_delay.to_be_bytes());

        for &(fid, pid, bv) in loss_fields {
            buf.push(fid);
            buf.extend_from_slice(&pid.to_be_bytes());
            buf.push(bv);
        }

        buf
    }

    #[test]
    fn parse_cast_feedback_no_loss() {
        let data = build_cast_feedback(1000, 2000, 42, 400, &[]);
        let result = parse_compound(&data).unwrap();

        assert_eq!(result.cast_feedbacks.len(), 1);
        let fb = &result.cast_feedbacks[0];
        assert_eq!(fb.receiver_sync_source, 1000);
        assert_eq!(fb.sender_sync_source, 2000);
        assert_eq!(fb.checkpoint_frame_id, 42);
        assert_eq!(fb.playout_delay_ms, 400);
        assert!(fb.nacks.is_empty());
    }

    #[test]
    fn parse_cast_feedback_with_nack() {
        let data = build_cast_feedback(1000, 2000, 10, 400, &[(11, 5, 0x00)]);
        let result = parse_compound(&data).unwrap();

        let fb = &result.cast_feedbacks[0];
        assert_eq!(fb.nacks.len(), 1);
        assert_eq!(fb.nacks[0].packet_id, 5);
    }

    #[test]
    fn parse_cast_feedback_bitvector_expansion() {
        // Loss field: frame 11, packet 5, bitvector 0b00000101 (packets 6 and 8)
        let data = build_cast_feedback(1000, 2000, 10, 400, &[(11, 5, 0b0000_0101)]);
        let result = parse_compound(&data).unwrap();

        let fb = &result.cast_feedbacks[0];
        assert_eq!(fb.nacks.len(), 3); // packet 5 + packets 6 and 8
        assert_eq!(fb.nacks[0].packet_id, 5);
        assert_eq!(fb.nacks[1].packet_id, 6);
        assert_eq!(fb.nacks[2].packet_id, 8);
    }

    #[test]
    fn parse_all_packets_lost() {
        let data = build_cast_feedback(1000, 2000, 10, 400, &[(11, ALL_PACKETS_LOST, 0xFF)]);
        let result = parse_compound(&data).unwrap();

        let fb = &result.cast_feedbacks[0];
        assert_eq!(fb.nacks.len(), 1);
        assert_eq!(fb.nacks[0].packet_id, ALL_PACKETS_LOST);
    }

    #[test]
    fn parse_picture_loss() {
        let mut buf = Vec::new();
        // RTCP header: V=2, subtype=picture_loss, PT=PAYLOAD_SPECIFIC
        buf.push(0x80 | RTCP_SUBTYPE_PICTURE_LOSS);
        buf.push(RTCP_PAYLOAD_SPECIFIC);
        buf.extend_from_slice(&2u16.to_be_bytes()); // length = 2 words
        buf.extend_from_slice(&1000u32.to_be_bytes());
        buf.extend_from_slice(&2000u32.to_be_bytes());

        let result = parse_compound(&buf).unwrap();
        assert_eq!(result.picture_loss.len(), 1);
        assert_eq!(result.picture_loss[0].receiver_sync_source, 1000);
        assert_eq!(result.picture_loss[0].sender_sync_source, 2000);
    }

    #[test]
    fn parse_receiver_reference_time() {
        let mut buf = Vec::new();
        // RTCP header: V=2, RC=0, PT=EXTENDED_REPORTS
        buf.push(0x80);
        buf.push(RTCP_EXTENDED_REPORTS);
        buf.extend_from_slice(&4u16.to_be_bytes()); // length = 4 words (16 bytes)
        buf.extend_from_slice(&5000u32.to_be_bytes());

        // Block: type=4, reserved=0, block_length=2
        buf.push(4); // block type
        buf.push(0); // reserved
        buf.extend_from_slice(&2u16.to_be_bytes()); // block length
        buf.extend_from_slice(&0xAAAA_BBBBu32.to_be_bytes()); // NTP seconds
        buf.extend_from_slice(&0xCCDD_EEFFu32.to_be_bytes()); // NTP fraction

        let result = parse_compound(&buf).unwrap();
        assert_eq!(result.receiver_reference_times.len(), 1);
        let rrt = &result.receiver_reference_times[0];
        assert_eq!(rrt.receiver_sync_source, 5000);
        assert_eq!(rrt.ntp_timestamp.upper_32(), 0xAAAA_BBBB);
        assert_eq!(rrt.ntp_timestamp.fraction(), 0xCCDD_EEFF);
    }

    #[test]
    fn parse_signed_cumulative_packet_loss() {
        let mut buf = vec![0x81, RTCP_RECEIVER_REPORT, 0, 7];
        buf.extend_from_slice(&1000u32.to_be_bytes());
        buf.extend_from_slice(&2000u32.to_be_bytes());
        buf.extend_from_slice(&[3, 0xff, 0xff, 0xff]);
        buf.extend_from_slice(&1234u32.to_be_bytes());
        buf.extend_from_slice(&5u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());

        let result = parse_compound(&buf).unwrap();
        assert_eq!(result.receiver_reports[0].fraction_lost, 3);
        assert_eq!(result.receiver_reports[0].cumulative_lost, -1);
    }

    #[test]
    fn build_sender_report() {
        let ntp = NtpTimestamp((100 << 32) | 0x8000_0000);
        let buf = SenderReportBuilder::build(12345, ntp, 90000, 100, 50000);

        assert_eq!(buf.len(), 28);
        assert_eq!(buf[0], 0x80);
        assert_eq!(buf[1], RTCP_SENDER_REPORT);
        let sync_source = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(sync_source, 12345);
    }

    #[test]
    fn parse_truncated_header_is_rejected() {
        assert_eq!(
            parse_compound(&[0x80, 200]).unwrap_err(),
            ParseError::Truncated
        );
    }

    #[test]
    fn parse_truncated_payload() {
        // Valid header claiming 6 words, but only 4 bytes provided
        let data = [0x80, RTCP_RECEIVER_REPORT, 0, 6 /* missing payload */];
        assert_eq!(parse_compound(&data).unwrap_err(), ParseError::Truncated);
    }

    #[test]
    fn parse_bad_version() {
        let data = [0x00, 200, 0, 0]; // version 0
        assert_eq!(parse_compound(&data).unwrap_err(), ParseError::BadVersion);
    }

    #[test]
    fn parse_empty() {
        let result = parse_compound(&[]).unwrap();
        assert!(result.cast_feedbacks.is_empty());
    }
}

/// Packets from openscreen's compound_rtcp_parser_fuzzer_seeds (written by its receiver-side RTCP builder).
#[cfg(test)]
mod openscreen_vectors {
    use super::*;

    macro_rules! seed {
        ($name:literal) => {
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/openscreen_rtcp/", $name, ".bin"))
        };
    }

    const ALL: [&[u8]; 19] = [
        seed!("builder_basics"),
        seed!("builder_including_picture_loss_indicator"),
        seed!("builder_including_receiver_report_block"),
        seed!("builder_with_lots_of_nacks_and_some_acks"),
        seed!("builder_with_lots_of_nacks_and_some_more_acks"),
        seed!("builder_with_lots_of_nacks"),
        seed!("builder_with_multiple_acks"),
        seed!("builder_with_nack_mix"),
        seed!("builder_with_one_ack"),
        seed!("feedback_with_nacks"),
        seed!("feedback_with_one_nack_and_one_ack"),
        seed!("feedback_with_zero_nacks_and_many_acks"),
        seed!("picture_loss_indicator_and_reference_time_report"),
        seed!("picture_loss_indicator"),
        seed!("receiver_report_and_reference_time_report"),
        seed!("receiver_report_with_report_block"),
        seed!("receiver_report_without_report_block"),
        seed!("simple_feedback"),
        seed!("three_extended_reports"),
    ];

    #[test]
    fn every_seed_parses() {
        for (i, packet) in ALL.iter().enumerate() {
            parse_compound(packet).unwrap_or_else(|e| panic!("seed {i}: {e:?}"));
        }
    }

    #[test]
    fn nack_mix_expands_bitvector_and_all_lost() {
        let parsed = parse_compound(seed!("builder_with_nack_mix")).unwrap();
        assert_eq!(parsed.receiver_reference_times.len(), 1);
        let fb = &parsed.cast_feedbacks[0];
        assert_eq!((fb.receiver_sync_source, fb.sender_sync_source), (2, 1));
        assert_eq!((fb.checkpoint_frame_id, fb.playout_delay_ms), (0x0f, 400));
        let nacks: Vec<(i64, u16)> = fb.nacks.iter().map(|n| (n.frame_id.raw(), n.packet_id)).collect();
        assert_eq!(
            nacks,
            [(0x10, 0), (0x10, 1), (0x10, 2), (0x10, 7), (0x10, 15), (0x11, 19), (0x12, 0xffff), (0x13, 0xffff)]
        );
    }

    #[test]
    fn multiple_acks_bitvector() {
        let parsed = parse_compound(seed!("builder_with_multiple_acks")).unwrap();
        let fb = &parsed.cast_feedbacks[0];
        assert_eq!(fb.checkpoint_frame_id, 0x16);
        let acks = fb.ack_bitvector.as_ref().unwrap();
        assert_eq!((acks.feedback_count, acks.data.as_slice()), (2, &[0x02, 0, 0x0c, 0, 0, 0][..]));
    }

    #[test]
    fn picture_loss_indicator() {
        let parsed = parse_compound(seed!("builder_including_picture_loss_indicator")).unwrap();
        let pli = &parsed.picture_loss[0];
        assert_eq!((pli.receiver_sync_source, pli.sender_sync_source), (2, 1));
    }
}
