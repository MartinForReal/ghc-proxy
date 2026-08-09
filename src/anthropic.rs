//! Translation between the Anthropic Messages API and the OpenAI Chat
//! Completions API, in both directions, plus streaming conversion.

use crate::config::Config;
use crate::filters::{strip_system_prompt, strip_tool_result_suffix};
use crate::translate;
use serde_json::{json, Map, Value};

fn arr(v: &Value, key: &str) -> Vec<Value> {
    v.get(key)
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default()
}

fn type_of(block: &Value) -> &str {
    block.get("type").and_then(|t| t.as_str()).unwrap_or("")
}

/// Builds an OpenAI chat-completions request body from an Anthropic Messages
/// request body (mirrors ghc-tunnel's translation path).
pub fn anthropic_to_openai(req: &Value, cfg: &Config) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    // System prompt.
    match req.get("system") {
        Some(Value::String(s)) => {
            messages.push(json!({
                "role": "system",
                "content": strip_system_prompt(s, &cfg.system_prompt_remove)
            }));
        }
        Some(Value::Array(blocks)) => {
            let texts: Vec<String> = blocks
                .iter()
                .filter(|b| {
                    type_of(b) == "text"
                        && !b
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .contains("x-anthropic-billing-header")
                })
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(String::from))
                .collect();
            if !texts.is_empty() {
                messages.push(json!({
                    "role": "system",
                    "content": strip_system_prompt(&texts.join("\n\n"), &cfg.system_prompt_remove)
                }));
            }
        }
        _ => {}
    }

    for msg in arr(req, "messages") {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = msg.get("content").cloned().unwrap_or(Value::Null);
        match role {
            "user" => {
                if let Value::Array(blocks) = &content {
                    let tool_results: Vec<&Value> = blocks
                        .iter()
                        .filter(|b| type_of(b) == "tool_result")
                        .collect();
                    let others: Vec<Value> = blocks
                        .iter()
                        .filter(|b| type_of(b) != "tool_result")
                        .cloned()
                        .collect();
                    for tr in tool_results {
                        let c = normalize_tool_result_content(
                            tr.get("content"),
                            &cfg.tool_result_suffix_remove,
                        );
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": tr.get("tool_use_id").cloned().unwrap_or(Value::Null),
                            "content": c
                        }));
                    }
                    if !others.is_empty() {
                        if let Some(c) = extract_user_content(&others) {
                            messages.push(json!({"role": "user", "content": c}));
                        }
                    }
                } else {
                    messages.push(json!({"role": "user", "content": content}));
                }
            }
            "assistant" => {
                if let Value::Array(blocks) = &content {
                    let tool_uses: Vec<&Value> =
                        blocks.iter().filter(|b| type_of(b) == "tool_use").collect();
                    let text: String = blocks
                        .iter()
                        .filter(|b| matches!(type_of(b), "text" | "thinking"))
                        .map(|b| {
                            let key = if type_of(b) == "text" {
                                "text"
                            } else {
                                "thinking"
                            };
                            b.get(key)
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string()
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    if !tool_uses.is_empty() {
                        let tool_calls: Vec<Value> = tool_uses
                            .iter()
                            .map(|u| {
                                json!({
                                    "id": u.get("id").cloned().unwrap_or(Value::Null),
                                    "type": "function",
                                    "function": {
                                        "name": u.get("name").cloned().unwrap_or(Value::Null),
                                        "arguments": serde_json::to_string(
                                            u.get("input").unwrap_or(&json!({}))
                                        ).unwrap_or_else(|_| "{}".to_string())
                                    }
                                })
                            })
                            .collect();
                        messages.push(json!({
                            "role": "assistant",
                            "content": if text.is_empty() { Value::Null } else { Value::String(text) },
                            "tool_calls": tool_calls
                        }));
                    } else {
                        messages.push(json!({"role": "assistant", "content": text}));
                    }
                } else {
                    messages.push(json!({"role": "assistant", "content": content}));
                }
            }
            _ => {}
        }
    }

    let model = req.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let translated = translate::translate(&cfg.model_mappings, model);
    let mut out = Map::new();
    out.insert("model".into(), Value::String(translated));
    out.insert("messages".into(), Value::Array(messages));
    if let Some(mt) = req.get("max_tokens") {
        out.insert("max_tokens".into(), mt.clone());
    }
    out.insert(
        "stream".into(),
        Value::Bool(req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false)),
    );
    if let Some(v) = req.get("temperature") {
        if !v.is_null() {
            out.insert("temperature".into(), v.clone());
        }
    }
    if let Some(v) = req.get("top_p") {
        if !v.is_null() {
            out.insert("top_p".into(), v.clone());
        }
    }
    if let Some(v) = req.get("stop_sequences") {
        out.insert("stop".into(), v.clone());
    }
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let mapped: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").cloned().unwrap_or(Value::Null),
                        "description": t.get("description").cloned().unwrap_or(Value::Null),
                        "parameters": t.get("input_schema").cloned().unwrap_or(json!({}))
                    }
                })
            })
            .collect();
        out.insert("tools".into(), Value::Array(mapped));
    }
    if let Some(tc) = req.get("tool_choice") {
        let t = tc.get("type").and_then(|x| x.as_str()).unwrap_or("");
        match t {
            "auto" => {
                out.insert("tool_choice".into(), Value::String("auto".into()));
            }
            "any" => {
                out.insert("tool_choice".into(), Value::String("required".into()));
            }
            "none" => {
                out.insert("tool_choice".into(), Value::String("none".into()));
            }
            "tool" => {
                if let Some(name) = tc.get("name") {
                    out.insert(
                        "tool_choice".into(),
                        json!({"type": "function", "function": {"name": name}}),
                    );
                }
            }
            _ => {}
        }
    }

    Value::Object(out)
}

