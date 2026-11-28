use super::{AsyncTool, ToolContext};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;

const CARGO_TIMEOUT_SECS: u64 = 120;
/// `cargo test` compiles test harnesses (and their dependencies) before running,
/// so it is given a more generous timeout than the other diagnostics.
const CARGO_TEST_TIMEOUT_SECS: u64 = 300;
/// Internal raw capture cap when parsing `cargo test` output into a compact
/// summary, ensuring large passing-test logs don't cut off `failures:` blocks.
const CARGO_TEST_RAW_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum number of individual failure output blocks included in a summary.
const MAX_FAILURE_BLOCKS: usize = 20;
/// Maximum lines kept per failure output block (head + tail).
const MAX_FAILURE_LINES: usize = 80;
/// Maximum lines kept for compiler build errors when `cargo test` fails to compile.
const MAX_BUILD_ERROR_LINES: usize = 120;
/// `cargo build --all-targets` performs full codegen and linking for every target
/// and the dependency tree, making it heavier than `check`/`clippy`; it gets the
/// same generous budget as `test`.
const CARGO_BUILD_TIMEOUT_SECS: u64 = 300;
/// `add` and `update` fetch the registry index and re-resolve the dependency
/// graph, so they are network-bound; give them a generous budget to avoid
/// spurious timeouts.
const CARGO_NETWORK_TIMEOUT_SECS: u64 = 300;

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum CargoAction {
    /// Check the package and all targets for compilation errors (`cargo check --all-targets`).
    Check,
    /// Run Clippy lints without modifying code (`cargo clippy --all-targets`).
    Clippy,
    /// Run all tests for the package and its targets (`cargo test --color never`).
    Test,
    /// Verify code formatting without modifying files (`cargo fmt --all --check`).
    FmtCheck,
    /// Format all workspace Rust code in-place (`cargo fmt --all`).
    Fmt,
    /// Build package documentation without dependencies (`cargo doc --no-deps`) to verify doc comments and intra-doc links.
    Doc,
    /// Inspect workspace packages and resolved metadata (`cargo metadata --no-deps --format-version 1`).
    Metadata,
    /// Display information about a package in the registry or dependency graph (`cargo info <package>`).
    Info,
    /// Display the dependency tree (`cargo tree`).
    Tree,
    /// Build the package and all targets (`cargo build --all-targets`).
    Build,
    /// Add a dependency to `Cargo.toml` (`cargo add <name>[@<version>]`).
    Add,
    /// Update dependencies in `Cargo.lock` (`cargo update [<name>] [--precise <version>]`).
    Update,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CargoArgs {
    /// The Cargo action to run.
    action: CargoAction,
    /// Workspace package name (`--package <name>`) for build actions, or the
    /// dependency/crate name (required for `add`, optional for `update`,
    /// required for `info`).
    #[serde(default)]
    package: String,
    /// Optional binary target name (`--bin <name>`).
    #[serde(default)]
    bin: String,
    /// For `test`: optional test name filter substring (`cargo test <filter>`).
    #[serde(default)]
    filter: String,
    /// For `test`: return raw, unsummarized Cargo/libtest output instead of a compact summary.
    #[serde(default)]
    raw: bool,
    /// Optional version: `@version` for `add`, or `--precise` for `update`.
    #[serde(default)]
    version: String,
    /// For `add`: register as a dev-dependency (`--dev`).
    #[serde(default)]
    dev: bool,
    /// For `add`: register as a build-dependency (`--build`).
    #[serde(default)]
    build_dep: bool,
    /// For `add`: comma-separated feature names to enable (`--features <f>`).
    #[serde(default)]
    features: String,
}

fn validate_identifier<'a>(raw: &'a str, field: &str) -> Result<Option<&'a str>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.starts_with('-') {
        bail!("Security violation: '{field}' cannot start with '-': '{trimmed}'");
    }
    if trimmed.contains(['\0', '\n', '\r', ' ', '\t', '/', '\\']) {
        bail!("Invalid characters in '{field}': '{trimmed}'");
    }
    Ok(Some(trimmed))
}

