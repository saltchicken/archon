use super::{
    AsyncTool, ToolContext,
    utils::http::{SsrfPolicy, validate_public_url},
};
use anyhow::{Context, Result, bail};
use reqwest::Url;
use schemars::JsonSchema;
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;
use std::{
    net::TcpListener,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, OnceLock},
    time::Duration,
};
use thirtyfour::{
    By, ChromiumLikeCapabilities, DesiredCapabilities, WebDriver, prelude::ElementQueryable,
};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
    time::{Instant, sleep},
};
use tracing::{debug, info, warn};

#[cfg(unix)]
use crate::core::processes::kill_process_group;

const DEFAULT_WAIT_SECS: u64 = 10;
const DRIVER_START_TIMEOUT_SECS: u64 = 5;
const MAX_EXTRACT_ELEMENTS: usize = 200;

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserAction {
    /// Navigate to a URL (optionally waiting for a CSS selector to appear).
    Goto,
    /// Extract visible text from the page or a specific CSS selector.
    Read,
    /// Click an element matching a CSS selector.
    Click,
    /// Type text into an input/textarea matching a CSS selector.
    Type,
    /// Execute JavaScript in the page context and return its JSON result.
    Eval,
    /// Extract structured data (text, attributes, href) from elements matching a CSS selector, parsed server-side with the `scraper` crate.
    Extract,
    /// Capture a PNG screenshot of the current viewport (or element) to a file.
    Screenshot,
    /// Show current URL, title, and session status.
    Status,
    /// Close the active browser session and free resources.
    Close,
}

#[derive(Deserialize, JsonSchema)]
pub struct BrowserArgs {
    /// Action to perform.
    pub action: BrowserAction,
    /// Target URL (required for `goto`).
    #[serde(default)]
    pub url: Option<String>,
    /// CSS selector to target (used by `read`, `click`, `type`, `extract`, `screenshot`, or as wait condition in `goto`).
    #[serde(default)]
    pub selector: Option<String>,
    /// Text to type (required for `type`).
    #[serde(default)]
    pub text: Option<String>,
    /// Clear existing input before typing (default: true).
    #[serde(default = "default_true")]
    pub clear: bool,
    /// JavaScript snippet to execute (required for `eval`).
    #[serde(default)]
    pub script: Option<String>,
    /// Relative workspace path to save screenshot PNG (optional for `screenshot`; defaults to a temp file).
    #[serde(default)]
    pub path: Option<String>,
    /// Max seconds to wait for `selector` to appear (default: 10).
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

fn default_true() -> bool {
    true
}

struct ManagedDriverProcess {
    child: Child,
    #[cfg(unix)]
    pgid: Option<u32>,
}

impl ManagedDriverProcess {
    fn spawn(port: u16) -> Result<Self> {
        let mut cmd = Command::new("chromedriver");
        cmd.arg(format!("--port={port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let child = cmd.spawn().context(
            "Failed to spawn `chromedriver`. Ensure it is installed in PATH \
             or set ARCHON_WEBDRIVER_URL to an existing WebDriver endpoint.",
        )?;
        #[cfg(unix)]
        let pgid = child.id();

        Ok(Self {
            child,
            #[cfg(unix)]
            pgid,
        })
    }

    async fn terminate(mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            kill_process_group(pgid);
        }
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

impl Drop for ManagedDriverProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            kill_process_group(pgid);
        }
        let _ = self.child.start_kill();
    }
}

struct BrowserSession {
    driver: WebDriver,
    /// Present only when Archon spawned `chromedriver` itself.
    driver_proc: Option<ManagedDriverProcess>,
}

impl BrowserSession {
    async fn teardown(self) {
        if let Err(e) = self.driver.quit().await {
            debug!("WebDriver quit error during teardown: {e}");
        }
        if let Some(proc) = self.driver_proc {
            proc.terminate().await;
        }
    }
}

pub struct BrowserRegistry {
    session: Mutex<Option<BrowserSession>>,
}

impl BrowserRegistry {
    pub fn global() -> Arc<Self> {
        static INSTANCE: OnceLock<Arc<BrowserRegistry>> = OnceLock::new();
        INSTANCE
            .get_or_init(|| {
                Arc::new(Self {
                    session: Mutex::new(None),
                })
            })
            .clone()
    }

