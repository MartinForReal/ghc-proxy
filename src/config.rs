//! Configuration handling: config directory resolution, YAML config file
//! generation and loading, and default values.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Default VS Code version string sent in upstream request headers.
///
/// Kept in sync with the `engines.vscode` baseline of the latest
/// `microsoft/vscode-copilot-chat` release (see "Mimicking the Copilot client"
/// in the README for how to refresh these values).
pub const VSCODE_VERSION: &str = "1.130.0";
/// Default GitHub Copilot API version header value (`X-GitHub-Api-Version`),
/// matching the `X-GitHub-Api-Version` constant in the Copilot Chat client
/// source (`src/platform/networking/common/networking.ts`).
pub const API_VERSION: &str = "2025-05-01";
/// Default Copilot Chat plugin version string, matching the `version` field of
/// the latest `microsoft/vscode-copilot-chat` release.
pub const COPILOT_VERSION: &str = "0.48.1";
/// Config schema version used to detect when defaults/options changed and a
/// persisted config should be rewritten with migrated values.
///
/// Bumped to 6 to remove the retired GitHub Models settings from persisted
/// configuration.
pub const CONFIG_VERSION: u32 = 6;

/// Default model name that Claude "opus"/"sonnet" requests are mapped to.
///
/// The catalog carries `claude-opus-4.6` through `claude-opus-5`; this is the
/// newest generally-available one, and it matches 4.8 on every published
/// capability -- 1M context, 64k output, billing multiplier 1, vision.
pub const DEFAULT_OPUS: &str = "claude-opus-5";
/// Default model name that Claude "haiku" requests are mapped to.
pub const DEFAULT_HAIKU: &str = "claude-haiku-4.5";
/// Default model name that Gemini requests are mapped to.
///
/// The Gemini CLI ships its own model table and sends ids Copilot has never
/// served (`gemini-2.5-pro` by default), so without a mapping every request
/// from it is rejected outright.
pub const DEFAULT_GEMINI_PRO: &str = "gemini-3.1-pro-preview";
/// Default model name that Gemini "flash" requests are mapped to.
pub const DEFAULT_GEMINI_FLASH: &str = "gemini-3.6-flash";

/// GitHub OAuth client id used for the Device Flow (same id used by ghc-tunnel).
pub const GITHUB_CLIENT_ID: &str = "01ab8ac9400c4e429b23";

/// GitHub REST API base URL.
pub const GITHUB_API: &str = "https://api.github.com";

/// Default listen address.
pub const DEFAULT_ADDRESS: &str = "127.0.0.1";
/// Default listen port.
pub const DEFAULT_PORT: u16 = 8314;

/// Model name mapping table (exact + prefix).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelMappings {
    #[serde(default)]
    pub exact: BTreeMap<String, String>,
    #[serde(default)]
    pub prefix: BTreeMap<String, String>,
}

/// Parsed representation of `config.yaml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_loaded_config_version")]
    pub config_version: u32,
    #[serde(default = "default_address")]
    pub address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub debug: bool,
    #[serde(default = "default_account_type")]
    pub account_type: String,
    #[serde(default = "default_vscode_version")]
    pub vscode_version: String,
    #[serde(default = "default_api_version")]
    pub api_version: String,
    #[serde(default = "default_copilot_version")]
    pub copilot_version: String,
    #[serde(default)]
    pub model_mappings: ModelMappings,
    #[serde(default)]
    pub system_prompt_remove: Vec<String>,
    #[serde(default)]
    pub system_prompt_add: Vec<String>,
    #[serde(default)]
    pub tool_result_suffix_remove: Vec<String>,
    #[serde(default = "default_max_retries")]
    pub max_connection_retries: u32,
    /// Maximum seconds of silence allowed between two reads from an upstream
    /// response before the request is treated as dead. This bounds *silence*,
    /// not total duration, so long streaming answers are unaffected. `0`
    /// disables the timeout.
    ///
    /// Without it a half-open connection never errors and never ends: the
    /// request hangs forever, the client hangs with it, and the stream-
    /// interrupted handling never runs because no error is ever observed.
    #[serde(default = "default_read_timeout")]
    pub upstream_read_timeout_seconds: u64,
    /// When true, never route to the upstream `/v1/messages` endpoint; always
    /// translate Anthropic requests through the OpenAI chat completions API.
    #[serde(default)]
    pub redirect_anthropic: bool,
    /// When true, give `cache_control` breakpoints that carry no explicit `ttl`
    /// the one-hour tier instead of the five-minute default.
    ///
    /// Worth it only when turns regularly run longer than five minutes: an
    /// entry is written during prefill, so a turn that takes longer than the
    /// TTL outlives its own cache and the next turn pays a full cold prefill.
    /// The trade is that every write bills at the higher extended rate, which
    /// on a workload of many small incremental writes costs more than the
    /// occasional expiry it prevents.
    #[serde(default)]
    pub extend_cache_ttl: bool,
    /// When true, log the GitHub and Copilot tokens whenever they are resolved
    /// or refreshed. Useful for debugging; keep disabled in shared environments.
    #[serde(default)]
    pub show_token: bool,
    /// When true, fetch the latest VS Code version at startup and use it for
    /// the `Editor-Version` header (falling back to `vscode_version`).
    #[serde(default)]
    pub dynamic_vscode_version: bool,
    /// Check GitHub releases on startup and replace this binary when a newer
    /// version is available. Enabled by default, including for config files
    /// written before the key existed; disable with `auto_upgrade: false`,
    /// `--no-auto-upgrade`, or `GHC_PROXY_AUTO_UPGRADE=0`.
    ///
    /// The replacement takes effect on the next start; the running process
    /// keeps serving the old code until then.
    #[serde(default = "default_true")]
    pub auto_upgrade: bool,
    /// Minimum number of seconds between successive proxied requests. `None`
    /// disables rate limiting.
    #[serde(default)]
    pub rate_limit_seconds: Option<u64>,
    /// When rate limiting is active, wait for the interval to elapse instead of
    /// rejecting the request with HTTP 429.
    #[serde(default)]
    pub rate_limit_wait: bool,
    /// When true, require interactive approval (Enter on the console) before
    /// each proxied request is forwarded upstream.
    #[serde(default)]
    pub manual_approve: bool,
    /// Optional API key guarding the LLM endpoints. When set, every request to
    /// the OpenAI/Anthropic/Gemini-compatible endpoints must present a matching
    /// key (`Authorization: Bearer`, `x-api-key`, or `x-goog-api-key`). When
    /// `None`/empty, authentication is disabled and all requests are accepted.
    #[serde(default)]
    pub api_key: Option<String>,
}

