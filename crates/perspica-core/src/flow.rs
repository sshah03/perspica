//! Call relationships between the items of the changed files, derived from
//! identifier references (no type information, so edges are by name).
//!
//! Two uses:
//! - a **reading order**: changed code walked from entry points down to the
//!   changed functions they reach, the order a reviewer should read it in;
//! - **test reach**: which changed functions the changed tests exercise,
//!   directly or through other functions.

use crate::cross_file::InternalAnalysis;
use crate::manifest::{Location, ManifestEntryId, Side};
use crate::parser::{bare_name, ident_hash, SemanticItem};
use crate::roles::FileRole;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

/// Paths through unchanged code are followed this far when linking changed items.
const MAX_HOPS: usize = 3;
/// Test reach is followed this far from a test.
const MAX_TEST_DEPTH: usize = 6;
/// Shorter names aren't linked at all.
const MIN_NAME_LEN: usize = 3;
/// Names defined in more places than this are too ambiguous to link by name.
const MAX_DEFS_PER_NAME: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Types and data definitions. Read first: they're what the code manipulates.
    Type,
    /// Functions and methods, walked from entry points down their calls.
    Function,
    /// Constants, variables, top-level statements.
    Other,
}

/// One changed item in the reading order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadingStep {
    pub name: String,
    pub file: String,
    pub kind: StepKind,
    /// Nesting under the step that calls it (0 = entry point).
    pub depth: usize,
    /// Manifest entries describing this item's changes.
    pub entry_ids: Vec<ManifestEntryId>,
    /// Changed items that call this one (directly or through unchanged code).
    pub called_by: Vec<String>,
    /// Changed items this one calls, in the order they first appear in its body.
    pub calls: Vec<String>,
    /// Already shown earlier in the order; listed again only to show the call.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub repeat: bool,
}

/// How the changed tests reach a changed function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReach {
    pub entry_id: ManifestEntryId,
    pub name: String,
    pub file: String,
    /// Test → … → function, shortest found. Empty when no changed test reaches it.
    pub via: Vec<String>,
    /// Changed tests that reach it (up to 3), as `file` + test name.
    pub tests: Vec<TestRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRef {
    pub file: String,
    pub name: String,
    pub line: usize,
}

struct Node<'a> {
    file: usize,
    name: String,
    kind: StepKind,
    span: (usize, usize),
    refs: &'a HashSet<u64>,
    is_test: bool,
    /// Top-level unnamed statement (e.g. a `describe(...)` block in a test file).
    unnamed: bool,
    source_text: &'a str,
}

pub(crate) struct Graph<'a> {
    nodes: Vec<Node<'a>>,
    /// node → nodes it references by name.
    out: Vec<Vec<usize>>,
    /// node → changed-entry ids located in it.
    entries: Vec<Vec<ManifestEntryId>>,
}

