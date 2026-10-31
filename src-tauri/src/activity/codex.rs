use super::common::{jsonl, read_only, timestamp, utc};
use super::types::{
    execution_id, CollectedExecution, ExecutionRecord, Relation, TokenUsage, UsageRecord,
};
use super::{store, ActivityError};
use crate::model::TaskStatus;
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

struct Conversation {
    title: String,
    path: String,
    rollout: PathBuf,
    provider: Option<String>,
    model: Option<String>,
    parent: Option<String>,
}

pub(super) fn collect(
    db: &Connection,
    home: &Path,
    cutoff: i64,
    now: &str,
) -> Result<usize, ActivityError> {
    let history = read_only(&home.join(".codex/thread_history_1.sqlite"))?;
    let state = read_only(&home.join(".codex/state_5.sqlite"))?;
    let mut visibility = state.prepare("SELECT id,archived=0 FROM threads")?;
    let sessions = visibility
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<(String, bool)>, _>>()?;
    store::sync_sessions(db, "codex", &sessions)?;
    let mut statement = state.prepare(
        "SELECT id,title,cwd,rollout_path,model_provider,model,source
         FROM threads WHERE updated_at >= ?1",
    )?;
    let rows = statement.query_map([cutoff], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    let mut conversations = HashMap::new();
    let mut run_conversations = HashMap::new();
    for row in rows {
        let (id, title, path, rollout, provider, model, source) = row?;
        if let Some((conversation, Some(run))) =
            crate::codex::rollout_session_ids(Path::new(&rollout))
        {
            if conversation == id {
                run_conversations.insert(run, id.clone());
            }
        }
        let source = serde_json::from_str::<Value>(&source).ok();
        let parent = source
            .as_ref()
            .and_then(|value| value.pointer("/subagent/thread_spawn/parent_thread_id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        conversations.insert(
            id,
            Conversation {
                title,
                path,
                rollout: PathBuf::from(rollout),
                provider,
                model,
                parent,
            },
        );
    }
    let mut statement = history.prepare(
        "SELECT thread_id,turn_id,status,started_at,completed_at,duration_ms,
                root_turn_id
         FROM thread_turns WHERE started_at >= ?1 OR completed_at >= ?1
         ORDER BY thread_id,rollout_ordinal",
    )?;
    let rows = statement.query_map([cutoff], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<u64>>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;
    let mut sessions = HashSet::new();
    let mut indexes = HashMap::<String, u64>::new();
    for row in rows {
        let (session, turn, status, start, end, duration_ms, root) = row?;
        let session = run_conversations.get(&session).cloned().unwrap_or(session);
        let Some(conversation) = conversations.get(&session) else {
            continue;
        };
        let index = indexes.entry(session.clone()).or_default();
        *index += 1;
        let started_at = start.and_then(utc);
        let completed_at = end.and_then(utc);
        let status = match status.as_str() {
            "completed" => TaskStatus::Succeeded,
            "failed" => TaskStatus::Failed,
            "interrupted" => TaskStatus::Interrupted,
            _ => TaskStatus::Unknown,
        };
        store::upsert(
            db,
            &CollectedExecution {
                record: ExecutionRecord {
                    id: execution_id("codex", &session, &turn),
                    client: "codex".into(),
                    session_id: session.clone(),
                    turn_id: turn,
                    root_id: None,
                    parent_id: None,
                    is_child: conversation.parent.is_some(),
                    title: conversation.title.clone(),
                    project_path: conversation.path.clone(),
                    model: conversation.model.clone(),
                    provider: conversation.provider.clone(),
                    turn_index: *index,
                    status,
                    attention: None,
                    started_at: started_at.clone(),
                    completed_at: completed_at.clone(),
                    observed_at: completed_at.or(started_at).unwrap_or_else(|| now.into()),
                    duration_ms,
                    active_ms: None,
                    waiting_ms: None,
                    tokens: TokenUsage::default(),
                    live: false,
                    child_count: 0,
                },
                relation: Relation {
                    parent_session: conversation.parent.clone(),
                    root_native_turn: root,
                    parent_native_turn: None,
                    ..Relation::default()
                },
            },
        )?;
        sessions.insert(session);
    }
    let mut invalid = 0;
    for session in sessions {
        let Some(conversation) = conversations.get(&session) else {
            continue;
        };
        if !conversation.rollout.is_file() {
            continue;
        }
        invalid += jsonl::<(), _>(db, &conversation.rollout, 0, |db, value, _| {
            if value.get("type").and_then(Value::as_str) != Some("token_usage_record") {
                return Ok(());
            }
            let payload = &value["payload"];
            if payload.get("thread_id").and_then(Value::as_str) != Some(session.as_str()) {
                return Ok(());
            }
            let Some(turn) = payload.get("turn_id").and_then(Value::as_str) else {
                return Ok(());
            };
            let Some(response) = payload.get("response_id").and_then(Value::as_str) else {
                return Ok(());
            };
            let Some(recorded_at) = value.get("timestamp").and_then(timestamp) else {
                return Ok(());
            };
            let usage = &payload["usage"];
            store::usage(
                db,
                &UsageRecord {
                    id: format!("codex:{session}:{response}"),
                    execution_id: execution_id("codex", &session, turn),
                    client: "codex".into(),
                    recorded_at,
                    tokens: TokenUsage {
                        input: usage.get("input_tokens").and_then(Value::as_u64),
                        output: usage.get("output_tokens").and_then(Value::as_u64),
                        cache_read: usage.get("cached_input_tokens").and_then(Value::as_u64),
                        cache_write: usage
                            .get("cache_write_input_tokens")
                            .and_then(Value::as_u64),
                        reasoning: usage.get("reasoning_output_tokens").and_then(Value::as_u64),
                        total: usage.get("total_tokens").and_then(Value::as_u64),
                        input_includes_cache: true,
                        output_includes_reasoning: true,
                    },
                },
            )?;
            // Only mutate the execution named by this record, never inherited parent history.
            let key = execution_id("codex", &session, turn);
            if let Some(root) = payload.get("root_turn_id").and_then(Value::as_str) {
                let relation = Relation {
                    parent_session: conversation.parent.clone(),
                    root_native_turn: Some(root.into()),
                    parent_native_turn: None,
                    ..Relation::default()
                };
                db.execute(
                    "UPDATE activity_executions SET relation=?2 WHERE id=?1",
                    params![key, serde_json::to_string(&relation)?],
                )?;
            }
            Ok(())
        })?;
    }
    Ok(invalid)
}
