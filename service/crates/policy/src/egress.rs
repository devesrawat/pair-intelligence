use crate::config::EgressRule;
use pair_core::types::DataClass;

/// A parsed destination. `scheme` is `None` for a bare `host` or `host:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub scheme: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

const SCHEME_FALLBACK: &str = "https";

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        "ssh" => Some(22),
        _ => None,
    }
}

/// Parses `host`, `host:port` or a URL into scheme, lowercase host and port. Anything
/// ambiguous is an error.
pub fn parse_destination(input: &str) -> Result<Destination, String> {
    if input.is_empty()
        || input
            .chars()
            .any(|c| c.is_whitespace() || c == '\\' || c.is_control())
    {
        return Err(format!("unparseable destination {input:?}"));
    }
    let (scheme, rest) = match input.split_once("://") {
        Some((scheme, rest))
            if !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) =>
        {
            (Some(scheme.to_ascii_lowercase()), rest)
        }
        Some(_) => return Err(format!("unparseable destination {input:?}")),
        None => (None, input),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (host_port, None),
    };
    let port = match port {
        None => None,
        Some(p) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => Some(
            p.parse::<u16>()
                .map_err(|_| format!("invalid port in {input:?}"))?,
        ),
        Some(_) => return Err(format!("invalid port in {input:?}")),
    };
    let host = host.to_ascii_lowercase();
    let valid = !host.is_empty()
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
        && !host.contains("..")
        && host
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-');
    if valid {
        Ok(Destination { scheme, host, port })
    } else {
        Err(format!("invalid host in {input:?}"))
    }
}

/// Extracts the lowercase host from `host`, `host:port` or a URL.
pub fn parse_host(input: &str) -> Result<String, String> {
    parse_destination(input).map(|d| d.host)
}

/// Default deny: a destination passes only if a rule names its host, data class, scheme and port.
pub fn check_destination(
    rules: &[EgressRule],
    destination: &str,
    data_class: DataClass,
) -> Result<(), String> {
    let dest = parse_destination(destination)?;
    let host = &dest.host;
    let rule = rules.iter().find(|r| match r.host.strip_prefix("*.") {
        Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(&format!(".{suffix}")),
        None => &r.host == host,
    });
    let Some(rule) = rule else {
        return Err(format!("egress to {host} is not on the allowlist"));
    };
    if !rule.data_classes.contains(&data_class) {
        return Err(format!(
            "egress to {host} is not approved for data class {data_class:?}"
        ));
    }
    let scheme = dest.scheme.as_deref().unwrap_or(SCHEME_FALLBACK);
    if !rule.schemes.iter().any(|s| s == scheme) {
        return Err(format!("egress to {host} over {scheme} is not allowed"));
    }
    let default = default_port(scheme);
    let port = dest.port.or(default);
    let port_ok = match port {
        None => false,
        Some(p) if rule.ports.is_empty() => Some(p) == default,
        Some(p) => rule.ports.contains(&p),
    };
    if port_ok {
        Ok(())
    } else {
        Err(format!("egress to {host} on port {port:?} is not allowed"))
    }
}
