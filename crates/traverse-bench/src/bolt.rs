//! Minimal async Bolt 5.4 client for benchmarking.
//!
//! Only implements: handshake, HELLO, LOGON, RUN, PULL, RESET.
//! No TLS, no routing, no transactions - just enough for benchmark queries.

use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;

// -- Bolt constants --------------------------------------------------

const BOLT_MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

// Client message tags
const TAG_HELLO: u8 = 0x01;
const TAG_LOGON: u8 = 0x6A;
const TAG_RUN: u8 = 0x10;
const TAG_PULL: u8 = 0x3F;
const TAG_RESET: u8 = 0x0F;

// Server message tags
const TAG_SUCCESS: u8 = 0x70;
const TAG_RECORD: u8 = 0x71;
const TAG_IGNORED: u8 = 0x7E;
const TAG_FAILURE: u8 = 0x7F;

// -- Error type ------------------------------------------------------

#[derive(Debug)]
pub enum BoltError {
    Io(io::Error),
    Protocol(String),
    ServerFailure(String),
}

impl From<io::Error> for BoltError {
    fn from(e: io::Error) -> Self {
        BoltError::Io(e)
    }
}

impl std::fmt::Display for BoltError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoltError::Io(e) => write!(f, "IO: {e}"),
            BoltError::Protocol(s) => write!(f, "Protocol: {s}"),
            BoltError::ServerFailure(s) => write!(f, "Server: {s}"),
        }
    }
}

// -- Connection ------------------------------------------------------

pub struct BoltConnection {
    reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
    writer: BufWriter<tokio::io::WriteHalf<TcpStream>>,
    /// Reusable buffer for building outgoing messages.
    encode_buf: Vec<u8>,
    /// Reusable buffer for reading incoming messages.
    read_buf: Vec<u8>,
    /// Negotiated Bolt version (major, minor).
    bolt_version: (u8, u8),
}

impl BoltConnection {
    /// Connect to a Bolt server, perform handshake and auth.
    ///
    /// Supports Bolt 5.4 (Traverse, Neo4j 5+) and Bolt 4.x (Memgraph).
    pub async fn connect(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
    ) -> Result<Self, BoltError> {
        let stream = TcpStream::connect((host, port)).await?;
        stream.set_nodelay(true)?;

        let (rd, wr) = tokio::io::split(stream);
        let mut conn = BoltConnection {
            reader: BufReader::new(rd),
            writer: BufWriter::new(wr),
            encode_buf: Vec::with_capacity(4096),
            read_buf: Vec::with_capacity(4096),
            bolt_version: (0, 0),
        };

        conn.handshake().await?;

        if conn.bolt_version.0 >= 5 {
            // Bolt 5.x: HELLO then separate LOGON
            conn.hello().await?;
            conn.logon(user, password).await?;
        } else {
            // Bolt 4.x: credentials go in HELLO (no LOGON step)
            conn.hello_v4(user, password).await?;
        }

        Ok(conn)
    }

    async fn handshake(&mut self) -> Result<(), BoltError> {
        // Send magic + 4 version proposals
        self.writer.write_all(&BOLT_MAGIC).await?;

        // Proposal 1: Bolt 5.4  [padding=0, range=0, minor=4, major=5]
        self.writer.write_all(&0x0000_0405u32.to_be_bytes()).await?;
        // Proposal 2: Bolt 4.1  [padding=0, range=0, minor=1, major=4]
        self.writer.write_all(&0x0000_0104u32.to_be_bytes()).await?;
        // Proposals 3-4: empty
        self.writer.write_all(&[0u8; 8]).await?;
        self.writer.flush().await?;

        // Read negotiated version (4 bytes)
        let mut resp = [0u8; 4];
        self.reader.read_exact(&mut resp).await?;

        // resp = [padding, padding, minor, major]
        let major = resp[3];
        let minor = resp[2];
        if major == 0 {
            return Err(BoltError::Protocol(
                "server rejected all Bolt version proposals".into(),
            ));
        }
        if major != 5 && major != 4 {
            return Err(BoltError::Protocol(format!(
                "unexpected Bolt version {major}.{minor}"
            )));
        }
        self.bolt_version = (major, minor);
        Ok(())
    }

