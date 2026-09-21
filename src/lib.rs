use std::time::Duration;

pub mod client;
pub mod server;

pub use client::{H3Client, H3Response, SseEvent, SseStream};

/// Idle timeout advertised by both ends. QUIC uses the lower of the two.
pub const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// PING interval for an otherwise-idle connection. Stays well under
/// [`MAX_IDLE_TIMEOUT`] so a lost PING is not fatal.
pub const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

#[cfg(test)]
mod tests {
    use super::{KEEP_ALIVE_INTERVAL, MAX_IDLE_TIMEOUT};

    #[test]
    fn several_pings_can_be_lost_before_the_connection_idles_out() {
        assert!(KEEP_ALIVE_INTERVAL * 3 < MAX_IDLE_TIMEOUT);
    }
}
