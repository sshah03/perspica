use crate::llm::LlmProvider;
use perspica_core::cross_file::CrossFileManifest;
use perspica_core::manifest::{DependencyChangeType, Location, Side};
use perspica_core::DiffResult;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubGroup {
    pub label: String,
    pub entry_ids: Vec<u32>,
    /// One description per entry id (same order).
    pub descriptions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentGroup {
    pub label: String,
    pub entry_ids: Vec<u32>,
    /// One description per entry id (same order).
    #[serde(default)]
    pub descriptions: Vec<String>,
    #[serde(default)]
    pub sub_groups: Vec<SubGroup>,
    /// "low" | "medium" | "high": how much scrutiny this group deserves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    /// What a reviewer should verify for this group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_note: Option<String>,
    /// Deterministic group of mechanical changes (formatting, renames, moves), never sent to the LLM.
    #[serde(default)]
    pub mechanical: bool,
    /// With agent-session requirements: "requested" (the user asked for it, with a
    /// verified quote), "autonomous" (the agent's own decision), or "mixed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement: Option<RequirementQuote>,
}

/// The user's words a requested group traces back to, checked verbatim against the prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequirementQuote {
    pub id: String,
    pub quote: String,
}

#[derive(Debug)]
pub struct IntelResult {
    pub groups: Vec<IntentGroup>,
    pub summary: String,
    pub concerns: Vec<String>,
}

/// Everything the LLM analysis needs about the diff.
pub struct IntelInput<'a> {
    pub results: &'a [DiffResult],
    /// (path, old_source, new_source), parallel to `results`.
    pub sources: &'a [(String, String, String)],
    pub cross_file: &'a CrossFileManifest,
    /// Author-provided context: PR title/body or commit messages.
    pub author_context: Option<&'a str>,
    /// The user's own prompts from the coding-agent sessions that made the change.
    pub requirements: &'a [crate::sessions::Requirement],
}

#[derive(Deserialize)]
struct LlmResponse {
    groups: Vec<LlmGroup>,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    concerns: Vec<String>,
}

#[derive(Deserialize)]
struct LlmSubGroup {
    label: String,
    #[serde(default)]
    entry_ids: Vec<u32>,
}

#[derive(Deserialize)]
struct LlmGroup {
    label: String,
    #[serde(default)]
    entry_ids: Vec<u32>,
    #[serde(default)]
    sub_groups: Vec<LlmSubGroup>,
    #[serde(default)]
    risk: Option<String>,
    #[serde(default)]
    review_note: Option<String>,
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    requirement: Option<String>,
    #[serde(default)]
    quote: Option<String>,
}

/// Output contract shared by standard and deep analysis.
pub const OUTPUT_SPEC: &str = r#"OUTPUT: Respond with ONLY this JSON, no markdown fences, no explanation:
{
  "groups": [
    {
      "label": "Intent, 3-8 words",
      "risk": "low | medium | high",
      "review_note": "What to verify, max 20 words",
      "entry_ids": [1, 3],
      "sub_groups": [ { "label": "5-10 words", "entry_ids": [4, 5] } ]
    }
  ],
  "summary": "2-3 sentences, max 70 words: what changed and why.",
  "concerns": ["Max 5 items, max 30 words each. Empty list if none."]
}

RULES:
- Every ID listed under CHANGES must appear exactly once, in a group's entry_ids or in one of its sub_groups. Do not invent IDs.
- Each group is one developer INTENT (why the changes were made), which often spans several files.
  Do not make one group per file or module unless that file really is a separate intent.
- Use sub_groups only when a group has 2+ clearly distinct parts; otherwise omit sub_groups and use entry_ids.
- Order groups the way a reviewer should read them: foundations (types, dependencies, APIs) before their users.
- risk: "high" for behavior changes on critical paths, security, data handling, public API breaks, or
  stale references; "low" for renames, identical-behavior refactors, tests, docs.
- Concerns must be supported by the CHANGES or CONTEXT shown. Do not speculate about code you cannot see;
  if you are unsure, leave it out. If AUTHOR CONTEXT is given, note significant changes it does not mention.
- Be terse. Refer to changes by symbol and file name, never by ID (readers never see IDs).
- Do NOT include any text outside the JSON object."#;

