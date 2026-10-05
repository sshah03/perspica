use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

/// Output cap for non-streaming requests. Grouping JSON for a large PR is long;
/// a low cap truncates it and the parse fails.
const MAX_OUTPUT_TOKENS: u32 = 16000;

/// Run `claude -p` with the prompt on stdin (argv has a length limit).
async fn run_claude(args: &[&str], prompt: &str) -> Result<String, String> {
    // Run outside the repository: Claude Code is an agent and would otherwise
    // explore the working tree with its own tools: slower, and it would read
    // more code than perspica's documented context.
    let mut child = tokio::process::Command::new("claude")
        .current_dir(std::env::temp_dir())
        .arg("-p")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run claude CLI: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await.map_err(|e| format!("Failed to write prompt: {e}"))?;
    }
    let output = child.wait_with_output().await.map_err(|e| format!("claude CLI failed: {e}"))?;
    if !output.status.success() {
        // Claude Code reports some errors (like being logged out) on stdout.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let msg = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        if is_logged_out(msg) {
            return Err("Claude Code isn't logged in. Run `claude auth login` in a terminal, then try again.".into());
        }
        return Err(format!("claude CLI error: {msg}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug)]
pub enum LlmToolResponse {
    Text(String),
    ToolCalls(String, Vec<ToolCall>), // assistant text + tool calls
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, prompt: &str) -> Result<String, String>;

    async fn complete_with_tools(
        &self,
        messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<LlmToolResponse, String> {
        // Default: flatten messages into single prompt, ignore tools
        let prompt = messages.iter()
            .map(|m| format!("[{}]: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n\n");
        let text = self.complete(&prompt).await?;
        Ok(LlmToolResponse::Text(text))
    }

    fn supports_tools(&self) -> bool { false }
    fn name(&self) -> &str;
    /// The model requests go to.
    fn model(&self) -> &str;
    /// The same provider and credentials with another model.
    fn with_model(&self, model: &str) -> Box<dyn LlmProvider>;
    /// Suggested models for this provider, for the viewer's picker. Any model
    /// id the provider accepts can still be typed in.
    fn model_options(&self) -> Vec<ModelOption> { vec![] }
    /// Characters of changed code to send by default. Local models have small context windows.
    fn context_budget(&self) -> usize { 100_000 }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub note: String,
}

impl ModelOption {
    fn new(id: &str, label: &str, note: &str) -> Self {
        ModelOption { id: id.into(), label: label.into(), note: note.into() }
    }
}

/// Default Claude model for intent grouping.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";

pub fn claude_models() -> Vec<ModelOption> {
    vec![
        ModelOption::new("claude-opus-5-5", "Claude Opus 5.5", "Strong grouping and grounded concerns (default)"),
        ModelOption::new("claude-sonnet-5-5", "Claude Sonnet 5.5", "Faster and cheaper; good for most diffs"),
        ModelOption::new("claude-haiku-4-5", "Claude Haiku 4.5", "Fastest; a rough first pass, vaguer notes"),
        ModelOption::new("claude-fable-5-1", "Claude Fable 5.1", "Most capable; slowest and most expensive"),
    ]
}

// --- Claude Code CLI ---
// Uses the user's existing Claude Code auth (Pro/Max OAuth, API key, etc.)
// by running `claude -p`, so anyone with Claude Code set up has nothing to configure.

pub struct ClaudeCodeProvider {
    model: String,
}

#[async_trait]
impl LlmProvider for ClaudeCodeProvider {
    async fn complete(&self, prompt: &str) -> Result<String, String> {
        let stdout = run_claude(&["--model", &self.model], prompt).await?;
        if stdout.trim().is_empty() {
            return Err("claude CLI returned empty output".to_string());
        }
        Ok(stdout)
    }

    async fn complete_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<LlmToolResponse, String> {
        // Use claude -p --output-format json --json-schema for structured tool calling
        let tool_desc = tools.iter()
            .map(|t| format!("- {}({}): {}", t.name,
                t.parameters["properties"].as_object()
                    .map(|p| p.keys().cloned().collect::<Vec<_>>().join(", "))
                    .unwrap_or_default(),
                t.description))
            .collect::<Vec<_>>()
            .join("\n");

        let conversation = messages.iter()
            .map(|m| format!("[{}]: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n\n");

        let prompt = format!(
            "{conversation}\n\nAvailable tools:\n{tool_desc}\n\n\
            If you need more context, set action to \"tool_call\" with the tool_name and tool_args. \
            If you have enough context for the final analysis, set action to \"final_answer\" with your answer."
        );

        let schema = r#"{"type":"object","properties":{"action":{"type":"string","enum":["tool_call","final_answer"]},"tool_name":{"type":"string"},"tool_args":{"type":"object"},"answer":{"type":"string"}},"required":["action"]}"#;

        let stdout = run_claude(&["--output-format", "json", "--json-schema", schema, "--model", &self.model], &prompt).await?;
        let parsed: serde_json::Value = serde_json::from_str(&stdout)
            .map_err(|e| format!("Failed to parse claude output: {e}"))?;

        // The structured output is in the "structured_output" field
        let structured = &parsed["structured_output"];
        let action = structured["action"].as_str().unwrap_or("final_answer");

        if action == "tool_call" {
            let tool_name = structured["tool_name"].as_str().unwrap_or("").to_string();
            let tool_args = structured["tool_args"].clone();
            Ok(LlmToolResponse::ToolCalls(String::new(), vec![ToolCall {
                id: "cc_call_0".to_string(),
                name: tool_name,
                arguments: if tool_args.is_null() { serde_json::json!({}) } else { tool_args },
            }]))
        } else {
            let answer = structured["answer"].as_str()
                .or_else(|| parsed["result"].as_str())
                .unwrap_or("")
                .to_string();
            Ok(LlmToolResponse::Text(answer))
        }
    }

    fn supports_tools(&self) -> bool { true }

    fn name(&self) -> &str {
        "Claude Code"
    }
    fn model(&self) -> &str { &self.model }
    fn with_model(&self, model: &str) -> Box<dyn LlmProvider> { Box::new(ClaudeCodeProvider { model: model.to_string() }) }
    fn model_options(&self) -> Vec<ModelOption> { claude_models() }
}

// --- Anthropic API (direct) ---

pub struct AnthropicProvider {
    api_key: String,
    model: String,
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn complete(&self, prompt: &str) -> Result<String, String> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": MAX_OUTPUT_TOKENS,
            "messages": [{"role": "user", "content": prompt}]
        });

        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Anthropic request failed: {e}"))?;

        let status = resp.status();
        let text = resp.text().await.map_err(|e| format!("Read response: {e}"))?;

        if !status.is_success() {
            return Err(format!("Anthropic API error ({status}): {text}"));
        }

        let parsed: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("Parse response: {e}"))?;
        let content = parsed["content"][0]["text"]
            .as_str()
            .ok_or("No text in Anthropic response")?;
        Ok(content.to_string())
    }

    async fn complete_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<LlmToolResponse, String> {
        let client = reqwest::Client::new();

        let api_messages: Vec<serde_json::Value> = messages.iter()
            .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
            .collect();

        let api_tools: Vec<serde_json::Value> = tools.iter()
            .map(|t| serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            }))
            .collect();

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": MAX_OUTPUT_TOKENS,
            "messages": api_messages,
            "tools": api_tools,
        });

        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Anthropic request failed: {e}"))?;

        let status = resp.status();
        let text = resp.text().await.map_err(|e| format!("Read response: {e}"))?;
        if !status.is_success() {
            return Err(format!("Anthropic API error ({status}): {text}"));
        }

        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("Parse response: {e}"))?;

        let mut result_text = String::new();
        let mut tool_calls = Vec::new();

        if let Some(content) = parsed["content"].as_array() {
            for block in content {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = block["text"].as_str() {
                            result_text.push_str(t);
                        }
                    }
                    Some("tool_use") => {
                        tool_calls.push(ToolCall {
                            id: block["id"].as_str().unwrap_or("").to_string(),
                            name: block["name"].as_str().unwrap_or("").to_string(),
                            arguments: block["input"].clone(),
                        });
                    }
                    _ => {}
                }
            }
        }

        if tool_calls.is_empty() {
            Ok(LlmToolResponse::Text(result_text))
        } else {
            Ok(LlmToolResponse::ToolCalls(result_text, tool_calls))
        }
    }

    fn supports_tools(&self) -> bool { true }

    fn name(&self) -> &str {
        "Anthropic API"
    }
    fn model(&self) -> &str { &self.model }
    fn with_model(&self, model: &str) -> Box<dyn LlmProvider> { Box::new(AnthropicProvider { api_key: self.api_key.clone(), model: model.to_string() }) }
    fn model_options(&self) -> Vec<ModelOption> { claude_models() }
}

