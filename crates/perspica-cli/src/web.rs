use crate::{deep, intel, learn, llm, MultiFileResult};
use axum::{extract::{Request, State}, http::{header, Method, StatusCode}, middleware::{self, Next}, response::{Html, IntoResponse, Response}, routing::{get, post}, Json, Router};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

struct AppState {
    result: RwLock<MultiFileResult>,
    /// None until the user sets one up (see `/api/llm/check`).
    provider: RwLock<Option<Box<dyn llm::LlmProvider>>>,
    detect: llm::Detect,
    /// Serializes deep-analysis runs (one at a time).
    deep_lock: Mutex<()>,
    /// Identifies this diff for saved analyses.
    key: String,
    /// Learn jobs running now ("outline", or a concept id while its lesson is written), and what each is
    /// doing: "writing", or "checking" while the lesson is checked against the code.
    learn_busy: std::sync::Mutex<HashMap<String, &'static str>>,
}

pub async fn serve(result: MultiFileResult, port: u16, provider: Option<Box<dyn llm::LlmProvider>>, detect: llm::Detect, open_browser: bool, key: String) {
    let state = Arc::new(AppState { result: RwLock::new(result), provider: RwLock::new(provider), detect, deep_lock: Mutex::new(()), key, learn_busy: Default::default() });

    // Use the requested port, or the next free one.
    let mut listener = None;
    for p in port..port.saturating_add(20) {
        if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", p)).await {
            listener = Some(l);
            break;
        }
    }
    let listener = match listener {
        Some(l) => l,
        None => match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("perspica: could not start web server: {e}");
                std::process::exit(1);
            }
        },
    };

    // Only this machine's own pages may talk to the server (see `local_only`).
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let allowed: Arc<Vec<String>> = Arc::new(["127.0.0.1", "localhost", "[::1]"].iter().map(|h| format!("{h}:{port}")).collect());
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/style.css", get(style_handler))
        .route("/app.js", get(js_handler))
        .route("/favicon.svg", get(favicon_handler))
        .route("/api/diff", get(api_handler))
        .route("/api/analyze", post(analyze_handler))
        .route("/api/llm/check", post(check_handler))
        .route("/api/learn", post(learn_handler))
        .route("/api/learn/status", get(learn_status_handler))
        .route("/api/learn/layout", post(learn_layout_handler))
        .layer(middleware::from_fn(move |req, next| local_only(allowed.clone(), req, next)))
        .with_state(state);
    let url = format!("http://{}", listener.local_addr().map(|a| a.to_string()).unwrap_or_default());
    eprintln!("Serving {url}  (Ctrl+C to stop)");

    if open_browser {
        let url_clone = url.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let _ = open::that(&url_clone);
        });
    }

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("perspica: server error: {e}");
    }
}

/// Reject anything that isn't the viewer itself talking to its own server:
/// - a Host other than localhost:<port> (DNS rebinding: a web page whose domain
///   now resolves to 127.0.0.1 would otherwise read the diff, file contents included);
/// - a POST from another origin, or without a JSON body (cross-site forms can't
///   send JSON without a CORS preflight, which this server never answers), so
///   no other page can start an analysis on the user's account.
async fn local_only(allowed: Arc<Vec<String>>, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    if !allowed.iter().any(|a| a == host) {
        return (StatusCode::FORBIDDEN, "perspica only answers requests for localhost").into_response();
    }
    if req.method() == Method::POST {
        let origin_ok = req.headers().get(header::ORIGIN)
            .and_then(|o| o.to_str().ok())
            .is_none_or(|o| allowed.iter().any(|a| o == format!("http://{a}")));
        let json = req.headers().get(header::CONTENT_TYPE)
            .and_then(|c| c.to_str().ok())
            .is_some_and(|c| c.starts_with("application/json"));
        if !origin_ok {
            return (StatusCode::FORBIDDEN, "cross-origin requests are not allowed").into_response();
        }
        if !json {
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "expected application/json").into_response();
        }
    }
    next.run(req).await
}