    fn external_webdriver_url() -> Option<String> {
        std::env::var("ARCHON_WEBDRIVER_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn allocate_local_port() -> Result<u16> {
        let listener =
            TcpListener::bind("127.0.0.1:0").context("Failed to bind ephemeral local port")?;
        Ok(listener.local_addr()?.port())
    }

    async fn wait_for_driver_ready(wd_url: &str, proc: &mut ManagedDriverProcess) -> Result<()> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()?;
        let status_url = format!("{wd_url}/status");
        let deadline = Instant::now() + Duration::from_secs(DRIVER_START_TIMEOUT_SECS);

        while Instant::now() < deadline {
            if let Some(exit) = proc.child.try_wait()? {
                bail!("`chromedriver` exited prematurely with status {exit}");
            }
            if let Ok(resp) = client.get(&status_url).send().await
                && resp.status().is_success()
            {
                return Ok(());
            }
            sleep(Duration::from_millis(50)).await;
        }

        bail!("Timed out waiting for `chromedriver` to become ready at {wd_url}");
    }

    async fn connect_new() -> Result<BrowserSession> {
        let (wd_url, mut driver_proc) = match Self::external_webdriver_url() {
            Some(url) => (url, None),
            None => {
                let port = Self::allocate_local_port()?;
                let url = format!("http://127.0.0.1:{port}");
                info!("Spawning managed chromedriver on {url}");
                let mut proc = ManagedDriverProcess::spawn(port)?;
                if let Err(e) = Self::wait_for_driver_ready(&url, &mut proc).await {
                    proc.terminate().await;
                    return Err(e);
                }
                (url, Some(proc))
            }
        };

        let mut caps = DesiredCapabilities::chrome();
        caps.add_arg("--headless=new")?;
        caps.add_arg("--disable-gpu")?;
        caps.add_arg("--window-size=1440,900")?;

        info!("Starting headless WebDriver session at {wd_url}");
        match WebDriver::new(&wd_url, caps).await {
            Ok(driver) => Ok(BrowserSession {
                driver,
                driver_proc,
            }),
            Err(e) => {
                if let Some(proc) = driver_proc.take() {
                    proc.terminate().await;
                }
                Err(e).with_context(|| format!("Failed to connect to WebDriver at {wd_url}"))
            }
        }
    }

    async fn get_or_init(&self) -> Result<WebDriver> {
        let mut guard = self.session.lock().await;
        if let Some(ref existing) = *guard {
            if existing.driver.status().await.is_ok() {
                return Ok(existing.driver.clone());
            }
            warn!("Existing WebDriver session is stale; reconnecting...");
            if let Some(stale) = guard.take() {
                stale.teardown().await;
            }
        }
        let session = Self::connect_new().await?;
        let driver = session.driver.clone();
        *guard = Some(session);
        Ok(driver)
    }

    pub async fn status(&self) -> String {
        let guard = self.session.lock().await;
        let Some(ref session) = *guard else {
            return "Browser status: inactive (no open session).".into();
        };
        let url = session
            .driver
            .current_url()
            .await
            .map(|u| u.to_string())
            .unwrap_or_default();
        let title = session.driver.title().await.unwrap_or_default();
        let managed = if session.driver_proc.is_some() {
            "managed chromedriver"
        } else {
            "external WebDriver"
        };
        format!("Browser status: active ({managed})\nURL: {url}\nTitle: {title}")
    }

    pub async fn close(&self) -> Result<String> {
        let existing = self.session.lock().await.take();
        match existing {
            Some(session) => {
                session.teardown().await;
                Ok("Browser session closed.".into())
            }
            None => Ok("No active browser session.".into()),
        }
    }

    /// Called by `CoreEngine::run` on shutdown.
    pub async fn shutdown(&self) {
        if let Some(session) = self.session.lock().await.take() {
            info!("Shutting down active WebDriver session...");
            session.teardown().await;
        }
    }
}

/// Ensure the script has an explicit `return` statement.
///
/// Chromedriver ≥ 157 wraps the script in a function body, so bare
/// expressions evaluate to `undefined` (serialised as `null`) unless
/// you use an explicit `return`.  Single-expression scripts get one
/// auto-prepended; multi-statement scripts are passed through unchanged
/// (the user is expected to include their own `return`).
///
/// Detection: only a *top-level* `return` (parenthesis depth 0 and brace
/// depth 0) counts. `return`s nested inside a callback/function body, e.g.
/// `[1,2,3].map(function (x) { return x * 2; })`, are deliberately ignored so
/// that a `return` is still auto-prepended for the outer expression.
/// String literals, template literals, and comments are skipped, so the
/// substring "return" (e.g. in a CSS selector like `.return-btn`) never
/// produces a false positive.
fn ensure_return(script: &str) -> String {
    let trimmed = script.trim();
    if has_return_statement(trimmed) {
        trimmed.to_string()
    } else {
        format!("return {trimmed}")
    }
}

