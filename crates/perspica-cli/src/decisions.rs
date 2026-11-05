//! Decisions: how a change was decided, from the coding-agent sessions that made it. The model groups
//! the session into topics, says who made each choice (you, the agent with your agreement, or the agent
//! on its own), and picks the moments that tell how each topic was settled. Every quote is checked
//! against the transcript, and anything that doesn't match is dropped. Only for the local user's own
//! changes: other people's sessions aren't on this machine.

use crate::intel::{self, IntelInput};
use crate::sessions::{self, TraceEvent};
use serde::{Deserialize, Serialize};

const MAX_TOPICS: usize = 6;
const MAX_DECISIONS: usize = 4;
const MAX_MOMENTS: usize = 10;
/// Quotes longer than this aren't quotes any more.
const MAX_QUOTE_WORDS: usize = 60;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Decisions {
    pub provider: String,
    pub model: String,
    /// Unix seconds.
    pub saved_at: u64,
    /// How long the model took.
    #[serde(default)]
    pub seconds: u64,
    /// How many session events it read.
    #[serde(default)]
    pub events: usize,
    #[serde(default)]
    pub topics: Vec<Topic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topic {
    /// "t1", "t2", … most important first.
    pub id: String,
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub decisions: Vec<Decision>,
    /// How the topic was settled, oldest first.
    #[serde(default)]
    pub timeline: Vec<Moment>,
    /// Limits the agent named that the change leaves unresolved.
    #[serde(default)]
    pub open: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    /// "you": the person asked for or decided it. "approved": the agent proposed it and the person
    /// agreed. "agent": the agent decided it without asking.
    pub who: String,
    /// The agent's choice is in the diff, but the session never discusses it.
    #[serde(default)]
    pub unseen: bool,
    pub title: String,
    /// What a reviewer should check, when there's something specific.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look: Option<String>,
    /// When it was decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// The timeline moment where it was decided, when that moment is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moment: Option<usize>,
    #[serde(default)]
    pub code: Vec<CodeRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeRef {
    pub name: String,
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Moment {
    pub ts: String,
    /// "you", "agent", "notes" (the agent's thinking) or "run" (a command or its output).
    pub who: String,
    pub quote: String,
    /// A short tag, like "Changed course".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

const SPEC: &str = r#"Group how this change was decided into TOPICS. A topic is one thread of work where related decisions were discussed and settled together, for example "how failed requests are retried" or "where settings are validated". Most changes have 1 to 6 topics. Only include topics whose outcome is in the changes, and skip routine steps like running tests, fixing typos or formatting. If the session didn't shape the change in any way worth a reviewer's time, reply with {"topics": []}. That includes a change that only does exactly what the developer asked, with nothing left to choose.

For each topic:
- "title": a short name, at most 7 words, sentence case.
- "summary": one plain sentence on what was settled.
- "decisions": the 1 to 4 choices in this topic a reviewer would weigh in on, each one short sentence (at most 18 words). Include the developer's explicit requirements (names, signatures, constraints, "add a test"), merge related choices into one (but never a developer's choice with one the agent made alone), and leave out details nobody would debate, like thresholds, limits, docs and wording. Say who made it:
  "you": the developer asked for it or decided it in their own words.
  "approved": the agent proposed it, or offered options, and the developer agreed or picked one, even loosely ("yes", "do 1 and 2", "the first one", "only X for now"). A developer's pick among the agent's options is never "agent", and neither is something the agent had already built that the developer then confirmed or picked when offered the choice.
  "agent": the agent decided it without asking. This includes details the agent added while implementing after the developer agreed to a proposal, when the proposal didn't include them. These matter most to a reviewer, so be exact: only what the developer actually said or agreed to is theirs.
  "unseen": it's in the changes, but the session never discusses it.
  Give "symbols" (names of functions, types or constants in the changes), "files", and "event": the number of the event where it was decided (0 for "unseen"). For "you" and "approved", "event" is the developer's own message that asked for, picked or agreed to it. Before calling anything "agent", look through the developer's later messages: if one picks or confirms it, it's "approved", with that message as its event.
  Add "look" only when a reviewer should check something specific, in one plain sentence. Most decisions don't need it.
- "timeline": the 3 to 10 moments that tell how the topic was settled, in order: the ask, proposals, measurements, turning points and the developer's calls. Each has "event" and a "quote" copied character for character from that event, at most 40 words. Quotes are checked against the transcript, and any that don't match are dropped. Add "label" (at most 4 words) only for a turning point ("Changed course") or to say what a measurement was.
- "open": limits or risks the agent named that this change leaves unresolved, each one short sentence with an "event" and "quote". Often none.

Write plainly, for a reviewer who wasn't in the session. Say "the agent" and "the developer", never "I" or "you".

Reply with only JSON: {"topics": [{"title": "...", "summary": "...", "decisions": [{"who": "...", "title": "...", "symbols": [], "files": [], "event": 0, "look": "..."}], "timeline": [{"event": 1, "quote": "...", "label": "..."}], "open": [{"title": "...", "event": 1, "quote": "..."}]}]}
Most important topic first."#;

pub fn prompt(input: &IntelInput<'_>, trace: &[TraceEvent]) -> String {
    let author = input.author_context
        .filter(|c| !c.trim().is_empty())
        .map(|c| format!("AUTHOR CONTEXT (commit messages; may be incomplete):\n{}\n\n", intel::truncate(c, 3000)))
        .unwrap_or_default();
    let session: String = trace.iter()
        .map(|e| format!("[{}] {} {}: {}\n", e.n, e.ts.get(..16).unwrap_or(&e.ts).replace('T', " "), e.who, e.text))
        .collect();
    format!(
        "You're helping a developer review a change that a coding agent made with them. Below are the change \
         and the session that produced it, as numbered events. \"user\" is the developer, \"agent\" is what the \
         agent said, \"notes\" are the agent's thinking, \"edit\" and \"command\" are its tool calls on the \
         changed files, \"result\" is what a command returned, and \"compaction\" marks where the agent's \
         memory of the session was summarized.\n\n\
         {author}CHANGES (id [type] location name: detail):\n{}\n\nCODE:\n{}\n\nSESSION:\n{session}\n{SPEC}",
        intel::build_entry_listing(input),
        intel::build_rich_context(input.results, input.sources, input.budget),
    )
}

#[derive(Deserialize)]
struct RawReply {
    #[serde(default)]
    topics: Vec<RawTopic>,
}

#[derive(Deserialize)]
struct RawTopic {
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    decisions: Vec<RawDecision>,
    #[serde(default)]
    timeline: Vec<RawQuote>,
    #[serde(default)]
    open: Vec<RawQuote>,
}

#[derive(Deserialize)]
struct RawDecision {
    #[serde(default)]
    who: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    symbols: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    event: usize,
    #[serde(default)]
    look: Option<String>,
}

#[derive(Deserialize)]
struct RawQuote {
    #[serde(default)]
    title: String,
    #[serde(default)]
    event: usize,
    #[serde(default)]
    quote: String,
    #[serde(default)]
    label: Option<String>,
}

/// The topics, keeping only what the transcript and the code back up. `sources` are the changed files
/// as (path, old, new).
pub fn parse(response: &str, trace: &[TraceEvent], sources: &[(String, String, String)]) -> Result<Vec<Topic>, String> {
    let start = response.find('{').ok_or("The reply had no JSON.")?;
    let end = response.rfind('}').ok_or("The reply had no JSON.")?;
    let raw: RawReply = serde_json::from_str(&response[start..=end]).map_err(|e| format!("Couldn't read the reply: {e}"))?;
    let event = |n: usize| trace.iter().find(|e| e.n == n);
    let checked = |q: &RawQuote| -> Option<(usize, &TraceEvent, String)> {
        let e = event(q.event)?;
        let quote = q.quote.trim().trim_matches(|c| c == '"' || c == '“' || c == '”').trim().to_string();
        (quote.split_whitespace().count() <= MAX_QUOTE_WORDS && sessions::quote_in(&quote, &e.text)).then_some((q.event, e, quote))
    };
    let quotes = raw.topics.iter().map(|t| t.timeline.len() + t.open.len()).sum::<usize>();
    let mut topics = Vec::new();
    for t in raw.topics.into_iter().take(MAX_TOPICS) {
        let mut moments: Vec<(usize, Moment)> = t.timeline.iter()
            .filter_map(|q| {
                let (n, e, quote) = checked(q)?;
                let who = match e.who.as_str() { "user" => "you", "agent" => "agent", "notes" => "notes", "compaction" => return None, _ => "run" };
                let label = q.label.as_deref().map(str::trim).filter(|l| !l.is_empty()).map(|l| clip(l, 40));
                Some((n, Moment { ts: e.ts.clone(), who: who.into(), quote, label }))
            })
            .collect();
        moments.sort_by(|a, b| a.0.cmp(&b.0));
        moments.dedup_by(|a, b| a.0 == b.0 && a.1.quote == b.1.quote);
        moments.truncate(MAX_MOMENTS);
        let decisions: Vec<Decision> = t.decisions.into_iter()
            .filter(|d| !d.title.trim().is_empty())
            .take(MAX_DECISIONS)
            .map(|d| {
                let unseen = d.who == "unseen";
                // The developer's decisions point at the developer's own words. One that points elsewhere
                // is still shown as theirs: the label is the model's best reading, and dropping it would hide it.
                let who = match d.who.as_str() { "you" | "approved" => d.who.clone(), _ => "agent".to_string() };
                if (who == "you" || who == "approved") && event(d.event).is_some_and(|e| e.who != "user") {
                    eprintln!("perspica: a decision credited to the developer cites event {}, which isn't theirs", d.event);
                }
                let at = (!unseen).then(|| event(d.event).map(|e| e.ts.clone())).flatten();
                let moment = (!unseen).then(|| moments.iter().position(|(n, _)| *n == d.event)).flatten();
                let code = d.symbols.iter().take(3).filter_map(|s| locate(s, &d.files, sources)).collect();
                let look = d.look.map(|l| l.trim().to_string()).filter(|l| !l.is_empty());
                Decision { who, unseen, title: clip(d.title.trim(), 240), look, at, moment, code }
            })
            .collect();
        let open = t.open.iter().filter(|q| !q.title.trim().is_empty() && checked(q).is_some()).map(|q| clip(q.title.trim(), 240)).collect();
        // A topic needs something a reviewer can check: the moments, or at least one decision.
        if t.title.trim().is_empty() || (moments.is_empty() && decisions.is_empty()) {
            continue;
        }
        topics.push(Topic {
            id: format!("t{}", topics.len() + 1),
            title: clip(t.title.trim(), 80),
            summary: clip(t.summary.trim(), 400),
            decisions,
            timeline: moments.into_iter().map(|(_, m)| m).collect(),
            open,
        });
    }
    let kept = topics.iter().map(|t: &Topic| t.timeline.len() + t.open.len()).sum::<usize>();
    if kept < quotes {
        eprintln!("perspica: decisions kept {kept} of {quotes} quotes; the rest weren't in the transcript");
    }
    Ok(topics)
}

/// Where a symbol is defined in the changed files: the files the model named first, then the rest.
/// Any language: a line that names it as a whole word next to a definition keyword, or failing that,
/// the first line that names it.
fn locate(symbol: &str, files: &[String], sources: &[(String, String, String)]) -> Option<CodeRef> {
    let name = symbol.rsplit(['.', ':', '#', '/']).next().unwrap_or(symbol).trim_end_matches("()").trim();
    if name.len() < 2 || !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$') {
        return None;
    }
    let named = |path: &str| files.iter().any(|f| f == path || path.ends_with(&format!("/{f}")));
    let mut order: Vec<&(String, String, String)> = sources.iter().filter(|s| named(&s.0)).collect();
    order.extend(sources.iter().filter(|s| !named(&s.0)));
    let found = |defining: bool| order.iter().find_map(|(path, _, new)| {
        new.lines().enumerate()
            .find(|(_, l)| names(l, name) && (!defining || defines(l, name)))
            .map(|(i, _)| CodeRef { name: symbol.to_string(), file: path.clone(), line: i + 1 })
    });
    found(true).or_else(|| found(false))
}

fn names(line: &str, name: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    line.match_indices(name).any(|(i, _)| {
        !line[..i].chars().next_back().is_some_and(word) && !line[i + name.len()..].chars().next().is_some_and(word)
    })
}

/// Whether the line looks like it defines `name`, in most languages.
fn defines(line: &str, name: &str) -> bool {
    const KEYWORDS: [&str; 24] = ["fn", "function", "def", "class", "struct", "enum", "interface", "type", "const", "let", "var", "val",
        "func", "module", "impl", "trait", "static", "public", "private", "protected", "async", "export", "#define", "macro_rules!"];
    let t = line.trim_start();
    let Some(i) = t.find(name) else { return false };
    let before: Vec<&str> = t[..i].split(|c: char| c.is_whitespace() || c == '(' || c == '*' || c == '&').filter(|w| !w.is_empty()).collect();
    let after = t[i + name.len()..].trim_start();
    // Not the right side of an assignment, or a call being returned.
    let head = &t[..i];
    (before.iter().any(|w| KEYWORDS.contains(w)) && !head.contains('=') && !head.contains("return "))
        // `name = function`, `name: (…) =>`, `name(args) {` in object literals and class bodies.
        || (before.is_empty() && (after.starts_with('=') || after.starts_with(':') || (after.starts_with('(') && t.trim_end().ends_with('{'))))
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(n: usize, who: &str, text: &str) -> TraceEvent {
        TraceEvent { n, ts: format!("2026-02-01T00:00:{n:02}Z"), session: "s".into(), who: who.into(), text: text.into() }
    }

    #[test]
    fn quotes_that_dont_match_are_dropped() {
        let trace = [ev(1, "user", "Cache the lookups, and keep it simple"), ev(2, "agent", "I'll use an LRU with 1,000 entries."), ev(3, "user", "yes")];
        let sources = [("src/cache.py".to_string(), String::new(), "import x\n\nclass LruCache:\n    def get(self, key):\n        pass\n".to_string())];
        let reply = r#"Here you go: {"topics": [{"title": "Caching lookups", "summary": "Lookups go through an LRU.",
            "decisions": [{"who": "approved", "title": "Use an LRU cache", "symbols": ["LruCache.get", "Missing"], "files": ["src/cache.py"], "event": 2},
                          {"who": "unseen", "title": "Keys are strings", "event": 0}],
            "timeline": [{"event": 2, "quote": "I'll use an LRU with 1,000 entries."}, {"event": 1, "quote": "Cache the lookups,   and keep it SIMPLE"},
                         {"event": 2, "quote": "I'll use a hash map"}, {"event": 9, "quote": "yes"}],
            "open": [{"title": "No eviction metrics", "event": 2, "quote": "not said"}]}]}"#;
        let topics = parse(reply, &trace, &sources).unwrap();
        assert_eq!(topics.len(), 1);
        let t = &topics[0];
        assert_eq!(t.timeline.iter().map(|m| m.who.as_str()).collect::<Vec<_>>(), vec!["you", "agent"], "sorted, and only real quotes");
        assert_eq!(t.decisions[0].moment, Some(1));
        assert_eq!(t.decisions[0].code.len(), 1);
        assert_eq!((t.decisions[0].code[0].file.as_str(), t.decisions[0].code[0].line), ("src/cache.py", 4));
        assert!(t.decisions[1].unseen && t.decisions[1].who == "agent" && t.decisions[1].at.is_none());
        assert!(t.open.is_empty());
    }

    #[test]
    fn definitions_across_languages() {
        for (line, name) in [("pub async fn check_lesson(p: &str) {", "check_lesson"), ("def retry(self, n):", "retry"), ("func (s *Server) Retry() error {", "Retry"),
            ("export const MAX_RETRIES = 3;", "MAX_RETRIES"), ("  retry(attempts) {", "retry"), ("public static int retry(int n) {", "retry"), ("class Backoff < Base", "Backoff")] {
            assert!(defines(line, name), "{line}");
        }
        for (line, name) in [("    return retry(n - 1);", "retry"), ("let x = check_lesson(p);", "check_lesson")] {
            assert!(!defines(line, name), "{line}");
        }
        assert!(names("a(retry)", "retry") && !names("retry_all()", "retry"));
    }

    #[test]
    fn nothing_to_say_is_fine() {
        assert!(parse(r#"{"topics": []}"#, &[], &[]).unwrap().is_empty());
        assert!(parse("no json here", &[], &[]).is_err());
    }
}