/// Normalizes the `content` of an Anthropic `tool_result` block into something
/// the OpenAI chat-completions API accepts on a `tool` message.
///
/// Anthropic allows `content` to be either a plain string or an array of
/// content blocks, and MCP servers routinely return the array form (a
/// screenshot, or text plus an image). Forwarding that array unchanged makes
/// the upstream reject the whole request with
/// `type has to be either 'image_url' or 'text'`, so every MCP tool that
/// returns anything but a bare string fails. Text blocks are joined and image
/// blocks are rewritten as `image_url` data URLs; a pure-text result collapses
/// back to a plain string, which is what the API expects most of the time.
fn normalize_tool_result_content(content: Option<&Value>, suffixes: &[String]) -> Value {
    match content {
        None | Some(Value::Null) => Value::String(String::new()),
        Some(Value::String(s)) => Value::String(strip_tool_result_suffix(s, suffixes)),
        Some(Value::Array(blocks)) => {
            let has_image = blocks.iter().any(|b| type_of(b) == "image");
            if !has_image {
                let text = blocks
                    .iter()
                    .filter_map(|b| match type_of(b) {
                        "text" => b.get("text").and_then(|t| t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return Value::String(strip_tool_result_suffix(&text, suffixes));
            }
            let mut out: Vec<Value> = Vec::new();
            for b in blocks {
                match type_of(b) {
                    "text" => {
                        let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        out.push(json!({
                            "type": "text",
                            "text": strip_tool_result_suffix(text, suffixes)
                        }));
                    }
                    "image" => out.push(image_block_to_image_url(b)),
                    _ => {}
                }
            }
            Value::Array(out)
        }
        // Anything else (a number, an object) is forwarded as a JSON string so
        // the upstream still receives valid `tool` content.
        Some(other) => Value::String(other.to_string()),
    }
}

/// Converts an Anthropic `image` content block into an OpenAI `image_url` part.
/// Both the `base64` and `url` source forms are supported.
fn image_block_to_image_url(block: &Value) -> Value {
    let src = block.get("source");
    let source_type = src
        .and_then(|s| s.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("base64");
    if source_type == "url" {
        let url = src
            .and_then(|s| s.get("url"))
            .and_then(|u| u.as_str())
            .unwrap_or("");
        return json!({"type": "image_url", "image_url": {"url": url}});
    }
    let media = src
        .and_then(|s| s.get("media_type"))
        .and_then(|m| m.as_str())
        .unwrap_or("image/png");
    let data = src
        .and_then(|s| s.get("data"))
        .and_then(|d| d.as_str())
        .unwrap_or("");
    json!({
        "type": "image_url",
        "image_url": {"url": format!("data:{media};base64,{data}")}
    })
}

/// Extracts the OpenAI `content` value (string or multimodal array) from a set
/// of Anthropic user content blocks.
fn extract_user_content(blocks: &[Value]) -> Option<Value> {
    let has_image = blocks.iter().any(|b| type_of(b) == "image");
    if !has_image {
        let parts: Vec<String> = blocks
            .iter()
            .filter_map(|b| match type_of(b) {
                "text" => Some(
                    b.get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                "thinking" => Some(
                    b.get("thinking")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                _ => None,
            })
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(Value::String(parts.join("\n\n")))
        }
    } else {
        let mut out: Vec<Value> = Vec::new();
        for b in blocks {
            match type_of(b) {
                "text" => out.push(json!({"type": "text", "text": b.get("text")})),
                "thinking" => out.push(json!({"type": "text", "text": b.get("thinking")})),
                "image" => out.push(image_block_to_image_url(b)),
                _ => {}
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(Value::Array(out))
        }
    }
}

/// Maps an OpenAI finish reason to an Anthropic stop reason.
pub fn map_finish_reason(reason: Option<&str>) -> Value {
    match reason {
        Some("stop") => Value::String("end_turn".into()),
        Some("length") => Value::String("max_tokens".into()),
        Some("tool_calls") => Value::String("tool_use".into()),
        Some("content_filter") => Value::String("refusal".into()),
        _ => Value::Null,
    }
}

/// Converts an OpenAI chat-completion response into an Anthropic message
/// response.
pub fn openai_to_anthropic(resp: &Value) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let mut finish: Option<String> = None;
    for choice in arr(resp, "choices") {
        if let Some(message) = choice.get("message") {
            if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    content.push(json!({"type": "text", "text": text}));
                }
            }
            if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tool_calls {
                    let func = tc.get("function");
                    let args = func
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                        .unwrap_or("{}");
                    let input: Value = serde_json::from_str(args)
                        .unwrap_or_else(|_| json!({"_raw_arguments": args}));
                    content.push(json!({
                        "type": "tool_use",
                        "id": tc.get("id").cloned().unwrap_or(Value::Null),
                        "name": func.and_then(|f| f.get("name")).cloned().unwrap_or(Value::Null),
                        "input": input
                    }));
                }
            }
        }
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            finish = Some(fr.to_string());
        }
    }

    let usage = resp.get("usage").cloned().unwrap_or(json!({}));
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|c| c.as_u64())
        .unwrap_or(0);
    let prompt = usage
        .get("prompt_tokens")
        .and_then(|p| p.as_u64())
        .unwrap_or(0);
    let input_tokens = prompt.saturating_sub(cached);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(|c| c.as_u64())
        .unwrap_or(0);

    let mut usage_out = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens
    });
    if cached > 0 {
        usage_out["cache_read_input_tokens"] = json!(cached);
    }

    json!({
        "id": resp.get("id").cloned().unwrap_or_else(|| Value::String(uuid::Uuid::new_v4().to_string())),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": resp.get("model").cloned().unwrap_or_else(|| Value::String(String::new())),
        "stop_reason": map_finish_reason(finish.as_deref()),
        "stop_sequence": Value::Null,
        "usage": usage_out
    })
}

