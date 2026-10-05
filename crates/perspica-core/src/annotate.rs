//! Link display hunks to manifest entries and mark mechanical noise.
//!
//! Works on any hunks (AST-derived or `git diff`), so the viewer can show the
//! diff the reviewer expects while still knowing *what* each line is.

use crate::classify::find_identifier;
use crate::diff::{is_nontrivial, norm_line};
use crate::manifest::{ChangeKind, ChangeManifest, DiffHunk, Location, Noise, Side, Span};
use std::collections::{HashMap, HashSet};

/// How many non-trivial lines have to match in order before a block counts as moved.
/// Single lines repeat by coincidence.
const MIN_MOVED_RUN: usize = 3;
/// Above this many cells (removed lines times added lines) pair lines greedily instead of with a full LCS.
const MAX_ALIGN_CELLS: usize = 1 << 20;

/// Global context shared by every file's annotation pass.
#[derive(Default)]
pub struct AnnotateContext {
    /// (old bare name, new bare name) for renames of top-level items, which
    /// callers in any file may reference. Member renames (`Svc.get`) are passed
    /// per file instead: a bare `get` elsewhere is usually a different method.
    pub renames: Vec<(String, String)>,
    /// Removed and added code lines across all files, used to find moved code.
    removed: Runs,
    added: Runs,
}

/// A run of changed code lines, normalized. Blank and comment lines are skipped.
#[derive(Default)]
struct Runs {
    runs: Vec<Run>,
    /// Where each non-trivial normalized line shows up, as (run, position).
    at: HashMap<String, Vec<(usize, usize)>>,
}

struct Run {
    file: usize,
    lines: Vec<usize>,
    norms: Vec<String>,
}

impl Runs {
    fn push(&mut self, run: Run) {
        if run.norms.is_empty() { return; }
        let r = self.runs.len();
        for (p, n) in run.norms.iter().enumerate() {
            if is_nontrivial(n) { self.at.entry(n.clone()).or_default().push((r, p)); }
        }
        self.runs.push(run);
    }
}

impl AnnotateContext {
    /// Record one file's changed lines.
    pub fn collect_lines(&mut self, hunks: &[DiffHunk], facts: &FileFacts) {
        let file = facts.file;
        for h in hunks {
            let mut removed = Run { file, lines: Vec::new(), norms: Vec::new() };
            let mut added = Run { file, lines: Vec::new(), norms: Vec::new() };
            for c in &h.changes {
                let Some((side, line, text)) = side_line(c) else {
                    self.removed.push(std::mem::replace(&mut removed, Run { file, lines: Vec::new(), norms: Vec::new() }));
                    self.added.push(std::mem::replace(&mut added, Run { file, lines: Vec::new(), norms: Vec::new() }));
                    continue;
                };
                let n = facts.move_key(side, line, text);
                if n.is_empty() || facts.is_comment(side, line) { continue; }
                let run = if side == Side::Old { &mut removed } else { &mut added };
                run.lines.push(line);
                run.norms.push(n);
            }
            self.removed.push(removed);
            self.added.push(added);
        }
    }
}

/// Side, line number and text of a changed line. `None` for context lines.
fn side_line(c: &crate::manifest::Change) -> Option<(Side, usize, &str)> {
    match c.kind {
        ChangeKind::Removed => Some((Side::Old, c.old_span.as_ref()?.start_line, c.content_old.as_deref().unwrap_or(""))),
        ChangeKind::Context => None,
        _ => Some((Side::New, c.new_span.as_ref()?.start_line, c.content_new.as_deref().unwrap_or(""))),
    }
}

