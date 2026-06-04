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

/// Returns a cached BPE encoder for the given tokenizer name (as advertised by
/// a model's `capabilities.tokenizer` field). Falls back to `cl100k_base` for
/// unknown or empty names.
fn bpe_for(tokenizer: &str) -> &'static tiktoken_rs::CoreBPE {
    use std::sync::OnceLock;
    match tokenizer {
        "o200k_base" => {
            static C: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
            C.get_or_init(|| tiktoken_rs::o200k_base().expect("o200k_base"))
        }
        "p50k_base" | "p50k_edit" => {
            static C: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
            C.get_or_init(|| tiktoken_rs::p50k_base().expect("p50k_base"))
        }
        "r50k_base" | "gpt2" => {
            static C: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
            C.get_or_init(|| tiktoken_rs::r50k_base().expect("r50k_base"))
        }
        _ => {
            static C: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
            C.get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base"))
        }
    }
}

/// Counts the number of tokens in `text` using the BPE encoder named by
/// `tokenizer`. Unknown tokenizers fall back to `cl100k_base`.
pub fn count_tokens(text: &str, tokenizer: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    bpe_for(tokenizer).encode_ordinary(text).len() as u64
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
        assert_eq!(
            strip_tool_result_suffix("[done]\nresult", &s),
            "[done]\nresult"
        );
    }

    #[test]
    fn token_estimate() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn bpe_counts_tokens() {
        // Empty input is always zero.
        assert_eq!(count_tokens("", "cl100k_base"), 0);
        // A short ASCII string encodes to a small, non-zero token count.
        assert!(count_tokens("hello world", "cl100k_base") >= 1);
        // Unknown tokenizer names fall back to cl100k_base without panicking.
        assert_eq!(
            count_tokens("hello world", "does-not-exist"),
            count_tokens("hello world", "cl100k_base")
        );
        // o200k_base is selectable and yields a non-zero count.
        assert!(count_tokens("hello world", "o200k_base") >= 1);
    }
}
