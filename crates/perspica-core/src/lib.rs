pub mod parser;
pub mod diff;
pub mod classify;
pub mod manifest;
pub mod languages;
pub mod cross_file;
pub mod annotate;
pub mod roles;
pub mod flow;

use manifest::{Change, ChangeKind, ChangeManifest, DiffHunk, LineRange, Location, Side, Span};
use serde::{Deserialize, Serialize};

/// Supported languages for semantic analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    TypeScript,
    /// TypeScript/JavaScript with JSX (.tsx, .jsx).
    Tsx,
    Python,
    Rust,
    Go,
    Java,
    C,
    Scala,
    /// Not parsed: shown as a plain diff with no classification.
    Unknown,
}

impl Language {
    pub fn from_extension(ext: &str) -> Self {
        match ext {
            "ts" | "mts" | "cts" | "js" | "mjs" | "cjs" => Language::TypeScript,
            "tsx" | "jsx" => Language::Tsx,
            "py" | "pyi" => Language::Python,
            "rs" => Language::Rust,
            "go" => Language::Go,
            "java" => Language::Java,
            "c" | "h" => Language::C,
            "scala" | "sc" => Language::Scala,
            _ => Language::Unknown,
        }
    }

    pub fn from_path(path: &str) -> Self {
        let file = path.rsplit('/').next().unwrap_or(path);
        match file.rsplit_once('.') {
            Some((_, ext)) => Language::from_extension(&ext.to_ascii_lowercase()),
            None => Language::Unknown,
        }
    }
}

/// Per-language metadata extracted during analysis.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LanguageInfo {
    pub language: Option<String>,
}

/// How much of a file's diff needs a reviewer's attention.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ReviewStats {
    /// Added + removed lines.
    pub changed_lines: usize,
    /// Changed lines marked as mechanical noise (formatting, comments, renames, moves, generated).
    pub mechanical_lines: usize,
    /// The file was parsed and classified (false for unsupported languages / generated files).
    pub parsed: bool,
    /// Generated or vendored file (lockfile, bundle, snapshot …).
    pub generated: bool,
    /// What the file is: source, test, docs, generated, vendored.
    #[serde(default)]
    pub role: roles::FileRole,
    /// Changed lines that are test code (whole test files, or tests inside source files).
    #[serde(default)]
    pub test_lines: usize,
}

/// The complete analysis result.
#[derive(Debug, Serialize, Deserialize)]
pub struct DiffResult {
    /// The diff hunks, annotated with manifest links and noise.
    pub hunks: Vec<DiffHunk>,
    /// Classified changes grouped by type.
    pub manifest: ChangeManifest,
    /// Per-language metadata.
    pub language_info: LanguageInfo,
    #[serde(default)]
    pub review: ReviewStats,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("unsupported language")]
    UnsupportedLanguage,
}

/// Analyze a single pair of sources. Entry ids start at 1.
pub fn analyze(old_source: &str, new_source: &str, language: Language) -> Result<DiffResult, Error> {
    let file = cross_file::FileChange::new("", old_source, new_source, language);
    let mut multi = analyze_multi(std::slice::from_ref(&file))?;
    Ok(multi.file_results.remove(0).1)
}

/// Analyze a set of file changes together: per-file classification, then
/// cross-file moves, broken references, signature impact, and hunk annotation.
/// Entry ids are unique across all files.
pub fn analyze_multi(files: &[cross_file::FileChange]) -> Result<cross_file::MultiDiffResult, Error> {
    let mut next_id: u32 = 1;
    let mut analyses = Vec::with_capacity(files.len());
    for file in files {
        analyses.push(analyze_file(file, &mut next_id)?);
    }

    let moves = cross_file::detect_cross_file_moves(&mut analyses, &mut next_id);
    cross_file::filter_dead_code_cross_file(&mut analyses);
    let vanished = cross_file::vanished_names(&analyses, &moves).into_iter()
        .map(|(name, renamed_to, origin, owner)| cross_file::VanishedSymbol { name, renamed_to, origin, owner })
        .collect();
    let broken_references = cross_file::detect_broken_references(&analyses, &moves, &mut next_id);
    let signature_impacts = cross_file::detect_signature_impacts(&analyses, &mut next_id);

    // Annotate hunks with manifest links and noise.
    // Renames of top-level items apply everywhere; member renames (`Svc.get`,
    // `Type::new`) only in their own file, since a bare `get` elsewhere is
    // usually some other type's method.
    let is_member = |name: &str| parser::bare_name(name) != name;
    let bare_pair = |old: &str, new: &str| (parser::bare_name(old).to_string(), parser::bare_name(new).to_string());
    let mut ctx = annotate::AnnotateContext::default();
    let mut local_renames: Vec<Vec<(String, String)>> = vec![Vec::new(); analyses.len()];
    for (fi, a) in analyses.iter().enumerate() {
        ctx.collect_lines(&a.result.hunks);
        for r in &a.result.manifest.renames {
            let pair = bare_pair(&r.old_name, &r.new_name);
            if is_member(&r.old_name) { local_renames[fi].push(pair) } else { ctx.renames.push(pair) }
        }
    }
    for m in &moves {
        if let Some(new) = &m.renamed_to {
            let pair = bare_pair(&m.name, new);
            if is_member(&m.name) {
                for (fi, f) in files.iter().enumerate() {
                    if f.new_path == m.from_file || f.new_path == m.to_file { local_renames[fi].push(pair.clone()); }
                }
            } else {
                ctx.renames.push(pair);
            }
        }
    }
    let test_spans: Vec<Vec<Location>> = analyses.iter().map(test_locations).collect();
    for (fi, a) in analyses.iter_mut().enumerate() {
        let path = &files[fi].new_path;
        let mut moved_spans: Vec<Location> = a.result.manifest.moved_code.iter()
            .flat_map(|m| [m.from_location.clone(), m.to_location.clone()])
            .collect();
        for m in moves.iter().filter(|m| !m.modified && m.renamed_to.is_none()) {
            if &m.from_file == path { moved_spans.push(m.from_location.clone()); }
            if &m.to_file == path { moved_spans.push(m.to_location.clone()); }
        }
        let generated = a.result.review.generated;
        let detect_comments = a.result.review.parsed;
        let hunks = std::mem::take(&mut a.result.hunks);
        a.result.hunks = split_by_tests(hunks, a.result.review.role, &test_spans[fi]);
        let indent_sensitive = files[fi].language == Language::Python || annotate::is_indent_sensitive(path);
        annotate::annotate_file(
            &mut a.result.hunks, &a.result.manifest, &moved_spans, &ctx, &local_renames[fi],
            generated, detect_comments, indent_sensitive,
        );
        let (changed, mechanical) = a.result.hunks.iter()
            .flat_map(|h| h.changes.iter())
            .filter(|c| c.kind != ChangeKind::Context)
            .fold((0, 0), |(t, m), c| (t + 1, m + c.noise.is_some() as usize));
        a.result.review.changed_lines = changed;
        a.result.review.mechanical_lines = mechanical;
        mark_tests(&mut a.result, &test_spans[fi]);
    }
    let graph = flow::Graph::build(&analyses);
    let reading_order = graph.reading_order(&analyses);
    let test_reach = graph.test_reach(&analyses);
    drop(graph);

    Ok(cross_file::MultiDiffResult {
        file_results: analyses.into_iter().map(|a| (a.path, a.result)).collect(),
        cross_file: cross_file::CrossFileManifest { moves, broken_references, signature_impacts, vanished, reading_order, test_reach },
    })
}

