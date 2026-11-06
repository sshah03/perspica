//! A review on its GitHub pull request, and the Action that turns it into a check.
//!
//! The author posts one comment from their own machine, with `perspica --pr N --post` or Post in the
//! viewer. It's short, with the details folded away, and it ends with the data the Action reads: the
//! reading order, the concerns, the lessons and the quiz. Nothing from the author's agent sessions goes in.
//!
//! The Action (`perspica github action`) never runs code from the pull request. It reads the comments
//! through the API, grades quiz answers that reviewers reply with, and keeps one check up to date. A pull
//! request with no comment still gets perspica's structural review, which needs no model.

use crate::{learn, MultiFileResult};
use serde::{Deserialize, Serialize};
use std::process::Command;

/// Marks perspica's comment, so posting again updates it.
const MARK: &str = "<!-- perspica:comment -->";
const DATA: &str = "<!-- perspica:data ";
const GRADED: &str = "<!-- perspica:graded ";
/// The check's name. Branch protection can require it.
pub const CHECK: &str = "perspica";
/// Who the Action's own comments come from. Nobody else can post as it.
const BOT: &str = "github-actions[bot]";
/// GitHub allows 65,536 characters in a comment.
const MAX_COMMENT: usize = 60_000;
const MAX_ANNOTATIONS: usize = 50;
const HOME: &str = "https://github.com/sshah03/perspica";

// --- What the author chooses to post ---

/// What goes in the comment. Saved, so the next post starts from the same choices.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PostSettings {
    /// Analyze's concerns, folded.
    pub concerns: bool,
    /// The lessons and their quiz.
    pub lessons: bool,
    /// The author's own quiz score, once they've answered every question.
    pub quiz_score: bool,
    /// Ask reviewers to take the quiz before approving. Needs the Action, which grades it.
    pub ask_quiz: bool,
}

impl Default for PostSettings {
    fn default() -> Self {
        PostSettings { concerns: true, lessons: true, quiz_score: true, ask_quiz: false }
    }
}

/// One of the author's quiz answers in the viewer. Only the first answer to a question counts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuizAnswer {
    /// The concept's id and title, as the viewer keys it.
    pub concept: String,
    pub question: String,
    pub choice: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub struct Score {
    pub right: usize,
    pub answered: usize,
    pub total: usize,
}

impl Score {
    pub fn complete(&self) -> bool {
        self.total > 0 && self.answered == self.total
    }
}

/// The author's first-try score on the lessons written so far.
pub fn author_score(learn: Option<&learn::Learn>, answers: &[QuizAnswer]) -> Score {
    let mut s = Score::default();
    for c in learn.into_iter().flat_map(|l| &l.concepts) {
        let key = format!("{}|{}", c.id, c.title);
        for q in c.lesson.iter().flat_map(|l| &l.quiz) {
            s.total += 1;
            if let Some(a) = answers.iter().find(|a| a.concept == key && a.question == q.question) {
                s.answered += 1;
                s.right += usize::from(a.choice == q.answer);
            }
        }
    }
    s
}

