mod claude;
mod codex;
mod common;
mod hook_attention;
mod process;
mod store;
mod types;
mod zcode;

pub use process::ProcessAnchor;
pub use types::{
    ActivityEvent, ActivityPage, ActivityQuery, ActivitySnapshot, ActivitySummary, ClientSummaries,
    DailyUsage, ExecutionRecord, SessionPage, SessionSummary, SourceHealth, TokenCoverage,
    TokenUsage,
};

use crate::model::{AttentionKind, TaskSnapshot, TaskStatus};
use chrono::{DateTime, Duration, Local, SecondsFormat, Utc};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, Instant};
use thiserror::Error;
use types::{execution_id, CollectedExecution, Relation, UsageRecord};

pub const LIVE_EVIDENCE_SECONDS: i64 = 120;
pub const DEFAULT_HISTORY_RETENTION_DAYS: u16 = 30;
const HISTORY_CLEANUP_INTERVAL: StdDuration = StdDuration::from_secs(6 * 60 * 60);

#[derive(Debug, Error)]
pub enum ActivityError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("metadata error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("filesystem error: {0}")]
    Filesystem(#[from] std::io::Error),
    #[error("timestamp error: {0}")]
    Timestamp(#[from] chrono::ParseError),
    #[error("{0}")]
    Unavailable(String),
    #[error("invalid activity event: {0}")]
    InvalidEvent(String),
    #[error("activity state is unavailable")]
    Poisoned,
}

struct State {
    db: Connection,
    sources: Vec<SourceHealth>,
    live: HashMap<String, ActivityEvent>,
    started_at: String,
    pending: Vec<ActivityEvent>,
    usage_cache: Option<(u64, Vec<UsageRecord>)>,
    derived_cache: Option<DerivedActivity>,
    last_cleanup: Option<(Instant, u16)>,
}

struct DerivedActivity {
    revision: u64,
    records: Vec<ExecutionRecord>,
    snapshot: ActivitySnapshot,
}

pub struct ActivityService {
    home: PathBuf,
    state: Mutex<State>,
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn terminal(status: &TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Interrupted
    )
}

pub fn events_path(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Agent Monitor/activity-events.jsonl")
}

pub fn ingest_hook(client: &str, event: &str, input: Value) -> Result<(), ActivityError> {
    let home =
        dirs::home_dir().ok_or_else(|| ActivityError::Unavailable("用户目录未找到".into()))?;
    ingest_hook_at(&home, client, event, input)
}

pub fn ingest_hook_at(
    home: &Path,
    client: &str,
    event: &str,
    input: Value,
) -> Result<(), ActivityError> {
    if !matches!(client, "claude" | "zcode") {
        return Err(ActivityError::InvalidEvent("unsupported client".into()));
    }
    let Some(session) = input.get("session_id").and_then(Value::as_str) else {
        return Err(ActivityError::InvalidEvent("session_id is required".into()));
    };
    let event = match event {
        "Notification" => match input.get("notification_type").and_then(Value::as_str) {
            Some("permission_prompt") => "PermissionRequest",
            Some("elicitation_dialog" | "agent_needs_input") => "UserInput",
            _ => return Ok(()),
        },
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure"
        | "PermissionRequest" | "Stop" | "StopFailure" | "SessionEnd" | "SubagentStart"
        | "SubagentStop" => event,
        _ => return Err(ActivityError::InvalidEvent("unsupported hook".into())),
    };
    let event = hook_attention::event_name(client, event, &input);
    let event = ActivityEvent {
        client: client.into(),
        session_id: session.into(),
        turn_id: input
            .get(if client == "zcode" {
                "turnId"
            } else {
                "turn_id"
            })
            .and_then(Value::as_str)
            .map(str::to_owned),
        event: event.into(),
        tool_call_id: hook_attention::tool_call_id(client, &input),
        observed_at: now(),
        native_timestamp: if client == "zcode" {
            input
                .get("timestamp")
                .filter(|value| value.is_string())
                .and_then(common::timestamp)
        } else {
            None
        },
        is_child: if client == "claude" {
            Some(input.get("agent_id").and_then(Value::as_str).is_some())
        } else {
            None
        },
        agent_id: if client == "claude" {
            input
                .get("agent_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else {
            None
        },
        process: process::capture(),
    };
    append_event(home, &event)
}

fn append_event(home: &Path, event: &ActivityEvent) -> Result<(), ActivityError> {
    validate_event(event)?;
    let path = events_path(home);
    let parent = path.parent().ok_or(ActivityError::Poisoned)?;
    std::fs::create_dir_all(parent)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut file = options.open(path)?;
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    file.write_all(&line)?;
    Ok(())
}

fn validate_event(event: &ActivityEvent) -> Result<(), ActivityError> {
    if !matches!(event.client.as_str(), "codex" | "claude" | "zcode")
        || event.session_id.len() > 512
        || event.session_id.is_empty()
        || event.turn_id.as_ref().is_some_and(|id| id.len() > 512)
        || event.agent_id.as_ref().is_some_and(|id| id.len() > 512)
        || event.tool_call_id.as_ref().is_some_and(|id| id.len() > 512)
        || !matches!(
            event.event.as_str(),
            "UserPromptSubmit"
                | "PreToolUse"
                | "PostToolUse"
                | "PostToolUseFailure"
                | "PermissionRequest"
                | "UserInput"
                | "Stop"
                | "StopFailure"
                | "SessionEnd"
                | "SubagentStart"
                | "SubagentStop"
        )
    {
        return Err(ActivityError::InvalidEvent("unsupported metadata".into()));
    }
    DateTime::parse_from_rfc3339(&event.observed_at)?;
    if let Some(timestamp) = &event.native_timestamp {
        DateTime::parse_from_rfc3339(timestamp)?;
    }
    Ok(())
}

fn lifecycle_current_key(event: &ActivityEvent) -> String {
    if event.client == "claude" {
        if let Some(agent) = &event.agent_id {
            return format!("current:claude:{}:agent:{agent}", event.session_id);
        }
    }
    format!("current:{}:{}", event.client, event.session_id)
}

impl ActivityService {
    pub fn open_default() -> Result<Self, ActivityError> {
        let home =
            dirs::home_dir().ok_or_else(|| ActivityError::Unavailable("用户目录未找到".into()))?;
        let path = home.join("Library/Application Support/Agent Monitor/activity.db");
        Self::open(home, path)
    }

    pub fn open(home: impl AsRef<Path>, database: impl AsRef<Path>) -> Result<Self, ActivityError> {
        let db = store::open(database.as_ref())?;
        if store::metadata::<String>(&db, "backfill_since")?.is_none() {
            store::save_metadata(
                &db,
                "backfill_since",
                &(Utc::now() - Duration::days(30)).to_rfc3339_opts(SecondsFormat::Millis, true),
            )?;
        }
        let pending =
            store::metadata::<Vec<ActivityEvent>>(&db, "pending_lifecycle")?.unwrap_or_default();
        Ok(Self {
            home: home.as_ref().into(),
            state: Mutex::new(State {
                db,
                sources: ["codex", "claude", "zcode"]
                    .into_iter()
                    .map(|client| SourceHealth {
                        client: client.into(),
                        state: "unavailable".into(),
                        observed_at: None,
                        detail: "等待首次读取".into(),
                    })
                    .collect(),
                live: HashMap::new(),
                started_at: now(),
                pending,
                usage_cache: None,
                derived_cache: None,
                last_cleanup: None,
            }),
        })
    }

    pub fn sync(
        &self,
        live: &[TaskSnapshot],
        sessions: &[crate::codex::SessionActivity],
    ) -> Result<ActivitySnapshot, ActivityError> {
        self.sync_with_retention(live, sessions, DEFAULT_HISTORY_RETENTION_DAYS)
    }

    pub fn sync_with_retention(
        &self,
        live: &[TaskSnapshot],
        sessions: &[crate::codex::SessionActivity],
        retention_days: u16,
    ) -> Result<ActivitySnapshot, ActivityError> {
        let mut state = self.state.lock().map_err(|_| ActivityError::Poisoned)?;
        let before_changes = database_changes(&state.db)?;
        let previous_sources = state.sources.clone();
        let previous_live = state.live.clone();
        let since = store::metadata::<String>(&state.db, "backfill_since")?
            .ok_or(ActivityError::Poisoned)?;
        let cutoff = DateTime::parse_from_rfc3339(&since)?.timestamp();
        let observed = now();
        let mut sources = Vec::new();
        // Own the transaction so every early error, including a failed COMMIT,
        // rolls back before the next background or manual refresh.
        let transaction = state
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for client in ["codex", "claude", "zcode"] {
            transaction.execute_batch("SAVEPOINT source_read")?;
            let result = match client {
                "codex" => codex::collect(&transaction, &self.home, cutoff, &observed),
                "claude" => claude::collect(&transaction, &self.home, &since),
                "zcode" => zcode::collect(&transaction, &self.home, cutoff * 1000),
                _ => unreachable!(),
            };
            let health = match result {
                Ok(invalid) => SourceHealth {
                    client: client.into(),
                    state: if invalid == 0 { "available" } else { "error" }.into(),
                    observed_at: Some(observed.clone()),
                    detail: if invalid == 0 {
                        "本机记录已同步".into()
                    } else {
                        format!("{invalid} 条记录格式异常，已保留可读取记录")
                    },
                },
                Err(error) => {
                    transaction.execute_batch("ROLLBACK TO source_read")?;
                    SourceHealth {
                        client: client.into(),
                        state: if matches!(
                            error,
                            ActivityError::Unavailable(_) | ActivityError::Filesystem(_)
                        ) {
                            "unavailable"
                        } else {
                            "error"
                        }
                        .into(),
                        observed_at: None,
                        detail: error.to_string(),
                    }
                }
            };
            transaction.execute_batch("RELEASE source_read")?;
            sources.push(health);
        }
        sync_codex_live(&transaction, live, &observed)?;
        transaction.commit()?;
        state.sources = sources;
        state.live.retain(|_, event| event.client != "codex");
        for session in sessions {
            if live
                .iter()
                .any(|task| task.provider == "codex" && task.task_id == session.session_id)
            {
                continue;
            }
            state.live.insert(
                execution_id("codex", &session.session_id, &session.turn_id),
                ActivityEvent {
                    client: "codex".into(),
                    session_id: session.session_id.clone(),
                    turn_id: Some(session.turn_id.clone()),
                    event: match session.attention {
                        Some(AttentionKind::Approval) => "PermissionRequest",
                        Some(AttentionKind::UserInput) => "UserInput",
                        None => "PreToolUse",
                    }
                    .into(),
                    observed_at: observed.clone(),
                    native_timestamp: None,
                    tool_call_id: None,
                    is_child: None,
                    agent_id: None,
                    process: None,
                },
            );
        }
        for task in live {
            if task.provider != "codex"
                || task.archived
                || !matches!(task.status, TaskStatus::Running | TaskStatus::NeedsAction)
            {
                continue;
            }
            let Some(turn) = task.turn_id.as_ref() else {
                continue;
            };
            state
                .live
                .entry(execution_id("codex", &task.task_id, turn))
                .or_insert_with(|| ActivityEvent {
                    client: "codex".into(),
                    session_id: task.task_id.clone(),
                    turn_id: Some(turn.clone()),
                    event: match task.attention {
                        Some(AttentionKind::Approval) => "PermissionRequest",
                        Some(AttentionKind::UserInput) => "UserInput",
                        None => "PreToolUse",
                    }
                    .into(),
                    observed_at: observed.clone(),
                    native_timestamp: None,
                    tool_call_id: None,
                    is_child: Some(task.parent_task_id.is_some()),
                    agent_id: None,
                    process: None,
                });
        }
        self.consume_events(&mut state)?;
        state.live.retain(|_, event| {
            event
                .process
                .as_ref()
                .map(process::is_alive)
                .unwrap_or(true)
        });
        if state.last_cleanup.map_or(true, |(last, days)| {
            days != retention_days || last.elapsed() >= HISTORY_CLEANUP_INTERVAL
        }) {
            if prune_history(&state.db, i64::from(retention_days.max(1)))? > 0 {
                state.usage_cache = None;
                state.derived_cache = None;
            }
            state.last_cleanup = Some((Instant::now(), retention_days));
        }
        let changed = database_changes(&state.db)? != before_changes
            || source_health_changed(&previous_sources, &state.sources)
            || live_state_changed(&previous_live, &state.live)
            || state.derived_cache.is_none();
        if changed {
            state.derived_cache = None;
        }
        let sources = state.sources.clone();
        if let Some(cache) = state.derived_cache.as_mut() {
            refresh_cached_activity(cache, &sources);
            return Ok(cache.snapshot.clone());
        }
        Ok(derived_activity(&mut state)?.snapshot.clone())
    }

    fn consume_events(&self, state: &mut State) -> Result<(), ActivityError> {
        let path = events_path(&self.home);
        if !path.is_file() {
            return Ok(());
        }
        let mut events = Vec::new();
        common::jsonl::<(), _>(&state.db, &path, 0, |_, value, _| {
            let event = serde_json::from_value::<ActivityEvent>(value.clone())?;
            validate_event(&event)?;
            events.push(event);
            Ok(())
        })?;
        state.pending.extend(events);
        let mut records = store::records(&state.db)?;
        let mut unresolved = Vec::new();
        for event in state.pending.drain(..) {
            if matches!(event.event.as_str(), "SubagentStart" | "SubagentStop") {
                continue;
            }
            if event.client == "zcode" && event.event == "UserPromptSubmit" {
                if let Some(pending) = zcode::pending_from_hook(&state.db, &event)? {
                    if !records
                        .iter()
                        .any(|item| item.record.id == pending.record.id)
                    {
                        store::upsert(&state.db, &pending)?;
                        records.push(pending);
                    }
                }
            }
            let id = if let Some(turn) = event.turn_id.as_ref() {
                let id = execution_id(&event.client, &event.session_id, turn);
                records
                    .iter()
                    .any(|item| {
                        item.record.id == id
                            && (event.client != "claude"
                                || item.record.is_child == event.is_child.unwrap_or(false))
                    })
                    .then_some(id)
            } else {
                let current = store::metadata::<String>(&state.db, &lifecycle_current_key(&event))?;
                current.filter(|id| {
                    records.iter().any(|item| {
                        item.record.id == *id
                            && item.record.is_child == event.is_child.unwrap_or(false)
                            && (!terminal(&item.record.status)
                                || matches!(
                                    event.event.as_str(),
                                    "Stop" | "StopFailure" | "SessionEnd"
                                ))
                            && (event.event == "UserPromptSubmit"
                                || item
                                    .record
                                    .started_at
                                    .as_ref()
                                    .map_or(true, |started| started <= &event.observed_at))
                    })
                })
            };
            let Some(id) = id else {
                unresolved.push(event);
                continue;
            };
            let key = format!("hook_attention:{id}");
            let mut pending =
                store::metadata::<hook_attention::Pending>(&state.db, &key)?.unwrap_or_default();
            let previous = pending.clone();
            pending.apply(&event.event, event.tool_call_id.as_deref());
            if pending != previous {
                store::save_metadata(&state.db, &key, &pending)?;
            }
            store::save_metadata(&state.db, &format!("lifecycle:{id}"), &event)?;
            if event.observed_at >= state.started_at {
                state.live.insert(id, event);
            }
        }
        for item in &records {
            if item.record.client == "zcode" && !terminal(&item.record.status) {
                if let Some(event) = store::metadata::<ActivityEvent>(
                    &state.db,
                    &format!("lifecycle:{}", item.record.id),
                )? {
                    if event.turn_id.as_deref() == Some(item.record.turn_id.as_str())
                        && matches!(
                            event.event.as_str(),
                            "UserPromptSubmit"
                                | "PreToolUse"
                                | "PostToolUse"
                                | "PostToolUseFailure"
                                | "PermissionRequest"
                                | "UserInput"
                        )
                    {
                        zcode::observe_current(
                            &state.db,
                            &event.session_id,
                            &item.record.id,
                            &event.observed_at,
                        )?;
                    }
                }
            }
        }
        state.pending = unresolved;
        store::save_metadata(&state.db, "pending_lifecycle", &state.pending)?;
        for client in ["claude", "zcode"] {
            let pending = state
                .pending
                .iter()
                .filter(|event| event.client == client)
                .count();
            if pending > 0 {
                if let Some(source) = state
                    .sources
                    .iter_mut()
                    .find(|source| source.client == client)
                {
                    source
                        .detail
                        .push_str(&format!("；{pending} 个事件等待原生轮次关联"));
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn record_event(&self, event: ActivityEvent) -> Result<(), ActivityError> {
        append_event(&self.home, &event)
    }

    pub fn snapshot(&self) -> Result<ActivitySnapshot, ActivityError> {
        let mut state = self.state.lock().map_err(|_| ActivityError::Poisoned)?;
        Ok(derived_activity(&mut state)?.snapshot.clone())
    }

    pub fn history(&self, query: ActivityQuery) -> Result<ActivityPage, ActivityError> {
        let mut state = self.state.lock().map_err(|_| ActivityError::Poisoned)?;
        let records = history_records(&mut state, &query)?;
        let total = records.len() as u64;
        let offset = query.offset.unwrap_or(0).min(total) as usize;
        let limit = query.limit.unwrap_or(50).min(10_000) as usize;
        Ok(ActivityPage {
            executions: records.into_iter().skip(offset).take(limit).collect(),
            total,
        })
    }

    pub fn sessions(&self, mut query: ActivityQuery) -> Result<SessionPage, ActivityError> {
        let mut state = self.state.lock().map_err(|_| ActivityError::Poisoned)?;
        query.roots_only = Some(true);
        let records = history_records(&mut state, &query)?;
        let total_turns = records.len() as u64;
        let mut groups: Vec<Vec<ExecutionRecord>> = Vec::new();
        let mut indices = HashMap::new();
        for record in records {
            let key = (record.client.clone(), record.session_id.clone());
            let index = *indices.entry(key).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[index].push(record);
        }
        let total = groups.len() as u64;
        let offset = query.offset.unwrap_or(0).min(total) as usize;
        let limit = query.limit.unwrap_or(20).min(200) as usize;
        let sessions = groups
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|rounds| SessionSummary {
                latest: rounds[0].clone(),
                turn_count: rounds.len() as u64,
                child_count: rounds.iter().map(|round| round.child_count).sum(),
                // Root records already include their descendants' request usage.
                tokens: TokenUsage::sum(rounds.iter().map(|round| &round.tokens), true),
            })
            .collect();
        Ok(SessionPage {
            sessions,
            total,
            total_turns,
        })
    }

    pub fn execution(&self, id: &str) -> Result<Option<ExecutionRecord>, ActivityError> {
        let mut state = self.state.lock().map_err(|_| ActivityError::Poisoned)?;
        Ok(derived_activity(&mut state)?
            .records
            .iter()
            .find(|record| record.id == id)
            .cloned())
    }
}

fn history_records(
    state: &mut State,
    query: &ActivityQuery,
) -> Result<Vec<ExecutionRecord>, ActivityError> {
    let since = query
        .since
        .as_deref()
        .map(DateTime::parse_from_rfc3339)
        .transpose()?
        .map(|date| {
            date.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        });
    let records = derived_activity(state)?.records.clone();
    let hidden = store::hidden_sessions(&state.db)?;
    let records: Vec<_> = records
        .into_iter()
        .filter(|record| {
            !hidden.contains(&(record.client.clone(), record.session_id.clone()))
                && query.client.as_ref().map_or(true, |v| &record.client == v)
                && query
                    .status
                    .as_ref()
                    .map_or(true, |v| status_name(&record.status) == v)
                && query
                    .session_id
                    .as_ref()
                    .map_or(true, |v| &record.session_id == v)
                && since.as_ref().map_or(true, |v| {
                    record.started_at.as_ref().is_some_and(|at| at >= v)
                })
                && (query.roots_only != Some(true) || !record.is_child)
                && (query.children_only != Some(true) || record.is_child)
                && query.root_id.as_ref().map_or(true, |id| {
                    &record.id == id || record.root_id.as_ref() == Some(id)
                })
        })
        .collect();
    Ok(records)
}

fn status_name(status: &TaskStatus) -> &str {
    match status {
        TaskStatus::Running => "running",
        TaskStatus::NeedsAction => "needs_action",
        TaskStatus::Succeeded => "succeeded",
        TaskStatus::Failed => "failed",
        TaskStatus::Interrupted => "interrupted",
        TaskStatus::Unknown | TaskStatus::Idle => "unknown",
    }
}

fn database_changes(db: &Connection) -> Result<u64, ActivityError> {
    Ok(db.query_row("SELECT total_changes()", [], |row| row.get(0))?)
}

fn source_health_changed(previous: &[SourceHealth], current: &[SourceHealth]) -> bool {
    previous.len() != current.len()
        || previous.iter().zip(current).any(|(left, right)| {
            left.client != right.client || left.state != right.state || left.detail != right.detail
        })
}

fn live_event_equivalent(left: &ActivityEvent, right: &ActivityEvent) -> bool {
    left.client == right.client
        && left.session_id == right.session_id
        && left.turn_id == right.turn_id
        && left.event == right.event
        && left.native_timestamp == right.native_timestamp
        && left.tool_call_id == right.tool_call_id
        && left.is_child == right.is_child
        && left.agent_id == right.agent_id
        && left.process == right.process
}

fn live_state_changed(
    previous: &HashMap<String, ActivityEvent>,
    current: &HashMap<String, ActivityEvent>,
) -> bool {
    previous.len() != current.len()
        || previous.iter().any(|(id, event)| {
            !current
                .get(id)
                .is_some_and(|next| live_event_equivalent(event, next))
        })
}

fn refresh_cached_activity(cache: &mut DerivedActivity, sources: &[SourceHealth]) {
    let observed = now();
    for record in &mut cache.records {
        if record.live {
            record.duration_ms = common::duration(&record.started_at, &Some(observed.clone()));
        }
    }
    for record in &mut cache.snapshot.executions {
        if record.live {
            record.duration_ms = common::duration(&record.started_at, &Some(observed.clone()));
        }
    }
    cache.snapshot.sources = sources.to_vec();
    cache.snapshot.generated_at = observed;
}

fn prune_history(db: &Connection, retention_days: i64) -> Result<usize, ActivityError> {
    let cutoff = (Utc::now() - Duration::days(retention_days)).to_rfc3339();
    let transaction = db.unchecked_transaction()?;
    store::save_metadata(&transaction, "history_cutoff", &cutoff)?;
    let removed_requests = transaction.execute(
        "DELETE FROM activity_requests
         WHERE execution_id IN (
           SELECT id FROM activity_executions
           WHERE json_extract(payload,'$.status') IN ('succeeded','failed','interrupted')
             AND COALESCE(json_extract(payload,'$.completedAt'), observed_at) < ?1
         )",
        [&cutoff],
    )?;
    let removed_executions = transaction.execute(
        "DELETE FROM activity_executions
         WHERE json_extract(payload,'$.status') IN ('succeeded','failed','interrupted')
           AND COALESCE(json_extract(payload,'$.completedAt'), observed_at) < ?1",
        [&cutoff],
    )?;
    transaction.execute(
        "DELETE FROM activity_metadata
         WHERE key LIKE 'lifecycle:%'
           AND json_extract(payload,'$.observedAt') < ?1
           AND NOT EXISTS (
             SELECT 1 FROM activity_executions
             WHERE activity_executions.id = substr(activity_metadata.key, 11)
           )",
        [&cutoff],
    )?;
    transaction.commit()?;
    Ok(removed_requests + removed_executions)
}

fn sync_codex_live(
    db: &Connection,
    live: &[TaskSnapshot],
    observed: &str,
) -> Result<(), ActivityError> {
    for task in live {
        if task.provider != "codex" || task.archived {
            continue;
        }
        let Some(turn) = task.turn_id.as_ref() else {
            continue;
        };
        let id = execution_id("codex", &task.task_id, turn);
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM activity_executions WHERE id=?1)",
            [&id],
            |row| row.get(0),
        )?;
        if !exists && matches!(task.status, TaskStatus::Running | TaskStatus::NeedsAction) {
            store::upsert(
                db,
                &CollectedExecution {
                    record: ExecutionRecord {
                        id: id.clone(),
                        client: "codex".into(),
                        session_id: task.task_id.clone(),
                        turn_id: turn.clone(),
                        root_id: None,
                        parent_id: None,
                        is_child: task.parent_task_id.is_some(),
                        title: task.title.clone(),
                        project_path: task.project_path.clone(),
                        model: None,
                        provider: None,
                        turn_index: 0,
                        status: TaskStatus::Unknown,
                        attention: None,
                        started_at: None,
                        completed_at: None,
                        observed_at: observed.into(),
                        duration_ms: None,
                        active_ms: None,
                        waiting_ms: None,
                        tokens: TokenUsage::default(),
                        live: false,
                        child_count: 0,
                    },
                    relation: Relation {
                        parent_session: task.parent_task_id.clone(),
                        ..Relation::default()
                    },
                },
            )?;
        }
        if terminal(&task.status) {
            db.execute(
                "UPDATE activity_executions
                 SET payload=json_set(payload,'$.status',?2,'$.completedAt',?3,'$.observedAt',?4)
                 WHERE id=?1
                   AND (json_extract(payload,'$.status') IS NOT ?2
                        OR json_extract(payload,'$.completedAt') IS NOT ?3
                        OR json_extract(payload,'$.observedAt') IS NOT ?4)",
                rusqlite::params![
                    id,
                    status_name(&task.status),
                    task.completed_at,
                    task.updated_at
                ],
            )?;
        }
    }
    Ok(())
}

fn derived_activity(state: &mut State) -> Result<&DerivedActivity, ActivityError> {
    let revision = state
        .db
        .query_row("SELECT total_changes()", [], |row| row.get::<_, u64>(0))?;
    if state
        .derived_cache
        .as_ref()
        .is_some_and(|cache| cache.revision == revision)
    {
        return Ok(state.derived_cache.as_ref().unwrap());
    }
    let usage_revision =
        store::metadata::<u64>(&state.db, "usage_revision")?.ok_or(ActivityError::Poisoned)?;
    if state.usage_cache.as_ref().map(|(revision, _)| *revision) != Some(usage_revision) {
        state.usage_cache = Some((usage_revision, store::usages(&state.db)?));
    }
    let usage = &state.usage_cache.as_ref().unwrap().1;
    let records = normalized_records(state, usage)?;
    let snapshot = build_snapshot(state, &records, usage)?;
    state.derived_cache = Some(DerivedActivity {
        revision,
        records,
        snapshot,
    });
    Ok(state.derived_cache.as_ref().unwrap())
}

fn normalized_records(
    state: &State,
    usage: &[UsageRecord],
) -> Result<Vec<ExecutionRecord>, ActivityError> {
    let collected = store::records(&state.db)?;
    let hidden = store::hidden_sessions(&state.db)?;
    let mut by_execution: HashMap<&str, Vec<&TokenUsage>> = HashMap::new();
    for item in usage {
        by_execution
            .entry(&item.execution_id)
            .or_default()
            .push(&item.tokens);
    }
    let ids: HashSet<_> = collected
        .iter()
        .map(|item| item.record.id.clone())
        .collect();
    let mut root_turns: HashMap<(&str, &str), Vec<&str>> = HashMap::new();
    for item in &collected {
        if !item.record.is_child {
            root_turns
                .entry((&item.record.client, &item.record.turn_id))
                .or_default()
                .push(&item.record.id);
        }
    }
    let mut records: Vec<_> = collected
        .iter()
        .map(|item| -> Result<ExecutionRecord, ActivityError> {
            let mut record = item.record.clone();
            if let Some(tokens) = by_execution.get(record.id.as_str()) {
                record.tokens = TokenUsage::sum(tokens.iter().copied(), false);
            }
            if record.is_child {
                if record.client == "claude" {
                    if let Some(prompt) = &item.relation.root_prompt_id {
                        if let Some(turn) = store::metadata::<String>(
                            &state.db,
                            &format!("prompt:claude:{}:{prompt}", record.session_id),
                        )? {
                            let root = execution_id("claude", &record.session_id, &turn);
                            if ids.contains(&root) {
                                record.root_id = Some(root);
                            }
                        }
                    }
                }
                if let Some(root) = item
                    .relation
                    .root_native_turn
                    .as_deref()
                    .and_then(|turn| root_turns.get(&(record.client.as_str(), turn)))
                {
                    if root.len() == 1 && record.root_id.is_none() {
                        record.root_id = Some(root[0].into());
                    }
                }
                if let (Some(session), Some(turn)) = (
                    &item.relation.parent_session,
                    &item.relation.parent_native_turn,
                ) {
                    let parent = execution_id(&record.client, session, turn);
                    if ids.contains(&parent) {
                        record.parent_id = Some(parent);
                    }
                }
            }
            record.live = false;
            if !terminal(&record.status) {
                record.status = TaskStatus::Unknown;
                record.attention = None;
                let persisted = store::metadata::<ActivityEvent>(
                    &state.db,
                    &format!("lifecycle:{}", record.id),
                )?;
                let event = state.live.get(&record.id).or(persisted.as_ref());
                if let Some(event) = event {
                    if event.observed_at > record.observed_at {
                        record.observed_at = event.observed_at.clone();
                    }
                    let source_available = state.sources.iter().any(|source| {
                        source.client == record.client && source.state == "available"
                    });
                    let current = record.client == "codex"
                        || store::metadata::<String>(&state.db, &lifecycle_current_key(event))?
                            .as_deref()
                            == Some(record.id.as_str());
                    let evidence = if let Some(anchor) = &event.process {
                        process::is_alive(anchor)
                    } else {
                        event.observed_at >= state.started_at
                            && DateTime::parse_from_rfc3339(&event.observed_at)
                                .ok()
                                .is_some_and(|at| {
                                    let age = Utc::now()
                                        .signed_duration_since(at.with_timezone(&Utc))
                                        .num_seconds();
                                    (0..=LIVE_EVIDENCE_SECONDS).contains(&age)
                                })
                    };
                    if source_available
                        && current
                        && evidence
                        && !hidden.contains(&(record.client.clone(), record.session_id.clone()))
                    {
                        apply_live_event(&mut record, event);
                    }
                    if event.event == "StopFailure" {
                        record.status = TaskStatus::Failed;
                        record.completed_at = Some(event.observed_at.clone());
                        record.duration_ms =
                            common::duration(&record.started_at, &record.completed_at);
                        record.live = false;
                    }
                }
            }
            if record.live && record.client != "codex" {
                if let Some(attention) = store::metadata::<hook_attention::Pending>(
                    &state.db,
                    &format!("hook_attention:{}", record.id),
                )?
                .and_then(|pending| pending.current())
                {
                    record.status = TaskStatus::NeedsAction;
                    record.attention = Some(attention);
                }
            }
            if record.live {
                record.duration_ms = common::duration(&record.started_at, &Some(now()));
            } else if terminal(&record.status) && record.duration_ms.is_none() {
                record.duration_ms = common::duration(&record.started_at, &record.completed_at);
            }
            Ok(record)
        })
        .collect::<Result<_, ActivityError>>()?;
    let direct_tokens: HashMap<_, _> = records
        .iter()
        .map(|record| (record.id.clone(), record.tokens.clone()))
        .collect();
    let parent_map: HashMap<_, _> = records
        .iter()
        .map(|record| {
            (
                record.id.clone(),
                (record.parent_id.clone(), record.is_child),
            )
        })
        .collect();
    for record in &mut records {
        if record.is_child && record.root_id.is_none() {
            let mut parent = record.parent_id.clone();
            let mut visited = HashSet::new();
            while let Some(id) = parent {
                if !visited.insert(id.clone()) {
                    break;
                }
                let Some((next, is_child)) = parent_map.get(&id) else {
                    break;
                };
                if !is_child {
                    record.root_id = Some(id);
                    break;
                }
                parent = next.clone();
            }
        }
    }
    let mut children = HashMap::<String, Vec<String>>::new();
    for record in &records {
        if let Some(root) = &record.root_id {
            children
                .entry(root.clone())
                .or_default()
                .push(record.id.clone());
        }
    }
    for record in &mut records {
        if let Some(children) = children.get(&record.id) {
            record.child_count = children.len() as u64;
            let tokens = std::iter::once(&record.id)
                .chain(children.iter())
                .filter_map(|id| direct_tokens.get(id));
            record.tokens = TokenUsage::sum(tokens, false);
        }
    }
    Ok(records)
}

fn apply_live_event(record: &mut ExecutionRecord, event: &ActivityEvent) {
    match event.event.as_str() {
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
            record.status = TaskStatus::Running;
            record.live = true;
            record.attention = None;
        }
        "PermissionRequest" => {
            record.status = TaskStatus::NeedsAction;
            record.attention = Some(AttentionKind::Approval);
            record.live = true;
        }
        "UserInput" => {
            record.status = TaskStatus::NeedsAction;
            record.attention = Some(AttentionKind::UserInput);
            record.live = true;
        }
        "Stop" | "SessionEnd" => {
            record.status = TaskStatus::Unknown;
            record.live = false;
            record.attention = None;
        }
        _ => {}
    }
    record.observed_at = event.observed_at.clone();
}

fn local_date(value: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|date| date.with_timezone(&Local).date_naive().to_string())
}

fn aggregate_summary(
    records: &[&ExecutionRecord],
    usage: &[&UsageRecord],
    today: &str,
) -> ActivitySummary {
    let roots: Vec<_> = records.iter().filter(|record| !record.is_child).collect();
    let known_ids: HashSet<_> = records.iter().map(|record| record.id.as_str()).collect();
    let assigned: Vec<_> = usage
        .iter()
        .filter(|item| known_ids.contains(item.execution_id.as_str()))
        .collect();
    ActivitySummary {
        running: roots
            .iter()
            .filter(|record| record.live && record.status == TaskStatus::Running)
            .count() as u64,
        needs_action: roots
            .iter()
            .filter(|record| record.live && record.status == TaskStatus::NeedsAction)
            .count() as u64,
        completed_today: roots
            .iter()
            .filter(|record| {
                record.status == TaskStatus::Succeeded
                    && record
                        .completed_at
                        .as_deref()
                        .and_then(local_date)
                        .as_deref()
                        == Some(today)
            })
            .count() as u64,
        failed_today: roots
            .iter()
            .filter(|record| {
                record.status == TaskStatus::Failed
                    && record
                        .completed_at
                        .as_deref()
                        .and_then(local_date)
                        .as_deref()
                        == Some(today)
            })
            .count() as u64,
        total_turns: roots.len() as u64,
        tokens: TokenUsage::sum(assigned.iter().map(|item| &item.tokens), true),
        token_coverage: TokenCoverage {
            recorded_executions: assigned
                .iter()
                .map(|item| item.execution_id.as_str())
                .collect::<HashSet<_>>()
                .len() as u64,
            total_executions: records.len() as u64,
        },
    }
}

fn build_snapshot(
    state: &State,
    records: &[ExecutionRecord],
    usage: &[UsageRecord],
) -> Result<ActivitySnapshot, ActivityError> {
    let today = Local::now().date_naive().to_string();
    let all_records: Vec<_> = records.iter().collect();
    let known_ids: HashSet<_> = all_records
        .iter()
        .map(|record| record.id.as_str())
        .collect();
    let assigned: Vec<_> = usage
        .iter()
        .filter(|item| known_ids.contains(item.execution_id.as_str()))
        .collect();
    let summary = aggregate_summary(&all_records, &assigned, &today);
    let client_summary = |client: &str| {
        aggregate_summary(
            &all_records
                .iter()
                .copied()
                .filter(|record| record.client == client)
                .collect::<Vec<_>>(),
            &assigned,
            &today,
        )
    };
    let client_summaries = ClientSummaries {
        codex: client_summary("codex"),
        claude: client_summary("claude"),
        zcode: client_summary("zcode"),
    };
    let roots: Vec<_> = all_records
        .iter()
        .filter(|record| !record.is_child)
        .collect();
    let mut daily = BTreeMap::<(String, String), (u64, u64, Vec<&TokenUsage>)>::new();
    for record in roots {
        if let Some(date) = record.started_at.as_deref().and_then(local_date) {
            let value = daily.entry((date, record.client.clone())).or_default();
            value.0 += 1;
        }
        if record.status == TaskStatus::Succeeded {
            if let Some(date) = record.completed_at.as_deref().and_then(local_date) {
                daily.entry((date, record.client.clone())).or_default().1 += 1;
            }
        }
    }
    for request in assigned {
        if let Some(date) = local_date(&request.recorded_at) {
            daily
                .entry((date, request.client.clone()))
                .or_default()
                .2
                .push(&request.tokens);
        }
    }
    let daily = daily
        .into_iter()
        .map(|((date, client), (turns, completed, tokens))| DailyUsage {
            date,
            client,
            turns,
            completed,
            tokens: TokenUsage::sum(tokens, true),
        })
        .collect();
    let hidden = store::hidden_sessions(&state.db)?;
    let mut recent: Vec<_> = records
        .iter()
        .filter(|record| !hidden.contains(&(record.client.clone(), record.session_id.clone())))
        .cloned()
        .collect();
    recent.sort_by(|left, right| {
        let priority = |record: &ExecutionRecord| match record.status {
            TaskStatus::NeedsAction if record.live => 0,
            TaskStatus::Running if record.live => 1,
            _ => 2,
        };
        priority(left)
            .cmp(&priority(right))
            .then_with(|| right.observed_at.cmp(&left.observed_at))
            .then_with(|| left.id.cmp(&right.id))
    });
    recent.truncate(500);
    Ok(ActivitySnapshot {
        protocol_version: "activity.v1".into(),
        generated_at: now(),
        executions: recent,
        summary,
        client_summaries,
        daily,
        sources: state.sources.clone(),
    })
}

#[cfg(test)]
mod tests;
