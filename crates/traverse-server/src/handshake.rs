use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Result, ServerError};

/// Bolt magic preamble: 60 60 B0 17
const BOLT_MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

/// Negotiated Bolt version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoltVersion {
    pub major: u8,
    pub minor: u8,
}

impl BoltVersion {
    pub fn new(major: u8, minor: u8) -> Self {
        Self { major, minor }
    }
}

impl std::fmt::Display for BoltVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Server side of the Bolt handshake:
///
/// 1. read and verify the 4-byte magic preamble;
/// 2. read 4 version proposals (32 bits each, range-encoded);
/// 3. take the first proposal containing a supported version (6.0, 5.4-5.8),
///    its highest supported minor;
/// 4. write back the agreed version (0.0 if none).
///
/// Returns the negotiated version.
pub async fn perform_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
) -> Result<BoltVersion> {
    // magic
    let mut magic = [0u8; 4];
    stream.read_exact(&mut magic).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            ServerError::ConnectionClosed
        } else {
            ServerError::Handshake(format!("failed to read magic: {e}"))
        }
    })?;

    if magic != BOLT_MAGIC {
        return Err(ServerError::Handshake(format!(
            "invalid magic preamble: {magic:02X?}, expected {BOLT_MAGIC:02X?}"
        )));
    }

    // 4 version proposals, 32 bits each
    let mut proposals = [0u32; 4];
    for proposal in &mut proposals {
        *proposal = stream
            .read_u32()
            .await
            .map_err(|e| ServerError::Handshake(format!("failed to read version proposal: {e}")))?;
    }

    // best matching version. Each proposal is a big-endian u32 whose bytes are,
    // low to high:
    //   byte0 (bits 0-7)   = major
    //   byte1 (bits 8-15)  = minor
    //   byte2 (bits 16-23) = range (how many minor versions below `minor` are
    //                        also accepted)
    //   byte3 (bits 24-31) = padding

    let mut best: Option<BoltVersion> = None;

    // first-match priority: the first proposal with any supported version wins; within it the
    // highest minor is tried first (r=0)
    'outer: for &proposal in &proposals {
        if proposal == 0 {
            continue;
        }

        let major = (proposal & 0xFF) as u8;
        let minor = ((proposal >> 8) & 0xFF) as u8;
        let range = ((proposal >> 16) & 0xFF) as u8;

        for r in 0..=range {
            let candidate_minor = minor.saturating_sub(r);
            let candidate = BoltVersion::new(major, candidate_minor);

            if is_supported(candidate) {
                best = Some(candidate);
                break 'outer;
            }
        }
    }

    let version = best.unwrap_or(BoltVersion::new(0, 0));

    // negotiated version as 4 bytes: [padding, padding, minor, major]
    let response: [u8; 4] = [0, 0, version.minor, version.major];
    stream.write_all(&response).await?;
    stream.flush().await?;

    if version.major == 0 {
        return Err(ServerError::Handshake(
            "no supported Bolt version found".into(),
        ));
    }

    Ok(version)
}

fn is_supported(v: BoltVersion) -> bool {
    matches!(
        (v.major, v.minor),
        (5, 1) | (5, 2) | (5, 3) | (5, 4) | (5, 5) | (5, 6) | (5, 7) | (5, 8) | (6, 0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn handshake_negotiates_6_0() {
        let (mut client, mut server) = duplex(256);

        let server_task = tokio::spawn(async move { perform_handshake(&mut server).await });

        // magic
        client.write_all(&BOLT_MAGIC).await.unwrap();

        // proposal 1: Bolt 6.0 (major=6, minor=0, range=0) = [0x00, 0x00, 0x00, 0x06]
        client.write_u32(0x0000_0006).await.unwrap();
        // proposal 2: Bolt 5.4-5.8 (major=5, minor=8, range=4) -> 0x00_04_08_05
        client.write_u32(0x0004_0805).await.unwrap();
        // proposals 3-4: empty
        client.write_u32(0).await.unwrap();
        client.write_u32(0).await.unwrap();

        let version = server_task.await.unwrap().unwrap();
        assert_eq!(version, BoltVersion::new(6, 0));

        // response
        let mut resp = [0u8; 4];
        client.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp, [0, 0, 0, 6]);
    }

    #[tokio::test]
    async fn handshake_negotiates_5_4() {
        let (mut client, mut server) = duplex(256);

        let server_task = tokio::spawn(async move { perform_handshake(&mut server).await });

        client.write_all(&BOLT_MAGIC).await.unwrap();
        // only Bolt 5.4 offered: major=5, minor=4, range=0
        client.write_u32(0x0000_0405).await.unwrap();
        client.write_u32(0).await.unwrap();
        client.write_u32(0).await.unwrap();
        client.write_u32(0).await.unwrap();

        let version = server_task.await.unwrap().unwrap();
        assert_eq!(version, BoltVersion::new(5, 4));
    }

    #[tokio::test]
    async fn handshake_rejects_unsupported() {
        let (mut client, mut server) = duplex(256);

        let server_task = tokio::spawn(async move { perform_handshake(&mut server).await });

        client.write_all(&BOLT_MAGIC).await.unwrap();
        // only Bolt 3.0 offered (unsupported)
        client.write_u32(0x0000_0003).await.unwrap();
        client.write_u32(0).await.unwrap();
        client.write_u32(0).await.unwrap();
        client.write_u32(0).await.unwrap();

        let result = server_task.await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn handshake_bad_magic() {
        let (mut client, mut server) = duplex(256);

        let server_task = tokio::spawn(async move { perform_handshake(&mut server).await });

        // wrong magic bytes
        client.write_all(&[0x00, 0x00, 0x00, 0x00]).await.unwrap();
        // write proposals so the read doesn't hang
        for _ in 0..4 {
            client.write_u32(0).await.unwrap();
        }

        let result = server_task.await.unwrap();
        assert!(result.is_err());
    }
}
