use crate::diff::{signature_differs, DiffOutput, MatchKind};
use crate::manifest::*;
use crate::parser::{bare_name, ident_hash, Param, SemanticItem, SemanticTree};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Classify matched/unmatched items into a structured ChangeManifest.
/// Entry ids are allocated from `next_id`.
pub fn classify(
    old_tree: &SemanticTree,
    new_tree: &SemanticTree,
    diff_output: &DiffOutput,
    old_source: &str,
    new_source: &str,
    next_id: &mut ManifestEntryId,
) -> ChangeManifest {
    let mut manifest = ChangeManifest::default();
    let mut id = || { let v = *next_id; *next_id += 1; v };

    // Items fully explained by an extraction are not reported as plain adds/removes.
    let extracted_old: HashSet<usize> = diff_output.extractions.iter()
        .filter(|e| e.new_idx.is_none())
        .map(|e| e.old_idx)
        .collect();
    let extracted_new: HashSet<usize> = diff_output.extractions.iter()
        .flat_map(|e| e.extracted.iter().copied())
        .collect();

    for pair in &diff_output.matched {
        let old_item = &old_tree.items[pair.old_idx];
        let new_item = &new_tree.items[pair.new_idx];
        if matches!(new_item, SemanticItem::Import { .. }) {
            continue; // imports are compared per module below
        }
        let shell_changed = old_tree.meta.get(pair.old_idx).map(|m| m.shell_hash) != new_tree.meta.get(pair.new_idx).map(|m| m.shell_hash);

        match pair.match_kind {
            MatchKind::Exact => {}
            MatchKind::FormattingOnly => {
                manifest.formatting_only.push(FormattingEntry {
                    id: id(),
                    location: location_of(new_item),
                    description: format!("{}: formatting/comments only", display_name(new_item, new_source)),
                });
            }
            MatchKind::Rename | MatchKind::RenameModified => {
                if let (Some(old_name), Some(new_name)) = (old_item.name(), new_item.name()) {
                    manifest.renames.push(RenameEntry {
                        id: id(),
                        old_name: old_name.to_string(),
                        new_name: new_name.to_string(),
                        kind: symbol_kind_of(old_item),
                        locations: vec![location_of(new_item)],
                    });
                }
                if pair.match_kind == MatchKind::RenameModified {
                    classify_modified(old_item, new_item, new_source, shell_changed, &mut manifest, &mut id);
                }
            }
            MatchKind::Modified => {
                classify_modified(old_item, new_item, new_source, shell_changed, &mut manifest, &mut id);
            }
        }
    }

    for &ri in &diff_output.removed {
        let item = &old_tree.items[ri];
        if matches!(item, SemanticItem::Import { .. }) || extracted_old.contains(&ri) {
            continue;
        }
        manifest.logic_changes.push(LogicChangeEntry {
            id: id(),
            name: display_name(item, old_source),
            kind: symbol_kind_of(item),
            description: "removed".to_string(),
            location: location_of_old(item),
        });
    }

    for &ai in &diff_output.added {
        let item = &new_tree.items[ai];
        if matches!(item, SemanticItem::Import { .. }) || extracted_new.contains(&ai) {
            continue;
        }
        manifest.logic_changes.push(LogicChangeEntry {
            id: id(),
            name: display_name(item, new_source),
            kind: symbol_kind_of(item),
            description: "added".to_string(),
            location: location_of(item),
        });
    }

    classify_imports(old_tree, new_tree, &mut manifest, &mut id);

    for e in &diff_output.extractions {
        let original = &old_tree.items[e.old_idx];
        manifest.extracted_functions.push(ExtractionEntry {
            id: id(),
            original_name: original.name().unwrap_or("?").to_string(),
            extracted_names: e.extracted.iter()
                .filter_map(|&ni| new_tree.items[ni].name().map(str::to_string))
                .collect(),
            location_original: location_of_old(original),
            locations_new: e.extracted.iter().map(|&ni| location_of(&new_tree.items[ni])).collect(),
        });
    }

    for &mi in &diff_output.moved {
        let pair = &diff_output.matched[mi];
        let (old_item, new_item) = (&old_tree.items[pair.old_idx], &new_tree.items[pair.new_idx]);
        if matches!(new_item, SemanticItem::Import { .. }) {
            continue;
        }
        manifest.moved_code.push(MoveEntry {
            id: id(),
            name: display_name(new_item, new_source),
            kind: symbol_kind_of(new_item),
            from_location: location_of_old(old_item),
            to_location: location_of(new_item),
        });
    }

    detect_dead_code(old_tree, new_tree, diff_output, new_source, &mut manifest, &mut id);

    manifest
}