/// Validate a `cargo test` filter string (allows module paths like `a::b` and
/// doc-test paths like `src/lib.rs`, while blocking flag injection and whitespace).
fn validate_test_filter(raw: &str) -> Result<Option<&str>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.starts_with('-') {
        bail!("Security violation: 'filter' cannot start with '-': '{trimmed}'");
    }
    if trimmed.contains(['\0', '\n', '\r', ' ', '\t', '\\']) {
        bail!("Invalid characters in 'filter': '{trimmed}'");
    }
    Ok(Some(trimmed))
}

/// Split a comma-separated `features` list into validated, non-empty names.
fn validate_features(raw: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let validated = match validate_identifier(trimmed, "features")? {
            Some(v) => v,
            None => bail!("empty feature name in '{raw}'"),
        };
        out.push(validated.to_string());
    }
    Ok(out)
}

impl AsyncTool for Tool {
    const NAME: &'static str = "cargo";
    const DESCRIPTION: &'static str = "Executes Cargo commands (check, clippy, test, fmt_check, fmt, doc, metadata, info, tree, build, add, update) within the workspace.";
    const SUBACTIONS: &'static [&'static str] = &[
        "check",
        "clippy",
        "test",
        "fmt_check",
        "fmt",
        "doc",
        "metadata",
        "info",
        "tree",
        "build",
        "add",
        "update",
    ];
    const DEFAULT_ACTION: Option<&'static str> = Some("check");
    const PARAM_HINT: &'static str =
        "<check|clippy|test|fmt|fmt_check|doc|metadata|info|tree|build|add|update> [package]";
    const SHORT_SUMMARY: &'static str = "Run Cargo commands (check, clippy, test, fmt, doc, build, add, update, metadata, info, tree).";
    type Args = CargoArgs;

    fn positional_keys(
        _token: &str,
        _map: &serde_json::Map<String, serde_json::Value>,
    ) -> &'static [&'static str] {
        &["action", "package"]
    }

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let (cmd_args, empty_msg) = build_command_args(&args)?;

        let timeout_secs = match args.action {
            CargoAction::Test => CARGO_TEST_TIMEOUT_SECS,
            CargoAction::Build => CARGO_BUILD_TIMEOUT_SECS,
            CargoAction::Add | CargoAction::Update => CARGO_NETWORK_TIMEOUT_SECS,
            _ => CARGO_TIMEOUT_SECS,
        };

        if matches!(args.action, CargoAction::Test) && !args.raw {
            let mut cmd = tokio::process::Command::new("cargo");
            cmd.current_dir(&ctx.root).args(&cmd_args);
            let cap = (CARGO_TEST_RAW_CAPTURE_BYTES as u64).saturating_add(1);
            let (out_bytes, err_bytes, status) =
                super::run_captured_command(cmd, "cargo", cap, timeout_secs).await?;
            let stdout = String::from_utf8_lossy(&out_bytes);
            let stderr = String::from_utf8_lossy(&err_bytes);
            let filter = validate_test_filter(&args.filter)?;
            return Ok(format_test_output(
                &stdout,
                &stderr,
                status.success(),
                status.code(),
                filter,
            ));
        }

        ctx.run_command_with_timeout("cargo", &cmd_args, empty_msg, timeout_secs)
            .await
    }
}

