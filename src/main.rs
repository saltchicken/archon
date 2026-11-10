mod core;
mod tools;
mod ui;

use anyhow::Result;
use core::{config::Config, ollama::run_preflight, CoreEngine};
use std::{env, path::PathBuf};
use tokio::sync::mpsc;
use ui::TuiApp;

#[tokio::main]
async fn main() -> Result<()> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        ui::restore_terminal(&mut std::io::stdout(), true);
        original_hook(panic_info);
    }));

    let config_path = dirs::config_dir()
        .map(|dir| dir.join("archon").join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    let config = Config::load(&config_path);
    let args: Vec<String> = env::args().collect();

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
    tokio::spawn(engine.run(cmd_rx));

    TuiApp::new(target_model, config, cmd_tx).run(ev_rx).await
}