/// Returns `true` if the script contains a *top-level* `return` statement.
///
/// A `return` only counts as top-level when it is **not** nested inside a
/// function body (a `function (...) { ... }`, `function { ... }`, or
/// `=> { ... }` body). This correctly ignores `return`s that belong to an
/// inner callback (for example
/// `[1,2,3].map(function (x) { return x * 2; })`), which would otherwise
/// suppress the required top-level `return` and make the whole expression
/// evaluate to `undefined` (serialised as `null`).
///
/// A `return` inside a *plain* block is still top-level, e.g.
/// `if (x) { return 1; }` or `{ return 'a'; }`.
///
/// String literals, template literals, and line/block comments are skipped,
/// so the substring "return" (e.g. inside a CSS selector such as
/// `.return-btn`) never triggers a false positive.
fn has_return_statement(script: &str) -> bool {
    let chars: Vec<char> = script.chars().collect();
    let n = chars.len();
    let is_ident = |ch: char| ch.is_ascii_alphanumeric() || ch == '_';

    let mut i = 0;
    let mut paren_depth: i32 = 0;
    let mut brace_stack: Vec<bool> = Vec::new(); // true = function body
    let mut fn_awaiting_paren = false; // saw `function`; a param-list `(` may follow
    let mut fn_param_depth: Option<i32> = None; // open depth of a function param list
    let mut next_brace_is_fn = false; // the next `{` opens a function body

    while i < n {
        let c = chars[i];

        // Line comment: skip to the end of the line.
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // Block comment: skip to the closing `*/`.
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2; // consume the closing `*/` (or run off the end)
            continue;
        }
        // String / template literal: skip to the matching (unescaped) quote.
        if c == '"' || c == '\'' || c == '`' {
            let quote = c;
            i += 1;
            while i < n && chars[i] != quote {
                if chars[i] == '\\' {
                    i += 1; // skip the escaped character
                }
                i += 1;
            }
            i += 1; // skip the closing quote (or run off the end)
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }

        // `function` keyword (whole word).
        if c == 'f'
            && chars[i..]
                .iter()
                .collect::<String>()
                .starts_with("function")
            && chars.get(i + 8).map(|ch| !is_ident(*ch)).unwrap_or(true)
        {
            fn_awaiting_paren = true;
            next_brace_is_fn = true; // covers a paramless `function { ... }`
            i += 8;
            continue;
        }

        // Arrow `=>` (a `=` immediately followed by `>`).
        if c == '=' && i + 1 < n && chars[i + 1] == '>' {
            next_brace_is_fn = true;
            i += 2;
            continue;
        }

        match c {
            '(' => {
                paren_depth += 1;
                if fn_awaiting_paren {
                    fn_param_depth = Some(paren_depth);
                    fn_awaiting_paren = false;
                }
                i += 1;
            }
            ')' => {
                paren_depth -= 1;
                if matches!(fn_param_depth, Some(d) if paren_depth == d - 1) {
                    next_brace_is_fn = true;
                    fn_param_depth = None;
                }
                i += 1;
            }
            '{' => {
                brace_stack.push(next_brace_is_fn);
                next_brace_is_fn = false;
                fn_awaiting_paren = false;
                i += 1;
            }
            '}' => {
                brace_stack.pop();
                i += 1;
            }
            'r' if paren_depth == 0 => {
                let is_word = chars.get(i + 6).map(|ch| !is_ident(*ch)).unwrap_or(true);
                if chars[i..].iter().collect::<String>().starts_with("return") && is_word {
                    // A top-level `return` unless we are nested inside a
                    // function body (a callback), in which case it is not the
                    // top-level return the driver needs.
                    if !brace_stack.iter().any(|&b| b) {
                        return true;
                    }
                }
                i += 1;
            }
            _ => {
                // Any other token ends a pending `function`/`=>` body.
                fn_awaiting_paren = false;
                next_brace_is_fn = false;
                i += 1;
            }
        }
    }

    false
}

