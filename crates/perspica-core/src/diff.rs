use crate::manifest::{Change, ChangeKind, DiffHunk, LineRange, Span};
use crate::parser::{SemanticItem, SemanticTree};

/// Result of diffing two semantic trees.
#[derive(Debug, Default)]
pub struct DiffOutput {
    pub matched: Vec<MatchedPair>,
    pub added: Vec<usize>,   // indices into new_tree.items
    pub removed: Vec<usize>, // indices into old_tree.items
    pub hunks: Vec<DiffHunk>,
    /// Functions whose body was split out into new functions.
    pub extractions: Vec<Extraction>,
    /// Indices into `matched` of unchanged items that moved position in the file.
    pub moved: Vec<usize>,
}

#[derive(Debug)]
pub struct MatchedPair {
    pub old_idx: usize,
    pub new_idx: usize,
    pub match_kind: MatchKind,
}

/// One function split into several.
#[derive(Debug, Clone)]
pub struct Extraction {
    /// The original function (old tree).
    pub old_idx: usize,
    /// Where the original still exists in the new tree (partial extraction), if it does.
    pub new_idx: Option<usize>,
    /// The new functions that received its code (new tree).
    pub extracted: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// Same name, identical text.
    Exact,
    /// Different name, otherwise identical syntax.
    Rename,
    /// Different name, body substantially similar but edited.
    RenameModified,
    /// Same name, syntax changed.
    Modified,
    /// Same name, same syntax; only whitespace or comments differ.
    FormattingOnly,
}

/// Minimum token similarity for a fuzzy (rename + edit) match.
const FUZZY_RENAME_THRESHOLD: f64 = 0.75;
/// Items smaller than this many tokens are never fuzzy-matched (too ambiguous).
const FUZZY_MIN_TOKENS: usize = 12;
/// Skip fuzzy matching when the candidate matrix is larger than this (keeps huge rewrites fast).
const FUZZY_MAX_PAIRS: usize = 4000;
/// Fraction of a deleted function's non-trivial lines that must reappear in new functions.
const EXTRACTION_COVERAGE: f64 = 0.70;
/// Fraction of a new function's lines that must come from code removed elsewhere (partial extraction).
const PARTIAL_EXTRACTION_COVERAGE: f64 = 0.60;

