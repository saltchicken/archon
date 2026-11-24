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

pub async fn validate_public_url(url: &Url, allow_private: bool) -> Result<Vec<SocketAddr>> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!(
            "Security violation: unsupported URL scheme '{}'. Only 'http' and 'https' are allowed.",
            url.scheme()
        );
    }
    let Some(host_str) = url.host_str() else {
        bail!("Invalid URL: missing host");
    };

    let port = url.port_or_known_default().unwrap_or(443);

    if let Ok(ip) = host_str.trim_matches(['[', ']']).parse::<IpAddr>() {
        if !allow_private && is_forbidden_ip(ip) {
            bail!("Security violation: access to private or reserved IP '{ip}' is blocked");
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    if !allow_private {
        let lower = host_str.trim_end_matches('.').to_ascii_lowercase();
        if lower == "localhost"
            || [".localhost", ".local", ".internal", ".home.arpa"]
                .iter()
                .any(|s| lower.ends_with(s))
        {
            bail!("Security violation: access to local/internal host '{host_str}' is blocked");
        }
    }

    let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host_str, port))
        .await
        .with_context(|| format!("Failed to resolve host '{host_str}'"))?
        .collect();

    if resolved.is_empty() {
        bail!("DNS resolution returned no addresses for '{host_str}'");
    }
    if !allow_private && let Some(addr) = resolved.iter().find(|a| is_forbidden_ip(a.ip())) {
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

fn decode_entity_token(entity: &str) -> Option<char> {
    let code = if let Some(hex) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        entity.strip_prefix('#')?.parse::<u32>().ok()?
    };

    match char::from_u32(code)? {
        '\0' => None,
        '\u{00A0}' => Some(' '),
        ch => Some(ch),
    }
}

fn decode_named_entity(entity: &str) -> Option<&'static str> {
    match entity {
        "nbsp" => Some(" "),
        "amp" => Some("\x26"),
        "lt" => Some("<"),
        "gt" => Some(">"),
        "quot" => Some("\""),
        "apos" => Some("'"),
        "mdash" => Some("—"),
        "ndash" => Some("–"),
        "hellip" => Some("…"),
        "rsquo" => Some("’"),
        "lsquo" => Some("‘"),
        "rdquo" => Some("”"),
        "ldquo" => Some("“"),
        "copy" => Some("©"),
        "reg" => Some("®"),
        "trade" => Some("™"),
        "bull" => Some("•"),
        "rarr" => Some("->"),
        "larr" => Some("<-"),
        "times" => Some("×"),
        "minus" => Some("−"),
        _ => None,
    }
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('\x26') {
        return s.to_string();
    }

    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(amp_idx) = rest.find('\x26') {
        out.push_str(&rest[..amp_idx]);
        let after_amp = &rest[amp_idx + 1..];
        let window = &after_amp[..after_amp.floor_char_boundary(12)];

        if let Some(semi_idx) = window.find(';') {
            let token = &window[..semi_idx];
            if let Some(replacement) = decode_named_entity(token)
                .or_else(|| decode_named_entity(&token.to_ascii_lowercase()))
            {
                out.push_str(replacement);
                rest = &after_amp[semi_idx + 1..];
                continue;
            }
            if let Some(ch) = decode_entity_token(token) {
                out.push(ch);
                rest = &after_amp[semi_idx + 1..];
                continue;
            }
        }

        out.push('\x26');
        rest = after_amp;
    }

    out.push_str(rest);
    out
}
