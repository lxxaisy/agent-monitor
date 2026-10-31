use super::common::{duration, jsonl, jsonl_files, timestamp};
use super::types::{
    execution_id, CollectedExecution, ExecutionRecord, Relation, TokenUsage, UsageRecord,
};
use super::{store, ActivityError};
use crate::model::TaskStatus;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Default, Serialize, Deserialize)]
struct ParserState {
    title: Option<String>,
    current: Option<ExecutionRecord>,
    sequence: u64,
    last_message: Option<String>,
    root_prompt_id: Option<String>,
}

fn save_current(db: &Connection, state: &ParserState, cutoff: &str) -> Result<(), ActivityError> {
    let Some(current) = &state.current else {
        return Ok(());
    };
    let mut record = current.clone();
    if let Some(title) =
        store::metadata::<String>(db, &format!("title:claude:{}", record.session_id))?
    {
        record.title = title;
    }
    if record
        .started_at
        .as_deref()
        .is_some_and(|value| value >= cutoff)
        || record
            .completed_at
            .as_deref()
            .is_some_and(|value| value >= cutoff)
    {
        store::upsert(
            db,
            &CollectedExecution {
                record: record.clone(),
                relation: Relation {
                    root_prompt_id: if record.is_child {
                        state.root_prompt_id.clone()
                    } else {
                        None
                    },
                    ..Relation::default()
                },
            },
        )?;
        let current_key = if record.is_child {
            let (agent, _) = record.turn_id.split_once('/').ok_or_else(|| {
                ActivityError::InvalidEvent("Claude child has no native agent identity".into())
            })?;
            format!("current:claude:{}:agent:{agent}", record.session_id)
        } else {
            format!("current:claude:{}", record.session_id)
        };
        store::save_metadata(db, &current_key, &record.id)?;
    }
    Ok(())
}

