use super::{
    AsyncTool, ToolContext,
    fetch_url::{build_pinned_client, decode_entities, validate_public_url},
};
use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::Url;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{collections::HashSet, fmt::Write as _, sync::LazyLock};

const DDG_HTML_URL: &str = "https://html.duckduckgo.com/html/";
const DDG_LITE_URL: &str = "https://lite.duckduckgo.com/lite/";
const DEFAULT_MAX_RESULTS: usize = 8;
const MAX_RESULTS_CAP: usize = 20;

pub struct Tool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TimeRange {
    /// Past 24 hours.
    Day,
    /// Past 7 days.
    Week,
    /// Past 30 days.
    Month,
    /// Past 12 months.
    Year,
}

impl TimeRange {
    fn as_ddg_param(self) -> &'static str {
        match self {
            Self::Day => "d",
            Self::Week => "w",
            Self::Month => "m",
            Self::Year => "y",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct WebSearchArgs {
    /// The search query string (supports standard search operators like `site:`, `"exact phrase"`, `-exclude`, `filetype:`).
    pub query: String,
    /// Maximum number of search results to return (default: 8, max: 20).
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Optional recency filter (`"day"`, `"week"`, `"month"`, `"year"`).
    #[serde(default)]
    pub time_range: Option<TimeRange>,
    /// Optional region/locale code (e.g. `"us-en"`, `"uk-en"`, `"de-de"`, or `"wt-wt"` for global; defaults to `"wt-wt"`).
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

static HTML_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<[^>]+>").expect("valid regex"));

static DDG_HTML_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<a\b[^>]*class="[^"]*\bresult__a\b[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>|<a\b[^>]*href="([^"]+)"[^>]*class="[^"]*\bresult__a\b[^"]*"[^>]*>(.*?)</a>"#,
    )
    .expect("valid regex")
});

static DDG_HTML_SNIPPET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<(?:a|div|span)\b[^>]*class="[^"]*\bresult__snippet\b[^"]*"[^>]*>(.*?)</(?:a|div|span)>"#)
        .expect("valid regex")
});

static DDG_LITE_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<a\b[^>]*class=['"][^'"]*\bresult-link\b[^'"]*['"][^>]*href=['"]([^'"]+)['"][^>]*>(.*?)</a>|<a\b[^>]*href=['"]([^'"]+)['"][^>]*class=['"][^'"]*\bresult-link\b[^'"]*['"][^>]*>(.*?)</a>"#,
    )
    .expect("valid regex")
});

static DDG_LITE_SNIPPET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<td\b[^>]*class=['"][^'"]*\bresult-snippet\b[^'"]*['"][^>]*>(.*?)</td>"#)
        .expect("valid regex")
});

fn clean_inline_html(fragment: &str) -> String {
    let stripped = HTML_TAG_RE.replace_all(fragment, " ");
    let decoded = decode_entities(&stripped);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn unwrap_ddg_url(raw_href: &str) -> Option<String> {
    let decoded_href = decode_entities(raw_href.trim());
    if decoded_href.is_empty() {
        return None;
    }

    let normalized = if decoded_href.starts_with("//") {
        format!("https:{decoded_href}")
    } else if decoded_href.starts_with('/') {
        format!("https://duckduckgo.com{decoded_href}")
    } else {
        decoded_href
    };

    let parsed = Url::parse(&normalized).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }

    if let Some(host) = parsed.host_str()
        && host.ends_with("duckduckgo.com")
    {
        if parsed.path().starts_with("/y.js") || parsed.query_pairs().any(|(k, _)| k == "ad_domain")
        {
            return None;
        }
        if let Some((_, target)) = parsed.query_pairs().find(|(k, _)| k == "uddg") {
            let target_url = Url::parse(target.trim()).ok()?;
            if matches!(target_url.scheme(), "http" | "https") {
                return Some(target_url.to_string());
            }
        }
        return None;
    }

    Some(parsed.to_string())
}