/// Entries for a same-identity item whose syntax changed.
fn classify_modified(
    old_item: &SemanticItem,
    new_item: &SemanticItem,
    new_source: &str,
    shell_changed: bool,
    manifest: &mut ChangeManifest,
    id: &mut impl FnMut() -> ManifestEntryId,
) {
    match (old_item, new_item) {
        (
            SemanticItem::Function { params: op, return_type: or, body_hash: ob, decl_hash: od, .. },
            SemanticItem::Function { params: np, return_type: nr, body_hash: nb, decl_hash: nd, name, .. },
        ) => {
            let before = manifest.ids().len();
            if let Some(desc) = describe_signature_change(op, np, or, nr) {
                manifest.signature_changes.push(SignatureChangeEntry {
                    id: id(),
                    name: name.clone(),
                    kind: SymbolKind::Function,
                    description: desc,
                    location: location_of(new_item),
                });
            }
            if ob != nb {
                manifest.logic_changes.push(LogicChangeEntry {
                    id: id(),
                    name: name.clone(),
                    kind: SymbolKind::Function,
                    description: "body modified".to_string(),
                    location: location_of(new_item),
                });
            }
            // Visibility, `async`, modifiers, generics, attributes …
            if od != nd && manifest.ids().len() == before {
                manifest.logic_changes.push(LogicChangeEntry {
                    id: id(),
                    name: name.clone(),
                    kind: SymbolKind::Function,
                    description: "declaration modified".to_string(),
                    location: location_of(new_item),
                });
            }
        }
        (
            SemanticItem::Class { methods: om, fields: of, .. },
            SemanticItem::Class { methods: nm, fields: nf, name, .. },
        ) => {
            let before = manifest.ids().len();
            classify_members(name, om, nm, manifest, id);
            let old_fields: BTreeSet<&str> = of.iter().map(|f| f.name.as_str()).collect();
            let new_fields: BTreeSet<&str> = nf.iter().map(|f| f.name.as_str()).collect();
            let added: Vec<&str> = new_fields.difference(&old_fields).copied().collect();
            let removed: Vec<&str> = old_fields.difference(&new_fields).copied().collect();
            let retyped: Vec<&str> = nf.iter()
                .filter(|f| of.iter().any(|o| o.name == f.name && o.type_annotation != f.type_annotation))
                .map(|f| f.name.as_str())
                .collect();
            let members_changed = manifest.ids().len() > before;
            let mut parts = Vec::new();
            if !added.is_empty() { parts.push(format!("added field{} {}", pl(added.len()), ticks(&added))); }
            if !removed.is_empty() { parts.push(format!("removed field{} {}", pl(removed.len()), ticks(&removed))); }
            if !retyped.is_empty() { parts.push(format!("changed type of {}", ticks(&retyped))); }
            if !parts.is_empty() {
                manifest.logic_changes.push(LogicChangeEntry {
                    id: id(),
                    name: name.clone(),
                    kind: SymbolKind::Class,
                    description: parts.join(", "),
                    location: location_of(new_item),
                });
            }
            // Same methods and nothing else changed, just their order.
            if manifest.ids().len() == before && !shell_changed && parts.is_empty() && om.len() == nm.len() && om.len() > 1 {
                manifest.formatting_only.push(FormattingEntry {
                    id: id(),
                    location: location_of(new_item),
                    description: format!("{name}: methods reordered, otherwise unchanged"),
                });
                return;
            }
            // Something changed outside the methods and fields, like the extends clause, decorators or a nested class.
            if manifest.ids().len() == before || (shell_changed && parts.is_empty()) {
                manifest.logic_changes.push(LogicChangeEntry {
                    id: id(),
                    name: name.clone(),
                    kind: SymbolKind::Class,
                    description: if members_changed { "changed outside its methods" } else { "declaration modified" }.to_string(),
                    location: location_of(new_item),
                });
            }
        }
        _ => {
            let description = match new_item {
                SemanticItem::TypeDef { .. } => "type definition changed",
                SemanticItem::Variable { .. } => "value changed",
                _ => "modified",
            };
            manifest.logic_changes.push(LogicChangeEntry {
                id: id(),
                name: display_name(new_item, new_source),
                kind: symbol_kind_of(new_item),
                description: description.to_string(),
                location: location_of(new_item),
            });
        }
    }
}