pub(super) fn collect(db: &Connection, home: &Path, cutoff: &str) -> Result<usize, ActivityError> {
    let root = home.join(".claude/projects");
    if !root.is_dir() {
        return Err(ActivityError::Unavailable(
            "Claude Code 数据目录未找到".into(),
        ));
    }
    let mut paths = Vec::new();
    jsonl_files(&root, &mut paths)?;
    paths.sort();
    let cutoff_date = DateTime::parse_from_rfc3339(cutoff)?.with_timezone(&Utc);
    let mut invalid = 0;
    for path in &paths {
        if std::fs::metadata(&path)?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .is_some_and(|value| value.as_secs() < cutoff_date.timestamp() as u64)
        {
            continue;
        }
        invalid += jsonl::<ParserState, _>(db, &path, 0, |db, value, state| {
            let kind = value.get("type").and_then(Value::as_str);
            if kind == Some("custom-title") {
                if let Some(title) = value.get("customTitle").and_then(Value::as_str) {
                    state.title = Some(title.into());
                    if let Some(record) = state.current.as_mut() {
                        record.title = title.into();
                    }
                    if let Some(session) = value.get("sessionId").and_then(Value::as_str) {
                        store::save_metadata(db, &format!("title:claude:{session}"), &title)?;
                        // A conversation rename applies to all of its already archived turns.
                        db.execute(
                            "UPDATE activity_executions
                             SET payload=json_set(payload,'$.title',?2)
                             WHERE client='claude' AND session_id=?1",
                            params![session, title],
                        )?;
                    }
                    save_current(db, state, cutoff)?;
                }
                return Ok(());
            }
            let Some(at) = value.get("timestamp").and_then(timestamp) else {
                return Ok(());
            };
            let Some(session) = value.get("sessionId").and_then(Value::as_str) else {
                return Ok(());
            };
            if kind == Some("user") {
                let tool_result = value
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool_result")
                        })
                    });
                if tool_result || value.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    return Ok(());
                }
                let Some(user_id) = value.get("uuid").and_then(Value::as_str) else {
                    return Ok(());
                };
                let child = value.get("isSidechain").and_then(Value::as_bool) == Some(true);
                let turn = if child {
                    let Some(agent_id) = value.get("agentId").and_then(Value::as_str) else {
                        return Ok(());
                    };
                    format!("{agent_id}/{user_id}")
                } else {
                    user_id.to_owned()
                };
                if state
                    .current
                    .as_ref()
                    .is_some_and(|record| record.turn_id == turn)
                {
                    return Ok(());
                }
                save_current(db, state, cutoff)?;
                state.sequence += 1;
                state.root_prompt_id = value
                    .get("promptId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if !child {
                    if let Some(prompt) = &state.root_prompt_id {
                        store::save_metadata(
                            db,
                            &format!("prompt:claude:{session}:{prompt}"),
                            &turn,
                        )?;
                    }
                }
                state.current = Some(ExecutionRecord {
                    id: execution_id("claude", session, &turn),
                    client: "claude".into(),
                    session_id: session.into(),
                    turn_id: turn,
                    root_id: None,
                    parent_id: None,
                    is_child: child,
                    title: state.title.clone().unwrap_or_else(|| "未命名会话".into()),
                    project_path: value
                        .get("cwd")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    model: None,
                    provider: None,
                    turn_index: value
                        .pointer("/turnPosition/promptIndex")
                        .and_then(Value::as_u64)
                        .unwrap_or(state.sequence),
                    status: TaskStatus::Unknown,
                    attention: None,
                    started_at: Some(at.clone()),
                    completed_at: None,
                    observed_at: at,
                    duration_ms: None,
                    active_ms: None,
                    waiting_ms: None,
                    tokens: TokenUsage::default(),
                    live: false,
                    child_count: 0,
                });
                state.last_message = None;
                return save_current(db, state, cutoff);
            }
            let Some(record) = state.current.as_mut() else {
                return Ok(());
            };
            if kind == Some("assistant") {
                let message = &value["message"];
                record.model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                record.observed_at = at.clone();
                if let Some(response) = message.get("id").and_then(Value::as_str) {
                    let usage = &message["usage"];
                    if usage.is_object() && at.as_str() >= cutoff {
                        let input = usage.get("input_tokens").and_then(Value::as_u64);
                        let output = usage.get("output_tokens").and_then(Value::as_u64);
                        let cache_read =
                            usage.get("cache_read_input_tokens").and_then(Value::as_u64);
                        let cache_write = usage
                            .get("cache_creation_input_tokens")
                            .and_then(Value::as_u64);
                        let total =
                            input.zip(output).zip(cache_read).zip(cache_write).map(
                                |(((input, output), read), write)| input + output + read + write,
                            );
                        store::usage(
                            db,
                            &UsageRecord {
                                id: format!("claude:{response}"),
                                execution_id: record.id.clone(),
                                client: "claude".into(),
                                recorded_at: at.clone(),
                                tokens: TokenUsage {
                                    input,
                                    output,
                                    cache_read,
                                    cache_write,
                                    reasoning: usage
                                        .pointer("/output_tokens_details/thinking_tokens")
                                        .and_then(Value::as_u64),
                                    total,
                                    input_includes_cache: false,
                                    output_includes_reasoning: true,
                                },
                            },
                        )?;
                    }
                    state.last_message = Some(response.into());
                }
                if matches!(
                    message.get("stop_reason").and_then(Value::as_str),
                    Some("end_turn" | "stop_sequence")
                ) {
                    record.status =
                        if value.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
                            TaskStatus::Failed
                        } else {
                            TaskStatus::Succeeded
                        };
                    record.completed_at = Some(at);
                    record.duration_ms = duration(&record.started_at, &record.completed_at);
                    record.live = false;
                }
            }
            if kind == Some("system")
                && value.get("subtype").and_then(Value::as_str) == Some("stop_hook_summary")
                && value.get("preventedContinuation").and_then(Value::as_bool) == Some(true)
            {
                record.status = TaskStatus::Unknown;
                record.completed_at = None;
                record.duration_ms = None;
            }
            save_current(db, state, cutoff)
        })?;
    }
    let mut sessions = Vec::new();
    for path in paths {
        if let Some(cursor) =
            store::metadata::<Value>(db, &format!("cursor-v2:{}", path.display()))?
        {
            if let Some(session) = cursor
                .pointer("/state/current/sessionId")
                .and_then(Value::as_str)
            {
                sessions.push((session.to_owned(), true));
            }
        }
    }
    store::sync_sessions(db, "claude", &sessions)?;
    Ok(invalid)
}
