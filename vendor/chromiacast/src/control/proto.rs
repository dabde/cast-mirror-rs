use std::fmt;

#[derive(Clone)]
pub struct CastMessage {
    pub source_id: String,
    pub destination_id: String,
    pub namespace: String,
    pub payload: Payload,
}

#[derive(Debug, Clone)]
pub enum Payload {
    String(String),
    Binary(Vec<u8>),
}

impl CastMessage {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(128);
        encode_varint_field(&mut buf, 1, 0); // protocol_version = CASTV2_1_0
        encode_string_field(&mut buf, 2, &self.source_id);
        encode_string_field(&mut buf, 3, &self.destination_id);
        encode_string_field(&mut buf, 4, &self.namespace);
        match &self.payload {
            Payload::String(s) => {
                encode_varint_field(&mut buf, 5, 0); // payload_type = STRING
                encode_string_field(&mut buf, 6, s);
            }
            Payload::Binary(b) => {
                encode_varint_field(&mut buf, 5, 1); // payload_type = BINARY
                encode_bytes_field(&mut buf, 7, b);
            }
        }
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let mut pos = 0;
        let mut source_id = None;
        let mut destination_id = None;
        let mut namespace = None;
        let mut payload_type: i32 = 0;
        let mut payload_utf8 = None;
        let mut payload_binary = None;

        while pos < data.len() {
            let tag = decode_varint(data, &mut pos)?;
            let field_number = (tag >> 3) as u32;
            let wire_type = (tag & 0x07) as u8;

            match (field_number, wire_type) {
                (1, 0) => {
                    decode_varint(data, &mut pos)?;
                } // protocol_version, ignore
                (2, 2) => {
                    source_id = Some(decode_string(data, &mut pos)?);
                }
                (3, 2) => {
                    destination_id = Some(decode_string(data, &mut pos)?);
                }
                (4, 2) => {
                    namespace = Some(decode_string(data, &mut pos)?);
                }
                (5, 0) => {
                    payload_type = decode_varint(data, &mut pos)? as i32;
                }
                (6, 2) => {
                    payload_utf8 = Some(decode_string(data, &mut pos)?);
                }
                (7, 2) => {
                    payload_binary = Some(decode_bytes(data, &mut pos)?);
                }
                (_, 0) => {
                    decode_varint(data, &mut pos)?;
                }
                (_, 2) => {
                    let _ = decode_bytes(data, &mut pos)?;
                }
                (_, 5) => {
                    skip_bytes(data, &mut pos, 4)?;
                } // 32-bit
                (_, 1) => {
                    skip_bytes(data, &mut pos, 8)?;
                } // 64-bit
                _ => return Err(DecodeError::UnknownWireType(wire_type)),
            }
        }

        let payload = if payload_type == 1 {
            Payload::Binary(payload_binary.unwrap_or_default())
        } else {
            Payload::String(payload_utf8.unwrap_or_default())
        };

        Ok(CastMessage {
            source_id: source_id.ok_or(DecodeError::MissingField("source_id"))?,
            destination_id: destination_id.ok_or(DecodeError::MissingField("destination_id"))?,
            namespace: namespace.ok_or(DecodeError::MissingField("namespace"))?,
            payload,
        })
    }
}

impl fmt::Debug for CastMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CastMessage")
            .field("source_id", &self.source_id)
            .field("destination_id", &self.destination_id)
            .field("namespace", &self.namespace)
            .field("payload", &self.payload)
            .finish()
    }
}

#[derive(Debug)]
pub enum DecodeError {
    Truncated,
    InvalidVarint,
    InvalidUtf8,
    UnknownWireType(u8),
    MissingField(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "truncated protobuf"),
            DecodeError::InvalidVarint => write!(f, "invalid varint"),
            DecodeError::InvalidUtf8 => write!(f, "invalid UTF-8 in string field"),
            DecodeError::UnknownWireType(wt) => write!(f, "unknown wire type {}", wt),
            DecodeError::MissingField(name) => write!(f, "missing required field: {}", name),
        }
    }
}

impl std::error::Error for DecodeError {}

fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        if value < 0x80 {
            buf.push(value as u8);
            return;
        }
        buf.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
}

fn encode_varint_field(buf: &mut Vec<u8>, field: u32, value: u64) {
    encode_varint(buf, (field as u64) << 3);
    encode_varint(buf, value);
}

fn encode_bytes_field(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    encode_varint(buf, ((field as u64) << 3) | 2);
    encode_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

fn encode_string_field(buf: &mut Vec<u8>, field: u32, s: &str) {
    encode_bytes_field(buf, field, s.as_bytes());
}

fn decode_varint(data: &[u8], pos: &mut usize) -> Result<u64, DecodeError> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= data.len() {
            return Err(DecodeError::Truncated);
        }
        let byte = data[*pos];
        *pos += 1;
        if shift == 63 && byte > 1 {
            return Err(DecodeError::InvalidVarint);
        }
        value |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift >= 64 {
            return Err(DecodeError::InvalidVarint);
        }
    }
}

