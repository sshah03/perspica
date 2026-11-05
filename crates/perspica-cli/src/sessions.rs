//! Coding-agent sessions behind a change: the user's own instructions, read from
//! the agent's local transcripts. They're the requirements a reviewer checks the
//! change against, and let intent grouping tell requested work from the agent's
//! own decisions.
//!
//! Supported:
//! - Claude Code: `~/.claude/projects/<repo path with non-alphanumerics as '-'>/*.jsonl`
//!   (`$CLAUDE_CONFIG_DIR/projects` when set);
//! - Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (`$CODEX_HOME/sessions`),
//!   matched to the repository by the session's working directory.
//!
//! Nothing is uploaded here; the prompts are only sent to an LLM with `-s`.
//! No agent installed, or no matching session, just means no requirements.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Prompts kept per diff (most recent first when trimming).
const MAX_PROMPTS: usize = 20;
/// Characters kept per prompt.
const MAX_PROMPT_CHARS: usize = 1200;
/// Sessions kept per diff.
const MAX_SESSIONS: usize = 4;
/// A pause this long after the last edit means the session has moved on.
const BREAK_SECS: i64 = 2 * 3600;
/// Shorter prompts are acknowledgements ("yes", "go ahead"), not requirements.
const MIN_PROMPT_WORDS: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub agent: String,
    pub id: String,
    /// ISO-8601 timestamps of the first and last relevant activity.
    pub started: String,
    pub ended: String,
    /// Changed files this session edited or ran commands on.
    pub files_touched: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    /// "R1", "R2", … in chronological order.
    pub id: String,
    pub session: String,
    pub timestamp: String,
    pub text: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SessionContext {
    pub sessions: Vec<AgentSession>,
    pub requirements: Vec<Requirement>,
}

impl SessionContext {
    pub fn is_empty(&self) -> bool {
        self.requirements.is_empty()
    }
}

/// Sessions in this repository that touched any of `changed` (repo-relative paths)
/// between `since` and `until` (unix seconds; `None` = now), and the user prompts
/// sent in that window. A committed change bounds the window at its commit time:
/// later prompts can't have produced it.
pub fn find(root: &Path, changed: &[String], since: i64, until: Option<i64>) -> SessionContext {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dir = |var: &str, default: &str| std::env::var_os(var).map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(default)));
    let claude = dir("CLAUDE_CONFIG_DIR", ".claude").map(|d| d.join("projects"));
    let codex = dir("CODEX_HOME", ".codex").map(|d| d.join("sessions"));
    find_in(claude.as_deref(), codex.as_deref(), root, changed, since, until)
}

fn find_in(claude: Option<&Path>, codex: Option<&Path>, root: &Path, changed: &[String], since: i64, until: Option<i64>) -> SessionContext {
    let found = relevant_sessions(claude, codex, root, changed, since, until).into_iter()
        .map(|s| {
            let prompts = s.events.iter()
                .filter(|e| e.kind == Kind::User)
                .filter_map(|e| requirement_text(&e.text).map(|t| (e.ts.clone(), t)))
                .collect();
            (s.info, prompts)
        })
        .collect();
    finish(found)
}

/// Every session in this repository that touched a changed file in the window, cut to the part that can
/// have produced the change.
fn relevant_sessions(claude: Option<&Path>, codex: Option<&Path>, root: &Path, changed: &[String], since: i64, until: Option<i64>) -> Vec<Relevant> {
    let matchers = PathMatchers::new(root, changed);
    let until = until.unwrap_or(i64::MAX);
    let mut found = Vec::new();
    if let Some(dir) = claude {
        for f in claude_files(dir, root, since) {
            found.extend(read_claude(&f, &matchers).and_then(|s| relevant(s, &matchers, since, until)));
        }
    }
    if let Some(dir) = codex {
        for f in jsonl_files_since(dir, since, 4) {
            found.extend(read_codex(&f, root, &matchers).and_then(|s| relevant(s, &matchers, since, until)));
        }
    }
    found
}

/// Claude Code transcripts for this repository (and its subdirectories / worktrees).
fn claude_files(projects: &Path, root: &Path, since: i64) -> Vec<PathBuf> {
    let slug = project_slug(root);
    let Ok(dirs) = std::fs::read_dir(projects) else { return vec![] };
    let mut files: Vec<PathBuf> = Vec::new();
    for d in dirs.flatten() {
        let name = d.file_name().to_string_lossy().into_owned();
        // The repo itself, or a subdirectory / worktree under it.
        if name != slug && !name.starts_with(&format!("{slug}-")) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(d.path()) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let fresh = e.metadata().ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|t| t.as_secs() as i64 >= since);
            if fresh && p.extension().is_some_and(|x| x == "jsonl") {
                files.push(p);
            }
        }
    }

    files
}

