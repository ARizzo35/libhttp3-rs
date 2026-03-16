# libhttp3

A Rust library for HTTP/3 clients and servers, built on [Quinn](https://github.com/quinn-rs/quinn) (QUIC), [h3](https://github.com/hyperium/h3), and [Axum](https://github.com/tokio-rs/axum).

## Features

- **HTTP/3 Server** -- Serve an Axum `Router` over QUIC/HTTP3 with a single function call
- **HTTP/3 Client** -- Full-featured client with GET, POST, PUT, DELETE methods
- **SSE Streaming** -- Server-Sent Events over HTTP/3
- **Binary Streaming** -- Length-prefixed binary frame protocol over HTTP/3
- **TLS** -- Built-in rustls integration with certificate loading

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
libhttp3 = "0"
```

### Server

Use `http3_serve` to serve any Axum router over HTTP/3. You need a TLS certificate and key in PEM format.

```rust
use axum::{Router, routing::get};
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(|| async { "Hello over HTTP/3!" }));

    let addr: SocketAddr = "0.0.0.0:4433".parse()?;

    libhttp3::server::http3_serve(
        app,
        addr,
        "certs/server.crt".into(),
        "certs/server.key".into(),
    ).await
}
```

### Client

`H3Client` connects to an HTTP/3 server using a CA certificate for TLS verification.

```rust
use libhttp3::H3Client;
use bytes::Bytes;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut client = H3Client::new(
        "localhost",
        4433,
        "certs/ca.crt".into(),
        None, // optional TLS server name override
    ).await?;

    // GET request
    let resp = client.get("/").await?;
    println!("Status: {}", resp.status);
    println!("Body: {}", String::from_utf8_lossy(&resp.body));

    // POST with JSON body
    let body = Bytes::from(r#"{"key": "value"}"#);
    let resp = client.post("/data", Some(body)).await?;
    println!("POST status: {}", resp.status);

    Ok(())
}
```

### SSE Streaming

```rust
let mut stream = client.get_stream("/events").await?;

while let Some(event) = stream.next_event().await? {
    if let Some(ref event_type) = event.event_type {
        println!("Event type: {}", event_type);
    }
    println!("Data: {}", event.data);
}
```

### Binary Streaming

Uses a length-prefixed frame protocol (`[4-byte big-endian length][payload]`, max 16 MiB per frame).

```rust
let mut stream = client.get_binary_stream("/binary").await?;

while let Some(frame) = stream.next_frame().await? {
    println!("Received {} bytes", frame.len());
}
```

## Examples

See the [`examples/`](examples/) directory for complete, runnable examples:

- [`server.rs`](examples/server.rs) -- HTTP/3 server with multiple routes
- [`client.rs`](examples/client.rs) -- HTTP/3 client making requests

Run the server:

```sh
cargo run --example server
```

Then in another terminal:

```sh
cargo run --example client
```

## TLS Certificates

Both the server and client require TLS certificates. For local development, generate self-signed certs:

```sh
# Generate CA key and certificate
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
  -days 365 -nodes -keyout certs/ca.key -out certs/ca.crt \
  -subj "/CN=localhost CA"

# Generate server key and CSR
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
  -nodes -keyout certs/server.key -out certs/server.csr \
  -subj "/CN=localhost"

# Sign server cert with CA
openssl x509 -req -in certs/server.csr -CA certs/ca.crt -CAkey certs/ca.key \
  -CAcreateserial -out certs/server.crt -days 365 \
  -extfile <(printf "subjectAltName=DNS:localhost,IP:127.0.0.1")
```
