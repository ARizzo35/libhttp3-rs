use anyhow::Result;
use axum::Router;
use bytes::Bytes;
use h3_quinn::quinn;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

/// The sending half of a close signal for [`http3_serve`].
pub type CloseHandle = watch::Sender<()>;

/// The receiving half, passed into [`http3_serve`].
pub type CloseSignal = watch::Receiver<()>;

/// Create a linked [`CloseHandle`]/[`CloseSignal`] pair for [`http3_serve`].
pub fn close_signal() -> (CloseHandle, CloseSignal) {
    watch::channel(())
}

pub async fn http3_serve(
    router: Router,
    addr: SocketAddr,
    certpath: PathBuf,
    keypath: PathBuf,
    close_signal: Option<CloseSignal>,
) -> Result<()> {
    // Install the default crypto provider. The only way this fails is if a
    // provider is already installed.
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("crypto provider already installed; reusing the existing one");
    }

    // Load certificate and private key from files
    let cert_pem = fs::read_to_string(certpath)?;
    let key_pem = fs::read_to_string(keypath)?;

    let cert_der =
        rustls_pemfile::certs(&mut cert_pem.as_bytes()).collect::<Result<Vec<_>, _>>()?;
    let cert = cert_der
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No certificate found in file"))?;

    let key_der = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
        .ok_or_else(|| anyhow::anyhow!("No private key found in file"))?;

    // Configure TLS with rustls (standard rustls configuration)
    // See: https://docs.rs/rustls/latest/rustls/server/struct.ServerConfig.html
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key_der)?;

    // HTTP/3 requires ALPN protocol negotiation
    tls_config.alpn_protocols = vec![b"h3".to_vec()];

    // Enable 0-RTT (early data) - allows clients to send data in first packet
    // WARNING: 0-RTT data can be replayed, only use for idempotent operations
    tls_config.max_early_data_size = u32::MAX;

    // Configure QUIC transport with Quinn (standard Quinn configuration)
    // See: https://docs.rs/quinn/latest/quinn/struct.ServerConfig.html
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls_config)?,
    ));

    // Configure QUIC transport parameters directly
    // See: https://docs.rs/quinn/latest/quinn/struct.TransportConfig.html
    let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
    transport_config
        .max_concurrent_bidi_streams(100_u32.into()) // Max concurrent HTTP requests
        .max_concurrent_uni_streams(100_u32.into()) // Max concurrent unidirectional streams
        .max_idle_timeout(Some(crate::MAX_IDLE_TIMEOUT.try_into()?))
        .keep_alive_interval(Some(crate::KEEP_ALIVE_INTERVAL));

    // Bind and listen
    let endpoint = quinn::Endpoint::server(server_config, addr)?;
    while let Some(incoming) = endpoint.accept().await {
        let remote_addr = incoming.remote_address();
        let router = router.clone();
        let conn_close_signal = close_signal.as_ref().map(|rx| {
            let mut rx = rx.clone();
            rx.borrow_and_update();
            rx
        });
        tokio::spawn(async move {
            match handle_connection(incoming, router, conn_close_signal).await {
                Ok(()) => tracing::info!("HTTP/3 connection from {} closed", remote_addr),
                Err(e) => tracing::error!("HTTP/3 connection from {} failed: {}", remote_addr, e),
            }
        });
    }

    Ok(())
}

