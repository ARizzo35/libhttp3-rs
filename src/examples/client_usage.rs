use anyhow::Result;
use bytes::Bytes;
use defvar::defvar;
use libhttp3::H3Client;
use tracing::info;

defvar! {
    TLS_CERT_PATH: String = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR not set")
        + "/../apid/certs/certificate.crt"
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Create a new HTTP/3 client
    let mut client =
        H3Client::new("localhost", 4433, TLS_CERT_PATH.to_string().into(), None).await?;

    info!("HTTP/3 client connected successfully!");

    // Example: GET request
    let endpoint = "/api/v1/vehicle/telemetry";
    match client.get(endpoint).await {
        Ok(response) => {
            info!("GET {} - Status: {}", endpoint, response.status);
            info!("Response body: {}", String::from_utf8_lossy(&response.body));
        }
        Err(e) => info!("GET request failed: {}", e),
    }

    // Example: POST request with JSON body
    let json_body = r#"{"armed": true, "mode": "Hold"}"#;
    let endpoint = "/api/v1/vehicle/system";
    match client.post(endpoint, Some(Bytes::from(json_body))).await {
        Ok(response) => {
            info!("POST {} - Status: {}", endpoint, response.status);
            info!("Response body: {}", String::from_utf8_lossy(&response.body));
        }
        Err(e) => info!("POST request failed: {}", e),
    }

    // Example: PUT request
    let json_body = r#"{
      "PlannedWaypointMission": {
        "ignore_water": false,
        "waypoints": [
          {
            "loc": {
              "lat": 30.2957,
              "lon": -97.5927,
              "alt_m": 0
            },
            "radius_m": 5,
            "speed_mode": "Cruise",
            "hold_time_s": null
          }
        ],
        "repeat_total": 0
      }
    }"#;
    let endpoint = "/api/v1/vehicle/mission";
    match client.put(endpoint, Some(Bytes::from(json_body))).await {
        Ok(response) => {
            info!("PUT {} - Status: {}", endpoint, response.status);
            info!("Response body: {}", String::from_utf8_lossy(&response.body));
        }
        Err(e) => info!("PUT request failed: {}", e),
    }

    // Example: DELETE request
    let endpoint = "/api/v1/vehicle/mission";
    match client.get(endpoint).await {
        Ok(response) => {
            info!("DELETE {} - Status: {}", endpoint, response.status);
            info!("Response body: {}", String::from_utf8_lossy(&response.body));
        }
        Err(e) => info!("DELETE request failed: {}", e),
    }

    // Example: SSE stream
    let endpoint = "/api/v2/vehicle/test/stream";
    match client.get_stream(endpoint).await {
        Ok(mut stream) => {
            info!("SSE stream started successfully!");

            // Process events from the stream
            let mut event_count = 0;
            while let Ok(Some(event)) = stream.next_event().await {
                info!("SSE Event received: {:?}", event);
                event_count += 1;

                // Stop after 10 events for demo purposes
                if event_count >= 10 {
                    break;
                }
            }

            info!("SSE stream ended after {} events", event_count);
        }
        Err(e) => info!("SSE stream failed: {}", e),
    }

    Ok(())
}