// --- The data in the comment ---

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Stamp {
    pub v: u32,
    /// The commit this describes.
    pub sha: String,
    pub version: String,
    pub files: usize,
    pub changed_lines: usize,
    pub mechanical_lines: usize,
    pub test_lines: usize,
    /// Files whose changes are all mechanical (formatting, renames, moves) or generated.
    pub skim: Vec<String>,
    pub summary: Option<String>,
    pub groups: Vec<StampGroup>,
    pub concerns: Vec<String>,
    pub lessons: Vec<StampLesson>,
    pub quiz: Vec<learn::Question>,
    pub ask_quiz: bool,
    /// Right first try, out of how many.
    pub author_score: Option<(usize, usize)>,
    /// Places the change may have broken: references to removed names, calls not updated.
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StampGroup {
    pub label: String,
    pub risk: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StampLesson {
    pub title: String,
    pub tldr: String,
    pub in_this_change: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Annotation {
    pub path: String,
    pub line: usize,
    pub title: String,
    pub message: String,
}

/// What a comment says about `multi`. Only the analysis goes in: never the author's prompts, Decisions,
/// or which groups they asked for.
pub fn stamp(multi: &MultiFileResult, settings: &PostSettings, score: Score, sha: &str) -> Stamp {
    let groups = multi.intent_groups.iter().flatten().filter(|g| !g.mechanical)
        .map(|g| StampGroup { label: g.label.clone(), risk: g.risk.clone(), note: g.review_note.clone() })
        .collect();
    let lessons: Vec<&learn::Lesson> = if settings.lessons {
        multi.learn.iter().flat_map(|l| &l.concepts).filter_map(|c| c.lesson.as_ref()).collect()
    } else {
        vec![]
    };
    let titles = multi.learn.iter().flat_map(|l| &l.concepts).filter(|c| c.lesson.is_some()).map(|c| c.title.clone());
    let quiz: Vec<learn::Question> = lessons.iter().flat_map(|l| l.quiz.iter().cloned()).collect();
    let cf = &multi.cross_file;
    let mut annotations: Vec<Annotation> = cf.broken_references.iter().map(|b| Annotation {
        path: b.reference_file.clone(),
        line: b.reference_location.line_start.max(1),
        title: format!("`{}` {}", b.symbol_name, if b.renamed_from.is_some() { "was renamed" } else { "was removed" }),
        message: b.reason.clone(),
    }).collect();
    for s in &cf.signature_impacts {
        for c in s.call_sites.iter().filter(|c| !c.updated) {
            annotations.push(Annotation {
                path: c.file.clone(),
                line: c.line.max(1),
                title: format!("`{}` changed and this call wasn't updated", perspica_core::cross_file::call_name(&s.name)),
                message: s.description.clone(),
            });
        }
    }
    annotations.dedup();
    Stamp {
        v: 1,
        sha: sha.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        files: multi.files.len(),
        changed_lines: multi.results.iter().map(|r| r.review.changed_lines).sum(),
        mechanical_lines: multi.results.iter().map(|r| r.review.mechanical_lines).sum(),
        test_lines: multi.results.iter().map(|r| r.review.test_lines).sum(),
        skim: multi.files.iter().zip(&multi.results)
            .filter(|(_, r)| r.review.changed_lines > 0 && (r.review.generated || r.review.mechanical_lines >= r.review.changed_lines))
            .map(|(f, _)| f.new_path.clone()).collect(),
        summary: multi.summary.clone(),
        groups,
        concerns: if settings.concerns { multi.concerns.clone() } else { vec![] },
        lessons: lessons.iter().zip(titles).map(|(l, title)| StampLesson { title, tldr: l.tldr.clone(), in_this_change: l.in_this_change.clone() }).collect(),
        ask_quiz: settings.ask_quiz && !quiz.is_empty(),
        author_score: (settings.quiz_score && score.complete() && !quiz.is_empty()).then_some((score.right, score.total)),
        quiz,
        annotations,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

impl Stamp {
    pub fn headline(&self) -> String {
        let careful = self.groups.iter().filter(|g| matches!(g.risk.as_deref(), Some("high" | "medium"))).count();
        let mut parts = vec![if self.groups.is_empty() {
            format!("{} changed", plural(self.files, "file", "files"))
        } else {
            format!("{} to review, {careful} worth a careful look", plural(self.groups.len(), "change", "changes"))
        }];
        if self.mechanical_lines > 0 {
            parts.push(format!("{} of {} changed lines are mechanical", group_digits(self.mechanical_lines), group_digits(self.changed_lines)));
        }
        if !self.annotations.is_empty() {
            parts.push(format!("{} may need updating", plural(self.annotations.len(), "place", "places")));
        }
        parts.join(" · ")
    }

    /// Identifies the quiz, so answers to an older one don't count for a new one.
    pub fn quiz_id(&self) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in serde_json::to_string(&self.quiz).unwrap_or_default().bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{h:016x}")
    }
}

fn group_digits(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

const LETTERS: &[u8] = b"abcde";

/// The comment, ready to post. `graded`: the repo has the Action, so reviewers reply with their answers.
/// Without it, each answer is folded under its question.
pub fn comment(s: &Stamp, graded: bool) -> String {
    let mut s = s.clone();
    // Too long for GitHub: drop the least useful parts first.
    loop {
        let text = render(&s, graded);
        if text.len() <= MAX_COMMENT { return text; }
        if s.annotations.len() > 10 { s.annotations.truncate(10); }
        else if s.skim.len() > 20 { s.skim.truncate(20); }
        else if s.concerns.len() > 5 { s.concerns.truncate(5); }
        else if s.quiz.iter().any(|q| !q.explanation.is_empty()) { s.quiz.iter_mut().for_each(|q| q.explanation.clear()); }
        else if !s.lessons.is_empty() { s.lessons.clear(); }
        else if s.summary.is_some() { s.summary = None; }
        else { s.groups.truncate(s.groups.len() / 2); s.concerns.clear(); s.annotations.clear(); if s.groups.is_empty() { return render(&s, graded); } }
    }
}

/// The comment without its data, as the author previews it.
pub fn preview(s: &Stamp, graded: bool) -> String {
    let c = comment(s, graded);
    c.lines().filter(|l| !l.starts_with(DATA) && *l != MARK).collect::<Vec<_>>().join("\n").trim().to_string()
}

fn render(s: &Stamp, graded: bool) -> String {
    let mut out = vec![MARK.to_string(), format!("**perspica** · {}", s.headline())];
    let mut lines = vec![];
    if s.ask_quiz {
        lines.push(format!("**Quiz requested.** The author asks reviewers to take the quiz below before approving ({}).", plural(s.quiz.len(), "question", "questions")));
    }
    if let Some((right, total)) = s.author_score {
        lines.push(format!("The author took the quiz: {right} of {total} right first try."));
    }
    if !lines.is_empty() {
        out.push(lines.join("\n"));
    }
    let fold = |title: String, body: String| format!("<details><summary>{title}</summary>\n\n{}\n\n</details>", body.trim());
    if !s.groups.is_empty() {
        let body = s.groups.iter().enumerate().map(|(i, g)| {
            let risk = g.risk.as_deref().map(|r| format!(", {r} risk")).unwrap_or_default();
            let note = g.note.as_deref().map(|n| format!(". {}", n.trim_end_matches('.'))).unwrap_or_default();
            format!("{}. **{}**{risk}{note}.", i + 1, g.label.trim_end_matches('.'))
        }).collect::<Vec<_>>().join("\n");
        out.push(fold(format!("Reading order ({})", s.groups.len()), body));
    }
    if !s.concerns.is_empty() {
        out.push(fold(format!("Worth checking ({})", s.concerns.len()), s.concerns.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n")));
    }
    if !s.annotations.is_empty() {
        let body = s.annotations.iter().map(|a| format!("- `{}:{}` {}. {}", a.path, a.line, a.title, a.message)).collect::<Vec<_>>().join("\n");
        out.push(fold(format!("May need updating ({})", s.annotations.len()), body));
    }
    if !s.lessons.is_empty() {
        let body = s.lessons.iter().map(|l| format!("- **{}.** {}", l.title.trim_end_matches('.'), l.tldr)).collect::<Vec<_>>().join("\n");
        out.push(fold(format!("Lessons ({})", s.lessons.len()), body));
    }
    if !s.quiz.is_empty() {
        let mut body = vec![];
        for (i, q) in s.quiz.iter().enumerate() {
            let choices = q.choices.iter().enumerate().map(|(k, c)| format!("- **{}.** {c}", LETTERS[k] as char)).collect::<Vec<_>>().join("\n");
            let mut item = format!("**{}.** {}\n\n{choices}", i + 1, q.question);
            if !graded {
                let why = if q.explanation.is_empty() { String::new() } else { format!(" {}", q.explanation) };
                item.push_str(&format!("\n\n<details><summary>Answer</summary>\n\n**{}.**{why}\n\n</details>", LETTERS[q.answer] as char));
            }
            body.push(item);
        }
        let example = (1..=s.quiz.len().min(3)).map(|n| format!("{n}{}", LETTERS[(n - 1) % 3] as char)).collect::<Vec<_>>().join(" ");
        if graded {
            body.push(format!("Reply with `/quiz` and your answers, like `/quiz {example}`. The perspica check grades them."));
        }
        let title = if s.ask_quiz { "Quiz" } else { "Quiz, optional" };
        out.push(fold(format!("{title} ({})", plural(s.quiz.len(), "question", "questions")), body.join("\n\n")));
    }
    out.push(format!("<sub>Posted with [perspica]({HOME}) from the author's machine, for {}.</sub>", short(&s.sha)));
    out.push(format!("{DATA}{} -->", encode(s)));
    out.join("\n\n")
}

/// JSON that can sit inside an HTML comment: without a single `-`, nothing in it can end the comment.
/// Every `-` is in a string (no number is negative), where `-` means the same.
fn encode<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_default().replace('-', "\\u002d")
}

fn decode<T: serde::de::DeserializeOwned>(body: &str, tag: &str) -> Option<T> {
    let start = body.find(tag)? + tag.len();
    let end = body[start..].find(" -->")? + start;
    serde_json::from_str(body[start..end].trim()).ok()
}

// --- Talking to GitHub from the author's machine, with the `gh` CLI ---

fn gh(args: &[&str], input: Option<&str>) -> Result<String, String> {
    use std::io::Write;
    let mut cmd = Command::new("gh");
    cmd.args(args).env_remove("CLICOLOR_FORCE").env_remove("GH_FORCE_TTY").env("NO_COLOR", "1")
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
        .stdin(if input.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() });
    let mut child = cmd.spawn().map_err(|e| format!("couldn't run gh (install the GitHub CLI from https://cli.github.com): {e}"))?;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.contains("gh auth login") { "gh isn't logged in. Run `gh auth login` first.".into() } else { format!("gh {}: {err}", args.first().unwrap_or(&"")) });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The pull request a review can be posted to.
#[derive(Debug, Clone)]
pub struct Share {
    /// The PR number, when the review is of `--pr N`. Otherwise the current branch's PR.
    pub pr: Option<u64>,
    /// The commit the review describes, or why it isn't one.
    pub head: Result<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrState {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// `owner/name` of the repo the PR is on (the upstream one, for a fork).
    pub repo: String,
    pub base: String,
    pub author: String,
    pub me: String,
    pub own: bool,
    pub head: String,
    /// Why this review can't be posted to it.
    pub problem: Option<String>,
    /// The repo has the perspica Action, which grades the quiz.
    pub action: bool,
    /// perspica's comment there, if there is one.
    pub comment: Option<u64>,
}

pub fn pr_state(share: &Share) -> Result<PrState, String> {
    let n = share.pr.map(|n| n.to_string());
    let mut args = vec!["pr", "view"];
    if let Some(n) = &n { args.push(n); }
    args.extend(["--json", "number,title,url,author,headRefOid,baseRefName"]);
    let v: serde_json::Value = match gh(&args, None) {
        Ok(out) => serde_json::from_str(&out).map_err(|e| format!("gh output: {e}"))?,
        Err(e) if e.contains("no pull requests found") => return Err("This branch has no pull request on GitHub yet. Open one, then post.".into()),
        Err(e) => return Err(e),
    };
    let url = v["url"].as_str().unwrap_or_default().to_string();
    let repo = url.split("/pull/").next().and_then(|u| u.strip_prefix("https://github.com/")).unwrap_or_default().to_string();
    let me = gh(&["api", "user", "--jq", ".login"], None)?.trim().to_string();
    let author = v["author"]["login"].as_str().unwrap_or_default().to_string();
    let head = v["headRefOid"].as_str().unwrap_or_default().to_string();
    let number = v["number"].as_u64().unwrap_or_default();
    let base = v["baseRefName"].as_str().unwrap_or_default().to_string();
    let own = !me.is_empty() && me.eq_ignore_ascii_case(&author);
    let problem = if !own {
        Some(format!("This is {author}'s pull request. Only its author posts perspica's comment there."))
    } else {
        match &share.head {
            Err(e) => Some(e.clone()),
            Ok(h) if *h != head => Some(format!("PR #{number} is at {}, but this review is of {}. Push your branch, or run perspica --pr {number}.", short(&head), short(h))),
            Ok(_) => None,
        }
    };
    let action = !repo.is_empty() && gh(&["api", &format!("repos/{repo}/contents/.github/workflows/perspica.yml?ref={base}"), "--silent"], None).is_ok();
    let comment = if repo.is_empty() { None } else {
        gh(&["api", "--paginate", &format!("repos/{repo}/issues/{number}/comments?per_page=100"),
            "--jq", &format!(".[] | select(.user.login == \"{me}\" and (.body | contains(\"{MARK}\"))) | .id")], None)
            .ok().and_then(|out| out.lines().filter_map(|l| l.trim().parse().ok()).next_back())
    };
    Ok(PrState { number, title: v["title"].as_str().unwrap_or_default().to_string(), url, repo, base, author, me, own, head, problem, action, comment })
}

/// Posts the comment, or updates perspica's comment if there is one. Returns its URL.
pub fn post(pr: &PrState, body: &str) -> Result<String, String> {
    let payload = serde_json::json!({ "body": body }).to_string();
    let out = match pr.comment {
        Some(id) => gh(&["api", "-X", "PATCH", &format!("repos/{}/issues/comments/{id}", pr.repo), "--input", "-"], Some(&payload))?,
        None => gh(&["api", "-X", "POST", &format!("repos/{}/issues/{}/comments", pr.repo, pr.number), "--input", "-"], Some(&payload))?,
    };
    let v: serde_json::Value = serde_json::from_str(&out).map_err(|e| format!("gh output: {e}"))?;
    Ok(v["html_url"].as_str().unwrap_or(&pr.url).to_string())
}

/// The comment as GitHub would show it.
pub fn render_html(repo: &str, markdown: &str) -> Option<String> {
    let payload = serde_json::json!({ "text": markdown, "mode": "gfm", "context": repo }).to_string();
    gh(&["api", "markdown", "--input", "-"], Some(&payload)).ok()
}

/// What's worth saying before the author posts.
pub fn notes(pr: &PrState, settings: &PostSettings, multi: &MultiFileResult, score: Score) -> Vec<String> {
    let mut notes = vec![];
    if multi.intent_groups.as_ref().is_none_or(|g| g.iter().all(|g| g.mechanical)) {
        notes.push("There's no Analyze result for this diff, so the comment has only the basic summary. Analyze first for a reading order and concerns.".into());
    }
    let lessons = multi.learn.iter().flat_map(|l| &l.concepts).filter(|c| c.lesson.is_some()).count();
    if settings.lessons && lessons == 0 {
        notes.push("No lessons are written yet. Open Learn first to add lessons and a quiz.".into());
    }
    if settings.lessons && settings.quiz_score && score.total > 0 && !score.complete() {
        notes.push(if score.answered == 0 {
            "Take the quiz in Learn to add your score.".to_string()
        } else {
            format!("You've answered {} of {} quiz questions. Your score is added once you've answered them all.", score.answered, score.total)
        });
    }
    if settings.ask_quiz && !pr.action {
        notes.push(format!("Asking reviewers for the quiz needs the perspica Action in {}, which grades it. Add it with `perspica github init`.", pr.repo));
    }
    notes
}

/// What's in the comment, in a line.
pub fn describe(s: &Stamp) -> String {
    let mut parts = vec!["the counts".to_string()];
    if !s.groups.is_empty() { parts.push("a reading order".into()); }
    if !s.concerns.is_empty() { parts.push(plural(s.concerns.len(), "concern", "concerns")); }
    if !s.annotations.is_empty() { parts.push(plural(s.annotations.len(), "place that may need updating", "places that may need updating")); }
    if !s.lessons.is_empty() { parts.push(format!("{} with a {}-question quiz", plural(s.lessons.len(), "lesson", "lessons"), s.quiz.len())); }
    if s.author_score.is_some() { parts.push("your quiz score".into()); }
    let last = parts.pop().unwrap_or_default();
    let list = if parts.is_empty() { last } else { format!("{} and {last}", parts.join(", ")) };
    let ask = if s.ask_quiz { " It asks reviewers to take the quiz before approving." } else { "" };
    format!("The comment has {list}.{ask}")
}

/// `perspica --post`: preview, ask, post.
pub fn post_cli(multi: &MultiFileResult, share: &Share, settings: PostSettings, key: &str, yes: bool) -> Result<(), String> {
    use std::io::{BufRead, IsTerminal};
    eprintln!("Looking up the pull request…");
    let pr = pr_state(share)?;
    if let Some(p) = &pr.problem {
        return Err(p.clone());
    }
    let score = author_score(multi.learn.as_ref(), &crate::saved::load_quiz(key));
    let mut used = settings.clone();
    used.ask_quiz &= pr.action;
    let s = stamp(multi, &used, score, &pr.head);
    println!("{}\n", preview(&s, pr.action));
    for n in notes(&pr, &settings, multi, score) {
        eprintln!("Note: {n}");
    }
    eprintln!("{} Change what's in it with --concerns, --lessons, --quiz-score or --ask-quiz (=false to leave one out). perspica remembers your choice.", describe(&s));
    let verb = if pr.comment.is_some() { "Update" } else { "Post" };
    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err("Nothing posted. Add --yes to post without being asked.".into());
        }
        eprint!("{verb} perspica's comment on {}#{}? [y/N] ", pr.repo, pr.number);
        let mut answer = String::new();
        let _ = std::io::stdin().lock().read_line(&mut answer);
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            eprintln!("Nothing posted.");
            return Ok(());
        }
    }
    let url = post(&pr, &comment(&s, pr.action))?;
    crate::saved::save_post_settings(&settings);
    eprintln!("{}: {url}", if pr.comment.is_some() { "Updated" } else { "Posted" });
    if !pr.action {
        eprintln!("Tip: `perspica github init` adds the perspica check to {}. It grades the reviewer quiz and gives every pull request a summary.", pr.repo);
    }
    Ok(())
}

// --- The Action ---

/// A reviewer's answers to one quiz, across their replies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Graded {
    pub quiz: String,
    pub user: String,
    /// Their first answer to each question.
    pub first: Vec<Option<usize>>,
    /// Whether they've answered it right yet.
    pub right: Vec<bool>,
}

impl Graded {
    pub fn passed(&self) -> bool {
        !self.right.is_empty() && self.right.iter().all(|r| *r)
    }

    fn first_right(&self, quiz: &[learn::Question]) -> usize {
        quiz.iter().zip(&self.first).filter(|(q, f)| **f == Some(q.answer)).count()
    }
}

/// `/quiz 1a 2c 3b`: question numbers from 1, letters from a. Words that aren't answers are skipped.
pub fn parse_answers(body: &str) -> Option<Vec<(usize, usize)>> {
    let rest = body.trim_start().strip_prefix("/quiz")?;
    if rest.starts_with(|c: char| c.is_alphanumeric()) { return None; }
    let words: Vec<String> = rest.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()).map(str::to_ascii_lowercase).collect();
    let letter = |l: &str| (l.len() == 1).then(|| LETTERS.iter().position(|c| l.starts_with(*c as char))).flatten();
    let mut answers = vec![];
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        i += 1;
        let digits = w.find(|c: char| !c.is_ascii_digit()).unwrap_or(w.len());
        let Some(n) = w[..digits].parse::<usize>().ok().filter(|n| *n > 0) else { continue };
        let rest = w[digits..].trim_start_matches(['.', ':', ')']);
        // "2b", or "2" then "b".
        let k = if rest.is_empty() { words.get(i).and_then(|l| letter(l)).inspect(|_| i += 1) } else { letter(rest) };
        if let Some(k) = k { answers.push((n - 1, k)); }
    }
    (!answers.is_empty()).then_some(answers)
}

