//! Container healthcheck: `pair-api --healthcheck` probes local `/healthz` over raw TCP
//! so the runtime image needs no curl.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const OK_STATUS_LINE: &str = "HTTP/1.1 200";

/// True if `GET /healthz` on `addr` returns 200.
pub async fn probe(addr: &str) -> bool {
    let attempt = async {
        let mut stream = TcpStream::connect(addr).await.ok()?;
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .ok()?;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.ok()?;
        Some(String::from_utf8_lossy(&buf).starts_with(OK_STATUS_LINE))
    };
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, attempt).await,
        Ok(Some(true))
    )
}

/// Loopback address derived from the configured bind address (keeps its port).
pub fn local_addr(bind: &str) -> String {
    let port = bind.rsplit(':').next().unwrap_or("8080");
    format!("127.0.0.1:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_addr_keeps_port() {
        assert_eq!(local_addr("0.0.0.0:9090"), "127.0.0.1:9090");
        assert_eq!(local_addr("127.0.0.1:8080"), "127.0.0.1:8080");
    }

    #[tokio::test]
    async fn test_probe_unreachable_is_false() {
        assert!(!probe("127.0.0.1:1").await);
    }
}
