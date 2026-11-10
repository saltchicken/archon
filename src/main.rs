mod core;
mod tools;
mod ui;

use anyhow::Result;
use core::{config::Config, ollama::run_preflight, tools::ToolRegistry, CoreEngine};
use crossterm::{
    event::{DisableMouseCapture, PopKeyboardEnhancementFlags},
    execute,
    terminal::{disable_raw_mode, LeaveAlternateScreen},
};
use serde_json::{json, Value};
use std::env;
use tokio::sync::mpsc;
use ui::TuiApp;

#[tokio::main]
async fn main() -> Result<()> {
    // Graceful Panic Handler: Ensures the terminal isn't left permanently broken 
    // and that advanced Kitty keyboard protocols are popped if the app crashes.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let mut stdout = std::io::stdout();
        let _ = disable_raw_mode();
        let _ = execute!(
            stdout,
            LeaveAlternateScreen,
            DisableMouseCapture,
            PopKeyboardEnhancementFlags
        );
        original_hook(panic_info);
    }));

    let config = Config::load("config.toml");
    let args: Vec<String> = env::args().collect();

    // Multi-call binary routing: `archon tool <name> [--schema | <json_payload>]`
    if args.get(1).map(|s| s.as_str()) == Some("tool") {
        let tool_name = args.get(2).map(|s| s.as_str()).unwrap_or("");
        let arg = args.get(3).map(|s| s.as_str()).unwrap_or("");

        if arg == "--schema" {
            if let Some(schema) = ToolRegistry::builtin_schema(tool_name) {
                println!("{}", serde_json::to_string(&schema)?);
                return Ok(());
            } else {
                eprintln!("Unknown built-in tool: {tool_name}");
                std::process::exit(1);
            }
        }

        let payload: Value = serde_json::from_str(arg).unwrap_or_else(|_| json!({}));
        let tool_args = payload.get("arguments").unwrap_or(&payload);
        let allowed_cmds: Vec<String> = payload
            .pointer("/context/allowed_commands")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_else(|| config.app.allowed_commands.clone());

        match ToolRegistry::execute_builtin(tool_name, tool_args, &allowed_cmds) {
            Some(Ok(out)) => {
                println!("{out}");
                return Ok(());
            }
            Some(Err(e)) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
            None => {
                eprintln!("Unknown built-in tool: {tool_name}");
                std::process::exit(1);
            }
        }
    }

    let target_model = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| config.app.default_model.clone());

    if let Err(e) = run_preflight(&target_model, &config).await {
        eprintln!("{e}");
        std::process::exit(1);
    }

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();

    let engine = CoreEngine::new(target_model.clone(), config.clone(), ev_tx)?;
    tokio::spawn(async move {
        engine.run(cmd_rx).await;
    });

    let app = TuiApp::new(target_model, config, cmd_tx);
    app.run(ev_rx).await
}
