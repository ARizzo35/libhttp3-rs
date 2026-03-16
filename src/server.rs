use anyhow::Result;
use axum::Router;
use bytes::Bytes;
use h3_quinn::quinn;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

pub async fn http3_serve(
    router: Router,
    addr: SocketAddr,
    certpath: PathBuf,
    keypath: PathBuf,
) -> Result<()> {
    // Install default crypto provider
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install crypto provider");

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
        .max_idle_timeout(Some(std::time::Duration::from_secs(60).try_into()?)); // Connection timeout

    // Bind and listen
    let endpoint = quinn::Endpoint::server(server_config, addr)?;
    while let Some(incoming) = endpoint.accept().await {
        let remote_addr = incoming.remote_address();
        let router = router.clone();
        tokio::spawn(async move {
            match handle_connection(incoming, router).await {
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
    let h3_conn = h3::server::builder()
        .build(h3_quinn::Connection::new(conn))
        .await?;

    tokio::pin!(h3_conn);

    // Accept H3 requests (standard h3 API)
    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                let app = app.clone();
                tracing::info!("Handling request");
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
