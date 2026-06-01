//! Model name translation based on exact and prefix mappings.

use crate::config::ModelMappings;

/// Translates an incoming model name using the configured mappings.
///
/// Exact matches take priority over prefix matches. If nothing matches the
/// original name is returned unchanged.
pub fn translate(mappings: &ModelMappings, model: &str) -> String {
    if let Some(target) = mappings.exact.get(model) {
        return target.clone();
    }
    for (prefix, target) in &mappings.prefix {
        if model.starts_with(prefix.as_str()) {
            return target.clone();
        }
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
}
