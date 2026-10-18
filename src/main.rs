mod autopush;
mod cli;
mod config;
mod error;
mod listener;
mod machine;
mod machine_ledger;
mod notifications;
#[cfg(unix)]
mod privileges;
mod push;
mod status;
mod transaction;
mod tweet_lookup;
mod twitter;
mod web;
mod x_metadata;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use cli::{AccountArg, Cli, Commands};
use config::{
    Account, Config, ConfigLock, DEFAULT_ACCOUNT_ID, Registration, TwitterConfig, TwitterSource,
};
use console::style;
use dialoguer::{Input, Password};
use error::{AngelicAngelError, Result};
use indicatif::ProgressBar;
use tokio::task::JoinSet;
use tracing_subscriber::EnvFilter;

/// 自動登録に失敗してから終了するまでの待ち時間。`restart: unless-stopped` の再起動ループが
/// X や AutoPush のエンドポイントを連打しないための間隔になる。
const AUTO_REGISTER_FAILURE_DELAY: Duration = Duration::from_secs(30);

fn main() {
    let cli = Cli::parse();

    // 設定ファイルやロックを開く前に、root なら設定ディレクトリの所有者へ権限を落とす。
    #[cfg(unix)]
    {
        if let Err(e) = privileges::drop_to_config_owner(&cli.config) {
            eprintln!("{} {}", style("error:").red().bold(), e);
            std::process::exit(1);
        }
    }

    run(cli);
}

