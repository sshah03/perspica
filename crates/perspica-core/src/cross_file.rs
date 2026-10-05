use crate::classify::find_identifier;
use crate::diff::dice;
use crate::manifest::{ChangeKind, DiffHunk, Location, ManifestEntryId, Side, SymbolKind};
use crate::parser::{bare_name, ident_hash, SemanticItem, SemanticTree};
use crate::{DiffResult, Language};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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
    /// Only with `PERSPICA_GRAPH_DEBUG=1`: the call graph itself, for evaluation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_debug: Option<crate::flow::GraphDebug>,
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
    /// Only its own file can use it, like a private top-level Kotlin function.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub file_private: bool,
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
    /// The parameters before and after, for judging each call (not serialized).
    #[serde(skip)]
    pub params: Option<(Vec<crate::parser::Param>, Vec<crate::parser::Param>)>,
    /// A function defined inside another one. Only that function can call it, so we don't
    /// search the rest of the repo.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallSite {
    pub file: String,
    pub line: usize,
    pub text: String,
    /// The call was changed in this diff, or what it passes still fits the new signature.
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

/// A private top-level Kotlin function or `file` C# type, which only its own file can use.
pub(crate) fn file_private(analyses: &[InternalAnalysis], name: &str, origin: &str) -> bool {
    overloads(origin) && old_item(analyses, name, origin).is_some_and(|(_, m)| !m.exported)
}

/// The top-level item called `name` in the old version of `origin`.
fn old_item<'a>(analyses: &'a [InternalAnalysis], name: &str, origin: &str) -> Option<(&'a SemanticItem, &'a crate::parser::ItemMeta)> {
    let a = analyses.iter().find(|a| a.path == origin)?;
    a.old_tree.items.iter().zip(&a.old_tree.meta).find(|(i, _)| i.name() == Some(name))
}

