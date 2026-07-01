//! ghc-proxy binary entry point: CLI handling and server startup.

use ghc_proxy::{auth, config, server, state::AppState};
use std::net::SocketAddr;
use std::sync::Arc;

mod setup;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Parsed command-line options.
#[derive(Debug, Default)]
struct Cli {
    setup: bool,
    claudecode: bool,
    codex: bool,
    gemini: bool,
    defaults: bool,
    port: Option<u16>,
    address: Option<String>,
    debug: Option<bool>,
    account_type: Option<String>,
    config: bool,
    version: bool,
    help: bool,
    auth: bool,
    info: bool,
    check_usage: bool,
    json: bool,
    show_token: bool,
    rate_limit: Option<u64>,
    wait: bool,
    manual: bool,
    fetch_version: Option<bool>,
    update_config: bool,
    auto_upgrade: Option<bool>,
}

fn parse_args() -> Cli {
    let mut cli = Cli::default();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-s" | "--setup" => cli.setup = true,
            "--claudecode" => cli.claudecode = true,
            "--codex" => cli.codex = true,
            "--gemini" => cli.gemini = true,
            "-d" | "--default" => cli.defaults = true,
            "-c" | "--config" => cli.config = true,
            "-v" | "--version" => cli.version = true,
            "-h" | "--help" => cli.help = true,
            "auth" | "--auth" => cli.auth = true,
            "info" | "debug" | "--info" => cli.info = true,
            "check-usage" | "--check-usage" => cli.check_usage = true,
            "--json" => cli.json = true,
            "--show-token" => cli.show_token = true,
            "--wait" => cli.wait = true,
            "--manual" => cli.manual = true,
            "--fetch-version" => cli.fetch_version = Some(true),
            "--no-fetch-version" => cli.fetch_version = Some(false),
            "--update-config" => cli.update_config = true,
            "--auto-upgrade" => cli.auto_upgrade = Some(true),
            "--no-auto-upgrade" => cli.auto_upgrade = Some(false),
            "--rate-limit" => {
                if let Some(v) = args.next() {
                    cli.rate_limit = v.parse().ok();
                }
            }
            "-p" | "--port" => {
                if let Some(v) = args.next() {
                    cli.port = v.parse().ok();
                }
            }
            "-a" | "--address" => {
                cli.address = args.next();
            }
            "--debug" => {
                cli.debug = Some(true);
            }
            "--no-debug" => {
                cli.debug = Some(false);
            }
            "--account-type" => {
                cli.account_type = args.next();
            }
            other => {
                eprintln!("Unknown option: {other}");
            }
        }
    }
    cli
}

fn print_help() {
    println!(
        "ghc-proxy v{VERSION} – GitHub Copilot API Proxy

Usage: ghc-proxy [options]

Options:
  -s, --setup             Launch the interactive setup wizard (sign in + map
                          models); writes the config file
      --claudecode        Configure Claude Code (~/.claude/settings.json) to use
                          this proxy (with --setup)
      --codex             Configure Codex (~/.codex/config.toml) to use this
                          proxy (with --setup)
      --gemini            Configure Gemini CLI (~/.gemini/.env) to use this
                          proxy (with --setup)
  -d, --default           Reset config to defaults during setup
  -p, --port <port>       Port to listen on (default: {port})
  -a, --address <addr>    Address to listen on (default: {addr})
      --debug             Enable debug mode
      --no-debug          Disable debug mode
      --account-type <type> Set account type (individual/business/enterprise)
  -c, --config            Generate default config file
      auth                Authenticate with GitHub and exit (CI/headless flows)
      check-usage         Print Copilot quota/usage and exit
      info                Print diagnostic info (version, paths, token) and exit
      --json              Emit machine-readable JSON (use with info)
      --show-token        Log GitHub and Copilot tokens on refresh
      --rate-limit <secs> Minimum seconds between forwarded requests
      --wait              When rate limited, wait instead of returning HTTP 429
      --manual            Require interactive approval before each request
      --fetch-version     Fetch the latest VS Code version at startup
      --no-fetch-version  Disable dynamic VS Code version fetching
            --auto-upgrade      Auto-upgrade app when a newer release is available
            --no-auto-upgrade   Disable app auto-upgrade
            --update-config     Persist migrated config/default additions back to config.yaml
  -v, --version           Show version
  -h, --help              Show this help

Environment Variables:
  GHC_PROXY_ADDRESS                 Override listen address
  GHC_PROXY_PORT                    Override listen port
  GHC_PROXY_DEBUG                   Enable debug mode (true/1)
  GHC_PROXY_ACCOUNT_TYPE            Set account type
  GHC_PROXY_VSCODE_VERSION          Override VS Code version
  GHC_PROXY_API_VERSION             Override API version
  GHC_PROXY_COPILOT_VERSION         Override Copilot version
  GHC_PROXY_MAX_CONNECTION_RETRIES  Set max connection retries
  GHC_PROXY_REDIRECT_ANTHROPIC      Redirect Anthropic requests (true/1)
  GHC_PROXY_SHOW_TOKEN              Log tokens on refresh (true/1)
  GHC_PROXY_DYNAMIC_VSCODE_VERSION  Fetch latest VS Code version (true/1)
    GHC_PROXY_AUTO_UPGRADE            Auto-upgrade app on startup (true/1)
  GHC_PROXY_RATE_LIMIT_SECONDS      Minimum seconds between requests
  GHC_PROXY_RATE_LIMIT_WAIT         Wait instead of rejecting when limited (true/1)
  GHC_PROXY_MANUAL_APPROVE          Require manual approval per request (true/1)
  GHC_PROXY_GITHUB_MODELS_ENABLED   Route publisher/model ids to GitHub Models (true/1)
  GHC_PROXY_GITHUB_MODELS_ORG       Attribute GitHub Models inference to an org
  GHC_PROXY_GITHUB_MODELS_TOKEN     Dedicated token for GitHub Models (models scope)

Priority: CLI flags > Environment variables > Config file > Defaults",
        port = config::DEFAULT_PORT,
        addr = config::DEFAULT_ADDRESS,
    );
}

