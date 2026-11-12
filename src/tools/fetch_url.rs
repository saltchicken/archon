use crate::tools::{auto_schema, parse_args};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::{redirect::Policy, Client, Url};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

const FETCH_TIMEOUT_SECS: u64 = 20;
const MAX_REDIRECTS: usize = 5;

#[derive(Deserialize, JsonSchema)]
struct FetchUrlArgs {
    /// The HTTP or HTTPS URL to fetch (must resolve to a public IP address).
    url: String,
    /// If true, returns raw HTML source instead of stripping HTML tags and scripts into readable text.
    #[serde(default)]
    raw: bool,
}

pub fn schema() -> Value {
    auto_schema::<FetchUrlArgs>(
        "fetch_url",
        "Fetches content from a public HTTP/HTTPS URL. Automatically converts HTML pages into readable text and pretty-prints JSON unless `raw: true` is set.",
    )
}

fn is_forbidden_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Carrier-Grade NAT (100.64.0.0/10)
        || (octets[0] == 100 && (octets[1] & 0b1100_0000) == 64)
        // IETF protocol assignments & benchmarking (192.0.0.0/24, 198.18.0.0/15)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        // Reserved for future use (240.0.0.0/4)
        || octets[0] >= 240
}

fn is_forbidden_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_forbidden_ipv4(v4);
    }
    let segs = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Unique Local Addresses (fc00::/7)
        || (segs[0] & 0xfe00) == 0xfc00
        // Link-Local Unicast (fe80::/10)
        || (segs[0] & 0xffc0) == 0xfe80
        // Documentation (2001:db8::/32)
        || (segs[0] == 0x2001 && segs[1] == 0x0db8)
}

fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_ipv4(v4),
        IpAddr::V6(v6) => is_forbidden_ipv6(v6),
    }
}

async fn validate_public_url(url: &Url) -> Result<Vec<SocketAddr>> {
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
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
        || lower.ends_with(".home.arpa")
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

    for addr in &resolved {
        if is_forbidden_ip(addr.ip()) {
            bail!(
                "Security violation: host '{host_str}' resolved to private/reserved IP '{}'",
                addr.ip()
            );
        }
    }

    Ok(resolved)
}

fn build_pinned_client(url: &Url, addrs: &[SocketAddr]) -> Result<Client> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .redirect(Policy::none())
        .user_agent("archon/0.3 (+https://github.com/archon)");

    if let Some(host) = url.host_str()
        && host.trim_matches(['[', ']']).parse::<IpAddr>().is_err()
    {
        builder = builder.resolve_to_addrs(host, addrs);
    }

    Ok(builder.build()?)
}

fn remove_tag_blocks(mut html: String, tag: &str) -> String {
    let open_pat = format!("<{tag}");
    let close_pat = format!("</{tag}>");

    loop {
        let lower = html.to_ascii_lowercase();
        let Some(start) = lower.find(&open_pat) else {
            break;
        };
        // Ensure the character immediately after `<tag` is whitespace, `>`, or `/`
        let after_open = html[start + open_pat.len()..].chars().next();
        if !matches!(after_open, Some(c) if c.is_ascii_whitespace() || c == '>' || c == '/') {
            break;
        }

        if let Some(rel_end) = lower[start..].find(&close_pat) {
            let end = start + rel_end + close_pat.len();
            html.replace_range(start..end, " ");
        } else {
            html.truncate(start);
            break;
        }
    }
    html
}

fn remove_html_comments(mut html: String) -> String {
    while let Some(start) = html.find("<!--") {
        if let Some(rel_end) = html[start + 4..].find("-->") {
            let end = start + 4 + rel_end + 3;
            html.replace_range(start..end, " ");
        } else {
            html.truncate(start);
            break;
        }
    }
    html
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&rarr;", "->")
        .replace("&larr;", "<-")
}

