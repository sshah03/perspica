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
    let matchers = PathMatchers::new(root, changed);
    let until = until.unwrap_or(i64::MAX);
    let mut found: Vec<(AgentSession, Vec<(String, String)>)> = Vec::new();
    if let Some(dir) = claude {
        for f in claude_files(dir, root, since) {
            found.extend(read_session(&f, &matchers, since, until));
        }
    }
    if let Some(dir) = codex {
        for f in jsonl_files_since(dir, since, 4) {
            found.extend(read_codex_session(&f, root, &matchers, since, until));
        }
    }
    finish(found)
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

/// (session, prompts as (timestamp, text)) if the transcript touched a changed file after `since`.
fn read_session(path: &Path, m: &PathMatchers, since: i64, until: i64) -> Option<(AgentSession, Vec<(String, String)>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut touched = vec![false; m.changed.len()];
    let mut prompts = Vec::new();
    let (mut first, mut last): (Option<String>, Option<String>) = (None, None);
    let mut id = path.file_stem()?.to_string_lossy().into_owned();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let Some(ts) = v["timestamp"].as_str() else { continue };
        if parse_iso(ts).is_none_or(|t| t < since || t > until) {
            continue;
        }
        if let Some(s) = v["sessionId"].as_str() {
            id = s.to_string();
        }
        match v["type"].as_str() {
            Some("user") if v["isMeta"].as_bool() != Some(true) && v["isSidechain"].as_bool() != Some(true) => {
                if let Some(p) = user_prompt(&v["message"]["content"]) {
                    prompts.push((ts.to_string(), p));
                }
            }
            Some("assistant") => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] != "tool_use" {
                        continue;
                    }
                    let input = &block["input"];
                    match block["name"].as_str().unwrap_or("") {
                        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                            let p = input["file_path"].as_str().or(input["notebook_path"].as_str()).unwrap_or("");
                            if let Some(i) = m.file(p) {
                                touched[i] = true;
                            }
                        }
                        "Bash" => {
                            for i in m.mentions(input["command"].as_str().unwrap_or("")) {
                                touched[i] = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => continue,
        }
        if first.is_none() {
            first = Some(ts.to_string());
        }
        last = Some(ts.to_string());
    }
    let files_touched: Vec<String> = m.changed.iter().zip(&touched).filter(|(_, t)| **t).map(|(c, _)| c.clone()).collect();
    if files_touched.is_empty() {
        return None;
    }
    let session = AgentSession {
        agent: "claude-code".into(),
        id,
        started: first.unwrap_or_default(),
        ended: last.unwrap_or_default(),
        files_touched,
    };
    Some((session, prompts))
}

/// A Codex rollout, if it ran in this repository and touched a changed file in the window.
fn read_codex_session(path: &Path, root: &Path, m: &PathMatchers, since: i64, until: i64) -> Option<(AgentSession, Vec<(String, String)>)> {
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
    let mut touched = vec![false; m.changed.len()];
    let mut prompts = Vec::new();
    let (mut first, mut last): (Option<String>, Option<String>) = (None, None);
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v["type"] != "response_item" {
            continue;
        }
        let Some(ts) = v["timestamp"].as_str() else { continue };
        if parse_iso(ts).is_none_or(|t| t < since || t > until) {
            continue;
        }
        let p = &v["payload"];
        match p["type"].as_str().unwrap_or("") {
            "message" if p["role"] == "user" => {
                let joined: String = p["content"].as_array().into_iter().flatten()
                    .filter(|c| matches!(c["type"].as_str(), Some("input_text" | "text")))
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Some(prompt) = user_prompt(&serde_json::Value::String(joined)) {
                    prompts.push((ts.to_string(), prompt));
                }
            }
            // Shell commands, patches and scripted edits all carry the paths they touch.
            "function_call" | "custom_tool_call" | "local_shell_call" => {
                let input = [&p["arguments"], &p["input"], &p["action"]].iter()
                    .map(|x| if x.is_string() { x.as_str().unwrap_or("").to_string() } else if x.is_null() { String::new() } else { x.to_string() })
                    .collect::<Vec<_>>()
                    .join(" ");
                for i in m.mentions(&input) {
                    touched[i] = true;
                }
            }
            _ => continue,
        }
        if first.is_none() {
            first = Some(ts.to_string());
        }
        last = Some(ts.to_string());
    }
    let files_touched: Vec<String> = m.changed.iter().zip(&touched).filter(|(_, t)| **t).map(|(c, _)| c.clone()).collect();
    if files_touched.is_empty() {
        return None;
    }
    Some((AgentSession { agent: "codex".into(), id, started: first.unwrap_or_default(), ended: last.unwrap_or_default(), files_touched }, prompts))
}

/// The text a person typed, or `None` for tool results and harness messages.
fn user_prompt(content: &serde_json::Value) -> Option<String> {
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
            r#"{"type":"assistant","timestamp":"2026-02-01T00:00:04Z","sessionId":"s1","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cd /work/my.repo/src && python3 - <<'EOF'\np='net/fetch.rs'\nEOF"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-02-01T00:01:00Z","sessionId":"s1","message":{"content":"and cap them at 3"}}"#,
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
    fn quotes() {
        assert!(quote_in("\"cap them  at 3\"", "and Cap them at 3 please"));
        assert!(!quote_in("cap them at 5", "and cap them at 3"));
    }
}
