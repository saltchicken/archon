use super::{AsyncTool, ToolContext};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;

const CARGO_TIMEOUT_SECS: u64 = 120;
/// `cargo test` compiles test harnesses (and their dependencies) before running,
/// so it is given a more generous timeout than the other diagnostics.
const CARGO_TEST_TIMEOUT_SECS: u64 = 300;

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
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CargoArgs {
    /// The Cargo action to run.
    action: CargoAction,
    /// Optional workspace package name (`--package <name>`), or crate name (required when `action` is `info`).
    #[serde(default)]
    package: String,
    /// Optional binary target name (`--bin <name>`).
    #[serde(default)]
    bin: String,
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

impl AsyncTool for Tool {
    const NAME: &'static str = "cargo";
    const DESCRIPTION: &'static str = "Executes Cargo commands (check, clippy, test, fmt_check, fmt, doc, metadata, info, tree) within the workspace.";
    type Args = CargoArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
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

        if matches!(args.action, CargoAction::Info) {
            let Some(pkg) = validate_identifier(&args.package, "package")? else {
                bail!("'package' is required for cargo info");
            };
            cmd_args.push(pkg.into());
        }

        let timeout_secs = match args.action {
            CargoAction::Test => CARGO_TEST_TIMEOUT_SECS,
            _ => CARGO_TIMEOUT_SECS,
        };

        ctx.run_command_with_timeout("cargo", &cmd_args, empty_msg, timeout_secs)
            .await
    }
}

#[cfg(test)]
mod tests {
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
            msg.contains("unknown field `crate_name`, expected one of `action`, `package`, `bin`"),
            "unexpected error: {msg}"
        );
    }
}