/// Extra output fields and rules when agent-session requirements are available.
pub const REQUIREMENTS_SPEC: &str = r#"REQUIREMENTS RULES (the USER REQUIREMENTS section is present):
- Add to every group: "origin": "requested" | "autonomous" | "mixed".
  "requested": the user asked for this work. Also add "requirement": "R<n>" and "quote": the user's exact words
  (3-15 words, copied verbatim from that requirement) that ask for it.
  "autonomous": the coding agent decided this on its own: a design choice, extra refactor, new behavior or
  default the user never asked for. Reviewers check these first, so be honest: when the user only gave a goal
  ("make it better"), the specific choices are autonomous.
  "mixed": requested work that includes significant unrequested choices; give requirement and quote too.
- Requirements the diff does not seem to address may be listed in concerns."#;

/// Run the LLM analysis: one call for intent grouping and a summary.
pub async fn run_analysis(input: &IntelInput<'_>, provider: &dyn LlmProvider) -> Result<IntelResult, String> {
    if llm_entry_ids(input).is_empty() {
        return Ok(IntelResult {
            groups: mechanical_group(&all_entries(input)).into_iter().collect(),
            summary: "Only mechanical changes (formatting, comments, renames, or unchanged moves). No behavior change detected.".into(),
            concerns: vec![],
        });
    }
    let prompt = build_prompt(input, None);
    let response = provider.complete(&prompt).await?;
    parse_and_validate(&response, input)
}

/// The full prompt. `extra` is appended context (deep mode adds file structures etc.).
pub fn build_prompt(input: &IntelInput<'_>, extra: Option<&str>) -> String {
    let changes = build_entry_listing(input);
    let context = build_rich_context(input.results, input.sources);
    let author = input.author_context
        .filter(|c| !c.trim().is_empty())
        .map(|c| format!("AUTHOR CONTEXT (PR description / commit messages; may be incomplete):\n{}\n\n", truncate(c, 3000)))
        .unwrap_or_default();
    let extra = extra.map(|e| format!("\n{e}\n")).unwrap_or_default();
    let (reqs, req_spec) = if input.requirements.is_empty() {
        (String::new(), String::new())
    } else {
        let list: String = input.requirements.iter().map(|r| format!("[{}] {}\n", r.id, r.text)).collect();
        (
            format!("USER REQUIREMENTS (the user's own messages to the coding agent that made this change, oldest first; \
                     later messages may refine or override earlier ones):\n{}\n", truncate(&list, 8000)),
            format!("\n\n{REQUIREMENTS_SPEC}"),
        )
    };
    format!(
        "You are a senior engineer reviewing a code change. Group the classified changes by developer intent, \
         assess review risk, and write a concise PR summary.\n\n\
         {author}{reqs}CHANGES (id [type] location name: detail; one id may cover several related changes):\n{changes}\n\nCONTEXT:\n{context}\n{extra}\n{OUTPUT_SPEC}{req_spec}"
    )
}

/// Entry ids the LLM must group (mechanical entries are grouped deterministically).
pub fn llm_entry_ids(input: &IntelInput<'_>) -> Vec<u32> {
    all_entries(input).into_iter().filter(|e| !e.mechanical).map(|e| e.id).collect()
}

struct Entry {
    id: u32,
    kind: &'static str,
    loc: String,
    text: String,
    mechanical: bool,
    /// Index into `results` for per-file entries.
    file: Option<usize>,
    /// The symbol the entry is about, for merging related entries.
    symbol: Option<String>,
}

impl Entry {
    fn line(&self) -> String {
        format!("#{} [{}] {} {}", self.id, self.kind, self.loc, self.text)
    }
    fn description(&self) -> String {
        format!("{}  {}", self.text, self.loc)
    }
}