// --- Anthropic via Bearer Token (ANTHROPIC_AUTH_TOKEN) ---
// For users routing through gateways or using tokens from Claude Code's OAuth flow.

pub struct AnthropicBearerProvider {
    token: String,
    model: String,
}

#[async_trait]
impl LlmProvider for AnthropicBearerProvider {
    async fn complete(&self, prompt: &str) -> Result<String, String> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": MAX_OUTPUT_TOKENS,
            "messages": [{"role": "user", "content": prompt}]
        });

        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("Authorization", format!("Bearer {}", self.token))
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Anthropic (bearer) request failed: {e}"))?;

        let status = resp.status();
        let text = resp.text().await.map_err(|e| format!("Read response: {e}"))?;

        if !status.is_success() {
            return Err(format!("Anthropic API error ({status}): {text}"));
        }

        let parsed: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("Parse response: {e}"))?;
        let content = parsed["content"][0]["text"]
            .as_str()
            .ok_or("No text in Anthropic (bearer) response")?;
        Ok(content.to_string())
    }

    fn name(&self) -> &str {
        "Anthropic (bearer token)"
    }
    fn model(&self) -> &str { &self.model }
    fn with_model(&self, model: &str) -> Box<dyn LlmProvider> { Box::new(AnthropicBearerProvider { token: self.token.clone(), model: model.to_string() }) }
    fn model_options(&self) -> Vec<ModelOption> { claude_models() }
}

