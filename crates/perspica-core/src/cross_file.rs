use crate::classify::find_identifier;
use crate::diff::dice;
use crate::manifest::{ChangeKind, DiffHunk, Location, ManifestEntryId, Side, SymbolKind};
use crate::parser::{bare_name, ident_hash, SemanticItem, SemanticTree};
use crate::{DiffResult, Language};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CrossFileManifest {
    pub moves: Vec<CrossFileMoveEntry>,
    pub broken_references: Vec<BrokenReferenceEntry>,
    /// Call sites of functions whose signature changed.
    #[serde(default)]
    pub signature_impacts: Vec<SignatureImpactEntry>,
    /// Names that no longer exist anywhere in the changed files (renamed or removed).
    /// Callers can search the rest of the repository for stale references.
    #[serde(default)]
    pub vanished: Vec<VanishedSymbol>,
    /// Changed code in the order to read it: entry points first, then what they call.
    #[serde(default)]
    pub reading_order: Vec<crate::flow::ReadingStep>,
    /// For each changed function, whether (and how) the changed tests reach it.
    #[serde(default)]
    pub test_reach: Vec<crate::flow::TestReach>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VanishedSymbol {
    pub name: String,
    pub renamed_to: Option<String>,
    /// File where it was defined.
    pub origin: String,
    /// For a method, the type it belonged to: only `Owner.name` / `self.name` mentions can refer to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// A name that no longer exists: (name, what it became, file it was defined in, owner type for methods).
pub type Vanished = (String, Option<String>, String, Option<String>);

#[derive(Debug, Serialize, Deserialize)]
pub struct CrossFileMoveEntry {
    pub id: ManifestEntryId,
    pub name: String,
    pub kind: SymbolKind,
    pub from_file: String,
    pub from_location: Location,
    pub to_file: String,
    pub to_location: Location,
    /// Name at the destination, when the move also renamed the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_to: Option<String>,
    /// The body was edited as part of the move.
    #[serde(default)]
    pub modified: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BrokenReferenceEntry {
    pub id: ManifestEntryId,
    /// The name that no longer exists.
    pub symbol_name: String,
    /// What it was renamed to, if it was renamed (None = removed).
    pub renamed_from: Option<String>,
    pub reference_file: String,
    pub reference_location: Location,
    pub reason: String,
    /// The referencing line.
    #[serde(default)]
    pub line_text: String,
    /// Whether the referencing file is part of this diff.
    #[serde(default = "yes")]
    pub in_diff: bool,
}

fn yes() -> bool { true }

#[derive(Debug, Serialize, Deserialize)]
pub struct SignatureImpactEntry {
    pub id: ManifestEntryId,
    pub name: String,
    pub description: String,
    pub definition: Location,
    pub call_sites: Vec<CallSite>,
    /// Visible outside its file (`pub`, `export` …). Private functions can only
    /// be called from their own file, so call sites elsewhere are other functions.
    #[serde(default = "yes")]
    pub exported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallSite {
    pub file: String,
    pub line: usize,
    pub text: String,
    /// The call site line was itself changed in this diff.
    pub updated: bool,
    /// The file is part of this diff.
    #[serde(default = "yes")]
    pub in_diff: bool,
}

/// Input for multi-file analysis.
pub struct FileChange {
    pub old_path: String,
    pub new_path: String,
    pub old_source: String,
    pub new_source: String,
    pub language: Language,
    /// Hunks to display instead of the AST-derived ones (e.g. from `git diff`).
    /// Classification still comes from the AST; hunks are linked to it afterwards.
    pub display_hunks: Option<Vec<DiffHunk>>,
    /// Role from outside the source (e.g. `.gitattributes`); detected from the path when `None`.
    pub role: Option<crate::roles::FileRole>,
}

impl FileChange {
    pub fn new(path: impl Into<String>, old_source: impl Into<String>, new_source: impl Into<String>, language: Language) -> Self {
        let path = path.into();
        FileChange {
            old_path: path.clone(),
            new_path: path,
            old_source: old_source.into(),
            new_source: new_source.into(),
            language,
            display_hunks: None,
            role: None,
        }
    }
}

/// Result of multi-file analysis.
#[derive(Debug, Serialize, Deserialize)]
pub struct MultiDiffResult {
    pub file_results: Vec<(String, DiffResult)>,
    pub cross_file: CrossFileManifest,
}

/// Internal analysis result that preserves the semantic trees and diff output
/// for cross-file passes.
pub(crate) struct InternalAnalysis {
    pub path: String,
    pub result: DiffResult,
    pub old_tree: SemanticTree,
    pub new_tree: SemanticTree,
    pub diff_output: crate::diff::DiffOutput,
    pub new_source: String,
}

/// Similarity above which a same-named item removed in one file and added in
/// another counts as "moved and modified".
const MODIFIED_MOVE_THRESHOLD: f64 = 0.6;

/// Detect cross-file moves: items deleted in one file and added in another.
/// Matching entries' plain "removed"/"added" logic entries are dropped.
pub(crate) fn detect_cross_file_moves(
    analyses: &mut [InternalAnalysis],
    next_id: &mut ManifestEntryId,
) -> Vec<CrossFileMoveEntry> {
    struct Cand { file: usize, idx: usize, name: String, disc: std::mem::Discriminant<SemanticItem>, shape: u64, norm: u64 }
    let mut dels = Vec::new();
    let mut adds = Vec::new();
    for (fi, a) in analyses.iter().enumerate() {
        for &i in &a.diff_output.removed {
            let item = &a.old_tree.items[i];
            if let (Some(name), Some(m)) = (item.name(), a.old_tree.meta.get(i)) {
                if matches!(item, SemanticItem::Import { .. }) { continue; }
                dels.push(Cand { file: fi, idx: i, name: name.to_string(), disc: std::mem::discriminant(item), shape: m.shape_hash, norm: m.norm_hash });
            }
        }
        for &i in &a.diff_output.added {
            let item = &a.new_tree.items[i];
            if let (Some(name), Some(m)) = (item.name(), a.new_tree.meta.get(i)) {
                if matches!(item, SemanticItem::Import { .. }) { continue; }
                adds.push(Cand { file: fi, idx: i, name: name.to_string(), disc: std::mem::discriminant(item), shape: m.shape_hash, norm: m.norm_hash });
            }
        }
    }
    if dels.is_empty() || adds.is_empty() {
        return vec![];
    }

    let mut used = vec![false; adds.len()];
    let mut pairs: Vec<(usize, usize, bool)> = Vec::new(); // (del, add, modified)
    // Exact moves (possibly renamed), then same-name moves with edits.
    for (di, d) in dels.iter().enumerate() {
        let hit = adds.iter().enumerate().position(|(ai, a)| {
            !used[ai] && a.file != d.file && a.disc == d.disc && (a.norm == d.norm || a.shape == d.shape)
        });
        if let Some(ai) = hit {
            used[ai] = true;
            pairs.push((di, ai, false));
        }
    }
    let paired: HashSet<usize> = pairs.iter().map(|p| p.0).collect();
    for (di, d) in dels.iter().enumerate() {
        if paired.contains(&di) { continue; }
        let old_tokens = &analyses[d.file].old_tree.meta[d.idx].tokens;
        let hit = adds.iter().enumerate().position(|(ai, a)| {
            !used[ai] && a.file != d.file && a.disc == d.disc && bare_name(&a.name) == bare_name(&d.name)
                && dice(old_tokens, &analyses[a.file].new_tree.meta[a.idx].tokens) >= MODIFIED_MOVE_THRESHOLD
        });
        if let Some(ai) = hit {
            used[ai] = true;
            pairs.push((di, ai, true));
        }
    }

    let mut moves = Vec::new();
    for (di, ai, modified) in pairs {
        let (d, a) = (&dels[di], &adds[ai]);
        let old_item = &analyses[d.file].old_tree.items[d.idx];
        let new_item = &analyses[a.file].new_tree.items[a.idx];
        let (from_file, to_file) = (analyses[d.file].path.clone(), analyses[a.file].path.clone());
        let from_location = Location { file: Some(from_file.clone()), line_start: old_item.span().start_line, line_end: old_item.span().end_line, side: Side::Old };
        let to_location = Location { file: Some(to_file.clone()), line_start: new_item.span().start_line, line_end: new_item.span().end_line, side: Side::New };
        drop_plain_entry(&mut analyses[d.file].result, &from_location);
        drop_plain_entry(&mut analyses[a.file].result, &to_location);
        moves.push(CrossFileMoveEntry {
            id: { let v = *next_id; *next_id += 1; v },
            name: d.name.clone(),
            kind: kind_of(old_item),
            from_file,
            from_location,
            to_file,
            to_location,
            renamed_to: (a.name != d.name).then(|| a.name.clone()),
            modified,
        });
    }
    moves
}

/// Remove the "added"/"removed" logic and dead-code entries that a cross-file move explains.
fn drop_plain_entry(result: &mut DiffResult, loc: &Location) {
    let m = &mut result.manifest;
    m.logic_changes.retain(|l| !(l.location.side == loc.side && l.location.line_start == loc.line_start
        && (l.description == "added" || l.description == "removed")));
    m.dead_code.retain(|d| !(loc.side == Side::New && d.location.line_start == loc.line_start));
}

/// Names that existed before but are no longer defined anywhere in the changed files,
/// with what they became (Some(new) = renamed, None = removed).
pub(crate) fn vanished_names(analyses: &[InternalAnalysis], moves: &[CrossFileMoveEntry]) -> Vec<Vanished> {
    let defined: HashSet<&str> = analyses.iter()
        .flat_map(|a| a.new_tree.items.iter())
        .flat_map(|item| {
            let mut names: Vec<&str> = item.name().map(bare_name).into_iter().collect();
            if let SemanticItem::Class { methods, .. } = item {
                names.extend(methods.iter().filter_map(|m| m.name()));
            }
            names
        })
        .collect();
    let moved: HashSet<&str> = moves.iter().filter(|m| m.renamed_to.is_none()).map(|m| bare_name(&m.name)).collect();
    let mut out: Vec<Vanished> = Vec::new();
    let mut seen = HashSet::new();
    let owner = |name: &str| owner_of(name).map(str::to_string);
    for a in analyses {
        for r in &a.result.manifest.renames {
            let (old, new) = (bare_name(&r.old_name), bare_name(&r.new_name));
            if !defined.contains(old) && seen.insert(old.to_string()) {
                out.push((old.to_string(), Some(new.to_string()), a.path.clone(), owner(&r.old_name)));
            }
        }
        for m in moves.iter().filter(|m| m.renamed_to.is_some()) {
            let old = bare_name(&m.name);
            if !defined.contains(old) && seen.insert(old.to_string()) {
                out.push((old.to_string(), m.renamed_to.clone(), m.from_file.clone(), owner(&m.name)));
            }
        }
        for l in &a.result.manifest.logic_changes {
            if l.description != "removed" || l.location.side != Side::Old { continue; }
            if !matches!(l.kind, SymbolKind::Function | SymbolKind::Class | SymbolKind::Type | SymbolKind::Variable) { continue; }
            let name = bare_name(&l.name);
            if name.len() < 3 || defined.contains(name) || moved.contains(name) { continue; }
            if seen.insert(name.to_string()) {
                out.push((name.to_string(), None, a.path.clone(), owner(&l.name)));
            }
        }
    }
    out
}

/// Stale references to vanished names in the new versions of the changed files.
pub(crate) fn detect_broken_references(
    analyses: &[InternalAnalysis],
    moves: &[CrossFileMoveEntry],
    next_id: &mut ManifestEntryId,
) -> Vec<BrokenReferenceEntry> {
    let mut broken = Vec::new();
    for (name, renamed_to, origin, owner) in vanished_names(analyses, moves) {
        for a in analyses {
            for (line, text) in scan_references(&a.new_source, &name, owner.as_deref()).into_iter().take(5) {
                broken.push(BrokenReferenceEntry {
                    id: { let v = *next_id; *next_id += 1; v },
                    symbol_name: name.clone(),
                    renamed_from: renamed_to.clone(),
                    reference_file: a.path.clone(),
                    reference_location: Location { file: Some(a.path.clone()), line_start: line, line_end: line, side: Side::New },
                    reason: broken_reason(&name, renamed_to.as_deref(), &origin),
                    line_text: text,
                    in_diff: true,
                });
            }
        }
    }
    broken
}

pub fn broken_reason(name: &str, renamed_to: Option<&str>, origin: &str) -> String {
    match renamed_to {
        Some(new) => format!("still uses `{name}`, renamed to `{new}` in {origin}"),
        None => format!("still uses `{name}`, removed from {origin}"),
    }
}

/// Call sites of every function whose signature changed, marking which ones the diff touched.
pub(crate) fn detect_signature_impacts(
    analyses: &[InternalAnalysis],
    next_id: &mut ManifestEntryId,
) -> Vec<SignatureImpactEntry> {
    let mut impacts = Vec::new();
    for a in analyses {
        for sig in &a.result.manifest.signature_changes {
            // Only changes that can break an existing call (e.g. not a new optional param).
            let params = |tree: &SemanticTree| find_function(tree, &sig.name).map(|p| p.to_vec());
            if let (Some(old), Some(new)) = (params(&a.old_tree), params(&a.new_tree)) {
                if !crate::classify::breaks_callers(&old, &new) {
                    continue;
                }
            }
            let name = bare_name(&sig.name).to_string();
            let exported = a.new_tree.items.iter().zip(&a.new_tree.meta)
                .find(|(i, _)| i.name() == Some(sig.name.as_str()))
                .is_none_or(|(_, m)| m.exported);
            let mut call_sites = Vec::new();
            for b in analyses {
                // Only code can call it, and a private function only from its own file.
                if !b.result.review.parsed || (!exported && b.path != a.path) {
                    continue;
                }
                let added = added_lines(&b.result.hunks);
                let accept = |q: Option<&str>| call_qualifier_ok(q, &a.path, &sig.name);
                for (line, text) in scan_calls(&b.new_source, &name, &accept) {
                    if b.path == a.path && line == sig.location.line_start { continue; }
                    call_sites.push(CallSite { file: b.path.clone(), line, text, updated: added.contains(&line), in_diff: true });
                }
            }
            impacts.push(SignatureImpactEntry {
                id: { let v = *next_id; *next_id += 1; v },
                name: sig.name.clone(),
                description: sig.description.clone(),
                definition: sig.location.clone(),
                call_sites,
                exported,
            });
        }
    }
    impacts
}

/// Parameters of the function or `Class.method` named `name`.
fn find_function<'t>(tree: &'t SemanticTree, name: &str) -> Option<&'t [crate::parser::Param]> {
    for item in &tree.items {
        match item {
            SemanticItem::Function { name: n, params, .. } if n == name => return Some(params),
            SemanticItem::Class { name: c, methods, .. } => {
                for m in methods {
                    if let SemanticItem::Function { name: n, params, .. } = m {
                        if format!("{c}.{n}") == name { return Some(params); }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Drop dead-code entries for items referenced from another changed file.
pub(crate) fn filter_dead_code_cross_file(analyses: &mut [InternalAnalysis]) {
    let refs: Vec<HashSet<u64>> = analyses.iter()
        .map(|a| a.new_tree.meta.iter().flat_map(|m| m.refs.iter().copied()).collect())
        .collect();
    for (fi, a) in analyses.iter_mut().enumerate() {
        a.result.manifest.dead_code.retain(|d| {
            let h = ident_hash(bare_name(&d.name));
            !refs.iter().enumerate().any(|(i, r)| i != fi && r.contains(&h))
        });
    }
}

fn added_lines(hunks: &[DiffHunk]) -> HashSet<usize> {
    hunks.iter()
        .flat_map(|h| h.changes.iter())
        .filter(|c| c.kind == ChangeKind::Added)
        .filter_map(|c| c.new_span.as_ref().map(|s| s.start_line))
        .collect()
}

fn looks_like_comment(trimmed: &str) -> bool {
    trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with("* ")
        || trimmed.starts_with("# ") || trimmed == "#" || trimmed.starts_with("--")
}

/// Lines (1-indexed) in `source` that mention `name` as an identifier, skipping
/// comment lines. With an `owner` (a method's type), only mentions that can refer
/// to that type's method count; see `reference_ok`.
pub fn scan_references(source: &str, name: &str, owner: Option<&str>) -> Vec<(usize, String)> {
    if !source.contains(name) {
        return vec![];
    }
    let mut out = Vec::new();
    for (i, l) in source.lines().enumerate() {
        if looks_like_comment(l.trim_start()) { continue; }
        let mut offset = 0;
        while let Some(pos) = find_identifier(&l[offset..], name) {
            let at = offset + pos;
            if reference_ok(qualifier(&l[..at]), owner) {
                out.push((i + 1, l.trim().chars().take(160).collect()));
                break;
            }
            offset = at + name.len();
        }
    }
    out
}

/// The type a qualified name belongs to: `service.Flush` → `service`, `a::B::run` → `B`.
pub fn owner_of(name: &str) -> Option<&str> {
    name.rsplit_once("::").or_else(|| name.rsplit_once('.')).map(|(o, _)| bare_name(o)).filter(|o| !o.is_empty())
}

/// Whether a mention qualified by `q` can refer to a vanished name. A method (one
/// with an `owner` type) needs that type, `self` or `this` in front of it: some
/// other receiver (`w.Flush()`, `tmpl.Flush()`) is most likely another type's
/// method of the same name, and a missed reference costs less than a false alarm.
pub fn reference_ok(q: Option<&str>, owner: Option<&str>) -> bool {
    match owner {
        None => true,
        Some(o) => q.is_some_and(|q| q.eq_ignore_ascii_case(o) || matches!(q, "self" | "this" | "Self" | "cls")),
    }
}

/// Whether `line` defines `name` rather than using it: `fn name(`, `def name(`,
/// `func (r *T) name(`, `function name(`, or a C/Java/TS-style `Type name(…) {`.
/// A vanished name that is still defined somewhere else is ambiguous, not stale.
pub fn looks_like_definition(line: &str, name: &str) -> bool {
    let t = line.trim();
    if looks_like_comment(t) { return false; }
    let Some(at) = find_identifier(t, name) else { return false };
    let before = t[..at].trim_end();
    let rest = t[at + name.len()..].trim_start();
    let last_word = before.rsplit(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or("");
    // A type or a binding of that name: `class Opt(`, `object Opt {`, `struct Opt`, `val Opt =`.
    if matches!(last_word, "class" | "object" | "trait" | "struct" | "enum" | "interface" | "type" | "typedef" | "impl" | "record"
        | "val" | "var" | "let" | "const" | "static" | "lazy") { return true; }
    if !rest.starts_with('(') { return false; }
    if matches!(last_word, "fn" | "function" | "def" | "func" | "sub" | "proc") { return true; }
    if before.starts_with("func (") && before.ends_with(')') { return true; }
    let qualified = before.ends_with('.') || before.ends_with("::");
    let control = ["return", "if", "else", "while", "for", "switch", "case", "await", "new", "yield", "=", "(", ",", "!", "&&", "||", "=>"]
        .iter().any(|k| before.ends_with(k));
    !qualified && !control && t.ends_with('{')
}

/// Lines that call `name(…)`, skipping comments and declarations. `accept`
/// receives the call's qualifier (`web` in `web::serve(`, `obj` in `obj.run(`).
pub fn scan_calls(source: &str, name: &str, accept: &dyn Fn(Option<&str>) -> bool) -> Vec<(usize, String)> {
    if !source.contains(name) {
        return vec![];
    }
    // Search code only: mentions in strings, docstrings and comments aren't calls.
    let masked = code_only(source);
    let originals: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();
    for (i, l) in masked.lines().enumerate() {
        let t = originals.get(i).copied().unwrap_or("").trim_start();
        if looks_like_comment(t) { continue; }
        let mut offset = 0;
        while let Some(pos) = find_identifier(&l[offset..], name) {
            let at = offset + pos;
            let rest = l[at + name.len()..].trim_start();
            let before = &l[..at];
            let is_decl = ["fn", "function", "def", "func"].iter().any(|kw| before.trim_end().ends_with(kw))
                || looks_like_signature(rest);
            if rest.starts_with('(') && !is_decl && accept(qualifier(before)) {
                out.push((i + 1, t.chars().take(160).collect()));
                break;
            }
            offset = at + name.len();
        }
    }
    out
}

/// `source` with the contents of string literals and comments replaced by
/// spaces (newlines kept), so positions and line numbers still line up.
/// Handles "…", '…', `…`, triple-quoted strings, //, /* */ and # comments.
/// A `'` after `&`, `<` or a word character is a Rust lifetime, not a quote.
pub fn code_only(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        // Line comments.
        if (c == '/' && next == Some('/')) || (c == '#' && next != Some('[') && next != Some('!') && next != Some('{')) {
            while i < chars.len() && chars[i] != '\n' { out.push(' '); i += 1; }
            continue;
        }
        // Block comments.
        if c == '/' && next == Some('*') {
            out.push_str("  "); i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) { out.push(blank(chars[i])); i += 1; }
            if i < chars.len() { out.push_str("  "); i += 2; }
            continue;
        }
        let lifetime = c == '\'' && i > 0 && (chars[i - 1] == '&' || chars[i - 1] == '<' || chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
        if (c == '"' || c == '\'' || c == '`') && !lifetime {
            let triple = next == Some(c) && chars.get(i + 2) == Some(&c) && c != '`';
            let q = if triple { 3 } else { 1 };
            for _ in 0..q { out.push(c); }
            i += q;
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() { out.push(' '); out.push(blank(chars[i + 1])); i += 2; continue; }
                let closes = if triple { chars[i] == c && chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c) } else { chars[i] == c };
                if closes { for _ in 0..q { out.push(c); } i += q; break; }
                // Ordinary quotes end at the line (unterminated or mismatched).
                if !triple && c != '`' && chars[i] == '\n' { break; }
                out.push(blank(chars[i]));
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `(name: Type …` / `(name?: Type …`: a parameter list with type annotations,
/// i.e. a method signature (TypeScript interfaces, overloads), not a call.
fn looks_like_signature(rest: &str) -> bool {
    let Some(inner) = rest.strip_prefix('(') else { return false };
    let inner = inner.trim_start();
    let name_len = inner.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(inner.len());
    name_len > 0 && inner[name_len..].trim_start().trim_start_matches('?').trim_start().starts_with(':')
        && !inner[name_len..].trim_start().starts_with("::")
}

/// The path segment or receiver directly before a call (`a::b::` → `b`, `x.` → `x`).
fn qualifier(before: &str) -> Option<&str> {
    let head = before.strip_suffix("::").or_else(|| before.strip_suffix('.'))?;
    let start = head.rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).map(|p| p + 1).unwrap_or(0);
    Some(&head[start..])
}

/// Whether a call qualified by `q` can refer to `sig_name` defined in `def_path`.
/// Free functions: unqualified, or qualified by their module / self / crate.
/// Methods: any `.method(` receiver, or `Type::method(` with the right type.
pub fn call_qualifier_ok(q: Option<&str>, def_path: &str, sig_name: &str) -> bool {
    let Some(q) = q else { return true };
    let owner = sig_name.rsplit_once("::").or_else(|| sig_name.rsplit_once('.')).map(|(o, _)| bare_name(o));
    let stem = def_path.rsplit('/').next().unwrap_or(def_path).split('.').next().unwrap_or("");
    let dir = def_path.rsplit('/').nth(1).unwrap_or("");
    let module = if matches!(stem, "mod" | "index" | "__init__" | "lib") { dir } else { stem };
    match owner {
        Some(owner) => q == owner || matches!(q, "Self" | "self" | "this") || !q.chars().next().is_some_and(|c| c.is_uppercase()),
        None => q.is_empty() || q == module || matches!(q, "self" | "super" | "crate" | "exports" | "module"),
    }
}

fn kind_of(item: &SemanticItem) -> SymbolKind {
    match item {
        SemanticItem::Function { .. } => SymbolKind::Function,
        SemanticItem::Class { .. } => SymbolKind::Class,
        SemanticItem::Variable { .. } => SymbolKind::Variable,
        SemanticItem::TypeDef { .. } => SymbolKind::Type,
        _ => SymbolKind::Variable,
    }
}