/// Old- and new-side spans of test items inside a source file.
fn test_locations(a: &cross_file::InternalAnalysis) -> Vec<Location> {
    let side = |tree: &parser::SemanticTree, old: bool| -> Vec<Location> {
        tree.items.iter().zip(&tree.meta)
            .filter(|(_, m)| m.is_test)
            .map(|(i, _)| {
                let s = i.span();
                if old { Location::old(s.start_line, s.end_line) } else { Location::new(s.start_line, s.end_line) }
            })
            .collect()
    };
    let mut v = side(&a.old_tree, true);
    v.extend(side(&a.new_tree, false));
    v
}

fn change_is_test(c: &Change, role: roles::FileRole, spans: &[Location]) -> bool {
    if role == roles::FileRole::Test {
        return true;
    }
    match (c.kind, &c.old_span, &c.new_span) {
        (ChangeKind::Removed, Some(s), _) => spans.iter().any(|l| l.contains(Side::Old, s.start_line)),
        (ChangeKind::Added | ChangeKind::Modified, _, Some(s)) => spans.iter().any(|l| l.contains(Side::New, s.start_line)),
        _ => false,
    }
}

/// Split hunks where changes switch between test and non-test code, so inline
/// tests land in their own hunks.
fn split_by_tests(hunks: Vec<DiffHunk>, role: roles::FileRole, spans: &[Location]) -> Vec<DiffHunk> {
    if role == roles::FileRole::Test || spans.is_empty() {
        return hunks;
    }
    let mut out = Vec::with_capacity(hunks.len());
    for h in hunks {
        let mut parts: Vec<Vec<Change>> = vec![Vec::new()];
        let mut current: Option<bool> = None;
        for c in &h.changes {
            if c.kind != ChangeKind::Context {
                let t = change_is_test(c, role, spans);
                if current.is_some_and(|p| p != t) {
                    parts.push(Vec::new());
                }
                current = Some(t);
            }
            parts.last_mut().unwrap().push(c.clone());
        }
        if parts.len() == 1 {
            out.push(h);
        } else {
            out.extend(parts.into_iter().map(hunk_from));
        }
    }
    out
}

/// Flag test hunks and entries, and count changed lines of test code that aren't noise.
fn mark_tests(r: &mut DiffResult, spans: &[Location]) {
    let role = r.review.role;
    if role != roles::FileRole::Test && spans.is_empty() {
        return;
    }
    let mut test_lines = 0;
    for h in &mut r.hunks {
        let mut all = true;
        let mut any = false;
        for c in h.changes.iter().filter(|c| c.kind != ChangeKind::Context) {
            if change_is_test(c, role, spans) {
                any = true;
                test_lines += c.noise.is_none() as usize;
            } else {
                all = false;
            }
        }
        h.test = any && all;
    }
    r.review.test_lines = test_lines;
    let in_test = |loc: &Location| role == roles::FileRole::Test || spans.iter().any(|s| {
        s.side == loc.side && loc.line_start >= s.line_start && loc.line_end <= s.line_end
    });
    let mut ids: Vec<u32> = r.manifest.locations().into_iter().filter(|(_, l)| in_test(l)).map(|(id, _)| id).collect();
    ids.sort_unstable();
    ids.dedup();
    r.manifest.test_entries = ids;
}

fn analyze_file(file: &cross_file::FileChange, next_id: &mut u32) -> Result<cross_file::InternalAnalysis, Error> {
    let path = file.new_path.clone();
    let role = file.role.unwrap_or_else(|| {
        let content = if file.new_source.is_empty() { &file.old_source } else { &file.new_source };
        roles::detect_role(if path.is_empty() { &file.old_path } else { &path }, content)
    });
    let generated = role.is_noise();
    let parsed = file.language != Language::Unknown && !generated;
    let old_empty = file.old_source.trim().is_empty();
    let new_empty = file.new_source.trim().is_empty();

    let (old_tree, new_tree, diff_output, manifest) = if parsed {
        let lang_support = languages::get_language_support(file.language);
        let old_tree = if old_empty { parser::SemanticTree::default() } else { parser::parse(&file.old_source, &*lang_support)? };
        let new_tree = if new_empty { parser::SemanticTree::default() } else { parser::parse(&file.new_source, &*lang_support)? };
        let diff_output = diff::diff(&old_tree, &new_tree, &file.old_source, &file.new_source);
        let manifest = classify::classify(&old_tree, &new_tree, &diff_output, &file.old_source, &file.new_source, next_id);
        (old_tree, new_tree, diff_output, manifest)
    } else {
        (Default::default(), Default::default(), Default::default(), ChangeManifest::default())
    };

    let hunks = match &file.display_hunks {
        Some(h) => h.clone(),
        None if old_empty || new_empty => whole_file_hunk(&file.old_source, &file.new_source),
        None if parsed => clean_ast_hunks(diff_output.hunks.clone()),
        None => {
            let old: Vec<&str> = file.old_source.lines().collect();
            let new: Vec<&str> = file.new_source.lines().collect();
            let span = |n: usize| Span { start_line: 1, start_col: 0, end_line: n.max(1), end_col: 0 };
            let changes = diff::line_diff(&old, &new, &span(old.len()), &span(new.len()));
            context_hunks(changes, 3)
        }
    };

    let mut manifest = manifest;
    let f = if path.is_empty() { None } else { Some(path.clone()) };
    for loc in manifest.locations_mut() {
        loc.file = f.clone();
    }

    let result = DiffResult {
        hunks,
        manifest,
        language_info: LanguageInfo { language: Some(format!("{:?}", file.language)) },
        review: ReviewStats { parsed, generated, role, ..Default::default() },
    };
    Ok(cross_file::InternalAnalysis {
        path,
        result,
        old_tree,
        new_tree,
        diff_output,
        new_source: file.new_source.clone(),
    })
}

/// One hunk covering an entirely added or deleted file.
fn whole_file_hunk(old_source: &str, new_source: &str) -> Vec<DiffHunk> {
    let removed = new_source.trim().is_empty();
    let src = if removed { old_source } else { new_source };
    let lines: Vec<&str> = src.lines().collect();
    if lines.is_empty() {
        return vec![];
    }
    let changes = lines.iter().enumerate().map(|(i, line)| {
        let s = Some(Span { start_line: i + 1, start_col: 0, end_line: i + 1, end_col: line.len() });
        if removed {
            Change { kind: ChangeKind::Removed, old_span: s, new_span: None, content_old: Some(line.to_string()), content_new: None, noise: None }
        } else {
            Change { kind: ChangeKind::Added, old_span: None, new_span: s, content_old: None, content_new: Some(line.to_string()), noise: None }
        }
    }).collect();
    let full = LineRange { start: 1, end: lines.len() };
    let empty = LineRange { start: 0, end: 0 };
    vec![DiffHunk {
        old_range: if removed { full.clone() } else { empty.clone() },
        new_range: if removed { empty } else { full },
        changes,
        manifest_refs: vec![],
        noise: None,
        test: false,
    }]
}