fn parse_ddg_results(
    html: &str,
    max_results: usize,
    link_re: &Regex,
    snippet_re: &Regex,
) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut seen_urls = HashSet::new();

    let links: Vec<(usize, String, String)> = link_re
        .captures_iter(html)
        .filter_map(|cap| {
            let m = cap.get(0)?;
            let raw_url = cap.get(1).or_else(|| cap.get(3))?.as_str();
            let raw_title = cap.get(2).or_else(|| cap.get(4))?.as_str();
            let url = unwrap_ddg_url(raw_url)?;
            let title = clean_inline_html(raw_title);
            (!title.is_empty()).then_some((m.end(), url, title))
        })
        .collect();

    for (idx, (link_end, url, title)) in links.iter().enumerate() {
        if results.len() >= max_results {
            break;
        }
        if !seen_urls.insert(url.clone()) {
            continue;
        }

        let next_link_pos = links.get(idx + 1).map_or(html.len(), |(pos, _, _)| *pos);
        let snippet = snippet_re
            .captures(&html[*link_end..next_link_pos])
            .and_then(|c| c.get(1))
            .map(|m| clean_inline_html(m.as_str()))
            .unwrap_or_default();

        results.push(SearchResult {
            title: title.clone(),
            url: url.clone(),
            snippet,
        });
    }

    results
}

async fn fetch_search_endpoint(
    endpoint: &str,
    query: &str,
    region: &str,
    time_range: Option<TimeRange>,
) -> Result<String> {
    let mut url = Url::parse(endpoint).context("Invalid search endpoint URL")?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("q", query);
        pairs.append_pair("kl", region);
        if let Some(tr) = time_range {
            pairs.append_pair("df", tr.as_ddg_param());
        }
    }

    let addrs = validate_public_url(&url).await?;
    let client = build_pinned_client(&url, &addrs)?;

    let resp = client
        .get(url.clone())
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (X11; Linux x86_64; rv:135.0) Gecko/20100101 Firefox/135.0",
        )
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .send()
        .await
        .with_context(|| format!("Search HTTP request failed for '{endpoint}'"))?;

    let status = resp.status();
    if status.as_u16() == 202 || status.as_u16() == 403 || status.as_u16() == 429 {
        bail!("Search endpoint '{endpoint}' rate-limited or challenged request (HTTP {status})");
    }
    if !status.is_success() {
        bail!("Search endpoint '{endpoint}' returned HTTP {status}");
    }

    resp.text()
        .await
        .with_context(|| format!("Failed to read response body from '{endpoint}'"))
}

fn format_results(query: &str, results: &[SearchResult], max_output_size: usize) -> String {
    if results.is_empty() {
        return format!("No web search results found for query: '{query}'");
    }

    let mut out = format!(
        "Web search results for '{query}' ({} result{}):\n",
        results.len(),
        if results.len() == 1 { "" } else { "s" }
    );

    for (idx, item) in results.iter().enumerate() {
        let _ = writeln!(out, "\n{}. {}", idx + 1, item.title);
        let _ = writeln!(out, "   URL: {}", item.url);
        if !item.snippet.is_empty() {
            let _ = writeln!(out, "   Snippet: {}", item.snippet);
        }
        if max_output_size > 0 && out.len() > max_output_size {
            break;
        }
    }

    out.trim_end().to_string()
}

impl AsyncTool for Tool {
    const NAME: &'static str = "web_search";
    const DESCRIPTION: &'static str = "Searches the web using DuckDuckGo and returns ranked results with titles, URLs, and text snippets. Use `fetch_url` to read the full content of any returned URL.";
    type Args = WebSearchArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let query = args.query.trim();
        if query.is_empty() {
            bail!("'query' cannot be empty");
        }