/// `.jsonl` files under `dir` (up to `depth` levels) modified at or after `since`.
fn jsonl_files_since(dir: &Path, since: i64, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(meta) = e.metadata() else { continue };
        let fresh = meta.modified().ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|t| t.as_secs() as i64 >= since);
        if meta.is_dir() && depth > 0 {
            // Date directories (2026/09/29) older than `since` can't hold fresh sessions,
            // but their mtimes don't say much; the files' own mtimes decide.
            out.extend(jsonl_files_since(&p, since, depth - 1));
        } else if fresh && p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
    out
}

/// Where the sessions behind a change can be read again: the repository, the changed files, and the
/// time window (unix seconds).
#[derive(Debug, Clone)]
pub struct Window {
    pub root: PathBuf,
    pub changed: Vec<String>,
    pub since: i64,
    pub until: Option<i64>,
}

/// One moment of the sessions behind a change, for the Decisions tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEvent {
    /// 1, 2, … in time order across sessions.
    pub n: usize,
    pub ts: String,
    pub session: String,
    /// "user", "agent", "notes" (the agent's thinking), "edit", "command", "result" or "compaction".
    pub who: String,
    pub text: String,
}

/// Characters kept per event, by kind.
const TRACE_CLIP: [(&str, usize); 7] = [("user", 4000), ("agent", 3000), ("notes", 2000), ("edit", 300), ("command", 400), ("result", 400), ("compaction", 0)];
/// Longer tool output is left out, unless it's an error.
const TRACE_RESULT_MAX: usize = 600;

/// Everything said and done in the sessions behind a change, oldest first, cut to fit `budget` characters.
pub fn trace(root: &Path, changed: &[String], since: i64, until: Option<i64>, budget: usize) -> Vec<TraceEvent> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dir = |var: &str, default: &str| std::env::var_os(var).map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(default)));
    let claude = dir("CLAUDE_CONFIG_DIR", ".claude").map(|d| d.join("projects"));
    let codex = dir("CODEX_HOME", ".codex").map(|d| d.join("sessions"));
    trace_in(claude.as_deref(), codex.as_deref(), root, changed, since, until, budget)
}

fn trace_in(claude: Option<&Path>, codex: Option<&Path>, root: &Path, changed: &[String], since: i64, until: Option<i64>, budget: usize) -> Vec<TraceEvent> {
    let mut found = relevant_sessions(claude, codex, root, changed, since, until);
    found.sort_by(|a, b| b.info.files_touched.len().cmp(&a.info.files_touched.len()).then(b.info.ended.cmp(&a.info.ended)));
    found.truncate(MAX_SESSIONS);
    let mut events: Vec<TraceEvent> = found.iter()
        .flat_map(|s| s.events.iter().filter_map(|e| {
            let who = match e.kind {
                Kind::User => "user",
                Kind::Agent => "agent",
                Kind::Notes => "notes",
                // Only tool calls on the changed files say anything about this change.
                Kind::Tool if e.touches.is_empty() => return None,
                Kind::Tool if e.writes => "edit",
                Kind::Tool => "command",
                Kind::Result if !e.is_error && e.text.len() > TRACE_RESULT_MAX => return None,
                Kind::Result => "result",
                Kind::Compaction => "compaction",
            };
            let text = if e.kind == Kind::Compaction { "(the agent's context was summarized here)".to_string() } else { e.text.trim().to_string() };
            (!text.is_empty()).then(|| TraceEvent { n: 0, ts: e.ts.clone(), session: s.info.id.clone(), who: who.into(), text })
        }))
        .collect();
    events.sort_by(|a, b| a.ts.cmp(&b.ts));
    for e in &mut events {
        let max = TRACE_CLIP.iter().find(|(w, _)| *w == e.who).map(|(_, m)| *m).unwrap_or(1000);
        if max > 0 {
            e.text = clip(&e.text, max);
        }
    }
    fit(&mut events, budget);
    for (i, e) in events.iter_mut().enumerate() {
        e.n = i + 1;
    }
    events
}

/// Cuts the trace to `budget` characters, least telling first: tool output, then commands, then the
/// length of the agent's messages, then its oldest messages. The person's own words go last.
fn fit(events: &mut Vec<TraceEvent>, budget: usize) {
    let size = |es: &[TraceEvent]| es.iter().map(|e| e.text.len() + 24).sum::<usize>();
    for who in ["result", "command"] {
        if size(events) <= budget {
            return;
        }
        events.retain(|e| e.who != who);
    }
    for max in [1200, 500] {
        if size(events) <= budget {
            return;
        }
        for e in events.iter_mut().filter(|e| e.who == "agent" || e.who == "notes") {
            e.text = clip(&e.text, max);
        }
    }
    while size(events) > budget {
        match events.iter().position(|e| e.who != "user") {
            Some(i) => { events.remove(i); }
            None => {
                events.remove(0);
            }
        }
    }
}

pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn finish(mut found: Vec<(AgentSession, Vec<(String, String)>)>) -> SessionContext {
    // Most files touched first, then most recent.
    found.sort_by(|a, b| b.0.files_touched.len().cmp(&a.0.files_touched.len()).then(b.0.ended.cmp(&a.0.ended)));
    found.truncate(MAX_SESSIONS);

    let mut prompts: Vec<(String, String, String)> = found.iter()
        .flat_map(|(s, ps)| ps.iter().map(move |(ts, text)| (ts.clone(), s.id.clone(), text.clone())))
        .collect();
    prompts.sort();
    if prompts.len() > MAX_PROMPTS {
        prompts.drain(..prompts.len() - MAX_PROMPTS);
    }
    let requirements = prompts.into_iter().enumerate()
        .map(|(i, (timestamp, session, text))| Requirement { id: format!("R{}", i + 1), session, timestamp, text })
        .collect();
    let mut sessions: Vec<AgentSession> = found.into_iter().map(|(s, _)| s).collect();
    sessions.sort_by(|a, b| a.started.cmp(&b.started));
    SessionContext { sessions, requirements }
}

/// Claude Code's project directory name: the absolute path with every
/// non-alphanumeric character replaced by '-'.
fn project_slug(root: &Path) -> String {
    root.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// Recognizes mentions of changed files in tool inputs: absolute or
/// repo-relative paths, and distinctive `dir/file` suffixes (agents often `cd`
/// into a directory and edit by relative path).
struct PathMatchers {
    root: String,
    changed: Vec<String>,
    suffixes: Vec<(String, usize)>,
}

impl PathMatchers {
    fn new(root: &Path, changed: &[String]) -> Self {
        let mut suffixes = Vec::new();
        for (i, p) in changed.iter().enumerate() {
            let parts: Vec<&str> = p.split('/').collect();
            if parts.len() >= 2 {
                suffixes.push((parts[parts.len() - 2..].join("/"), i));
            }
            // A file name alone only when it's distinctive.
            let name = parts[parts.len() - 1];
            if name.len() >= 8 && changed.iter().filter(|q| q.ends_with(&format!("/{name}")) || *q == name).count() == 1 {
                suffixes.push((name.to_string(), i));
            }
        }
        PathMatchers { root: root.to_string_lossy().trim_end_matches('/').to_string(), changed: changed.to_vec(), suffixes }
    }

    /// Changed files an edit tool's `file_path` points at.
    fn file(&self, path: &str) -> Option<usize> {
        let rel = path.strip_prefix(&self.root).map(|r| r.trim_start_matches('/')).unwrap_or(path);
        self.changed.iter().position(|c| c == rel)
    }

    /// Changed files a shell command names, and whether it writes them. A command run in another
    /// directory names other files: there, a bare `README.md` is some other project's.
    fn command(&self, cmd: &str, cwd: Option<&str>) -> (Vec<usize>, bool) {
        let elsewhere = cwd.is_some_and(|c| !self.inside(c)) || self.cds_elsewhere(cmd);
        let touches: Vec<usize> = if elsewhere {
            self.changed.iter().enumerate().filter(|(_, c)| cmd.contains(&format!("{}/{c}", self.root))).map(|(i, _)| i).collect()
        } else {
            self.mentions(cmd)
        };
        let writes = !touches.is_empty() && writes_files(cmd);
        (touches, writes)
    }

    fn inside(&self, dir: &str) -> bool {
        let dir = dir.trim_end_matches('/');
        dir == self.root || dir.starts_with(&format!("{}/", self.root))
    }

    /// Whether the command `cd`s to a directory outside the repository.
    fn cds_elsewhere(&self, cmd: &str) -> bool {
        let home = std::env::var("HOME").unwrap_or_default();
        cmd.match_indices("cd ").any(|(i, _)| {
            if cmd[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                return false;
            }
            let dir: String = cmd[i + 3..].trim_start().chars().take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '&' | '|' | ')')).collect();
            let dir = dir.trim_matches(|c| c == '"' || c == '\'');
            let dir = match dir.strip_prefix('~') { Some(rest) if !home.is_empty() => format!("{home}{rest}"), _ => dir.to_string() };
            dir.starts_with('/') && !self.inside(&dir)
        })
    }

    /// `path` relative to the repository, when it's inside it.
    fn relative<'p>(&self, path: &'p str) -> &'p str {
        path.strip_prefix(&self.root).map(|r| r.trim_start_matches('/')).unwrap_or(path)
    }

    /// Changed files mentioned in a shell command.
    fn mentions(&self, text: &str) -> Vec<usize> {
        let mut hits: Vec<usize> = self.changed.iter().enumerate()
            .filter(|(_, c)| text.contains(c.as_str()))
            .map(|(i, _)| i)
            .collect();
        for (s, i) in &self.suffixes {
            if !hits.contains(i) && contains_path_token(text, s) {
                hits.push(*i);
            }
        }
        hits
    }
}