fn all_entries(input: &IntelInput<'_>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let e = |id, kind, loc: String, text: String, mechanical| Entry { id, kind, loc, text, mechanical, file: None, symbol: None };
    for (fi, r) in input.results.iter().enumerate() {
        let start = out.len();
        let m = &r.manifest;
        let test = |id: u32, kind: &'static str| if m.test_entries.contains(&id) { "test" } else { kind };
        for x in &m.renames {
            out.push(Entry { symbol: Some(x.new_name.clone()), ..e(x.id, test(x.id, "rename"), fmt_loc(x.locations.first()), format!("{} → {}", x.old_name, x.new_name), false) });
        }
        for x in &m.signature_changes {
            out.push(Entry { symbol: Some(x.name.clone()), ..e(x.id, test(x.id, "signature"), fmt_loc(Some(&x.location)), format!("{}: {}", x.name, x.description), false) });
        }
        for x in &m.dependency_changes {
            let text = match x.change_type {
                DependencyChangeType::Added => format!("+ {}{}", x.name, syms(&x.symbols_added)),
                DependencyChangeType::Removed => format!("- {}", x.name),
                DependencyChangeType::Changed => {
                    let parts: Vec<String> = x.symbols_added.iter().map(|s| format!("+{s}"))
                        .chain(x.symbols_removed.iter().map(|s| format!("-{s}")))
                        .collect();
                    format!("~ {} ({})", x.name, parts.join(", "))
                }
            };
            let kind = if x.internal { "import" } else { "dependency" };
            out.push(e(x.id, kind, fmt_loc(x.used_in.first()), text, false));
        }
        for x in &m.extracted_functions {
            out.push(Entry { symbol: Some(x.original_name.clone()), ..e(x.id, "extraction", fmt_loc(Some(&x.location_original)), format!("{} → [{}]", x.original_name, x.extracted_names.join(" + ")), false) });
        }
        for x in &m.dead_code {
            out.push(Entry { symbol: Some(x.name.clone()), ..e(x.id, "dead_code", fmt_loc(Some(&x.location)), format!("{} ({})", x.name, x.reason), false) });
        }
        for x in &m.logic_changes {
            out.push(Entry { symbol: Some(x.name.clone()), ..e(x.id, test(x.id, "logic"), fmt_loc(Some(&x.location)), format!("{}: {}", x.name, x.description), false) });
        }
        for x in &m.moved_code {
            out.push(e(x.id, "moved", fmt_loc(Some(&x.to_location)), format!("{} moved within file (unchanged)", x.name), true));
        }
        for x in &m.formatting_only {
            out.push(e(x.id, "formatting", fmt_loc(Some(&x.location)), x.description.clone(), true));
        }
        for entry in &mut out[start..] {
            entry.file = Some(fi);
        }
    }
    let cf = input.cross_file;
    for x in &cf.moves {
        let mut text = format!("{} moved {} → {}", x.name, x.from_file, x.to_file);
        if let Some(n) = &x.renamed_to { text.push_str(&format!(" (renamed to {n})")); }
        if x.modified { text.push_str(" (edited)"); }
        out.push(e(x.id, "cross_file_move", fmt_loc(Some(&x.to_location)), text, !x.modified && x.renamed_to.is_none()));
    }
    for x in &cf.broken_references {
        out.push(e(x.id, "stale_reference", fmt_loc(Some(&x.reference_location)), format!("{}: `{}`", x.reason, x.line_text), false));
    }
    for x in &cf.signature_impacts {
        let stale = x.call_sites.iter().filter(|c| !c.updated).count();
        if stale == 0 { continue; } // fully updated call sites are covered by the signature entry
        out.push(e(x.id, "call_sites", fmt_loc(Some(&x.definition)), format!("{}: {} call site(s) not updated in this diff", x.name, stale), false));
    }
    out
}

fn syms(s: &[String]) -> String {
    if s.is_empty() { String::new() } else { format!(" {{{}}}", s.join(", ")) }
}

/// Below this many entries, the model groups individual entries; above it,
/// related entries are pre-clustered into units so the response stays short.
const UNIT_THRESHOLD: usize = 40;
/// Hunks touching more entries than this (e.g. a whole new file) are split
/// into chunks instead of becoming one giant unit.
const MAX_HUNK_MERGE: usize = 12;
const CHUNK: usize = 8;

/// A set of related entries the model groups as one item.
pub struct Unit {
    pub id: u32,
    pub entries: Vec<u32>,
    line: String,
}