/// Adds a reply's answers to what the reviewer answered before.
pub fn grade(prev: Option<Graded>, quiz: &[learn::Question], quiz_id: &str, user: &str, answers: &[(usize, usize)]) -> Graded {
    let mut g = prev.filter(|g| g.quiz == quiz_id && g.first.len() == quiz.len())
        .unwrap_or_else(|| Graded { quiz: quiz_id.into(), user: user.into(), first: vec![None; quiz.len()], right: vec![false; quiz.len()] });
    for &(i, k) in answers {
        let Some(q) = quiz.get(i) else { continue };
        if k >= q.choices.len() { continue; }
        g.first[i].get_or_insert(k);
        g.right[i] |= k == q.answer;
    }
    g
}

/// The Action's reply to a reviewer's answers.
pub fn reply(g: &Graded, quiz: &[learn::Question], answers: &[(usize, usize)]) -> String {
    let total = quiz.len();
    let mut out = vec![];
    let right = g.right.iter().filter(|r| **r).count();
    out.push(if g.passed() {
        format!("@{} passed the quiz: {} of {total} right first try.", g.user, g.first_right(quiz))
    } else {
        format!("@{} {right} of {total} right so far.", g.user)
    });
    for &(i, k) in answers {
        let Some(q) = quiz.get(i) else {
            out.push(format!("There's no question {}.", i + 1));
            continue;
        };
        if k >= q.choices.len() {
            out.push(format!("**{}.** has no choice {}.", i + 1, LETTERS[k] as char));
        } else if k != q.answer && !g.right[i] {
            out.push(format!("**{}.** Not quite. {}", i + 1, q.explanation).trim_end().to_string());
        }
    }
    let left: Vec<String> = g.right.iter().enumerate().filter(|(_, r)| !**r).map(|(i, _)| (i + 1).to_string()).collect();
    if !left.is_empty() {
        out.push(format!("Still to answer right: {}. Reply with `/quiz` again, the question number and your letter.", left.join(", ")));
    }
    out.push(format!("{GRADED}{} -->", encode(g)));
    out.join("\n\n")
}

