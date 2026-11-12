use crate::{
    core::config::WorkspaceSnapshotSettings,
    tools::{auto_schema, parse_args, validate_relative_path, workspace_root},
};
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use pathdiff::diff_paths;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};
use tree_sitter::{Language, Node, Parser};

const PAGERANK_ITERATIONS: usize = 25;
const PAGERANK_DAMPING: f64 = 0.85;

#[derive(Deserialize, JsonSchema)]
struct RepoMapArgs {
    /// Optional relative directory or file path within the workspace to scope the repo map (defaults to workspace root).
    #[serde(default)]
    target_path: String,
    /// Optional focus symbols or query terms (e.g. `["CoreEngine", "Provider"]`). Boosts PageRank personalization toward files defining or referencing these symbols.
    #[serde(default)]
    focus_symbols: Vec<String>,
    /// Optional glob patterns to filter included files (e.g. `["src/**/*.rs", "**/*.ts"]`).
    #[serde(default)]
    include: Vec<String>,
    /// Optional glob patterns to exclude from the repo map.
    #[serde(default)]
    exclude: Vec<String>,
    /// Whether to include leading doc comments above AST symbol signatures (default: true).
    #[serde(default = "default_true")]
    include_docs: bool,
    /// Approximate token budget for the rendered repository map (default: 8000, min: 500, max: 64000).
    #[serde(default)]
    max_tokens: Option<usize>,
}

fn default_true() -> bool {
    true
}

pub fn schema() -> Value {
    auto_schema::<RepoMapArgs>(
        "repo_map",
        "Generates an AST-accurate, PageRank-ranked repository map across Rust, TypeScript/JavaScript, Python, and Go files. Extracts structs, traits, enums, classes, interfaces, functions, methods, doc comments, and 1-based line ranges while ranking files and symbols by cross-file reference centrality.",
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SupportedLang {
    Rust,
    TypeScript,
    Tsx,
    Python,
    Go,
}

impl SupportedLang {
    fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some(Self::Rust),
            "ts" | "js" | "mjs" | "cjs" => Some(Self::TypeScript),
            "tsx" | "jsx" => Some(Self::Tsx),
            "py" => Some(Self::Python),
            "go" => Some(Self::Go),
            _ => None,
        }
    }

    fn ts_language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    fn code_fence_tag(self) -> &'static str {
        match self {
            Self::Rust => "rs",
            Self::TypeScript => "ts",
            Self::Tsx => "tsx",
            Self::Python => "py",
            Self::Go => "go",
        }
    }
}

#[derive(Clone)]
struct SymbolDef {
    name: String,
    kind: &'static str,
    signature: String,
    doc: Option<String>,
    start_line: usize,
    end_line: usize,
    depth: usize,
    cross_file_refs: usize,
    score: f64,
}

struct ParsedFile {
    display_path: String,
    lang: SupportedLang,
    total_lines: usize,
    defs: Vec<SymbolDef>,
    ident_counts: HashMap<String, usize>,
    pagerank: f64,
}

fn find_git_root(start_path: &Path) -> Option<PathBuf> {
    start_path
        .ancestors()
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        if pat.contains('/') || pat.contains('\\') {
            builder
                .add(Glob::new(pat).with_context(|| format!("Invalid glob pattern: {pat}"))?);
        } else {
            builder
                .add(Glob::new(pat).with_context(|| format!("Invalid glob pattern: {pat}"))?);
            let recursive = format!("**/{pat}");
            builder.add(
                Glob::new(&recursive)
                    .with_context(|| format!("Invalid glob pattern: {recursive}"))?,
            );
        }
    }
    Ok(builder.build()?)
}