    async fn hello(&mut self) -> Result<(), BoltError> {
        // HELLO {user_agent: "traverse-bench/0.1", bolt_agent: {product: "traverse-bench/0.1"}}
        self.encode_buf.clear();
        self.encode_buf.push(0xB1);
        self.encode_buf.push(TAG_HELLO);
        encode_map_header(&mut self.encode_buf, 2);
        encode_string(&mut self.encode_buf, "user_agent");
        encode_string(&mut self.encode_buf, "traverse-bench/0.1");
        encode_string(&mut self.encode_buf, "bolt_agent");
        // bolt_agent is a map: {product: "traverse-bench/0.1"}
        encode_map_header(&mut self.encode_buf, 1);
        encode_string(&mut self.encode_buf, "product");
        encode_string(&mut self.encode_buf, "traverse-bench/0.1");

        self.send_message().await?;
        self.expect_success().await
    }

    /// HELLO for Bolt 4.x - credentials are included in the HELLO map.
    async fn hello_v4(&mut self, user: &str, password: &str) -> Result<(), BoltError> {
        // Bolt 4.x requires auth fields in HELLO even with empty credentials.
        self.encode_buf.clear();
        self.encode_buf.push(0xB1);
        self.encode_buf.push(TAG_HELLO);
        encode_map_header(&mut self.encode_buf, 4);
        encode_string(&mut self.encode_buf, "user_agent");
        encode_string(&mut self.encode_buf, "traverse-bench/0.1");
        encode_string(&mut self.encode_buf, "scheme");
        encode_string(&mut self.encode_buf, "basic");
        encode_string(&mut self.encode_buf, "principal");
        encode_string(&mut self.encode_buf, user);
        encode_string(&mut self.encode_buf, "credentials");
        encode_string(&mut self.encode_buf, password);

        self.send_message().await?;
        self.expect_success().await
    }

    async fn logon(&mut self, user: &str, password: &str) -> Result<(), BoltError> {
        // LOGON {scheme: "basic", principal: user, credentials: password}
        self.encode_buf.clear();
        self.encode_buf.push(0xB1);
        self.encode_buf.push(TAG_LOGON);
        encode_map_header(&mut self.encode_buf, 3);
        encode_string(&mut self.encode_buf, "scheme");
        encode_string(&mut self.encode_buf, "basic");
        encode_string(&mut self.encode_buf, "principal");
        encode_string(&mut self.encode_buf, user);
        encode_string(&mut self.encode_buf, "credentials");
        encode_string(&mut self.encode_buf, password);

        self.send_message().await?;
        self.expect_success().await
    }

    /// Execute a query with integer parameters and drain all result rows.
    /// Returns Ok(()) on success, Err on failure.
    pub async fn run_and_pull(
        &mut self,
        cypher: &str,
        params: &[(&str, i64)],
    ) -> Result<(), BoltError> {
        // RUN cypher, params, {}
        self.encode_buf.clear();
        self.encode_buf.push(0xB3); // 3 fields
        self.encode_buf.push(TAG_RUN);
        encode_string(&mut self.encode_buf, cypher);
        encode_map_header(&mut self.encode_buf, params.len());
        for (key, val) in params {
            encode_string(&mut self.encode_buf, key);
            encode_integer(&mut self.encode_buf, *val);
        }
        encode_map_header(&mut self.encode_buf, 0); // empty extra
        self.send_message().await?;

        // PULL {n: -1}
        self.encode_buf.clear();
        self.encode_buf.push(0xB1); // 1 field
        self.encode_buf.push(TAG_PULL);
        encode_map_header(&mut self.encode_buf, 1);
        encode_string(&mut self.encode_buf, "n");
        encode_integer(&mut self.encode_buf, -1);
        self.send_message().await?;

        // Read RUN response (SUCCESS)
        self.expect_success().await?;

        // Read PULL responses: RECORD* then SUCCESS
        loop {
            let tag = self.read_response_tag().await?;
            match tag {
                TAG_SUCCESS => break,
                TAG_RECORD => continue, // skip row data
                TAG_FAILURE => {
                    return Err(BoltError::ServerFailure("query failed during PULL".into()));
                }
                other => {
                    return Err(BoltError::Protocol(format!(
                        "unexpected tag during PULL: 0x{other:02X}"
                    )));
                }
            }
        }

        Ok(())
    }

    /// Send a RESET to recover from a failed state.
    pub async fn reset(&mut self) -> Result<(), BoltError> {
        self.encode_buf.clear();
        self.encode_buf.push(0xB0);
        self.encode_buf.push(TAG_RESET);
        self.send_message().await?;
        // Drain until SUCCESS (may get IGNORED for pending messages)
        loop {
            let tag = self.read_response_tag().await?;
            if tag == TAG_SUCCESS {
                break;
            }
        }
        Ok(())
    }

