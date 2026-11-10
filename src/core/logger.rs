use std::path::PathBuf;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub fn init(history_dir: &str) -> tracing_appender::non_blocking::WorkerGuard {
    let log_dir = match history_dir.strip_prefix("~/") {
        Some(stripped) => dirs::home_dir().unwrap_or_default().join(stripped).join("logs"),
        None => PathBuf::from(history_dir).join("logs"),
    };

    std::fs::create_dir_all(&log_dir).unwrap_or_default();

    let file_appender = RollingFileAppender::new(Rotation::DAILY, log_dir, "archon.log");
    
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(true),
        )
        .init();

    guard 
}