/// Cluster non-mechanical entries into units. Entries that share a diff hunk
/// or are about the same symbol in the same file are merged. Small diffs keep
/// one unit per entry (unit id = entry id).
pub fn build_units(input: &IntelInput<'_>) -> Vec<Unit> {
    let entries: Vec<Entry> = all_entries(input).into_iter().filter(|e| !e.mechanical).collect();
    if entries.len() <= UNIT_THRESHOLD {
        return entries.iter().map(|e| Unit { id: e.id, entries: vec![e.id], line: e.line() }).collect();
    }
    let index: std::collections::HashMap<u32, usize> = entries.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
    let mut parent: Vec<usize> = (0..entries.len()).collect();
    fn find(p: &mut [usize], x: usize) -> usize {
        let mut r = x;
        while p[r] != r { r = p[r]; }
        let mut c = x;
        while p[c] != r { let n = p[c]; p[c] = r; c = n; }
        r
    }
    let union = |p: &mut Vec<usize>, a: usize, b: usize| { let (ra, rb) = (find(p, a), find(p, b)); if ra != rb { p[rb.max(ra)] = rb.min(ra); } };

    // Same symbol in the same file (e.g. signature + body change of one function).
    let mut by_symbol: std::collections::HashMap<(usize, &str), usize> = std::collections::HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if let (Some(f), Some(sym)) = (e.file, e.symbol.as_deref()) {
            match by_symbol.get(&(f, sym)) {
                Some(&j) => union(&mut parent, i, j),
                None => { by_symbol.insert((f, sym), i); }
            }
        }
    }
    // Entries that share a diff hunk.
    for r in input.results {
        for h in &r.hunks {
            let ids: Vec<usize> = h.manifest_refs.iter().filter_map(|id| index.get(id).copied()).collect();
            let chunks: Vec<&[usize]> = if ids.len() > MAX_HUNK_MERGE { ids.chunks(CHUNK).collect() } else { vec![&ids[..]] };
            for chunk in chunks {
                for w in chunk.windows(2) { union(&mut parent, w[0], w[1]); }
            }
        }
    }

    let mut clusters: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..entries.len() {
        let root = find(&mut parent, i);
        clusters.entry(root).or_default().push(i);
    }
    clusters.into_values().enumerate().map(|(k, members)| {
        let id = k as u32 + 1;
        let first = &entries[members[0]];
        let mut kinds: Vec<&str> = members.iter().map(|&i| entries[i].kind).collect();
        kinds.dedup();
        let mut text = String::new();
        for (n, &i) in members.iter().enumerate() {
            let t = &entries[i].text;
            if text.len() + t.len() > 260 {
                text.push_str(&format!("; (+{} more)", members.len() - n));
                break;
            }
            if !text.is_empty() { text.push_str("; "); }
            text.push_str(t);
        }
        Unit {
            id,
            entries: members.iter().map(|&i| entries[i].id).collect(),
            line: format!("#{id} [{}] {} {}", kinds.join("+"), first.loc, text),
        }
    }).collect()
}

/// (non-mechanical entries, units sent to the model).
pub fn llm_item_counts(input: &IntelInput<'_>) -> (usize, usize) {
    (llm_entry_ids(input).len(), build_units(input).len())
}

fn build_entry_listing(input: &IntelInput<'_>) -> String {
    let entries = all_entries(input);
    let mechanical = entries.iter().filter(|e| e.mechanical).count();
    let mut lines: Vec<String> = build_units(input).into_iter().map(|u| u.line).collect();
    if lines.is_empty() {
        lines.push("(no semantic changes, only mechanical edits)".into());
    }
    if mechanical > 0 {
        lines.push(format!("({mechanical} mechanical entries (formatting, renames, unchanged moves) are grouped separately; ignore them)"));
    }
    lines.join("\n")
}