/// What annotating one file needs to know about its source.
pub struct FileFacts<'a> {
    /// Index of this file in the list of files.
    pub file: usize,
    /// Old and new source lines.
    pub lines: (&'a [&'a str], &'a [&'a str]),
    /// Comment-only lines of the old and new source (1-based).
    pub comments: (&'a HashSet<usize>, &'a HashSet<usize>),
    /// Lines inside a multi-line string. Whitespace matters on these.
    pub strings: (&'a HashSet<usize>, &'a HashSet<usize>),
    /// Lines with a regex or a `#define`. The regex text has to match exactly, and for a `#define` (`None`) the whole line does.
    pub literals: (&'a crate::parser::LiteralLines, &'a crate::parser::LiteralLines),
    /// (old, new) spans of functions and methods that exist on both sides.
    /// Code moving around inside one of these is a reorder, not a move.
    pub bodies: &'a [(Span, Span)],
    /// Leading whitespace is syntax (Python, YAML).
    pub indent_sensitive: bool,
    /// A file we don't parse (Markdown, Makefiles, config). Indentation and trailing whitespace
    /// can matter there, so only spacing inside a line counts as formatting.
    pub unparsed: bool,
}

impl FileFacts<'_> {
    fn pick<T>(side: Side, pair: (T, T)) -> T {
        if side == Side::Old { pair.0 } else { pair.1 }
    }

    fn is_comment(&self, side: Side, line: usize) -> bool {
        Self::pick(side, self.comments).contains(&line)
    }

    fn line(&self, side: Side, line: usize) -> Option<&str> {
        Self::pick(side, self.lines).get(line.checked_sub(1)?).copied()
    }

    /// Key for comparing lines for formatting. Whitespace between tokens is dropped,
    /// except inside strings and where indentation matters.
    fn format_key(&self, side: Side, line: usize, text: &str) -> String {
        if Self::pick(side, self.strings).contains(&line) {
            text.to_string()
        } else if let Some(literal) = Self::pick(side, self.literals).get(&line) {
            match literal {
                None => text.trim().to_string(),
                Some(regexes) => {
                    // Set the regexes aside, normalize the rest, then add the regexes back as they are.
                    let mut rest = text.to_string();
                    for r in regexes { rest = rest.replacen(r.as_str(), "\u{0}", 1); }
                    let mut key = norm_code_line(&rest, self.indent_sensitive);
                    for r in regexes { key.push('\u{0}'); key.push_str(r); }
                    key
                }
            }
        } else if self.unparsed {
            let mut key = norm_code_line(text, true);
            key.push_str(&text[text.trim_end().len()..]);
            key
        } else {
            norm_code_line(text, self.indent_sensitive)
        }
    }

    /// Key for comparing lines for moves. All whitespace is dropped, except inside strings.
    fn move_key(&self, side: Side, line: usize, text: &str) -> String {
        if Self::pick(side, self.strings).contains(&line) { text.to_string() } else { norm_line(text) }
    }
}

/// Changed lines in a function that are the same code in the same order on both sides,
/// ignoring whitespace. Returns (old lines, new lines) with their noise, moved if the text is
/// identical and formatting if not. Doing this over the whole function keeps a reformat
/// dimmed even when git lines things up unevenly. Reorders and edits stay unpaired.
fn function_formatting(hunks: &[DiffHunk], facts: &FileFacts) -> (HashMap<usize, Noise>, HashMap<usize, Noise>) {
    let (mut old_changed, mut new_changed) = (Vec::new(), Vec::new());
    for c in hunks.iter().flat_map(|h| h.changes.iter()) {
        match side_line(c) {
            Some((Side::Old, line, _)) => old_changed.push(line),
            Some((_, line, _)) => new_changed.push(line),
            None => {}
        }
    }
    let (mut old_fmt, mut new_fmt) = (HashMap::new(), HashMap::new());
    for (o, n) in facts.bodies {
        let touched = old_changed.iter().any(|l| o.start_line <= *l && *l <= o.end_line)
            || new_changed.iter().any(|l| n.start_line <= *l && *l <= n.end_line);
        if !touched { continue; }
        let code = |span: &Span, side: Side| -> Vec<(usize, String, &str)> {
            (span.start_line..=span.end_line)
                .filter_map(|l| {
                    let t = facts.line(side, l)?;
                    (!t.trim().is_empty() && !facts.is_comment(side, l)).then(|| (l, facts.format_key(side, l, t), t))
                })
                .collect()
        };
        let (a, b) = (code(o, Side::Old), code(n, Side::New));
        let pairs = align(a.len(), b.len(), |i, j| {
            (a[i].1 == b[j].1).then_some(if a[i].2 == b[j].2 { Noise::Moved } else { Noise::Formatting })
        });
        for (i, j, kind) in pairs {
            old_fmt.insert(a[i].0, kind);
            new_fmt.insert(b[j].0, kind);
        }
    }
    (old_fmt, new_fmt)
}