impl<'a> Graph<'a> {
    pub(crate) fn build(analyses: &'a [InternalAnalysis]) -> Graph<'a> {
        let mut nodes = Vec::new();
        for (fi, a) in analyses.iter().enumerate() {
            let role = a.result.review.role;
            if !a.result.review.parsed {
                continue;
            }
            for (item, meta) in a.new_tree.items.iter().zip(&a.new_tree.meta) {
                let text = span_text(&a.new_source, item.span().start_line, item.span().end_line);
                let is_test = meta.is_test || role == FileRole::Test;
                match item {
                    SemanticItem::Import { .. } => {}
                    SemanticItem::Class { name, methods, span, .. } => {
                        nodes.push(Node { file: fi, name: name.clone(), kind: StepKind::Type, span: (span.start_line, span.end_line), refs: &meta.refs, is_test, unnamed: false, source_text: text });
                        for (k, m) in methods.iter().enumerate() {
                            let (Some(mname), Some(refs)) = (m.name(), meta.method_refs.get(k)) else { continue };
                            let s = m.span();
                            nodes.push(Node {
                                file: fi,
                                name: format!("{name}.{mname}"),
                                kind: StepKind::Function,
                                span: (s.start_line, s.end_line),
                                refs,
                                is_test,
                                unnamed: false,
                                source_text: span_text(&a.new_source, s.start_line, s.end_line),
                            });
                        }
                    }
                    other => {
                        let s = other.span();
                        nodes.push(Node {
                            file: fi,
                            name: other.name().map(str::to_string).unwrap_or_default(),
                            kind: match other {
                                SemanticItem::Function { .. } => StepKind::Function,
                                SemanticItem::TypeDef { .. } => StepKind::Type,
                                _ => StepKind::Other,
                            },
                            span: (s.start_line, s.end_line),
                            refs: &meta.refs,
                            is_test,
                            unnamed: other.name().is_none(),
                            source_text: text,
                        });
                    }
                }
            }
        }

        let mut by_name: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            // One- and two-letter names (`p`, `ok`) collide with locals everywhere.
            if !n.unnamed && !n.is_test && bare_name(&n.name).chars().count() >= MIN_NAME_LEN {
                by_name.entry(ident_hash(bare_name(&n.name))).or_default().push(i);
            }
        }
        // A function in another file is only reachable if the caller's file names
        // that file's module (an import, `mod`, `crate::x::`, `from './x'` …).
        let module_of: Vec<String> = analyses.iter().map(|a| module_name(&a.path)).collect();
        let sees = |caller: usize, callee: usize| -> bool {
            caller == callee || {
                let m = &module_of[callee];
                !m.is_empty() && crate::classify::contains_identifier(&analyses[caller].new_source, m)
            }
        };
        // The directory a file is in, for telling same-named methods apart.
        let dir_of = |file: usize| analyses[file].path.rsplit('/').nth(1).unwrap_or("");
        let out: Vec<Vec<usize>> = nodes.iter().enumerate().map(|(i, n)| {
            let mut targets: Vec<usize> = n.refs.iter()
                .filter_map(|h| by_name.get(h))
                .filter(|defs| defs.len() <= MAX_DEFS_PER_NAME)
                .flat_map(|defs| {
                    // The same name on several types (`JsonReader.read`, `JsonTreeReader.read`):
                    // keep the ones whose type the caller names, then the ones in its directory.
                    let mut cands: Vec<usize> = defs.clone();
                    if cands.len() > 1 {
                        let owned: Vec<usize> = cands.iter().copied()
                            .filter(|&d| crate::cross_file::owner_of(&nodes[d].name).is_some_and(|o| n.refs.contains(&ident_hash(o))))
                            .collect();
                        if !owned.is_empty() && owned.len() < cands.len() { cands = owned; }
                    }
                    if cands.len() > 1 {
                        let near: Vec<usize> = cands.iter().copied().filter(|&d| dir_of(nodes[d].file) == dir_of(n.file)).collect();
                        if !near.is_empty() && near.len() < cands.len() { cands = near; }
                    }
                    prefer_local(&cands, &nodes, n.file)
                })
                .filter(|&t| t != i && !contains(&nodes[i], &nodes[t]) && sees(n.file, nodes[t].file))
                .collect();
            targets.sort_unstable();
            targets.dedup();
            targets
        }).collect();

        // Entries → the most specific node containing their new-side location.
        let mut entries = vec![Vec::new(); nodes.len()];
        for (fi, a) in analyses.iter().enumerate() {
            for (id, loc) in changed_locations(a) {
                let best = nodes.iter().enumerate()
                    .filter(|(_, n)| n.file == fi && n.span.0 <= loc.line_start && loc.line_start <= n.span.1)
                    .min_by_key(|(_, n)| n.span.1 - n.span.0)
                    .map(|(i, _)| i);
                if let Some(i) = best {
                    if !entries[i].contains(&id) {
                        entries[i].push(id);
                    }
                }
            }
        }
        Graph { nodes, out, entries }
    }