/// Prints diagnostic information about the runtime and configuration. When
/// `as_json` is true the output is a single JSON object suitable for tooling.
fn print_info(as_json: bool) {
    let config_dir = config::config_dir();
    let config_path = config::config_path();
    let token_path = auth::token_file_path();
    let token_exists = std::env::var("GITHUB_TOKEN")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
        || token_path.exists();
    if as_json {
        let info = serde_json::json!({
            "version": VERSION,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "config_dir": config_dir.display().to_string(),
            "config_path": config_path.display().to_string(),
            "config_exists": config_path.exists(),
            "token_path": token_path.display().to_string(),
            "token_exists": token_exists,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&info).unwrap_or_default()
        );
    } else {
        println!("ghc-proxy {VERSION}");
        println!(
            "os:            {} ({})",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        println!("config_dir:    {}", config_dir.display());
        println!(
            "config:        {} ({})",
            config_path.display(),
            if config_path.exists() {
                "exists"
            } else {
                "missing"
            }
        );
        println!(
            "github_token:  {} ({})",
            token_path.display(),
            if token_exists { "available" } else { "missing" }
        );
    }
}

const CLAUDE_CODE_PROXY_API_KEY: &str = "ghc-proxy";

/// Merges `env.ANTHROPIC_BASE_URL = base_url` into the given Claude Code
/// `settings.json` content, preserving every other setting. `existing` is the
/// current file contents (or `None`/empty for a new file). Returns the
/// pretty-printed JSON to write, or an error if `existing` is not a JSON object.
fn merge_claude_settings(existing: Option<&str>, base_url: &str) -> Result<String, String> {
    let mut root: serde_json::Value = match existing {
        Some(contents) if !contents.trim().is_empty() => serde_json::from_str(contents)
            .map_err(|e| format!("existing settings.json is not valid JSON: {e}"))?,
        _ => serde_json::json!({}),
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| "existing settings.json is not a JSON object".to_string())?;
    let env = obj.entry("env").or_insert_with(|| serde_json::json!({}));
    if !env.is_object() {
        *env = serde_json::json!({});
    }
    let env_obj = env.as_object_mut().unwrap();
    env_obj.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        serde_json::Value::String(base_url.to_string()),
    );
    if !env_obj.contains_key("ANTHROPIC_API_KEY") {
        env_obj.insert(
            "ANTHROPIC_API_KEY".to_string(),
            serde_json::Value::String(CLAUDE_CODE_PROXY_API_KEY.to_string()),
        );
    }
    // Make Claude Code compact context earlier. Because the proxy routes through
    // Copilot, whose tokenizer differs from Anthropic's, local token estimates
    // can run lower than real usage and bump into the model's full window. These
    // defaults trigger auto-compaction at ~85% of the reported window. Existing
    // user values are preserved.
    if !env_obj.contains_key("CLAUDE_CODE_AUTO_COMPACT_WINDOW") {
        env_obj.insert(
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_string(),
            serde_json::Value::String("1".to_string()),
        );
    }
    if !env_obj.contains_key("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE") {
        env_obj.insert(
            "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE".to_string(),
            serde_json::Value::String("85".to_string()),
        );
    }

    serde_json::to_string_pretty(&root).map_err(|e| e.to_string())
}

