use crate::MultiFileResult;
use colored::Colorize;
use perspica_core::flow::StepKind;
use perspica_core::manifest::{Change, ChangeKind, DependencyChangeType, DiffHunk, Location, Noise};
use perspica_core::roles::FileRole;

/// Reading-order lines shown before truncating.
const MAX_READING_STEPS: usize = 30;

const CONTEXT_LINES: usize = 3;
const DIVIDER: &str = "────────────────────────────────────────────────────────────";

pub fn render_multi(multi: &MultiFileResult, color: bool, show_noise: bool) {
    colored::control::set_override(color);
    println!();

    let (added, removed, changed, mechanical) = totals(multi);
    let tests: usize = multi.results.iter().map(|r| r.review.test_lines).sum();
    let title = if multi.source.label.is_empty() { "Changes".to_string() } else { multi.source.label.clone() };
    println!(
        "  {} {}  {}  {}  {}",
        "◆".green().bold(),
        title.bold(),
        format!("{} file{}", multi.files.len(), pl(multi.files.len())).dimmed(),
        format!("+{added}").green().bold(),
        format!("-{removed}").red().bold(),
    );
    if let Some(t) = &multi.source.pr_title {
        println!("    {}", t.white());
    }
    if changed > 0 && (mechanical > 0 || tests > 0) {
        let pct = mechanical * 100 / changed;
        let test_part = if tests > 0 { format!(", {} are tests", tests.to_string().bold()) } else { String::new() };
        println!(
            "    {} {} of {} changed lines are mechanical{}, {} need review",
            "≈".dimmed(),
            mechanical.to_string().bold(),
            changed,
            test_part,
            changed.saturating_sub(mechanical + tests).to_string().bold(),
        );
        if pct >= 50 {
            println!("      {}", "(formatting, comments, renames, moves and generated files are collapsed below)".dimmed());
        }
    }
    println!();

    render_requirements(multi);
    render_attention(multi);

    if let Some(ref groups) = multi.intent_groups {
        println!("  {} {}", "◆".cyan().bold(), "What Changed".bold());
        println!("  {}", DIVIDER.dimmed());
        for (i, group) in groups.iter().enumerate() {
            let risk = match group.risk.as_deref() {
                Some("high") => " high risk ".on_red().white().bold().to_string(),
                Some("medium") => " medium ".on_yellow().black().to_string(),
                Some("low") => " low ".dimmed().to_string(),
                _ => String::new(),
            };
            let label = if group.mechanical { group.label.dimmed().to_string() } else { group.label.white().bold().to_string() };
            let origin = match group.origin.as_deref() {
                Some("requested") => format!(" {}", "asked".green().dimmed()),
                Some("mixed") => format!(" {}", "partly asked".yellow()),
                Some("autonomous") => format!(" {}", "agent's call".yellow().bold()),
                _ => String::new(),
            };
            println!("  {}  {} {}{}", format!("{}.", i + 1).cyan().bold(), label, risk, origin);
            if let Some(r) = &group.requirement {
                println!("       {}", format!("“{}” ({})", r.quote, r.id).dimmed());
            }
            if let Some(note) = &group.review_note {
                println!("       {} {}", "verify:".yellow(), note);
            }
            if group.mechanical {
                println!("       {}", format!("{} entries", group.entry_ids.len()).dimmed());
                continue;
            }
            for sg in &group.sub_groups {
                if group.sub_groups.len() > 1 || sg.label != group.label {
                    println!("       {} {}", "├".dimmed(), sg.label.bold());
                }
                for desc in &sg.descriptions {
                    println!("       {}   {}", "│".dimmed(), desc.dimmed());
                }
            }
        }
        println!();
    }

    if let Some(ref summary) = multi.summary {
        println!("  {} {}", "◆".magenta().bold(), "Summary".bold());
        println!("  {}", DIVIDER.dimmed());
        for line in word_wrap(summary, 74) {
            println!("    {line}");
        }
        println!();
    }

    if multi.intent_groups.is_none() {
        render_manifest(multi);
        println!();
    }

    render_reading_order(multi);

    // Source first, then tests, docs, and generated/vendored files.
    let rank = |r: &perspica_core::DiffResult| match r.review.role {
        FileRole::Source => 0,
        FileRole::Test => 1,
        FileRole::Docs => 2,
        FileRole::Generated | FileRole::Vendored => 3,
    };
    let mut order: Vec<usize> = (0..multi.results.len()).collect();
    order.sort_by_key(|&i| rank(&multi.results[i]));
    for i in order {
        let result = &multi.results[i];
        let file = &multi.files[i];
        let (fa, fd) = count(&result.hunks);
        let status = match file.status.as_str() {
            "A" => " new".green().to_string(),
            "D" => " deleted".red().to_string(),
            "R" => format!(" renamed from {}", file.old_path).cyan().to_string(),
            _ => String::new(),
        };
        let role = match result.review.role {
            FileRole::Test => " test".yellow().dimmed().to_string(),
            FileRole::Docs => " docs".dimmed().to_string(),
            FileRole::Vendored => " vendored".dimmed().to_string(),
            _ => String::new(),
        };
        println!(
            "  {} {}{}{}  {}  {}",
            "━━".blue().bold(),
            file.new_path.white().bold(),
            status,
            role,
            format!("+{fa}").green(),
            format!("-{fd}").red(),
        );
        if file.binary {
            println!("    {}\n", "binary file changed".dimmed());
            continue;
        }
        if result.review.generated && !show_noise {
            let what = if result.review.role == FileRole::Vendored { "vendored" } else { "generated" };
            println!("    {}\n", format!("{what} file, {} changed lines hidden (--show-noise to show)", fa + fd).dimmed());
            continue;
        }
        if !result.review.parsed && !result.review.generated && file.language == "Unknown" {
            println!("    {}", "not semantically analyzed (unsupported language)".dimmed());
        }
        for hunk in &result.hunks {
            render_hunk(hunk, show_noise);
        }
        println!();
    }
}