fn decode_bytes(data: &[u8], pos: &mut usize) -> Result<Vec<u8>, DecodeError> {
    let len = usize::try_from(decode_varint(data, pos)?).map_err(|_| DecodeError::Truncated)?;
    let end = pos.checked_add(len).ok_or(DecodeError::Truncated)?;
    let result = data.get(*pos..end).ok_or(DecodeError::Truncated)?.to_vec();
    *pos = end;
    Ok(result)
}

fn skip_bytes(data: &[u8], pos: &mut usize, len: usize) -> Result<(), DecodeError> {
    let end = pos.checked_add(len).ok_or(DecodeError::Truncated)?;
    if end > data.len() {
        return Err(DecodeError::Truncated);
    }
    *pos = end;
    Ok(())
}

fn decode_string(data: &[u8], pos: &mut usize) -> Result<String, DecodeError> {
    let bytes = decode_bytes(data, pos)?;
    String::from_utf8(bytes).map_err(|_| DecodeError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip_string() {
        let msg = CastMessage {
            source_id: "sender-0".into(),
            destination_id: "receiver-0".into(),
            namespace: "urn:x-cast:com.google.cast.tp.heartbeat".into(),
            payload: Payload::String(r#"{"type":"PING"}"#.into()),
        };
        let encoded = msg.encode();
        let decoded = CastMessage::decode(&encoded).unwrap();
        assert_eq!(decoded.source_id, "sender-0");
        assert_eq!(decoded.destination_id, "receiver-0");
        assert_eq!(decoded.namespace, "urn:x-cast:com.google.cast.tp.heartbeat");
        match decoded.payload {
            Payload::String(s) => assert_eq!(s, r#"{"type":"PING"}"#),
            _ => panic!("expected string payload"),
        }
    }

    #[test]
    fn encode_decode_roundtrip_binary() {
        let msg = CastMessage {
            source_id: "s".into(),
            destination_id: "d".into(),
            namespace: "ns".into(),
            payload: Payload::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        };
        let encoded = msg.encode();
        let decoded = CastMessage::decode(&encoded).unwrap();
        match decoded.payload {
            Payload::Binary(b) => assert_eq!(b, vec![0xDE, 0xAD, 0xBE, 0xEF]),
            _ => panic!("expected binary payload"),
        }
    }

    #[test]
    fn decode_missing_field() {
        let mut buf = Vec::new();
        encode_string_field(&mut buf, 2, "sender");
        // missing destination_id (field 3), namespace (field 4)
        let err = CastMessage::decode(&buf).unwrap_err();
        assert!(matches!(err, DecodeError::MissingField(_)));
    }

    #[test]
    fn decode_empty_is_error() {
        let err = CastMessage::decode(&[]).unwrap_err();
        assert!(matches!(err, DecodeError::MissingField(_)));
    }

    #[test]
    fn truncated_unknown_fixed_width_field_is_rejected() {
        let data = [0x0d, 0, 0];
        assert!(matches!(
            CastMessage::decode(&data),
            Err(DecodeError::Truncated)
        ));
    }

    #[test]
    fn varint_encoding() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, 0);
        assert_eq!(buf, vec![0x00]);

        buf.clear();
        encode_varint(&mut buf, 127);
        assert_eq!(buf, vec![0x7F]);

        buf.clear();
        encode_varint(&mut buf, 128);
        assert_eq!(buf, vec![0x80, 0x01]);

        buf.clear();
        encode_varint(&mut buf, 300);
        assert_eq!(buf, vec![0xAC, 0x02]);
    }

    #[test]
    fn rejects_varints_that_exceed_u64_without_panicking() {
        let mut position = 0;
        let overflow = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        assert!(matches!(
            decode_varint(&overflow, &mut position),
            Err(DecodeError::InvalidVarint)
        ));

        position = 0;
        assert!(matches!(
            decode_varint(&[0x80; 10], &mut position),
            Err(DecodeError::InvalidVarint)
        ));
    }

    #[test]
    fn accepts_largest_u64_varint() {
        let mut position = 0;
        let encoded = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert_eq!(decode_varint(&encoded, &mut position).unwrap(), u64::MAX);
        assert_eq!(position, encoded.len());
    }

    #[test]
    fn skips_unknown_fields() {
        let msg = CastMessage {
            source_id: "s".into(),
            destination_id: "d".into(),
            namespace: "ns".into(),
            payload: Payload::String("test".into()),
        };
        let mut encoded = msg.encode();
        // Append an unknown varint field (field 99, wire type 0, value 42)
        encode_varint_field(&mut encoded, 99, 42);
        let decoded = CastMessage::decode(&encoded).unwrap();
        assert_eq!(decoded.source_id, "s");
    }
}
