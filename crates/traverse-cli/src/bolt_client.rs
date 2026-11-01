use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;

use traverse_cypher::types::CypherValue;
use traverse_server::bolt_type::packstream_to_cypher_value;
use traverse_server::framing;
use traverse_server::message::{
    ClientMessage, ServerMessage, decode_server_message, encode_client_message,
};
use traverse_server::packstream::{self, Value};

/// Bolt magic preamble.
const BOLT_MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

/// Result of a query execution over Bolt.
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<CypherValue>>,
    pub stats: Vec<(String, Value)>,
    pub plan: Option<CypherValue>,
    /// Database name from server response metadata (if present).
    pub db: Option<String>,
}

/// Error type for BoltClient operations.
#[derive(Debug)]
pub enum BoltError {
    Io(std::io::Error),
    Server(traverse_server::error::ServerError),
    Protocol(String),
    QueryFailed { code: String, message: String },
}

impl std::fmt::Display for BoltError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoltError::Io(e) => write!(f, "I/O error: {e}"),
            BoltError::Server(e) => write!(f, "server error: {e}"),
            BoltError::Protocol(msg) => write!(f, "protocol error: {msg}"),
            BoltError::QueryFailed { code, message } => {
                write!(f, "{code}: {message}")
            }
        }
    }
}

impl From<std::io::Error> for BoltError {
    fn from(e: std::io::Error) -> Self {
        BoltError::Io(e)
    }
}

impl From<traverse_server::error::ServerError> for BoltError {
    fn from(e: traverse_server::error::ServerError) -> Self {
        BoltError::Server(e)
    }
}

/// Combined async read+write trait for type-erased streams.
trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncStream for T {}

/// Pinned, boxed async stream (works for both plain TCP and TLS).
type BoxedStream = Box<dyn AsyncStream>;

/// Async Bolt 5.4 client.
pub struct BoltClient {
    stream: BufStream<BoxedStream>,
    /// Target database name sent in RUN/BEGIN extras.
    pub database: Option<String>,
}

impl BoltClient {
    /// Connect to a Bolt server, perform handshake and authentication.
    /// If `tls` is true, wraps the connection in TLS (for bolt+s:// URIs).
    /// If `tls_insecure` is true, skips certificate verification.
    pub async fn connect(
        addr: &str,
        user: Option<&str>,
        password: Option<&str>,
        tls: bool,
        tls_insecure: bool,
    ) -> Result<Self, BoltError> {
        let tcp = TcpStream::connect(addr).await?;

        let boxed: BoxedStream = if tls {
            // Ensure a global default provider is installed (needed for
            // NoCertVerifier::supported_verify_schemes).
            let _ = rustls::crypto::ring::default_provider().install_default();
            let provider = rustls::crypto::CryptoProvider::get_default()
                .cloned()
                .expect("just installed");

            let config = if tls_insecure {
                rustls::ClientConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .map_err(|e| BoltError::Protocol(format!("TLS config error: {e}")))?
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
                    .with_no_client_auth()
            } else {
                let root_store = rustls::RootCertStore::from_iter(
                    webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
                );
                rustls::ClientConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .map_err(|e| BoltError::Protocol(format!("TLS config error: {e}")))?
                    .with_root_certificates(root_store)
                    .with_no_client_auth()
            };
            let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

            // For IP addresses, use "localhost" as SNI since IPs in SNI
            // can cause issues and self-signed certs typically use DNS names.
            let host = addr.split(':').next().unwrap_or(addr);
            let sni_name = if host.parse::<std::net::IpAddr>().is_ok() {
                "localhost".to_string()
            } else {
                host.to_string()
            };
            let server_name =
                rustls::pki_types::ServerName::try_from(sni_name.clone()).map_err(|e| {
                    BoltError::Protocol(format!("invalid TLS server name '{sni_name}': {e}"))
                })?;

            let tls_stream = connector
                .connect(server_name, tcp)
                .await
                .map_err(|e| BoltError::Protocol(format!("TLS handshake failed: {e}")))?;
            Box::new(tls_stream) as BoxedStream
        } else {
            Box::new(tcp) as BoxedStream
        };

        let mut client = Self {
            stream: BufStream::new(boxed),
            database: None,
        };

        // Send magic preamble
        client.stream.write_all(&BOLT_MAGIC).await?;
        // Propose Bolt 5.4
        client.stream.write_u32(0x0000_0405).await?;
        client.stream.write_u32(0).await?;
        client.stream.write_u32(0).await?;
        client.stream.write_u32(0).await?;
        client.stream.flush().await?;

        // Read negotiated version
        let mut resp = [0u8; 4];
        client.stream.read_exact(&mut resp).await?;
        if resp == [0, 0, 0, 0] {
            return Err(BoltError::Protocol(
                "server rejected all proposed Bolt versions".into(),
            ));
        }

        // HELLO
        client
            .send_message(&ClientMessage::Hello {
                extra: vec![(
                    "user_agent".into(),
                    Value::String("traverse-cli/0.1.0".into()),
                )],
            })
            .await?;
        client.expect_success("HELLO").await?;

        // LOGON
        let mut auth = vec![];
        if let (Some(u), Some(p)) = (user, password) {
            auth.push(("scheme".into(), Value::String("basic".into())));
            auth.push(("principal".into(), Value::String(u.to_string())));
            auth.push(("credentials".into(), Value::String(p.to_string())));
        } else {
            auth.push(("scheme".into(), Value::String("none".into())));
        }
        client.send_message(&ClientMessage::Logon { auth }).await?;
        client.expect_success("LOGON").await?;

        Ok(client)
    }