/// Split a flat line diff into hunks with `ctx` lines of context.
fn context_hunks(changes: Vec<Change>, ctx: usize) -> Vec<DiffHunk> {
    let n = changes.len();
    let mut keep = vec![false; n];
    for (i, c) in changes.iter().enumerate() {
        if c.kind != ChangeKind::Context {
            for k in &mut keep[i.saturating_sub(ctx)..(i + ctx + 1).min(n)] { *k = true; }
        }
    }
    let mut hunks = Vec::new();
    let mut cur: Vec<Change> = Vec::new();
    for (i, c) in changes.into_iter().enumerate() {
        if keep[i] {
            cur.push(c);
        } else if !cur.is_empty() {
            hunks.push(hunk_from(std::mem::take(&mut cur)));
        }
    }
    if !cur.is_empty() { hunks.push(hunk_from(cur)); }
    hunks
}

fn hunk_from(changes: Vec<Change>) -> DiffHunk {
    let range = |side: Side| {
        let lines: Vec<usize> = changes.iter()
            .filter_map(|c| if side == Side::Old { c.old_span.as_ref() } else { c.new_span.as_ref() })
            .map(|s| s.start_line)
            .collect();
        LineRange { start: lines.first().copied().unwrap_or(0), end: lines.last().copied().unwrap_or(0) }
    };
    DiffHunk { old_range: range(Side::Old), new_range: range(Side::New), changes, manifest_refs: vec![], noise: None, test: false }
}

/// Drop formatting placeholders from AST hunks and merge overlaps.
fn clean_ast_hunks(hunks: Vec<DiffHunk>) -> Vec<DiffHunk> {
    let hunks: Vec<DiffHunk> = hunks.into_iter()
        .filter(|h| h.changes.iter().any(|c| matches!(c.kind, ChangeKind::Added | ChangeKind::Removed | ChangeKind::Context)))
        .map(|mut h| { h.changes.retain(|c| c.kind != ChangeKind::Modified); h })
        .collect();
    merge_overlapping_hunks(hunks)
}