/// Merges a sequence of streamed OpenAI chat-completion chunks into a single
/// non-streaming chat-completion object.
pub fn merge_chat_chunks(chunks: &[Value]) -> Value {
    if chunks.is_empty() {
        return json!({});
    }
    let first = &chunks[0];
    let mut content = String::new();
    // index -> (id, name, arguments)
    let mut tool_calls: std::collections::BTreeMap<i64, (String, String, String)> =
        std::collections::BTreeMap::new();
    let mut finish: Option<String> = None;
    let mut usage = json!({});

    for chunk in chunks {
        if let Some(u) = chunk.get("usage") {
            if !u.is_null() {
                usage = u.clone();
            }
        }
        for choice in arr(chunk, "choices") {
            if let Some(delta) = choice.get("delta") {
                if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
                    content.push_str(c);
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                        let entry = tool_calls.entry(idx).or_default();
                        if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                            entry.0 = id.to_string();
                        }
                        if let Some(func) = tc.get("function") {
                            if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                                entry.1 = name.to_string();
                            }
                            if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                                entry.2.push_str(args);
                            }
                        }
                    }
                }
            }
            if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                finish = Some(fr.to_string());
            }
        }
    }

    let mut message = Map::new();
    message.insert("role".into(), Value::String("assistant".into()));
    message.insert(
        "content".into(),
        if content.is_empty() {
            Value::Null
        } else {
            Value::String(content)
        },
    );
    if !tool_calls.is_empty() {
        let calls: Vec<Value> = tool_calls
            .values()
            .map(|(id, name, args)| {
                json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": args}
                })
            })
            .collect();
        message.insert("tool_calls".into(), Value::Array(calls));
    }

    json!({
        "id": first.get("id").cloned().unwrap_or_else(|| Value::String(String::new())),
        "object": "chat.completion",
        "created": first.get("created").cloned().unwrap_or(json!(0)),
        "model": first.get("model").cloned().unwrap_or_else(|| Value::String(String::new())),
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish
        }],
        "usage": usage
    })
}

/// Streaming state used to convert OpenAI chat-completion SSE chunks into
/// Anthropic Messages SSE events.
#[derive(Default)]
pub struct AnthropicStreamState {
    message_start_sent: bool,
    content_block_index: i64,
    content_block_open: bool,
    /// Set once a terminating `message_stop` has been emitted.
    finished: bool,
    /// OpenAI tool-call index -> anthropic content block index.
    tool_calls: std::collections::HashMap<i64, i64>,
}

impl AnthropicStreamState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Processes one OpenAI streaming chunk and returns the Anthropic SSE
    /// events to emit.
    pub fn process(&mut self, chunk: &Value) -> Vec<Value> {
        let mut events: Vec<Value> = Vec::new();
        let choices = arr(chunk, "choices");
        if choices.is_empty() {
            return events;
        }
        let choice = &choices[0];
        let delta = choice.get("delta").cloned().unwrap_or(json!({}));

        if !self.message_start_sent {
            let usage = chunk.get("usage").cloned().unwrap_or(json!({}));
            let cached = usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|c| c.as_u64())
                .unwrap_or(0);
            let prompt = usage
                .get("prompt_tokens")
                .and_then(|p| p.as_u64())
                .unwrap_or(0);
            let mut usage_out = json!({
                "input_tokens": prompt.saturating_sub(cached),
                "output_tokens": 0
            });
            if cached > 0 {
                usage_out["cache_read_input_tokens"] = json!(cached);
            }
            events.push(json!({
                "type": "message_start",
                "message": {
                    "id": chunk.get("id").cloned().unwrap_or_else(|| Value::String(uuid::Uuid::new_v4().to_string())),
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": chunk.get("model").cloned().unwrap_or_else(|| Value::String(String::new())),
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": usage_out
                }
            }));
            self.message_start_sent = true;
        }

        // Text delta.
        if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
            // If a tool-use block is currently open, close it first.
            if self.content_block_open
                && self
                    .tool_calls
                    .values()
                    .any(|&i| i == self.content_block_index)
            {
                events
                    .push(json!({"type": "content_block_stop", "index": self.content_block_index}));
                self.content_block_index += 1;
                self.content_block_open = false;
            }
            if !self.content_block_open {
                events.push(json!({
                    "type": "content_block_start",
                    "index": self.content_block_index,
                    "content_block": {"type": "text", "text": ""}
                }));
                self.content_block_open = true;
            }
            events.push(json!({
                "type": "content_block_delta",
                "index": self.content_block_index,
                "delta": {"type": "text_delta", "text": text}
            }));
        }

        // Tool-call deltas.
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                let id = tc.get("id").and_then(|i| i.as_str());
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str());
                if let (Some(id), Some(name)) = (id, name) {
                    if self.content_block_open {
                        events.push(json!({"type": "content_block_stop", "index": self.content_block_index}));
                        self.content_block_index += 1;
                        self.content_block_open = false;
                    }
                    let block_index = self.content_block_index;
                    self.tool_calls.insert(idx, block_index);
                    events.push(json!({
                        "type": "content_block_start",
                        "index": block_index,
                        "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                    }));
                    self.content_block_open = true;
                }
                if let Some(args) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                {
                    if let Some(&block_index) = self.tool_calls.get(&idx) {
                        events.push(json!({
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": {"type": "input_json_delta", "partial_json": args}
                        }));
                    }
                }
            }
        }

        // Finish.
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            if self.content_block_open {
                events
                    .push(json!({"type": "content_block_stop", "index": self.content_block_index}));
                self.content_block_open = false;
            }
            let usage = chunk.get("usage").cloned().unwrap_or(json!({}));
            let cached = usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|c| c.as_u64())
                .unwrap_or(0);
            let prompt = usage
                .get("prompt_tokens")
                .and_then(|p| p.as_u64())
                .unwrap_or(0);
            let output = usage
                .get("completion_tokens")
                .and_then(|c| c.as_u64())
                .unwrap_or(0);
            let mut usage_out = json!({
                "input_tokens": prompt.saturating_sub(cached),
                "output_tokens": output
            });
            if cached > 0 {
                usage_out["cache_read_input_tokens"] = json!(cached);
            }
            events.push(json!({
                "type": "message_delta",
                "delta": {"stop_reason": map_finish_reason(Some(fr)), "stop_sequence": Value::Null},
                "usage": usage_out
            }));
            events.push(json!({"type": "message_stop"}));
            self.finished = true;
        }

        events
    }

    /// Whether the upstream already delivered a `finish_reason`, i.e. the
    /// Anthropic event sequence was properly terminated.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Closes an unterminated stream.
    ///
    /// When the upstream connection drops (or simply ends) before sending a
    /// `finish_reason`, no `message_stop` was emitted. Anthropic clients block
    /// waiting for one, and a partially received assistant turn can end up
    /// recorded as if it were complete. This emits the missing
    /// `content_block_stop` / `message_delta` / `message_stop` sequence with an
    /// explicit `stop_reason`, so the client sees a terminated — and, when
    /// `stop_reason` is `"error"`, visibly incomplete — message.
    ///
    /// Returns no events when the stream was already terminated or never
    /// started.
    pub fn finish(&mut self, stop_reason: &str) -> Vec<Value> {
        if self.finished || !self.message_start_sent {
            self.finished = true;
            return Vec::new();
        }
        let mut events = Vec::new();
        if self.content_block_open {
            events.push(json!({"type": "content_block_stop", "index": self.content_block_index}));
            self.content_block_open = false;
        }
        events.push(json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
            "usage": {"output_tokens": 0}
        }));
        events.push(json!({"type": "message_stop"}));
        self.finished = true;
        events
    }
}