fn default_address() -> String {
    DEFAULT_ADDRESS.to_string()
}
fn default_loaded_config_version() -> u32 {
    // Missing in old files; we treat that as legacy and migrate to
    // `CONFIG_VERSION` on load.
    0
}
fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_account_type() -> String {
    "individual".to_string()
}
fn default_vscode_version() -> String {
    VSCODE_VERSION.to_string()
}
fn default_api_version() -> String {
    API_VERSION.to_string()
}
fn default_copilot_version() -> String {
    COPILOT_VERSION.to_string()
}
fn default_max_retries() -> u32 {
    3
}
/// Deliberately far above the longest silence a healthy upstream produces.
///
/// An earlier 120s default was wrong: it was reasoned from the ~60s idle window
/// the upstream load balancer enforces, but measurement against the real API
/// showed Copilot buffers `input_json_delta` until a tool call's argument JSON
/// is complete and then flushes it in one burst. A 35,899-token answer went
/// **329.5 seconds** without emitting a byte, and the silence scales with
/// output size at roughly 9.5ms/token — so 120s would have aborted perfectly
/// healthy large tool calls. 15 minutes clears the worst measured case with
/// room to spare while still bounding a genuinely dead connection.
fn default_read_timeout() -> u64 {
    900
}

impl Default for Config {
    fn default() -> Self {
        Config {
            config_version: CONFIG_VERSION,
            address: default_address(),
            port: default_port(),
            debug: false,
            account_type: default_account_type(),
            vscode_version: default_vscode_version(),
            api_version: default_api_version(),
            copilot_version: default_copilot_version(),
            model_mappings: default_model_mappings(),
            system_prompt_remove: Vec::new(),
            system_prompt_add: Vec::new(),
            tool_result_suffix_remove: Vec::new(),
            max_connection_retries: default_max_retries(),
            upstream_read_timeout_seconds: default_read_timeout(),
            redirect_anthropic: false,
            extend_cache_ttl: false,
            show_token: false,
            dynamic_vscode_version: false,
            auto_upgrade: true,
            rate_limit_seconds: None,
            rate_limit_wait: false,
            manual_approve: false,
            api_key: None,
        }
    }
}

impl Config {
    /// Upstream Copilot API base URL, derived from the configured account type.
    pub fn copilot_base_url(&self) -> String {
        if self.account_type == "individual" {
            "https://api.githubcopilot.com".to_string()
        } else {
            format!("https://api.{}.githubcopilot.com", self.account_type)
        }
    }

    pub fn editor_plugin_version(&self) -> String {
        format!("copilot-chat/{}", self.copilot_version)
    }

    pub fn user_agent(&self) -> String {
        format!("GitHubCopilotChat/{}", self.copilot_version)
    }

}