/// Annotate one file's hunks in place.
/// `moved_spans` are locations of whole items known to have moved unchanged.
/// `local_renames` apply to this file only.
#[allow(clippy::too_many_arguments)]
pub fn annotate_file(
    hunks: &mut [DiffHunk],
    manifest: &ChangeManifest,
    moved_spans: &[Location],
    ctx: &AnnotateContext,
    local_renames: &[(String, String)],
    facts: &FileFacts,
    generated: bool,
    detect_comments: bool,
) {
    let renames: Vec<(String, String)> = ctx.renames.iter().chain(local_renames).cloned().collect();
    let formatted = if generated { Default::default() } else { function_formatting(hunks, facts) };
    let opts = BlockOpts { renames: &renames, detect_comments, formatted: &formatted, facts };
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
    /// Lines `function_formatting` paired: (old, new).
    formatted: &'a (HashMap<usize, Noise>, HashMap<usize, Noise>),
    facts: &'a FileFacts<'a>,
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

    // Blank lines and comments. Comments come from the parser, so `* rate` continuing an expression is code.
    for c in block.iter_mut() {
        if c.noise.is_some() { continue; }
        let t = text(c);
        if t.trim().is_empty() {
            c.noise = Some(Noise::Formatting);
        } else if opts.detect_comments && is_comment(c, opts.facts) {
            if !crate::parser::is_directive(&t) { c.noise = Some(Noise::Comment); }
        } else if let Some((side, line, _)) = side_line(c) {
            if let Some(&kind) = FileFacts::pick(side, (&opts.formatted.0, &opts.formatted.1)).get(&line) {
                c.noise = Some(kind);
            }
        }
    }

    // Pair removed and added lines in order. Same tokens means formatting, same after renames
    // means rename. Keeping the order means a reorder or an edited line doesn't get paired
    // with a similar line somewhere else.
    let key = |c: &crate::manifest::Change, t: &str| match side_line(c) {
        Some((side, line, _)) => opts.facts.format_key(side, line, t),
        None => t.to_string(),
    };
    let removed: Vec<usize> = (0..block.len()).filter(|&k| block[k].kind == ChangeKind::Removed && block[k].noise.is_none()).collect();
    let added: Vec<usize> = (0..block.len()).filter(|&k| block[k].kind != ChangeKind::Removed && block[k].noise.is_none()).collect();
    let added_keys: Vec<String> = added.iter().map(|&k| key(&block[k], &text(&block[k]))).collect();
    let removed_keys: Vec<(String, Option<String>)> = removed.iter().map(|&k| {
        let t = text(&block[k]);
        let renamed = if opts.renames.is_empty() { t.clone() } else { apply_renames(&t, opts.renames) };
        (key(&block[k], &t), (renamed != t).then(|| key(&block[k], &renamed)))
    }).collect();
    let pairs = align(removed.len(), added.len(), |i, j| {
        let (plain, renamed) = &removed_keys[i];
        if *plain == added_keys[j] { Some(Noise::Formatting) } else if renamed.as_ref() == Some(&added_keys[j]) { Some(Noise::Rename) } else { None }
    });
    for (i, j, kind) in pairs {
        block[removed[i]].noise = Some(kind);
        block[added[j]].noise = Some(kind);
    }

    // Moved: inside a known moved item, or part of a run of lines that shows up in the
    // same order on the other side.
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
    for kind in [ChangeKind::Removed, ChangeKind::Added] {
        mark_moved_runs(block, kind, ctx, opts);
    }
}

fn is_comment(c: &crate::manifest::Change, facts: &FileFacts) -> bool {
    side_line(c).is_some_and(|(side, line, _)| facts.is_comment(side, line))
}

