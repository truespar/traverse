use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Result, ServerError};

/// Max message size: 16 MB.
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Max chunk payload.
const MAX_CHUNK_SIZE: usize = 0xFFFF;

/// Reads one Bolt message from the chunked transport: u16-length-prefixed chunks ended by a
/// zero-length chunk (0x00 0x00).
pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut message = Vec::new();

    loop {
        let chunk_size = reader.read_u16().await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                ServerError::ConnectionClosed
            } else {
                ServerError::Io(e)
            }
        })? as usize;

        if chunk_size == 0 {
            break;
        }

        if message.len() + chunk_size > MAX_MESSAGE_SIZE {
            return Err(ServerError::Protocol(format!(
                "message exceeds maximum size of {MAX_MESSAGE_SIZE} bytes"
            )));
        }

        let start = message.len();
        message.resize(start + chunk_size, 0);
        reader
            .read_exact(&mut message[start..])
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    ServerError::ConnectionClosed
                } else {
                    ServerError::Io(e)
                }
            })?;
    }

    // empty message = NOOP keep-alive (0x0000 chunk pair); an empty vec lets the caller skip
    // decode/dispatch
    Ok(message)
}

/// Writes one Bolt message to the chunked transport: chunks of at most 65535 bytes, then a
/// zero-length terminator. Does **not** flush; flush once after a batch.
pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, data: &[u8]) -> Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        let remaining = data.len() - offset;
        let chunk_size = remaining.min(MAX_CHUNK_SIZE);

        writer.write_u16(chunk_size as u16).await?;
        writer.write_all(&data[offset..offset + chunk_size]).await?;
        offset += chunk_size;
    }

    // terminator
    writer.write_u16(0).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_small_message() {
        let (client, server) = tokio::io::duplex(4096);
        let (_reader, mut writer) = tokio::io::split(client);
        let (mut server_reader, _server_writer) = tokio::io::split(server);

        let data = b"hello world";

        let write_task = tokio::spawn(async move {
            write_message(&mut writer, data).await.unwrap();
            writer.flush().await.unwrap();
        });

        let read_task =
            tokio::spawn(async move { read_message(&mut server_reader).await.unwrap() });

        write_task.await.unwrap();
        let result = read_task.await.unwrap();
        assert_eq!(result, data);
    }

    #[tokio::test]
    async fn roundtrip_large_message() {
        let (client, server) = tokio::io::duplex(256 * 1024);
        let (_reader, mut writer) = tokio::io::split(client);
        let (mut server_reader, _server_writer) = tokio::io::split(server);

        // 200 KB message (several chunks)
        let data: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
        let data_clone = data.clone();

        let write_task = tokio::spawn(async move {
            write_message(&mut writer, &data_clone).await.unwrap();
            writer.flush().await.unwrap();
        });

        let read_task =
            tokio::spawn(async move { read_message(&mut server_reader).await.unwrap() });

        write_task.await.unwrap();
        let result = read_task.await.unwrap();
        assert_eq!(result, data);
    }

    #[tokio::test]
    async fn connection_closed_on_eof() {
        let (client, server) = tokio::io::duplex(4096);
        let (mut server_reader, _) = tokio::io::split(server);
        drop(client); // close the connection

        let result = read_message(&mut server_reader).await;
        assert!(result.is_err());
    }
}