// --- OpenAI ---

pub struct OpenAiProvider {
    api_key: String,
    model: String,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn complete(&self, prompt: &str) -> Result<String, String> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": MAX_OUTPUT_TOKENS
        });

        let resp = client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("OpenAI request failed: {e}"))?;

        let status = resp.status();
        let text = resp.text().await.map_err(|e| format!("Read response: {e}"))?;

        if !status.is_success() {
            return Err(format!("OpenAI API error ({status}): {text}"));
        }

        let parsed: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("Parse response: {e}"))?;
        let content = parsed["choices"][0]["message"]["content"]
            .as_str()
            .ok_or("No content in OpenAI response")?;
        Ok(content.to_string())
    }

    fn name(&self) -> &str {
        "OpenAI"
    }
    fn model(&self) -> &str { &self.model }
    fn with_model(&self, model: &str) -> Box<dyn LlmProvider> { Box::new(OpenAiProvider { api_key: self.api_key.clone(), model: model.to_string() }) }
}

// --- Ollama ---

/// Ollama's address: `OLLAMA_HOST` (as Ollama reads it: `host`, `host:port` or a URL), else localhost.
fn ollama_url() -> String {
    let host = std::env::var("OLLAMA_HOST").ok().filter(|h| !h.trim().is_empty());
    let Some(host) = host else { return "http://localhost:11434".into() };
    let host = host.trim().trim_end_matches('/');
    // Like Ollama: a bare host gets port 11434; a URL keeps its scheme's default port.
    let (scheme, rest, bare) = match host.split_once("://") {
        Some((scheme, rest)) => (scheme, rest, false),
        None => ("http", host, true),
    };
    // A server bound to every interface is reached on this machine.
    let rest = rest.replacen("0.0.0.0", "127.0.0.1", 1);
    let has_port = rest.rsplit_once(':').is_some_and(|(_, p)| p.chars().all(|c| c.is_ascii_digit()));
    format!("{scheme}://{rest}{}", if bare && !has_port { ":11434" } else { "" })
}
/// What the setup steps suggest pulling: good at following a JSON format, fits 16 GB of RAM.
pub const OLLAMA_SUGGESTED: &str = "gemma4:12b";
/// Largest context window perspica asks a local model for; bigger changes are refused
/// rather than silently cut off.
const OLLAMA_MAX_CTX: usize = 65536;