fn is_noise_identifier(name: &str) -> bool {
    if name.len() < 2 {
        return true;
    }
    matches!(
        name,
        "self"
            | "Self"
            | "super"
            | "crate"
            | "new"
            | "default"
            | "from"
            | "into"
            | "clone"
            | "iter"
            | "next"
            | "len"
            | "is_empty"
            | "push"
            | "get"
            | "set"
            | "fmt"
            | "Result"
            | "Option"
            | "Some"
            | "None"
            | "Ok"
            | "Err"
            | "String"
            | "Vec"
            | "Box"
            | "Arc"
            | "Path"
            | "PathBuf"
            | "Value"
            | "Error"
            | "Context"
            | "true"
            | "false"
            | "main"
            | "init"
            | "test"
    )
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_leading_docs(node: Node<'_>, source: &str) -> Option<String> {
    let mut docs = Vec::new();
    let mut cur = node.prev_sibling();
    let mut expected_end_row = node.start_position().row;

    while let Some(sib) = cur {
        let kind = sib.kind();
        let sib_end_row = sib.end_position().row;
        let sib_start_row = sib.start_position().row;

        if expected_end_row.saturating_sub(sib_end_row) > 1 {
            break;
        }

        if matches!(kind, "attribute_item" | "inner_attribute_item" | "decorator") {
            expected_end_row = sib_start_row;
            cur = sib.prev_sibling();
            continue;
        }

        if kind.contains("comment") {
            let raw = &source[sib.byte_range()];
            for line in raw.lines().rev() {
                let t = line.trim();
                let cleaned = t
                    .strip_prefix("///")
                    .or_else(|| t.strip_prefix("//!"))
                    .or_else(|| t.strip_prefix("//"))
                    .or_else(|| t.strip_prefix('#'))
                    .or_else(|| t.strip_prefix("/**"))
                    .or_else(|| t.strip_prefix("/*"))
                    .or_else(|| t.strip_prefix('*'))
                    .map(|s| s.trim_end_matches("*/").trim());

                if let Some(c) = cleaned
                    && !c.is_empty()
                {
                    docs.push(c.to_string());
                }
            }
            expected_end_row = sib_start_row;
            cur = sib.prev_sibling();
        } else {
            break;
        }
    }

    if docs.is_empty() {
        return None;
    }
    docs.reverse();
    let summary = docs.first()?.clone();
    Some(if summary.len() > 140 {
        match summary.char_indices().nth(140) {
            Some((idx, _)) => format!("{}...", &summary[..idx]),
            None => summary,
        }
    } else {
        summary
    })
}

fn extract_python_docstring(node: Node<'_>, source: &str) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "expression_statement"
            && let Some(first) = child.named_child(0)
            && first.kind() == "string"
        {
            let raw = &source[first.byte_range()];
            let trimmed = raw
                .trim_matches(|c| c == '"' || c == '\'')
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())?;
            return Some(trimmed.chars().take(140).collect());
        }
        if !child.kind().contains("comment") {
            break;
        }
    }
    None
}

fn find_body_start_byte(node: Node<'_>, lang: SupportedLang) -> Option<usize> {
    if let Some(body) = node.child_by_field_name("body") {
        // For Rust structs/enums/type aliases that are short, keep compact signatures
        if lang == SupportedLang::Rust
            && matches!(node.kind(), "struct_item" | "enum_item")
            && node.end_position().row.saturating_sub(node.start_position().row) <= 10
        {
            return None;
        }
        return Some(body.start_byte());
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "block"
                | "statement_block"
                | "class_body"
                | "interface_body"
                | "declaration_list"
                | "field_declaration_list"
                | "enum_variant_list"
        ) {
            if lang == SupportedLang::Rust
                && matches!(
                    child.kind(),
                    "field_declaration_list" | "enum_variant_list"
                )
                && node.end_position().row.saturating_sub(node.start_position().row) <= 10
            {
                return None;
            }
            return Some(child.start_byte());
        }
    }
    None
}

fn format_node_signature(node: Node<'_>, source: &str, lang: SupportedLang) -> String {
    let start = node.start_byte();
    let end = find_body_start_byte(node, lang).unwrap_or_else(|| node.end_byte());
    let slice = &source[start..end.max(start).min(source.len())];

    if lang == SupportedLang::Rust && matches!(node.kind(), "struct_item" | "enum_item") && end == node.end_byte() {
        let lines: Vec<String> = slice
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.starts_with("//") && !l.starts_with('#'))
            .map(String::from)
            .collect();
        let joined = collapse_whitespace(&lines.join(" "));
        if joined.len() <= 140 {
            return joined;
        }
    }

    let header = collapse_whitespace(slice.trim().trim_end_matches(['{', ':', ';']));
    if header.len() > 160 {
        match header.char_indices().nth(160) {
            Some((idx, _)) => format!("{}...", &header[..idx]),
            None => header,
        }
    } else {
        header
    }
}

