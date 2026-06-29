//! Translation between the Google Gemini `generateContent` API and the OpenAI
//! Chat Completions API, in both directions, plus streaming conversion.
//!
//! Copilot does not expose a native Gemini wire format, so Gemini-compatible
//! requests are translated into OpenAI chat completions, forwarded upstream, and
//! the responses translated back. This mirrors the Anthropic translation path in
//! `anthropic.rs`.

use serde_json::{json, Map, Value};

fn arr(v: &Value, key: &str) -> Vec<Value> {
    v.get(key)
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Collects the text from a Gemini `parts` array into a single string.
fn parts_text(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("")
}

/// Whether any content part carries inline image data (vision request).
pub fn has_image(req: &Value) -> bool {
    arr(req, "contents").iter().any(|c| {
        c.get("parts")
            .and_then(|p| p.as_array())
            .map(|parts| parts.iter().any(|p| p.get("inlineData").is_some()))
            .unwrap_or(false)
    })
}

/// Whether the last content turn is from the model (used for `X-Initiator`).
pub fn is_agent(req: &Value) -> bool {
    arr(req, "contents")
        .last()
        .and_then(|c| c.get("role").and_then(|r| r.as_str()))
        .map(|r| r == "model")
        .unwrap_or(false)
}

/// Builds the OpenAI `content` value from a set of Gemini parts, emitting a
/// multimodal array when inline image data is present and a plain string
/// otherwise.
fn user_content_from_parts(parts: &[Value]) -> Value {
    let has_image = parts.iter().any(|p| p.get("inlineData").is_some());
    if !has_image {
        return Value::String(parts_text(parts));
    }
    let mut out: Vec<Value> = Vec::new();
    for p in parts {
        if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
            if !text.is_empty() {
                out.push(json!({"type": "text", "text": text}));
            }
        } else if let Some(inline) = p.get("inlineData") {
            let mime = inline
                .get("mimeType")
                .and_then(|m| m.as_str())
                .unwrap_or("image/png");
            let data = inline.get("data").and_then(|d| d.as_str()).unwrap_or("");
            out.push(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{mime};base64,{data}")}
            }));
        }
    }
    Value::Array(out)
}