pub struct OllamaProvider {
    model: String,
    /// The models the user has pulled, for the viewer's picker.
    installed: Vec<ModelOption>,
}

impl OllamaProvider {
    async fn generate(&self, body: &serde_json::Value) -> Result<(reqwest::StatusCode, String), String> {
        let resp = reqwest::Client::new()
            .post(format!("{}/api/generate", ollama_url()))
            .json(body)
            .send()
            .await
            .map_err(|e| format!("Ollama request failed (is `ollama serve` running?): {e}"))?;
        let status = resp.status();
        Ok((status, resp.text().await.map_err(|e| format!("Read response: {e}"))?))
    }
}

/// Context window for a prompt: its tokens (≈ 3 characters each for code), room for
/// the answer, rounded up to 8K. Ollama's default window is only a few thousand
/// tokens and it drops whatever doesn't fit, without an error.
fn ollama_ctx(prompt: &str) -> Result<usize, String> {
    let needed = prompt.len() / 3 + MAX_OUTPUT_TOKENS as usize;
    let ctx = needed.div_ceil(8192) * 8192;
    if ctx > OLLAMA_MAX_CTX {
        return Err(format!(
            "This change is too large for a local model (about {}K tokens of context; perspica asks Ollama for at most {}K). Review a smaller range, or use Claude Code or an API key.",
            needed / 1000, OLLAMA_MAX_CTX / 1024
        ));
    }
    Ok(ctx)
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    async fn complete(&self, prompt: &str) -> Result<String, String> {
        let mut body = serde_json::json!({
            "model": self.model,
            "prompt": prompt,
            "stream": false,
            // perspica parses the answer as JSON; constrain the output to it.
            "format": "json",
            // Thinking models reason at local speeds (≈20 tokens/s): on a 10-file PR,
            // gemma4:12b thought for 10+ minutes without answering. Without it: 50 s.
            "think": false,
            "options": { "num_ctx": ollama_ctx(prompt)?, "num_predict": MAX_OUTPUT_TOKENS, "temperature": 0.2 },
        });
        let (mut status, mut text) = self.generate(&body).await?;
        // Models without a thinking mode may reject the setting.
        if !status.is_success() && text.contains("think") {
            if let Some(b) = body.as_object_mut() { b.remove("think"); }
            (status, text) = self.generate(&body).await?;
        }
        if !status.is_success() {
            if text.contains("not found") {
                return Err(format!("Ollama doesn't have {}. Run `ollama pull {}`, or pick another model.", self.model, self.model));
            }
            return Err(format!("Ollama API error ({status}): {text}"));
        }
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("Parse response: {e}"))?;
        let content = parsed["response"].as_str().ok_or("No response in Ollama output")?;
        Ok(content.to_string())
    }

    fn name(&self) -> &str { "Ollama" }
    fn model(&self) -> &str { &self.model }
    fn with_model(&self, model: &str) -> Box<dyn LlmProvider> {
        Box::new(OllamaProvider { model: model.to_string(), installed: self.installed.clone() })
    }
    fn model_options(&self) -> Vec<ModelOption> { self.installed.clone() }
    fn context_budget(&self) -> usize { 16_000 }
}

/// A pulled model, from Ollama's `/api/tags`.
#[derive(Debug, Clone)]
struct OllamaModel {
    name: String,
    /// Billions of parameters, from `details.parameter_size` ("12.2B").
    params: f64,
}

