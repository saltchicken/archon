use super::{
    AsyncTool, ToolContext,
    utils::http::{build_pinned_client, decode_entities, validate_public_url},
};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Url;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const MAX_REDIRECTS: usize = 5;

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
pub struct FetchUrlArgs {
    /// The HTTP or HTTPS URL to fetch (must resolve to a public IP address).
    url: String,
    /// If true, returns raw HTML source instead of stripping HTML tags and scripts into readable text.
    #[serde(default)]
    raw: bool,
}

fn remove_tag_blocks(mut html: String, tag: &str) -> String {
    let open_pat = format!("<{tag}");
    let close_pat = format!("</{tag}>");

    loop {
        let lower = html.to_ascii_lowercase();
        let Some(start) = lower.find(&open_pat) else {
            break;
        };
        let after_open = html[start + open_pat.len()..].chars().next();
        if !matches!(after_open, Some(c) if c.is_ascii_whitespace() || c == '>' || c == '/') {
            break;
        }
        if let Some(rel_end) = lower[start..].find(&close_pat) {
            html.replace_range(start..start + rel_end + close_pat.len(), " ");
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
            html.replace_range(start..start + 7 + rel_end, " ");
        } else {
            html.truncate(start);
            break;
        }
    }
    html
}

fn html_to_text(raw_html: &str) -> String {
    let cleaned = ["script", "style", "svg", "noscript", "head"]
        .into_iter()
        .fold(
            remove_html_comments(raw_html.to_string()),
            remove_tag_blocks,
        );

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
        let collapsed = raw_line.split_whitespace().collect::<Vec<_>>().join(" ");
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

impl AsyncTool for Tool {
    const NAME: &'static str = "fetch_url";
    const DESCRIPTION: &'static str = "Fetches content from a public HTTP/HTTPS URL. Automatically converts HTML pages into readable text and serializes JSON as compact output unless `raw: true` is set.";
    type Args = FetchUrlArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let max_output_size = ctx.max_output_size;
        let mut target_url = Url::parse(args.url.trim())
            .with_context(|| format!("Invalid URL: '{}'", args.url.trim()))?;

        let mut resp = None;
        for hop in 0..=MAX_REDIRECTS {
            let addrs = validate_public_url(&target_url).await?;
            let client = build_pinned_client(&target_url, &addrs)?;

            let r = client
                .get(target_url.clone())
                .header(
                    "Accept",
                    "text/markdown, text/plain, application/json, text/html;q=0.9, */*;q=0.5",
                )
                .send()
                .await
                .with_context(|| format!("HTTP request failed for '{target_url}'"))?;

            if r.status().is_redirection() {
                if hop == MAX_REDIRECTS {
                    bail!("Exceeded maximum redirect limit ({MAX_REDIRECTS})");
                }
                let loc = r
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .with_context(|| {
                        format!(
                            "Redirect response (HTTP {}) missing Location header",
                            r.status()
                        )
                    })?
                    .to_str()
                    .context("Invalid non-ASCII Location redirect header")?;
                target_url = target_url
                    .join(loc)
                    .with_context(|| format!("Failed to resolve redirect URL '{loc}'"))?;
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

        if ["image/", "audio/", "video/"]
            .iter()
            .any(|p| content_type.starts_with(p))
            || matches!(
                content_type.as_str(),
                "application/octet-stream" | "application/pdf" | "application/zip"
            )
        {
            bail!("Refusing to fetch binary content-type '{content_type}' from {target_url}");
        }

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
            let take = chunk.len().min(remaining);
            body_bytes.extend_from_slice(&chunk[..take]);
            if take < chunk.len() {
                break;
            }
        }

        let raw_text = String::from_utf8_lossy(&body_bytes).into_owned();
        let formatted_body = if !args.raw && content_type.contains("html") {
            html_to_text(&raw_text)
        } else if !args.raw && content_type.contains("json") {
            serde_json::from_str::<Value>(&raw_text)
                .and_then(|v| serde_json::to_string(&v))
                .unwrap_or(raw_text)
        } else {
            raw_text
        };

        if !status.is_success() {
            bail!(
                "HTTP {status} from {target_url}:\n{}",
                formatted_body.trim()
            );
        }

        Ok(format!(
            "URL: {target_url}\nStatus: {status}\nContent-Type: {content_type}\n\n{}",
            formatted_body.trim()
        ))
    }
}
