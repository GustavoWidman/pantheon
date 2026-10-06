use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use pantheon::{
    config::Config,
    discord::Discord,
    memory::{Kind, Memory},
    runtime::Harness,
};
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(version, about = "Pantheon — durable always-on Discord agent")]
struct Cli {
    #[arg(long, env = "PANTHEON_CONFIG", default_value = "pantheon.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Action>,
}
#[derive(Subcommand)]
enum Action {
    /// Run the Discord gateway, agent workers, durable scheduler and outbox.
    Run,
    /// Check configuration, credentials and bundled browser dependencies without network calls.
    Doctor,
    /// Discover authenticated providers and live model/effort catalogs without inference.
    Models {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value = "")]
        query: String,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Sign in with ChatGPT using the bundled official Codex CLI and configured auth home.
    LoginCodex {
        #[arg(long)]
        device_auth: bool,
    },
    /// Export one channel's complete memory tree as HTML (stop the service first).
    Export {
        #[arg(long)]
        channel: u64,
        #[arg(long)]
        output: PathBuf,
    },
    /// Import UTF-8 history as a durable note (stop the service first).
    Import {
        #[arg(long)]
        channel: u64,
        #[arg(long)]
        file: PathBuf,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pantheon=info".into()),
        )
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    if matches!(cli.command, Some(Action::Doctor)) {
        return doctor(&config);
    }
    if let Some(Action::LoginCodex { device_auth }) = cli.command {
        return config.auth.login(device_auth).await;
    }
    if let Some(Action::Models {
        provider,
        query,
        offset,
        limit,
    }) = &cli.command
    {
        std::fs::create_dir_all(&config.state_dir)?;
        let catalog = pantheon::models::Catalog::default();
        catalog.initialize(&config);
        let api = pantheon::provider::Provider::new(30)?.with_auth(config.auth.clone());
        catalog.refresh(&config, &api).await;
        println!(
            "{}",
            serde_json::to_string_pretty(&catalog.report(
                provider.as_deref(),
                query,
                *offset,
                *limit
            ))?
        );
        return Ok(());
    }
    std::fs::create_dir_all(&config.state_dir)?;
    // Entire daemon, operational DB and offline CLI share one lifetime writer lock.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.state_dir.join("daemon.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context(
        "another Pantheon process owns this state directory; stop it before offline export/import",
    )?;
    match cli.command.unwrap_or(Action::Run) {
        Action::Export { channel, output } => {
            let m = Memory::open(
                config.state_dir.join("chats").join(channel.to_string()),
                config.agent.view_bytes,
            )?;
            std::fs::write(&output, m.export_html())?;
            println!("Exported memory to {}", output.display());
        }
        Action::Import { channel, file } => {
            let mut m = Memory::open(
                config.state_dir.join("chats").join(channel.to_string()),
                config.agent.view_bytes,
            )?;
            let index = m.append(Kind::Note, &std::fs::read_to_string(file)?)?;
            println!("Imported note {index}");
        }
        Action::Run => {
            let token = std::env::var("DISCORD_TOKEN").context("missing DISCORD_TOKEN")?;
            let discord = Arc::new(
                Discord::new(
                    token,
                    config.discord.application_id,
                    config.discord.allowed_users.clone(),
                )?
                .with_state_dir(&config.state_dir)?,
            );
            let shutdown = CancellationToken::new();
            let signal = shutdown.clone();
            tokio::spawn(async move {
                #[cfg(unix)]
                {
                    let mut term =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .expect("SIGTERM handler");
                    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}};
                }
                #[cfg(not(unix))]
                {
                    let _ = tokio::signal::ctrl_c().await;
                }
                signal.cancel();
            });
            let h = Harness::new(config, discord.clone(), shutdown.clone())?;
            let (tx, rx) = tokio::sync::mpsc::channel(1024);
            let stop = shutdown.clone();
            let gateway = tokio::spawn(async move {
                let result = discord.run(tx, stop.clone()).await;
                stop.cancel();
                result
            });
            let result = h.run(rx).await;
            shutdown.cancel();
            let gateway_result = gateway.await.context("gateway panic")?;
            result?;
            gateway_result?;
        }
        Action::Doctor | Action::LoginCodex { .. } | Action::Models { .. } => unreachable!(),
    }
    drop(lock);
    Ok(())
}
fn doctor(config: &Config) -> Result<()> {
    config.instructions()?;
    pantheon::skills::Skills::load(&config.skills)?;
    config.mcp.validate()?;
    ensure!(
        config.workspace.is_dir(),
        "workspace directory does not exist"
    );
    let mut missing = vec![];
    for server in config.mcp.servers.values() {
        for key in server.env.values().chain(server.bearer_env.iter()) {
            if std::env::var_os(key).is_none() && !missing.contains(key) {
                missing.push(key.clone());
            }
        }
        if let Some(command) = &server.command {
            let exists = if command.components().count() > 1 {
                command.is_file()
            } else {
                std::env::var_os("PATH").is_some_and(|path| {
                    std::env::split_paths(&path).any(|dir| dir.join(command).is_file())
                })
            };
            if !exists {
                missing.push(format!("MCP command {}", command.display()));
            }
        }
    }
    if std::env::var("DISCORD_TOKEN").is_err() {
        missing.push("DISCORD_TOKEN".to_string());
    }
    for model in [&config.agent.model, &config.agent.compactor_model] {
        if model.starts_with("codex/") {
            if config.auth.inspect().is_err() && !missing.iter().any(|s| s == "Codex ChatGPT login")
            {
                missing.push("Codex ChatGPT login".into());
            }
            continue;
        }
        let (vendor, _) = pantheon::provider::model_parts(model)?;
        let key = if vendor == "openai" {
            "OPENAI_API_KEY"
        } else {
            "ANTHROPIC_API_KEY"
        };
        if std::env::var(key).is_err() && !missing.iter().any(|s| s == key) {
            missing.push(key.into());
        }
    }
    if let Some(model) = &config.web.search_model {
        if model.starts_with("codex/") {
            config.auth.inspect()?;
        } else {
            let (vendor, _) = pantheon::provider::model_parts(model)?;
            let key = if vendor == "openai" {
                "OPENAI_API_KEY"
            } else {
                "ANTHROPIC_API_KEY"
            };
            if std::env::var(key).is_err() && !missing.iter().any(|s| s == key) {
                missing.push(key.into());
            }
        }
    }
    if [&config.agent.model, &config.agent.compactor_model]
        .into_iter()
        .any(|model| model.starts_with("codex/"))
        || config
            .web
            .search_model
            .as_deref()
            .is_some_and(|model| model.starts_with("codex/"))
    {
        let cli = config.auth.cli();
        let exists = if cli.components().count() > 1 {
            cli.is_file()
        } else {
            std::env::var_os("PATH").is_some_and(|path| {
                std::env::split_paths(&path).any(|directory| directory.join(&cli).is_file())
            })
        };
        if !exists {
            missing.push("official Codex CLI (auth.codex_cli)".into());
        }
    }
    for binary in ["bash", "Xvfb", "x11vnc", "websockify"] {
        if !std::env::var_os("PATH")
            .unwrap_or_default()
            .to_string_lossy()
            .split(':')
            .any(|p| std::path::Path::new(p).join(binary).is_file())
        {
            missing.push(binary.into());
        }
    }
    for variable in [
        "PANTHEON_CAMOUFOX",
        "PANTHEON_BROWSER_WORKER",
        "PANTHEON_NOVNC_WEB",
    ] {
        match std::env::var_os(variable) {
            Some(p) if std::path::Path::new(&p).exists() => {}
            _ => missing.push(variable.into()),
        }
    }
    ensure!(
        missing.is_empty(),
        "missing runtime requirements: {} (use the Nix package for the bundled browser)",
        missing.join(", ")
    );
    println!(
        "Configuration, authorization, credentials and bundled runtime paths are present. No live API calls made."
    );
    Ok(())
}