/// Assemble the full `cargo` argv for a request, returning the arguments and the
/// message to report when the command produces no output.
fn build_command_args(args: &CargoArgs) -> Result<(Vec<String>, &'static str)> {
    let (base_flags, allow_targets, allow_bin, empty_msg): (&[&str], bool, bool, &str) =
        match args.action {
            CargoAction::Check => (
                &["check", "--all-targets", "--color", "never"],
                true,
                true,
                "cargo check passed with no warnings or errors.",
            ),
            CargoAction::Clippy => (
                &["clippy", "--all-targets", "--color", "never"],
                true,
                true,
                "cargo clippy passed with no warnings or errors.",
            ),
            CargoAction::Test => (
                &["test", "--color", "never"],
                true,
                true,
                "All cargo tests passed.",
            ),
            CargoAction::FmtCheck => (
                &["fmt", "--all", "--check"],
                false,
                false,
                "All files are properly formatted.",
            ),
            CargoAction::Fmt => (
                &["fmt", "--all"],
                false,
                false,
                "Formatted all workspace packages.",
            ),
            CargoAction::Doc => (
                &["doc", "--no-deps", "--color", "never"],
                true,
                false,
                "Documentation built successfully with no warnings or errors.",
            ),
            CargoAction::Metadata => (
                &["metadata", "--no-deps", "--format-version", "1"],
                false,
                false,
                "No metadata output.",
            ),
            CargoAction::Info => (
                &["info", "--color", "never"],
                false,
                false,
                "No package information output.",
            ),
            CargoAction::Tree => (
                &["tree", "--color", "never"],
                true,
                false,
                "No dependency tree output.",
            ),
            CargoAction::Build => (
                &["build", "--all-targets", "--color", "never"],
                true,
                true,
                "cargo build completed successfully.",
            ),
            CargoAction::Add => (
                &["add", "--color", "never"],
                false,
                false,
                "Dependency added.",
            ),
            CargoAction::Update => (
                &["update", "--color", "never"],
                false,
                false,
                "Dependencies updated.",
            ),
        };

    let mut cmd_args: Vec<String> = base_flags.iter().map(|&s| s.into()).collect();
    if allow_targets {
        if let Some(pkg) = validate_identifier(&args.package, "package")? {
            cmd_args.extend(["--package".into(), pkg.into()]);
        }
        if allow_bin && let Some(bin) = validate_identifier(&args.bin, "bin")? {
            cmd_args.extend(["--bin".into(), bin.into()]);
        }
    }

    match args.action {
        CargoAction::Test => {
            if let Some(filter) = validate_test_filter(&args.filter)? {
                cmd_args.push(filter.into());
            }
        }
        CargoAction::Info => {
            let Some(pkg) = validate_identifier(&args.package, "package")? else {
                bail!("'package' is required for cargo info");
            };
            cmd_args.push(pkg.into());
        }
        CargoAction::Add => {
            let pkg = match validate_identifier(&args.package, "package")? {
                Some(p) => p,
                None => bail!("'package' is required for cargo add"),
            };
            let spec = if args.version.trim().is_empty() {
                pkg.to_string()
            } else {
                let ver = match validate_identifier(&args.version, "version")? {
                    Some(v) => v,
                    None => bail!("'version' is empty"),
                };
                format!("{pkg}@{ver}")
            };
            cmd_args.push(spec);
            if args.dev {
                cmd_args.push("--dev".into());
            }
            if args.build_dep {
                cmd_args.push("--build".into());
            }
            for feat in validate_features(&args.features)? {
                cmd_args.extend(["--features".into(), feat]);
            }
        }
        CargoAction::Update => {
            if let Some(pkg) = validate_identifier(&args.package, "package")? {
                cmd_args.push(pkg.into());
            }
            if let Some(ver) = validate_identifier(&args.version, "version")? {
                cmd_args.extend(["--precise".into(), ver.into()]);
            }
        }
        _ => {}
    }

    Ok((cmd_args, empty_msg))
}

#[derive(Debug, Default)]
struct SuiteSummary {
    label: Option<String>,
    passed: usize,
    failed: usize,
    ignored: usize,
    measured: usize,
    filtered_out: usize,
    duration_secs: f64,
}

#[derive(Debug)]
struct FailureBlock {
    suite: Option<String>,
    name: String,
    lines: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TestParseSection {
    Tests,
    FailureOutputs,
    FailureList,
}

struct ParsedStderr {
    suite_labels: Vec<String>,
    warning_count: usize,
    compile_errors: Vec<String>,
}

fn parse_test_stderr(stderr: &str, succeeded: bool) -> ParsedStderr {
    let mut suite_labels = Vec::new();
    let mut warning_count = 0usize;
    let mut has_compile_error = false;
    let mut filtered_error_lines = Vec::new();

    for line in stderr.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Running ") {
            let label = rest
                .rsplit_once(" (")
                .filter(|(_, suffix)| suffix.ends_with(')'))
                .map_or(rest, |(left, _)| left);
            suite_labels.push(label.to_string());
            continue;
        }
        if trimmed.starts_with("Doc-tests ") {
            suite_labels.push(trimmed.to_string());
            continue;
        }
        if is_cargo_progress_line(trimmed) {
            continue;
        }
        if trimmed.starts_with("warning: ") && !is_warning_summary_line(trimmed) {
            warning_count += 1;
        }
        if trimmed.starts_with("error[")
            || trimmed.starts_with("error: could not compile ")
            || trimmed.starts_with("error: aborting due to ")
        {
            has_compile_error = true;
        }
        filtered_error_lines.push(line.to_string());
    }

    // Also catch top-level `error:` lines (e.g. manifest/feature/resolution errors)
    // when the command failed without reaching test execution.
    if !succeeded
        && !has_compile_error
        && suite_labels.is_empty()
        && filtered_error_lines
            .iter()
            .any(|l| l.trim_start().starts_with("error:"))
    {
        has_compile_error = true;
    }

    let compile_errors = if !succeeded && has_compile_error {
        trim_blank_edges(filtered_error_lines)
    } else {
        Vec::new()
    };

    ParsedStderr {
        suite_labels,
        warning_count,
        compile_errors,
    }
}