/// Anthropic request keys forwarded to the upstream `/v1/messages` endpoint.
const ALLOWED_ANTHROPIC_KEYS: &[&str] = &[
    "model",
    "messages",
    "max_tokens",
    "system",
    "metadata",
    "stop_sequences",
    "stream",
    "temperature",
    "top_p",
    "top_k",
    "tools",
    "tool_choice",
    "thinking",
    "output_config",
    "service_tier",
    // Claude Code's automatic context editing. Copilot accepts this once the
    // matching beta is requested; silently dropping it disables compaction on
    // the client without any diagnostic.
    "context_management",
];

/// Anthropic beta flag that unlocks the `context_management` request field.
pub const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";
/// Anthropic beta flag that unlocks the 1M-token context window.
pub const CONTEXT_1M_BETA: &str = "context-1m-2025-08-07";

/// Builds the `anthropic-beta` header value for an upstream request.
///
/// The client's own `anthropic-beta` header is preserved — Claude Code sends
/// flags there that the proxy has no business dropping — and the flags this
/// proxy derives are appended when missing. Returns `None` when there is
/// nothing to send.
pub fn merge_anthropic_beta(client_value: Option<&str>, derived: &[&str]) -> Option<String> {
    let mut flags: Vec<String> = Vec::new();
    if let Some(value) = client_value {
        for flag in value.split(',') {
            let flag = flag.trim();
            if !flag.is_empty() && !flags.iter().any(|f| f == flag) {
                flags.push(flag.to_string());
            }
        }
    }
    for flag in derived {
        if !flags.iter().any(|f| f == flag) {
            flags.push((*flag).to_string());
        }
    }
    (!flags.is_empty()).then(|| flags.join(","))
}

/// Whether the request asks for Claude Code's context-editing feature, which
/// requires the `context-management-2025-06-27` beta to be requested.
pub fn uses_context_management(req: &Value) -> bool {
    req.get("context_management")
        .map(|v| !v.is_null())
        .unwrap_or(false)
}

fn clean_cache_control(block: &mut Value, extend_ttl: bool) {
    if let Some(cc) = block.get_mut("cache_control") {
        if cc.get("type").and_then(|t| t.as_str()) == Some("ephemeral") {
            if let Some(obj) = cc.as_object_mut() {
                if obj.contains_key("scope") {
                    obj.remove("scope");
                }
                // Only fill a gap. An explicit `ttl` is the client's decision
                // and overriding it would bill the extended rate against a
                // choice someone deliberately made.
                if extend_ttl && !obj.contains_key("ttl") {
                    obj.insert("ttl".to_string(), Value::String("1h".to_string()));
                }
            }
        }
    }
}

fn is_empty_text_block(block: &Value) -> bool {
    if type_of(block) != "text" {
        return false;
    }
    block
        .get("text")
        .and_then(|t| t.as_str())
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
}

/// Filters an Anthropic request down to the allowed keys and strips the
/// unsupported `scope` field from ephemeral `cache_control` blocks. When
/// `extend_ttl` is set, breakpoints left without a `ttl` are promoted to the
/// one-hour tier.
pub fn sanitize_anthropic_request(req: &Value, extend_ttl: bool) -> Value {
    let mut out = Map::new();
    if let Some(obj) = req.as_object() {
        for (k, v) in obj {
            if ALLOWED_ANTHROPIC_KEYS.contains(&k.as_str()) {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    let mut out = Value::Object(out);

    if let Some(tools) = out.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for t in tools {
            clean_cache_control(t, extend_ttl);
        }
    }
    if let Some(system) = out.get_mut("system").and_then(|s| s.as_array_mut()) {
        for s in system {
            clean_cache_control(s, extend_ttl);
        }
    }
    if let Some(messages) = out.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            if let Some(Value::String(s)) = msg.get_mut("content") {
                if s.trim().is_empty() {
                    *s = String::new();
                }
            }
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                for block in content.iter_mut() {
                    clean_cache_control(block, extend_ttl);
                }
                content.retain(|block| !is_empty_text_block(block));
            }
        }
        messages.retain(|msg| {
            if let Some(content) = msg.get("content") {
                match content {
                    Value::String(s) => !s.trim().is_empty(),
                    Value::Array(blocks) => !blocks.is_empty(),
                    _ => true,
                }
            } else {
                true
            }
        });
    }
    out
}

