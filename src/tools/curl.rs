use super::{
    AsyncTool, ToolContext,
    utils::http::{build_pinned_client, validate_public_url},
};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, Method, Url};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::fmt::Write;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_SECS: u64 = 20;
const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl HttpMethod {
    const ALL: &'static [(&'static str, Self)] = &[
        ("get", Self::Get),
        ("post", Self::Post),
        ("put", Self::Put),
        ("patch", Self::Patch),
        ("delete", Self::Delete),
        ("head", Self::Head),
        ("options", Self::Options),
    ];

    /// Parse a method name case-insensitively (accepts a few common aliases).
    fn parse(raw: &str) -> Option<Self> {
        let norm = raw.trim().to_ascii_lowercase();
        let norm = if norm == "del" {
            "delete"
        } else {
            norm.as_str()
        };
        Self::ALL
            .iter()
            .find(|(name, _)| *name == norm)
            .map(|(_, m)| *m)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CurlArgs {
    /// The full HTTP or HTTPS URL to request, including any query string (e.g. `https://api.example.com/v1/items?limit=10`).
    url: String,
    /// HTTP method. One of: `get`, `post`, `put`, `patch`, `delete`, `head`, `options` (case-insensitive).
    /// Defaults to `get`, or `post` when a `body`/`json` payload is provided.
    #[serde(default)]
    method: Option<String>,
    /// Request headers as `Name: value` strings (e.g. `["Content-Type: application/json", "Authorization: Bearer <token>"]`).
    #[serde(default)]
    headers: Vec<String>,
    /// Raw request body string. Pair it with an appropriate `Content-Type` header. Mutually exclusive with `json`.
    #[serde(default)]
    body: Option<String>,
    /// JSON object/array to send as the request body (auto-sets `Content-Type: application/json`). Mutually exclusive with `body`.
    #[serde(default)]
    json: Option<Value>,
    /// Request only the response headers (like `curl -I`); forces the `HEAD` method and omits the body.
    #[serde(default)]
    head: bool,
    /// Include the response headers in the output (default: true).
    #[serde(default = "default_show_headers")]
    show_headers: bool,
    /// Maximum seconds to wait for the whole request (default: 20).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

fn default_show_headers() -> bool {
    true
}

pub struct Tool;

fn resolve_method(args: &CurlArgs) -> Result<HttpMethod> {
    if args.head {
        return Ok(HttpMethod::Head);
    }
    if let Some(raw) = args
        .method
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return HttpMethod::parse(raw).ok_or_else(|| {
            anyhow::anyhow!(
                "Invalid `method` '{raw}'. Expected one of: get, post, put, patch, delete, head, options."
            )
        });
    }
    Ok(if args.body.is_some() || args.json.is_some() {
        HttpMethod::Post
    } else {
        HttpMethod::Get
    })
}

fn build_request(
    client: &Client,
    url: &Url,
    method: HttpMethod,
    args: &CurlArgs,
) -> reqwest::RequestBuilder {
    let mut req = match method {
        HttpMethod::Get => client.get(url.clone()),
        HttpMethod::Post => client.post(url.clone()),
        HttpMethod::Put => client.put(url.clone()),
        HttpMethod::Patch => client.patch(url.clone()),
        HttpMethod::Delete => client.delete(url.clone()),
        HttpMethod::Head => client.head(url.clone()),
        HttpMethod::Options => client.request(Method::OPTIONS, url.clone()),
    };

    let mut content_type_set = false;
    for h in &args.headers {
        if let Some((name, value)) = h.split_once(':') {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if name.eq_ignore_ascii_case("content-type") {
                content_type_set = true;
            }
            req = req.header(name, value.trim());
        }
    }

    // A HEAD request must not carry a body (see the `head` field: "omits the
    // body"), so only attach a payload for non-HEAD methods. This also keeps
    // `head=true` from contradicting any `body`/`json` the caller passed in.
    if !matches!(method, HttpMethod::Head) {
        if let Some(json) = &args.json {
            if !content_type_set {
                req = req.header(reqwest::header::CONTENT_TYPE, "application/json");
            }
            req = req.body(serde_json::to_string(json).unwrap_or_else(|_| "{}".into()));
        } else if let Some(body) = &args.body {
            req = req.body(body.clone());
        }
    }

    req
}

async fn read_capped_body(resp: reqwest::Response, cap: usize) -> Result<String> {
    let mut body_bytes = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk_res) = stream.next().await {
        let chunk = chunk_res.context("Error reading HTTP response stream")?;
        let remaining = cap.saturating_sub(body_bytes.len());
        if remaining == 0 {
            break;
        }
        let take = chunk.len().min(remaining);
        body_bytes.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&body_bytes).into_owned())
}