/// Patches Claude Code's `settings.json` so its Anthropic requests route
/// through this proxy by setting `env.ANTHROPIC_BASE_URL` and ensuring
/// `env.ANTHROPIC_API_KEY` is present. Any existing settings are preserved
/// (merged); the file and directory are created if missing. Returns the path
/// that was written.
fn configure_claude_code(cfg: &ghc_proxy::config::Config) -> std::io::Result<std::path::PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "home directory not found")
        })?
        .join(".claude");
    let path = dir.join("settings.json");
    let base_url = format!("http://{}:{}", cfg.address, cfg.port);

    // Start from the existing settings when present; refuse to clobber a file
    // that is not valid JSON so we never destroy data.
    let existing = std::fs::read_to_string(&path).ok();
    let merged = merge_claude_settings(existing.as_deref(), &base_url)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, merged + "\n")?;
    Ok(path)
}

/// Recommended default model for Codex when none is detected.
const CODEX_DEFAULT_MODEL: &str = "gpt-5.5";
/// Internal provider id written into the Codex config for this proxy.
const CODEX_PROVIDER_ID: &str = "ghc-proxy";

/// Merges a `[model_providers.ghc-proxy]` block plus top-level `model`,
/// `model_provider`, and (optionally) `model_context_window` into the given
/// Codex `config.toml` content, preserving every other setting. `existing` is
/// the current file contents (or `None`/empty for a new file). Returns the
/// serialized TOML to write, or an error if `existing` is not valid TOML.
fn merge_codex_config(
    existing: Option<&str>,
    base_url: &str,
    model: &str,
    context_window: Option<u64>,
) -> Result<String, String> {
    use toml::Value;
    let mut root: Value = match existing {
        Some(contents) if !contents.trim().is_empty() => toml::from_str::<Value>(contents)
            .map_err(|e| format!("existing config.toml is not valid TOML: {e}"))?,
        _ => Value::Table(toml::map::Map::new()),
    };
    let table = root
        .as_table_mut()
        .ok_or_else(|| "existing config.toml is not a TOML table".to_string())?;

    table.insert("model".to_string(), Value::String(model.to_string()));
    table.insert(
        "model_provider".to_string(),
        Value::String(CODEX_PROVIDER_ID.to_string()),
    );
    if let Some(window) = context_window {
        table.insert(
            "model_context_window".to_string(),
            Value::Integer(window as i64),
        );
    }

    // Ensure [model_providers] exists and is a table, then write our provider.
    let providers = table
        .entry("model_providers".to_string())
        .or_insert_with(|| Value::Table(toml::map::Map::new()));
    if !providers.is_table() {
        *providers = Value::Table(toml::map::Map::new());
    }
    let providers = providers.as_table_mut().unwrap();
    let mut provider = toml::map::Map::new();
    provider.insert("name".to_string(), Value::String("GHC Proxy".to_string()));
    provider.insert(
        "base_url".to_string(),
        Value::String(format!("{base_url}/v1")),
    );
    provider.insert(
        "wire_api".to_string(),
        Value::String("responses".to_string()),
    );
    providers.insert(CODEX_PROVIDER_ID.to_string(), Value::Table(provider));

    toml::to_string_pretty(&root).map_err(|e| e.to_string())
}

/// Patches Codex's `~/.codex/config.toml` so it routes through this proxy by
/// adding a `model_providers.ghc-proxy` block and selecting it. Any existing
/// settings are preserved (merged); the file and directory are created if
/// missing. `context_window` is written as `model_context_window` when known.
/// Returns the path that was written.
fn configure_codex(
    cfg: &ghc_proxy::config::Config,
    model: &str,
    context_window: Option<u64>,
) -> std::io::Result<std::path::PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "home directory not found")
        })?
        .join(".codex");
    let path = dir.join("config.toml");
    let base_url = format!("http://{}:{}", cfg.address, cfg.port);

    let existing = std::fs::read_to_string(&path).ok();
    let merged = merge_codex_config(existing.as_deref(), &base_url, model, context_window)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, merged)?;
    Ok(path)
}

/// Default model written for the Gemini CLI when none is detected.
const GEMINI_DEFAULT_MODEL: &str = "gemini-2.5-pro";
/// Default API key placeholder written for local Gemini CLI auth.
const GEMINI_PROXY_API_KEY: &str = "ghc-proxy";

