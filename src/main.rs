mod core;
mod tools;
mod ui;

use anyhow::Result;
use core::{config::Config, ollama::run_preflight, CoreEngine, EngineEvent};
use std::env;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
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
        .join("archon/config.toml");

    let (config, config_err) = Config::load(&base_config_path, ".archon.toml");
    let guard = core::logger::init(&config.app.history_dir);
    if let Some(ref err) = config_err {
        warn!("{err}");
    } else {
        info!("Archon starting up. Config loaded successfully.");
    }

    let target_model = env::args()
        .nth(1)
        .unwrap_or_else(|| config.app.default_model.clone());
    info!("Targeting model: {}", target_model);

    if let Err(e) = run_preflight(&target_model, &config).await {
        error!("Preflight failed: {}", e);
        if let Some(cfg_err) = config_err {
            eprintln!("Warning: {cfg_err}");
        }
        eprintln!("{e}");
        drop(guard);
        std::process::exit(1);
    }

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();

    if let Some(err) = config_err {
        let _ = ev_tx.send(EngineEvent::Notification {
            message: err,
            is_error: true,
        });
    }

    let engine = CoreEngine::new(target_model.clone(), config.clone(), ev_tx)?;
    let engine_handle = tokio::spawn(engine.run(cmd_rx));

    let res = TuiApp::new(target_model, config, cmd_tx).run(ev_rx).await;
    let _ = engine_handle.await;
    drop(guard);

    if let Err(e) = res {
        eprintln!("{e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}