    fn changed(&self, i: usize) -> bool {
        !self.entries[i].is_empty()
    }

    /// Changed functions reachable from `i` through unchanged functions only,
    /// ordered by where the call leading to each first appears in `i`.
    fn changed_callees(&self, i: usize) -> Vec<usize> {
        let mut found: Vec<(usize, usize)> = Vec::new(); // (callee, first hop)
        let mut seen = HashSet::from([i]);
        let mut queue = VecDeque::from([(i, 0, usize::MAX)]);
        while let Some((n, d, hop)) = queue.pop_front() {
            if d >= MAX_HOPS {
                continue;
            }
            for &t in &self.out[n] {
                let node = &self.nodes[t];
                if node.kind != StepKind::Function || node.is_test || !seen.insert(t) {
                    continue;
                }
                let hop = if d == 0 { t } else { hop };
                if self.changed(t) {
                    found.push((t, hop));
                } else {
                    queue.push_back((t, d + 1, hop));
                }
            }
        }
        let text = self.nodes[i].source_text;
        let pos = |hop: usize| crate::classify::find_identifier(text, bare_name(&self.nodes[hop].name)).unwrap_or(usize::MAX);
        found.sort_by_key(|&(t, hop)| (pos(hop), self.nodes[t].file, self.nodes[t].span.0));
        found.into_iter().map(|(t, _)| t).collect()
    }

    pub(crate) fn reading_order(&self, analyses: &[InternalAnalysis]) -> Vec<ReadingStep> {
        let changed: Vec<usize> = (0..self.nodes.len()).filter(|&i| self.changed(i) && !self.nodes[i].is_test).collect();
        let (types, rest): (Vec<usize>, Vec<usize>) = changed.iter().partition(|&&i| self.nodes[i].kind == StepKind::Type);
        let (functions, others): (Vec<usize>, Vec<usize>) = rest.iter().partition(|&&i| self.nodes[i].kind == StepKind::Function);
        let changed = functions;
        let callees: HashMap<usize, Vec<usize>> = changed.iter().map(|&i| (i, self.changed_callees(i))).collect();
        let mut callers: HashMap<usize, Vec<usize>> = HashMap::new();
        for (&from, tos) in &callees {
            for &t in tos {
                callers.entry(t).or_default().push(from);
            }
        }
        for v in callers.values_mut() {
            v.sort_unstable();
        }
        let reach = |i: usize| -> usize {
            let mut seen = HashSet::from([i]);
            let mut stack = vec![i];
            while let Some(n) = stack.pop() {
                for &t in callees.get(&n).map(Vec::as_slice).unwrap_or(&[]) {
                    if seen.insert(t) { stack.push(t); }
                }
            }
            seen.len()
        };
        // Entry points: nothing changed calls them. Bigger flows first, then file order.
        let mut roots: Vec<usize> = changed.iter().copied().filter(|i| !callers.contains_key(i)).collect();
        roots.sort_by_key(|&i| (std::cmp::Reverse(reach(i)), self.nodes[i].file, self.nodes[i].span.0));

        let flat = |i: usize, kind: StepKind| ReadingStep {
            name: self.nodes[i].name.clone(),
            file: analyses[self.nodes[i].file].path.clone(),
            kind,
            depth: 0,
            entry_ids: self.entries[i].clone(),
            called_by: vec![],
            calls: vec![],
            repeat: false,
        };
        let mut steps: Vec<ReadingStep> = types.iter().map(|&i| flat(i, StepKind::Type)).collect();
        let mut shown = HashSet::new();
        let name_of = |i: usize| self.nodes[i].name.clone();
        let visit = |start: usize, steps: &mut Vec<ReadingStep>, shown: &mut HashSet<usize>| {
            let mut stack = vec![(start, 0usize)];
            while let Some((i, depth)) = stack.pop() {
                let repeat = !shown.insert(i);
                let calls = callees.get(&i).cloned().unwrap_or_default();
                steps.push(ReadingStep {
                    name: name_of(i),
                    file: analyses[self.nodes[i].file].path.clone(),
                    kind: StepKind::Function,
                    depth,
                    entry_ids: self.entries[i].clone(),
                    called_by: callers.get(&i).map(|v| v.iter().map(|&c| name_of(c)).collect()).unwrap_or_default(),
                    calls: calls.iter().map(|&c| name_of(c)).collect(),
                    repeat,
                });
                if !repeat {
                    for &c in calls.iter().rev() {
                        stack.push((c, depth + 1));
                    }
                }
            }
        };
        for r in roots {
            if !shown.contains(&r) {
                visit(r, &mut steps, &mut shown);
            }
        }
        // Cycles with no entry point.
        for &i in &changed {
            if !shown.contains(&i) {
                visit(i, &mut steps, &mut shown);
            }
        }
        steps.extend(others.iter().map(|&i| flat(i, StepKind::Other)));
        steps
    }