/// Merges Gemini CLI environment variables into the lines of an existing `.env`
/// file, preserving unrelated entries and any user-set `GEMINI_API_KEY`. Returns
/// the full `.env` text to write.
fn merge_gemini_env(existing: Option<&str>, base_url: &str, model: &str) -> String {
    use std::collections::BTreeSet;
    // Keys we manage; existing values for these are replaced (except the API key
    // which is only filled when absent).
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out_lines: Vec<String> = Vec::new();
    let mut has_api_key = false;

    if let Some(contents) = existing {
        for line in contents.lines() {
            let trimmed = line.trim_start();
            let key = trimmed
                .split_once('=')
                .map(|(k, _)| k.trim().to_string())
                .unwrap_or_default();
            match key.as_str() {
                "GOOGLE_GEMINI_BASE_URL" => {
                    out_lines.push(format!("GOOGLE_GEMINI_BASE_URL={base_url}/v1beta"));
                    seen.insert(key);
                }
                "GEMINI_MODEL" => {
                    out_lines.push(format!("GEMINI_MODEL={model}"));
                    seen.insert(key);
                }
                "GEMINI_TELEMETRY_ENABLED" => {
                    out_lines.push("GEMINI_TELEMETRY_ENABLED=false".to_string());
                    seen.insert(key);
                }
                "GEMINI_API_KEY" => {
                    // Preserve a user-set key untouched.
                    out_lines.push(line.to_string());
                    has_api_key = true;
                    seen.insert(key);
                }
                _ => out_lines.push(line.to_string()),
            }
        }
    }

    if !seen.contains("GOOGLE_GEMINI_BASE_URL") {
        out_lines.push(format!("GOOGLE_GEMINI_BASE_URL={base_url}/v1beta"));
    }
    if !has_api_key {
        out_lines.push(format!("GEMINI_API_KEY={GEMINI_PROXY_API_KEY}"));
    }
    if !seen.contains("GEMINI_MODEL") {
        out_lines.push(format!("GEMINI_MODEL={model}"));
    }
    if !seen.contains("GEMINI_TELEMETRY_ENABLED") {
        out_lines.push("GEMINI_TELEMETRY_ENABLED=false".to_string());
    }

    let mut s = out_lines.join("\n");
    s.push('\n');
    s
}

/// Patches the Gemini CLI's `~/.gemini/.env` so it routes through this proxy by
/// writing `GOOGLE_GEMINI_BASE_URL`, `GEMINI_API_KEY`, `GEMINI_MODEL`, and
/// disabling telemetry. Existing unrelated entries are preserved. Also writes a
/// `settings.json` selecting the api-key auth method to skip the first-launch
/// prompt. Returns the `.env` path that was written.
fn configure_gemini_cli(
    cfg: &ghc_proxy::config::Config,
    model: &str,
) -> std::io::Result<std::path::PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "home directory not found")
        })?
        .join(".gemini");
    let env_path = dir.join(".env");
    let base_url = format!("http://{}:{}", cfg.address, cfg.port);

    let existing = std::fs::read_to_string(&env_path).ok();
    let merged = merge_gemini_env(existing.as_deref(), &base_url, model);

    std::fs::create_dir_all(&dir)?;
    std::fs::write(&env_path, merged)?;

    // Best-effort: select the api-key auth method to skip the first-run prompt.
    let settings_path = dir.join("settings.json");
    let settings_existing = std::fs::read_to_string(&settings_path).ok();
    if let Ok(merged_settings) = merge_gemini_settings(settings_existing.as_deref()) {
        let _ = std::fs::write(&settings_path, merged_settings + "\n");
    }

    Ok(env_path)
}

/// Merges `security.auth.selectedType = "gemini-api-key"` into a Gemini CLI
/// `settings.json`, preserving every other setting.
fn merge_gemini_settings(existing: Option<&str>) -> Result<String, String> {
    let mut root: serde_json::Value = match existing {
        Some(contents) if !contents.trim().is_empty() => serde_json::from_str(contents)
            .map_err(|e| format!("existing settings.json is not valid JSON: {e}"))?,
        _ => serde_json::json!({}),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| "existing settings.json is not a JSON object".to_string())?;
    let security = obj
        .entry("security")
        .or_insert_with(|| serde_json::json!({}));
    if !security.is_object() {
        *security = serde_json::json!({});
    }
    let auth = security
        .as_object_mut()
        .unwrap()
        .entry("auth")
        .or_insert_with(|| serde_json::json!({}));
    if !auth.is_object() {
        *auth = serde_json::json!({});
    }
    auth.as_object_mut().unwrap().insert(
        "selectedType".to_string(),
        serde_json::Value::String("gemini-api-key".to_string()),
    );
    serde_json::to_string_pretty(&root).map_err(|e| e.to_string())
}