/// 権限を落とした後に tokio のランタイムを作る（スレッドを増やす前に uid を確定させる）。
#[tokio::main]
async fn run(cli: Cli) {
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

    // `listen` も SIGINT を自分で受け、受信タスクの終了を待ってから設定ロックを手放す。
    if let Commands::Listen = cli.command {
        let result = async {
            let _lock = ConfigLock::acquire(&config_path)?;
            cmd_listen(&config_path).await
        }
        .await;
        if let Err(e) = result {
            eprintln!("{} {}", style("error:").red().bold(), e);
            std::process::exit(1);
        }
        return;
    }

    let task = async {
        match cli.command {
            Commands::Init {
                account: AccountArg { id },
                auth_token,
                ct0,
            } => {
                let _lock = ConfigLock::acquire(&config_path)?;
                cmd_init(&config_path, &id, auth_token, ct0).await
            },
            Commands::Register(AccountArg { id }) => {
                let _lock = ConfigLock::acquire(&config_path)?;
                cmd_register(&config_path, &id).await
            },
            Commands::Status { account } => cmd_status(&config_path, account.as_deref()).await,
            Commands::Unregister(AccountArg { id }) => {
                let _lock = ConfigLock::acquire(&config_path)?;
                cmd_unregister(&config_path, &id).await
            },
            // `serve` and `listen` handle SIGINT themselves to stop their receivers gracefully.
            Commands::Serve { .. } | Commands::Listen => {
                unreachable!("handled before the interrupt guard")
            },
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
    account_id: &str,
    arg_auth_token: Option<String>,
    arg_ct0: Option<String>,
) -> Result<()> {
    eprintln!("{}", style("Initializing configuration").bold());
    eprintln!();

    // 入力を求める前に、既存の設定を読めることを確かめる（読めないまま上書きしない）。
    Config::load_optional(config_path)?;

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

    init_account(config_path, account_id, TwitterConfig { auth_token, ct0 })?;
    eprintln!(
        "{} Saved to {}",
        style("done").green().bold(),
        style(config_path.display()).underlined()
    );

    Ok(())
}

/// アカウントの認証情報を保存し、登録を消す。ほかのアカウントと、このアカウントの転送先・通知
/// フィルターはそのまま残す。ファイルが存在するのに読めない場合は、何も書かずに失敗する。
fn init_account(config_path: &Path, account_id: &str, twitter: TwitterConfig) -> Result<()> {
    Config::modify_or_create_account(config_path, account_id, |current| {
        let mut account = current.unwrap_or_else(|| Account::new(account_id));
        account.twitter = twitter;
        account.registration = None;
        Ok::<_, AngelicAngelError>((Some(account), ()))
    })
}

/// `register` が X に渡す認証情報。`default` は環境変数があれば設定ファイルが無くても登録できる。
fn register_twitter(
    stored: Option<&Config>,
    account_id: &str,
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
) -> Result<TwitterConfig> {
    let stored = stored
        .and_then(|c| c.account(account_id))
        .map(|a| a.twitter.clone())
        .unwrap_or_default();
    config::require_twitter_with(account_id, env_auth, env_csrf, &stored)
}

/// 登録を設定ファイルへ保存する。ほかの項目（認証情報を含む）は読み直した最新の値のまま残す。
fn save_registration(
    config_path: &Path,
    account_id: &str,
    registration: Registration,
) -> Result<()> {
    Config::modify_or_create_account(config_path, account_id, |current| {
        let mut account = current.unwrap_or_else(|| Account::new(account_id));
        account.registration = Some(registration);
        Ok::<_, AngelicAngelError>((Some(account), ()))
    })
}

async fn cmd_register(config_path: &Path, account_id: &str) -> Result<()> {
    eprintln!("{}", style("Registering push subscription").bold());
    eprintln!();

    let stored = Config::load_optional(config_path)?;
    let (env_auth, env_csrf) = config::twitter_env_pair();
    let twitter = register_twitter(
        stored.as_ref(),
        account_id,
        env_auth.as_deref(),
        env_csrf.as_deref(),
    )?;

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
        account_id,
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

/// `listen` でのアカウント 1 つの進め方。
#[derive(Debug)]
enum ListenPlan {
    /// 保存済みの登録で受信を始める。
    Start(Registration),
    /// 登録が無いので、環境変数の認証情報で登録してから受信を始める。`default` だけが取る。
    AutoRegister(TwitterConfig),
    /// `default` に登録も、自動登録に使える環境変数も無い。何もせず、警告も出さない。
    Unregistered,
    /// `default` 以外で登録が無い。環境変数は使えないので起動せず、警告する。
    Skipped,
}

/// すべてのアカウントの進め方を決める。
///
/// 環境変数が効くのは `default` だけで、`default` は設定に無くても暗黙に存在する（環境変数だけで
/// 動かす運用は設定ファイルが無いところから始まる）。片方だけの設定は、登録済みでも起動時に気づけ
/// るようここで弾く。
fn plan_listen(
    stored: Option<&Config>,
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
) -> Result<Vec<(String, ListenPlan)>> {
    let env_twitter = config::env_twitter_with(DEFAULT_ACCOUNT_ID, env_auth, env_csrf)?;
    let configured = stored.map(|c| c.accounts.as_slice()).unwrap_or_default();
    let ids = configured
        .iter()
        .map(|a| a.id.as_str())
        .chain(
            configured
                .iter()
                .all(|a| a.id != DEFAULT_ACCOUNT_ID)
                .then_some(DEFAULT_ACCOUNT_ID),
        )
        .collect::<Vec<_>>();
    Ok(ids
        .into_iter()
        .map(|id| {
            let registration = configured
                .iter()
                .find(|a| a.id == id)
                .and_then(|a| a.registration.clone());
            let plan = match registration {
                Some(registration) => ListenPlan::Start(registration),
                None if id != DEFAULT_ACCOUNT_ID => ListenPlan::Skipped,
                None => match &env_twitter {
                    Some(twitter) => ListenPlan::AutoRegister(twitter.clone()),
                    None => ListenPlan::Unregistered,
                },
            };
            (id.to_string(), plan)
        })
        .collect())
}

/// アカウント 1 つの受信が止まった理由。
#[derive(Debug)]
struct AccountFailure {
    error: AngelicAngelError,
    /// 自動登録の失敗。X や AutoPush を呼んだ後なので、再起動ループが連打しないよう終了前に待つ。
    auto_registration: bool,
}

impl AccountFailure {
    fn listener(error: AngelicAngelError) -> Self {
        Self {
            error,
            auto_registration: false,
        }
    }
}

/// 環境変数の認証情報で登録して保存する。
///
/// 保存に失敗すると再起動のたびに X 側の登録が積み重なるので、X へ登録する前に書き込めることを
/// 確かめる。その失敗は X を呼ぶ前なので待たない。
async fn auto_register(
    config_path: &Path,
    account_id: &str,
    twitter: &TwitterConfig,
) -> std::result::Result<Registration, AccountFailure> {
    config::ensure_writable(config_path).map_err(AccountFailure::listener)?;

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
        save_registration(config_path, account_id, registration.clone())?;
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
        Err(error) => Err(AccountFailure {
            error,
            auto_registration: true,
        }),
    }
}

/// アカウント 1 つ分の `listen`。登録が無ければ（`default` だけ）先に自動登録する。
async fn run_account(
    config_path: PathBuf,
    account_id: String,
    plan: ListenPlan,
) -> std::result::Result<(), AccountFailure> {
    let registration = match plan {
        ListenPlan::Start(registration) => registration,
        ListenPlan::AutoRegister(twitter) => {
            auto_register(&config_path, &account_id, &twitter).await?
        },
        ListenPlan::Unregistered | ListenPlan::Skipped => return Ok(()),
    };

    tracing::info!(
        config = %config_path.display(),
        account = %account_id,
        "config loaded, starting listener"
    );

    listener::listen(&account_id, registration, &config_path)
        .await
        .map_err(AccountFailure::listener)
}

/// 受信を並行して走らせ、全アカウントが止まるまで待つ。
///
/// 1 つのアカウントが失敗しても、そのアカウントだけが止まって ID 付きでログに残り、ほかは動き続ける。
/// エラーを返すのは全アカウントが失敗したときだけ（Docker の再起動ポリシーが効く）。その中に
/// 自動登録の失敗があれば、`failure_delay` だけ待ってから返す。`shutdown` が完了したら（失敗後の
/// 待ち中でも）受信タスクをすべて中断し、終了を待ってから `Ok` で返す。呼び出し側はその後で設定
/// ロックを手放せる。
async fn supervise<T, F, Fut>(
    accounts: Vec<(String, T)>,
    run: F,
    failure_delay: Duration,
    shutdown: impl Future<Output = ()>,
) -> Result<()>
where
    T: Send + 'static,
    F: Fn(String, T) -> Fut,
    Fut: Future<Output = std::result::Result<(), AccountFailure>> + Send + 'static,
{
    let mut shutdown = std::pin::pin!(shutdown);
    let total = accounts.len();
    let mut tasks = JoinSet::new();
    for (id, plan) in accounts {
        let receiver = run(id.clone(), plan);
        tasks.spawn(async move { (id, receiver.await) });
    }

    let mut failures: Vec<(String, AngelicAngelError)> = Vec::new();
    let mut auto_registration_failed = false;
    loop {
        let joined = tokio::select! {
            joined = tasks.join_next() => joined,
            () = &mut shutdown => {
                tasks.shutdown().await;
                return Ok(());
            },
        };
        let Some(joined) = joined else { break };
        // パニックしたタスクは ID を持ち帰れないので、ID 不明の失敗として数える。
        let (account_id, outcome) = joined.unwrap_or_else(|e| {
            let failure = AccountFailure::listener(AngelicAngelError::Config(format!(
                "a receiver task failed: {e}"
            )));
            ("(unknown)".to_string(), Err(failure))
        });
        match outcome {
            Ok(()) => tracing::info!(account = %account_id, "receiver ended"),
            Err(failure) => {
                if failure.auto_registration {
                    eprintln!(
                        "{} automatic registration failed for account {:?}: {}",
                        style("error:").red().bold(),
                        account_id,
                        failure.error
                    );
                } else {
                    eprintln!(
                        "{} account {:?} stopped: {}",
                        style("error:").red().bold(),
                        account_id,
                        failure.error
                    );
                }
                auto_registration_failed |= failure.auto_registration;
                failures.push((account_id, failure.error));
            },
        }
    }

    if failures.is_empty() || failures.len() < total {
        return Ok(());
    }
    if auto_registration_failed {
        eprintln!(
            "Waiting {}s before exiting so a restart loop does not hammer X.",
            failure_delay.as_secs()
        );
        tokio::select! {
            () = tokio::time::sleep(failure_delay) => {},
            () = &mut shutdown => return Ok(()),
        }
    }
    if failures.len() == 1 {
        return Err(failures.remove(0).1);
    }
    Err(AngelicAngelError::Config(format!(
        "every account stopped: {}",
        failures
            .iter()
            .map(|(id, e)| format!("{id}: {e}"))
            .collect::<Vec<_>>()
            .join("; ")
    )))
}

async fn cmd_listen(config_path: &Path) -> Result<()> {
    let stored = Config::load_optional(config_path)?;
    let (env_auth, env_csrf) = config::twitter_env_pair();
    let runnable = plan_listen(stored.as_ref(), env_auth.as_deref(), env_csrf.as_deref())?
        .into_iter()
        .filter(|(id, plan)| match plan {
            ListenPlan::Start(_) | ListenPlan::AutoRegister(_) => true,
            ListenPlan::Unregistered => false,
            ListenPlan::Skipped => {
                tracing::warn!(
                    account = %id,
                    "no registration for this account and no credentials from the environment; skipping it (run `register --account {id}`)"
                );
                false
            },
        })
        .collect::<Vec<_>>();

    if runnable.is_empty() {
        // 従来どおり、設定ファイルが無ければ読み込み失敗、あれば未登録として報告する。
        Config::load(config_path)?;
        return Err(AngelicAngelError::Config(
            "No registration found. Run `register` first.".to_string(),
        ));
    }

    let config_path = config_path.to_path_buf();
    supervise(
        runnable,
        |id, plan| run_account(config_path.clone(), id, plan),
        AUTO_REGISTER_FAILURE_DELAY,
        async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("\n{}", style("Interrupted").red().bold());
        },
    )
    .await
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

/// `status` に出すアカウント 1 つ分の行（認証情報と登録）。
fn account_status_lines(
    account: &Account,
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
) -> Vec<String> {
    let resolved = config::resolve_twitter_with(&account.id, env_auth, env_csrf, &account.twitter);
    let from_env = matches!(resolved, Ok((_, TwitterSource::Environment)));
    let mut lines = credential_lines(resolved)
        .into_iter()
        .map(|line| style(line).dim().to_string())
        .collect::<Vec<_>>();
    lines.push(String::new());

    // `default` のコマンドは従来どおり `--account` を付けない。
    let flag = if account.id == DEFAULT_ACCOUNT_ID {
        String::new()
    } else {
        format!(" --account {}", account.id)
    };
    match &account.registration {
        Some(reg) => {
            lines.push(format!(
                "{}  {}",
                style("Registration").cyan().bold(),
                style("active").green()
            ));
            lines.push(format!(
                "  endpoint    {}...",
                style(&reg.endpoint.chars().take(50).collect::<String>()).dim()
            ));
            lines.push(format!("  uaid        {}", style(&reg.autopush.uaid).dim()));
            lines.push(format!(
                "  channel_id  {}",
                style(&reg.autopush.channel_id).dim()
            ));
        },
        None => {
            lines.push(format!(
                "{}  {}",
                style("Registration").cyan().bold(),
                style("not registered").yellow()
            ));
            let register = style(format!("angelic-angel register{flag}")).bold();
            if from_env {
                lines.push(format!(
                    "  Run {} to register (or just start {}).",
                    register,
                    style("angelic-angel listen").bold()
                ));
            } else {
                lines.push(format!("  Run {register} to register."));
            }
        },
    }
    lines
}

/// `status` が出すアカウント。`only` があればそのアカウントだけ。
///
/// 設定に無い `default` は、`only` で指名されたとき、設定にアカウントが 1 つも無いとき、環境変数の
/// 認証情報があるときに限り暗黙のアカウントとして並べる（`listen` が起動する集合と揃える）。
fn status_accounts(
    config: &Config,
    only: Option<&str>,
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
) -> Result<Vec<Account>> {
    let implicit_default = || Account::new(DEFAULT_ACCOUNT_ID);
    match only {
        Some(id) => match config.account(id) {
            Some(account) => Ok(vec![account.clone()]),
            None if id == DEFAULT_ACCOUNT_ID => Ok(vec![implicit_default()]),
            None => Err(AngelicAngelError::Config(format!(
                "account {id:?} not found in the config"
            ))),
        },
        None => {
            let mut accounts = config.accounts.clone();
            let has_default = accounts.iter().any(|a| a.id == DEFAULT_ACCOUNT_ID);
            let env_given = !matches!(
                config::env_twitter_with(DEFAULT_ACCOUNT_ID, env_auth, env_csrf),
                Ok(None)
            );
            if !has_default && (accounts.is_empty() || env_given) {
                accounts.push(implicit_default());
            }
            Ok(accounts)
        },
    }
}

/// `status` の出力全体。設定を読めなかったとき（`loaded` が `None`）は従来どおり「見つからない」と
/// 表示する。アカウントが 1 つだけなら従来と同じ出力で、複数あるときだけ `Account: <id>` で区切る。
fn status_lines(
    config_path: &Path,
    loaded: Option<&Config>,
    only: Option<&str>,
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
) -> Result<Vec<String>> {
    let mut lines = vec![style("Status").bold().to_string(), String::new()];
    match loaded {
        Some(config) => {
            let accounts = status_accounts(config, only, env_auth, env_csrf)?;
            lines.push(format!(
                "{}  {}",
                style("Config").cyan().bold(),
                style(config_path.display()).dim()
            ));
            let labelled = accounts.len() > 1;
            for account in &accounts {
                if labelled {
                    lines.push(String::new());
                    lines.push(format!(
                        "{} {}",
                        style("Account:").cyan().bold(),
                        account.id
                    ));
                }
                lines.extend(account_status_lines(account, env_auth, env_csrf));
            }
        },
        None => {
            if let Some(id) = only.filter(|id| *id != DEFAULT_ACCOUNT_ID) {
                return Err(AngelicAngelError::Config(format!(
                    "account {id:?} not found in the config"
                )));
            }
            lines.push(format!(
                "{}  {}",
                style("Config").cyan().bold(),
                style("not found").red()
            ));
            let resolved = config::resolve_twitter_with(
                DEFAULT_ACCOUNT_ID,
                env_auth,
                env_csrf,
                &TwitterConfig::default(),
            );
            if matches!(resolved, Ok((_, TwitterSource::Environment)) | Err(_)) {
                lines.extend(
                    credential_lines(resolved)
                        .into_iter()
                        .map(|line| style(line).dim().to_string()),
                );
                lines.push(format!(
                    "  Run {} to register and create it.",
                    style("angelic-angel listen").bold()
                ));
            } else {
                lines.push(format!(
                    "  Run {} to initialize.",
                    style("angelic-angel init").bold()
                ));
            }
        },
    }
    Ok(lines)
}

async fn cmd_status(config_path: &Path, only: Option<&str>) -> Result<()> {
    let loaded = Config::load(config_path).ok();
    let (env_auth, env_csrf) = config::twitter_env_pair();
    let lines = status_lines(
        config_path,
        loaded.as_ref(),
        only,
        env_auth.as_deref(),
        env_csrf.as_deref(),
    )?;
    for line in lines {
        eprintln!("{line}");
    }
    Ok(())
}

async fn cmd_unregister(config_path: &Path, account_id: &str) -> Result<()> {
    eprintln!("{}", style("Unregistering").bold());
    eprintln!();

    let config = Config::load(config_path)?;
    let reg = config
        .account(account_id)
        .and_then(|a| a.registration.clone())
        .ok_or_else(|| {
            AngelicAngelError::Config(if account_id == DEFAULT_ACCOUNT_ID {
                "No registration found.".to_string()
            } else {
                format!("No registration found for account {account_id:?}.")
            })
        })?;

    spin(
        "Unregistering from AutoPush...",
        "AutoPush unregistered",
        autopush::unregister(&reg.autopush),
    )
    .await?;

    Config::modify_account(config_path, account_id, |current| {
        Ok::<_, AngelicAngelError>(match current {
            Some(mut account) => {
                account.registration = None;
                (Some(account), ())
            },
            None => (None, ()),
        })
    })?;

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
        let mut account = Account::new(DEFAULT_ACCOUNT_ID);
        account.twitter = TwitterConfig {
            auth_token: auth_token.into(),
            ct0: ct0.into(),
        };
        if registered {
            account.registration = Some(registration("stored"));
        }
        Config {
            accounts: vec![account],
        }
    }

    fn default_account(config: &Config) -> &Account {
        config.account(DEFAULT_ACCOUNT_ID).unwrap()
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

    fn default_register_twitter(
        stored: Option<&Config>,
        env_auth: Option<&str>,
        env_csrf: Option<&str>,
    ) -> Result<TwitterConfig> {
        register_twitter(stored, DEFAULT_ACCOUNT_ID, env_auth, env_csrf)
    }

    #[test]
    fn register_prefers_env_and_works_without_a_config_file() {
        let file = stored("file-auth", "file-ct0", false);
        let t = default_register_twitter(Some(&file), Some("env-auth"), Some("env-ct0")).unwrap();
        assert_eq!(
            (t.auth_token.as_str(), t.ct0.as_str()),
            ("env-auth", "env-ct0")
        );
        let t = default_register_twitter(None, Some("env-auth"), Some("env-ct0")).unwrap();
        assert_eq!(t.auth_token, "env-auth");

        let t = default_register_twitter(Some(&file), None, None).unwrap();
        assert_eq!(t.auth_token, "file-auth");
        assert!(default_register_twitter(None, None, None).is_err());
        assert!(default_register_twitter(Some(&Config::default()), None, None).is_err());
    }

    #[test]
    fn register_rejects_a_half_set_environment() {
        let file = stored("file-auth", "file-ct0", false);
        for (auth, csrf) in [(Some("a"), None), (None, Some("c")), (Some("a"), Some(" "))] {
            assert!(default_register_twitter(Some(&file), auth, csrf).is_err());
            assert!(default_register_twitter(None, auth, csrf).is_err());
        }
    }

    #[test]
    fn register_never_applies_the_environment_to_another_account() {
        let mut file = stored("file-auth", "file-ct0", false);
        file.upsert_account(account_with_twitter("work", "work-auth", "work-ct0"));
        file.upsert_account(Account::new("empty"));

        let t = register_twitter(Some(&file), "work", Some("env-auth"), Some("env-ct0")).unwrap();
        assert_eq!(
            (t.auth_token.as_str(), t.ct0.as_str()),
            ("work-auth", "work-ct0")
        );
        // 片方だけの環境変数は `default` のエラーで、ほかのアカウントには無関係。
        let t = register_twitter(Some(&file), "work", Some("env-auth"), None).unwrap();
        assert_eq!(t.auth_token, "work-auth");
        // 認証情報の無いアカウントは、環境変数があっても登録できない。
        let err = register_twitter(Some(&file), "empty", Some("env-auth"), Some("env-ct0"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"empty\""), "{err}");
        assert!(register_twitter(None, "work", Some("env-auth"), Some("env-ct0")).is_err());
    }

    fn account_with_twitter(id: &str, auth_token: &str, ct0: &str) -> Account {
        let mut account = Account::new(id);
        account.twitter = TwitterConfig {
            auth_token: auth_token.into(),
            ct0: ct0.into(),
        };
        account
    }

    fn registered_account(id: &str) -> Account {
        let mut account = account_with_twitter(id, &format!("{id}-auth"), &format!("{id}-ct0"));
        account.registration = Some(registration(id));
        account
    }

    fn config_of(accounts: Vec<Account>) -> Config {
        Config { accounts }
    }

    /// 各アカウントの進め方を `(id, 種別)` に直す。
    fn plan_kinds(plans: Vec<(String, ListenPlan)>) -> Vec<(String, &'static str)> {
        plans
            .into_iter()
            .map(|(id, plan)| {
                let kind = match plan {
                    ListenPlan::Start(_) => "start",
                    ListenPlan::AutoRegister(_) => "auto-register",
                    ListenPlan::Unregistered => "unregistered",
                    ListenPlan::Skipped => "skipped",
                };
                (id, kind)
            })
            .collect()
    }

    fn kinds(stored: Option<&Config>, env: Option<(&str, &str)>) -> Vec<(String, &'static str)> {
        let plans = plan_listen(stored, env.map(|e| e.0), env.map(|e| e.1)).unwrap();
        plan_kinds(plans)
    }

    fn expected(entries: &[(&str, &'static str)]) -> Vec<(String, &'static str)> {
        entries
            .iter()
            .map(|(id, kind)| (id.to_string(), *kind))
            .collect()
    }

    #[test]
    fn listen_auto_registers_only_from_environment_without_a_registration() {
        // 登録済みなら環境変数があっても再登録しない。
        let registered = stored("", "", true);
        let plans = plan_listen(Some(&registered), Some("env-auth"), Some("env-ct0")).unwrap();
        assert!(matches!(
            plans.as_slice(),
            [(id, ListenPlan::Start(r))] if id == DEFAULT_ACCOUNT_ID && r.autopush.uaid == "stored"
        ));

        // 未登録なら環境変数の値で登録する（設定ファイルが無くても、ファイル側に別の値があっても）。
        for file in [None, Some(stored("file-auth", "file-ct0", false))] {
            let plans = plan_listen(file.as_ref(), Some("env-auth"), Some("env-ct0")).unwrap();
            assert!(matches!(
                plans.as_slice(),
                [(id, ListenPlan::AutoRegister(t))]
                    if id == DEFAULT_ACCOUNT_ID && t.auth_token == "env-auth" && t.ct0 == "env-ct0"
            ));
        }

        // 環境変数が無ければ、ファイルに認証情報があっても自動登録しない（従来どおり）。
        let file = stored("file-auth", "file-ct0", false);
        for file in [None, Some(&file)] {
            for env in [(None, None), (Some(" "), Some(""))] {
                let plans = plan_listen(file, env.0, env.1).unwrap();
                assert_eq!(
                    plan_kinds(plans),
                    expected(&[(DEFAULT_ACCOUNT_ID, "unregistered")])
                );
            }
        }
    }

    #[test]
    fn listen_plans_every_account_and_gives_the_environment_to_default_only() {
        let env = Some(("env-auth", "env-ct0"));
        let idle = account_with_twitter("idle", "idle-auth", "idle-ct0");

        // 登録済みはすべて起動する。登録の無い `default` 以外は、環境変数があっても起動しない。
        let both = config_of(vec![
            registered_account("default"),
            registered_account("work"),
            idle.clone(),
        ]);
        let want = expected(&[("default", "start"), ("work", "start"), ("idle", "skipped")]);
        assert_eq!(kinds(Some(&both), env), want);
        assert_eq!(kinds(Some(&both), None), want);

        // 設定に `default` が無くても、環境変数があれば暗黙の `default` を自動登録する。
        let no_default = config_of(vec![registered_account("work"), idle.clone()]);
        assert_eq!(
            kinds(Some(&no_default), env),
            expected(&[
                ("work", "start"),
                ("idle", "skipped"),
                ("default", "auto-register")
            ])
        );
        // 環境変数が無ければ、暗黙の `default` は何もせず警告も出さない（`unregistered`）。
        assert_eq!(
            kinds(Some(&no_default), None),
            expected(&[
                ("work", "start"),
                ("idle", "skipped"),
                ("default", "unregistered")
            ])
        );

        // `default` が設定にあって未登録なら、環境変数で自動登録する。
        let unregistered_default = config_of(vec![
            account_with_twitter("default", "file-auth", "file-ct0"),
            registered_account("work"),
        ]);
        assert_eq!(
            kinds(Some(&unregistered_default), env),
            expected(&[("default", "auto-register"), ("work", "start")])
        );
        assert_eq!(
            kinds(Some(&unregistered_default), None),
            expected(&[("default", "unregistered"), ("work", "start")])
        );
    }

    #[test]
    fn listen_rejects_a_half_set_environment_even_when_registered() {
        let registered = stored("file-auth", "file-ct0", true);
        let several = config_of(vec![registered_account("default"), registered_account("w")]);
        for file in [None, Some(&registered), Some(&several)] {
            assert!(plan_listen(file, Some("a"), None).is_err());
            assert!(plan_listen(file, None, Some("c")).is_err());
        }
    }

    #[test]
    fn registration_is_saved_next_to_the_existing_settings_without_credentials() {
        let dir = config::test_temp_dir();
        let path = dir.join("angelic-angel.toml");

        // 設定ファイルが無ければ新規作成する。環境変数の認証情報は書かれない。
        save_registration(&path, DEFAULT_ACCOUNT_ID, registration("first")).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("twitter") && !written.contains("auth_token"));
        assert_eq!(
            default_account(&Config::load(&path).unwrap())
                .registration
                .clone()
                .unwrap()
                .autopush
                .uaid,
            "first"
        );

        // 既存の Webhook・通知フィルタ・ファイル側の認証情報は保持する。
        let mut existing = stored("file-auth", "file-ct0", false);
        let account = existing.account_mut(DEFAULT_ACCOUNT_ID).unwrap();
        account.webhook_endpoint = Some("https://example.com/h".into());
        account.notification_filter.tweet = false;
        existing.save(&path).unwrap();
        save_registration(&path, DEFAULT_ACCOUNT_ID, registration("second")).unwrap();
        let loaded = Config::load(&path).unwrap();
        let loaded = default_account(&loaded);
        assert_eq!(loaded.twitter.auth_token, "file-auth");
        assert_eq!(
            loaded.webhook_endpoint.as_deref(),
            Some("https://example.com/h")
        );
        assert!(!loaded.notification_filter.tweet);
        assert_eq!(
            loaded.registration.as_ref().unwrap().autopush.uaid,
            "second"
        );
    }

    #[test]
    fn registration_is_saved_into_the_named_account_only() {
        let path = config::test_temp_dir().join("angelic-angel.toml");
        config_of(vec![
            registered_account("default"),
            account_with_twitter("work", "work-auth", "work-ct0"),
        ])
        .save(&path)
        .unwrap();

        save_registration(&path, "work", registration("work-new")).unwrap();
        // 新しいアカウントは作られる。
        save_registration(&path, "fresh", registration("fresh-new")).unwrap();

        let loaded = Config::load(&path).unwrap();
        let uaid = |id: &str| {
            loaded
                .account(id)
                .and_then(|a| a.registration.as_ref())
                .map(|r| r.autopush.uaid.clone())
        };
        assert_eq!(uaid("default").as_deref(), Some("default"));
        assert_eq!(uaid("work").as_deref(), Some("work-new"));
        assert_eq!(uaid("fresh").as_deref(), Some("fresh-new"));
        assert_eq!(
            loaded.account("work").unwrap().twitter.auth_token,
            "work-auth"
        );
        assert!(save_registration(&path, "Bad Id", registration("x")).is_err());
    }

    fn broken_config(path: &Path) -> &'static str {
        let broken = "[[accounts]]\nid = \"default\"\nregistration = [broken\n";
        std::fs::write(path, broken).unwrap();
        broken
    }

    #[tokio::test]
    async fn init_refuses_to_overwrite_a_config_it_cannot_parse() {
        let path = config::test_temp_dir().join("angelic-angel.toml");
        let broken = broken_config(&path);
        let creds = TwitterConfig {
            auth_token: "a".into(),
            ct0: "c".into(),
        };

        assert!(init_account(&path, DEFAULT_ACCOUNT_ID, creds).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);

        // 認証情報を渡して対話を省いても、書き込みの前に読み込みの失敗で止まる。
        let result = cmd_init(&path, "work", Some("a".into()), Some("c".into())).await;
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    }

    #[tokio::test]
    async fn init_starts_fresh_only_when_the_file_does_not_exist() {
        let path = config::test_temp_dir().join("angelic-angel.toml");
        cmd_init(
            &path,
            "work",
            Some("work-auth".into()),
            Some("work-ct0".into()),
        )
        .await
        .unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(
            loaded.account("work").unwrap().twitter.auth_token,
            "work-auth"
        );
    }

    #[test]
    fn init_replaces_credentials_and_registration_of_one_account_only() {
        let path = config::test_temp_dir().join("angelic-angel.toml");
        let mut work = registered_account("work");
        work.webhook_endpoint = Some("https://work.example/h".into());
        work.webhook_secret = Some("work-secret".into());
        work.notification_filter.retweet = false;
        let default = registered_account("default");
        config_of(vec![default.clone(), work]).save(&path).unwrap();

        init_account(
            &path,
            "work",
            TwitterConfig {
                auth_token: "new-auth".into(),
                ct0: "new-ct0".into(),
            },
        )
        .unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(
            loaded
                .accounts
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            ["default", "work"]
        );
        let work = loaded.account("work").unwrap();
        assert_eq!(work.twitter.auth_token, "new-auth");
        assert_eq!(work.twitter.ct0, "new-ct0");
        assert!(work.registration.is_none());
        assert_eq!(
            work.webhook_endpoint.as_deref(),
            Some("https://work.example/h")
        );
        assert_eq!(work.webhook_secret.as_deref(), Some("work-secret"));
        assert!(!work.notification_filter.retweet);
        // 他のアカウントは registration も含めてそのまま。
        let kept = loaded.account("default").unwrap();
        assert_eq!(kept.twitter.auth_token, default.twitter.auth_token);
        assert_eq!(
            kept.registration.as_ref().unwrap().autopush.uaid,
            default.registration.as_ref().unwrap().autopush.uaid
        );

        // 新しいアカウントは末尾に追加される。
        init_account(&path, "extra", TwitterConfig::default()).unwrap();
        let ids = Config::load(&path).unwrap();
        assert_eq!(
            ids.accounts
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            ["default", "work", "extra"]
        );
        assert!(init_account(&path, "Bad Id", TwitterConfig::default()).is_err());
    }

    fn plain(lines: Vec<String>) -> String {
        console::strip_ansi_codes(&lines.join("\n")).into_owned()
    }

    fn status_text(
        loaded: Option<&Config>,
        only: Option<&str>,
        env: Option<(&str, &str)>,
    ) -> Result<String> {
        status_lines(
            Path::new("/data/c.toml"),
            loaded,
            only,
            env.map(|e| e.0),
            env.map(|e| e.1),
        )
        .map(plain)
    }

    const SINGLE_REGISTERED: &str = "\
Status

Config  /data/c.toml
  auth_token  file-auth...
  ct0         file-ct0...

Registration  active
  endpoint    https://push.example/e...
  uaid        stored
  channel_id  c";

    #[test]
    fn status_of_a_single_account_keeps_the_single_account_format() {
        let one = stored("file-auth", "file-ct0", true);
        assert_eq!(
            status_text(Some(&one), None, None).unwrap(),
            SINGLE_REGISTERED
        );
        // 認証情報が環境変数でも、`default` 1 つなら同じ並びで取得元だけを示す。
        let from_env = status_text(Some(&one), None, Some(("env-auth", "env-ct0"))).unwrap();
        assert_eq!(
            from_env,
            SINGLE_REGISTERED.replace(
                "  auth_token  file-auth...\n  ct0         file-ct0...",
                "  credentials from environment (TWITTER_AUTH_TOKEN / TWITTER_CSRF_TOKEN)"
            )
        );

        let unregistered = stored("file-auth", "file-ct0", false);
        let text = status_text(Some(&unregistered), None, None).unwrap();
        assert!(
            text.ends_with(
                "Registration  not registered\n  Run angelic-angel register to register."
            ),
            "{text}"
        );
        let text = status_text(Some(&unregistered), None, Some(("e-a", "e-c"))).unwrap();
        assert!(
            text.ends_with(
                "Registration  not registered\n  Run angelic-angel register to register (or just start angelic-angel listen)."
            ),
            "{text}"
        );

        // アカウントが 1 つも無い設定は、従来どおり `default` が未設定として出る。
        let text = status_text(Some(&Config::default()), None, None).unwrap();
        assert_eq!(
            text,
            "Status\n\nConfig  /data/c.toml\n  credentials not set\n\nRegistration  not registered\n  \
             Run angelic-angel register to register."
        );
    }

    #[test]
    fn status_without_a_config_keeps_the_not_found_output() {
        let text = status_text(None, None, None).unwrap();
        assert_eq!(
            text,
            "Status\n\nConfig  not found\n  Run angelic-angel init to initialize."
        );
        let text = status_text(None, None, Some(("env-auth", "env-ct0"))).unwrap();
        assert!(text.contains("credentials from environment"), "{text}");
        assert!(text.contains("angelic-angel listen"), "{text}");
        assert!(!text.contains("env-auth"), "{text}");
        let text = status_text(None, None, Some(("env-auth", " "))).unwrap();
        assert!(text.contains("TWITTER_CSRF_TOKEN is not"), "{text}");
        // `default` 以外を指名しても、設定が無ければ存在しない。
        assert!(status_text(None, Some("work"), None).is_err());
        assert!(status_text(None, Some(DEFAULT_ACCOUNT_ID), None).is_ok());
    }

    #[test]
    fn status_of_several_accounts_labels_each_block() {
        let several = config_of(vec![
            registered_account("default"),
            account_with_twitter("work", "work-auth", "work-ct0"),
        ]);
        let text = status_text(Some(&several), None, None).unwrap();
        assert_eq!(
            text,
            "\
Status

Config  /data/c.toml

Account: default
  auth_token  default-auth...
  ct0         default-ct0...

Registration  active
  endpoint    https://push.example/e...
  uaid        default
  channel_id  c

Account: work
  auth_token  work-auth...
  ct0         work-ct0...

Registration  not registered
  Run angelic-angel register --account work to register."
        );

        // 環境変数が効くのは `default` だけで、値は出さない。
        let text = status_text(Some(&several), None, Some(("env-auth-secret", "env-ct0"))).unwrap();
        assert!(text.contains("credentials from environment"), "{text}");
        assert!(text.contains("work-auth..."), "{text}");
        assert!(
            !text.contains("env-auth-secret") && !text.contains("default-auth"),
            "{text}"
        );
    }

    #[test]
    fn status_account_flag_prints_that_block_only() {
        let several = config_of(vec![
            registered_account("default"),
            account_with_twitter("work", "work-auth", "work-ct0"),
        ]);
        let text = status_text(Some(&several), Some("work"), None).unwrap();
        assert_eq!(
            text,
            "\
Status

Config  /data/c.toml
  auth_token  work-auth...
  ct0         work-ct0...

Registration  not registered
  Run angelic-angel register --account work to register."
        );
        let text = status_text(Some(&several), Some("default"), None).unwrap();
        assert!(
            text.contains("default-auth") && !text.contains("work-auth"),
            "{text}"
        );
        assert!(!text.contains("Account:"), "{text}");

        let err = status_text(Some(&several), Some("nope"), None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("\"nope\"") && err.contains("not found"),
            "{err}"
        );
        // 設定に無くても、`default` を指名すれば暗黙のアカウントとして出る。
        let only_work = config_of(vec![registered_account("work")]);
        assert!(
            status_text(Some(&only_work), Some(DEFAULT_ACCOUNT_ID), None)
                .unwrap()
                .contains("credentials not set")
        );
    }

    #[test]
    fn status_lists_the_implicit_default_only_when_listen_would_run_it() {
        let only_work = config_of(vec![registered_account("work")]);
        let text = status_text(Some(&only_work), None, None).unwrap();
        assert!(
            !text.contains("Account:") && !text.contains("credentials from"),
            "{text}"
        );
        assert!(text.contains("work-auth"), "{text}");

        let text = status_text(Some(&only_work), None, Some(("env-auth", "env-ct0"))).unwrap();
        assert!(
            text.contains("Account: work") && text.contains("Account: default"),
            "{text}"
        );
        assert!(text.contains("credentials from environment"), "{text}");
    }

    #[derive(Clone, Copy)]
    enum Behavior {
        /// 指定ミリ秒の後に正常終了する。
        Succeed(u64),
        /// 指定ミリ秒の後に受信の失敗で止まる。
        Fail(u64),
        /// 指定ミリ秒の後に自動登録の失敗で止まる。
        FailAutoRegistration(u64),
    }

    type Done = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    async fn behave(
        done: Done,
        id: String,
        behavior: Behavior,
    ) -> std::result::Result<(), AccountFailure> {
        let (millis, failure) = match behavior {
            Behavior::Succeed(ms) => (ms, None),
            Behavior::Fail(ms) => (ms, Some(false)),
            Behavior::FailAutoRegistration(ms) => (ms, Some(true)),
        };
        tokio::time::sleep(Duration::from_millis(millis)).await;
        done.lock().unwrap().push(id);
        match failure {
            None => Ok(()),
            Some(auto_registration) => Err(AccountFailure {
                error: AngelicAngelError::Config("boom".into()),
                auto_registration,
            }),
        }
    }

    async fn run_supervised(
        accounts: &[(&str, Behavior)],
        failure_delay: Duration,
    ) -> (Result<()>, Vec<String>, Duration) {
        let done = Done::default();
        let started = std::time::Instant::now();
        let result = supervise(
            accounts
                .iter()
                .map(|(id, behavior)| (id.to_string(), *behavior))
                .collect(),
            |id, behavior| behave(done.clone(), id, behavior),
            failure_delay,
            std::future::pending(),
        )
        .await;
        let finished = done.lock().unwrap().clone();
        (result, finished, started.elapsed())
    }

    const LONG: Duration = Duration::from_secs(5);
    const QUICK: Duration = Duration::from_secs(2);

    #[tokio::test]
    async fn a_failing_account_stops_alone_and_the_others_keep_running() {
        let (result, finished, elapsed) = run_supervised(
            &[
                ("a", Behavior::Fail(0)),
                ("b", Behavior::Succeed(150)),
                ("c", Behavior::FailAutoRegistration(0)),
            ],
            LONG,
        )
        .await;

        // `b` は `a` と `c` の失敗の後も最後まで動き、全員が失敗したわけではないので成功で終わる。
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(finished.last().map(String::as_str), Some("b"));
        assert_eq!(finished.len(), 3);
        // 自動登録の失敗があっても、ほかが動いている限り待たない。
        assert!(elapsed < QUICK, "{elapsed:?}");
    }

    #[tokio::test]
    async fn supervision_succeeds_when_every_receiver_ends_cleanly() {
        let (result, finished, _) = run_supervised(
            &[("a", Behavior::Succeed(0)), ("b", Behavior::Succeed(20))],
            LONG,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(finished, ["a", "b"]);
    }

    #[tokio::test]
    async fn supervision_fails_only_when_every_account_failed() {
        // 1 件だけなら、そのエラーをそのまま返す。
        let (result, _, elapsed) = run_supervised(&[("a", Behavior::Fail(0))], LONG).await;
        assert_eq!(result.unwrap_err().to_string(), "config error: boom");
        // 自動登録の失敗ではないので待たない。
        assert!(elapsed < QUICK, "{elapsed:?}");

        let (result, finished, elapsed) = run_supervised(
            &[
                ("a", Behavior::Fail(0)),
                ("b", Behavior::Fail(30)),
                ("c", Behavior::Fail(60)),
            ],
            LONG,
        )
        .await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("every account stopped"), "{error}");
        for id in ["a", "b", "c"] {
            assert!(error.contains(&format!("{id}: ")), "{error}");
        }
        assert_eq!(finished, ["a", "b", "c"]);
        assert!(elapsed < QUICK, "{elapsed:?}");
    }

    #[tokio::test]
    async fn only_an_automatic_registration_failure_delays_the_failing_exit() {
        let delay = Duration::from_millis(400);

        let (result, _, elapsed) =
            run_supervised(&[("a", Behavior::FailAutoRegistration(0))], delay).await;
        assert!(result.is_err());
        assert!(elapsed >= delay, "{elapsed:?}");

        // 自動登録の失敗が混ざっていれば、全件が失敗した時に 1 回だけ待つ。
        let (result, finished, elapsed) = run_supervised(
            &[
                ("a", Behavior::FailAutoRegistration(0)),
                ("b", Behavior::Fail(0)),
                ("c", Behavior::FailAutoRegistration(0)),
            ],
            delay,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(finished.len(), 3);
        assert!(elapsed >= delay && elapsed < delay * 2, "{elapsed:?}");

        // 受信の失敗だけなら待たない。
        let (result, _, elapsed) =
            run_supervised(&[("a", Behavior::Fail(0)), ("b", Behavior::Fail(0))], LONG).await;
        assert!(result.is_err());
        assert!(elapsed < QUICK, "{elapsed:?}");
    }

    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn shutdown_waits_for_the_receivers_to_be_dropped() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dropped = Arc::new(AtomicUsize::new(0));
        let result = supervise(
            vec![("a".to_string(), ()), ("b".to_string(), ())],
            |_, ()| {
                let flag = DropFlag(dropped.clone());
                async move {
                    let _flag = flag;
                    std::future::pending::<std::result::Result<(), AccountFailure>>().await
                }
            },
            LONG,
            tokio::time::sleep(Duration::from_millis(100)),
        )
        .await;

        // 戻った時点で、受信タスクは中断されて破棄されている（待たずに数えて確かめる）。
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn shutdown_interrupts_the_wait_after_a_failure() {
        let started = std::time::Instant::now();
        let result = supervise(
            vec![("a".to_string(), Behavior::FailAutoRegistration(0))],
            |id, behavior| behave(Done::default(), id, behavior),
            Duration::from_secs(60),
            tokio::time::sleep(Duration::from_millis(100)),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(started.elapsed() < QUICK, "{:?}", started.elapsed());
    }

    #[test]
    fn status_shows_the_environment_source_but_never_its_values() {
        let file = TwitterConfig {
            auth_token: "file-auth".into(),
            ct0: "file-ct0".into(),
        };
        let lines = credential_lines(config::resolve_twitter_with(
            DEFAULT_ACCOUNT_ID,
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

        let lines = credential_lines(config::resolve_twitter_with(
            DEFAULT_ACCOUNT_ID,
            None,
            None,
            &file,
        ))
        .join("\n");
        assert!(
            lines.contains("file-auth") && lines.contains("file-ct0"),
            "{lines}"
        );

        let lines = credential_lines(config::resolve_twitter_with(
            DEFAULT_ACCOUNT_ID,
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
            DEFAULT_ACCOUNT_ID,
            None,
            None,
            &TwitterConfig::default(),
        ))
        .join("\n");
        assert!(lines.contains("not set"), "{lines}");
    }
}