/// Converts a Gemini `generateContent` request body into an OpenAI chat
/// completions request body. `model` is the resolved upstream model id (the
/// Gemini wire format carries the model in the URL, not the body).
pub fn gemini_to_openai(req: &Value, model: &str, stream: bool) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    // System instruction -> system message.
    if let Some(sys) = req.get("systemInstruction").or_else(|| req.get("system_instruction")) {
        let parts = arr(sys, "parts");
        let text = parts_text(&parts);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    for content in arr(req, "contents") {
        let role = content.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let parts = arr(&content, "parts");

        // Function responses become OpenAI tool messages.
        let func_responses: Vec<&Value> = parts
            .iter()
            .filter(|p| p.get("functionResponse").is_some())
            .collect();
        if !func_responses.is_empty() {
            for fr in func_responses {
                let fr = fr.get("functionResponse").unwrap();
                let name = fr.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let response = fr.get("response").cloned().unwrap_or(json!({}));
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": name,
                    "content": serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string())
                }));
            }
            continue;
        }

        // Function calls (model turn) become OpenAI assistant tool_calls.
        let func_calls: Vec<&Value> = parts
            .iter()
            .filter(|p| p.get("functionCall").is_some())
            .collect();
        if role == "model" && !func_calls.is_empty() {
            let text = parts_text(&parts);
            let tool_calls: Vec<Value> = func_calls
                .iter()
                .map(|fc| {
                    let fc = fc.get("functionCall").unwrap();
                    let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let args = fc.get("args").cloned().unwrap_or(json!({}));
                    json!({
                        "id": name,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string())
                        }
                    })
                })
                .collect();
            messages.push(json!({
                "role": "assistant",
                "content": if text.is_empty() { Value::Null } else { Value::String(text) },
                "tool_calls": tool_calls
            }));
            continue;
        }

        let openai_role = if role == "model" { "assistant" } else { "user" };
        let content_val = if openai_role == "user" {
            user_content_from_parts(&parts)
        } else {
            Value::String(parts_text(&parts))
        };
        messages.push(json!({"role": openai_role, "content": content_val}));
    }

    let mut out = Map::new();
    out.insert("model".into(), Value::String(model.to_string()));
    out.insert("messages".into(), Value::Array(messages));
    out.insert("stream".into(), Value::Bool(stream));

    // generationConfig -> top-level OpenAI sampling params.
    if let Some(gc) = req.get("generationConfig").or_else(|| req.get("generation_config")) {
        if let Some(v) = gc.get("temperature") {
            out.insert("temperature".into(), v.clone());
        }
        if let Some(v) = gc.get("topP").or_else(|| gc.get("top_p")) {
            out.insert("top_p".into(), v.clone());
        }
        if let Some(v) = gc.get("maxOutputTokens").or_else(|| gc.get("max_output_tokens")) {
            out.insert("max_tokens".into(), v.clone());
        }
        if let Some(v) = gc.get("stopSequences").or_else(|| gc.get("stop_sequences")) {
            out.insert("stop".into(), v.clone());
        }
    }

    // tools.functionDeclarations -> OpenAI function tools.
    let mut openai_tools: Vec<Value> = Vec::new();
    for tool in arr(req, "tools") {
        for decl in arr(&tool, "functionDeclarations")
            .into_iter()
            .chain(arr(&tool, "function_declarations"))
        {
            openai_tools.push(json!({
                "type": "function",
                "function": {
                    "name": decl.get("name").cloned().unwrap_or(Value::Null),
                    "description": decl.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": decl.get("parameters").cloned().unwrap_or(json!({"type": "object"}))
                }
            }));
        }
    }
    if !openai_tools.is_empty() {
        out.insert("tools".into(), Value::Array(openai_tools));
    }

    Value::Object(out)
}

/// Maps an OpenAI finish reason to a Gemini finish reason.
pub fn map_finish_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("stop") | Some("tool_calls") => "STOP",
        Some("length") => "MAX_TOKENS",
        Some("content_filter") => "SAFETY",
        _ => "STOP",
    }
}

/// Builds a Gemini `usageMetadata` object from an OpenAI `usage` object.
fn usage_to_gemini(usage: &Value) -> Value {
    let prompt = usage.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
    let completion = usage
        .get("completion_tokens")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(|t| t.as_u64())
        .unwrap_or(prompt + completion);
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|c| c.as_u64())
        .unwrap_or(0);
    let mut m = json!({
        "promptTokenCount": prompt,
        "candidatesTokenCount": completion,
        "totalTokenCount": total
    });
    if cached > 0 {
        m["cachedContentTokenCount"] = json!(cached);
    }
    m
}

/// Converts an OpenAI chat-completion response into a Gemini
/// `generateContent` response.
pub fn openai_to_gemini(resp: &Value) -> Value {
    let mut parts: Vec<Value> = Vec::new();
    let mut finish: Option<String> = None;

    for choice in arr(resp, "choices") {
        if let Some(message) = choice.get("message") {
            if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    parts.push(json!({"text": text}));
                }
            }
            if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tool_calls {
                    let func = tc.get("function");
                    let name = func
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("");
                    let args_str = func
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                        .unwrap_or("{}");
                    let args: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
                    parts.push(json!({
                        "functionCall": {"name": name, "args": args}
                    }));
                }
            }
        }
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            finish = Some(fr.to_string());
        }
    }

    let usage = resp.get("usage").cloned().unwrap_or(json!({}));
    json!({
        "candidates": [{
            "content": {"role": "model", "parts": parts},
            "finishReason": map_finish_reason(finish.as_deref()),
            "index": 0
        }],
        "usageMetadata": usage_to_gemini(&usage),
        "modelVersion": resp.get("model").cloned().unwrap_or(Value::Null)
    })
}