#[derive(Debug, Clone)]
pub struct IssueComment {
    pub user: String,
    pub body: String,
}

/// The newest comment perspica posted for the PR's author.
pub fn latest_stamp(comments: &[IssueComment], author: &str) -> Option<Stamp> {
    comments.iter().rev().filter(|c| c.user.eq_ignore_ascii_case(author) && c.body.contains(MARK)).find_map(|c| decode(&c.body, DATA))
}

/// Each reviewer's latest grading for this quiz, from the Action's own replies.
pub fn gradings(comments: &[IssueComment], quiz_id: &str) -> Vec<Graded> {
    let mut out: Vec<Graded> = vec![];
    for g in comments.iter().filter(|c| c.user == BOT).filter_map(|c| decode::<Graded>(&c.body, GRADED)).filter(|g| g.quiz == quiz_id) {
        out.retain(|o| o.user != g.user);
        out.push(g);
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckRun {
    /// "completed", or "in_progress" while it waits on a reviewer's quiz.
    pub status: &'static str,
    pub conclusion: Option<&'static str>,
    pub title: String,
    pub summary: String,
    pub annotations: Vec<Annotation>,
}

/// The check for the PR's latest commit. `own`: perspica's structural review of that commit, made by the
/// Action, for when the author's comment is missing or describes an older commit.
pub fn check_run(stamp: Option<&Stamp>, head: &str, graded: &[Graded], own: Option<&Stamp>, pr: u64) -> Option<CheckRun> {
    let fresh = stamp.filter(|s| s.sha == head);
    let Some(s) = stamp else {
        let own = own?;
        let mut summary = vec!["perspica's structural review of this pull request. It uses no model and sends nothing anywhere.\n".to_string()];
        summary.extend(glance(own));
        if !own.annotations.is_empty() {
            summary.push(String::new());
            summary.push(annotation_list(&own.annotations));
        }
        summary.push(String::new());
        summary.push(format!("The author can add a reading order, risks, lessons and a quiz by running `perspica --pr {pr} --post` on their machine. [About perspica]({HOME})"));
        return Some(CheckRun { status: "completed", conclusion: Some("neutral"), title: own.headline(), summary: summary.join("\n"), annotations: cap(&own.annotations) });
    };
    let passed: Vec<&Graded> = graded.iter().filter(|g| g.passed()).collect();
    let (status, conclusion, lead) = match (s.ask_quiz, passed.is_empty()) {
        (true, true) => ("in_progress", None, "Waiting on a reviewer's quiz".to_string()),
        (true, false) => ("completed", Some("success"), format!("Quiz passed by {}", passed.iter().map(|g| format!("@{}", g.user)).collect::<Vec<_>>().join(", "))),
        (false, _) => ("completed", Some("neutral"), String::new()),
    };
    // Counts describe the latest commit when the comment is older.
    let mut shown = s.clone();
    if fresh.is_none() {
        if let Some(own) = own {
            shown.files = own.files;
            shown.changed_lines = own.changed_lines;
            shown.mechanical_lines = own.mechanical_lines;
            shown.test_lines = own.test_lines;
            shown.skim = own.skim.clone();
            shown.annotations = own.annotations.clone();
        } else {
            shown.annotations.clear();
        }
    }
    let title = [lead, shown.headline()].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" · ");
    let mut out = vec![];
    if fresh.is_none() {
        out.push(format!("> The author posted this for {}. The pull request has newer commits, so parts of it may be out of date.\n", short(&s.sha)));
    }
    if let Some(sum) = &s.summary {
        out.push(format!("{sum}\n"));
    }
    out.extend(glance(&shown));
    out.push(String::new());
    if !s.groups.is_empty() {
        out.push("### Reading order".into());
        for (i, g) in s.groups.iter().enumerate() {
            let risk = g.risk.as_deref().map(|r| format!(", {r} risk")).unwrap_or_default();
            out.push(format!("{}. **{}**{risk}{}", i + 1, g.label, g.note.as_deref().map(|n| format!("<br>{n}")).unwrap_or_default()));
        }
        out.push(String::new());
    }
    if !s.concerns.is_empty() {
        out.push("### Worth checking".into());
        out.extend(s.concerns.iter().map(|c| format!("- {c}")));
        out.push(String::new());
    }
    if !shown.annotations.is_empty() {
        out.push("### May need updating".into());
        out.push(annotation_list(&shown.annotations));
        out.push(String::new());
    }
    if !s.lessons.is_empty() {
        out.push("### Lessons".into());
        for l in &s.lessons {
            out.push(format!("**{}.** {}", l.title.trim_end_matches('.'), l.tldr));
            if !l.in_this_change.is_empty() { out.push(format!("\n{}", l.in_this_change)); }
            out.push(String::new());
        }
    }
    if !s.quiz.is_empty() {
        out.push("### Quiz".into());
        out.push(format!("{} in the perspica comment{}. Reviewers answer by replying with `/quiz` and their answers.",
            plural(s.quiz.len(), "question", "questions"), if s.ask_quiz { ", which the author asks reviewers to take before approving" } else { ", optional" }));
        if let Some((r, t)) = s.author_score { out.push(format!("- The author: {r} of {t} right first try")); }
        for g in graded {
            let first = g.first_right(&s.quiz);
            out.push(format!("- @{}: {}, {first} of {} right first try", g.user, if g.passed() { "passed" } else { "not finished" }, s.quiz.len()));
        }
        out.push(String::new());
    }
    out.push(format!("<sub>perspica {}, posted from the author's machine for {}.</sub>", s.version, short(&s.sha)));
    let mut summary = out.join("\n");
    if summary.len() > 65_000 {
        let mut end = 64_000;
        while !summary.is_char_boundary(end) { end -= 1; }
        summary.truncate(end);
        summary.push_str("\n\n…");
    }
    Some(CheckRun { status, conclusion, title, summary, annotations: cap(&shown.annotations) })
}

/// Counts a reviewer can plan by, for the check's page.
fn glance(s: &Stamp) -> Vec<String> {
    let mut out = vec![format!("- {}", plural(s.files, "file changed", "files changed"))];
    if s.changed_lines > 0 {
        out.push(format!("- {} of {} changed lines are mechanical (formatting, renames, moved code), and {} are tests",
            group_digits(s.mechanical_lines), group_digits(s.changed_lines), group_digits(s.test_lines)));
    }
    if !s.skim.is_empty() {
        let shown: Vec<String> = s.skim.iter().take(15).map(|p| format!("`{p}`")).collect();
        let more = if s.skim.len() > 15 { format!(" and {} more", s.skim.len() - 15) } else { String::new() };
        out.push(format!("- Only mechanical or generated, safe to skim: {}{more}", shown.join(", ")));
    }
    out
}

fn annotation_list(a: &[Annotation]) -> String {
    a.iter().map(|a| format!("- `{}:{}` {}. {}", a.path, a.line, a.title, a.message)).collect::<Vec<_>>().join("\n")
}

fn cap(a: &[Annotation]) -> Vec<Annotation> {
    a.iter().take(MAX_ANNOTATIONS).cloned().collect()
}

struct Api {
    client: reqwest::Client,
    base: String,
    token: String,
    repo: String,
}

impl Api {
    async fn send(&self, method: reqwest::Method, path: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
        let url = if path.starts_with("http") { path.to_string() } else { format!("{}/repos/{}/{path}", self.base, self.repo) };
        let mut req = self.client.request(method, &url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "perspica");
        if let Some(b) = body { req = req.json(&b); }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("{status} from {url}: {}", text.chars().take(300).collect::<String>()));
        }
        Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    async fn comments(&self, pr: u64) -> Result<Vec<IssueComment>, String> {
        let mut out = vec![];
        for page in 1..=30 {
            let v = self.send(reqwest::Method::GET, &format!("issues/{pr}/comments?per_page=100&page={page}"), None).await?;
            let items = v.as_array().cloned().unwrap_or_default();
            out.extend(items.iter().map(|c| IssueComment {
                user: c["user"]["login"].as_str().unwrap_or_default().to_string(),
                body: c["body"].as_str().unwrap_or_default().to_string(),
            }));
            if items.len() < 100 { break; }
        }
        Ok(out)
    }
}

