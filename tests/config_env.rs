//! 実際のバイナリで、設定ファイルの選び方・複数アカウント・旧形式の移行・環境変数の扱いを確かめる。
//! 環境変数は子プロセスにだけ渡し、テストプロセス自身の環境は変更しない。
//! 認証情報はすべてダミー。`status` / `init` / `unregister`（登録が無いときのエラー）だけを実行し、
//! ネットワークに出る `register` / `listen` は実行しない（それらは scripts/docker-smoke.sh が
//! `--network none` で確かめる）。

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 破棄時（テストが失敗した場合も）に中身ごと消す一時ディレクトリ。
struct TempDir(PathBuf);

impl Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(name: &str) -> TempDir {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("aa-cli-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

fn write_config(path: &Path) {
    std::fs::write(path, "[twitter]\nauth_token = \"a\"\nct0 = \"b\"\n").unwrap();
}

/// 環境の影響を受けないバイナリ起動。
fn command(cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_angelic-angel"));
    command
        .current_dir(cwd)
        .env_remove("ANGELIC_ANGEL_CONFIG")
        .env_remove("TWITTER_AUTH_TOKEN")
        .env_remove("TWITTER_CSRF_TOKEN")
        .env_remove("WEBHOOK_ENDPOINT")
        .env_remove("WEBHOOK_SECRET_KEY")
        .env("NO_COLOR", "1");
    command
}

/// `--config <config> <args...>` を、指定した環境変数付きで実行する。
fn run(cwd: &Path, config: &Path, envs: &[(&str, &str)], args: &[&str]) -> Output {
    command(cwd)
        .arg("--config")
        .arg(config)
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .unwrap()
}

/// stdout と stderr を続けた文字列（`status` は stderr に出す）。
fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// 成功を確かめて出力全体を返す。
fn run_ok(cwd: &Path, config: &Path, envs: &[(&str, &str)], args: &[&str]) -> String {
    let output = run(cwd, config, envs, args);
    let out = text(&output);
    assert!(output.status.success(), "{args:?} failed: {out}");
    out
}

/// 失敗（終了コード付き）を確かめて出力全体を返す。
fn run_fail(cwd: &Path, config: &Path, envs: &[(&str, &str)], args: &[&str], code: i32) -> String {
    let output = run(cwd, config, envs, args);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(code), "{args:?}: {out}");
    out
}

/// 複数アカウントの `status` から、`Account: <id>` で始まる 1 ブロックを取り出す。
fn account_block<'a>(out: &'a str, id: &str) -> &'a str {
    let marker = format!("Account: {id}\n");
    let start = out
        .find(&marker)
        .unwrap_or_else(|| panic!("no block for account {id}: {out}"));
    out[start + marker.len()..]
        .split("Account: ")
        .next()
        .unwrap()
}

