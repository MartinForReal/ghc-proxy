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
    config: bool,
    version: bool,
    help: bool,
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
            "-p" | "--port" => {
                if let Some(v) = args.next() {
                    cli.port = v.parse().ok();
                }
            }
            "-a" | "--address" => {
                cli.address = args.next();
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
  -s, --setup             Generate default config (setup)
      --claudecode        Update Claude Code settings only (use with --setup)
  -d, --default           Use defaults for setup prompts
  -p, --port <port>       Port to listen on (default: {port})
  -a, --address <addr>    Address to listen on (default: {addr})
  -c, --config            Generate default config file
  -v, --version           Show version
  -h, --help              Show this help",
        port = config::DEFAULT_PORT,
        addr = config::DEFAULT_ADDRESS,
    );
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

    let cli = parse_args();

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
    if cli.setup {
        // The interactive wizard from ghc-tunnel is not reproduced; we ensure a
        // default config exists and continue to start the server.
        if let Ok(Some(path)) = config::generate_default_config() {
            println!("Configuration file generated at: {}", path.display());
        }
        if cli.claudecode {
            println!(
                "Configure ~/.claude/settings.json with ANTHROPIC_BASE_URL pointing at this proxy."
            );
            return;
        }
    }

    // Load configuration (generates a default file on first run).
    let mut cfg = config::load_config();
    if let Some(addr) = cli.address.clone() {
        cfg.address = addr;
    }
    if let Some(port) = cli.port {
        cfg.port = port;
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