/// `needle` appears in `text` not glued to other path characters.
fn contains_path_token(text: &str, needle: &str) -> bool {
    let is_path = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.');
    text.match_indices(needle).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + needle.len()..].chars().next();
        !before.is_some_and(|c| is_path(c) && c != '.') && !after.is_some_and(is_path)
    })
}

/// What one transcript line or block was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Something the person typed.
    User,
    /// What the agent said.
    Agent,
    /// The agent's thinking, when the transcript keeps its text.
    Notes,
    /// A tool call: an edit, or a command.
    Tool,
    /// What a tool call returned.
    Result,
    /// The agent's context was summarized here, so it forgot the details before.
    Compaction,
}

#[derive(Debug, Clone)]
struct Event {
    ts: String,
    kind: Kind,
    text: String,
    /// Changed files a tool call edits or names.
    touches: Vec<usize>,
    /// Whether the tool call changed those files, as far as the transcript shows.
    writes: bool,
    /// Whether it changed some other file: work on something else.
    writes_other: bool,
    is_error: bool,
}

impl Event {
    fn new(ts: &str, kind: Kind, text: String) -> Self {
        Event { ts: ts.to_string(), kind, text, touches: vec![], writes: false, writes_other: false, is_error: false }
    }
}

/// A whole transcript, before it's cut to the change.
struct Transcript {
    agent: &'static str,
    id: String,
    events: Vec<Event>,
}

/// The part of a session that can have produced the change.
struct Relevant {
    info: AgentSession,
    events: Vec<Event>,
}

/// Cuts a transcript to the window, and then to where it moves on to other work after its last edit of
/// a changed file: the prompt before it starts writing other files, or one after a long break. The talk
/// right after the last edit stays, since a choice like "only dotfiles for now" often needs no new edit.
/// `None` if it never touched a changed file in the window.
fn relevant(t: Transcript, m: &PathMatchers, since: i64, until: i64) -> Option<Relevant> {
    let mut events: Vec<Event> = t.events.into_iter()
        .filter(|e| parse_iso(&e.ts).is_some_and(|ts| ts >= since && ts <= until))
        .collect();
    // Reading a file (cat, grep, git diff) doesn't make a session part of the change. Writing it does.
    let mut touched = vec![false; m.changed.len()];
    for i in events.iter().filter(|e| e.writes).flat_map(|e| &e.touches) {
        touched[*i] = true;
    }
    let files_touched: Vec<String> = m.changed.iter().zip(&touched).filter(|(_, t)| **t).map(|(c, _)| c.clone()).collect();
    if files_touched.is_empty() {
        return None;
    }
    let last = events.iter().rposition(|e| e.writes && !e.touches.is_empty()).unwrap_or(0);
    let secs = |e: &Event| parse_iso(&e.ts).unwrap_or(0);
    let mut cut = events.len();
    let mut prompt = None;
    for i in last + 1..events.len() {
        let e = &events[i];
        if e.kind == Kind::User {
            if secs(e) - secs(&events[i - 1]) > BREAK_SECS {
                cut = i;
                break;
            }
            prompt = Some(i);
        } else if e.writes_other {
            cut = prompt.unwrap_or(i);
            break;
        }
    }
    events.truncate(cut);
    let info = AgentSession {
        agent: t.agent.into(),
        id: t.id,
        started: events.first().map(|e| e.ts.clone()).unwrap_or_default(),
        ended: events.last().map(|e| e.ts.clone()).unwrap_or_default(),
        files_touched,
    };
    Some(Relevant { info, events })
}

/// Whether a shell command changes files, rather than only reading them. Agents often edit with
/// scripts, redirects and in-place tools instead of their edit tool.
fn writes_files(cmd: &str) -> bool {
    // Shell tools, which only count where a command starts: "rm " inside "perform " isn't one.
    const TOOLS: [&str; 12] = ["sed -i", "perl -pi", "tee ", "git apply", "patch -", "patch <", "mv ", "cp ", "rm ", "truncate ",
        "git checkout -- ", "git restore "];
    // Edits from inside scripts and patch tools, wherever they appear.
    const CALLS: [&str; 5] = ["apply_patch", ".write(", "write_text(", "writeFile", "File.write"];
    let starts_command = |i: usize| cmd[..i].trim_end_matches(' ').chars().next_back().is_none_or(|c| matches!(c, ';' | '&' | '|' | '(' | '`' | '\n' | '{'))
        || cmd[..i].ends_with("sudo ") || cmd[..i].ends_with("xargs ");
    TOOLS.iter().any(|w| cmd.match_indices(w).any(|(i, _)| starts_command(i))) || CALLS.iter().any(|w| cmd.contains(w)) || redirects_to_file(cmd)
}