// The viewer ships inside the binary: after an upgrade the browser must not
// keep running a cached copy of the old one.
const NO_CACHE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-cache");

/// The viewer as one self-contained page, with the result embedded, for `--format html`.
pub fn export_html(result: &MultiFileResult) -> String {
    let mut v = serde_json::to_value(result).unwrap_or_default();
    // Only the model names, so the page can say which model wrote the analysis.
    v["capabilities"] = serde_json::json!({ "models": llm::claude_models() });
    // `</script>` inside a string would end the script tag early.
    let data = serde_json::to_string(&v).unwrap_or_default().replace("</", "<\\/").replace("<!--", "<\\u0021--");
    let favicon = format!("data:image/svg+xml,{}", include_str!("../web/favicon.svg").replace('#', "%23").replace('"', "'").replace('\n', " "));
    let title = match (&result.source.repo, &result.source.pr_title) {
        (Some(repo), Some(t)) => format!("{repo}: {t} · perspica"),
        _ => format!("{} · perspica", result.source.label),
    };
    include_str!("../web/index.html")
        .replace("<title>perspica</title>", &format!("<title>{}</title>", html_escape(&title)))
        .replace("href=\"/favicon.svg\"", &format!("href=\"{favicon}\""))
        .replace("<link rel=\"stylesheet\" href=\"/style.css\">", &format!("<style>\n{}\n</style>", include_str!("../web/style.css")))
        .replace("<script src=\"/app.js\"></script>", &format!(
            "<script id=\"perspica-data\" type=\"application/json\">{data}</script>\n<script>\n{}\n</script>",
            include_str!("../web/app.js").replace("</script", "<\\/script")))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

async fn index_handler() -> impl IntoResponse {
    ([NO_CACHE], Html(include_str!("../web/index.html")))
}

async fn style_handler() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8"), NO_CACHE], include_str!("../web/style.css"))
}

async fn favicon_handler() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "max-age=86400")], include_str!("../web/favicon.svg"))
}

async fn js_handler() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), NO_CACHE], include_str!("../web/app.js"))
}

async fn api_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let result = state.result.read().await;
    let mut v = serde_json::to_value(&*result).unwrap_or_default();
    drop(result);
    v["capabilities"] = capabilities(&state).await;
    Json(v)
}

/// What the Analyze button can do, or, without a provider, what the user can do to set one up.
async fn capabilities(state: &AppState) -> serde_json::Value {
    let (entries, units, chars) = {
        let result = state.result.read().await;
        let sources = result.file_sources();
        let (entries, units) = intel::llm_item_counts(&intel::IntelInput {
            results: &result.results,
            sources: &sources,
            cross_file: &result.cross_file,
            author_context: None,
            requirements: &result.source.sessions.requirements,
            budget: None,
        });
        (entries, units, intel::context_chars(&result.results, &sources))
    };
    let provider = state.provider.read().await;
    let setup = match &*provider {
        Some(_) => None,
        None => Some(llm::setup().await),
    };
    serde_json::json!({
        "llm": provider.as_ref().map(|p| p.name()),
        "model": provider.as_ref().map(|p| p.model()),
        "models": provider.as_ref().map(|p| p.model_options()).unwrap_or_default(),
        "thorough": provider.is_some(),
        "llm_setup": setup,
        "llm_entries": entries,
        "llm_units": units,
        // The dialog offers to send everything when the change is bigger than the budget.
        "context_chars": chars,
        "context_budget": provider.as_ref().map(|p| p.context_budget()),
    })
}

/// Look for a provider again: after logging in to Claude Code or starting
/// Ollama, analysis works without restarting perspica. (Environment variables
/// can't change under a running process, so a new API key still needs a restart.)
async fn check_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    if state.provider.read().await.is_none() {
        if let Some(p) = state.detect.run().await {
            *state.provider.write().await = Some(p);
        }
    }
    Json(capabilities(&state).await)
}