/// Parse the LLM response and repair it: drop unknown/duplicate ids, collect
/// ungrouped ids into an "Other changes" group, append the mechanical group,
/// and attach one description per id.
pub fn parse_and_validate(response: &str, input: &IntelInput<'_>) -> Result<IntelResult, String> {
    let json_str = extract_json(response);
    let parsed: LlmResponse =
        serde_json::from_str(json_str).map_err(|e| format!("Failed to parse LLM response: {e}"))?;

    let entries = all_entries(input);
    let desc: BTreeMap<u32, String> = entries.iter().map(|e| (e.id, e.description())).collect();
    // The model answers in unit ids; expand them back to entry ids.
    let units = build_units(input);
    let unit_entries: std::collections::HashMap<u32, &Vec<u32>> = units.iter().map(|u| (u.id, &u.entries)).collect();
    let mut seen: HashSet<u32> = HashSet::new();
    let mut take = |ids: Vec<u32>| -> Vec<u32> {
        ids.into_iter()
            .filter(|id| unit_entries.contains_key(id) && seen.insert(*id))
            .flat_map(|id| unit_entries[&id].iter().copied())
            .collect()
    };
    let describe = |ids: &[u32]| -> Vec<String> {
        ids.iter().map(|id| desc.get(id).cloned().unwrap_or_else(|| format!("change #{id}"))).collect()
    };

    let mut groups = Vec::new();
    let reqs = input.requirements;
    for g in parsed.groups {
        let (origin, requirement) = validate_origin(&g, reqs);
        // Group-level ids and sub_groups may both be present; keep both.
        let mut raw_subs = Vec::new();
        if !g.entry_ids.is_empty() {
            raw_subs.push(LlmSubGroup { label: g.label.clone(), entry_ids: g.entry_ids });
        }
        raw_subs.extend(g.sub_groups);
        let sub_groups: Vec<SubGroup> = raw_subs.into_iter()
            .map(|sg| {
                let ids = take(sg.entry_ids);
                SubGroup { label: sg.label, descriptions: describe(&ids), entry_ids: ids }
            })
            .filter(|sg| !sg.entry_ids.is_empty())
            .collect();
        if sub_groups.is_empty() {
            continue;
        }
        let ids: Vec<u32> = sub_groups.iter().flat_map(|s| s.entry_ids.iter().copied()).collect();
        groups.push(IntentGroup {
            label: g.label,
            descriptions: describe(&ids),
            entry_ids: ids,
            sub_groups,
            risk: g.risk.map(|r| r.to_lowercase()).filter(|r| ["low", "medium", "high"].contains(&r.as_str())),
            review_note: g.review_note.filter(|n| !n.trim().is_empty()),
            mechanical: false,
            origin,
            requirement,
        });
    }

    let mut missing: Vec<u32> = units.iter().filter(|u| !seen.contains(&u.id)).flat_map(|u| u.entries.iter().copied()).collect();
    missing.sort_unstable();
    if !missing.is_empty() {
        groups.push(IntentGroup {
            label: "Other changes".into(),
            descriptions: describe(&missing),
            sub_groups: vec![SubGroup { label: "Not grouped by the model".into(), descriptions: describe(&missing), entry_ids: missing.clone() }],
            entry_ids: missing,
            risk: None,
            review_note: None,
            mechanical: false,
            origin: None,
            requirement: None,
        });
    }
    if let Some(g) = mechanical_group(&entries) {
        groups.push(g);
    }

    let concerns = parsed.concerns.into_iter().map(|c| strip_id_refs(&c)).filter(|c| !c.is_empty()).collect();
    for g in &mut groups {
        g.review_note = g.review_note.take().map(|n| strip_id_refs(&n));
    }
    Ok(IntelResult { groups, summary: strip_id_refs(&parsed.summary), concerns })
}

/// Keep an origin only when it's supported: a "requested"/"mixed" group needs a
/// quote that really appears in the user's messages (the cited one, or any).
fn validate_origin(g: &LlmGroup, reqs: &[crate::sessions::Requirement]) -> (Option<String>, Option<RequirementQuote>) {
    if reqs.is_empty() {
        return (None, None);
    }
    let origin = g.origin.as_deref().map(|o| o.trim().to_lowercase());
    let quote = g.quote.as_deref().map(str::trim).filter(|q| !q.is_empty());
    let verified = quote.and_then(|q| {
        let cited = g.requirement.as_deref().and_then(|id| reqs.iter().find(|r| r.id.eq_ignore_ascii_case(id.trim())));
        cited.filter(|r| crate::sessions::quote_in(q, &r.text))
            .or_else(|| reqs.iter().find(|r| crate::sessions::quote_in(q, &r.text)))
            .map(|r| RequirementQuote { id: r.id.clone(), quote: q.trim_matches('"').to_string() })
    });
    match origin.as_deref() {
        Some("autonomous") => (Some("autonomous".into()), None),
        Some(o @ ("requested" | "mixed")) if verified.is_some() => (Some(o.to_string()), verified),
        // Claimed as requested but the quote isn't in anything the user said.
        _ => (None, None),
    }
}

