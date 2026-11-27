use super::{AsyncTool, ToolContext};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;

const CARGO_TIMEOUT_SECS: u64 = 120;
/// `cargo test` compiles test harnesses (and their dependencies) before running,
/// so it is given a more generous timeout than the other diagnostics.
const CARGO_TEST_TIMEOUT_SECS: u64 = 300;
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
    type Args = CargoArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let (cmd_args, empty_msg) = build_command_args(&args)?;

        let timeout_secs = match args.action {
            CargoAction::Test => CARGO_TEST_TIMEOUT_SECS,
            CargoAction::Build => CARGO_BUILD_TIMEOUT_SECS,
            CargoAction::Add | CargoAction::Update => CARGO_NETWORK_TIMEOUT_SECS,
            _ => CARGO_TIMEOUT_SECS,
        };

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

#[cfg(test)]
mod tests {
    use super::{CargoArgs, build_command_args};

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
                "unknown field `crate_name`, expected one of `action`, `package`, `bin`, `version`, `dev`, `build_dep`, `features`"
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
}