/// Diff two semantic trees.
///
/// Phase 1 matches items: by name, then by name-masked shape (pure renames), then
/// by token similarity (rename + edit). Extractions and in-file moves are detected
/// from what remains. Phase 2 generates line hunks for changed pairs.
pub fn diff(
    old_tree: &SemanticTree,
    new_tree: &SemanticTree,
    old_source: &str,
    new_source: &str,
) -> DiffOutput {
    let old_lines: Vec<&str> = old_source.lines().collect();
    let new_lines: Vec<&str> = new_source.lines().collect();
    let old_meta = |i: usize| old_tree.meta.get(i);
    let new_meta = |i: usize| new_tree.meta.get(i);

    let mut matched = Vec::new();
    let mut old_matched = vec![false; old_tree.items.len()];
    let mut new_matched = vec![false; new_tree.items.len()];

    // Pass 1: same name, same kind.
    let mut by_name: std::collections::HashMap<(std::mem::Discriminant<SemanticItem>, &str), Vec<usize>> =
        std::collections::HashMap::new();
    for (ni, item) in new_tree.items.iter().enumerate() {
        if let Some(name) = item.name() {
            by_name.entry((std::mem::discriminant(item), name)).or_default().push(ni);
        }
    }
    for (oi, old_item) in old_tree.items.iter().enumerate() {
        let Some(name) = old_item.name() else { continue };
        let Some(cands) = by_name.get_mut(&(std::mem::discriminant(old_item), name)) else { continue };
        let Some(pos) = cands.iter().position(|&ni| !new_matched[ni]) else { continue };
        let ni = cands.remove(pos);
        let new_item = &new_tree.items[ni];
        let match_kind = if span_lines(old_item.span(), &old_lines) == span_lines(new_item.span(), &new_lines) {
            MatchKind::Exact
        } else if old_meta(oi).map(|m| m.norm_hash) == new_meta(ni).map(|m| m.norm_hash)
            && old_meta(oi).is_some()
            && !signature_differs(old_item, new_item)
        {
            MatchKind::FormattingOnly
        } else {
            MatchKind::Modified
        };
        matched.push(MatchedPair { old_idx: oi, new_idx: ni, match_kind });
        old_matched[oi] = true;
        new_matched[ni] = true;
    }

    // Pass 1b: unnamed items (top-level statements) with identical syntax: unchanged.
    let mut unnamed_new: std::collections::HashMap<u64, Vec<usize>> = std::collections::HashMap::new();
    for (ni, item) in new_tree.items.iter().enumerate() {
        if item.name().is_none() && !new_matched[ni] {
            if let Some(m) = new_meta(ni) { unnamed_new.entry(m.norm_hash).or_default().push(ni); }
        }
    }
    for (oi, item) in old_tree.items.iter().enumerate() {
        if item.name().is_some() || old_matched[oi] { continue; }
        let Some(m) = old_meta(oi) else { continue };
        if let Some(cands) = unnamed_new.get_mut(&m.norm_hash) {
            if let Some(ni) = cands.pop() {
                let kind = if span_lines(item.span(), &old_lines) == span_lines(new_tree.items[ni].span(), &new_lines) {
                    MatchKind::Exact
                } else {
                    MatchKind::FormattingOnly
                };
                matched.push(MatchedPair { old_idx: oi, new_idx: ni, match_kind: kind });
                old_matched[oi] = true;
                new_matched[ni] = true;
            }
        }
    }

    // Pass 1c: edited top-level statements. Pair unnamed items by token similarity.
    for (oi, item) in old_tree.items.iter().enumerate() {
        if item.name().is_some() || old_matched[oi] { continue; }
        let Some(om) = old_meta(oi) else { continue };
        let best = new_tree.items.iter().enumerate()
            .filter(|(ni, n)| n.name().is_none() && !new_matched[*ni])
            .filter_map(|(ni, _)| new_meta(ni).map(|nm| (ni, dice(&om.tokens, &nm.tokens))))
            .filter(|&(_, sim)| sim >= 0.5)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((ni, _)) = best {
            matched.push(MatchedPair { old_idx: oi, new_idx: ni, match_kind: MatchKind::Modified });
            old_matched[oi] = true;
            new_matched[ni] = true;
        }
    }

    // Pass 2: pure renames, identical syntax once the item's own name is masked.
    // Small items (`fn f() -> bool { true }`) share shapes by coincidence, so they
    // pair only when the match is unambiguous on both sides.
    for (oi, old_item) in old_tree.items.iter().enumerate() {
        if old_matched[oi] || !renamable(old_item) { continue; }
        let Some(om) = old_meta(oi) else { continue };
        let same_shape_new: Vec<usize> = new_tree.items.iter().enumerate()
            .filter(|(ni, n)| !new_matched[*ni] && same_kind(old_item, n))
            .filter(|(ni, _)| new_meta(*ni).is_some_and(|nm| nm.shape_hash == om.shape_hash))
            .map(|(ni, _)| ni)
            .collect();
        let Some(&ni) = same_shape_new.first() else { continue };
        if om.tokens.len() < FUZZY_MIN_TOKENS {
            let same_shape_old = old_tree.items.iter().enumerate()
                .filter(|(i, o)| !old_matched[*i] && same_kind(old_item, o))
                .filter(|(i, _)| old_meta(*i).is_some_and(|m| m.shape_hash == om.shape_hash))
                .count();
            if same_shape_new.len() > 1 || same_shape_old > 1 { continue; }
        }
        matched.push(MatchedPair { old_idx: oi, new_idx: ni, match_kind: MatchKind::Rename });
        old_matched[oi] = true;
        new_matched[ni] = true;
    }

    // Extractions: a deleted function whose lines reappear across 2+ new functions.
    let mut extractions = Vec::new();
    let unmatched_old_fns: Vec<usize> = (0..old_tree.items.len())
        .filter(|&i| !old_matched[i] && matches!(old_tree.items[i], SemanticItem::Function { .. }))
        .collect();
    let unmatched_new_fns: Vec<usize> = (0..new_tree.items.len())
        .filter(|&i| !new_matched[i] && matches!(new_tree.items[i], SemanticItem::Function { .. }))
        .collect();
    let mut extracted_old = vec![false; old_tree.items.len()];
    let mut extracted_new = vec![false; new_tree.items.len()];
    if unmatched_new_fns.len() >= 2 {
        let new_bodies: Vec<(usize, std::collections::HashSet<String>)> = unmatched_new_fns.iter()
            .map(|&ni| (ni, nontrivial_lines(new_tree.items[ni].span(), &new_lines)))
            .collect();
        for &oi in &unmatched_old_fns {
            let del = nontrivial_lines(old_tree.items[oi].span(), &old_lines);
            if del.len() < 3 { continue; }
            let receivers: Vec<usize> = new_bodies.iter()
                .filter(|(ni, body)| !extracted_new[*ni] && body.iter().filter(|l| del.contains(*l)).count() >= 2)
                .map(|(ni, _)| *ni)
                .collect();
            if receivers.len() < 2 { continue; }
            let covered = del.iter()
                .filter(|l| new_bodies.iter().any(|(ni, b)| receivers.contains(ni) && b.contains(*l)))
                .count();
            if covered as f64 / del.len() as f64 >= EXTRACTION_COVERAGE {
                extracted_old[oi] = true;
                for &ni in &receivers { extracted_new[ni] = true; }
                extractions.push(Extraction { old_idx: oi, new_idx: None, extracted: receivers });
            }
        }
    }

    // Pass 3: fuzzy renames (rename + edit) by token similarity, best pairs first.
    let fuzzy_old: Vec<usize> = (0..old_tree.items.len())
        .filter(|&i| !old_matched[i] && !extracted_old[i] && renamable(&old_tree.items[i]))
        .collect();
    let fuzzy_new: Vec<usize> = (0..new_tree.items.len())
        .filter(|&i| !new_matched[i] && !extracted_new[i] && renamable(&new_tree.items[i]))
        .collect();
    if !fuzzy_old.is_empty() && fuzzy_old.len() * fuzzy_new.len() <= FUZZY_MAX_PAIRS {
        let mut cands = Vec::new();
        for &oi in &fuzzy_old {
            let Some(om) = old_meta(oi) else { continue };
            if om.tokens.len() < FUZZY_MIN_TOKENS { continue; }
            for &ni in &fuzzy_new {
                if !same_kind(&old_tree.items[oi], &new_tree.items[ni]) { continue; }
                let Some(nm) = new_meta(ni) else { continue };
                if nm.tokens.len() < FUZZY_MIN_TOKENS { continue; }
                let sim = dice(&om.tokens, &nm.tokens);
                if sim >= FUZZY_RENAME_THRESHOLD {
                    cands.push((sim, oi, ni));
                }
            }
        }
        cands.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        for (_, oi, ni) in cands {
            if old_matched[oi] || new_matched[ni] { continue; }
            matched.push(MatchedPair { old_idx: oi, new_idx: ni, match_kind: MatchKind::RenameModified });
            old_matched[oi] = true;
            new_matched[ni] = true;
        }
    }

    // Partial extractions: a modified function lost lines that now live in a new
    // function it calls.
    for pair in &matched {
        if pair.match_kind != MatchKind::Modified { continue; }
        let (old_item, new_item) = (&old_tree.items[pair.old_idx], &new_tree.items[pair.new_idx]);
        if !matches!(old_item, SemanticItem::Function { .. }) { continue; }
        let before = nontrivial_lines(old_item.span(), &old_lines);
        let after = nontrivial_lines(new_item.span(), &new_lines);
        let lost: std::collections::HashSet<&String> = before.iter().filter(|l| !after.contains(*l)).collect();
        if lost.len() < 3 { continue; }
        let Some(caller_meta) = new_meta(pair.new_idx) else { continue };
        let mut receivers = Vec::new();
        for (ni, item) in new_tree.items.iter().enumerate() {
            if new_matched[ni] || extracted_new[ni] || !matches!(item, SemanticItem::Function { .. }) { continue; }
            let Some(name) = item.name() else { continue };
            if !caller_meta.refs.contains(&crate::parser::ident_hash(crate::parser::bare_name(name))) { continue; }
            let body = nontrivial_lines(item.span(), &new_lines);
            if body.len() < 2 { continue; }
            let from_caller = body.iter().filter(|l| lost.contains(l)).count();
            if from_caller as f64 / body.len() as f64 >= PARTIAL_EXTRACTION_COVERAGE {
                receivers.push(ni);
            }
        }
        if !receivers.is_empty() {
            for &ni in &receivers { extracted_new[ni] = true; }
            extractions.push(Extraction { old_idx: pair.old_idx, new_idx: Some(pair.new_idx), extracted: receivers });
        }
    }

    // In-file moves: unchanged items whose relative order changed. Items on the
    // longest increasing run of new positions stayed put; the rest moved.
    let mut stable: Vec<(usize, usize)> = matched.iter().enumerate()
        .filter(|(_, p)| matches!(p.match_kind, MatchKind::Exact | MatchKind::FormattingOnly))
        .map(|(mi, p)| (mi, p.old_idx))
        .collect();
    stable.sort_by_key(|&(_, oi)| old_tree.items[oi].span().start_line);
    let positions: Vec<usize> = stable.iter()
        .map(|&(mi, _)| new_tree.items[matched[mi].new_idx].span().start_line)
        .collect();
    let keep = longest_increasing(&positions);
    let moved: Vec<usize> = stable.iter().enumerate()
        .filter(|(k, (mi, _))| {
            let span = old_tree.items[matched[*mi].old_idx].span();
            !keep[*k] && span.end_line - span.start_line >= 1
        })
        .map(|(_, (mi, _))| *mi)
        .collect();

    // Collect unmatched
    let removed: Vec<usize> = old_matched
        .iter()
        .enumerate()
        .filter(|(_, m)| !**m)
        .map(|(i, _)| i)
        .collect();
    let added: Vec<usize> = new_matched
        .iter()
        .enumerate()
        .filter(|(_, m)| !**m)
        .map(|(i, _)| i)
        .collect();

    // Phase 2: Generate hunks with actual source lines
    let mut hunks = Vec::new();

    for pair in &matched {
        let old_item = &old_tree.items[pair.old_idx];
        let new_item = &new_tree.items[pair.new_idx];
        match pair.match_kind {
            MatchKind::Exact => continue,
            MatchKind::FormattingOnly => {
                // Still generate a hunk so it can be shown collapsed
                hunks.push(make_formatting_hunk(old_item, new_item));
            }
            MatchKind::Rename => {
                hunks.push(make_rename_hunk(old_item, new_item, &old_lines, &new_lines));
            }
            MatchKind::Modified | MatchKind::RenameModified => {
                hunks.push(make_line_diff_hunk(old_item, new_item, &old_lines, &new_lines));
            }
        }
    }

    for &ri in &removed {
        hunks.push(whole_item_hunk(&old_tree.items[ri], &old_lines, ChangeKind::Removed));
    }
    for &ai in &added {
        hunks.push(whole_item_hunk(&new_tree.items[ai], &new_lines, ChangeKind::Added));
    }

    DiffOutput {
        matched,
        added,
        removed,
        hunks,
        extractions,
        moved,
    }
}