/// Remove "(#12)" / "#12" entry-id references the model sometimes leaks into prose.
fn strip_id_refs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let paren = chars[i] == '(' && chars.get(i + 1) == Some(&'#');
        let start = if paren { i + 2 } else if chars[i] == '#' { i + 1 } else { usize::MAX };
        if start != usize::MAX && chars.get(start).is_some_and(|c| c.is_ascii_digit()) {
            let mut j = start;
            // Id lists and ranges: "#5, #6", "#34–79", "#34-#79".
            let continues = |k: usize| chars.get(k).is_some_and(|c| c.is_ascii_digit() || *c == '#');
            while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == ',' || chars[j] == ' ' || chars[j] == '#'
                || (matches!(chars[j], '-' | '–' | '—') && continues(j + 1)))
            {
                if chars[j] == ' ' && chars.get(j + 1).is_none_or(|c| *c != '#') { break; }
                j += 1;
            }
            if paren && chars.get(j) == Some(&')') { j += 1; }
            // Drop the space before a removed reference.
            if out.ends_with(' ') { out.pop(); }
            i = j;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out.trim().to_string()
}

fn mechanical_group(entries: &[Entry]) -> Option<IntentGroup> {
    let ids: Vec<u32> = entries.iter().filter(|e| e.mechanical).map(|e| e.id).collect();
    if ids.is_empty() {
        return None;
    }
    let descriptions: Vec<String> = entries.iter().filter(|e| e.mechanical).map(Entry::description).collect();
    Some(IntentGroup {
        label: "Mechanical changes (formatting, renames, moves)".into(),
        entry_ids: ids.clone(),
        descriptions: descriptions.clone(),
        sub_groups: vec![SubGroup { label: "No behavior change".into(), entry_ids: ids, descriptions }],
        risk: Some("low".into()),
        review_note: None,
        mechanical: true,
        origin: None,
        requirement: None,
    })
}

/// Build rich context with a char budget: imports, small function bodies, then hunks.
pub fn build_rich_context(results: &[DiffResult], sources: &[(String, String, String)]) -> String {
    let mut budget: i64 = 16000; // ~4000 tokens
    let mut sections = Vec::new();

    // 1. Small function bodies (<50 lines), before and/or after.
    let mut bodies = String::from("Changed items (full source when small):\n");
    'outer: for (i, (path, old_src, new_src)) in sources.iter().enumerate() {
        let Some(result) = results.get(i) else { continue };
        let old_lines: Vec<&str> = old_src.lines().collect();
        let new_lines: Vec<&str> = new_src.lines().collect();
        // Extracted helpers too. Their lines are marked as moved, so the diff below leaves them out.
        let extracted = result.manifest.extracted_functions.iter()
            .flat_map(|e| e.locations_new.iter().zip(&e.extracted_names).map(|(loc, name)| (loc, name, "extracted")));
        let changed = result.manifest.logic_changes.iter()
            .map(|lc| (&lc.location, &lc.name, if lc.location.side == Side::Old { "removed" } else { "new" }));
        for (loc, name, version) in changed.chain(extracted) {
            let lines = if loc.side == Side::Old { &old_lines } else { &new_lines };
            let start = loc.line_start.saturating_sub(1);
            let end = loc.line_end.min(lines.len());
            let count = end.saturating_sub(start);
            if count == 0 || count > 50 { continue; }
            let entry = format!("--- {path}:{} {name} ({version})\n{}\n\n", loc.line_start, lines[start..end].join("\n"));
            if budget - (entry.len() as i64) < 4000 { break 'outer; }
            budget -= entry.len() as i64;
            bodies.push_str(&entry);
        }
    }
    if bodies.lines().count() > 1 {
        sections.push(bodies);
    }

    // 2. Diff hunks, skipping mechanical lines (fill remaining budget).
    if budget > 500 {
        let hunks = build_hunks_budgeted(results, sources, budget as usize);
        if !hunks.is_empty() {
            sections.push(format!("Diff (+ added, - removed; unmarked lines are unchanged or only reformatted, moved or renamed):\n{hunks}"));
        }
    }

    if sections.is_empty() { "(none)".into() } else { sections.join("\n") }
}

fn build_hunks_budgeted(results: &[DiffResult], sources: &[(String, String, String)], budget: usize) -> String {
    use perspica_core::manifest::ChangeKind;
    let mut out = Vec::new();
    let mut total = 0;
    for (i, result) in results.iter().enumerate() {
        if result.review.generated { continue; }
        let path = sources.get(i).map(|s| s.0.as_str()).unwrap_or("?");
        let mut header_done = false;
        for hunk in &result.hunks {
            if hunk.noise.is_some() { continue; }
            // Show the whole hunk. Real changes get +/-, everything else is plain context.
            for change in &hunk.changes {
                let text = change.content_new.as_deref().or(change.content_old.as_deref()).unwrap_or("");
                let line = match (change.kind, change.noise.is_some()) {
                    (ChangeKind::Added | ChangeKind::Modified, false) => format!("+ {text}"),
                    (ChangeKind::Removed, false) => format!("- {}", change.content_old.as_deref().unwrap_or("")),
                    (ChangeKind::Removed, true) => continue,
                    _ => format!("  {text}"),
                };
                if !header_done {
                    out.push(format!("=== {path}"));
                    header_done = true;
                }
                total += line.len() + 1;
                if total > budget {
                    out.push("[truncated]".into());
                    return out.join("\n");
                }
                out.push(line);
            }
        }
    }
    out.join("\n")
}