/// `perspica github action`: what the workflow runs on each pull request event.
pub fn action() -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(run_action())
}

async fn run_action() -> Result<(), String> {
    let env = |k: &str| std::env::var(k).map_err(|_| format!("{k} isn't set. This runs in the perspica GitHub workflow (see `perspica github init`)."));
    let api = Api {
        client: reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().map_err(|e| e.to_string())?,
        base: std::env::var("GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".into()),
        token: env("GITHUB_TOKEN")?,
        repo: env("GITHUB_REPOSITORY")?,
    };
    let event_name = env("GITHUB_EVENT_NAME")?;
    let event: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(env("GITHUB_EVENT_PATH")?).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let number = match event_name.as_str() {
        "pull_request" | "pull_request_target" => event["pull_request"]["number"].as_u64(),
        "issue_comment" if !event["issue"]["pull_request"].is_null() => event["issue"]["number"].as_u64(),
        _ => None,
    };
    let Some(number) = number else {
        println!("perspica: nothing to do for {event_name} events outside pull requests.");
        return Ok(());
    };
    let pr = api.send(reqwest::Method::GET, &format!("pulls/{number}"), None).await?;
    let head = pr["head"]["sha"].as_str().unwrap_or_default().to_string();
    let author = pr["user"]["login"].as_str().unwrap_or_default().to_string();
    let mut comments = api.comments(number).await?;
    let stamp = latest_stamp(&comments, &author);

    // A reviewer's quiz answers.
    if event_name == "issue_comment" && event["action"] == "created" {
        let c = &event["comment"];
        let user = c["user"]["login"].as_str().unwrap_or_default();
        if let Some(answers) = parse_answers(c["body"].as_str().unwrap_or_default()).filter(|_| user != BOT) {
            let text = match &stamp {
                None => "There's no perspica quiz on this pull request yet.".to_string(),
                Some(s) if s.quiz.is_empty() => "The perspica comment on this pull request has no quiz.".to_string(),
                Some(_) if user.eq_ignore_ascii_case(&author) => "The quiz here is for reviewers. Your own score comes from perspica on your machine when you post.".to_string(),
                Some(s) => {
                    let id = s.quiz_id();
                    let prev = gradings(&comments, &id).into_iter().find(|g| g.user == user);
                    reply(&grade(prev, &s.quiz, &id, user, &answers), &s.quiz, &answers)
                }
            };
            api.send(reqwest::Method::POST, &format!("issues/{number}/comments"), Some(serde_json::json!({ "body": text }))).await?;
            comments.push(IssueComment { user: BOT.into(), body: text });
        }
    }

    let graded = stamp.as_ref().map(|s| gradings(&comments, &s.quiz_id())).unwrap_or_default();
    // Our own look at the latest commit, when the author's comment doesn't describe it. Only when the
    // workflow checked out the repository (pull request events).
    let own = if stamp.as_ref().is_none_or(|s| s.sha != head) && std::path::Path::new(".git").exists() {
        match review_pr(number, &head) {
            Ok(s) => Some(s),
            Err(e) => { eprintln!("perspica: couldn't review the pull request itself: {e}"); None }
        }
    } else {
        None
    };
    let Some(check) = check_run(stamp.as_ref(), &head, &graded, own.as_ref(), number) else {
        println!("perspica: no comment and no review of our own, so no check to update.");
        return Ok(());
    };
    update_check(&api, &head, &check).await?;
    println!("perspica: {} ({})", check.title, check.conclusion.unwrap_or(check.status));
    Ok(())
}

