use crate::{
    core::{config::WorkspaceSnapshotSettings, session::truncate_str},
    tools::utils::{
        auto_schema, parse_args, read_utf8_capped, resolve_scan_roots, walk_workspace_files,
    },
};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    path::{Path, PathBuf},
};
use tree_sitter::{Language, Node, Parser, Tree};

const PAGERANK_ITERATIONS: usize = 25;
const PAGERANK_DAMPING: f64 = 0.85;

#[derive(Default, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepoMapAction {
    /// Generate a PageRank-ranked AST repository map of file skeletons and symbol signatures (default).
    #[default]
    Map,
    /// Extract the full source implementation (including leading comments/attributes and 1-based line numbers) of a named function, method, struct, enum, trait, impl, class, interface, or type.
    Definition,
    /// Find all AST identifier references/usages of `symbol` across the workspace, grouped by file and enclosing AST scope (e.g., which function or method calls it).
    References,
}

#[derive(Deserialize, JsonSchema)]
struct RepoMapArgs {
    /// The AST operation to perform: `map` (default), `definition`, or `references`.
    #[serde(default)]
    action: RepoMapAction,
    /// Target symbol name when `action` is `definition` or `references`. Supports bare names (e.g. `sanitize_schema_for_gemini`, `CoreEngine`) or qualified names (e.g. `CoreEngine::handle_command`, `Provider.stream_turn`). If passed when `action` is `map`, automatically boosts PageRank personalization for that symbol.
    #[serde(default)]
    symbol: String,
    /// Optional focus symbols or query terms when `action` is `map` (e.g. `["CoreEngine", "Provider"]`). Boosts PageRank personalization toward files defining or referencing these symbols.
    #[serde(default)]
    focus_symbols: Vec<String>,
    /// Optional relative directory or file path within the workspace to scope the operation (defaults to workspace root).
    #[serde(default)]
    target_path: String,
    /// Optional glob patterns to filter included files (e.g. `["src/**/*.rs", "**/*.ts"]`).
    #[serde(default)]
    include: Vec<String>,
    /// Optional glob patterns to exclude.
    #[serde(default)]
    exclude: Vec<String>,
    /// When `action` is `map`, whether to include leading doc comments above AST symbol signatures (default: true).
    #[serde(default = "default_true")]
    include_docs: bool,
    /// When `action` is `map`, approximate token budget for the rendered repository map (default: 8000, min: 500, max: 64000).
    #[serde(default)]
    max_tokens: Option<usize>,
    /// When `action` is `definition` or `references`, maximum number of matches to return (default: 20 for `definition`, 100 for `references`).
    #[serde(default)]
    max_results: Option<usize>,
}

fn default_true() -> bool {
    true
}

pub fn schema() -> Value {
    auto_schema::<RepoMapArgs>(
        "repo_map",
        "AST-accurate code intelligence powered by Tree-sitter across Rust, TypeScript/JavaScript, Python, and Go. Supports three actions: `map` (generates a PageRank-ranked repository map of signatures and 1-based line ranges), `definition` (reads the exact source implementation of a symbol without needing its file path or line range), and `references` (finds all AST call sites and usages of a symbol grouped by enclosing function/scope).",
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
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
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

struct SymbolMatch {
    display_path: String,
    lang: SupportedLang,
    kind: &'static str,
    qualified_name: String,
    start_line: usize,
    end_line: usize,
    snippet: String,
}

struct ReferenceHit {
    line: usize,
    col: usize,
    enclosing_scope: String,
    line_text: String,
}

fn is_noise_identifier(name: &str) -> bool {
    name.len() < 2
        || matches!(
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
        if expected_end_row.saturating_sub(sib.end_position().row) > 1 {
            break;
        }
        if matches!(
            kind,
            "attribute_item" | "inner_attribute_item" | "decorator"
        ) {
            expected_end_row = sib.start_position().row;
            cur = sib.prev_sibling();
            continue;
        }
        if !kind.contains("comment") {
            break;
        }

        for line in source[sib.byte_range()].lines().rev().map(str::trim) {
            let cleaned = ["///", "//!", "//", "#", "/**", "/*", "*"]
                .iter()
                .find_map(|p| line.strip_prefix(p))
                .map(|s| s.trim_end_matches("*/").trim())
                .filter(|c| !c.is_empty());
            if let Some(c) = cleaned {
                docs.push(c.to_string());
            }
        }
        expected_end_row = sib.start_position().row;
        cur = sib.prev_sibling();
    }

    docs.last().map(|s| truncate_str(s, 140))
}

fn extract_python_docstring(node: Node<'_>, source: &str) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "expression_statement"
            && let Some(first) = child.named_child(0).filter(|n| n.kind() == "string")
        {
            let line = source[first.byte_range()]
                .trim_matches(['"', '\''])
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())?;
            return Some(line.chars().take(140).collect());
        }
        if !child.kind().contains("comment") {
            break;
        }
    }
    None
}

