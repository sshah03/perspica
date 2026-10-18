//! Learn: the technical concepts behind a change that are worth understanding
//! before approving it. Each gets an explanation, an interactive piece the model
//! writes, and a quiz. It runs in two steps: an outline lists the concepts (or
//! says there's nothing worth learning), then a concept's lesson is written the
//! first time it's opened, so you only pay for what you read.

use crate::intel::{self, IntelInput};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Concepts by the size of the real change: small fixes teach one idea at most.
fn max_concepts(changed_lines: usize) -> usize {
    match changed_lines {
        0..80 => 1,
        80..400 => 2,
        _ => 3,
    }
}
const MAX_QUESTIONS: usize = 3;
/// The interactive piece is stored and embedded in saved pages. Anything longer is dropped.
const MAX_INTERACTIVE_CHARS: usize = 60_000;
const INTERACTIVE_START: &str = "===INTERACTIVE===";
const INTERACTIVE_END: &str = "===END===";
/// The forms an interactive piece can take.
const KINDS: [&str; 5] = ["animation", "diagram", "simulation", "comparison", "formula"];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Learn {
    pub provider: String,
    pub model: String,
    /// Unix seconds.
    pub saved_at: u64,
    /// Why there's nothing worth learning, when the outline found no concepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nothing: Option<String>,
    #[serde(default)]
    pub concepts: Vec<Concept>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Concept {
    /// "c1", "c2", … in the outline's order.
    pub id: String,
    pub title: String,
    /// architecture, algorithm, data structure, …
    pub kind: String,
    /// How the change uses it and why it matters for judging the change.
    pub why_here: String,
    /// Alternatives the change likely didn't use, and why.
    #[serde(default)]
    pub chosen_over: Vec<String>,
    /// Changed files where it shows up. The lesson only gets these files' code.
    #[serde(default)]
    pub files: Vec<String>,
    /// How the lesson shows it: one of `KINDS`, or "none" for text and links only.
    #[serde(default)]
    pub visual: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson: Option<Lesson>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lesson {
    pub model: String,
    pub saved_at: u64,
    /// One sentence: what it is and why it matters here.
    #[serde(default)]
    pub tldr: String,
    /// Plain language for someone meeting the concept for the first time.
    #[serde(default)]
    pub intuition: String,
    /// How it works, when to use it, what it costs and how it fails.
    pub explanation: String,
    /// Only in lessons written before tldr and intuition existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_points: Vec<String>,
    #[serde(default)]
    pub in_this_change: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive: Option<Interactive>,
    #[serde(default)]
    pub quiz: Vec<Question>,
    /// Links to documentation, on hosts in `SOURCE_HOSTS`, checked to load.
    #[serde(default)]
    pub sources: Vec<Source>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interactive {
    /// animation, diagram, simulation, comparison or formula.
    #[serde(default)]
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub caption: String,
    /// A complete HTML document. The viewer runs it in a sandboxed frame with no network.
    pub html: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    pub choices: Vec<String>,
    /// Index of the correct choice.
    pub answer: usize,
    #[serde(default)]
    pub explanation: String,
}

// What the model sends back. Models sometimes write null for an empty list or string.

#[derive(Deserialize)]
struct RawOutline {
    #[serde(default)]
    nothing: Option<String>,
    #[serde(default)]
    concepts: Option<Vec<RawConcept>>,
}

#[derive(Deserialize)]
struct RawConcept {
    title: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    why_here: Option<String>,
    #[serde(default)]
    chosen_over: Option<Vec<String>>,
    #[serde(default)]
    files: Option<Vec<String>>,
    #[serde(default)]
    visual: Option<String>,
}

#[derive(Deserialize)]
struct RawLesson {
    #[serde(default)]
    tldr: Option<String>,
    #[serde(default)]
    intuition: Option<String>,
    #[serde(default)]
    explanation: Option<String>,
    #[serde(default)]
    key_points: Option<Vec<String>>,
    #[serde(default)]
    in_this_change: Option<String>,
    #[serde(default)]
    interactive: Option<RawInteractive>,
    #[serde(default)]
    quiz: Option<Vec<RawQuestion>>,
    #[serde(default)]
    takeaways: Option<Vec<String>>,
    #[serde(default)]
    sources: Option<Vec<RawSource>>,
}

#[derive(Deserialize)]
struct RawSource {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct RawInteractive {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    caption: Option<String>,
}

#[derive(Deserialize)]
struct RawQuestion {
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    choices: Option<Vec<String>>,
    #[serde(default)]
    answer: Option<serde_json::Value>,
    #[serde(default)]
    explanation: Option<String>,
}

const OUTLINE_SPEC: &str = r#"OUTPUT: Respond with ONLY this JSON, no markdown fences, no explanation:
{
  "nothing": "One sentence, max 30 words, on why there's nothing worth learning here, or null when there are concepts.",
  "concepts": [
    {
      "title": "The concept, 2-6 words",
      "kind": "architecture | design pattern | algorithm | data structure | concurrency | protocol | security | math | library | language",
      "why_here": "How this change uses it and why it matters for judging the change. Max 30 words. Name code in backticks.",
      "chosen_over": ["A plausible alternative and why this change likely didn't use it. Max 18 words each."],
      "files": ["changed file paths where it shows up"],
      "visual": "none | animation | diagram | simulation | comparison | formula"
    }
  ]
}

RULES:
- At most MAX_CONCEPTS, most important first. An empty list is the right answer for many changes, and fewer
  is better than more. Never pad the list to look thorough.
- Include a concept only if all three hold: this change actually uses or introduces it; a capable engineer
  might not know it well; and a reviewer who doesn't understand it could approve a wrong change. Not renames,
  formatting, ordinary refactors, basic language features, small helper techniques or general advice.
- Architecture and design decisions come first: a new layer or module boundary, a cache, queue or index, data
  or control flow that moved, sync to async, a new extension point, state moved from globals into explicit
  context, a dependency replaced by in-house code, a component's default implementation switched or an old
  one deprecated. When the change makes such a decision, it is concept 1. Name the decision itself ("Context passed explicitly
  instead of read from a global"), not just the technique.
- An algorithm, library behavior or language detail belongs on the list only when misreading it could hide a
  bug in this change.
- chosen_over: 0-3 items, only alternatives the code or context supports. Leave it empty rather than guess.
- visual: how the lesson should show it. "none" unless a picture shows what words can't: a process over time
  (animation), several parts and how they connect, or an architecture before and after (diagram), how a
  number changes behavior (simulation), two behaviors on the same input (comparison), or math (formula).
  Library and language details are usually "none": text and a link to the docs teach them better. Across
  a list, several "none" is normal.
- Refer to code by symbol and file name, never by change id (readers never see ids).
- Reply with the JSON only. Don't use tools or write files."#;

const LESSON_SPEC: &str = r#"OUTPUT: This JSON object (and, only if the VISUAL section below asks for one, an HTML piece after
it). No markdown fences anywhere.
{
  "tldr": "One sentence, max 18 words: what the concept is and why it matters in this change.",
  "intuition": "30-60 words for someone meeting the concept for the first time (rules below).",
  "explanation": "About 100 words, never over 130: how it works, when it's the right choice, what it costs, how it fails.",
  "in_this_change": "30-70 words: where this change uses it and what a reviewer should check.",
  "sources": [ { "title": "Page title", "url": "https://…" } ],
  "interactive": null,
  "takeaways": ["The 1-3 things a reader must understand, max 12 words each. Most concepts have one or two."],
  "quiz": [
    { "question": "…", "choices": ["…", "…", "…"], "answer": 0, "explanation": "…" }
  ]
}

WHO IT'S FOR: developers of every level reading the same page. A newcomer should get the idea from tldr,
intuition and any visual. An expert should find explanation and in_this_change worth reading, not a rehash
of basics. Don't repeat a point from one section in another.

WRITING RULES (every text field, including the quiz):
- Write like a senior engineer explaining at a whiteboard: short sentences, concrete, every sentence earns
  its place. The word counts are ceilings, not targets: say it in fewer words when you can.
- explanation is for a developer who just read the intuition. Cover what they need to judge the change; leave
  out trivia, edge cases nobody here hits, and history.
- Define a term the first time you use it, in a few words. No filler ("It's important to note", "In
  summary", "Let's dive in"), no hype, no hedging stacks.
- intuition: plain words, one concrete example, or an analogy only if it's accurate. No code jargon left
  undefined.
- explanation: paragraphs separated by a blank line; "- " starts a list item. `code` in backticks, **bold**
  at most twice. No headings.
- in_this_change: name code in backticks; say what would go wrong if the concept were misused here.
- For an architecture or design decision, describe the system before and after: the parts, how data or
  control flows between them, what moved where, and the tradeoff that buys (what gets easier, what gets
  harder or slower, what can now fail).
- sources: 0-3 links to official documentation, specs, RFCs or well-known references (MDN, the language's
  own docs, docs.rs, Wikipedia, an RFC) that cover the concept. For a project-specific idea, a page in the
  repository under review (REPO_URL) works too. Only pages you are sure exist, with no query
  string. Leave the list empty rather than guess; links that don't load are dropped.
- Only state what the code shown supports. If something isn't shown, don't guess about it. Don't give reasons
  for a design that the code or the description doesn't give. Refer to code by
  symbol and file name, never by change id.

QUIZ: questions are about paths the code can really reach, and every answer agrees with the code and with the
visual (if the visual can run the quiz's input, it gives the quiz's answer). One multiple-choice question per takeaway, so most quizzes have 1 or 2 questions and none more than 3.
When the core idea and how this change uses it are really the same point, that's one takeaway and one question.
3-4 choices each; "answer" is the 0-based index of the one
correct choice. Start with the core idea (a newcomer who read the intuition can answer it); prefer questions
about this change's own code ("what happens in `foo` if…") and its costs or failure modes. Wrong choices are
plausible mistakes. Choices are shown in shuffled order, so in explanations refer to a choice by
what it says, never by its letter or position. explanation: max 30 words, says why the answer is right and
why the most tempting wrong choice is wrong, without restating the question.

Reply with text only. Don't use tools or write files."#;

const NO_VISUAL: &str = "VISUAL: none for this concept. Leave \"interactive\" as null and write no HTML. \
    The text and the sources carry it.";

/// What each form of visual is for.
fn visual_guidance(kind: &str) -> &'static str {
    match kind {
        "animation" => "It shows something unfolding over time: messages between parties, a walk through a structure, \
            state changing until something happens.",
        "diagram" => "It shows structure or relationships: the parts, how data or control flows between them, and for \
            an architecture change, what moved.",
        "simulation" => "It shows how behavior depends on a number (load, input size, a probability), computed from the \
            real rule in this change, with the point where behavior changes easy to see.",
        "comparison" => "It shows old and new behavior, or two approaches, on the same input, so the real difference \
            is obvious.",
        _ => "It makes math easier to feel than read: the formula in MathML, and the result responding as the reader \
            changes an input, using this change's real constants.",
    }
}

const VISUAL_RULES: &str = r#"Replace "interactive": null with { "kind": "KIND", "title": "Max 8 words", "caption": "What to look at or do, max 20 words" }.
Then, after the JSON, write the piece between these two marker lines:
===INTERACTIVE===
<!doctype html>
<html>…a complete document…</html>
===END===

THE FIGURE. It sits beside this lesson in a code review, read by a developer who knows code but maybe not this
concept. Aim for what the best explorable explanations do (Sam Rose's samwho.dev, Bartosz Ciechanowski, Red Blob
Games, Lin Clark's code cartoons): within a few seconds the reader sees the mechanism happen, and playing with it
makes the idea stick. It should be better than any textbook figure of this idea, and something a developer would
want to share.

WHAT GREAT LOOKS LIKE
- The drawing carries the idea and words support it. The lesson already holds the reasoning, caveats and code, so
  the figure needs only small labels on the things themselves and at most a line saying what's happening. Every
  mark earns its place, because clutter costs the reader attention they're spending on the review.
- Its form comes from this change. Draw the real thing in its own shape: the queue filling, the tree an error
  climbs, the bytes in their order, messages crossing between two parties, a curve bending at a limit. Pick an
  instance small enough to take in whole (cells you could count, the actual string, a handful of nodes), with values
  that make the point visible. A layout that would fit any PR is a sign to look again.
- The reader can read it without being told: requests as balls that travel, a queue as a column that fills, a
  pointer as an arrow that moves, a failed attempt that fades. State shows through form (fill, outline, position,
  fading) more than through text or badges.
- A quiet scene, with color for the story. Structure in --fg-muted and --border; --accent for the thing acting;
  --bad for the failure and --good for the right outcome, meaning the same thing throughout. Thin lines, simple
  shapes, and the figure floats on the page rather than sitting in cards.
- The first view already says something. Picture a screenshot taken the moment it loads, before anything moves or
  is clicked: it shows the answer, the failure next to the right result, or the key moment. When something moves,
  the motion replays how it got there.
- Interaction where the reader learns by changing something: drag the thing itself, scrub through time, pick the
  input that matters. Motion shows cause before effect, one thing at a time, with easing. A still figure is a fine
  answer when nothing needs to move. To compare two behaviors, two small versions from the same start, side by
  side, usually read faster than a toggle.

DEFAULTS TO NOTICE. Generated figures drift toward these. Each is right sometimes, so use one when this concept
really calls for it, never out of habit: bordered cards and panels; pills and badges carrying the state; rows of
text or key: value tables; stat tiles; code blocks or code expressions as labels inside the figure; legends; a row
of Play, Step and Back buttons; a before/after toggle where two small side-by-side views would do; sentences of
prose inside the figure.

HOW TO WORK
1. Before writing HTML, decide for yourself (not in the output): the one thing the reader should see, the picture
   that shows it, what they can do with it, and what you're leaving out. If you'd draw the same thing for an
   unrelated PR, look again.
2. Build it.
3. Read it back as someone seeing it cold, at 760px and on a phone, in light and dark. Can they tell what they're
   looking at and what it shows within a few seconds? If not, change the picture rather than adding words. Spend
   your boldness on one element and keep the rest quiet. Then remove one thing.

TRUTH. Everything drawn matches what this change's code really does: real names, values worked out by tracing the
code on a concrete input, and the old behavior as it really was. A reviewer checks the figure against the code
afterwards, so a few exact claims beat many approximate ones.

CRAFT. Readable in both themes; nothing overlapping or cut off anywhere from 300 to 900px wide; text 12px or larger;
nothing important only on hover, since phones can't hover; with prefers-reduced-motion, change things instantly.

TOOLS already in the frame: native <button>, <select> and <input type="range"> are styled; a selected toggle has
aria-pressed="true", and a group of toggles goes in <div class="k-seg">. Kit.steps(total, show, { interval, start })
calls show(i) to draw step i, opens on step start (0 by default, so pass the key step to open there), and wires
buttons marked data-kit="play" and data-kit="step" and a data-kit="count" label, which it keeps in step with what's
drawn; it returns { go(i), play(), stop() }. Colors and fonts come only from these variables, so it matches the viewer's
theme: --bg, --bg-alt, --fg, --fg-muted, --border, --accent, --good, --bad, --warn, --font, --mono. Leave the page
background transparent. No emoji.

HTML RULES:
- One complete, self-contained HTML document with inline <style> and <script>, under 20,000 characters,
  plain modern JavaScript, no frameworks. It runs in a sandboxed frame with no network: no external scripts,
  stylesheets, fonts or images, no requests, no alert(). Draw with SVG, canvas and HTML; write formulas in MathML.
- Wrap the script in an IIFE, so its names can't collide with the page's own (closed, name, status, top).
- Before you finish, read the script through once: every function and variable it uses is defined, and the
  first view draws completely when the page loads, with no click."#;

fn visual_section(kind: &str) -> String {
    if !KINDS.contains(&kind) {
        return NO_VISUAL.to_string();
    }
    format!("VISUAL: show this concept as a {kind}. {}\n{}", visual_guidance(kind), VISUAL_RULES.replace("KIND", kind))
}

/// What the checking pass returns when the draft needs no changes.
pub const NO_CHANGES: &str = "===NO CHANGES===";

const VERIFY_SPEC: &str = r#"CHECK THE DRAFT. Another engineer wrote the draft above for this concept. Before it reaches reviewers, check it
against the code, as a careful reviewer would, and fix what's wrong. Work through each point:
1. Every claim in the title, tldr, intuition, explanation, in_this_change, quiz and the visual (in every state its
   controls can reach) is true for this code and for the real runtime. Anything that only holds in a model says so briefly in the lesson text.
2. Trace each number, output and string by hand on its input, the way this code computes it. Numbers, defaults and
   labels come from this code (its own constants, conversions and default settings), not from a reference table, a
   spec or memory. Fix any that don't match. A count or badge covers every
   item drawn, unchanged ones included, and has the same scope as its sentence.
3. "Before" is the real old behavior. If something is called hypothetical, check the removed code (and any removed
   dependency) first: when the old code really did it, show it as the real before.
4. For each absolute (every, only, always, never, can't, forever, "if X then Y"), look for a counterexample in the code,
   the tests and the rest of the diff, including the visual's own cases. A claimed cost or failure (spins forever,
   leaks, can't be caught) holds only if every exit path and every other check in the code allows it. A claim copied from a code comment is checked
   too: a changelog entry or a test can contradict it. Qualify it or remove it. The tldr and title agree with every
   case the visual shows.
5. Everything the diff adds is accounted for: no "breaks", "requires" or "can't catch" when the diff has code that
   handles it.
   A claim that rests on code not shown here (a default set in another file, a library's exact output, a value only
   running the code would give) goes only as far as the code shown supports: say what it depends on, or use the
   test's own values and say they're the test's.
6. The lesson and visual together show what sets the problem up, the condition that decides the outcome, and the
   outcome a user would see for the input shown. A case that was right before and is an error now is a
   regression, not colored as a fix.
7. Highlighted code is the line that causes the behavior. A part's detail quotes that part's own code. Each state
   change is pinned to the function that makes it.
8. Quiz answers agree with the code and with the visual, ask about paths the code can reach, and every question's
   premise is true.
9. A fact you correct in one place is corrected everywhere it appears: tldr, intuition, explanation, quiz, visual.
10. The script defines everything it uses and runs inside an IIFE, and the first view draws with no click.
11. The visual reads clearly at a glance: verdicts and colors match the outcomes they label, emphasis lands on what
   matters, captions and labels agree with what's drawn, and no text overlaps other text or lines. Text that only repeats what's already drawn or said can go.

OUTPUT: if nothing needs fixing, reply with exactly NO_CHANGES and nothing else. Otherwise reply with only a list of
fixes, each one in this form, and nothing else:
===FIND===
text copied exactly from the DRAFT, long enough to appear only once
===REPLACE===
the corrected text
===END===
- FIND is copied character for character from the draft as shown, including the JSON escaping inside the JSON part
  (\" and \n), so keep it short: a sentence, a label, a line, a number with a few words around it.
- In the JSON part, REPLACE keeps the JSON valid. To change a quiz answer, fix the question, choices, "answer" and
  explanation together, with one FIND that covers all of them.
- In the HTML, change only what's wrong. Keep the elements, classes and layout, and keep a replacement about as long
  as what it replaces, so it fits where the old text was. Fix the wrong labels, values or states; don't redraw.
- Fix only what's wrong or confusing, and keep the draft about as long as it is. Correct a claim in place, or cut it,
  rather than explaining around it. Don't restyle or reword correct text, and never remove a true claim unless it
  only repeats one. When the draft leaves out something a reviewer needs (points 5 and 6), add it to the lesson text
  in a sentence, not to the figure."#;

/// The prompt for the checking pass: the lesson's own prompt (so it sees the same code and rules), then the draft.
pub fn verify_prompt(lesson_prompt: &str, draft: &str) -> String {
    format!("{lesson_prompt}\n\nDRAFT:\n{draft}\n\n{}", VERIFY_SPEC.replace("NO_CHANGES", NO_CHANGES))
}

const LAYOUT_SPEC: &str = r#"FIX THE LAYOUT. The interactive figure below is from a lesson in a code review. It was drawn in a browser,
and a layout check found these problems:
PROBLEMS

Fix them with the smallest changes that make the figure read cleanly at every width from 300 to 900px: move, shorten
or wrap labels, give text room, let rows wrap, size drawings to their container, and keep text 12px or larger. Keep
what the figure shows, its values and words, and its look. Don't redraw it.

OUTPUT: if nothing needs fixing, reply with exactly NO_CHANGES and nothing else. Otherwise reply with only a list of
fixes, each one in this form, and nothing else:
===FIND===
text copied exactly from the figure, long enough to appear only once
===REPLACE===
the corrected text
===END===

FIGURE:
"#;

/// The prompt for fixing what the viewer's layout check found in a figure: overlapping or cut-off text, text too
/// small to read, a figure wider than its frame.
pub fn layout_prompt(html: &str, problems: &[String]) -> String {
    let list = problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n");
    format!("{}{html}", LAYOUT_SPEC.replace("PROBLEMS", &list).replace("NO_CHANGES", NO_CHANGES))
}

/// Applies the checking pass's fixes to the draft. A fix is used only when its FIND text appears exactly
/// once, so a fix that was copied wrong is skipped instead of landing in the wrong place. A fix is also
/// skipped when the result isn't `valid`, so one broken fix doesn't cost the others.
/// Returns the patched draft, how many fixes were used, and how many were skipped.
pub fn apply_fixes(draft: &str, reply: &str, valid: impl Fn(&str) -> bool) -> (String, usize, usize) {
    let mut out = draft.to_string();
    let (mut used, mut skipped) = (0, 0);
    let mut rest = reply;
    while let Some(at) = rest.find("===FIND===") {
        let body = &rest[at + "===FIND===".len()..];
        let (Some(r), Some(e)) = (body.find("===REPLACE==="), body.find("===END===")) else { skipped += 1; break };
        if e < r {
            skipped += 1;
            rest = &body[e + "===END===".len()..];
            continue;
        }
        let trim = |t: &str| t.strip_prefix('\n').unwrap_or(t).strip_suffix('\n').unwrap_or(t).to_string();
        let find = trim(&body[..r]);
        let replace = trim(&body[r + "===REPLACE===".len()..e]);
        let patched = (!find.is_empty() && out.matches(find.as_str()).count() == 1)
            .then(|| out.replacen(find.as_str(), &replace, 1))
            .filter(|p| valid(p));
        match patched {
            Some(p) => {
                out = p;
                used += 1;
            }
            None => skipped += 1,
        }
        rest = &body[e + "===END===".len()..];
    }
    (out, used, skipped)
}

/// Said instead of calling the model when the change is only mechanical.
pub const NOTHING_MECHANICAL: &str = "This change only touches formatting, comments, docs, renames or unchanged moves, so there's nothing new to learn from it.";

/// Lines that really changed: not mechanical, not in generated files.
pub fn changed_lines(input: &IntelInput<'_>) -> usize {
    use perspica_core::manifest::ChangeKind;
    input.results.iter()
        .filter(|r| !r.review.generated)
        .flat_map(|r| r.hunks.iter().filter(|h| h.noise.is_none()))
        .flat_map(|h| h.changes.iter())
        .filter(|c| c.noise.is_none() && matches!(c.kind, ChangeKind::Added | ChangeKind::Removed | ChangeKind::Modified))
        .count()
}

/// Whether there's anything for the model to look at.
pub fn mechanical_only(input: &IntelInput<'_>) -> bool {
    intel::llm_entry_ids(input).is_empty()
}

fn author_and_requirements(input: &IntelInput<'_>) -> String {
    let author = input.author_context
        .filter(|c| !c.trim().is_empty())
        .map(|c| format!("AUTHOR CONTEXT (PR description / commit messages; may be incomplete):\n{}\n\n", intel::truncate(c, 3000)))
        .unwrap_or_default();
    let reqs = if input.requirements.is_empty() {
        String::new()
    } else {
        let list: String = input.requirements.iter().map(|r| format!("[{}] {}\n", r.id, r.text)).collect();
        format!("USER REQUIREMENTS (the user's own messages to the coding agent that made this change, oldest first). \
                 When they explain a choice, use them for why_here and chosen_over:\n{}\n", intel::truncate(&list, 8000))
    };
    author + &reqs
}

pub fn outline_prompt(input: &IntelInput<'_>) -> String {
    let cap = max_concepts(changed_lines(input));
    let spec = OUTLINE_SPEC.replace("MAX_CONCEPTS", &if cap == 1 { "1 concept".into() } else { format!("{cap} concepts") });
    format!(
        "You are a senior engineer helping a code reviewer understand the ideas behind a change before they approve it. \
         Find the few significant ideas a reviewer must understand to judge it: first the architecture and design \
         decisions it makes, then algorithms and data structures, concurrency, protocols, security and math, and \
         non-obvious behavior of a library or language feature the change relies on.\n\n\
         {}CHANGES (id [type] location name: detail):\n{}\n\nCONTEXT:\n{}\n\n{spec}",
        author_and_requirements(input),
        intel::build_entry_listing(input),
        intel::build_rich_context(input.results, input.sources, input.budget),
    )
}

/// The prompt for one concept's lesson. It gets only the code of the files the concept appears in.
pub fn lesson_prompt(input: &IntelInput<'_>, concept: &Concept, others: &[&str], repo_url: Option<&str>) -> String {
    let files: HashSet<usize> = input.sources.iter().enumerate()
        .filter(|(_, (path, _, _))| concept.files.iter().any(|f| f == path))
        .map(|(i, _)| i)
        .collect();
    let only = (!files.is_empty()).then_some(&files);
    let alternatives = if concept.chosen_over.is_empty() {
        String::new()
    } else {
        format!("ALTERNATIVES IT WAS LIKELY CHOSEN OVER:\n{}\n", concept.chosen_over.iter().map(|a| format!("- {a}\n")).collect::<String>())
    };
    let others = if others.is_empty() {
        String::new()
    } else {
        format!("OTHER CONCEPTS IN THIS CHANGE (taught separately, don't repeat them): {}\n", others.join("; "))
    };
    format!(
        "You are teaching one concept behind a code change to the developers reviewing it, so they can judge the \
         change with confidence.\n\n\
         CONCEPT: {} ({})\nWHY IT'S HERE: {}\n{alternatives}{others}\n\
         {}CHANGES (id [type] location name: detail):\n{}\n\nCODE (the changed code where this concept appears):\n{}\n\n{LESSON_SPEC}\n\n{}",
        concept.title, concept.kind, concept.why_here,
        author_and_requirements(input),
        intel::build_entry_listing(input),
        intel::build_rich_context_only(input.results, input.sources, only, input.budget),
        visual_section(&concept.visual),
    ).replace("REPO_URL", repo_url.filter(|r| r.starts_with("https://github.com/")).unwrap_or("not available"))
}

/// The concepts, or why there are none. `paths` are the changed files.
pub fn parse_outline(response: &str, paths: &[String], changed_lines: usize) -> Result<(Option<String>, Vec<Concept>), String> {
    let (raw, _) = first_json::<RawOutline>(response).map_err(|e| {
        eprintln!("perspica: couldn't read a concept list. It started: {}", clip(response, 300));
        format!("Failed to parse the model's concept list: {e}")
    })?;
    let concepts: Vec<Concept> = raw.concepts.unwrap_or_default().into_iter()
        .filter(|c| !c.title.trim().is_empty())
        .take(max_concepts(changed_lines))
        .enumerate()
        .map(|(i, c)| Concept {
            id: format!("c{}", i + 1),
            title: clip(&c.title, 80),
            kind: c.kind.map(|k| clip(&k.to_lowercase(), 30)).filter(|k| !k.is_empty()).unwrap_or_else(|| "concept".into()),
            why_here: c.why_here.map(|w| clip(&w, 600)).unwrap_or_default(),
            chosen_over: c.chosen_over.unwrap_or_default().into_iter().map(|a| clip(&a, 240)).filter(|a| !a.is_empty()).take(3).collect(),
            // Only real changed files, so the lesson's code filter can't drop everything by mistake.
            files: c.files.unwrap_or_default().into_iter().filter(|f| paths.contains(f)).collect(),
            visual: c.visual.map(|v| v.trim().to_lowercase()).filter(|v| KINDS.contains(&v.as_str())).unwrap_or_else(|| "none".into()),
            lesson: None,
        })
        .collect();
    let nothing = if concepts.is_empty() {
        Some(raw.nothing.map(|n| clip(&n, 400)).filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Nothing in this change needs background to review.".into()))
    } else {
        None
    };
    Ok((nothing, concepts))
}

pub fn parse_lesson(response: &str, model: &str, visual: &str) -> Result<Lesson, String> {
    // The HTML goes after the JSON, between marker lines, so it needs no JSON escaping.
    let (json_part, html) = match response.find(INTERACTIVE_START) {
        Some(at) => {
            let rest = &response[at + INTERACTIVE_START.len()..];
            let html = rest.find(INTERACTIVE_END).map_or(rest, |end| &rest[..end]);
            (&response[..at], Some(strip_fence(html)))
        }
        None => (response, None),
    };
    let (raw, after) = first_json::<RawLesson>(json_part).map_err(|e| {
        eprintln!("perspica: couldn't read a lesson. It started: {}", clip(response, 300));
        format!("Failed to parse the model's lesson: {e}")
    })?;
    // Without the marker lines, a document written after the JSON is still the piece.
    let html = html.or_else(|| {
        let lower = after.to_ascii_lowercase();
        lower.find("<!doctype").or_else(|| lower.find("<html")).map(|at| strip_fence(&after[at..]))
    });
    let explanation = raw.explanation.unwrap_or_default().trim().to_string();
    if explanation.is_empty() {
        return Err("The model's lesson had no explanation.".into());
    }
    let interactive = match (raw.interactive, html) {
        (Some(meta), Some(html)) if KINDS.contains(&visual) && !html.is_empty() && html.len() <= MAX_INTERACTIVE_CHARS => Some(Interactive {
            kind: visual.to_string(),
            title: meta.title.map(|t| clip(&t, 80)).filter(|t| !t.is_empty()).unwrap_or_else(|| "Try it".into()),
            caption: meta.caption.map(|c| clip(&c, 300)).unwrap_or_default(),
            html: html.to_string(),
        }),
        _ => None,
    };
    Ok(Lesson {
        model: model.to_string(),
        saved_at: crate::saved::now(),
        tldr: raw.tldr.map(|t| clip(&t, 300)).unwrap_or_default(),
        intuition: raw.intuition.unwrap_or_default().trim().to_string(),
        explanation,
        key_points: raw.key_points.unwrap_or_default().into_iter().map(|k| clip(&k, 240)).filter(|k| !k.is_empty()).take(6).collect(),
        in_this_change: raw.in_this_change.unwrap_or_default().trim().to_string(),
        interactive,
        // One question per takeaway: a quiz is only as long as what there is to check.
        quiz: raw.quiz.unwrap_or_default().into_iter().filter_map(question)
            .take(raw.takeaways.as_ref().map_or(MAX_QUESTIONS, |t| t.len().clamp(1, MAX_QUESTIONS)))
            .collect(),
        sources: raw.sources.unwrap_or_default().into_iter().filter_map(source).take(3).collect(),
    })
}

/// Documentation sites a lesson may link to. Models invent URLs, and the code under review can
/// steer what they write, so links go only to these sites, with no query string, and only after
/// `check_sources` has seen them load.
const SOURCE_HOSTS: &[&str] = &[
    "doc.rust-lang.org", "docs.rs", "rust-lang.github.io", "developer.mozilla.org", "html.spec.whatwg.org",
    "tc39.es", "nodejs.org", "www.typescriptlang.org", "docs.python.org", "peps.python.org", "go.dev",
    "pkg.go.dev", "docs.oracle.com", "openjdk.org", "learn.microsoft.com", "kotlinlang.org",
    "docs.scala-lang.org", "www.php.net", "ruby-doc.org", "guides.rubyonrails.org", "api.rubyonrails.org",
    "en.wikipedia.org", "datatracker.ietf.org", "www.rfc-editor.org", "www.w3.org", "owasp.org",
    "cheatsheetseries.owasp.org", "martinfowler.com", "react.dev", "flask.palletsprojects.com",
    "docs.github.com", "tree-sitter.github.io", "www.sqlite.org", "www.postgresql.org", "redis.io",
    "kubernetes.io", "docs.docker.com", "man7.org", "pubs.opengroup.org", "en.cppreference.com",
    "docs.kernel.org", "docs.ruby-lang.org", "www.scala-lang.org", "typelevel.org", "docs.gradle.org",
    "developer.android.com",
];

/// Whether a lesson may link to `url`: a documentation site, the repository under review or a page in
/// it (`repo_url`, which comes from GitHub, not from the model), or the project's own site (`project_host`,
/// from the repository's settings, which a pull request can't change).
fn allowed_link(url: &str, repo_url: Option<&str>, project_host: Option<&str>) -> bool {
    allowed_site(url, repo_url, project_host) && well_formed(url)
}

/// Whether `url` is on one of those sites, whatever its query string. Used for where a link
/// ends up after redirects, which the site chooses, not the model (docs sites add `?view=…`).
fn allowed_site(url: &str, repo_url: Option<&str>, project_host: Option<&str>) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '#']).next().unwrap_or("");
    let repo = repo_url.filter(|r| r.starts_with("https://github.com/")).map(|r| r.trim_end_matches('/'));
    let in_repo = repo.is_some_and(|r| url == r || url.starts_with(&format!("{r}/")) || url.starts_with(&format!("{r}#")));
    let on_project_site = project_host.is_some_and(|p| host == p || host.ends_with(&format!(".{p}")));
    SOURCE_HOSTS.contains(&host) || in_repo || on_project_site
}

/// No query string, credentials or spaces: nothing in the link can carry data somewhere.
fn well_formed(url: &str) -> bool {
    url.starts_with("https://") && !url.contains('?') && !url.contains('@') && !url.chars().any(char::is_whitespace)
}

fn source(s: RawSource) -> Option<Source> {
    let url = s.url?.trim().to_string();
    if !well_formed(&url) {
        return None;
    }
    let host = url.trim_start_matches("https://").split(['/', '#']).next().unwrap_or("").to_string();
    let title = s.title.map(|t| clip(&t, 120)).filter(|t| !t.is_empty()).unwrap_or(host);
    Some(Source { title, url })
}

/// Code hosts and package registries: a project "homepage" there is not the project's own site.
const NOT_PROJECT_SITES: &[&str] = &["github.com", "gitlab.com", "bitbucket.org", "npmjs.com", "pypi.org", "crates.io", "rubygems.org", "packagist.org", "nuget.org"];

/// The project's own site from its GitHub settings, like `libuv.org`, so its docs subdomains count too.
async fn project_host(client: &reqwest::Client, repo_url: Option<&str>) -> Option<String> {
    let repo = repo_url?.strip_prefix("https://github.com/")?.trim_end_matches('/');
    let v: serde_json::Value = client.get(format!("https://api.github.com/repos/{repo}")).send().await.ok()?.json().await.ok()?;
    let homepage = v["homepage"].as_str()?.trim();
    let host = homepage.split("://").nth(1)?.split(['/', '?', '#']).next()?.trim_start_matches("www.").to_lowercase();
    (host.contains('.') && !NOT_PROJECT_SITES.iter().any(|n| host == *n || host.ends_with(&format!(".{n}")))).then_some(host)
}

/// Keep the links on allowed sites that load, and say which were left out.
pub async fn check_sources(lesson: &mut Lesson, repo_url: Option<&str>) {
    if lesson.sources.is_empty() {
        return;
    }
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .user_agent(concat!("perspica/", env!("CARGO_PKG_VERSION")))
        .build() else { lesson.sources.clear(); return };
    // Only ask GitHub for the project's site when a link needs it.
    let needs_site = lesson.sources.iter().any(|s| !allowed_link(&s.url, repo_url, None));
    let site = if needs_site { project_host(&client, repo_url).await } else { None };
    let mut ok = Vec::new();
    for s in &lesson.sources {
        let keep = if !allowed_link(&s.url, repo_url, site.as_deref()) {
            eprintln!("perspica: left out a lesson link that isn't on a known documentation site: {}", s.url);
            false
        } else if !client.get(&s.url).send().await
            .is_ok_and(|r| r.status().is_success() && allowed_site(r.url().as_str(), repo_url, site.as_deref())) {
            eprintln!("perspica: left out a lesson link that didn't load: {}", s.url);
            false
        } else {
            true
        };
        ok.push(keep);
    }
    let mut ok = ok.into_iter();
    lesson.sources.retain(|_| ok.next().unwrap_or(false));
}

/// A question the viewer can grade: 2-5 non-empty choices and an answer among them.
fn question(q: RawQuestion) -> Option<Question> {
    let text = q.question?.trim().to_string();
    let choices: Vec<String> = q.choices?.into_iter().map(|c| c.trim().to_string()).collect();
    if text.is_empty() || !(2..=5).contains(&choices.len()) || choices.iter().any(|c| c.is_empty()) {
        return None;
    }
    // Some models write the index as a string.
    let answer = match q.answer? {
        serde_json::Value::Number(n) => n.as_u64()? as usize,
        serde_json::Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    if answer >= choices.len() {
        return None;
    }
    // Models put the right answer first far more often than chance. Shuffle the choices,
    // seeded by the question so the order stays the same every time it's shown.
    let mut order: Vec<usize> = (0..choices.len()).collect();
    let mut seed = text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    for i in (1..order.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        order.swap(i, (seed % (i as u64 + 1)) as usize);
    }
    let answer = order.iter().position(|&o| o == answer)?;
    let choices = order.into_iter().map(|o| choices[o].clone()).collect();
    Some(Question { question: text, choices, answer, explanation: q.explanation.unwrap_or_default().trim().to_string() })
}

/// The first JSON object in a model's reply, and the text after it. Reading stops at the end of
/// that object, so braces in whatever follows (an HTML piece, a note) don't get pulled in.
fn first_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<(T, &str), String> {
    let start = text.find('{').ok_or("no JSON object in the reply")?;
    let mut values = serde_json::Deserializer::from_str(&text[start..]).into_iter::<T>();
    let value = values.next().ok_or("no JSON object in the reply")?.map_err(|e| e.to_string())?;
    Ok((value, &text[start + values.byte_offset()..]))
}

fn strip_fence(html: &str) -> &str {
    let t = html.trim();
    let t = t.strip_prefix("```html").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    t.strip_suffix("```").unwrap_or(t).trim()
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fixes_apply_only_when_they_match_once() {
        let draft = "{\"tldr\": \"Two of three\"}\n===INTERACTIVE===\n<b>2 of 3</b><i>x</i><i>x</i>\n===END===";
        let reply = "===FIND===\nTwo of three\n===REPLACE===\nThree of three\n===END===\n\
                     ===FIND===\n<b>2 of 3</b>\n===REPLACE===\n<b>3 of 3</b>\n===END===\n\
                     ===FIND===\n<i>x</i>\n===REPLACE===\n<i>y</i>\n===END===\n\
                     ===FIND===\nnot in the draft\n===REPLACE===\nanything\n===END===";
        let (out, used, skipped) = apply_fixes(draft, reply, |_| true);
        assert_eq!((used, skipped), (2, 2), "the repeated <i>x</i> and the missing text are skipped");
        assert!(out.contains("Three of three") && out.contains("<b>3 of 3</b>") && out.matches("<i>x</i>").count() == 2);
    }

    #[test]
    fn layout_prompt_lists_the_problems_and_ends_with_the_figure() {
        let p = layout_prompt("<svg></svg>", &["at 380px wide, \"a\" overlaps \"b\"".to_string()]);
        assert!(p.contains("- at 380px wide, \"a\" overlaps \"b\""));
        assert!(p.contains(NO_CHANGES) && !p.contains("PROBLEMS"));
        assert!(p.ends_with("FIGURE:\n<svg></svg>"));
    }

    #[test]
    fn a_fix_that_breaks_the_lesson_is_skipped_alone() {
        let draft = "{\"tldr\": \"Two of three\", \"intuition\": \"x\"}";
        let reply = "===FIND===\nTwo of three\n===REPLACE===\nThree of three\n===END===\n\
                     ===FIND===\n\"intuition\": \"x\"\n===REPLACE===\n\"intuition\": \"x\n===END===";
        let valid = |t: &str| serde_json::from_str::<serde_json::Value>(t).is_ok();
        let (out, used, skipped) = apply_fixes(draft, reply, valid);
        assert_eq!((used, skipped), (1, 1));
        assert!(out.contains("Three of three") && valid(&out));
    }

    use super::*;

    fn sources() -> Vec<String> {
        vec!["src/ring.rs".into()]
    }

    #[test]
    fn outline_with_concepts() {
        let r = r#"```json
{"nothing": null, "concepts": [
  {"title": "Consistent hashing", "kind": "Algorithm", "why_here": "`Ring::node_for` maps keys to nodes.",
   "chosen_over": ["Modulo hashing: remaps most keys when a node joins"], "files": ["src/ring.rs", "src/made_up.rs"]},
  {"title": "  ", "kind": null}
]}
```"#;
        let (nothing, concepts) = parse_outline(r, &sources(), 500).unwrap();
        assert!(nothing.is_none());
        assert_eq!(concepts.len(), 1);
        assert_eq!(concepts[0].id, "c1");
        assert_eq!(concepts[0].kind, "algorithm");
        assert_eq!(concepts[0].files, ["src/ring.rs"]);
        // No visual named means none.
        assert_eq!(concepts[0].visual, "none");
    }

    #[test]
    fn small_changes_teach_one_concept() {
        let r = r#"{"concepts": [{"title": "A", "files": []}, {"title": "B", "files": []}]}"#;
        assert_eq!(parse_outline(r, &sources(), 20).unwrap().1.len(), 1);
        assert_eq!(parse_outline(r, &sources(), 200).unwrap().1.len(), 2);
    }

    #[test]
    fn outline_with_nothing() {
        let (nothing, concepts) = parse_outline(r#"{"nothing": "Just a config tweak.", "concepts": []}"#, &sources(), 10).unwrap();
        assert!(concepts.is_empty());
        assert_eq!(nothing.as_deref(), Some("Just a config tweak."));
        let (nothing, _) = parse_outline(r#"{"concepts": null}"#, &sources(), 10).unwrap();
        assert!(nothing.is_some());
    }

    #[test]
    fn lesson_with_interactive_and_bad_questions() {
        let r = r#"{"explanation": "A ring of hashes {not json}.", "key_points": ["One"], "in_this_change": "In `node_for`.",
 "tldr": "Keys sit on a ring.", "interactive": {"kind": "Animation", "title": "The ring", "caption": "Add a node."},
 "quiz": [
   {"question": "Q1", "choices": ["a", "b", "c"], "answer": 1, "explanation": "b"},
   {"question": "Q2", "choices": ["a", "b"], "answer": "0"},
   {"question": "Q3", "choices": ["a", "b"], "answer": 5},
   {"question": "Q4", "choices": ["only"], "answer": 0}
 ]}
===INTERACTIVE===
```html
<!doctype html><html><body><script>const o = {a: 1};</script></body></html>
```
===END==="#;
        let l = parse_lesson(r, "m", "animation").unwrap();
        // No takeaways listed: up to the cap.
        assert_eq!(l.quiz.len(), 2);
        assert_eq!(l.quiz[0].choices[l.quiz[0].answer], "b");
        assert_eq!(l.quiz[1].choices[l.quiz[1].answer], "a");
        assert_eq!(l.tldr, "Keys sit on a ring.");
        let i = l.interactive.unwrap();
        assert_eq!(i.kind, "animation");
        assert!(i.html.starts_with("<!doctype html>") && i.html.ends_with("</html>"));
    }

    #[test]
    fn quiz_no_longer_than_takeaways() {
        let r = r#"{"explanation": "E.", "takeaways": ["one idea"], "quiz": [
            {"question": "Q1", "choices": ["a", "b"], "answer": 0}, {"question": "Q2", "choices": ["a", "b"], "answer": 1}]}"#;
        assert_eq!(parse_lesson(r, "m", "none").unwrap().quiz.len(), 1);
    }

    #[test]
    fn shuffled_answers_spread_over_positions() {
        let mut at = [0; 4];
        for i in 0..200 {
            let q = question(RawQuestion {
                question: Some(format!("Question {i}?")),
                choices: Some(vec!["right".into(), "w1".into(), "w2".into(), "w3".into()]),
                answer: Some(serde_json::json!(0)),
                explanation: None,
            }).unwrap();
            assert_eq!(q.choices[q.answer], "right");
            at[q.answer] += 1;
        }
        assert!(at.iter().all(|&n| n >= 25), "{at:?}");
    }

    #[test]
    fn links_only_to_known_sites() {
        let repo = Some("https://github.com/libuv/libuv");
        let ok = |url: &str| allowed_link(url, repo, Some("libuv.org"));
        assert!(ok("https://doc.rust-lang.org/std/collections/struct.BTreeSet.html"));
        assert!(ok("https://github.com/libuv/libuv"));
        assert!(ok("https://github.com/libuv/libuv/blob/v1.x/docs/src/fs.rst"));
        assert!(ok("https://docs.libuv.org/en/v1.x/fs.html"));
        assert!(ok("https://libuv.org/"));
        assert!(!ok("https://github.com/libuv/libuv-evil/x"));
        assert!(!ok("https://github.com/evil/x"));
        assert!(!ok("https://notlibuv.org/"));
        assert!(!ok("https://evil.example/docs"));
        assert!(!ok("http://doc.rust-lang.org/std/"));
        assert!(!ok("https://en.wikipedia.org/wiki/X?leak=code"));
        assert!(!ok("https://doc.rust-lang.org.evil.example/"));
        assert!(!ok("https://user@docs.rs/serde"));
        // A docs site may redirect to the same page with a query string; that's still its own site.
        assert!(allowed_site("https://learn.microsoft.com/en-us/dotnet/api/system.icomparable-1?view=net-10.0", repo, None));
        assert!(!allowed_site("https://evil.example/?x=1", repo, None));
    }

    #[test]
    fn lesson_with_html_but_no_markers() {
        // The case that failed: the piece written right after the JSON, without the marker lines.
        let r = "```json\n{\"explanation\": \"E.\", \"interactive\": {\"title\": \"T\"}, \"quiz\": []}\n```\n<!doctype html><html><body><script>const a = {b: 1};</script></body></html>";
        let l = parse_lesson(r, "m", "diagram").unwrap();
        assert_eq!(l.explanation, "E.");
        assert!(l.interactive.unwrap().html.starts_with("<!doctype html>"));
        // And trailing prose after the JSON is ignored.
        assert!(parse_lesson("{\"explanation\": \"E.\"}\nHope this helps! {not json}", "m", "none").is_ok());
    }

    #[test]
    fn lesson_without_interactive() {
        let l = parse_lesson(r#"{"explanation": "Text.", "interactive": null, "quiz": []}"#, "m", "none").unwrap();
        assert!(l.interactive.is_none());
        assert!(parse_lesson(r#"{"explanation": ""}"#, "m", "none").is_err());
    }
}
