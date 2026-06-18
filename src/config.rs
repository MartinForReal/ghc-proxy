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
pub const VSCODE_VERSION: &str = "1.123.0";
/// Default GitHub Copilot API version header value (`X-GitHub-Api-Version`),
/// matching the `X-GitHub-Api-Version` constant in the Copilot Chat client
/// source (`src/platform/networking/common/networking.ts`).
pub const API_VERSION: &str = "2025-05-01";
/// Default Copilot Chat plugin version string, matching the `version` field of
/// the latest `microsoft/vscode-copilot-chat` release.
pub const COPILOT_VERSION: &str = "0.48.1";
/// Config schema version used to detect when defaults/options changed and a
/// persisted config should be rewritten with migrated values.
pub const CONFIG_VERSION: u32 = 2;

/// Default model name that Claude "opus"/"sonnet" requests are mapped to.
pub const DEFAULT_OPUS: &str = "claude-opus-4.8";
/// Default model name that Claude "haiku" requests are mapped to.
pub const DEFAULT_HAIKU: &str = "claude-haiku-4.5";

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
    /// When true, never route to the upstream `/v1/messages` endpoint; always
    /// translate Anthropic requests through the OpenAI chat completions API.
    #[serde(default)]
    pub redirect_anthropic: bool,
    /// When true, log the GitHub and Copilot tokens whenever they are resolved
    /// or refreshed. Useful for debugging; keep disabled in shared environments.
    #[serde(default)]
    pub show_token: bool,
    /// When true, fetch the latest VS Code version at startup and use it for
    /// the `Editor-Version` header (falling back to `vscode_version`).
    #[serde(default)]
    pub dynamic_vscode_version: bool,
    /// When true, check GitHub releases and auto-upgrade this binary when a
    /// newer version is available.
    #[serde(default)]
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
            redirect_anthropic: false,
            show_token: false,
            dynamic_vscode_version: false,
            auto_upgrade: false,
            rate_limit_seconds: None,
            rate_limit_wait: false,
            manual_approve: false,
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
    for k in ["opus", "sonnet", "opus4-7", "opus4-8", "4-7[1m]", "4-8[1m]"] {
        exact.insert(k.to_string(), opus.clone());
    }
    exact.insert("haiku".to_string(), haiku.clone());

    let mut prefix = BTreeMap::new();
    for k in [
        "claude-sonnet-4-",
        "claude-opus-4.5-",
        "claude-opus-4.6-",
        "claude-opus-4.7-",
        "claude-opus-4.8-",
        "claude-opus-4-5-",
        "claude-opus-4-6-",
        "claude-opus-4-7-",
        "claude-opus-4-8-",
        "claude-opus-4.5",
        "claude-opus-4.6",
        "claude-opus-4.7",
        "claude-opus-4.8",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-opus-4-6[1m]",
        "claude-opus-4-7[1m]",
        "claude-opus-4-8[1m]",
        "claude-sonnet-4-7",
        "claude-sonnet-4-8",
        "claude-sonnet-4-6",
        "claude-sonnet-4-5",
    ] {
        prefix.insert(k.to_string(), opus.clone());
    }
    for k in ["claude-haiku-4.5-", "claude-haiku-4-5-"] {
        prefix.insert(k.to_string(), haiku.clone());
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
    if cfg.redirect_anthropic {
        s.push('\n');
        s.push_str(
            "# Always translate Anthropic requests through the OpenAI chat completions API\n",
        );
        let _ = writeln!(s, "redirect_anthropic: {}", cfg.redirect_anthropic);
    }
    if cfg.show_token
        || cfg.dynamic_vscode_version
        || cfg.rate_limit_seconds.is_some()
        || cfg.rate_limit_wait
        || cfg.manual_approve
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
                if migrate_config(&mut cfg) {
                    needs_write_back = true;
                }
                if needs_write_back && write_back_on_migration {
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
    if let Ok(val) = std::env::var("GHC_PROXY_REDIRECT_ANTHROPIC") {
        cfg.redirect_anthropic = val.eq_ignore_ascii_case("true") || val == "1";
        tracing::info!(
            "✓ Overriding redirect_anthropic from GHC_PROXY_REDIRECT_ANTHROPIC: {}",
            cfg.redirect_anthropic
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

    cfg
}

/// Read-only configuration load used by default runtime paths.
pub fn load_config() -> Config {
    load_config_with_options(false)
}

/// Applies in-place config migrations from older schema versions.
/// Returns true when the config was modified and should be written back.
fn migrate_config(cfg: &mut Config) -> bool {
    let mut changed = false;

    if cfg.config_version < CONFIG_VERSION {
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

        cfg.config_version = CONFIG_VERSION;
        changed = true;
    }

    changed
}