    /// For each changed non-test function, the shortest path from a changed test.
    pub(crate) fn test_reach(&self, analyses: &[InternalAnalysis]) -> Vec<TestReach> {
        const MAX_TESTS: usize = 3;
        let tests: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| self.nodes[i].is_test && self.changed(i))
            .collect();
        // BFS from every changed test at once; remember each node's parent and origin test.
        let mut parent: HashMap<usize, usize> = HashMap::new();
        let mut origin: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut depth: HashMap<usize, usize> = HashMap::new();
        let mut queue = VecDeque::new();
        for &t in &tests {
            depth.insert(t, 0);
            origin.insert(t, vec![t]);
            queue.push_back(t);
        }
        while let Some(n) = queue.pop_front() {
            let d = depth[&n];
            if d >= MAX_TEST_DEPTH {
                continue;
            }
            let from = origin[&n].clone();
            for &t in &self.out[n] {
                if self.nodes[t].is_test {
                    continue;
                }
                match depth.get(&t) {
                    None => {
                        depth.insert(t, d + 1);
                        parent.insert(t, n);
                        origin.insert(t, from.clone());
                        queue.push_back(t);
                    }
                    // Same distance from another test: record it as another origin.
                    Some(&dt) if dt == d + 1 => {
                        let o = origin.get_mut(&t).unwrap();
                        for f in &from {
                            if o.len() < MAX_TESTS && !o.contains(f) { o.push(*f); }
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut out = Vec::new();
        for i in 0..self.nodes.len() {
            let n = &self.nodes[i];
            if n.is_test || n.unnamed || !self.changed(i) {
                continue;
            }
            let a = &analyses[n.file];
            if a.result.review.role != FileRole::Source {
                continue;
            }
            // Only functions/methods: the thing a test exercises.
            let Some(entry_id) = self.entries[i].iter().copied().find(|id| is_function_entry(a, *id)) else { continue };
            let (via, tests) = if depth.contains_key(&i) {
                let label = |node: &Node| if node.unnamed { unnamed_label(node.source_text) } else { node.name.clone() };
                let mut path = vec![label(n)];
                let mut cur = i;
                while let Some(&p) = parent.get(&cur) {
                    path.push(label(&self.nodes[p]));
                    cur = p;
                }
                path.reverse();
                let tests = origin[&i].iter().map(|&t| {
                    let tn = &self.nodes[t];
                    TestRef {
                        file: analyses[tn.file].path.clone(),
                        name: if tn.unnamed { first_line(tn.source_text) } else { tn.name.clone() },
                        line: tn.span.0,
                    }
                }).collect();
                (path, tests)
            } else {
                (vec![], vec![])
            };
            out.push(TestReach { entry_id, name: n.name.clone(), file: a.path.clone(), via, tests });
        }
        out
    }
}

/// The name other files use for this one: its stem, or the directory for
/// `index` / `mod` / `__init__` / `lib` files.
fn module_name(path: &str) -> String {
    let mut parts = path.rsplit('/');
    let file = parts.next().unwrap_or("");
    let stem = file.split('.').next().unwrap_or("");
    if matches!(stem, "index" | "mod" | "__init__" | "lib" | "main") {
        parts.next().unwrap_or(stem).to_string()
    } else {
        stem.to_string()
    }
}

/// Prefer definitions in the caller's own file when a name is defined in several.
fn prefer_local(defs: &[usize], nodes: &[Node], file: usize) -> Vec<usize> {
    let local: Vec<usize> = defs.iter().copied().filter(|&d| nodes[d].file == file).collect();
    if local.is_empty() { defs.to_vec() } else { local }
}

/// `inner` is nested inside `outer` (a method inside its class).
fn contains(outer: &Node, inner: &Node) -> bool {
    outer.file == inner.file && outer.span.0 <= inner.span.0 && inner.span.1 <= outer.span.1
}

fn span_text(source: &str, start: usize, end: usize) -> &str {
    let mut offset = 0;
    let mut begin = None;
    for (n, line) in source.split_inclusive('\n').enumerate() {
        let ln = n + 1;
        if ln == start {
            begin = Some(offset);
        }
        offset += line.len();
        if ln == end {
            return begin.map_or("", |b| &source[b..offset]);
        }
    }
    begin.map_or("", |b| &source[b..])
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().chars().take(80).collect()
}

/// A short name for an unnamed item, such as `test("adds numbers", () => …)`: its
/// first string literal (the test's title), else the start of its first line.
fn unnamed_label(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let short = |s: &str, n: usize| if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_string() };
    for q in ['"', '\'', '`'] {
        if let Some(start) = line.find(q) {
            if let Some(len) = line[start + 1..].find(q) {
                return format!("“{}”", short(&line[start + 1..start + 1 + len], 40));
            }
        }
    }
    short(line, 40)
}

/// New-side locations of entries describing changed (not removed) code.
fn changed_locations(a: &InternalAnalysis) -> Vec<(ManifestEntryId, Location)> {
    let m = &a.result.manifest;
    let mut v: Vec<(ManifestEntryId, Location)> = Vec::new();
    v.extend(m.logic_changes.iter().filter(|e| e.location.side == Side::New).map(|e| (e.id, e.location.clone())));
    v.extend(m.signature_changes.iter().map(|e| (e.id, e.location.clone())));
    v.extend(m.renames.iter().flat_map(|e| e.locations.iter().map(move |l| (e.id, l.clone()))));
    v.extend(m.extracted_functions.iter().flat_map(|e| e.locations_new.iter().map(move |l| (e.id, l.clone()))));
    v
}

fn is_function_entry(a: &InternalAnalysis, id: ManifestEntryId) -> bool {
    use crate::manifest::SymbolKind;
    let m = &a.result.manifest;
    m.logic_changes.iter().any(|e| e.id == id && matches!(e.kind, SymbolKind::Function | SymbolKind::Method))
        || m.signature_changes.iter().any(|e| e.id == id)
        || m.renames.iter().any(|e| e.id == id && matches!(e.kind, SymbolKind::Function | SymbolKind::Method))
}

#[cfg(test)]
mod tests {
    use super::unnamed_label;

    #[test]
    fn unnamed_items_are_labeled_by_their_title() {
        assert_eq!(unnamed_label("test.each([false, true])(\"keeps keys (jitless: %s)\", async (jitless) => {"), "“keeps keys (jitless: %s)”");
        assert_eq!(unnamed_label("it('adds numbers', () => {"), "“adds numbers”");
        assert_eq!(unnamed_label("describe(`a very long title that goes on and on and on for a while`, () => {"), "“a very long title that goes on and on an…”");
        assert_eq!(unnamed_label("setup();"), "setup();");
    }
}