/// What the user asked the coding agent for, in their own words.
fn render_requirements(multi: &MultiFileResult) {
    const SHOWN: usize = 6;
    let ctx = &multi.source.sessions;
    if ctx.is_empty() {
        return;
    }
    let n = ctx.sessions.len();
    let mut agents: Vec<&str> = ctx.sessions.iter().map(|s| match s.agent.as_str() { "claude-code" => "Claude Code", "codex" => "Codex", other => other }).collect();
    agents.dedup();
    println!("  {} {}  {}", "◆".yellow().bold(), "Asked For".bold(),
        format!("your prompts in {n} {} session{} that edited these files", agents.join(" and "), pl(n)).dimmed());
    println!("  {}", DIVIDER.dimmed());
    let skip = ctx.requirements.len().saturating_sub(SHOWN);
    if skip > 0 {
        println!("    {}", format!("… {skip} earlier").dimmed());
    }
    for r in &ctx.requirements[skip..] {
        let text: String = r.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let short: String = text.chars().take(100).collect();
        let more = if text.chars().count() > 100 { "…" } else { "" };
        println!("    {} {short}{more}", r.id.dimmed());
    }
    println!();
}

/// Changed code in reading order: types first, then call flows from entry points.
fn render_reading_order(multi: &MultiFileResult) {
    let steps = &multi.cross_file.reading_order;
    let functions = steps.iter().filter(|s| s.kind == StepKind::Function).count();
    if functions < 2 {
        return;
    }
    let reach: std::collections::HashMap<(&str, &str), &perspica_core::flow::TestReach> = multi.cross_file.test_reach.iter()
        .map(|t| ((t.file.as_str(), t.name.as_str()), t))
        .collect();
    let any_tests = multi.results.iter().any(|r| r.review.test_lines > 0);
    println!("  {} {}", "◆".blue().bold(), "Reading Order".bold());
    println!("  {}", DIVIDER.dimmed());
    // A class and its companion object share a name: list it once.
    let mut types: Vec<&str> = steps.iter().filter(|s| s.kind == StepKind::Type).map(|s| s.name.as_str()).collect();
    types.dedup();
    if !types.is_empty() {
        println!("    {} {}", "types:".dimmed(), types.join(", "));
    }
    let flow: Vec<_> = steps.iter().filter(|s| s.kind == StepKind::Function).collect();
    for (shown, s) in flow.iter().enumerate() {
        if shown == MAX_READING_STEPS {
            println!("    {}", format!("… {} more", flow.len() - shown).dimmed());
            break;
        }
        let indent = "  ".repeat(s.depth.min(8));
        let marker = if s.depth == 0 { "▸".blue().bold().to_string() } else { "└".dimmed().to_string() };
        let name = if s.repeat { format!("{} (above)", s.name).dimmed().to_string() } else if s.depth == 0 { s.name.bold().to_string() } else { s.name.normal().to_string() };
        let tested = match reach.get(&(s.file.as_str(), s.name.as_str())) {
            _ if s.repeat || !any_tests => String::new(),
            Some(t) if !t.via.is_empty() => format!("  {}", "✓ tested".green().dimmed()),
            Some(_) => format!("  {}", "not reached by changed tests".dimmed()),
            None => String::new(),
        };
        println!("    {indent}{marker} {name}  {}{tested}", short_path(&s.file).dimmed());
    }
    if !any_tests {
        println!("    {}", "no tests changed in this diff".dimmed());
    }
    println!();
}