fn maybe_pretty_json(content_type: &str, body: &str) -> String {
    let looks_json =
        content_type.contains("json") || body.starts_with('{') || body.starts_with('[');
    if !looks_json {
        return body.to_string();
    }
    serde_json::from_str::<Value>(body)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| body.to_string())
}

impl AsyncTool for Tool {
    const NAME: &'static str = "curl";
    const DESCRIPTION: &'static str = "General-purpose HTTP/HTTPS client (like `curl`): send a request with a custom method, headers, and a body (raw string or JSON) and return the status line, headers, and body. Only public URLs are allowed (same SSRF protections as `fetch_url`). Prefer `fetch_url` when you want an HTML page converted to readable text.";
    type Args = CurlArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        if args.body.is_some() && args.json.is_some() {
            bail!("Provide either `body` or `json`, not both.");
        }

        let url_arg = args.url.trim().to_string();
        if url_arg.is_empty() {
            bail!("A non-empty `url` is required.");
        }

        let method = resolve_method(&args)?;
        let has_body_payload =
            !matches!(method, HttpMethod::Head) && (args.body.is_some() || args.json.is_some());
        // Only follow redirects for requests that won't replay a body (curl-like:
        // don't replay a POST/PUT/PATCH body). HEAD never carries a body here.
        let follow_redirects = !has_body_payload;
        let timeout_secs = args.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1);

        let mut target_url =
            Url::parse(&url_arg).with_context(|| format!("Invalid URL: '{url_arg}'"))?;

        let start = Instant::now();
        let mut resp = None;
        for hop in 0..=MAX_REDIRECTS {
            let addrs = validate_public_url(&target_url, ctx.config.app.http_allow_private).await?;
            let client = build_pinned_client(&target_url, &addrs)?;

            let req = build_request(&client, &target_url, method, &args)
                .timeout(Duration::from_secs(timeout_secs));
            let r = req
                .send()
                .await
                .with_context(|| format!("HTTP request failed for '{target_url}'"))?;

            if r.status().is_redirection() && follow_redirects {
                if hop == MAX_REDIRECTS {
                    bail!("Exceeded maximum redirect limit ({MAX_REDIRECTS})");
                }
                let loc = r
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .context("Redirect response missing Location header")?
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
        let elapsed = start.elapsed();

        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        let mut out = String::new();
        let _ = writeln!(
            out,
            "> {} {}  ({:.2}s)",
            method.as_str(),
            target_url,
            elapsed.as_secs_f64()
        );
        let _ = writeln!(out, "HTTP {status}");
        out.push('\n');

        if args.show_headers {
            out.push_str("Headers:\n");
            for (name, value) in resp.headers() {
                let _ = writeln!(
                    out,
                    "  {}: {}",
                    name.as_str(),
                    value.to_str().unwrap_or("<binary>")
                );
            }
            out.push('\n');
        }

        if !matches!(method, HttpMethod::Head) {
            let body = read_capped_body(resp, ctx.max_output_size.max(1)).await?;
            let trimmed = body.trim();
            out.push_str("Body:\n");
            if trimmed.is_empty() {
                out.push_str("  (empty)\n");
            } else {
                let formatted = maybe_pretty_json(&content_type, trimmed);
                for line in formatted.lines() {
                    let _ = writeln!(out, "  {line}");
                }
            }
        }

        Ok(out.trim_end().to_string())
    }
}