async fn find_with_wait(
    driver: &WebDriver,
    selector: &str,
    wait_secs: u64,
) -> Result<thirtyfour::WebElement> {
    driver
        .query(By::Css(selector))
        .wait(Duration::from_secs(wait_secs), Duration::from_millis(150))
        .first()
        .await
        .with_context(|| format!("Element '{selector}' not found within {wait_secs}s"))
}

/// Extract structured data (inner text, all attributes, and a resolved
/// `href`) from every element matching `selector` in an HTML `source`.
///
/// This is the CSS-selector extraction engine behind the `extract` action.
/// It parses the rendered HTML once with `scraper` and returns a JSON object:
/// `{ selector, count, truncated, items: [{ text, attrs, href }] }`.
fn extract_from_html(source: &str, selector: &str) -> Result<serde_json::Value> {
    let document = Html::parse_document(source);
    let sel = Selector::parse(selector)
        .map_err(|e| anyhow::anyhow!("Invalid CSS selector '{selector}': {e}"))?;
    let anchor_sel = Selector::parse("a[href]").expect("'a[href]' is a valid CSS selector");

    let matches: Vec<ElementRef> = document.select(&sel).collect();
    let truncated = matches.len() > MAX_EXTRACT_ELEMENTS;

    let items: Vec<serde_json::Value> = matches
        .iter()
        .take(MAX_EXTRACT_ELEMENTS)
        .map(|element| {
            let text: String = element.text().collect();
            let attrs: serde_json::Map<String, serde_json::Value> = element
                .value()
                .attrs
                .iter()
                .map(|(name, value)| {
                    (
                        name.local.to_string(),
                        serde_json::Value::String(value.to_string()),
                    )
                })
                .collect();
            let href = element.attr("href").map(str::to_owned).or_else(|| {
                element
                    .select(&anchor_sel)
                    .next()
                    .and_then(|a| a.attr("href").map(str::to_owned))
            });
            serde_json::json!({
                "text": text.trim(),
                "attrs": attrs,
                "href": href,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "selector": selector,
        "count": items.len(),
        "truncated": truncated,
        "items": items,
    }))
}

impl AsyncTool for Tool {
    const NAME: &'static str = "browser";
    const DESCRIPTION: &'static str = "Controls a headless browser via WebDriver (thirtyfour) to render JS pages, interact with DOM elements, evaluate JavaScript, extract structured data via CSS selectors, and capture screenshots.";
    type Args = BrowserArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let reg = BrowserRegistry::global();
        let wait_secs = args.wait_secs.unwrap_or(DEFAULT_WAIT_SECS).clamp(1, 60);

        match args.action {
            BrowserAction::Close => reg.close().await,

            BrowserAction::Status => Ok(reg.status().await),

            BrowserAction::Goto => {
                let raw_url = args
                    .url
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("'url' is required for browser:goto")?;

                let parsed =
                    Url::parse(raw_url).with_context(|| format!("Invalid URL: '{raw_url}'"))?;

                let policy = SsrfPolicy::from_config(
                    ctx.config.app.http_allow_private,
                    &ctx.config.app.http_allowed_cidrs,
                );
                validate_public_url(&parsed, &policy).await?;

                let driver = reg.get_or_init().await?;
                driver
                    .goto(parsed.as_str())
                    .await
                    .with_context(|| format!("Failed to navigate to '{parsed}'"))?;

                if let Some(sel) = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    find_with_wait(&driver, sel, wait_secs).await?;
                }

                let final_url = driver.current_url().await?;
                let title = driver.title().await.unwrap_or_default();
                Ok(format!("Navigated to: {final_url}\nTitle: {title}"))
            }

            BrowserAction::Read => {
                let driver = reg.get_or_init().await?;
                let text = if let Some(sel) = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let elem = find_with_wait(&driver, sel, wait_secs).await?;
                    elem.text().await.context("Failed to read element text")?
                } else {
                    let body = driver
                        .find(By::Tag("body"))
                        .await
                        .context("Page has no <body>")?;
                    body.text().await.context("Failed to read page text")?
                };
                let url = driver
                    .current_url()
                    .await
                    .map(|u| u.to_string())
                    .unwrap_or_default();
                let title = driver.title().await.unwrap_or_default();
                Ok(format!("URL: {url}\nTitle: {title}\n\n{}", text.trim()))
            }

            BrowserAction::Click => {
                let sel = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("'selector' is required for browser:click")?;
                let driver = reg.get_or_init().await?;
                let elem = find_with_wait(&driver, sel, wait_secs).await?;
                elem.click()
                    .await
                    .with_context(|| format!("Failed to click '{sel}'"))?;
                let url = driver
                    .current_url()
                    .await
                    .map(|u| u.to_string())
                    .unwrap_or_default();
                Ok(format!("Clicked '{sel}'. Current URL: {url}"))
            }

            BrowserAction::Type => {
                let sel = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("'selector' is required for browser:type")?;
                let text = args
                    .text
                    .as_deref()
                    .context("'text' is required for browser:type")?;
                let driver = reg.get_or_init().await?;
                let elem = find_with_wait(&driver, sel, wait_secs).await?;
                if args.clear {
                    let _ = elem.clear().await;
                }
                elem.send_keys(text)
                    .await
                    .with_context(|| format!("Failed to type into '{sel}'"))?;
                Ok(format!(
                    "Typed {} chars into '{sel}'.",
                    text.chars().count()
                ))
            }

            BrowserAction::Eval => {
                let script = args
                    .script
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("'script' is required for browser:eval")?;
                let driver = reg.get_or_init().await?;
                let ret = driver
                    .execute(ensure_return(script), vec![])
                    .await
                    .context("JavaScript evaluation failed")?;
                Ok(serde_json::to_string_pretty(ret.json())?)
            }

            BrowserAction::Extract => {
                let selector = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("'selector' is required for browser:extract")?;
                let driver = reg.get_or_init().await?;
                let ret = driver
                    .execute("return document.documentElement.outerHTML", vec![])
                    .await
                    .context("Failed to read rendered HTML for extract")?;
                let source = ret
                    .json()
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("rendered HTML source was not a string"))?;
                let result = extract_from_html(&source, selector)?;
                Ok(serde_json::to_string_pretty(&result)?)
            }

            BrowserAction::Screenshot => {
                let driver = reg.get_or_init().await?;
                let png = if let Some(sel) = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let elem = find_with_wait(&driver, sel, wait_secs).await?;
                    elem.screenshot_as_png()
                        .await
                        .context("Failed to capture element screenshot")?
                } else {
                    driver
                        .screenshot_as_png()
                        .await
                        .context("Failed to capture viewport screenshot")?
                };

                let out_path: PathBuf = match args
                    .path
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(rel) => ctx.resolve_new_path(rel)?,
                    None => std::env::temp_dir().join(format!(
                        "archon-screenshot-{}.png",
                        chrono::Local::now().format("%Y%m%d_%H%M%S_%3f")
                    )),
                };
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&out_path, &png).with_context(|| {
                    format!("Failed to write screenshot to {}", out_path.display())
                })?;

                Ok(format!(
                    "Saved screenshot ({} bytes) to {}",
                    png.len(),
                    out_path.display()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_return_prepends_bare_expression() {
        assert_eq!(ensure_return("document.title"), "return document.title");
    }

    #[test]
    fn ensure_return_keeps_existing_return() {
        assert_eq!(
            ensure_return("return document.title"),
            "return document.title"
        );
    }

    #[test]
    fn ensure_return_handles_multiline_with_return() {
        let script = "let el = document.body;\nreturn el.textContent;";
        assert_eq!(ensure_return(script), script);
    }

    #[test]
    fn ensure_return_detects_return_after_semicolon() {
        assert_eq!(
            ensure_return("let x = 1; return x;"),
            "let x = 1; return x;"
        );
    }

    #[test]
    fn ensure_return_detects_return_inside_block() {
        let script = "if (true) { return 'yes'; }";
        assert_eq!(ensure_return(script), script);
    }

    #[test]
    fn ensure_return_false_positive_css_selector() {
        // This should NOT be treated as having a return statement.
        let script = "document.querySelector('.return-btn').textContent";
        assert_eq!(
            ensure_return(script),
            "return document.querySelector('.return-btn').textContent"
        );
    }

    #[test]
    fn ensure_return_false_positive_variable_name() {
        let script = "return_value";
        // 'return_value' is a single identifier, not a return statement.
        assert_eq!(ensure_return(script), "return return_value");
    }

    #[test]
    fn ensure_return_handles_return_semicolon_only() {
        assert_eq!(ensure_return("return;"), "return;");
    }

    #[test]
    fn has_return_statement_negative_cases() {
        assert!(!has_return_statement(
            "document.querySelector('.return-btn')"
        ));
        assert!(!has_return_statement("let returnCount = 0;"));
        assert!(!has_return_statement("el.setAttribute('data-return', '1')"));
    }

    #[test]
    fn has_return_statement_positive_cases() {
        assert!(has_return_statement("return 42;"));
        assert!(has_return_statement("return 42"));
        assert!(has_return_statement("let x = 1; return x;"));
        assert!(has_return_statement("{ return 'a'; }"));
        assert!(has_return_statement("if (x) return y; else return z;"));
    }

    #[test]
    fn ensure_return_fixes_callback_false_positive() {
        // A `return` inside a callback must not suppress the top-level `return`
        // (this is the real-world bug that produced `null` from
        // `[1,2,3].map(function (x) { return x * 2; }).length`).
        assert_eq!(
            ensure_return("[1,2,3].map(function (x) { return x * 2; }).length"),
            "return [1,2,3].map(function (x) { return x * 2; }).length"
        );
    }

    #[test]
    fn ensure_return_fixes_arrow_callback_false_positive() {
        assert_eq!(
            ensure_return("[1,2,3].map((x) => x * 2).length"),
            "return [1,2,3].map((x) => x * 2).length"
        );
    }

    #[test]
    fn has_return_statement_ignores_callback_return() {
        assert!(!has_return_statement(
            "[1,2,3].map(function (x) { return x; }).length"
        ));
        assert!(!has_return_statement(
            "[1,2,3].forEach(function (x) { return; })"
        ));
        assert!(!has_return_statement(
            "JSON.stringify({ fn: function () { return 1; } })"
        ));
    }

    #[test]
    fn has_return_statement_ignores_string_and_comment_return() {
        assert!(!has_return_statement("JSON.stringify({ s: 'return 1' })"));
        assert!(!has_return_statement("// return 1\ndocument.title"));
        assert!(!has_return_statement("/* return 1 */\ndocument.title"));
    }

    #[test]
    fn has_return_statement_allows_top_level_return_after_block() {
        // The second `return` is top-level (depth 0); the first is inside the
        // block and is correctly ignored.
        assert!(has_return_statement("if (x) { return 1; } return 2;"));
    }

    #[test]
    fn extract_from_html_pulls_text_and_attrs() {
        let html =
            r#"<html><body><article class="post" id="a1"><h2>Title A</h2></article></body></html>"#;
        let out = extract_from_html(html, "article.post h2").unwrap();
        let items = out["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["text"], "Title A");
        assert_eq!(out["count"], 1);
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn extract_from_html_follows_descendant_anchor_href() {
        let html = r#"<ul><li data-id="7"><a href="/doc/7">Read</a></li></ul>"#;
        let out = extract_from_html(html, "li").unwrap();
        let item = &out["items"][0];
        assert_eq!(item["attrs"]["data-id"], "7");
        assert_eq!(item["href"], "/doc/7");
        assert_eq!(item["text"], "Read");
    }

    #[test]
    fn extract_from_html_prefers_own_href() {
        let html = r#"<a class="link" href="/self">link <a href="/child">nested</a></a>"#;
        let out = extract_from_html(html, "a.link").unwrap();
        assert_eq!(out["items"][0]["href"], "/self");
    }

    #[test]
    fn extract_from_html_invalid_selector_errors() {
        // A leading combinator (no left-hand compound) is invalid CSS.
        assert!(extract_from_html("<html></html>", "> div").is_err());
    }

    #[test]
    fn extract_from_html_truncates_over_max() {
        let html = format!(
            "<ul>{}</ul>",
            std::iter::repeat_n("<li>x</li>", 220).collect::<String>()
        );
        let out = extract_from_html(&html, "li").unwrap();
        assert_eq!(out["count"], 200);
        assert_eq!(out["truncated"], true);
    }

    #[test]
    fn browser_args_deserialization() {
        let json = r#"{"action":"goto","url":"https://example.com","wait_secs":5}"#;
        let args: BrowserArgs = serde_json::from_str(json).unwrap();
        assert!(matches!(args.action, BrowserAction::Goto));
        assert_eq!(args.url.as_deref(), Some("https://example.com"));
        assert_eq!(args.wait_secs, Some(5));
    }

    #[test]
    fn browser_args_defaults() {
        let json = r#"{"action":"status"}"#;
        let args: BrowserArgs = serde_json::from_str(json).unwrap();
        assert!(matches!(args.action, BrowserAction::Status));
        assert_eq!(args.url, None);
        assert!(args.clear, "clear should default to true");
    }
}