fn render_attention(multi: &MultiFileResult) {
    let cf = &multi.cross_file;
    let mut lines: Vec<String> = Vec::new();
    for b in &cf.broken_references {
        let scope = if b.in_diff { "" } else { " (file not in this diff)" };
        lines.push(format!(
            "{}  {}:{} {}{}\n         {}",
            "✗".red().bold(),
            b.reference_file,
            b.reference_location.line_start,
            b.reason,
            scope.dimmed(),
            b.line_text.dimmed(),
        ));
    }
    for s in &cf.signature_impacts {
        let stale: Vec<_> = s.call_sites.iter().filter(|c| !c.updated).collect();
        if stale.is_empty() { continue; }
        let mut l = format!(
            "{}  {} changed ({}): {} call site{} not updated:",
            "!".yellow().bold(),
            s.name.yellow().bold(),
            s.description,
            stale.len(),
            pl(stale.len()),
        );
        for c in stale.iter().take(5) {
            l.push_str(&format!("\n         {}:{}  {}", c.file, c.line, c.text.dimmed()));
        }
        if stale.len() > 5 {
            l.push_str(&format!("\n         … {} more", stale.len() - 5));
        }
        lines.push(l);
    }
    for r in &multi.results {
        for d in &r.manifest.dead_code {
            lines.push(format!("{}  {} {} {}", "☠".red(), d.name.red(), d.reason.dimmed(), short_loc(&d.location).dimmed()));
        }
    }
    for c in &multi.concerns {
        lines.push(format!("{}  {}", "?".magenta().bold(), c));
    }
    if lines.is_empty() {
        return;
    }
    println!("  {} {}", "◆".red().bold(), "Needs Attention".bold());
    println!("  {}", DIVIDER.dimmed());
    for l in lines {
        println!("    {l}");
    }
    println!();
}

