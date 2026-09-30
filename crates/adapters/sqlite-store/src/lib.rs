use anyhow::{Context, Result, bail};
use fabushi_chatgpt_domain::{RunId, TaskId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionRecord {
    pub task_id: TaskId,
    pub run_id: RunId,
    pub expected_revision: i64,
    pub next_revision: i64,
    pub event_kind: String,
    pub event_payload_json: String,
    pub materialized_state_json: String,
    pub effect_kind: String,
    pub effect_payload_json: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEffect {
    pub id: i64,
    pub task_id: String,
    pub run_id: String,
    pub effect_kind: String,
    pub effect_payload_json: String,
    pub idempotency_key: String,
    pub attempt_count: i64,
}

pub struct SqliteStore {
    connection: Connection,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let connection = Connection::open(path).context("open SQLite store")?;
        Self::from_connection(connection)
    }

    pub fn in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory().context("open in-memory SQLite store")?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("enable SQLite WAL")?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .context("enable SQLite foreign keys")?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .context("configure SQLite busy timeout")?;

        let mut store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS schema_meta (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tasks (
                task_id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                state_json TEXT NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS runs (
                run_id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                revision INTEGER NOT NULL,
                state_json TEXT NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                FOREIGN KEY(task_id) REFERENCES tasks(task_id)
            );

            CREATE TABLE IF NOT EXISTS dispatch_attempts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                run_id TEXT NOT NULL,
                dispatch_id TEXT NOT NULL,
                prepared_intent_json TEXT NOT NULL,
                settlement_json TEXT,
                created_at_unix_ms INTEGER NOT NULL,
                settled_at_unix_ms INTEGER,
                UNIQUE(task_id, run_id, dispatch_id)
            );

            CREATE TABLE IF NOT EXISTS run_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                run_id TEXT NOT NULL,
                revision INTEGER NOT NULL,
                event_kind TEXT NOT NULL,
                event_payload_json TEXT NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                UNIQUE(run_id, revision)
            );

            CREATE TABLE IF NOT EXISTS effect_outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                run_id TEXT NOT NULL,
                revision INTEGER NOT NULL,
                effect_kind TEXT NOT NULL,
                effect_payload_json TEXT NOT NULL,
                idempotency_key TEXT NOT NULL UNIQUE,
                status TEXT NOT NULL CHECK(status IN ('pending','settled','failed')),
                attempt_count INTEGER NOT NULL DEFAULT 0,
                created_at_unix_ms INTEGER NOT NULL,
                settled_at_unix_ms INTEGER,
                settlement_json TEXT
            );

            CREATE TABLE IF NOT EXISTS approval_fingerprints (
                fingerprint TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                run_id TEXT NOT NULL,
                phase TEXT NOT NULL,
                round INTEGER NOT NULL,
                conversation_fingerprint TEXT NOT NULL,
                settlement_until_unix_ms INTEGER,
                state TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS ui_session_leases (
                lease_name TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL,
                generation INTEGER NOT NULL,
                expires_at_unix_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS attachments (
                attachment_id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                file_name TEXT NOT NULL,
                sha256 TEXT NOT NULL,
                storage_ref TEXT NOT NULL,
                metadata_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS acceptance_evidence (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT,
                run_id TEXT,
                evidence_kind TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                sha256 TEXT,
                created_at_unix_ms INTEGER NOT NULL
            );
            ",
        )?;

        transaction.execute(
            "INSERT INTO schema_meta(key, value) VALUES('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [SCHEMA_VERSION],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.connection
            .query_row(
                "SELECT value FROM schema_meta WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .context("read schema version")
    }

    pub fn record_transition(&mut self, record: &TransitionRecord, now_unix_ms: i64) -> Result<()> {
        validate_json(&record.event_payload_json, "event payload")?;
        validate_json(&record.materialized_state_json, "materialized state")?;
        validate_json(&record.effect_payload_json, "effect payload")?;

        if record.next_revision != record.expected_revision + 1 {
            bail!("next revision must be expected revision + 1");
        }

        let transaction = self.connection.transaction()?;
        ensure_task_revision(
            &transaction,
            record.task_id.as_str(),
            record.expected_revision,
            now_unix_ms,
        )?;

        let changed = transaction.execute(
            "UPDATE tasks
             SET revision=?2, state_json=?3, updated_at_unix_ms=?4
             WHERE task_id=?1 AND revision=?5",
            params![
                record.task_id.as_str(),
                record.next_revision,
                record.materialized_state_json,
                now_unix_ms,
                record.expected_revision
            ],
        )?;

        if changed != 1 {
            bail!("task revision conflict");
        }

        transaction.execute(
            "INSERT INTO runs(run_id, task_id, revision, state_json, updated_at_unix_ms)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(run_id) DO UPDATE SET
                 revision=excluded.revision,
                 state_json=excluded.state_json,
                 updated_at_unix_ms=excluded.updated_at_unix_ms",
            params![
                record.run_id.as_str(),
                record.task_id.as_str(),
                record.next_revision,
                record.materialized_state_json,
                now_unix_ms
            ],
        )?;

        transaction.execute(
            "INSERT INTO run_events(task_id, run_id, revision, event_kind, event_payload_json, created_at_unix_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.task_id.as_str(),
                record.run_id.as_str(),
                record.next_revision,
                record.event_kind,
                record.event_payload_json,
                now_unix_ms
            ],
        )?;

        transaction.execute(
            "INSERT INTO effect_outbox(
                 task_id, run_id, revision, effect_kind, effect_payload_json,
                 idempotency_key, status, created_at_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)",
            params![
                record.task_id.as_str(),
                record.run_id.as_str(),
                record.next_revision,
                record.effect_kind,
                record.effect_payload_json,
                record.idempotency_key,
                now_unix_ms
            ],
        )?;

        transaction.commit()?;
        Ok(())
    }

    pub fn pending_effects(&self, limit: usize) -> Result<Vec<PendingEffect>> {
        let mut statement = self.connection.prepare(
            "SELECT id, task_id, run_id, effect_kind, effect_payload_json, idempotency_key, attempt_count
             FROM effect_outbox
             WHERE status='pending'
             ORDER BY id ASC
             LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(PendingEffect {
                id: row.get(0)?,
                task_id: row.get(1)?,
                run_id: row.get(2)?,
                effect_kind: row.get(3)?,
                effect_payload_json: row.get(4)?,
                idempotency_key: row.get(5)?,
                attempt_count: row.get(6)?,
            })
        })?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("read pending effects")
    }

    pub fn mark_effect_attempted(&self, effect_id: i64) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE effect_outbox SET attempt_count=attempt_count+1
             WHERE id=?1 AND status='pending'",
            [effect_id],
        )?;
        if changed != 1 {
            bail!("pending effect not found");
        }
        Ok(())
    }

    pub fn settle_effect(
        &self,
        effect_id: i64,
        settlement_json: &str,
        now_unix_ms: i64,
    ) -> Result<()> {
        validate_json(settlement_json, "effect settlement")?;
        let changed = self.connection.execute(
            "UPDATE effect_outbox
             SET status='settled', settlement_json=?2, settled_at_unix_ms=?3
             WHERE id=?1 AND status='pending'",
            params![effect_id, settlement_json, now_unix_ms],
        )?;
        if changed != 1 {
            bail!("pending effect not found or already settled");
        }
        Ok(())
    }

    pub fn task_revision(&self, task_id: &TaskId) -> Result<Option<i64>> {
        self.connection
            .query_row(
                "SELECT revision FROM tasks WHERE task_id=?1",
                [task_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .context("read task revision")
    }

    #[cfg(test)]
    fn count_rows(&self, table: &str) -> Result<i64> {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        self.connection
            .query_row(&sql, [], |row| row.get(0))
            .context("count rows")
    }
}

fn ensure_task_revision(
    transaction: &Transaction<'_>,
    task_id: &str,
    expected_revision: i64,
    now_unix_ms: i64,
) -> Result<()> {
    let current: Option<i64> = transaction
        .query_row(
            "SELECT revision FROM tasks WHERE task_id=?1",
            [task_id],
            |row| row.get(0),
        )
        .optional()?;

    match current {
        Some(revision) if revision == expected_revision => Ok(()),
        Some(_) => bail!("task revision conflict"),
        None if expected_revision == 0 => {
            transaction.execute(
                "INSERT INTO tasks(task_id, revision, state_json, updated_at_unix_ms)
                 VALUES(?1, 0, '{}', ?2)",
                params![task_id, now_unix_ms],
            )?;
            Ok(())
        }
        None => bail!("task does not exist at expected revision"),
    }
}

fn validate_json(value: &str, label: &str) -> Result<()> {
    let _: Value = serde_json::from_str(value).with_context(|| format!("invalid {label} JSON"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transition(expected_revision: i64, key: &str) -> TransitionRecord {
        TransitionRecord {
            task_id: TaskId::new("task-1"),
            run_id: RunId::new("run-1"),
            expected_revision,
            next_revision: expected_revision + 1,
            event_kind: "dispatch_prepared".into(),
            event_payload_json: r#"{"dispatch":"d1"}"#.into(),
            materialized_state_json: format!(r#"{{"revision":{}}}"#, expected_revision + 1),
            effect_kind: "send".into(),
            effect_payload_json: r#"{"prompt":"hello"}"#.into(),
            idempotency_key: key.into(),
        }
    }

    #[test]
    fn enables_schema_and_wal_contract() {
        let store = SqliteStore::in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), 1);
        assert_eq!(store.count_rows("tasks").unwrap(), 0);
        assert_eq!(store.count_rows("effect_outbox").unwrap(), 0);
    }

    #[test]
    fn commits_state_event_and_effect_atomically() {
        let mut store = SqliteStore::in_memory().unwrap();
        store
            .record_transition(&transition(0, "effect-1"), 100)
            .unwrap();

        assert_eq!(
            store.task_revision(&TaskId::new("task-1")).unwrap(),
            Some(1)
        );
        assert_eq!(store.count_rows("run_events").unwrap(), 1);
        assert_eq!(store.count_rows("effect_outbox").unwrap(), 1);

        let effects = store.pending_effects(10).unwrap();
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].idempotency_key, "effect-1");
    }

    #[test]
    fn revision_conflict_rolls_back_event_and_effect() {
        let mut store = SqliteStore::in_memory().unwrap();
        store
            .record_transition(&transition(0, "effect-1"), 100)
            .unwrap();

        let error = store
            .record_transition(&transition(0, "effect-2"), 200)
            .unwrap_err();
        assert!(error.to_string().contains("revision conflict"));

        assert_eq!(
            store.task_revision(&TaskId::new("task-1")).unwrap(),
            Some(1)
        );
        assert_eq!(store.count_rows("run_events").unwrap(), 1);
        assert_eq!(store.count_rows("effect_outbox").unwrap(), 1);
    }

    #[test]
    fn idempotency_key_prevents_duplicate_effect_in_same_transition() {
        let mut store = SqliteStore::in_memory().unwrap();
        store
            .record_transition(&transition(0, "same-key"), 100)
            .unwrap();

        let duplicate = transition(1, "same-key");
        assert!(store.record_transition(&duplicate, 200).is_err());

        assert_eq!(
            store.task_revision(&TaskId::new("task-1")).unwrap(),
            Some(1)
        );
        assert_eq!(store.count_rows("run_events").unwrap(), 1);
        assert_eq!(store.count_rows("effect_outbox").unwrap(), 1);
    }

    #[test]
    fn effect_settlement_is_one_way() {
        let mut store = SqliteStore::in_memory().unwrap();
        store
            .record_transition(&transition(0, "effect-1"), 100)
            .unwrap();

        let effect = store.pending_effects(1).unwrap().pop().unwrap();
        store.mark_effect_attempted(effect.id).unwrap();
        store
            .settle_effect(effect.id, r#"{"observed":true}"#, 200)
            .unwrap();

        assert!(store.pending_effects(10).unwrap().is_empty());
        assert!(
            store
                .settle_effect(effect.id, r#"{"observed":true}"#, 300)
                .is_err()
        );
    }
}