/// Built-in default model mappings (mirrors ghc-tunnel defaults).
pub fn default_model_mappings() -> ModelMappings {
    let opus = DEFAULT_OPUS.to_string();
    let haiku = DEFAULT_HAIKU.to_string();
    let mut exact = BTreeMap::new();
    for k in [
        "opus", "sonnet", "opus4-7", "opus4-8", "opus5", "4-7[1m]", "4-8[1m]", "5[1m]",
    ] {
        exact.insert(k.to_string(), opus.clone());
    }
    exact.insert("haiku".to_string(), haiku.clone());

    // Every spelling of a Claude model resolves to the current best one. The
    // list is exhaustive rather than pattern-based because a request naming a
    // model that has since been superseded should still be served, and because
    // Anthropic writes the same version two ways (`4.8` and `4-8`).
    //
    // Full ids need no `[1m]` spelling of their own: the suffix is stripped
    // generically, and a prefix entry matches the suffixed id anyway. Only the
    // bare aliases above (`4-8[1m]`) still need listing, because stripping them
    // leaves `4-8`, which nothing else maps.
    let mut prefix = BTreeMap::new();
    for k in [
        "claude-sonnet-4-",
        "claude-opus-4.5-",
        "claude-opus-4.6-",
        "claude-opus-4.7-",
        "claude-opus-4.8-",
        "claude-opus-5-",
        "claude-opus-4-5-",
        "claude-opus-4-6-",
        "claude-opus-4-7-",
        "claude-opus-4-8-",
        "claude-opus-4.5",
        "claude-opus-4.6",
        "claude-opus-4.7",
        "claude-opus-4.8",
        "claude-opus-5",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-sonnet-4-7",
        "claude-sonnet-4-8",
        "claude-sonnet-4-6",
        "claude-sonnet-4-5",
        "claude-sonnet-4.6",
        "claude-sonnet-5-",
        "claude-sonnet-5",
    ] {
        prefix.insert(k.to_string(), opus.clone());
    }
    for k in ["claude-haiku-4.5-", "claude-haiku-4-5-"] {
        prefix.insert(k.to_string(), haiku.clone());
    }

    // The Gemini CLI resolves its own aliases client-side and puts a concrete
    // id on the wire, so the catch-all carries whatever generation it happens
    // to ship with. Flash spellings are listed separately because the longest
    // matching prefix wins, and answering a flash request with a pro model
    // would silently bill the caller for the wrong tier.
    prefix.insert("gemini-".to_string(), DEFAULT_GEMINI_PRO.to_string());
    for k in [
        "gemini-flash",
        "gemini-2.0-flash",
        "gemini-2.5-flash",
        "gemini-3-flash",
        "gemini-3.1-flash",
        "gemini-3.5-flash",
        "gemini-3.6-flash",
    ] {
        prefix.insert(k.to_string(), DEFAULT_GEMINI_FLASH.to_string());
    }

    ModelMappings { exact, prefix }
}

/// Returns the configuration directory: `%APPDATA%/ghc-tunnel` on Windows,
/// `~/.ghc-tunnel` elsewhere.
pub fn config_dir() -> PathBuf {
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("ghc-tunnel");
        }
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".ghc-tunnel")
}

/// Path to `config.yaml` within the configuration directory.
pub fn config_path() -> PathBuf {
    config_dir().join("config.yaml")
}

/// Renders a YAML scalar, quoting it only when necessary so that values such as
/// `4-7[1m]` or `claude-opus-4.7` round-trip cleanly through the YAML parser.
fn yaml_scalar(s: &str) -> String {
    let safe = s
        .chars()
        .next()
        .map(|c| c.is_ascii_alphabetic())
        .unwrap_or(false)
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '/'));
    if safe {
        s.to_string()
    } else {
        serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
    }
}

/// Renders a YAML list, using the inline `[]` form when empty.
fn yaml_list(items: &[String]) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    let mut out = String::from("\n");
    for item in items {
        out.push_str(&format!("  - {}\n", yaml_scalar(item)));
    }
    out.pop();
    out
}

