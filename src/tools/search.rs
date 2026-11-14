use super::{
    BlockingTool, ToolContext,
    utils::search::{SearchExecutionParams, search_candidate_files},
};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Regular expression (or literal string when `fixed_strings: true`) to search for across files.
    pattern: String,
    /// Optional relative directory or file path within the workspace to restrict the search scope (defaults to workspace root).
    #[serde(default)]
    path: String,
    /// Optional glob patterns to filter which files are searched (e.g. `["*.rs", "src/**/*.toml"]`).
    #[serde(default)]
    include: Vec<String>,
    /// Optional glob patterns to exclude from search.
    #[serde(default)]
    exclude: Vec<String>,
    /// If true, treats `pattern` as a literal string instead of a regular expression.
    #[serde(default)]
    fixed_strings: bool,
    /// If true, returns only the list of matching file paths with their match counts (sorted by match count descending) instead of individual lines, avoiding early line-limit truncation on broad queries.
    #[serde(default)]
    files_with_matches: bool,
    /// Optional case sensitivity override. Defaults to smart-case (case-insensitive unless `pattern` contains uppercase characters).
    #[serde(default)]
    case_sensitive: Option<bool>,
    /// Number of surrounding context lines to show before and after each match (0 to 5, default: 0). Ignored when `files_with_matches: true`.
    #[serde(default)]
    context_lines: Option<usize>,
    /// Maximum number of matching lines (or matching files when `files_with_matches: true`) to return across the workspace (default: 100, max: 500).
    #[serde(default)]
    max_results: Option<usize>,
}

impl BlockingTool for Tool {
    const NAME: &'static str = "search";
    const DESCRIPTION: &'static str = "Searches file contents in the workspace using regular expressions or literal strings. Use `files_with_matches` to survey all matching files and counts across large codebases. For AST symbol definitions or call sites, prefer `repo_map` (`action: \"definition\"` or `\"references\"`). Respects .gitignore and outputs 1-based line numbers.";
    type Args = SearchArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        if args.pattern.is_empty() {
            bail!("'pattern' cannot be empty");
        }

        let (base_root, scan_root) =
            ctx.resolve_scan_roots(&args.path, "path escapes workspace root")?;

        let candidate_files =
            ctx.walk_workspace_files(&base_root, &scan_root, &args.include, &args.exclude)?;

        search_candidate_files(
            candidate_files,
            &SearchExecutionParams {
                pattern: &args.pattern,
                fixed_strings: args.fixed_strings,
                files_with_matches: args.files_with_matches,
                case_sensitive: args.case_sensitive,
                context_lines: args.context_lines,
                max_results: args.max_results,
                max_file_size: ctx.snapshot.max_size,
                max_output_size: ctx.max_output_size,
            },
        )
    }
}
