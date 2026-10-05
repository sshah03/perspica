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
    // Unused code in tests is nearly always on purpose (type tests, fixtures kept for later).
    for a in analyses.iter_mut().filter(|a| a.result.review.role == roles::FileRole::Test) {
        a.result.manifest.dead_code.clear();
    }
    let vanished = cross_file::vanished_names(&analyses, &moves).into_iter()
        .map(|(name, renamed_to, origin, owner)| cross_file::VanishedSymbol { name, renamed_to, origin, owner })
        .collect();
    let broken_references = cross_file::detect_broken_references(&analyses, &moves, &mut next_id);
    let signature_impacts = cross_file::detect_signature_impacts(&analyses, &mut next_id);

    // Annotate hunks with manifest links and noise.
    // Renames of top-level items apply everywhere; member renames (`Svc.get`,
    // `Type::new`) only in their own file, since a bare `get` elsewhere is
    // usually some other type's method, unless the name is distinctive (`write_usage`).
    let is_member = |name: &str| parser::bare_name(name) != name && !cross_file::distinctive(parser::bare_name(name));
    let bare_pair = |old: &str, new: &str| (parser::bare_name(old).to_string(), parser::bare_name(new).to_string());
    // Only identifiers get renamed. A destructuring pattern that gained a name is an edit.
    let identifier = |n: &str| !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$');
    let mut ctx = annotate::AnnotateContext::default();
    let mut local_renames: Vec<Vec<(String, String)>> = vec![Vec::new(); analyses.len()];
    let lines: Vec<(Vec<&str>, Vec<&str>)> = files.iter().map(|f| (f.old_source.lines().collect(), f.new_source.lines().collect())).collect();
    let bodies: Vec<Vec<(manifest::Span, manifest::Span)>> = analyses.iter().map(function_pairs).collect();
    for (fi, a) in analyses.iter().enumerate() {
        ctx.collect_lines(&a.result.hunks, &file_facts(fi, &files[fi], &lines[fi], &bodies[fi], &a.old_tree, &a.new_tree));
        for r in &a.result.manifest.renames {
            let pair = bare_pair(&r.old_name, &r.new_name);
            if !identifier(&pair.0) || !identifier(&pair.1) { continue; }
            if is_member(&r.old_name) { local_renames[fi].push(pair) } else { ctx.renames.push(pair) }
        }
    }
    for m in &moves {
        if let Some(new) = &m.renamed_to {
            let pair = bare_pair(&m.name, new);
            if !identifier(&pair.0) || !identifier(&pair.1) { continue; }
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
        let facts = file_facts(fi, &files[fi], &lines[fi], &bodies[fi], &a.old_tree, &a.new_tree);
        annotate::annotate_file(
            &mut a.result.hunks, &a.result.manifest, &moved_spans, &ctx, &local_renames[fi], &facts,
            generated, detect_comments,
        );
        let (changed, mechanical) = a.result.hunks.iter()
            .flat_map(|h| h.changes.iter())
            .filter(|c| c.kind != ChangeKind::Context)
            .fold((0, 0), |(t, m), c| (t + 1, m + c.noise.is_some() as usize));
        a.result.review.changed_lines = changed;
        a.result.review.mechanical_lines = mechanical;
        only_renamed_references(&mut a.result);
        mark_tests(&mut a.result, &test_spans[fi]);
    }
    let graph = flow::Graph::build(&analyses);
    let graph_debug = std::env::var_os("PERSPICA_GRAPH_DEBUG").map(|_| graph.debug(&analyses));
    let reading_order = graph.reading_order(&analyses);
    let test_reach = graph.test_reach(&analyses);
    drop(graph);

    Ok(cross_file::MultiDiffResult {
        file_results: analyses.into_iter().map(|a| (a.path, a.result)).collect(),
        cross_file: cross_file::CrossFileManifest { moves, broken_references, signature_impacts, vanished, reading_order, test_reach, graph_debug },
    })
}

fn file_facts<'a>(
    fi: usize,
    file: &cross_file::FileChange,
    lines: &'a (Vec<&'a str>, Vec<&'a str>),
    bodies: &'a [(manifest::Span, manifest::Span)],
    old_tree: &'a parser::SemanticTree,
    new_tree: &'a parser::SemanticTree,
) -> annotate::FileFacts<'a> {
    annotate::FileFacts {
        file: fi,
        lines: (&lines.0, &lines.1),
        comments: (&old_tree.comment_lines, &new_tree.comment_lines),
        strings: (&old_tree.string_lines, &new_tree.string_lines),
        literals: (&old_tree.literal_lines, &new_tree.literal_lines),
        bodies,
        indent_sensitive: file.language == Language::Python || annotate::is_indent_sensitive(&file.new_path),
        unparsed: file.language == Language::Unknown,
    }
}

/// Items where every changed line only follows a rename elsewhere, like `old_name(x)` to
/// `new_name(x)`. These aren't logic changes, so they move to `formatting_only` with the same id.
fn only_renamed_references(result: &mut DiffResult) {
    let mut moved = Vec::new();
    for (k, e) in result.manifest.logic_changes.iter().enumerate() {
        if matches!(e.description.as_str(), "added" | "removed") || e.location.side != Side::New { continue; }
        let (a, b) = (e.location.line_start, e.location.line_end);
        let mut renamed = false;
        let mut other = false;
        for h in &result.hunks {
            // Removed lines count toward the next new-side line.
            let mut upcoming = h.new_range.end.max(h.new_range.start);
            let mut at = vec![0usize; h.changes.len()];
            for (i, c) in h.changes.iter().enumerate().rev() {
                if let Some(s) = &c.new_span { upcoming = s.start_line; }
                at[i] = upcoming;
            }
            for (i, c) in h.changes.iter().enumerate() {
                if c.kind == ChangeKind::Context || !(a..=b).contains(&at[i]) { continue; }
                match c.noise {
                    Some(manifest::Noise::Rename) => renamed = true,
                    Some(manifest::Noise::Formatting | manifest::Noise::Comment) => {}
                    _ => other = true,
                }
            }
        }
        if renamed && !other { moved.push(k); }
    }
    for k in moved.into_iter().rev() {
        let e = result.manifest.logic_changes.remove(k);
        result.manifest.formatting_only.push(manifest::FormattingEntry {
            id: e.id,
            location: e.location,
            description: format!("{}: only renamed references", e.name),
        });
    }
}