async fn handle_connection(
    incoming: quinn::Incoming,
    app: Router,
    mut close_signal: Option<CloseSignal>,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = incoming
        .await
        .map_err(|e| format!("TLS handshake failed: {e}"))?;
    let remote_addr = conn.remote_address();

    tracing::info!("HTTP/3 connection established from {}", remote_addr);

    // Build H3 connection (standard h3 + h3-quinn integration)
    // See: https://docs.rs/h3/latest/h3/server/struct.Builder.html
    // You can configure H3 protocol settings directly here:
    //   .max_field_section_size(8192) - header size limits
    //   .send_grease(true) - GREASE for compatibility testing
    //
    // `conn` is cloned so a copy survives independently of the one moved
    // into the h3 connection, for `close_all` to close later.
    let h3_conn = h3::server::builder()
        .build(h3_quinn::Connection::new(conn.clone()))
        .await?;

    tokio::pin!(h3_conn);

    // Accept H3 requests (standard h3 API)
    loop {
        // `changed()` resolves once when a new value has been sent since
        // this receiver last observed one.
        let closed = async {
            match close_signal.as_mut() {
                Some(rx) => {
                    if rx.changed().await.is_err() {
                        std::future::pending::<()>().await
                    }
                }
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            result = h3_conn.accept() => match result {
                Ok(Some(resolver)) => {
                    let app = app.clone();
                    tracing::debug!("Handling request");
                    tokio::spawn(async move {
                        if let Err(e) = handle_request(resolver, app).await {
                            tracing::error!("Request error: {}", e);
                        }
                    });
                }
                Ok(None) => {
                    tracing::info!("Connection closed by peer: {}", remote_addr);
                    break;
                }
                Err(e) => {
                    // h3-axum helper: distinguish graceful closes from errors
                    if h3_axum::is_graceful_h3_close(&e) {
                        tracing::debug!("Connection closed gracefully: {}", remote_addr);
                    } else {
                        tracing::error!("H3 connection error: {:?}", e);
                    }
                    break;
                }
            },
            _ = closed => {
                tracing::info!("closing HTTP/3 connection from {} on external signal", remote_addr);
                conn.close(0u32.into(), b"");
                break;
            }
        }
    }

    Ok(())
}

async fn handle_request(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    app: Router,
) -> Result<(), h3_axum::BoxError> {
    h3_axum::serve_h3_with_axum(app, resolver).await
}

#[cfg(test)]
mod tests {
    use super::{CloseSignal, close_signal, http3_serve};
    use crate::client::H3Client;
    use axum::Router;
    use axum::routing::get;
    use http::StatusCode;
    use std::net::{SocketAddr, UdpSocket};
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::time::Instant;

    fn reserve_port() -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("reserve udp port");
        let addr = socket.local_addr().expect("local addr");
        drop(socket);
        addr
    }

    fn cert_paths() -> (PathBuf, PathBuf, PathBuf) {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/certs");
        (
            base.join("server.crt"),
            base.join("server.key"),
            base.join("ca.crt"),
        )
    }

    fn test_router() -> Router {
        Router::new().route("/", get(|| async { "hi" }))
    }

    /// Spawns `http3_serve` on a freshly reserved port and gives the QUIC
    /// endpoint time to bind before returning.
    async fn spawn_server(close_signal: Option<CloseSignal>) -> SocketAddr {
        let addr = reserve_port();
        let (cert, key, _ca) = cert_paths();
        tokio::spawn(http3_serve(test_router(), addr, cert, key, close_signal));
        tokio::time::sleep(Duration::from_millis(100)).await;
        addr
    }

    /// Connects an `H3Client`, retrying while the freshly spawned server
    /// finishes binding its UDP socket.
    async fn connect(addr: SocketAddr) -> H3Client {
        let (_, _, ca) = cert_paths();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match H3Client::new("localhost", addr.port(), ca.clone(), None).await {
                Ok(client) => return client,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(e) => panic!("connect h3 client: {e:#}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn signal_closes_the_connection() {
        let (handle, signal) = close_signal();
        let addr = spawn_server(Some(signal)).await;
        let mut client = connect(addr).await;

        let resp = client.get("/").await.expect("first request should succeed");
        assert_eq!(resp.status, StatusCode::OK);

        handle.send(()).expect("send close signal");
        tokio::time::sleep(Duration::from_millis(200)).await;

        client
            .get("/")
            .await
            .expect_err("connection should be closed after the signal");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn none_behaves_like_no_signal_at_all() {
        let addr = spawn_server(None).await;
        let mut client = connect(addr).await;

        let resp = client
            .get("/")
            .await
            .expect("a request should succeed with no close signal configured");
        assert_eq!(resp.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_connections_are_unaffected_by_a_past_signal() {
        let (handle, signal) = close_signal();
        let addr = spawn_server(Some(signal)).await;

        let mut client1 = connect(addr).await;
        client1
            .get("/")
            .await
            .expect("first connection should work");

        handle.send(()).expect("send close signal");
        tokio::time::sleep(Duration::from_millis(200)).await;
        client1
            .get("/")
            .await
            .expect_err("the old connection should be closed");

        let mut client2 = connect(addr).await;
        let resp = client2
            .get("/")
            .await
            .expect("a fresh connection opened after the signal should still work");
        assert_eq!(resp.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_signal_closes_every_open_connection() {
        let (handle, signal) = close_signal();
        let addr = spawn_server(Some(signal)).await;

        let mut client_a = connect(addr).await;
        let mut client_b = connect(addr).await;
        client_a.get("/").await.expect("client a's first request");
        client_b.get("/").await.expect("client b's first request");

        handle.send(()).expect("send close signal");
        tokio::time::sleep(Duration::from_millis(200)).await;

        client_a
            .get("/")
            .await
            .expect_err("client a should be closed");
        client_b
            .get("/")
            .await
            .expect_err("client b should be closed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_signal_is_repeatable_across_multiple_fires() {
        let (handle, signal) = close_signal();
        let addr = spawn_server(Some(signal)).await;

        let mut client1 = connect(addr).await;
        client1
            .get("/")
            .await
            .expect("first connection should work");
        handle.send(()).expect("first close");
        tokio::time::sleep(Duration::from_millis(200)).await;
        client1
            .get("/")
            .await
            .expect_err("first connection should be closed");

        let mut client2 = connect(addr).await;
        client2
            .get("/")
            .await
            .expect("second connection should work");
        handle.send(()).expect("second close");
        tokio::time::sleep(Duration::from_millis(200)).await;
        client2
            .get("/")
            .await
            .expect_err("a later signal should also close a later connection");
    }
}
