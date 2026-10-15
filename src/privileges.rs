//! root で起動されたとき、既存の設定ファイルの所有者（無ければ設定ディレクトリの所有者）へ
//! 権限を落としてから本体を動かす。
//!
//! distroless にはシェルが無く entrypoint スクリプトを置けない。公式の postgres や redis の
//! イメージが entrypoint で行うのと同じことを、アプリ自身が起動直後に行う。
//! root 以外で起動されたときは何もしない（`user:` 指定や `cargo run` の挙動は変わらない）。

use std::fs::{DirBuilder, Metadata};
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, chown, lchown};
use std::path::{Path, PathBuf};

use crate::error::{AngelicAngelError, Result};

/// root 所有のディレクトリを引き取るときの切り替え先（distroless の nonroot）。
const DEFAULT_ID: u32 = 65532;

/// `sibling` と同じ、設定ファイル名が取れないときの既定名。
const DEFAULT_CONFIG_NAME: &str = "angelic-angel.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
}

/// 切り替え先と、ディレクトリの所有者を変えて引き取るかどうか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Plan {
    target: Identity,
    take_over: bool,
}

/// 切り替え先を、既存の設定ファイルの所有者、無ければディレクトリの所有者の順で決める。
///
/// どちらも root（Docker が bind mount 用に作った場合や空の名前付きボリューム）なら既定の
/// 65532:65532 とする。ディレクトリが root 所有のときだけ、切り替え先へ引き取る。
/// ディレクトリが非 root 所有で設定ファイルの所有者と異なるときは設定ファイルに合わせるので、
/// ディレクトリに書けず `.lock` を作れない場合は従来どおりのエラーになる（引き取らない）。
fn plan(dir_owner: Identity, file_owner: Option<Identity>) -> Plan {
    let target = match file_owner {
        Some(file) if file.uid != 0 => file,
        _ if dir_owner.uid != 0 => dir_owner,
        _ => Identity {
            uid: DEFAULT_ID,
            gid: DEFAULT_ID,
        },
    };
    Plan {
        target,
        take_over: dir_owner.uid == 0,
    }
}

/// 設定ファイルの親ディレクトリ。相対パスで親が空なら `.`。
fn config_dir(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// 設定ファイルと同じ場所に作られる、このアプリが管理するファイルか。
/// 設定本体・`.lock`・履歴 DB（とその journal/wal/shm）・`.legacy.bak`・`.tmp-*` だけが対象。
/// 履歴 DB は `default` が `.notifications.sqlite3`、ほかのアカウントが `.<id>.notifications.sqlite3`。
fn is_managed(config_name: &[u8], entry: &[u8]) -> bool {
    let Some(rest) = entry.strip_prefix(config_name) else {
        return false;
    };
    rest.is_empty()
        || rest == b".lock"
        || rest == b".legacy.bak"
        || is_history_db(rest)
        || rest.starts_with(b".tmp-")
}

/// 設定ファイル名の後ろの部分が、履歴 DB（`-journal` / `-wal` / `-shm` を含む）か。
/// アカウント ID は設定で許される形式のものだけを受け付け、任意の文字列は通さない。
fn is_history_db(rest: &[u8]) -> bool {
    const DB: &[u8] = b".notifications.sqlite3";
    if rest.starts_with(DB) {
        return true;
    }
    let Some(after_dot) = rest.strip_prefix(b".") else {
        return false;
    };
    // アカウント ID は `.` を含められないので、最初の `.` までが ID。
    let Some(end) = after_dot.iter().position(|b| *b == b'.') else {
        return false;
    };
    let (id, db) = after_dot.split_at(end);
    db.starts_with(DB)
        && std::str::from_utf8(id).is_ok_and(|id| crate::config::validate_account_id(id).is_ok())
}

/// 既存の設定ファイルの所有者。無い場合、通常ファイルでない場合（symlink は辿らない）は `None`。
fn config_file_owner(config_path: &Path) -> Result<Option<Identity>> {
    match std::fs::symlink_metadata(config_path) {
        Ok(meta) if meta.is_file() => Ok(Some(Identity {
            uid: meta.uid(),
            gid: meta.gid(),
        })),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error("stat", config_path, e)),
    }
}

fn io_error(action: &str, path: &Path, e: std::io::Error) -> AngelicAngelError {
    AngelicAngelError::Config(format!("cannot {action} {}: {e}", path.display()))
}

fn is_root() -> bool {
    // SAFETY: geteuid は引数も副作用も持たず、常に成功する。
    unsafe { libc::geteuid() == 0 }
}

