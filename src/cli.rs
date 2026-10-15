use crate::config::{DEFAULT_ACCOUNT_ID, validate_account_id};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

fn parse_account_id(raw: &str) -> Result<String, String> {
    validate_account_id(raw)
        .map(|()| raw.to_string())
        .map_err(|e| e.to_string())
}

/// The account a command works on.
#[derive(Args)]
pub struct AccountArg {
    /// Account id (a-z, 0-9, '_' or '-'). Credentials from the environment only apply to `default`
    #[arg(
        long = "account",
        value_name = "ID",
        default_value = DEFAULT_ACCOUNT_ID,
        value_parser = parse_account_id
    )]
    pub id: String,
}

#[derive(Parser)]
#[command(name = "angelic-angel")]
#[command(
    about = "Twitter Web Push Receiver",
    long_about = "A server for receiving tweet notifications by emulating browser Web Push."
)]
pub struct Cli {
    /// Configuration file path
    #[arg(
        short,
        long,
        env = "ANGELIC_ANGEL_CONFIG",
        default_value = "angelic-angel.toml"
    )]
    pub config: PathBuf,

    /// Enable verbose output (debug logs)
    #[arg(short, long)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize config file (interactive if no arguments given)
    Init {
        #[command(flatten)]
        account: AccountArg,
        /// Twitter auth_token
        #[arg(long)]
        auth_token: Option<String>,
        /// Twitter ct0 (CSRF token)
        #[arg(long)]
        ct0: Option<String>,
    },
    /// Register AutoPush subscription and Twitter Push endpoint
    Register(AccountArg),
    /// Start listening for push notifications for every registered account
    Listen,
    /// Show current config and registration status (every account unless --account is given)
    Status {
        /// Show only this account
        #[arg(long = "account", value_name = "ID", value_parser = parse_account_id)]
        account: Option<String>,
    },
    /// Unregister AutoPush subscription
    Unregister(AccountArg),
    /// Run the WebUI server together with the managed receiver
    Serve {
        /// Address to bind the HTTP server to
        #[arg(long, env = "WEBUI_BIND", default_value = "127.0.0.1:8080")]
        bind: String,
        /// Directory containing the built WebUI assets
        #[arg(long, env = "WEBUI_ASSETS_DIR", default_value = "web/dist")]
        assets_dir: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_token_command_is_gone() {
        assert!(Cli::try_parse_from(["angelic-angel", "web-token"]).is_err());
        assert!(Cli::try_parse_from(["angelic-angel", "serve"]).is_ok());
    }

    fn parse(args: &[&str]) -> Commands {
        let args = ["angelic-angel"].into_iter().chain(args.iter().copied());
        Cli::try_parse_from(args).unwrap().command
    }

    #[test]
    fn account_defaults_to_default_and_is_validated() {
        assert!(matches!(
            parse(&["register"]),
            Commands::Register(AccountArg { id }) if id == DEFAULT_ACCOUNT_ID
        ));
        assert!(matches!(
            parse(&["unregister", "--account", "work-2"]),
            Commands::Unregister(AccountArg { id }) if id == "work-2"
        ));
        assert!(matches!(
            parse(&["init", "--account", "work", "--ct0", "c"]),
            Commands::Init { account: AccountArg { id }, ct0: Some(_), auth_token: None }
                if id == "work"
        ));
        assert!(matches!(
            parse(&["status"]),
            Commands::Status { account: None }
        ));
        assert!(matches!(
            parse(&["status", "--account", "work"]),
            Commands::Status { account: Some(id) } if id == "work"
        ));

        let too_long = "x".repeat(33);
        for bad in ["", "Work", "-a", "a b", "a/b", too_long.as_str()] {
            for command in ["init", "register", "unregister", "status"] {
                assert!(
                    Cli::try_parse_from(["angelic-angel", command, "--account", bad]).is_err(),
                    "{command} --account {bad:?}"
                );
            }
        }
        assert!(Cli::try_parse_from(["angelic-angel", "listen", "--account", "work"]).is_err());
    }

    #[test]
    fn config_flag_is_parsed_and_backed_by_the_environment_variable() {
        // 環境変数との優先順位は、実バイナリを使う tests/config_env.rs で確かめる。
        let cli =
            Cli::try_parse_from(["angelic-angel", "--config", "/tmp/c.toml", "status"]).unwrap();
        assert_eq!(cli.config, PathBuf::from("/tmp/c.toml"));
        let command = <Cli as clap::CommandFactory>::command();
        let arg = command
            .get_arguments()
            .find(|a| a.get_id() == "config")
            .unwrap();
        assert_eq!(arg.get_env().unwrap(), "ANGELIC_ANGEL_CONFIG");
    }
}
