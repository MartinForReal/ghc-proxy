//! Model name translation based on exact and prefix mappings.

use crate::config::ModelMappings;

/// Suffix a client appends to a model id to ask for its 1M-token context
/// variant. This is the wire form of Claude Desktop's `supports1m` model
/// option: the picker offers `<id>` and `<id>[1m]` as separate entries.
pub const CONTEXT_1M_SUFFIX: &str = "[1m]";

/// Splits a trailing [`CONTEXT_1M_SUFFIX`] off a model id, reporting whether it
/// was present. Upstream has no separate 1M model -- the variant is the same id
/// plus the `context-1m-2025-08-07` beta.
pub fn split_context_1m(model: &str) -> (&str, bool) {
    match model.strip_suffix(CONTEXT_1M_SUFFIX) {
        Some(base) => (base, true),
        None => (model, false),
    }
}

/// Translates an incoming model name using the configured mappings.
///
/// Exact matches take priority over prefix matches. When several prefixes
/// match, the longest (most specific) one wins, so a prefix like
/// `claude-opus-4.8-` takes precedence over a shorter `claude-opus-4.8`.
/// If nothing matches the original name is returned unchanged.
pub fn translate(mappings: &ModelMappings, model: &str) -> String {
    if let Some(target) = lookup(mappings, model) {
        return target;
    }
    // The full id is tried first so a hand-written `[1m]` mapping still wins.
    // Falling back to the base id lets any model carry the suffix instead of
    // only the handful spelled out in the default table.
    let (base, is_1m) = split_context_1m(model);
    if is_1m {
        return lookup(mappings, base).unwrap_or_else(|| base.to_string());
    }
    model.to_string()
}

/// Resolves one id against the mappings, exact table first.
fn lookup(mappings: &ModelMappings, model: &str) -> Option<String> {
    if let Some(target) = mappings.exact.get(model) {
        return Some(target.clone());
    }
    let mut best: Option<(&String, &String)> = None;
    for (prefix, target) in &mappings.prefix {
        if model.starts_with(prefix.as_str())
            && best.is_none_or(|(best_prefix, _)| prefix.len() > best_prefix.len())
        {
            best = Some((prefix, target));
        }
    }
    best.map(|(_, target)| target.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        default_model_mappings, DEFAULT_GEMINI_FLASH, DEFAULT_GEMINI_PRO, DEFAULT_HAIKU,
        DEFAULT_OPUS,
    };

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

    /// The Gemini CLI ships its own model table, so it asks for ids Copilot has
    /// never served. Without these mappings every request from it is rejected.
    #[test]
    fn gemini_ids_resolve_to_a_served_model() {
        let m = default_model_mappings();
        assert_eq!(translate(&m, "gemini-2.5-pro"), DEFAULT_GEMINI_PRO);
        assert_eq!(translate(&m, "gemini-3-pro-preview"), DEFAULT_GEMINI_PRO);
        assert_eq!(translate(&m, "gemini-9-pro-unheard-of"), DEFAULT_GEMINI_PRO);
    }

    /// The catch-all `gemini-` prefix must not swallow the flash tier: longest
    /// match wins, so a flash request stays on a flash model.
    #[test]
    fn gemini_flash_stays_on_the_flash_tier() {
        let m = default_model_mappings();
        for id in [
            "gemini-2.5-flash",
            "gemini-3-flash-preview",
            "gemini-3.5-flash",
            "gemini-3.1-flash-lite",
        ] {
            assert_eq!(translate(&m, id), DEFAULT_GEMINI_FLASH, "{id}");
        }
    }

    /// `gemma-*` is a different family Copilot does not serve; the Gemini
    /// catch-all must not claim it and hide that with a wrong answer.
    #[test]
    fn gemma_is_left_alone() {
        let m = default_model_mappings();
        assert_eq!(translate(&m, "gemma-4-31b-it"), "gemma-4-31b-it");
    }

    #[test]
    fn context_1m_suffix_is_split_off() {
        assert_eq!(
            split_context_1m("claude-opus-5[1m]"),
            ("claude-opus-5", true)
        );
        assert_eq!(split_context_1m("claude-opus-5"), ("claude-opus-5", false));
        // Only a trailing suffix counts.
        assert_eq!(split_context_1m("[1m]-model"), ("[1m]-model", false));
    }

    /// The suffix used to be a handful of hand-written alias entries, so any id
    /// missing from that table answered 404 instead of resolving.
    #[test]
    fn any_model_accepts_the_1m_suffix() {
        let m = default_model_mappings();
        assert_eq!(translate(&m, "claude-haiku-4.5[1m]"), DEFAULT_HAIKU);
        assert_eq!(translate(&m, "sonnet[1m]"), DEFAULT_OPUS);
        assert_eq!(translate(&m, "gpt-4o[1m]"), "gpt-4o");
    }

    /// The default table used to spell out a `[1m]` entry per full id. They
    /// were dropped, so pin the two routes that make them unnecessary.
    #[test]
    fn full_ids_resolve_with_the_1m_suffix_without_their_own_entry() {
        let m = default_model_mappings();
        for id in [
            "claude-opus-4-6[1m]",
            "claude-opus-4-7[1m]",
            "claude-opus-4-8[1m]",
            "claude-opus-5[1m]",
            "claude-opus-4.8[1m]",
        ] {
            assert_eq!(translate(&m, id), DEFAULT_OPUS, "{id}");
        }
        // The bare aliases still need their own entries: stripping `4-8[1m]`
        // leaves `4-8`, which nothing maps.
        for alias in ["4-7[1m]", "4-8[1m]", "5[1m]"] {
            assert_eq!(translate(&m, alias), DEFAULT_OPUS, "{alias}");
        }
    }

    #[test]
    fn explicit_1m_mapping_beats_the_base_id() {
        let mut m = default_model_mappings();
        m.exact
            .insert("claude-opus-5[1m]".to_string(), "pinned-1m".to_string());
        assert_eq!(translate(&m, "claude-opus-5[1m]"), "pinned-1m");
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
