//! Model name translation based on exact and prefix mappings.

use crate::config::ModelMappings;

/// Translates an incoming model name using the configured mappings.
///
/// Exact matches take priority over prefix matches. When several prefixes
/// match, the longest (most specific) one wins, so an entry like
/// `claude-opus-4.7-xhigh` takes precedence over a shorter `claude-opus-4.7`.
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
    use crate::config::default_model_mappings;

    #[test]
    fn exact_mapping_wins() {
        let m = default_model_mappings();
        assert_eq!(translate(&m, "opus"), "claude-opus-4.7-1m");
        assert_eq!(translate(&m, "haiku"), "claude-haiku-4.5");
    }

    #[test]
    fn prefix_mapping_applies() {
        let m = default_model_mappings();
        assert_eq!(
            translate(&m, "claude-sonnet-4-20250101"),
            "claude-opus-4.7-1m"
        );
        assert_eq!(
            translate(&m, "claude-haiku-4.5-20250101"),
            "claude-haiku-4.5"
        );
    }

    #[test]
    fn unmapped_passthrough() {
        let m = default_model_mappings();
        assert_eq!(translate(&m, "gpt-4o"), "gpt-4o");
    }

    #[test]
    fn longest_prefix_wins() {
        let mut m = default_model_mappings();
        // A more specific prefix must take precedence over the shorter
        // `claude-opus-4.7` entry that would otherwise match first in sorted
        // key order.
        m.prefix.insert(
            "claude-opus-4.7-xhigh".to_string(),
            "claude-opus-4.7-xhigh".to_string(),
        );
        assert_eq!(
            translate(&m, "claude-opus-4.7-xhigh"),
            "claude-opus-4.7-xhigh"
        );
    }
}
