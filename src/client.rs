use anyhow::{Context, Result};
use bytes::{Buf, Bytes};
use h3_quinn::quinn::{ClientConfig, Endpoint};
use http::{Method, Request, StatusCode};
use quinn::crypto::rustls::QuicClientConfig;
use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};
use tokio::net::lookup_host;
use tracing::debug;

#[derive(Clone)]
pub struct H3Client {
    #[allow(unused)]
    h3_conn: Arc<h3::client::Connection<h3_quinn::Connection, Bytes>>,
    h3_send_request: h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    server_host: String,
}

#[derive(Debug)]
pub struct H3Response {
    pub status: StatusCode,
    pub headers: HashMap<String, String>,
    pub body: Bytes,
}

#[derive(Debug)]
pub struct SseEvent {
    pub event_type: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

impl H3Client {
    pub async fn new(
        server_name: &str,
        server_port: u16,
        ca_path: PathBuf,
        tls_server_name: Option<&str>,
    ) -> Result<Self> {
        // Install default crypto provider (if needed)
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        // Load CA certificate
        let ca_cert_data = fs::read(ca_path).context("Failed to read CA certificate")?;

        let ca_certs = rustls_pemfile::certs(&mut ca_cert_data.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to parse CA certificate")?;

        let mut root_store = rustls::RootCertStore::empty();
        for cert in ca_certs {
            root_store
                .add(cert)
                .context("Failed to add CA certificate to store")?;
        }

        // Create TLS config with the CA certificate
        let mut tls_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        // Set ALPN protocols for HTTP/3
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        let client_config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls_config)?));

        // Create Quinn endpoint
        let mut endpoint = Endpoint::client("[::]:0".parse()?)?;
        endpoint.set_default_client_config(client_config);

        let server = format!("{}:{}", server_name, server_port);

        // Use DNS resolution to get the socket address (IPv4 only)
        let server_addr = lookup_host(&server)
            .await
            .context(format!("Failed to resolve server address: {server}"))?
            .find(|addr| addr.is_ipv4())
            .context("No IPv4 addresses found for server")?;

        debug!("Connecting to {}...", server_addr);

        // Use tls_server_name or server_name for validation
        let validation_name = tls_server_name.unwrap_or(server_name);

        // Connect to server using validation
        let quinn_conn = endpoint
            .connect(server_addr, validation_name)?
            .await
            .context("Failed to establish QUIC connection")?;

        debug!("QUIC connection established");

        // Create H3 connection
        let (h3_conn, h3_send_request) =
            h3::client::new(h3_quinn::Connection::new(quinn_conn)).await?;

        debug!("HTTP/3 connection established with {}", server);