/// The pulled models that can write text (not embedding models).
fn ollama_models(tags_json: &str) -> Vec<OllamaModel> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(tags_json) else { return vec![] };
    v["models"].as_array().into_iter().flatten().filter_map(|m| {
        let name = m["name"].as_str()?.to_string();
        let family = m["details"]["family"].as_str().unwrap_or("").to_ascii_lowercase();
        if name.contains("embed") || family.contains("bert") {
            return None;
        }
        let params = m["details"]["parameter_size"].as_str()
            .and_then(|p| p.trim_end_matches(['B', 'b']).parse::<f64>().ok())
            .unwrap_or(0.0);
        Some(OllamaModel { name, params })
    }).collect()
}

/// Model families in order of preference for this task: following a JSON format
/// over a long prompt of code. Newer and code-tuned first; small models last.
const OLLAMA_PREFERRED: &[&str] = &[
    "qwen3-coder", "qwen3.8", "qwen3.6", "gemma4", "devstral", "mistral-small", "granite4", "gpt-oss", "qwen3", "gemma3", "deepseek-r1",
];

/// Below this many billions of parameters, results are rough.
const OLLAMA_SMALL: f64 = 7.0;

fn pick_ollama_model(models: &[OllamaModel]) -> Option<String> {
    let rank = |m: &OllamaModel| {
        let family = OLLAMA_PREFERRED.iter().position(|f| m.name.starts_with(f)).unwrap_or(OLLAMA_PREFERRED.len());
        // Usable size first, then family, then the larger model of a family.
        ((m.params > 0.0 && m.params < OLLAMA_SMALL) as usize, family, -(m.params * 10.0) as i64)
    };
    models.iter().min_by_key(|m| rank(m)).map(|m| m.name.clone())
}

fn ollama_options(models: &[OllamaModel]) -> Vec<ModelOption> {
    let mut sorted = models.to_vec();
    sorted.sort_by(|a, b| b.params.total_cmp(&a.params));
    sorted.iter().map(|m| {
        let note = match m.params {
            p if p <= 0.0 => "local".to_string(),
            p if p < OLLAMA_SMALL => format!("{p}B, local; small, expect rough results"),
            p => format!("{p}B, local"),
        };
        ModelOption { id: m.name.clone(), label: m.name.clone(), note }
    }).collect()
}

/// Ollama's pulled models, or None when it isn't running.
async fn ollama_installed() -> Option<Vec<OllamaModel>> {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(2)).build().ok()?;
    let resp = client.get(format!("{}/api/tags", ollama_url())).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    Some(ollama_models(&resp.text().await.ok()?))
}

/// The command-line choices provider detection starts from; kept so the viewer
/// can detect again after the user logs in to Claude Code or starts Ollama.
#[derive(Debug, Clone, Default)]
pub struct Detect {
    pub api_key: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl Detect {
    pub async fn run(&self) -> Option<Box<dyn LlmProvider>> {
        detect_provider(self.api_key.as_deref(), self.provider.as_deref(), self.model.as_deref()).await
    }
}

/// Auto-detect LLM provider in priority order.
///
/// Detection chain:
/// 1. Explicit --api-key + --provider flags
/// 2. PERSPICA_API_KEY env var (with PERSPICA_PROVIDER)
/// 3. ANTHROPIC_AUTH_TOKEN env var (bearer token, for gateways)
/// 4. ANTHROPIC_API_KEY env var (direct API key)
/// 5. OPENAI_API_KEY env var
/// 6. Claude Code CLI (`claude` on PATH with active session)
/// 7. Local Ollama on localhost:11434
/// 8. None: perspica shows its results without the LLM analysis
pub async fn detect_provider(
    api_key: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
) -> Option<Box<dyn LlmProvider>> {
    // 1. Explicit flags
    if let Some(key) = api_key {
        let prov = provider.unwrap_or("anthropic");
        return Some(make_provider(prov, key, model));
    }

    // --provider ollama needs no key: choose the local model even when Claude Code or a key is set up.
    if provider == Some("ollama") {
        let models = ollama_installed().await?;
        let best = model.map(str::to_string).or_else(|| pick_ollama_model(&models))?;
        return Some(Box::new(OllamaProvider { model: best, installed: ollama_options(&models) }));
    }

    // 2. PERSPICA_API_KEY
    if let Ok(key) = std::env::var("PERSPICA_API_KEY") {
        let prov = std::env::var("PERSPICA_PROVIDER").unwrap_or_else(|_| "anthropic".to_string());
        return Some(make_provider(&prov, &key, model));
    }

    // 3. ANTHROPIC_AUTH_TOKEN (bearer token, for LLM gateways)
    if let Ok(token) = std::env::var("ANTHROPIC_AUTH_TOKEN") {
        return Some(Box::new(AnthropicBearerProvider {
            token,
            model: model.unwrap_or(DEFAULT_CLAUDE_MODEL).to_string(),
        }));
    }

    // 4. ANTHROPIC_API_KEY (direct API key)
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        return Some(Box::new(AnthropicProvider {
            api_key: key,
            model: model.unwrap_or(DEFAULT_CLAUDE_MODEL).to_string(),
        }));
    }