/// Method-level changes inside a class, reported as `Class.method`.
fn classify_members(
    class: &str,
    old: &[SemanticItem],
    new: &[SemanticItem],
    manifest: &mut ChangeManifest,
    id: &mut impl FnMut() -> ManifestEntryId,
) {
    let qualified = |m: &SemanticItem| format!("{class}.{}", m.name().unwrap_or("?"));
    let mut new_used = vec![false; new.len()];
    let mut old_unmatched = Vec::new();
    for o in old {
        match new.iter().enumerate().position(|(i, n)| !new_used[i] && n.name() == o.name()) {
            Some(i) => {
                new_used[i] = true;
                classify_modified(o, &new[i], "", false, manifest, id);
                // classify_modified names entries by bare method name; qualify them.
                qualify_last(manifest, &new[i], &qualified(&new[i]));
            }
            None => old_unmatched.push(o),
        }
    }
    let mut new_unmatched: Vec<&SemanticItem> = new.iter().enumerate().filter(|(i, _)| !new_used[*i]).map(|(_, n)| n).collect();
    for o in old_unmatched {
        // Renamed method: identical body and signature under a new name, and the
        // only such candidate on both sides (trivial bodies like `{}` repeat).
        let same = |a: &SemanticItem, b: &SemanticItem| {
            a.body_hash().is_some() && a.body_hash() == b.body_hash() && !signature_differs(a, b)
        };
        let candidates: Vec<usize> = new_unmatched.iter().enumerate()
            .filter(|(_, n)| same(o, n))
            .map(|(i, _)| i)
            .collect();
        let rivals = old.iter().filter(|x| !std::ptr::eq(*x, o) && same(x, o) && !new.iter().any(|n| n.name() == x.name())).count();
        let renamed = (candidates.len() == 1 && rivals == 0).then(|| candidates[0]);
        if let Some(pos) = renamed {
            let n = new_unmatched.remove(pos);
            manifest.renames.push(RenameEntry {
                id: id(),
                old_name: qualified(o),
                new_name: qualified(n),
                kind: SymbolKind::Method,
                locations: vec![location_of(n)],
            });
            continue;
        }
        manifest.logic_changes.push(LogicChangeEntry {
            id: id(),
            name: qualified(o),
            kind: SymbolKind::Method,
            description: "removed".to_string(),
            location: location_of_old(o),
        });
    }
    for n in new_unmatched {
        manifest.logic_changes.push(LogicChangeEntry {
            id: id(),
            name: qualified(n),
            kind: SymbolKind::Method,
            description: "added".to_string(),
            location: location_of(n),
        });
    }
}

/// Rename entries just produced for a method to its qualified name.
fn qualify_last(manifest: &mut ChangeManifest, item: &SemanticItem, qualified: &str) {
    let line = item.span().start_line;
    for s in manifest.signature_changes.iter_mut().filter(|s| s.location.line_start == line) {
        s.name = qualified.to_string();
        s.kind = SymbolKind::Method;
    }
    for l in manifest.logic_changes.iter_mut().filter(|l| l.location.line_start == line && l.location.side == Side::New) {
        l.name = qualified.to_string();
        l.kind = SymbolKind::Method;
    }
}

