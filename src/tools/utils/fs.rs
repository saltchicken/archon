use anyhow::{Context, Result, bail};
use std::{
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write as _},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncRead, AsyncReadExt};

pub enum FileOutcome {
    Content(String),
    Skipped(&'static str),
    Error(String),
}

pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub fn render_directory_listing<F>(
    target: &Path,
    root: &Path,
    display: &str,
    max_depth: usize,
    max_output_size: usize,
    mut allow_entry: F,
) -> Result<String>
where
    F: FnMut(&Path) -> bool,
{
    if target.is_file() {
        return Ok(format!(
            "{display} (file, {})",
            format_size(target.metadata()?.len())
        ));
    }

    let mut out = format!("Directory listing for '{display}' (max_depth: {max_depth}):\n");
    let mut count = 0usize;

    #[allow(clippy::too_many_arguments)]
    fn walk<F: FnMut(&Path) -> bool>(
        dir: &Path,
        root: &Path,
        depth: usize,
        max_depth: usize,
        out: &mut String,
        max_output_size: usize,
        count: &mut usize,
        allow_entry: &mut F,
    ) -> Result<()> {
        let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)
            .with_context(|| format!("Failed to read directory {}", dir.display()))?
            .flatten()
            .collect();

        entries.sort_by(|a, b| {
            let a_dir = a.file_type().is_ok_and(|t| t.is_dir());
            let b_dir = b.file_type().is_ok_and(|t| t.is_dir());
            b_dir
                .cmp(&a_dir)
                .then_with(|| a.file_name().cmp(&b.file_name()))
        });

        let indent = "  ".repeat(depth);
        for entry in entries {
            if out.len() > max_output_size {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" || !allow_entry(&path) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            *count += 1;

            if meta.is_dir() {
                let child_count = fs::read_dir(&path).map_or(0, Iterator::count);
                let _ = writeln!(out, "{indent}{name}/ ({child_count} items)");
                if depth < max_depth
                    && let Ok(canon) = dunce::canonicalize(&path)
                    && canon.starts_with(root)
                {
                    walk(
                        &canon,
                        root,
                        depth + 1,
                        max_depth,
                        out,
                        max_output_size,
                        count,
                        allow_entry,
                    )?;
                }
            } else {
                let _ = writeln!(out, "{indent}{name} ({})", format_size(meta.len()));
            }
        }
        Ok(())
    }

    walk(
        target,
        root,
        1,
        max_depth,
        &mut out,
        max_output_size,
        &mut count,
        &mut allow_entry,
    )?;

    if count == 0 {
        Ok(format!("Directory '{display}' is empty."))
    } else {
        Ok(out.trim_end().to_string())
    }
}

/// Atomically writes `content` to `target_path` via a sibling temporary file,
/// preserving existing file permissions when overwriting an existing file.
pub fn atomic_write_file(target_path: &Path, content: impl AsRef<[u8]>) -> Result<()> {
    let parent = target_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let existing_perms = fs::metadata(target_path).ok().map(|m| m.permissions());

    let file_name = target_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let pid = std::process::id();

    let (mut tmp_file, tmp_path) = (0..64u32)
        .find_map(|attempt| {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let candidate = parent.join(format!(".{file_name}.archon-{pid}-{nanos}-{attempt}.tmp"));
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
                .ok()
                .map(|f| (f, candidate))
        })
        .context("Failed to create temporary file for atomic write")?;

    let write_res = (|| -> Result<()> {
        tmp_file
            .write_all(content.as_ref())
            .context("Error writing to temporary file")?;
        tmp_file
            .sync_all()
            .context("Error syncing temporary file to disk")?;
        drop(tmp_file);

        if let Some(perms) = existing_perms {
            fs::set_permissions(&tmp_path, perms)
                .context("Error preserving file permissions on temporary file")?;
        }

        fs::rename(&tmp_path, target_path).context("Error committing atomic file write")?;
        Ok(())
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }

    write_res
}

pub async fn read_capped<R: AsyncRead + Unpin>(
    mut reader: R,
    cap: u64,
) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    (&mut reader).take(cap).read_to_end(&mut buf).await?;
    Ok(buf)
}

pub fn read_text_file_inspected(path: &Path, max_file_size: u64) -> FileOutcome {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) => return FileOutcome::Error(format!("Error reading file: {e}")),
    };
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => return FileOutcome::Error(format!("Error reading file: {e}")),
    };
    if len == 0 {
        return FileOutcome::Content(String::new());
    }
    if len > max_file_size {
        return FileOutcome::Error("File exceeds maximum size limit.".into());
    }

    let mut chunk = [0u8; 8192];
    let n = match file.read(&mut chunk) {
        Ok(0) => return FileOutcome::Content(String::new()),
        Ok(n) => n,
        Err(e) => return FileOutcome::Error(format!("Error reading file: {e}")),
    };
    let sample = &chunk[..n];
    let ctrl_count = sample
        .iter()
        .filter(|&&b| b < 32 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0C))
        .count();
    if sample.contains(&0) || (ctrl_count as f32 / sample.len() as f32) > 0.3 {
        return FileOutcome::Skipped("Binary file detected.");
    }

    let mut buffer = Vec::with_capacity(len as usize);
    buffer.extend_from_slice(sample);
    if let Err(e) = file.read_to_end(&mut buffer) {
        return FileOutcome::Error(format!("Error reading file: {e}"));
    }

    match String::from_utf8(buffer) {
        Ok(s) => FileOutcome::Content(s),
        Err(_) => FileOutcome::Skipped("Non-UTF-8 text / binary file detected."),
    }
}

