use bytes::Bytes;
use libhttp3::H3Client;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Connect to the HTTP/3 server
    let mut client = H3Client::new(
        "localhost",
        4433,
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/certs/ca.crt").into(),
        None,
    )
    .await?;

    println!("Connected to HTTP/3 server\n");

    // GET /
    let resp = client.get("/").await?;
    println!("GET / -> {} {}", resp.status, String::from_utf8_lossy(&resp.body));

    // GET /health
    let resp = client.get("/health").await?;
    println!("GET /health -> {} {}", resp.status, String::from_utf8_lossy(&resp.body));

    // POST /echo
    let resp = client.post("/echo", Some(Bytes::from("ping"))).await?;
    println!("POST /echo -> {} {}", resp.status, String::from_utf8_lossy(&resp.body));

    println!("\nAll requests completed successfully!");
    Ok(())
}
