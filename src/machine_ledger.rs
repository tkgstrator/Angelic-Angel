//! Private, durable at-most-once dispatch ledger. Tombstones are retained indefinitely.
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
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
    fn from(_: rusqlite::Error) -> Self {
        Self::Storage
    }
}

/// `kind` is `post`, `like` or `follow`. Posts keep `post_id`; likes and follows store their
/// result object in `result` (absent for posts and while no result exists).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Operation {
    pub operation_id: String,
    pub kind: String,
    pub state: String,
    pub post_id: Option<String>,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
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
        if !metadata.file_type().is_file() {
            return Err(Error::Storage);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(Error::Storage);
            }
        }
        Self::initialize(Connection::open(path)?)
    }

    fn initialize(connection: Connection) -> Result<Self, Error> {
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS credentials (
                account TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, revision TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS operations (
                operation_id TEXT PRIMARY KEY, account TEXT NOT NULL, client TEXT NOT NULL,
                key TEXT NOT NULL, request_hash TEXT NOT NULL, revision TEXT NOT NULL,
                state TEXT NOT NULL, post_id TEXT, error TEXT,
                UNIQUE(account, client, key));
            UPDATE operations SET state='unknown', error='process_interrupted'
                WHERE state IN ('accepted', 'dispatching');",
        )?;
        // Additive migration only: databases written by earlier versions keep every row, and rows
        // without a kind are posts.
        Self::add_column(&connection, "kind", "TEXT NOT NULL DEFAULT 'post'")?;
        Self::add_column(&connection, "result", "TEXT")?;
        Ok(Self(Mutex::new(connection)))
    }

    fn add_column(connection: &Connection, name: &str, definition: &str) -> Result<(), Error> {
        let present: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('operations') WHERE name=?1",
            [name],
            |row| row.get(0),
        )?;
        if present == 0 {
            let sql = format!("ALTER TABLE operations ADD COLUMN {name} {definition}");
            connection.execute_batch(&sql)?;
        }
        Ok(())
    }

    fn read(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<Operation> {
        let text: Option<String> = row.get(first + 5)?;
        let result = text
            .map(|text| serde_json::from_str(&text))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    first + 5,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
        Ok(Operation {
            operation_id: row.get(first)?,
            kind: row.get(first + 1)?,
            state: row.get(first + 2)?,
            post_id: row.get(first + 3)?,
            error: row.get(first + 4)?,
            result,
        })
    }

    #[cfg(test)]
    pub fn read_only(&self) {
        self.0
            .lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
    }

    pub fn revision(&self, account: &str, fingerprint: &str) -> Result<String, Error> {
        let connection = self.0.lock().map_err(|_| Error::Storage)?;
        let old: Option<(String, String)> = connection
            .query_row(
                "SELECT fingerprint, revision FROM credentials WHERE account=?1",
                [account],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((old_fingerprint, revision)) = old
            && old_fingerprint == fingerprint
        {
            return Ok(revision);
        }
        let revision = Uuid::new_v4().to_string();
        connection.execute("INSERT INTO credentials(account,fingerprint,revision) VALUES(?1,?2,?3)
            ON CONFLICT(account) DO UPDATE SET fingerprint=excluded.fingerprint, revision=excluded.revision",
            params![account, fingerprint, revision])?;
        Ok(revision)
    }

    pub fn existing(
        &self,
        account: &str,
        client: &str,
        key: &str,
        hash: &str,
    ) -> Result<Option<Operation>, Error> {
        let connection = self.0.lock().map_err(|_| Error::Storage)?;
        Self::find(&connection, account, client, key, hash)
    }

    fn find(
        connection: &Connection,
        account: &str,
        client: &str,
        key: &str,
        hash: &str,
    ) -> Result<Option<Operation>, Error> {
        let found: Option<(String, Operation)> = connection
            .query_row(
                "SELECT request_hash, operation_id, kind, state, post_id, error, result
                FROM operations WHERE account=?1 AND client=?2 AND key=?3",
                params![account, client, key],
                |row| Ok((row.get(0)?, Self::read(row, 1)?)),
            )
            .optional()?;
        match found {
            Some((saved_hash, _)) if saved_hash != hash => Err(Error::Conflict),
            Some((_, operation)) => Ok(Some(operation)),
            None => Ok(None),
        }
    }

    /// Returns `(operation, fresh)`. Only the winner of the unique insert may dispatch.
    pub fn reserve(
        &self,
        account: &str,
        client: &str,
        key: &str,
        hash: &str,
        revision: &str,
    ) -> Result<(Operation, bool), Error> {
        self.reserve_as("post", account, client, key, hash, revision)
    }

    /// Like [`Ledger::reserve`] for an explicit operation kind. The caller includes the kind in
    /// the content hash, so the same key with another kind or target is an idempotency conflict.
    pub fn reserve_as(
        &self,
        kind: &str,
        account: &str,
        client: &str,
        key: &str,
        hash: &str,
        revision: &str,
    ) -> Result<(Operation, bool), Error> {
        let connection = self.0.lock().map_err(|_| Error::Storage)?;
        if let Some(operation) = Self::find(&connection, account, client, key, hash)? {
            return Ok((operation, false));
        }
        let operation = Operation {
            operation_id: Uuid::new_v4().to_string(),
            kind: kind.into(),
            state: "accepted".into(),
            post_id: None,
            error: None,
            result: None,
        };
        let id = &operation.operation_id;
        connection.execute(
            "INSERT INTO operations
            (operation_id,account,client,key,request_hash,revision,state,kind)
            VALUES(?1,?2,?3,?4,?5,?6,'accepted',?7)",
            params![id, account, client, key, hash, revision, kind],
        )?;
        Ok((operation, true))
    }

    pub fn get(&self, account: &str, client: &str, id: &str) -> Result<Option<Operation>, Error> {
        let connection = self.0.lock().map_err(|_| Error::Storage)?;
        Ok(connection
            .query_row(
                "SELECT operation_id,kind,state,post_id,error,result FROM operations
            WHERE account=?1 AND client=?2 AND operation_id=?3",
                params![account, client, id],
                |row| Self::read(row, 0),
            )
            .optional()?)
    }

    pub fn transition(
        &self,
        id: &str,
        from: &str,
        state: &str,
        post: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), Error> {
        let connection = self.0.lock().map_err(|_| Error::Storage)?;
        if connection.execute(
            "UPDATE operations SET state=?1,post_id=?2,error=?3
            WHERE operation_id=?4 AND state=?5",
            params![state, post, error, id, from],
        )? != 1
        {
            return Err(Error::Storage);
        }
        Ok(())
    }

    /// Like [`Ledger::transition`], recording a like/follow result object instead of a post ID.
    pub fn transition_result(
        &self,
        id: &str,
        from: &str,
        state: &str,
        result: Option<&Value>,
        error: Option<&str>,
    ) -> Result<(), Error> {
        let text = result
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| Error::Storage)?;
        let connection = self.0.lock().map_err(|_| Error::Storage)?;
        if connection.execute(
            "UPDATE operations SET state=?1,result=?2,error=?3
            WHERE operation_id=?4 AND state=?5",
            params![state, text, error, id, from],
        )? != 1
        {
            return Err(Error::Storage);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Ledger {
        Ledger::initialize(Connection::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn same_key_never_dispatches_twice_and_content_must_match() {
        let ledger = memory();
        let (first, fresh) = ledger.reserve("a", "bot", "key", "hash", "rev").unwrap();
        assert!(fresh);
        let (second, fresh) = ledger.reserve("a", "bot", "key", "hash", "rev").unwrap();
        assert!(!fresh);
        assert_eq!(first.operation_id, second.operation_id);
        assert!(matches!(
            ledger.reserve("a", "bot", "key", "different", "rev"),
            Err(Error::Conflict)
        ));
        assert!(ledger.reserve("b", "bot", "key", "hash", "rev").unwrap().1);
        assert!(
            ledger
                .reserve("a", "other", "key", "hash", "rev")
                .unwrap()
                .1
        );
        assert!(
            ledger
                .get("b", "bot", &first.operation_id)
                .unwrap()
                .is_none()
        );
        assert!(
            ledger
                .get("a", "other", &first.operation_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn concurrent_reservation_has_one_winner() {
        let ledger = std::sync::Arc::new(memory());
        let workers: Vec<_> = (0..16)
            .map(|_| {
                let ledger = ledger.clone();
                std::thread::spawn(move || ledger.reserve("a", "b", "k", "h", "r").unwrap().1)
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .filter_map(|w| w.join().ok())
                .filter(|fresh| *fresh)
                .count(),
            1
        );
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
        ledger
            .transition(
                &operation.operation_id,
                "accepted",
                "dispatching",
                None,
                None,
            )
            .unwrap();
        ledger
            .0
            .lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
        assert!(
            ledger
                .transition(
                    &operation.operation_id,
                    "dispatching",
                    "succeeded",
                    Some("123"),
                    None
                )
                .is_err()
        );
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap();
        let (saved, fresh) = ledger.reserve("a", "b", "k", "h", "r").unwrap();
        assert!(!fresh);
        assert_eq!(saved.state, "unknown");
        assert_eq!(
            ledger
                .get("a", "b", &accepted.operation_id)
                .unwrap()
                .unwrap()
                .state,
            "unknown"
        );
    }

    #[test]
    fn succeeded_results_survive_restart_without_redispatch() {
        let directory = crate::config::test_temp_dir();
        let path = directory.join("success.sqlite3");
        let ledger = Ledger::open(&path).unwrap();
        let (operation, _) = ledger.reserve("a", "b", "k", "h", "r").unwrap();
        ledger
            .transition(
                &operation.operation_id,
                "accepted",
                "dispatching",
                None,
                None,
            )
            .unwrap();
        ledger
            .transition(
                &operation.operation_id,
                "dispatching",
                "succeeded",
                Some("123"),
                None,
            )
            .unwrap();
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
        ledger
            .0
            .lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
        assert!(ledger.reserve("a", "b", "k", "h", "r").is_err());
        assert!(
            Ledger::open(Path::new("/nonexistent-machine-fixture/operations.sqlite3")).is_err()
        );
    }

    #[test]
    fn kinds_and_results_are_stored_replayed_and_conflict_across_kinds() {
        let ledger = memory();
        let (post, _) = ledger.reserve("a", "b", "post-key", "h1", "r").unwrap();
        assert_eq!(post.kind, "post");
        let (like, fresh) = ledger
            .reserve_as("like", "a", "b", "like-key", "h2", "r")
            .unwrap();
        assert!(fresh);
        assert_eq!(like.kind, "like");
        assert!(like.result.is_none());
        ledger
            .transition(&like.operation_id, "accepted", "dispatching", None, None)
            .unwrap();
        let result = serde_json::json!({"postId": "1", "alreadyLiked": true});
        ledger
            .transition_result(
                &like.operation_id,
                "dispatching",
                "succeeded",
                Some(&result),
                None,
            )
            .unwrap();
        let saved = ledger.get("a", "b", &like.operation_id).unwrap().unwrap();
        assert_eq!(saved.kind, "like");
        assert_eq!(saved.state, "succeeded");
        assert_eq!(saved.post_id, None);
        assert_eq!(saved.result, Some(result));
        let (replayed, fresh) = ledger
            .reserve_as("like", "a", "b", "like-key", "h2", "r")
            .unwrap();
        assert!(!fresh);
        assert_eq!(replayed, saved);
        assert!(matches!(
            ledger.reserve_as("follow", "a", "b", "like-key", "h3", "r"),
            Err(Error::Conflict)
        ));
        let value = serde_json::to_value(&saved).unwrap();
        assert_eq!(value["kind"], "like");
        assert_eq!(value["result"]["alreadyLiked"], true);
        let value = serde_json::to_value(&post).unwrap();
        assert_eq!(value["kind"], "post");
        assert!(value.get("result").is_none());
        // A transition only applies from the expected state, and never twice.
        assert!(
            ledger
                .transition_result(&like.operation_id, "dispatching", "failed", None, None)
                .is_err()
        );
    }

    #[test]
    fn legacy_databases_gain_kind_columns_without_losing_rows() {
        let directory = crate::config::test_temp_dir();
        let path = directory.join("legacy.sqlite3");
        drop(Ledger::open(&path).unwrap());
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TABLE operations;
                CREATE TABLE operations (
                    operation_id TEXT PRIMARY KEY, account TEXT NOT NULL, client TEXT NOT NULL,
                    key TEXT NOT NULL, request_hash TEXT NOT NULL, revision TEXT NOT NULL,
                    state TEXT NOT NULL, post_id TEXT, error TEXT,
                    UNIQUE(account, client, key));
                INSERT INTO operations
                    VALUES('old-id','a','b','old-key','old-hash','rev','succeeded','99',NULL);",
            )
            .unwrap();
        drop(connection);
        for _ in 0..2 {
            let ledger = Ledger::open(&path).unwrap();
            let saved = ledger.get("a", "b", "old-id").unwrap().unwrap();
            assert_eq!(saved.kind, "post");
            assert_eq!(saved.state, "succeeded");
            assert_eq!(saved.post_id.as_deref(), Some("99"));
            assert!(saved.result.is_none());
            let (replayed, fresh) = ledger
                .reserve("a", "b", "old-key", "old-hash", "rev")
                .unwrap();
            assert!(!fresh);
            assert_eq!(replayed.operation_id, "old-id");
            assert!(matches!(
                ledger.reserve("a", "b", "old-key", "different", "rev"),
                Err(Error::Conflict)
            ));
            let (like, _) = ledger
                .reserve_as("like", "a", "b", "new-key", "new-hash", "rev")
                .unwrap();
            assert_eq!(like.kind, "like");
        }
    }
}
