use axum::{
    Router,
    routing::{get, post},
};
use std::net::SocketAddr;

async fn index() -> &'static str {
    "Hello over HTTP/3!"
}

async fn echo(body: String) -> String {
    body
}

async fn health() -> &'static str {
    r#"{"status": "ok"}"#
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/echo", post(echo))
        .route("/health", get(health));

    let addr: SocketAddr = "0.0.0.0:4433".parse()?;
    println!("HTTP/3 server listening on {addr}");

    libhttp3::server::http3_serve(
        app,
        addr,
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/certs/server.crt").into(),
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/certs/server.key").into(),
    )
    .await
}
