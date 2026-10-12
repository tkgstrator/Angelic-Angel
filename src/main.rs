mod autopush;
mod cli;
mod config;
mod error;
mod listener;
mod notifications;
mod push;
mod status;
mod transaction;
mod twitter;
mod web;

use std::path::Path;

use clap::Parser;
use cli::{Cli, Commands};
use config::{Config, ConfigLock};
use console::style;
use dialoguer::{Input, Password};
use error::Result;
use indicatif::ProgressBar;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let filter = if cli.verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("warn")
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config_path = cli.config;

    if let Commands::Serve { bind, assets_dir } = cli.command {
        if let Err(e) = web::serve(config_path, bind, assets_dir).await {
            eprintln!("{} {}", style("error:").red().bold(), e);
            std::process::exit(1);
        }
        return;
    }

    let task = async {
        match cli.command {
            Commands::Init { auth_token, ct0 } => {
                let _lock = ConfigLock::acquire(&config_path)?;
                cmd_init(&config_path, auth_token, ct0).await
            },
            Commands::Register => {
                let _lock = ConfigLock::acquire(&config_path)?;
                cmd_register(&config_path).await
            },
            Commands::Listen => {
                let _lock = ConfigLock::acquire(&config_path)?;
                cmd_listen(&config_path).await
            },
            Commands::Status => cmd_status(&config_path).await,
            Commands::Unregister => {
                let _lock = ConfigLock::acquire(&config_path)?;
                cmd_unregister(&config_path).await
            },
            // `serve` handles SIGINT/SIGTERM itself to stop the receiver gracefully.
            Commands::Serve { .. } => unreachable!("handled before the interrupt guard"),
        }
    };

    let result = tokio::select! {
        result = task => result,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\n{}", style("Interrupted").red().bold());
            return;
        }
    };

    if let Err(e) = result {
        eprintln!("{} {}", style("error:").red().bold(), e);
        std::process::exit(1);
    }
}

fn spinner(msg: &str) -> ProgressBar {
    let sp = ProgressBar::new_spinner();
    sp.set_style(
        indicatif::ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    sp.enable_steady_tick(std::time::Duration::from_millis(80));
    sp.set_message(msg.to_string());
    sp
}

async fn spin<F, T>(msg: &str, done_msg: &str, fut: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let sp = spinner(msg);
    match fut.await {
        Ok(v) => {
            sp.finish_with_message(format!("{} {}", style("done").green(), done_msg));
            Ok(v)
        },
        Err(e) => {
            sp.finish_with_message(format!("{} {}", style("fail").red(), msg));
            Err(e)
        },
    }
}

async fn cmd_init(
    config_path: &Path,
    arg_auth_token: Option<String>,
    arg_ct0: Option<String>,
) -> Result<()> {
    eprintln!("{}", style("Initializing configuration").bold());
    eprintln!();

    let auth_token = match arg_auth_token {
        Some(v) => v,
        None => Password::new()
            .with_prompt("auth_token")
            .interact()
            .map_err(|e| error::AngelicAngelError::Config(format!("input error: {}", e)))?,
    };

    let ct0 = match arg_ct0 {
        Some(v) => v,
        None => Input::new()
            .with_prompt("ct0")
            .interact_text()
            .map_err(|e| error::AngelicAngelError::Config(format!("input error: {}", e)))?,
    };

    let previous = Config::load(config_path).ok();
    let webhook_endpoint = previous.as_ref().and_then(|c| c.webhook_endpoint.clone());
    let notification_filter = previous.map(|c| c.notification_filter).unwrap_or_default();
    let config = Config {
        twitter: config::TwitterConfig { auth_token, ct0 },
        registration: None,
        webhook_endpoint,
        notification_filter,
    };

    config.save(config_path)?;
    eprintln!(
        "{} Saved to {}",
        style("done").green().bold(),
        style(config_path.display()).underlined()
    );

    Ok(())
}

async fn cmd_register(config_path: &Path) -> Result<()> {
    eprintln!("{}", style("Registering push subscription").bold());
    eprintln!();

    let mut config = Config::load(config_path)?;

    let subscription = spin(
        "Registering with AutoPush...",
        "AutoPush registered",
        push::subscribe(),
    )
    .await?;

    spin(
        "Registering with Twitter...",
        "Twitter Push registered",
        twitter::register(&config.twitter, &subscription),
    )
    .await?;

    config.registration = Some(config::Registration {
        endpoint: subscription.endpoint,
        autopush: subscription.autopush,
        keys: subscription.keys,
    });

    config.save(config_path)?;
    eprintln!();
    eprintln!(
        "{} Saved to {}",
        style("done").green().bold(),
        style(config_path.display()).underlined()
    );

    Ok(())
}

async fn cmd_listen(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let registration = config.registration.ok_or_else(|| {
        error::AngelicAngelError::Config("No registration found. Run `register` first.".to_string())
    })?;

    tracing::info!(config = %config_path.display(), "config loaded, starting listener");

    listener::listen(registration, config_path).await?;

    Ok(())
}

async fn cmd_status(config_path: &Path) -> Result<()> {
    eprintln!("{}", style("Status").bold());
    eprintln!();

    match Config::load(config_path) {
        Ok(config) => {
            eprintln!(
                "{}  {}",
                style("Config").cyan().bold(),
                style(config_path.display()).dim()
            );
            eprintln!(
                "  auth_token  {}...",
                style(
                    &config
                        .twitter
                        .auth_token
                        .chars()
                        .take(20)
                        .collect::<String>()
                )
                .dim()
            );
            eprintln!(
                "  ct0         {}...",
                style(&config.twitter.ct0.chars().take(20).collect::<String>()).dim()
            );

            eprintln!();

            match config.registration {
                Some(reg) => {
                    eprintln!(
                        "{}  {}",
                        style("Registration").cyan().bold(),
                        style("active").green()
                    );
                    eprintln!(
                        "  endpoint    {}...",
                        style(&reg.endpoint.chars().take(50).collect::<String>()).dim()
                    );
                    eprintln!("  uaid        {}", style(&reg.autopush.uaid).dim());
                    eprintln!("  channel_id  {}", style(&reg.autopush.channel_id).dim());
                },
                None => {
                    eprintln!(
                        "{}  {}",
                        style("Registration").cyan().bold(),
                        style("not registered").yellow()
                    );
                    eprintln!(
                        "  Run {} to register.",
                        style("angelic-angel register").bold()
                    );
                },
            }
        },
        Err(_) => {
            eprintln!(
                "{}  {}",
                style("Config").cyan().bold(),
                style("not found").red()
            );
            eprintln!(
                "  Run {} to initialize.",
                style("angelic-angel init").bold()
            );
        },
    }

    Ok(())
}

async fn cmd_unregister(config_path: &Path) -> Result<()> {
    eprintln!("{}", style("Unregistering").bold());
    eprintln!();

    let mut config = Config::load(config_path)?;
    let reg = config
        .registration
        .as_ref()
        .ok_or_else(|| error::AngelicAngelError::Config("No registration found.".to_string()))?;

    spin(
        "Unregistering from AutoPush...",
        "AutoPush unregistered",
        autopush::unregister(&reg.autopush),
    )
    .await?;

    config.registration = None;
    config.save(config_path)?;

    eprintln!();
    eprintln!(
        "{} Registration removed from {}",
        style("done").green().bold(),
        style(config_path.display()).underlined()
    );

    Ok(())
}
