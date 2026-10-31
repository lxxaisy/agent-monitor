use super::common::{read_only, sqlite_version, utc_ms, FileVersion};
use super::types::{
    execution_id, CollectedExecution, ExecutionRecord, Relation, TokenUsage, UsageRecord,
};
use super::{store, ActivityError, ActivityEvent};
use crate::model::TaskStatus;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Serialize, Deserialize)]
struct Conversation {
    title: String,
    path: String,
    parent: Option<String>,
    parent_turn: Option<String>,
}

fn pending_execution(
    db: &Connection,
    conversation: &Conversation,
    session: &str,
    turn: &str,
    observed_at: &str,
) -> Result<CollectedExecution, ActivityError> {
    let id = execution_id("zcode", session, turn);
    Ok(CollectedExecution {
        record: ExecutionRecord {
            started_at: store::metadata(db, &format!("native_start:{id}"))?,
            id,
            client: "zcode".into(),
            session_id: session.into(),
            turn_id: turn.into(),
            root_id: None,
            parent_id: None,
            is_child: conversation.parent.is_some(),
            title: conversation.title.clone(),
            project_path: conversation.path.clone(),
            model: None,
            provider: None,
            turn_index: 0,
            status: TaskStatus::Unknown,
            attention: None,
            completed_at: None,
            observed_at: observed_at.into(),
            duration_ms: None,
            active_ms: None,
            waiting_ms: None,
            tokens: TokenUsage::default(),
            live: false,
            child_count: 0,
        },
        relation: Relation {
            parent_session: conversation.parent.clone(),
            parent_native_turn: conversation.parent_turn.clone(),
            ..Relation::default()
        },
    })
}

pub(super) fn pending_from_hook(
    db: &Connection,
    event: &ActivityEvent,
) -> Result<Option<CollectedExecution>, ActivityError> {
    let Some(turn) = &event.turn_id else {
        return Ok(None);
    };
    let id = execution_id("zcode", &event.session_id, turn);
    if let Some(timestamp) = &event.native_timestamp {
        store::save_metadata(db, &format!("native_start:{id}"), timestamp)?;
        db.execute("UPDATE activity_executions SET started_at=?2,payload=json_set(payload,'$.startedAt',?2) WHERE id=?1 AND started_at IS NULL", rusqlite::params![id, timestamp])?;
    }
    let Some(conversation) =
        store::metadata::<Conversation>(db, &format!("session:zcode:{}", event.session_id))?
    else {
        return Ok(None);
    };
    pending_execution(
        db,
        &conversation,
        &event.session_id,
        turn,
        &event.observed_at,
    )
    .map(Some)
}

pub(super) fn observe_current(
    db: &Connection,
    session: &str,
    id: &str,
    at: &str,
) -> Result<(), ActivityError> {
    let time_key = format!("current_time:zcode:{session}");
    if store::metadata::<String>(db, &time_key)?
        .as_deref()
        .map_or(true, |previous| at >= previous)
    {
        store::save_metadata(db, &format!("current:zcode:{session}"), &id)?;
        store::save_metadata(db, &time_key, &at)?;
    }
    Ok(())
}

fn status(value: &str) -> TaskStatus {
    match value {
        "completed" => TaskStatus::Succeeded,
        "error" => TaskStatus::Failed,
        "cancelled" => TaskStatus::Interrupted,
        _ => TaskStatus::Unknown,
    }
}