/// Renders a fully-commented `config.yaml` document from the given config,
/// reflecting all of its current values (server settings, account type, header
/// versions, model mappings, content filters and retry settings).
pub fn render_config_yaml(cfg: &Config) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    s.push_str("# GitHub Copilot API Proxy Configuration\n");
    s.push_str("# ========================================\n\n");
    let _ = writeln!(s, "config_version: {}", cfg.config_version);
    s.push('\n');
    s.push_str("# Server Settings\n");
    let _ = writeln!(s, "address: {}", cfg.address);
    let _ = writeln!(s, "port: {}", cfg.port);
    let _ = writeln!(s, "debug: {}", cfg.debug);
    s.push('\n');
    s.push_str("# GitHub Copilot Account Type\n");
    s.push_str("# Options: \"individual\" | \"business\" | \"enterprise\"\n");
    let _ = writeln!(s, "account_type: {}", cfg.account_type);
    s.push('\n');
    s.push_str("# Header version strings (only affect request headers to Copilot API)\n");
    let _ = writeln!(s, "vscode_version: \"{}\"", cfg.vscode_version);
    let _ = writeln!(s, "api_version: \"{}\"", cfg.api_version);
    let _ = writeln!(s, "copilot_version: \"{}\"", cfg.copilot_version);
    s.push_str("# Check GitHub releases on startup and replace this binary when a newer\n");
    s.push_str("# version is available. Takes effect on the next start.\n");
    let _ = writeln!(s, "auto_upgrade: {}", cfg.auto_upgrade);
    s.push('\n');
    s.push_str("# Model Name Mappings\n");
    s.push_str("# Two types: exact (full name match) and prefix (starts-with match)\n");
    s.push_str("model_mappings:\n");
    s.push_str("  exact:\n");
    for (k, v) in &cfg.model_mappings.exact {
        let _ = writeln!(s, "    {}: {}", yaml_scalar(k), yaml_scalar(v));
    }
    s.push_str("  prefix:\n");
    for (k, v) in &cfg.model_mappings.prefix {
        let _ = writeln!(s, "    {}: {}", yaml_scalar(k), yaml_scalar(v));
    }
    s.push('\n');
    s.push_str("# Content Filtering\n");
    s.push_str("# system_prompt_remove: strings to strip from system prompts\n");
    s.push_str("# system_prompt_add: strings to append to system prompts\n");
    s.push_str("# tool_result_suffix_remove: trailing strings to strip from tool results\n");
    let _ = writeln!(
        s,
        "system_prompt_remove: {}",
        yaml_list(&cfg.system_prompt_remove)
    );
    let _ = writeln!(
        s,
        "system_prompt_add: {}",
        yaml_list(&cfg.system_prompt_add)
    );
    let _ = writeln!(
        s,
        "tool_result_suffix_remove: {}",
        yaml_list(&cfg.tool_result_suffix_remove)
    );
    s.push('\n');
    s.push_str("# Retry Settings\n");
    s.push_str("# Max retries for upstream connection errors (0 = no retries)\n");
    let _ = writeln!(s, "max_connection_retries: {}", cfg.max_connection_retries);
    s.push_str("# Max seconds of silence from an upstream response before it is treated as\n");
    s.push_str("# dead. Bounds silence, not total duration, so long streams are fine.\n");
    s.push_str("# 0 disables the timeout.\n");
    let _ = writeln!(
        s,
        "upstream_read_timeout_seconds: {}",
        cfg.upstream_read_timeout_seconds
    );
    if cfg.redirect_anthropic {
        s.push('\n');
        s.push_str(
            "# Always translate Anthropic requests through the OpenAI chat completions API\n",
        );
        let _ = writeln!(s, "redirect_anthropic: {}", cfg.redirect_anthropic);
    }
    if cfg.extend_cache_ttl {
        s.push('\n');
        s.push_str("# Promote cache_control breakpoints without an explicit ttl to the 1h tier.\n");
        s.push_str(
            "# Helps when turns run longer than 5m and expire their own cache; costs more\n",
        );
        s.push_str("# per write, so it loses on workloads of many small incremental writes.\n");
        let _ = writeln!(s, "extend_cache_ttl: {}", cfg.extend_cache_ttl);
    }
    if cfg.show_token
        || cfg.dynamic_vscode_version
        || cfg.rate_limit_seconds.is_some()
        || cfg.rate_limit_wait
        || cfg.manual_approve
        || cfg.api_key.is_some()
    {
        s.push('\n');
        s.push_str("# Diagnostics & request controls\n");
        if cfg.show_token {
            let _ = writeln!(s, "show_token: {}", cfg.show_token);
        }
        if cfg.dynamic_vscode_version {
            let _ = writeln!(s, "dynamic_vscode_version: {}", cfg.dynamic_vscode_version);
        }
        if let Some(secs) = cfg.rate_limit_seconds {
            let _ = writeln!(s, "rate_limit_seconds: {secs}");
        }
        if cfg.rate_limit_wait {
            let _ = writeln!(s, "rate_limit_wait: {}", cfg.rate_limit_wait);
        }
        if cfg.manual_approve {
            let _ = writeln!(s, "manual_approve: {}", cfg.manual_approve);
        }
        if let Some(ref key) = cfg.api_key {
            if !key.is_empty() {
                s.push_str(
                    "# API key required on LLM endpoints (Bearer / x-api-key / x-goog-api-key)\n",
                );
                let _ = writeln!(s, "api_key: {}", yaml_scalar(key));
            }
        }
    }
    s
}

/// Default `config.yaml` contents.
pub fn default_config_yaml() -> String {
    render_config_yaml(&Config::default())
}

/// Writes the given configuration to `config.yaml`, creating the configuration
/// directory if necessary and overwriting any existing file. Returns the path
/// that was written.
pub fn write_config(cfg: &Config) -> std::io::Result<PathBuf> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = config_path();
    std::fs::write(&path, render_config_yaml(cfg))?;
    Ok(path)
}

/// Ensures the config directory exists and writes the default `config.yaml`
/// if one does not already exist. Returns the path that was generated, or
/// `None` if a config already existed.
pub fn generate_default_config() -> std::io::Result<Option<PathBuf>> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = config_path();
    if path.exists() {
        return Ok(None);
    }
    std::fs::write(&path, default_config_yaml())?;
    Ok(Some(path))
}