/// root で起動されたときだけ、既存の設定ファイルの所有者（無ければ設定ディレクトリの所有者）
/// へ権限を落とす。
///
/// 切り替えた場合はその ID を返す。root でなければ何もせず `None`。
/// 設定ファイルやロックを開く前に呼ぶこと。root のまま続行することはなく、
/// 想定外の状態はすべてエラーにする。
pub fn drop_to_config_owner(config_path: &Path) -> Result<Option<Identity>> {
    if !is_root() {
        return Ok(None);
    }

    let dir = config_dir(config_path);
    let meta = ensure_dir(&dir)?;
    let plan = plan(
        Identity {
            uid: meta.uid(),
            gid: meta.gid(),
        },
        config_file_owner(config_path)?,
    );
    if plan.take_over {
        take_over(&dir, config_path, plan.target)?;
    }
    switch_to(plan.target)?;

    eprintln!(
        "running as {}:{} owning {}",
        plan.target.uid,
        plan.target.gid,
        dir.display()
    );
    Ok(Some(plan.target))
}

/// ディレクトリの状態を返す。無ければ 0700 で作る。
fn ensure_dir(dir: &Path) -> Result<Metadata> {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => return Ok(meta),
        Ok(_) => {
            return Err(AngelicAngelError::Config(format!(
                "{} is not a directory",
                dir.display()
            )));
        },
        Err(e) if e.kind() == ErrorKind::NotFound => {},
        Err(e) => return Err(io_error("stat", dir, e)),
    }

    // 親は既定の権限で用意し、切り替え後の所有者がたどれるようにする。
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| io_error("create", parent, e))?;
    }
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {},
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {},
        Err(e) => return Err(io_error("create", dir, e)),
    }
    std::fs::metadata(dir).map_err(|e| io_error("stat", dir, e))
}

/// root 所有のディレクトリと、その中の既知のファイルのうち root 所有のものを `target` の所有にする。
fn take_over(dir: &Path, config_path: &Path, target: Identity) -> Result<()> {
    // 先にファイルを渡す。ディレクトリを渡した後は root が読めるとは限らない（ケーパビリティ次第）。
    let name = config_path
        .file_name()
        .unwrap_or_else(|| DEFAULT_CONFIG_NAME.as_ref());
    chown_managed_files(dir, name.as_bytes(), 0, target)?;

    chown(dir, Some(target.uid), Some(target.gid)).map_err(|e| io_error("chown", dir, e))?;
    // chown を黙って無視するファイルシステム（NAS の共有など）があるので、所有者を確かめる。
    let meta = std::fs::metadata(dir).map_err(|e| io_error("stat", dir, e))?;
    if meta.uid() != target.uid || meta.gid() != target.gid {
        return Err(AngelicAngelError::Config(format!(
            "{} is still owned by {}:{} after chown to {}:{}; use a directory owned by a non-root user",
            dir.display(),
            meta.uid(),
            meta.gid(),
            target.uid,
            target.gid
        )));
    }
    Ok(())
}

/// `dir` 直下の管理ファイルのうち `from_uid` 所有のものを `to` の所有にし、その数を返す。
/// 再帰せず、シンボリックリンクやファイル以外は触らない。
fn chown_managed_files(
    dir: &Path,
    config_name: &[u8],
    from_uid: u32,
    to: Identity,
) -> Result<usize> {
    let mut changed = 0;
    for entry in std::fs::read_dir(dir).map_err(|e| io_error("read", dir, e))? {
        let entry = entry.map_err(|e| io_error("read", dir, e))?;
        if !is_managed(config_name, entry.file_name().as_bytes()) {
            continue;
        }
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).map_err(|e| io_error("stat", &path, e))?;
        if !meta.is_file() || meta.uid() != from_uid {
            continue;
        }
        // lchown はリンク先ではなくパス自身を変える。
        lchown(&path, Some(to.uid), Some(to.gid)).map_err(|e| io_error("chown", &path, e))?;
        changed += 1;
    }
    Ok(changed)
}

fn syscall(name: &str, result: libc::c_int) -> Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(AngelicAngelError::Config(format!(
            "cannot drop root privileges: {name} failed: {}",
            std::io::Error::last_os_error()
        )))
    }
}

