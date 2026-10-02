use anyhow::{Context, Result, bail};
use fabushi_chatgpt_domain::{AttachmentId, DispatchId, RunId, TaskId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

const SCHEMA_VERSION: i64 = 3;

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
    pub prepared_dispatch: Option<PreparedDispatch>,
    pub prepared_approval: Option<PreparedApproval>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateTransitionRecord {
    pub task_id: TaskId,
    pub run_id: RunId,
    pub expected_revision: i64,
    pub next_revision: i64,
    pub event_kind: String,
    pub event_payload_json: String,
    pub materialized_state_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDispatch {
    pub dispatch_id: DispatchId,
    pub prepared_intent_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRecord {
    pub attachment_id: AttachmentId,
    pub task_id: TaskId,
    pub file_name: String,
    pub sha256: String,
    pub storage_ref: String,
    pub metadata_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompleteContinuousPhase {
    pub run_id: RunId,
    pub dispatch_id: DispatchId,
    pub event_payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedApproval {
    pub fingerprint: String,
    pub phase: String,
    pub round: i64,
    pub conversation_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalFingerprintRecord {
    pub fingerprint: String,
    pub task_id: String,
    pub run_id: String,
    pub phase: String,
    pub round: i64,
    pub conversation_fingerprint: String,
    pub settlement_until_unix_ms: Option<i64>,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSettlementRecord {
    pub task_id: String,
    pub run_id: String,
    pub phase: String,
    pub round: i64,
    pub conversation_fingerprint: String,
    pub progress_signature: String,
    pub no_final_since_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWakeRecord {
    pub task_id: String,
    pub run_id: String,
    pub reason: String,
    pub wake_at_unix_ms: i64,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiSessionLease {
    pub lease_name: String,
    pub owner_id: String,
    pub generation: i64,
    pub expires_at_unix_ms: i64,
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
            .busy_timeout(std::time::Duration::from_secs(5))
            .context("configure SQLite busy timeout")?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("enable SQLite WAL")?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .context("enable SQLite foreign keys")?;

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

            CREATE TABLE IF NOT EXISTS review_settlement_states (
                task_id TEXT NOT NULL,
                run_id TEXT NOT NULL,
                phase TEXT NOT NULL,
                round INTEGER NOT NULL,
                conversation_fingerprint TEXT NOT NULL,
                progress_signature TEXT NOT NULL,
                no_final_since_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY(task_id, run_id, phase, round, conversation_fingerprint)
            );

            CREATE TABLE IF NOT EXISTS task_wakes (
                task_id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                reason TEXT NOT NULL,
                wake_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_task_wakes_deadline
                ON task_wakes(wake_at_unix_ms);

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
        if let Some(dispatch) = record.prepared_dispatch.as_ref() {
            validate_json(&dispatch.prepared_intent_json, "prepared dispatch intent")?;
        }

        if record.next_revision != record.expected_revision + 1 {
            bail!("next revision must be expected revision + 1");
        }

        let transaction = self.connection.transaction()?;
        if let Some(approval) = record.prepared_approval.as_ref() {
            transaction.execute(
                "INSERT INTO approval_fingerprints(
                     fingerprint, task_id, run_id, phase, round,
                     conversation_fingerprint, settlement_until_unix_ms, state
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, NULL, 'pending')
                 ON CONFLICT(fingerprint) DO NOTHING",
                params![
                    approval.fingerprint,
                    record.task_id.as_str(),
                    record.run_id.as_str(),
                    approval.phase,
                    approval.round,
                    approval.conversation_fingerprint,
                ],
            )?;

            let stored: (String, String, String, i64, String) = transaction.query_row(
                "SELECT task_id, run_id, phase, round, conversation_fingerprint
                 FROM approval_fingerprints WHERE fingerprint=?1",
                [approval.fingerprint.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
            let expected = (
                record.task_id.as_str().to_owned(),
                record.run_id.as_str().to_owned(),
                approval.phase.clone(),
                approval.round,
                approval.conversation_fingerprint.clone(),
            );
            if stored != expected {
                bail!("approval fingerprint already exists with a different identity");
            }
        }

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

        if let Some(dispatch) = record.prepared_dispatch.as_ref() {
            transaction.execute(
                "INSERT INTO dispatch_attempts(
                     task_id, run_id, dispatch_id, prepared_intent_json, created_at_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(task_id, run_id, dispatch_id) DO NOTHING",
                params![
                    record.task_id.as_str(),
                    record.run_id.as_str(),
                    dispatch.dispatch_id.as_str(),
                    dispatch.prepared_intent_json,
                    now_unix_ms
                ],
            )?;

            let stored: String = transaction.query_row(
                "SELECT prepared_intent_json FROM dispatch_attempts
                 WHERE task_id=?1 AND run_id=?2 AND dispatch_id=?3",
                params![
                    record.task_id.as_str(),
                    record.run_id.as_str(),
                    dispatch.dispatch_id.as_str()
                ],
                |row| row.get(0),
            )?;
            if stored != dispatch.prepared_intent_json {
                bail!("dispatch identity already exists with a different prepared intent");
            }
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

    pub fn record_state_transition(
        &mut self,
        record: &StateTransitionRecord,
        now_unix_ms: i64,
    ) -> Result<()> {
        validate_json(&record.event_payload_json, "event payload")?;
        validate_json(&record.materialized_state_json, "materialized state")?;
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

    pub fn review_settlement(
        &self,
        task_id: &TaskId,
        run_id: &RunId,
        phase: &str,
        round: i64,
        conversation_fingerprint: &str,
    ) -> Result<Option<ReviewSettlementRecord>> {
        self.connection
            .query_row(
                "SELECT task_id, run_id, phase, round, conversation_fingerprint,
                        progress_signature, no_final_since_unix_ms
                 FROM review_settlement_states
                 WHERE task_id=?1 AND run_id=?2 AND phase=?3 AND round=?4
                   AND conversation_fingerprint=?5",
                params![
                    task_id.as_str(),
                    run_id.as_str(),
                    phase,
                    round,
                    conversation_fingerprint
                ],
                |row| {
                    Ok(ReviewSettlementRecord {
                        task_id: row.get(0)?,
                        run_id: row.get(1)?,
                        phase: row.get(2)?,
                        round: row.get(3)?,
                        conversation_fingerprint: row.get(4)?,
                        progress_signature: row.get(5)?,
                        no_final_since_unix_ms: row.get(6)?,
                    })
                },
            )
            .optional()
            .context("read review settlement")
    }

    pub fn store_review_settlement(
        &self,
        record: &ReviewSettlementRecord,
        now_unix_ms: i64,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO review_settlement_states(
                 task_id, run_id, phase, round, conversation_fingerprint,
                 progress_signature, no_final_since_unix_ms, updated_at_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(task_id, run_id, phase, round, conversation_fingerprint)
             DO UPDATE SET
                 progress_signature=excluded.progress_signature,
                 no_final_since_unix_ms=excluded.no_final_since_unix_ms,
                 updated_at_unix_ms=excluded.updated_at_unix_ms",
            params![
                record.task_id,
                record.run_id,
                record.phase,
                record.round,
                record.conversation_fingerprint,
                record.progress_signature,
                record.no_final_since_unix_ms,
                now_unix_ms
            ],
        )?;
        Ok(())
    }

    pub fn clear_review_settlement(
        &self,
        task_id: &TaskId,
        run_id: &RunId,
        phase: &str,
        round: i64,
        conversation_fingerprint: &str,
    ) -> Result<()> {
        self.connection.execute(
            "DELETE FROM review_settlement_states
             WHERE task_id=?1 AND run_id=?2 AND phase=?3 AND round=?4
               AND conversation_fingerprint=?5",
            params![
                task_id.as_str(),
                run_id.as_str(),
                phase,
                round,
                conversation_fingerprint
            ],
        )?;
        Ok(())
    }

    pub fn clear_review_settlements_for_task(&self, task_id: &TaskId) -> Result<usize> {
        self.connection
            .execute(
                "DELETE FROM review_settlement_states WHERE task_id=?1",
                [task_id.as_str()],
            )
            .context("clear review settlements for task")
    }

    pub fn approval_fingerprint(
        &self,
        fingerprint: &str,
    ) -> Result<Option<ApprovalFingerprintRecord>> {
        self.connection
            .query_row(
                "SELECT fingerprint, task_id, run_id, phase, round,
                        conversation_fingerprint, settlement_until_unix_ms, state
                 FROM approval_fingerprints WHERE fingerprint=?1",
                [fingerprint],
                |row| {
                    Ok(ApprovalFingerprintRecord {
                        fingerprint: row.get(0)?,
                        task_id: row.get(1)?,
                        run_id: row.get(2)?,
                        phase: row.get(3)?,
                        round: row.get(4)?,
                        conversation_fingerprint: row.get(5)?,
                        settlement_until_unix_ms: row.get(6)?,
                        state: row.get(7)?,
                    })
                },
            )
            .optional()
            .context("read approval fingerprint")
    }

    pub fn arm_approval_settlement(
        &self,
        fingerprint: &str,
        settlement_until_unix_ms: i64,
    ) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE approval_fingerprints
             SET settlement_until_unix_ms=?2, state='settling'
             WHERE fingerprint=?1 AND state IN ('pending','settling')",
            params![fingerprint, settlement_until_unix_ms],
        )?;
        if changed != 1 {
            bail!("approval fingerprint not found or already settled");
        }
        Ok(())
    }

    pub fn settle_approval_fingerprint(&self, fingerprint: &str) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE approval_fingerprints
             SET state='settled'
             WHERE fingerprint=?1 AND state IN ('pending','settling')",
            [fingerprint],
        )?;
        if changed != 1 {
            bail!("approval fingerprint not found or already settled");
        }
        Ok(())
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

    pub fn settle_confirmed_dispatch_effect(
        &mut self,
        effect_id: i64,
        task_id: &TaskId,
        run_id: &RunId,
        dispatch_id: &DispatchId,
        settlement_json: &str,
        now_unix_ms: i64,
    ) -> Result<()> {
        validate_json(settlement_json, "dispatch effect settlement")?;
        let transaction = self.connection.transaction()?;
        let effect_changed = transaction.execute(
            "UPDATE effect_outbox
             SET status='settled', settlement_json=?2, settled_at_unix_ms=?3
             WHERE id=?1 AND status='pending'",
            params![effect_id, settlement_json, now_unix_ms],
        )?;
        if effect_changed != 1 {
            bail!("pending Send effect not found or already settled");
        }
        let dispatch_changed = transaction.execute(
            "UPDATE dispatch_attempts
             SET settlement_json=?4, settled_at_unix_ms=?5
             WHERE task_id=?1 AND run_id=?2 AND dispatch_id=?3
               AND settlement_json IS NULL",
            params![
                task_id.as_str(),
                run_id.as_str(),
                dispatch_id.as_str(),
                settlement_json,
                now_unix_ms
            ],
        )?;
        if dispatch_changed != 1 {
            bail!("pending dispatch attempt not found or already settled");
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn settle_dispatch_attempt(
        &self,
        task_id: &TaskId,
        run_id: &RunId,
        dispatch_id: &DispatchId,
        settlement_json: &str,
        now_unix_ms: i64,
    ) -> Result<()> {
        validate_json(settlement_json, "dispatch settlement")?;
        let changed = self.connection.execute(
            "UPDATE dispatch_attempts
             SET settlement_json=?4, settled_at_unix_ms=?5
             WHERE task_id=?1 AND run_id=?2 AND dispatch_id=?3",
            params![
                task_id.as_str(),
                run_id.as_str(),
                dispatch_id.as_str(),
                settlement_json,
                now_unix_ms
            ],
        )?;
        if changed != 1 {
            bail!("dispatch attempt not found");
        }
        Ok(())
    }

    pub fn dispatch_attempt_settlement(
        &self,
        task_id: &TaskId,
        run_id: &RunId,
        dispatch_id: &DispatchId,
    ) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT settlement_json FROM dispatch_attempts
                 WHERE task_id=?1 AND run_id=?2 AND dispatch_id=?3",
                params![task_id.as_str(), run_id.as_str(), dispatch_id.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
            .context("read dispatch attempt settlement")
    }

    pub fn latest_incomplete_continuous_phase(
        &self,
        task_id: &TaskId,
    ) -> Result<Option<IncompleteContinuousPhase>> {
        self.connection
            .query_row(
                "SELECT phase.run_id, dispatch.dispatch_id, phase.event_payload_json
                 FROM run_events AS phase
                 JOIN dispatch_attempts AS dispatch
                   ON dispatch.task_id=phase.task_id AND dispatch.run_id=phase.run_id
                 WHERE phase.task_id=?1
                   AND phase.event_kind='continuous_phase_started'
                   AND NOT EXISTS (
                       SELECT 1
                       FROM run_events AS outcome
                       WHERE outcome.run_id=phase.run_id
                         AND outcome.event_kind IN (
                             'continuous_work_result_saved',
                             'continuous_review_applied'
                         )
                   )
                 ORDER BY phase.id DESC, dispatch.id DESC
                 LIMIT 1",
                [task_id.as_str()],
                |row| {
                    Ok(IncompleteContinuousPhase {
                        run_id: RunId::new(row.get::<_, String>(0)?),
                        dispatch_id: DispatchId::new(row.get::<_, String>(1)?),
                        event_payload_json: row.get(2)?,
                    })
                },
            )
            .optional()
            .context("read latest incomplete continuous phase")
    }

    pub fn upsert_attachment(&self, attachment: &AttachmentRecord) -> Result<()> {
        validate_json(&attachment.metadata_json, "attachment metadata")?;
        self.connection.execute(
            "INSERT INTO attachments(attachment_id, task_id, file_name, sha256, storage_ref, metadata_json)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(attachment_id) DO UPDATE SET
               task_id=excluded.task_id,
               file_name=excluded.file_name,
               sha256=excluded.sha256,
               storage_ref=excluded.storage_ref,
               metadata_json=excluded.metadata_json",
            params![
                attachment.attachment_id.as_str(),
                attachment.task_id.as_str(),
                attachment.file_name,
                attachment.sha256,
                attachment.storage_ref,
                attachment.metadata_json,
            ],
        )?;
        Ok(())
    }

    pub fn task_attachments(&self, task_id: &TaskId) -> Result<Vec<AttachmentRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT attachment_id, task_id, file_name, sha256, storage_ref, metadata_json
             FROM attachments
             WHERE task_id=?1
             ORDER BY attachment_id ASC",
        )?;
        let rows = statement.query_map([task_id.as_str()], |row| {
            Ok(AttachmentRecord {
                attachment_id: AttachmentId::new(row.get::<_, String>(0)?),
                task_id: TaskId::new(row.get::<_, String>(1)?),
                file_name: row.get(2)?,
                sha256: row.get(3)?,
                storage_ref: row.get(4)?,
                metadata_json: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("read task attachments")
    }

    pub fn task_wake(&self, task_id: &TaskId) -> Result<Option<TaskWakeRecord>> {
        self.connection
            .query_row(
                "SELECT task_id, run_id, reason, wake_at_unix_ms
                 FROM task_wakes WHERE task_id=?1",
                [task_id.as_str()],
                |row| {
                    Ok(TaskWakeRecord {
                        task_id: row.get(0)?,
                        run_id: row.get(1)?,
                        reason: row.get(2)?,
                        wake_at_unix_ms: row.get(3)?,
                    })
                },
            )
            .optional()
            .context("read task wake")
    }

    pub fn arm_task_wake(
        &mut self,
        task_id: &TaskId,
        run_id: &RunId,
        reason: &str,
        requested_wake_at_unix_ms: i64,
        now_unix_ms: i64,
    ) -> Result<TaskWakeRecord> {
        let transaction = self.connection.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT run_id, reason, wake_at_unix_ms
                 FROM task_wakes WHERE task_id=?1",
                [task_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;

        let wake_at_unix_ms = match existing {
            Some((existing_run, existing_reason, existing_deadline))
                if existing_run == run_id.as_str()
                    && existing_reason == reason
                    && existing_deadline > now_unix_ms =>
            {
                existing_deadline
            }
            _ => requested_wake_at_unix_ms,
        };

        transaction.execute(
            "INSERT INTO task_wakes(task_id, run_id, reason, wake_at_unix_ms, updated_at_unix_ms)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(task_id) DO UPDATE SET
               run_id=excluded.run_id,
               reason=excluded.reason,
               wake_at_unix_ms=excluded.wake_at_unix_ms,
               updated_at_unix_ms=excluded.updated_at_unix_ms",
            params![
                task_id.as_str(),
                run_id.as_str(),
                reason,
                wake_at_unix_ms,
                now_unix_ms,
            ],
        )?;
        transaction.commit()?;
        Ok(TaskWakeRecord {
            task_id: task_id.as_str().to_owned(),
            run_id: run_id.as_str().to_owned(),
            reason: reason.to_owned(),
            wake_at_unix_ms,
        })
    }

    pub fn clear_task_wake(&self, task_id: &TaskId) -> Result<bool> {
        Ok(self.connection.execute(
            "DELETE FROM task_wakes WHERE task_id=?1",
            [task_id.as_str()],
        )? > 0)
    }

    pub fn task_state_json(&self, task_id: &TaskId) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT state_json FROM tasks WHERE task_id=?1",
                [task_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .context("read task state")
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

    pub fn acquire_ui_session_lease(
        &mut self,
        lease_name: &str,
        owner_id: &str,
        now_unix_ms: i64,
        ttl_ms: i64,
    ) -> Result<Option<UiSessionLease>> {
        if lease_name.trim().is_empty() || owner_id.trim().is_empty() {
            bail!("lease name and owner id must be non-empty");
        }
        if ttl_ms <= 0 {
            bail!("lease ttl must be positive");
        }
        let expires_at = now_unix_ms
            .checked_add(ttl_ms)
            .ok_or_else(|| anyhow::anyhow!("lease expiry overflow"))?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO ui_session_leases(lease_name, owner_id, generation, expires_at_unix_ms)
             VALUES(?1, ?2, 1, ?3)
             ON CONFLICT(lease_name) DO UPDATE SET
                 owner_id=excluded.owner_id,
                 generation=CASE
                     WHEN ui_session_leases.expires_at_unix_ms <= ?4
                         THEN ui_session_leases.generation + 1
                     WHEN ui_session_leases.owner_id = excluded.owner_id
                         THEN ui_session_leases.generation
                     ELSE ui_session_leases.generation + 1
                 END,
                 expires_at_unix_ms=excluded.expires_at_unix_ms
             WHERE ui_session_leases.owner_id = excluded.owner_id
                OR ui_session_leases.expires_at_unix_ms <= ?4",
            params![lease_name, owner_id, expires_at, now_unix_ms],
        )?;
        let lease = query_ui_session_lease(&transaction, lease_name)?;
        transaction.commit()?;
        Ok(lease
            .filter(|lease| lease.owner_id == owner_id && lease.expires_at_unix_ms > now_unix_ms))
    }

    pub fn renew_ui_session_lease(
        &self,
        lease: &UiSessionLease,
        now_unix_ms: i64,
        ttl_ms: i64,
    ) -> Result<bool> {
        if ttl_ms <= 0 {
            bail!("lease ttl must be positive");
        }
        let expires_at = now_unix_ms
            .checked_add(ttl_ms)
            .ok_or_else(|| anyhow::anyhow!("lease expiry overflow"))?;
        let changed = self.connection.execute(
            "UPDATE ui_session_leases
             SET expires_at_unix_ms=?4
             WHERE lease_name=?1 AND owner_id=?2 AND generation=?3
               AND expires_at_unix_ms>?5",
            params![
                lease.lease_name,
                lease.owner_id,
                lease.generation,
                expires_at,
                now_unix_ms
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn release_ui_session_lease(&self, lease: &UiSessionLease) -> Result<bool> {
        let changed = self.connection.execute(
            "DELETE FROM ui_session_leases
             WHERE lease_name=?1 AND owner_id=?2 AND generation=?3",
            params![lease.lease_name, lease.owner_id, lease.generation],
        )?;
        Ok(changed == 1)
    }

    pub fn ui_session_lease(&self, lease_name: &str) -> Result<Option<UiSessionLease>> {
        query_ui_session_lease(&self.connection, lease_name)
    }

    #[cfg(test)]
    fn count_rows(&self, table: &str) -> Result<i64> {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        self.connection
            .query_row(&sql, [], |row| row.get(0))
            .context("count rows")
    }
}

fn query_ui_session_lease(
    connection: &Connection,
    lease_name: &str,
) -> Result<Option<UiSessionLease>> {
    connection
        .query_row(
            "SELECT lease_name, owner_id, generation, expires_at_unix_ms
             FROM ui_session_leases WHERE lease_name=?1",
            [lease_name],
            |row| {
                Ok(UiSessionLease {
                    lease_name: row.get(0)?,
                    owner_id: row.get(1)?,
                    generation: row.get(2)?,
                    expires_at_unix_ms: row.get(3)?,
                })
            },
        )
        .optional()
        .context("read UI session lease")
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
            prepared_dispatch: None,
            prepared_approval: None,
        }
    }

    #[test]
    fn enables_schema_and_wal_contract() {
        let store = SqliteStore::in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), 3);
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
    fn latest_incomplete_continuous_phase_disappears_after_result_transition() {
        let store = SqliteStore::in_memory().unwrap();
        let task_id = TaskId::new("task-resume");
        store
            .connection
            .execute(
                "INSERT INTO tasks(task_id, revision, state_json, updated_at_unix_ms)
                 VALUES(?1, 1, '{}', 1)",
                [task_id.as_str()],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO run_events(
                     task_id, run_id, revision, event_kind, event_payload_json, created_at_unix_ms
                 ) VALUES(?1, 'run-resume', 1, 'continuous_phase_started', ?2, 1)",
                params![
                    task_id.as_str(),
                    r#"{"phase":"work","round":2,"goalRevision":0}"#
                ],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO dispatch_attempts(
                     task_id, run_id, dispatch_id, prepared_intent_json, created_at_unix_ms
                 ) VALUES(?1, 'run-resume', 'dispatch-resume', '{}', 2)",
                [task_id.as_str()],
            )
            .unwrap();

        let candidate = store
            .latest_incomplete_continuous_phase(&task_id)
            .unwrap()
            .unwrap();
        assert_eq!(candidate.run_id, RunId::new("run-resume"));
        assert_eq!(candidate.dispatch_id, DispatchId::new("dispatch-resume"));

        store
            .connection
            .execute(
                "INSERT INTO run_events(
                     task_id, run_id, revision, event_kind, event_payload_json, created_at_unix_ms
                 ) VALUES(?1, 'run-resume', 2, 'continuous_work_result_saved', '{}', 3)",
                [task_id.as_str()],
            )
            .unwrap();
        assert!(
            store
                .latest_incomplete_continuous_phase(&task_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn state_only_transition_is_atomic_without_creating_an_effect() {
        let mut store = SqliteStore::in_memory().unwrap();
        store
            .record_state_transition(
                &StateTransitionRecord {
                    task_id: TaskId::new("task-state"),
                    run_id: RunId::new("run-state"),
                    expected_revision: 0,
                    next_revision: 1,
                    event_kind: "continuous_phase_started".into(),
                    event_payload_json: r#"{"phase":"work","round":1}"#.into(),
                    materialized_state_json: r#"{"orchestration":{"phase":"work","round":1}}"#
                        .into(),
                },
                100,
            )
            .unwrap();

        assert_eq!(
            store
                .task_state_json(&TaskId::new("task-state"))
                .unwrap()
                .as_deref(),
            Some(r#"{"orchestration":{"phase":"work","round":1}}"#)
        );
        assert_eq!(store.count_rows("run_events").unwrap(), 1);
        assert!(store.pending_effects(10).unwrap().is_empty());
    }

    #[test]
    fn commits_prepared_dispatch_with_state_event_and_effect() {
        let mut store = SqliteStore::in_memory().unwrap();
        let mut record = transition(0, "dispatch-effect");
        record.prepared_dispatch = Some(PreparedDispatch {
            dispatch_id: DispatchId::new("dispatch-1"),
            prepared_intent_json: r#"{"prompt":"hello","dispatchId":"dispatch-1"}"#.into(),
        });
        store.record_transition(&record, 100).unwrap();

        assert_eq!(
            store
                .dispatch_attempt_settlement(
                    &TaskId::new("task-1"),
                    &RunId::new("run-1"),
                    &DispatchId::new("dispatch-1"),
                )
                .unwrap(),
            None
        );
        store
            .settle_dispatch_attempt(
                &TaskId::new("task-1"),
                &RunId::new("run-1"),
                &DispatchId::new("dispatch-1"),
                r#"{"confirmed":true}"#,
                200,
            )
            .unwrap();
        assert_eq!(
            store
                .dispatch_attempt_settlement(
                    &TaskId::new("task-1"),
                    &RunId::new("run-1"),
                    &DispatchId::new("dispatch-1"),
                )
                .unwrap()
                .as_deref(),
            Some(r#"{"confirmed":true}"#)
        );
    }

    #[test]
    fn same_dispatch_identity_can_be_retried_with_new_effect_baseline() {
        let mut store = SqliteStore::in_memory().unwrap();
        let stable_intent = PreparedDispatch {
            dispatch_id: DispatchId::new("dispatch-retry"),
            prepared_intent_json: r#"{"prompt":"hello","dispatchId":"dispatch-retry"}"#.into(),
        };
        let mut first = transition(0, "retry-effect-1");
        first.effect_payload_json =
            r#"{"prompt":"hello","dispatchId":"dispatch-retry","baseline":"u0"}"#.into();
        first.prepared_dispatch = Some(stable_intent.clone());
        store.record_transition(&first, 100).unwrap();

        let mut second = transition(1, "retry-effect-2");
        second.effect_payload_json =
            r#"{"prompt":"hello","dispatchId":"dispatch-retry","baseline":"u1"}"#.into();
        second.prepared_dispatch = Some(stable_intent);
        store.record_transition(&second, 200).unwrap();

        assert_eq!(store.count_rows("dispatch_attempts").unwrap(), 1);
        assert_eq!(store.count_rows("effect_outbox").unwrap(), 2);
        assert_eq!(
            store.task_revision(&TaskId::new("task-1")).unwrap(),
            Some(2)
        );
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
    fn ui_session_lease_fences_other_owner_until_expiry() {
        let mut store = SqliteStore::in_memory().unwrap();
        let first = store
            .acquire_ui_session_lease("chatgpt-desktop", "owner-a", 100, 1_000)
            .unwrap()
            .unwrap();
        assert_eq!(first.generation, 1);
        assert!(
            store
                .acquire_ui_session_lease("chatgpt-desktop", "owner-b", 500, 1_000)
                .unwrap()
                .is_none()
        );
        let takeover = store
            .acquire_ui_session_lease("chatgpt-desktop", "owner-b", 1_100, 1_000)
            .unwrap()
            .unwrap();
        assert_eq!(takeover.generation, 2);
        assert_eq!(takeover.owner_id, "owner-b");
        assert!(!store.release_ui_session_lease(&first).unwrap());
        assert_eq!(
            store.ui_session_lease("chatgpt-desktop").unwrap(),
            Some(takeover)
        );
    }

    #[test]
    fn ui_session_lease_renewal_requires_live_fencing_token() {
        let mut store = SqliteStore::in_memory().unwrap();
        let first = store
            .acquire_ui_session_lease("chatgpt-desktop", "owner-a", 100, 1_000)
            .unwrap()
            .unwrap();
        assert!(store.renew_ui_session_lease(&first, 500, 1_000).unwrap());
        let renewed = store.ui_session_lease("chatgpt-desktop").unwrap().unwrap();
        assert_eq!(renewed.generation, first.generation);
        assert_eq!(renewed.expires_at_unix_ms, 1_500);

        let reacquired = store
            .acquire_ui_session_lease("chatgpt-desktop", "owner-a", 1_500, 1_000)
            .unwrap()
            .unwrap();
        assert_eq!(reacquired.generation, first.generation + 1);
        assert!(!store.renew_ui_session_lease(&first, 1_600, 1_000).unwrap());
        assert!(store.release_ui_session_lease(&reacquired).unwrap());
        assert!(store.ui_session_lease("chatgpt-desktop").unwrap().is_none());
    }

    #[test]
    fn confirmed_dispatch_and_effect_settle_atomically() {
        let mut store = SqliteStore::in_memory().unwrap();
        let mut record = transition(0, "atomic-send-effect");
        record.prepared_dispatch = Some(PreparedDispatch {
            dispatch_id: DispatchId::new("dispatch-atomic"),
            prepared_intent_json: r#"{"prompt":"hello","dispatchId":"dispatch-atomic"}"#.into(),
        });
        store.record_transition(&record, 100).unwrap();
        let effect = store.pending_effects(1).unwrap().pop().unwrap();
        store.mark_effect_attempted(effect.id).unwrap();

        let missing = store
            .settle_confirmed_dispatch_effect(
                effect.id,
                &TaskId::new("task-1"),
                &RunId::new("run-1"),
                &DispatchId::new("missing-dispatch"),
                r#"{"confirmed":true}"#,
                150,
            )
            .unwrap_err();
        assert!(missing.to_string().contains("dispatch attempt"));
        assert_eq!(store.pending_effects(10).unwrap().len(), 1);

        store
            .settle_confirmed_dispatch_effect(
                effect.id,
                &TaskId::new("task-1"),
                &RunId::new("run-1"),
                &DispatchId::new("dispatch-atomic"),
                r#"{"confirmed":true}"#,
                200,
            )
            .unwrap();
        assert!(store.pending_effects(10).unwrap().is_empty());
        assert_eq!(
            store
                .dispatch_attempt_settlement(
                    &TaskId::new("task-1"),
                    &RunId::new("run-1"),
                    &DispatchId::new("dispatch-atomic"),
                )
                .unwrap()
                .as_deref(),
            Some(r#"{"confirmed":true}"#)
        );
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
    #[test]
    fn approval_intent_is_atomic_with_state_event_and_effect() {
        let mut store = SqliteStore::in_memory().unwrap();
        let mut record = transition(0, "approval-effect");
        record.effect_kind = "approve_current_conversation".into();
        record.prepared_approval = Some(PreparedApproval {
            fingerprint: "approval-fp".into(),
            phase: "work".into(),
            round: 3,
            conversation_fingerprint: "conversation-fp".into(),
        });

        store.record_transition(&record, 100).unwrap();

        let approval = store.approval_fingerprint("approval-fp").unwrap().unwrap();
        assert_eq!(approval.task_id, "task-1");
        assert_eq!(approval.run_id, "run-1");
        assert_eq!(approval.phase, "work");
        assert_eq!(approval.round, 3);
        assert_eq!(approval.conversation_fingerprint, "conversation-fp");
        assert_eq!(approval.state, "pending");
        assert_eq!(approval.settlement_until_unix_ms, None);
        assert_eq!(store.pending_effects(10).unwrap().len(), 1);
    }

    #[test]
    fn review_settlement_is_scoped_to_full_run_identity() {
        let store = SqliteStore::in_memory().unwrap();
        let record = ReviewSettlementRecord {
            task_id: "task-review".into(),
            run_id: "run-review".into(),
            phase: "review".into(),
            round: 3,
            conversation_fingerprint: "conversation-a".into(),
            progress_signature: "progress-a".into(),
            no_final_since_unix_ms: 1_000,
        };
        store.store_review_settlement(&record, 1_100).unwrap();

        let exact = store
            .review_settlement(
                &TaskId::new("task-review"),
                &RunId::new("run-review"),
                "review",
                3,
                "conversation-a",
            )
            .unwrap();
        assert!(exact.is_some());

        let wrong = [
            (
                "task-other",
                "run-review",
                "review",
                3_i64,
                "conversation-a",
            ),
            (
                "task-review",
                "run-other",
                "review",
                3_i64,
                "conversation-a",
            ),
            ("task-review", "run-review", "work", 3_i64, "conversation-a"),
            (
                "task-review",
                "run-review",
                "review",
                4_i64,
                "conversation-a",
            ),
            (
                "task-review",
                "run-review",
                "review",
                3_i64,
                "conversation-b",
            ),
        ];
        for (task, run, phase, round, conversation) in wrong {
            assert!(
                store
                    .review_settlement(
                        &TaskId::new(task),
                        &RunId::new(run),
                        phase,
                        round,
                        conversation,
                    )
                    .unwrap()
                    .is_none()
            );
        }

        store
            .clear_review_settlement(
                &TaskId::new("task-review"),
                &RunId::new("run-review"),
                "review",
                3,
                "conversation-a",
            )
            .unwrap();
        assert!(
            store
                .review_settlement(
                    &TaskId::new("task-review"),
                    &RunId::new("run-review"),
                    "review",
                    3,
                    "conversation-a",
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn task_wake_preserves_existing_deadline_across_restart_rearm() {
        let mut store = SqliteStore::in_memory().unwrap();
        let task = TaskId::new("task-a");
        let run = RunId::new("run-a");
        let first = store
            .arm_task_wake(&task, &run, "rate_limit_cooldown", 301_000, 1_000)
            .unwrap();
        assert_eq!(first.wake_at_unix_ms, 301_000);

        let restored = store
            .arm_task_wake(&task, &run, "rate_limit_cooldown", 331_000, 31_000)
            .unwrap();
        assert_eq!(restored.wake_at_unix_ms, 301_000);
        assert_eq!(store.task_wake(&task).unwrap().unwrap(), restored);

        let different_run = store
            .arm_task_wake(
                &task,
                &RunId::new("run-b"),
                "rate_limit_cooldown",
                400_000,
                40_000,
            )
            .unwrap();
        assert_eq!(different_run.wake_at_unix_ms, 400_000);
    }

    #[test]
    fn approval_settlement_window_is_persisted_and_readable() {
        let mut store = SqliteStore::in_memory().unwrap();
        let mut record = transition(0, "approval-window-effect");
        record.effect_kind = "approve_current_conversation".into();
        record.prepared_approval = Some(PreparedApproval {
            fingerprint: "approval-window".into(),
            phase: "review".into(),
            round: 7,
            conversation_fingerprint: "conversation-7".into(),
        });
        store.record_transition(&record, 100).unwrap();

        store
            .arm_approval_settlement("approval-window", 12_345)
            .unwrap();
        let approval = store
            .approval_fingerprint("approval-window")
            .unwrap()
            .unwrap();
        assert_eq!(approval.state, "settling");
        assert_eq!(approval.settlement_until_unix_ms, Some(12_345));
    }
    #[test]
    fn task_attachments_are_persisted_by_task_identity() {
        let store = SqliteStore::in_memory().unwrap();
        let task = TaskId::new("task-attachments");
        let other = TaskId::new("task-other");
        store
            .upsert_attachment(&AttachmentRecord {
                attachment_id: AttachmentId::new("attachment-b"),
                task_id: task.clone(),
                file_name: "b.txt".into(),
                sha256: "sha-b".into(),
                storage_ref: "/store/b.txt".into(),
                metadata_json: r#"{"byteLen":2}"#.into(),
            })
            .unwrap();
        store
            .upsert_attachment(&AttachmentRecord {
                attachment_id: AttachmentId::new("attachment-a"),
                task_id: task.clone(),
                file_name: "a.txt".into(),
                sha256: "sha-a".into(),
                storage_ref: "/store/a.txt".into(),
                metadata_json: r#"{"byteLen":1}"#.into(),
            })
            .unwrap();
        store
            .upsert_attachment(&AttachmentRecord {
                attachment_id: AttachmentId::new("attachment-other"),
                task_id: other,
                file_name: "other.txt".into(),
                sha256: "sha-other".into(),
                storage_ref: "/store/other.txt".into(),
                metadata_json: r#"{"byteLen":5}"#.into(),
            })
            .unwrap();

        let attachments = store.task_attachments(&task).unwrap();
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].attachment_id.as_str(), "attachment-a");
        assert_eq!(attachments[1].attachment_id.as_str(), "attachment-b");
        assert_eq!(attachments[0].task_id, task);
    }
}
