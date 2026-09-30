//! Link display hunks to manifest entries and mark mechanical noise.
//!
//! Works on any hunks (AST-derived or `git diff`), so the viewer can show the
//! diff the reviewer expects while still knowing *what* each line is.

use crate::classify::find_identifier;
use crate::diff::{is_nontrivial, norm_line};
use crate::manifest::{ChangeKind, ChangeManifest, DiffHunk, Location, Noise, Side};
use std::collections::HashMap;

/// Consecutive moved-looking lines required before calling a block "moved"
/// (single lines repeat by coincidence).
const MIN_MOVED_RUN: usize = 3;

/// Global context shared by every file's annotation pass.
#[derive(Default)]
pub struct AnnotateContext {
    /// (old bare name, new bare name) for renames of top-level items, which
    /// callers in any file may reference. Member renames (`Svc.get`) are passed
    /// per file instead: a bare `get` elsewhere is usually a different method.
    pub renames: Vec<(String, String)>,
    /// Normalized non-trivial removed/added lines across all files.
    pub removed_lines: HashMap<String, usize>,
    pub added_lines: HashMap<String, usize>,
}

impl AnnotateContext {
    pub fn collect_lines(&mut self, hunks: &[DiffHunk]) {
        for c in hunks.iter().flat_map(|h| h.changes.iter()) {
            match c.kind {
                ChangeKind::Removed => bump(&mut self.removed_lines, c.content_old.as_deref()),
                ChangeKind::Added => bump(&mut self.added_lines, c.content_new.as_deref()),
                _ => {}
            }
        }
    }
}

fn bump(map: &mut HashMap<String, usize>, line: Option<&str>) {
    if let Some(l) = line {
        let n = norm_line(l);
        if is_nontrivial(&n) {
            *map.entry(n).or_default() += 1;
        }
    }
}

/// Annotate one file's hunks in place.
/// `moved_spans` are locations of whole items known to have moved unchanged.
/// `local_renames` apply to this file only; `indent_sensitive` means leading
/// whitespace is syntax (Python, YAML), so re-indenting is never formatting.
#[allow(clippy::too_many_arguments)]
pub fn annotate_file(
    hunks: &mut [DiffHunk],
    manifest: &ChangeManifest,
    moved_spans: &[Location],
    ctx: &AnnotateContext,
    local_renames: &[(String, String)],
    generated: bool,
    detect_comments: bool,
    indent_sensitive: bool,
) {
    let renames: Vec<(String, String)> = ctx.renames.iter().chain(local_renames).cloned().collect();
    let opts = BlockOpts { renames: &renames, detect_comments, indent_sensitive };
    let entry_locs = manifest.locations();
    for hunk in hunks.iter_mut() {
        // Link hunk to manifest entries by changed-line overlap.
        let mut refs: Vec<u32> = Vec::new();
        // New-side line each removed line sits in front of, so deletions inside a
        // modified item (whose entry only has a new-side location) still link to it.
        let mut next_new = vec![0usize; hunk.changes.len()];
        let mut upcoming = hunk.new_range.end.max(hunk.new_range.start);
        for (k, c) in hunk.changes.iter().enumerate().rev() {
            if let Some(s) = &c.new_span {
                upcoming = s.start_line;
            }
            next_new[k] = upcoming;
        }
        for (k, c) in hunk.changes.iter().enumerate() {
            let (side, line) = match (c.kind, &c.old_span, &c.new_span) {
                (ChangeKind::Removed, Some(s), _) => (Side::Old, s.start_line),
                (ChangeKind::Added | ChangeKind::Modified, _, Some(s)) => (Side::New, s.start_line),
                _ => continue,
            };
            let mut linked = false;
            for (id, loc) in &entry_locs {
                if loc.contains(side, line) {
                    linked = true;
                    if !refs.contains(id) { refs.push(*id); }
                }
            }
            if !linked && c.kind == ChangeKind::Removed {
                for (id, loc) in &entry_locs {
                    if loc.contains(Side::New, next_new[k]) && !refs.contains(id) {
                        refs.push(*id);
                    }
                }
            }
        }
        hunk.manifest_refs = refs;

        if generated {
            for c in hunk.changes.iter_mut().filter(|c| is_change(c.kind)) {
                c.noise = Some(Noise::Generated);
            }
            hunk.noise = Some(Noise::Generated);
            continue;
        }
        annotate_noise(hunk, moved_spans, ctx, &opts);
    }
}

struct BlockOpts<'a> {
    renames: &'a [(String, String)],
    detect_comments: bool,
    indent_sensitive: bool,
}

fn is_change(kind: ChangeKind) -> bool {
    !matches!(kind, ChangeKind::Context)
}

