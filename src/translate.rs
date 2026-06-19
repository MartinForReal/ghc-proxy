//! Model name translation based on exact and prefix mappings.

use crate::config::ModelMappings;

/// Translates an incoming model name using the configured mappings.
///
/// Exact matches take priority over prefix matches. When several prefixes
/// match, the longest (most specific) one wins, so a prefix like
/// `claude-opus-4.8-` takes precedence over a shorter `claude-opus-4.8`.
/// If nothing matches the original name is returned unchanged.
pub fn translate(mappings: &ModelMappings, model: &str) -> String {
    if let Some(target) = mappings.exact.get(model) {
        return target.clone();
    }
    let mut best: Option<(&String, &String)> = None;
    for (prefix, target) in &mappings.prefix {
        if model.starts_with(prefix.as_str())
            && best.is_none_or(|(best_prefix, _)| prefix.len() > best_prefix.len())
        {
            best = Some((prefix, target));
        }
    }
    if let Some((_, target)) = best {
        return target.clone();
    }
    model.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{default_model_mappings, DEFAULT_HAIKU, DEFAULT_OPUS};

    #[test]
    fn exact_mapping_wins() {
        let m = default_model_mappings();
        assert_eq!(translate(&m, "opus"), DEFAULT_OPUS);
        assert_eq!(translate(&m, "haiku"), DEFAULT_HAIKU);
    }

    #[test]
    fn prefix_mapping_applies() {
        let m = default_model_mappings();
        assert_eq!(translate(&m, "claude-sonnet-4-20250101"), DEFAULT_OPUS);
        assert_eq!(translate(&m, "claude-haiku-4.5-20250101"), DEFAULT_HAIKU);
    }

    #[test]
    fn unmapped_passthrough() {
        let m = default_model_mappings();
        assert_eq!(translate(&m, "gpt-4o"), "gpt-4o");
    }

    #[test]
    fn longest_prefix_wins() {
        let mut m = default_model_mappings();
        // Give the shorter prefix a different valid target so the assertion
        // can observe that the longer `claude-opus-4.8-` prefix wins.
        m.prefix
            .insert("claude-opus-4.8".to_string(), DEFAULT_HAIKU.to_string());
        assert_eq!(translate(&m, "claude-opus-4.8-20250101"), DEFAULT_OPUS);
    }
}
