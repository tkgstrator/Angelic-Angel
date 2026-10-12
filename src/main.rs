mod autopush;
mod cli;
mod config;
mod error;
mod listener;
mod notifications;
mod push;
mod status;
mod transaction;
mod tweet_lookup;
mod twitter;
mod web;

use std::path::Path;
use std::time::Duration;

use clap::Parser;
use cli::{Cli, Commands};
use config::{Config, ConfigLock, Registration, TwitterConfig, TwitterSource};
use console::style;
use dialoguer::{Input, Password};
use error::{AngelicAngelError, Result};
use indicatif::ProgressBar;
use tracing_subscriber::EnvFilter;

/// 自動登録に失敗してから終了するまでの待ち時間。`restart: unless-stopped` の再起動ループが
/// X や AutoPush のエンドポイントを連打しないための間隔になる。
const AUTO_REGISTER_FAILURE_DELAY: Duration = Duration::from_secs(30);

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

/// `register` が X に渡す認証情報。環境変数があれば設定ファイルが無くても登録できる。
fn register_twitter(
    stored: Option<&Config>,
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
) -> Result<TwitterConfig> {
    let stored = stored.map(|c| c.twitter.clone()).unwrap_or_default();
    config::require_twitter_with(env_auth, env_csrf, &stored)
}

/// 登録を設定ファイルへ保存する。ほかの項目（認証情報を含む）は読み直した最新の値のまま残す。
fn save_registration(config_path: &Path, registration: Registration) -> Result<()> {
    Config::modify(config_path, |current| {
        let mut config = current.unwrap_or_default();
        config.registration = Some(registration);
        Ok::<_, AngelicAngelError>((Some(config), ()))
    })
}

async fn cmd_register(config_path: &Path) -> Result<()> {
    eprintln!("{}", style("Registering push subscription").bold());
    eprintln!();

    let stored = Config::load_optional(config_path)?;
    let (env_auth, env_csrf) = config::twitter_env_pair();
    let twitter = register_twitter(stored.as_ref(), env_auth.as_deref(), env_csrf.as_deref())?;

    let subscription = spin(
        "Registering with AutoPush...",
        "AutoPush registered",
        push::subscribe(),
    )
    .await?;

    spin(
        "Registering with Twitter...",
        "Twitter Push registered",
        twitter::register(&twitter, &subscription),
    )
    .await?;

    save_registration(
        config_path,
        Registration {
            endpoint: subscription.endpoint,
            autopush: subscription.autopush,
            keys: subscription.keys,
        },
    )?;
    eprintln!();
    eprintln!(
        "{} Saved to {}",
        style("done").green().bold(),
        style(config_path.display()).underlined()
    );

    Ok(())
}

/// `listen` の起動時の進め方。
#[derive(Debug)]
enum ListenPlan {
    /// 保存済みの登録で受信を始める。
    Start(Registration),
    /// 登録が無いので、環境変数の認証情報で登録してから受信を始める。
    AutoRegister(TwitterConfig),
    /// 登録も、自動登録に使える環境変数も無い。
    Unregistered,
}

fn plan_listen(
    stored: Option<&Config>,
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
) -> Result<ListenPlan> {
    // 片方だけの設定は、登録済みでも起動時に気づけるようここで弾く。
    let env_twitter = config::env_twitter_with(env_auth, env_csrf)?;
    Ok(
        match (stored.and_then(|c| c.registration.clone()), env_twitter) {
            (Some(registration), _) => ListenPlan::Start(registration),
            (None, Some(twitter)) => ListenPlan::AutoRegister(twitter),
            (None, None) => ListenPlan::Unregistered,
        },
    )
}

/// 環境変数の認証情報で登録して保存する。
///
/// 保存に失敗すると再起動のたびに X 側の登録が積み重なるので、X へ登録する前に書き込めることを
/// 確かめる。登録処理を始めてからの失敗は、再起動ループを抑えるため待ってから返す。
async fn auto_register(config_path: &Path, twitter: &TwitterConfig) -> Result<Registration> {
    config::ensure_writable(config_path)?;

    eprintln!(
        "No registration found; registering with {} / {} from the environment",
        config::TWITTER_AUTH_TOKEN_ENV,
        config::TWITTER_CSRF_TOKEN_ENV
    );
    let registered = async {
        let subscription = push::subscribe().await?;
        twitter::register(twitter, &subscription).await?;
        let registration = Registration {
            endpoint: subscription.endpoint,
            autopush: subscription.autopush,
            keys: subscription.keys,
        };
        save_registration(config_path, registration.clone())?;
        Ok::<_, AngelicAngelError>(registration)
    }
    .await;

    match registered {
        Ok(registration) => {
            eprintln!(
                "{} Registered and saved to {}",
                style("done").green().bold(),
                style(config_path.display()).underlined()
            );
            Ok(registration)
        },
        Err(e) => {
            eprintln!(
                "{} automatic registration failed: {}",
                style("error:").red().bold(),
                e
            );
            eprintln!(
                "Waiting {}s before exiting so a restart loop does not hammer X.",
                AUTO_REGISTER_FAILURE_DELAY.as_secs()
            );
            tokio::time::sleep(AUTO_REGISTER_FAILURE_DELAY).await;
            Err(e)
        },
    }
}