/// A `>` or `>>` into a file. Not `2>&1`, `> /dev/null`, or a comparison in an inline script.
fn redirects_to_file(cmd: &str) -> bool {
    let b = cmd.as_bytes();
    (0..b.len()).filter(|&i| b[i] == b'>').any(|i| {
        let before = i.checked_sub(1).map(|j| b[j]);
        // `2>`, `&>`, `=>`, `->`, `>>` (handled at its first `>`), and `<>` aren't a plain redirect start.
        if before.is_some_and(|c| c.is_ascii_digit() || matches!(c, b'&' | b'=' | b'-' | b'>' | b'<')) {
            return false;
        }
        let rest = cmd[i + 1..].trim_start_matches('>').trim_start();
        let target: String = rest.chars().take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '&' | '|' | ')')).collect();
        let target = target.trim_matches(|c| c == '"' || c == '\'');
        !target.is_empty() && !target.starts_with("/dev/") && !target.starts_with('&') && !target.starts_with('=')
            && !target.starts_with(|c: char| c.is_ascii_digit()) && (target.contains('.') || target.contains('/'))
    })
}

/// A Claude Code transcript as events.
fn read_claude(path: &Path, m: &PathMatchers) -> Option<Transcript> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut id = path.file_stem()?.to_string_lossy().into_owned();
    let mut events = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let Some(ts) = v["timestamp"].as_str() else { continue };
        // Subagents' work and the harness's own messages aren't the conversation.
        if v["isSidechain"].as_bool() == Some(true) || v["isMeta"].as_bool() == Some(true) {
            continue;
        }
        if let Some(s) = v["sessionId"].as_str() {
            id = s.to_string();
        }
        let content = &v["message"]["content"];
        match v["type"].as_str() {
            Some("user") if v["isCompactSummary"].as_bool() == Some(true) => events.push(Event::new(ts, Kind::Compaction, String::new())),
            Some("user") => {
                for b in content.as_array().into_iter().flatten().filter(|b| b["type"] == "tool_result") {
                    let mut e = Event::new(ts, Kind::Result, block_text(&b["content"]));
                    e.is_error = b["is_error"].as_bool() == Some(true);
                    events.push(e);
                }
                if let Some(t) = user_text(content) {
                    events.push(Event::new(ts, Kind::User, t));
                }
            }
            Some("assistant") => {
                for b in content.as_array().into_iter().flatten() {
                    match b["type"].as_str() {
                        Some("text") => events.push(Event::new(ts, Kind::Agent, b["text"].as_str().unwrap_or("").to_string())),
                        Some("thinking") => {
                            let t = b["thinking"].as_str().unwrap_or("").trim();
                            if !t.is_empty() {
                                events.push(Event::new(ts, Kind::Notes, t.to_string()));
                            }
                        }
                        Some("tool_use") => {
                            let input = &b["input"];
                            let name = b["name"].as_str().unwrap_or("");
                            let mut e = Event::new(ts, Kind::Tool, String::new());
                            match name {
                                "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                                    let p = input["file_path"].as_str().or(input["notebook_path"].as_str()).unwrap_or("");
                                    e.touches.extend(m.file(p));
                                    e.writes = !e.touches.is_empty();
                                    e.writes_other = e.touches.is_empty();
                                    e.text = format!("{name} {}", m.relative(p));
                                }
                                "Bash" => {
                                    let cmd = input["command"].as_str().unwrap_or("");
                                    (e.touches, e.writes) = m.command(cmd, v["cwd"].as_str());
                                    e.writes_other = e.touches.is_empty() && writes_files(cmd);
                                    e.text = cmd.to_string();
                                }
                                _ => continue,
                            }
                            events.push(e);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Some(Transcript { agent: "claude-code", id, events })
}

/// A Codex rollout as events, if it ran in this repository.
fn read_codex(path: &Path, root: &Path, m: &PathMatchers) -> Option<Transcript> {
    let text = std::fs::read_to_string(path).ok()?;
    let meta = text.lines().take(5)
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "session_meta")?;
    let cwd = meta["payload"]["cwd"].as_str()?;
    let root = root.to_string_lossy();
    if cwd != root && !cwd.starts_with(&format!("{root}/")) {
        return None;
    }
    let id = meta["payload"]["id"].as_str().or(meta["payload"]["session_id"].as_str()).unwrap_or("codex").to_string();
    let mut events = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v["type"] != "response_item" {
            continue;
        }
        let Some(ts) = v["timestamp"].as_str() else { continue };
        let p = &v["payload"];
        let texts = |kinds: &[&str]| p["content"].as_array().into_iter().flatten()
            .filter(|c| c["type"].as_str().is_some_and(|t| kinds.contains(&t)))
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        match p["type"].as_str().unwrap_or("") {
            "message" if p["role"] == "user" => {
                if let Some(t) = user_text(&serde_json::Value::String(texts(&["input_text", "text"]))) {
                    events.push(Event::new(ts, Kind::User, t));
                }
            }
            "message" if p["role"] == "assistant" => events.push(Event::new(ts, Kind::Agent, texts(&["output_text", "text"]))),
            "reasoning" => {
                let t = p["summary"].as_array().into_iter().flatten().filter_map(|s| s["text"].as_str()).collect::<Vec<_>>().join("\n");
                if !t.trim().is_empty() {
                    events.push(Event::new(ts, Kind::Notes, t));
                }
            }
            // Shell commands, patches and scripted edits all carry the paths they touch.
            "function_call" | "custom_tool_call" | "local_shell_call" => {
                let input = [&p["arguments"], &p["input"], &p["action"]].iter()
                    .map(|x| if x.is_string() { x.as_str().unwrap_or("").to_string() } else if x.is_null() { String::new() } else { x.to_string() })
                    .collect::<Vec<_>>()
                    .join(" ");
                let mut e = Event::new(ts, Kind::Tool, String::new());
                (e.touches, e.writes) = m.command(&input, None);
                e.writes_other = e.touches.is_empty() && writes_files(&input);
                e.text = input;
                events.push(e);
            }
            "function_call_output" | "custom_tool_call_output" => events.push(Event::new(ts, Kind::Result, block_text(&p["output"]))),
            _ => {}
        }
    }
    Some(Transcript { agent: "codex", id, events })
}

/// Text from a tool result's content: a string, or a list of text blocks.
fn block_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// The text a person typed, or `None` for tool results and harness messages.
fn user_text(content: &serde_json::Value) -> Option<String> {
    let raw = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => {
            if blocks.iter().any(|b| b["type"] == "tool_result") {
                return None;
            }
            blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")
        }
        _ => return None,
    };
    let text = strip_tag_blocks(&raw, "system-reminder");
    let t = text.trim();
    // Slash-command wrappers, command output, task notifications, interrupts,
    // and instructions the harness injects as "user" messages (Codex sends the
    // repo's AGENTS.md this way).
    if t.is_empty() || t.starts_with('<') || t.starts_with("Caveat:") || t.starts_with("[Request interrupted")
        || t.starts_with("# AGENTS.md instructions") || t.contains("<INSTRUCTIONS>")
    {
        return None;
    }
    Some(t.to_string())
}