        let max_results = args
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, MAX_RESULTS_CAP);
        let region = args
            .region
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("wt-wt");

        let mut last_err = None;
        let endpoints = [
            (DDG_HTML_URL, &*DDG_HTML_LINK_RE, &*DDG_HTML_SNIPPET_RE),
            (DDG_LITE_URL, &*DDG_LITE_LINK_RE, &*DDG_LITE_SNIPPET_RE),
        ];

        for (endpoint, link_re, snippet_re) in endpoints {
            match fetch_search_endpoint(endpoint, query, region, args.time_range).await {
                Ok(body) => {
                    let results = parse_ddg_results(&body, max_results, link_re, snippet_re);
                    if !results.is_empty() {
                        return Ok(format_results(query, &results, ctx.max_output_size));
                    }
                    last_err = None;
                }
                Err(e) => {
                    last_err.get_or_insert(e);
                }
            }
        }

        match last_err {
            Some(err) => Err(err),
            None => Ok(format_results(query, &[], ctx.max_output_size)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_duckduckgo_redirect_and_filters_ads() {
        let redirect =
            "//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fstd%2F&amp;rut=12345";
        assert_eq!(
            unwrap_ddg_url(redirect).as_deref(),
            Some("https://doc.rust-lang.org/std/")
        );

        let direct = "https://crates.io/crates/tokio";
        assert_eq!(
            unwrap_ddg_url(direct).as_deref(),
            Some("https://crates.io/crates/tokio")
        );

        let ad_link =
            "https://duckduckgo.com/y.js?ad_domain=example.com&amp;u3=https%3A%2F%2Fexample.com";
        assert_eq!(unwrap_ddg_url(ad_link), None);
    }

    #[test]
    fn parses_ddg_html_results_and_deduplicates() {
        let sample_html = r#"
        <div class="result results_links results_links_deep web-result">
            <h2 class="result__title">
                <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=abc">
                    <b>Rust</b> Programming Language
                </a>
            </h2>
            <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F">
                A language empowering everyone to build reliable and efficient software.
            </a>
        </div>
        <div class="result results_links results_links_deep web-result">
            <h2 class="result__title">
                <a class="result__a" href="https://duckduckgo.com/y.js?ad_domain= track.example.com">Sponsored Ad</a>
            </h2>
            <a class="result__snippet">Ad snippet</a>
        </div>
        <div class="result results_links results_links_deep web-result">
            <h2 class="result__title">
                <a href="https://tokio.rs/" class="result__a">Tokio - An asynchronous <b>Rust</b> runtime</a>
            </h2>
            <div class="result__snippet">Tokio is an event-driven, non-blocking I/O platform &amp; runtime.</div>
        </div>
        "#;

        let results = parse_ddg_results(sample_html, 10, &DDG_HTML_LINK_RE, &DDG_HTML_SNIPPET_RE);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(
            results[0].snippet,
            "A language empowering everyone to build reliable and efficient software."
        );
        assert_eq!(results[1].title, "Tokio - An asynchronous Rust runtime");
        assert_eq!(results[1].url, "https://tokio.rs/");
        assert_eq!(
            results[1].snippet,
            "Tokio is an event-driven, non-blocking I/O platform & runtime."
        );
    }

    #[test]
    fn parses_ddg_lite_results() {
        let sample_lite = r#"
        <table>
            <tr>
                <td>1.&nbsp;</td>
                <td>
                    <a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2F" class='result-link'>Docs.rs</a>
                </td>
            </tr>
            <tr>
                <td>&nbsp;&nbsp;&nbsp;</td>
                <td class='result-snippet'>
                    <b>Docs.rs</b> is the documentation host for crates published to crates.io.
                </td>
            </tr>
        </table>
        "#;

        let results = parse_ddg_results(sample_lite, 5, &DDG_LITE_LINK_RE, &DDG_LITE_SNIPPET_RE);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Docs.rs");
        assert_eq!(results[0].url, "https://docs.rs/");
        assert_eq!(
            results[0].snippet,
            "Docs.rs is the documentation host for crates published to crates.io."
        );
    }
}
