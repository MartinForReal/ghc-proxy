//! ghc-proxy binary entry point: CLI handling and server startup.

use ghc_proxy::{auth, config, server, state::AppState};
use std::net::SocketAddr;
use std::sync::Arc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Parsed command-line options.
#[derive(Debug, Default)]
struct Cli {
    setup: bool,
    claudecode: bool,
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
}

fn parse_args() -> Cli {
    let mut cli = Cli::default();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-s" | "--setup" => cli.setup = true,
            "--claudecode" => cli.claudecode = true,
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
  -s, --setup             Show the setup guide and write/update the config file
      --claudecode        Include Claude Code setup instructions (use with --setup)
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
  GHC_PROXY_RATE_LIMIT_SECONDS      Minimum seconds between requests
  GHC_PROXY_RATE_LIMIT_WAIT         Wait instead of rejecting when limited (true/1)
  GHC_PROXY_MANUAL_APPROVE          Require manual approval per request (true/1)

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
        println!("{}", serde_json::to_string_pretty(&info).unwrap_or_default());
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

/// Prints an interactive-style setup guide after the configuration file has
/// been written/updated. Always shown for `--setup`, even when a config file
/// already existed.
fn print_setup_guide(cfg: &ghc_proxy::config::Config, path: &std::path::Path, claudecode: bool) {
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
        println!(
            "  Set ANTHROPIC_BASE_URL in ~/.claude/settings.json to point at\n  http://{}:{} so Claude Code routes through this proxy.",
            cfg.address, cfg.port
        );
    }
    println!("{bar}");
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
        let mut cfg = config::load_config();
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

        match config::write_config(&cfg) {
            Ok(path) => print_setup_guide(&cfg, &path, cli.claudecode),
            Err(e) => eprintln!("Failed to write config: {e}"),
        }
        return;
    }

    // Load configuration (generates a default file on first run).
    let mut cfg = config::load_config();

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
    if let Some(fetch) = cli.fetch_version {
        cfg.dynamic_vscode_version = fetch;
    }

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
    println!("OpenAI API:     http://{host}:{port}/v1/chat/completions");
    println!("Responses API:  http://{host}:{port}/v1/responses");
    println!("Anthropic API:  http://{host}:{port}/v1/messages");

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