fn whole_item_hunk(item: &SemanticItem, lines: &[&str], kind: ChangeKind) -> DiffHunk {
    let span = item.span();
    let body = span_lines(span, lines);
    let range = span_to_line_range(span);
    let changes = body
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let s = Some(Span { start_line: span.start_line + i, start_col: 0, end_line: span.start_line + i, end_col: line.len() });
            let (old_span, new_span, content_old, content_new) = if kind == ChangeKind::Removed {
                (s, None, Some(line.to_string()), None)
            } else {
                (None, s, None, Some(line.to_string()))
            };
            Change { kind, old_span, new_span, content_old, content_new, noise: None }
        })
        .collect();
    let empty = LineRange { start: 0, end: 0 };
    let (old_range, new_range) = if kind == ChangeKind::Removed { (range, empty) } else { (empty, range) };
    DiffHunk { old_range, new_range, changes, manifest_refs: vec![], noise: None, test: false }
}

/// Items that can be matched across a name change.
fn renamable(item: &SemanticItem) -> bool {
    matches!(item, SemanticItem::Function { .. } | SemanticItem::Class { .. } | SemanticItem::TypeDef { .. } | SemanticItem::Variable { .. })
}

/// Sørensen–Dice similarity of two sorted token multisets.
pub fn dice(a: &[u32], b: &[u32]) -> f64 {
    let (mut i, mut j, mut common) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => { common += 1; i += 1; j += 1; }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    (2 * common) as f64 / (a.len() + b.len()).max(1) as f64
}

