use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::proto::CastMessage;

/// Cancel-safe length-prefixed CastMessage reader.
///
/// Survives `select!` cancellation by tracking read progress in `self`.
/// Each `read_message()` call resumes where a previous cancelled call left off.
pub struct FramedReader<R> {
    reader: R,
    len_buf: [u8; 4],
    len_filled: usize,
    body_buf: Vec<u8>,
    body_filled: usize,
    body_expected: usize,
}

impl<R: AsyncRead + Unpin> FramedReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            len_buf: [0; 4],
            len_filled: 0,
            body_buf: Vec::new(),
            body_filled: 0,
            body_expected: 0,
        }
    }

    pub async fn read_message(&mut self) -> io::Result<CastMessage> {
        while self.len_filled < 4 {
            let n = self
                .reader
                .read(&mut self.len_buf[self.len_filled..])
                .await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed while reading message length",
                ));
            }
            self.len_filled += n;
        }

        if self.body_expected == 0 {
            self.body_expected = u32::from_be_bytes(self.len_buf) as usize;
            if self.body_expected == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "zero-length message",
                ));
            }
            if self.body_expected > 1 << 20 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "message too large",
                ));
            }
            self.body_buf.resize(self.body_expected, 0);
            self.body_filled = 0;
        }

        while self.body_filled < self.body_expected {
            let n = self
                .reader
                .read(&mut self.body_buf[self.body_filled..self.body_expected])
                .await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed while reading message body",
                ));
            }
            self.body_filled += n;
        }

        let msg = CastMessage::decode(&self.body_buf[..self.body_expected])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        self.len_filled = 0;
        self.body_expected = 0;
        self.body_filled = 0;
        Ok(msg)
    }
}

pub struct FramedWriter<W> {
    writer: W,
}

impl<W: AsyncWrite + Unpin> FramedWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    pub async fn write_message(&mut self, msg: &CastMessage) -> io::Result<()> {
        let encoded = msg.encode();
        let len = (encoded.len() as u32).to_be_bytes();
        self.writer.write_all(&len).await?;
        self.writer.write_all(&encoded).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::proto::Payload;
    use super::*;

    #[tokio::test]
    async fn roundtrip_through_framing() {
        let msg = CastMessage {
            source_id: "sender-0".into(),
            destination_id: "receiver-0".into(),
            namespace: "urn:x-cast:test".into(),
            payload: Payload::String(r#"{"type":"TEST"}"#.into()),
        };

        let mut buf: Vec<u8> = Vec::new();
        {
            let mut writer = FramedWriter::new(&mut buf);
            writer.write_message(&msg).await.unwrap();
        }

        let mut reader = FramedReader::new(&buf[..]);
        let decoded = reader.read_message().await.unwrap();

        assert_eq!(decoded.source_id, "sender-0");
        assert_eq!(decoded.destination_id, "receiver-0");
        assert_eq!(decoded.namespace, "urn:x-cast:test");
    }

    #[tokio::test]
    async fn multiple_messages() {
        let msgs: Vec<CastMessage> = (0..3)
            .map(|i| CastMessage {
                source_id: "s".into(),
                destination_id: "d".into(),
                namespace: format!("ns-{}", i),
                payload: Payload::String(format!("payload-{}", i)),
            })
            .collect();

        let mut buf: Vec<u8> = Vec::new();
        {
            let mut writer = FramedWriter::new(&mut buf);
            for msg in &msgs {
                writer.write_message(msg).await.unwrap();
            }
        }

        let mut reader = FramedReader::new(&buf[..]);
        for expected in &msgs {
            let decoded = reader.read_message().await.unwrap();
            assert_eq!(decoded.namespace, expected.namespace);
        }
    }

    #[tokio::test]
    async fn eof_returns_error() {
        let buf: &[u8] = &[];
        let mut reader = FramedReader::new(buf);
        let result = reader.read_message().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn truncated_body_returns_error() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&100u32.to_be_bytes()); // claims 100 bytes
        buf.extend_from_slice(&[0u8; 10]); // only 10 bytes follow
        let mut reader = FramedReader::new(&buf[..]);
        let result = reader.read_message().await;
        assert!(result.is_err());
    }
}
