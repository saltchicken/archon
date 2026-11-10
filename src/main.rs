mod core;
mod tools;
mod ui;

use anyhow::Result;
use core::{config::Config, ollama::run_preflight, CoreEngine};
use std::{env, path::PathBuf};
use tokio::sync::mpsc;
use tracing::{error, info};
use ui::TuiApp;

#[tokio::main]
async fn main() -> Result<()> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        ui::restore_terminal(&mut std::io::stdout(), true);
        original_hook(panic_info);
    }));

    let base_config_path = dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
        .join("archon")
        .join("config.toml");

    let local_config_path = PathBuf::from(".archon.toml");

    let config = Config::load(&base_config_path, &local_config_path);

    let _guard = core::logger::init(&config.app.history_dir);
    info!("Archon starting up. Config loaded successfully.");

    let args: Vec<String> = env::args().collect();
    let target_model = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| config.app.default_model.clone());

    info!("Targeting model: {}", target_model);

    if let Err(e) = run_preflight(&target_model, &config).await {
        error!("Preflight failed: {}", e);
        eprintln!("{e}");
        std::process::exit(1);
    }

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();

    let engine = CoreEngine::new(target_model.clone(), config.clone(), ev_tx)?;
    let engine_handle = tokio::spawn(engine.run(cmd_rx));

    TuiApp::new(target_model, config, cmd_tx).run(ev_rx).await?;

    let _ = engine_handle.await;
    Ok(())
}