/// perspica's structural review of the PR, run by this same binary. It reads the code and runs none of it.
fn review_pr(number: u64, head: &str) -> Result<Stamp, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = Command::new(exe).args(["--pr", &number.to_string(), "--json", "--no-sessions"]).env("NO_COLOR", "1").output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let multi: MultiFileResult = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    Ok(stamp(&multi, &PostSettings { concerns: false, lessons: false, quiz_score: false, ask_quiz: false }, Score::default(), head))
}

async fn update_check(api: &Api, head: &str, check: &CheckRun) -> Result<(), String> {
    let existing = api.send(reqwest::Method::GET, &format!("commits/{head}/check-runs?check_name={CHECK}&filter=latest"), None).await?;
    let current = existing["check_runs"].as_array().and_then(|a| a.first()).cloned();
    let annotations: Vec<serde_json::Value> = check.annotations.iter().map(|a| serde_json::json!({
        "path": a.path, "start_line": a.line, "end_line": a.line, "annotation_level": "warning", "title": a.title, "message": a.message,
    })).collect();
    let mut body = serde_json::json!({ "name": CHECK, "status": check.status, "output": { "title": check.title, "summary": check.summary } });
    if let Some(c) = check.conclusion { body["conclusion"] = c.into(); }
    match current {
        // A completed check can't go back to waiting, so that takes a new one.
        Some(run) if !(run["status"] == "completed" && check.status != "completed") => {
            // Annotations add up on each update, so they're only sent while there are none.
            if run["output"]["annotations_count"].as_u64().unwrap_or(0) == 0 && !annotations.is_empty() {
                body["output"]["annotations"] = annotations.into();
            }
            api.send(reqwest::Method::PATCH, &format!("check-runs/{}", run["id"]), Some(body)).await?;
        }
        _ => {
            body["head_sha"] = head.into();
            if !annotations.is_empty() { body["output"]["annotations"] = annotations.into(); }
            api.send(reqwest::Method::POST, "check-runs", Some(body)).await?;
        }
    }
    Ok(())
}

// --- Setting up the Action ---

pub fn workflow() -> String {
    let v = env!("CARGO_PKG_VERSION");
    format!(r#"# perspica: a summary check on every pull request, and the reviewer quiz when an author asks for one.
# Written by `perspica github init`. It reads the pull request's code but never runs it.
name: perspica
on:
  pull_request_target:
    types: [opened, synchronize, reopened]
  issue_comment:
    types: [created, edited]
permissions:
  contents: read
  # Replies to quiz answers. Commenting on a pull request needs this, not just issues: write.
  pull-requests: write
  issues: write
  checks: write
concurrency:
  group: perspica-${{{{ github.event.pull_request.number || github.event.issue.number }}}}
jobs:
  update:
    # Comments only matter on pull requests: perspica's own, and quiz answers.
    if: >-
      github.event_name == 'pull_request_target' ||
      (github.event.issue.pull_request && (contains(github.event.comment.body, 'perspica:data') || startsWith(github.event.comment.body, '/quiz')))
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@v4
        if: github.event_name == 'pull_request_target'
        with:
          fetch-depth: 0
      - name: Install perspica
        run: curl -fsSL https://raw.githubusercontent.com/sshah03/perspica/v{v}/install.sh | PERSPICA_VERSION={v} sh
      - name: Update the perspica check
        run: ~/.local/bin/perspica github action
        env:
          GITHUB_TOKEN: ${{{{ github.token }}}}
          GH_TOKEN: ${{{{ github.token }}}}
"#)
}

/// `perspica github <command>`.
pub fn cli(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("action") => {
            if let Err(e) = action() {
                eprintln!("perspica: {e}");
                std::process::exit(1);
            }
        }
        Some("init") => init(),
        _ => {
            eprintln!("Usage: perspica github init     add the perspica check to this repository");
            eprintln!("       perspica github action   what that check runs (in GitHub Actions only)");
            std::process::exit(if args.is_empty() { 0 } else { 1 });
        }
    }
}

