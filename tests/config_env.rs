//! `ANGELIC_ANGEL_CONFIG` と `--config` の優先順位を、実際のバイナリで確かめる。
//! 環境変数は子プロセスにだけ渡し、テストプロセス自身の環境は変更しない。

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("aa-cli-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_config(path: &Path) {
    std::fs::write(path, "[twitter]\nauth_token = \"a\"\nct0 = \"b\"\n").unwrap();
}

/// `status` を実行し、stderr を返す。
fn status(cwd: &Path, config_env: Option<&Path>, config_arg: Option<&Path>) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_angelic-angel"));
    command
        .current_dir(cwd)
        .env_remove("ANGELIC_ANGEL_CONFIG")
        .env_remove("TWITTER_AUTH_TOKEN")
        .env_remove("TWITTER_CSRF_TOKEN")
        .env("NO_COLOR", "1");
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

    let _ = std::fs::remove_dir_all(&dir);
}