/// Prints an interactive-style setup guide after the configuration file has
/// been written/updated. Always shown for `--setup`, even when a config file
/// already existed.
fn print_setup_guide(
    cfg: &ghc_proxy::config::Config,
    path: &std::path::Path,
    claudecode: bool,
    codex: bool,
    gemini: bool,
) {
    let bar = "=".repeat(60);
    println!("\n{bar}");
    println!("ghc-proxy setup");
    println!("{bar}");
    println!("\nConfiguration file updated at:\n  {}", path.display());
    println!("\nCurrent settings:");
    println!("  address:        {}", cfg.address);
    println!("  port:           {}", cfg.port);
    println!("  debug:          {}", cfg.debug);
    println!("  account_type:   {}", cfg.account_type);
    println!(
        "  model mappings: {} exact, {} prefix",
        cfg.model_mappings.exact.len(),
        cfg.model_mappings.prefix.len()
    );

    println!("\nNext steps:");
    println!("  1. Authenticate with GitHub. A token is resolved from, in order:");
    println!("       - the GITHUB_TOKEN environment variable,");
    println!(
        "       - the saved token file at {},",
        config::config_dir().join("github_token.txt").display()
    );
    println!("       - interactive GitHub Device Flow on first run.");
    println!(
        "  2. Edit {} to customise model mappings and filters.",
        path.display()
    );
    println!(
        "  3. Start the proxy:  ghc-proxy --port {} --address {}",
        cfg.port, cfg.address
    );
    println!(
        "  4. Open the dashboard at http://{}:{}/ to view stats and the",
        cfg.address, cfg.port
    );
    println!("     full list of supported models.");

    if claudecode {
        println!("\nClaude Code:");
        match configure_claude_code(cfg) {
            Ok(p) => {
                println!(
                    "  Set env.ANTHROPIC_BASE_URL=http://{}:{} and env.ANTHROPIC_API_KEY={} in:\n    {}",
                    cfg.address,
                    cfg.port,
                    CLAUDE_CODE_PROXY_API_KEY,
                    p.display()
                );
                println!("  Claude Code will now route through this proxy.");
            }
            Err(e) => {
                println!("  Failed to update Claude Code settings: {e}");
                println!(
                    "  Manually set env.ANTHROPIC_BASE_URL=http://{}:{} and env.ANTHROPIC_API_KEY={}\n  in ~/.claude/settings.json",
                    cfg.address,
                    cfg.port,
                    CLAUDE_CODE_PROXY_API_KEY
                );
            }
        }
    }

    if codex {
        println!("\nCodex:");
        match configure_codex(cfg, CODEX_DEFAULT_MODEL, None) {
            Ok(p) => {
                println!(
                    "  Added model_provider \"{CODEX_PROVIDER_ID}\" (base_url http://{}:{}/v1)\n  and selected model {CODEX_DEFAULT_MODEL} in:\n    {}",
                    cfg.address,
                    cfg.port,
                    p.display()
                );
                println!("  Codex will now route through this proxy.");
            }
            Err(e) => {
                println!("  Failed to update Codex settings: {e}");
                println!(
                    "  Manually add a [model_providers.{CODEX_PROVIDER_ID}] block with\n  base_url=http://{}:{}/v1 in ~/.codex/config.toml",
                    cfg.address,
                    cfg.port
                );
            }
        }
    }

    if gemini {
        println!("\nGemini CLI:");
        match configure_gemini_cli(cfg, GEMINI_DEFAULT_MODEL) {
            Ok(p) => {
                println!(
                    "  Set GOOGLE_GEMINI_BASE_URL=http://{}:{}/v1beta and GEMINI_MODEL={GEMINI_DEFAULT_MODEL}\n  in:\n    {}",
                    cfg.address,
                    cfg.port,
                    p.display()
                );
                println!("  Gemini CLI will now route through this proxy.");
            }
            Err(e) => {
                println!("  Failed to update Gemini CLI settings: {e}");
                println!(
                    "  Manually set GOOGLE_GEMINI_BASE_URL=http://{}:{}/v1beta in ~/.gemini/.env",
                    cfg.address, cfg.port
                );
            }
        }
    }
    println!("{bar}");
}

