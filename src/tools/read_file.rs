use crate::tools::{auto_schema, parse_args, resolve_existing_path};
use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{
    fmt::Write as _,
    fs::File,
    io::{BufRead, BufReader, Read},
};

#[derive(Deserialize, JsonSchema)]
struct ReadFileArgs {
    /// Relative path to the file in the workspace.
    file_path: String,
    /// Optional 1-based starting line number (inclusive). If set, output lines are prefixed with line numbers.
    #[serde(default)]
    start_line: Option<usize>,
    /// Optional 1-based ending line number (inclusive). If set, output lines are prefixed with line numbers.
    #[serde(default)]
    end_line: Option<usize>,
}

pub fn schema() -> Value {
    auto_schema::<ReadFileArgs>(
        "read_file",
        "Read the contents of a file in the workspace, optionally restricted to a 1-based line range [start_line, end_line].",
    )
}

pub fn execute(json_args: &Value, max_output_size: usize) -> Result<String> {
    let args: ReadFileArgs = parse_args(json_args, "read_file")?;
    let (_, target_path) = resolve_existing_path(&args.file_path)?;

    if !target_path.is_file() {
        bail!("Target is not a file: {}", args.file_path.trim());
    }

    let file = File::open(&target_path).context("Error opening file")?;

    // Fast path: full raw file read when no line range is requested
    if args.start_line.is_none() && args.end_line.is_none() {
        let cap = (max_output_size as u64).saturating_add(1);
        let mut buf = Vec::new();
        file.take(cap)
            .read_to_end(&mut buf)
            .context("Error reading file")?;
        return Ok(String::from_utf8_lossy(&buf).into_owned());
    }

    let start = args.start_line.unwrap_or(1).max(1);
    let end = args.end_line.unwrap_or(usize::MAX);
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