/// Mask of which elements lie on one longest strictly-increasing subsequence.
fn longest_increasing(xs: &[usize]) -> Vec<bool> {
    let n = xs.len();
    let mut tails: Vec<usize> = Vec::new(); // indices into xs
    let mut prev = vec![usize::MAX; n];
    for i in 0..n {
        let pos = tails.partition_point(|&t| xs[t] < xs[i]);
        if pos > 0 { prev[i] = tails[pos - 1]; }
        if pos == tails.len() { tails.push(i); } else { tails[pos] = i; }
    }
    let mut keep = vec![false; n];
    let mut k = tails.last().copied().unwrap_or(usize::MAX);
    while k != usize::MAX {
        keep[k] = true;
        k = prev[k];
    }
    keep
}

/// Whitespace-free form of a line, for comparing code independent of formatting.
pub fn norm_line(line: &str) -> String {
    line.split_whitespace().collect()
}

/// A normalized line carries meaning on its own (not just a brace or keyword).
pub fn is_nontrivial(norm: &str) -> bool {
    norm.len() >= 4
        && norm.chars().any(|c| c.is_alphanumeric())
        && !matches!(norm, "else{" | "}else{" | "return;" | "break;" | "pass" | "continue;" | "});" | "end")
}

/// The set of non-trivial normalized lines within a span.
pub fn nontrivial_lines(span: &Span, lines: &[&str]) -> std::collections::HashSet<String> {
    span_lines(span, lines)
        .into_iter()
        .skip(1) // the declaration line differs by name/signature
        .map(norm_line)
        .filter(|l| is_nontrivial(l))
        .collect()
}