fn find_body_start_byte(node: Node<'_>, lang: SupportedLang) -> Option<usize> {
    let is_short_rust_adt = lang == SupportedLang::Rust
        && matches!(node.kind(), "struct_item" | "enum_item")
        && node
            .end_position()
            .row
            .saturating_sub(node.start_position().row)
            <= 10;

    if let Some(body) = node.child_by_field_name("body") {
        return (!is_short_rust_adt).then(|| body.start_byte());
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
            if is_short_rust_adt
                && matches!(child.kind(), "field_declaration_list" | "enum_variant_list")
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

    if lang == SupportedLang::Rust
        && matches!(node.kind(), "struct_item" | "enum_item")
        && end == node.end_byte()
    {
        let joined = collapse_whitespace(
            &slice
                .lines()
                .map(str::trim)
                .filter(|l| !l.starts_with("//") && !l.starts_with('#'))
                .collect::<Vec<_>>()
                .join(" "),
        );
        if joined.len() <= 140 {
            return joined;
        }
    }

    truncate_str(
        &collapse_whitespace(slice.trim().trim_end_matches(['{', ':', ';'])),
        160,
    )
}

fn extract_node_name(node: Node<'_>, source: &str, lang: SupportedLang) -> Option<String> {
    if let Some(n) = node.child_by_field_name("name") {
        let name = source[n.byte_range()].trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    if lang == SupportedLang::Rust
        && node.kind() == "impl_item"
        && let Some(ty) = node.child_by_field_name("type")
    {
        let ty_name = collapse_whitespace(&source[ty.byte_range()]);
        return Some(match node.child_by_field_name("trait") {
            Some(tr) => format!(
                "{} for {ty_name}",
                collapse_whitespace(&source[tr.byte_range()])
            ),
            None => ty_name,
        });
    }

    let mut cursor = node.walk();
    if lang == SupportedLang::Go
        && matches!(
            node.kind(),
            "type_declaration" | "const_declaration" | "var_declaration"
        )
    {
        return node
            .named_children(&mut cursor)
            .find(|c| c.kind().ends_with("_spec"))
            .and_then(|c| c.child_by_field_name("name"))
            .map(|n| source[n.byte_range()].trim().to_string());
    }

    if matches!(lang, SupportedLang::TypeScript | SupportedLang::Tsx)
        && node.kind() == "lexical_declaration"
    {
        return node
            .named_children(&mut cursor)
            .find(|c| c.kind() == "variable_declarator")
            .and_then(|c| c.child_by_field_name("name"))
            .map(|n| source[n.byte_range()].trim().to_string());
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
            "lexical_declaration"
                if node
                    .parent()
                    .is_some_and(|p| matches!(p.kind(), "program" | "export_statement")) =>
            {
                Some(("const", false))
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

fn walk_symbol_tree<F>(
    node: Node<'_>,
    source: &str,
    lang: SupportedLang,
    parent_scope: Option<&str>,
    depth: usize,
    visitor: &mut F,
) where
    F: FnMut(Node<'_>, &'static str, &str, Option<&str>, usize) -> bool,
{
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        if matches!(kind, "export_statement" | "decorated_definition") {
            walk_symbol_tree(child, source, lang, parent_scope, depth, visitor);
            continue;
        }

        if let Some((sym_kind, recurse_into_body)) = classify_symbol_node(child, lang) {
            if let Some(name) = extract_node_name(child, source, lang) {
                if !visitor(child, sym_kind, &name, parent_scope, depth) {
                    return;
                }
                if recurse_into_body && depth < 2 {
                    let body = child.child_by_field_name("body").unwrap_or(child);
                    walk_symbol_tree(body, source, lang, Some(&name), depth + 1, visitor);
                }
            }
        } else if matches!(
            kind,
            "declaration_list" | "class_body" | "interface_body" | "block"
        ) {
            walk_symbol_tree(child, source, lang, parent_scope, depth, visitor);
        }
    }
}

fn collect_ast_symbols(
    node: Node<'_>,
    source: &str,
    lang: SupportedLang,
    defs: &mut Vec<SymbolDef>,
) {
    walk_symbol_tree(
        node,
        source,
        lang,
        None,
        0,
        &mut |child, sym_kind, name, _, depth| {
            let doc = extract_leading_docs(child, source).or_else(|| {
                (lang == SupportedLang::Python)
                    .then(|| extract_python_docstring(child, source))
                    .flatten()
            });
            defs.push(SymbolDef {
                name: name.to_string(),
                kind: sym_kind,
                signature: format_node_signature(child, source, lang),
                doc,
                start_line: child.start_position().row + 1,
                end_line: child.end_position().row + 1,
                depth,
                cross_file_refs: 0,
                score: 0.0,
            });
            true
        },
    );
}

fn collect_identifiers(node: Node<'_>, source: &str, counts: &mut HashMap<String, usize>) {
    if matches!(
        node.kind(),
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

fn parse_ast_file(
    parser: &mut Parser,
    abs_path: &Path,
    lang: SupportedLang,
    max_file_size: u64,
    required_substring: Option<&str>,
) -> Option<(String, Tree)> {
    let source = read_utf8_capped(abs_path, max_file_size)?;
    if let Some(needle) = required_substring
        && !source.contains(needle)
    {
        return None;
    }
    parser.set_language(&lang.ts_language()).ok()?;
    let tree = parser.parse(&source, None)?;
    Some((source, tree))
}

fn parse_source_file(
    parser: &mut Parser,
    abs_path: &Path,
    display_path: String,
    lang: SupportedLang,
    max_file_size: u64,
) -> Option<ParsedFile> {
    let (source, tree) = parse_ast_file(parser, abs_path, lang, max_file_size, None)?;
    let root = tree.root_node();

    let mut defs = Vec::new();
    collect_ast_symbols(root, &source, lang, &mut defs);

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
    let base = raw_name
        .rsplit_once(" for ")
        .map_or(raw_name, |(_, rhs)| rhs);
    base.split('<').next().unwrap_or(base).trim()
}

fn compute_pagerank_and_scores(files: &mut [ParsedFile], focus_terms: &[String]) {
    let n = files.len();
    if n == 0 {
        return;
    }

    let mut symbol_definers: HashMap<String, Vec<usize>> = HashMap::new();
    for (file_idx, file) in files.iter().enumerate() {
        let mut seen = HashSet::new();
        for def in &file.defs {
            let key = clean_def_symbol_key(&def.name);
            if !is_noise_identifier(key) && seen.insert(key.to_string()) {
                symbol_definers
                    .entry(key.to_string())
                    .or_default()
                    .push(file_idx);
            }
        }
    }

    let mut adj: Vec<HashMap<usize, f64>> = vec![HashMap::new(); n];
    let mut cross_file_symbol_hits: HashMap<(usize, String), usize> = HashMap::new();

    for src_idx in 0..n {
        for (ident, &count) in &files[src_idx].ident_counts {
            let Some(definers) = symbol_definers.get(ident) else {
                continue;
            };
            let edge_delta = (count as f64).ln_1p() / (definers.len() as f64).sqrt();
            for &dst_idx in definers.iter().filter(|&&d| d != src_idx) {
                *adj[src_idx].entry(dst_idx).or_insert(0.0) += edge_delta;
                *cross_file_symbol_hits
                    .entry((dst_idx, ident.clone()))
                    .or_insert(0) += count;
            }
        }
    }

    let mut personalization = vec![1.0f64; n];
    let lower_focus: Vec<String> = focus_terms.iter().map(|s| s.to_ascii_lowercase()).collect();

    for (i, file) in files.iter().enumerate() {
        let p = file.display_path.as_str();
        if ["main.rs", "lib.rs", "mod.rs", "index.ts", "__init__.py"]
            .iter()
            .any(|s| p.ends_with(s))
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

    let pers_sum: f64 = personalization.iter().sum::<f64>().max(1e-9);
    for v in &mut personalization {
        *v /= pers_sum;
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
            let focus_boost = if lower_focus
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

fn parse_symbol_query(raw: &str) -> (Option<String>, String) {
    let trimmed = raw.trim();
    if let Some((parent, leaf)) = trimmed
        .rsplit_once("::")
        .or_else(|| trimmed.rsplit_once('.'))
    {
        let p = parent
            .rsplit_once("::")
            .or_else(|| parent.rsplit_once('.'))
            .map_or(parent, |(_, rhs)| rhs)
            .trim();
        (
            (!p.is_empty()).then(|| p.to_string()),
            leaf.trim().to_string(),
        )
    } else {
        (None, trimmed.to_string())
    }
}

fn expand_start_row_for_docs_and_attrs(node: Node<'_>) -> usize {
    let mut start_row = node.start_position().row;
    let mut cur = node.prev_sibling();

    while let Some(sib) = cur {
        let kind = sib.kind();
        if start_row.saturating_sub(sib.end_position().row) > 1 {
            break;
        }
        if kind.contains("comment")
            || matches!(
                kind,
                "attribute_item" | "inner_attribute_item" | "decorator"
            )
        {
            start_row = sib.start_position().row;
            cur = sib.prev_sibling();
        } else {
            break;
        }
    }
    start_row
}

fn format_numbered_slice(lines: &[&str], start_row: usize, end_row: usize) -> String {
    let mut out = String::new();
    let clamped_end = end_row.min(lines.len().saturating_sub(1));
    for (idx, &line) in lines
        .iter()
        .enumerate()
        .take(clamped_end + 1)
        .skip(start_row)
    {
        let _ = writeln!(out, "{:>4}: {}", idx + 1, line);
    }
    out
}

fn matches_symbol_name(
    def_name: &str,
    parent_scope: Option<&str>,
    target_parent: Option<&str>,
    target_leaf: &str,
) -> bool {
    let clean_name = clean_def_symbol_key(def_name);
    if clean_name != target_leaf && def_name != target_leaf {
        return false;
    }
    target_parent.is_none_or(|req_parent| {
        parent_scope.is_some_and(|actual| {
            clean_def_symbol_key(actual).eq_ignore_ascii_case(req_parent)
                || actual.eq_ignore_ascii_case(req_parent)
        })
    })
}

fn find_enclosing_scope(mut node: Node<'_>, source: &str, lang: SupportedLang) -> String {
    let mut scopes = Vec::new();
    while let Some(parent) = node.parent() {
        if let Some((kind, _)) = classify_symbol_node(parent, lang)
            && let Some(name) = extract_node_name(parent, source, lang)
        {
            scopes.push(format!("{kind} {name}"));
            if scopes.len() >= 2 {
                break;
            }
        }
        node = parent;
    }

    if scopes.is_empty() {
        "(top-level)".into()
    } else {
        scopes.reverse();
        scopes.join(" -> ")
    }
}

fn walk_references(
    node: Node<'_>,
    source: &str,
    lines: &[&str],
    lang: SupportedLang,
    target_leaf: &str,
    hits: &mut Vec<ReferenceHit>,
    max_results: usize,
) {
    if hits.len() >= max_results {
        return;
    }

    if matches!(
        node.kind(),
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
    ) {
        let ident = source[node.byte_range()].trim();
        if ident == target_leaf {
            let row = node.start_position().row;
            let col = node.start_position().column + 1;
            let line_no = row + 1;

            if hits.last().is_none_or(|last| last.line != line_no) {
                let raw_line = lines.get(row).copied().unwrap_or("").trim();
                let line_text = truncate_str(raw_line, 240);
                hits.push(ReferenceHit {
                    line: line_no,
                    col,
                    enclosing_scope: find_enclosing_scope(node, source, lang),
                    line_text,
                });
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if hits.len() >= max_results {
            break;
        }
        walk_references(child, source, lines, lang, target_leaf, hits, max_results);
    }
}

fn collect_candidate_files(
    args: &RepoMapArgs,
    settings: &WorkspaceSnapshotSettings,
) -> Result<Vec<(PathBuf, String, SupportedLang)>> {
    let (base_root, scan_root) = resolve_scan_roots(
        settings.git_root,
        &args.target_path,
        "target_path escapes workspace root",
    )?;

    let walked = walk_workspace_files(
        &base_root,
        &scan_root,
        settings.git_root,
        &args.include,
        &settings.exclude,
        &args.exclude,
        settings.max_files,
    )?;

    Ok(walked
        .into_iter()
        .filter_map(|(abs_path, rel_display)| {
            let lang = SupportedLang::from_path(&abs_path)?;
            Some((abs_path, rel_display, lang))
        })
        .collect())
}

fn execute_map(
    args: RepoMapArgs,
    candidate_files: Vec<(PathBuf, String, SupportedLang)>,
    settings: &WorkspaceSnapshotSettings,
    max_output_size: usize,
) -> Result<String> {
    let mut parser = Parser::new();
    let mut parsed_files: Vec<ParsedFile> = candidate_files
        .into_iter()
        .filter_map(|(abs_path, display_path, lang)| {
            parse_source_file(
                &mut parser,
                &abs_path,
                display_path,
                lang,
                settings.max_size,
            )
        })
        .collect();

    if parsed_files.is_empty() {
        return Ok(
            "No supported source files (.rs, .ts, .tsx, .js, .jsx, .py, .go) found in scope."
                .into(),
        );
    }

    let mut focus_terms = args.focus_symbols;
    if !args.symbol.trim().is_empty() {
        focus_terms.push(args.symbol.trim().to_string());
    }

    compute_pagerank_and_scores(&mut parsed_files, &focus_terms);
    parsed_files.sort_by(|a, b| {
        b.pagerank
            .partial_cmp(&a.pagerank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.display_path.cmp(&b.display_path))
    });

    let token_budget = args.max_tokens.unwrap_or(8_000).clamp(500, 64_000);
    let byte_budget = (token_budget.saturating_mul(4)).min(max_output_size.saturating_sub(256));
    let total_symbols: usize = parsed_files.iter().map(|f| f.defs.len()).sum();

    let mut out = format!(
        "# AST Repository Map ({} files parsed, {total_symbols} symbols extracted — ranked by PageRank)\n\n",
        parsed_files.len()
    );

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
                "- `{}` ({}) — `{file_path}:{}-{}` ({} cross-file refs)",
                def.name, def.kind, def.start_line, def.end_line, def.cross_file_refs
            );
        }
        out.push('\n');
    }

    out.push_str("## Ranked File Skeletons\n\n");
    let mut omitted_files = 0usize;

    for (rank_idx, file) in parsed_files.iter().enumerate() {
        let mut block = format!(
            "### {}. `{}` (rank: {:.3}, {} lines, {} symbols)\n\n",
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
                    format!(
                        " // [L{}-L{} | {} refs]",
                        def.start_line, def.end_line, def.cross_file_refs
                    )
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

fn execute_definition(
    args: &RepoMapArgs,
    candidate_files: Vec<(PathBuf, String, SupportedLang)>,
    settings: &WorkspaceSnapshotSettings,
    max_output_size: usize,
) -> Result<String> {
    let (target_parent, target_leaf) = parse_symbol_query(&args.symbol);
    if target_leaf.is_empty() {
        bail!("'symbol' is required when `action` is `definition`");
    }

    let max_results = args.max_results.unwrap_or(20).clamp(1, 100);
    let mut matches = Vec::new();
    let mut parser = Parser::new();

    for (abs_path, display_path, lang) in candidate_files {
        if matches.len() >= max_results {
            break;
        }
        let Some((source, tree)) = parse_ast_file(
            &mut parser,
            &abs_path,
            lang,
            settings.max_size,
            Some(&target_leaf),
        ) else {
            continue;
        };

        let lines: Vec<&str> = source.lines().collect();
        walk_symbol_tree(
            tree.root_node(),
            &source,
            lang,
            None,
            0,
            &mut |child, sym_kind, name, parent_scope, _| {
                if matches.len() >= max_results {
                    return false;
                }
                if matches_symbol_name(name, parent_scope, target_parent.as_deref(), &target_leaf) {
                    let start_row = expand_start_row_for_docs_and_attrs(child);
                    let end_row = child.end_position().row;
                    let qualified_name = match parent_scope {
                        Some(p) => format!("{p}::{name}"),
                        None => name.to_string(),
                    };
                    matches.push(SymbolMatch {
                        display_path: display_path.clone(),
                        lang,
                        kind: sym_kind,
                        qualified_name,
                        start_line: start_row + 1,
                        end_line: end_row + 1,
                        snippet: format_numbered_slice(&lines, start_row, end_row),
                    });
                }
                matches.len() < max_results
            },
        );
    }

    if matches.is_empty() {
        return Ok(format!(
            "No AST definition found for symbol '{}'. Try `repo_map` (`action: \"map\"`) or `search`.",
            args.symbol.trim()
        ));
    }

    let mut out = format!(
        "# AST Definitions for `{}` ({} found)\n\n",
        args.symbol.trim(),
        matches.len()
    );

    for m in &matches {
        let block = format!(
            "## `{}` ({}) — `{}:{}-{}`\n\n```{}\n{}```\n\n",
            m.qualified_name,
            m.kind,
            m.display_path,
            m.start_line,
            m.end_line,
            m.lang.code_fence_tag(),
            m.snippet
        );
        if out.len() + block.len() > max_output_size {
            let _ = writeln!(
                out,
                "*[Output budget reached; narrow `target_path` or `symbol` to inspect remaining definitions.]*"
            );
            break;
        }
        out.push_str(&block);
    }

    Ok(out.trim_end().to_string())
}

fn execute_references(
    args: &RepoMapArgs,
    candidate_files: Vec<(PathBuf, String, SupportedLang)>,
    settings: &WorkspaceSnapshotSettings,
    max_output_size: usize,
) -> Result<String> {
    let (_, target_leaf) = parse_symbol_query(&args.symbol);
    if target_leaf.is_empty() {
        bail!("'symbol' is required when `action` is `references`");
    }

    let max_results = args.max_results.unwrap_or(100).clamp(1, 500);
    let mut total_hits = 0usize;
    let mut matched_files = 0usize;
    let mut out = String::new();
    let mut parser = Parser::new();

    for (abs_path, display_path, lang) in candidate_files {
        if total_hits >= max_results || out.len() > max_output_size {
            break;
        }
        let Some((source, tree)) = parse_ast_file(
            &mut parser,
            &abs_path,
            lang,
            settings.max_size,
            Some(&target_leaf),
        ) else {
            continue;
        };

        let lines: Vec<&str> = source.lines().collect();
        let remaining = max_results - total_hits;
        let mut file_hits = Vec::new();
        walk_references(
            tree.root_node(),
            &source,
            &lines,
            lang,
            &target_leaf,
            &mut file_hits,
            remaining,
        );

        if file_hits.is_empty() {
            continue;
        }

        matched_files += 1;
        total_hits += file_hits.len();
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = writeln!(out, "=== {display_path} ({} refs) ===", file_hits.len());
        for hit in file_hits {
            let _ = writeln!(
                out,
                "{:>4}:{:<3} [{}] {}",
                hit.line, hit.col, hit.enclosing_scope, hit.line_text
            );
            if out.len() > max_output_size {
                break;
            }
        }
    }

    if total_hits == 0 {
        return Ok(format!(
            "No AST identifier references found for '{}'.",
            target_leaf
        ));
    }

    let suffix = if total_hits >= max_results {
        format!(" — stopped at limit ({max_results})")
    } else {
        String::new()
    };
    let _ = write!(
        out,
        "\n[Found {total_hits} AST reference(s) across {matched_files} file(s){suffix}]"
    );
    Ok(out)
}

pub fn execute(
    json_args: &Value,
    settings: &WorkspaceSnapshotSettings,
    max_output_size: usize,
) -> Result<String> {
    let args: RepoMapArgs = parse_args(json_args, "repo_map")?;
    let candidate_files = collect_candidate_files(&args, settings)?;
    if candidate_files.is_empty() {
        return Ok(
            "No supported source files (.rs, .ts, .tsx, .js, .jsx, .py, .go) found in scope."
                .into(),
        );
    }

    match args.action {
        RepoMapAction::Map => execute_map(args, candidate_files, settings, max_output_size),
        RepoMapAction::Definition => {
            execute_definition(&args, candidate_files, settings, max_output_size)
        }
        RepoMapAction::References => {
            execute_references(&args, candidate_files, settings, max_output_size)
        }
    }
}