    // 5. OPENAI_API_KEY
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        return Some(Box::new(OpenAiProvider {
            api_key: key,
            model: model.unwrap_or("gpt-4o-mini").to_string(),
        }));
    }

    // 6. Claude Code CLI on PATH (uses whatever login the user has)
    if claude_cli_status().await == ClaudeCli::Ready {
        return Some(Box::new(ClaudeCodeProvider {
            model: model.unwrap_or(DEFAULT_CLAUDE_MODEL).to_string(),
        }));
    }

    // 7. Local Ollama, when it has a model to run
    if let Some(models) = ollama_installed().await {
        if let Some(best) = model.map(str::to_string).or_else(|| pick_ollama_model(&models)) {
            return Some(Box::new(OllamaProvider { model: best, installed: ollama_options(&models) }));
        }
    }

    None
}

fn is_logged_out(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("not logged in") || m.contains("please run /login") || m.contains("invalid api key")
}

/// Whether the Claude Code CLI can run analyses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeCli {
    Missing,
    LoggedOut,
    Ready,
}

pub async fn claude_cli_status() -> ClaudeCli {
    let run = |args: &'static [&'static str]| tokio::process::Command::new("claude").args(args).output();
    match run(&["--version"]).await {
        Ok(o) if o.status.success() => {}
        _ => return ClaudeCli::Missing,
    }
    // `claude auth status` prints JSON (and exits 1 when logged out). Versions
    // without it can't tell us, so assume they work and let a run report otherwise.
    let Ok(o) = run(&["auth", "status"]).await else { return ClaudeCli::Ready };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) else { return ClaudeCli::Ready };
    let logged_in = v["loggedIn"].as_bool().unwrap_or(true);
    // Bedrock, Vertex and the like authenticate outside Claude Code's login.
    let third_party = v["apiProvider"].as_str().is_some_and(|p| p != "firstParty");
    if logged_in || third_party { ClaudeCli::Ready } else { ClaudeCli::LoggedOut }
}

/// What the user can do to enable the LLM analysis, when no provider was found.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Setup {
    pub claude: ClaudeCli,
    /// Ollama is running but has no model that can write text.
    pub ollama_no_model: bool,
    /// The model to suggest pulling.
    pub ollama_suggested: &'static str,
}

pub async fn setup() -> Setup {
    let ollama_no_model = ollama_installed().await.is_some_and(|m| m.is_empty());
    Setup { claude: claude_cli_status().await, ollama_no_model, ollama_suggested: OLLAMA_SUGGESTED }
}

impl Setup {
    /// Terminal version of the viewer's setup dialog.
    pub fn hint(&self) -> String {
        let claude = match self.claude {
            ClaudeCli::LoggedOut => "Claude Code is installed but not logged in: run `claude auth login`",
            _ => "Claude Code: perspica uses its login, no API key needed. Install it and run `claude auth login`",
        };
        format!(
            "No LLM found for the optional analysis (intent groups, risk, a summary). To enable it:\n  \
             - {claude}\n  \
             - or add `export ANTHROPIC_API_KEY=…` (or OPENAI_API_KEY) to your shell profile, e.g. ~/.zshrc\n  \
             - or run a local model with Ollama: `ollama pull {}`{}",
            OLLAMA_SUGGESTED,
            if self.ollama_no_model { " (Ollama is running but has no model yet)" } else { "" },
        )
    }
}

