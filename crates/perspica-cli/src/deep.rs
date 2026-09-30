use crate::llm::{LlmProvider, LlmToolResponse, Message, ToolCall, ToolSchema};
use crate::intel;
use serde::{Deserialize, Serialize};

const MAX_ITERATIONS: usize = 5;
const MAX_TOOL_CALLS: usize = 10;
const MAX_LINES_PER_RESPONSE: usize = 300;

#[derive(Debug, Serialize, Deserialize)]
pub struct DeepResult {
    pub groups: Vec<intel::IntentGroup>,
    pub summary: String,
    #[serde(default)]
    pub concerns: Vec<String>,
    pub tool_calls_made: usize,
    pub iterations: usize,
}

/// Run deep analysis: a bounded tool-calling loop that lets the LLM explore the
/// changed files. Providers without tool calling get a single call with extra context.
pub async fn deep_analyze(input: &intel::IntelInput<'_>, provider: &dyn LlmProvider) -> Result<DeepResult, String> {
    let file_sources = input.sources;
    if !provider.supports_tools() {
        return deep_analyze_single_shot(input, provider).await;
    }

    let tools = build_tool_schemas();
    let explore = "You may use the available tools to explore the changed files when the context above is not \
        enough to understand intent. Do NOT explore exhaustively; focus on changes whose purpose is unclear. \
        When you have enough understanding, respond with the final JSON.";
    let mut messages = vec![Message { role: "user".into(), content: intel::build_prompt(input, Some(explore)) }];

    let mut total_tool_calls = 0usize;
    let mut iterations = 0usize;

    for _ in 0..MAX_ITERATIONS {
        iterations += 1;
        match provider.complete_with_tools(&messages, &tools).await? {
            LlmToolResponse::Text(text) => {
                return finish(&text, input, total_tool_calls, iterations);
            }
            LlmToolResponse::ToolCalls(assistant_text, calls) => {
                let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                // The API rejects empty assistant turns.
                let assistant_text = if assistant_text.trim().is_empty() {
                    format!("(calling tools: {})", names.join(", "))
                } else {
                    assistant_text
                };
                messages.push(Message { role: "assistant".into(), content: assistant_text });
                if total_tool_calls + calls.len() > MAX_TOOL_CALLS {
                    messages.push(Message {
                        role: "user".into(),
                        content: "You have reached the tool call limit. Provide your final JSON analysis now.".into(),
                    });
                    continue;
                }
                let mut tool_results = Vec::new();
                for call in &calls {
                    let result = execute_tool(call, file_sources);
                    tool_results.push(format!("[Tool: {}] {}", call.name, truncate_lines(&result, MAX_LINES_PER_RESPONSE)));
                    total_tool_calls += 1;
                    eprint!("  → {} ", call.name);
                }
                eprintln!();
                messages.push(Message { role: "user".into(), content: format!("Tool results:\n{}", tool_results.join("\n\n")) });
            }
        }
    }

    // Iteration limit: one last request for the answer without tools.
    messages.push(Message { role: "user".into(), content: "Stop exploring and provide your final JSON analysis now.".into() });
    let prompt = messages.iter().map(|m| format!("[{}]: {}", m.role, m.content)).collect::<Vec<_>>().join("\n\n");
    let text = provider.complete(&prompt).await?;
    finish(&text, input, total_tool_calls, iterations + 1)
}

fn finish(text: &str, input: &intel::IntelInput<'_>, tool_calls: usize, iterations: usize) -> Result<DeepResult, String> {
    let r = intel::parse_and_validate(text, input)?;
    Ok(DeepResult { groups: r.groups, summary: r.summary, concerns: r.concerns, tool_calls_made: tool_calls, iterations })
}

/// Single-shot deep analysis for providers without tool calling: file structures
/// and full sources of small files up front.
async fn deep_analyze_single_shot(input: &intel::IntelInput<'_>, provider: &dyn LlmProvider) -> Result<DeepResult, String> {
    let mut extra = String::from("FILE STRUCTURES (changed files):\n");
    for (path, _old, new_src) in input.sources {
        let structure = file_structure(path, new_src);
        if !structure.is_empty() {
            extra.push_str(&format!("--- {path} ---\n{structure}\n"));
        }
    }
    let mut budget = 20000i64;
    extra.push_str("\nFULL SOURCE OF SMALL FILES:\n");
    for (path, _old, new_src) in input.sources {
        let n = new_src.lines().count();
        if n > 0 && n <= 100 && budget > 0 {
            let entry = format!("=== {path} ({n} lines) ===\n{new_src}\n");
            budget -= entry.len() as i64;
            extra.push_str(&entry);
        }
    }
    let text = provider.complete(&intel::build_prompt(input, Some(&extra))).await?;
    finish(&text, input, 0, 1)
}