pub(super) fn collect(
    db: &Connection,
    home: &Path,
    cutoff_ms: i64,
) -> Result<usize, ActivityError> {
    let path = home.join(".zcode/cli/db/db.sqlite");
    let version = sqlite_version(&path)?;
    if store::metadata::<Vec<Option<FileVersion>>>(db, "source_version:zcode:visibility")?.as_ref()
        == Some(&version)
    {
        return Ok(0);
    }
    let source = read_only(&path)?;
    let mut visibility =
        source.prepare("SELECT id,time_archived IS NULL AND time_deleted IS NULL FROM session")?;
    let sessions = visibility
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<(String, bool)>, _>>()?;
    store::sync_sessions(db, "zcode", &sessions)?;
    let mut statement = source.prepare("SELECT id,title,directory,parent_id FROM session")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut conversations = HashMap::new();
    for row in rows {
        let (id, title, path, parent) = row?;
        conversations.insert(
            id,
            Conversation {
                title,
                path,
                parent,
                parent_turn: None,
            },
        );
    }
    let mut links = source.prepare(
        "SELECT child_session_id,parent_session_id FROM session_task_link
         WHERE parent_session_id IS NOT NULL",
    )?;
    let rows = links.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (child, parent) = row?;
        if let Some(conversation) = conversations.get_mut(&child) {
            conversation.parent = Some(parent);
        }
    }
    let mut workflow_links = source.prepare(
        "SELECT a.session_id,r.parent_session_id,MIN(t.turn_id)
         FROM dwf_actor a JOIN dwf_run r ON r.id=a.run_id
         JOIN tool_usage t ON t.session_id=r.parent_session_id AND t.tool_call_id=r.tool_call_id
         WHERE a.session_id IS NOT NULL
         GROUP BY a.session_id,r.parent_session_id HAVING COUNT(DISTINCT t.turn_id)=1",
    )?;
    let rows = workflow_links.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (child, parent, turn) = row?;
        if let Some(conversation) = conversations.get_mut(&child) {
            conversation.parent = Some(parent);
            conversation.parent_turn = Some(turn);
        }
    }
    for (session, conversation) in &conversations {
        store::save_metadata(db, &format!("session:zcode:{session}"), conversation)?;
    }
    let mut statement = source.prepare(
        "SELECT session_id,turn_id,status,started_at,completed_at,duration_ms,
                input_tokens,output_tokens,reasoning_tokens,cache_creation_input_tokens,
                cache_read_input_tokens,computed_total_tokens
         FROM turn_usage WHERE started_at >= ?1 OR completed_at >= ?1
         ORDER BY started_at,session_id,turn_id",
    )?;
    let rows = statement.query_map([cutoff_ms], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<u64>>(5)?,
            TokenUsage {
                input: row.get(6)?,
                output: row.get(7)?,
                reasoning: row.get(8)?,
                cache_write: row.get(9)?,
                cache_read: row.get(10)?,
                total: row.get(11)?,
                input_includes_cache: true,
                output_includes_reasoning: true,
            },
        ))
    })?;
    let mut indexes = HashMap::<String, u64>::new();
    let mut known = HashSet::new();
    for row in rows {
        let (session, turn, raw_status, start, end, duration_ms, tokens) = row?;
        let Some(conversation) = conversations.get(&session) else {
            continue;
        };
        let Some(started_at) = utc_ms(start) else {
            continue;
        };
        let completed_at = end.and_then(utc_ms);
        let index = indexes.entry(session.clone()).or_default();
        *index += 1;
        let id = execution_id("zcode", &session, &turn);
        known.insert(id.clone());
        let current_id = id.clone();
        let native_session = session.clone();
        let current_at = started_at.clone();
        store::upsert(
            db,
            &CollectedExecution {
                record: ExecutionRecord {
                    id,
                    client: "zcode".into(),
                    session_id: session,
                    turn_id: turn,
                    root_id: None,
                    parent_id: None,
                    is_child: conversation.parent.is_some(),
                    title: conversation.title.clone(),
                    project_path: conversation.path.clone(),
                    model: None,
                    provider: None,
                    turn_index: *index,
                    status: status(&raw_status),
                    attention: None,
                    started_at: Some(started_at.clone()),
                    completed_at: completed_at.clone(),
                    observed_at: completed_at.unwrap_or(started_at),
                    duration_ms,
                    active_ms: None,
                    waiting_ms: None,
                    tokens,
                    live: false,
                    child_count: 0,
                },
                relation: Relation {
                    parent_session: conversation.parent.clone(),
                    parent_native_turn: conversation.parent_turn.clone(),
                    ..Relation::default()
                },
            },
        )?;
        observe_current(db, &native_session, &current_id, &current_at)?;
    }
    let mut statement = source.prepare(
        "SELECT id,session_id,turn_id,provider_id,model_id,started_at,completed_at,
                input_tokens,output_tokens,reasoning_tokens,cache_creation_input_tokens,
                cache_read_input_tokens,computed_total_tokens
         FROM model_usage WHERE started_at >= ?1 AND turn_id IS NOT NULL
         ORDER BY started_at,id",
    )?;
    let rows = statement.query_map([cutoff_ms], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<i64>>(6)?,
            TokenUsage {
                input: row.get(7)?,
                output: row.get(8)?,
                reasoning: row.get(9)?,
                cache_write: row.get(10)?,
                cache_read: row.get(11)?,
                total: row.get(12)?,
                input_includes_cache: true,
                output_includes_reasoning: true,
            },
        ))
    })?;
    let mut models_by_turn = HashMap::new();
    for row in rows {
        let (request, session, turn, provider, model, start, _end, tokens) = row?;
        let id = execution_id("zcode", &session, &turn);
        let Some(recorded_at) = utc_ms(start) else {
            continue;
        };
        if !known.contains(&id) {
            if let Some(conversation) = conversations.get(&session) {
                store::upsert(
                    db,
                    &pending_execution(db, conversation, &session, &turn, &recorded_at)?,
                )?;
                known.insert(id.clone());
            }
        }
        store::usage(
            db,
            &UsageRecord {
                id: format!("zcode:{request}"),
                execution_id: id.clone(),
                client: "zcode".into(),
                recorded_at,
                tokens,
            },
        )?;
        // Preserve the existing ordered last-request rule, but write it only
        // once per turn rather than for every historical request in that turn.
        models_by_turn.insert(id, (model, provider));
    }
    for (id, (model, provider)) in models_by_turn {
        db.prepare_cached(
            "UPDATE activity_executions SET payload=json_set(payload,'$.model',?2,'$.provider',?3)
             WHERE id=?1 AND (json_extract(payload,'$.model') IS NOT ?2
                          OR json_extract(payload,'$.provider') IS NOT ?3)",
        )?
        .execute(rusqlite::params![id, model, provider])?;
    }
    store::save_metadata(db, "source_version:zcode:visibility", &version)?;
    Ok(0)
}
