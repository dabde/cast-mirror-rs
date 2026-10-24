//! Minimal protobuf surface used by Android TV Remote Service v2.
//!
//! These definitions intentionally model only the messages Chromiacast sends
//! or observes. Protobuf's unknown-field rules let newer devices extend the
//! protocol without making the control client claim support for those fields.

use prost::Message;

pub(crate) const PAIRING_STATUS_OK: i32 = 200;
pub(crate) const PAIRING_PROTOCOL_VERSION: u32 = 2;

pub(crate) const ROLE_INPUT: i32 = 1;
pub(crate) const ENCODING_HEXADECIMAL: i32 = 3;
pub(crate) const PAIRING_SYMBOL_LENGTH: u32 = 6;

#[derive(Clone, PartialEq, Message)]
pub(crate) struct PairingEnvelope {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(int32, tag = "2")]
    pub status: i32,
    #[prost(message, optional, tag = "10")]
    pub pairing_request: Option<PairingRequest>,
    #[prost(message, optional, tag = "11")]
    pub pairing_request_ack: Option<PairingRequestAck>,
    #[prost(message, optional, tag = "20")]
    pub options: Option<PairingOptions>,
    #[prost(message, optional, tag = "30")]
    pub configuration: Option<PairingConfiguration>,
    #[prost(message, optional, tag = "31")]
    pub configuration_ack: Option<PairingConfigurationAck>,
    #[prost(message, optional, tag = "40")]
    pub secret: Option<PairingSecret>,
    #[prost(message, optional, tag = "41")]
    pub secret_ack: Option<PairingSecretAck>,
}

impl PairingEnvelope {
    pub(crate) fn ok() -> Self {
        Self {
            protocol_version: PAIRING_PROTOCOL_VERSION,
            status: PAIRING_STATUS_OK,
            ..Self::default()
        }
    }
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct PairingRequest {
    #[prost(string, tag = "1")]
    pub service_name: String,
    #[prost(string, optional, tag = "2")]
    pub client_name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct PairingRequestAck {
    #[prost(string, optional, tag = "1")]
    pub server_name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct PairingEncoding {
    #[prost(int32, tag = "1")]
    pub encoding_type: i32,
    #[prost(uint32, tag = "2")]
    pub symbol_length: u32,
}

impl PairingEncoding {
    pub(crate) fn hexadecimal_pin() -> Self {
        Self {
            encoding_type: ENCODING_HEXADECIMAL,
            symbol_length: PAIRING_SYMBOL_LENGTH,
        }
    }

    pub(crate) fn is_hexadecimal_pin(&self) -> bool {
        self.encoding_type == ENCODING_HEXADECIMAL && self.symbol_length == PAIRING_SYMBOL_LENGTH
    }
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct PairingOptions {
    #[prost(message, repeated, tag = "1")]
    pub input_encodings: Vec<PairingEncoding>,
    #[prost(message, repeated, tag = "2")]
    pub output_encodings: Vec<PairingEncoding>,
    #[prost(int32, optional, tag = "3")]
    pub preferred_role: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct PairingConfiguration {
    #[prost(message, optional, tag = "1")]
    pub encoding: Option<PairingEncoding>,
    #[prost(int32, tag = "2")]
    pub client_role: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct PairingConfigurationAck {}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct PairingSecret {
    #[prost(bytes = "vec", tag = "1")]
    pub secret: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct PairingSecretAck {
    #[prost(bytes = "vec", tag = "1")]
    pub secret: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RemoteMessage {
    #[prost(message, optional, tag = "1")]
    pub configure: Option<RemoteConfigure>,
    #[prost(message, optional, tag = "2")]
    pub set_active: Option<RemoteSetActive>,
    #[prost(message, optional, tag = "3")]
    pub error: Option<RemoteError>,
    #[prost(message, optional, tag = "8")]
    pub ping_request: Option<RemotePingRequest>,
    #[prost(message, optional, tag = "9")]
    pub ping_response: Option<RemotePingResponse>,
    #[prost(message, optional, tag = "10")]
    pub key_inject: Option<RemoteKeyInject>,
    #[prost(message, optional, tag = "40")]
    pub start: Option<RemoteStart>,
    #[prost(message, optional, tag = "50")]
    pub volume: Option<RemoteVolume>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RemoteConfigure {
    #[prost(int32, tag = "1")]
    pub features: i32,
    #[prost(message, optional, tag = "2")]
    pub device_info: Option<RemoteDeviceInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RemoteDeviceInfo {
    #[prost(string, tag = "1")]
    pub model: String,
    #[prost(string, tag = "2")]
    pub vendor: String,
    #[prost(int32, tag = "3")]
    pub unknown_one: i32,
    #[prost(string, tag = "4")]
    pub unknown_two: String,
    #[prost(string, tag = "5")]
    pub package_name: String,
    #[prost(string, tag = "6")]
    pub app_version: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RemoteSetActive {
    #[prost(int32, tag = "1")]
    pub features: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RemoteError {
    #[prost(bool, tag = "1")]
    pub value: bool,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RemotePingRequest {
    #[prost(int32, tag = "1")]
    pub value: i32,
    #[prost(int32, tag = "2")]
    pub auxiliary_value: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RemotePingResponse {
    #[prost(int32, tag = "1")]
    pub value: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RemoteKeyInject {
    #[prost(int32, tag = "1")]
    pub key_code: i32,
    #[prost(int32, tag = "2")]
    pub direction: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RemoteStart {
    #[prost(bool, tag = "1")]
    pub powered_on: bool,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RemoteVolume {
    #[prost(uint32, tag = "6")]
    pub maximum: u32,
    #[prost(uint32, tag = "7")]
    pub level: u32,
    #[prost(bool, tag = "8")]
    pub muted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_key_injection_matches_the_deployed_wire_shape() {
        let message = RemoteMessage {
            key_inject: Some(RemoteKeyInject {
                key_code: 243,
                direction: 3,
            }),
            ..RemoteMessage::default()
        };
        // field 10 (length-delimited), nested enum values 243 and 3
        assert_eq!(
            message.encode_to_vec(),
            [0x52, 0x05, 0x08, 0xf3, 0x01, 0x10, 0x03]
        );
    }

    #[test]
    fn pairing_request_matches_the_polo_field_numbers() {
        let message = PairingEnvelope {
            pairing_request: Some(PairingRequest {
                service_name: "atvremote".into(),
                client_name: Some("Pronk".into()),
            }),
            ..PairingEnvelope::ok()
        };
        let encoded = message.encode_to_vec();
        let decoded = PairingEnvelope::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.protocol_version, 2);
        assert_eq!(decoded.status, 200);
        assert_eq!(decoded.pairing_request.unwrap().service_name, "atvremote");
    }
}