/// ディレクトリ直下のファイルの内容を続けた文字列（秘密値が書かれていないことの検査用）。
fn all_file_contents(dir: &Path) -> String {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .map(|entry| String::from_utf8_lossy(&std::fs::read(entry.path()).unwrap()).into_owned())
        .collect()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// `status` は認証情報の先頭 20 文字だけを表示する。ダミー値は先頭 20 文字の後ろに `HIDDENTAIL`
// を付け、その印が出力に現れないことで「全体は出ない」ことを確かめる。
const DEFAULT_AUTH: &str = "default-auth-visible-HIDDENTAIL1";
const DEFAULT_CT0: &str = "default-ct0-visible-HIDDENTAIL2";
const WORK_AUTH: &str = "work-auth-token-vis-HIDDENTAIL3";
const WORK_CT0: &str = "work-ct0-token-visi-HIDDENTAIL4";
const DEFAULT_AUTH_SHOWN: &str = "default-auth-visible...";
const DEFAULT_CT0_SHOWN: &str = "default-ct0-visible-...";
const WORK_AUTH_SHOWN: &str = "work-auth-token-vis-...";
const WORK_CT0_SHOWN: &str = "work-ct0-token-visi-...";
const WORK_WEBHOOK_SECRET: &str = "dummy-webhook-secret-WHSECRET";
const ENV_AUTH: &str = "env-auth-dummy-ENVSECRET1";
const ENV_CT0: &str = "env-ct0-dummy-ENVSECRET2";

/// 出力のどこにも現れてはいけない値の印。
const NEVER_SHOWN: [&str; 7] = [
    "HIDDENTAIL1",
    "HIDDENTAIL2",
    "HIDDENTAIL3",
    "HIDDENTAIL4",
    "WHSECRET",
    "ENVSECRET1",
    "ENVSECRET2",
];

fn assert_no_secret(out: &str) {
    for secret in NEVER_SHOWN {
        assert!(!out.contains(secret), "leaked {secret}: {out}");
    }
}

/// 実在しない登録（`status` の表示確認にだけ使う）。`table` は `registration` か
/// `accounts.registration`。
fn registration(table: &str, n: u8) -> String {
    format!(
        "[{table}]\nendpoint = \"https://invalid.example/push-{n}\"\n\n\
         [{table}.autopush]\n\
         uaid = \"00000000-0000-0000-0000-00000000000{n}\"\n\
         channel_id = \"00000000-0000-0000-0000-00000000001{n}\"\n\n\
         [{table}.keys]\n\
         public_key = [1, 2, 3]\nprivate_key = [1, 2, 3]\nauth_secret = [1, 2, 3]\n"
    )
}

/// `default`（登録済み）と `work`（未登録・Webhook secret あり）の 2 アカウント。
fn two_account_config() -> String {
    format!(
        "[[accounts]]\nid = \"default\"\n\n\
         [accounts.twitter]\nauth_token = \"{DEFAULT_AUTH}\"\nct0 = \"{DEFAULT_CT0}\"\n\n\
         {}\n\
         [[accounts]]\nid = \"work\"\n\
         webhook_endpoint = \"https://hook.invalid/work\"\n\
         webhook_secret = \"{WORK_WEBHOOK_SECRET}\"\n\n\
         [accounts.twitter]\nauth_token = \"{WORK_AUTH}\"\nct0 = \"{WORK_CT0}\"\n",
        registration("accounts.registration", 1)
    )
}

/// 2 アカウントとも認証情報だけで、登録は無い（`unregister` がネットワークに出ない構成）。
fn unregistered_config() -> String {
    format!(
        "[[accounts]]\nid = \"default\"\n\n\
         [accounts.twitter]\nauth_token = \"{DEFAULT_AUTH}\"\nct0 = \"{DEFAULT_CT0}\"\n\n\
         [[accounts]]\nid = \"work\"\n\n\
         [accounts.twitter]\nauth_token = \"{WORK_AUTH}\"\nct0 = \"{WORK_CT0}\"\n"
    )
}

/// 旧形式（トップレベルの `[twitter]` / `[registration]`）。
fn legacy_config() -> String {
    format!(
        "[twitter]\nauth_token = \"{DEFAULT_AUTH}\"\nct0 = \"{DEFAULT_CT0}\"\n\n{}",
        registration("registration", 1)
    )
}

/// `status` を実行し、stderr を返す。
fn status(cwd: &Path, config_env: Option<&Path>, config_arg: Option<&Path>) -> String {
    let mut command = command(cwd);
    if let Some(path) = config_env {
        command.env("ANGELIC_ANGEL_CONFIG", path);
    }
    if let Some(path) = config_arg {
        command.arg("--config").arg(path);
    }
    let output = command.arg("status").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn config_path_comes_from_environment_and_flag_wins() {
    let dir = temp_dir("config");
    let from_env = dir.join("from-env.toml");
    let from_flag = dir.join("from-flag.toml");
    write_config(&from_env);
    write_config(&from_flag);

    let out = status(&dir, Some(&from_env), None);
    assert!(out.contains(from_env.to_str().unwrap()), "{out}");

    let out = status(&dir, Some(&from_env), Some(&from_flag));
    assert!(out.contains(from_flag.to_str().unwrap()), "{out}");
    assert!(!out.contains(from_env.to_str().unwrap()), "{out}");

    // どちらも無ければカレントディレクトリの既定ファイルを使う。
    write_config(&dir.join("angelic-angel.toml"));
    let out = status(&dir, None, None);
    assert!(out.contains("angelic-angel.toml"), "{out}");
    assert!(!out.contains("from-env"), "{out}");
}

#[test]
fn status_lists_every_account_and_account_flag_selects_one() {
    let dir = temp_dir("accounts");
    let config = dir.join("config.toml");
    let source = two_account_config();
    std::fs::write(&config, &source).unwrap();

    // 指定なしは全アカウントを `Account: <id>` で区切って並べる。
    let all = run_ok(&dir, &config, &[], &["status"]);
    let default = account_block(&all, "default");
    assert!(default.contains(DEFAULT_AUTH_SHOWN), "{all}");
    assert!(default.contains(DEFAULT_CT0_SHOWN), "{all}");
    assert!(default.contains("Registration  active"), "{all}");
    assert!(!default.contains("work-"), "{all}");
    let work = account_block(&all, "work");
    assert!(work.contains(WORK_AUTH_SHOWN), "{all}");
    assert!(work.contains(WORK_CT0_SHOWN), "{all}");
    assert!(work.contains("not registered"), "{all}");
    assert!(work.contains("register --account work"), "{all}");
    assert!(!work.contains("default-"), "{all}");
    // 認証情報は先頭 20 文字まで。Webhook secret は出さない。
    assert_no_secret(&all);

    // `--account` は 1 アカウントだけ（区切りの見出しは付けない）。
    let work_only = run_ok(&dir, &config, &[], &["status", "--account", "work"]);
    assert!(work_only.contains(WORK_AUTH_SHOWN), "{work_only}");
    assert!(!work_only.contains("default-"), "{work_only}");
    assert_no_secret(&work_only);
    let default_only = run_ok(&dir, &config, &[], &["status", "--account", "default"]);
    assert!(default_only.contains(DEFAULT_AUTH_SHOWN), "{default_only}");
    assert!(!default_only.contains("work-"), "{default_only}");
    assert_no_secret(&default_only);

    // 設定に無いアカウントと、形式が不正な ID は失敗する。
    let missing = run_fail(&dir, &config, &[], &["status", "--account", "nope"], 1);
    assert!(missing.contains("\"nope\" not found"), "{missing}");
    run_fail(&dir, &config, &[], &["status", "--account", "Work"], 2);

    // 読むだけ: 設定は書き換えず、旧形式の退避ファイルも作らない。
    assert_eq!(std::fs::read_to_string(&config).unwrap(), source);
    assert!(!dir.join("config.toml.legacy.bak").exists());
}

#[test]
fn legacy_config_is_backed_up_once_before_the_first_save() {
    let dir = temp_dir("legacy");
    let config = dir.join("angelic-angel.toml");
    let backup = dir.join("angelic-angel.toml.legacy.bak");
    let original = legacy_config();
    std::fs::write(&config, &original).unwrap();

    // 読むだけでは移行も退避もしない。旧形式は `default` として見える。
    let out = run_ok(&dir, &config, &[], &["status"]);
    assert!(out.contains(DEFAULT_AUTH_SHOWN), "{out}");
    assert!(out.contains("Registration  active"), "{out}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
    assert!(!backup.exists());

    // 最初の保存（別アカウントの追加）で、原本をそのまま退避してから変換する。
    let out = run_ok(
        &dir,
        &config,
        &[],
        &[
            "init",
            "--account",
            "work",
            "--auth-token",
            WORK_AUTH,
            "--ct0",
            WORK_CT0,
        ],
    );
    assert_no_secret(&out);
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
    let converted = std::fs::read_to_string(&config).unwrap();
    assert!(converted.contains("[[accounts]]"), "{converted}");
    assert!(converted.contains("id = \"default\""), "{converted}");
    assert!(converted.contains("id = \"work\""), "{converted}");
    assert!(!converted.starts_with("[twitter]"), "{converted}");
    assert!(!converted.contains("\n[twitter]"), "{converted}");
    assert!(!converted.contains("\n[registration"), "{converted}");
    // `default` の認証情報と登録は失われない。
    assert!(converted.contains(DEFAULT_AUTH), "{converted}");
    assert!(converted.contains("push-1"), "{converted}");
    #[cfg(unix)]
    {
        assert_eq!(mode(&backup), 0o600);
        assert_eq!(mode(&config), 0o600);
    }

    // 2 回目以降の保存では退避を上書きしない。
    run_ok(
        &dir,
        &config,
        &[],
        &[
            "init",
            "--account",
            "work",
            "--auth-token",
            "other-auth",
            "--ct0",
            "other-ct0",
        ],
    );
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);

    // 変換後は 2 アカウントが並び、`default` の登録は残り、`work` は未登録。
    let all = run_ok(&dir, &config, &[], &["status"]);
    assert!(
        account_block(&all, "default").contains("Registration  active"),
        "{all}"
    );
    assert!(
        account_block(&all, "work").contains("not registered"),
        "{all}"
    );
    assert!(!all.contains("HIDDENTAIL"), "{all}");
}

#[test]
fn an_existing_legacy_backup_is_never_overwritten() {
    let dir = temp_dir("backup");
    let config = dir.join("angelic-angel.toml");
    let backup = dir.join("angelic-angel.toml.legacy.bak");
    std::fs::write(&config, legacy_config()).unwrap();
    std::fs::write(&backup, "SENTINEL: an earlier backup\n").unwrap();

    run_ok(
        &dir,
        &config,
        &[],
        &["init", "--auth-token", "new-auth", "--ct0", "new-ct0"],
    );

    assert_eq!(
        std::fs::read_to_string(&backup).unwrap(),
        "SENTINEL: an earlier backup\n"
    );
    let converted = std::fs::read_to_string(&config).unwrap();
    assert!(converted.contains("[[accounts]]"), "{converted}");
    assert!(converted.contains("new-auth"), "{converted}");
}

#[test]
fn mixing_accounts_with_legacy_keys_is_rejected_without_writing() {
    let dir = temp_dir("mixed");
    let config = dir.join("angelic-angel.toml");
    let source = "[twitter]\nauth_token = \"a\"\nct0 = \"b\"\n\n[[accounts]]\nid = \"work\"\n";
    std::fs::write(&config, source).unwrap();

    let out = run_fail(
        &dir,
        &config,
        &[],
        &[
            "init",
            "--account",
            "work",
            "--auth-token",
            "x",
            "--ct0",
            "y",
        ],
        1,
    );
    assert!(out.contains("cannot be combined"), "{out}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), source);
    assert!(!dir.join("angelic-angel.toml.legacy.bak").exists());
}

#[test]
fn environment_credentials_apply_to_the_default_account_only() {
    let dir = temp_dir("env");
    let config = dir.join("config.toml");
    let envs = [
        ("TWITTER_AUTH_TOKEN", ENV_AUTH),
        ("TWITTER_CSRF_TOKEN", ENV_CT0),
    ];

    // 両方に設定ファイルの認証情報があっても、環境変数が効くのは `default` だけ。
    std::fs::write(&config, two_account_config()).unwrap();
    let all = run_ok(&dir, &config, &envs, &["status"]);
    let default = account_block(&all, "default");
    assert!(default.contains("credentials from environment"), "{all}");
    assert!(!default.contains(DEFAULT_AUTH_SHOWN), "{all}");
    let work = account_block(&all, "work");
    assert!(work.contains(WORK_AUTH_SHOWN), "{all}");
    assert!(!work.contains("from environment"), "{all}");
    assert_no_secret(&all);

    let work_only = run_ok(&dir, &config, &envs, &["status", "--account", "work"]);
    assert!(work_only.contains(WORK_AUTH_SHOWN), "{work_only}");
    assert!(!work_only.contains("from environment"), "{work_only}");
    assert_no_secret(&work_only);

    // `default` が設定に無くても、環境変数があれば暗黙の `default` として並ぶ。
    // 認証情報の無い `work` へは環境変数を渡さない。
    std::fs::write(&config, "[[accounts]]\nid = \"work\"\n").unwrap();
    let implicit = run_ok(&dir, &config, &envs, &["status"]);
    assert!(
        account_block(&implicit, "default").contains("credentials from environment"),
        "{implicit}"
    );
    let work = account_block(&implicit, "work");
    assert!(work.contains("credentials not set"), "{implicit}");
    assert!(!work.contains("from environment"), "{implicit}");
    assert_no_secret(&implicit);
    // 環境変数が無ければ、認証情報の無い `work` だけが残る。
    let bare = run_ok(&dir, &config, &[], &["status"]);
    assert!(bare.contains("credentials not set"), "{bare}");
    assert!(!bare.contains("Account:"), "{bare}");

    // 片方だけの設定は `default` のエラーになり、他のアカウントには影響しない。
    std::fs::write(&config, two_account_config()).unwrap();
    for (half, message) in [
        (
            ("TWITTER_AUTH_TOKEN", ENV_AUTH),
            "TWITTER_AUTH_TOKEN is set but TWITTER_CSRF_TOKEN is not",
        ),
        (
            ("TWITTER_CSRF_TOKEN", ENV_CT0),
            "TWITTER_CSRF_TOKEN is set but TWITTER_AUTH_TOKEN is not",
        ),
    ] {
        let all = run_ok(&dir, &config, &[half], &["status"]);
        assert!(account_block(&all, "default").contains(message), "{all}");
        let work = account_block(&all, "work");
        assert!(work.contains(WORK_AUTH_SHOWN), "{all}");
        assert!(!work.contains("is set but"), "{all}");
        assert_no_secret(&all);

        let work_only = run_ok(&dir, &config, &[half], &["status", "--account", "work"]);
        assert!(work_only.contains(WORK_AUTH_SHOWN), "{work_only}");
        assert!(!work_only.contains("is set but"), "{work_only}");
    }

    // 環境変数の値は設定ファイルにも他のファイルにも書かれない。
    let fresh = dir.join("fresh.toml");
    run_ok(
        &dir,
        &fresh,
        &envs,
        &[
            "init",
            "--account",
            "work",
            "--auth-token",
            WORK_AUTH,
            "--ct0",
            WORK_CT0,
        ],
    );
    let stored = std::fs::read_to_string(&fresh).unwrap();
    assert!(stored.contains(WORK_AUTH), "{stored}");
    let written = all_file_contents(&dir);
    assert!(
        !written.contains("ENVSECRET"),
        "environment value was written to disk"
    );
}

#[test]
fn account_ids_are_validated_before_anything_is_touched() {
    let dir = temp_dir("ids");
    let config = dir.join("angelic-angel.toml");
    let too_long = "x".repeat(33);

    for bad in [
        "",
        "Work",
        "-work",
        "_work",
        "a/b",
        "a b",
        "a.b",
        too_long.as_str(),
    ] {
        for args in [
            vec!["init", "--auth-token", "a", "--ct0", "b"],
            vec!["register"],
            vec!["unregister"],
            vec!["status"],
        ] {
            // `--account=<値>` なら、`-work` もフラグではなく値として ID 検証に届く。
            let flag = format!("--account={bad}");
            let mut args = args;
            args.push(&flag);
            let out = run_fail(&dir, &config, &[], &args, 2);
            assert!(out.contains("invalid account id"), "{args:?}: {out}");
        }
    }
    // `listen` と `serve` は全アカウントを扱うので `--account` を持たない。
    run_fail(&dir, &config, &[], &["listen", "--account", "work"], 2);
    run_fail(&dir, &config, &[], &["serve", "--account", "work"], 2);
    assert!(!config.exists());

    // 境界の値は受け付ける。
    for good in ["a", "0", "work-2", "a_b", &"x".repeat(32)] {
        let out = run_ok(
            &dir,
            &config,
            &[],
            &["init", "--account", good, "--auth-token", "a", "--ct0", "b"],
        );
        assert!(out.contains("Saved to"), "{good}: {out}");
    }
}

#[test]
fn unregister_reports_a_missing_registration_per_account() {
    let dir = temp_dir("unregister");
    let config = dir.join("config.toml");
    let source = unregistered_config();
    std::fs::write(&config, &source).unwrap();

    let default = run_fail(&dir, &config, &[], &["unregister"], 1);
    assert!(default.contains("No registration found."), "{default}");
    for id in ["work", "absent"] {
        let out = run_fail(&dir, &config, &[], &["unregister", "--account", id], 1);
        assert!(
            out.contains(&format!("No registration found for account \"{id}\"")),
            "{out}"
        );
    }
    assert_no_secret(&default);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), source);
}
