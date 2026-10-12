use anyhow::{Context, Result};
use fabushi_chatgpt_application::TaskStore;
use fabushi_chatgpt_domain::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

pub struct SqliteTaskStore {
    connection: Mutex<Connection>,
}
impl SqliteTaskStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let created = !parent.exists();
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if created {
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
                }
            }
        }
        let c = Connection::open(path)?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY, revision INTEGER NOT NULL, created_at INTEGER NOT NULL, deleted INTEGER NOT NULL DEFAULT 0, body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS events(sequence INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT NOT NULL,at INTEGER NOT NULL,role TEXT NOT NULL,text TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS events_task ON events(task_id,at);
          CREATE TABLE IF NOT EXISTS settings(id INTEGER PRIMARY KEY CHECK(id=1),revision INTEGER NOT NULL,body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS leases(endpoint TEXT PRIMARY KEY,owner TEXT NOT NULL,expires_at INTEGER NOT NULL);")?;
        c.execute(
            "INSERT OR IGNORE INTO settings(id,revision,body) VALUES(1,0,?1)",
            [serde_json::to_string(&Settings::default())?],
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self {
            connection: Mutex::new(c),
        })
    }
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite lock poisoned"))
    }
    pub fn restore(&self, id: &str) -> Result<()> {
        self.lock()?
            .execute("UPDATE tasks SET deleted=0 WHERE id=?1", [id])?;
        Ok(())
    }
}
impl TaskStore for SqliteTaskStore {
    fn list(&self) -> Result<Vec<Task>> {
        let c = self.lock()?;
        let mut stmt =
            c.prepare("SELECT body FROM tasks WHERE deleted=0 ORDER BY created_at,id")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| serde_json::from_str(&r?).context("invalid durable task"))
            .collect()
    }
    fn insert(&self, t: &Task) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO tasks(id,revision,created_at,body) VALUES(?1,?2,?3,?4)",
            params![t.id, t.revision, t.created_at, serde_json::to_string(t)?],
        )?;
        Ok(())
    }
    fn save(&self, t: &mut Task, expected: u64, activity: Option<Activity>) -> Result<bool> {
        let mut c = self.lock()?;
        let tx = c.transaction()?;
        let mut saved = t.clone();
        saved.revision = expected + 1;
        let changed = tx.execute(
            "UPDATE tasks SET revision=?1,body=?2 WHERE id=?3 AND revision=?4 AND deleted=0",
            params![
                saved.revision,
                serde_json::to_string(&saved)?,
                saved.id,
                expected
            ],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        if let Some(a) = activity {
            tx.execute(
                "INSERT INTO events(task_id,at,role,text) VALUES(?1,?2,?3,?4)",
                params![a.task_id, a.at, a.role, bounded(&a.text, 12_000)],
            )?;
        }
        tx.commit()?;
        t.revision = saved.revision;
        Ok(true)
    }
    fn delete(&self, id: &str) -> Result<()> {
        self.lock()?.execute(
            "UPDATE tasks SET deleted=1,revision=revision+1 WHERE id=?1",
            [id],
        )?;
        Ok(())
    }
    fn activity(&self, id: &str, since: u64, limit: usize) -> Result<Vec<Activity>> {
        let c = self.lock()?;
        let mut stmt=c.prepare("SELECT task_id,at,role,text FROM events WHERE (task_id=?1 OR ?1='') AND at>=?2 ORDER BY sequence DESC LIMIT ?3")?;
        let mut rows = stmt
            .query_map(params![id, since, limit.min(360)], |r| {
                Ok(Activity {
                    task_id: r.get(0)?,
                    at: r.get(1)?,
                    role: r.get(2)?,
                    text: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }
    fn operations(&self, id: &str, since: u64, limit: usize) -> Result<Vec<Activity>> {
        let c = self.lock()?;
        // Filter before LIMIT so high-frequency scans cannot evict actions.
        let mut stmt=c.prepare("SELECT task_id,at,role,text FROM events WHERE (task_id=?1 OR ?1='') AND at>=?2 AND role NOT IN ('scan','assistant') ORDER BY sequence DESC LIMIT ?3")?;
        let mut rows = stmt
            .query_map(params![id, since, limit.min(50_000)], |r| {
                Ok(Activity {
                    task_id: r.get(0)?,
                    at: r.get(1)?,
                    role: r.get(2)?,
                    text: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }
    fn settings(&self) -> Result<Settings> {
        let body: String =
            self.lock()?
                .query_row("SELECT body FROM settings WHERE id=1", [], |r| r.get(0))?;
        Ok(serde_json::from_str(&body)?)
    }
    fn save_settings(&self, s: &mut Settings) -> Result<bool> {
        let mut saved = s.clone();
        saved.revision += 1;
        let changed = self.lock()?.execute(
            "UPDATE settings SET revision=?1,body=?2 WHERE id=1 AND revision=?3",
            params![saved.revision, serde_json::to_string(&saved)?, s.revision],
        )?;
        if changed > 0 {
            s.revision = saved.revision;
        }
        Ok(changed > 0)
    }
    fn lease(&self, endpoint: &str, owner: &str, now: u64, ttl: u64) -> Result<bool> {
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: Option<(String, u64)> = tx
            .query_row(
                "SELECT owner,expires_at FROM leases WHERE endpoint=?1",
                [endpoint],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if current.is_some_and(|(existing, expires)| existing != owner && expires > now) {
            return Ok(false);
        }
        tx.execute("INSERT INTO leases(endpoint,owner,expires_at) VALUES(?1,?2,?3) ON CONFLICT(endpoint) DO UPDATE SET owner=excluded.owner,expires_at=excluded.expires_at",params![endpoint,owner,now+ttl])?;
        tx.commit()?;
        Ok(true)
    }
    fn release(&self, endpoint: &str, owner: &str) -> Result<()> {
        self.lock()?.execute(
            "DELETE FROM leases WHERE endpoint=?1 AND owner=?2",
            params![endpoint, owner],
        )?;
        Ok(())
    }
    fn heartbeat(&self, endpoint: &str) -> Result<Option<(String, u64)>> {
        Ok(self
            .lock()?
            .query_row(
                "SELECT owner,expires_at FROM leases WHERE endpoint=?1",
                [endpoint],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> SqliteTaskStore {
        let path = std::env::temp_dir()
            .join(format!("fabushi-store-{}", std::process::id()))
            .join("state.sqlite");
        let _ = std::fs::remove_file(&path);
        SqliteTaskStore::open(&path).unwrap()
    }
    #[test]
    fn operation_history_survives_scan_flood_and_honors_hour_window() {
        let root = std::env::temp_dir().join(format!("fabushi-operations-{}", std::process::id()));
        let s = SqliteTaskStore::open(&root.join("state.sqlite")).unwrap();
        let mut t = Task::new(
            "history".into(),
            "history".into(),
            "goal".into(),
            TaskMode::Goal,
            1,
        );
        s.insert(&t).unwrap();
        let event = |at, role: &str, text: &str| {
            Some(Activity {
                task_id: t.id.clone(),
                at,
                role: role.into(),
                text: text.into(),
            })
        };
        let first = event(1_000, "action", "older than one hour");
        let recent = event(4_000_000, "action", "clicked stay in chat");
        s.save(&mut t, 0, first).unwrap();
        s.save(&mut t, 1, recent).unwrap();
        for n in 0..500 {
            let rev = t.revision;
            let a = Activity {
                task_id: t.id.clone(),
                at: 4_000_100 + n,
                role: "scan".into(),
                text: "poll".into(),
            };
            s.save(&mut t, rev, Some(a)).unwrap();
        }
        let actions = s.operations("history", 3_600_000, 360).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].text, "clicked stay in chat");
    }
    #[test]
    fn stale_worker_cannot_overwrite_manual_pause_and_only_one_owner_leases() {
        let s = store();
        let t = Task::new(
            "test".into(),
            "title".into(),
            "goal".into(),
            TaskMode::Goal,
            1,
        );
        s.insert(&t).unwrap();
        let mut paused = t.clone();
        paused.pause();
        assert!(s.save(&mut paused, 0, None).unwrap());
        let mut stale = t;
        stale.state = TaskState::Done;
        assert!(!s.save(&mut stale, 0, None).unwrap());
        assert_eq!(s.list().unwrap()[0].state, TaskState::Paused);
        assert!(s.lease("desktop", "a", 100, 100).unwrap());
        assert!(!s.lease("desktop", "b", 101, 100).unwrap());
        assert!(s.lease("desktop", "b", 201, 100).unwrap());
        s.delete("test").unwrap();
        assert!(s.list().unwrap().is_empty());
        s.restore("test").unwrap();
        assert_eq!(s.list().unwrap().len(), 1);
    }
}