/// Check if two items have different function signatures (params or return type).
pub(crate) fn signature_differs(a: &SemanticItem, b: &SemanticItem) -> bool {
    if let (
        SemanticItem::Function {
            params: pa,
            return_type: ra,
            ..
        },
        SemanticItem::Function {
            params: pb,
            return_type: rb,
            ..
        },
    ) = (a, b)
    {
        if pa.len() != pb.len() {
            return true;
        }
        if pa.iter().zip(pb.iter()).any(|(a, b)| {
            a.name != b.name || a.type_annotation != b.type_annotation
        }) {
            return true;
        }
        if ra != rb {
            return true;
        }
    }
    false
}

fn same_kind(a: &SemanticItem, b: &SemanticItem) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

fn span_to_line_range(span: &Span) -> LineRange {
    LineRange {
        start: span.start_line,
        end: span.end_line,
    }
}

/// Get the source lines covered by a span (1-indexed).
fn span_lines<'a>(span: &Span, lines: &[&'a str]) -> Vec<&'a str> {
    let start = span.start_line.saturating_sub(1); // convert to 0-indexed
    let end = span.end_line.min(lines.len());
    if start >= lines.len() {
        return vec![];
    }
    lines[start..end].to_vec()
}

/// Generate a line-level diff hunk for modified items using a simple LCS-based approach.
fn make_line_diff_hunk(
    old_item: &SemanticItem,
    new_item: &SemanticItem,
    old_lines: &[&str],
    new_lines: &[&str],
) -> DiffHunk {
    let old_span_lines = span_lines(old_item.span(), old_lines);
    let new_span_lines = span_lines(new_item.span(), new_lines);

    let changes = line_diff(&old_span_lines, &new_span_lines, old_item.span(), new_item.span());

    DiffHunk {
        old_range: span_to_line_range(old_item.span()),
        new_range: span_to_line_range(new_item.span()),
        changes,
        manifest_refs: vec![],
        noise: None,
        test: false,
    }
}