/// Compare imports per module and per symbol, independent of statement order.
fn classify_imports(
    old_tree: &SemanticTree,
    new_tree: &SemanticTree,
    manifest: &mut ChangeManifest,
    id: &mut impl FnMut() -> ManifestEntryId,
) {
    fn collect(tree: &SemanticTree) -> BTreeMap<String, (BTreeSet<String>, Location)> {
        let mut map: BTreeMap<String, (BTreeSet<String>, Location)> = BTreeMap::new();
        for item in &tree.items {
            let SemanticItem::Import { source, symbols, span } = item else { continue };
            let loc = Location::new(span.start_line, span.end_line);
            if source.is_empty() {
                // Grouped import where every symbol is its own module (Go).
                for s in symbols {
                    let module = s.rsplit(' ').next().unwrap_or(s).to_string();
                    map.entry(module).or_insert_with(|| (BTreeSet::new(), loc.clone()));
                }
            } else {
                let entry = map.entry(source.clone()).or_insert_with(|| (BTreeSet::new(), loc.clone()));
                entry.0.extend(symbols.iter().cloned());
            }
        }
        map
    }
    let old = collect(old_tree);
    let new = collect(new_tree);

    for (module, (new_syms, loc)) in &new {
        match old.get(module) {
            None => manifest.dependency_changes.push(DependencyChange {
                id: id(),
                change_type: DependencyChangeType::Added,
                name: module.clone(),
                used_in: vec![loc.clone()],
                symbols_added: new_syms.iter().cloned().collect(),
                symbols_removed: vec![],
                internal: is_internal_module(module),
            }),
            Some((old_syms, _)) if old_syms != new_syms => manifest.dependency_changes.push(DependencyChange {
                id: id(),
                change_type: DependencyChangeType::Changed,
                name: module.clone(),
                used_in: vec![loc.clone()],
                symbols_added: new_syms.difference(old_syms).cloned().collect(),
                symbols_removed: old_syms.difference(new_syms).cloned().collect(),
                internal: is_internal_module(module),
            }),
            Some(_) => {}
        }
    }
    for (module, (old_syms, loc)) in &old {
        if !new.contains_key(module) {
            let mut loc = loc.clone();
            loc.side = Side::Old;
            manifest.dependency_changes.push(DependencyChange {
                id: id(),
                change_type: DependencyChangeType::Removed,
                name: module.clone(),
                used_in: vec![loc],
                symbols_added: vec![],
                symbols_removed: old_syms.iter().cloned().collect(),
                internal: is_internal_module(module),
            });
        }
    }
}

/// Relative or project-internal import (not an external package).
fn is_internal_module(module: &str) -> bool {
    module.starts_with('.')
        || module.starts_with('/')
        || module.starts_with("@/")
        || module.starts_with("~/")
        || module == "crate" || module.starts_with("crate::")
        || module == "super" || module.starts_with("super::")
        || module == "self" || module.starts_with("self::")
}

