use anyhow::Result;
use archon::cli::{CliApp, CliArgs};
use archon::core::{
    CoreEngine,
    config::{
        Config, archon_config_root, ensure_workspace_config, load_dotenv, workspace_config_dir_for,
    },
    provider::Provider,
};
use archon::ui::TuiApp;
use std::env;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    let cli_args = CliArgs::parse()?;

    let cwd = env::current_dir()?;
    let base_config_path = archon_config_root().join("config.toml");

    if let Some(sched_cmd) = cli_args.schedule_cmd {
        let ws_config_dir = workspace_config_dir_for(&cwd);
        let (config, config_err) = Config::load(&base_config_path, &ws_config_dir);
        if let Some(cfg_err) = config_err {
            eprintln!("{cfg_err}");
            std::process::exit(1);
        }
        if let Err(e) = archon::core::schedule::handle_command(sched_cmd, &config) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    if let Some(skill_cmd) = cli_args.skill_cmd {
        let ws_config_dir = workspace_config_dir_for(&cwd);
        let (config, config_err) = Config::load(&base_config_path, &ws_config_dir);
        if let Some(cfg_err) = config_err {
            eprintln!("{cfg_err}");
            std::process::exit(1);
        }
        if let Err(e) = archon::core::skill_check::handle_command(skill_cmd, &config) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    if !cli_args.headless {
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            archon::ui::restore_terminal(&mut std::io::stdout(), true);
            original_hook(panic_info);
        }));
    }

    let ws_config_dir = if cli_args.headless {
        workspace_config_dir_for(&cwd)
    } else {
        let Some(dir) = ensure_workspace_config(&cwd)? else {
            println!("Exiting Archon.");
            std::process::exit(0);
        };
        dir
    };

    // Load .env from the workspace config directory or working directory before reading config
    load_dotenv(ws_config_dir.join(".env"));
    load_dotenv(".env");

    let (mut config, config_err) = Config::load(&base_config_path, &ws_config_dir);
    if let Some(ref allowed) = cli_args.allowed_tools {
        config.restrict_enabled_tools(allowed);
    }
    if cli_args.model.is_some() {
        config.set_cli_model_override();
    }
    let guard = archon::core::logger::init(&config.app.history_dir);
    if let Some(ref err) = config_err {
        warn!("{err}");
    } else {
        info!("Archon starting up. Config loaded successfully.");
    }

    let target_model = cli_args
        .model
        .clone()
        .unwrap_or_else(|| config.startup_model());
    let provider = Provider::from_model(&target_model);

    info!(
        "Targeting model: {} (Provider: {:?})",
        target_model, provider
    );

    if let Err(e) = provider.run_preflight(&target_model, &config).await {
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

    let mut engine = CoreEngine::new(target_model.clone(), config.clone(), ev_tx)?;
    if cli_args.no_save {
        engine.set_save_session(false);
    }
    let engine_handle = tokio::spawn(engine.run(cmd_rx));

    let res = if cli_args.headless {
        CliApp::new(cli_args, target_model, config, cmd_tx)
            .run(ev_rx)
            .await
    } else {
        TuiApp::new(target_model, config, cmd_tx).run(ev_rx).await
    };
    let _ = engine_handle.await;
    drop(guard);

    if let Err(e) = res {
        eprintln!("{e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}