pub fn read_utf8_capped(path: &Path, max_file_size: u64) -> Option<String> {
    match read_text_file_inspected(path, max_file_size) {
        FileOutcome::Content(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

pub fn read_file_contents(
    target_path: &Path,
    display_path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_output_size: usize,
) -> Result<String> {
    if !target_path.is_file() {
        bail!("Target is not a file: {display_path}");
    }

    let file = File::open(target_path).context("Error opening file")?;

    // Fast path: full raw file read when no line range is requested
    if start_line.is_none() && end_line.is_none() {
        let cap = (max_output_size as u64).saturating_add(1);
        let mut buf = Vec::new();
        file.take(cap)
            .read_to_end(&mut buf)
            .context("Error reading file")?;
        return Ok(String::from_utf8_lossy(&buf).into_owned());
    }

    let start = start_line.unwrap_or(1).max(1);
    let end = end_line.unwrap_or(usize::MAX);
    if start > end {
        bail!("Invalid line range: start_line ({start}) cannot be greater than end_line ({end})");
    }

    let mut reader = BufReader::new(file);
    let mut raw_line = Vec::new();
    let mut line_no = 0usize;
    let mut shown_start = None;
    let mut shown_end = 0usize;
    let mut out = String::new();
    let mut hit_cap = false;

    loop {
        raw_line.clear();
        let bytes_read = reader
            .read_until(b'\n', &mut raw_line)
            .context("Error reading file line")?;
        if bytes_read == 0 {
            break;
        }
        line_no += 1;

        if line_no >= start && line_no <= end && !hit_cap {
            let text = String::from_utf8_lossy(&raw_line);
            let trimmed = text.trim_end_matches(['\r', '\n']);
            let _ = writeln!(out, "{line_no:>4}: {trimmed}");
            shown_start.get_or_insert(line_no);
            shown_end = line_no;

            if out.len() > max_output_size {
                hit_cap = true;
            }
        }
    }

    if line_no == 0 {
        return Ok("(Empty file)".into());
    }

    if let Some(first) = shown_start {
        let _ = write!(out, "\n[Showing lines {first}..{shown_end} of {line_no}]");
    } else {
        bail!("Requested start_line ({start}) exceeds total lines in file ({line_no})");
    }

    Ok(out)
}