#[derive(Deserialize, Default)]
struct AnalyzeRequest {
    /// Model id; the provider's default when absent.
    #[serde(default)]
    model: Option<String>,
    /// "standard" (one request) or "thorough" (the model may read definitions first).
    #[serde(default)]
    depth: Option<String>,
    /// Send all of the changed code, not just the provider's budget.
    #[serde(default)]
    full: bool,
}

async fn analyze_handler(State(state): State<Arc<AppState>>, Json(req): Json<AnalyzeRequest>) -> impl IntoResponse {
    analyze(state, req).await
}

async fn analyze(state: Arc<AppState>, req: AnalyzeRequest) -> (StatusCode, Json<serde_json::Value>) {
    let deep_mode = req.depth.as_deref() == Some("thorough");
    let base = state.provider.read().await;
    let Some(base) = &*base else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
            "error": "No LLM set up yet. Log in to Claude Code, set ANTHROPIC_API_KEY or OPENAI_API_KEY, or run Ollama."
        })));
    };
    let Ok(_guard) = state.deep_lock.try_lock() else {
        return (StatusCode::CONFLICT, Json(serde_json::json!({"error": "An analysis is already running."})));
    };
    let chosen = req.model.as_deref().map(str::trim).filter(|m| !m.is_empty() && *m != base.model()).map(|m| base.with_model(m));
    let provider: &dyn llm::LlmProvider = chosen.as_deref().unwrap_or(&**base);
    let started = std::time::Instant::now();

    let outcome = {
        let result = state.result.read().await;
        let sources = result.file_sources();
        let input = intel::IntelInput {
            results: &result.results,
            sources: &sources,
            cross_file: &result.cross_file,
            author_context: result.source.author_context.as_deref(),
            requirements: &result.source.sessions.requirements,
            budget: (!req.full).then_some(provider.context_budget()),
        };
        if deep_mode {
            deep::deep_analyze(&input, provider).await
        } else {
            intel::run_analysis(&input, provider).await.map(|r| deep::DeepResult {
                groups: r.groups,
                summary: r.summary,
                concerns: r.concerns,
                tool_calls_made: 0,
                iterations: 1,
            })
        }
    };

    match outcome {
        Ok(d) => {
            let mut result = state.result.write().await;
            result.intent_groups = Some(d.groups.clone());
            result.summary = Some(d.summary.clone()).filter(|s| !s.trim().is_empty());
            result.concerns = d.concerns.clone();
            result.llm_error = None;
            result.llm_provider = Some(provider.name().to_string());
            result.llm_model = Some(provider.model().to_string());
            result.llm_saved_at = None;
            crate::saved::save(&state.key, &crate::saved::SavedAnalysis {
                provider: provider.name().to_string(),
                model: provider.model().to_string(),
                depth: if deep_mode { "thorough" } else { "standard" }.into(),
                saved_at: crate::saved::now(),
                groups: d.groups.clone(),
                summary: d.summary.clone(),
                concerns: d.concerns.clone(),
            });
            (StatusCode::OK, Json(serde_json::json!({
                "groups": d.groups,
                "summary": d.summary,
                "concerns": d.concerns,
                "tool_calls_made": d.tool_calls_made,
                "iterations": d.iterations,
                "model": provider.model(),
                "seconds": started.elapsed().as_secs(),
            })))
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e}))),
    }
}

#[derive(Deserialize, Default)]
struct LearnRequest {
    /// Model id; the provider's default when absent.
    #[serde(default)]
    model: Option<String>,
    /// Send all of the changed code, not just the provider's budget.
    #[serde(default)]
    full: bool,
    /// The concept whose lesson to write. Without it, find the concepts.
    #[serde(default)]
    concept: Option<String>,
}

/// Marks a Learn job as running until it's dropped.
struct LearnJob<'a>(&'a AppState, String);

