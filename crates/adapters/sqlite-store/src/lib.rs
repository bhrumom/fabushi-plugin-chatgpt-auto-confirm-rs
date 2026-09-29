use anyhow::{Context, Result, anyhow, bail};
use fabushi_chatgpt_application::{
    AccountBrowserLease, DurableTargetLease, OwnershipStore, QueueClaim, QueueSnapshot, QueueStore,
    RunJournal,
};
use fabushi_chatgpt_domain::{
    ApprovalFingerprint, AutomationTaskReport, QueuePhase, QueueTask, RecoveryEnvelope,
    RunCheckpoint, RunEvent, RunEventKind, RunRecord, RunState, TaskState,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create sqlite queue directory {}", parent.display()))?;
        }
        let connection = Connection::open(path).context("open sqlite queue store")?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let connection =
            Connection::open_in_memory().context("open in-memory sqlite queue store")?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        connection.execute_batch(
            r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS tasks (
    task_id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    status TEXT NOT NULL,
    priority INTEGER NOT NULL,
    current_revision INTEGER NOT NULL,
    body_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS runs (
    run_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    state TEXT NOT NULL,
    revision INTEGER NOT NULL,
    body_json TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    finished_at_ms INTEGER,
    FOREIGN KEY(task_id) REFERENCES tasks(task_id)
);

CREATE TABLE IF NOT EXISTS run_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES runs(run_id)
);

CREATE TABLE IF NOT EXISTS approval_fingerprints (
    fingerprint TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    settled INTEGER NOT NULL DEFAULT 0,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS worker_leases (
    run_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    lease_revision INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES runs(run_id)
);

CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    applied_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS queue_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL,
    run_id TEXT,
    from_phase TEXT NOT NULL,
    to_phase TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    FOREIGN KEY(task_id) REFERENCES tasks(task_id)
);

CREATE TABLE IF NOT EXISTS account_browser_leases (
    account_id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    process_identity TEXT NOT NULL,
    browser_pid INTEGER,
    endpoint TEXT,
    profile_dir TEXT NOT NULL,
    state TEXT NOT NULL,
    lease_revision INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS target_leases (
    target_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL UNIQUE,
    account_id TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    state TEXT NOT NULL,
    lease_revision INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS browser_lifecycle_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    details_json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tasks_status_priority
    ON tasks(status, priority DESC, updated_at_ms ASC);
CREATE INDEX IF NOT EXISTS idx_runs_task
    ON runs(task_id, started_at_ms DESC);
CREATE INDEX IF NOT EXISTS idx_run_events_run
    ON run_events(run_id, sequence);
CREATE INDEX IF NOT EXISTS idx_worker_leases_expiry
    ON worker_leases(expires_at_ms);
CREATE INDEX IF NOT EXISTS idx_queue_events_task
    ON queue_events(task_id, sequence);
CREATE INDEX IF NOT EXISTS idx_account_browser_lease_expiry
    ON account_browser_leases(expires_at_ms);
CREATE INDEX IF NOT EXISTS idx_target_lease_expiry
    ON target_leases(expires_at_ms);

INSERT OR IGNORE INTO schema_migrations(version,name,applied_at_ms)
VALUES
    (1,'base_queue_and_run_journal',strftime('%s','now') * 1000),
    (2,'queue_phase_journal',strftime('%s','now') * 1000),
    (3,'browser_and_target_ownership',strftime('%s','now') * 1000);

PRAGMA user_version=3;
"#,
        )?;
        Ok(())
    }

    pub fn journal(
        &self,
        run_id: impl Into<String>,
        owner_id: impl Into<String>,
    ) -> SqliteRunJournal {
        SqliteRunJournal {
            store: self.clone(),
            run_id: run_id.into(),
            owner_id: owner_id.into(),
        }
    }

    pub fn journal_mode(&self) -> Result<String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        Ok(connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?)
    }

    fn load_tasks_in_tx(tx: &rusqlite::Transaction<'_>) -> Result<Vec<QueueTask>> {
        let mut statement = tx.prepare(
            "SELECT body_json FROM tasks ORDER BY priority DESC, updated_at_ms ASC, task_id ASC",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(serde_json::from_str(&row?)?);
        }
        Ok(tasks)
    }

    fn task_status_name(status: &TaskState) -> &'static str {
        match status {
            TaskState::Queued => "queued",
            TaskState::Waiting => "waiting",
            TaskState::Running => "running",
            TaskState::Completed => "completed",
            TaskState::Blocked => "blocked",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }

    fn run_state_name(state: &RunState) -> &'static str {
        match state {
            RunState::Dispatching => "dispatching",
            RunState::Running => "running",
            RunState::WaitingApproval => "waiting_approval",
            RunState::Recovering => "recovering",
            RunState::Complete => "complete",
            RunState::TimedOut => "timed_out",
            RunState::Failed => "failed",
            RunState::Cancelled => "cancelled",
        }
    }
}