fn html_to_text(raw_html: &str) -> String {
    let mut cleaned = remove_html_comments(raw_html.to_string());
    for tag in ["script", "style", "svg", "noscript", "head"] {
        cleaned = remove_tag_blocks(cleaned, tag);
    }

    let mut out = String::with_capacity(cleaned.len() / 2);
    let mut in_tag = false;
    let mut tag_name = String::new();
    let mut collecting_tag_name = false;

    for ch in cleaned.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let t = tag_name.trim_start_matches('/').to_ascii_lowercase();
                if matches!(
                    t.as_str(),
                    "p" | "div"
                        | "br"
                        | "hr"
                        | "tr"
                        | "li"
                        | "ul"
                        | "ol"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "pre"
                        | "blockquote"
                        | "section"
                        | "article"
                        | "header"
                        | "footer"
                        | "main"
                        | "table"
                ) {
                    out.push('\n');
                    if t == "li" && !tag_name.starts_with('/') {
                        out.push_str("• ");
                    }
                } else if matches!(t.as_str(), "td" | "th") {
                    out.push_str(" | ");
                }
                tag_name.clear();
            } else if collecting_tag_name {
                if ch.is_ascii_whitespace() {
                    collecting_tag_name = false;
                } else if tag_name.len() < 16 {
                    tag_name.push(ch);
                }
            }
        } else if ch == '<' {
            in_tag = true;
            collecting_tag_name = true;
            tag_name.clear();
        } else {
            out.push(ch);
        }
    }

    let decoded = decode_entities(&out);
    let mut lines_out = Vec::new();
    let mut prev_empty = false;

    for raw_line in decoded.lines() {
        let collapsed: String = raw_line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            if !prev_empty && !lines_out.is_empty() {
                lines_out.push(String::new());
            }
            prev_empty = true;
        } else {
            lines_out.push(collapsed);
            prev_empty = false;
        }
    }

    lines_out.join("\n").trim().to_string()
}

pub async fn execute(json_args: &Value, max_output_size: usize) -> Result<String> {
    let args: FetchUrlArgs = parse_args(json_args, "fetch_url")?;
    let mut current_url =
        Url::parse(args.url.trim()).with_context(|| format!("Invalid URL: '{}'", args.url.trim()))?;

    let mut resp = None;
    for hop in 0..=MAX_REDIRECTS {
        let addrs = validate_public_url(&current_url).await?;
        let client = build_pinned_client(&current_url, &addrs)?;

        let r = client
            .get(current_url.clone())
            .header(
                "Accept",
                "text/markdown, text/plain, application/json, text/html;q=0.9, */*;q=0.5",
            )
            .send()
            .await
            .with_context(|| format!("HTTP request failed for '{current_url}'"))?;

        if r.status().is_redirection() {
            if hop == MAX_REDIRECTS {
                bail!("Exceeded maximum redirect limit ({MAX_REDIRECTS})");
            }
            let Some(loc) = r.headers().get(reqwest::header::LOCATION) else {
                bail!("Redirect response (HTTP {}) missing Location header", r.status());
            };
            let loc_str = loc
                .to_str()
                .context("Invalid non-ASCII Location redirect header")?;
            current_url = current_url
                .join(loc_str)
                .with_context(|| format!("Failed to resolve redirect URL '{loc_str}'"))?;
            continue;
        }

        resp = Some(r);
        break;
    }

    let resp = resp.context("Failed to obtain HTTP response")?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_ascii_lowercase();

    if content_type.starts_with("image/")
        || content_type.starts_with("audio/")
        || content_type.starts_with("video/")
        || content_type == "application/octet-stream"
        || content_type == "application/pdf"
        || content_type == "application/zip"
    {
        bail!("Refusing to fetch binary content-type '{content_type}' from {current_url}");
    }

    // Allow reading up to 4x max_output_size in raw HTML before stripping tags down to text
    let raw_byte_cap = if !args.raw && content_type.contains("html") {
        max_output_size.saturating_mul(4).max(262_144)
    } else {
        max_output_size.saturating_add(1)
    };

    let mut body_bytes = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk_res) = stream.next().await {
        let chunk = chunk_res.context("Error reading HTTP response stream")?;
        let remaining = raw_byte_cap.saturating_sub(body_bytes.len());
        if remaining == 0 {
            break;
        }
        if chunk.len() > remaining {
            body_bytes.extend_from_slice(&chunk[..remaining]);
            break;
        }
        body_bytes.extend_from_slice(&chunk);
    }

    let raw_text = String::from_utf8_lossy(&body_bytes).into_owned();
    let formatted_body = if !args.raw && content_type.contains("html") {
        html_to_text(&raw_text)
    } else if !args.raw && content_type.contains("json") {
        serde_json::from_str::<Value>(&raw_text)
            .and_then(|v| serde_json::to_string_pretty(&v))
            .unwrap_or(raw_text)
    } else {
        raw_text
    };

    if !status.is_success() {
        bail!(
            "HTTP {status} from {current_url}:\n{}",
            formatted_body.trim()
        );
    }

    Ok(format!(
        "URL: {current_url}\nStatus: {status}\nContent-Type: {content_type}\n\n{}",
        formatted_body.trim()
    ))
}
