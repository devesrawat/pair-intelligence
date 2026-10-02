//! Endpoint guard: PAIR is cloud-only. Local, private and local-inference endpoints are rejected.
use pair_core::error::{ErrorCode, PairError, Result};
use std::net::{Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

/// Default port of a local Ollama daemon; never a valid cloud endpoint.
pub const LOCAL_INFERENCE_PORTS: &[u16] = &[11434];

/// A validated provider base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    base: String,
}

impl Endpoint {
    /// Validate a production endpoint: https only, public host, not a local-inference port.
    pub fn parse(raw: &str) -> Result<Self> {
        let url = Url::parse(raw).map_err(|e| disallowed(format!("invalid endpoint url: {e}")))?;
        if url.scheme() != "https" {
            return Err(disallowed("endpoint must use https"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(disallowed("endpoint must not embed credentials"));
        }
        let host = url.host().ok_or_else(|| disallowed("endpoint has no host"))?;
        check_host(&host)?;
        if let Some(port) = url.port() {
            if LOCAL_INFERENCE_PORTS.contains(&port) {
                return Err(disallowed(format!("port {port} is a local-inference port")));
            }
        }
        Ok(Self { base: url.as_str().trim_end_matches('/').to_owned() })
    }

    /// TEST-ONLY escape hatch for mock HTTP fixtures bound to 127.0.0.1.
    /// Must never be reachable from configuration loading.
    pub fn unchecked_for_tests(raw: &str) -> Self {
        Self { base: raw.trim_end_matches('/').to_owned() }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }
}

fn disallowed(msg: impl Into<String>) -> PairError {
    PairError::new(ErrorCode::ProviderDisallowed, msg)
}

fn check_host(host: &Host<&str>) -> Result<()> {
    match host {
        Host::Domain(d) => check_domain(d),
        Host::Ipv4(ip) => check_v4(*ip),
        Host::Ipv6(ip) => check_v6(*ip),
    }
}

fn check_domain(d: &str) -> Result<()> {
    let d = d.trim_end_matches('.').to_ascii_lowercase();
    let local = d == "localhost"
        || d.ends_with(".localhost")
        || d.ends_with(".local")
        || d.ends_with(".internal")
        || d.ends_with(".lan")
        || !d.contains('.');
    if local {
        return Err(disallowed(format!("host {d} is local or non-public")));
    }
    Ok(())
}

fn check_v4(ip: Ipv4Addr) -> Result<()> {
    let [a, b, ..] = ip.octets();
    let cgnat = a == 100 && (64..128).contains(&b);
    if ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast() || cgnat
    {
        return Err(disallowed(format!("address {ip} is local or private")));
    }
    Ok(())
}

fn check_v6(ip: Ipv6Addr) -> Result<()> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return check_v4(v4);
    }
    let seg0 = ip.segments()[0];
    let unique_local = seg0 & 0xfe00 == 0xfc00;
    let link_local = seg0 & 0xffc0 == 0xfe80;
    if ip.is_loopback() || ip.is_unspecified() || unique_local || link_local {
        return Err(disallowed(format!("address {ip} is local or private")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disallowed_local_endpoint_rejected() {
        let bad = [
            "https://localhost",
            "https://api.localhost/x",
            "https://127.0.0.1",
            "https://127.9.9.9:8443",
            "https://[::1]",
            "https://10.0.0.5",
            "https://172.16.3.1",
            "https://192.168.1.10",
            "https://169.254.1.1",
            "https://0x7f.1",
            "https://2130706433",
            "https://[fd00::1]",
            "https://[::ffff:127.0.0.1]",
            "https://mac.local",
            "https://ollama",
            "https://example.com:11434",
            "http://api.anthropic.com",
            "https://user:pw@api.anthropic.com",
        ];
        for raw in bad {
            let err = Endpoint::parse(raw).expect_err(raw);
            assert_eq!(err.code, ErrorCode::ProviderDisallowed, "{raw}");
        }
    }

    #[test]
    fn public_cloud_endpoints_accepted() {
        let ep = Endpoint::parse("https://api.anthropic.com/").expect("ok");
        assert_eq!(ep.url("/v1/messages"), "https://api.anthropic.com/v1/messages");
        assert!(Endpoint::parse("https://ollama.com").is_ok());
    }
}
