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
                    let tool_results: Vec<&Value> =
                        blocks.iter().filter(|b| type_of(b) == "tool_result").collect();
                    let others: Vec<Value> = blocks
                        .iter()
                        .filter(|b| type_of(b) != "tool_result")
                        .cloned()
                        .collect();
                    for tr in tool_results {
                        let mut c = tr.get("content").cloned().unwrap_or(Value::String(String::new()));
                        if let Value::String(s) = &c {
                            c = Value::String(strip_tool_result_suffix(
                                s,
                                &cfg.tool_result_suffix_remove,
                            ));
                        }
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
                            let key = if type_of(b) == "text" { "text" } else { "thinking" };
                            b.get(key).and_then(|t| t.as_str()).unwrap_or("").to_string()
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

/// Extracts the OpenAI `content` value (string or multimodal array) from a set
/// of Anthropic user content blocks.
fn extract_user_content(blocks: &[Value]) -> Option<Value> {
    let has_image = blocks.iter().any(|b| type_of(b) == "image");
    if !has_image {
        let parts: Vec<String> = blocks
            .iter()
            .filter_map(|b| match type_of(b) {
                "text" => Some(b.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()),
                "thinking" => Some(
                    b.get("thinking").and_then(|t| t.as_str()).unwrap_or("").to_string(),
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
                "image" => {
                    let src = b.get("source");
                    let media = src
                        .and_then(|s| s.get("media_type"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("");
                    let data = src
                        .and_then(|s| s.get("data"))
                        .and_then(|d| d.as_str())
                        .unwrap_or("");
                    out.push(json!({
                        "type": "image_url",
                        "image_url": {"url": format!("data:{media};base64,{data}")}
                    }));
                }
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
    let prompt = usage.get("prompt_tokens").and_then(|p| p.as_u64()).unwrap_or(0);
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
            let prompt = usage.get("prompt_tokens").and_then(|p| p.as_u64()).unwrap_or(0);
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
                && self.tool_calls.values().any(|&i| i == self.content_block_index)
            {
                events.push(json!({"type": "content_block_stop", "index": self.content_block_index}));
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
                let name = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str());
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
                events.push(json!({"type": "content_block_stop", "index": self.content_block_index}));
                self.content_block_open = false;
            }
            let usage = chunk.get("usage").cloned().unwrap_or(json!({}));
            let cached = usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|c| c.as_u64())
                .unwrap_or(0);
            let prompt = usage.get("prompt_tokens").and_then(|p| p.as_u64()).unwrap_or(0);
            let output = usage.get("completion_tokens").and_then(|c| c.as_u64()).unwrap_or(0);
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
        }

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
    "service_tier",
];

fn clean_cache_control(block: &mut Value) {
    if let Some(cc) = block.get_mut("cache_control") {
        if cc.get("type").and_then(|t| t.as_str()) == Some("ephemeral") {
            if let Some(obj) = cc.as_object_mut() {
                if obj.contains_key("scope") {
                    obj.remove("scope");
                }
            }
        }
    }
}

/// Filters an Anthropic request down to the allowed keys and strips the
/// unsupported `scope` field from ephemeral `cache_control` blocks.
pub fn sanitize_anthropic_request(req: &Value) -> Value {
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
            clean_cache_control(t);
        }
    }
    if let Some(system) = out.get_mut("system").and_then(|s| s.as_array_mut()) {
        for s in system {
            clean_cache_control(s);
        }
    }
    if let Some(messages) = out.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages {
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                for block in content {
                    clean_cache_control(block);
                }
            }
        }
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
pub fn has_image(req: &Value) -> bool {
    arr(req, "messages").iter().any(|m| {
        m.get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| blocks.iter().any(|b| type_of(b) == "image"))
            .unwrap_or(false)
    })
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
        let start = st.process(&json!({"id": "1", "model": "m", "choices": [{"delta": {"content": "hi"}}]}));
        assert_eq!(start[0]["type"], "message_start");
        let end = st.process(&json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}));
        assert!(end.iter().any(|e| e["type"] == "message_stop"));
    }

    #[test]
    fn sanitize_drops_unknown_keys() {
        let req = json!({"model": "m", "messages": [], "foo": "bar"});
        let out = sanitize_anthropic_request(&req);
        assert!(out.get("foo").is_none());
        assert_eq!(out["model"], "m");
    }
}
