use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use futures_util::StreamExt;

// ── Config ──────────────────────────────────────────────────────────────────

pub const OLLAMA_URL: &str = "http://0.0.0.0:8000/v1/chat/completions";
pub const OLLAMA_MODEL: &str = "gemma-4-31b-it";

// Tool definition the GUI LLM can call when it needs system-level tasks
pub fn delegate_tool_definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "delegate_to_daemon",
            "description": "Delegate a task to the ARIA system daemon for execution. Use this when
the user needs to interact with the local file system (type: 'fs'), search the web
(type: 'web'), run OS commands (type: 'os'), or send a Hedera/HBAR payment or make an
x402 payment (type: 'pay'). ARIA CAN send real payments via a governed daemon — do not
refuse payment requests or tell the user to use an external wallet; always delegate
payment tasks with type: 'pay' and let the daemon handle allowlist/cap/approval checks.
Always infer the correct type from context",
            "parameters": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "A clear, detailed description of the task to execute."
                    },
                    "type": {
                        "type": "string",
                        "enum": ["fs", "web", "os", "pay"],
                        "description": "The skill category: 'fs' for file system, 'web' for web
        search/browse, 'os' for OS-level commands, 'pay' for Hedera or x402 payments."
                    }
                },
                "required": ["task", "type"]
            }
        }
    })
}

// Lets the router ask a clarifying question instead of guessing a task
// description/type before delegating, or answering as if it already knew
// what the user meant. Previously the router had exactly two moves —
// delegate, or answer in plain text — with nothing in between.
pub const ASK_TOOL_NAME: &str = "ask_user";

pub fn ask_tool_definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": ASK_TOOL_NAME,
            "description": "Ask the user a clarifying question before responding or delegating,
when the request is ambiguous or you're missing information needed to answer or to fill in
delegate_to_daemon's task/type correctly. Prefer this over guessing.",
            "parameters": {
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question to ask the user, in plain language."
                    }
                },
                "required": ["question"]
            }
        }
    })
}

// ── Request / Response Types ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: Some(content.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn assistant_text(content: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: Some(content.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

/// Result from a single streaming LLM call
#[derive(Debug)]
pub enum LlmStreamResult {
    /// LLM emitted a tool call - needs delegation to daemon
    ToolCall { id: String, name: String, arguments: String },
    /// LLM called ask_user - needs to pause and ask the user something
    /// before continuing, instead of guessing.
    Ask { question: String },
    /// LLM finished a normal text response
    TextDone { full_text: String },
}

// ── Streaming LLM Call ────────────────────────────────────────────────────────

const SYSTEM_PROMPT: &str = "You are ARIA. For system tasks (files, web, OS commands, payments) \
call delegate_to_daemon. For multi-step tasks, perform only one step at a time; you will see the outcome before starting the next step. \
If the request is ambiguous or you're missing information needed to \
answer or to fill in delegate_to_daemon's task/type correctly, call ask_user instead of guessing. \
Otherwise just reply normally.";

fn system_prefixed(history: &[ChatMessage]) -> Vec<Value> {
    let mut messages = vec![json!({ "role": "system", "content": SYSTEM_PROMPT })];
    messages.extend(history.iter().map(|m| serde_json::to_value(m).unwrap_or(Value::Null)));
    messages
}

/// Stream a chat completion. Emits tokens via `on_token` callback.
/// Returns LlmStreamResult indicating whether the LLM finished with text or a tool call.
pub async fn stream_chat<F>(
    client: &Client,
    messages: &[ChatMessage],
    on_token: F,
) -> Result<LlmStreamResult, String>
where
    F: Fn(String) + Send + Sync,
{
    let body = json!({
        "model": OLLAMA_MODEL,
        "messages": system_prefixed(messages),
        "tools": [delegate_tool_definition(), ask_tool_definition()],
        "stream": true,
    });

    let response = client
        .post(OLLAMA_URL)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("LLM request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("LLM error {status}: {text}"));
    }

    let mut stream = response.bytes_stream();
    let mut full_text = String::new();

    // Accumulated tool call fields from stream deltas
    let mut tool_call_id = String::new();
    let mut tool_call_name = String::new();
    let mut tool_call_args = String::new();
    let mut is_tool_call = false;

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| format!("Stream error: {e}"))?;
        let raw = String::from_utf8_lossy(&bytes);

        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line == "data: [DONE]" {
                continue;
            }
            let data = line.strip_prefix("data: ").unwrap_or(line);
            let Ok(json): Result<Value, _> = serde_json::from_str(data) else {
                continue;
            };

            let delta = &json["choices"][0]["delta"];

            // Handle tool calls in stream
            if let Some(tool_calls) = delta["tool_calls"].as_array() {
                is_tool_call = true;
                for tc in tool_calls {
                    let idx = tc["index"].as_i64().unwrap_or(0);
                    if idx == 0 {
                        if let Some(id) = tc["id"].as_str() {
                            if !id.is_empty() { tool_call_id = id.to_string(); }
                        }
                        if let Some(name) = tc["function"]["name"].as_str() {
                            if !name.is_empty() { tool_call_name = name.to_string(); }
                        }
                        if let Some(args) = tc["function"]["arguments"].as_str() {
                            tool_call_args.push_str(args);
                        }
                    }
                }
                continue;
            }

            // Handle text token
            if let Some(content) = delta["content"].as_str() {
                if !content.is_empty() {
                    full_text.push_str(content);
                    on_token(content.to_string());
                }
            }
        }
    }

    if is_tool_call && !tool_call_name.is_empty() {
        if tool_call_name == ASK_TOOL_NAME {
            let question = serde_json::from_str::<Value>(&tool_call_args)
                .ok()
                .and_then(|v| v.get("question").and_then(|q| q.as_str()).map(str::to_string))
                .unwrap_or_else(|| "(no question provided)".to_string());
            return Ok(LlmStreamResult::Ask { question });
        }
        Ok(LlmStreamResult::ToolCall {
            id: tool_call_id,
            name: tool_call_name,
            arguments: tool_call_args,
        })
    } else {
        Ok(LlmStreamResult::TextDone { full_text })
    }
}