impl LearnJob<'_> {
    fn set(&self, phase: &'static str) {
        self.0.learn_busy.lock().unwrap_or_else(|e| e.into_inner()).insert(self.1.clone(), phase);
    }
}

impl Drop for LearnJob<'_> {
    fn drop(&mut self) {
        self.0.learn_busy.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.1);
    }
}

async fn learn_handler(State(state): State<Arc<AppState>>, Json(req): Json<LearnRequest>) -> impl IntoResponse {
    learn_run(state, req).await
}

#[derive(Deserialize)]
struct LayoutRequest {
    concept: String,
    /// What the viewer's layout check found when it drew the figure.
    #[serde(default)]
    problems: Vec<String>,
    /// The figure to fix, when it's a fix of a fix. Otherwise the saved one.
    #[serde(default)]
    html: Option<String>,
    /// Save `html` as the lesson's figure instead of fixing anything. The viewer saves a fix only after its
    /// own check shows it helped. `saved_at` names the lesson it was made for.
    #[serde(default)]
    save: bool,
    #[serde(default)]
    saved_at: u64,
    #[serde(default)]
    model: Option<String>,
}

/// Fixes what the viewer's layout check found in a lesson's figure, with small edits to its HTML, or saves a fix
/// the viewer has checked. The checking pass reads the figure as text, so it can't see these.
async fn learn_layout_handler(State(state): State<Arc<AppState>>, Json(req): Json<LayoutRequest>) -> (StatusCode, Json<serde_json::Value>) {
    let fail = |code: StatusCode, msg: &str| (code, Json(serde_json::json!({ "error": msg })));
    if req.save {
        let Some(html) = req.html.filter(|h| !h.trim().is_empty()) else {
            return fail(StatusCode::BAD_REQUEST, "No figure to save.");
        };
        let mut result = state.result.write().await;
        let Some(learn) = result.learn.as_mut() else {
            return fail(StatusCode::CONFLICT, "The lesson changed while its figure was fixed.");
        };
        // The lesson may have been rewritten meanwhile; then this fix is for an old figure.
        let Some(lesson) = learn.concepts.iter_mut().find(|c| c.id == req.concept).and_then(|c| c.lesson.as_mut()).filter(|l| l.saved_at == req.saved_at) else {
            return fail(StatusCode::CONFLICT, "The lesson changed while its figure was fixed.");
        };
        let Some(figure) = lesson.interactive.as_mut() else {
            return fail(StatusCode::NOT_FOUND, "That lesson has no figure.");
        };
        figure.html = html;
        let lesson = lesson.clone();
        crate::saved::save_learn(&state.key, learn);
        return (StatusCode::OK, Json(serde_json::json!({ "concept": req.concept, "lesson": lesson })));
    }

    let problems: Vec<String> = req.problems.iter().take(12).map(|p| p.chars().take(200).collect()).collect();
    if problems.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "No layout problems were given.");
    }
    let base = state.provider.read().await;
    let Some(base) = &*base else {
        return fail(StatusCode::SERVICE_UNAVAILABLE, "No LLM set up yet.");
    };
    let job = format!("layout:{}", req.concept);
    if state.learn_busy.lock().unwrap_or_else(|e| e.into_inner()).insert(job.clone(), "layout").is_some() {
        return fail(StatusCode::CONFLICT, "That figure is already being fixed.");
    }
    let _job = LearnJob(&state, job);
    let chosen = req.model.as_deref().map(str::trim).filter(|m| !m.is_empty() && *m != base.model()).map(|m| base.with_model(m));
    let provider: &dyn llm::LlmProvider = chosen.as_deref().unwrap_or(&**base);

    let html = match req.html {
        Some(h) => h,
        None => {
            let result = state.result.read().await;
            let lesson = result.learn.as_ref().and_then(|l| l.concepts.iter().find(|c| c.id == req.concept)).and_then(|c| c.lesson.as_ref());
            match lesson.and_then(|l| l.interactive.as_ref()) {
                Some(i) => i.html.clone(),
                None => return fail(StatusCode::NOT_FOUND, "That lesson has no figure."),
            }
        }
    };
    let reply = match provider.complete(&learn::layout_prompt(&html, &problems)).await {
        Ok(r) => r,
        Err(e) => return fail(StatusCode::BAD_GATEWAY, &e),
    };
    let (fixed, used, skipped) = if reply.trim() == learn::NO_CHANGES { (html, 0, 0) } else { learn::apply_fixes(&html, &reply, |t| !t.trim().is_empty()) };
    eprintln!("perspica: suggested layout fixes for the figure: {used} applied, {skipped} skipped");
    (StatusCode::OK, Json(serde_json::json!({ "concept": req.concept, "html": fixed, "fixes": used })))
}

