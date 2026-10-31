use super::types::{CollectedExecution, ExecutionRecord, Relation, TokenUsage, UsageRecord};
use super::ActivityError;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;

pub(super) fn open(path: &Path) -> Result<Connection, ActivityError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS activity_executions (
           id TEXT PRIMARY KEY, client TEXT NOT NULL, session_id TEXT NOT NULL,
           turn_id TEXT NOT NULL, started_at TEXT, observed_at TEXT NOT NULL,
           payload TEXT NOT NULL, relation TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS activity_session_idx
           ON activity_executions(client, session_id, started_at);
         CREATE TABLE IF NOT EXISTS activity_requests (
           id TEXT PRIMARY KEY, execution_id TEXT NOT NULL, client TEXT NOT NULL,
           recorded_at TEXT NOT NULL, tokens TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS activity_request_execution_idx
           ON activity_requests(execution_id);
         CREATE TABLE IF NOT EXISTS activity_metadata (
           key TEXT PRIMARY KEY, payload TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS activity_sessions (
           client TEXT NOT NULL, session_id TEXT NOT NULL, visible INTEGER NOT NULL,
           PRIMARY KEY(client,session_id)
         );
         INSERT OR IGNORE INTO activity_metadata(key,payload) VALUES('usage_revision','0');
         CREATE TRIGGER IF NOT EXISTS activity_usage_insert AFTER INSERT ON activity_requests BEGIN
           UPDATE activity_metadata SET payload=CAST(payload AS INTEGER)+1 WHERE key='usage_revision';
         END;
         CREATE TRIGGER IF NOT EXISTS activity_usage_update AFTER UPDATE ON activity_requests BEGIN
           UPDATE activity_metadata SET payload=CAST(payload AS INTEGER)+1 WHERE key='usage_revision';
         END;
         CREATE TRIGGER IF NOT EXISTS activity_usage_delete AFTER DELETE ON activity_requests BEGIN
           UPDATE activity_metadata SET payload=CAST(payload AS INTEGER)+1 WHERE key='usage_revision';
         END;",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(connection)
}

pub(super) fn sync_sessions(
    db: &Connection,
    client: &str,
    sessions: &[(String, bool)],
) -> Result<(), ActivityError> {
    let current: std::collections::HashMap<_, _> = sessions.iter().cloned().collect();
    let mut statement =
        db.prepare("SELECT DISTINCT session_id FROM activity_executions WHERE client=?1")?;
    let known = statement
        .query_map([client], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut update = db.prepare_cached(
        "INSERT INTO activity_sessions(client,session_id,visible) VALUES(?1,?2,?3)
         ON CONFLICT(client,session_id) DO UPDATE SET visible=excluded.visible
         WHERE activity_sessions.visible IS NOT excluded.visible",
    )?;
    for session in known.iter().chain(current.keys()) {
        update.execute(params![
            client,
            session,
            current.get(session).copied().unwrap_or(false)
        ])?;
    }
    Ok(())
}

pub(super) fn hidden_sessions(
    db: &Connection,
) -> Result<std::collections::HashSet<(String, String)>, ActivityError> {
    let mut statement =
        db.prepare("SELECT client,session_id FROM activity_sessions WHERE visible=0")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(super) fn metadata<T: DeserializeOwned>(
    db: &Connection,
    key: &str,
) -> Result<Option<T>, ActivityError> {
    let value: Option<String> = db
        .query_row(
            "SELECT payload FROM activity_metadata WHERE key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|text| serde_json::from_str(&text).map_err(ActivityError::from))
        .transpose()
}

pub(super) fn save_metadata<T: Serialize>(
    db: &Connection,
    key: &str,
    value: &T,
) -> Result<(), ActivityError> {
    let payload = serde_json::to_string(value)?;
    db.execute(
        "INSERT INTO activity_metadata(key,payload) VALUES (?1,?2)
         ON CONFLICT(key) DO UPDATE SET payload=excluded.payload
         WHERE activity_metadata.payload IS NOT excluded.payload",
        params![key, payload],
    )?;
    Ok(())
}

pub(super) fn upsert(db: &Connection, execution: &CollectedExecution) -> Result<(), ActivityError> {
    let mut record = execution.record.clone();
    if super::terminal(&record.status) {
        let expired: bool = db
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM activity_metadata
             WHERE key='history_cutoff' AND json_extract(payload,'$') > ?1)",
            )?
            .query_row(
                [record
                    .completed_at
                    .as_deref()
                    .unwrap_or(&record.observed_at)],
                |row| row.get(0),
            )?;
        if expired {
            return Ok(());
        }
    }
    let mut relation = execution.relation.clone();
    let previous_relation: Option<String> = db
        .query_row(
            "SELECT relation FROM activity_executions WHERE id=?1",
            [&record.id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(previous) = previous_relation {
        let previous: Relation = serde_json::from_str(&previous)?;
        if relation.parent_session.is_none() {
            relation.parent_session = previous.parent_session;
        }
        if relation.parent_native_turn.is_none() {
            relation.parent_native_turn = previous.parent_native_turn;
        }
        if relation.root_native_turn.is_none() {
            relation.root_native_turn = previous.root_native_turn;
        }
        if relation.root_prompt_id.is_none() {
            relation.root_prompt_id = previous.root_prompt_id;
        }
    }
    let saved_index: Option<u64> = db.query_row(
        "SELECT CAST(json_extract(payload,'$.turnIndex') AS INTEGER) FROM activity_executions WHERE id=?1",
        [&record.id], |row| row.get(0),
    ).optional()?;
    if let Some(index) = saved_index.filter(|index| *index > 0) {
        record.turn_index = index;
    } else if record.client != "claude" {
        let index: u64 = db.query_row(
            "SELECT COALESCE(MAX(CAST(json_extract(payload,'$.turnIndex') AS INTEGER)),0)
             FROM activity_executions WHERE client=?1 AND session_id=?2",
            params![record.client, record.session_id],
            |row| row.get(0),
        )?;
        record.turn_index = index + 1;
    }
    let payload = serde_json::to_string(&record)?;
    let relation = serde_json::to_string(&relation)?;
    db.execute(
        "INSERT INTO activity_executions(id,client,session_id,turn_id,started_at,observed_at,payload,relation)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(id) DO UPDATE SET started_at=excluded.started_at,
           observed_at=excluded.observed_at,payload=excluded.payload,relation=excluded.relation
         WHERE activity_executions.started_at IS NOT excluded.started_at
            OR activity_executions.observed_at IS NOT excluded.observed_at
            OR activity_executions.payload IS NOT excluded.payload
            OR activity_executions.relation IS NOT excluded.relation",
        params![
            record.id,
            record.client,
            record.session_id,
            record.turn_id,
            record.started_at,
            record.observed_at,
            payload,
            relation,
        ],
    )?;
    Ok(())
}

pub(super) fn usage(db: &Connection, usage: &UsageRecord) -> Result<(), ActivityError> {
    db.prepare_cached(
        "INSERT INTO activity_requests(id,execution_id,client,recorded_at,tokens)
         SELECT ?1,?2,?3,?4,?5
         WHERE NOT EXISTS (
           SELECT 1 FROM activity_metadata WHERE key='history_cutoff'
             AND json_extract(payload,'$') > ?4
             AND NOT EXISTS (SELECT 1 FROM activity_executions WHERE id=?2)
         )
         ON CONFLICT(id) DO UPDATE SET tokens=excluded.tokens
         WHERE activity_requests.tokens IS NOT excluded.tokens",
    )?
    .execute(params![
        usage.id,
        usage.execution_id,
        usage.client,
        usage.recorded_at,
        serde_json::to_string(&usage.tokens)?,
    ])?;
    Ok(())
}

pub(super) fn records(db: &Connection) -> Result<Vec<CollectedExecution>, ActivityError> {
    let mut statement = db.prepare(
        "SELECT payload,relation FROM activity_executions
         ORDER BY COALESCE(started_at, observed_at) DESC,id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (payload, relation) = row?;
        Ok(CollectedExecution {
            record: serde_json::from_str::<ExecutionRecord>(&payload)?,
            relation: serde_json::from_str::<Relation>(&relation)?,
        })
    })
    .collect()
}

pub(super) fn usages(db: &Connection) -> Result<Vec<UsageRecord>, ActivityError> {
    let mut statement =
        db.prepare("SELECT id,execution_id,client,recorded_at,tokens FROM activity_requests")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (id, execution_id, client, recorded_at, tokens) = row?;
        Ok(UsageRecord {
            id,
            execution_id,
            client,
            recorded_at,
            tokens: serde_json::from_str::<TokenUsage>(&tokens)?,
        })
    })
    .collect()
}