/// Ensures `max_tokens` is large enough to accommodate the requested
/// `thinking.budget_tokens`.
pub fn adjust_thinking_budget(req: &Value) -> Value {
    let budget = req
        .get("thinking")
        .and_then(|t| t.get("budget_tokens"))
        .and_then(|b| b.as_u64());
    let Some(budget) = budget else {
        return req.clone();
    };
    if budget == 0 {
        return req.clone();
    }
    let max_tokens = req.get("max_tokens").and_then(|m| m.as_u64()).unwrap_or(0);
    if max_tokens <= budget {
        let new_max = budget + budget.min(16384);
        let mut out = req.clone();
        out["max_tokens"] = json!(new_max);
        return out;
    }
    req.clone()
}

/// Maps a legacy `thinking.budget_tokens` value to an `output_config.effort`
/// level accepted by adaptive-thinking models. Thresholds approximate the
/// previous budget tiers: up to ~8k tokens is treated as low effort, up to ~24k
/// as medium, and anything larger as high.
fn effort_for_budget(budget: u64) -> &'static str {
    match budget {
        0..=8_191 => "low",
        8_192..=24_575 => "medium",
        _ => "high",
    }
}

/// Rewrites a legacy `thinking: {type: "enabled", budget_tokens: N}` block into
/// the adaptive form required by newer models such as `claude-opus-4.8`:
/// `thinking: {type: "adaptive"}` plus `output_config: {effort: ...}`, where the
/// effort level is derived from the original token budget.
///
/// Returns `None` when there is no enabled-style thinking block to transform, so
/// callers can leave requests for models that still accept `enabled` untouched.
pub fn adapt_thinking_to_adaptive(req: &Value) -> Option<Value> {
    let thinking = req.get("thinking")?;
    if thinking.get("type").and_then(|t| t.as_str()) != Some("enabled") {
        return None;
    }
    let budget = thinking
        .get("budget_tokens")
        .and_then(|b| b.as_u64())
        .unwrap_or(0);
    let effort = effort_for_budget(budget);
    let mut out = req.clone();
    out["thinking"] = json!({ "type": "adaptive" });
    out["output_config"] = json!({ "effort": effort });
    Some(out)
}

/// Applies `system_prompt_add` / `system_prompt_remove` to a direct Anthropic
/// request, and strips the `x-anthropic-billing-header` marker text.
pub fn apply_system_prompt(req: &Value, cfg: &Config) -> Value {
    let system = req.get("system");
    match system {
        None | Some(Value::Null) => {
            if cfg.system_prompt_add.is_empty() {
                return req.clone();
            }
            let mut out = req.clone();
            out["system"] = Value::Array(
                cfg.system_prompt_add
                    .iter()
                    .map(|t| json!({"type": "text", "text": t}))
                    .collect(),
            );
            out
        }
        Some(Value::String(s)) => {
            let mut text = strip_system_prompt(s, &cfg.system_prompt_remove);
            for add in &cfg.system_prompt_add {
                if !text.contains(add.as_str()) {
                    text.push_str("\n\n");
                    text.push_str(add);
                }
            }
            let mut out = req.clone();
            out["system"] = Value::String(text);
            out
        }
        Some(Value::Array(blocks)) => {
            let existing_text: String = blocks
                .iter()
                .filter(|b| type_of(b) == "text")
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            let mut changed = false;
            let mut result: Vec<Value> = Vec::new();
            for b in blocks {
                if type_of(b) == "text" {
                    let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if text.starts_with("x-anthropic-billing-header:") {
                        changed = true;
                        continue;
                    }
                    let stripped = strip_system_prompt(text, &cfg.system_prompt_remove);
                    if stripped != text {
                        changed = true;
                        let mut nb = b.clone();
                        nb["text"] = Value::String(stripped);
                        result.push(nb);
                    } else {
                        result.push(b.clone());
                    }
                } else {
                    result.push(b.clone());
                }
            }
            for add in &cfg.system_prompt_add {
                if !existing_text.contains(add.as_str()) {
                    result.push(json!({"type": "text", "text": add}));
                    changed = true;
                }
            }
            if changed {
                let mut out = req.clone();
                out["system"] = Value::Array(result);
                out
            } else {
                req.clone()
            }
        }
        _ => req.clone(),
    }
}

/// Applies `tool_result_suffix_remove` to string tool results within a direct
/// Anthropic request.
pub fn apply_tool_result_suffix(req: &Value, cfg: &Config) -> Value {
    if cfg.tool_result_suffix_remove.is_empty() {
        return req.clone();
    }
    let Some(messages) = req.get("messages").and_then(|m| m.as_array()) else {
        return req.clone();
    };
    let mut changed = false;
    let new_messages: Vec<Value> = messages
        .iter()
        .map(|msg| {
            let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
                return msg.clone();
            };
            let mut block_changed = false;
            let new_content: Vec<Value> = content
                .iter()
                .map(|block| {
                    if type_of(block) == "tool_result" {
                        if let Some(s) = block.get("content").and_then(|c| c.as_str()) {
                            let stripped =
                                strip_tool_result_suffix(s, &cfg.tool_result_suffix_remove);
                            if stripped != s {
                                block_changed = true;
                                let mut nb = block.clone();
                                nb["content"] = Value::String(stripped);
                                return nb;
                            }
                        }
                    }
                    block.clone()
                })
                .collect();
            if block_changed {
                changed = true;
                let mut nm = msg.clone();
                nm["content"] = Value::Array(new_content);
                nm
            } else {
                msg.clone()
            }
        })
        .collect();
    if changed {
        let mut out = req.clone();
        out["messages"] = Value::Array(new_messages);
        out
    } else {
        req.clone()
    }
}

