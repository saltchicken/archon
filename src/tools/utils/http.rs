use anyhow::{Context, Result, bail};
use reqwest::{Client, Url, redirect::Policy};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

const FETCH_TIMEOUT_SECS: u64 = 20;

fn is_forbidden_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (o[0] == 100 && (o[1] & 0b1100_0000) == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && matches!(o[1], 18 | 19))
        || o[0] >= 240
}

fn is_forbidden_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_forbidden_ipv4(v4);
    }
    let s = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] == 0x2001 && s[1] == 0x0db8)
}

fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_ipv4(v4),
        IpAddr::V6(v6) => is_forbidden_ipv6(v6),
    }
}

pub async fn validate_public_url(url: &Url) -> Result<Vec<SocketAddr>> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!(
            "Security violation: unsupported URL scheme '{}'. Only 'http' and 'https' are allowed.",
            url.scheme()
        );
    }
    let Some(host_str) = url.host_str() else {
        bail!("Invalid URL: missing host");
    };

    let lower = host_str.trim_end_matches('.').to_ascii_lowercase();
    if lower == "localhost"
        || [".localhost", ".local", ".internal", ".home.arpa"]
            .iter()
            .any(|s| lower.ends_with(s))
    {
        bail!("Security violation: access to local/internal host '{host_str}' is blocked");
    }

    let port = url.port_or_known_default().unwrap_or(443);
    if let Ok(ip) = host_str.trim_matches(['[', ']']).parse::<IpAddr>() {
        if is_forbidden_ip(ip) {
            bail!("Security violation: access to private or reserved IP '{ip}' is blocked");
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host_str, port))
        .await
        .with_context(|| format!("Failed to resolve host '{host_str}'"))?
        .collect();

    if resolved.is_empty() {
        bail!("DNS resolution returned no addresses for '{host_str}'");
    }
    if let Some(addr) = resolved.iter().find(|a| is_forbidden_ip(a.ip())) {
        bail!(
            "Security violation: host '{host_str}' resolved to private/reserved IP '{}'",
            addr.ip()
        );
    }

    Ok(resolved)
}

pub fn build_pinned_client(url: &Url, addrs: &[SocketAddr]) -> Result<Client> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .redirect(Policy::none())
        .user_agent("archon/0.3 (+https://github.com/archon)");

    if let Some(host) = url
        .host_str()
        .filter(|h| h.trim_matches(['[', ']']).parse::<IpAddr>().is_err())
    {
        builder = builder.resolve_to_addrs(host, addrs);
    }
    Ok(builder.build()?)
}

pub fn decode_entities(s: &str) -> String {
    // Using `\x26` for `&` prevents browsers/clipboards from decoding HTML entities when copying code
    const ENTITIES: &[(&str, &str)] = &[
        ("\x26nbsp;", " "),
        ("\x26#160;", " "),
        ("\x26amp;", "\x26"),
        ("\x26lt;", "<"),
        ("\x26gt;", ">"),
        ("\x26quot;", "\""),
        ("\x26#39;", "'"),
        ("\x26#x27;", "'"),
        ("\x26#x2F;", "/"),
        ("\x26apos;", "'"),
        ("\x26mdash;", "—"),
        ("\x26ndash;", "–"),
        ("\x26rarr;", "->"),
        ("\x26larr;", "<-"),
    ];
    ENTITIES
        .iter()
        .fold(s.to_string(), |acc, (k, v)| acc.replace(k, v))
}