        Ok(H3Client {
            h3_conn: Arc::new(h3_conn),
            h3_send_request,
            server_host: server.clone(),
        })
    }

    pub async fn delete(&mut self, path: &str) -> Result<H3Response> {
        self.request(Method::DELETE, path, None).await
    }

    pub async fn get(&mut self, path: &str) -> Result<H3Response> {
        self.request(Method::GET, path, None).await
    }

    pub async fn get_stream(&mut self, path: &str) -> Result<SseStream> {
        self.sse_stream(path).await
    }

    pub async fn get_binary_stream(&mut self, path: &str) -> Result<BinaryStream> {
        let req = Request::builder()
            .method(Method::GET)
            .uri(path)
            .header("Host", &self.server_host)
            .body(())?;

        debug!("Starting binary stream for {}", path);

        let mut request_stream = self.h3_send_request.send_request(req).await?;
        request_stream.finish().await?;

        let response = request_stream.recv_response().await?;
        debug!(
            "Binary stream response: {} {}",
            response.status(),
            response.status().canonical_reason().unwrap_or("")
        );

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Binary stream request failed with status: {}",
                response.status()
            ));
        }

        Ok(BinaryStream::new(request_stream))
    }

    pub async fn post(&mut self, path: &str, body: Option<Bytes>) -> Result<H3Response> {
        self.request(Method::POST, path, body).await
    }

    pub async fn put(&mut self, path: &str, body: Option<Bytes>) -> Result<H3Response> {
        self.request(Method::PUT, path, body).await
    }

    async fn request(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Bytes>,
    ) -> Result<H3Response> {
        let mut req_builder = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", &self.server_host);

        if body.is_some() {
            req_builder = req_builder.header("Content-Type", "application/json");
        }

        let req = req_builder.body(())?;

        debug!("Sending {} request to {}", req.method(), path);

        let mut request_stream = self.h3_send_request.send_request(req).await?;

        if let Some(body_data) = body {
            request_stream.send_data(body_data).await?;
        }
        request_stream.finish().await?;

        debug!("Request sent, waiting for response...");

        let response = request_stream.recv_response().await?;
        let status = response.status();

        debug!(
            "Response received: {} {}",
            status,
            status.canonical_reason().unwrap_or("")
        );

        let mut headers = HashMap::new();
        for (name, value) in response.headers() {
            headers.insert(name.to_string(), value.to_str().unwrap_or("").to_string());
        }

        let mut body_data = Vec::new();
        while let Some(chunk) = request_stream.recv_data().await? {
            body_data.extend_from_slice(chunk.chunk());
        }

        Ok(H3Response {
            status,
            headers,
            body: Bytes::from(body_data),
        })
    }

    async fn sse_stream(&mut self, path: &str) -> Result<SseStream> {
        let req = Request::builder()
            .method(Method::GET)
            .uri(path)
            .header("Host", &self.server_host)
            .header("Accept", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .body(())?;

        debug!("Starting SSE stream for {}", path);

        let mut request_stream = self.h3_send_request.send_request(req).await?;
        request_stream.finish().await?;

        debug!("SSE request sent, waiting for response...");

        let response = request_stream.recv_response().await?;
        debug!(
            "SSE response received: {} {}",
            response.status(),
            response.status().canonical_reason().unwrap_or("")
        );

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "SSE request failed with status: {}",
                response.status()
            ));
        }

        let encoding = response
            .headers()
            .iter()
            .find(|(name, _)| name.as_str().eq_ignore_ascii_case("content-encoding"))
            .map(|(_, value)| value.to_str().unwrap_or("").to_string());

        if let Some(ref enc) = encoding {
            debug!("SSE stream has Content-Encoding: {}", enc);
        }

        Ok(SseStream::new(request_stream, encoding))
    }
}

pub struct SseStream {
    request_stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    buffer: Vec<u8>,
    current_event: SseEventBuilder,
    encoding: Option<String>,
}

#[derive(Default)]
struct SseEventBuilder {
    event_type: Option<String>,
    data_lines: Vec<String>,
    id: Option<String>,
}

impl SseEventBuilder {
    fn process_line(&mut self, line: &str) {
        if line.starts_with("data:") {
            let data = line.strip_prefix("data:").unwrap_or("").trim_start();
            self.data_lines.push(data.to_string());
        } else if line.starts_with("event:") {
            self.event_type = Some(line.strip_prefix("event:").unwrap_or("").trim().to_string());
        } else if line.starts_with("id:") {
            self.id = Some(line.strip_prefix("id:").unwrap_or("").trim().to_string());
        }
    }

    fn build_event(&self) -> Option<SseEvent> {
        if self.data_lines.is_empty() && self.event_type.is_none() && self.id.is_none() {
            return None;
        }

        Some(SseEvent {
            event_type: self.event_type.clone(),
            data: self.data_lines.join("\n"),
            id: self.id.clone(),
        })
    }

    fn reset(&mut self) {
        self.event_type = None;
        self.data_lines.clear();
        self.id = None;
    }
}

impl SseStream {
    fn new(
        request_stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
        encoding: Option<String>,
    ) -> Self {
        Self {
            request_stream,
            buffer: Vec::new(),
            current_event: SseEventBuilder::default(),
            encoding,
        }
    }

    pub async fn next_event(&mut self) -> Result<Option<SseEvent>> {
        loop {
            match self.request_stream.recv_data().await? {
                Some(chunk) => {
                    let chunk_bytes = chunk.chunk().to_vec();
                    self.buffer.extend_from_slice(&chunk_bytes);

                    while let Some(newline_pos) = self.buffer.iter().position(|&b| b == b'\n') {
                        let line_bytes = self.buffer.drain(..=newline_pos).collect::<Vec<u8>>();
                        let line = String::from_utf8_lossy(&line_bytes).trim_end().to_string();

                        if line.is_empty() {
                            if let Some(event) = self.current_event.build_event() {
                                self.current_event.reset();
                                return Ok(Some(event));
                            }
                        } else {
                            self.current_event.process_line(&line);
                        }
                    }
                }
                None => {
                    if let Some(event) = self.current_event.build_event() {
                        self.current_event.reset();
                        return Ok(Some(event));
                    }
                    return Ok(None);
                }
            }
        }
    }