fn annotate_noise(hunk: &mut DiffHunk, moved_spans: &[Location], ctx: &AnnotateContext, opts: &BlockOpts) {
    let n = hunk.changes.len();
    let mut i = 0;
    while i < n {
        if !is_change(hunk.changes[i].kind) {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && is_change(hunk.changes[i].kind) {
            i += 1;
        }
        annotate_block(&mut hunk.changes[start..i], moved_spans, ctx, opts);
    }

    let mut counts: HashMap<Noise, usize> = HashMap::new();
    let mut all = true;
    let mut any = false;
    for c in hunk.changes.iter().filter(|c| is_change(c.kind)) {
        any = true;
        match c.noise {
            Some(k) => *counts.entry(k).or_default() += 1,
            None => all = false,
        }
    }
    hunk.noise = if any && all {
        counts.into_iter().max_by_key(|(k, v)| (*v, *k as u8)).map(|(k, _)| k)
    } else {
        None
    };
}

/// A run of consecutive removed/added lines.
fn annotate_block(block: &mut [crate::manifest::Change], moved_spans: &[Location], ctx: &AnnotateContext, opts: &BlockOpts) {
    let text = |c: &crate::manifest::Change| -> String {
        match c.kind {
            ChangeKind::Removed => c.content_old.clone().unwrap_or_default(),
            _ => c.content_new.clone().unwrap_or_default(),
        }
    };

    // Blank lines and comments.
    for c in block.iter_mut() {
        if c.noise.is_some() { continue; }
        let t = text(c);
        let trimmed = t.trim();
        if trimmed.is_empty() {
            c.noise = Some(Noise::Formatting);
        } else if opts.detect_comments && is_comment_line(trimmed) {
            c.noise = Some(Noise::Comment);
        }
    }

    // Pair removed with added lines: identical tokens → formatting; identical
    // after applying renames → rename.
    let norm = |t: &str| norm_code_line(t, opts.indent_sensitive);
    let mut added_by_norm: HashMap<String, Vec<usize>> = HashMap::new();
    for (k, c) in block.iter().enumerate() {
        if c.kind == ChangeKind::Added && c.noise.is_none() {
            added_by_norm.entry(norm(&text(c))).or_default().push(k);
        }
    }
    for k in 0..block.len() {
        if block[k].kind != ChangeKind::Removed || block[k].noise.is_some() { continue; }
        let t = text(&block[k]);
        let norm_t = norm(&t);
        if let Some(j) = added_by_norm.get_mut(&norm_t).and_then(|v| v.pop()) {
            block[k].noise = Some(Noise::Formatting);
            block[j].noise = Some(Noise::Formatting);
            continue;
        }
        if !opts.renames.is_empty() {
            let renamed = apply_renames(&t, opts.renames);
            if renamed != t {
                if let Some(j) = added_by_norm.get_mut(&norm(&renamed)).and_then(|v| v.pop()) {
                    block[k].noise = Some(Noise::Rename);
                    block[j].noise = Some(Noise::Rename);
                }
            }
        }
    }

    // Moved: inside a known moved item, or a run of lines that appear verbatim
    // on the opposite side elsewhere in the diff.
    for c in block.iter_mut() {
        if c.noise.is_some() { continue; }
        let (side, line) = match c.kind {
            ChangeKind::Removed => (Side::Old, c.old_span.as_ref().map(|s| s.start_line)),
            _ => (Side::New, c.new_span.as_ref().map(|s| s.start_line)),
        };
        if let Some(line) = line {
            if moved_spans.iter().any(|l| l.contains(side, line)) {
                c.noise = Some(Noise::Moved);
            }
        }
    }
    let seen_elsewhere = |c: &crate::manifest::Change| -> bool {
        let n = norm_line(&text(c));
        if !is_nontrivial(&n) { return false; }
        let other = if c.kind == ChangeKind::Removed { &ctx.added_lines } else { &ctx.removed_lines };
        other.contains_key(&n)
    };
    let mut k = 0;
    while k < block.len() {
        let kind = block[k].kind;
        let start = k;
        while k < block.len() && block[k].kind == kind && block[k].noise.is_none() && seen_elsewhere(&block[k]) {
            k += 1;
        }
        if k - start >= MIN_MOVED_RUN {
            for c in &mut block[start..k] {
                c.noise = Some(Noise::Moved);
            }
        }
        if k == start {
            k += 1;
        }
    }
}

/// Normalize a line for "formatting only" comparison: whitespace between tokens
/// is dropped (kept as one space between two word characters, so `int x` ≠ `intx`),
/// string literals are kept verbatim, and leading indentation is kept when it is syntax.
fn norm_code_line(line: &str, indent_sensitive: bool) -> String {
    let mut out = String::with_capacity(line.len());
    let body = if indent_sensitive {
        let trimmed = line.trim_start();
        out.push_str(&line[..line.len() - trimmed.len()]);
        trimmed
    } else {
        line
    };
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut pending_space = false;
    for c in body.chars() {
        if let Some(q) = quote {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && out.chars().last().is_some_and(is_word) && is_word(c) {
            out.push(' ');
        }
        pending_space = false;
        if matches!(c, '"' | '\'' | '`') {
            quote = Some(c);
        }
        out.push(c);
    }
    out
}

/// Languages/formats where indentation is syntax.
pub fn is_indent_sensitive(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("");
    matches!(ext, "py" | "pyi" | "pyw" | "yaml" | "yml" | "coffee" | "pug" | "sass" | "haml" | "nim")
}

fn is_comment_line(t: &str) -> bool {
    t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with("*/")
        || t == "*"
        || t.starts_with("* ")
        || t == "#"
        || t.starts_with("# ")
        || t.starts_with("#!") && !t.starts_with("#![")
        || t.starts_with("<!--")
        || t.starts_with("-- ")
}

/// Replace every identifier occurrence of an old name with its new name.
fn apply_renames(line: &str, renames: &[(String, String)]) -> String {
    let mut out = line.to_string();
    for (old, new) in renames {
        if !out.contains(old.as_str()) { continue; }
        let mut result = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(pos) = find_identifier(rest, old) {
            result.push_str(&rest[..pos]);
            result.push_str(new);
            rest = &rest[pos + old.len()..];
        }
        result.push_str(rest);
        out = result;
    }
    out
}

/// Paths of generated/vendored files whose diffs are noise for review.
pub fn is_generated_path(path: &str) -> bool {
    crate::roles::is_generated_path(path) || crate::roles::is_vendored_path(path)
}