/// Merge overlapping or adjacent hunks into single hunks.
/// This prevents duplicate hunk headers when multiple semantic items
/// produce hunks that cover the same or overlapping line ranges.
fn merge_overlapping_hunks(mut hunks: Vec<manifest::DiffHunk>) -> Vec<manifest::DiffHunk> {
    if hunks.len() <= 1 {
        return hunks;
    }

    // Sort by new_range start (or old_range if new is 0)
    hunks.sort_by_key(|h| {
        if h.new_range.start > 0 { h.new_range.start }
        else { h.old_range.start }
    });

    let mut merged: Vec<manifest::DiffHunk> = Vec::new();

    for hunk in hunks {
        if let Some(last) = merged.last_mut() {
            let last_end = last.new_range.end.max(last.old_range.end);
            let hunk_start = if hunk.new_range.start > 0 { hunk.new_range.start }
                else { hunk.old_range.start };

            // Merge if overlapping or within 8 lines (avoids fragmented hunks)
            if hunk_start <= last_end + 8 {
                // Extend the range
                last.old_range.end = last.old_range.end.max(hunk.old_range.end);
                last.old_range.start = last.old_range.start.min(
                    if hunk.old_range.start > 0 { hunk.old_range.start } else { last.old_range.start }
                );
                last.new_range.end = last.new_range.end.max(hunk.new_range.end);
                last.new_range.start = last.new_range.start.min(
                    if hunk.new_range.start > 0 { hunk.new_range.start } else { last.new_range.start }
                );
                // Merge changes, deduplicating by line number
                for ch in hunk.changes {
                    let dominated = last.changes.iter().any(|existing| {
                        existing.kind == ch.kind
                            && existing.old_span == ch.old_span
                            && existing.new_span == ch.new_span
                    });
                    if !dominated {
                        last.changes.push(ch);
                    }
                }
                last.manifest_refs.extend(hunk.manifest_refs);
                continue;
            }
        }
        merged.push(hunk);
    }

    // Sort changes within each merged hunk by line number
    for hunk in &mut merged {
        hunk.changes.sort_by_key(|ch| {
            ch.new_span.as_ref().map(|s| s.start_line)
                .or(ch.old_span.as_ref().map(|s| s.start_line))
                .unwrap_or(0)
        });
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rename_ts() {
        let old = r#"function processData(x: string): string { return x.trim(); }"#;
        let new = r#"function normalizeInput(x: string): string { return x.trim(); }"#;
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert_eq!(result.manifest.renames.len(), 1);
        assert_eq!(result.manifest.renames[0].old_name, "processData");
        assert_eq!(result.manifest.renames[0].new_name, "normalizeInput");
    }

    #[test]
    fn test_signature_change_ts() {
        let old = "function auth(user: string): boolean {\n    return true;\n}";
        let new = "function auth(user: string, pass: string): boolean {\n    return true;\n}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert_eq!(result.manifest.signature_changes.len(), 1);
        assert!(result.manifest.signature_changes[0].description.contains("added param"));
    }

    #[test]
    fn test_add_dependency_ts() {
        let old = r#"function foo(): void { }"#;
        let new = "import { z } from \"zod\";\nfunction foo(): void { }";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert_eq!(result.manifest.dependency_changes.len(), 1);
        assert_eq!(result.manifest.dependency_changes[0].name, "zod");
    }

    #[test]
    fn test_logic_change_ts() {
        let old = r#"function greet(name: string): string { return "hi"; }"#;
        let new = r#"function greet(name: string): string { return "hello " + name; }"#;
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert_eq!(result.manifest.logic_changes.len(), 1);
        assert_eq!(result.manifest.logic_changes[0].name, "greet");
    }

    #[test]
    fn test_formatting_only_ts() {
        let old = "function foo(): void {\n  return;\n}";
        let new = "function foo(): void {\n    return;\n}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert_eq!(result.manifest.formatting_only.len(), 1);
        assert!(result.manifest.renames.is_empty());
        assert!(result.manifest.logic_changes.is_empty());
    }

    #[test]
    fn test_rename_py() {
        let old = "def process_data(x):\n    return x.strip()";
        let new = "def normalize_input(x):\n    return x.strip()";
        let result = analyze(old, new, Language::Python).unwrap();
        assert_eq!(result.manifest.renames.len(), 1);
        assert_eq!(result.manifest.renames[0].old_name, "process_data");
        assert_eq!(result.manifest.renames[0].new_name, "normalize_input");
    }

    #[test]
    fn test_no_changes() {
        let src = r#"function foo(): void { return; }"#;
        let result = analyze(src, src, Language::TypeScript).unwrap();
        assert!(result.manifest.renames.is_empty());
        assert!(result.manifest.logic_changes.is_empty());
        assert!(result.manifest.signature_changes.is_empty());
        assert!(result.manifest.formatting_only.is_empty());
        assert!(result.hunks.is_empty());
    }

    #[test]
    fn test_export_class_change() {
        let old = "export class Foo {\n  static bar() { return 1; }\n}";
        let new = "export class Foo {\n  static bar() { return 2; }\n}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        // Class body changed: reported at the member level, not as formatting
        assert_eq!(result.manifest.logic_changes.len(), 1);
        assert_eq!(result.manifest.logic_changes[0].name, "Foo.bar");
        assert!(result.manifest.formatting_only.is_empty());
    }

    #[test]
    fn test_export_class_no_change() {
        let src = "export class Foo {\n  static bar() { return 1; }\n}";
        let result = analyze(src, src, Language::TypeScript).unwrap();
        assert!(result.manifest.logic_changes.is_empty());
        assert!(result.manifest.formatting_only.is_empty());
        assert!(result.hunks.is_empty());
    }

    #[test]
    fn test_export_function_change() {
        let old = "export function greet(): string {\n  return 'hi';\n}";
        let new = "export function greet(): string {\n  return 'hello';\n}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert_eq!(result.manifest.logic_changes.len(), 1);
        assert_eq!(result.manifest.logic_changes[0].name, "greet");
    }

    #[test]
    fn test_export_interface_change() {
        let old = "export interface Config {\n  port: number;\n}";
        let new = "export interface Config {\n  port: number;\n  host: string;\n}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        // Interface changed: should be detected as modified
        assert!(!result.hunks.is_empty());
    }

    #[test]
    fn test_language_from_extension() {
        assert_eq!(Language::from_extension("ts"), Language::TypeScript);
        assert_eq!(Language::from_extension("py"), Language::Python);
        assert_eq!(Language::from_extension("rs"), Language::Rust);
        assert_eq!(Language::from_extension("go"), Language::Go);
        assert_eq!(Language::from_extension("java"), Language::Java);
        assert_eq!(Language::from_extension("c"), Language::C);
        assert_eq!(Language::from_extension("h"), Language::C);
        assert_eq!(Language::from_extension("xyz"), Language::Unknown);
    }

    #[test]
    fn test_rename_rust() {
        let old = "fn process_data(x: i32) -> i32 {\n    x + 1\n}";
        let new = "fn normalize_input(x: i32) -> i32 {\n    x + 1\n}";
        let result = analyze(old, new, Language::Rust).unwrap();
        assert_eq!(result.manifest.renames.len(), 1);
        assert_eq!(result.manifest.renames[0].old_name, "process_data");
        assert_eq!(result.manifest.renames[0].new_name, "normalize_input");
    }

    #[test]
    fn test_logic_change_rust() {
        let old = "fn greet() -> String {\n    \"hi\".to_string()\n}";
        let new = "fn greet() -> String {\n    \"hello world\".to_string()\n}";
        let result = analyze(old, new, Language::Rust).unwrap();
        assert_eq!(result.manifest.logic_changes.len(), 1);
    }

    #[test]
    fn test_rename_go() {
        let old = "package main\n\nfunc processData(x int) int {\n\treturn x + 1\n}";
        let new = "package main\n\nfunc normalizeInput(x int) int {\n\treturn x + 1\n}";
        let result = analyze(old, new, Language::Go).unwrap();
        assert_eq!(result.manifest.renames.len(), 1);
        assert_eq!(result.manifest.renames[0].old_name, "processData");
        assert_eq!(result.manifest.renames[0].new_name, "normalizeInput");
    }

    #[test]
    fn test_logic_change_go() {
        let old = "package main\n\nfunc greet() string {\n\treturn \"hi\"\n}";
        let new = "package main\n\nfunc greet() string {\n\treturn \"hello world\"\n}";
        let result = analyze(old, new, Language::Go).unwrap();
        assert_eq!(result.manifest.logic_changes.len(), 1);
    }

    #[test]
    fn test_logic_change_java() {
        let old = "class Foo {\n    int getValue() { return 1; }\n}";
        let new = "class Foo {\n    int getValue() { return 42; }\n}";
        let result = analyze(old, new, Language::Java).unwrap();
        assert!(!result.hunks.is_empty());
    }

    #[test]
    fn test_rename_c() {
        let old = "int process_data(int x) {\n    return x + 1;\n}";
        let new = "int normalize_input(int x) {\n    return x + 1;\n}";
        let result = analyze(old, new, Language::C).unwrap();
        assert_eq!(result.manifest.renames.len(), 1);
    }

    #[test]
    fn test_extraction_detected() {
        // Use the fixture files which have enough body overlap for 70% threshold
        let old = std::fs::read_to_string("tests/fixtures/extract_function/before.ts")
            .unwrap_or_else(|_| {
                std::fs::read_to_string("../../tests/fixtures/extract_function/before.ts").unwrap()
            });
        let new = std::fs::read_to_string("tests/fixtures/extract_function/after.ts")
            .unwrap_or_else(|_| {
                std::fs::read_to_string("../../tests/fixtures/extract_function/after.ts").unwrap()
            });
        let result = analyze(&old, &new, Language::TypeScript).unwrap();
        assert!(!result.manifest.extracted_functions.is_empty(),
            "Should detect extraction: processUserData split into validateUser + transformUser");
    }

    #[test]
    fn test_dead_code_detected() {
        let old = "function main(): void {\n    helper();\n}\n\nfunction helper(): void {\n    console.log(\"hi\");\n}";
        let new = "function main(): void {\n    console.log(\"hi\");\n}\n\nfunction helper(): void {\n    console.log(\"hi\");\n}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        // helper() is no longer called by main(), so it should be flagged as dead
        assert!(!result.manifest.dead_code.is_empty(),
            "helper should be flagged as dead code after main stopped calling it");
    }

    #[test]
    fn test_dead_code_not_flagged_when_referenced() {
        let old = "function main(): void {\n    helper();\n}\n\nfunction helper(): void {\n    return;\n}";
        let new = "function main(): void {\n    helper();\n    return;\n}\n\nfunction helper(): void {\n    return;\n}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        // helper() is still called, so it should not be flagged
        assert!(result.manifest.dead_code.is_empty(),
            "helper should not be flagged as dead code when still referenced");
    }

    #[test]
    fn test_cross_file_move() {
        let files = vec![
            cross_file::FileChange::new(
                "utils.ts",
                "export function helper(): string {\n    return \"help\";\n}\n\nfunction main(): void {\n    helper();\n}",
                "function main(): void {\n    helper();\n}",
                Language::TypeScript,
            ),
            cross_file::FileChange::new(
                "auth.ts",
                "function login(): void {\n    return;\n}",
                "function login(): void {\n    return;\n}\n\nexport function helper(): string {\n    return \"help\";\n}",
                Language::TypeScript,
            ),
        ];
        let result = analyze_multi(&files).unwrap();
        assert_eq!(result.cross_file.moves.len(), 1, "helper() moved from utils.ts to auth.ts");
        // The move replaces the plain added/removed entries.
        for (_, r) in &result.file_results {
            assert!(r.manifest.logic_changes.iter().all(|l| l.name != "helper"));
        }
    }

    #[test]
    fn test_comment_only_change_is_formatting() {
        let old = "function f(a: number): number {\n  return a + 1;\n}";
        let new = "function f(a: number): number {\n  // add one\n  return a + 1;\n}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert!(result.manifest.logic_changes.is_empty(), "comments are not logic changes");
        assert_eq!(result.manifest.formatting_only.len(), 1);
    }

    #[test]
    fn test_top_level_comments_are_not_items() {
        let old = "// one\nfunction f(): void {}";
        let new = "// two\n/** doc */\nfunction f(): void {}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert!(result.manifest.logic_changes.is_empty());
    }

    #[test]
    fn test_rename_with_edits_is_fuzzy_rename() {
        let old = "function computeTotal(items: Item[]): number {\n  let sum = 0;\n  for (const i of items) {\n    sum += i.price * i.qty;\n  }\n  return sum;\n}";
        let new = "function calculateTotal(items: Item[]): number {\n  let sum = 0;\n  for (const i of items) {\n    sum += i.price * i.quantity;\n  }\n  return sum;\n}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert_eq!(result.manifest.renames.len(), 1);
        assert_eq!(result.manifest.renames[0].new_name, "calculateTotal");
        assert_eq!(result.manifest.logic_changes.len(), 1, "body edit reported alongside the rename");
    }

    #[test]
    fn test_class_member_changes() {
        let old = "export class Svc {\n  a(): number { return 1; }\n  b(): number { return 2; }\n}";
        let new = "export class Svc {\n  a(): number { return 10; }\n  b(): number { return 2; }\n  c(): void {}\n}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        let names: Vec<&str> = result.manifest.logic_changes.iter().map(|l| l.name.as_str()).collect();
        assert!(names.contains(&"Svc.a"), "{names:?}");
        assert!(names.contains(&"Svc.c"), "{names:?}");
        assert!(!names.contains(&"Svc.b"), "{names:?}");
    }

    #[test]
    fn test_exported_function_not_dead() {
        let old = "export function a(): void {}";
        let new = "export function a(): void {}\nexport function b(): void {}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert!(result.manifest.dead_code.is_empty());
    }

    #[test]
    fn test_rust_trait_impl_method_not_dead() {
        let old = "trait T { fn go(&self); }\nstruct S;";
        let new = "trait T { fn go(&self); }\nstruct S;\nimpl T for S {\n    fn go(&self) {}\n}\nfn helper() {}";
        let result = analyze(old, new, Language::Rust).unwrap();
        let dead: Vec<&str> = result.manifest.dead_code.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(dead, vec!["helper"], "only the private free fn is dead");
    }

    #[test]
    fn test_rust_use_grouped_by_module() {
        let old = "use crate::{a, b};\nuse std::collections::HashMap;";
        let new = "use crate::{a, c};\nuse std::collections::HashMap;\nuse serde::Serialize;";
        let result = analyze(old, new, Language::Rust).unwrap();
        let deps = &result.manifest.dependency_changes;
        let changed = deps.iter().find(|d| d.name == "crate").expect("crate import changed");
        assert_eq!(changed.symbols_added, vec!["c"]);
        assert_eq!(changed.symbols_removed, vec!["b"]);
        assert!(changed.internal);
        assert!(deps.iter().any(|d| d.name == "serde" && !d.internal));
        assert_eq!(deps.len(), 2);
    }

    #[test]
    fn test_in_file_move_detected() {
        let old = "function a(): number {\n  return 1;\n}\nfunction b(): number {\n  return 2;\n}\nfunction c(): number {\n  return 3;\n}";
        let new = "function c(): number {\n  return 3;\n}\nfunction a(): number {\n  return 1;\n}\nfunction b(): number {\n  return 2;\n}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert_eq!(result.manifest.moved_code.len(), 1);
        assert_eq!(result.manifest.moved_code[0].name, "c");
        assert!(result.manifest.logic_changes.is_empty());
    }

    #[test]
    fn test_partial_extraction() {
        let old = "function handle(req: Req): Res {\n  const user = lookup(req.id);\n  if (!user.active) {\n    throw new Error('inactive');\n  }\n  const token = sign(user.id, secret);\n  return respond(token);\n}";
        let new = "function handle(req: Req): Res {\n  const user = lookup(req.id);\n  return respond(issue(user));\n}\n\nfunction issue(user: User): string {\n  if (!user.active) {\n    throw new Error('inactive');\n  }\n  const token = sign(user.id, secret);\n  return token;\n}";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert_eq!(result.manifest.extracted_functions.len(), 1, "{:?}", result.manifest);
        assert_eq!(result.manifest.extracted_functions[0].original_name, "handle");
        assert_eq!(result.manifest.extracted_functions[0].extracted_names, vec!["issue"]);
    }

    #[test]
    fn test_broken_reference_after_rename() {
        let files = vec![
            cross_file::FileChange::new(
                "api.ts",
                "export function processData(x: string): string {\n  return x.trim();\n}",
                "export function normalizeInput(x: string): string {\n  return x.trim();\n}",
                Language::TypeScript,
            ),
            cross_file::FileChange::new(
                "main.ts",
                "import { processData } from './api';\nexport const out = processData(' a ');\nexport const n = 1;",
                "import { processData } from './api';\nexport const out = processData(' a ');\nexport const n = 2;",
                Language::TypeScript,
            ),
        ];
        let result = analyze_multi(&files).unwrap();
        let broken = &result.cross_file.broken_references;
        assert!(broken.iter().any(|b| b.reference_file == "main.ts" && b.symbol_name == "processData"), "{broken:?}");
    }

    #[test]
    fn test_method_rename_only_flags_its_own_type() {
        // `Store.flush` is renamed; `self.flush()` in the class and `Store.flush` elsewhere are
        // stale, but another type's `flush` (a writer's, here) is not.
        let files = vec![
            cross_file::FileChange::new(
                "store.py",
                "class Store:\n    def flush(self):\n        return 1\n\n    def close(self):\n        self.flush()\n        return 0\n",
                "class Store:\n    def finish(self):\n        return 1\n\n    def close(self):\n        self.flush()\n        return 0\n",
                Language::Python,
            ),
            cross_file::FileChange::new(
                "main.py",
                "from store import Store\nimport sys\n\ndef run(w):\n    Store.flush(Store())\n    w.flush()\n    sys.stdout.flush()\n    return 1\n",
                "from store import Store\nimport sys\n\ndef run(w):\n    Store.flush(Store())\n    w.flush()\n    sys.stdout.flush()\n    return 2\n",
                Language::Python,
            ),
        ];
        let result = analyze_multi(&files).unwrap();
        let mut hits: Vec<(String, usize)> = result.cross_file.broken_references.iter().map(|b| (b.reference_file.clone(), b.reference_location.line_start)).collect();
        hits.sort();
        assert_eq!(hits, vec![("main.py".to_string(), 5), ("store.py".to_string(), 6)], "{:?}", result.cross_file.broken_references);
        assert_eq!(result.cross_file.vanished[0].owner.as_deref(), Some("Store"));
    }

    #[test]
    fn test_definition_lines() {
        use cross_file::looks_like_definition as def;
        assert!(def("func (s *RegexpWriter) Flush() (int, error) {", "Flush"));
        assert!(def("pub fn flush(&mut self) -> io::Result<()> {", "flush"));
        assert!(def("    def flush(self):", "flush"));
        assert!(def("  public long getCharacterOffset() {", "getCharacterOffset"));
        assert!(def("  flush() {", "flush"));
        assert!(!def("    err = tmpl.Flush()", "Flush"));
        assert!(!def("    if flush() {", "flush"));
        assert!(!def("    x := flush(a)", "flush"));
        assert!(!def("// flush() writes everything", "flush"));
    }

    #[test]
    fn test_scala_items() {
        let src = "package a.b\n\nimport scala.collection.mutable.{Map, ListBuffer}\nimport cats.syntax.all._\n\ncase class User(id: Long, name: String)\n\nsealed trait Event\nobject Event {\n  final case class Created(user: User) extends Event\n  case object Flushed extends Event\n}\n\nenum Color:\n  case Red, Green\n\nobject Svc {\n  val Limit: Int = 100\n  type Id = Long\n  def normalize(name: String, strict: Boolean = false): String = name.trim\n  given Show[User] = Show.show(_.name)\n  extension (u: User)\n    def display: String = u.name\n}\n";
        let lang = languages::get_language_support(Language::Scala);
        let tree = parser::parse(src, &*lang).unwrap();
        let names: Vec<String> = tree.items.iter().filter_map(|i| i.name().map(str::to_string)).collect();
        assert_eq!(names, ["scala.collection.mutable", "cats.syntax.all", "User", "Event", "Event", "Color", "Svc"], "{names:?}");
        let import = &tree.items[0];
        assert!(matches!(import, parser::SemanticItem::Import { symbols, .. } if symbols == &["Map", "ListBuffer"]), "{import:?}");
        let fields = |i: usize| match &tree.items[i] { parser::SemanticItem::Class { fields, .. } => fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>(), _ => vec![] };
        assert_eq!(fields(2), ["id", "name"]);
        assert_eq!(fields(4), ["Created", "Flushed"]);
        assert_eq!(fields(5), ["Red", "Green"]);
        let methods = match &tree.items[6] { parser::SemanticItem::Class { methods, .. } => methods, _ => panic!() };
        let m: Vec<(&str, Vec<bool>)> = methods.iter().map(|m| match m { parser::SemanticItem::Function { name, params, .. } => (name.as_str(), params.iter().map(|p| p.optional).collect()), _ => panic!() }).collect();
        assert_eq!(m, [("normalize", vec![false, true]), ("given Show[User]", vec![])]);
        assert_eq!(fields(6), ["Limit", "Id"]);
    }

    #[test]
    fn test_signature_impact_marks_stale_call_sites() {
        let files = vec![
            cross_file::FileChange::new(
                "lib.ts",
                "export function auth(user: string): boolean {\n  return !!user;\n}",
                "export function auth(user: string, pass: string): boolean {\n  return !!user && !!pass;\n}",
                Language::TypeScript,
            ),
            cross_file::FileChange::new(
                "app.ts",
                "export const a = auth('x');\nexport const b = auth('y');",
                "export const a = auth('x', 'p');\nexport const b = auth('y');",
                Language::TypeScript,
            ),
        ];
        let result = analyze_multi(&files).unwrap();
        let impact = &result.cross_file.signature_impacts[0];
        assert_eq!(impact.call_sites.len(), 2);
        assert!(impact.call_sites.iter().any(|c| c.line == 1 && c.updated));
        assert!(impact.call_sites.iter().any(|c| c.line == 2 && !c.updated));
    }

    #[test]
    fn test_noise_annotation_rename_and_formatting() {
        let files = vec![cross_file::FileChange::new(
            "a.ts",
            "function oldName(x: number): number {\n  return x * 2;\n}\nexport const v = oldName(2);\nexport const w = 1+2;",
            "function newName(x: number): number {\n  return x * 2;\n}\nexport const v = newName(2);\nexport const w = 1 + 2;",
            Language::TypeScript,
        )];
        let result = analyze_multi(&files).unwrap();
        let r = &result.file_results[0].1;
        assert_eq!(r.review.changed_lines, r.review.mechanical_lines, "rename + reformat is all mechanical: {:?}", r.hunks);
        assert!(r.hunks.iter().all(|h| !h.manifest_refs.is_empty() || h.noise.is_some()));
    }

    #[test]
    fn test_unknown_language_still_diffs() {
        let result = analyze("a\nb\nc\n", "a\nB\nc\n", Language::Unknown).unwrap();
        assert!(!result.review.parsed);
        assert_eq!(result.review.changed_lines, 2);
    }

    #[test]
    fn test_new_file_items_classified() {
        let result = analyze("", "export function a(): void {}\nfunction b(): void {}", Language::TypeScript).unwrap();
        assert_eq!(result.hunks.len(), 1);
        assert_eq!(result.manifest.logic_changes.len(), 2);
    }

    #[test]
    fn test_generated_file_is_noise() {
        let files = vec![cross_file::FileChange::new("Cargo.lock", "a = 1\n", "a = 2\n", Language::Unknown)];
        let result = analyze_multi(&files).unwrap();
        let r = &result.file_results[0].1;
        assert!(r.review.generated);
        assert_eq!(r.review.mechanical_lines, r.review.changed_lines);
    }

    #[test]
    fn test_python_docstring_ignored_top_level_call_kept() {
        let old = "\"\"\"Module doc.\"\"\"\ndef main():\n    pass\n\nmain()";
        let new = "\"\"\"Updated doc.\"\"\"\ndef main():\n    pass\n\nmain(1)";
        let result = analyze(old, new, Language::Python).unwrap();
        assert_eq!(result.manifest.logic_changes.len(), 1, "{:?}", result.manifest.logic_changes);
        assert!(result.manifest.logic_changes[0].name.starts_with("main("));
    }

    #[test]
    fn test_tsx_parses_jsx() {
        let old = "export function App() {\n  return <div>hi</div>;\n}";
        let new = "export function App() {\n  return <div>hello</div>;\n}";
        let result = analyze(old, new, Language::Tsx).unwrap();
        assert_eq!(result.manifest.logic_changes.len(), 1);
        assert_eq!(result.manifest.logic_changes[0].name, "App");
    }

    #[test]
    fn test_rust_attributes_are_not_items() {
        let old = "struct A;";
        let new = "#[derive(Debug)]\nstruct A;\n#[derive(Clone)]\nstruct B;";
        let result = analyze(old, new, Language::Rust).unwrap();
        let names: Vec<(&str, &str)> = result.manifest.logic_changes.iter()
            .map(|l| (l.name.as_str(), l.description.as_str()))
            .collect();
        // The derive on A is part of A's declaration, not a standalone "added" item.
        assert_eq!(names, vec![("A", "declaration modified"), ("B", "added")], "{names:?}");
    }

    #[test]
    fn test_rust_attribute_change_is_visible() {
        for (old, new) in [
            ("#[derive(Debug)]\nstruct A { x: u32 }", "#[derive(Debug, Clone, Copy)]\nstruct A { x: u32 }"),
            ("#[test]\nfn t() {\n    run();\n}", "fn t() {\n    run();\n}"),
        ] {
            let result = analyze(old, new, Language::Rust).unwrap();
            assert_eq!(result.manifest.logic_changes.len(), 1, "{new}: {:?}", result.manifest);
            assert!(!result.hunks.is_empty(), "{new}: no hunks");
            assert!(result.hunks.iter().all(|h| h.noise.is_none() && !h.manifest_refs.is_empty()), "{new}: {:?}", result.hunks);
        }
    }

    #[test]
    fn test_function_modifier_change_is_reported() {
        for (old, new, lang) in [
            ("fn foo() -> u32 {\n    1\n}", "pub fn foo() -> u32 {\n    1\n}", Language::Rust),
            ("fn foo() -> u32 {\n    1\n}", "async fn foo() -> u32 {\n    1\n}", Language::Rust),
            ("function foo(): number {\n  return 1;\n}", "async function foo(): number {\n  return 1;\n}", Language::TypeScript),
            ("class S {\n  get(): number {\n    return 1;\n  }\n}", "class S {\n  async get(): number {\n    return 1;\n  }\n}", Language::TypeScript),
        ] {
            let result = analyze(old, new, lang).unwrap();
            let logic: Vec<(&str, &str)> = result.manifest.logic_changes.iter()
                .map(|l| (l.name.as_str(), l.description.as_str()))
                .collect();
            assert_eq!(logic.len(), 1, "{new}: {logic:?}");
            assert_eq!(logic[0].1, "declaration modified", "{new}");
        }
    }

    #[test]
    fn test_renesting_statements_is_not_formatting() {
        let old = "def f(c):\n    if c:\n        a()\n        b()\n";
        let new = "def f(c):\n    if c:\n        a()\n    b()\n";
        let result = analyze(old, new, Language::Python).unwrap();
        assert!(result.manifest.formatting_only.is_empty(), "{:?}", result.manifest.formatting_only);
        assert_eq!(result.manifest.logic_changes.len(), 1);
        assert!(result.hunks.iter().any(|h| h.noise.is_none()), "{:?}", result.hunks);
    }

    #[test]
    fn test_string_whitespace_is_not_formatting() {
        let files = vec![cross_file::FileChange::new(
            "a.ts",
            "export function f(xs: string[]): string {\n  return xs.join(\", \");\n}",
            "export function f(xs: string[]): string {\n  return xs.join(\",\");\n}",
            Language::TypeScript,
        )];
        let result = analyze_multi(&files).unwrap();
        let r = &result.file_results[0].1;
        assert_eq!(r.review.mechanical_lines, 0, "{:?}", r.hunks);
    }

    #[test]
    fn test_python_reindent_is_not_formatting() {
        // Git-style display hunks: the moved line changes only its indentation.
        let old = "def f(c):\n    if c:\n        a()\n        b()\n";
        let new = "def f(c):\n    if c:\n        a()\n    b()\n";
        let old_lines: Vec<&str> = old.lines().collect();
        let new_lines: Vec<&str> = new.lines().collect();
        let span = |n: usize| manifest::Span { start_line: 1, start_col: 0, end_line: n, end_col: 0 };
        let hunks = context_hunks(diff::line_diff(&old_lines, &new_lines, &span(4), &span(4)), 3);
        let mut file = cross_file::FileChange::new("a.py", old, new, Language::Python);
        file.display_hunks = Some(hunks);
        let result = analyze_multi(&[file]).unwrap();
        assert_eq!(result.file_results[0].1.review.mechanical_lines, 0, "{:?}", result.file_results[0].1.hunks);
    }

    #[test]
    fn test_member_rename_does_not_hide_other_files() {
        let files = vec![
            cross_file::FileChange::new(
                "a.ts",
                "export class Svc {\n  get(k: string): number {\n    return lookup(k) + 1;\n  }\n}",
                "export class Svc {\n  fetch(k: string): number {\n    return lookup(k) + 1;\n  }\n}",
                Language::TypeScript,
            ),
            cross_file::FileChange::new(
                "b.ts",
                "export const v = m.get('k');",
                "export const v = m.fetch('k');",
                Language::TypeScript,
            ),
        ];
        let result = analyze_multi(&files).unwrap();
        assert_eq!(result.file_results[0].1.manifest.renames.len(), 1);
        let b = &result.file_results[1].1;
        assert_eq!(b.review.mechanical_lines, 0, "{:?}", b.hunks);
    }

    #[test]
    fn test_trivial_identical_functions_are_not_renames() {
        let old = "fn on_open() -> bool { true }\nfn on_save() -> bool { true }";
        let new = "fn on_close() -> bool { true }\nfn on_quit() -> bool { true }";
        let result = analyze(old, new, Language::Rust).unwrap();
        assert!(result.manifest.renames.is_empty(), "{:?}", result.manifest.renames);
        // A lone small rename is still recognized.
        let result = analyze("fn on_open() -> bool { true }", "fn opened() -> bool { true }", Language::Rust).unwrap();
        assert_eq!(result.manifest.renames.len(), 1);
    }

    #[test]
    fn test_inline_rust_tests_are_tiered_and_linked() {
        let old = "pub fn parse(s: &str) -> u32 {\n    s.len() as u32\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n}\n";
        let new = "pub fn parse(s: &str) -> u32 {\n    s.trim().len() as u32\n}\n\npub fn other() -> u32 {\n    7\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn trims() {\n        assert_eq!(parse(\" a \"), 1);\n    }\n}\n";
        let files = vec![cross_file::FileChange::new("src/lib.rs", old, new, Language::Rust)];
        let result = analyze_multi(&files).unwrap();
        let r = &result.file_results[0].1;
        let names: Vec<&str> = r.manifest.logic_changes.iter().map(|l| l.name.as_str()).collect();
        assert!(names.contains(&"tests::trims"), "inline test fn is its own item: {names:?}");
        let trims = r.manifest.logic_changes.iter().find(|l| l.name == "tests::trims").unwrap();
        assert!(r.manifest.test_entries.contains(&trims.id));
        assert!(r.hunks.iter().any(|h| h.test), "{:?}", r.hunks);
        assert!(r.hunks.iter().any(|h| !h.test));
        assert!(r.review.test_lines > 0 && r.review.test_lines < r.review.changed_lines);
        let reach: Vec<(&str, Vec<String>)> = result.cross_file.test_reach.iter().map(|c| (c.name.as_str(), c.via.clone())).collect();
        assert_eq!(reach, vec![("parse", vec!["tests::trims".to_string(), "parse".to_string()]), ("other", vec![])], "{reach:?}");
    }

    #[test]
    fn test_test_file_role() {
        let files = vec![
            cross_file::FileChange::new("pkg/a.go", "package a\n\nfunc Add(x, y int) int {\n\treturn x + y\n}\n", "package a\n\nfunc Add(x, y int) int {\n\treturn y + x\n}\n", Language::Go),
            cross_file::FileChange::new("pkg/a_test.go", "package a\n", "package a\n\nfunc TestAdd(t *testing.T) {\n\tif Add(1, 2) != 3 {\n\t\tt.Fatal()\n\t}\n}\n", Language::Go),
        ];
        let result = analyze_multi(&files).unwrap();
        let t = &result.file_results[1].1;
        assert_eq!(t.review.role, roles::FileRole::Test);
        assert!(t.hunks.iter().all(|h| h.test));
        assert_eq!(result.cross_file.test_reach.len(), 1);
        assert_eq!(result.cross_file.test_reach[0].tests[0].file, "pkg/a_test.go");
        assert_eq!(result.cross_file.test_reach[0].via, vec!["TestAdd", "Add"]);
    }

    #[test]
    fn test_reading_order_follows_calls() {
        // handle → (unchanged) route → validate; handle → save (in another file).
        let old_a = "import { save } from './b';\nexport function handle(r: Req) {\n  route(r);\n  return 1;\n}\nfunction route(r: Req) {\n  validate(r);\n}\nfunction validate(r: Req) {\n  return r.ok;\n}\n";
        let new_a = "import { save } from './b';\nexport function handle(r: Req) {\n  route(r);\n  save(r);\n  return 2;\n}\nfunction route(r: Req) {\n  validate(r);\n}\nfunction validate(r: Req) {\n  return r.ok && r.id > 0;\n}\n";
        let old_b = "export function save(r: Req) {\n  db.put(r);\n}\nexport function unrelated() {\n  return 1;\n}\n";
        let new_b = "export function save(r: Req) {\n  db.put(r.id, r);\n}\nexport function unrelated() {\n  return 2;\n}\n";
        let files = vec![
            cross_file::FileChange::new("a.ts", old_a, new_a, Language::TypeScript),
            cross_file::FileChange::new("b.ts", old_b, new_b, Language::TypeScript),
        ];
        let result = analyze_multi(&files).unwrap();
        let order: Vec<(usize, &str)> = result.cross_file.reading_order.iter().map(|s| (s.depth, s.name.as_str())).collect();
        assert_eq!(order, vec![(0, "handle"), (1, "validate"), (1, "save"), (0, "unrelated")], "{order:?}");
        // Without the import, `save` in b.ts can't be what a.ts calls.
        let unimported = vec![
            cross_file::FileChange::new("a.ts", &old_a[old_a.find('\n').unwrap() + 1..], &new_a[new_a.find('\n').unwrap() + 1..], Language::TypeScript),
            cross_file::FileChange::new("b.ts", old_b, new_b, Language::TypeScript),
        ];
        let result = analyze_multi(&unimported).unwrap();
        let save = result.cross_file.reading_order.iter().find(|s| s.name == "save").unwrap();
        assert_eq!(save.depth, 0, "not linked without an import");
        let validate = result.cross_file.reading_order.iter().find(|s| s.name == "validate").unwrap();
        assert_eq!(validate.called_by, vec!["handle"]);
        assert!(!validate.entry_ids.is_empty());
    }

    #[test]
    fn test_private_signature_change_ignores_other_files() {
        let files = vec![
            cross_file::FileChange::new("a.ts", "function run(x: number) {\n  return x;\n}\nrun(1);\n", "function run(x: number, y: number) {\n  return x + y;\n}\nrun(1, 2);\n", Language::TypeScript),
            cross_file::FileChange::new("b.ts", "export const a = 1;\n", "export const a = run(3);\n", Language::TypeScript),
            cross_file::FileChange::new("NOTES.md", "", "call run(x) here\n", Language::Unknown),
        ];
        let result = analyze_multi(&files).unwrap();
        let impact = &result.cross_file.signature_impacts[0];
        assert!(!impact.exported);
        assert!(impact.call_sites.iter().all(|c| c.file == "a.ts"), "{:?}", impact.call_sites);
    }

    #[test]
    fn test_optional_param_is_not_breaking_and_strings_are_not_calls() {
        let old = "class Student:\n    def decide(self, text):\n        return text\n\n\ndef use(s):\n    return s.decide(\"hi\")\n";
        let new = "class Student:\n    def decide(self, text, route=None):\n        return text\n\n\ndef use(s):\n    return s.decide(\"hi\")\n";
        let files = vec![cross_file::FileChange::new("a.py", old, new, Language::Python)];
        let result = analyze_multi(&files).unwrap();
        assert!(result.cross_file.signature_impacts.is_empty(), "optional param: {:?}", result.cross_file.signature_impacts);
        assert!(result.file_results[0].1.manifest.signature_changes[0].description.contains("optional"));

        // A required param is breaking, but a docstring mention is not a call site.
        let new2 = "class Student:\n    def decide(self, text, route):\n        return text\n\n\ndef use(s):\n    \"\"\"Calls s.decide(text) once.\"\"\"\n    return s.decide(\"hi\")\n";
        let files = vec![cross_file::FileChange::new("a.py", old, new2, Language::Python)];
        let result = analyze_multi(&files).unwrap();
        let sites: Vec<usize> = result.cross_file.signature_impacts[0].call_sites.iter().map(|c| c.line).collect();
        assert_eq!(sites, vec![8], "only the real call: {sites:?}");
    }

    #[test]
    fn test_destructured_param_described_by_fields() {
        let old = "export function upd({ id, label, status }: { id: string; label?: string; status?: string }) {\n  return id;\n}\n";
        let new = "export function upd({ id, label }: { id: string; label?: string }) {\n  return id;\n}\n";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        let d = &result.manifest.signature_changes[0].description;
        assert!(d.contains("removed field `status`") && !d.contains('\n'), "{d}");
    }

    #[test]
    fn test_arrow_function_constants_are_functions() {
        let old = "const helper = (x: number): number => x + 1;\nexport const run = (a: number) => {\n  return helper(a);\n};\n";
        let new = "const helper = (x: number): number => x + 2;\nexport const run = (a: number, b = 0) => {\n  return helper(a) + b;\n};\n";
        let files = vec![cross_file::FileChange::new("a.ts", old, new, Language::TypeScript)];
        let result = analyze_multi(&files).unwrap();
        let m = &result.file_results[0].1.manifest;
        let logic: Vec<String> = m.logic_changes.iter().map(|l| format!("{}: {}", l.name, l.description)).collect();
        assert!(logic.contains(&"helper: body modified".to_string()), "{logic:?}");
        assert_eq!(m.signature_changes[0].description, "added optional param `b`");
        let order: Vec<(usize, &str)> = result.cross_file.reading_order.iter().map(|s| (s.depth, s.name.as_str())).collect();
        assert_eq!(order, vec![(0, "run"), (1, "helper")], "{order:?}");
    }

    #[test]
    fn test_type_only_change_and_signatures_raise_no_call_sites() {
        // Widening a parameter's type needs no caller changes.
        let old = "export function pre(v: NoUndef<T>): T {\n  return v;\n}\nexport const a = pre(1);\n";
        let new = "export function pre(v: T): T {\n  return v;\n}\nexport const a = pre(1);\n";
        let result = analyze(old, new, Language::TypeScript).unwrap();
        assert_eq!(result.manifest.signature_changes.len(), 1, "still reported as a signature change");
        let files = vec![cross_file::FileChange::new("a.ts", old, new, Language::TypeScript)];
        assert!(analyze_multi(&files).unwrap().cross_file.signature_impacts.is_empty());
        // An overload signature in an interface is not a call.
        assert!(cross_file::scan_calls("interface S {\n  pre(def: T): S;\n  pre(def?: U): S;\n}\nx.pre(1);\n", "pre", &|_| true)
            .iter().map(|(l, _)| *l).eq([5]));
    }

    #[test]
    fn test_add_import_c() {
        let old = "#include <stdio.h>\nint foo() { return 1; }";
        let new = "#include <stdio.h>\n#include <stdlib.h>\nint foo() { return 1; }";
        let result = analyze(old, new, Language::C).unwrap();
        assert_eq!(result.manifest.dependency_changes.len(), 1);
    }
}