fn file_structure(path: &str, src: &str) -> String {
    use perspica_core::parser::SemanticItem;
    let lang = perspica_core::Language::from_path(path);
    if lang == perspica_core::Language::Unknown {
        return String::new();
    }
    let support = perspica_core::languages::get_language_support(lang);
    let Ok(tree) = perspica_core::parser::parse(src, &*support) else { return String::new() };
    tree.items.iter()
        .filter_map(|item| {
            let kind = match item {
                SemanticItem::Function { .. } => "fn",
                SemanticItem::Class { .. } => "class",
                SemanticItem::Import { .. } => "import",
                SemanticItem::Variable { .. } => "var",
                SemanticItem::TypeDef { .. } => "type",
                SemanticItem::Other { .. } => return None,
            };
            Some(format!("  {} {} (lines {}-{})", kind, item.name().unwrap_or("?"), item.span().start_line, item.span().end_line))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_tool_schemas() -> Vec<ToolSchema> {
    vec![
        ToolSchema {
            name: "get_definition".into(),
            description: "Get the full source code of a function, class, or type by name and file path.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The name of the function/class/type"},
                    "file": {"type": "string", "description": "The file path"}
                },
                "required": ["name", "file"]
            }),
        },
        ToolSchema {
            name: "find_references".into(),
            description: "Find all locations where a symbol name is used across changed files.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The symbol name to search for"}
                },
                "required": ["name"]
            }),
        },
        ToolSchema {
            name: "get_file_structure".into(),
            description: "Get the list of top-level items (functions, classes, types) in a file.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string", "description": "The file path"}
                },
                "required": ["file"]
            }),
        },
        ToolSchema {
            name: "get_file_source".into(),
            description: "Get the full source of a file (only for files under 200 lines).".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string", "description": "The file path"},
                    "version": {"type": "string", "description": "old or new", "enum": ["old", "new"]}
                },
                "required": ["file"]
            }),
        },
    ]
}

fn execute_tool(call: &ToolCall, sources: &[(String, String, String)]) -> String {
    match call.name.as_str() {
        "get_definition" => {
            let name = call.arguments["name"].as_str().unwrap_or("");
            let file = call.arguments["file"].as_str().unwrap_or("");
            // Find the file and search for the definition
            if let Some((path, _old, new_src)) = find_source(sources, file) {
                {
                    let lang = perspica_core::Language::from_path(path);
                    let support = perspica_core::languages::get_language_support(lang);
                    if let Ok(tree) = perspica_core::parser::parse(new_src, &*support) {
                        for item in &tree.items {
                            if item.name() == Some(name) || item.name().map(perspica_core::parser::bare_name) == Some(name) {
                                let lines: Vec<&str> = new_src.lines().collect();
                                let start = item.span().start_line.saturating_sub(1);
                                let end = item.span().end_line.min(lines.len());
                                return lines[start..end].join("\n");
                            }
                        }
                    }
                    return format!("Symbol '{}' not found in {}", name, path);
                }
            }
            format!("File '{}' not found in diff", file)
        }
        "find_references" => {
            let name = call.arguments["name"].as_str().unwrap_or("");
            let mut refs = Vec::new();
            for (path, _old, new_src) in sources {
                for (i, line) in new_src.lines().enumerate() {
                    if perspica_core::classify::contains_identifier(line, name) {
                        refs.push(format!("{}:{}: {}", path, i + 1, line.trim()));
                    }
                }
            }
            if refs.is_empty() {
                format!("No references to '{}' found", name)
            } else {
                refs.join("\n")
            }
        }
        "get_file_structure" => {
            let file = call.arguments["file"].as_str().unwrap_or("");
            match find_source(sources, file) {
                Some((path, _, new_src)) => {
                    let s = file_structure(path, new_src);
                    if s.is_empty() { format!("{path} is not a parsed language") } else { s }
                }
                None => format!("File '{}' not found in diff", file),
            }
        }
        "get_file_source" => {
            let file = call.arguments["file"].as_str().unwrap_or("");
            let version = call.arguments["version"].as_str().unwrap_or("new");
            if let Some((_path, old_src, new_src)) = find_source(sources, file) {
                {
                    let src = if version == "old" { old_src } else { new_src };
                    if src.lines().count() > 200 {
                        return format!("File too large ({} lines). Use get_definition for specific items.", src.lines().count());
                    }
                    return src.clone();
                }
            }
            format!("File '{}' not found", file)
        }
        _ => format!("Unknown tool: {}", call.name),
    }
}

/// Exact path match first, then suffix match (the model often drops leading dirs).
fn find_source<'a>(sources: &'a [(String, String, String)], file: &str) -> Option<&'a (String, String, String)> {
    if file.is_empty() { return None; }
    sources.iter().find(|s| s.0 == file)
        .or_else(|| sources.iter().find(|s| s.0.ends_with(&format!("/{file}"))))
        .or_else(|| sources.iter().find(|s| file.ends_with(&format!("/{}", s.0))))
}

fn truncate_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        text.to_string()
    } else {
        let mut result: Vec<&str> = lines[..max_lines].to_vec();
        result.push("[truncated]");
        result.join("\n")
    }
}