fn make_provider(provider: &str, key: &str, model: Option<&str>) -> Box<dyn LlmProvider> {
    match provider {
        "openai" => Box::new(OpenAiProvider {
            api_key: key.to_string(),
            model: model.unwrap_or("gpt-4o-mini").to_string(),
        }),
        "ollama" => Box::new(OllamaProvider { model: model.unwrap_or(OLLAMA_SUGGESTED).to_string(), installed: vec![] }),
        _ => Box::new(AnthropicProvider {
            api_key: key.to_string(),
            model: model.unwrap_or(DEFAULT_CLAUDE_MODEL).to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logged_out_messages() {
        // What `claude -p` prints on stdout when logged out.
        assert!(is_logged_out("Not logged in · Please run /login"));
        assert!(is_logged_out("Invalid API key · Please run /login"));
        assert!(!is_logged_out("Error: rate limited, try again later"));
    }

    #[test]
    fn setup_hint_names_the_fix() {
        let setup = |claude| Setup { claude, ollama_no_model: false, ollama_suggested: OLLAMA_SUGGESTED };
        assert!(setup(ClaudeCli::LoggedOut).hint().contains("claude auth login"));
        assert!(setup(ClaudeCli::Missing).hint().contains("ANTHROPIC_API_KEY"));
    }

    fn tags(models: &[(&str, &str, &str)]) -> String {
        let ms: Vec<_> = models.iter().map(|(n, fam, size)| serde_json::json!({"name": n, "details": {"family": fam, "parameter_size": size}})).collect();
        serde_json::json!({ "models": ms }).to_string()
    }

    #[test]
    fn ollama_picks_a_capable_model() {
        // A usable model of a preferred family beats a small or older one.
        let m = ollama_models(&tags(&[("llama3.2:latest", "llama", "3.2B"), ("gemma4:e2b", "gemma4", "5.1B"), ("gemma4:12b", "gemma4", "12.2B")]));
        assert_eq!(pick_ollama_model(&m).as_deref(), Some("gemma4:12b"));
        let m = ollama_models(&tags(&[("gemma4:12b", "gemma4", "12.2B"), ("qwen3-coder:30b", "qwen3moe", "30.5B")]));
        assert_eq!(pick_ollama_model(&m).as_deref(), Some("qwen3-coder:30b"));
        // Only small ones: still usable, the preferred family first.
        let m = ollama_models(&tags(&[("llama3.2:latest", "llama", "3.2B"), ("gemma4:e2b", "gemma4", "5.1B")]));
        assert_eq!(pick_ollama_model(&m).as_deref(), Some("gemma4:e2b"));
        // Embedding models can't write the analysis.
        let m = ollama_models(&tags(&[("nomic-embed-text:latest", "nomic-bert", "137M")]));
        assert!(m.is_empty() && pick_ollama_model(&m).is_none());
    }

    #[test]
    fn ollama_host() {
        let url = |h: Option<&str>| {
            match h { Some(h) => std::env::set_var("OLLAMA_HOST", h), None => std::env::remove_var("OLLAMA_HOST") }
            ollama_url()
        };
        assert_eq!(url(None), "http://localhost:11434");
        assert_eq!(url(Some("0.0.0.0")), "http://127.0.0.1:11434");
        assert_eq!(url(Some("gpu-box:8080")), "http://gpu-box:8080");
        assert_eq!(url(Some("https://ollama.example.com/")), "https://ollama.example.com");
        std::env::remove_var("OLLAMA_HOST");
    }

    #[test]
    fn ollama_context_fits_the_prompt() {
        assert_eq!(ollama_ctx("x").unwrap(), 16384);
        assert_eq!(ollama_ctx(&"x".repeat(60_000)).unwrap(), 40960);
        assert!(ollama_ctx(&"x".repeat(400_000)).is_err());
    }
}