fn make_rename_hunk(
    old_item: &SemanticItem,
    new_item: &SemanticItem,
    old_lines: &[&str],
    new_lines: &[&str],
) -> DiffHunk {
    let old_span_lines = span_lines(old_item.span(), old_lines);
    let new_span_lines = span_lines(new_item.span(), new_lines);

    // For renames, show old declaration removed, new declaration added
    let mut changes = Vec::new();
    if let Some(first_old) = old_span_lines.first() {
        changes.push(Change {
            kind: ChangeKind::Removed,
            old_span: Some(old_item.span().clone()),
            new_span: None,
            content_old: Some(first_old.to_string()),
            content_new: None,
            noise: None,
        });
    }
    if let Some(first_new) = new_span_lines.first() {
        changes.push(Change {
            kind: ChangeKind::Added,
            old_span: None,
            new_span: Some(new_item.span().clone()),
            content_old: None,
            content_new: Some(first_new.to_string()),
            noise: None,
        });
    }

    DiffHunk {
        old_range: span_to_line_range(old_item.span()),
        new_range: span_to_line_range(new_item.span()),
        changes,
        manifest_refs: vec![],
        noise: None,
        test: false,
    }
}

fn make_formatting_hunk(old_item: &SemanticItem, new_item: &SemanticItem) -> DiffHunk {
    let lines = old_item.span().end_line - old_item.span().start_line + 1;
    DiffHunk {
        old_range: span_to_line_range(old_item.span()),
        new_range: span_to_line_range(new_item.span()),
        changes: vec![Change {
            kind: ChangeKind::Modified,
            old_span: Some(old_item.span().clone()),
            new_span: Some(new_item.span().clone()),
            content_old: Some(format!("~ formatting changes ({lines} lines)")),
            content_new: Some(format!("~ formatting changes ({lines} lines)")),
            noise: Some(crate::manifest::Noise::Formatting),
        }],
        manifest_refs: vec![],
        noise: Some(crate::manifest::Noise::Formatting),
        test: false,
    }
}

/// Above this many LCS cells, the middle of a diff is emitted as a plain
/// remove/add block instead (bounded memory on huge items).
const MAX_LCS_CELLS: usize = 4_000_000;

