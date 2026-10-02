use crate::config::EgressRule;
use pair_core::types::DataClass;

/// Extracts the lowercase host from `host`, `host:port` or a URL. Anything ambiguous is an error.
pub fn parse_host(input: &str) -> Result<String, String> {
    if input.is_empty()
        || input
            .chars()
            .any(|c| c.is_whitespace() || c == '\\' || c.is_control())
    {
        return Err(format!("unparseable destination {input:?}"));
    }
    let rest = match input.split_once("://") {
        Some((scheme, rest))
            if !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) =>
        {
            rest
        }
        Some(_) => return Err(format!("unparseable destination {input:?}")),
        None => input,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (host_port, None),
    };
    if port.is_some_and(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
        return Err(format!("invalid port in {input:?}"));
    }
    let host = host.to_ascii_lowercase();
    let valid = !host.is_empty()
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
        && !host.contains("..")
        && host
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-');
    if valid {
        Ok(host)
    } else {
        Err(format!("invalid host in {input:?}"))
    }
}

/// Default deny: a destination passes only if a rule names its host and the data class.
pub fn check_destination(
    rules: &[EgressRule],
    destination: &str,
    data_class: DataClass,
) -> Result<(), String> {
    let host = parse_host(destination)?;
    let rule = rules.iter().find(|r| match r.host.strip_prefix("*.") {
        Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(&format!(".{suffix}")),
        None => r.host == host,
    });
    match rule {
        None => Err(format!("egress to {host} is not on the allowlist")),
        Some(r) if !r.data_classes.contains(&data_class) => Err(format!(
            "egress to {host} is not approved for data class {data_class:?}"
        )),
        Some(_) => Ok(()),
    }
}
