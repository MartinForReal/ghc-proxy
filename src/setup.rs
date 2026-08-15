//! Interactive first-run setup wizard.
//!
//! Runs when the binary is started with `--setup`, or on a normal launch when
//! no configuration file exists yet and the process is attached to a terminal.
//! The wizard walks the user through server settings, GitHub authentication
//! (Device Flow), and model-mapping configuration, then hands a finished
//! [`Config`] back to `main` to persist and use.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::sync::Arc;

use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, FuzzySelect, Input, Select};
use ghc_proxy::config::{self, Config, ModelMappings};
use ghc_proxy::{auth, state::AppState};

/// The result of a completed wizard run.
pub struct Outcome {
    /// The configuration the user assembled.
    pub cfg: Config,
    /// Whether the user asked to also configure Claude Code.
    pub configure_claude_code: bool,
}

/// Whether an interactive wizard can run, i.e. both stdin and stdout are
/// attached to a terminal. Returns false for piped/redirected/detached
/// processes so headless and CI launches never block on a prompt.
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn bar() {
    println!("{}", "=".repeat(60));
}

fn section(title: &str) {
    println!("\n{}", "-".repeat(60));
    println!("{title}");
    println!("{}", "-".repeat(60));
}

/// Runs the interactive setup wizard, returning the assembled configuration or
/// `None` if the user cancelled.
///
/// `starting` provides the default values shown at each prompt (the existing
/// configuration, or built-in defaults). `claudecode_flag` forces the Claude
/// Code step on without asking (set when `--claudecode` is passed).
pub async fn run(starting: Config, claudecode_flag: bool) -> Option<Outcome> {
    println!();
    bar();
    println!("ghc-proxy interactive setup");
    bar();
    println!(
        "This wizard configures the proxy, signs in to GitHub, and sets up\n\
         model mappings. Press Ctrl-C at any time to cancel."
    );

    // --- Step 1: server settings -----------------------------------------
    let defaults = starting.clone();
    let settings =
        match tokio::task::spawn_blocking(move || prompt_server_settings(&defaults)).await {
            Ok(Ok(s)) => s,
            _ => return cancelled(),
        };
    let mut cfg = starting;
    cfg.address = settings.address;
    cfg.port = settings.port;
    cfg.account_type = settings.account_type;

    // --- Step 2: GitHub authentication -----------------------------------
    section("GitHub authentication");
    let client = reqwest::Client::new();
    let token = match auth::resolve_github_token(&client).await {
        Some(t) => t,
        None => {
            eprintln!("Could not obtain a GitHub token. Setup cancelled.");
            return None;
        }
    };
    println!("✓ GitHub token ready.");

    // --- Step 3: fetch the model catalog ---------------------------------
    section("Available models");
    let model_ids = fetch_model_ids(&cfg, &token).await;
    if model_ids.is_empty() {
        println!("⚠ Could not fetch the model catalog. Default mappings will be offered.");
    } else {
        println!(
            "✓ {} models available from GitHub Copilot.",
            model_ids.len()
        );
    }

    // --- Step 4: model mappings ------------------------------------------
    match tokio::task::spawn_blocking(move || prompt_model_mappings(model_ids)).await {
        Ok(Ok(m)) => cfg.model_mappings = m,
        _ => {
            // Keep whatever mappings `starting` already had on cancel/error.
            println!("Keeping existing model mappings.");
        }
    }

    // --- Step 5: Claude Code ---------------------------------------------
    let configure_claude_code = if claudecode_flag {
        true
    } else {
        matches!(
            tokio::task::spawn_blocking(prompt_claude_code).await,
            Ok(Ok(true))
        )
    };

    Some(Outcome {
        cfg,
        configure_claude_code,
    })
}

fn cancelled() -> Option<Outcome> {
    println!("Setup cancelled.");
    None
}

struct ServerSettings {
    address: String,
    port: u16,
    account_type: String,
}

fn prompt_server_settings(defaults: &Config) -> dialoguer::Result<ServerSettings> {
    let theme = ColorfulTheme::default();
    section("Server settings");

    let address: String = Input::with_theme(&theme)
        .with_prompt("Listen address")
        .default(defaults.address.clone())
        .interact_text()?;

    let port: u16 = Input::with_theme(&theme)
        .with_prompt("Listen port")
        .default(defaults.port)
        .interact_text()?;

    let types = ["individual", "business", "enterprise"];
    let current = types
        .iter()
        .position(|t| *t == defaults.account_type)
        .unwrap_or(0);
    let idx = Select::with_theme(&theme)
        .with_prompt("Copilot account type")
        .items(types)
        .default(current)
        .interact()?;

    Ok(ServerSettings {
        address,
        port,
        account_type: types[idx].to_string(),
    })
}

fn prompt_model_mappings(model_ids: Vec<String>) -> dialoguer::Result<ModelMappings> {
    let theme = ColorfulTheme::default();
    section("Model mappings");

    let custom_available = !model_ids.is_empty();
    let mut options = vec!["Use recommended defaults (opus / sonnet / haiku aliases)"];
    if custom_available {
        options.push("Map aliases to specific models from your catalog");
    }
    options.push("Start with no mappings");

    let choice = Select::with_theme(&theme)
        .with_prompt("How should models be mapped?")
        .items(&options)
        .default(0)
        .interact()?;

    let selected = options[choice];
    if selected.starts_with("Use recommended") {
        return Ok(config::default_model_mappings());
    }
    if selected.starts_with("Start with no") {
        return Ok(ModelMappings::default());
    }

    // Custom: pick a target model for each common alias.
    let mut exact = BTreeMap::new();
    for alias in ["opus", "sonnet", "haiku"] {
        let default_idx = model_ids
            .iter()
            .position(|m| m.contains(alias))
            .unwrap_or(0);
        let idx = FuzzySelect::with_theme(&theme)
            .with_prompt(format!("Target model for \"{alias}\""))
            .items(&model_ids)
            .default(default_idx)
            .interact()?;
        exact.insert(alias.to_string(), model_ids[idx].clone());
    }

    let keep_prefixes = Confirm::with_theme(&theme)
        .with_prompt("Also include the built-in prefix mappings (recommended)?")
        .default(true)
        .interact()?;
    let prefix = if keep_prefixes {
        config::default_model_mappings().prefix
    } else {
        BTreeMap::new()
    };

    Ok(ModelMappings { exact, prefix })
}

fn prompt_claude_code() -> dialoguer::Result<bool> {
    let theme = ColorfulTheme::default();
    Confirm::with_theme(&theme)
        .with_prompt("Configure Claude Code (~/.claude/settings.json) to use this proxy?")
        .default(false)
        .interact()
}

/// Resolves a Copilot token and fetches the available model ids, returning a
/// sorted, de-duplicated list. Returns an empty vector on any failure.
async fn fetch_model_ids(cfg: &Config, token: &str) -> Vec<String> {
    let state = Arc::new(AppState::new(cfg.clone(), token.to_string()));
    if state.refresh_copilot_token().await.is_err() {
        return Vec::new();
    }
    if state.load_models().await.is_err() {
        return Vec::new();
    }
    let guard = state.models.read().await;
    let mut ids: Vec<String> = guard
        .as_ref()
        .and_then(|m| m.get("data"))
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids.dedup();
    ids
}