fn extract_node_name(node: Node<'_>, source: &str, lang: SupportedLang) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("name") {
        let name = source[name_node.byte_range()].trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    if lang == SupportedLang::Rust && node.kind() == "impl_item" {
        if let Some(ty) = node.child_by_field_name("type") {
            let ty_name = collapse_whitespace(&source[ty.byte_range()]);
            if let Some(tr) = node.child_by_field_name("trait") {
                let tr_name = collapse_whitespace(&source[tr.byte_range()]);
                return Some(format!("{tr_name} for {ty_name}"));
            }
            return Some(ty_name);
        }
    }

    if lang == SupportedLang::Go && matches!(node.kind(), "type_declaration" | "const_declaration" | "var_declaration") {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind().ends_with("_spec")
                && let Some(n) = child.child_by_field_name("name")
            {
                return Some(source[n.byte_range()].trim().to_string());
            }
        }
    }

    if matches!(lang, SupportedLang::TypeScript | SupportedLang::Tsx)
        && node.kind() == "lexical_declaration"
    {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "variable_declarator"
                && let Some(n) = child.child_by_field_name("name")
            {
                return Some(source[n.byte_range()].trim().to_string());
            }
        }
    }

    None
}

fn classify_symbol_node(node: Node<'_>, lang: SupportedLang) -> Option<(&'static str, bool)> {
    let k = node.kind();
    match lang {
        SupportedLang::Rust => match k {
            "struct_item" => Some(("struct", false)),
            "enum_item" => Some(("enum", false)),
            "trait_item" => Some(("trait", true)),
            "impl_item" => Some(("impl", true)),
            "function_item" | "function_signature_item" => Some(("fn", false)),
            "mod_item" => Some(("mod", true)),
            "type_item" => Some(("type", false)),
            "const_item" | "static_item" => Some(("const", false)),
            "macro_definition" => Some(("macro", false)),
            _ => None,
        },
        SupportedLang::TypeScript | SupportedLang::Tsx => match k {
            "function_declaration" | "generator_function_declaration" => Some(("fn", false)),
            "method_definition" | "method_signature" => Some(("method", false)),
            "class_declaration" | "abstract_class_declaration" => Some(("class", true)),
            "interface_declaration" => Some(("interface", true)),
            "type_alias_declaration" => Some(("type", false)),
            "enum_declaration" => Some(("enum", false)),
            "lexical_declaration" => {
                // Only include top-level const/let declarations or exported arrow functions
                if node.parent().is_some_and(|p| {
                    matches!(p.kind(), "program" | "export_statement")
                }) {
                    Some(("const", false))
                } else {
                    None
                }
            }
            _ => None,
        },
        SupportedLang::Python => match k {
            "class_definition" => Some(("class", true)),
            "function_definition" => {
                let is_method = node
                    .parent()
                    .and_then(|p| p.parent())
                    .is_some_and(|gp| gp.kind() == "class_definition");
                Some((if is_method { "method" } else { "def" }, false))
            }
            _ => None,
        },
        SupportedLang::Go => match k {
            "function_declaration" => Some(("func", false)),
            "method_declaration" => Some(("method", false)),
            "type_declaration" => Some(("type", false)),
            "const_declaration" => Some(("const", false)),
            _ => None,
        },
    }
}

fn collect_ast_symbols(
    node: Node<'_>,
    source: &str,
    lang: SupportedLang,
    depth: usize,
    defs: &mut Vec<SymbolDef>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();

        // Unwrap export statements or Python decorated definitions transparently
        if matches!(kind, "export_statement" | "decorated_definition") {
            collect_ast_symbols(child, source, lang, depth, defs);
            continue;
        }

        if let Some((sym_kind, recurse_into_body)) = classify_symbol_node(child, lang) {
            if let Some(name) = extract_node_name(child, source, lang) {
                let doc = extract_leading_docs(child, source).or_else(|| {
                    if lang == SupportedLang::Python {
                        extract_python_docstring(child, source)
                    } else {
                        None
                    }
                });
                let signature = format_node_signature(child, source, lang);
                let start_line = child.start_position().row + 1;
                let end_line = child.end_position().row + 1;

                defs.push(SymbolDef {
                    name,
                    kind: sym_kind,
                    signature,
                    doc,
                    start_line,
                    end_line,
                    depth,
                    cross_file_refs: 0,
                    score: 0.0,
                });

                if recurse_into_body && depth < 2 {
                    if let Some(body) = child.child_by_field_name("body") {
                        collect_ast_symbols(body, source, lang, depth + 1, defs);
                    } else {
                        collect_ast_symbols(child, source, lang, depth + 1, defs);
                    }
                }
            }
        } else if depth > 0
            && matches!(
                kind,
                "declaration_list" | "class_body" | "interface_body" | "block"
            )
        {
            collect_ast_symbols(child, source, lang, depth, defs);
        }
    }
}