/// Whether the message list contains an image content block.
///
/// Images can appear either directly in a message's content or nested inside a
/// `tool_result` block — the shape every MCP screenshot tool produces. Missing
/// the nested case means the request goes upstream without
/// `Copilot-Vision-Request`, so the image is silently ignored or rejected.
pub fn has_image(req: &Value) -> bool {
    arr(req, "messages").iter().any(|m| {
        m.get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| blocks.iter().any(block_has_image))
            .unwrap_or(false)
    })
}

/// Whether a content block is an image, or carries one in its nested
/// `tool_result` content.
fn block_has_image(block: &Value) -> bool {
    match type_of(block) {
        "image" => true,
        "tool_result" => block
            .get("content")
            .and_then(|c| c.as_array())
            .map(|inner| inner.iter().any(|b| type_of(b) == "image"))
            .unwrap_or(false),
        _ => false,
    }
}

/// Object keys skipped when flattening a request for token estimation: opaque
/// identifiers and, most importantly, base64 image payloads which would
/// otherwise dominate the count.
const NON_COUNTABLE_KEYS: &[&str] = &[
    "data",
    "type",
    "id",
    "tool_use_id",
    "media_type",
    "cache_control",
];

/// Recursively appends every string leaf of `value` to `out`.
fn push_strings(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(items) => {
            for item in items {
                push_strings(item, out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                if NON_COUNTABLE_KEYS.contains(&k.as_str()) {
                    continue;
                }
                push_strings(v, out);
            }
        }
        _ => {}
    }
}

/// Flattens the countable text of an Anthropic Messages request: the system
/// prompt, every message content block, and the tool definitions. Used for the
/// local token estimate served by `/v1/messages/count_tokens` when the upstream
/// native counting endpoint is unavailable.
pub fn collect_countable_text(req: &Value) -> String {
    let mut out = String::new();
    if let Some(system) = req.get("system") {
        push_strings(system, &mut out);
    }
    for msg in arr(req, "messages") {
        if let Some(content) = msg.get("content") {
            push_strings(content, &mut out);
        }
    }
    for tool in arr(req, "tools") {
        push_strings(&tool, &mut out);
    }
    out
}

/// Fixed token overhead the API charges for each message's role/framing.
const PER_MESSAGE_OVERHEAD: u64 = 4;
/// Fixed token overhead for each tool definition beyond its serialized schema.
const PER_TOOL_OVERHEAD: u64 = 8;
/// Fixed overhead charged once per request.
const REQUEST_OVERHEAD: u64 = 8;