fn is_cargo_progress_line(trimmed: &str) -> bool {
    [
        "Compiling ",
        "Checking ",
        "Fresh ",
        "Finished ",
        "Locking ",
        "Updating ",
        "Downloading ",
        "Downloaded ",
        "Blocking waiting for file lock ",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
}

fn is_warning_summary_line(trimmed: &str) -> bool {
    trimmed.starts_with("warning: ")
        && trimmed.contains(" generated ")
        && (trimmed.ends_with(" warning") || trimmed.ends_with(" warnings"))
}

fn parse_running_count(trimmed: &str) -> Option<usize> {
    let rest = trimmed.strip_prefix("running ")?;
    let num = rest
        .strip_suffix(" tests")
        .or_else(|| rest.strip_suffix(" test"))?;
    num.trim().parse().ok()
}

fn parse_test_result_line(trimmed: &str) -> Option<SuiteSummary> {
    let rest = trimmed.strip_prefix("test result: ")?;
    let (_, stats) = rest.split_once(". ")?;

    let mut summary = SuiteSummary::default();
    for part in stats.split(';') {
        let p = part.trim();
        if let Some(n) = p.strip_suffix(" passed") {
            summary.passed = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = p.strip_suffix(" failed") {
            summary.failed = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = p.strip_suffix(" ignored") {
            summary.ignored = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = p.strip_suffix(" measured") {
            summary.measured = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = p.strip_suffix(" filtered out") {
            summary.filtered_out = n.trim().parse().unwrap_or(0);
        } else if let Some(s) = p
            .strip_prefix("finished in ")
            .and_then(|s| s.strip_suffix('s'))
        {
            summary.duration_secs = s.trim().parse().unwrap_or(0.0);
        }
    }
    Some(summary)
}

fn parse_failure_stdout_header(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix("---- ")
        .and_then(|s| s.strip_suffix(" stdout ----"))
}

fn trim_blank_edges(mut lines: Vec<String>) -> Vec<String> {
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines
}

fn cap_block_lines(lines: Vec<String>, max_lines: usize, hint: &str) -> Vec<String> {
    let trimmed = trim_blank_edges(lines);
    if trimmed.len() <= max_lines {
        return trimmed;
    }
    let head_len = (max_lines * 2) / 3;
    let tail_len = max_lines.saturating_sub(head_len + 1).max(1);
    let omitted = trimmed.len().saturating_sub(head_len + tail_len);
    let mut out = Vec::with_capacity(max_lines);
    out.extend(trimmed[..head_len].iter().cloned());
    out.push(format!("... ({omitted} lines omitted{hint}) ..."));
    out.extend(trimmed[trimmed.len() - tail_len..].iter().cloned());
    out
}

fn tail_lines(text: &str, max_lines: usize) -> Vec<String> {
    let all: Vec<String> = text.lines().map(ToString::to_string).collect();
    let trimmed = trim_blank_edges(all);
    if trimmed.len() <= max_lines {
        trimmed
    } else {
        let start = trimmed.len() - max_lines;
        let mut out = vec![format!("... ({start} earlier lines omitted) ...")];
        out.extend(trimmed[start..].iter().cloned());
        out
    }
}

fn format_test_output(
    stdout: &str,
    stderr: &str,
    succeeded: bool,
    exit_code: Option<i32>,
    filter: Option<&str>,
) -> String {
    let stderr_info = parse_test_stderr(stderr, succeeded);

    let mut suites: Vec<SuiteSummary> = Vec::new();
    let mut failure_blocks: Vec<FailureBlock> = Vec::new();
    let mut all_failed_names: Vec<(Option<String>, String)> = Vec::new();

    let mut section = TestParseSection::Tests;
    let mut suite_idx = 0usize;
    let mut current_label: Option<String> = stderr_info.suite_labels.first().cloned();
    let mut current_failed_names: Vec<String> = Vec::new();
    let mut current_failure: Option<(String, Vec<String>)> = None;
    let mut saw_suite_start = false;

    for line in stdout.lines() {
        let trimmed = line.trim();

        if section != TestParseSection::FailureOutputs && parse_running_count(trimmed).is_some() {
            current_label = stderr_info.suite_labels.get(suite_idx).cloned();
            current_failed_names.clear();
            section = TestParseSection::Tests;
            saw_suite_start = true;
            continue;
        }

        match section {
            TestParseSection::Tests => {
                if let Some(rest) = trimmed.strip_prefix("test ")
                    && let Some((name, outcome)) = rest.rsplit_once(" ... ")
                {
                    if outcome == "FAILED" {
                        let name_str = name.to_string();
                        current_failed_names.push(name_str.clone());
                        all_failed_names.push((current_label.clone(), name_str));
                    }
                    continue;
                }
                if trimmed == "failures:" {
                    section = TestParseSection::FailureOutputs;
                    continue;
                }
                if let Some(mut summary) = parse_test_result_line(trimmed) {
                    summary.label = current_label.take();
                    suites.push(summary);
                    suite_idx += 1;
                    saw_suite_start = false;
                    current_failed_names.clear();
                    section = TestParseSection::Tests;
                }
            }
            TestParseSection::FailureOutputs => {
                if let Some(name) = parse_failure_stdout_header(trimmed)
                    && (current_failure.is_none() || current_failed_names.iter().any(|f| f == name))
                {
                    if let Some((prev_name, prev_lines)) = current_failure.take() {
                        failure_blocks.push(FailureBlock {
                            suite: current_label.clone(),
                            name: prev_name,
                            lines: prev_lines,
                        });
                    }
                    current_failure = Some((name.to_string(), Vec::new()));
                    continue;
                }
                if trimmed == "failures:" {
                    if let Some((prev_name, prev_lines)) = current_failure.take() {
                        failure_blocks.push(FailureBlock {
                            suite: current_label.clone(),
                            name: prev_name,
                            lines: prev_lines,
                        });
                    }
                    section = TestParseSection::FailureList;
                    continue;
                }
                if let Some(mut summary) = parse_test_result_line(trimmed) {
                    if let Some((prev_name, prev_lines)) = current_failure.take() {
                        failure_blocks.push(FailureBlock {
                            suite: current_label.clone(),
                            name: prev_name,
                            lines: prev_lines,
                        });
                    }
                    summary.label = current_label.take();
                    suites.push(summary);
                    suite_idx += 1;
                    saw_suite_start = false;
                    current_failed_names.clear();
                    section = TestParseSection::Tests;
                    continue;
                }
                if let Some((_, ref mut lines)) = current_failure {
                    lines.push(line.to_string());
                }
            }
            TestParseSection::FailureList => {
                if let Some(mut summary) = parse_test_result_line(trimmed) {
                    summary.label = current_label.take();
                    suites.push(summary);
                    suite_idx += 1;
                    saw_suite_start = false;
                    current_failed_names.clear();
                    section = TestParseSection::Tests;
                }
            }
        }
    }

    if let Some((prev_name, prev_lines)) = current_failure.take() {
        failure_blocks.push(FailureBlock {
            suite: current_label.clone(),
            name: prev_name,
            lines: prev_lines,
        });
    }

    let total_passed: usize = suites.iter().map(|s| s.passed).sum();
    let total_failed: usize = suites
        .iter()
        .map(|s| s.failed)
        .sum::<usize>()
        .max(all_failed_names.len());
    let total_ignored: usize = suites.iter().map(|s| s.ignored).sum();
    let total_filtered: usize = suites.iter().map(|s| s.filtered_out).sum();
    let total_secs: f64 = suites.iter().map(|s| s.duration_secs).sum();

    let mut out = String::new();
    let status_word = if succeeded { "PASSED" } else { "FAILED" };

    if suites.is_empty() && !succeeded && !stderr_info.compile_errors.is_empty() {
        out.push_str("cargo test: FAILED (compilation error)");
    } else if suites.is_empty() && !succeeded {
        out.push_str("cargo test: FAILED");
        if saw_suite_start && let Some(ref lbl) = current_label {
            out.push_str(&format!(" (aborted in {lbl})"));
        }
    } else {
        out.push_str(&format!(
            "cargo test: {status_word} — {total_passed} passed, {total_failed} failed, {total_ignored} ignored"
        ));
        if total_filtered > 0 {
            out.push_str(&format!(", {total_filtered} filtered out"));
        }
        let suite_word = if suites.len() == 1 { "suite" } else { "suites" };
        out.push_str(&format!(
            " ({} {suite_word}, {total_secs:.2}s)",
            suites.len()
        ));
    }

    if let Some(f) = filter {
        out.push_str(&format!(" [filter: {f}]"));
    }
    if stderr_info.warning_count > 0 {
        let w_word = if stderr_info.warning_count == 1 {
            "warning"
        } else {
            "warnings"
        };
        out.push_str(&format!(
            " | {} compiler {w_word}",
            stderr_info.warning_count
        ));
    }

    if suites.len() > 1 {
        let non_empty: Vec<&SuiteSummary> = suites
            .iter()
            .filter(|s| s.passed > 0 || s.failed > 0 || s.ignored > 0)
            .collect();
        if !non_empty.is_empty() {
            out.push_str("\n\nSuites:");
            for s in non_empty {
                let label = s.label.as_deref().unwrap_or("suite");
                out.push_str(&format!(
                    "\n  {label}: {} passed, {} failed, {} ignored ({:.2}s)",
                    s.passed, s.failed, s.ignored, s.duration_secs
                ));
            }
        }
    }

    if succeeded
        && total_passed == 0
        && total_failed == 0
        && total_ignored == 0
        && total_filtered > 0
    {
        out.push_str("\nNo tests matched the filter.");
    }

    // Append detailed failure blocks (up to MAX_FAILURE_BLOCKS), plus any failed
    // test names that lacked a `---- ... stdout ----` block.
    let mut rendered_names: Vec<String> = Vec::new();
    let total_failures = failure_blocks.len().max(all_failed_names.len());
    if total_failures > 0 {
        out.push_str(&format!("\n\nFailures ({total_failures}):"));
        for (idx, block) in failure_blocks.iter().take(MAX_FAILURE_BLOCKS).enumerate() {
            rendered_names.push(block.name.clone());
            let prefix = block
                .suite
                .as_deref()
                .map_or_else(String::new, |s| format!("[{s}] "));
            out.push_str(&format!("\n\n{}. {prefix}{}", idx + 1, block.name));
            let hint = format!("; re-run with filter=\"{}\" raw=true", block.name);
            let capped = cap_block_lines(block.lines.clone(), MAX_FAILURE_LINES, &hint);
            for line in capped {
                out.push_str("\n   ");
                out.push_str(&line);
            }
        }

        let mut next_idx = failure_blocks.len().min(MAX_FAILURE_BLOCKS);
        for (suite, name) in &all_failed_names {
            if rendered_names.iter().any(|r| r == name) {
                continue;
            }
            next_idx += 1;
            let prefix = suite
                .as_deref()
                .map_or_else(String::new, |s| format!("[{s}] "));
            out.push_str(&format!("\n{next_idx}. {prefix}{name}"));
        }
    }

    let has_compile_errors = !stderr_info.compile_errors.is_empty();
    if has_compile_errors {
        out.push_str("\n\nBuild errors:\n");
        let capped = cap_block_lines(
            stderr_info.compile_errors,
            MAX_BUILD_ERROR_LINES,
            "; re-run with raw=true for full stderr",
        );
        out.push_str(&capped.join("\n"));
    }

    // Fallback: non-zero exit with no parsed test failures and no compile errors
    // (e.g. a test aborted/segfaulted or called `process::exit`).
    if !succeeded && failure_blocks.is_empty() && all_failed_names.is_empty() && !has_compile_errors
    {
        if !stderr.trim().is_empty() {
            out.push_str("\n\n[STDERR tail]\n");
            out.push_str(&tail_lines(stderr, 40).join("\n"));
        }
        if !stdout.trim().is_empty() {
            out.push_str("\n\n[STDOUT tail]\n");
            out.push_str(&tail_lines(stdout, 40).join("\n"));
        }
        match exit_code {
            Some(code) => out.push_str(&format!("\n[Exit code: {code}]")),
            None => out.push_str("\n[Process terminated by signal]"),
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::{CargoArgs, build_command_args, format_test_output};

    fn cargo_args(
        action: &str,
        package: &str,
        version: &str,
        dev: bool,
        build_dep: bool,
        features: &str,
    ) -> CargoArgs {
        CargoArgs {
            action: serde_json::from_value(serde_json::json!(action)).unwrap(),
            package: package.into(),
            bin: String::new(),
            filter: String::new(),
            raw: false,
            version: version.into(),
            dev,
            build_dep,
            features: features.into(),
        }
    }

    #[test]
    fn cargo_args_rejects_unknown_fields() {
        let err = crate::tools::deserialize_tool_args(
            "cargo",
            &serde_json::json!({
                "action": "info",
                "crate_name": "eframe"
            }),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Invalid arguments provided to cargo"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains(
                "unknown field `crate_name`, expected one of `action`, `package`, `bin`, `filter`, `raw`, `version`, `dev`, `build_dep`, `features`"
            ),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn add_builds_expected_argv() {
        let args = cargo_args("add", "serde", "1.0", true, false, "derive, alloc");
        let (argv, empty_msg) = build_command_args(&args).unwrap();
        assert_eq!(
            argv,
            vec![
                "add",
                "--color",
                "never",
                "serde@1.0",
                "--dev",
                "--features",
                "derive",
                "--features",
                "alloc",
            ]
        );
        assert_eq!(empty_msg, "Dependency added.");
    }

    #[test]
    fn add_without_version_or_flags() {
        let args = cargo_args("add", "serde", "", false, false, "");
        let (argv, _) = build_command_args(&args).unwrap();
        assert_eq!(argv, vec!["add", "--color", "never", "serde"]);
    }

    #[test]
    fn update_all_and_precise() {
        let all = cargo_args("update", "", "", false, false, "");
        assert_eq!(
            build_command_args(&all).unwrap().0,
            vec!["update", "--color", "never"]
        );

        let precise = cargo_args("update", "serde", "1.0.200", false, false, "");
        assert_eq!(
            build_command_args(&precise).unwrap().0,
            vec![
                "update",
                "--color",
                "never",
                "serde",
                "--precise",
                "1.0.200"
            ]
        );
    }

    #[test]
    fn add_rejects_flag_injection() {
        // A package name that would inject a flag.
        let evil_pkg = cargo_args("add", "--config=evil", "", false, false, "");
        assert!(build_command_args(&evil_pkg).is_err());

        // A feature entry that would inject a flag.
        let evil_feat = cargo_args("add", "serde", "", false, false, "derive,--features=x");
        assert!(build_command_args(&evil_feat).is_err());
    }

    #[test]
    fn add_requires_package() {
        let no_pkg = cargo_args("add", "", "", false, false, "");
        assert!(build_command_args(&no_pkg).is_err());
    }

    #[test]
    fn test_filter_builds_argv_and_rejects_flag_injection() {
        let mut args = cargo_args("test", "archon", "", false, false, "");
        args.filter = "engine::runner::tests::handles_abort".into();
        let (argv, _) = build_command_args(&args).unwrap();
        assert_eq!(
            argv,
            vec![
                "test",
                "--color",
                "never",
                "--package",
                "archon",
                "engine::runner::tests::handles_abort",
            ]
        );

        let mut evil = cargo_args("test", "", "", false, false, "");
        evil.filter = "--nocapture".into();
        assert!(build_command_args(&evil).is_err());
    }

    #[test]
    fn format_test_output_summarizes_multi_suite_passes_and_warnings() {
        let stderr = "\
   Compiling archon v0.3.0 (/repo)
warning: unused variable: `x`
  --> src/lib.rs:10:9
warning: `archon` (lib test) generated 1 warning
    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.23s
     Running unittests src/lib.rs (target/debug/deps/archon-1234)
     Running tests/cli.rs (target/debug/deps/cli-5678)
   Doc-tests archon
";
        let stdout = "\
running 3 tests
test alpha::one ... ok
test alpha::two ... ok
test alpha::three ... ignored, slow

test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.12s

running 2 tests
test cli_help ... ok
test cli_version ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        let formatted = format_test_output(stdout, stderr, true, Some(0), None);
        assert!(formatted.contains(
            "cargo test: PASSED — 4 passed, 0 failed, 1 ignored (3 suites, 0.20s) | 1 compiler warning"
        ));
        assert!(formatted.contains("unittests src/lib.rs: 2 passed, 0 failed, 1 ignored (0.12s)"));
        assert!(formatted.contains("tests/cli.rs: 2 passed, 0 failed, 0 ignored (0.08s)"));
        assert!(!formatted.contains("alpha::one"));
        assert!(!formatted.contains("Compiling archon"));
    }

    #[test]
    fn format_test_output_includes_failure_blocks_and_ignores_fake_headers() {
        let stderr = "\
     Running unittests src/lib.rs (target/debug/deps/archon-1234)
     Running tests/cli.rs (target/debug/deps/cli-5678)
";
        let stdout = "\
running 2 tests
test alpha::passes ... ok
test alpha::breaks ... FAILED

failures:

---- alpha::breaks stdout ----
---- fake_nested_header stdout ----
thread 'alpha::breaks' panicked at src/lib.rs:42:5:
assertion `left == right` failed
  left: 1
 right: 2

failures:
    alpha::breaks

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s

running 1 test
test cli_ok ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let formatted = format_test_output(stdout, stderr, false, Some(101), None);
        assert!(
            formatted
                .contains("cargo test: FAILED — 2 passed, 1 failed, 0 ignored (2 suites, 0.06s)")
        );
        assert!(formatted.contains("Failures (1):"));
        assert!(formatted.contains("1. [unittests src/lib.rs] alpha::breaks"));
        assert!(formatted.contains("---- fake_nested_header stdout ----"));
        assert!(formatted.contains("left: 1"));
        assert!(!formatted.contains("alpha::passes"));
        assert!(!formatted.contains("cli_ok"));
    }

    #[test]
    fn format_test_output_surfaces_compile_errors() {
        let stderr = "\
   Compiling archon v0.3.0 (/repo)
error[E0308]: mismatched types
  --> src/lib.rs:5:12
   |
 5 |     return 42;
   |            ^^ expected `()`, found integer

error: could not compile `archon` (lib test) due to 1 previous error
";
        let formatted = format_test_output("", stderr, false, Some(101), None);
        assert!(formatted.starts_with("cargo test: FAILED (compilation error)"));
        assert!(formatted.contains("Build errors:"));
        assert!(formatted.contains("error[E0308]: mismatched types"));
        assert!(!formatted.contains("Compiling archon"));
    }

    #[test]
    fn format_test_output_handles_unparsed_abort_and_zero_filter_matches() {
        let aborted = format_test_output(
            "running 1 test\n",
            "Running unittests src/lib.rs (target/debug/deps/archon-1)\nfatal runtime error: abort\n",
            false,
            None,
            Some("abort_test"),
        );
        assert!(
            aborted.contains(
                "cargo test: FAILED (aborted in unittests src/lib.rs) [filter: abort_test]"
            )
        );
        assert!(aborted.contains("fatal runtime error: abort"));
        assert!(aborted.contains("[Process terminated by signal]"));

        let no_matches = format_test_output(
            "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s\n",
            "Running unittests src/lib.rs (target/debug/deps/archon-1)\n",
            true,
            Some(0),
            Some("nonexistent"),
        );
        assert!(no_matches.contains("0 passed, 0 failed, 0 ignored, 1 filtered out"));
        assert!(no_matches.contains("No tests matched the filter."));
    }
}