/// What each running Learn job is doing, so the viewer can say when a lesson is being checked.
async fn learn_status_handler(State(state): State<Arc<AppState>>) -> Json<HashMap<String, &'static str>> {
    Json(state.learn_busy.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

/// A second pass that checks a lesson against the code and fixes what's wrong. The lesson prompt
/// leaves the detailed checking to this pass. The draft is kept when the check finds nothing,
/// fails, or returns something that doesn't parse.
async fn check_lesson(provider: &dyn llm::LlmProvider, prompt: &str, draft_text: &str, draft: learn::Lesson, visual: &str) -> learn::Lesson {
    match provider.complete(&learn::verify_prompt(prompt, draft_text)).await {
        Ok(r) if r.trim() == learn::NO_CHANGES => {
            eprintln!("perspica: checked the lesson, no changes");
            draft
        }
        Ok(r) => {
            let (patched, used, skipped) = learn::apply_fixes(draft_text, &r, |t| learn::parse_lesson(t, provider.model(), visual).is_ok());
            if used == 0 {
                eprintln!("perspica: checked the lesson; kept the draft ({skipped} fixes didn't apply)");
                return draft;
            }
            match learn::parse_lesson(&patched, provider.model(), visual) {
                Ok(fixed) => {
                    eprintln!("perspica: checked the lesson: {used} fixes applied, {skipped} skipped");
                    fixed
                }
                Err(e) => {
                    eprintln!("perspica: kept the draft lesson, the fixed one didn't parse: {e}");
                    draft
                }
            }
        }
        Err(e) => {
            eprintln!("perspica: kept the draft lesson, the check failed: {e}");
            draft
        }
    }
}

async fn learn_run(state: Arc<AppState>, req: LearnRequest) -> (StatusCode, Json<serde_json::Value>) {
    let fail = |code: StatusCode, msg: &str| (code, Json(serde_json::json!({ "error": msg })));
    let base = state.provider.read().await;
    let Some(base) = &*base else {
        return fail(StatusCode::SERVICE_UNAVAILABLE, "No LLM set up yet. Log in to Claude Code, set ANTHROPIC_API_KEY or OPENAI_API_KEY, or run Ollama.");
    };
    let job = req.concept.clone().unwrap_or_else(|| "outline".into());
    // One run per job. Lessons for different concepts can be written side by side.
    {
        let mut busy = state.learn_busy.lock().unwrap_or_else(|e| e.into_inner());
        if busy.contains_key(&job) {
            return fail(StatusCode::CONFLICT, "That's already being written.");
        }
        busy.insert(job.clone(), "writing");
    }
    let _job = LearnJob(&state, job);
    let chosen = req.model.as_deref().map(str::trim).filter(|m| !m.is_empty() && *m != base.model()).map(|m| base.with_model(m));
    let provider: &dyn llm::LlmProvider = chosen.as_deref().unwrap_or(&**base);
    let started = std::time::Instant::now();

    // Build the prompt, then let go of the result while the model works,
    // so other requests (and other lessons finishing) aren't held up.
    let (prompt, paths, concept_title, visual, changed, repo_url) = {
        let result = state.result.read().await;
        let sources = result.file_sources();
        let input = intel::IntelInput {
            results: &result.results,
            sources: &sources,
            cross_file: &result.cross_file,
            author_context: result.source.author_context.as_deref(),
            requirements: &result.source.sessions.requirements,
            budget: (!req.full).then_some(provider.context_budget()),
        };
        let paths: Vec<String> = sources.iter().map(|s| s.0.clone()).collect();
        let changed = learn::changed_lines(&input);
        let repo_url = result.source.repo_url.clone();
        match &req.concept {
            None if learn::mechanical_only(&input) => (None, paths, None, String::new(), changed, repo_url),
            None => (Some(learn::outline_prompt(&input)), paths, None, String::new(), changed, repo_url),
            Some(id) => {
                let Some(learn) = &result.learn else {
                    return fail(StatusCode::BAD_REQUEST, "Find the concepts first.");
                };
                let Some(concept) = learn.concepts.iter().find(|c| &c.id == id) else {
                    return fail(StatusCode::NOT_FOUND, "That concept isn't in the current list. Reload the page.");
                };
                let others: Vec<&str> = learn.concepts.iter().filter(|c| &c.id != id).map(|c| c.title.as_str()).collect();
                (Some(learn::lesson_prompt(&input, concept, &others, repo_url.as_deref())), paths, Some(concept.title.clone()), concept.visual.clone(), changed, repo_url)
            }
        }
    };
    let response = match &prompt {
        Some(p) => match provider.complete(p).await {
            Ok(r) => Some(r),
            Err(e) => return fail(StatusCode::BAD_GATEWAY, &e),
        },
        None => None,
    };

    // A lesson's links are checked before the result is locked: the sites can be slow.
    let lesson = match (&req.concept, &response) {
        (Some(_), Some(r)) => match learn::parse_lesson(r, provider.model(), &visual) {
            Ok(draft) => {
                _job.set("checking");
                let mut l = check_lesson(provider, prompt.as_deref().unwrap_or_default(), r, draft, &visual).await;
                learn::check_sources(&mut l, repo_url.as_deref()).await;
                Some(l)
            }
            Err(e) => return fail(StatusCode::BAD_GATEWAY, &e),
        },
        _ => None,
    };
    let seconds = started.elapsed().as_secs();
    let mut result = state.result.write().await;
    match (&req.concept, response) {
        (None, response) => {
            let (nothing, concepts) = match response {
                Some(r) => match learn::parse_outline(&r, &paths, changed) {
                    Ok(parsed) => parsed,
                    Err(e) => return fail(StatusCode::BAD_GATEWAY, &e),
                },
                None => (Some(learn::NOTHING_MECHANICAL.to_string()), vec![]),
            };
            let found = learn::Learn {
                provider: provider.name().to_string(),
                model: provider.model().to_string(),
                saved_at: crate::saved::now(),
                nothing,
                concepts,
            };
            crate::saved::save_learn(&state.key, &found);
            result.learn = Some(found.clone());
            (StatusCode::OK, Json(serde_json::json!({ "learn": found, "seconds": seconds })))
        }
        (Some(id), Some(_)) => {
            let Some(lesson) = lesson else {
                return fail(StatusCode::INTERNAL_SERVER_ERROR, "No lesson was written.");
            };
            // The concepts may have been looked for again while this lesson was written.
            let Some(learn) = result.learn.as_mut() else {
                return fail(StatusCode::CONFLICT, "The concept list changed while this was written.");
            };
            let Some(concept) = learn.concepts.iter_mut().find(|c| &c.id == id && Some(&c.title) == concept_title.as_ref()) else {
                return fail(StatusCode::CONFLICT, "The concept list changed while this was written.");
            };
            concept.lesson = Some(lesson.clone());
            crate::saved::save_learn(&state.key, learn);
            (StatusCode::OK, Json(serde_json::json!({ "concept": id, "lesson": lesson, "seconds": seconds })))
        }
        (Some(_), None) => fail(StatusCode::INTERNAL_SERVER_ERROR, "No lesson was written."),
    }
}
