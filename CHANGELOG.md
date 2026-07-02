# Changelog

All notable changes to this project will be documented in this file.

## [1.2.1] - 2026-07-02

### Fixed
- Fixed `/v1/messages` (Claude Code) incorrectly routing to GitHub Models when model name contained `/`
  - `/v1/messages` uses Anthropic Messages API which is not supported by GitHub Models
  - Now always routes to Copilot upstream regardless of model format
- Fixed `/v1/responses` (Codex) incorrectly routing to GitHub Models
  - Added explicit validation to reject GitHub Models models with error message recommending `/v1/chat/completions`
- Fixed 7 clippy warnings:
  - Collapsed nested `if-let` in `anthropic.rs` (lines 683-688)
  - Replaced `sort_by` with `sort_by_key` in `server.rs` (lines 1838, 1843)
  - Replaced `len() > 0` with `!is_empty()` in `server.rs` (line 1849)
  - Added `#[allow(dead_code)]` for unused utility functions: `is_prompt_cache_eligible`, `extract_prompt_cache_hit`, `filter_tools_by_frequency`

### Updated
- **Documentation**: Updated all docs to reflect current implementation
  - README.md: Added CLI options reference, version numbers, and architecture overview
  - docs/configuration.md: Added `config_version` and `auto_upgrade` fields documentation
  - docs/getting-started.md: Added startup endpoint details
  - docs/api.md: Added metrics, audit, and reload endpoints with adaptive-thinking behavior notes
  - docs/claude-code.md: Clarified API key "add if missing, don't overwrite" behavior

### Performance
- Confirmed model mapping lookups are in-memory with O(log n) BTreeMap performance (~5-10µs per request)
  - No optimization needed; current implementation is production-ready

## [1.2.0] - Previous Release

Previous releases information would go here.