/// Builds a single Gemini streaming chunk from a text delta.
pub fn gemini_stream_text_chunk(text: &str, model: &Value) -> Value {
    json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": text}]},
            "index": 0
        }],
        "modelVersion": model.clone()
    })
}

/// Builds the final Gemini streaming chunk carrying the finish reason and usage.
pub fn gemini_stream_final_chunk(finish: Option<&str>, usage: &Value, model: &Value) -> Value {
    json!({
        "candidates": [{
            "content": {"role": "model", "parts": []},
            "finishReason": map_finish_reason(finish),
            "index": 0
        }],
        "usageMetadata": usage_to_gemini(usage),
        "modelVersion": model.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_text_request() {
        let req = json!({
            "contents": [{"role": "user", "parts": [{"text": "hello"}]}],
            "systemInstruction": {"parts": [{"text": "be nice"}]}
        });
        let out = gemini_to_openai(&req, "gpt-4o", false);
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][0]["content"], "be nice");
        assert_eq!(out["messages"][1]["role"], "user");
        assert_eq!(out["messages"][1]["content"], "hello");
        assert_eq!(out["model"], "gpt-4o");
    }

    #[test]
    fn generation_config_maps_params() {
        let req = json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {"temperature": 0.5, "maxOutputTokens": 256, "topP": 0.9}
        });
        let out = gemini_to_openai(&req, "m", false);
        assert_eq!(out["temperature"], 0.5);
        assert_eq!(out["max_tokens"], 256);
        assert_eq!(out["top_p"], 0.9);
    }

    #[test]
    fn function_declarations_become_tools() {
        let req = json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "tools": [{"functionDeclarations": [{"name": "get_weather", "description": "d", "parameters": {"type": "object"}}]}]
        });
        let out = gemini_to_openai(&req, "m", false);
        assert_eq!(out["tools"][0]["type"], "function");
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
    }

    #[test]
    fn model_turn_with_function_call() {
        let req = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "weather?"}]},
                {"role": "model", "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "SF"}}}]},
                {"role": "user", "parts": [{"functionResponse": {"name": "get_weather", "response": {"temp": 70}}}]}
            ]
        });
        let out = gemini_to_openai(&req, "m", false);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(msgs[2]["role"], "tool");
    }

    #[test]
    fn response_text_to_gemini() {
        let resp = json!({
            "model": "m",
            "choices": [{"message": {"content": "hello there"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let out = openai_to_gemini(&resp);
        assert_eq!(out["candidates"][0]["content"]["parts"][0]["text"], "hello there");
        assert_eq!(out["candidates"][0]["finishReason"], "STOP");
        assert_eq!(out["usageMetadata"]["promptTokenCount"], 10);
        assert_eq!(out["usageMetadata"]["candidatesTokenCount"], 5);
        assert_eq!(out["usageMetadata"]["totalTokenCount"], 15);
    }

    #[test]
    fn response_tool_call_to_gemini() {
        let resp = json!({
            "model": "m",
            "choices": [{
                "message": {"content": null, "tool_calls": [{"id": "1", "type": "function", "function": {"name": "f", "arguments": "{\"x\":1}"}}]},
                "finish_reason": "tool_calls"
            }],
            "usage": {}
        });
        let out = openai_to_gemini(&resp);
        let part = &out["candidates"][0]["content"]["parts"][0];
        assert_eq!(part["functionCall"]["name"], "f");
        assert_eq!(part["functionCall"]["args"]["x"], 1);
    }

    #[test]
    fn detects_image_and_agent() {
        let req = json!({
            "contents": [{"role": "user", "parts": [{"inlineData": {"mimeType": "image/png", "data": "abc"}}]}]
        });
        assert!(has_image(&req));
        let agent_req = json!({"contents": [{"role": "model", "parts": [{"text": "x"}]}]});
        assert!(is_agent(&agent_req));
    }

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(map_finish_reason(Some("stop")), "STOP");
        assert_eq!(map_finish_reason(Some("length")), "MAX_TOKENS");
        assert_eq!(map_finish_reason(Some("content_filter")), "SAFETY");
        assert_eq!(map_finish_reason(None), "STOP");
    }
}