fn maybe_auto_upgrade(enabled: bool) {
    if !enabled {
        return;
    }

    tracing::info!("Checking for ghc-proxy updates...");
    let updater = match self_update::backends::github::Update::configure()
        .repo_owner("MartinForReal")
        .repo_name("ghc-proxy")
        .bin_name("ghc-proxy")
        .bin_path_in_archive("ghc-proxy-v{{ version }}-{{ target }}/{{ bin }}")
        .no_confirm(true)
        .show_download_progress(true)
        .current_version(VERSION)
        .build()
    {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("Auto-upgrade setup failed: {e}");
            return;
        }
    };

    match updater.update() {
        Ok(status) => {
            let new_version = status.version();
            if new_version != VERSION {
                tracing::info!(
                    "ghc-proxy updated from {VERSION} to {new_version}. Restart to use the new binary."
                );
            } else {
                tracing::info!("ghc-proxy is already up to date ({VERSION}).");
            }
        }
        Err(e) => tracing::warn!("Auto-upgrade check/update failed: {e}"),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let mut cli = parse_args();

    if cli.help {
        print_help();
        return;
    }
    if cli.version {
        println!("ghc-proxy {VERSION}");
        return;
    }
    if cli.config {
        match config::generate_default_config() {
            Ok(Some(path)) => println!("Configuration file generated at: {}", path.display()),
            Ok(None) => println!(
                "Configuration file already exists at: {}",
                config::config_path().display()
            ),
            Err(e) => eprintln!("Failed to generate config: {e}"),
        }
        return;
    }
    if cli.info {
        print_info(cli.json);
        return;
    }
    if cli.auth {
        let client = reqwest::Client::new();
        match auth::resolve_github_token(&client).await {
            Some(token) => {
                println!("Authenticated. Token saved to:");
                println!("  {}", auth::token_file_path().display());
                if cli.show_token {
                    println!("  token: {token}");
                }
            }
            None => {
                eprintln!("Authentication failed.");
                std::process::exit(1);
            }
        }
        return;
    }
    if cli.check_usage {
        let mut cfg = config::load_config_with_options(cli.update_config);
        cfg.show_token = cfg.show_token || cli.show_token;
        let client = reqwest::Client::new();
        let Some(github_token) = auth::resolve_github_token(&client).await else {
            eprintln!("No GitHub token available.");
            std::process::exit(1);
        };
        let state = Arc::new(AppState::new(cfg, github_token));
        match state.fetch_usage().await {
            Ok(v) => println!(
                "{}",
                serde_json::to_string_pretty(&ghc_proxy::state::summarize_usage(&v))
                    .unwrap_or_default()
            ),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if cli.setup {
        // Build the configuration that setup should persist: start from the
        // existing config (or built-in defaults when `--default` is given) and
        // layer any CLI overrides on top.
        let mut cfg = if cli.defaults {
            config::Config::default()
        } else {
            config::load_config()
        };
        if let Some(addr) = cli.address.take() {
            cfg.address = addr;
        }
        if let Some(port) = cli.port {
            cfg.port = port;
        }
        if let Some(debug_mode) = cli.debug {
            cfg.debug = debug_mode;
        }
        if let Some(account_type) = cli.account_type.take() {
            cfg.account_type = account_type;
        }

        // In a terminal, walk the user through an interactive wizard; otherwise
        // (piped/headless) fall back to writing the config non-interactively.
        let codex_flag = cli.codex;
        let gemini_flag = cli.gemini;
        if setup::is_interactive() {
            if let Some(outcome) = setup::run(cfg, cli.claudecode).await {
                match config::write_config(&outcome.cfg) {
                    Ok(path) => print_setup_guide(
                        &outcome.cfg,
                        &path,
                        outcome.configure_claude_code,
                        codex_flag,
                        gemini_flag,
                    ),
                    Err(e) => eprintln!("Failed to write config: {e}"),
                }
            }
        } else {
            match config::write_config(&cfg) {
                Ok(path) => print_setup_guide(&cfg, &path, cli.claudecode, codex_flag, gemini_flag),
                Err(e) => eprintln!("Failed to write config: {e}"),
            }
        }
        return;
    }

    // Load configuration (generates a default file on first run). On a genuine
    // first run with no config file and an attached terminal, launch the
    // interactive setup wizard instead so the user can sign in and choose
    // model mappings before the server starts.
    let first_run = !config::config_path().exists();
    let write_back_on_migration = cli.update_config;
    let mut cfg = if first_run && setup::is_interactive() {
        match setup::run(config::Config::default(), cli.claudecode).await {
            Some(outcome) => {
                match config::write_config(&outcome.cfg) {
                    Ok(path) => tracing::info!("✓ Configuration written to {}", path.display()),
                    Err(e) => tracing::warn!("Failed to write config: {e}"),
                }
                if outcome.configure_claude_code {
                    match configure_claude_code(&outcome.cfg) {
                        Ok(p) => tracing::info!("✓ Claude Code configured at {}", p.display()),
                        Err(e) => tracing::warn!("Failed to configure Claude Code: {e}"),
                    }
                }
                if cli.codex {
                    match configure_codex(&outcome.cfg, CODEX_DEFAULT_MODEL, None) {
                        Ok(p) => tracing::info!("✓ Codex configured at {}", p.display()),
                        Err(e) => tracing::warn!("Failed to configure Codex: {e}"),
                    }
                }
                if cli.gemini {
                    match configure_gemini_cli(&outcome.cfg, GEMINI_DEFAULT_MODEL) {
                        Ok(p) => tracing::info!("✓ Gemini CLI configured at {}", p.display()),
                        Err(e) => tracing::warn!("Failed to configure Gemini CLI: {e}"),
                    }
                }
                outcome.cfg
            }
            None => config::load_config_with_options(write_back_on_migration),
        }
    } else {
        config::load_config_with_options(write_back_on_migration)
    };

    // Apply CLI overrides (highest priority)
    if let Some(addr) = cli.address {
        tracing::info!("✓ Overriding address from CLI: {}", addr);
        cfg.address = addr;
    }
    if let Some(port) = cli.port {
        tracing::info!("✓ Overriding port from CLI: {}", port);
        cfg.port = port;
    }
    if let Some(debug_mode) = cli.debug {
        tracing::info!("✓ Overriding debug from CLI: {}", debug_mode);
        cfg.debug = debug_mode;
    }
    if let Some(account_type) = cli.account_type {
        tracing::info!("✓ Overriding account_type from CLI: {}", account_type);
        cfg.account_type = account_type;
    }
    if cli.show_token {
        cfg.show_token = true;
    }
    if let Some(secs) = cli.rate_limit {
        cfg.rate_limit_seconds = Some(secs);
    }
    if cli.wait {
        cfg.rate_limit_wait = true;
    }
    if cli.manual {
        cfg.manual_approve = true;
    }
    if let Some(auto_upgrade) = cli.auto_upgrade {
        cfg.auto_upgrade = auto_upgrade;
    }
    if let Some(fetch) = cli.fetch_version {
        cfg.dynamic_vscode_version = fetch;
    }

    // Optionally self-update from GitHub releases before serving traffic.
    maybe_auto_upgrade(cfg.auto_upgrade);

    // Optionally refresh the VS Code version used in upstream headers.
    if cfg.dynamic_vscode_version {
        let client = reqwest::Client::new();
        match ghc_proxy::util::fetch_latest_vscode_version(&client).await {
            Some(ver) => {
                tracing::info!("✓ Using latest VS Code version: {ver}");
                cfg.vscode_version = ver;
            }
            None => tracing::warn!(
                "Could not fetch latest VS Code version; using {}",
                cfg.vscode_version
            ),
        }
    }

    let host = cfg.address.clone();
    let port = cfg.port;
    if cfg.debug {
        tracing::debug!("Debug mode enabled");
    }

    // Resolve a GitHub token (env var, saved file, or Device Flow).
    let bootstrap_client = reqwest::Client::new();
    let github_token = match auth::resolve_github_token(&bootstrap_client).await {
        Some(t) => t,
        None => {
            eprintln!("\n{}", "=".repeat(60));
            eprintln!("ERROR: No GitHub token available!");
            eprintln!("Options:");
            eprintln!("  1. Set GITHUB_TOKEN environment variable");
            eprintln!(
                "  2. Create github_token.txt in {}",
                config::config_dir().display()
            );
            eprintln!("  3. Run again for interactive Device Flow auth");
            eprintln!("{}", "=".repeat(60));
            std::process::exit(1);
        }
    };

    let app_state = Arc::new(AppState::new(cfg, github_token));

    // Prime the Copilot token and model list.
    if let Err(e) = app_state.refresh_copilot_token().await {
        eprintln!("Fatal error: {e}");
        std::process::exit(1);
    }
    if let Err(e) = app_state.load_models().await {
        tracing::warn!("{e}");
    }

    // Keep model catalog fresh without restart.
    {
        let state = app_state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30 * 60));
            loop {
                interval.tick().await;
                if let Err(e) = state.load_models().await {
                    tracing::warn!("Periodic model refresh failed: {e}");
                }
            }
        });
    }

    let app = server::router(app_state.clone());

    let addr: SocketAddr = match format!("{host}:{port}").parse() {
        Ok(a) => a,
        Err(_) => {
            // Fall back to resolving host names via 127.0.0.1.
            SocketAddr::from(([127, 0, 0, 1], port))
        }
    };

    println!("\nStarting GitHub Copilot API Proxy on {host}:{port}");
    println!("Dashboard:      http://{host}:{port}/");
    println!("Metrics UI:     http://{host}:{port}/metrics/dashboard");
    println!("OpenMetrics:    http://{host}:{port}/metrics");
    println!("Reload config:  POST http://{host}:{port}/api/config/reload");
    println!("OpenAI API:     http://{host}:{port}/v1/chat/completions");
    println!("Responses API:  http://{host}:{port}/v1/responses");
    println!("Anthropic API:  http://{host}:{port}/v1/messages");
    println!("Gemini API:     http://{host}:{port}/v1beta/models/{{model}}:generateContent");
    println!("OpenAPI spec:   http://{host}:{port}/openapi.json");

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Server error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{merge_claude_settings, merge_codex_config};

    #[test]
    fn codex_config_new_file() {
        let out =
            merge_codex_config(None, "http://127.0.0.1:8314", "gpt-5.5", Some(272000)).unwrap();
        let v: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(v["model"].as_str(), Some("gpt-5.5"));
        assert_eq!(v["model_provider"].as_str(), Some("ghc-proxy"));
        assert_eq!(v["model_context_window"].as_integer(), Some(272000));
        assert_eq!(
            v["model_providers"]["ghc-proxy"]["base_url"].as_str(),
            Some("http://127.0.0.1:8314/v1")
        );
        assert_eq!(
            v["model_providers"]["ghc-proxy"]["wire_api"].as_str(),
            Some("responses")
        );
    }

    #[test]
    fn codex_config_preserves_existing_keys() {
        let existing = "approval_policy = \"on-request\"\n[tui]\ntheme = \"dark\"\n";
        let out = merge_codex_config(Some(existing), "http://x", "gpt-5.5", None).unwrap();
        let v: toml::Value = toml::from_str(&out).unwrap();
        // Unrelated keys preserved.
        assert_eq!(v["approval_policy"].as_str(), Some("on-request"));
        assert_eq!(v["tui"]["theme"].as_str(), Some("dark"));
        // Provider added.
        assert_eq!(v["model_provider"].as_str(), Some("ghc-proxy"));
        // No context window key when None.
        assert!(v.get("model_context_window").is_none());
    }

    #[test]
    fn codex_config_rejects_invalid_toml() {
        assert!(merge_codex_config(Some("=not valid="), "http://x", "m", None).is_err());
    }

    #[test]
    fn gemini_env_new_file() {
        let out = super::merge_gemini_env(None, "http://127.0.0.1:8314", "gemini-2.5-pro");
        assert!(out.contains("GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:8314/v1beta"));
        assert!(out.contains("GEMINI_API_KEY=ghc-proxy"));
        assert!(out.contains("GEMINI_MODEL=gemini-2.5-pro"));
        assert!(out.contains("GEMINI_TELEMETRY_ENABLED=false"));
    }

    #[test]
    fn gemini_env_preserves_user_key_and_other_vars() {
        let existing =
            "FOO=bar\nGEMINI_API_KEY=real-key\nGOOGLE_GEMINI_BASE_URL=http://old/v1beta\n";
        let out = super::merge_gemini_env(Some(existing), "http://x", "m");
        // Unrelated var preserved.
        assert!(out.contains("FOO=bar"));
        // User key untouched.
        assert!(out.contains("GEMINI_API_KEY=real-key"));
        assert!(!out.contains("GEMINI_API_KEY=ghc-proxy"));
        // Base URL updated to the new value (old removed).
        assert!(out.contains("GOOGLE_GEMINI_BASE_URL=http://x/v1beta"));
        assert!(!out.contains("http://old/v1beta"));
    }

    #[test]
    fn gemini_settings_selects_api_key_auth() {
        let out = super::merge_gemini_settings(None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["security"]["auth"]["selectedType"], "gemini-api-key");
    }

    #[test]
    fn gemini_settings_preserves_existing() {
        let existing = r#"{"theme":"dark"}"#;
        let out = super::merge_gemini_settings(Some(existing)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["security"]["auth"]["selectedType"], "gemini-api-key");
    }

    #[test]
    fn creates_env_when_file_is_new() {
        let out = merge_claude_settings(None, "http://127.0.0.1:8314").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8314");
        assert_eq!(v["env"]["ANTHROPIC_API_KEY"], "ghc-proxy");
        assert_eq!(v["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "1");
        assert_eq!(v["env"]["CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"], "85");
    }

    #[test]
    fn preserves_existing_compaction_overrides() {
        let existing = r#"{"env":{"CLAUDE_AUTOCOMPACT_PCT_OVERRIDE":"70"}}"#;
        let out = merge_claude_settings(Some(existing), "http://x").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["env"]["CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"], "70");
        assert_eq!(v["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "1");
    }

    #[test]
    fn preserves_existing_settings_and_env() {
        let existing = r#"{
            "theme": "dark",
            "env": {
              "FOO": "bar",
              "ANTHROPIC_BASE_URL": "http://old",
              "ANTHROPIC_API_KEY": "real-key"
            }
        }"#;
        let out = merge_claude_settings(Some(existing), "http://127.0.0.1:9000").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        // Unrelated keys are untouched.
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["env"]["FOO"], "bar");
        // The base URL is overwritten with the new value.
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:9000");
        // Existing API key is preserved.
        assert_eq!(v["env"]["ANTHROPIC_API_KEY"], "real-key");
    }

    #[test]
    fn replaces_non_object_env() {
        let out = merge_claude_settings(Some(r#"{"env": "oops"}"#), "http://x").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "http://x");
        assert_eq!(v["env"]["ANTHROPIC_API_KEY"], "ghc-proxy");
    }

    #[test]
    fn fills_missing_api_key_only() {
        let out = merge_claude_settings(
            Some(r#"{"env":{"ANTHROPIC_BASE_URL":"http://old"}}"#),
            "http://x",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["env"]["ANTHROPIC_API_KEY"], "ghc-proxy");
    }

    #[test]
    fn leaves_existing_api_key_untouched_even_if_empty() {
        let out = merge_claude_settings(
            Some(r#"{"env":{"ANTHROPIC_BASE_URL":"http://old","ANTHROPIC_API_KEY":"   "}}"#),
            "http://x",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["env"]["ANTHROPIC_API_KEY"], "   ");
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(merge_claude_settings(Some("{not json"), "http://x").is_err());
    }

    #[test]
    fn rejects_non_object_root() {
        assert!(merge_claude_settings(Some("[1, 2, 3]"), "http://x").is_err());
    }
}