fn init() {
    let Some(root) = crate::git::repo_root() else {
        eprintln!("perspica: run this inside the repository");
        std::process::exit(1);
    };
    let path = root.join(".github/workflows/perspica.yml");
    let text = workflow();
    let before = std::fs::read_to_string(&path).ok();
    if before.as_deref() == Some(text.as_str()) {
        println!(".github/workflows/perspica.yml is already up to date.");
    } else {
        if let Err(e) = std::fs::create_dir_all(path.parent().expect("has a parent")).and_then(|_| std::fs::write(&path, &text)) {
            eprintln!("perspica: couldn't write {}: {e}", path.display());
            std::process::exit(1);
        }
        println!("{} .github/workflows/perspica.yml.", if before.is_some() { "Updated" } else { "Wrote" });
    }
    let settings = crate::git::repo_identity().and_then(|(_, url)| url).map(|u| format!("{u}/settings/branches")).unwrap_or_else(|| "the repository's Settings, under Branches".into());
    println!("
Next:
  1. Commit it and merge it into your default branch. Pull requests get the check from there.
  2. Post from your machine with `perspica --pr <number> --post`, or Post in the viewer.

Every pull request then gets a perspica check with a summary. When an author asks reviewers to
take the quiz, the check waits until one passes. To keep a pull request from merging until then,
make `{CHECK}` a required check in {settings}.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(text: &str, answer: usize) -> learn::Question {
        learn::Question { question: text.into(), choices: vec!["one".into(), "two".into(), "three".into()], answer, explanation: format!("Because {answer}.") }
    }

    fn sample() -> Stamp {
        Stamp {
            v: 1, sha: "abcdef1234567890".into(), version: "0.5.0".into(), files: 4, changed_lines: 1326, mechanical_lines: 218, test_lines: 300, skim: vec!["Cargo.lock".into()],
            summary: Some("Adds a Decisions tab.".into()),
            groups: vec![
                StampGroup { label: "Session detection".into(), risk: Some("medium".into()), note: Some("Check read-only sessions".into()) },
                StampGroup { label: "Viewer tab".into(), risk: Some("low".into()), note: None },
            ],
            concerns: vec!["defines() matches substrings.".into()],
            lessons: vec![StampLesson { title: "Trace budget".into(), tldr: "Trims oldest first.".into(), in_this_change: "`fit` drops events.".into() }],
            quiz: vec![question("What does `fit` drop -- first?", 1), question("Why?", 0)],
            ask_quiz: true,
            author_score: Some((2, 2)),
            annotations: vec![Annotation { path: "src/a.rs".into(), line: 3, title: "`foo` was removed".into(), message: "still called here".into() }],
        }
    }

    #[test]
    fn the_comment_carries_its_data_safely() {
        let s = sample();
        let c = comment(&s, true);
        assert!(c.starts_with(MARK));
        let data_line = c.lines().find(|l| l.starts_with(DATA)).unwrap();
        // Nothing in the data can end the HTML comment early, even text with `--` or `-->` in it.
        assert_eq!(data_line.matches("-->").count(), 1);
        assert!(!data_line[DATA.len()..data_line.len() - 4].contains('-'));
        let back: Stamp = decode(&c, DATA).unwrap();
        assert_eq!(back.quiz[0].question, "What does `fit` drop -- first?");
        assert_eq!(back.sha, s.sha);
        assert!(c.contains("**perspica** · 2 changes to review, 1 worth a careful look · 218 of 1,326 changed lines are mechanical · 1 place may need updating"));
        assert!(c.contains("**Quiz requested.**") && c.contains("The author took the quiz: 2 of 2 right first try."));
        assert!(c.contains("/quiz 1a 2b"), "graded quizzes ask for a reply");
        assert!(!c.contains("<summary>Answer</summary>"), "and don't show the answers");
        let p = preview(&s, true);
        assert!(!p.contains(DATA) && !p.contains(MARK));
    }

    #[test]
    fn without_the_action_answers_are_folded_under_each_question() {
        let mut s = sample();
        s.ask_quiz = false;
        let c = comment(&s, false);
        assert_eq!(c.matches("<summary>Answer</summary>").count(), 2);
        assert!(c.contains("Quiz, optional (2 questions)") && !c.contains("/quiz 1a"));
    }

    #[test]
    fn long_comments_fit_on_github() {
        let mut s = sample();
        s.concerns = (0..400).map(|i| format!("concern {i} {}", "x".repeat(200))).collect();
        s.lessons = (0..20).map(|i| StampLesson { title: format!("L{i}"), tldr: "t".repeat(300), in_this_change: "y".repeat(3000) }).collect();
        let c = comment(&s, true);
        assert!(c.len() <= MAX_COMMENT, "{}", c.len());
        assert!(decode::<Stamp>(&c, DATA).is_some(), "the data survives trimming");
    }

    #[test]
    fn nothing_from_the_sessions_goes_in() {
        let prompt = "only have the one that's clicked showing";
        let multi: MultiFileResult = serde_json::from_value(serde_json::json!({
            "version": "0", "files": [], "results": [], "cross_file": serde_json::to_value(perspica_core::cross_file::CrossFileManifest::default()).unwrap(),
            "source": { "label": "PR #1", "sessions": { "sessions": [], "requirements": [{ "id": "R1", "session": "s", "timestamp": "t", "text": prompt }] } },
            "intent_groups": [{ "label": "Decisions tab", "entry_ids": [1], "risk": "medium", "origin": "requested", "requirement": { "id": "R1", "quote": prompt } }],
            "summary": "A summary.", "concerns": [],
            "decisions": { "provider": "p", "model": "m", "saved_at": 0, "seconds": 0, "events": 1, "topics": [{ "id": "t1", "title": prompt, "summary": prompt, "decisions": [], "timeline": [], "open": [] }] },
        })).unwrap();
        let c = comment(&stamp(&multi, &PostSettings::default(), Score::default(), "abc"), true);
        assert!(c.contains("Decisions tab"));
        assert!(!c.contains("clicked showing") && !c.contains("requested") && !c.contains("R1"), "{c}");
    }

    #[test]
    fn quiz_replies_parse() {
        assert_eq!(parse_answers("/quiz 1a 2C, 3.b 4)a"), Some(vec![(0, 0), (1, 2), (2, 1), (3, 0)]));
        assert_eq!(parse_answers("/quiz I think 2 a and 1b"), Some(vec![(1, 0), (0, 1)]), "loose words are skipped");
        assert_eq!(parse_answers("/quiz 3 c"), Some(vec![(2, 2)]));
        assert_eq!(parse_answers("/quizzes 1a"), None);
        assert_eq!(parse_answers("thanks /quiz 1a"), None, "only a reply that starts with it");
        assert_eq!(parse_answers("/quiz"), None);
        assert_eq!(parse_answers("/quiz 0a 1z"), None);
    }

    #[test]
    fn grading_keeps_first_tries_and_passes_once_all_are_right() {
        let quiz = vec![question("q1", 0), question("q2", 2)];
        let g = grade(None, &quiz, "id", "rev", &[(0, 0), (1, 1)]);
        assert_eq!(g.first, vec![Some(0), Some(1)]);
        assert!(!g.passed());
        let text = reply(&g, &quiz, &[(0, 0), (1, 1)]);
        assert!(text.contains("@rev 1 of 2 right so far.") && text.contains("**2.** Not quite. Because 2.") && text.contains("Still to answer right: 2. Reply"), "{text}");
        let g = grade(Some(g), &quiz, "id", "rev", &[(1, 2)]);
        assert!(g.passed());
        assert_eq!(g.first, vec![Some(0), Some(1)], "a second try doesn't change the first");
        let text = reply(&g, &quiz, &[(1, 2)]);
        assert!(text.starts_with("@rev passed the quiz: 1 of 2 right first try."), "{text}");
        // Answers to an older quiz don't carry over.
        let fresh = grade(Some(g), &quiz, "other", "rev", &[]);
        assert!(!fresh.passed() && fresh.first == vec![None, None]);
    }

    #[test]
    fn only_the_authors_comment_and_the_actions_gradings_count() {
        let s = sample();
        let id = s.quiz_id();
        let posted = comment(&s, true);
        let g = grade(None, &s.quiz, &id, "rev", &[(0, 1), (1, 0)]);
        let forged = reply(&g, &s.quiz, &[]);
        let comments = vec![
            IssueComment { user: "someone".into(), body: posted.replace("abcdef", "000000") },
            IssueComment { user: "Author".into(), body: posted.clone() },
            IssueComment { user: "rev".into(), body: forged.clone() },
            IssueComment { user: BOT.into(), body: forged },
        ];
        assert_eq!(latest_stamp(&comments, "author").unwrap().sha, s.sha);
        let found = gradings(&comments, &id);
        assert_eq!(found.len(), 1);
        assert!(found[0].passed());
        assert!(gradings(&comments, "older quiz").is_empty());
    }

    #[test]
    fn the_check_waits_on_a_requested_quiz() {
        let s = sample();
        let waiting = check_run(Some(&s), &s.sha, &[], None, 7).unwrap();
        assert_eq!((waiting.status, waiting.conclusion), ("in_progress", None));
        assert!(waiting.title.starts_with("Waiting on a reviewer's quiz · 2 changes"), "{}", waiting.title);
        assert_eq!(waiting.annotations.len(), 1);
        let g = grade(None, &s.quiz, &s.quiz_id(), "rev", &[(0, 1), (1, 0)]);
        let done = check_run(Some(&s), &s.sha, &[g], None, 7).unwrap();
        assert_eq!((done.status, done.conclusion), ("completed", Some("success")));
        assert!(done.title.starts_with("Quiz passed by @rev"));
        assert!(done.summary.contains("@rev: passed, 2 of 2 right first try"));

        let mut optional = s.clone();
        optional.ask_quiz = false;
        let plain = check_run(Some(&optional), &s.sha, &[], None, 7).unwrap();
        assert_eq!(plain.conclusion, Some("neutral"));
    }

    #[test]
    fn an_older_comment_is_marked_and_its_lines_left_out() {
        let s = sample();
        let c = check_run(Some(&s), "fffffff", &[], None, 7).unwrap();
        assert!(c.summary.contains("The author posted this for abcdef1."));
        assert!(c.annotations.is_empty(), "line numbers from another commit would point at the wrong lines");
        let own = Stamp { sha: "fffffff".into(), files: 5, changed_lines: 10, annotations: vec![Annotation { path: "b.rs".into(), line: 9, ..Default::default() }], ..Default::default() };
        let c = check_run(Some(&s), "fffffff", &[], Some(&own), 7).unwrap();
        assert_eq!(c.annotations[0].path, "b.rs");
    }

    #[test]
    fn without_a_comment_the_check_is_perspicas_own_review() {
        assert!(check_run(None, "h", &[], None, 7).is_none());
        let own = Stamp { sha: "h".into(), files: 3, changed_lines: 40, mechanical_lines: 12, ..Default::default() };
        let c = check_run(None, "h", &[], Some(&own), 7).unwrap();
        assert_eq!(c.conclusion, Some("neutral"));
        assert_eq!(c.title, "3 files changed · 12 of 40 changed lines are mechanical");
        assert!(c.summary.contains("perspica --pr 7 --post"));
        assert!(c.summary.contains("- 12 of 40 changed lines are mechanical (formatting, renames, moved code), and 0 are tests"), "{}", c.summary);
    }

    #[test]
    fn the_authors_score_counts_first_answers_on_written_lessons() {
        let learn: learn::Learn = serde_json::from_value(serde_json::json!({
            "provider": "p", "model": "m", "saved_at": 0,
            "concepts": [
                { "id": "c1", "title": "A", "kind": "k", "why_here": "", "lesson": { "model": "m", "saved_at": 0, "explanation": "", "quiz": [
                    { "question": "q1", "choices": ["x", "y"], "answer": 1 }, { "question": "q2", "choices": ["x", "y"], "answer": 0 } ] } },
                { "id": "c2", "title": "B", "kind": "k", "why_here": "" },
            ],
        })).unwrap();
        let a = |q: &str, choice| QuizAnswer { concept: "c1|A".into(), question: q.into(), choice };
        assert_eq!(author_score(Some(&learn), &[a("q1", 1)]), Score { right: 1, answered: 1, total: 2 });
        let done = author_score(Some(&learn), &[a("q1", 1), a("q2", 1), a("old question", 0)]);
        assert_eq!(done, Score { right: 1, answered: 2, total: 2 });
        assert!(done.complete());
    }

    #[test]
    fn the_workflow_pins_this_version() {
        let w = workflow();
        assert!(w.contains(&format!("PERSPICA_VERSION={}", env!("CARGO_PKG_VERSION"))));
        assert!(w.contains("${{ github.token }}") && w.contains("pull_request_target"));
        assert!(w.contains("pull-requests: write"), "the Action replies to quiz answers on the pull request");
        assert!(!w.contains("ref: ${{ github.event.pull_request.head"), "never checks out the PR's code as the workspace");
    }
}