fn render_manifest(multi: &MultiFileResult) {
    println!("  {} {}", "◆".green().bold(), "Changes".bold());
    println!("  {}", DIVIDER.dimmed());
    let multi_file = multi.files.len() > 1;
    let loc = |l: &Location| if multi_file { format!("  {}", short_loc(l)).dimmed().to_string() } else { String::new() };
    let mut mechanical = 0usize;
    let mut test_changes = 0usize;
    for result in &multi.results {
        let m = &result.manifest;
        let is_test = |id: u32| m.test_entries.contains(&id);
        test_changes += m.test_entries.len();
        for s in m.signature_changes.iter().filter(|e| !is_test(e.id)) {
            println!("    {}  {}: {}{}", "σ".yellow().bold(), s.name.yellow().bold(), s.description, loc(&s.location));
        }
        // A test file's imports are part of its test changes, not the change itself.
        let test_file = result.review.role == FileRole::Test;
        for d in &m.dependency_changes {
            if test_file || is_test(d.id) {
                test_changes += !is_test(d.id) as usize;
                continue;
            }
            let (sym, name) = match d.change_type {
                DependencyChangeType::Added => ("+".green().bold(), d.name.green().bold()),
                DependencyChangeType::Removed => ("−".red().bold(), d.name.red()),
                DependencyChangeType::Changed => ("~".yellow().bold(), d.name.normal()),
            };
            let mut detail = String::new();
            if !d.symbols_added.is_empty() { detail.push_str(&format!(" +{}", d.symbols_added.join(", +"))); }
            if !d.symbols_removed.is_empty() { detail.push_str(&format!(" −{}", d.symbols_removed.join(", −"))); }
            let internal = if d.internal { " (internal)".dimmed().to_string() } else { String::new() };
            println!("    {}  {}{}{}{}", sym, name, detail.dimmed(), internal, d.used_in.first().map(&loc).unwrap_or_default());
        }
        for e in &m.extracted_functions {
            println!("    {}  {} → [{}]", "⊕".magenta().bold(), e.original_name.magenta(), e.extracted_names.join(" + ").magenta().bold());
        }
        for l in m.logic_changes.iter().filter(|e| !is_test(e.id)) {
            println!("    {}  {}: {}{}", "Δ".white().bold(), l.name.white().bold(), l.description.dimmed(), loc(&l.location));
        }
        for r in m.renames.iter().filter(|e| !is_test(e.id)) {
            println!("    {}  {} {} {}{}", "↔".cyan(), r.old_name.cyan(), "→".dimmed(), r.new_name.cyan().bold(),
                r.locations.first().map(&loc).unwrap_or_default());
        }
        mechanical += m.moved_code.len() + m.formatting_only.len();
    }
    for mv in &multi.cross_file.moves {
        let renamed = mv.renamed_to.as_ref().map(|n| format!(" as {n}")).unwrap_or_default();
        let edited = if mv.modified { " (edited)" } else { "" };
        println!("    {}  {} moved {} → {}{}{}", "⇄".cyan(), mv.name.cyan(), mv.from_file.dimmed(), mv.to_file, renamed, edited.yellow());
    }
    if test_changes > 0 {
        println!("    {}  {}", "✓".yellow().dimmed(), format!("{test_changes} test change{}", pl(test_changes)).dimmed());
    }
    if mechanical > 0 {
        println!("    {}  {}", "~".dimmed(), format!("{mechanical} formatting-only or unchanged-move region{}", pl(mechanical)).dimmed());
    }
}