/// Loads configuration from `config.yaml`, generating a default file first if
/// none exists. Falls back to built-in defaults on any parse error.
/// Environment variables can override config file values with the prefix `GHC_PROXY_`.
///
/// When `write_back_on_migration` is true, migrated config values are persisted
/// to disk. Otherwise migrations are applied only in-memory for this process.
pub fn load_config_with_options(write_back_on_migration: bool) -> Config {
    let path = config_path();
    if !path.exists() {
        if let Err(e) = generate_default_config() {
            tracing::warn!("Failed to generate default config: {e}");
        }
    }
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_norway::from_str::<Config>(&contents) {
            Ok(mut cfg) => {
                let mut needs_write_back = false;
                if cfg.model_mappings.exact.is_empty() && cfg.model_mappings.prefix.is_empty() {
                    cfg.model_mappings = default_model_mappings();
                    needs_write_back = true;
                }
                // A schema-version bump means this release added or changed
                // properties. Persist the re-rendered document unconditionally
                // so the file on disk gains the new keys with their defaults,
                // instead of waiting for an explicit `--update-config` run.
                let version_upgraded = migrate_config(&mut cfg);
                if version_upgraded {
                    needs_write_back = true;
                }
                if needs_write_back && (write_back_on_migration || version_upgraded) {
                    if let Err(e) = write_config(&cfg) {
                        tracing::warn!(
                            "Failed to persist migrated config to {}: {e}",
                            path.display()
                        );
                    }
                }
                tracing::info!("✓ Configuration loaded from: {}", path.display());
                cfg
            }
            Err(e) => {
                tracing::error!("Failed to parse config file at {}: {}", path.display(), e);
                tracing::warn!("Using default configuration values. Fix the config file to use custom settings.");
                let cfg = Config::default();
                if write_back_on_migration {
                    if let Err(write_err) = write_config(&cfg) {
                        tracing::warn!(
                            "Failed to rebuild corrupted config at {}: {write_err}",
                            path.display()
                        );
                    } else {
                        tracing::info!("✓ Rebuilt corrupted config file at {}", path.display());
                    }
                }
                cfg
            }
        },
        Err(e) => {
            tracing::debug!("Could not read config file at {}: {}", path.display(), e);
            tracing::info!("Using default configuration values");
            Config::default()
        }
    };

    // Apply environment variable overrides
    if let Ok(val) = std::env::var("GHC_PROXY_ADDRESS") {
        tracing::info!("✓ Overriding address from GHC_PROXY_ADDRESS: {}", val);
        cfg.address = val;
    }
    if let Ok(val) = std::env::var("GHC_PROXY_PORT") {
        if let Ok(port) = val.parse::<u16>() {
            tracing::info!("✓ Overriding port from GHC_PROXY_PORT: {}", port);
            cfg.port = port;
        } else {
            tracing::warn!(
                "Invalid GHC_PROXY_PORT value '{}': expected a number between 1-65535",
                val
            );
        }
    }
    if let Ok(val) = std::env::var("GHC_PROXY_DEBUG") {
        cfg.debug = val.eq_ignore_ascii_case("true") || val == "1";
        tracing::info!("✓ Overriding debug from GHC_PROXY_DEBUG: {}", cfg.debug);
    }
    if let Ok(val) = std::env::var("GHC_PROXY_ACCOUNT_TYPE") {
        tracing::info!(
            "✓ Overriding account_type from GHC_PROXY_ACCOUNT_TYPE: {}",
            val
        );
        cfg.account_type = val;
    }
    if let Ok(val) = std::env::var("GHC_PROXY_VSCODE_VERSION") {
        tracing::info!(
            "✓ Overriding vscode_version from GHC_PROXY_VSCODE_VERSION: {}",
            val
        );
        cfg.vscode_version = val;
    }
    if let Ok(val) = std::env::var("GHC_PROXY_API_VERSION") {
        tracing::info!(
            "✓ Overriding api_version from GHC_PROXY_API_VERSION: {}",
            val
        );
        cfg.api_version = val;
    }
    if let Ok(val) = std::env::var("GHC_PROXY_COPILOT_VERSION") {
        tracing::info!(
            "✓ Overriding copilot_version from GHC_PROXY_COPILOT_VERSION: {}",
            val
        );
        cfg.copilot_version = val;
    }
    if let Ok(val) = std::env::var("GHC_PROXY_MAX_CONNECTION_RETRIES") {
        if let Ok(retries) = val.parse::<u32>() {
            tracing::info!(
                "✓ Overriding max_connection_retries from GHC_PROXY_MAX_CONNECTION_RETRIES: {}",
                retries
            );
            cfg.max_connection_retries = retries;
        } else {
            tracing::warn!(
                "Invalid GHC_PROXY_MAX_CONNECTION_RETRIES value '{}': expected a positive number",
                val
            );
        }
    }
    if let Ok(val) = std::env::var("GHC_PROXY_UPSTREAM_READ_TIMEOUT") {
        match val.parse::<u64>() {
            Ok(secs) => {
                cfg.upstream_read_timeout_seconds = secs;
                tracing::info!(
                    "✓ Overriding upstream_read_timeout_seconds from GHC_PROXY_UPSTREAM_READ_TIMEOUT: {}",
                    secs
                );
            }
            Err(_) => tracing::warn!(
                "Invalid GHC_PROXY_UPSTREAM_READ_TIMEOUT value '{}': expected a number",
                val
            ),
        }
    }
    if let Ok(val) = std::env::var("GHC_PROXY_REDIRECT_ANTHROPIC") {
        cfg.redirect_anthropic = val.eq_ignore_ascii_case("true") || val == "1";
        tracing::info!(
            "✓ Overriding redirect_anthropic from GHC_PROXY_REDIRECT_ANTHROPIC: {}",
            cfg.redirect_anthropic
        );
    }
    if let Ok(val) = std::env::var("GHC_PROXY_EXTEND_CACHE_TTL") {
        cfg.extend_cache_ttl = val.eq_ignore_ascii_case("true") || val == "1";
        tracing::info!(
            "✓ Overriding extend_cache_ttl from GHC_PROXY_EXTEND_CACHE_TTL: {}",
            cfg.extend_cache_ttl
        );
    }
    if let Ok(val) = std::env::var("GHC_PROXY_SHOW_TOKEN") {
        cfg.show_token = val.eq_ignore_ascii_case("true") || val == "1";
        tracing::info!(
            "✓ Overriding show_token from GHC_PROXY_SHOW_TOKEN: {}",
            cfg.show_token
        );
    }
    if let Ok(val) = std::env::var("GHC_PROXY_DYNAMIC_VSCODE_VERSION") {
        cfg.dynamic_vscode_version = val.eq_ignore_ascii_case("true") || val == "1";
        tracing::info!(
            "✓ Overriding dynamic_vscode_version from GHC_PROXY_DYNAMIC_VSCODE_VERSION: {}",
            cfg.dynamic_vscode_version
        );
    }
    if let Ok(val) = std::env::var("GHC_PROXY_AUTO_UPGRADE") {
        cfg.auto_upgrade = val.eq_ignore_ascii_case("true") || val == "1";
        tracing::info!(
            "✓ Overriding auto_upgrade from GHC_PROXY_AUTO_UPGRADE: {}",
            cfg.auto_upgrade
        );
    }
    if let Ok(val) = std::env::var("GHC_PROXY_RATE_LIMIT_SECONDS") {
        match val.parse::<u64>() {
            Ok(secs) => {
                cfg.rate_limit_seconds = Some(secs);
                tracing::info!(
                    "✓ Overriding rate_limit_seconds from GHC_PROXY_RATE_LIMIT_SECONDS: {}",
                    secs
                );
            }
            Err(_) => tracing::warn!(
                "Invalid GHC_PROXY_RATE_LIMIT_SECONDS value '{}': expected a number",
                val
            ),
        }
    }
    if let Ok(val) = std::env::var("GHC_PROXY_RATE_LIMIT_WAIT") {
        cfg.rate_limit_wait = val.eq_ignore_ascii_case("true") || val == "1";
        tracing::info!(
            "✓ Overriding rate_limit_wait from GHC_PROXY_RATE_LIMIT_WAIT: {}",
            cfg.rate_limit_wait
        );
    }
    if let Ok(val) = std::env::var("GHC_PROXY_MANUAL_APPROVE") {
        cfg.manual_approve = val.eq_ignore_ascii_case("true") || val == "1";
        tracing::info!(
            "✓ Overriding manual_approve from GHC_PROXY_MANUAL_APPROVE: {}",
            cfg.manual_approve
        );
    }

    if let Ok(val) = std::env::var("GHC_PROXY_API_KEY") {
        let trimmed = val.trim();
        if trimmed.is_empty() {
            cfg.api_key = None;
        } else {
            cfg.api_key = Some(trimmed.to_string());
        }
        tracing::info!("API key auth enabled via GHC_PROXY_API_KEY");
    }

    cfg
}

