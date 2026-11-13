use crate::core::config::expand_tilde;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Maximum size of `archon.log` before it is compacted (5 MiB).
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// Portion of recent logs to retain when compacting (75% = 3.75 MiB).
const RETAIN_LOG_BYTES: u64 = (MAX_LOG_BYTES * 3) / 4;

struct BoundedFileWriter {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl BoundedFileWriter {
    fn new(path: PathBuf) -> Self {
        let mut writer = Self {
            path,
            file: None,
            size: 0,
        };
        writer.open_and_check();
        writer
    }

    fn open_and_check(&mut self) {
        if let Ok(file) = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .append(true)
            .open(&self.path)
        {
            self.size = file.metadata().map_or(0, |m| m.len());
            self.file = Some(file);
            if self.size > MAX_LOG_BYTES {
                let _ = self.compact();
            }
        }
    }

    /// Retains the newest `RETAIN_LOG_BYTES` aligned to a newline boundary.
    fn compact(&mut self) -> io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.flush()?;

        let len = file.metadata()?.len();
        if len <= RETAIN_LOG_BYTES {
            self.size = len;
            return Ok(());
        }

        let seek_pos = len - RETAIN_LOG_BYTES;
        file.seek(SeekFrom::Start(seek_pos))?;

        let mut tail = Vec::with_capacity(RETAIN_LOG_BYTES as usize);
        file.read_to_end(&mut tail)?;

        // Align to the first newline so we don't leave a partial log line at the top
        let start_offset = tail
            .iter()
            .position(|&b| b == b'\n')
            .map_or(0, |pos| pos + 1);
        let kept = &tail[start_offset..];

        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;

        let header = b"--- [archon.log compacted to stay under size limit] ---\n";
        file.write_all(header)?;
        file.write_all(kept)?;
        file.flush()?;

        self.size = (header.len() + kept.len()) as u64;
        Ok(())
    }
}

impl Write for BoundedFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.file.is_none() {
            self.open_and_check();
        }
        if self.size.saturating_add(buf.len() as u64) > MAX_LOG_BYTES {
            let _ = self.compact();
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(buf.len());
        };
        let written = file.write(buf)?;
        self.size = self.size.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

/// Removes legacy daily `archon.log.YYYY-MM-DD` files left by `Rotation::DAILY`.
fn cleanup_legacy_daily_logs(log_dir: &Path) {
    for entry in fs::read_dir(log_dir).into_iter().flatten().flatten() {
        if let Some(name) = entry.file_name().to_str()
            && name.starts_with("archon.log.")
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub fn init(history_dir: &str) -> tracing_appender::non_blocking::WorkerGuard {
    let log_dir = expand_tilde(history_dir).join("logs");
    let _ = fs::create_dir_all(&log_dir);
    cleanup_legacy_daily_logs(&log_dir);

    let log_path = log_dir.join("archon.log");
    let writer = BoundedFileWriter::new(log_path);
    let (non_blocking, guard) = tracing_appender::non_blocking(writer);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(true),
        )
        .init();

    guard
}
