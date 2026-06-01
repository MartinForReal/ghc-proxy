//! Configuration handling: config directory resolution, YAML config file
//! generation and loading, and default values.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Default VS Code version string sent in upstream request headers.
pub const VSCODE_VERSION: &str = "1.93.0";
/// Default GitHub Copilot API version header value.
pub const API_VERSION: &str = "2025-04-01";
/// Default Copilot Chat plugin version string.
pub const COPILOT_VERSION: &str = "0.26.7";

/// Default model name that Claude "opus"/"sonnet" requests are mapped to.
pub const DEFAULT_OPUS: &str = "claude-opus-4.7-1m";
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
}

fn default_address() -> String {
    DEFAULT_ADDRESS.to_string()
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
    for k in ["opus", "sonnet", "opus4-7", "4-7[1m]"] {
        exact.insert(k.to_string(), opus.clone());
    }
    exact.insert("haiku".to_string(), haiku.clone());

    let mut prefix = BTreeMap::new();
    for k in [
        "claude-sonnet-4-",
        "claude-opus-4.5-",
        "claude-opus-4.6-",
        "claude-opus-4.7-",
        "claude-opus-4-5-",
        "claude-opus-4-6-",
        "claude-opus-4-7-",
        "claude-opus-4.5",
        "claude-opus-4.6",
        "claude-opus-4.7",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-6[1m]",
        "claude-opus-4-7[1m]",
        "claude-sonnet-4-7",
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

/// Default `config.yaml` contents.
pub fn default_config_yaml() -> String {
    format!(
        r#"# GitHub Copilot API Proxy Configuration
# ========================================

# Server Settings
address: {addr}
port: {port}
debug: false

# GitHub Copilot Account Type
# Options: "individual" | "business" | "enterprise"
account_type: individual

# Header version strings (only affect request headers to Copilot API)
vscode_version: "{vscode}"
api_version: "{api}"
copilot_version: "{copilot}"

# Model Name Mappings
# Two types: exact (full name match) and prefix (starts-with match)
model_mappings:
  exact:
    opus: {opus}
    sonnet: {opus}
    opus4-7: {opus}
    "4-7[1m]": {opus}
    haiku: {haiku}
  prefix:
    claude-sonnet-4-: {opus}
    claude-opus-4.5-: {opus}
    claude-opus-4.6-: {opus}
    claude-opus-4.7-: {opus}
    claude-opus-4-5-: {opus}
    claude-opus-4-6-: {opus}
    claude-opus-4-7-: {opus}
    "claude-opus-4.5": {opus}
    "claude-opus-4.6": {opus}
    "claude-opus-4.7": {opus}
    "claude-opus-4-6": {opus}
    "claude-opus-4-7": {opus}
    "claude-opus-4-6[1m]": {opus}
    "claude-opus-4-7[1m]": {opus}
    claude-sonnet-4-7: {opus}
    claude-sonnet-4-6: {opus}
    claude-sonnet-4-5: {opus}
    claude-haiku-4.5-: {haiku}
    claude-haiku-4-5-: {haiku}

# Content Filtering
# system_prompt_remove: strings to strip from system prompts
# system_prompt_add: strings to append to system prompts
# tool_result_suffix_remove: trailing strings to strip from tool results
system_prompt_remove: []
system_prompt_add: []
tool_result_suffix_remove: []

# Retry Settings
# Max retries for upstream connection errors (0 = no retries)
max_connection_retries: 3
"#,
        addr = DEFAULT_ADDRESS,
        port = DEFAULT_PORT,
        vscode = VSCODE_VERSION,
        api = API_VERSION,
        copilot = COPILOT_VERSION,
        opus = DEFAULT_OPUS,
        haiku = DEFAULT_HAIKU,
    )
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
pub fn load_config() -> Config {
    let path = config_path();
    if !path.exists() {
        if let Err(e) = generate_default_config() {
            tracing::warn!("Failed to generate default config: {e}");
        }
    }
    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_yaml::from_str::<Config>(&contents) {
            Ok(mut cfg) => {
                if cfg.model_mappings.exact.is_empty() && cfg.model_mappings.prefix.is_empty() {
                    cfg.model_mappings = default_model_mappings();
                }
                tracing::info!("Loaded configuration from: {}", path.display());
                cfg
            }
            Err(e) => {
                tracing::warn!("Error loading config: {e}");
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}
