use crate::{deep, intel, llm, MultiFileResult};
use axum::{extract::{Request, State}, http::{header, Method, StatusCode}, middleware::{self, Next}, response::{Html, IntoResponse, Response}, routing::{get, post}, Json, Router};
use serde::Deserialize;
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
}

pub async fn serve(result: MultiFileResult, port: u16, provider: Option<Box<dyn llm::LlmProvider>>, detect: llm::Detect, open_browser: bool, key: String) {
    let state = Arc::new(AppState { result: RwLock::new(result), provider: RwLock::new(provider), detect, deep_lock: Mutex::new(()), key });

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
