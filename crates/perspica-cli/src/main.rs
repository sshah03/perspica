mod git;
mod render_tty;
mod render_json;
mod intel;
mod llm;
mod web;
mod deep;
mod sessions;
mod saved;

use clap::Parser;
use perspica_core::cross_file::{BrokenReferenceEntry, CallSite, CrossFileManifest, FileChange};
use perspica_core::manifest::{Location, Side};
use perspica_core::{DiffResult, Language};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "perspica",
    about = "Review code changes by what they do",
    version,
    after_help = "Examples:\n  perspica                      review the current branch (run inside a repo)\n  perspica old.ts new.ts\n  perspica --staged --web\n  perspica --git main...feature -s\n  perspica --pr 123 --web"
)]
struct Cli {
    /// Old file path
    old_file: Option<PathBuf>,

    /// New file path
    new_file: Option<PathBuf>,

    /// Override language detection
    #[arg(short, long)]
    language: Option<String>,

    /// Output format: tty (default), json, web, html (the viewer saved as one file, see --out)
    #[arg(short, long, default_value = "tty")]
    format: String,

    /// Where --format html writes the page
    #[arg(long, default_value = "perspica-review.html")]
    out: PathBuf,

    /// Shorthand for --format json
    #[arg(long)]
    json: bool,

    /// Open results in browser
    #[arg(long)]
    web: bool,

    /// Port for web viewer (the next free port is used if taken)
    #[arg(long, default_value = "7890")]
    port: u16,

    /// Don't open a browser tab (web mode)
    #[arg(long)]
    no_open: bool,

    /// Disable colored output
    #[arg(long)]
    no_color: bool,

    /// Show mechanical changes (formatting, renames, moves) in full in the terminal
    #[arg(long)]
    show_noise: bool,

    // -- Git integration --
    /// Diff staged changes
    #[arg(long)]
    staged: bool,

    /// Diff working tree against HEAD, or specify a commit range (a..b, a...b, or a ref)
    #[arg(long, num_args = 0..=1, default_missing_value = "HEAD")]
    git: Option<String>,

    /// Review a GitHub pull request by number (uses the `gh` CLI)
    #[arg(long)]
    pr: Option<u64>,

    /// Review the current branch against its merge-base with <BASE> (default: origin/HEAD,
    /// main or master). What `perspica` does with no arguments
    #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "BASE")]
    branch: Option<String>,

    // -- LLM analysis --
    /// Group changes by intent, with risk and a summary, using an LLM. Sends the change
    /// list and changed code, never whole files
    #[arg(short, long)]
    summarize: bool,

    /// Thorough analysis: the LLM may first read definitions and small files from the
    /// changed files. Slower. Implies -s
    #[arg(short, long)]
    deep: bool,

    /// LLM API key (better: set it in the environment, see the README)
    #[arg(long)]
    api_key: Option<String>,

    /// With --api-key: anthropic (default) or openai. `ollama` needs no key and uses a local model
    #[arg(long)]
    provider: Option<String>,

    /// Model to use instead of the provider's default
    #[arg(long)]
    model: Option<String>,

    /// Don't read the Claude Code or Codex sessions behind your change (your prompts are
    /// shown, and sent with -s). Never read for other people's changes
    #[arg(long)]
    no_sessions: bool,

    /// Run the LLM analysis again even if a saved one matches this diff
    #[arg(long)]
    fresh: bool,

    /// Send all of the changed code to the LLM, however large. By default perspica sends up to
    /// 100,000 characters (16,000 for local models)
    #[arg(long)]
    full_context: bool,
}