fn render_hunk(hunk: &DiffHunk, show_noise: bool) {
    let (a, d) = count(std::slice::from_ref(hunk));
    let header = format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_range.start,
        hunk.old_range.end.saturating_sub(hunk.old_range.start) + 1,
        hunk.new_range.start,
        hunk.new_range.end.saturating_sub(hunk.new_range.start) + 1,
    );
    if let (Some(noise), false) = (hunk.noise, show_noise) {
        println!("    {}  {}", header.blue().dimmed(), format!("~ {} line{}, {}", a + d, pl(a + d), noise_label(noise)).dimmed().italic());
        return;
    }
    if hunk.test {
        println!("    {}  {}", header.blue().dimmed(), "test".yellow().dimmed());
    } else {
        println!("    {}", header.blue().dimmed());
    }

    let changes = &hunk.changes;
    let keep = keep_mask(changes, CONTEXT_LINES);
    let mut hidden = 0u32;
    for (ci, ch) in changes.iter().enumerate() {
        if !keep[ci] {
            hidden += 1;
            continue;
        }
        if hidden > 0 {
            println!("         {}", format!("⋮ {hidden} lines hidden ⋮").dimmed());
            hidden = 0;
        }
        let old_ln = ch.old_span.as_ref().map(|s| s.start_line);
        let new_ln = ch.new_span.as_ref().map(|s| s.start_line);
        let quiet = ch.noise.is_some();
        match ch.kind {
            ChangeKind::Context => {
                let text = ch.content_old.as_deref().unwrap_or("");
                println!("    {} {} {}", fmt_ln(old_ln, new_ln).dimmed(), "│".dimmed(), text.dimmed());
            }
            ChangeKind::Added => {
                let text = format!("+{}", ch.content_new.as_deref().unwrap_or(""));
                let body = if quiet { text.green().dimmed().to_string() } else { text.green().bold().to_string() };
                println!("    {} {} {}{}", fmt_ln(None, new_ln).green(), "│".green(), body, tag(ch));
            }
            ChangeKind::Removed => {
                let text = format!("-{}", ch.content_old.as_deref().unwrap_or(""));
                let body = if quiet { text.red().dimmed().to_string() } else { text.red().to_string() };
                println!("    {} {} {}{}", fmt_ln(old_ln, None).red(), "│".red(), body, tag(ch));
            }
            ChangeKind::Modified | ChangeKind::Moved => {
                let text = ch.content_new.as_deref().or(ch.content_old.as_deref()).unwrap_or("");
                println!("    {} {} {}", "         ".dimmed(), "│".dimmed(), format!("~{text}").dimmed().italic());
            }
        }
    }
    if hidden > 0 {
        println!("         {}", format!("⋮ {hidden} lines hidden ⋮").dimmed());
    }
}

fn tag(ch: &Change) -> String {
    match ch.noise {
        Some(Noise::Rename) => "  ↔".cyan().dimmed().to_string(),
        Some(Noise::Moved) => "  ⇄".cyan().dimmed().to_string(),
        _ => String::new(),
    }
}

fn noise_label(n: Noise) -> &'static str {
    match n {
        Noise::Formatting => "formatting only",
        Noise::Comment => "comments only",
        Noise::Rename => "rename only",
        Noise::Moved => "moved, unchanged",
        Noise::Generated => "generated",
    }
}

fn totals(multi: &MultiFileResult) -> (usize, usize, usize, usize) {
    let (mut a, mut d, mut c, mut m) = (0, 0, 0, 0);
    for r in &multi.results {
        let (fa, fd) = count(&r.hunks);
        a += fa;
        d += fd;
        c += r.review.changed_lines;
        m += r.review.mechanical_lines;
    }
    (a, d, c, m)
}

fn count(hunks: &[DiffHunk]) -> (usize, usize) {
    let (mut add, mut del) = (0, 0);
    for ch in hunks.iter().flat_map(|h| h.changes.iter()) {
        match ch.kind {
            ChangeKind::Added => add += 1,
            ChangeKind::Removed => del += 1,
            _ => {}
        }
    }
    (add, del)
}

fn keep_mask(changes: &[Change], ctx: usize) -> Vec<bool> {
    let mut keep = vec![false; changes.len()];
    for (i, ch) in changes.iter().enumerate() {
        if ch.kind != ChangeKind::Context {
            for k in &mut keep[i.saturating_sub(ctx)..(i + ctx + 1).min(changes.len())] {
                *k = true;
            }
        }
    }
    keep
}

fn fmt_ln(old: Option<usize>, new: Option<usize>) -> String {
    match (old, new) {
        (Some(o), Some(n)) => format!("{o:>4} {n:>4}"),
        (Some(o), None) => format!("{o:>4}     "),
        (None, Some(n)) => format!("     {n:>4}"),
        (None, None) => "          ".to_string(),
    }
}

fn short_loc(loc: &Location) -> String {
    match &loc.file {
        Some(f) => format!("{}:{}", short_path(f), loc.line_start),
        None => format!("L{}", loc.line_start),
    }
}

fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').collect();
    if parts.len() > 2 { format!("…/{}", parts[parts.len() - 2..].join("/")) } else { p.to_string() }
}

fn pl(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn word_wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.len() + word.len() + 1 > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() { current.push(' '); }
        current.push_str(word);
    }
    if !current.is_empty() { lines.push(current); }
    lines
}