async fn cmd_listen(config_path: &Path) -> Result<()> {
    let stored = Config::load_optional(config_path)?;
    let (env_auth, env_csrf) = config::twitter_env_pair();
    let registration = match plan_listen(stored.as_ref(), env_auth.as_deref(), env_csrf.as_deref())?
    {
        ListenPlan::Start(registration) => registration,
        ListenPlan::AutoRegister(twitter) => auto_register(config_path, &twitter).await?,
        ListenPlan::Unregistered => {
            // 従来どおり、設定ファイルが無ければ読み込み失敗、あれば未登録として報告する。
            Config::load(config_path)?;
            return Err(AngelicAngelError::Config(
                "No registration found. Run `register` first.".to_string(),
            ));
        },
    };

    tracing::info!(config = %config_path.display(), "config loaded, starting listener");

    listener::listen(registration, config_path).await?;

    Ok(())
}

/// `status` に出す認証情報の行。環境変数の値は出さず、取得元だけを示す。
fn credential_lines(resolved: Result<(TwitterConfig, TwitterSource)>) -> Vec<String> {
    let prefix = |value: &str| value.chars().take(20).collect::<String>();
    match resolved {
        Ok((_, TwitterSource::Environment)) => vec![format!(
            "  credentials from environment ({} / {})",
            config::TWITTER_AUTH_TOKEN_ENV,
            config::TWITTER_CSRF_TOKEN_ENV
        )],
        Ok((twitter, TwitterSource::Config)) => vec![
            format!("  auth_token  {}...", prefix(&twitter.auth_token)),
            format!("  ct0         {}...", prefix(&twitter.ct0)),
        ],
        Ok((_, TwitterSource::None)) => vec!["  credentials not set".to_string()],
        Err(e) => vec![format!("  credentials {e}")],
    }
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
            let resolved = config::resolve_twitter(&config.twitter);
            let from_env = matches!(resolved, Ok((_, TwitterSource::Environment)));
            for line in credential_lines(resolved) {
                eprintln!("{}", style(line).dim());
            }

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
                    if from_env {
                        eprintln!(
                            "  Run {} to register (or just start {}).",
                            style("angelic-angel register").bold(),
                            style("angelic-angel listen").bold()
                        );
                    } else {
                        eprintln!(
                            "  Run {} to register.",
                            style("angelic-angel register").bold()
                        );
                    }
                },
            }
        },
        Err(_) => {
            eprintln!(
                "{}  {}",
                style("Config").cyan().bold(),
                style("not found").red()
            );
            let resolved = config::resolve_twitter(&TwitterConfig::default());
            if matches!(resolved, Ok((_, TwitterSource::Environment)) | Err(_)) {
                for line in credential_lines(resolved) {
                    eprintln!("{}", style(line).dim());
                }
                eprintln!(
                    "  Run {} to register and create it.",
                    style("angelic-angel listen").bold()
                );
            } else {
                eprintln!(
                    "  Run {} to initialize.",
                    style("angelic-angel init").bold()
                );
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(auth_token: &str, ct0: &str, registered: bool) -> Config {
        let mut config = Config {
            twitter: TwitterConfig {
                auth_token: auth_token.into(),
                ct0: ct0.into(),
            },
            ..Config::default()
        };
        if registered {
            config.registration = Some(registration("stored"));
        }
        config
    }

    fn registration(uaid: &str) -> Registration {
        Registration {
            endpoint: "https://push.example/e".into(),
            autopush: config::AutoPushSession {
                uaid: uaid.into(),
                channel_id: "c".into(),
            },
            keys: config::WebPushKeys {
                public_key: vec![1],
                private_key: vec![2],
                auth_secret: vec![3],
            },
        }
    }

    #[test]
    fn register_prefers_env_and_works_without_a_config_file() {
        let file = stored("file-auth", "file-ct0", false);
        let t = register_twitter(Some(&file), Some("env-auth"), Some("env-ct0")).unwrap();
        assert_eq!(
            (t.auth_token.as_str(), t.ct0.as_str()),
            ("env-auth", "env-ct0")
        );
        let t = register_twitter(None, Some("env-auth"), Some("env-ct0")).unwrap();
        assert_eq!(t.auth_token, "env-auth");

        let t = register_twitter(Some(&file), None, None).unwrap();
        assert_eq!(t.auth_token, "file-auth");
        assert!(register_twitter(None, None, None).is_err());
        assert!(register_twitter(Some(&Config::default()), None, None).is_err());
    }

    #[test]
    fn register_rejects_a_half_set_environment() {
        let file = stored("file-auth", "file-ct0", false);
        for (auth, csrf) in [(Some("a"), None), (None, Some("c")), (Some("a"), Some(" "))] {
            assert!(register_twitter(Some(&file), auth, csrf).is_err());
            assert!(register_twitter(None, auth, csrf).is_err());
        }
    }

    #[test]
    fn listen_auto_registers_only_from_environment_without_a_registration() {
        // 登録済みなら環境変数があっても再登録しない。
        let registered = stored("", "", true);
        let plan = plan_listen(Some(&registered), Some("env-auth"), Some("env-ct0")).unwrap();
        assert!(matches!(plan, ListenPlan::Start(r) if r.autopush.uaid == "stored"));

        // 未登録なら環境変数の値で登録する（設定ファイルが無くても、ファイル側に別の値があっても）。
        for file in [None, Some(stored("file-auth", "file-ct0", false))] {
            let plan = plan_listen(file.as_ref(), Some("env-auth"), Some("env-ct0")).unwrap();
            assert!(matches!(
                plan,
                ListenPlan::AutoRegister(t) if t.auth_token == "env-auth" && t.ct0 == "env-ct0"
            ));
        }

        // 環境変数が無ければ、ファイルに認証情報があっても自動登録しない（従来どおり）。
        let file = stored("file-auth", "file-ct0", false);
        for file in [None, Some(&file)] {
            assert!(matches!(
                plan_listen(file, None, None).unwrap(),
                ListenPlan::Unregistered
            ));
            assert!(matches!(
                plan_listen(file, Some(" "), Some("")).unwrap(),
                ListenPlan::Unregistered
            ));
        }
    }

    #[test]
    fn listen_rejects_a_half_set_environment_even_when_registered() {
        let registered = stored("file-auth", "file-ct0", true);
        for file in [None, Some(&registered)] {
            assert!(plan_listen(file, Some("a"), None).is_err());
            assert!(plan_listen(file, None, Some("c")).is_err());
        }
    }

    #[test]
    fn registration_is_saved_next_to_the_existing_settings_without_credentials() {
        let dir = config::test_temp_dir();
        let path = dir.join("angelic-angel.toml");

        // 設定ファイルが無ければ新規作成する。環境変数の認証情報は書かれない。
        save_registration(&path, registration("first")).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("twitter") && !written.contains("auth_token"));
        assert_eq!(
            Config::load(&path)
                .unwrap()
                .registration
                .unwrap()
                .autopush
                .uaid,
            "first"
        );

        // 既存の Webhook・通知フィルタ・ファイル側の認証情報は保持する。
        let mut existing = stored("file-auth", "file-ct0", false);
        existing.webhook_endpoint = Some("https://example.com/h".into());
        existing.notification_filter.tweet = false;
        existing.save(&path).unwrap();
        save_registration(&path, registration("second")).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.twitter.auth_token, "file-auth");
        assert_eq!(
            loaded.webhook_endpoint.as_deref(),
            Some("https://example.com/h")
        );
        assert!(!loaded.notification_filter.tweet);
        assert_eq!(loaded.registration.unwrap().autopush.uaid, "second");
    }

    #[test]
    fn status_shows_the_environment_source_but_never_its_values() {
        let file = TwitterConfig {
            auth_token: "file-auth".into(),
            ct0: "file-ct0".into(),
        };
        let lines = credential_lines(config::resolve_twitter_with(
            Some("env-auth-secret"),
            Some("env-ct0-secret"),
            &file,
        ))
        .join("\n");
        assert!(lines.contains("from environment"), "{lines}");
        assert!(lines.contains("TWITTER_AUTH_TOKEN") && lines.contains("TWITTER_CSRF_TOKEN"));
        assert!(
            !lines.contains("secret") && !lines.contains("file-"),
            "{lines}"
        );

        let lines = credential_lines(config::resolve_twitter_with(None, None, &file)).join("\n");
        assert!(
            lines.contains("file-auth") && lines.contains("file-ct0"),
            "{lines}"
        );

        let lines = credential_lines(config::resolve_twitter_with(
            Some("env-auth-secret"),
            None,
            &file,
        ))
        .join("\n");
        assert!(lines.contains("TWITTER_CSRF_TOKEN is not"), "{lines}");
        assert!(
            !lines.contains("secret") && !lines.contains("file-"),
            "{lines}"
        );

        let lines = credential_lines(config::resolve_twitter_with(
            None,
            None,
            &TwitterConfig::default(),
        ))
        .join("\n");
        assert!(lines.contains("not set"), "{lines}");
    }
}
