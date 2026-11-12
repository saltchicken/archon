use crate::tools::{auto_schema, parse_args, run_workspace_command_with_timeout};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const CARGO_TIMEOUT_SECS: u64 = 120;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum CargoAction {
    /// Check the package and all targets for compilation errors (`cargo check --locked --all-targets`).
    Check,
    /// Run Clippy lints without modifying code (`cargo clippy --locked --all-targets`).
    Clippy,
    /// Compile and run unit/integration tests (`cargo test --locked`). Set `no_run: true` to only compile tests.
    Test,
    /// Verify code formatting without modifying files (`cargo fmt --all --check`).
    FmtCheck,
    /// Format all workspace Rust code in-place (`cargo fmt --all`).
    Fmt,
    /// Build package documentation without dependencies (`cargo doc --locked --no-deps`) to verify doc comments and intra-doc links.
    Doc,
    /// Inspect workspace packages and resolved metadata (`cargo metadata --locked --no-deps --format-version 1`).
    Metadata,
}

#[derive(Deserialize, JsonSchema)]
struct CargoArgs {
    /// The Cargo action to run.
    action: CargoAction,
    /// Optional workspace package name (`--package <name>`).
    #[serde(default)]
    package: String,
    /// Optional binary target name (`--bin <name>`).
    #[serde(default)]
    bin: String,
    /// Optional test name filter (only used when `action` is `test`).
    #[serde(default)]
    test_filter: String,
    /// If true during `test`, compiles test binaries without executing them (`--no-run`).
    #[serde(default)]
    no_run: bool,
}

pub fn schema() -> Value {
    auto_schema::<CargoArgs>(
        "cargo",
        "Executes Cargo commands (check, clippy, test, fmt_check, fmt, doc, metadata) within the workspace.",
    )
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

fn push_target_filters(cmd_args: &mut Vec<String>, args: &CargoArgs) -> Result<()> {
    if let Some(pkg) = validate_identifier(&args.package, "package")? {
        cmd_args.extend(["--package".into(), pkg.into()]);
    }
    if let Some(bin) = validate_identifier(&args.bin, "bin")? {
        cmd_args.extend(["--bin".into(), bin.into()]);
    }
    Ok(())
}

pub async fn execute(json_args: &Value, max_output_size: usize) -> Result<String> {
    let args: CargoArgs = parse_args(json_args, "cargo")?;
    let mut cmd_args: Vec<String> = Vec::new();

    let empty_msg = match args.action {
        CargoAction::Check => {
            cmd_args.extend([
                "check".into(),
                "--locked".into(),
                "--all-targets".into(),
                "--color".into(),
                "never".into(),
            ]);
            push_target_filters(&mut cmd_args, &args)?;
            "cargo check passed with no warnings or errors."
        }
        CargoAction::Clippy => {
            cmd_args.extend([
                "clippy".into(),
                "--locked".into(),
                "--all-targets".into(),
                "--color".into(),
                "never".into(),
            ]);
            push_target_filters(&mut cmd_args, &args)?;
            "cargo clippy passed with no warnings or errors."
        }
        CargoAction::Test => {
            cmd_args.extend([
                "test".into(),
                "--locked".into(),
                "--color".into(),
                "never".into(),
            ]);
            push_target_filters(&mut cmd_args, &args)?;
            if args.no_run {
                cmd_args.push("--no-run".into());
            }
            if let Some(filter) = validate_identifier(&args.test_filter, "test_filter")? {
                cmd_args.push(filter.into());
            }
            if !args.no_run {
                cmd_args.extend(["--".into(), "--nocapture".into()]);
            }
            "cargo test completed successfully."
        }
        CargoAction::FmtCheck => {
            cmd_args.extend(["fmt".into(), "--all".into(), "--check".into()]);
            "All files are properly formatted."
        }
        CargoAction::Fmt => {
            cmd_args.extend(["fmt".into(), "--all".into()]);
            "Formatted all workspace packages."
        }
        CargoAction::Doc => {
            cmd_args.extend([
                "doc".into(),
                "--locked".into(),
                "--no-deps".into(),
                "--color".into(),
                "never".into(),
            ]);
            if let Some(pkg) = validate_identifier(&args.package, "package")? {
                cmd_args.extend(["--package".into(), pkg.into()]);
            }
            "Documentation built successfully with no warnings or errors."
        }
        CargoAction::Metadata => {
            cmd_args.extend([
                "metadata".into(),
                "--locked".into(),
                "--no-deps".into(),
                "--format-version".into(),
                "1".into(),
            ]);
            "No metadata output."
        }
    };

    run_workspace_command_with_timeout(
        "cargo",
        &cmd_args,
        empty_msg,
        max_output_size,
        CARGO_TIMEOUT_SECS,
    )
    .await
}