/// Multi-file analysis output.
#[derive(Debug, Serialize, Deserialize)]
pub struct MultiFileResult {
    pub version: String,
    pub source: SourceInfo,
    pub files: Vec<FileEntry>,
    pub results: Vec<DiffResult>,
    pub cross_file: CrossFileManifest,
    pub intent_groups: Option<Vec<intel::IntentGroup>>,
    pub summary: Option<String>,
    #[serde(default)]
    pub concerns: Vec<String>,
    /// Name of the LLM provider used, if any.
    #[serde(default)]
    pub llm_provider: Option<String>,
    /// Model the intent groups came from.
    #[serde(default)]
    pub llm_model: Option<String>,
    /// When the intent groups were loaded from a saved analysis: when it was made (unix seconds).
    #[serde(default)]
    pub llm_saved_at: Option<u64>,
    /// Why the LLM analysis did not run or failed.
    #[serde(default)]
    pub llm_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceInfo {
    /// e.g. "main..feature", "staged changes", "PR #12"
    pub label: String,
    /// The label in full (commit ids, refs, PR URL), for a tooltip.
    #[serde(default)]
    pub detail: Option<String>,
    /// Which repository this is: `owner/name` (or the folder name), and its web URL.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub repo_url: Option<String>,
    #[serde(default)]
    pub pr_title: Option<String>,
    #[serde(default)]
    pub pr_url: Option<String>,
    /// PR description or commit messages, used as author context for the LLM.
    #[serde(default, skip_serializing)]
    pub author_context: Option<String>,
    /// Coding-agent sessions that made the change, and the user's prompts in them.
    #[serde(default)]
    pub sessions: sessions::SessionContext,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub old_path: String,
    pub new_path: String,
    pub language: String,
    pub old_source: String,
    pub new_source: String,
    /// A, M, D, R, C, T
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub binary: bool,
}

impl MultiFileResult {
    pub fn file_sources(&self) -> Vec<(String, String, String)> {
        self.files.iter().map(|f| (f.new_path.clone(), f.old_source.clone(), f.new_source.clone())).collect()
    }
}

fn parse_language(lang_str: &str) -> Language {
    match lang_str.to_lowercase().as_str() {
        "typescript" | "ts" | "javascript" | "js" => Language::TypeScript,
        "tsx" | "jsx" => Language::Tsx,
        "python" | "py" => Language::Python,
        "rust" | "rs" => Language::Rust,
        "go" => Language::Go,
        "java" => Language::Java,
        "c" => Language::C,
        "scala" => Language::Scala,
        "csharp" | "c#" | "cs" => Language::CSharp,
        "kotlin" | "kt" => Language::Kotlin,
        "php" => Language::Php,
        "ruby" | "rb" => Language::Ruby,
        _ => Language::Unknown,
    }
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("perspica: {msg}");
    std::process::exit(1);
}

fn main() {
    // Exit quietly when the reader goes away (`perspica | head`) instead of panicking.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let format = if cli.json { "json" } else if cli.web { "web" } else { cli.format.as_str() };

    // Gather inputs
    let file_mode = cli.old_file.is_some() || cli.new_file.is_some();
    let branch_base = match &cli.branch {
        Some(b) => Some(b.as_str()),
        None if !file_mode && !cli.staged && cli.git.is_none() && cli.pr.is_none() && git::repo_root().is_some() => Some(""),
        None => None,
    };
    if !file_mode {
        // git reports root-relative paths; run everything from the root.
        if let Some(root) = git::repo_root() {
            let _ = std::env::set_current_dir(root);
        }
    }
    let mut source = SourceInfo { label: String::new(), pr_title: None, pr_url: None, author_context: None, sessions: Default::default(), detail: None, repo: None, repo_url: None };
    let mut target: Option<git::Target> = None;
    let diffs: Vec<git::GitFileDiff> = if let Some(pr) = cli.pr {
        let (t, info) = git::pr_target(pr).unwrap_or_else(|e| fail(e));
        source.label = format!("PR #{}", info.number);
        source.detail = Some(format!("PR #{}: {}\n{}\n{}", info.number, info.title, info.url, t.detail()));
        source.author_context = Some(format!("{}\n\n{}", info.title, info.body));
        source.pr_title = Some(info.title);
        source.pr_url = Some(info.url);
        let d = git::collect(&t).unwrap_or_else(|e| fail(e));
        target = Some(t);
        d
    } else if let Some(base) = branch_base {
        let t = git::Target::branch(Some(base).filter(|b| !b.is_empty())).unwrap_or_else(|e| fail(e));
        source.label = t.label();
        source.detail = Some(t.detail());
        source.author_context = git::commit_messages(&t);
        let d = git::collect(&t).unwrap_or_else(|e| fail(e));
        target = Some(t);
        d
    } else if cli.staged || cli.git.is_some() {
        let t = if cli.staged { git::Target::Staged } else { git::Target::from_spec(cli.git.as_deref()).unwrap_or_else(|e| fail(e)) };
        source.label = t.label();
        source.detail = Some(t.detail());
        source.author_context = git::commit_messages(&t);
        let d = git::collect(&t).unwrap_or_else(|e| fail(e));
        target = Some(t);
        d
    } else {
        file_pair_input(&cli)
    };

    // Agent sessions describe the local user's own work only.
    if let (Some(t), false) = (&target, cli.no_sessions || target.as_ref().is_some_and(|t| !git::is_own_change(t))) {
        if let (Some(root), Some(since)) = (git::repo_root(), git::base_time(t)) {
            let changed: Vec<String> = diffs.iter().map(|d| d.new_path.clone()).collect();
            // A little slack: the commit is made just after the last prompt's work.
            let until = git::head_time(t).map(|h| h + 120);
            source.sessions = sessions::find(&root, &changed, since, until);
            // Reading transcripts should never be silent.
            let ctx = &source.sessions;
            if !ctx.is_empty() {
                let mut agents: Vec<&str> = ctx.sessions.iter().map(|s| if s.agent == "codex" { "Codex" } else { "Claude Code" }).collect();
                agents.dedup();
                let n = ctx.sessions.len();
                eprintln!("Using your prompts from {n} {} session{} that edited these files (--no-sessions to skip).",
                    agents.join(" and "), if n == 1 { "" } else { "s" });
            }
        }
    }

    if target.is_some() {
        if let Some((name, url)) = git::repo_identity() {
            source.repo = Some(name);
            source.repo_url = url;
        }
    }

    if diffs.is_empty() {
        eprintln!("No changes found{}.", if source.label.is_empty() { String::new() } else { format!(" ({})", source.label) });
        std::process::exit(0);
    }

    // Analyze
    let changes: Vec<FileChange> = diffs.iter().map(|d| {
        let hunks = git::parse_unified_diff(&d.raw_unified_diff);
        FileChange {
            old_path: d.old_path.clone(),
            new_path: d.new_path.clone(),
            old_source: d.old_content.clone(),
            new_source: d.new_content.clone(),
            language: d.language,
            display_hunks: if hunks.is_empty() && !d.binary { None } else { Some(hunks) },
            role: d.role,
        }
    }).collect();
    let analysis = perspica_core::analyze_multi(&changes).unwrap_or_else(|e| fail(format!("analysis failed: {e}")));
    drop(changes);

    let files: Vec<FileEntry> = diffs.into_iter().map(|d| FileEntry {
        old_path: d.old_path,
        new_path: d.new_path,
        language: format!("{:?}", d.language),
        old_source: d.old_content,
        new_source: d.new_content,
        status: d.status.to_string(),
        binary: d.binary,
    }).collect();

    let mut multi = MultiFileResult {
        version: env!("CARGO_PKG_VERSION").to_string(),
        source,
        files,
        results: analysis.file_results.into_iter().map(|(_, r)| r).collect(),
        cross_file: analysis.cross_file,
        intent_groups: None,
        summary: None,
        concerns: vec![],
        llm_provider: None,
        llm_model: None,
        llm_saved_at: None,
        llm_error: None,
    };

    if let Some(t) = &target {
        enrich_with_repo_references(&mut multi, t);
    }

    // Optional LLM analysis
    let rt = tokio::runtime::Runtime::new().unwrap_or_else(|e| fail(e));
    let wants_llm = cli.summarize || cli.deep;
    let detect = llm::Detect { api_key: cli.api_key.clone(), provider: cli.provider.clone(), model: cli.model.clone() };
    let provider = if wants_llm || format == "web" {
        rt.block_on(detect.run())
    } else {
        None
    };
    multi.llm_provider = provider.as_ref().map(|p| p.name().to_string());

    // A saved analysis of this exact diff: shown in the viewer, and reused by -s
    // when it came from the same model and depth (unless --fresh).
    let key = saved::key(&multi);
    let mut reused = false;
    if let Some(s) = saved::load(&key).filter(|_| !cli.fresh) {
        let matches = cli.model.as_deref().is_none_or(|m| m == s.model) && (!cli.deep || s.depth == "thorough");
        if wants_llm && matches {
            eprintln!("Using the saved analysis of this diff by {} from {}; --fresh to run it again.", s.model, ago(s.saved_at));
            saved::apply(&mut multi, s);
            reused = true;
        } else if !wants_llm && (format == "web" || format == "html") {
            saved::apply(&mut multi, s);
        }
    }
    let wants_llm = wants_llm && !reused;

    if wants_llm {
        match &provider {
            Some(p) => rt.block_on(run_llm(&mut multi, &**p, cli.deep, cli.full_context)),
            // The viewer explains the setup when Analyze is clicked.
            None if format == "web" => {}
            None => eprintln!("{}\nShowing the analysis without it.\n", rt.block_on(llm::setup()).hint()),
        }
    }

    // Render
    if wants_llm && multi.llm_error.is_none() {
        if let (Some(groups), Some(p)) = (&multi.intent_groups, &provider) {
            saved::save(&key, &saved::SavedAnalysis {
                provider: p.name().to_string(),
                model: p.model().to_string(),
                depth: if cli.deep { "thorough" } else { "standard" }.into(),
                saved_at: saved::now(),
                groups: groups.clone(),
                summary: multi.summary.clone().unwrap_or_default(),
                concerns: multi.concerns.clone(),
            });
        }
    }

    match format {
        "json" => render_json::render_multi(&multi),
        "html" => {
            // A page is made to be shared, so leave out the prompts from your own agent sessions.
            multi.source.sessions = Default::default();
            if let Err(e) = std::fs::write(&cli.out, web::export_html(&multi)) {
                fail(format!("could not write {}: {e}", cli.out.display()));
            }
            eprintln!("Wrote {}", cli.out.display());
        }
        "web" => rt.block_on(web::serve(multi, cli.port, provider, detect, !cli.no_open, key)),
        _ => render_tty::render_multi(&multi, !cli.no_color, cli.show_noise),
    }
}

/// Two files on disk (or one missing, for added/deleted).
fn file_pair_input(cli: &Cli) -> Vec<git::GitFileDiff> {
    let (Some(old_path), Some(new_path)) = (&cli.old_file, &cli.new_file) else {
        eprintln!("Usage: perspica <old_file> <new_file>");
        eprintln!("       perspica [--branch [<base>]] | --staged | --git [<range>] | --pr <number>");
        eprintln!("Run `perspica --help` for all options.");
        std::process::exit(1);
    };
    let read = |p: &PathBuf| std::fs::read(p).unwrap_or_else(|e| fail(format!("reading {}: {e}", p.display())));
    let (old_bytes, new_bytes) = (read(old_path), read(new_path));
    let binary = old_bytes.contains(&0) || new_bytes.contains(&0);
    let language = match &cli.language {
        Some(l) => parse_language(l),
        None => Language::from_path(&new_path.display().to_string()),
    };
    let raw = git::no_index_diff(&old_path.display().to_string(), &new_path.display().to_string()).unwrap_or_default();
    if old_bytes == new_bytes {
        return vec![];
    }
    vec![git::GitFileDiff {
        old_path: old_path.display().to_string(),
        new_path: new_path.display().to_string(),
        old_content: if binary { String::new() } else { String::from_utf8_lossy(&old_bytes).into_owned() },
        new_content: if binary { String::new() } else { String::from_utf8_lossy(&new_bytes).into_owned() },
        language: if binary { Language::Unknown } else { language },
        raw_unified_diff: raw,
        status: 'M',
        binary,
        // Comparing two files directly means reviewing them as code, wherever they live.
        role: Some(perspica_core::roles::FileRole::Source),
    }]
}

/// Search the whole repository (not just changed files) for references to
/// names that vanished and for calls to functions whose signature changed.
fn enrich_with_repo_references(multi: &mut MultiFileResult, target: &git::Target) {
    // Plenty of lines to scan (the per-symbol cap below limits what is reported).
    const MAX_HITS: usize = 3000;
    const MAX_PER_SYMBOL: usize = 10;
    let in_diff: std::collections::HashSet<&str> = multi.files.iter().map(|f| f.new_path.as_str()).collect();
    // A name an import brought in only exists in its own file, which the core already checked.
    let vanished: Vec<perspica_core::cross_file::Vanished> = multi.cross_file.vanished.iter()
        .filter(|v| !v.import_name)
        .map(|v| (v.name.clone(), v.renamed_to.clone(), v.origin.clone(), v.owner.clone()))
        .collect();
    // Exported functions, and private ones in languages where they reach the rest of a package.
    // Local functions are only called from the function they're in, and that's already checked.
    let reaches_out = |s: &perspica_core::cross_file::SignatureImpactEntry| !s.local && (s.exported
        || s.definition.file.as_deref().is_some_and(|f| f.ends_with(".go") || f.ends_with(".rs") || f.ends_with(".py") || f.ends_with(".c")));
    let sig_names: Vec<String> = multi.cross_file.signature_impacts.iter()
        .filter(|s| reaches_out(s))
        .map(|s| perspica_core::cross_file::call_name(&s.name).to_string())
        .filter(|n| n.len() >= 3)
        .collect();
    // Skip multi-line names like a destructuring pattern `{ a, b }`. They can never match a single
    // line, and git grep splits them into patterns like `{` that match almost every line.
    let mut names: Vec<&str> = vanished.iter().map(|v| v.0.as_str()).chain(sig_names.iter().map(String::as_str))
        .filter(|n| !n.contains('\n'))
        .collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return;
    }
    let hits = git::grep_words(target, &names, MAX_HITS);
    // A vanished name that some other code still defines (another type's method
    // of the same name, say) can't be told apart from it by name: drop it.
    let ambiguous: std::collections::HashSet<&str> = hits.iter()
        .filter(|(name, path, _, text)| !in_diff.contains(path.as_str()) && vanished.iter().any(|v| &v.0 == name)
            && Language::from_path(path) != Language::Unknown && (perspica_core::cross_file::looks_like_definition(text, name)
                || (perspica_core::cross_file::overloads(path) && perspica_core::cross_file::defines_function(text, name, path))))
        .map(|(name, ..)| name.as_str())
        .collect();
    // In the method's own file, `this.name` or `self.name` still means the removed method,
    // even if something else in the repo has the same name.
    let origin_of: std::collections::HashMap<&str, &str> = vanished.iter().map(|v| (v.0.as_str(), v.2.as_str())).collect();
    let own_member = |b: &perspica_core::cross_file::BrokenReferenceEntry| {
        origin_of.get(b.symbol_name.as_str()) == Some(&b.reference_file.as_str())
            && ["this.", "self."].iter().any(|q| {
                let pat = format!("{q}{}", b.symbol_name);
                b.line_text.match_indices(&pat).any(|(i, _)| !b.line_text[i + pat.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_'))
            })
    };
    multi.cross_file.broken_references.retain(|b| !ambiguous.contains(b.symbol_name.as_str()) || own_member(b));
    multi.cross_file.vanished.retain(|v| !ambiguous.contains(v.name.as_str()));
    let file_private: std::collections::HashSet<String> = multi.cross_file.vanished.iter()
        .filter(|v| v.file_private).map(|v| v.name.clone()).collect();
    let mut next_id = max_id(multi) + 1;
    let mut per_symbol: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    // Read the files the checks below may need in one go. A git process per file adds up.
    let mut wanted: Vec<&str> = hits.iter()
        .map(|(_, path, ..)| path.as_str())
        .filter(|p| !in_diff.contains(p) && Language::from_path(p) != Language::Unknown)
        .collect();
    wanted.sort_unstable();
    wanted.dedup();
    let mut sources = git::read_new_many(target, &wanted);
    for (name, path, line, text) in &hits {
        if in_diff.contains(path.as_str()) || ambiguous.contains(name.as_str()) {
            continue; // already scanned precisely by the core, or not a stale name after all
        }
        // Mentions in docs, config or other non-code files aren't references, and vendored or
        // generated code (`.yarn/releases/…cjs`, `vendor/`, `dist/`) isn't the project's to update.
        if Language::from_path(path) == Language::Unknown || perspica_core::roles::is_docs_path(path)
            || perspica_core::roles::is_vendored_path(path) || perspica_core::roles::is_generated_path(path) {
            continue;
        }
        // The cap counts what's reported, not what's looked at (imports and other types' names don't use it up).
        if per_symbol.get(name.as_str()).copied().unwrap_or(0) >= MAX_PER_SYMBOL {
            continue;
        }
        // Something private to its own file isn't used anywhere else.
        if let Some((_, renamed_to, origin, owner)) = vanished.iter().find(|v| &v.0 == name).filter(|_| !file_private.contains(name)) {
            // Same rule as the core: a method's name only counts on its own type.
            if perspica_core::cross_file::scan_references_in(text, name, owner.as_deref(), origin, path).is_empty() {
                continue;
            }
            // A file that declares the name itself (a variable, parameter, field) is using its own.
            let source = sources.entry(path.clone()).or_insert_with(|| git::read_new(target, path));
            if source.as_deref().is_some_and(|src| ((perspica_core::cross_file::declares_name(src, name) || perspica_core::cross_file::imports_name_from_elsewhere(src, path, name, origin)
                    || (perspica_core::cross_file::overloads(path) && perspica_core::cross_file::defines_function(src, name, path)))
                    && !perspica_core::cross_file::qualified_mention(text, name))
                || (path.ends_with(".go") && perspica_core::cross_file::go_package_named(src, name) && perspica_core::cross_file::go_package_mention(text, name))) {
                continue;
            }
            multi.cross_file.broken_references.push(BrokenReferenceEntry {
                id: next_id,
                symbol_name: name.clone(),
                renamed_from: renamed_to.clone(),
                reference_file: path.clone(),
                reference_location: Location { file: Some(path.clone()), line_start: *line, line_end: *line, side: Side::New },
                reason: perspica_core::cross_file::broken_reason(name, renamed_to.as_deref(), origin),
                line_text: text.clone(),
                in_diff: false,
            });
            next_id += 1;
            *per_symbol.entry(name.as_str()).or_default() += 1;
        }
        for impact in multi.cross_file.signature_impacts.iter_mut() {
            let def = impact.definition.file.clone().unwrap_or_default();
            if !impact.exported && !perspica_core::cross_file::private_reaches(&def, path) && !includes_c_file(target, path, &def, 2) {
                continue;
            }
            let callee = perspica_core::cross_file::call_name(&impact.name);
            let callee_sig = if callee != perspica_core::parser::bare_name(&impact.name) { callee.to_string() } else { impact.name.clone() };
            let accept = |q: Option<&str>| perspica_core::cross_file::call_qualifier_ok_from(q, &def, &callee_sig, path);
            if callee != name { continue; }
            // A file that imports this name from another module calls a different function. In C# and
            // Kotlin, so does one with its own method of that name.
            if perspica_core::cross_file::unqualified_call(text, name) {
                let source = sources.entry(path.clone()).or_insert_with(|| git::read_new(target, path));
                if source.as_deref().is_some_and(|src| perspica_core::cross_file::imports_it_elsewhere(src, path, &def, name)
                    || (perspica_core::cross_file::overloads(path) && perspica_core::cross_file::defines_function(src, name, path))) { continue; }
            }
            let masked = perspica_core::cross_file::code_only_in(text, path);
            let Some((_, _, open)) = perspica_core::cross_file::scan_calls_at(text, &masked, name, &accept, path).into_iter().next() else { continue };
            let args = perspica_core::cross_file::call_arguments(&masked[open..], path);
            if let (Some((old, _)), Some(a)) = (&impact.params, &args) {
                if perspica_core::cross_file::other_overload(old, a, &def) { continue; }
            }
            // A call on one line that still fits the new signature has nothing to update.
            let fits = match (&impact.params, &args) {
                (Some((old, new)), Some(args)) => perspica_core::cross_file::still_fits(old, new, args, perspica_core::cross_file::named_args(&def), &def),
                _ => false,
            };
            impact.call_sites.push(CallSite { file: path.clone(), line: *line, text: text.clone(), updated: fits, in_diff: false });
            *per_symbol.entry(name.as_str()).or_default() += 1;
        }
    }
}

/// Whether the C file at `path` compiles in `def` (a `.c` file), directly or through a header of
/// the repo (`#include "common.h"` → `#include "../cJSON.c"`): then it sees `def`'s static functions.
fn includes_c_file(target: &git::Target, path: &str, def: &str, depth: usize) -> bool {
    if !def.ends_with(".c") { return false; }
    let Some(source) = git::read_new(target, path) else { return false };
    if perspica_core::cross_file::includes_file(&source, def) { return true; }
    if depth == 0 { return false; }
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
    source.lines().filter_map(|l| l.trim_start().strip_prefix("#include")).filter_map(|r| r.trim().strip_prefix('"')?.split('"').next())
        .filter(|h| h.ends_with(".h"))
        .any(|h| {
            let mut parts: Vec<&str> = if dir.is_empty() { vec![] } else { dir.split('/').collect() };
            for seg in h.split('/') { match seg { ".." => { parts.pop(); } "." => {} s => parts.push(s) } }
            includes_c_file(target, &parts.join("/"), def, depth - 1)
        })
}

/// "3 minutes ago", "2 days ago".
fn ago(unix: u64) -> String {
    let secs = saved::now().saturating_sub(unix);
    let (n, unit) = match secs {
        0..=89 => return "just now".into(),
        90..=5399 => (secs / 60, "minute"),
        5400..=129_599 => (secs / 3600, "hour"),
        _ => (secs / 86_400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

fn max_id(multi: &MultiFileResult) -> u32 {
    let cf = &multi.cross_file;
    multi.results.iter().flat_map(|r| r.manifest.ids())
        .chain(cf.moves.iter().map(|m| m.id))
        .chain(cf.broken_references.iter().map(|b| b.id))
        .chain(cf.signature_impacts.iter().map(|s| s.id))
        .max()
        .unwrap_or(0)
}

async fn run_llm(multi: &mut MultiFileResult, provider: &dyn llm::LlmProvider, deep: bool, full: bool) {
    use std::io::IsTerminal;
    let label = format!("Analyzing with {}{}", provider.name(), if deep { " (thorough)" } else { "" });
    let tty = std::io::stderr().is_terminal();
    eprint!("{label}…{}", if tty { " [0s]" } else { "\n" });
    let start = std::time::Instant::now();
    let timer_label = label.clone();
    let timer = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if tty {
                eprint!("\r{timer_label}… [{}s]", start.elapsed().as_secs());
            }
        }
    });

    let sources = multi.file_sources();
    let budget = provider.context_budget();
    let chars = intel::context_chars(&multi.results, &sources);
    if !full && chars > budget {
        eprintln!("\rThis change has {chars} characters of changed code. perspica sends the first {budget}; add --full-context to send all of it.");
    }
    let input = intel::IntelInput {
        results: &multi.results,
        sources: &sources,
        cross_file: &multi.cross_file,
        author_context: multi.source.author_context.as_deref(),
        requirements: &multi.source.sessions.requirements,
        budget: (!full).then_some(budget),
    };
    let result = if deep {
        match deep::deep_analyze(&input, provider).await {
            Ok(d) => {
                eprint!("\r{label}: {} tool calls, {} iterations", d.tool_calls_made, d.iterations);
                Ok(intel::IntelResult { groups: d.groups, summary: d.summary, concerns: d.concerns })
            }
            Err(e) => {
                eprintln!("\rDeep analysis failed: {e}. Falling back to standard.");
                intel::run_analysis(&input, provider).await
            }
        }
    } else {
        intel::run_analysis(&input, provider).await
    };
    timer.abort();
    let secs = start.elapsed().as_secs();
    match result {
        Ok(r) => {
            eprintln!("\r{label} [{secs}s] ✓        ");
            multi.intent_groups = Some(r.groups);
            multi.summary = Some(r.summary).filter(|s| !s.trim().is_empty());
            multi.concerns = r.concerns;
            multi.llm_model = Some(provider.model().to_string());
            multi.llm_saved_at = None;
        }
        Err(e) => {
            eprintln!("\r{label} [{secs}s] ✗        ");
            eprintln!("Analysis failed: {e}");
            eprintln!("Showing the results without it.");
            multi.llm_error = Some(e);
        }
    }
}