fn collect_identifiers(node: Node<'_>, source: &str, counts: &mut HashMap<String, usize>) {
    let kind = node.kind();
    if matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
    ) {
        let ident = source[node.byte_range()].trim();
        if !is_noise_identifier(ident) {
            *counts.entry(ident.to_string()).or_insert(0) += 1;
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_identifiers(child, source, counts);
    }
}

fn parse_source_file(
    abs_path: &Path,
    display_path: String,
    max_file_size: u64,
) -> Option<ParsedFile> {
    let lang = SupportedLang::from_path(abs_path)?;
    let meta = fs::metadata(abs_path).ok()?;
    if meta.len() == 0 || meta.len() > max_file_size {
        return None;
    }

    let source = fs::read_to_string(abs_path).ok()?;
    let mut parser = Parser::new();
    parser.set_language(&lang.ts_language()).ok()?;
    let tree = parser.parse(&source, None)?;
    let root = tree.root_node();

    let mut defs = Vec::new();
    collect_ast_symbols(root, &source, lang, 0, &mut defs);

    let mut ident_counts = HashMap::new();
    collect_identifiers(root, &source, &mut ident_counts);

    Some(ParsedFile {
        display_path,
        lang,
        total_lines: source.lines().count().max(1),
        defs,
        ident_counts,
        pagerank: 0.0,
    })
}

fn clean_def_symbol_key(raw_name: &str) -> &str {
    // For Rust `impl Trait for Type`, index by the target type name
    if let Some((_, rhs)) = raw_name.rsplit_once(" for ") {
        rhs.split('<').next().unwrap_or(rhs).trim()
    } else {
        raw_name.split('<').next().unwrap_or(raw_name).trim()
    }
}

fn compute_pagerank_and_scores(files: &mut [ParsedFile], focus_terms: &[String]) {
    let n = files.len();
    if n == 0 {
        return;
    }

    // Map symbol name -> list of file indices that define it
    let mut symbol_definers: HashMap<String, Vec<usize>> = HashMap::new();
    for (file_idx, file) in files.iter().enumerate() {
        let mut seen_in_file = HashSet::new();
        for def in &file.defs {
            let key = clean_def_symbol_key(&def.name);
            if !is_noise_identifier(key) && seen_in_file.insert(key.to_string()) {
                symbol_definers
                    .entry(key.to_string())
                    .or_default()
                    .push(file_idx);
            }
        }
    }

    // Build weighted directed graph: edges[src][dst] = weight of src referencing symbols defined in dst
    let mut adj: Vec<HashMap<usize, f64>> = vec![HashMap::new(); n];
    let mut cross_file_symbol_hits: HashMap<(usize, String), usize> = HashMap::new();

    for src_idx in 0..n {
        for (ident, &count) in &files[src_idx].ident_counts {
            let Some(definers) = symbol_definers.get(ident) else {
                continue;
            };
            // Downweight overly generic symbols defined in many files
            let idf_weight = 1.0 / (definers.len() as f64).sqrt();
            let edge_delta = (count as f64).ln_1p() * idf_weight;

            for &dst_idx in definers {
                if dst_idx != src_idx {
                    *adj[src_idx].entry(dst_idx).or_insert(0.0) += edge_delta;
                    *cross_file_symbol_hits
                        .entry((dst_idx, ident.clone()))
                        .or_insert(0) += count;
                }
            }
        }
    }

    // Personalization vector (boosts entrypoints, core modules, or user-specified `focus_symbols`)
    let mut personalization = vec![1.0f64; n];
    let lower_focus: Vec<String> = focus_terms.iter().map(|s| s.to_ascii_lowercase()).collect();

    for (i, file) in files.iter().enumerate() {
        let p = file.display_path.as_str();
        if p.ends_with("main.rs")
            || p.ends_with("lib.rs")
            || p.ends_with("mod.rs")
            || p.ends_with("index.ts")
            || p.ends_with("__init__.py")
        {
            personalization[i] += 0.5;
        }
        if !lower_focus.is_empty() {
            let path_lower = p.to_ascii_lowercase();
            if lower_focus.iter().any(|q| path_lower.contains(q)) {
                personalization[i] += 5.0;
            }
            for def in &file.defs {
                let name_lower = def.name.to_ascii_lowercase();
                if lower_focus.iter().any(|q| name_lower.contains(q)) {
                    personalization[i] += 8.0;
                }
            }
        }
    }

    let pers_sum: f64 = personalization.iter().sum();
    for v in &mut personalization {
        *v /= pers_sum.max(1e-9);
    }

    let mut ranks = personalization.clone();
    for _ in 0..PAGERANK_ITERATIONS {
        let mut next_ranks = vec![0.0f64; n];
        let mut dangling_mass = 0.0f64;

        for src in 0..n {
            let out_sum: f64 = adj[src].values().sum();
            if out_sum <= 1e-12 {
                dangling_mass += ranks[src];
            } else {
                for (&dst, &w) in &adj[src] {
                    next_ranks[dst] += PAGERANK_DAMPING * ranks[src] * (w / out_sum);
                }
            }
        }

        for i in 0..n {
            next_ranks[i] += PAGERANK_DAMPING * dangling_mass * personalization[i]
                + (1.0 - PAGERANK_DAMPING) * personalization[i];
        }
        ranks = next_ranks;
    }

    for (i, file) in files.iter_mut().enumerate() {
        file.pagerank = ranks[i];
        for def in &mut file.defs {
            let key = clean_def_symbol_key(&def.name);
            let refs = cross_file_symbol_hits
                .get(&(i, key.to_string()))
                .copied()
                .unwrap_or(0);
            def.cross_file_refs = refs;

            let kind_boost = match def.kind {
                "trait" | "interface" | "struct" | "enum" | "class" => 1.4,
                "impl" | "mod" => 1.2,
                _ => 1.0,
            };
            let focus_boost = if !lower_focus.is_empty()
                && lower_focus
                    .iter()
                    .any(|q| def.name.to_ascii_lowercase().contains(q))
            {
                4.0
            } else {
                1.0
            };

            def.score = file.pagerank
                * (1.0 + (refs as f64).ln_1p())
                * kind_boost
                * focus_boost
                * if def.depth == 0 { 1.15 } else { 0.9 };
        }
    }
}