/// Dead code: non-exported items that nothing else references *because of this diff*.
/// Deliberately conservative: a missed detection is cheaper than a false alarm.
fn detect_dead_code(
    old_tree: &SemanticTree,
    new_tree: &SemanticTree,
    diff_output: &DiffOutput,
    new_source: &str,
    manifest: &mut ChangeManifest,
    id: &mut impl FnMut() -> ManifestEntryId,
) {
    let new_lines: Vec<&str> = new_source.lines().collect();
    if new_tree.meta.len() != new_tree.items.len() || old_tree.meta.len() != old_tree.items.len() {
        return;
    }
    let touched_old: Vec<usize> = diff_output.removed.iter().copied()
        .chain(diff_output.matched.iter().filter(|p| p.match_kind != MatchKind::Exact).map(|p| p.old_idx))
        .collect();
    let added: HashSet<usize> = diff_output.added.iter().copied().collect();
    let modified_new: HashSet<usize> = diff_output.matched.iter()
        .filter(|p| p.match_kind != MatchKind::Exact)
        .map(|p| p.new_idx)
        .collect();

    for (ni, item) in new_tree.items.iter().enumerate() {
        if !matches!(item, SemanticItem::Function { .. } | SemanticItem::Variable { .. } | SemanticItem::TypeDef { .. } | SemanticItem::Class { .. }) {
            continue;
        }
        let Some(name) = item.name() else { continue };
        let bare = bare_name(name);
        // Methods (`Type::method`) are reached through values; skip. Single chars are too noisy.
        if bare.len() < 2 || bare != name || new_tree.meta[ni].exported || modified_new.contains(&ni) {
            continue;
        }
        // `const { a, b: c } = require(…)` binds a and c: it's used if either is.
        let bound = bound_names(bare);
        let span = item.span();
        let used = |name: &str| {
            let h = ident_hash(name);
            new_tree.meta.iter().enumerate().any(|(i, m)| i != ni && m.refs.contains(&h))
                // Also honor textual mentions outside the item (HTML handlers in template
                // strings, reflection, string-based registration), so err on the side of silence.
                || new_lines.iter().enumerate()
                    .filter(|(i, _)| *i + 1 < span.start_line || *i + 1 > span.end_line)
                    .any(|(_, l)| contains_identifier(l, name))
        };
        if bound.iter().any(|n| used(n)) {
            continue;
        }
        let h = ident_hash(bare);
        let is_new = added.contains(&ni);
        let lost_callers = !is_new && touched_old.iter().any(|&oi| {
            old_tree.items[oi].name().map(bare_name) != Some(bare) && old_tree.meta[oi].refs.contains(&h)
        });
        if is_new || lost_callers {
            manifest.dead_code.push(DeadCodeEntry {
                id: id(),
                name: name.to_string(),
                kind: symbol_kind_of(item),
                location: location_of(item),
                reason: if is_new {
                    "added but never referenced".to_string()
                } else {
                    "no remaining references after this change".to_string()
                },
            });
        }
    }
}

/// A parameter that takes what's left over, by the language's syntax: Some(true) for extra keyword
/// arguments (Python's `**kwargs`), Some(false) for extra positional ones (`*args`, `...rest`, Go's
/// `args ...T`, C's and Java's `...`). In C, Rust and Go a leading `*` is a pointer, not this.
pub fn catch_all(p: &Param, path: &str) -> Option<bool> {
    let ext = path.rsplit('.').next().unwrap_or("");
    let ty = p.type_annotation.as_deref().unwrap_or("").trim_start();
    match ext {
        "py" if p.name.starts_with("**") => Some(true),
        "py" if p.name.starts_with('*') => Some(false),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" if p.name.starts_with("...") => Some(false),
        "go" if ty.starts_with("...") => Some(false),
        "c" | "h" | "java" | "scala" if p.name == "..." || ty.ends_with("...") || ty.ends_with('*') && ty.trim_end_matches('*').ends_with(':') => Some(false),
        _ => None,
    }
}

/// The names a declaration binds: `x` → [x]; `{ a, b: c, ...d }` / `[a, b]` → [a, c, d].
pub(crate) fn bound_names(name: &str) -> Vec<&str> {
    let t = name.trim();
    if !(t.starts_with('{') || t.starts_with('[') || t.starts_with('(')) { return vec![t]; }
    t.trim_matches(|c| matches!(c, '{' | '}' | '[' | ']' | '(' | ')'))
        .split(',')
        .filter_map(|part| {
            let local = part.rsplit(':').next().unwrap_or(part).split('=').next().unwrap_or("").trim().trim_start_matches("...");
            (!local.is_empty()).then_some(local)
        })
        .collect()
}

/// Check if `source` contains `name` as a standalone identifier (word boundary match).
pub fn contains_identifier(source: &str, name: &str) -> bool {
    find_identifier(source, name).is_some()
}

/// Byte offset of the first standalone occurrence of `name` in `source`.
pub fn find_identifier(source: &str, name: &str) -> Option<usize> {
    if name.is_empty() {
        return None;
    }
    let bytes = source.as_bytes();
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    for (i, _) in source.match_indices(name) {
        let before_ok = i == 0 || !is_ident(bytes[i - 1]);
        let after = i + name.len();
        let after_ok = after >= bytes.len() || !is_ident(bytes[after]);
        if before_ok && after_ok {
            return Some(i);
        }
    }
    None
}