/// Read-only configuration load used by default runtime paths.
pub fn load_config() -> Config {
    load_config_with_options(false)
}

/// Applies in-place config migrations from older schema versions.
///
/// Returns true when the persisted file predates `CONFIG_VERSION`. Because
/// missing keys are filled from the `serde` defaults at parse time, rewriting
/// the file after a version bump is what materializes properties introduced by
/// a newer release with their default values.
fn migrate_config(cfg: &mut Config) -> bool {
    if cfg.config_version >= CONFIG_VERSION {
        return false;
    }

    if cfg.config_version < 2 {
        // Ensure new aliases introduced with Opus 4.8 exist in legacy files.
        let opus = DEFAULT_OPUS.to_string();
        for k in ["opus4-8", "4-8[1m]"] {
            cfg.model_mappings.exact.insert(k.to_string(), opus.clone());
        }
        for k in [
            "claude-opus-4.8-",
            "claude-opus-4-8-",
            "claude-opus-4.8",
            "claude-opus-4-8",
            "claude-opus-4-8[1m]",
            "claude-sonnet-4-8",
        ] {
            cfg.model_mappings
                .prefix
                .insert(k.to_string(), opus.clone());
        }

        // If legacy default aliases still point at old built-in Opus values,
        // lift them to the current default.
        for k in ["opus", "sonnet", "opus4-7", "4-7[1m]"] {
            if let Some(v) = cfg.model_mappings.exact.get_mut(k) {
                if v == "claude-opus-4.7-1m" || v == "claude-opus-4.7" {
                    *v = opus.clone();
                }
            }
        }
    }

    if cfg.config_version < 4 {
        // Opus 5 and Sonnet 5 entered the catalog. Add their aliases so the new
        // names resolve, and change nothing else.
        //
        // Deliberately no "lift stale defaults to the new one" pass. A value
        // that equals the previous built-in default is indistinguishable from a
        // version the user pinned on purpose -- and pinning is common: a real
        // config in the wild carried `claude-opus-4-7: claude-opus-4.7` beside
        // `haiku: claude-opus-4.7`, both of which such a pass would silently
        // rewrite. Existing mappings keep pointing where they were told to;
        // `--setup` or `--default` is how a user asks for the new defaults.
        let opus = DEFAULT_OPUS.to_string();
        for k in ["opus5", "5[1m]"] {
            cfg.model_mappings
                .exact
                .entry(k.to_string())
                .or_insert_with(|| opus.clone());
        }
        for k in [
            "claude-opus-5-",
            "claude-opus-5",
            "claude-opus-5[1m]",
            "claude-sonnet-5-",
            "claude-sonnet-5",
            "claude-sonnet-4.6",
        ] {
            cfg.model_mappings
                .prefix
                .entry(k.to_string())
                .or_insert_with(|| opus.clone());
        }
    }

    if cfg.config_version < 5 {
        // The Gemini CLI sends ids Copilot never served, so files written
        // before these mappings existed reject every Gemini request. Only
        // missing keys are added; an id the user already pointed somewhere
        // stays pointed there.
        for k in [
            "gemini-flash",
            "gemini-2.0-flash",
            "gemini-2.5-flash",
            "gemini-3-flash",
            "gemini-3.1-flash",
            "gemini-3.5-flash",
            "gemini-3.6-flash",
        ] {
            cfg.model_mappings
                .prefix
                .entry(k.to_string())
                .or_insert_with(|| DEFAULT_GEMINI_FLASH.to_string());
        }
        cfg.model_mappings
            .prefix
            .entry("gemini-".to_string())
            .or_insert_with(|| DEFAULT_GEMINI_PRO.to_string());
    }

    // Any older schema version is lifted to the current one; the caller
    // persists the re-rendered document so newly introduced properties appear
    // on disk with their defaults.
    cfg.config_version = CONFIG_VERSION;
    true
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_upgrade_is_on_by_default_including_for_legacy_configs() {
        assert!(Config::default().auto_upgrade);

        // Config files written before the key existed must also opt in, which
        // is what `#[serde(default)]` would have got wrong.
        let legacy = "address: 127.0.0.1\nport: 8314\nmax_connection_retries: 3\n";
        let cfg: Config = serde_norway::from_str(legacy).unwrap();
        assert!(cfg.auto_upgrade);

        // An explicit opt-out is still honoured, and survives a round trip
        // through the generated config file.
        let off: Config = serde_norway::from_str("auto_upgrade: false\n").unwrap();
        assert!(!off.auto_upgrade);
        let rendered: Config = serde_norway::from_str(&render_config_yaml(&off)).unwrap();
        assert!(!rendered.auto_upgrade);
    }

    #[test]
    fn read_timeout_defaults_are_sane() {
        // Silence, not total duration — a long stream must never be cut off by
        // this, but a genuinely dead connection must be.
        let cfg = Config::default();
        assert_eq!(cfg.upstream_read_timeout_seconds, 900);
        // The longest silence measured against the real upstream was 329.5s,
        // while a tool call's argument JSON was buffered. The default must
        // clear that with margin or it aborts healthy requests.
        assert!(cfg.upstream_read_timeout_seconds > 330);
        // The rendered config round-trips the value, including the 0 (disabled)
        // case, so operators can turn it off.
        let off = Config {
            upstream_read_timeout_seconds: 0,
            ..Default::default()
        };
        let parsed: Config = serde_norway::from_str(&render_config_yaml(&off)).unwrap();
        assert_eq!(parsed.upstream_read_timeout_seconds, 0);
        let parsed: Config = serde_norway::from_str(&render_config_yaml(&cfg)).unwrap();
        assert_eq!(parsed.upstream_read_timeout_seconds, 900);
    }

    #[test]
    fn read_timeout_missing_from_legacy_config_uses_default() {
        // Existing config files predate the key and must keep working.
        let legacy = "address: 127.0.0.1\nport: 8314\nmax_connection_retries: 3\n";
        let cfg: Config = serde_norway::from_str(legacy).unwrap();
        assert_eq!(cfg.upstream_read_timeout_seconds, 900);
    }

    #[test]
    fn default_rendered_config_reparses() {
        // The default document is written on first run and on corruption rebuild;
        // it must always re-parse.
        let yaml = default_config_yaml();
        serde_norway::from_str::<Config>(&yaml).expect("default config re-parses");
    }

    #[test]
    fn legacy_config_gains_new_properties_with_defaults() {
        // A file written by an older release omits properties added since. They
        // parse as their defaults, and the migration reports that the document
        // must be rewritten so the new keys are materialized on disk.
        let yaml = "config_version: 1\naddress: 0.0.0.0\nport: 9000\n";
        let mut cfg: Config = serde_norway::from_str(yaml).expect("legacy config parses");
        assert!(migrate_config(&mut cfg));
        assert_eq!(cfg.config_version, CONFIG_VERSION);
        // User-set values survive the migration...
        assert_eq!(cfg.address, "0.0.0.0");
        assert_eq!(cfg.port, 9000);
        // ...while properties the old file never had take their defaults.
        assert_eq!(cfg.max_connection_retries, default_max_retries());
        assert_eq!(cfg.api_version, API_VERSION);
        // The re-rendered document carries them forward.
        let rendered = render_config_yaml(&cfg);
        assert!(rendered.contains(&format!("config_version: {CONFIG_VERSION}")));
        assert!(!rendered.contains("github_models:"));
    }

    #[test]
    fn current_version_config_is_not_rewritten() {
        // Nothing to migrate: an up-to-date file must not be rewritten on load.
        let mut cfg = Config::default();
        assert!(!migrate_config(&mut cfg));
        assert_eq!(cfg.config_version, CONFIG_VERSION);
    }

    #[test]
    fn opus_alias_migration_only_applies_to_pre_v2_files() {
        // The v2 alias backfill must not clobber customizations in files that
        // are already at v2 or newer.
        let mut cfg = Config::default();
        cfg.model_mappings
            .exact
            .insert("opus".to_string(), "my-model".to_string());
        assert!(!migrate_config(&mut cfg));
        assert_eq!(
            cfg.model_mappings.exact.get("opus").map(String::as_str),
            Some("my-model")
        );
    }

    #[test]
    fn opus_5_migration_adds_aliases_without_touching_existing_ones() {
        let yaml = "config_version: 3\n";
        let mut cfg: Config = serde_norway::from_str(yaml).expect("v3 config parses");
        // A hand-tuned file: version-specific pins, and a tier alias pointed
        // somewhere the defaults would never put it. Both shapes were observed
        // in a real config, and a migration that "lifts stale defaults" rewrites
        // both, because a pin and a stale default look identical.
        cfg.model_mappings
            .exact
            .insert("opus".to_string(), "claude-opus-4.8".to_string());
        cfg.model_mappings
            .exact
            .insert("haiku".to_string(), "claude-opus-4.7".to_string());
        cfg.model_mappings
            .prefix
            .insert("claude-opus-4-7".to_string(), "claude-opus-4.7".to_string());
        cfg.model_mappings.prefix.insert(
            "claude-sonnet-4.6".to_string(),
            "pinned-by-hand".to_string(),
        );

        assert!(migrate_config(&mut cfg));
        assert_eq!(cfg.config_version, CONFIG_VERSION);

        // Nothing already in the file is rewritten, including a key this
        // migration would otherwise seed.
        for (k, want) in [("opus", "claude-opus-4.8"), ("haiku", "claude-opus-4.7")] {
            assert_eq!(
                cfg.model_mappings.exact.get(k).map(String::as_str),
                Some(want),
                "exact alias {k} must not be rewritten"
            );
        }
        for (k, want) in [
            ("claude-opus-4-7", "claude-opus-4.7"),
            ("claude-sonnet-4.6", "pinned-by-hand"),
        ] {
            assert_eq!(
                cfg.model_mappings.prefix.get(k).map(String::as_str),
                Some(want),
                "prefix {k} must not be rewritten"
            );
        }

        // Names that were not in the file are seeded with the new default.
        assert_eq!(
            cfg.model_mappings.exact.get("opus5").map(String::as_str),
            Some(DEFAULT_OPUS)
        );
        assert_eq!(
            cfg.model_mappings
                .prefix
                .get("claude-opus-5")
                .map(String::as_str),
            Some(DEFAULT_OPUS)
        );
        assert_eq!(
            cfg.model_mappings
                .prefix
                .get("claude-sonnet-5")
                .map(String::as_str),
            Some(DEFAULT_OPUS)
        );
    }

    #[test]
    fn migration_seeds_gemini_mappings_without_touching_pins() {
        let yaml = "config_version: 4\n";
        let mut cfg: Config = serde_norway::from_str(yaml).expect("v4 config parses");
        cfg.model_mappings
            .prefix
            .insert("gemini-2.5-flash".to_string(), "pinned-by-hand".to_string());

        assert!(migrate_config(&mut cfg));
        assert_eq!(cfg.config_version, CONFIG_VERSION);

        assert_eq!(
            cfg.model_mappings
                .prefix
                .get("gemini-2.5-flash")
                .map(String::as_str),
            Some("pinned-by-hand"),
            "an id the user pointed somewhere stays pointed there"
        );
        assert_eq!(
            cfg.model_mappings.prefix.get("gemini-").map(String::as_str),
            Some(DEFAULT_GEMINI_PRO)
        );
        assert_eq!(
            cfg.model_mappings
                .prefix
                .get("gemini-3.5-flash")
                .map(String::as_str),
            Some(DEFAULT_GEMINI_FLASH)
        );
    }
}