/// Stale references to vanished names in the new versions of the changed files.
pub(crate) fn detect_broken_references(
    analyses: &[InternalAnalysis],
    moves: &[CrossFileMoveEntry],
    next_id: &mut ManifestEntryId,
) -> Vec<BrokenReferenceEntry> {
    let mut broken = Vec::new();
    for (name, renamed_to, origin, owner) in vanished_names(analyses, moves) {
        let private = file_private(analyses, &name, &origin);
        // A Kotlin or C# function is only called, so `name.x` is something else.
        let function = overloads(&origin) && old_item(analyses, &name, &origin).is_some_and(|(i, _)| matches!(i, SemanticItem::Function { .. }));
        // A changelog names old things on purpose. Config and docs mention names for other reasons.
        for a in analyses.iter().filter(|a| !crate::roles::is_changelog_path(&a.path) && crate::Language::from_path(&a.path) != crate::Language::Unknown) {
            // A private one can only be used in its own file.
            if private && a.path != origin { continue; }
            // Most files never mention the name, so find references first and only then run the other checks.
            let found = scan_references_in(&a.new_source, &name, owner.as_deref(), &origin, &a.path);
            if found.is_empty() { continue; }
            // `const name = …`, `name := …`, a `name` parameter or field: this file's own name explains its
            // bare mentions, not a qualified call like `opts.name()` (a method can't be the local).
            let declares = declares_name(&a.new_source, &name) || imports_name_from_elsewhere(&a.new_source, &a.path, &name, &origin)
                || (overloads(&a.path) && defines_function(&a.new_source, &name, &a.path));
            // In Go, `name.X` in a file that is or imports package `name` is the package (`stacktrace.Take`).
            let go_pkg = a.path.ends_with(".go") && go_package_named(&a.new_source, &name);
            let refs = found.into_iter()
                .filter(|(_, text)| !(go_pkg && go_package_mention(text, &name)))
                .filter(|(_, text)| !(function && find_identifier(text, &name).is_some_and(|at| text[at + name.len()..].starts_with('.'))))
                .filter(|(_, text)| !declares || qualified_mention(text, &name));
            for (line, text) in refs.take(5) {
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

/// Local functions by enclosing function and name. A name defined twice in the same scope,
/// like in an `if` and an `else`, is skipped since we can't tell which one a call means.
fn unique_locals(locals: &[crate::parser::LocalFn]) -> HashMap<(&str, &str), &crate::parser::LocalFn> {
    let mut seen: HashMap<(&str, &str), Option<&crate::parser::LocalFn>> = HashMap::new();
    for l in locals {
        seen.entry((l.scope_name.as_str(), l.name.as_str()))
            .and_modify(|v| *v = None)
            .or_insert(Some(l));
    }
    seen.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect()
}

/// Calls to local functions whose signature changed. We only look in the function that
/// defines them, since nothing else can call them.
pub(crate) fn local_signature_impacts(analyses: &[InternalAnalysis], next_id: &mut ManifestEntryId) -> Vec<SignatureImpactEntry> {
    let mut impacts = Vec::new();
    for a in analyses {
        let new = unique_locals(&a.new_tree.local_fns);
        if new.is_empty() { continue; }
        let old = unique_locals(&a.old_tree.local_fns);
        let mut changed: Vec<(&crate::parser::LocalFn, &crate::parser::LocalFn)> = new.iter()
            .filter_map(|(k, n)| old.get(k).map(|o| (*o, *n)))
            .filter(|(o, n)| crate::classify::breaks_callers(&o.params, &n.params, &a.path))
            .collect();
        if changed.is_empty() { continue; }
        changed.sort_by_key(|(_, n)| n.span.start_line);
        let masked = code_only_in(&a.new_source, &a.path);
        let added = added_lines(&a.result.hunks);
        let bare = |q: Option<&str>| q.is_none();
        for (o, n) in changed {
            let mut call_sites = Vec::new();
            for (line, text, open) in scan_calls_at(&a.new_source, &masked, &n.name, &bare, &a.path) {
                if line < n.scope.start_line || line > n.scope.end_line || line == n.span.start_line { continue; }
                let args = call_arguments(&masked[open..], &a.path);
                let end = line + args.as_ref().map_or(0, |x| x.lines);
                let touched = (line..=end).any(|l| added.contains(&l));
                let fits = matches!(&args, Some(x) if still_fits(&o.params, &n.params, x, named_args(&a.path), &a.path))
                    || matches!(&args, Some(x) if loose_arity(&a.path) && !x.spread && leaves_out_new(&o.params, &n.params, x));
                call_sites.push(CallSite { file: a.path.clone(), line, text, updated: touched || fits, in_diff: true });
            }
            impacts.push(SignatureImpactEntry {
                id: { let v = *next_id; *next_id += 1; v },
                name: n.name.clone(),
                description: crate::classify::describe_signature_change(&o.params, &n.params, &None, &None)
                    .unwrap_or_else(|| "signature changed".into()),
                definition: Location { file: Some(a.path.clone()), line_start: n.span.start_line, line_end: n.span.end_line, side: Side::New },
                call_sites,
                exported: false,
                params: Some((o.params.clone(), n.params.clone())),
                local: true,
            });
        }
    }
    impacts
}

/// Builtins and globals that a name falls back to once a local function with that name is gone.
const FALLBACK_NAMES: &[&str] = &[
    "abs", "all", "any", "ascii", "bin", "bool", "bytes", "callable", "chr", "dict", "dir", "divmod", "enumerate",
    "eval", "exec", "filter", "float", "format", "frozenset", "getattr", "globals", "hasattr", "hash", "help", "hex",
    "id", "input", "int", "isinstance", "issubclass", "iter", "len", "list", "locals", "map", "max", "min", "next",
    "object", "oct", "open", "ord", "pow", "print", "property", "range", "repr", "reversed", "round", "set", "setattr",
    "slice", "sorted", "str", "sum", "super", "tuple", "type", "vars", "zip",
    "alert", "clearInterval", "clearTimeout", "console", "decodeURI", "encodeURI", "fetch", "isNaN", "parseFloat",
    "parseInt", "queueMicrotask", "require", "setInterval", "setTimeout", "structuredClone",
];

/// References to local functions that were removed or renamed, in the function they were in.
/// We skip a name if it still means something else in the file, like another definition, a
/// variable, a parameter, an import or a builtin.
pub(crate) fn local_broken_references(analyses: &[InternalAnalysis], next_id: &mut ManifestEntryId) -> Vec<BrokenReferenceEntry> {
    let mut broken = Vec::new();
    for a in analyses.iter().filter(|a| !crate::roles::is_changelog_path(&a.path)) {
        let old = unique_locals(&a.old_tree.local_fns);
        if old.is_empty() { continue; }
        let new = unique_locals(&a.new_tree.local_fns);
        let mut gone: Vec<(&(&str, &str), &&crate::parser::LocalFn)> = old.iter().filter(|(k, _)| !new.contains_key(*k)).collect();
        if gone.is_empty() { continue; }
        gone.sort_by_key(|(_, o)| o.span.start_line);
        let item_names = |t: &SemanticTree| -> HashSet<String> {
            t.items.iter().flat_map(|i| {
                let mut v: Vec<String> = i.name().map(|n| bare_name(n).to_string()).into_iter().collect();
                if let SemanticItem::Class { methods, .. } = i { v.extend(methods.iter().filter_map(|m| m.name().map(str::to_string))); }
                v
            }).collect()
        };
        let (old_items, new_items) = (item_names(&a.old_tree), item_names(&a.new_tree));
        let lines = a.new_source.lines().count();
        for ((scope_name, name), o) in gone {
            let (scope_name, name) = (*scope_name, *name);
            if FALLBACK_NAMES.contains(&name) || old_items.contains(name) || new_items.contains(name) { continue; }
            // Skip it if it was defined in another scope before, or is defined anywhere now.
            if a.old_tree.local_fns.iter().filter(|l| l.name == name).count() > 1 || a.new_tree.local_fns.iter().any(|l| l.name == name) { continue; }
            if declares_name(&a.new_source, name) || import_of(&a.new_source, &a.path, name).is_some() { continue; }
            // Find where the enclosing function is now.
            let (from, to) = if scope_name.is_empty() {
                (1, lines)
            } else {
                let spans: Vec<(usize, usize)> = a.new_tree.items.iter()
                    .flat_map(|i| match i {
                        SemanticItem::Class { methods, .. } => methods.iter().collect::<Vec<_>>(),
                        _ => vec![i],
                    })
                    .filter(|i| i.name().map(bare_name) == Some(scope_name))
                    .map(|i| (i.span().start_line, i.span().end_line))
                    .chain(a.new_tree.local_fns.iter().filter(|l| l.name == scope_name).map(|l| (l.span.start_line, l.span.end_line)))
                    .collect();
                match spans.as_slice() { [one] => *one, _ => continue }
            };
            // Treat it as renamed if exactly one new local function in the same scope takes the same number of parameters.
            let renames: Vec<&str> = new.iter()
                .filter(|((s, n), l)| *s == scope_name && !old.contains_key(&(*s, *n)) && l.params.len() == o.params.len())
                .map(|((_, n), _)| *n)
                .collect();
            let renamed_to = match renames.as_slice() { [one] => Some(one.to_string()), _ => None };
            let origin = if scope_name.is_empty() { a.path.clone() } else { format!("{scope_name} in {}", a.path) };
            let refs = scan_references_in(&a.new_source, name, None, &a.path, &a.path).into_iter()
                .filter(|(line, _)| (from..=to).contains(line));
            for (line, text) in refs.take(5) {
                broken.push(BrokenReferenceEntry {
                    id: { let v = *next_id; *next_id += 1; v },
                    symbol_name: name.to_string(),
                    renamed_from: renamed_to.clone(),
                    reference_file: a.path.clone(),
                    reference_location: Location { file: Some(a.path.clone()), line_start: line, line_end: line, side: Side::New },
                    reason: broken_reason(name, renamed_to.as_deref(), &origin),
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
    // Each file's code with comments and strings blanked out, computed once when a file needs it.
    let masks: Vec<std::cell::OnceCell<String>> = analyses.iter().map(|_| std::cell::OnceCell::new()).collect();
    // Names the change renames (`stacktrace` → `Stack`), so a parameter of a renamed type isn't a changed type.
    let renames: HashMap<String, String> = analyses.iter()
        .flat_map(|a| a.result.manifest.renames.iter())
        .map(|r| (bare_name(&r.old_name).to_string(), bare_name(&r.new_name).to_string()))
        .collect();
    // Full renames, new name → old (`Formatter.FormatStack` → `stackFormatter.FormatStack`).
    let renamed_from: HashMap<&str, &str> = analyses.iter()
        .flat_map(|a| a.result.manifest.renames.iter())
        .map(|r| (r.new_name.as_str(), r.old_name.as_str()))
        .collect();
    for a in analyses {
        for sig in &a.result.manifest.signature_changes {
            // The function before the change, under its old name if it (or its type) was renamed,
            // and in whichever changed file it came from.
            let old_name = renamed_from.get(sig.name.as_str()).map(|s| s.to_string()).unwrap_or_else(|| {
                match owner_of(&sig.name).and_then(|o| renamed_from.get(o)) {
                    Some(old_owner) => sig.name.replacen(owner_of(&sig.name).unwrap_or(""), old_owner, 1),
                    None => sig.name.clone(),
                }
            });
            // With overloads, the one whose signature changed. Methods pair up in order.
            let nth = functions_named(&a.new_tree, &sig.name).iter().position(|(_, s)| s.start_line == sig.location.line_start).unwrap_or(0);
            let old = find_function(&a.old_tree, &old_name, nth)
                .or_else(|| analyses.iter().find_map(|o| find_function(&o.old_tree, &old_name, nth)))
                .map(|p| p.to_vec());
            let new = find_function(&a.new_tree, &sig.name, nth).map(|p| p.to_vec());
            // Without both versions there's nothing to judge a call against: don't guess.
            let (Some(old), Some(new)) = (old, new) else { continue };
            // Only changes that can break an existing call (e.g. not a new optional param).
            if !crate::classify::breaks_callers(&old, &new, &a.path) {
                continue;
            }
            let params = Some((old.into_iter().map(|p| renamed_types(p, &renames)).collect::<Vec<_>>(), new));
            // A constructor (`__init__`, `constructor`) is called by its class's name.
            let callee = call_name(&sig.name);
            let callee_sig = if callee != bare_name(&sig.name) { callee.to_string() } else { sig.name.clone() };
            let exported = a.new_tree.items.iter().zip(&a.new_tree.meta)
                .find(|(i, _)| i.name() == Some(sig.name.as_str()))
                .is_none_or(|(_, m)| m.exported);
            let mut call_sites = Vec::new();
            // Other changed files that define a function of the same name (another package's copy).
            let same_named: Vec<&str> = analyses.iter()
                .filter(|o| o.path != a.path && o.new_tree.items.iter().any(|i| matches!(i, SemanticItem::Function { .. }) && i.name().map(bare_name) == Some(callee)))
                .map(|o| o.path.as_str())
                .collect();
            for (bi, b) in analyses.iter().enumerate() {
                // A file that never mentions the name can't call it.
                if !b.new_source.contains(callee) {
                    continue;
                }
                // Only code can call it, and a private function only from its own file.
                if !b.result.review.parsed || (!exported && !private_reaches(&a.path, &b.path) && !includes_file(&b.new_source, &a.path)) {
                    continue;
                }
                // A call belongs to the nearest definition of that name: its own file's, or its package's.
                let shared = |p: &str| p.split('/').zip(b.path.split('/')).take_while(|(x, y)| x == y).count();
                if same_named.iter().any(|o| *o == b.path || shared(o) > shared(&a.path)) {
                    continue;
                }
                let added = added_lines(&b.result.hunks);
                let accept = |q: Option<&str>| call_qualifier_ok_from(q, &a.path, &callee_sig, &b.path);
                let masked = masks[bi].get_or_init(|| code_only_in(&b.new_source, &b.path));
                let elsewhere = b.path != a.path && imports_it_elsewhere(&b.new_source, &b.path, &a.path, callee);
                // In C# and Kotlin a bare call in a file with its own method of that name calls that one.
                let own = b.path != a.path && overloads(&b.path) && defines_function(&b.new_source, callee, &b.path);
                for (line, text, open) in scan_calls_at(&b.new_source, masked, callee, &accept, &b.path) {
                    if b.path == a.path && line == sig.location.line_start { continue; }
                    if (elsewhere || own) && unqualified_call(&text, callee) { continue; }
                    let args = call_arguments(&masked[open..], &b.path);
                    if matches!((&params, &args), (Some((o, _)), Some(x)) if other_overload(o, x, &a.path)) { continue; }
                    // The whole call counts, not just its first line: `f(\n  new_arg,\n)`.
                    let end = line + args.as_ref().map_or(0, |a| a.lines);
                    let touched = (line..=end).any(|l| added.contains(&l));
                    let fits = matches!((&params, &args), (Some((o, n)), Some(x)) if still_fits(o, n, x, named_args(&a.path), &a.path))
                        || matches!(&args, Some(x) if loose_arity(&a.path) && !x.spread && params.as_ref().is_some_and(|(o, n)| leaves_out_new(o, n, x)));
                    call_sites.push(CallSite { file: b.path.clone(), line, text, updated: touched || fits, in_diff: true });
                }
            }
            impacts.push(SignatureImpactEntry {
                id: { let v = *next_id; *next_id += 1; v },
                name: sig.name.clone(),
                description: sig.description.clone(),
                definition: sig.location.clone(),
                call_sites,
                exported,
                params,
                local: false,
            });
        }
    }
    impacts
}

/// Languages where a caller can pass an argument by name (`f(x=1)`), so parameter names matter.
pub fn named_args(path: &str) -> bool {
    path.ends_with(".py") || path.ends_with(".scala") || path.ends_with(".sc")
}

/// Languages that pick between overloads of a name by the arguments.
pub fn overloads(path: &str) -> bool {
    path.ends_with(".cs") || path.ends_with(".kt") || path.ends_with(".kts")
}

/// A call that doesn't fit the old signature either is a call to some other overload.
pub fn other_overload(old: &[crate::parser::Param], args: &CallArgs, def_path: &str) -> bool {
    overloads(def_path) && !still_fits(old, old, args, false, def_path)
}

/// JavaScript, TypeScript and Python import what they call: a file that uses imports but doesn't
/// import `name` from `def_path`'s module calls some other `name` (another package's copy, a local one).
pub fn imports_it_elsewhere(source: &str, caller: &str, def_path: &str, name: &str) -> bool {
    // A script in the global scope (no imports at all) sees everything.
    let uses_imports = explicit_imports(caller) && source.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("import ") || t.starts_with("from ") || t.contains("require(")
    });
    uses_imports && match import_of(source, caller, name) {
        None => true,
        // `import { name } from 'pkg'`: a package name may be this very repo (`valtio` importing itself).
        Some((m, true)) if bare_package(&m, caller) => false,
        Some((m, _)) => !module_matches(&m, caller, def_path),
    }
}

/// `name(` not called on something (`x.name(`, `X::name(`): it has to be imported by name.
pub fn unqualified_call(line: &str, name: &str) -> bool {
    crate::classify::find_identifier(line, name).is_some_and(|at| !line[..at].trim_end().ends_with(['.', ':']))
}

/// Languages where a name from another file has to be imported to be called.
fn explicit_imports(path: &str) -> bool {
    [".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".py"].iter().any(|e| path.ends_with(e))
}

/// The module a file imports `name` from: `import { name } from "./x"`, `import name from "x"`,
/// `const { name } = require("x")`, `from .x import name`. None if it doesn't import it by name.
/// Also tells whether the name is imported by name (`{ name }`, `from x import name`) rather than
/// bound locally to a default or whole-module import (`import name from`).
fn import_of(source: &str, path: &str, name: &str) -> Option<(String, bool)> {
    let py = path.ends_with(".py");
    // Imports can span lines (`import {\n a,\n b\n} from "x"`): read statement by statement.
    let mut stmt = String::new();
    for line in source.lines() {
        let t = line.trim();
        if stmt.is_empty() && !(t.starts_with("import ") || t.starts_with("from ") || t.starts_with("export ") || t.contains("require(")) { continue; }
        stmt.push_str(t); stmt.push(' ');
        let done = if py { !t.ends_with('\\') && (!stmt.contains('(') || stmt.contains(')')) } else { t.contains(" from ") || t.contains("require(") || t.ends_with(';') || (stmt.starts_with("import ") && (t.ends_with('"') || t.ends_with('\''))) };
        if !done { continue; }
        let s = std::mem::take(&mut stmt);
        if !crate::classify::contains_identifier(&s, name) { continue; }
        if py {
            if let Some(rest) = s.strip_prefix("from ") {
                let module = rest.split_whitespace().next().unwrap_or("");
                if s.contains(" import ") { return Some((module.to_string(), true)); }
            }
        } else {
            let quoted = s.split(['"', '\'', '`']).nth(1);
            let braces = s.find('{').zip(s.find('}')).is_some_and(|(o, c)| s[o..c].contains(name));
            if s.contains(" from ") || s.contains("require(") { if let Some(m) = quoted { return Some((m.to_string(), braces)); } }
        }
    }
    None
}

/// Whether an import specifier written in `from` (`./x`, `../lib/x`, `@scope/pkg`, `.x`, `pkg.x`)
/// can point at the file `target`.
fn module_matches(spec: &str, from: &str, target: &str) -> bool {
    let strip = |p: &str| -> String {
        // Only a code file's extension: `./Components.lib` names `Components.lib.tsx`.
        let code_ext = ["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts", "py"];
        let p = p.strip_suffix(".d.ts").unwrap_or(p);
        let p = p.rsplit_once('.').filter(|(_, ext)| code_ext.contains(ext)).map_or(p, |(stem, _)| stem);
        p.strip_suffix("/index").or_else(|| p.strip_suffix("/__init__")).unwrap_or(p).to_string()
    };
    let target_mod = strip(target);
    let dir: Vec<&str> = from.rsplit_once('/').map_or(vec![], |(d, _)| d.split('/').collect());
    if from.ends_with(".py") {
        // `.x` / `..x` relative, else dotted from the root (compare the tail).
        let dots = spec.chars().take_while(|&c| c == '.').count();
        let tail = spec[dots..].replace('.', "/");
        if dots > 0 {
            let mut base = dir.clone(); for _ in 1..dots { base.pop(); }
            let joined = if tail.is_empty() { base.join("/") } else { format!("{}/{tail}", base.join("/")) };
            return target_mod == joined.trim_start_matches('/');
        }
        return target_mod.ends_with(&tail);
    }
    if spec.starts_with('.') {
        let mut parts = dir.clone();
        for seg in spec.split('/') {
            match seg { "." => {}, ".." => { parts.pop(); }, s => parts.push(s) }
        }
        return strip(&parts.join("/")) == target_mod;
    }
    // A package name (`@tanstack/react-form`, `lodash/merge`): its last part names a directory on the way.
    let pkg = spec.trim_start_matches('@').split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>();
    pkg.iter().rev().take(2).any(|seg| target.split('/').any(|t| t == *seg))
}

/// Plain JavaScript: a caller may leave out any trailing argument (it's just `undefined`).
pub fn loose_arity(path: &str) -> bool {
    [".js", ".jsx", ".mjs", ".cjs"].iter().any(|e| path.ends_with(e))
}

/// The call passes only parameters that kept their place, leaving the new ones out.
fn leaves_out_new(old: &[crate::parser::Param], new: &[crate::parser::Param], args: &CallArgs) -> bool {
    args.keywords.is_empty() && args.positional <= new.len() && (0..args.positional).all(|i| old.get(i).is_some_and(|o| o.name == new[i].name))
}

/// The name a function is called by: a constructor (`Foo.__init__`, `Foo.constructor`) by its class.
pub fn call_name(sig_name: &str) -> &str {
    match (owner_of(sig_name), bare_name(sig_name)) {
        (Some(class), "__init__" | "__new__" | "constructor") => class,
        (_, name) => name,
    }
}

/// What a call passes: positional arguments, `name=` keyword arguments, and whether
/// anything is spread (`*args`, `...rest`), which makes the count unknowable.
#[derive(Debug, Clone, PartialEq)]
pub struct CallArgs {
    pub positional: usize,
    pub keywords: Vec<String>,
    pub spread: bool,
    /// Lines the call spans after its first.
    pub lines: usize,
}

/// The arguments of the call whose `(` starts `from` (strings and comments already blanked),
/// in the file `path` (spread syntax differs: `*args` in Python, `...args` in JavaScript,
/// `args...` in Go; elsewhere `*x` is a dereference).
pub fn call_arguments(from: &str, path: &str) -> Option<CallArgs> {
    let py = path.ends_with(".py");
    let go = path.ends_with(".go");
    let cs = path.ends_with(".cs");
    let kt = path.ends_with(".kt") || path.ends_with(".kts");
    let mut chars = from.char_indices();
    if chars.next()?.1 != '(' { return None; }
    let (mut depth, mut start, mut lines, mut closed, mut end) = (0usize, 1usize, 0usize, false, 0usize);
    let mut args: Vec<&str> = Vec::new();
    for (i, c) in chars {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth > 0 => depth -= 1,
            ')' => { args.push(&from[start..i]); closed = true; end = i + 1; break; }
            ',' if depth == 0 => { args.push(&from[start..i]); start = i + 1; }
            '\n' => lines += 1,
            _ => {}
        }
        if i > 20_000 { return None; }
    }
    if !closed { return None; }
    if args.last().is_none_or(|a| a.trim().is_empty()) { args.pop(); }
    let mut out = CallArgs { positional: 0, keywords: Vec::new(), spread: false, lines };
    // Kotlin passes a lambda after the parentheses as the last argument.
    if kt && from[end..].trim_start_matches([' ', '\t']).starts_with('{') { out.positional += 1; }
    for a in args {
        let a = a.trim();
        if a.starts_with("...") || ((py || kt) && a.starts_with('*')) || (go && a.ends_with("...")) { out.spread = true; continue; }
        // `name=value` (Python keyword), not `a == b` or `a => b`. C# writes `name: value`.
        let kw = if cs { a.find(':').filter(|&i| i > 0 && !a[i..].starts_with("::")) } else {
            a.find('=').filter(|&i| i > 0 && !a[i..].starts_with("==") && !a[i..].starts_with("=>") && !a[..i].ends_with(['!', '<', '>', '=']))
        };
        match kw.map(|i| a[..i].trim()).filter(|k| k.chars().all(|c| c.is_alphanumeric() || c == '_')) {
            Some(k) if !k.is_empty() => out.keywords.push(k.to_string()),
            _ => out.positional += 1,
        }
    }
    Some(out)
}

/// Whether a call that passes `args` means the same thing, and is still allowed, under the new
/// parameters: every argument it passes lands on the same parameter as before, it passes all the
/// new required ones, and not more than the new signature takes. With `named_args` (Python,
/// Scala) a parameter is the same if its name is; otherwise (positional-only languages, where a
/// rename can't break a call) if its type is, when both are written down.
pub fn still_fits(old: &[crate::parser::Param], new: &[crate::parser::Param], args: &CallArgs, named_args: bool, def_path: &str) -> bool {
    if args.spread { return false; }
    // `*args` / `...rest` take extra positional arguments, `**kwargs` extra keywords.
    let rest_pos = new.iter().any(|p| crate::classify::catch_all(p, def_path) == Some(false));
    let rest_kw = new.iter().any(|p| crate::classify::catch_all(p, def_path) == Some(true));
    let fixed = |ps: &[crate::parser::Param]| ps.iter().filter(|p| crate::classify::catch_all(p, def_path).is_none()).cloned().collect::<Vec<_>>();
    let (old, new) = (fixed(old), fixed(new));
    let (old, new) = (old.as_slice(), new.as_slice());
    let n = args.positional;
    if n > new.len() && !rest_pos { return false; }
    let n = n.min(new.len());
    // A parameter that moved (`(a, b)` → `(b, a)`) takes a different argument now; a pure rename doesn't.
    let moved = |o: &crate::parser::Param, p: &crate::parser::Param| o.name != p.name && (new.iter().any(|x| x.name == o.name) || old.iter().any(|x| x.name == p.name));
    let same = |o: &crate::parser::Param, p: &crate::parser::Param| if named_args { o.name == p.name } else {
        !moved(o, p) && match (&o.type_annotation, &p.type_annotation) { (Some(a), Some(b)) => a.split_whitespace().eq(b.split_whitespace()), _ => true }
    };
    if (0..n).any(|i| !old.get(i).is_some_and(|o| same(o, &new[i]))) { return false; }
    if !rest_kw && args.keywords.iter().any(|k| !new.iter().any(|p| &p.name == k)) { return false; }
    new.iter().enumerate().skip(n).all(|(_, p)| p.optional || args.keywords.contains(&p.name))
}

/// A parameter with the change's renames applied to its type.
fn renamed_types(mut p: crate::parser::Param, renames: &HashMap<String, String>) -> crate::parser::Param {
    if let Some(t) = &p.type_annotation {
        let mut out = String::with_capacity(t.len());
        let mut word = String::new();
        for c in t.chars().chain(std::iter::once('\0')) {
            if c.is_alphanumeric() || c == '_' { word.push(c); continue; }
            out.push_str(renames.get(&word).map_or(word.as_str(), String::as_str));
            word.clear();
            if c != '\0' { out.push(c); }
        }
        p.type_annotation = Some(out);
    }
    p
}

/// Parameters of the function or `Class.method` named `name`. With overloads, the `nth` one,
/// or the first if there aren't that many.
fn find_function<'t>(tree: &'t SemanticTree, name: &str, nth: usize) -> Option<&'t [crate::parser::Param]> {
    let all = functions_named(tree, name);
    all.get(nth).or(all.first()).map(|(p, _)| *p)
}

/// Every function or `Class.method` named `name`, in order.
fn functions_named<'t>(tree: &'t SemanticTree, name: &str) -> Vec<(&'t [crate::parser::Param], &'t crate::manifest::Span)> {
    let mut out = Vec::new();
    for item in &tree.items {
        match item {
            SemanticItem::Function { name: n, params, span, .. } if n == name => out.push((params.as_slice(), span)),
            SemanticItem::Class { name: c, methods, .. } => {
                for m in methods {
                    if let SemanticItem::Function { name: n, params, span, .. } = m {
                        if name.len() == c.len() + 1 + n.len() && name.starts_with(c.as_str()) && name[c.len()..].starts_with('.') && name.ends_with(n.as_str()) {
                            out.push((params.as_slice(), span));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Drop dead-code entries for items referenced from another changed file.
pub(crate) fn filter_dead_code_cross_file(analyses: &mut [InternalAnalysis]) {
    let refs: Vec<HashSet<u64>> = analyses.iter()
        .map(|a| a.new_tree.meta.iter().flat_map(|m| m.refs.iter().copied()).collect())
        .collect();
    for (fi, a) in analyses.iter_mut().enumerate() {
        a.result.manifest.dead_code.retain(|d| {
            let names = crate::classify::bound_names(bare_name(&d.name));
            !names.iter().any(|n| { let h = ident_hash(n); refs.iter().enumerate().any(|(i, r)| i != fi && r.contains(&h)) })
        });
    }
}

fn added_lines(hunks: &[DiffHunk]) -> HashSet<usize> {
    hunks.iter()
        .flat_map(|h| h.changes.iter())
        .filter(|c| c.kind == ChangeKind::Added)
        .filter_map(|c| c.new_span.as_ref().map(|s| s.start_line..=s.end_line.max(s.start_line)))
        .flatten()
        .collect()
}

fn looks_like_comment(trimmed: &str) -> bool {
    trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with("* ")
        || trimmed.starts_with("# ") || trimmed == "#" || trimmed.starts_with("--")
}

/// Lines (1-indexed) in `source` that mention `name` as an identifier, skipping
/// comment lines. With an `owner` (a method's type), only mentions that can refer
/// to that type's method count; see `reference_ok`.
pub fn scan_references(source: &str, name: &str, owner: Option<&str>, origin: &str) -> Vec<(usize, String)> {
    scan_references_in(source, name, owner, origin, "")
}

/// `scan_references` for the file at `path`. In Scala, Java, C# and Kotlin a method's own file can call it
/// bare, since `name(` is `this.name(`.
pub fn scan_references_in(source: &str, name: &str, owner: Option<&str>, origin: &str, path: &str) -> Vec<(usize, String)> {
    let implicit_this = path == origin && [".scala", ".sc", ".java", ".cs", ".kt", ".kts"].iter().any(|e| path.ends_with(e));
    if !source.contains(name) {
        return vec![];
    }
    // Code only: a name in a string (`if name == "old_name":` in a deprecation shim, a message)
    // isn't a use of it. Python's `__all__` lists exports as strings, so it's kept.
    let masked = code_only_in(source, path);
    let originals: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();
    for (i, m) in masked.lines().enumerate() {
        let original = originals.get(i).copied().unwrap_or("");
        let l = if original.contains("__all__") { original } else { m };
        if looks_like_comment(original.trim_start()) { continue; }
        // `package a.name` and `namespace A.Name` name a package, not the symbol.
        if overloads(path) && ["package ", "namespace "].iter().any(|k| original.trim_start().starts_with(k)) { continue; }
        let mut offset = 0;
        while let Some(pos) = find_identifier(&l[offset..], name) {
            let at = offset + pos;
            let q = qualifier(&l[..at]);
            // `self.` / `this.` in some other file is that file's type.
            let foreign_self = !path.is_empty() && path != origin && matches!(q, Some("self" | "this" | "Self" | "cls"));
            if (reference_ok(q, owner, origin, name) && !foreign_self) || (implicit_this && q.is_none() && !looks_like_definition(l, name)) {
                out.push((i + 1, original.trim().chars().take(160).collect()));
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
/// Anything else (a function, type or value from `origin`) is used bare or through its
/// module (`utils.split(`), never through some other value (`line.split(` is a string's).
pub fn reference_ok(q: Option<&str>, owner: Option<&str>, origin: &str, name: &str) -> bool {
    match owner {
        None => q.is_none_or(|q| {
            let stem = origin.rsplit('/').next().unwrap_or(origin).split('.').next().unwrap_or("");
            let dir = origin.rsplit('/').nth(1).unwrap_or("");
            q == stem || q == dir || matches!(q, "exports" | "module" | "crate" | "super")
        }),
        // A distinctive method name (`getOut`, `outer_sep`) on any value (`c.getOut(`, `.outer_sep()`):
        // nothing else is likely to share it, and the CLI drops it if the repo defines it anywhere else.
        Some(o) => q.is_some_and(|q| q.eq_ignore_ascii_case(o) || matches!(q, "self" | "this" | "Self" | "cls")
            || (distinctive(name) && !q.chars().next().is_some_and(|c| c.is_uppercase()))),
    }
}

/// A name unlikely to be shared by unrelated methods: long, compound (`getOut`, `outer_sep`), and
/// not one of the everyday ones every library has (`readLine`, `toString`).
pub fn distinctive(name: &str) -> bool {
    const COMMON: &[&str] = &["toString", "valueOf", "hashCode", "readLine", "readAll", "getName", "setName", "getValue", "setValue",
        "getType", "isEmpty", "addAll", "forEach", "toJSON", "to_string", "to_owned", "as_str", "as_ref", "into_iter", "is_empty",
        "is_some", "is_none", "unwrap_or", "and_then", "get_mut", "ToString", "GetHashCode", "GetType", "__init__", "__call__", "__enter__", "__exit__", "__repr__", "__str__"];
    let compound = name.contains('_') || name.chars().skip(1).any(|c| c.is_uppercase());
    name.trim_matches('_').len() >= 6 && compound && !COMMON.contains(&name)
}

/// `#include "../cJSON.c"`: a C file compiled into another, whose static functions that file sees.
pub fn includes_file(source: &str, def: &str) -> bool {
    let file = def.rsplit('/').next().unwrap_or(def);
    def.ends_with(".c") && source.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("#include") && t.trim_end().trim_end_matches(['"', '>']).ends_with(file)
    })
}

/// Whether the caller at `caller` can see a function defined at `def` that isn't exported:
/// its own file, or where the language lets private names reach further — a Go package (its
/// directory), a Rust module and its children (the directory and below), a Python package.
pub fn private_reaches(def: &str, caller: &str) -> bool {
    if def == caller { return true; }
    let dir = |p: &str| p.rsplit_once('/').map_or("", |(d, _)| d).to_string();
    let (d, c) = (dir(def), dir(caller));
    if def.ends_with(".go") || def.ends_with(".py") { return d == c; }
    if def.ends_with(".rs") { return c == d || c.starts_with(&format!("{d}/")) || (d.is_empty() && !c.contains("/../")); }
    false
}

/// `import name from "top-sites"` / `from other import name`: the file imports the name from some
/// module other than the one it was removed from, so its uses are that module's.
pub fn imports_name_from_elsewhere(source: &str, path: &str, name: &str, origin: &str) -> bool {
    match import_of(source, path, name) {
        // `import { name } from 'pkg'` may be the repo's own package: still possibly the removed one.
        Some((m, true)) if bare_package(&m, path) => false,
        Some((m, _)) => !module_matches(&m, path, origin),
        None => false,
    }
}

/// `'valtio'`, `'@scope/pkg'`: a package name rather than a path (Python's dotted names are paths).
fn bare_package(spec: &str, path: &str) -> bool {
    !path.ends_with(".py") && !spec.starts_with('.') && !spec.starts_with('/')
}

/// `x.name` / `x::name`: the name reached through something, which a local of that name can't be.
pub fn qualified_mention(line: &str, name: &str) -> bool {
    let mut offset = 0;
    while let Some(pos) = crate::classify::find_identifier(&line[offset..], name) {
        let at = offset + pos;
        let before = line[..at].trim_end();
        if before.ends_with('.') || before.ends_with("::") { return true; }
        offset = at + name.len();
    }
    false
}

/// Whether a Go file is package `name` or imports a package named `name` (`"…/stacktrace"`).
pub fn go_package_named(source: &str, name: &str) -> bool {
    source.lines().any(|l| {
        let t = l.trim();
        t == format!("package {name}") || t.trim_end_matches('"').ends_with(&format!("/{name}")) || t.contains(&format!("{name} \""))
    })
}

/// `name.X` or `package name`: a use of the package, not of a type called `name`.
pub fn go_package_mention(line: &str, name: &str) -> bool {
    let t = line.trim();
    t.starts_with("package ") || crate::classify::find_identifier(t, name).is_some_and(|at| t[at + name.len()..].starts_with('.'))
}

/// Whether `source` defines or declares `name` itself: as a function or type (see
/// `looks_like_definition`), a variable (`name :=`, `name = …` at the start of a line,
/// `for _, name :=`), a parameter (`(name T`, `, name ...T`) or a struct field (`name T` on its own line).
pub fn declares_name(source: &str, name: &str) -> bool {
    let masked = code_only(source);
    masked.lines().any(|l| {
        if looks_like_definition(l, name) { return true; }
        let t = l.trim();
        let mut offset = 0;
        while let Some(pos) = find_identifier(&t[offset..], name) {
            let at = offset + pos;
            let before = t[..at].trim_end();
            let after = t[at + name.len()..].trim_start();
            let qualified = before.ends_with('.') || before.ends_with("::");
            // `let Some(name) = …`, `let (a, name) = …`, `for name in`: a pattern binding before `=` / `in`.
            if !qualified && (t.starts_with("let ") || t.starts_with("if let ") || t.starts_with("while let ") || t.starts_with("for "))
                && t[at..].find(['=', ' ']).is_some() && t.find(" = ").or_else(|| t.find(" in ")).is_some_and(|eq| eq > at) { return true; }
            // `name :=`, `a, name :=`, `name = …` opening a statement (not `x == name`, not `obj.name =`).
            if !qualified && (after.starts_with(":=") || (after.starts_with(',') && t[at..].contains(":=")) || (before.is_empty() && after.starts_with('=') && !after.starts_with("=="))) { return true; }
            // A parameter or field: `(name T`, `, name T`, `name ...T`, or `name T` alone on a struct's line.
            let typed = after.starts_with("...") || after.chars().next().is_some_and(|c| c == '*' || c == '[' || c.is_alphabetic());
            if !qualified && typed && (before.ends_with('(') || before.ends_with(',') || before.is_empty()) && !after.starts_with("in ") && !after.starts_with("of ") {
                // `name Type` but not `name := `, `name(`, `name.x`, or a keyword statement (`return name`).
                let next = after.split(|c: char| c.is_whitespace() || c == ',' || c == ')').next().unwrap_or("");
                if !next.is_empty() && !matches!(next, "=" | "==" | "!=" | "&&" | "||" | "and" | "or" | "if" | "else") && !after.starts_with('(') { return true; }
            }
            offset = at + name.len();
        }
        false
    })
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
    if matches!(last_word, "fn" | "function" | "def" | "func" | "sub" | "proc" | "fun") { return true; }
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

/// Like `scan_calls`, on an already-masked source, also giving the byte offset of each call's `(`.
pub fn scan_calls_at(source: &str, masked: &str, name: &str, accept: &dyn Fn(Option<&str>) -> bool, path: &str) -> Vec<(usize, String, usize)> {
    if !source.contains(name) {
        return vec![];
    }
    let originals: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();
    let mut line_start = 0;
    for (i, l) in masked.split('\n').enumerate() {
        let t = originals.get(i).copied().unwrap_or("").trim_start();
        let base = line_start;
        line_start += l.len() + 1;
        if looks_like_comment(t) { continue; }
        let mut offset = 0;
        while let Some(pos) = find_identifier(&l[offset..], name) {
            let at = offset + pos;
            let after = &l[at + name.len()..];
            let rest = after.trim_start();
            let before = &l[..at];
            if rest.starts_with('(') && !is_declaration(before, rest, path) && accept(qualifier(before)) {
                let open = base + at + name.len() + (after.len() - rest.len());
                out.push((i + 1, t.chars().take(160).collect(), open));
                break;
            }
            offset = at + name.len();
        }
    }
    out
}

/// Whether the name between `before` and `rest` on a line is being defined rather than called.
fn is_declaration(before: &str, rest: &str, path: &str) -> bool {
    let b = before.trim_end();
    ["fn", "function", "def", "func", "class"].iter().any(|kw| b.ends_with(kw))
        || looks_like_signature(rest)
        || ((path.ends_with(".kt") || path.ends_with(".kts")) && kotlin_fun(b))
        || (path.ends_with(".cs") && csharp_return_type(b))
}

/// `fun name(`, `fun <T> name(` and `fun String.name(`.
fn kotlin_fun(before: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    before.match_indices("fun").any(|(i, _)| {
        let after = &before[i + 3..];
        !before[..i].ends_with(word) && (after.is_empty() || after.starts_with(' '))
            && !after.contains(['(', ')', '=', '{', '}', ';'])
    })
}

/// C# writes the return type right before a method's name, like `void Run(`, `Task<int> Get(`
/// or `string[] Split(`. A call comes after a keyword or an operator instead.
fn csharp_return_type(before: &str) -> bool {
    const KEYWORDS: &[&str] = &["return", "await", "new", "else", "yield", "throw", "case", "in", "is", "as", "not", "and", "or",
        "when", "out", "ref", "goto", "typeof", "nameof", "sizeof", "default", "lock", "using", "fixed", "checked", "unchecked",
        "from", "select", "where", "let", "on", "equals", "by", "into", "orderby", "join", "do", "var", "params", "stackalloc"];
    let word = |c: char| c.is_alphanumeric() || c == '_';
    match before.chars().last() {
        // A generic type, not `a > b` or `x => f(`.
        Some('>') => before.matches('<').count() >= before.matches('>').count(),
        Some(']') => true,
        // `int? Find(`, not `a ? f() : b`.
        Some('?') => before[..before.len() - 1].ends_with(|c: char| word(c) || c == '>'),
        Some(c) if word(c) => !KEYWORDS.contains(&before.rsplit(|c: char| !word(c)).next().unwrap_or("")),
        _ => false,
    }
}

/// Whether `source` defines a function or method called `name`, in C# or Kotlin.
pub fn defines_function(source: &str, name: &str, path: &str) -> bool {
    if !source.contains(name) { return false; }
    let masked = code_only_in(source, path);
    let found = masked.lines().any(|l| {
        let mut offset = 0;
        while let Some(pos) = find_identifier(&l[offset..], name) {
            let at = offset + pos;
            let rest = l[at + name.len()..].trim_start();
            if (rest.starts_with('(') || rest.starts_with('<')) && is_declaration(&l[..at], rest, path) { return true; }
            offset = at + name.len();
        }
        false
    });
    found
}

/// `source` with the contents of string literals and comments replaced by
/// spaces (newlines kept), so positions and line numbers still line up.
/// Handles "…", '…', `…`, triple-quoted strings, //, /* */ and # comments.
/// A `'` after `&`, `<` or a word character is a Rust lifetime, not a quote.
pub fn code_only(source: &str) -> String {
    code_only_in(source, "")
}

/// `code_only` for the file at `path`. In Kotlin `"${x}"` and `"$x"` are code, and in C# `$"{x}"` is.
pub fn code_only_in(source: &str, path: &str) -> String {
    let kt = path.ends_with(".kt") || path.ends_with(".kts");
    let cs = path.ends_with(".cs");
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        // Line comments. A `#` right after a dot is a JS private member (`this.#queue`), not a comment.
        let private_member = c == '#' && i > 0 && chars[i - 1] == '.';
        if (c == '/' && next == Some('/')) || (c == '#' && !private_member && next != Some('[') && next != Some('!') && next != Some('{')) {
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
            // Python f-strings have code inside `{ }` and JavaScript template literals inside `${ }`.
            let prefix: String = chars[..i].iter().rev().take_while(|p| p.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase();
            let fstring = c != '`' && prefix.contains('f') && prefix.len() <= 2 && prefix.chars().all(|p| matches!(p, 'f' | 'r' | 'b'))
                || cs && c == '"' && chars[..i].iter().rev().take_while(|p| matches!(p, '$' | '@')).any(|p| *p == '$');
            let dollar = kt && c == '"';
            for _ in 0..q { out.push(c); }
            i += q;
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() { out.push(' '); out.push(blank(chars[i + 1])); i += 2; continue; }
                let opens = ((c == '`' || dollar) && chars[i] == '$' && chars.get(i + 1) == Some(&'{'))
                    || (fstring && chars[i] == '{' && chars.get(i + 1) != Some(&'{'));
                if dollar && chars[i] == '$' && chars.get(i + 1).is_some_and(|n| n.is_alphabetic() || *n == '_') {
                    out.push('$'); i += 1;
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') { out.push(chars[i]); i += 1; }
                    continue;
                }
                if fstring && chars[i] == '{' && chars.get(i + 1) == Some(&'{') { out.push_str("  "); i += 2; continue; }
                if opens {
                    let start = if chars[i] == '$' { 2 } else { 1 };
                    for k in 0..start { out.push(chars[i + k]); }
                    i = interpolation(&chars, i + start, &mut out);
                    continue;
                }
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

fn blank_char(c: char) -> char {
    if c == '\n' { '\n' } else { ' ' }
}

/// Keeps the code inside an f-string's `{ }` or a template literal's `${ }`, up to and including
/// the closing brace. `i` starts just past the opening brace. Strings inside it are still blanked.
fn interpolation(chars: &[char], mut i: usize, out: &mut String) -> usize {
    let mut depth = 1;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '`' {
            out.push('`');
            i = template(chars, i + 1, out);
            continue;
        }
        // A quote only starts a string if it closes on the same line, so a stray `'` in a regex can't
        // swallow the rest of the code.
        if ch == '"' || ch == '\'' {
            let end = (i + 1..chars.len()).take_while(|&k| chars[k] != '\n').find(|&k| chars[k] == ch && chars[k - 1] != '\\');
            if let Some(end) = end {
                out.push(ch);
                for &c in &chars[i + 1..end] { out.push(blank_char(c)); }
                out.push(ch);
                i = end + 1;
                continue;
            }
        }
        if ch == '{' { depth += 1; }
        if ch == '}' {
            depth -= 1;
            if depth == 0 { out.push('}'); return i + 1; }
        }
        out.push(ch);
        i += 1;
    }
    i
}

/// Blanks the text of a template literal but keeps the code inside `${ }`, up to and including
/// the closing backtick. `i` starts just past the opening backtick.
fn template(chars: &[char], mut i: usize, out: &mut String) -> usize {
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && i + 1 < chars.len() {
            out.push(' ');
            out.push(blank_char(chars[i + 1]));
            i += 2;
            continue;
        }
        if ch == '`' { out.push('`'); return i + 1; }
        if ch == '$' && chars.get(i + 1) == Some(&'{') {
            out.push_str("${");
            i = interpolation(chars, i + 2, out);
            continue;
        }
        out.push(blank_char(ch));
        i += 1;
    }
    i
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
    if head.trim_end().ends_with("super()") { return Some("super"); }
    let start = head.rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).map(|p| p + 1).unwrap_or(0);
    Some(&head[start..])
}

/// Whether a call qualified by `q` can refer to `sig_name` defined in `def_path`.
/// Free functions: unqualified, or qualified by their module / self / crate.
/// Methods: any `.method(` receiver, or `Type::method(` with the right type.
pub fn call_qualifier_ok(q: Option<&str>, def_path: &str, sig_name: &str) -> bool {
    call_qualifier_ok_from(q, def_path, sig_name, def_path)
}

/// `call_qualifier_ok` for a call in the file `caller`. A method's `self.` / `Self::` / `this.` only
/// means its own type in its own file; and on some other value (`agent.request(`) a common name
/// (`request`, `usage`) is most likely another type's method, so it only counts there too, while a
/// distinctive one (`emitJavadoc`, `with_base`) counts anywhere.
pub fn call_qualifier_ok_from(q: Option<&str>, def_path: &str, sig_name: &str, caller: &str) -> bool {
    let own_file = caller == def_path;
    if let (Some(q), Some(_)) = (q, owner_of(sig_name)) {
        if matches!(q, "Self" | "self" | "this") && !own_file { return false; }
        let value = !q.chars().next().is_some_and(|c| c.is_uppercase()) && !matches!(q, "Self" | "self" | "this" | "super");
        if value && !own_file && !distinctive(bare_name(sig_name)) { return false; }
    }
    let Some(q) = q else { return true };
    let owner = sig_name.rsplit_once("::").or_else(|| sig_name.rsplit_once('.')).map(|(o, _)| bare_name(o));
    let stem = def_path.rsplit('/').next().unwrap_or(def_path).split('.').next().unwrap_or("");
    let dir = def_path.rsplit('/').nth(1).unwrap_or("");
    let module = if matches!(stem, "mod" | "index" | "__init__" | "lib") { dir } else { stem };
    // A parent's method (`super().run(`) or a macro's placeholder type (`$t::run(`) is some other function.
    if q == "super" || q.starts_with('$') { return false; }
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
