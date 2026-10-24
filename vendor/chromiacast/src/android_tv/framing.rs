use std::io;

use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_FRAME_SIZE: usize = 64 * 1024;

/// Cancel-safe protobuf-varint length-delimited reader.
pub(crate) struct FramedReader<R> {
    reader: R,
    length: u64,
    length_shift: u32,
    body: Vec<u8>,
    body_filled: usize,
    body_expected: usize,
}

impl<R> FramedReader<R>
where
    R: AsyncRead + Unpin,
{
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader,
            length: 0,
            length_shift: 0,
            body: Vec::new(),
            body_filled: 0,
            body_expected: 0,
        }
    }

    pub(crate) async fn read_frame(&mut self) -> io::Result<&[u8]> {
        if self.body_expected == 0 {
            loop {
                let mut byte = [0_u8; 1];
                let count = self.reader.read(&mut byte).await?;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "connection closed while reading protobuf frame length",
                    ));
                }
                let value = byte[0];
                if self.length_shift == 63 && value > 1 {
                    return Err(invalid_data("protobuf frame length varint overflow"));
                }
                self.length |= u64::from(value & 0x7f) << self.length_shift;
                if value & 0x80 == 0 {
                    break;
                }
                self.length_shift += 7;
                if self.length_shift >= 64 {
                    return Err(invalid_data("protobuf frame length varint overflow"));
                }
            }

            self.body_expected = usize::try_from(self.length)
                .map_err(|_| invalid_data("protobuf frame length exceeds usize"))?;
            if self.body_expected == 0 {
                return Err(invalid_data("zero-length protobuf frame"));
            }
            if self.body_expected > MAX_FRAME_SIZE {
                return Err(invalid_data("protobuf frame exceeds 64 KiB"));
            }
            self.body.resize(self.body_expected, 0);
        }

        while self.body_filled < self.body_expected {
            let count = self
                .reader
                .read(&mut self.body[self.body_filled..self.body_expected])
                .await?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed while reading protobuf frame body",
                ));
            }
            self.body_filled += count;
        }

        let expected = self.body_expected;
        self.length = 0;
        self.length_shift = 0;
        self.body_expected = 0;
        self.body_filled = 0;
        Ok(&self.body[..expected])
    }
}

pub(crate) struct FramedWriter<W> {
    writer: W,
    buffer: Vec<u8>,
}

impl<W> FramedWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub(crate) fn new(writer: W) -> Self {
        Self {
            writer,
            buffer: Vec::new(),
        }
    }

    pub(crate) async fn write_message<M>(&mut self, message: &M) -> io::Result<()>
    where
        M: Message,
    {
        let length = message.encoded_len();
        if length == 0 {
            return Err(invalid_data("refusing to write an empty protobuf frame"));
        }
        if length > MAX_FRAME_SIZE {
            return Err(invalid_data("protobuf frame exceeds 64 KiB"));
        }

        self.buffer.clear();
        encode_varint(length as u64, &mut self.buffer);
        message
            .encode(&mut self.buffer)
            .map_err(|error| invalid_data(format!("encode protobuf frame: {error}")))?;
        self.writer.write_all(&self.buffer).await?;
        self.writer.flush().await
    }

    pub(crate) async fn shutdown(&mut self) -> io::Result<()> {
        self.writer.shutdown().await
    }
}

fn encode_varint(mut value: u64, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            return;
        }
    }
}

fn invalid_data(detail: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::android_tv::proto::{RemoteKeyInject, RemoteMessage};

    #[tokio::test]
    async fn round_trips_messages_across_varint_boundaries() {
        let (left, right) = tokio::io::duplex(1024);
        let (_, left_writer) = tokio::io::split(left);
        let (right_reader, _) = tokio::io::split(right);
        let mut writer = FramedWriter::new(left_writer);
        let mut reader = FramedReader::new(right_reader);
        let message = RemoteMessage {
            key_inject: Some(RemoteKeyInject {
                key_code: 243,
                direction: 3,
            }),
            ..RemoteMessage::default()
        };

        writer.write_message(&message).await.unwrap();
        let decoded = RemoteMessage::decode(reader.read_frame().await.unwrap()).unwrap();
        assert_eq!(decoded.key_inject.unwrap().key_code, 243);
    }

    #[tokio::test]
    async fn a_cancelled_body_read_resumes_without_losing_bytes() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let mut reader = FramedReader::new(reader);
        writer.write_all(&[4, 1, 2]).await.unwrap();

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), reader.read_frame())
                .await
                .is_err()
        );

        writer.write_all(&[3, 4]).await.unwrap();
        assert_eq!(reader.read_frame().await.unwrap(), [1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn rejects_oversized_frames_before_allocating_the_body() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let mut prefix = Vec::new();
        encode_varint((MAX_FRAME_SIZE + 1) as u64, &mut prefix);
        writer.write_all(&prefix).await.unwrap();
        let mut reader = FramedReader::new(reader);
        assert_eq!(
            reader.read_frame().await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
