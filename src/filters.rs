//! Content filtering helpers: system prompt manipulation, tool result suffix
//! stripping, and rough token estimation.

/// Removes every configured `system_prompt_remove` substring from `text`.
pub fn strip_system_prompt(text: &str, remove: &[String]) -> String {
    let mut out = text.to_string();
    for needle in remove {
        if !needle.is_empty() && out.contains(needle.as_str()) {
            out = out.replace(needle.as_str(), "");
        }
    }
    out
}

/// Strips any configured trailing `tool_result_suffix_remove` suffix.
pub fn strip_tool_result_suffix(text: &str, suffixes: &[String]) -> String {
    let mut out = text.to_string();
    for suffix in suffixes {
        if !suffix.is_empty() && out.ends_with(suffix.as_str()) {
            out.truncate(out.len() - suffix.len());
        }
    }
    out
}

/// Rough token estimate: `ceil(len / 4)`.
pub fn estimate_tokens(text: &str) -> u64 {
    ((text.len() as f64) / 4.0).ceil() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_substrings() {
        let r = vec!["secret".to_string()];
        assert_eq!(strip_system_prompt("a secret b secret", &r), "a  b ");
    }

    #[test]
    fn strips_suffix_only() {
        let s = vec!["\n[done]".to_string()];
        assert_eq!(strip_tool_result_suffix("result\n[done]", &s), "result");
        assert_eq!(strip_tool_result_suffix("[done]\nresult", &s), "[done]\nresult");
    }

    #[test]
    fn token_estimate() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
