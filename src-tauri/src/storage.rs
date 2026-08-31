use crate::model::{Settings, TaskSnapshot};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("filesystem error: {0}")]
    Filesystem(#[from] std::io::Error),
}

pub struct SnapshotStore {
    connection: Mutex<Connection>,
    path: PathBuf,
}

impl SnapshotStore {
    pub fn open_default() -> Result<Self, StorageError> {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let path = home.join("Library/Application Support/Agent Monitor/monitor.db");
        Self::open(path)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = fs::metadata(parent)?.permissions();
                permissions.set_mode(0o700);
                fs::set_permissions(parent, permissions)?;
            }
        }
        let connection = Connection::open(&path)?;
        let store = Self {
            connection: Mutex::new(connection),
            path,
        };
        store.migrate()?;
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&store.path)?.permissions();
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o600);
            fs::set_permissions(&store.path, permissions)?;
        }
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        let connection = Connection::open_in_memory()?;
        let store = Self {
            connection: Mutex::new(connection),
            path: PathBuf::from(":memory:"),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), StorageError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS tasks (
               task_id TEXT PRIMARY KEY,
               payload TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS settings (
               id INTEGER PRIMARY KEY CHECK (id = 1),
               payload TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS notifications (
               notification_key TEXT PRIMARY KEY,
               created_at TEXT NOT NULL
             );",
        )?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load_tasks(&self) -> Result<Vec<TaskSnapshot>, StorageError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut statement =
            connection.prepare("SELECT payload FROM tasks ORDER BY updated_at DESC")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut tasks = Vec::new();
        for row in rows {
            let payload = row?;
            if let Ok(task) = serde_json::from_str::<TaskSnapshot>(&payload) {
                tasks.push(task);
            }
        }
        Ok(tasks)
    }

    pub fn save_tasks(&self, tasks: &[TaskSnapshot]) -> Result<(), StorageError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM tasks", [])?;
        for task in tasks {
            let payload = serde_json::to_string(task)?;
            transaction.execute(
                "INSERT INTO tasks(task_id, payload, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(task_id) DO UPDATE SET payload = excluded.payload, updated_at = excluded.updated_at",
                params![task.task_id, payload, task.updated_at],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn load_settings(&self) -> Result<Settings, StorageError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let payload: Option<String> = connection
            .query_row("SELECT payload FROM settings WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        payload
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map(|value| value.unwrap_or_default())
            .map_err(StorageError::from)
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<(), StorageError> {
        let payload = serde_json::to_string(settings)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        connection.execute(
            "INSERT INTO settings(id, payload) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET payload = excluded.payload",
            params![payload],
        )?;
        Ok(())
    }

    pub fn mark_notification_once(
        &self,
        key: &str,
        created_at: &str,
    ) -> Result<bool, StorageError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let inserted = connection.execute(
            "INSERT OR IGNORE INTO notifications(notification_key, created_at) VALUES (?1, ?2)",
            params![key, created_at],
        )?;
        Ok(inserted == 1)
    }

    pub fn clear(&self) -> Result<(), StorageError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        connection.execute_batch("DELETE FROM tasks; DELETE FROM notifications;")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProgressBasis, ProgressConfidence, TaskStatus};

    fn task() -> TaskSnapshot {
        TaskSnapshot {
            provider: "codex".into(),
            task_id: "t-1".into(),
            turn_id: Some("turn-1".into()),
            parent_task_id: None,
            title: "A task".into(),
            project_path: "/tmp/project".into(),
            source: "cli".into(),
            status: TaskStatus::Running,
            attention: None,
            progress_percent: Some(50),
            progress_basis: ProgressBasis::Plan,
            progress_confidence: ProgressConfidence::High,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:01Z".into(),
            completed_at: None,
            archived: false,
            last_error: None,
            plan_total_steps: 2,
            plan_completed_steps: 1,
            plan_in_progress_steps: 0,
        }
    }

    #[test]
    fn persists_snapshots_settings_and_notification_keys() {
        let store = SnapshotStore::open_in_memory().unwrap();
        store.save_tasks(&[task()]).unwrap();
        assert_eq!(store.load_tasks().unwrap()[0].task_id, "t-1");
        let mut replacement = task();
        replacement.task_id = "t-2".into();
        store.save_tasks(&[replacement]).unwrap();
        let tasks = store.load_tasks().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "t-2");
        assert!(store.mark_notification_once("k", "now").unwrap());
        assert!(!store.mark_notification_once("k", "later").unwrap());
        let settings = Settings::default();
        store.save_settings(&settings).unwrap();
        assert_eq!(store.load_settings().unwrap(), settings);
        store.clear().unwrap();
        assert!(store.load_tasks().unwrap().is_empty());
    }
}
