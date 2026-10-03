//! Endpoint guard: PAIR is cloud-only. Local, private and local-inference endpoints are rejected.
use async_trait::async_trait;
use pair_core::error::{ErrorCode, PairError, Result};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
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
        let host = url
            .host()
            .ok_or_else(|| disallowed("endpoint has no host"))?;
        check_host(&host)?;
        if let Some(port) = url.port() {
            if LOCAL_INFERENCE_PORTS.contains(&port) {
                return Err(disallowed(format!("port {port} is a local-inference port")));
            }
        }
        Ok(Self {
            base: url.as_str().trim_end_matches('/').to_owned(),
        })
    }

    /// TEST-ONLY escape hatch for mock HTTP fixtures bound to 127.0.0.1.
    /// Must never be reachable from configuration loading.
    pub fn unchecked_for_tests(raw: &str) -> Self {
        Self {
            base: raw.trim_end_matches('/').to_owned(),
        }
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

/// Single source of truth for the forbidden address ranges (static URL check and connect-time check).
pub fn check_ip(ip: IpAddr) -> Result<()> {
    match ip {
        IpAddr::V4(v4) => check_v4(v4),
        IpAddr::V6(v6) => check_v6(v6),
    }
}

fn check_v4(ip: Ipv4Addr) -> Result<()> {
    let [a, b, ..] = ip.octets();
    let this_network = a == 0;
    let cgnat = a == 100 && (64..128).contains(&b);
    let benchmarking = a == 198 && (18..20).contains(&b);
    let reserved_or_multicast = a >= 224;
    if ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || this_network
        || cgnat
        || benchmarking
        || reserved_or_multicast
    {
        return Err(disallowed(format!("address {ip} is local or private")));
    }
    Ok(())
}

/// NAT64 well-known prefix 64:ff9b::/96 embeds an IPv4 address in the low 32 bits.
fn nat64_embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let is_nat64 = s[..6] == [0x64, 0xff9b, 0, 0, 0, 0];
    is_nat64.then(|| Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8))
}

fn check_v6(ip: Ipv6Addr) -> Result<()> {
    if let Some(v4) = ip.to_ipv4_mapped().or_else(|| nat64_embedded_v4(ip)) {
        return check_v4(v4);
    }
    let seg0 = ip.segments()[0];
    let unique_local = seg0 & 0xfe00 == 0xfc00;
    let link_local = seg0 & 0xffc0 == 0xfe80;
    let site_local = seg0 & 0xffc0 == 0xfec0;
    let ipv4_compatible = ip.segments()[..6] == [0; 6];
    if ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || unique_local
        || link_local
        || site_local
        || ipv4_compatible
    {
        return Err(disallowed(format!("address {ip} is local or private")));
    }
    Ok(())
}

/// Host name -> IP addresses. Injectable so DNS-rebinding behaviour is testable offline.
#[async_trait]
pub trait HostLookup: Send + Sync {
    async fn lookup(&self, host: &str) -> std::io::Result<Vec<IpAddr>>;
}

/// System resolver via tokio.
#[derive(Debug, Default)]
pub struct SystemLookup;

#[async_trait]
impl HostLookup for SystemLookup {
    async fn lookup(&self, host: &str) -> std::io::Result<Vec<IpAddr>> {
        let addrs = tokio::net::lookup_host((host, 0)).await?;
        Ok(addrs.map(|a| a.ip()).collect())
    }
}

/// DNS resolver that rejects the whole answer set if ANY address is non-public.
/// Installed on every provider HTTP client so the check applies at connect time,
/// defeating DNS rebinding and hostile records for otherwise public-looking names.
#[derive(Clone)]
pub struct GuardedResolver {
    inner: Arc<dyn HostLookup>,
}

impl GuardedResolver {
    pub fn new(inner: Arc<dyn HostLookup>) -> Self {
        Self { inner }
    }

    pub fn system() -> Self {
        Self::new(Arc::new(SystemLookup))
    }

