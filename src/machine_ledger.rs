//! Private, durable at-most-once dispatch ledger. Tombstones are retained indefinitely.
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("operation storage unavailable")]
    Storage,
    #[error("idempotency key has different content")]
    Conflict,
}

impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self { Self::Storage }
}

#[derive(Debug, Clone, Serialize)]
pub struct Operation {
    pub operation_id: String,
    pub state: String,
    pub post_id: Option<String>,
    pub error: Option<String>,
}

pub struct Ledger(Mutex<Connection>);

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(_) => {},
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(_) => return Err(Error::Storage),
        }
        let metadata = std::fs::symlink_metadata(path).map_err(|_| Error::Storage)?;
        if !metadata.file_type().is_file() { return Err(Error::Storage); }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 { return Err(Error::Storage); }
        }
        Self::initialize(Connection::open(path)?)
    }

    fn initialize(connection: Connection) -> Result<Self, Error> {
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS credentials (
                account TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, revision TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS operations (
                operation_id TEXT PRIMARY KEY, account TEXT NOT NULL, client TEXT NOT NULL,
                key TEXT NOT NULL, request_hash TEXT NOT NULL, revision TEXT NOT NULL,
                state TEXT NOT NULL, post_id TEXT, error TEXT,
                UNIQUE(account, client, key));
            UPDATE operations SET state='unknown', error='process_interrupted'
                WHERE state IN ('accepted', 'dispatching');")?;
        Ok(Self(Mutex::new(connection)))
    }

    #[cfg(test)]
    pub fn read_only(&self) {
        self.0.lock().unwrap().execute_batch("PRAGMA query_only=ON").unwrap();
    }

    pub fn revision(&self, account: &str, fingerprint: &str) -> Result<String, Error> {
        let connection = self.0.lock().map_err(|_| Error::Storage)?;
        let old: Option<(String, String)> = connection.query_row(
            "SELECT fingerprint, revision FROM credentials WHERE account=?1", [account],
            |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        if let Some((old_fingerprint, revision)) = old && old_fingerprint == fingerprint {
            return Ok(revision);
        }
        let revision = Uuid::new_v4().to_string();
        connection.execute("INSERT INTO credentials(account,fingerprint,revision) VALUES(?1,?2,?3)
            ON CONFLICT(account) DO UPDATE SET fingerprint=excluded.fingerprint, revision=excluded.revision",
            params![account, fingerprint, revision])?;
        Ok(revision)
    }

    pub fn existing(&self, account: &str, client: &str, key: &str, hash: &str)
        -> Result<Option<Operation>, Error> {
        let connection = self.0.lock().map_err(|_| Error::Storage)?;
        Self::find(&connection, account, client, key, hash)
    }

    fn find(connection: &Connection, account: &str, client: &str, key: &str, hash: &str)
        -> Result<Option<Operation>, Error> {
        let found: Option<(String, Operation)> = connection.query_row(
            "SELECT request_hash, operation_id, state, post_id, error FROM operations
                WHERE account=?1 AND client=?2 AND key=?3", params![account, client, key],
            |row| Ok((row.get(0)?, Operation { operation_id: row.get(1)?, state: row.get(2)?,
                post_id: row.get(3)?, error: row.get(4)? }))).optional()?;
        match found {
            Some((saved_hash, _)) if saved_hash != hash => Err(Error::Conflict),
            Some((_, operation)) => Ok(Some(operation)),
            None => Ok(None),
        }
    }

    /// Returns `(operation, fresh)`. Only the winner of the unique insert may dispatch.
    pub fn reserve(&self, account: &str, client: &str, key: &str, hash: &str, revision: &str)
        -> Result<(Operation, bool), Error> {
        let connection = self.0.lock().map_err(|_| Error::Storage)?;
        if let Some(operation) = Self::find(&connection, account, client, key, hash)? {
            return Ok((operation, false));
        }
        let operation = Operation { operation_id: Uuid::new_v4().to_string(),
            state: "accepted".into(), post_id: None, error: None };
        connection.execute("INSERT INTO operations
            (operation_id,account,client,key,request_hash,revision,state) VALUES(?1,?2,?3,?4,?5,?6,'accepted')",
            params![operation.operation_id, account, client, key, hash, revision])?;
        Ok((operation, true))
    }

    pub fn get(&self, account: &str, client: &str, id: &str) -> Result<Option<Operation>, Error> {
        let connection = self.0.lock().map_err(|_| Error::Storage)?;
        Ok(connection.query_row("SELECT operation_id,state,post_id,error FROM operations
            WHERE account=?1 AND client=?2 AND operation_id=?3", params![account, client, id],
            |row| Ok(Operation { operation_id: row.get(0)?, state: row.get(1)?,
                post_id: row.get(2)?, error: row.get(3)? })).optional()?)
    }

    pub fn transition(&self, id: &str, from: &str, state: &str, post: Option<&str>, error: Option<&str>)
        -> Result<(), Error> {
        let connection = self.0.lock().map_err(|_| Error::Storage)?;
        if connection.execute("UPDATE operations SET state=?1,post_id=?2,error=?3
            WHERE operation_id=?4 AND state=?5", params![state, post, error, id, from])? != 1 {
            return Err(Error::Storage);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Ledger { Ledger::initialize(Connection::open_in_memory().unwrap()).unwrap() }

    #[test]
    fn same_key_never_dispatches_twice_and_content_must_match() {
        let ledger = memory();
        let (first, fresh) = ledger.reserve("a", "bot", "key", "hash", "rev").unwrap();
        assert!(fresh);
        let (second, fresh) = ledger.reserve("a", "bot", "key", "hash", "rev").unwrap();
        assert!(!fresh);
        assert_eq!(first.operation_id, second.operation_id);
        assert!(matches!(ledger.reserve("a", "bot", "key", "different", "rev"), Err(Error::Conflict)));
        assert!(ledger.reserve("b", "bot", "key", "hash", "rev").unwrap().1);
        assert!(ledger.reserve("a", "other", "key", "hash", "rev").unwrap().1);
        assert!(ledger.get("b", "bot", &first.operation_id).unwrap().is_none());
        assert!(ledger.get("a", "other", &first.operation_id).unwrap().is_none());
    }

    #[test]
    fn concurrent_reservation_has_one_winner() {
        let ledger = std::sync::Arc::new(memory());
        let workers: Vec<_> = (0..16).map(|_| {
            let ledger = ledger.clone();
            std::thread::spawn(move || ledger.reserve("a", "b", "k", "h", "r").unwrap().1)
        }).collect();
        assert_eq!(workers.into_iter().filter_map(|w| w.join().ok()).filter(|fresh| *fresh).count(), 1);
    }

    #[test]
    fn revision_survives_reopen_and_changes_for_changed_credentials() {
        let directory = crate::config::test_temp_dir();
        let path = directory.join("ledger.sqlite3");
        let ledger = Ledger::open(&path).unwrap();
        let first = ledger.revision("a", "fingerprint-one").unwrap();
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap();
        assert_eq!(first, ledger.revision("a", "fingerprint-one").unwrap());
        assert_ne!(first, ledger.revision("a", "fingerprint-two").unwrap());
    }

    #[test]
    fn crash_and_failed_success_storage_remain_non_replayable() {
        let directory = crate::config::test_temp_dir();
        let path = directory.join("ledger.sqlite3");
        let ledger = Ledger::open(&path).unwrap();
        let (operation, _) = ledger.reserve("a", "b", "k", "h", "r").unwrap();
        let (accepted, _) = ledger.reserve("a", "b", "accepted", "h", "r").unwrap();
        ledger.transition(&operation.operation_id, "accepted", "dispatching", None, None).unwrap();
        ledger.0.lock().unwrap().execute_batch("PRAGMA query_only=ON").unwrap();
        assert!(ledger.transition(&operation.operation_id, "dispatching", "succeeded", Some("123"), None).is_err());
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap();
        let (saved, fresh) = ledger.reserve("a", "b", "k", "h", "r").unwrap();
        assert!(!fresh);
        assert_eq!(saved.state, "unknown");
        assert_eq!(ledger.get("a", "b", &accepted.operation_id).unwrap().unwrap().state, "unknown");
    }

    #[test]
    fn succeeded_results_survive_restart_without_redispatch() {
        let directory = crate::config::test_temp_dir();
        let path = directory.join("success.sqlite3");
        let ledger = Ledger::open(&path).unwrap();
        let (operation, _) = ledger.reserve("a", "b", "k", "h", "r").unwrap();
        ledger.transition(&operation.operation_id, "accepted", "dispatching", None, None).unwrap();
        ledger.transition(&operation.operation_id, "dispatching", "succeeded", Some("123"), None).unwrap();
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap();
        let (saved, fresh) = ledger.reserve("a", "b", "k", "h", "r").unwrap();
        assert!(!fresh);
        assert_eq!(saved.state, "succeeded");
        assert_eq!(saved.post_id.as_deref(), Some("123"));
    }

    #[test]
    fn unavailable_storage_cannot_reserve_dispatch() {
        let ledger = memory();
        ledger.0.lock().unwrap().execute_batch("PRAGMA query_only=ON").unwrap();
        assert!(ledger.reserve("a", "b", "k", "h", "r").is_err());
        assert!(Ledger::open(Path::new("/nonexistent-machine-fixture/operations.sqlite3")).is_err());
    }
}
