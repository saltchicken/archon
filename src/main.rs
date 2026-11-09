mod core;
mod ui;

use anyhow::Result;
use core::{config::Config, ollama::run_preflight, CoreEngine};
use std::env;
use tokio::sync::mpsc;
use ui::TuiApp;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load("config.toml");
    let target_model = env::args()
        .nth(1)
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