    /// Resolve and vet; any private/local answer rejects the host.
    pub async fn resolve_checked(&self, host: &str) -> Result<Vec<SocketAddr>> {
        let ips = self.inner.lookup(host).await.map_err(|e| {
            PairError::new(
                ErrorCode::ProviderUnavailable,
                format!("dns lookup for {host} failed: {e}"),
            )
        })?;
        if ips.is_empty() {
            return Err(PairError::new(
                ErrorCode::ProviderUnavailable,
                format!("dns lookup for {host} returned no addresses"),
            ));
        }
        for ip in &ips {
            check_ip(*ip).map_err(|e| {
                disallowed(format!(
                    "host {host} resolves to non-public address: {}",
                    e.message
                ))
            })?;
        }
        Ok(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)).collect())
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let this = self.clone();
        Box::pin(async move {
            let addrs = this.resolve_checked(name.as_str()).await?;
            let iter: Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

/// HTTP client for provider calls: redirects disabled, connect-time DNS guard installed.
pub fn guarded_client(resolver: GuardedResolver) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(resolver))
        .build()
        .map_err(|e| PairError::new(ErrorCode::Internal, format!("http client: {e}")))
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

    struct StubLookup(Vec<IpAddr>);

    #[async_trait]
    impl HostLookup for StubLookup {
        async fn lookup(&self, _host: &str) -> std::io::Result<Vec<IpAddr>> {
            Ok(self.0.clone())
        }
    }

    fn resolver(ips: &[&str]) -> GuardedResolver {
        let ips = ips.iter().map(|s| s.parse().expect("ip literal")).collect();
        GuardedResolver::new(Arc::new(StubLookup(ips)))
    }

    #[tokio::test]
    async fn hostname_resolving_to_private_ip_rejected() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.0.1",
        ] {
            let err = resolver(&[ip])
                .resolve_checked("api.example.com")
                .await
                .expect_err(ip);
            assert_eq!(err.code, ErrorCode::ProviderDisallowed, "{ip}");
        }
    }

    #[tokio::test]
    async fn hostname_resolving_to_public_ip_allowed() {
        let addrs = resolver(&["160.79.104.10", "2606:4700::1111"])
            .resolve_checked("api.example.com")
            .await
            .expect("public ok");
        assert_eq!(addrs.len(), 2);
    }

    #[tokio::test]
    async fn mixed_answers_with_any_private_ip_rejected() {
        let err = resolver(&["160.79.104.10", "10.0.0.7"])
            .resolve_checked("api.example.com")
            .await
            .expect_err("mixed");
        assert_eq!(err.code, ErrorCode::ProviderDisallowed);
    }

    #[tokio::test]
    async fn client_refuses_to_connect_when_dns_answers_private() {
        let client = guarded_client(resolver(&["127.0.0.1"])).expect("client");
        let err = client
            .get("http://rebind.example.com:9/")
            .send()
            .await
            .expect_err("must not connect");
        assert!(err.is_connect(), "{err}");
    }

    #[tokio::test]
    async fn empty_answer_rejected() {
        let err = resolver(&[])
            .resolve_checked("api.example.com")
            .await
            .expect_err("empty");
        assert_eq!(err.code, ErrorCode::ProviderUnavailable);
    }

    #[test]
    fn public_cloud_endpoints_accepted() {
        let ep = Endpoint::parse("https://api.anthropic.com/").expect("ok");
        assert_eq!(
            ep.url("/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
        assert!(Endpoint::parse("https://ollama.com").is_ok());
    }

    fn assert_forbidden(ip: &str) {
        let parsed: IpAddr = ip.parse().expect("ip literal");
        let err = check_ip(parsed).expect_err(ip);
        assert_eq!(err.code, ErrorCode::ProviderDisallowed, "{ip}");
    }

    #[test]
    fn nat64_embedded_private_v4_rejected() {
        assert_forbidden("64:ff9b::a00:1");
        assert_forbidden("64:ff9b::7f00:1");
        assert!(check_ip("64:ff9b::808:808".parse().expect("ip")).is_ok());
    }

    #[test]
    fn ipv4_compatible_v6_rejected() {
        assert_forbidden("::7f00:1");
        assert_forbidden("::a00:1");
        assert_forbidden("::808:808");
    }

    #[test]
    fn site_local_fec0_rejected() {
        assert_forbidden("fec0::1");
        assert_forbidden("feff::1");
    }

    #[test]
    fn multicast_rejected() {
        assert_forbidden("ff02::1");
        assert_forbidden("ff00::1");
        assert_forbidden("224.0.0.1");
    }

    #[test]
    fn zero_slash_8_rejected() {
        assert_forbidden("0.1.2.3");
    }

    #[test]
    fn benchmarking_198_18_rejected() {
        assert_forbidden("198.18.0.1");
        assert_forbidden("198.19.255.255");
        assert!(check_ip("198.20.0.1".parse().expect("ip")).is_ok());
    }

    #[test]
    fn reserved_240_slash_4_rejected() {
        assert_forbidden("240.0.0.1");
        assert_forbidden("255.255.255.254");
    }
}