    pub fn is_compressed(&self) -> bool {
        self.encoding.is_some()
    }

    pub fn encoding(&self) -> Option<&str> {
        self.encoding.as_deref()
    }

    pub async fn next_raw_chunk(&mut self) -> Result<Option<Bytes>> {
        match self.request_stream.recv_data().await? {
            Some(chunk) => Ok(Some(Bytes::copy_from_slice(chunk.chunk()))),
            None => Ok(None),
        }
    }
}

/// Maximum allowed frame payload size (16 MiB).
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Try to extract a complete length-prefixed frame from the buffer.
/// Returns `Ok(Some(payload))` and drains the consumed bytes, `Ok(None)` if
/// the buffer doesn't yet contain a complete frame, or `Err` if the
/// advertised length exceeds `MAX_FRAME_SIZE`.
fn try_parse_frame(buffer: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(anyhow::anyhow!(
            "frame length {len} exceeds maximum allowed size {MAX_FRAME_SIZE}"
        ));
    }
    if buffer.len() < 4 + len {
        return Ok(None);
    }
    let frame = buffer[4..4 + len].to_vec();
    buffer.drain(..4 + len);
    Ok(Some(frame))
}

/// Length-prefixed binary frame stream over HTTP3.
/// Each frame is `[4-byte big-endian u32 length][payload]`.
pub struct BinaryStream {
    request_stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    buffer: Vec<u8>,
}

impl BinaryStream {
    fn new(request_stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>) -> Self {
        Self {
            request_stream,
            buffer: Vec::new(),
        }
    }

    /// Returns the next length-prefixed frame payload, or `None` if the stream ended.
    pub async fn next_frame(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(frame) = try_parse_frame(&mut self.buffer)? {
                return Ok(Some(frame));
            }

            match self.request_stream.recv_data().await? {
                Some(chunk) => self.buffer.extend_from_slice(chunk.chunk()),
                None => {
                    if self.buffer.is_empty() {
                        return Ok(None);
                    }
                    return Err(anyhow::anyhow!(
                        "stream ended with {} bytes of incomplete frame data",
                        self.buffer.len()
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::try_parse_frame;

    fn make_frame(payload: &[u8]) -> Vec<u8> {
        let len = (payload.len() as u32).to_be_bytes();
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&len);
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn parse_complete_frame() {
        let mut buf = make_frame(b"hello");
        let result = try_parse_frame(&mut buf).unwrap();
        assert_eq!(result, Some(b"hello".to_vec()));
        assert!(buf.is_empty());
    }

    #[test]
    fn parse_empty_payload() {
        let mut buf = make_frame(b"");
        let result = try_parse_frame(&mut buf).unwrap();
        assert_eq!(result, Some(vec![]));
        assert!(buf.is_empty());
    }

    #[test]
    fn parse_incomplete_header() {
        let mut buf = vec![0, 0];
        assert_eq!(try_parse_frame(&mut buf).unwrap(), None);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn parse_incomplete_payload() {
        let mut buf = vec![0, 0, 0, 10, 1, 2, 3];
        assert_eq!(try_parse_frame(&mut buf).unwrap(), None);
        assert_eq!(buf.len(), 7);
    }

    #[test]
    fn parse_empty_buffer() {
        let mut buf = vec![];
        assert_eq!(try_parse_frame(&mut buf).unwrap(), None);
    }

    #[test]
    fn parse_multiple_frames_sequentially() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&make_frame(b"first"));
        buf.extend_from_slice(&make_frame(b"second"));

        let first = try_parse_frame(&mut buf).unwrap();
        assert_eq!(first, Some(b"first".to_vec()));

        let second = try_parse_frame(&mut buf).unwrap();
        assert_eq!(second, Some(b"second".to_vec()));

        assert!(buf.is_empty());
    }

    #[test]
    fn parse_leaves_trailing_data() {
        let mut buf = make_frame(b"data");
        buf.extend_from_slice(&[0, 0, 0]);

        let result = try_parse_frame(&mut buf).unwrap();
        assert_eq!(result, Some(b"data".to_vec()));
        assert_eq!(buf, vec![0, 0, 0]);
    }

    #[test]
    fn parse_rejects_oversized_frame() {
        let oversized_len = (super::MAX_FRAME_SIZE as u32 + 1).to_be_bytes();
        let mut buf = oversized_len.to_vec();
        let err = try_parse_frame(&mut buf).unwrap_err();
        assert!(err.to_string().contains("exceeds maximum allowed size"));
    }
}