    // -- Pipelining (batch import) ----------------------------------

    /// Enqueue a RUN + PULL pair for a parameterless Cypher statement.
    /// Does NOT flush - call [`flush`](Self::flush) after enqueuing a batch.
    pub async fn enqueue_run_pull_no_params(&mut self, cypher: &str) -> Result<(), BoltError> {
        // RUN cypher, {}, {}
        self.encode_buf.clear();
        self.encode_buf.push(0xB3); // 3 fields
        self.encode_buf.push(TAG_RUN);
        encode_string(&mut self.encode_buf, cypher);
        encode_map_header(&mut self.encode_buf, 0); // empty params
        encode_map_header(&mut self.encode_buf, 0); // empty extra
        self.send_message_no_flush().await?;

        // PULL {n: -1}
        self.encode_buf.clear();
        self.encode_buf.push(0xB1); // 1 field
        self.encode_buf.push(TAG_PULL);
        encode_map_header(&mut self.encode_buf, 1);
        encode_string(&mut self.encode_buf, "n");
        encode_integer(&mut self.encode_buf, -1);
        self.send_message_no_flush().await?;

        Ok(())
    }

    /// Flush the write buffer (call after enqueuing a batch).
    pub async fn flush(&mut self) -> Result<(), BoltError> {
        self.writer.flush().await?;
        Ok(())
    }

    /// Drain responses for `count` pipelined RUN+PULL pairs.
    ///
    /// Returns `(successes, errors)`.  On any FAILURE the server enters
    /// a failed state; remaining responses arrive as IGNORED.  A RESET
    /// is sent automatically to recover.
    pub async fn drain_import_batch(&mut self, count: usize) -> Result<(usize, usize), BoltError> {
        let mut ok: usize = 0;
        let mut errors: usize = 0;
        let mut failed = false;

        for _ in 0..count {
            if failed {
                // Server is in FAILED state - remaining pairs are IGNORED.
                self.read_response_tag().await?; // IGNORED for RUN
                self.read_response_tag().await?; // IGNORED for PULL
                errors += 1;
                continue;
            }

            // RUN response
            let tag = self.read_response_tag().await?;
            match tag {
                TAG_SUCCESS => {}
                TAG_FAILURE | TAG_IGNORED => {
                    failed = true;
                    // Still need to drain the PULL response
                    self.read_response_tag().await?;
                    errors += 1;
                    continue;
                }
                other => {
                    return Err(BoltError::Protocol(format!(
                        "expected SUCCESS/FAILURE for RUN, got 0x{other:02X}"
                    )));
                }
            }

            // PULL responses: RECORD* then SUCCESS
            loop {
                let tag = self.read_response_tag().await?;
                match tag {
                    TAG_SUCCESS => {
                        ok += 1;
                        break;
                    }
                    TAG_RECORD => continue,
                    TAG_FAILURE | TAG_IGNORED => {
                        failed = true;
                        errors += 1;
                        break;
                    }
                    other => {
                        return Err(BoltError::Protocol(format!(
                            "unexpected tag during PULL: 0x{other:02X}"
                        )));
                    }
                }
            }
        }

        if failed {
            self.reset().await?;
        }

        Ok((ok, errors))
    }

    // -- Internal helpers --------------------------------------------

    /// Send the contents of encode_buf as a chunked Bolt message,
    /// WITHOUT flushing.  Used for pipelining.
    async fn send_message_no_flush(&mut self) -> Result<(), BoltError> {
        let data = &self.encode_buf;
        let mut offset = 0;
        while offset < data.len() {
            let remaining = data.len() - offset;
            let chunk_size = remaining.min(0xFFFF);
            self.writer
                .write_all(&(chunk_size as u16).to_be_bytes())
                .await?;
            self.writer
                .write_all(&data[offset..offset + chunk_size])
                .await?;
            offset += chunk_size;
        }
        // Zero terminator
        self.writer.write_all(&[0, 0]).await?;
        Ok(())
    }