    /// Execute a query and return all results.
    /// Automatically sends RESET after any failure to recover connection state.
    pub async fn run_query(
        &mut self,
        query: &str,
        params: HashMap<String, CypherValue>,
    ) -> Result<QueryResult, BoltError> {
        match self.run_query_inner(query, params).await {
            Ok(result) => Ok(result),
            Err(e) => {
                // Auto-reset after failure to recover connection state
                let _ = self.reset().await;
                Err(e)
            }
        }
    }

    async fn run_query_inner(
        &mut self,
        query: &str,
        params: HashMap<String, CypherValue>,
    ) -> Result<QueryResult, BoltError> {
        let ps_params: Vec<(String, Value)> = params
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    traverse_server::bolt_type::cypher_value_to_packstream(&v, 5),
                )
            })
            .collect();

        // RUN - include "db" extra when a database is selected
        let mut run_extra = vec![];
        if let Some(ref db) = self.database {
            run_extra.push(("db".into(), Value::String(db.clone())));
        }
        self.send_message(&ClientMessage::Run {
            query: query.to_string(),
            parameters: ps_params,
            extra: run_extra,
        })
        .await?;

        let run_meta = self.expect_success("RUN").await?;
        let columns = extract_columns(&run_meta);

        // PULL all
        self.send_message(&ClientMessage::Pull {
            extra: vec![("n".into(), Value::Integer(-1))],
        })
        .await?;

        let mut rows = Vec::new();
        loop {
            let msg = self.recv_server_message().await?;
            match msg {
                ServerMessage::Record { fields } => {
                    let row: Vec<CypherValue> = fields
                        .iter()
                        .map(|v| packstream_to_cypher_value(v).unwrap_or(CypherValue::Null))
                        .collect();
                    rows.push(row);
                }
                ServerMessage::Success { metadata } => {
                    let db = extract_db(&metadata);
                    return Ok(QueryResult {
                        columns,
                        rows,
                        stats: metadata,
                        plan: None,
                        db,
                    });
                }
                ServerMessage::Failure { metadata } => {
                    let (code, message) = extract_failure(&metadata);
                    return Err(BoltError::QueryFailed { code, message });
                }
                ServerMessage::Ignored => {
                    return Err(BoltError::Protocol("PULL was IGNORED".into()));
                }
            }
        }
    }

    /// Begin an explicit transaction.
    pub async fn begin_tx(&mut self) -> Result<(), BoltError> {
        let mut extra = vec![];
        if let Some(ref db) = self.database {
            extra.push(("db".into(), Value::String(db.clone())));
        }
        self.send_message(&ClientMessage::Begin { extra }).await?;
        self.expect_success("BEGIN").await?;
        Ok(())
    }

    /// Commit the current explicit transaction.
    pub async fn commit_tx(&mut self) -> Result<(), BoltError> {
        self.send_message(&ClientMessage::Commit).await?;
        self.expect_success("COMMIT").await?;
        Ok(())
    }

    /// Rollback the current explicit transaction.
    pub async fn rollback_tx(&mut self) -> Result<(), BoltError> {
        self.send_message(&ClientMessage::Rollback).await?;
        self.expect_success("ROLLBACK").await?;
        Ok(())
    }

    /// Send RESET to clear any error state.
    pub async fn reset(&mut self) -> Result<(), BoltError> {
        self.send_message(&ClientMessage::Reset).await?;
        self.expect_success("RESET").await?;
        Ok(())
    }

    /// Send GOODBYE to cleanly close the connection.
    pub async fn goodbye(&mut self) -> Result<(), BoltError> {
        self.send_message(&ClientMessage::Goodbye).await?;
        Ok(())
    }

    async fn send_message(&mut self, msg: &ClientMessage) -> Result<(), BoltError> {
        let value = encode_client_message(msg);
        let mut buf = Vec::new();
        packstream::encode_value(&mut buf, &value);
        framing::write_message(&mut self.stream, &buf).await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn recv_server_message(&mut self) -> Result<ServerMessage, BoltError> {
        let bytes = framing::read_message(&mut self.stream).await?;
        let mut pos = 0;
        let value = packstream::decode_value(&bytes, &mut pos)
            .map_err(|e| BoltError::Protocol(format!("decode error: {e}")))?;
        Ok(decode_server_message(value)?)
    }

    async fn expect_success(&mut self, context: &str) -> Result<Vec<(String, Value)>, BoltError> {
        let msg = self.recv_server_message().await?;
        match msg {
            ServerMessage::Success { metadata } => Ok(metadata),
            ServerMessage::Failure { metadata } => {
                let (code, message) = extract_failure(&metadata);
                Err(BoltError::QueryFailed {
                    code: format!("{context} failed: {code}"),
                    message,
                })
            }
            other => Err(BoltError::Protocol(format!(
                "expected SUCCESS after {context}, got {other:?}"
            ))),
        }
    }
}

pub fn extract_columns(metadata: &[(String, Value)]) -> Vec<String> {
    for (k, v) in metadata {
        if k == "fields"
            && let Value::List(items) = v
        {
            return items
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
    }
    vec![]
}

pub fn extract_db(metadata: &[(String, Value)]) -> Option<String> {
    for (k, v) in metadata {
        if k == "db"
            && let Value::String(s) = v
        {
            return Some(s.clone());
        }
    }
    None
}

/// Certificate verifier that accepts any certificate (for --tls-insecure).
#[derive(Debug)]
struct NoCertVerifier;

impl rustls::client::danger::ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::CryptoProvider::get_default()
            .map(|p| p.signature_verification_algorithms.supported_schemes())
            .unwrap_or_default()
    }
}

pub fn extract_failure(metadata: &[(String, Value)]) -> (String, String) {
    let mut code = "Neo.ClientError.Statement.SyntaxError".to_string();
    let mut message = "unknown error".to_string();
    for (k, v) in metadata {
        if k == "code"
            && let Value::String(s) = v
        {
            code = s.clone();
        }
        if k == "message"
            && let Value::String(s) = v
        {
            message = s.clone();
        }
    }
    (code, message)
}