/// A prompt worth checking the change against, shortened to `MAX_PROMPT_CHARS`.
fn requirement_text(t: &str) -> Option<String> {
    // Replies like "yes", "2" or "sounds good" only make sense next to the
    // agent's proposal; on their own they aren't requirements.
    if t.split_whitespace().count() < MIN_PROMPT_WORDS {
        return None;
    }
    let mut out: String = t.chars().take(MAX_PROMPT_CHARS).collect();
    if t.chars().count() > MAX_PROMPT_CHARS {
        out.push('…');
    }
    Some(out)
}

fn strip_tag_blocks(text: &str, tag: &str) -> String {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        out.push_str(&rest[..start]);
        match rest[start..].find(&close) {
            Some(end) => rest = &rest[start + end + close.len()..],
            None => { rest = ""; break; }
        }
    }
    out.push_str(rest);
    out
}

/// Seconds since the epoch for `YYYY-MM-DDTHH:MM:SS[.fff]Z`.
fn parse_iso(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from civil (Howard Hinnant).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + s)
}

/// Check that a quote really appears in a requirement (case- and whitespace-insensitive).
pub fn quote_in(quote: &str, text: &str) -> bool {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let q = norm(quote.trim_matches(|c: char| c == '"' || c == '\'' || c == '…' || c.is_whitespace()));
    q.len() >= 3 && norm(text).contains(&q)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_parse() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-09-24T01:28:05.069Z"), Some(1790213285));
    }

    #[test]
    fn prompts_and_touches() {
        let dir = std::env::temp_dir().join(format!("perspica-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = Path::new("/work/my.repo");
        let proj = dir.join(project_slug(root));
        std::fs::create_dir_all(&proj).unwrap();
        let lines = [
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00Z","sessionId":"s1","message":{"content":"too early"}}"#,
            r#"{"type":"user","timestamp":"2026-02-01T00:00:00Z","sessionId":"s1","message":{"content":"Add retries to the fetcher <system-reminder>ignore me</system-reminder>"}}"#,
            r#"{"type":"user","timestamp":"2026-02-01T00:00:01Z","sessionId":"s1","isMeta":true,"message":{"content":"meta"}}"#,
            r#"{"type":"user","timestamp":"2026-02-01T00:00:02Z","sessionId":"s1","message":{"content":"<task-notification>done</task-notification>"}}"#,
            r#"{"type":"user","timestamp":"2026-02-01T00:00:03Z","sessionId":"s1","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-02-01T00:00:04Z","sessionId":"s1","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cd /work/my.repo/src && python3 - <<'EOF'\np='net/fetch.rs'\nopen(p,'w').write(s)\nEOF"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-02-01T00:01:00Z","sessionId":"s1","message":{"content":"and cap them at 3"}}"#,
            r#"{"type":"assistant","timestamp":"2026-02-01T00:01:05Z","sessionId":"s1","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/work/my.repo/src/net/fetch.rs"}}]}}"#,
        ];
        std::fs::write(proj.join("s1.jsonl"), lines.join("\n")).unwrap();
        // An unrelated session in the same repo.
        std::fs::write(proj.join("s2.jsonl"), r#"{"type":"user","timestamp":"2026-02-02T00:00:00Z","sessionId":"s2","message":{"content":"unrelated"}}"#).unwrap();
        let since = parse_iso("2026-01-15T00:00:00Z").unwrap();
        let ctx = find_in(Some(&dir), None, root, &["src/net/fetch.rs".into(), "README.md".into()], since, None);
        let bounded = find_in(Some(&dir), None, root, &["src/net/fetch.rs".into()], since, parse_iso("2026-02-01T00:00:30Z"));
        assert_eq!(bounded.requirements.len(), 1, "prompts after the commit are excluded");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(ctx.sessions.len(), 1);
        assert_eq!(ctx.sessions[0].files_touched, vec!["src/net/fetch.rs"]);
        let texts: Vec<&str> = ctx.requirements.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["Add retries to the fetcher", "and cap them at 3"]);
        assert_eq!(ctx.requirements[1].id, "R2");
    }

    #[test]
    fn codex_sessions_and_missing_dirs() {
        let dir = std::env::temp_dir().join(format!("perspica-codex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let day = dir.join("2026/02/01");
        std::fs::create_dir_all(&day).unwrap();
        let lines: [&str; 4] = [
            r#"{"timestamp":"2026-02-01T00:00:00Z","type":"session_meta","payload":{"id":"cx1","cwd":"/work/app/sub"}}"#,
            r#"{"timestamp":"2026-02-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>cwd</environment_context>"}]}}"#,
            r#"{"timestamp":"2026-02-01T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Make the parser accept trailing commas"}]}}"#,
            &serde_json::json!({"timestamp": "2026-02-01T00:00:03Z", "type": "response_item", "payload": {"type": "function_call", "name": "shell",
                "arguments": serde_json::json!({"command": ["bash", "-lc", "apply_patch <<'EOF'\n*** Update File: src/parse.rs\nEOF"]}).to_string()}}).to_string(),
        ];
        std::fs::write(day.join("rollout-a.jsonl"), lines.join("\n")).unwrap();
        // Another repository's session is ignored.
        std::fs::write(day.join("rollout-b.jsonl"), lines[0].replace("/work/app/sub", "/elsewhere")).unwrap();
        let since = parse_iso("2026-01-01T00:00:00Z").unwrap();
        let ctx = find_in(None, Some(&dir), Path::new("/work/app"), &["src/parse.rs".into()], since, None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(ctx.sessions.len(), 1);
        assert_eq!(ctx.sessions[0].agent, "codex");
        assert_eq!(ctx.requirements.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), vec!["Make the parser accept trailing commas"]);
        // No agent directories at all: empty, no error.
        let none = find_in(Some(Path::new("/nonexistent/claude")), Some(Path::new("/nonexistent/codex")), Path::new("/work/app"), &["a.rs".into()], 0, None);
        assert!(none.is_empty() && none.sessions.is_empty());
    }

    #[test]
    fn only_writes_count_and_the_session_ends_at_its_last_edit() {
        let dir = std::env::temp_dir().join(format!("perspica-writes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = Path::new("/work/app");
        let proj = dir.join(project_slug(root));
        std::fs::create_dir_all(&proj).unwrap();
        let line = |ts: &str, kind: &str, content: serde_json::Value, cwd: &str| serde_json::json!({
            "type": kind, "timestamp": ts, "sessionId": "s", "cwd": cwd, "message": {"content": content}}).to_string();
        let bash = |cmd: &str| serde_json::json!([{"type": "tool_use", "name": "Bash", "input": {"command": cmd}}]);
        let s1 = [
            line("2026-02-01T00:00:00Z", "user", "Make the cache expire entries after an hour".into(), "/work/app"),
            line("2026-02-01T00:00:01Z", "assistant", bash("grep -n ttl src/cache.py 2>/dev/null"), "/work/app"),
            line("2026-02-01T00:00:02Z", "assistant", serde_json::json!([{"type": "tool_use", "name": "Edit", "input": {"file_path": "/work/app/src/cache.py"}}]), "/work/app"),
            line("2026-02-01T00:00:03Z", "assistant", serde_json::json!([{"type": "text", "text": "Done. Entries now expire after 3600s. Should a read refresh the timer?"}]), "/work/app"),
            line("2026-02-01T00:00:04Z", "user", "No, keep it fixed from the write".into(), "/work/app"),
            serde_json::json!({"type": "user", "timestamp": "2026-02-01T00:00:04Z", "sessionId": "s", "isCompactSummary": true,
                "message": {"content": "This session is being continued from a previous conversation"}}).to_string(),
            // Later work in another repository, with a README.md of its own.
            line("2026-02-01T00:05:00Z", "user", "Now update the other project's readme".into(), "/work/app"),
            line("2026-02-01T00:05:01Z", "assistant", bash("cd /work/other && cat > README.md <<'EOF'\nhi\nEOF"), "/work/app"),
        ];
        std::fs::write(proj.join("s1.jsonl"), s1.join("\n")).unwrap();
        // A session that only read the files isn't part of the change.
        let s2 = [
            line("2026-02-01T01:00:00Z", "user", "Explain what this diff does to the cache".into(), "/work/app"),
            line("2026-02-01T01:00:01Z", "assistant", bash("git diff src/cache.py README.md > /dev/null"), "/work/app"),
        ];
        std::fs::write(proj.join("s2.jsonl"), s2.join("\n")).unwrap();
        let changed = ["src/cache.py".to_string(), "README.md".to_string()];
        let found = relevant_sessions(Some(&dir), None, root, &changed, 0, None);
        let ctx = find_in(Some(&dir), None, root, &changed, 0, None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found.len(), 1, "the read-only session is left out");
        assert_eq!(found[0].info.files_touched, vec!["src/cache.py"], "the other project's README.md isn't this one");
        let kinds: Vec<Kind> = found[0].events.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![Kind::User, Kind::Tool, Kind::Tool, Kind::Agent, Kind::User, Kind::Compaction], "keeps the choice made after the last edit, ends where other work starts");
        assert_eq!(ctx.requirements.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), vec!["Make the cache expire entries after an hour", "No, keep it fixed from the write"]);
    }

    #[test]
    fn trace_fits_its_budget_and_keeps_the_persons_words() {
        let ev = |who: &str, text: &str| TraceEvent { n: 0, ts: String::new(), session: "s".into(), who: who.into(), text: text.into() };
        let mut es = vec![ev("user", "add a cache"), ev("command", &"x".repeat(300)), ev("agent", &"y".repeat(5000)), ev("result", "ok"), ev("user", "yes, do 2")];
        fit(&mut es, 1000);
        let whos: Vec<&str> = es.iter().map(|e| e.who.as_str()).collect();
        assert_eq!(whos, vec!["user", "agent", "user"]);
        assert!(es[1].text.chars().count() <= 501);
        let mut tight = es.clone();
        fit(&mut tight, 60);
        assert!(tight.iter().all(|e| e.who == "user"));
    }

    #[test]
    fn writes_and_reads() {
        for w in ["cat > src/a.rs <<'EOF'", "echo x >> notes/todo.md", "sed -i '' 's/a/b/' a.rs", "python3 - <<'EOF'\nopen(p,'w').write(s)\nEOF", "apply_patch <<'EOF'"] {
            assert!(writes_files(w), "{w}");
        }
        for w in ["rm a.rs", "cd src && mv a.rs b.rs", "(cp a.rs b.rs)", "git status; rm -f old.rs", "find . -name '*.o' | xargs rm "] {
            assert!(writes_files(w), "{w}");
        }
        for r in ["cargo test 2>&1 | tail", "grep -n x a.rs > /dev/null", "git diff a.rs", "python3 -c 'print(a > b)'", "node -e 'x => x >= 2'", "cat a.rs | head",
            "grep -rn 'perform ' a.rs", "lsof -i tcp 8080 a.rs", "echo 'confirm the form ' a.rs"] {
            assert!(!writes_files(r), "{r}");
        }
    }

    #[test]
    fn quotes() {
        assert!(quote_in("\"cap them  at 3\"", "and Cap them at 3 please"));
        assert!(!quote_in("cap them at 5", "and cap them at 3"));
    }
}