    /// Send the contents of encode_buf as a chunked Bolt message.
    async fn send_message(&mut self) -> Result<(), BoltError> {
        self.send_message_no_flush().await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Read a complete chunked Bolt message into read_buf.
    async fn read_message(&mut self) -> Result<(), BoltError> {
        self.read_buf.clear();
        loop {
            let chunk_size = self.reader.read_u16().await? as usize;
            if chunk_size == 0 {
                break;
            }
            let start = self.read_buf.len();
            self.read_buf.resize(start + chunk_size, 0);
            self.reader.read_exact(&mut self.read_buf[start..]).await?;
        }
        Ok(())
    }

    /// Read a response and return its structure tag.
    /// Skips over the actual field data (we don't need it for benchmarking).
    async fn read_response_tag(&mut self) -> Result<u8, BoltError> {
        self.read_message().await?;
        if self.read_buf.len() < 2 {
            return Err(BoltError::Protocol("empty response message".into()));
        }
        // First byte is structure marker (0xB0-0xBF), second is tag
        let marker = self.read_buf[0];
        if marker & 0xF0 != 0xB0 {
            return Err(BoltError::Protocol(format!(
                "expected structure marker, got 0x{marker:02X}"
            )));
        }
        Ok(self.read_buf[1])
    }

    /// Read a response and expect SUCCESS. Returns error on FAILURE.
    async fn expect_success(&mut self) -> Result<(), BoltError> {
        let tag = self.read_response_tag().await?;
        match tag {
            TAG_SUCCESS => Ok(()),
            TAG_FAILURE => {
                // Try to extract error message from the response
                let msg = extract_failure_message(&self.read_buf);
                Err(BoltError::ServerFailure(msg))
            }
            other => Err(BoltError::Protocol(format!(
                "expected SUCCESS, got tag 0x{other:02X}"
            ))),
        }
    }
}

// -- PackStream encoding (minimal) -----------------------------------

fn encode_string(buf: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    let len = b.len();
    if len <= 0x0F {
        buf.push(0x80 | len as u8);
    } else if len <= 0xFF {
        buf.push(0xD0);
        buf.push(len as u8);
    } else {
        buf.push(0xD1);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    }
    buf.extend_from_slice(b);
}

fn encode_integer(buf: &mut Vec<u8>, n: i64) {
    if (-16..=127).contains(&n) {
        buf.push(n as u8);
    } else if (i8::MIN as i64..=i8::MAX as i64).contains(&n) {
        buf.push(0xC8);
        buf.push(n as i8 as u8);
    } else if (i16::MIN as i64..=i16::MAX as i64).contains(&n) {
        buf.push(0xC9);
        buf.extend_from_slice(&(n as i16).to_be_bytes());
    } else if (i32::MIN as i64..=i32::MAX as i64).contains(&n) {
        buf.push(0xCA);
        buf.extend_from_slice(&(n as i32).to_be_bytes());
    } else {
        buf.push(0xCB);
        buf.extend_from_slice(&n.to_be_bytes());
    }
}

fn encode_map_header(buf: &mut Vec<u8>, len: usize) {
    if len <= 0x0F {
        buf.push(0xA0 | len as u8);
    } else if len <= 0xFF {
        buf.push(0xD8);
        buf.push(len as u8);
    } else {
        buf.push(0xD9);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    }
}

/// Best-effort extraction of error message from a FAILURE response.
fn extract_failure_message(data: &[u8]) -> String {
    // FAILURE is Structure(tag=0x7F, fields=[Map{message: String, code: String}])
    // The map contains PackStream-encoded key-value pairs.
    // Try to extract all string values from the raw data.
    let mut strings = Vec::new();
    let mut i = 2; // skip structure marker + tag
    while i < data.len() {
        let b = data[i];
        let (str_len, skip) = if b & 0xF0 == 0x80 {
            // Tiny string: 0x8X where X is the length
            ((b & 0x0F) as usize, 1)
        } else if b == 0xD0 && i + 1 < data.len() {
            // String8
            (data[i + 1] as usize, 2)
        } else if b == 0xD1 && i + 2 < data.len() {
            // String16
            (u16::from_be_bytes([data[i + 1], data[i + 2]]) as usize, 3)
        } else {
            i += 1;
            continue;
        };
        i += skip;
        if i + str_len <= data.len()
            && let Ok(s) = std::str::from_utf8(&data[i..i + str_len])
        {
            strings.push(s.to_string());
        }
        i += str_len;
    }
    if strings.is_empty() {
        format!(
            "FAILURE (raw {} bytes: {:02X?})",
            data.len(),
            &data[..data.len().min(64)]
        )
    } else {
        strings.join(" | ")
    }
}