/// Human-facing name: the item's name, or a snippet of its first line.
fn display_name(item: &SemanticItem, source: &str) -> String {
    if let Some(n) = item.name() {
        return n.to_string();
    }
    let line = source.lines().nth(item.span().start_line.saturating_sub(1)).unwrap_or("").trim();
    let mut snippet: String = line.chars().take(48).collect();
    if line.chars().count() > 48 {
        snippet.push('…');
    }
    if snippet.is_empty() { format!("line {}", item.span().start_line) } else { snippet }
}

fn symbol_kind_of(item: &SemanticItem) -> SymbolKind {
    match item {
        SemanticItem::Function { name, .. } if name.contains("::") || name.contains('.') => SymbolKind::Method,
        SemanticItem::Function { .. } => SymbolKind::Function,
        SemanticItem::Class { .. } => SymbolKind::Class,
        SemanticItem::Import { .. } => SymbolKind::Import,
        SemanticItem::Variable { .. } => SymbolKind::Variable,
        SemanticItem::TypeDef { .. } => SymbolKind::Type,
        SemanticItem::Other { .. } => SymbolKind::Module,
    }
}

fn location_of(item: &SemanticItem) -> Location {
    let span = item.span();
    Location::new(span.start_line, span.end_line)
}

fn location_of_old(item: &SemanticItem) -> Location {
    let span = item.span();
    Location::old(span.start_line, span.end_line)
}

fn pl(n: usize) -> &'static str { if n == 1 { "" } else { "s" } }

fn ticks(names: &[&str]) -> String {
    names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ")
}

/// Collapse whitespace and cap the length, for code quoted in a description.
fn one_line(text: &str, max: usize) -> String {
    let t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > max { format!("{}…", t.chars().take(max - 1).collect::<String>()) } else { t }
}