impl QueueStore for SqliteStore {
    fn enqueue_task(&self, task: &QueueTask) -> Result<()> {
        let mut task = task.clone();
        if task.created_at_ms == 0 {
            task.created_at_ms = now_ms();
        }
        task.updated_at_ms = now_ms();

        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        let existing: Option<(u64, String)> = connection
            .query_row(
                "SELECT current_revision, body_json FROM tasks WHERE task_id = ?1",
                params![task.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if let Some((revision, body)) = existing {
            let previous: QueueTask = serde_json::from_str(&body)?;
            if task.current_revision < revision {
                return Ok(());
            }
            if task.current_revision == revision {
                let content_changed = task.account_id != previous.account_id
                    || task.title != previous.title
                    || task.prompt != previous.prompt
                    || task.original_prompt != previous.original_prompt
                    || task.acceptance_prompt != previous.acceptance_prompt
                    || task.spec_digest != previous.spec_digest
                    || task.connector != previous.connector
                    || task.depends_on != previous.depends_on
                    || task.resource_locks != previous.resource_locks
                    || task.priority != previous.priority
                    || task.timeout_seconds != previous.timeout_seconds
                    || task.max_task_continuations != previous.max_task_continuations
                    || task.max_runtime_retries != previous.max_runtime_retries
                    || task.execution_profile != previous.execution_profile;
                if content_changed {
                    bail!(
                        "task {} revision {} changed without incrementing revision",
                        task.id,
                        revision
                    );
                }
                return Ok(());
            }
            task.status = TaskState::Queued;
            task.applied_revision = previous.applied_revision;
            task.runtime_retries = 0;
            task.attempts = 0;
            task.continuation_depth = 0;
            task.recovery_context = None;
            task.last_report = None;
            task.last_error = None;
            task.created_at_ms = previous.created_at_ms;
        }

        let body = serde_json::to_string(&task)?;
        connection.execute(
            r#"
INSERT INTO tasks(task_id, account_id, status, priority, current_revision, body_json, updated_at_ms)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
ON CONFLICT(task_id) DO UPDATE SET
    account_id=excluded.account_id,
    status=excluded.status,
    priority=excluded.priority,
    current_revision=excluded.current_revision,
    body_json=excluded.body_json,
    updated_at_ms=excluded.updated_at_ms
"#,
            params![
                task.id,
                task.account_id,
                Self::task_status_name(&task.status),
                task.priority,
                task.current_revision,
                body,
                task.updated_at_ms
            ],
        )?;
        Ok(())
    }

    fn claim_next_runnable(
        &self,
        owner_id: &str,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<Option<QueueClaim>> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        let tx = connection.transaction()?;

        let tasks = Self::load_tasks_in_tx(&tx)?;
        let statuses: HashMap<String, TaskState> = tasks
            .iter()
            .map(|task| (task.id.clone(), task.status.clone()))
            .collect();

        let running_locks: HashSet<String> = tasks
            .iter()
            .filter(|task| task.status == TaskState::Running)
            .flat_map(|task| task.resource_locks.iter().cloned())
            .collect();

        let selected = tasks.into_iter().find(|task| {
            let status_runnable = task.status == TaskState::Queued
                || (task.status == TaskState::Waiting
                    && task.waiting_until_ms.is_none_or(|until| until <= now_ms));
            let dependencies_complete = task
                .depends_on
                .iter()
                .all(|dependency| statuses.get(dependency) == Some(&TaskState::Completed));
            let locks_available = task
                .resource_locks
                .iter()
                .all(|lock| !running_locks.contains(lock));
            status_runnable && dependencies_complete && locks_available
        });

        let Some(mut task) = selected else {
            tx.commit()?;
            return Ok(None);
        };

        let from_phase = task.phase.clone();
        task.transition_phase(QueuePhase::Dispatched)?;
        task.status = TaskState::Running;
        task.waiting_until_ms = None;
        task.attempts += 1;
        task.updated_at_ms = now_ms;
        let run_id = format!("{}-{}-{}", task.id, task.current_revision, task.attempts);
        let mut run = RunRecord::new(&run_id, &task.id, now_ms);
        run.revision = 1;
        if let Some(checkpoint) = task
            .recovery_context
            .as_ref()
            .and_then(|recovery| recovery.checkpoint.clone())
        {
            run.checkpoint = checkpoint;
        }

        tx.execute(
            "UPDATE tasks SET status='running', body_json=?2, updated_at_ms=?3 WHERE task_id=?1",
            params![task.id, serde_json::to_string(&task)?, now_ms],
        )?;
        tx.execute(
            r#"
INSERT INTO runs(run_id, task_id, state, revision, body_json, started_at_ms, finished_at_ms)
VALUES (?1, ?2, 'dispatching', ?3, ?4, ?5, NULL)
"#,
            params![
                run.run_id,
                run.task_id,
                run.revision,
                serde_json::to_string(&run)?,
                run.started_at_ms
            ],
        )?;
        tx.execute(
            r#"
INSERT INTO worker_leases(run_id, task_id, owner_id, lease_revision, expires_at_ms)
VALUES (?1, ?2, ?3, 1, ?4)
"#,
            params![
                run.run_id,
                run.task_id,
                owner_id,
                now_ms + lease_duration_ms
            ],
        )?;
        tx.execute(
            r#"
INSERT INTO run_events(run_id, event_type, payload_json, created_at_ms)
VALUES (?1, 'run_started', '{}', ?2)
"#,
            params![run.run_id, now_ms],
        )?;
        tx.execute(
            r#"
INSERT INTO queue_events(task_id,run_id,from_phase,to_phase,reason,created_at_ms)
VALUES (?1,?2,?3,?4,'worker_claimed',?5)
"#,
            params![
                task.id,
                run.run_id,
                phase_name(&from_phase),
                phase_name(&task.phase),
                now_ms
            ],
        )?;
        tx.commit()?;

        Ok(Some(QueueClaim { task, run }))
    }

    fn renew_lease(
        &self,
        run_id: &str,
        owner_id: &str,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<bool> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        let changed = connection.execute(
            r#"
UPDATE worker_leases
SET lease_revision = lease_revision + 1, expires_at_ms = ?3
WHERE run_id = ?1 AND owner_id = ?2 AND expires_at_ms > ?4
"#,
            params![run_id, owner_id, now_ms + lease_duration_ms, now_ms],
        )?;
        Ok(changed == 1)
    }

    fn release_lease(&self, run_id: &str, owner_id: &str) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        connection.execute(
            "DELETE FROM worker_leases WHERE run_id=?1 AND owner_id=?2",
            params![run_id, owner_id],
        )?;
        Ok(())
    }

    fn settle_task(
        &self,
        task: &QueueTask,
        report: Option<&AutomationTaskReport>,
        recovery: Option<&RecoveryEnvelope>,
        waiting_until_ms: Option<i64>,
        error: Option<&str>,
    ) -> Result<()> {
        let mut task = task.clone();
        task.last_report = report.cloned();
        task.recovery_context = recovery.cloned();
        task.waiting_until_ms = waiting_until_ms;
        task.last_error = error.map(str::to_owned);
        task.updated_at_ms = now_ms();

        if let Some(report) = report {
            task.applied_revision = Some(report.applied_task_revision);
            if report.all_tasks_complete {
                task.status = TaskState::Completed;
            } else if report.status == fabushi_chatgpt_domain::TaskReportStatus::Blocked
                && waiting_until_ms.is_none()
            {
                task.status = TaskState::Blocked;
            } else {
                task.continuation_depth += 1;
                if task.max_task_continuations > 0
                    && task.continuation_depth > task.max_task_continuations
                {
                    task.status = TaskState::Failed;
                    task.last_error = Some("task_continuation_limit_reached".into());
                } else {
                    task.status = if waiting_until_ms.is_some() {
                        TaskState::Waiting
                    } else {
                        TaskState::Queued
                    };
                }
            }
        } else if waiting_until_ms.is_some() {
            task.status = TaskState::Waiting;
        } else if recovery.is_some() {
            task.status = TaskState::Queued;
        } else if error.is_some() {
            task.runtime_retries += 1;
            if task.runtime_retries > task.max_runtime_retries {
                task.status = TaskState::Failed;
            } else {
                task.status = TaskState::Queued;
            }
        }

        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        connection.execute(
            "UPDATE tasks SET status=?2, body_json=?3, updated_at_ms=?4 WHERE task_id=?1",
            params![
                task.id,
                Self::task_status_name(&task.status),
                serde_json::to_string(&task)?,
                task.updated_at_ms
            ],
        )?;
        Ok(())
    }

    fn recover_expired_leases(&self, now_ms: i64) -> Result<u32> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        let tx = connection.transaction()?;
        let mut statement = tx.prepare(
            r#"
SELECT l.run_id, l.task_id, t.body_json, r.body_json
FROM worker_leases l
JOIN tasks t ON t.task_id=l.task_id
JOIN runs r ON r.run_id=l.run_id
WHERE l.expires_at_ms <= ?1
"#,
        )?;
        let rows = statement.query_map(params![now_ms], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let expired: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
        drop(statement);

        for (run_id, task_id, task_json, run_json) in &expired {
            let mut task: QueueTask = serde_json::from_str(task_json)?;
            let run: RunRecord = serde_json::from_str(run_json)?;
            if task.status == TaskState::Running {
                task.runtime_retries += 1;
                task.status = if task.runtime_retries > task.max_runtime_retries {
                    TaskState::Failed
                } else {
                    TaskState::Queued
                };
                task.last_error = Some("worker_lease_expired".into());
                let mut recovered_progress = run.visible_progress_messages.clone();
                if let Some(latest) = run.latest_assistant_text.as_ref()
                    && !latest.trim().is_empty()
                    && !recovered_progress.iter().any(|value| value == latest)
                {
                    recovered_progress.push(latest.clone());
                }
                task.recovery_context = Some(RecoveryEnvelope {
                    version: 2,
                    task_id: task.id.clone(),
                    run_id: run.run_id.clone(),
                    exact_commit: task.known_exact_head.clone(),
                    conversation_url: run.canonical_conversation_url.clone(),
                    conversation_kind: task.conversation_kind.clone(),
                    original_goal: task.original_prompt.clone(),
                    acceptance_prompt: task.acceptance_prompt.clone(),
                    interrupted_turn_visible_content: recovered_progress.clone(),
                    progress_messages: recovered_progress,
                    completed: task
                        .last_report
                        .as_ref()
                        .map(|report| report.completed.clone())
                        .unwrap_or_default(),
                    remaining: task
                        .last_report
                        .as_ref()
                        .map(|report| report.remaining.clone())
                        .unwrap_or_else(|| task.pending_work.clone()),
                    blockers: task
                        .last_report
                        .as_ref()
                        .map(|report| report.blockers.clone())
                        .unwrap_or_default(),
                    known_ci_evidence: task.known_ci_evidence.clone(),
                    current_stage: task.current_stage.clone(),
                    pending_work: task.pending_work.clone(),
                    context_references: task.context_references.clone(),
                    last_committed_outbound_message: run
                        .checkpoint
                        .last_committed_outbound_message
                        .clone(),
                    outbound_delivery_confirmed: run.checkpoint.outbound_delivery_confirmed,
                    checkpoint: Some(run.checkpoint.clone()),
                    continuation_instruction:
                        "从异常中断处继续；先恢复 durable checkpoint 并观察现有会话，不要重复已完成步骤。".into(),
                });
                task.updated_at_ms = now_ms;
                tx.execute(
                    "UPDATE tasks SET status=?2, body_json=?3, updated_at_ms=?4 WHERE task_id=?1",
                    params![
                        task_id,
                        Self::task_status_name(&task.status),
                        serde_json::to_string(&task)?,
                        now_ms
                    ],
                )?;
            }

            let mut failed_run = run.clone();
            failed_run.state = RunState::Failed;
            failed_run.finished_at_ms = Some(now_ms);
            failed_run.revision += 1;
            tx.execute(
                "UPDATE runs SET state='failed', revision=?2, body_json=?3, finished_at_ms=?4 WHERE run_id=?1",
                params![
                    run_id,
                    failed_run.revision,
                    serde_json::to_string(&failed_run)?,
                    now_ms
                ],
            )?;
            tx.execute(
                "INSERT INTO run_events(run_id,event_type,payload_json,created_at_ms) VALUES (?1,'run_failed',?2,?3)",
                params![run_id, r#"{"reason":"worker_lease_expired"}"#, now_ms],
            )?;
        }

        tx.execute(
            "DELETE FROM worker_leases WHERE expires_at_ms <= ?1",
            params![now_ms],
        )?;
        tx.commit()?;
        Ok(expired.len() as u32)
    }

    fn snapshot(&self) -> Result<QueueSnapshot> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        let mut task_statement = connection.prepare(
            "SELECT body_json FROM tasks ORDER BY priority DESC, updated_at_ms ASC, task_id ASC",
        )?;
        let task_rows = task_statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut tasks = Vec::new();
        for row in task_rows {
            tasks.push(serde_json::from_str(&row?)?);
        }

        let mut run_statement = connection
            .prepare("SELECT body_json FROM runs ORDER BY started_at_ms ASC, run_id ASC")?;
        let run_rows = run_statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut runs = Vec::new();
        for row in run_rows {
            runs.push(serde_json::from_str(&row?)?);
        }
        Ok(QueueSnapshot { tasks, runs })
    }
}


impl OwnershipStore for SqliteStore {
    fn acquire_account_browser(&self, account_id: &str, owner_id: &str, process_identity: &str, profile_dir: &str, now_ms: i64, lease_duration_ms: i64) -> Result<bool> {
        let connection = self.connection.lock().map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        let existing: Option<(String, i64)> = connection.query_row(
            "SELECT owner_id, expires_at_ms FROM account_browser_leases WHERE account_id=?1",
            params![account_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if let Some((existing_owner, expires_at)) = existing
            && existing_owner != owner_id
            && expires_at > now_ms
        {
            return Ok(false);
        }
        connection.execute(
            r#"
INSERT INTO account_browser_leases(
    account_id,owner_id,process_identity,browser_pid,endpoint,profile_dir,state,
    lease_revision,expires_at_ms,updated_at_ms
)
VALUES (?1,?2,?3,NULL,NULL,?4,'starting',1,?5,?6)
ON CONFLICT(account_id) DO UPDATE SET
    owner_id=excluded.owner_id,
    process_identity=excluded.process_identity,
    browser_pid=NULL,
    endpoint=NULL,
    profile_dir=excluded.profile_dir,
    state='starting',
    lease_revision=account_browser_leases.lease_revision+1,
    expires_at_ms=excluded.expires_at_ms,
    updated_at_ms=excluded.updated_at_ms
"#,
            params![account_id, owner_id, process_identity, profile_dir, now_ms + lease_duration_ms, now_ms],
        )?;
        Ok(true)
    }

    fn bind_account_browser_process(&self, account_id: &str, owner_id: &str, browser_pid: u32, endpoint: &str, now_ms: i64) -> Result<()> {
        let connection = self.connection.lock().map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        let changed = connection.execute(
            r#"
UPDATE account_browser_leases
SET browser_pid=?3, endpoint=?4, state='running', lease_revision=lease_revision+1, updated_at_ms=?5
WHERE account_id=?1 AND owner_id=?2 AND expires_at_ms>?5
"#,
            params![account_id, owner_id, browser_pid, endpoint, now_ms],
        )?;
        if changed != 1 { bail!("account browser lease lost before process bind"); }
        Ok(())
    }

    fn renew_account_browser(&self, account_id: &str, owner_id: &str, now_ms: i64, lease_duration_ms: i64) -> Result<bool> {
        let connection = self.connection.lock().map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        Ok(connection.execute(
            r#"
UPDATE account_browser_leases
SET lease_revision=lease_revision+1, expires_at_ms=?3, updated_at_ms=?2
WHERE account_id=?1 AND owner_id=?4 AND expires_at_ms>?2
"#,
            params![account_id, now_ms, now_ms + lease_duration_ms, owner_id],
        )? == 1)
    }

    fn release_account_browser(&self, account_id: &str, owner_id: &str) -> Result<()> {
        let connection = self.connection.lock().map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        connection.execute(
            "DELETE FROM account_browser_leases WHERE account_id=?1 AND owner_id=?2",
            params![account_id, owner_id],
        )?;
        Ok(())
    }

    fn account_browser_lease(&self, account_id: &str) -> Result<Option<AccountBrowserLease>> {
        let connection = self.connection.lock().map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        connection.query_row(
            r#"SELECT account_id,owner_id,process_identity,browser_pid,endpoint,profile_dir,expires_at_ms
FROM account_browser_leases WHERE account_id=?1"#,
            params![account_id],
            |row| Ok(AccountBrowserLease {
                account_id: row.get(0)?,
                owner_id: row.get(1)?,
                process_identity: row.get(2)?,
                browser_pid: row.get::<_, Option<u32>>(3)?,
                endpoint: row.get(4)?,
                profile_dir: row.get(5)?,
                expires_at_ms: row.get(6)?,
            }),
        ).optional().map_err(Into::into)
    }

    fn acquire_target(&self, target_id: &str, run_id: &str, account_id: &str, owner_id: &str, now_ms: i64, lease_duration_ms: i64) -> Result<bool> {
        let mut connection = self.connection.lock().map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        let tx = connection.transaction()?;
        let conflicting: Option<(String, String, i64)> = tx.query_row(
            "SELECT target_id, owner_id, expires_at_ms FROM target_leases WHERE run_id=?1",
            params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        if let Some((existing_target, existing_owner, expires_at)) = conflicting
            && expires_at > now_ms
            && (existing_target != target_id || existing_owner != owner_id)
        {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute("DELETE FROM target_leases WHERE expires_at_ms<=?1 OR run_id=?2", params![now_ms, run_id])?;
        let inserted = tx.execute(
            r#"INSERT OR IGNORE INTO target_leases(
target_id,run_id,account_id,owner_id,state,lease_revision,expires_at_ms,updated_at_ms)
VALUES (?1,?2,?3,?4,'leased',1,?5,?6)"#,
            params![target_id, run_id, account_id, owner_id, now_ms + lease_duration_ms, now_ms],
        )?;
        tx.commit()?;
        Ok(inserted == 1)
    }

    fn renew_target(&self, target_id: &str, owner_id: &str, now_ms: i64, lease_duration_ms: i64) -> Result<bool> {
        let connection = self.connection.lock().map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        Ok(connection.execute(
            r#"UPDATE target_leases
SET lease_revision=lease_revision+1, expires_at_ms=?3, updated_at_ms=?2
WHERE target_id=?1 AND owner_id=?4 AND expires_at_ms>?2"#,
            params![target_id, now_ms, now_ms + lease_duration_ms, owner_id],
        )? == 1)
    }

    fn release_target(&self, target_id: &str, owner_id: &str) -> Result<()> {
        let connection = self.connection.lock().map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        connection.execute("DELETE FROM target_leases WHERE target_id=?1 AND owner_id=?2", params![target_id, owner_id])?;
        Ok(())
    }

    fn target_lease_for_run(&self, run_id: &str) -> Result<Option<DurableTargetLease>> {
        let connection = self.connection.lock().map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        connection.query_row(
            "SELECT target_id,run_id,account_id,owner_id,expires_at_ms FROM target_leases WHERE run_id=?1",
            params![run_id],
            |row| Ok(DurableTargetLease {
                target_id: row.get(0)?,
                run_id: row.get(1)?,
                account_id: row.get(2)?,
                owner_id: row.get(3)?,
                expires_at_ms: row.get(4)?,
            }),
        ).optional().map_err(Into::into)
    }

    fn record_browser_lifecycle(&self, account_id: &str, owner_id: &str, event_type: &str, details_json: &str, now_ms: i64) -> Result<()> {
        let connection = self.connection.lock().map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        connection.execute(
            "INSERT INTO browser_lifecycle_events(account_id,owner_id,event_type,details_json,created_at_ms) VALUES (?1,?2,?3,?4,?5)",
            params![account_id, owner_id, event_type, details_json, now_ms],
        )?;
        Ok(())
    }
}

pub struct SqliteRunJournal {
    store: SqliteStore,
    run_id: String,
    owner_id: String,
}

impl SqliteRunJournal {
    fn persist_event(&self, event: &RunEvent, checkpoint: Option<&RunCheckpoint>) -> Result<()> {
        let mut connection = self
            .store
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        let tx = connection.transaction()?;

        let lease_owner: Option<String> = tx
            .query_row(
                "SELECT owner_id FROM worker_leases WHERE run_id=?1",
                params![self.run_id],
                |row| row.get(0),
            )
            .optional()?;
        if lease_owner.as_deref() != Some(self.owner_id.as_str()) {
            bail!("run {} is not leased by {}", self.run_id, self.owner_id);
        }

        let body: String = tx.query_row(
            "SELECT body_json FROM runs WHERE run_id=?1",
            params![self.run_id],
            |row| row.get(0),
        )?;
        let mut run: RunRecord = serde_json::from_str(&body)?;
        run.revision += 1;
        run.state = event.state.clone();
        run.counters = event.counters.clone();
        if let Some(checkpoint) = checkpoint {
            run.checkpoint = checkpoint.clone();
        }
        if let Some(url) = &event.canonical_conversation_url {
            run.canonical_conversation_url = Some(url.clone());
        }
        if let Some(target_id) = &event.target_id {
            run.target_id = Some(target_id.clone());
        }
        if let Some(fingerprint) = &event.activity_fingerprint {
            run.last_activity_fingerprint = Some(fingerprint.clone());
        }
        if let Some(text) = &event.latest_assistant_text {
            run.latest_assistant_text = Some(text.clone());
        }
        if !event.visible_progress_messages.is_empty() {
            run.visible_progress_messages = event.visible_progress_messages.clone();
        }
        if matches!(
            event.kind,
            RunEventKind::RunCompleted | RunEventKind::RunFailed | RunEventKind::RunCancelled
        ) {
            run.finished_at_ms = Some(now_ms());
        }

        if let Some(next_phase) = event.queue_phase.as_ref() {
            let task_body: String = tx.query_row(
                "SELECT body_json FROM tasks WHERE task_id=?1",
                params![run.task_id],
                |row| row.get(0),
            )?;
            let mut task: QueueTask = serde_json::from_str(&task_body)?;
            let from_phase = task.phase.clone();
            task.transition_phase(next_phase.clone())?;
            if task.phase != from_phase {
                task.updated_at_ms = now_ms();
                tx.execute(
                    "UPDATE tasks SET body_json=?2, updated_at_ms=?3 WHERE task_id=?1",
                    params![task.id, serde_json::to_string(&task)?, task.updated_at_ms],
                )?;
                tx.execute(
                    r#"
INSERT INTO queue_events(task_id,run_id,from_phase,to_phase,reason,created_at_ms)
VALUES (?1,?2,?3,?4,?5,?6)
"#,
                    params![
                        task.id,
                        run.run_id,
                        phase_name(&from_phase),
                        phase_name(&task.phase),
                        serde_json::to_value(&event.kind)?
                            .as_str()
                            .unwrap_or("run_event"),
                        now_ms()
                    ],
                )?;
            }
        }

        tx.execute(
            r#"
INSERT INTO run_events(run_id,event_type,payload_json,created_at_ms)
VALUES (?1,?2,?3,?4)
"#,
            params![
                self.run_id,
                serde_json::to_value(&event.kind)?
                    .as_str()
                    .unwrap_or("unknown"),
                event.payload_json,
                now_ms()
            ],
        )?;
        tx.execute(
            "UPDATE runs SET state=?2, revision=?3, body_json=?4, finished_at_ms=?5 WHERE run_id=?1",
            params![
                self.run_id,
                SqliteStore::run_state_name(&run.state),
                run.revision,
                serde_json::to_string(&run)?,
                run.finished_at_ms
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

impl RunJournal for SqliteRunJournal {
    fn record(&self, event: &RunEvent) -> Result<()> {
        self.persist_event(event, None)
    }

    fn load_checkpoint(&self) -> Result<Option<RunCheckpoint>> {
        let connection = self
            .store
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        let body: Option<String> = connection
            .query_row(
                "SELECT body_json FROM runs WHERE run_id=?1",
                params![self.run_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(body) = body else {
            return Ok(None);
        };
        let run: RunRecord = serde_json::from_str(&body)?;
        Ok(Some(run.checkpoint))
    }

    fn record_with_checkpoint(&self, event: &RunEvent, checkpoint: &RunCheckpoint) -> Result<()> {
        self.persist_event(event, Some(checkpoint))
    }

    fn begin_approval_attempt(
        &self,
        fingerprint: &ApprovalFingerprint,
        max_attempts: u32,
    ) -> Result<bool> {
        let mut connection = self
            .store
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        let tx = connection.transaction()?;
        let state: Option<(u32, bool)> = tx
            .query_row(
                "SELECT attempts, settled FROM approval_fingerprints WHERE fingerprint=?1",
                params![fingerprint.0],
                |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .optional()?;
        if let Some((attempts, settled)) = state
            && (settled || attempts >= max_attempts)
        {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            r#"
INSERT INTO approval_fingerprints(fingerprint,run_id,attempts,settled,updated_at_ms)
VALUES (?1,?2,1,0,?3)
ON CONFLICT(fingerprint) DO UPDATE SET
    attempts=approval_fingerprints.attempts+1,
    run_id=excluded.run_id,
    updated_at_ms=excluded.updated_at_ms
"#,
            params![fingerprint.0, self.run_id, now_ms()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    fn settle_approval(&self, fingerprint: &ApprovalFingerprint) -> Result<()> {
        let connection = self
            .store
            .connection
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))?;
        connection.execute(
            "UPDATE approval_fingerprints SET settled=1, updated_at_ms=?2 WHERE fingerprint=?1",
            params![fingerprint.0, now_ms()],
        )?;
        Ok(())
    }
}

fn phase_name(phase: &QueuePhase) -> &'static str {
    match phase {
        QueuePhase::Queued => "queued",
        QueuePhase::Dispatched => "dispatched",
        QueuePhase::Submitting => "submitting",
        QueuePhase::Submitted => "submitted",
        QueuePhase::AwaitingAcknowledgement => "awaiting_acknowledgement",
        QueuePhase::AwaitingResponse => "awaiting_response",
        QueuePhase::Interrupted => "interrupted",
        QueuePhase::RateLimited => "rate_limited",
        QueuePhase::Recovering => "recovering",
        QueuePhase::Continuing => "continuing",
        QueuePhase::Completed => "completed",
        QueuePhase::FailedRetryable => "failed_retryable",
        QueuePhase::PermanentlyFailed => "permanently_failed",
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use fabushi_chatgpt_domain::{ExecutionProfile, RunCounters};

    fn task(id: &str) -> QueueTask {
        let mut task = QueueTask::new(id, "account-a", format!("prompt-{id}"));
        task.execution_profile = ExecutionProfile::default();
        task
    }

    #[test]
    fn sqlite_is_wal_and_event_state_transition_is_transactional() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(store.journal_mode().unwrap().to_ascii_lowercase(), "memory");

        let task = task("a");
        store.enqueue_task(&task).unwrap();
        let claim = store
            .claim_next_runnable("worker-1", 1000, 60_000)
            .unwrap()
            .unwrap();
        let journal = store.journal(&claim.run.run_id, "worker-1");
        let mut event = RunEvent::new(
            RunEventKind::PromptDispatchConfirmed,
            RunState::Running,
            RunCounters::default(),
        );
        event.latest_assistant_text = Some("progress".into());
        journal.record(&event).unwrap();

        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot.runs[0].revision, 2);
        assert_eq!(
            snapshot.runs[0].latest_assistant_text.as_deref(),
            Some("progress")
        );
    }

    #[test]
    fn dependency_and_resource_lock_gate_claims() {
        let store = SqliteStore::open_in_memory().unwrap();
        let a = task("a");
        let mut b = task("b");
        b.depends_on = vec!["a".into()];
        b.resource_locks = vec!["repo".into()];
        let mut c = task("c");
        c.resource_locks = vec!["repo".into()];
        c.priority = 10;
        store.enqueue_task(&a).unwrap();
        store.enqueue_task(&b).unwrap();
        store.enqueue_task(&c).unwrap();

        let first = store
            .claim_next_runnable("worker-1", 1000, 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(first.task.id, "c");
        let second = store
            .claim_next_runnable("worker-2", 1000, 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(second.task.id, "a");
    }

    #[test]
    fn expired_lease_requeues_with_recovery_envelope_and_progress() {
        let store = SqliteStore::open_in_memory().unwrap();
        let task = task("a");
        store.enqueue_task(&task).unwrap();
        let claim = store
            .claim_next_runnable("worker-1", 1000, 10)
            .unwrap()
            .unwrap();
        let journal = store.journal(&claim.run.run_id, "worker-1");
        let mut event = RunEvent::new(
            RunEventKind::SnapshotProgressed,
            RunState::Running,
            RunCounters::default(),
        );
        event.latest_assistant_text = Some("正在执行步骤 3".into());
        journal.record(&event).unwrap();

        assert_eq!(store.recover_expired_leases(1011).unwrap(), 1);
        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot.tasks[0].status, TaskState::Queued);
        let prompt = snapshot.tasks[0]
            .recovery_context
            .as_ref()
            .unwrap()
            .render_prompt();
        assert!(prompt.contains("正在执行步骤 3"));
    }

    #[test]
    fn approval_fingerprint_is_attempt_bounded_and_settled() {
        let store = SqliteStore::open_in_memory().unwrap();
        let task = task("a");
        store.enqueue_task(&task).unwrap();
        let claim = store
            .claim_next_runnable("worker-1", 1000, 60_000)
            .unwrap()
            .unwrap();
        let journal = store.journal(&claim.run.run_id, "worker-1");
        let fingerprint = ApprovalFingerprint::from_source("card");
        assert!(journal.begin_approval_attempt(&fingerprint, 3).unwrap());
        journal.settle_approval(&fingerprint).unwrap();
        assert!(!journal.begin_approval_attempt(&fingerprint, 3).unwrap());
    }

    #[test]
    fn revision_change_without_increment_is_rejected() {
        let store = SqliteStore::open_in_memory().unwrap();
        let mut original = task("a");
        original.created_at_ms = 1;
        original.updated_at_ms = 1;
        store.enqueue_task(&original).unwrap();
        let mut changed = original.clone();
        changed.prompt = "changed".into();
        assert!(store.enqueue_task(&changed).is_err());
    }
}
