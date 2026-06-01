//! Codex-specific adapters for the OpenAI `/v1/responses` endpoint.

use serde_json::{json, Value};

/// Rewrites the custom `apply_patch` tool as a standard OpenAI function tool
/// and strips unsupported tool types that Copilot rejects.
pub fn adapt_tools(req: &mut Value) {
    let Some(tools) = req.get("tools").and_then(|t| t.as_array()).cloned() else {
        return;
    };
    let mapped: Vec<Value> = tools
        .into_iter()
        .map(|t| {
            let is_apply_patch = t.get("type").and_then(|x| x.as_str()) == Some("custom")
                && t.get("name").and_then(|n| n.as_str()) == Some("apply_patch");
            if is_apply_patch {
                json!({
                    "type": "function",
                    "name": "apply_patch",
                    "description": "Use the `apply_patch` tool to edit files",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": "The entire contents of the apply_patch command"
                            }
                        },
                        "required": ["input"]
                    },
                    "strict": false
                })
            } else {
                t
            }
        })
        .filter(|t| {
            // Strip unsupported tool types.
            !matches!(
                t.get("type").and_then(|x| x.as_str()),
                Some("image_generation")
            )
        })
        .collect();
    req["tools"] = Value::Array(mapped);
}

/// Trims everything before the latest `compaction` marker in the input array.
pub fn apply_compaction(input: &[Value]) -> Vec<Value> {
    for i in (0..input.len()).rev() {
        if input[i].get("type").and_then(|t| t.as_str()) == Some("compaction") {
            if i == 0 {
                return input.to_vec();
            }
            return input[i..].to_vec();
        }
    }
    input.to_vec()
}

/// Whether the last input item has an `assistant` role (used for `X-Initiator`).
pub fn is_agent_initiator(input: &Value) -> bool {
    let Some(arr) = input.as_array() else {
        return false;
    };
    let Some(last) = arr.last() else {
        return false;
    };
    if !last.is_object() {
        return false;
    }
    match last.get("role") {
        Some(Value::String(r)) => r.to_lowercase() == "assistant",
        Some(_) => false,
        None => true,
    }
}

/// Whether any input item contains an `input_image` block (vision request).
pub fn has_input_image(input: &Value) -> bool {
    let Some(arr) = input.as_array() else {
        return false;
    };
    for item in arr {
        if !item.is_object() {
            continue;
        }
        if item.get("type").and_then(|t| t.as_str()) == Some("input_image") {
            return true;
        }
        if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
            if content
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("input_image"))
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_trims() {
        let input = vec![json!({"x":1}), json!({"type":"compaction"}), json!({"y":2})];
        let out = apply_compaction(&input);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["type"], "compaction");
    }

    #[test]
    fn apply_patch_rewritten() {
        let mut req = json!({"tools": [{"type": "custom", "name": "apply_patch"}]});
        adapt_tools(&mut req);
        assert_eq!(req["tools"][0]["type"], "function");
        assert_eq!(req["tools"][0]["name"], "apply_patch");
    }

    #[test]
    fn unsupported_tools_stripped() {
        let mut req = json!({"tools": [{"type": "image_generation"}, {"type": "function", "name": "x"}]});
        adapt_tools(&mut req);
        assert_eq!(req["tools"].as_array().unwrap().len(), 1);
    }
}