/// Field names of a destructuring pattern (`{ a, b: c, d = 1 }` → a, b, d).
fn pattern_fields(pattern: &str) -> Option<Vec<String>> {
    let p = pattern.trim();
    let inner = p.strip_prefix('{')?.strip_suffix('}')?;
    let mut fields = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in inner.chars().chain(std::iter::once(',')) {
        match c {
            '{' | '[' | '(' => { depth += 1; cur.push(c); }
            '}' | ']' | ')' => { depth -= 1; cur.push(c); }
            ',' if depth == 0 => {
                let name: String = cur.trim().trim_start_matches("...").chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
                if !name.is_empty() { fields.push(name); }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    Some(fields)
}

/// Whether existing calls need updating: a new required param, or a removed,
/// reordered or newly required one. A changed parameter *type* is reported as a
/// signature change but doesn't by itself mean callers must change (widening
/// `NoUndefined<T>` to `T` breaks nobody), so it raises no call sites.
pub(crate) fn breaks_callers(old: &[Param], new: &[Param], path: &str) -> bool {
    // `*args`, `**kwargs`, `...rest` take what's left over: where they sit doesn't matter, only
    // whether they're still there (dropping one breaks the callers that relied on it).
    let is_rest = |p: &&Param| catch_all(p, path).is_some();
    let kinds = |ps: &[Param]| (ps.iter().any(|p| catch_all(p, path) == Some(true)), ps.iter().any(|p| catch_all(p, path) == Some(false)));
    let ((old_kw, old_pos), (new_kw, new_pos)) = (kinds(old), kinds(new));
    if (old_kw && !new_kw) || (old_pos && !new_pos) { return true; }
    let old: Vec<Param> = old.iter().filter(|p| !is_rest(p)).cloned().collect();
    let new: Vec<Param> = new.iter().filter(|p| !is_rest(p)).cloned().collect();
    let (old, new) = (old.as_slice(), new.as_slice());
    for (i, p) in new.iter().enumerate() {
        match old.iter().position(|o| o.name == p.name) {
            None => {
                // A changed destructuring pattern at the same position: breaking if fields were removed.
                if let (Some(o), Some(nf)) = (old.get(i), pattern_fields(&p.name)) {
                    if let Some(of) = pattern_fields(&o.name) {
                        if of.iter().any(|f| !nf.contains(f)) { return true; }
                        continue;
                    }
                }
                if !p.optional { return true; }
            }
            Some(j) => {
                if j != i || (old[j].optional && !p.optional) {
                    return true;
                }
            }
        }
    }
    // A removed param breaks callers, unless it's a destructuring pattern
    // replaced in place (its fields were compared above).
    old.iter().enumerate().any(|(i, o)| {
        let gone = !new.iter().any(|n| n.name == o.name);
        let pattern_replaced = pattern_fields(&o.name).is_some() && new.get(i).is_some_and(|n| pattern_fields(&n.name).is_some());
        gone && !pattern_replaced
    })
}

fn describe_signature_change(
    old_params: &[Param],
    new_params: &[Param],
    old_ret: &Option<String>,
    new_ret: &Option<String>,
) -> Option<String> {
    let clean = |t: &Option<String>| t.as_deref().map(|t| one_line(t.trim_start_matches(':').trim(), 48));
    let mut parts = Vec::new();

    // Destructured object/array params (`{ a, b }: Opts`) at the same position:
    // describe the fields, not the whole pattern.
    let mut paired: Vec<(usize, usize)> = Vec::new();
    for (i, np) in new_params.iter().enumerate() {
        let Some(op) = old_params.get(i) else { continue };
        if op.name == np.name { continue; }
        if let (Some(of), Some(nf)) = (pattern_fields(&op.name), pattern_fields(&np.name)) {
            let added: Vec<&String> = nf.iter().filter(|f| !of.contains(f)).collect();
            let removed: Vec<&String> = of.iter().filter(|f| !nf.contains(f)).collect();
            let fmt = |v: &[&String]| v.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ");
            let mut d = Vec::new();
            if !added.is_empty() { d.push(format!("added field{} {}", if added.len() == 1 { "" } else { "s" }, fmt(&added))); }
            if !removed.is_empty() { d.push(format!("removed field{} {}", if removed.len() == 1 { "" } else { "s" }, fmt(&removed))); }
            if !d.is_empty() { parts.push(format!("destructured param: {}", d.join(", "))); }
            paired.push((i, i));
        }
    }
    let is_paired_new = |i: usize| paired.iter().any(|&(_, n)| n == i);
    let is_paired_old = |i: usize| paired.iter().any(|&(o, _)| o == i);

    for (i, np) in new_params.iter().enumerate() {
        if !is_paired_new(i) && !old_params.iter().any(|op| op.name == np.name) {
            let ty = clean(&np.type_annotation).map(|t| format!(": {t}")).unwrap_or_default();
            let opt = if np.optional { "optional " } else { "" };
            parts.push(format!("added {opt}param `{}{}`", one_line(&np.name, 40), ty));
        }
    }
    for (i, op) in old_params.iter().enumerate() {
        if !is_paired_old(i) && !new_params.iter().any(|np| np.name == op.name) {
            parts.push(format!("removed param `{}`", one_line(&op.name, 40)));
        }
    }
    for np in new_params {
        if let Some(op) = old_params.iter().find(|op| op.name == np.name) {
            if clean(&op.type_annotation) != clean(&np.type_annotation) {
                parts.push(format!(
                    "`{}`: {} → {}",
                    one_line(&np.name, 40),
                    clean(&op.type_annotation).unwrap_or_else(|| "untyped".into()),
                    clean(&np.type_annotation).unwrap_or_else(|| "untyped".into()),
                ));
            }
        }
    }
    let old_order: Vec<&str> = old_params.iter().map(|p| p.name.as_str()).filter(|n| new_params.iter().any(|p| p.name == *n)).collect();
    let new_order: Vec<&str> = new_params.iter().map(|p| p.name.as_str()).filter(|n| old_params.iter().any(|p| p.name == *n)).collect();
    if old_order != new_order {
        parts.push("params reordered".to_string());
    }

    let (or, nr) = (clean(old_ret), clean(new_ret));
    if or != nr {
        match (or, nr) {
            (None, Some(t)) => parts.push(format!("added return type `{t}`")),
            (Some(_), None) => parts.push("removed return type".to_string()),
            (Some(a), Some(b)) => parts.push(format!("return type `{a}` → `{b}`")),
            _ => {}
        }
    }

    if parts.is_empty() { None } else { Some(parts.join(", ")) }
}