/// Line-level diff: common prefix/suffix trimmed, LCS on the middle.
pub fn line_diff(old: &[&str], new: &[&str], old_span: &Span, new_span: &Span) -> Vec<Change> {
    let prefix = old.iter().zip(new.iter()).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..].iter().rev().zip(new[prefix..].iter().rev()).take_while(|(a, b)| a == b).count();
    let ctx = |i: usize, j: usize| {
        let (o, n) = (old_span.start_line + i, new_span.start_line + j);
        Change {
            kind: ChangeKind::Context,
            old_span: Some(Span { start_line: o, start_col: 0, end_line: o, end_col: old[i].len() }),
            new_span: Some(Span { start_line: n, start_col: 0, end_line: n, end_col: new[j].len() }),
            content_old: Some(old[i].to_string()),
            content_new: Some(new[j].to_string()),
            noise: None,
        }
    };
    let mut out: Vec<Change> = (0..prefix).map(|i| ctx(i, i)).collect();
    let old_mid = &old[prefix..old.len() - suffix];
    let new_mid = &new[prefix..new.len() - suffix];
    let mid_old_span = Span { start_line: old_span.start_line + prefix, ..old_span.clone() };
    let mid_new_span = Span { start_line: new_span.start_line + prefix, ..new_span.clone() };
    if old_mid.len().saturating_mul(new_mid.len()) > MAX_LCS_CELLS {
        for (i, l) in old_mid.iter().enumerate() {
            let ln = mid_old_span.start_line + i;
            out.push(Change { kind: ChangeKind::Removed, old_span: Some(Span { start_line: ln, start_col: 0, end_line: ln, end_col: l.len() }), new_span: None, content_old: Some(l.to_string()), content_new: None, noise: None });
        }
        for (j, l) in new_mid.iter().enumerate() {
            let ln = mid_new_span.start_line + j;
            out.push(Change { kind: ChangeKind::Added, old_span: None, new_span: Some(Span { start_line: ln, start_col: 0, end_line: ln, end_col: l.len() }), content_old: None, content_new: Some(l.to_string()), noise: None });
        }
    } else {
        out.extend(lcs_diff(old_mid, new_mid, &mid_old_span, &mid_new_span));
    }
    let (os, ns) = (old.len() - suffix, new.len() - suffix);
    out.extend((0..suffix).map(|k| ctx(os + k, ns + k)));
    out
}

fn lcs_diff(old: &[&str], new: &[&str], old_span: &Span, new_span: &Span) -> Vec<Change> {
    let m = old.len();
    let n = new.len();

    // Build LCS table
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            if old[i - 1] == new[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Backtrace to build diff
    let mut i = m;
    let mut j = n;

    // Collect in reverse, then reverse at the end
    let mut rev_changes = Vec::new();

    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old[i - 1] == new[j - 1] {
            let old_line_num = old_span.start_line + i - 1;
            let new_line_num = new_span.start_line + j - 1;
            rev_changes.push(Change {
                kind: ChangeKind::Context,
                old_span: Some(Span {
                    start_line: old_line_num,
                    start_col: 0,
                    end_line: old_line_num,
                    end_col: old[i - 1].len(),
                }),
                new_span: Some(Span {
                    start_line: new_line_num,
                    start_col: 0,
                    end_line: new_line_num,
                    end_col: new[j - 1].len(),
                }),
                content_old: Some(old[i - 1].to_string()),
                content_new: Some(new[j - 1].to_string()),
                noise: None,
            });
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            let line_num = new_span.start_line + j - 1;
            rev_changes.push(Change {
                kind: ChangeKind::Added,
                old_span: None,
                new_span: Some(Span {
                    start_line: line_num,
                    start_col: 0,
                    end_line: line_num,
                    end_col: new[j - 1].len(),
                }),
                content_old: None,
                content_new: Some(new[j - 1].to_string()),
                noise: None,
            });
            j -= 1;
        } else if i > 0 {
            let line_num = old_span.start_line + i - 1;
            rev_changes.push(Change {
                kind: ChangeKind::Removed,
                old_span: Some(Span {
                    start_line: line_num,
                    start_col: 0,
                    end_line: line_num,
                    end_col: old[i - 1].len(),
                }),
                new_span: None,
                content_old: Some(old[i - 1].to_string()),
                content_new: None,
                noise: None,
            });
            i -= 1;
        }
    }

    rev_changes.reverse();
    rev_changes
}