/// Estimates the input token count of an Anthropic Messages request when the
/// upstream native counting endpoint is unavailable.
///
/// Counting only the visible text under-reports badly: the API also charges for
/// per-message framing and for the full JSON schema of every tool definition.
/// A client that trusts a low number (Claude Code decides when to compact from
/// it) keeps growing the conversation until the request is hard-rejected, so
/// this deliberately errs on the side of the structural overhead being present.
pub fn estimate_input_tokens(req: &Value, tokenizer: &str) -> u64 {
    use crate::filters::count_tokens;

    let mut total = REQUEST_OVERHEAD;

    if let Some(system) = req.get("system") {
        let mut text = String::new();
        push_strings(system, &mut text);
        total += count_tokens(&text, tokenizer);
    }

    for msg in arr(req, "messages") {
        total += PER_MESSAGE_OVERHEAD;
        if let Some(content) = msg.get("content") {
            let mut text = String::new();
            push_strings(content, &mut text);
            total += count_tokens(&text, tokenizer);
        }
    }

    // Tool definitions are sent as JSON, so the schema punctuation and keys are
    // billed too — counting only the description would miss most of it.
    for tool in arr(req, "tools") {
        total += PER_TOOL_OVERHEAD;
        let serialized = serde_json::to_string(&tool).unwrap_or_default();
        total += count_tokens(&serialized, tokenizer);
    }

    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;

    #[test]
    fn anthropic_to_openai_basic() {
        let cfg = Config::default();
        let req = json!({
            "model": "claude-3",
            "system": "be helpful",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = anthropic_to_openai(&req, &cfg);
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][0]["content"], "be helpful");
        assert_eq!(out["messages"][1]["role"], "user");
        assert_eq!(out["messages"][1]["content"], "hi");
        assert_eq!(out["max_tokens"], 100);
    }

    #[test]
    fn countable_text_covers_system_messages_and_tools() {
        let req = json!({
            "system": [{"type": "text", "text": "system rules"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello there"}]},
                {"role": "assistant", "content": "sure thing"}
            ],
            "tools": [{"name": "read_file", "description": "reads a file"}]
        });
        let text = collect_countable_text(&req);
        assert!(text.contains("system rules"));
        assert!(text.contains("hello there"));
        assert!(text.contains("sure thing"));
        assert!(text.contains("read_file"));
        assert!(text.contains("reads a file"));
    }

    #[test]
    fn countable_text_skips_base64_image_payloads() {
        let blob = "A".repeat(5000);
        let req = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": blob}}
                ]
            }]
        });
        let text = collect_countable_text(&req);
        assert!(text.contains("describe"));
        assert!(!text.contains("AAAA"));
    }

    #[test]
    fn tool_result_becomes_tool_message() {
        let cfg = Config::default();
        let req = json!({
            "model": "claude-3",
            "messages": [{
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "abc", "content": "ok"}]
            }]
        });
        let out = anthropic_to_openai(&req, &cfg);
        assert_eq!(out["messages"][0]["role"], "tool");
        assert_eq!(out["messages"][0]["tool_call_id"], "abc");
        assert_eq!(out["messages"][0]["content"], "ok");
    }

    #[test]
    fn tool_result_block_array_is_flattened_to_text() {
        // MCP servers commonly return `content` as an array of blocks. Passing
        // the array through unchanged makes the upstream reject the request.
        let cfg = Config::default();
        let req = json!({
            "model": "claude-3",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "abc",
                    "content": [
                        {"type": "text", "text": "line one"},
                        {"type": "text", "text": "line two"}
                    ]
                }]
            }]
        });
        let out = anthropic_to_openai(&req, &cfg);
        assert_eq!(out["messages"][0]["role"], "tool");
        assert_eq!(out["messages"][0]["content"], "line one\nline two");
    }

    #[test]
    fn tool_result_image_block_becomes_image_url() {
        let cfg = Config::default();
        let req = json!({
            "model": "claude-3",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "abc",
                    "content": [
                        {"type": "text", "text": "screenshot:"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}}
                    ]
                }]
            }]
        });
        let out = anthropic_to_openai(&req, &cfg);
        let content = &out["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn url_source_images_are_passed_through() {
        let block =
            json!({"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}});
        let out = image_block_to_image_url(&block);
        assert_eq!(out["image_url"]["url"], "https://example.com/a.png");
    }

    #[test]
    fn vision_is_detected_inside_tool_results() {
        let img = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "x"}});
        // Directly in the message content.
        let direct = json!({"messages": [{"role": "user", "content": [img.clone()]}]});
        assert!(has_image(&direct));
        // Nested in a tool_result — the shape MCP screenshot tools produce.
        let nested = json!({"messages": [{
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t", "content": [
                {"type": "text", "text": "here"}, img
            ]}]
        }]});
        assert!(has_image(&nested));
        // No image anywhere.
        let none =
            json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]});
        assert!(!has_image(&none));
        // A string-content tool_result must not panic or false-positive.
        let plain = json!({"messages": [{
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]
        }]});
        assert!(!has_image(&plain));
    }

    #[test]
    fn tool_result_suffix_is_stripped_inside_blocks() {
        let suffixes = vec!["\n[end]".to_string()];
        let content = json!([{"type": "text", "text": "result\n[end]"}]);
        let out = normalize_tool_result_content(Some(&content), &suffixes);
        assert_eq!(out, json!("result"));
    }

    #[test]
    fn beta_header_merges_client_and_derived_flags() {
        // The client's flags are preserved and derived ones appended.
        assert_eq!(
            merge_anthropic_beta(Some("claude-code-20250219"), &[CONTEXT_1M_BETA]),
            Some(format!("claude-code-20250219,{CONTEXT_1M_BETA}"))
        );
        // Duplicates are collapsed.
        assert_eq!(
            merge_anthropic_beta(Some(CONTEXT_1M_BETA), &[CONTEXT_1M_BETA]),
            Some(CONTEXT_1M_BETA.to_string())
        );
        // Whitespace around client flags is tolerated.
        assert_eq!(
            merge_anthropic_beta(Some("a , b"), &[]),
            Some("a,b".to_string())
        );
        // Nothing to send.
        assert_eq!(merge_anthropic_beta(None, &[]), None);
        assert_eq!(merge_anthropic_beta(Some(""), &[]), None);
    }

    #[test]
    fn context_management_survives_sanitization() {
        let req = json!({
            "model": "m",
            "messages": [],
            "context_management": {"edits": [{"type": "clear_tool_uses_20250919"}]}
        });
        assert!(uses_context_management(&req));
        let out = sanitize_anthropic_request(&req, false);
        assert!(out.get("context_management").is_some());
    }

    #[test]
    fn token_estimate_includes_message_and_tool_overhead() {
        let bare = json!({"messages": [{"role": "user", "content": "hello"}]});
        let with_tools = json!({
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{
                "name": "read_file",
                "description": "Read a file from disk",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
            }]
        });
        let bare_count = estimate_input_tokens(&bare, "cl100k_base");
        let tool_count = estimate_input_tokens(&with_tools, "cl100k_base");
        // The tool schema must be billed, otherwise clients compact too late.
        assert!(
            tool_count > bare_count + 10,
            "tools added only {} tokens",
            tool_count - bare_count
        );
        // Even a bare request carries per-message framing overhead.
        assert!(bare_count > crate::filters::count_tokens("hello", "cl100k_base"));
    }

    #[test]
    fn openai_response_to_anthropic() {
        let resp = json!({
            "id": "x",
            "model": "m",
            "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let out = openai_to_anthropic(&resp);
        assert_eq!(out["type"], "message");
        assert_eq!(out["content"][0]["text"], "hello");
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert_eq!(out["usage"]["output_tokens"], 5);
    }

    #[test]
    fn merge_chunks_concatenates_text() {
        let chunks = vec![
            json!({"id": "1", "model": "m", "choices": [{"delta": {"content": "Hel"}}]}),
            json!({"choices": [{"delta": {"content": "lo"}, "finish_reason": "stop"}]}),
        ];
        let merged = merge_chat_chunks(&chunks);
        assert_eq!(merged["choices"][0]["message"]["content"], "Hello");
        assert_eq!(merged["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn stream_state_emits_message_start_and_stop() {
        let mut st = AnthropicStreamState::new();
        let start = st
            .process(&json!({"id": "1", "model": "m", "choices": [{"delta": {"content": "hi"}}]}));
        assert_eq!(start[0]["type"], "message_start");
        let end = st.process(&json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}));
        assert!(end.iter().any(|e| e["type"] == "message_stop"));
        assert!(st.is_finished());
        // A properly terminated stream needs no synthetic closing events.
        assert!(st.finish("error").is_empty());
    }

    #[test]
    fn finish_closes_a_stream_cut_before_finish_reason() {
        let mut st = AnthropicStreamState::new();
        st.process(&json!({"id": "1", "model": "m", "choices": [{"delta": {"content": "par"}}]}));
        assert!(!st.is_finished());
        // Upstream died here: no finish_reason, so no message_stop was sent and
        // an Anthropic client would block forever.
        let events = st.finish("error");
        assert_eq!(events[0]["type"], "content_block_stop");
        assert_eq!(events[1]["type"], "message_delta");
        assert_eq!(events[1]["delta"]["stop_reason"], "error");
        assert_eq!(events[2]["type"], "message_stop");
        assert!(st.is_finished());
        // Idempotent.
        assert!(st.finish("error").is_empty());
    }

    #[test]
    fn finish_is_noop_before_any_chunk_arrived() {
        // Nothing was ever emitted, so there is no half-open message to close.
        let mut st = AnthropicStreamState::new();
        assert!(st.finish("error").is_empty());
    }

    #[test]
    fn finish_closes_an_open_tool_use_block() {
        let mut st = AnthropicStreamState::new();
        st.process(&json!({
            "id": "1",
            "model": "m",
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "toolu_1", "function": {"name": "read_file", "arguments": "{\"p"}}
            ]}}]
        }));
        // The tool_use block is open with truncated JSON arguments.
        let events = st.finish("error");
        assert_eq!(events[0]["type"], "content_block_stop");
        assert!(events.iter().any(|e| e["type"] == "message_stop"));
    }

    #[test]
    fn sanitize_drops_unknown_keys() {
        let req = json!({"model": "m", "messages": [], "foo": "bar"});
        let out = sanitize_anthropic_request(&req, false);
        assert!(out.get("foo").is_none());
        assert_eq!(out["model"], "m");
    }

    #[test]
    fn sanitize_keeps_output_config() {
        let req = json!({"model": "m", "messages": [], "output_config": {"effort": "high"}});
        let out = sanitize_anthropic_request(&req, false);
        assert_eq!(out["output_config"]["effort"], "high");
    }

    fn ttl_probe_request() -> Value {
        json!({
            "model": "m",
            "tools": [{"name": "t", "cache_control": {"type": "ephemeral"}}],
            "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "u", "cache_control": {"type": "ephemeral"}}]
            }]
        })
    }

    /// Off by default: extended writes bill at a higher rate.
    #[test]
    fn cache_ttl_is_left_alone_unless_asked_for() {
        let out = sanitize_anthropic_request(&ttl_probe_request(), false);
        assert!(out["tools"][0]["cache_control"].get("ttl").is_none());
        assert!(out["system"][0]["cache_control"].get("ttl").is_none());
        assert!(out["messages"][0]["content"][0]["cache_control"]
            .get("ttl")
            .is_none());
    }

    #[test]
    fn cache_ttl_is_extended_at_every_breakpoint() {
        let out = sanitize_anthropic_request(&ttl_probe_request(), true);
        assert_eq!(out["tools"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(out["system"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(
            out["messages"][0]["content"][0]["cache_control"]["ttl"],
            "1h"
        );
    }

    /// A client that names a ttl has priced the trade itself.
    #[test]
    fn an_explicit_cache_ttl_is_never_overridden() {
        let req = json!({
            "model": "m",
            "system": [{
                "type": "text", "text": "s",
                "cache_control": {"type": "ephemeral", "ttl": "5m"}
            }],
            "messages": []
        });
        let out = sanitize_anthropic_request(&req, true);
        assert_eq!(out["system"][0]["cache_control"]["ttl"], "5m");
    }

    /// `scope` is rejected upstream, so it has to go even while a ttl goes in.
    #[test]
    fn extending_the_ttl_still_strips_scope() {
        let req = json!({
            "model": "m",
            "system": [{
                "type": "text", "text": "s",
                "cache_control": {"type": "ephemeral", "scope": "global"}
            }],
            "messages": []
        });
        let out = sanitize_anthropic_request(&req, true);
        assert!(out["system"][0]["cache_control"].get("scope").is_none());
        assert_eq!(out["system"][0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn sanitize_drops_empty_text_blocks() {
        let req = json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "   \n\t  "},
                    {"type": "tool_result", "tool_use_id": "x", "content": "ok"}
                ]
            }]
        });
        let out = sanitize_anthropic_request(&req, false);
        let blocks = out["messages"][0]["content"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_result");
    }

    #[test]
    fn sanitize_drops_messages_with_empty_content() {
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "   "},
                {"role": "assistant", "content": [{"type": "text", "text": "  "}]},
                {"role": "user", "content": [{"type": "text", "text": "hello"}]}
            ]
        });
        let out = sanitize_anthropic_request(&req, false);
        let messages = out["messages"].as_array().cloned().unwrap_or_default();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "hello");
    }

    #[test]
    fn adapt_thinking_rewrites_enabled_to_adaptive() {
        let req = json!({
            "model": "claude-opus-4.8",
            "thinking": {"type": "enabled", "budget_tokens": 16000},
            "messages": []
        });
        let out = adapt_thinking_to_adaptive(&req).expect("should transform");
        assert_eq!(out["thinking"]["type"], "adaptive");
        assert!(out["thinking"].get("budget_tokens").is_none());
        assert_eq!(out["output_config"]["effort"], "medium");
    }

    #[test]
    fn adapt_thinking_effort_thresholds() {
        let low = json!({"thinking": {"type": "enabled", "budget_tokens": 4000}});
        assert_eq!(
            adapt_thinking_to_adaptive(&low).unwrap()["output_config"]["effort"],
            "low"
        );
        let high = json!({"thinking": {"type": "enabled", "budget_tokens": 32000}});
        assert_eq!(
            adapt_thinking_to_adaptive(&high).unwrap()["output_config"]["effort"],
            "high"
        );
    }

    #[test]
    fn adapt_thinking_ignores_non_enabled() {
        // No thinking block at all.
        assert!(adapt_thinking_to_adaptive(&json!({"model": "m"})).is_none());
        // Already adaptive.
        let adaptive = json!({"thinking": {"type": "adaptive"}});
        assert!(adapt_thinking_to_adaptive(&adaptive).is_none());
    }
}