/// Pairs `n` removed lines with `m` added lines, keeping both in order (longest common
/// subsequence). `pair(i, j)` says whether two lines match and how.
fn align(n: usize, m: usize, pair: impl Fn(usize, usize) -> Option<Noise>) -> Vec<(usize, usize, Noise)> {
    if n == 0 || m == 0 { return vec![]; }
    let mut out = Vec::new();
    if n.saturating_mul(m) > MAX_ALIGN_CELLS {
        // Greedy: each removed line takes the next added line that matches.
        let mut next = 0;
        for i in 0..n {
            if let Some((j, kind)) = (next..m).find_map(|j| pair(i, j).map(|k| (j, k))) {
                out.push((i, j, kind));
                next = j + 1;
            }
        }
        return out;
    }
    // Longest common subsequence, filled from the end so we can walk it forward.
    let mut len = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            len[at(i, j)] = if pair(i, j).is_some() { len[at(i + 1, j + 1)] + 1 } else { len[at(i + 1, j)].max(len[at(i, j + 1)]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        match pair(i, j) {
            Some(kind) if len[at(i, j)] == len[at(i + 1, j + 1)] + 1 => { out.push((i, j, kind)); i += 1; j += 1; }
            _ if len[at(i + 1, j)] >= len[at(i, j + 1)] => i += 1,
            _ => j += 1,
        }
    }
    out
}

/// Marks runs of `kind` lines that show up in the same order in the other side's changes.
/// A run that only moved inside one function is a reorder, which can change behavior,
/// so it stays visible.
fn mark_moved_runs(block: &mut [crate::manifest::Change], kind: ChangeKind, ctx: &AnnotateContext, opts: &BlockOpts) {
    let is_kind = |c: &crate::manifest::Change| (c.kind == ChangeKind::Removed) == (kind == ChangeKind::Removed);
    // This side's code lines, in order: (index in block, line number, normalized text).
    let seq: Vec<(usize, usize, String)> = block.iter().enumerate()
        .filter(|(_, c)| is_kind(c) && !matches!(c.noise, Some(Noise::Comment)))
        .filter_map(|(k, c)| {
            let (side, line, text) = side_line(c)?;
            let n = opts.facts.move_key(side, line, text);
            (!n.is_empty()).then_some((k, line, n))
        })
        .collect();
    let other = if kind == ChangeKind::Removed { &ctx.added } else { &ctx.removed };
    let mut i = 0;
    while i < seq.len() {
        let mut best = 0;
        if block[seq[i].0].noise.is_none() {
            for &(r, p) in other.at.get(&seq[i].2).map(Vec::as_slice).unwrap_or_default().iter().take(64) {
                let run = &other.runs[r];
                let mut l = 0;
                while i + l < seq.len() && p + l < run.norms.len() && seq[i + l].2 == run.norms[p + l] {
                    l += 1;
                }
                if l <= best || seq[i..i + l].iter().filter(|x| is_nontrivial(&x.2)).count() < MIN_MOVED_RUN {
                    continue;
                }
                let here = (seq[i].1, seq[i + l - 1].1);
                let there = (run.lines[p], run.lines[p + l - 1]);
                let (old, new) = if kind == ChangeKind::Removed { (here, there) } else { (there, here) };
                // Inside the function, below its first line. A whole function moving is still a move.
                let inside = |s: &Span, side: Side, (a, b): (usize, usize)| {
                    let head = (s.start_line..=s.end_line)
                        .find(|&l| opts.facts.line(side, l).is_some_and(|t| !t.trim().is_empty()) && !opts.facts.is_comment(side, l))
                        .unwrap_or(s.start_line);
                    head < a && b <= s.end_line
                };
                let reordered = run.file == opts.facts.file
                    && opts.facts.bodies.iter().any(|(o, n)| inside(o, Side::Old, old) && inside(n, Side::New, new));
                if !reordered { best = l; }
            }
        }
        if best > 0 {
            for x in &seq[i..i + best] {
                if block[x.0].noise.is_none() { block[x.0].noise = Some(Noise::Moved); }
            }
            i += best;
        } else {
            i += 1;
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