fn fmt_loc(loc: Option<&Location>) -> String {
    match loc {
        Some(l) => match &l.file {
            Some(f) => format!("{f}:{}", l.line_start),
            None => format!("line {}", l.line_start),
        },
        None => String::new(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { return s.to_string(); }
    let mut end = max;
    while !s.is_char_boundary(end) { end -= 1; }
    format!("{}…", &s[..end])
}

pub fn extract_json(text: &str) -> &str {
    if let Some(start) = text.find("```json") {
        let after = &text[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            return &text[start..=end];
        }
    }
    text.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_with<'a>(results: &'a [DiffResult], cf: &'a CrossFileManifest) -> IntelInput<'a> {
        IntelInput { results, sources: &[], cross_file: cf, author_context: None, requirements: &[] }
    }

    #[test]
    fn extracted_helpers_reach_the_model() {
        // The helper's lines moved out of `run`, so the diff leaves them out. Its body should be shown instead.
        let old = "export function run(x: number): number {\n  const a = x * 2;\n  const b = a + 1;\n  const c = b * b;\n  log(c);\n  return c;\n}\n";
        let new = "export function run(x: number): number {\n  const c = square(x);\n  log(c);\n  return c;\n}\n\nfunction square(x: number): number {\n  const a = x * 2;\n  const b = a + 1;\n  const c = b * b;\n  return c;\n}\n";
        let r = perspica_core::analyze_multi(&[perspica_core::cross_file::FileChange::new("a.ts", old, new, perspica_core::Language::TypeScript)]).unwrap();
        let results: Vec<DiffResult> = r.file_results.into_iter().map(|(_, r)| r).collect();
        assert!(!results[0].manifest.extracted_functions.is_empty(), "{:?}", results[0].manifest);
        let context = build_rich_context(&results, &[("a.ts".into(), old.into(), new.into())]);
        assert!(context.contains("square (extracted)") && context.contains("const b = a + 1;"), "{context}");
        // The call site should keep its surrounding lines.
        assert!(context.contains("+   const c = square(x);") && context.contains("    log(c);"), "{context}");
    }

    #[test]
    fn group_ids_and_sub_groups_are_both_kept() {
        let r = perspica_core::analyze(
            "function a(): number { return 1; }\nfunction b(): number { return 2; }\nfunction c(): number { return 3; }",
            "function a(): number { return 10; }\nfunction b(): number { return 20; }\nfunction c(): number { return 30; }",
            perspica_core::Language::TypeScript,
        ).unwrap();
        let ids: Vec<u32> = r.manifest.logic_changes.iter().map(|l| l.id).collect();
        let results = vec![r];
        let cf = CrossFileManifest::default();
        let input = input_with(&results, &cf);
        let resp = format!(r#"{{"groups":[{{"label":"G","entry_ids":[{}],"sub_groups":[{{"label":"S","entry_ids":[{},{}]}}]}}],"summary":"s","concerns":[]}}"#, ids[0], ids[1], ids[2]);
        let out = parse_and_validate(&resp, &input).unwrap();
        assert_eq!(out.groups.len(), 1, "nothing falls into Other changes");
        assert_eq!(out.groups[0].entry_ids.len(), 3);
    }

    #[test]
    fn strips_entry_ids() {
        assert_eq!(strip_id_refs("foo (#2) is dead code"), "foo is dead code");
        assert_eq!(strip_id_refs("see #5 and #6, then"), "see and then");
        assert_eq!(strip_id_refs("issue #bar stays"), "issue #bar stays");
        assert_eq!(strip_id_refs("Call graph complexity (#34–79): cycles"), "Call graph complexity: cycles");
        assert_eq!(strip_id_refs("added (#98-#106) without"), "added without");
    }
}