pub fn execute(
    json_args: &Value,
    settings: &WorkspaceSnapshotSettings,
    max_output_size: usize,
) -> Result<String> {
    let args: RepoMapArgs = parse_args(json_args, "repo_map")?;
    let cwd = workspace_root()?;
    let base_root = if settings.git_root {
        find_git_root(&cwd).unwrap_or_else(|| cwd.clone())
    } else {
        cwd.clone()
    };
    let base_root = dunce::canonicalize(&base_root).unwrap_or(base_root);

    let scan_root = if args.target_path.trim().is_empty() || args.target_path.trim() == "." {
        base_root.clone()
    } else {
        let rel = validate_relative_path(&args.target_path)?;
        let candidate = base_root.join(rel);
        if !candidate.exists() {
            bail!("Target path does not exist: '{}'", args.target_path.trim());
        }
        let canon = dunce::canonicalize(&candidate)?;
        if !canon.starts_with(&base_root) {
            bail!("Security violation: target_path escapes workspace root");
        }
        canon
    };

    let include_set = if args.include.is_empty() {
        None
    } else {
        Some(build_globset(&args.include)?)
    };

    let mut exclude_patterns = vec![
        "**/.git/**".to_string(),
        "**/*.venv/**".to_string(),
        "**/node_modules/**".to_string(),
        "**/target/**".to_string(),
        "**/.env*".to_string(),
    ];
    exclude_patterns.extend(settings.exclude.iter().cloned());
    exclude_patterns.extend(args.exclude);
    let exclude_set = build_globset(&exclude_patterns)?;

    let mut parsed_files = Vec::new();
    if scan_root.is_file() {
        let rel = diff_paths(&scan_root, &base_root).unwrap_or_else(|| scan_root.clone());
        if let Some(pf) = parse_source_file(
            &scan_root,
            rel.to_string_lossy().into_owned(),
            settings.max_size,
        ) {
            parsed_files.push(pf);
        }
    } else {
        let walker = WalkBuilder::new(&scan_root)
            .hidden(false)
            .git_ignore(settings.git_root)
            .git_global(settings.git_root)
            .git_exclude(settings.git_root)
            .build();

        for entry in walker.flatten() {
            if parsed_files.len() >= settings.max_files {
                break;
            }
            let path = entry.path();
            if !path.is_file() || SupportedLang::from_path(path).is_none() {
                continue;
            }
            let Some(relative) = diff_paths(path, &base_root) else {
                continue;
            };
            if exclude_set.is_match(&relative) {
                continue;
            }
            if let Some(inc) = &include_set
                && !inc.is_match(&relative)
            {
                continue;
            }
            if let Some(pf) = parse_source_file(
                path,
                relative.to_string_lossy().into_owned(),
                settings.max_size,
            ) {
                parsed_files.push(pf);
            }
        }
    }

    if parsed_files.is_empty() {
        return Ok(
            "No supported source files (.rs, .ts, .tsx, .js, .jsx, .py, .go) found in scope."
                .into(),
        );
    }

    compute_pagerank_and_scores(&mut parsed_files, &args.focus_symbols);

    // Sort files by descending PageRank score (tie-breaking by path)
    parsed_files.sort_by(|a, b| {
        b.pagerank
            .partial_cmp(&a.pagerank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.display_path.cmp(&b.display_path))
    });

    let token_budget = args.max_tokens.unwrap_or(8_000).clamp(500, 64_000);
    // ~4 bytes per token, capped by the model's `max_output_size`
    let byte_budget = (token_budget.saturating_mul(4)).min(max_output_size.saturating_sub(256));

    let total_symbols: usize = parsed_files.iter().map(|f| f.defs.len()).sum();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# AST Repository Map ({} files parsed, {} symbols extracted — ranked by PageRank)\n",
        parsed_files.len(),
        total_symbols
    );

    // Collect top central symbols across the entire repo for a quick architectural index
    let mut top_symbols: Vec<(&str, &SymbolDef)> = parsed_files
        .iter()
        .flat_map(|f| {
            f.defs
                .iter()
                .filter(|d| d.depth == 0 && d.cross_file_refs > 0)
                .map(move |d| (f.display_path.as_str(), d))
        })
        .collect();

    top_symbols.sort_by(|a, b| {
        b.1.score
            .partial_cmp(&a.1.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if !top_symbols.is_empty() {
        out.push_str("## Top Central Symbols (Cross-File Hotspots)\n\n");
        for (file_path, def) in top_symbols.into_iter().take(15) {
            let _ = writeln!(
                out,
                "- `{}` ({}) — `{}:{}-{}` ({} cross-file refs)",
                def.name, def.kind, file_path, def.start_line, def.end_line, def.cross_file_refs
            );
        }
        out.push('\n');
    }

    out.push_str("## Ranked File Skeletons\n\n");

    let mut omitted_files = 0usize;
    for (rank_idx, file) in parsed_files.iter().enumerate() {
        let mut block = String::new();
        let _ = writeln!(
            block,
            "### {}. `{}` (rank: {:.3}, {} lines, {} symbols)\n",
            rank_idx + 1,
            file.display_path,
            file.pagerank,
            file.total_lines,
            file.defs.len()
        );

        if file.defs.is_empty() {
            block.push_str("*(No top-level AST declarations)*\n\n");
        } else {
            let _ = writeln!(block, "```{}", file.lang.code_fence_tag());
            for def in &file.defs {
                let indent = "    ".repeat(def.depth);
                if args.include_docs
                    && let Some(doc) = &def.doc
                {
                    let _ = writeln!(block, "{indent}/// {doc}");
                }
                let ref_tag = if def.cross_file_refs > 0 {
                    format!(" // [L{}-L{} | {} refs]", def.start_line, def.end_line, def.cross_file_refs)
                } else {
                    format!(" // [L{}-L{}]", def.start_line, def.end_line)
                };
                let _ = writeln!(block, "{indent}{}{ref_tag}", def.signature);
            }
            block.push_str("```\n\n");
        }

        if out.len() + block.len() > byte_budget {
            omitted_files = parsed_files.len() - rank_idx;
            break;
        }
        out.push_str(&block);
    }

    if omitted_files > 0 {
        let _ = writeln!(
            out,
            "*[Budget reached ({token_budget} tokens): omitted {omitted_files} lower-ranked file(s). Pass `target_path`, `focus_symbols`, or a higher `max_tokens` to inspect deeper.]*"
        );
    }

    Ok(out.trim_end().to_string())
}