/// supplementary groups を捨て、gid、uid の順に切り替える。すべて検査し、root に戻れないことまで確かめる。
fn switch_to(target: Identity) -> Result<()> {
    // SAFETY: 引数は空の配列（長さ 0・NULL）で、メモリを読み書きしない。
    syscall("setgroups", unsafe { libc::setgroups(0, std::ptr::null()) })?;
    // SAFETY: 値渡しの gid だけを取り、メモリに触れない。
    syscall("setgid", unsafe { libc::setgid(target.gid) })?;
    // SAFETY: 値渡しの uid だけを取り、メモリに触れない。
    syscall("setuid", unsafe { libc::setuid(target.uid) })?;

    // SAFETY: いずれも引数なしで現在の ID を返すだけで、常に成功する。
    let (uid, euid, gid, egid) = unsafe {
        (
            libc::getuid(),
            libc::geteuid(),
            libc::getgid(),
            libc::getegid(),
        )
    };
    if uid != target.uid || euid != target.uid || gid != target.gid || egid != target.gid {
        return Err(AngelicAngelError::Config(format!(
            "cannot drop root privileges: expected {}:{} but running as uid={uid} euid={euid} gid={gid} egid={egid}",
            target.uid, target.gid
        )));
    }
    // SAFETY: 値渡しの uid だけを取り、メモリに触れない。root に戻れてはならないので失敗が正常。
    if unsafe { libc::setuid(0) } == 0 {
        return Err(AngelicAngelError::Config(
            "cannot drop root privileges: setuid(0) still succeeds".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "aa-privileges-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn id(uid: u32, gid: u32) -> Identity {
        Identity { uid, gid }
    }

    fn planned(target: Identity, take_over: bool) -> Plan {
        Plan { target, take_over }
    }

    #[test]
    fn root_owned_directory_without_a_config_is_taken_over_as_the_default_user() {
        let expected = planned(id(65532, 65532), true);
        assert_eq!(plan(id(0, 0), None), expected);
        // グループだけ root 以外でも、所有者が root なら引き取る。
        assert_eq!(plan(id(0, 100), None), expected);
    }

    #[test]
    fn root_owned_config_in_a_root_owned_directory_is_taken_over_as_the_default_user() {
        assert_eq!(
            plan(id(0, 0), Some(id(0, 0))),
            planned(id(65532, 65532), true)
        );
        // 設定ファイルの gid だけ root 以外でも、所有者が root なら対象にしない。
        assert_eq!(
            plan(id(0, 0), Some(id(0, 100))),
            planned(id(65532, 65532), true)
        );
    }

    #[test]
    fn existing_config_owner_wins_over_a_root_owned_directory() {
        // 以前 `user: 1000:1000` で動かしたまま、root 所有になったディレクトリ。
        assert_eq!(
            plan(id(0, 0), Some(id(1000, 10))),
            planned(id(1000, 10), true)
        );
    }

    #[test]
    fn foreign_directory_owner_is_followed_without_touching_the_directory() {
        assert_eq!(
            plan(id(12345, 54321), None),
            planned(id(12345, 54321), false)
        );
        assert_eq!(
            plan(id(65532, 65532), None),
            planned(id(65532, 65532), false)
        );
        assert_eq!(
            plan(id(12345, 54321), Some(id(0, 0))),
            planned(id(12345, 54321), false)
        );
    }

    #[test]
    fn config_owner_differing_from_a_non_root_directory_owner_is_followed_without_take_over() {
        assert_eq!(
            plan(id(12345, 12345), Some(id(1000, 10))),
            planned(id(1000, 10), false)
        );
    }

    #[test]
    fn config_file_owner_ignores_missing_files_links_and_directories() {
        let base = temp_dir("owner");
        assert_eq!(config_file_owner(&base.join("missing.toml")).unwrap(), None);

        let file = base.join("c.toml");
        std::fs::write(&file, "x").unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(
            config_file_owner(&file).unwrap(),
            Some(id(meta.uid(), meta.gid()))
        );

        std::os::unix::fs::symlink(&file, base.join("link.toml")).unwrap();
        assert_eq!(config_file_owner(&base.join("link.toml")).unwrap(), None);

        std::fs::create_dir(base.join("dir.toml")).unwrap();
        assert_eq!(config_file_owner(&base.join("dir.toml")).unwrap(), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn config_dir_is_the_parent_or_the_current_directory() {
        assert_eq!(
            config_dir(Path::new("/config/angelic-angel.toml")),
            PathBuf::from("/config")
        );
        assert_eq!(config_dir(Path::new("a/b.toml")), PathBuf::from("a"));
        assert_eq!(
            config_dir(Path::new("angelic-angel.toml")),
            PathBuf::from(".")
        );
        assert_eq!(
            config_dir(Path::new("./angelic-angel.toml")),
            PathBuf::from(".")
        );
    }

    #[test]
    fn only_known_files_next_to_the_config_are_managed() {
        let name = b"angelic-angel.toml";
        for managed in [
            "angelic-angel.toml",
            "angelic-angel.toml.lock",
            "angelic-angel.toml.notifications.sqlite3",
            "angelic-angel.toml.notifications.sqlite3-journal",
            "angelic-angel.toml.notifications.sqlite3-wal",
            "angelic-angel.toml.notifications.sqlite3-shm",
            "angelic-angel.toml.work.notifications.sqlite3",
            "angelic-angel.toml.work.notifications.sqlite3-journal",
            "angelic-angel.toml.work.notifications.sqlite3-wal",
            "angelic-angel.toml.work.notifications.sqlite3-shm",
            "angelic-angel.toml.a-1_b.notifications.sqlite3",
            "angelic-angel.toml.legacy.bak",
            "angelic-angel.toml.tmp-42",
            "angelic-angel.toml.tmp-probe-42",
        ] {
            assert!(is_managed(name, managed.as_bytes()), "{managed}");
        }
        for other in [
            "other.toml",
            "other.toml.lock",
            "angelic-angel.toml.token",
            "angelic-angel.toml.notifications.json",
            "angelic-angel.toml.legacy.bak.old",
            "angelic-angel.toml.legacy",
            "angelic-angel.toml.Work.notifications.sqlite3",
            "angelic-angel.toml..notifications.sqlite3",
            "angelic-angel.toml.-work.notifications.sqlite3",
            "angelic-angel.toml.a.b.notifications.sqlite3",
            "angelic-angel.toml.a b.notifications.sqlite3",
            "angelic-angel.toml.work.notifications.json",
            "angelic-angel.toml.work.notificationsXsqlite3",
            "angelic-angel.toml.work",
            "other.toml.work.notifications.sqlite3",
            "angelic-angel.tomlx",
            "old-angelic-angel.toml",
            "angelic-angel",
        ] {
            assert!(!is_managed(name, other.as_bytes()), "{other}");
        }
    }

    #[test]
    fn missing_directory_is_created_private_with_its_parents() {
        let base = temp_dir("create");
        let dir = base.join("outer/config");
        let meta = ensure_dir(&dir).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        assert!(base.join("outer").is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn existing_directory_is_left_as_is() {
        let base = temp_dir("existing");
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        let meta = ensure_dir(&base).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o755);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_file_in_place_of_the_directory_is_an_error() {
        let base = temp_dir("not-a-dir");
        let file = base.join("config");
        std::fs::write(&file, "").unwrap();
        let err = ensure_dir(&file).unwrap_err().to_string();
        assert!(err.contains("is not a directory"), "{err}");
        // 親が通常ファイルのときも作成に失敗する。
        assert!(ensure_dir(&file.join("nested")).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn managed_files_are_counted_without_following_links_or_recursing() {
        let base = temp_dir("chown");
        let name = "angelic-angel.toml";
        for file in [
            name.to_string(),
            format!("{name}.lock"),
            format!("{name}.notifications.sqlite3"),
            format!("{name}.notifications.sqlite3-journal"),
            format!("{name}.work.notifications.sqlite3"),
            format!("{name}.work.notifications.sqlite3-wal"),
            format!("{name}.legacy.bak"),
            format!("{name}.tmp-1"),
            "other.toml".to_string(),
        ] {
            std::fs::write(base.join(file), "x").unwrap();
        }
        std::os::unix::fs::symlink(
            base.join("other.toml"),
            base.join(format!("{name}.tmp-link")),
        )
        .unwrap();
        std::fs::create_dir(base.join(format!("{name}.tmp-dir"))).unwrap();
        std::fs::write(
            base.join(format!("{name}.tmp-dir"))
                .join(format!("{name}.lock")),
            "x",
        )
        .unwrap();

        // root 以外でも、自分の ID への lchown は通る。
        let own = std::fs::metadata(base.join(name)).unwrap();
        let to = id(own.uid(), own.gid());
        assert_eq!(
            chown_managed_files(&base, name.as_bytes(), own.uid(), to).unwrap(),
            8
        );
        // 別の UID 所有のファイルだけを対象にする。
        assert_eq!(
            chown_managed_files(&base, name.as_bytes(), own.uid() + 1, to).unwrap(),
            0
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn nothing_happens_unless_running_as_root() {
        if is_root() {
            // root で実行すると、テストプロセス自身の権限が落ちてしまう。
            return;
        }
        let base = temp_dir("not-root");
        let config = base.join("missing/angelic-angel.toml");
        assert_eq!(drop_to_config_owner(&config).unwrap(), None);
        assert!(!base.join("missing").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