/// (old, new) spans of every function and method present on both sides of a file.
fn function_pairs(a: &cross_file::InternalAnalysis) -> Vec<(manifest::Span, manifest::Span)> {
    let mut out = Vec::new();
    for p in &a.diff_output.matched {
        match (&a.old_tree.items[p.old_idx], &a.new_tree.items[p.new_idx]) {
            (o @ parser::SemanticItem::Function { .. }, n @ parser::SemanticItem::Function { .. }) => out.push((o.span().clone(), n.span().clone())),
            (parser::SemanticItem::Class { methods: om, .. }, parser::SemanticItem::Class { methods: nm, .. }) => {
                for n in nm {
                    if let Some(o) = om.iter().find(|o| o.name().is_some() && o.name() == n.name()) {
                        out.push((o.span().clone(), n.span().clone()));
                    }
                }
            }
            _ => {}
        }
    }
    out
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
        assert!(def("  case class Opt(group: Seq[String])", "Opt"));
        assert!(def("  object Opt {", "Opt"));
        assert!(def("pub struct Opt;", "Opt"));
        assert!(def("const Opt = 3;", "Opt"));
        assert!(!def("    val x = Opt(Seq())", "Opt"));
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
        assert_eq!(fields(4), ["Created", "object Flushed"]);
        assert_eq!(fields(5), ["Red", "Green"]);
        let methods = match &tree.items[6] { parser::SemanticItem::Class { methods, .. } => methods, _ => panic!() };
        let m: Vec<(&str, Vec<bool>)> = methods.iter().map(|m| match m { parser::SemanticItem::Function { name, params, .. } => (name.as_str(), params.iter().map(|p| p.optional).collect()), _ => panic!() }).collect();
        assert_eq!(m, [("normalize", vec![false, true]), ("given Show[User]", vec![])]);
        assert_eq!(fields(6), ["Limit", "Id"]);
    }

    #[test]
    fn test_calls_are_spotted_with_their_qualifier() {
        use parser::{CallRef, Qualifier};
        let src = "struct A;\nimpl A {\n    fn run(&self, b: &B) -> u32 {\n        self.step();\n        B::make(1);\n        helper::<u32>(2);\n        b.go();\n        f().twice();\n        b.go(x.y()).again();\n        assert_eq!(check(3), 4);\n        let total = self.count;\n        total\n    }\n}\n";
        let lang = languages::get_language_support(Language::Rust);
        let tree = parser::parse(src, &*lang).unwrap();
        let run = tree.items.iter().position(|i| i.name() == Some("A::run")).expect("A::run item");
        let calls = &tree.meta[run].calls;
        let has = |q: Qualifier, name: &str, parens: bool| calls.contains(&CallRef { qualifier: q, name: parser::ident_hash(name), parens });
        assert!(has(Qualifier::SelfType, "step", true), "{calls:?}");
        assert!(has(Qualifier::Path(parser::ident_hash("B")), "make", true));
        assert!(has(Qualifier::None, "helper", true), "turbofish");
        assert!(has(Qualifier::Named(parser::ident_hash("b")), "go", true));
        assert!(has(Qualifier::Unknown, "twice", true), "after a call result");
        assert!(has(Qualifier::Named(parser::ident_hash("b")), "again", true), "a chain keeps its root: {calls:?}");
        assert!(has(Qualifier::None, "check", true), "inside a macro");
        // A field read and the definition's own name are not calls.
        assert!(!calls.iter().any(|c| c.name == parser::ident_hash("count") && c.parens));
        assert!(!calls.iter().any(|c| c.name == parser::ident_hash("run")));
        // `b: &B` binds b to B; so does `let w = a::Widget::new(…)`.
        let src2 = "fn f(b: &B) {\n    let w = a::Widget::new(1);\n    let n: u32 = 2;\n    w.go(b, n);\n}\n";
        let tree2 = parser::parse(src2, &*lang).unwrap();
        let binds = &tree2.meta[0].binds;
        assert_eq!(binds.get(&parser::ident_hash("w")), Some(&parser::ident_hash("Widget")), "{binds:?}");
        assert!(!binds.contains_key(&parser::ident_hash("n")), "primitives don't bind");
        // `T::new(…).unwrap()` is still a T; `T::new().build()` may not be.
        let src3 = "fn f() {\n    let m = Matcher::new(1).unwrap();\n    let p = Builder::new().build(2);\n}\n";
        let tree3 = parser::parse(src3, &*lang).unwrap();
        let binds = &tree3.meta[0].binds;
        assert_eq!(binds.get(&parser::ident_hash("m")), Some(&parser::ident_hash("Matcher")), "{binds:?}");
        assert!(!binds.contains_key(&parser::ident_hash("p")), "{binds:?}");
    }

    #[test]
    fn test_calls_resolve_through_their_qualifier() {
        let new = "struct Set;\nimpl Set {\n    fn add(&self) {}\n}\nstruct Other;\nimpl Other {\n    fn add(&self) {}\n}\n\
            struct A {\n    s: Set,\n}\nimpl A {\n    fn run(&self, xs: Vec<u32>) {\n        self.s.add();\n        self.b();\n        xs.iter().for_each(helper);\n    }\n    fn b(&self) {}\n}\nfn helper(x: &u32) {}\n";
        let files = vec![cross_file::FileChange::new("a.rs", "", new, Language::Rust)];
        let result = analyze_multi(&files).unwrap();
        let callers = |name: &str| -> Vec<String> {
            result.cross_file.reading_order.iter().find(|s| s.name == name).map(|s| s.called_by.clone()).unwrap_or_default()
        };
        // `self.s.add()` is Set's add (the field's type), not Other's.
        assert_eq!(callers("Set::add"), vec!["A::run"], "{:?}", result.cross_file.reading_order);
        assert!(callers("Other::add").is_empty());
        // A short name, settled by `self.`; a function passed by name.
        assert_eq!(callers("A::b"), vec!["A::run"]);
        assert_eq!(callers("helper"), vec!["A::run"]);
    }

    #[test]
    fn test_go_struct_fields_are_extracted() {
        let tree = parser::parse("package a\n\ntype Opts struct {\n\tName string\n\tA, B int\n\t*Base\n}\n", &*languages::get_language_support(Language::Go)).unwrap();
        let Some(parser::SemanticItem::Class { fields, .. }) = tree.items.iter().find(|i| i.name() == Some("Opts")) else { panic!("{:?}", tree.items) };
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Name", "A", "B", "Base"]);
        assert_eq!(fields[1].type_annotation.as_deref(), Some("int"));
    }

    #[test]
    fn test_receivers_take_the_type_a_call_returns() {
        let new = "class Ctx:\n    def pop(self):\n        pass\n\nclass Globals:\n    def pop(self):\n        pass\n\n\
            class App:\n    def ctx(self) -> Ctx:\n        return Ctx()\n\n    def run(self):\n        c = self.ctx()\n        c.pop()\n        d = {}\n        d.pop()\n";
        // Every method changes (each `pass` was `return 1`), so each is a step of its own.
        let old = new.replace("        pass\n", "        return 1\n").replace("        d.pop()\n", "");
        let files = vec![cross_file::FileChange::new("app.py", &old, new, Language::Python)];
        let result = analyze_multi(&files).unwrap();
        let callers = |name: &str| -> Vec<String> {
            result.cross_file.reading_order.iter().find(|s| s.name == name).map(|s| s.called_by.clone()).unwrap_or_default()
        };
        // `c = self.ctx()` makes c a Ctx; `d.pop()` on a dict is nobody's pop.
        assert_eq!(callers("Ctx.pop"), vec!["App.run"], "{:?}", result.cross_file.reading_order);
        assert!(callers("Globals.pop").is_empty(), "{:?}", result.cross_file.reading_order);
    }

    #[test]
    fn test_fields_take_the_type_they_are_assigned() {
        let new = "class Parser:\n    def wait_ready(self):\n        pass\n\nclass ReadAhead:\n    def wait_ready(self):\n        pass\n\n\
            class Conn:\n    def __init__(self):\n        self._parser = Parser()\n\n    def handle(self):\n        self._parser.wait_ready()\n";
        let old = new.replace("        pass\n", "        return 1\n").replace("        self._parser.wait_ready()\n", "        return 2\n");
        let files = vec![cross_file::FileChange::new("conn.py", &old, new, Language::Python)];
        let result = analyze_multi(&files).unwrap();
        let callers = |name: &str| -> Vec<String> {
            result.cross_file.reading_order.iter().find(|s| s.name == name).map(|s| s.called_by.clone()).unwrap_or_default()
        };
        // Two unrelated classes have `wait_ready`; `self._parser = Parser()` says which.
        assert_eq!(callers("Parser.wait_ready"), vec!["Conn.handle"], "{:?}", result.cross_file.reading_order);
        assert!(callers("ReadAhead.wait_ready").is_empty());
    }

    #[test]
    fn test_constructors_and_what_they_build() {
        let new = "class Stream:\n    def read(self):\n        return 2\n\nclass File:\n    def __init__(self, p):\n        self.p = 2\n\n    def encode(self) -> Stream:\n        return 2\n\n\
            class Other:\n    def encode(self):\n        return 2\n\n    def read(self):\n        return 2\n\ndef run():\n    with File(\"x\").encode() as s:\n        s.read()\n";
        let old = new.replace("return 2", "return 1").replace("self.p = 2", "self.p = 1").replace("    with File(\"x\").encode() as s:\n        s.read()\n", "    return 0\n");
        let files = vec![cross_file::FileChange::new("f.py", &old, new, Language::Python)];
        let result = analyze_multi(&files).unwrap();
        let callers = |name: &str| -> Vec<String> {
            result.cross_file.reading_order.iter().find(|s| s.name == name).map(|s| s.called_by.clone()).unwrap_or_default()
        };
        // `File("x")` runs __init__; `.encode()` on it is File's; `with … as s` makes s what encode returns.
        for f in ["File.__init__", "File.encode", "Stream.read"] {
            assert_eq!(callers(f), vec!["run"], "{f}: {:?}", result.cross_file.reading_order);
        }
        assert!(callers("Other.encode").is_empty() && callers("Other.read").is_empty());
    }

    #[test]
    fn test_functions_handed_on_as_values_in_js() {
        let new = "export function handleClick() {\n  return 2;\n}\n\nexport function handleError() {\n  return 2;\n}\n\nexport function handleSubmit() {\n  return 2;\n}\n\n\
            export function App(props) {\n  const local = 1;\n  render({ onError: handleError, handleSubmit, local });\n  return <button onClick={handleClick} />;\n}\n";
        let old = new.replace("return 2", "return 1").replace("  render({ onError: handleError, handleSubmit, local });\n", "").replace(" onClick={handleClick}", "");
        let files = vec![cross_file::FileChange::new("app.tsx", &old, new, Language::Tsx)];
        let result = analyze_multi(&files).unwrap();
        let callers = |name: &str| -> Vec<String> {
            result.cross_file.reading_order.iter().find(|s| s.name == name).map(|s| s.called_by.clone()).unwrap_or_default()
        };
        // `onClick={handleClick}`, `{ onError: handleError }` and `{ handleSubmit }` all hand a function on.
        for f in ["handleClick", "handleError", "handleSubmit"] {
            assert_eq!(callers(f), vec!["App"], "{f}: {:?}", result.cross_file.reading_order);
        }
    }

    #[test]
    fn test_aliases_resolve_to_their_target() {
        let new = "export type AnyApi = Api<any>;\n\nexport class Api<T> {\n  make() {\n    return 2;\n  }\n}\n\nexport class Other {\n  make() {\n    return 2;\n  }\n}\n\nexport function f(a: AnyApi) {\n  a.make();\n}\n";
        let old = new.replace("return 2", "return 1").replace("  a.make();\n", "");
        let files = vec![cross_file::FileChange::new("api.ts", &old, new, Language::TypeScript)];
        let result = analyze_multi(&files).unwrap();
        let callers = |name: &str| -> Vec<String> {
            result.cross_file.reading_order.iter().find(|s| s.name == name).map(|s| s.called_by.clone()).unwrap_or_default()
        };
        // `a: AnyApi` is an Api through the alias the change defines.
        assert_eq!(callers("Api.make"), vec!["f"], "{:?}", result.cross_file.reading_order);
        assert!(callers("Other.make").is_empty());
    }

    #[test]
    fn test_rust_self_path_is_the_receivers_type() {
        use parser::{CallRef, Qualifier};
        let src = "struct A;\nimpl A {\n    fn run() {\n        Self::step(1);\n    }\n}\n";
        let tree = parser::parse(src, &*languages::get_language_support(Language::Rust)).unwrap();
        let run = tree.items.iter().position(|i| i.name() == Some("A::run")).expect("A::run item");
        let calls = &tree.meta[run].calls;
        assert!(calls.contains(&CallRef { qualifier: Qualifier::SelfType, name: parser::ident_hash("step"), parens: true }), "{calls:?}");
    }

    #[test]
    fn test_python_self_is_the_receivers_type() {
        use parser::{CallRef, Qualifier};
        let src = "class A:\n    def run(self):\n        self.step()\n        cls.make()\n";
        let lang = languages::get_language_support(Language::Python);
        let tree = parser::parse(src, &*lang).unwrap();
        let calls: std::collections::HashSet<CallRef> = tree.meta.iter().flat_map(|m| m.method_calls.iter().flatten().copied()).collect();
        let has = |name: &str| calls.contains(&CallRef { qualifier: Qualifier::SelfType, name: parser::ident_hash(name), parens: true });
        assert!(has("step") && has("make"), "{calls:?}");
    }

    #[test]
    fn test_functions_passed_as_values_are_spotted() {
        use parser::{CallRef, Qualifier};
        let src = "fn f(qs: Vec<Q>, limit: usize) {\n    qs.iter().any(Q::is_and);\n    let (or, and) = (Q::or, Q::and);\n    x.or_else(fallback);\n    take(limit, other);\n    if let Some(found) = y { use_it(found); }\n}\n";
        let lang = languages::get_language_support(Language::Rust);
        let tree = parser::parse(src, &*lang).unwrap();
        let calls = &tree.meta[0].calls;
        let has = |q: Qualifier, name: &str| calls.contains(&CallRef { qualifier: q, name: parser::ident_hash(name), parens: false });
        let q = Qualifier::Path(parser::ident_hash("Q"));
        assert!(has(q, "is_and") && has(q, "or") && has(q, "and"), "{calls:?}");
        assert!(has(Qualifier::None, "fallback"), "a lone argument");
        assert!(has(Qualifier::None, "other"));
        // Parameters and pattern bindings passed along are values, not functions.
        assert!(!has(Qualifier::None, "limit") && !has(Qualifier::None, "found"), "{calls:?}");
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
    fn test_stale_call_sites_are_judged_by_what_they_pass() {
        let stale = |files: Vec<cross_file::FileChange>| -> Vec<(String, usize)> {
            let r = analyze_multi(&files).unwrap();
            r.cross_file.signature_impacts.iter().flat_map(|s| s.call_sites.iter().filter(|c| !c.updated).map(|c| (c.file.clone(), c.line))).collect()
        };
        let ts = |p: &str, o: &str, n: &str| cross_file::FileChange::new(p, o, n, Language::TypeScript);
        // A new required parameter: the caller that wasn't updated is stale, wherever it is.
        let lib = || ts("src/auth.ts", "export function auth(user: string): boolean {\n  return !!user;\n}\n", "export function auth(user: string, pass: string): boolean {\n  return !!user && !!pass;\n}\n");
        let app = "import { auth } from './auth';\nexport const a = auth('x');\n";
        assert_eq!(stale(vec![lib(), ts("src/app.ts", app, &format!("{app}// touched\n"))]), vec![("src/app.ts".into(), 2)]);
        // A new optional parameter breaks nobody.
        let opt = ts("src/auth.ts", "export function auth(user: string): boolean {\n  return !!user;\n}\n", "export function auth(user: string, pass?: string): boolean {\n  return !!user;\n}\n");
        assert!(stale(vec![opt, ts("src/app.ts", app, &format!("{app}// touched\n"))]).is_empty());
        // Swapped parameters: the same two arguments now land on different parameters.
        let swap = ts("src/auth.ts", "export function auth(user: string, pass: string): boolean {\n  return !!user;\n}\n", "export function auth(pass: string, user: string): boolean {\n  return !!user;\n}\n");
        let app2 = "import { auth } from './auth';\nexport const a = auth('x', 'y');\n";
        assert_eq!(stale(vec![swap, ts("src/app.ts", app2, &format!("{app2}// touched\n"))]).len(), 1);
        // A same-named function from another module isn't this one.
        let other = "import { auth } from './other-auth';\nexport const a = auth('x');\n";
        assert!(stale(vec![lib(), ts("src/app.ts", other, &format!("{other}// touched\n"))]).is_empty());
        // A multi-line call whose arguments were updated below its first line is updated.
        let multi_old = "import { auth } from './auth';\nexport const a = auth(\n  'x',\n);\n";
        let multi_new = "import { auth } from './auth';\nexport const a = auth(\n  'x',\n  'p',\n);\n";
        assert!(stale(vec![lib(), ts("src/app.ts", multi_old, multi_new)]).is_empty());
        // Python: a removed parameter still passed by keyword is stale.
        let py = |p: &str, o: &str, n: &str| cross_file::FileChange::new(p, o, n, Language::Python);
        let pylib = py("pkg/net.py", "def fetch(url, timeout=5):\n    return url\n", "def fetch(url):\n    return url\n");
        let caller = "from pkg.net import fetch\n\ndef run():\n    return fetch('u', timeout=1)\n";
        assert_eq!(stale(vec![pylib, py("pkg/run.py", caller, &format!("{caller}# touched\n"))]), vec![("pkg/run.py".into(), 4)]);
        // Rust: a removed parameter, multi-line caller untouched, is stale.
        let rs = |p: &str, o: &str, n: &str| cross_file::FileChange::new(p, o, n, Language::Rust);
        let rlib = rs("src/lib.rs", "pub fn size(n: u32, unit: u32) -> u32 {\n    n * unit\n}\n", "pub fn size(n: u32) -> u32 {\n    n\n}\n");
        let rcall = "fn main() {\n    let s = size(\n        3,\n        8,\n    );\n}\n";
        assert_eq!(stale(vec![rlib, rs("src/main.rs", rcall, &format!("{rcall}// touched\n"))]).len(), 1);
    }

    #[test]
    fn test_stale_references_and_unused_code_skip_the_noise() {
        let py = |p: &str, o: &str, n: &str| cross_file::FileChange::new(p, o, n, Language::Python);
        let lib_old = "def old_name():\n    return 1\n";
        let lib_new = "def new_name():\n    return 1\n\n\ndef __getattr__(name):\n    if name == \"old_name\":\n        return new_name\n";
        let broken = |extra: cross_file::FileChange| {
            let r = analyze_multi(&[py("pkg/api.py", lib_old, lib_new), extra]).unwrap();
            r.cross_file.broken_references.iter().map(|b| (b.reference_file.clone(), b.reference_location.line_start)).collect::<Vec<_>>()
        };
        // The deprecation shim's string isn't a use, a changelog names old things on purpose, a
        // file that defines the name itself is using its own, but a real call and `__all__` are stale.
        let user = "from pkg.api import old_name\n__all__ = [\"old_name\"]\n\ndef run():\n    return old_name()\n";
        let found = broken(py("pkg/use.py", "", user));
        assert!(found.contains(&("pkg/use.py".to_string(), 5)) && found.contains(&("pkg/use.py".to_string(), 2)), "{found:?}");
        assert!(!found.iter().any(|(f, _)| f == "pkg/api.py"), "{found:?}");
        assert!(broken(cross_file::FileChange::new("CHANGES.rst", "", "- ``old_name`` is deprecated.\n", Language::Unknown)).is_empty());
        assert!(broken(py("pkg/own.py", "", "def old_name():\n    return 2\n\n\nx = old_name()\n")).is_empty());
        // `const { a, b } = require(…)` is used if either name is.
        let js = "const { used, unused } = require('./m');\nconsole.log(used);\n";
        let r = analyze_multi(&[cross_file::FileChange::new("src/a.js", "", js, Language::TypeScript)]).unwrap();
        assert!(r.file_results[0].1.manifest.dead_code.is_empty(), "{:?}", r.file_results[0].1.manifest.dead_code);
    }

    #[test]
    fn test_go_capitalized_methods_and_shadowed_names() {
        let go = |p: &str, o: &str, n: &str| cross_file::FileChange::new(p, o, n, Language::Go);
        // `canvas.Compose(c).Render()`: Compose is a method (Go capitalizes exported ones), so the
        // chain is canvas's, and Render is Canvas's.
        let new = "package a\n\ntype Canvas struct{}\n\nfunc (c *Canvas) Compose(x int) *Canvas {\n\treturn c\n}\n\nfunc (c *Canvas) Render() string {\n\treturn \"b\"\n}\n\ntype Other struct{}\n\nfunc (o *Other) Render() string {\n\treturn \"b\"\n}\n\nfunc Draw(canvas *Canvas) string {\n\treturn canvas.Compose(1).Render()\n}\n";
        let old = new.replace("\"b\"", "\"a\"").replace("canvas.Compose(1).Render()", "\"\"");
        let r = analyze_multi(&[go("a.go", &old, new)]).unwrap();
        let callers = |name: &str| -> Vec<String> { r.cross_file.reading_order.iter().find(|s| s.name == name).map(|s| s.called_by.clone()).unwrap_or_default() };
        assert!(callers("Canvas.Render").contains(&"Draw".to_string()), "{:?}", r.cross_file.reading_order);
        assert!(callers("Other.Render").is_empty());
        // A removed lowercase type (`type layers []*Layer`) isn't what a `layers` variable or parameter is.
        let lib_old = "package a\n\ntype layers []int\n\nfunc sortLayers(l layers) {}\n";
        let lib_new = "package a\n\nfunc sortLayers(l []int) {}\n";
        let user = "package a\n\nfunc Add(layers ...int) int {\n\treturn len(layers)\n}\n";
        let r = analyze_multi(&[go("lib.go", lib_old, lib_new), go("user.go", "", user)]).unwrap();
        assert!(r.cross_file.broken_references.is_empty(), "{:?}", r.cross_file.broken_references);
    }

    #[test]
    fn test_warnings_from_fresh_prs() {
        let stale_calls = |r: &cross_file::MultiDiffResult| r.cross_file.signature_impacts.iter().flat_map(|s| s.call_sites.iter().filter(|c| !c.updated)).count();
        // Rust: `*x` is a dereference, not a spread; a renamed parameter of the same type breaks nobody.
        let rs = |p: &str, o: &str, n: &str| cross_file::FileChange::new(p, o, n, Language::Rust);
        let old = "fn bump(sections: &mut [u32], by: u32) {}\n\nfn run(v: &mut Vec<u32>, n: &u32) {\n    bump(v, *n);\n}\n";
        let new = old.replace("bump(sections: &mut [u32]", "bump(items: &mut [u32]");
        assert_eq!(stale_calls(&analyze_multi(&[rs("src/lib.rs", old, &new)]).unwrap()), 0);
        // Go: a type renamed inside a package of the same name; `stacktrace.Take` is the package,
        // and a parameter whose type is the renamed one still fits.
        let go = |p: &str, o: &str, n: &str| cross_file::FileChange::new(p, o, n, Language::Go);
        let pkg_old = "package stacktrace\n\ntype stacktrace struct{}\n\nfunc Take() string { return \"\" }\n\nfunc Format(s *stacktrace) {}\n";
        let pkg_new = "package stacktrace\n\ntype Stack struct{}\n\nfunc Take() string { return \"\" }\n\nfunc Format(s *Stack) {}\n";
        let user = "package zap\n\nimport \"go.uber.org/zap/internal/stacktrace\"\n\nfunc Log() string {\n\treturn stacktrace.Take()\n}\n";
        let r = analyze_multi(&[go("internal/stacktrace/stack.go", pkg_old, pkg_new), go("logger.go", "", user)]).unwrap();
        assert!(r.cross_file.broken_references.is_empty(), "{:?}", r.cross_file.broken_references);
        assert_eq!(stale_calls(&r), 0);
        // Python: a renamed free function `split` isn't what `line.split(",")` calls, but bare and
        // module-qualified uses still are.
        let py = |p: &str, o: &str, n: &str| cross_file::FileChange::new(p, o, n, Language::Python);
        let util_old = "def split(xs, n):\n    return xs\n";
        let util_new = "def split_iterable(xs, n):\n    return xs\n";
        let user = "from pkg import utils\nfrom pkg.utils import split\n\ndef run(line):\n    a = line.split(',')\n    b = split(a, 2)\n    return utils.split(b, 2)\n";
        let r = analyze_multi(&[py("pkg/utils.py", util_old, util_new), py("pkg/run.py", "", user)]).unwrap();
        let lines: Vec<usize> = r.cross_file.broken_references.iter().filter(|b| b.reference_file == "pkg/run.py").map(|b| b.reference_location.line_start).collect();
        assert!(!lines.contains(&5) && lines.contains(&6) && lines.contains(&7), "{lines:?}");
    }

    #[test]
    fn test_methods_of_renamed_types() {
        let go = |p: &str, o: &str, n: &str| cross_file::FileChange::new(p, o, n, Language::Go);
        let stale = |files: &[cross_file::FileChange]| analyze_multi(files).unwrap().cross_file.signature_impacts.iter()
            .flat_map(|s| s.call_sites.iter().filter(|c| !c.updated)).count();
        let user = "package a\n\nfunc Use(s *SSESource, c *Client) {\n\ts.SetTLS(1)\n\tc.SetTLS(2)\n}\n";
        // `EventSource` renamed to `SSESource`; SetTLS only changes its return type: nothing to update.
        let old = "package a\n\ntype EventSource struct{}\n\nfunc (e *EventSource) SetTLS(n int) *EventSource {\n\treturn e\n}\n";
        let new = "package a\n\ntype SSESource struct{}\n\nfunc (e *SSESource) SetTLS(n int) *SSESource {\n\treturn e\n}\n";
        assert_eq!(stale(&[go("sse.go", old, new), go("use.go", user, &format!("{user}// touched\n"))]), 0);
        // The same rename with a new required parameter: the untouched call is stale.
        let new2 = "package a\n\ntype SSESource struct{}\n\nfunc (e *SSESource) SetTLS(n int, strict bool) *SSESource {\n\treturn e\n}\n";
        assert!(stale(&[go("sse.go", old, new2), go("use.go", user, &format!("{user}// touched\n"))]) >= 1);
    }

    #[test]
    fn test_decorated_python_methods_are_tracked() {
        let old = "class Bool:\n    @staticmethod\n    def str_to_bool(value):\n        return bool(value)\n\n    def convert(self, value):\n        return self.str_to_bool(value)\n";
        let new = old.replace("def str_to_bool(value):", "def str_to_bool(value, strict):");
        let r = analyze_multi(&[cross_file::FileChange::new("types.py", old, &new, Language::Python)]).unwrap();
        let stale: Vec<usize> = r.cross_file.signature_impacts.iter().flat_map(|s| s.call_sites.iter().filter(|c| !c.updated).map(|c| c.line)).collect();
        assert_eq!(stale, vec![7], "{:?}", r.cross_file.signature_impacts);
    }

    #[test]
    fn test_method_call_sites_by_name_and_file() {
        let rs = |p: &str, o: &str, n: &str| cross_file::FileChange::new(p, o, n, Language::Rust);
        let stale = |files: &[cross_file::FileChange]| -> Vec<(String, usize)> {
            analyze_multi(files).unwrap().cross_file.signature_impacts.iter()
                .flat_map(|s| s.call_sites.iter().filter(|c| !c.updated).map(|c| (c.file.clone(), c.line))).collect()
        };
        // `Mode::deduce` gains a parameter. `Self::deduce(` in another impl's file is that type's;
        // a common name on some value elsewhere (`b.usage(`) is another type's; a distinctive one isn't.
        let mode_old = "pub struct Mode;\nimpl Mode {\n    pub fn deduce(a: u32) -> u32 { a }\n    pub fn usage(&self) {}\n    pub fn render_summary(&self) {}\n}\n";
        let mode_new = "pub struct Mode;\nimpl Mode {\n    pub fn deduce(a: u32, tty: bool) -> u32 { a }\n    pub fn usage(&self, w: u32) {}\n    pub fn render_summary(&self, w: u32) {}\n}\n";
        let other = "pub struct View;\nimpl View {\n    pub fn deduce(a: u32) -> u32 { Self::deduce(a) }\n    pub fn show(m: &Mode, b: &Builder) {\n        b.usage();\n        m.render_summary();\n    }\n}\n";
        let found = stale(&[rs("src/mode.rs", mode_old, mode_new), rs("src/view.rs", other, &format!("{other}// touched\n"))]);
        assert_eq!(found, vec![("src/view.rs".to_string(), 6)], "{found:?}");
    }

    #[test]
    fn test_catch_all_params_and_imports_from_elsewhere() {
        let py = |p: &str, o: &str, n: &str| cross_file::FileChange::new(p, o, n, Language::Python);
        // An optional parameter added before `**kwargs`: nobody breaks, and `int.__new__(` isn't ours.
        let old = "class DateTime(int):\n    def __new__(cls, year, tz=None, **kwargs):\n        return int.__new__(cls, year)\n\n\ndef make():\n    return DateTime(1, tz=None)\n";
        let new = old.replace("tz=None, **kwargs", "tz=None, fold=0, **kwargs");
        let r = analyze_multi(&[py("items.py", old, &new)]).unwrap();
        assert!(r.cross_file.signature_impacts.iter().all(|s| s.call_sites.iter().all(|c| c.updated)), "{:?}", r.cross_file.signature_impacts);
        // A removed name imported from another package is that package's; from its own module it's stale.
        let ts = |p: &str, o: &str, n: &str| cross_file::FileChange::new(p, o, n, Language::TypeScript);
        let lib_old = "export const topSites = [1];\nexport const other = 2;\n";
        let lib_new = "export const other = 2;\n";
        let outside = "import topSites from 'top-sites';\nexport const n = topSites.length;\n";
        let inside = "import { topSites } from './sites';\nexport const n = topSites.length;\n";
        let refs = |user: &str| analyze_multi(&[ts("src/sites.ts", lib_old, lib_new), ts("src/use.ts", "", user)]).unwrap().cross_file.broken_references.len();
        assert_eq!(refs(outside), 0);
        assert!(refs(inside) >= 1);
    }

    /// (text, noise) of every added line in one file's hunks.
    fn added_noise(r: &DiffResult) -> Vec<(String, Option<manifest::Noise>)> {
        r.hunks.iter().flat_map(|h| h.changes.iter())
            .filter(|c| matches!(c.kind, ChangeKind::Added | ChangeKind::Modified))
            .map(|c| (c.content_new.clone().unwrap_or_default().trim().to_string(), c.noise))
            .collect()
    }

    fn logic(r: &DiffResult) -> Vec<(String, String)> {
        r.manifest.logic_changes.iter().map(|l| (l.name.clone(), l.description.clone())).collect()
    }

    #[test]
    fn test_operator_lines_are_code_not_comments() {
        // rustfmt puts the operator at the start of continuation lines, so `* qty` is code.
        let old = "pub fn total(price: u32, qty: u32, extra: u32) -> u32 {\n    /* Price\n     * times quantity. */\n    price\n        * qty\n        + extra\n}\n";
        let new = "pub fn total(price: u32, qty: u32, extra: u32) -> u32 {\n    /* Price\n     * times the quantity. */\n    price\n        * extra\n        + qty\n}\n";
        let r = analyze_multi(&[cross_file::FileChange::new("a.rs", old, new, Language::Rust)]).unwrap();
        let added = added_noise(&r.file_results[0].1);
        assert!(added.contains(&("* times the quantity. */".into(), Some(manifest::Noise::Comment))), "{added:?}");
        assert!(added.contains(&("* extra".into(), None)), "{added:?}");
        assert!(added.contains(&("+ qty".into(), None)), "{added:?}");
    }

    #[test]
    fn test_reordered_statements_stay_visible() {
        let old = "pub fn run(x: u32) {\n    validate(x);\n    check(x);\n    lock(x);\n    save(x);\n    log(x);\n    notify(x);\n}\n";
        let new = "pub fn run(x: u32) {\n    save(x);\n    log(x);\n    notify(x);\n    validate(x);\n    check(x);\n    lock(x);\n}\n";
        let r = analyze_multi(&[cross_file::FileChange::new("a.rs", old, new, Language::Rust)]).unwrap();
        let r = &r.file_results[0].1;
        assert_eq!(r.review.mechanical_lines, 0, "{:?}", r.hunks);
        assert_eq!(logic(r), vec![("run".into(), "body modified".into())]);
    }

    #[test]
    fn test_edit_in_moved_code_stays_visible() {
        // Function moved to the end of the file and `return False` became `return True`,
        // which also shows up somewhere else in the old version.
        let f = "def is_async(call):\n    if inspect.isclass(call):\n        return False\n    if inspect.iscoroutinefunction(call):\n        return True\n    partial = getattr(call, 'func', None)\n    return inspect.iscoroutinefunction(partial)\n";
        let other = "def wrap(call):\n    async def inner(*args):\n        return call(*args)\n    return inner\n";
        let old = format!("{f}\n\n{other}");
        let new = format!("{other}\n\n{}", f.replace("return False", "return True"));
        let r = analyze_multi(&[cross_file::FileChange::new("u.py", &old, &new, Language::Python)]).unwrap();
        let r = &r.file_results[0].1;
        let added = added_noise(r);
        assert_eq!(added.iter().filter(|(t, n)| t == "return True" && n.is_none()).count(), 1, "{added:?}");
        assert!(logic(r).iter().any(|(n, _)| n == "is_async"), "{:?}", logic(r));
    }

    #[test]
    fn test_formatting_pairs_follow_line_order() {
        // Reformatted, and `javadoc = false` changed to `true`. The changed line shouldn't get
        // paired with the other `javadoc = true` that didn't change.
        let old = "class W {\n  void emit() {\n    javadoc = true;\n    try {\n      write(block, true);\n    } finally {\n      javadoc = false;\n    }\n  }\n}\n";
        let new = "class W {\n  void emit() {\n      javadoc = true;\n      try {\n        write(block,true);\n      } finally {\n        javadoc = true;\n      }\n  }\n}\n";
        let r = analyze_multi(&[cross_file::FileChange::new("W.java", old, new, Language::Java)]).unwrap();
        let r = &r.file_results[0].1;
        let visible: Vec<usize> = r.hunks.iter().flat_map(|h| h.changes.iter())
            .filter(|c| c.kind == ChangeKind::Added && c.noise.is_none())
            .filter_map(|c| c.new_span.as_ref().map(|s| s.start_line))
            .collect();
        assert_eq!(visible, vec![7], "{:?}", r.hunks);
    }

    #[test]
    fn test_reformat_is_mechanical_across_the_function() {
        let old = "func Allow(enc ...string) func(http.Handler) http.Handler {\n\tallowed := make(map[string]struct{}, len(enc))\n\treturn func(next http.Handler) http.Handler {\n\t\tfn := func(w http.ResponseWriter, r *http.Request) {\n\t\t\tif r.ContentLength == 0 {\n\t\t\t\tnext.ServeHTTP(w, r)\n\t\t\t\treturn\n\t\t\t}\n\t\t\tfor _, e := range r.Header[\"X\"] {\n\t\t\t\tif _, ok := allowed[e]; !ok {\n\t\t\t\t\tw.WriteHeader(415)\n\t\t\t\t\treturn\n\t\t\t\t}\n\t\t\t}\n\t\t\tnext.ServeHTTP(w, r)\n\t\t}\n\t\treturn http.HandlerFunc(fn)\n\t}\n}\n";
        let new: String = old.lines().enumerate()
            .map(|(i, l)| if i == 0 || l == "}" { l.to_string() } else { format!("\t{}", l.replace(", ", ",")) })
            .collect::<Vec<_>>().join("\n") + "\n";
        let r = analyze_multi(&[cross_file::FileChange::new("a.go", old, &new, Language::Go)]).unwrap();
        let r = &r.file_results[0].1;
        assert_eq!(r.review.mechanical_lines, r.review.changed_lines, "{:?}", r.hunks);
    }

    #[test]
    fn test_class_changes_outside_methods_are_reported() {
        let old = "final class A {\n  void top() {\n    run(1);\n  }\n\n  private static final class Counts<T> {\n    void drop(T t) {\n      put(t, count - 1);\n    }\n  }\n}\n";
        let new = old.replace("run(1);", "run(2);").replace("count - 1", "count - 2");
        let r = analyze(old, &new, Language::Java).unwrap();
        let l: Vec<(String, String)> = r.manifest.logic_changes.iter().map(|l| (l.name.clone(), l.description.clone())).collect();
        assert!(l.contains(&("A".into(), "changed outside its methods".into())), "{l:?}");
        assert!(l.contains(&("A.top".into(), "body modified".into())), "{l:?}");
    }

    #[test]
    fn test_reordered_methods_are_mechanical() {
        let a = "    def first(self):\n        return self.x + 1\n";
        let b = "    def second(self):\n        return self.y * 2\n";
        let old = format!("class C:\n{a}\n{b}");
        let new = format!("class C:\n{b}\n{a}");
        let r = analyze_multi(&[cross_file::FileChange::new("c.py", &old, &new, Language::Python)]).unwrap();
        let r = &r.file_results[0].1;
        assert!(r.manifest.logic_changes.is_empty(), "{:?}", r.manifest.logic_changes);
        assert_eq!(r.manifest.formatting_only.len(), 1, "{:?}", r.manifest);
        assert_eq!(r.review.mechanical_lines, r.review.changed_lines, "{:?}", r.hunks);
    }

    #[test]
    fn test_whitespace_inside_strings_is_content() {
        for (path, old, new, lang) in [
            ("a.ts", "export function q(): string {\n  return `select a,\n    b from t`;\n}\n", "export function q(): string {\n  return `select a,\n      b from t`;\n}\n", Language::TypeScript),
            ("a.ts", "export function q(name: string): string {\n  return `hello ${name}`;\n}\n", "export function q(name: string): string {\n  return `hello  ${name}`;\n}\n", Language::TypeScript),
            ("A.scala", "object A {\n  def q(n: String): String = {\n    s\"hello $n\"\n  }\n}\n", "object A {\n  def q(n: String): String = {\n    s\"hello  $n\"\n  }\n}\n", Language::Scala),
            ("a.ts", "export function words(s: string): string[] {\n  return s.split(/ +/);\n}\n", "export function words(s: string): string[] {\n  return s.split(/  +/);\n}\n", Language::TypeScript),
            ("m.c", "#define SQUARE(x) ((x) * (x))\n\nint f(int y) {\n  return SQUARE(y);\n}\n", "#define SQUARE (x) ((x) * (x))\n\nint f(int y) {\n  return SQUARE(y);\n}\n", Language::C),
        ] {
            let r = analyze_multi(&[cross_file::FileChange::new(path, old, new, lang)]).unwrap();
            let r = &r.file_results[0].1;
            let dimmed: Vec<_> = r.hunks.iter().flat_map(|h| h.changes.iter())
                .filter(|c| c.noise.is_some() && c.content_new.as_deref().or(c.content_old.as_deref()).is_some_and(|t| !t.trim().is_empty()))
                .collect();
            assert!(dimmed.is_empty(), "{new}: {dimmed:?}");
            assert!(!r.manifest.logic_changes.is_empty() && r.manifest.formatting_only.is_empty(), "{new}: {:?}", r.manifest);
        }
    }

    #[test]
    fn test_directives_decorators_and_default_exports_are_code() {
        for (path, old, new, lang) in [
            ("a.go", "package a\n\nfunc f(n int) int {\n\treturn n + 1\n}\n", "package a\n\n//go:noinline\nfunc f(n int) int {\n\treturn n + 1\n}\n", Language::Go),
            ("a.py", "def f(n):\n    return n + 1\n", "@functools.cache\ndef f(n):\n    return n + 1\n", Language::Python),
            ("a.ts", "export default define({\n  build(x) {\n    one(x);\n    two(x);\n  },\n});\n", "export default define({\n  build(x) {\n    two(x);\n    one(x);\n  },\n});\n", Language::TypeScript),
        ] {
            let r = analyze_multi(&[cross_file::FileChange::new(path, old, new, lang)]).unwrap();
            let r = &r.file_results[0].1;
            assert_eq!(r.review.mechanical_lines, 0, "{new}: {:?}", r.hunks);
            assert_eq!(r.manifest.logic_changes.len(), 1, "{new}: {:?}", r.manifest);
        }
    }

    #[test]
    fn test_linter_switches_show_but_are_not_logic() {
        // `// @ts-ignore` stays visible, but it's not a logic change.
        let old = "export function f(n: number): number {\n  return g(n);\n}\n";
        let new = "export function f(n: number): number {\n  // @ts-ignore\n  return g(n);\n}\n";
        let r = analyze_multi(&[cross_file::FileChange::new("a.ts", old, new, Language::TypeScript)]).unwrap();
        let r = &r.file_results[0].1;
        assert_eq!(r.review.mechanical_lines, 0, "{:?}", r.hunks);
        assert!(r.manifest.logic_changes.is_empty(), "{:?}", r.manifest.logic_changes);
        // A comment between the parts of a concatenated string isn't part of the string.
        let old = "def f(v):\n    return (\n        f\"a {v} \"  # type: ignore[x]\n        f\"b\"\n    )\n";
        let new = old.replace("  # type: ignore[x]", "");
        let r = analyze(old, &new, Language::Python).unwrap();
        assert!(r.manifest.logic_changes.is_empty(), "{:?}", r.manifest.logic_changes);
        // A trailing directive belongs to its own line, not the statement after it.
        let old = "from .a import b  # noqa: E402\n\nif t.TYPE_CHECKING:\n    import c\n";
        let new = "from .a import b  # noqa: E402\n\nif t.TYPE_CHECKING:\n    import d\n";
        let r = analyze(old, new, Language::Python).unwrap();
        assert_eq!(r.manifest.logic_changes.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), vec!["if t.TYPE_CHECKING:"]);
    }

    #[test]
    fn test_destructuring_changes_are_not_renames() {
        let old = "const { kConnecting, kUrl } = require('./symbols')\n\nexport function f(p) {\n  return p[kUrl]\n}\n";
        let new = "const { kConnecting, kUrl, kQueued } = require('./symbols')\n\nexport function f(p) {\n  return p[kUrl]\n}\n";
        let r = analyze_multi(&[cross_file::FileChange::new("pool.js", old, new, Language::TypeScript)]).unwrap();
        let r = &r.file_results[0].1;
        assert_eq!(r.review.mechanical_lines, 0, "{:?}", r.hunks);
        assert_eq!(r.manifest.logic_changes.len(), 1, "{:?}", r.manifest);
    }

    #[test]
    fn test_callers_only_following_a_rename_are_mechanical() {
        let files = [
            cross_file::FileChange::new("src/fmt.rs", "pub fn write_usage(out: &mut String, prog: &str) {\n    out.push_str(prog);\n    out.push('\\n');\n}\n", "pub fn print_usage(out: &mut String, prog: &str) {\n    out.push_str(prog);\n    out.push('\\n');\n}\n", Language::Rust),
            cross_file::FileChange::new("src/cli.rs", "pub fn help(out: &mut String) {\n    let name = program();\n    write_usage(out, &name);\n}\n", "pub fn help(out: &mut String) {\n    let name = program();\n    print_usage(out, &name);\n}\n", Language::Rust),
        ];
        let r = analyze_multi(&files).unwrap();
        let cli = &r.file_results[1].1;
        assert!(cli.manifest.logic_changes.is_empty(), "{:?}", cli.manifest.logic_changes);
        assert_eq!(cli.manifest.formatting_only.len(), 1, "{:?}", cli.manifest);
        assert_eq!(cli.review.mechanical_lines, cli.review.changed_lines);
        // A caller that also changed something else stays a logic change.
        let mut files = files;
        files[1].new_source = files[1].new_source.replace("program()", "program_name()");
        let r = analyze_multi(&files).unwrap();
        assert_eq!(logic(&r.file_results[1].1), vec![("help".into(), "body modified".into())]);
    }

    #[test]
    fn test_c_pointer_params_are_not_catch_alls() {
        // `*item` is a pointer in C, not Python's `*args`: dropping a parameter still breaks callers.
        let old = "static int parse(char *item, int *buffer) {\n    return 0;\n}\n\nint run(char *s, int *b) {\n    return parse(s, b);\n}\n";
        let new = old.replace("static int parse(char *item, int *buffer)", "static int parse(char *item)");
        let r = analyze_multi(&[cross_file::FileChange::new("lib.c", old, &new, Language::C)]).unwrap();
        let stale: Vec<usize> = r.cross_file.signature_impacts.iter().flat_map(|s| s.call_sites.iter().filter(|c| !c.updated).map(|c| c.line)).collect();
        assert_eq!(stale, vec![6], "{:?}", r.cross_file.signature_impacts);
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
    fn test_whitespace_that_renders_stays_visible_in_unparsed_files() {
        // Indenting makes a Markdown code block, two trailing spaces are a line break, and Makefiles need tabs.
        for (path, old, new) in [
            ("README.md", "run this:\n\nnpm install\n", "run this:\n\n    npm install\n"),
            ("README.md", "Line one\nLine two\n", "Line one  \nLine two\n"),
            ("Makefile", "build:\n\tcargo build\n", "build:\n    cargo build\n"),
        ] {
            let r = analyze_multi(&[cross_file::FileChange::new(path, old, new, Language::Unknown)]).unwrap();
            assert_eq!(r.file_results[0].1.review.mechanical_lines, 0, "{new:?}: {:?}", r.file_results[0].1.hunks);
        }
        // Spacing within a line is still formatting.
        let r = analyze_multi(&[cross_file::FileChange::new("Cargo.toml", "a = { version=\"1\" }\n", "a = { version = \"1\" }\n", Language::Unknown)]).unwrap();
        assert_eq!(r.file_results[0].1.review.mechanical_lines, 2, "{:?}", r.file_results[0].1.hunks);
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

