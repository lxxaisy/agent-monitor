use crate::codex::{
    plan_from_event, resolve_codex_home, resolve_codex_path, CodexClient, JsonlCursor, SessionEvent,
};
use crate::model::{
    aggregate_projects, notification_key, now_rfc3339, plan_counts_from_steps, progress_from_plan,
    AttentionKind, DashboardSnapshot, Diagnostics, NotificationEvent, PlanStep, ProgressConfidence,
    Settings, SyncState, TaskDetail, TaskSnapshot, TaskStatus, TurnSummary,
};
use crate::storage::{SnapshotStore, StorageError};
use chrono::DateTime;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MonitorError {
    #[error("monitor state is unavailable")]
    StateUnavailable,
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("task not found: {0}")]
    TaskNotFound(String),
    #[error("codex error: {0}")]
    Codex(#[from] crate::codex::CodexError),
    #[error("invalid task id")]
    InvalidTaskId,
    #[error("could not open Codex task: {0}")]
    OpenTask(std::io::Error),
    #[error("filesystem error: {0}")]
    Filesystem(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "type", content = "payload")]
pub enum MonitorSignal {
    SnapshotUpdated(DashboardSnapshot),
    TaskUpdated(TaskSnapshot),
    Notification(NotificationEvent),
    ConnectionChanged(SyncState),
    SyncError(String),
}

#[derive(Debug, Default)]
struct SessionTailer {
    cursor: JsonlCursor,
    compatibility_mode: bool,
    file_identity: Option<u64>,
}

#[derive(Debug, Clone)]
struct CachedPlan {
    steps: Vec<PlanStep>,
    confidence: ProgressConfidence,
}

#[derive(Debug, Clone)]
struct PendingSessionCall {
    call_id: Option<String>,
    attention: AttentionKind,
    remaining_questions: Option<HashSet<usize>>,
}

#[derive(Debug, Clone)]
struct SessionActiveTurn {
    turn_id: Option<String>,
    observed_at: Option<String>,
    session_path: Option<PathBuf>,
    last_seen: Instant,
    attention: Option<AttentionKind>,
    pending_calls: Vec<PendingSessionCall>,
}

#[derive(Debug, Clone)]
pub struct InputRequest {
    pub id: String,
    pub session_id: String,
    pub turn_id: String,
    call_id: String,
    remaining_questions: HashSet<usize>,
    accepted: bool,
}

struct MonitorInner {
    tasks: HashMap<String, TaskSnapshot>,
    details: HashMap<String, TaskDetail>,
    plan_cache: HashMap<String, CachedPlan>,
    settings: Settings,
    sync: SyncState,
    store: Arc<SnapshotStore>,
    client: Option<CodexClient>,
    tailers: HashMap<PathBuf, SessionTailer>,
    listeners: Vec<Sender<MonitorSignal>>,
    baseline_ready: bool,
    next_retry_at: Instant,
    retry_seconds: u64,
    last_event_sync: Option<Instant>,
    dirty_task_ids: HashSet<String>,
    session_active_turns: HashMap<String, SessionActiveTurn>,
    persistent_degradation: Option<String>,
    version_cache: Option<(PathBuf, Option<String>)>,
    started_at: DateTime<chrono::Utc>,
    input_requests: Vec<InputRequest>,
}

#[derive(Clone)]
pub struct MonitorService {
    inner: Arc<Mutex<MonitorInner>>,
    sync_lock: Arc<Mutex<()>>,
}

struct SyncContext {
    binary: PathBuf,
    codex_home: PathBuf,
    client: Option<CodexClient>,
    cached_details: HashMap<String, TaskDetail>,
    baseline_ready: bool,
    notifications_enabled: bool,
}

impl MonitorService {
    pub fn new(store: Arc<SnapshotStore>) -> Result<Self, MonitorError> {
        let loaded_tasks = store.load_tasks()?;
        let tasks = loaded_tasks
            .iter()
            .cloned()
            .map(|task| (task.task_id.clone(), task))
            .collect();
        let details = loaded_tasks
            .into_iter()
            .map(|snapshot| {
                let task_id = snapshot.task_id.clone();
                (
                    task_id,
                    TaskDetail {
                        snapshot,
                        recent_turn: None,
                    },
                )
            })
            .collect();
        let settings = store.load_settings()?;
        let sync = SyncState {
            codex_path: resolve_codex_path(settings.codex_path.as_deref())
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            codex_home: resolve_codex_home(settings.codex_home.as_deref())
                .display()
                .to_string(),
            ..SyncState::default()
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(MonitorInner {
                tasks,
                details,
                plan_cache: HashMap::new(),
                settings,
                sync,
                store,
                client: None,
                tailers: HashMap::new(),
                listeners: Vec::new(),
                baseline_ready: false,
                next_retry_at: Instant::now(),
                retry_seconds: 1,
                last_event_sync: None,
                dirty_task_ids: HashSet::new(),
                session_active_turns: HashMap::new(),
                persistent_degradation: None,
                version_cache: None,
                started_at: chrono::Utc::now(),
                input_requests: Vec::new(),
            })),
            sync_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn subscribe(&self) -> Receiver<MonitorSignal> {
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut inner) = self.inner.lock() {
            inner.listeners.push(sender);
        }
        receiver
    }

    pub fn pending_input_requests(&self) -> Result<Vec<InputRequest>, MonitorError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?
            .input_requests
            .iter()
            .filter(|request| request.accepted)
            .cloned()
            .collect())
    }

    pub fn session_activity(&self) -> Result<Vec<crate::codex::SessionActivity>, MonitorError> {
        let sessions: Vec<_> = self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?
            .session_active_turns
            .iter()
            .map(|(id, active)| (id.clone(), active.clone()))
            .collect();
        let mut live = Vec::new();
        let mut expired = Vec::new();
        for (id, active) in sessions {
            if session_active_turn_is_live(&active) {
                if let Some(turn_id) = active.turn_id {
                    live.push(crate::codex::SessionActivity {
                        session_id: id,
                        turn_id,
                        attention: active.attention,
                    });
                }
            } else {
                expired.push((id, active.last_seen));
            }
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?;
        for (id, last_seen) in expired {
            if inner
                .session_active_turns
                .get(&id)
                .is_some_and(|active| active.last_seen == last_seen)
            {
                inner.session_active_turns.remove(&id);
            }
        }
        Ok(live)
    }

    pub fn consume_input_requests(&self, ids: &[String]) -> Result<(), MonitorError> {
        self.inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?
            .input_requests
            .retain(|request| !ids.contains(&request.id));
        Ok(())
    }

    fn broadcast(inner: &mut MonitorInner, signal: MonitorSignal) {
        inner
            .listeners
            .retain(|sender| sender.send(signal.clone()).is_ok());
    }

    pub fn snapshot(&self) -> Result<DashboardSnapshot, MonitorError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?;
        let mut tasks: Vec<_> = inner.tasks.values().cloned().collect();
        tasks.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(DashboardSnapshot {
            protocol_version: "agent-monitor.v1".into(),
            projects: aggregate_projects(&tasks),
            tasks,
            sync: inner.sync.clone(),
            generated_at: now_rfc3339(),
        })
    }

    pub fn task_detail(&self, task_id: &str) -> Result<TaskDetail, MonitorError> {
        let cached = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| MonitorError::StateUnavailable)?;
            if let Some(detail) = inner.details.get(task_id) {
                detail.clone()
            } else {
                inner
                    .tasks
                    .get(task_id)
                    .cloned()
                    .map(|snapshot| TaskDetail {
                        snapshot,
                        recent_turn: None,
                    })
                    .ok_or_else(|| MonitorError::TaskNotFound(task_id.into()))?
            }
        };
        if cached.recent_turn.is_some() {
            return Ok(cached);
        }

        // A restored snapshot may not contain turn metadata. Hydrate it lazily
        // when the sync loop is idle, so opening a detail never starts a second
        // app-server process or blocks an in-flight reconciliation.
        let Ok(_sync_guard) = self.sync_lock.try_lock() else {
            return Ok(cached);
        };
        let client = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| MonitorError::StateUnavailable)?;
            if let Some(detail) = inner.details.get(task_id) {
                if detail.recent_turn.is_some() {
                    return Ok(detail.clone());
                }
            }
            inner.client.take()
        };
        let Some(client) = client else {
            return Ok(cached);
        };
        let result = client
            .read_thread(task_id)
            .map(|record| record.into_detail());
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?;
        if client.is_alive() {
            inner.client = Some(client);
        }
        let mut detail = match result {
            Ok(detail) => detail,
            Err(_) => return Ok(cached),
        };
        preserve_cached_progress(&mut detail, &cached);
        if let Some(active) = inner.session_active_turns.get(task_id).cloned() {
            if session_active_turn_is_live(&active) {
                apply_session_active_turn(&mut detail, &active, &now_rfc3339());
            } else {
                inner.session_active_turns.remove(task_id);
            }
        }
        if let Some(turn_id) = detail.snapshot.turn_id.as_deref() {
            if let Some(cached_plan) = inner.plan_cache.get(turn_id) {
                apply_cached_plan(&mut detail, cached_plan);
            }
        }
        inner
            .tasks
            .insert(task_id.to_string(), detail.snapshot.clone());
        inner.details.insert(task_id.to_string(), detail.clone());
        Ok(detail)
    }

    pub fn settings(&self) -> Result<Settings, MonitorError> {
        self.inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)
            .map(|inner| inner.settings.clone())
    }

    pub fn set_storage_degraded(&self, error: String) -> Result<(), MonitorError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?;
        inner.sync.status = "degraded".into();
        inner.sync.last_error = Some(error.clone());
        inner.persistent_degradation = Some(error);
        Self::emit_connection(&mut inner);
        Ok(())
    }

    pub fn update_settings(&self, settings: Settings) -> Result<Settings, MonitorError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?;
        inner.store.save_settings(&settings)?;
        let codex_home_changed = inner.settings.codex_home != settings.codex_home;
        let reconnect = inner.settings.codex_path != settings.codex_path || codex_home_changed;
        inner.settings = settings;
        inner.next_retry_at = Instant::now();
        if reconnect {
            inner.client = None;
            inner.sync.status = "connecting".into();
        }
        if codex_home_changed {
            inner.tailers.clear();
            inner.session_active_turns.clear();
            inner.sync.compatibility_mode = false;
        }
        Ok(inner.settings.clone())
    }

    pub fn clear_history(&self) -> Result<(), MonitorError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?;
        inner.store.clear()?;
        inner.tasks.clear();
        inner.details.clear();
        inner.session_active_turns.clear();
        inner.baseline_ready = false;
        let snapshot = DashboardSnapshot {
            protocol_version: "agent-monitor.v1".into(),
            tasks: Vec::new(),
            projects: Vec::new(),
            sync: inner.sync.clone(),
            generated_at: now_rfc3339(),
        };
        Self::broadcast(&mut inner, MonitorSignal::SnapshotUpdated(snapshot));
        Ok(())
    }

    pub fn diagnostics(&self) -> Result<Diagnostics, MonitorError> {
        let (binary, cached) = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| MonitorError::StateUnavailable)?;
            (inner.sync.codex_path.clone(), inner.version_cache.clone())
        };
        let version = if binary.is_empty() {
            None
        } else if let Some((_, version)) = cached.filter(|(path, _)| path == Path::new(&binary)) {
            version
        } else {
            crate::codex::codex_version(Path::new(&binary))
        };
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?;
        let same_binary = inner.sync.codex_path == binary;
        if same_binary && !binary.is_empty() {
            inner.version_cache = Some((PathBuf::from(binary), version.clone()));
        }
        Ok(Diagnostics {
            codex_path: inner.sync.codex_path.clone(),
            codex_home: inner.sync.codex_home.clone(),
            codex_version: if same_binary { version } else { None },
            connection_status: inner.sync.status.clone(),
            transport: "stdio".into(),
            compatibility_mode: inner.sync.compatibility_mode,
            degradation_reason: inner.sync.last_error.clone(),
        })
    }

    pub fn open_task(&self, task_id: &str) -> Result<(), MonitorError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?;
        let task = inner
            .tasks
            .get(task_id)
            .ok_or_else(|| MonitorError::TaskNotFound(task_id.into()))?;
        if !valid_task_id(&task.task_id) {
            return Err(MonitorError::InvalidTaskId);
        }
        #[cfg(target_os = "macos")]
        {
            CommandGuard::open_codex_app(&task.task_id).map_err(MonitorError::OpenTask)?;
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let path = resolve_codex_path(inner.settings.codex_path.as_deref())?;
            // Arguments are passed as individual values; no shell interpolation is involved.
            CommandGuard::spawn_cli(&path, &task.task_id).map_err(MonitorError::OpenTask)?;
            Ok(())
        }
    }

    pub fn sync_now(&self) -> Result<DashboardSnapshot, MonitorError> {
        self.sync_once(true)?;
        self.snapshot()
    }

    pub fn account_rate_limits(&self) -> Result<Value, String> {
        let _sync = self
            .sync_lock
            .try_lock()
            .map_err(|_| "Codex 正在同步，稍后刷新额度")?;
        let client = self
            .inner
            .lock()
            .map_err(|_| "Codex 状态不可用")?
            .client
            .take()
            .ok_or("Codex 尚未连接，无法读取账户额度")?;
        let result = client
            .account_rate_limits()
            .map_err(|_| "Codex 账户额度读取失败，请检查原客户端登录".to_string());
        self.inner.lock().map_err(|_| "Codex 状态不可用")?.client = Some(client);
        result
    }

    fn emit_connection(inner: &mut MonitorInner) {
        Self::broadcast(inner, MonitorSignal::ConnectionChanged(inner.sync.clone()));
    }

    fn mark_offline(inner: &mut MonitorInner, error: String) {
        let should_emit = inner.sync.status != "offline"
            || inner.sync.last_error.as_deref() != Some(error.as_str());
        inner.client = None;
        inner.sync.status = "offline".into();
        inner.sync.last_error = Some(error.clone());
        inner.next_retry_at = Instant::now() + Duration::from_secs(inner.retry_seconds);
        inner.retry_seconds = (inner.retry_seconds * 2).min(30);
        if should_emit {
            Self::emit_connection(inner);
            Self::broadcast(inner, MonitorSignal::SyncError(error));
        }
    }

    fn sync_once(&self, manual: bool) -> Result<(), MonitorError> {
        let _sync_guard = self
            .sync_lock
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?;
        let context = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| MonitorError::StateUnavailable)?;
            if !manual && Instant::now() < inner.next_retry_at {
                return Ok(());
            }
            let configured_path = inner.settings.codex_path.clone();
            let configured_home = inner.settings.codex_home.clone();
            let binary = match resolve_codex_path(configured_path.as_deref()) {
                Ok(path) => path,
                Err(error) => {
                    Self::mark_offline(&mut inner, error.to_string());
                    return Ok(());
                }
            };
            let codex_home = resolve_codex_home(configured_home.as_deref());
            inner.sync.codex_path = binary.display().to_string();
            inner.sync.codex_home = codex_home.display().to_string();
            let client = inner.client.take().filter(|client| client.is_alive());
            if client.is_none() {
                inner.version_cache = None;
                inner.sync.status = "connecting".into();
                Self::emit_connection(&mut inner);
            }
            let mut cached_details = inner.details.clone();
            if inner.dirty_task_ids.contains("*") {
                cached_details.clear();
            } else {
                for task_id in &inner.dirty_task_ids {
                    cached_details.remove(task_id);
                }
            }
            SyncContext {
                binary,
                codex_home,
                client,
                cached_details,
                baseline_ready: inner.baseline_ready,
                notifications_enabled: inner.settings.notifications_enabled,
            }
        };

        let client = match context.client {
            Some(client) => client,
            None => match CodexClient::connect(&context.binary, &context.codex_home) {
                Ok(client) => client,
                Err(error) => {
                    let mut inner = self
                        .inner
                        .lock()
                        .map_err(|_| MonitorError::StateUnavailable)?;
                    Self::mark_offline(&mut inner, error.to_string());
                    return Ok(());
                }
            },
        };
        let discovered = match client
            .discover_archived_cached(&context.cached_details)
            .and_then(|archived| {
                let active = client.discover_cached(false, &context.cached_details)?;
                let mut by_task_id: HashMap<String, TaskDetail> = archived
                    .into_iter()
                    .map(|detail| (detail.snapshot.task_id.clone(), detail))
                    .collect();
                // Active wins if a transitional Codex version briefly returns the
                // same thread from both filtered lists.
                for detail in active {
                    by_task_id.insert(detail.snapshot.task_id.clone(), detail);
                }
                Ok(by_task_id.into_values().collect::<Vec<_>>())
            }) {
            Ok(details) => details,
            Err(error) => {
                drop(client);
                let mut inner = self
                    .inner
                    .lock()
                    .map_err(|_| MonitorError::StateUnavailable)?;
                Self::mark_offline(&mut inner, error.to_string());
                return Ok(());
            }
        };

        let mut notifications = Vec::new();
        let mut updated_tasks = Vec::new();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?;
        let current_binary = resolve_codex_path(inner.settings.codex_path.as_deref()).ok();
        let current_home = resolve_codex_home(inner.settings.codex_home.as_deref());
        if current_binary.as_deref() != Some(context.binary.as_path())
            || current_home != context.codex_home
        {
            drop(client);
            inner.next_retry_at = Instant::now();
            return Ok(());
        }
        inner.client = Some(client);
        let now = now_rfc3339();
        let discovered_ids: HashSet<_> = discovered
            .iter()
            .map(|detail| detail.snapshot.task_id.clone())
            .collect();
        inner
            .tasks
            .retain(|task_id, _| discovered_ids.contains(task_id));
        inner
            .details
            .retain(|task_id, _| discovered_ids.contains(task_id));
        let discovered_turn_ids: HashSet<String> = discovered
            .iter()
            .filter_map(|detail| detail.snapshot.turn_id.clone())
            .collect();
        let known_turn_ids: HashSet<String> = inner
            .tasks
            .values()
            .filter_map(|task| task.turn_id.clone())
            .chain(discovered_turn_ids.iter().cloned())
            .chain(
                inner
                    .session_active_turns
                    .values()
                    .filter_map(|active| active.turn_id.clone()),
            )
            .collect();
        inner
            .plan_cache
            .retain(|turn_id, _| known_turn_ids.contains(turn_id));
        for mut detail in discovered {
            if let Some(previous) = inner.details.get(&detail.snapshot.task_id).cloned() {
                preserve_cached_progress(&mut detail, &previous);
            }
            let mut session_active_evidence = false;
            if let Some(active) = inner
                .session_active_turns
                .get(&detail.snapshot.task_id)
                .cloned()
            {
                session_active_evidence = session_active_turn_is_live(&active);
                if session_override_is_reconciled(&detail, &active) || !session_active_evidence {
                    inner.session_active_turns.remove(&detail.snapshot.task_id);
                } else {
                    apply_session_active_turn(&mut detail, &active, &now);
                }
            }
            if let Some(turn_id) = detail.snapshot.turn_id.as_deref() {
                if let Some(cached_plan) = inner.plan_cache.get(turn_id) {
                    apply_cached_plan(&mut detail, cached_plan);
                }
            }
            let task = detail.snapshot.clone();
            if context.baseline_ready {
                let event_type = match inner.tasks.get(&task.task_id) {
                    Some(previous) => transition_event(previous, &task),
                    None if session_active_evidence && !task.archived => terminal_event(&task),
                    None if !task.archived => first_observed_event(&task),
                    None => None,
                };
                if let Some(event_type) = event_type {
                    let key = notification_key(
                        &task.provider,
                        &task.task_id,
                        task.turn_id.as_deref(),
                        event_type,
                    );
                    if context.notifications_enabled
                        && inner.store.mark_notification_once(&key, &now)?
                    {
                        notifications.push(NotificationEvent {
                            event_type: event_type.into(),
                            task_id: task.task_id.clone(),
                            turn_id: task.turn_id.clone(),
                            title: task.title.clone(),
                            body: notification_body(&task),
                        });
                    }
                }
            }
            inner.tasks.insert(task.task_id.clone(), task.clone());
            inner.details.insert(task.task_id.clone(), detail);
            updated_tasks.push(task);
        }
        let all_tasks: Vec<_> = inner.tasks.values().cloned().collect();
        inner.store.save_tasks(&all_tasks)?;
        inner.sync.status =
            if inner.persistent_degradation.is_some() || inner.sync.compatibility_mode {
                "degraded".into()
            } else {
                "online".into()
            };
        inner.sync.last_error = inner.persistent_degradation.clone();
        inner.sync.last_sync_at = Some(now);
        // A successful API sync does not clear a file-format warning; the session watcher
        // may still be unable to interpret future records from this Codex version.
        inner.next_retry_at =
            Instant::now() + Duration::from_secs(inner.settings.refresh_interval_seconds.max(5));
        inner.retry_seconds = 1;
        inner.baseline_ready = true;
        inner.dirty_task_ids.clear();
        let snapshot = DashboardSnapshot {
            protocol_version: "agent-monitor.v1".into(),
            tasks: all_tasks.clone(),
            projects: aggregate_projects(&all_tasks),
            sync: inner.sync.clone(),
            generated_at: now_rfc3339(),
        };
        Self::broadcast(&mut inner, MonitorSignal::SnapshotUpdated(snapshot));
        for task in updated_tasks {
            Self::broadcast(&mut inner, MonitorSignal::TaskUpdated(task));
        }
        Self::emit_connection(&mut inner);
        for notification in notifications {
            Self::broadcast(&mut inner, MonitorSignal::Notification(notification));
        }
        Ok(())
    }

    fn process_external_events(&self) -> Result<(), MonitorError> {
        let mut should_sync = false;
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| MonitorError::StateUnavailable)?;
            let events = inner
                .client
                .as_ref()
                .map(CodexClient::drain_events)
                .unwrap_or_default();
            for event in events {
                if event.method.as_deref() == Some("turn/plan/updated") {
                    if let Some(params) = event.params.as_ref() {
                        if let (Some(turn_id), Some(plan)) =
                            (event_turn_id(params), plan_from_event(params))
                        {
                            inner.plan_cache.insert(
                                turn_id.to_string(),
                                CachedPlan {
                                    steps: plan,
                                    confidence: ProgressConfidence::Medium,
                                },
                            );
                        }
                    }
                }
                if matches!(
                    event.method.as_deref(),
                    Some(
                        "thread/started"
                            | "thread/archived"
                            | "thread/unarchived"
                            | "thread/deleted"
                            | "thread/name/updated"
                            | "turn/started"
                            | "turn/completed"
                            | "turn/plan/updated"
                            | "thread/status/changed"
                    )
                ) {
                    should_sync = true;
                    let task_id = event.params.as_ref().and_then(|params| {
                        event_thread_id(params).map(str::to_string).or_else(|| {
                            event_turn_id(params).and_then(|turn_id| {
                                inner
                                    .tasks
                                    .values()
                                    .find(|task| task.turn_id.as_deref() == Some(turn_id))
                                    .map(|task| task.task_id.clone())
                            })
                        })
                    });
                    inner
                        .dirty_task_ids
                        .insert(task_id.unwrap_or_else(|| "*".into()));
                }
            }
            if should_sync {
                let now = Instant::now();
                if inner
                    .last_event_sync
                    .map(|last| now.duration_since(last) < Duration::from_millis(500))
                    .unwrap_or(false)
                {
                    should_sync = false;
                } else {
                    inner.last_event_sync = Some(now);
                    // Event-driven reconciliation should not wait for the next
                    // scheduled poll while the app-server connection is alive.
                    inner.next_retry_at = now;
                }
            }
        }
        if should_sync {
            self.sync_once(false)?;
        }
        Ok(())
    }

    fn process_session_path(&self, path: PathBuf) -> Result<(), MonitorError> {
        let is_jsonl = path.extension().and_then(|extension| extension.to_str()) == Some("jsonl");
        let is_compressed_jsonl = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.ends_with(".jsonl.gz"))
            .unwrap_or(false);
        if is_compressed_jsonl {
            if let Ok(mut inner) = self.inner.lock() {
                inner.sync.compatibility_mode = true;
                inner.sync.status = "degraded".into();
                Self::emit_connection(&mut inner);
            }
            return Ok(());
        }
        if !is_jsonl {
            return Ok(());
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?;
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                inner.tailers.remove(&path);
                if let Some(task_id) = session_task_id(&path) {
                    inner.dirty_task_ids.insert(task_id);
                } else {
                    inner.dirty_task_ids.insert("*".into());
                }
                let should_sync = inner.client.is_some();
                if should_sync {
                    inner.next_retry_at = Instant::now();
                }
                drop(inner);
                if should_sync {
                    self.sync_once(false)?;
                }
                return Ok(());
            }
        };
        let identity = file_identity(&metadata);
        let tailer = inner
            .tailers
            .entry(path.clone())
            .or_insert_with(|| SessionTailer {
                // Files discovered after watcher installation are new to this monitor
                // instance, so their existing contents must be parsed from the beginning.
                file_identity: Some(identity),
                ..SessionTailer::default()
            });
        if tailer.file_identity != Some(identity) {
            tailer.cursor = JsonlCursor::default();
            tailer.compatibility_mode = false;
        }
        tailer.file_identity = Some(identity);
        tailer.cursor.reset_if_rotated(metadata.len());
        let mut file = File::open(&path)?;
        file.seek(SeekFrom::Start(tailer.cursor.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        tailer.cursor.offset += bytes.len() as u64;
        let (mut events, compatibility) = tailer.cursor.consume_bytes(&bytes);
        tailer.compatibility_mode |= compatibility;
        if compatibility {
            inner.sync.compatibility_mode = true;
            inner.sync.status = "degraded".into();
        }
        let path_task_id = session_task_id(&path);
        let mut relevant = false;
        let mut reconcile = false;
        let mut relevant_task_ids = HashSet::new();
        for event in &mut events {
            if event.task_id.is_none() {
                event.task_id = path_task_id.clone();
            }
            track_input_request(&mut inner, event);
            if let (Some(turn_id), Some(plan)) = (event.turn_id.as_ref(), event.plan.take()) {
                inner.plan_cache.insert(
                    turn_id.clone(),
                    CachedPlan {
                        steps: plan,
                        confidence: ProgressConfidence::Low,
                    },
                );
            }

            let lifecycle = session_event_lifecycle(&event.kind);
            if lifecycle != SessionLifecycle::None {
                let previous_active = event
                    .task_id
                    .as_ref()
                    .and_then(|task_id| inner.session_active_turns.get(task_id))
                    .cloned();
                if let Some(task_id) = apply_session_event(&mut inner, event, Some(&path)) {
                    relevant = true;
                    match (
                        lifecycle,
                        inner.session_active_turns.get(&task_id).cloned(),
                        inner.details.get(&task_id).cloned(),
                    ) {
                        (SessionLifecycle::Active, Some(active), Some(mut detail))
                            if !detail.snapshot.archived
                                && !session_override_is_reconciled(&detail, &active) =>
                        {
                            apply_session_active_turn(&mut detail, &active, &now_rfc3339());
                            if let Some(plan) = active
                                .turn_id
                                .as_ref()
                                .and_then(|turn| inner.plan_cache.get(turn))
                            {
                                apply_cached_plan(&mut detail, plan);
                            }
                            inner.tasks.insert(task_id.clone(), detail.snapshot.clone());
                            inner.details.insert(task_id.clone(), detail);
                        }
                        (SessionLifecycle::Active, Some(active), None)
                            if !matches!(event.kind.as_str(), "task_started" | "turn_started")
                                && previous_active
                                    .as_ref()
                                    .is_some_and(|previous| previous.turn_id == active.turn_id) =>
                        {
                            // Discovery may omit an in-flight session. Its next tool item
                            // cannot make the same full listing useful; keep the live
                            // evidence and let scheduled discovery or a turn boundary retry.
                        }
                        _ => reconcile = true,
                    }
                    relevant_task_ids.insert(task_id);
                }
            }
        }
        if relevant {
            if relevant_task_ids.is_empty() {
                inner.dirty_task_ids.insert("*".into());
            } else {
                inner.dirty_task_ids.extend(relevant_task_ids);
            }
            if reconcile && inner.client.is_some() {
                inner.next_retry_at = Instant::now();
            }
        }
        drop(inner);
        if reconcile {
            self.sync_once(false)?;
        }
        Ok(())
    }

    fn poll_session_files(&self, root: &Path) -> Result<(), MonitorError> {
        let mut files = Vec::new();
        collect_session_files(root, &mut files)?;
        let file_set: HashSet<_> = files.iter().cloned().collect();
        let known = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| MonitorError::StateUnavailable)?;
            inner
                .tailers
                .iter()
                .filter(|(path, _)| path.starts_with(root))
                .map(|(path, tailer)| (path.clone(), (tailer.cursor.offset, tailer.file_identity)))
                .collect::<HashMap<_, _>>()
        };

        for path in files {
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            let current = (metadata.len(), Some(file_identity(&metadata)));
            if known.get(&path) != Some(&current) {
                self.process_session_path(path)?;
            }
        }
        for path in known.keys().filter(|path| !file_set.contains(*path)) {
            self.process_session_path(path.clone())?;
        }
        Ok(())
    }

    pub fn start_background(&self) {
        let service = self.clone();
        thread::Builder::new()
            .name("agent-monitor-sync".into())
            .spawn(move || {
                let (path_sender, path_receiver) = mpsc::channel::<PathBuf>();
                let mut watched_sessions_path: Option<PathBuf> = None;
                let mut watcher: Option<notify::RecommendedWatcher> = None;
                let mut last_session_poll = Instant::now();
                loop {
                    let desired_sessions_path = service.settings().ok().map(|settings| {
                        resolve_codex_home(settings.codex_home.as_deref()).join("sessions")
                    });
                    if desired_sessions_path != watched_sessions_path {
                        watcher = None;
                        watched_sessions_path = desired_sessions_path;
                        if let Ok(mut inner) = service.inner.lock() {
                            inner.tailers.clear();
                        }
                    }
                    if watcher.is_none() {
                        if let Some(path) =
                            watched_sessions_path.as_ref().filter(|path| path.is_dir())
                        {
                            let sender = path_sender.clone();
                            if let Ok(mut next_watcher) = notify::recommended_watcher(
                                move |result: Result<Event, notify::Error>| {
                                    if let Ok(event) = result {
                                        if matches!(
                                            event.kind,
                                            EventKind::Create(_)
                                                | EventKind::Modify(_)
                                                | EventKind::Remove(_)
                                        ) {
                                            for path in event.paths {
                                                let _ = sender.send(path);
                                            }
                                        }
                                    }
                                },
                            ) {
                                if next_watcher.watch(path, RecursiveMode::Recursive).is_ok() {
                                    let _ = service.baseline_session_files(path);
                                    watcher = Some(next_watcher);
                                }
                            }
                        }
                    }
                    while let Ok(path) = path_receiver.try_recv() {
                        if watched_sessions_path
                            .as_ref()
                            .is_some_and(|sessions_path| path.starts_with(sessions_path))
                        {
                            let _ = service.process_session_path(path);
                        }
                    }
                    if last_session_poll.elapsed() >= SESSION_FILE_POLL_INTERVAL {
                        if let Some(path) = watched_sessions_path.as_deref() {
                            let _ = service.poll_session_files(path);
                        }
                        last_session_poll = Instant::now();
                    }
                    let _ = service.process_external_events();
                    let should_poll = service
                        .inner
                        .lock()
                        .map(|inner| Instant::now() >= inner.next_retry_at)
                        .unwrap_or(false);
                    if should_poll {
                        let _ = service.sync_once(false);
                    }
                    thread::sleep(Duration::from_millis(400));
                }
            })
            .ok();
    }

    fn baseline_session_files(&self, root: &Path) -> Result<(), MonitorError> {
        let mut files = Vec::new();
        collect_session_files(root, &mut files)?;
        let file_set: HashSet<PathBuf> = files.iter().cloned().collect();
        let mut parsed_files = Vec::new();
        for path in &files {
            let Ok(metadata) = std::fs::metadata(path) else {
                continue;
            };
            let (events, compatibility) = read_session_tail(path)?;
            parsed_files.push((
                path.clone(),
                metadata.len(),
                file_identity(&metadata),
                events,
                compatibility,
            ));
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MonitorError::StateUnavailable)?;
        inner
            .tailers
            .retain(|path, _| !path.starts_with(root) || file_set.contains(path));
        // Rebuild lifecycle evidence from bounded tails so a restart during an
        // active turn does not inherit a stale terminal state from SQLite.
        inner.session_active_turns.clear();
        for (path, file_len, identity, mut events, compatibility) in parsed_files {
            let path_task_id = session_task_id(&path);
            for event in &mut events {
                if event.task_id.is_none() {
                    event.task_id = path_task_id.clone();
                }
                if let (Some(turn_id), Some(plan)) = (event.turn_id.as_ref(), event.plan.take()) {
                    inner.plan_cache.insert(
                        turn_id.clone(),
                        CachedPlan {
                            steps: plan,
                            confidence: ProgressConfidence::Low,
                        },
                    );
                }
                if session_event_lifecycle(&event.kind) != SessionLifecycle::None {
                    apply_session_event(&mut inner, event, Some(&path));
                }
            }
            if compatibility {
                inner.sync.compatibility_mode = true;
                inner.sync.status = "degraded".into();
            }
            inner.tailers.insert(
                path,
                SessionTailer {
                    cursor: JsonlCursor {
                        offset: file_len,
                        pending: String::new(),
                    },
                    compatibility_mode: compatibility,
                    file_identity: Some(identity),
                },
            );
        }
        // Historical files can end without a terminal event after an old process
        // was killed. Only an open writer may establish a running state at startup.
        inner.session_active_turns.retain(|_, active| {
            active
                .session_path
                .as_deref()
                .map(session_file_is_open)
                .unwrap_or(false)
        });
        let active_found = !inner.session_active_turns.is_empty();
        if active_found && inner.client.is_some() {
            inner.next_retry_at = Instant::now();
        }
        Ok(())
    }
}

fn collect_session_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_session_files(&path, files)?;
            continue;
        }
        let is_jsonl = path.extension().and_then(|extension| extension.to_str()) == Some("jsonl");
        let is_compressed_jsonl = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.ends_with(".jsonl.gz"))
            .unwrap_or(false);
        if is_jsonl && !is_compressed_jsonl {
            files.push(path);
        }
    }
    Ok(())
}

const SESSION_BASELINE_TAIL_BYTES: u64 = 128 * 1024;
const SESSION_FILE_POLL_INTERVAL: Duration = Duration::from_secs(2);

fn read_session_tail(path: &Path) -> Result<(Vec<SessionEvent>, bool), std::io::Error> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let start = file_len.saturating_sub(SESSION_BASELINE_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;

    // A tail can begin in the middle of a JSONL record. Discard that partial
    // record and parse only complete records from the next newline onward.
    if start > 0 {
        let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
            return Ok((Vec::new(), false));
        };
        bytes.drain(..=newline);
    }
    let mut cursor = JsonlCursor::default();
    Ok(cursor.consume_bytes(&bytes))
}

fn event_thread_id(params: &Value) -> Option<&str> {
    params
        .get("threadId")
        .or_else(|| params.get("thread_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("thread")
                .and_then(|thread| thread.get("id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            params.get("turn").and_then(|turn| {
                turn.get("threadId")
                    .or_else(|| turn.get("thread_id"))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        turn.get("thread")
                            .and_then(|thread| thread.get("id"))
                            .and_then(Value::as_str)
                    })
            })
        })
}

fn event_turn_id(params: &Value) -> Option<&str> {
    params
        .get("turnId")
        .or_else(|| params.get("turn_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
        })
}

fn session_task_id(path: &Path) -> Option<String> {
    crate::codex::rollout_session_ids(path).map(|(conversation, _)| conversation)
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ino()
}

#[cfg(not(unix))]
fn file_identity(metadata: &std::fs::Metadata) -> u64 {
    metadata.len()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionLifecycle {
    Active,
    Terminal,
    None,
}

fn session_event_lifecycle(kind: &str) -> SessionLifecycle {
    match kind {
        "task_started"
        | "turn_started"
        | "sub_agent_activity"
        | "item_completed"
        | "custom_tool_call"
        | "custom_tool_call_output"
        | "function_call"
        | "function_call_output"
        | "question_reply"
        | "exec_approval_request"
        | "apply_patch_approval_request" => SessionLifecycle::Active,
        "task_complete" | "turn_aborted" => SessionLifecycle::Terminal,
        _ => SessionLifecycle::None,
    }
}

fn track_input_request(inner: &mut MonitorInner, event: &SessionEvent) {
    let Some(session_id) = event.task_id.as_deref() else {
        return;
    };
    inner.input_requests.retain_mut(|request| {
        if request.session_id == session_id {
            if event.rejected_input_call && event.call_id.as_deref() == Some(&request.call_id) {
                return false;
            }
            if event.accepted_input_call && event.call_id.as_deref() == Some(&request.call_id) {
                request.accepted = true;
            }
            for (call_id, index) in &event.answered_questions {
                if &request.call_id == call_id {
                    request.remaining_questions.remove(index);
                }
            }
        }
        !request.remaining_questions.is_empty()
    });
    let (Some(count), Some(call_id), Some(observed)) = (
        event.async_question_count,
        event.call_id.as_deref(),
        event
            .observed_at
            .as_deref()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok()),
    ) else {
        return;
    };
    // Only incremental requests created during this run may produce new alerts.
    // Baseline tails are read separately, and old/replayed records stay silent.
    if observed < inner.started_at {
        return;
    }
    let Some(turn_id) = event.turn_id.as_deref().or_else(|| {
        inner
            .session_active_turns
            .get(session_id)
            .and_then(|active| active.turn_id.as_deref())
    }) else {
        return;
    };
    let id = format!("codex:{session_id}:{turn_id}:needs_action:input:{call_id}");
    if !inner.input_requests.iter().any(|request| request.id == id) {
        inner.input_requests.push(InputRequest {
            id,
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            call_id: call_id.into(),
            remaining_questions: (0..count).collect(),
            accepted: false,
        });
    }
}

fn apply_session_event(
    inner: &mut MonitorInner,
    event: &SessionEvent,
    session_path: Option<&Path>,
) -> Option<String> {
    let task_id = event.task_id.clone()?;
    match session_event_lifecycle(&event.kind) {
        SessionLifecycle::Active => {
            let previous = inner.session_active_turns.get(&task_id).cloned();
            let new_turn = matches!(event.kind.as_str(), "task_started" | "turn_started")
                || event
                    .turn_id
                    .as_deref()
                    .zip(
                        previous
                            .as_ref()
                            .and_then(|active| active.turn_id.as_deref()),
                    )
                    .is_some_and(|(current, previous)| current != previous);
            let mut pending_calls = if new_turn {
                Vec::new()
            } else {
                previous
                    .as_ref()
                    .map(|active| active.pending_calls.clone())
                    .unwrap_or_default()
            };
            if event.kind == "question_reply"
                && !event.answered_questions.iter().any(|(call_id, index)| {
                    pending_calls.iter().any(|pending| {
                        pending.call_id.as_deref() == Some(call_id.as_str())
                            && pending
                                .remaining_questions
                                .as_ref()
                                .is_some_and(|remaining| remaining.contains(index))
                    })
                })
            {
                return None;
            }
            for (call_id, index) in &event.answered_questions {
                pending_calls.retain_mut(|pending| {
                    if pending.call_id.as_deref() == Some(call_id.as_str()) {
                        if let Some(remaining) = &mut pending.remaining_questions {
                            remaining.remove(index);
                            return !remaining.is_empty();
                        }
                    }
                    true
                });
            }
            if matches!(
                event.kind.as_str(),
                "function_call_output" | "custom_tool_call_output"
            ) {
                if let Some(call_id) = &event.call_id {
                    pending_calls.retain(|pending| {
                        pending.call_id.as_ref() != Some(call_id)
                            || (pending.remaining_questions.is_some() && !event.rejected_input_call)
                    });
                }
            }
            if let Some(attention) = &event.attention {
                if !pending_calls.iter().any(|pending| {
                    pending.call_id == event.call_id && pending.attention == *attention
                }) {
                    pending_calls.push(PendingSessionCall {
                        call_id: event.call_id.clone(),
                        attention: attention.clone(),
                        remaining_questions: event
                            .async_question_count
                            .map(|count| (0..count).collect()),
                    });
                }
            }
            let attention = pending_calls
                .last()
                .map(|pending| pending.attention.clone());
            inner.session_active_turns.insert(
                task_id.clone(),
                SessionActiveTurn {
                    turn_id: event
                        .turn_id
                        .clone()
                        .or_else(|| previous.as_ref().and_then(|active| active.turn_id.clone())),
                    observed_at: event.observed_at.clone().or_else(|| {
                        previous
                            .as_ref()
                            .and_then(|active| active.observed_at.clone())
                    }),
                    session_path: session_path.map(Path::to_path_buf).or_else(|| {
                        previous
                            .as_ref()
                            .and_then(|active| active.session_path.clone())
                    }),
                    last_seen: Instant::now(),
                    attention,
                    pending_calls,
                },
            );
        }
        SessionLifecycle::Terminal => {
            let should_clear = match (
                inner.session_active_turns.get(&task_id),
                event.turn_id.as_deref(),
            ) {
                (Some(active), Some(completed_turn)) => {
                    active.turn_id.as_deref() == Some(completed_turn)
                }
                (Some(_), None) => true,
                (None, _) => false,
            };
            if should_clear {
                inner.session_active_turns.remove(&task_id);
            }
        }
        SessionLifecycle::None => return None,
    }
    Some(task_id)
}

fn session_override_is_reconciled(detail: &TaskDetail, active: &SessionActiveTurn) -> bool {
    let terminal = matches!(
        detail.snapshot.status,
        TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Interrupted
    );
    if !terminal {
        return false;
    }
    let Some(observed_at) = active.observed_at.as_deref() else {
        return false;
    };
    let same_turn = active
        .turn_id
        .as_deref()
        .zip(detail.snapshot.turn_id.as_deref())
        .is_some_and(|(active_turn, api_turn)| active_turn == api_turn);
    let api_timestamp = if same_turn {
        detail.snapshot.completed_at.as_deref()
    } else {
        Some(detail.snapshot.updated_at.as_str())
    };
    api_timestamp.is_some_and(|api_timestamp| rfc3339_after(api_timestamp, observed_at))
}

fn rfc3339_after(left: &str, right: &str) -> bool {
    DateTime::parse_from_rfc3339(left)
        .ok()
        .zip(DateTime::parse_from_rfc3339(right).ok())
        .is_some_and(|(left, right)| left > right)
}

const SESSION_ACTIVITY_GRACE: Duration = Duration::from_secs(90);

fn session_active_turn_is_live(active: &SessionActiveTurn) -> bool {
    if active.last_seen.elapsed() <= SESSION_ACTIVITY_GRACE {
        return true;
    }
    active
        .session_path
        .as_deref()
        .map(session_file_is_open)
        .unwrap_or(true)
}

#[cfg(target_os = "macos")]
fn session_file_is_open(path: &Path) -> bool {
    Command::new("/usr/sbin/lsof")
        .arg("-t")
        .arg("--")
        .arg(path)
        .output()
        .map(|output| output.status.success() && !output.stdout.is_empty())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn session_file_is_open(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed <= SESSION_ACTIVITY_GRACE)
}

fn apply_session_active_turn(detail: &mut TaskDetail, active: &SessionActiveTurn, now: &str) {
    let previous_turn_id = detail.snapshot.turn_id.clone();
    let turn_changed = active
        .turn_id
        .as_deref()
        .zip(previous_turn_id.as_deref())
        .map(|(current, previous)| current != previous)
        .unwrap_or(false);
    let turn_id = active.turn_id.clone().or(previous_turn_id);

    detail.snapshot.turn_id = turn_id.clone();
    detail.snapshot.status = if active.attention.is_some() {
        TaskStatus::NeedsAction
    } else {
        TaskStatus::Running
    };
    detail.snapshot.attention = active.attention.clone();
    detail.snapshot.completed_at = None;
    detail.snapshot.last_error = None;
    detail.snapshot.updated_at = active
        .observed_at
        .clone()
        .unwrap_or_else(|| now.to_string());

    if turn_changed {
        detail.snapshot.progress_percent = None;
        detail.snapshot.progress_basis = crate::model::ProgressBasis::None;
        detail.snapshot.progress_confidence = ProgressConfidence::Low;
        detail.snapshot.plan_total_steps = 0;
        detail.snapshot.plan_completed_steps = 0;
        detail.snapshot.plan_in_progress_steps = 0;
    } else {
        let counts = crate::model::PlanCounts {
            total: detail.snapshot.plan_total_steps,
            completed: detail.snapshot.plan_completed_steps,
            in_progress: detail.snapshot.plan_in_progress_steps,
        };
        detail.snapshot.progress_percent = progress_from_plan(counts, &TaskStatus::Running);
    }

    if let Some(turn_id) = turn_id {
        if let Some(turn) = detail.recent_turn.as_mut() {
            if turn.turn_id == turn_id {
                turn.status = "inProgress".into();
                turn.completed_at = None;
                turn.error = None;
                if turn.started_at.is_none() {
                    turn.started_at = active.observed_at.clone();
                }
            } else {
                detail.recent_turn = Some(TurnSummary {
                    turn_id,
                    status: "inProgress".into(),
                    started_at: active.observed_at.clone(),
                    completed_at: None,
                    duration_seconds: None,
                    plan_steps: Vec::new(),
                    error: None,
                });
            }
        } else {
            detail.recent_turn = Some(TurnSummary {
                turn_id,
                status: "inProgress".into(),
                started_at: active.observed_at.clone(),
                completed_at: None,
                duration_seconds: None,
                plan_steps: Vec::new(),
                error: None,
            });
        }
    } else if let Some(turn) = detail.recent_turn.as_mut() {
        turn.status = "inProgress".into();
        turn.completed_at = None;
        turn.error = None;
    }
}

fn apply_cached_plan(detail: &mut TaskDetail, cached_plan: &CachedPlan) {
    let counts = plan_counts_from_steps(&cached_plan.steps);
    detail.snapshot.plan_total_steps = counts.total;
    detail.snapshot.plan_completed_steps = counts.completed;
    detail.snapshot.plan_in_progress_steps = counts.in_progress;
    detail.snapshot.progress_percent = progress_from_plan(counts, &detail.snapshot.status);
    detail.snapshot.progress_basis = if counts.total > 0 {
        crate::model::ProgressBasis::Plan
    } else {
        crate::model::ProgressBasis::None
    };
    detail.snapshot.progress_confidence = if counts.total > 0 {
        cached_plan.confidence.clone()
    } else {
        ProgressConfidence::Low
    };
    if let (Some(turn), Some(turn_id)) = (
        detail.recent_turn.as_mut(),
        detail.snapshot.turn_id.as_deref(),
    ) {
        if turn.turn_id == turn_id {
            turn.plan_steps = cached_plan.steps.clone();
        }
    }
}

fn preserve_cached_progress(detail: &mut TaskDetail, previous: &TaskDetail) {
    let same_turn = detail
        .snapshot
        .turn_id
        .as_deref()
        .zip(previous.snapshot.turn_id.as_deref())
        .map(|(current, cached)| current == cached)
        .unwrap_or(false);
    if !same_turn || detail.snapshot.plan_total_steps > 0 || previous.snapshot.plan_total_steps == 0
    {
        return;
    }

    let counts = crate::model::PlanCounts {
        total: previous.snapshot.plan_total_steps,
        completed: previous.snapshot.plan_completed_steps,
        in_progress: previous.snapshot.plan_in_progress_steps,
    };
    detail.snapshot.plan_total_steps = counts.total;
    detail.snapshot.plan_completed_steps = counts.completed;
    detail.snapshot.plan_in_progress_steps = counts.in_progress;
    detail.snapshot.progress_percent = progress_from_plan(counts, &detail.snapshot.status);
    detail.snapshot.progress_basis = crate::model::ProgressBasis::Plan;
    detail.snapshot.progress_confidence = previous.snapshot.progress_confidence.clone();

    if let (Some(current_turn), Some(cached_turn)) =
        (detail.recent_turn.as_mut(), previous.recent_turn.as_ref())
    {
        if current_turn.turn_id == cached_turn.turn_id && current_turn.plan_steps.is_empty() {
            current_turn.plan_steps = cached_turn.plan_steps.clone();
        }
    }
}

fn transition_event(previous: &TaskSnapshot, current: &TaskSnapshot) -> Option<&'static str> {
    if previous.task_id != current.task_id {
        return None;
    }
    let was_live = matches!(
        previous.status,
        TaskStatus::Running | TaskStatus::NeedsAction
    );
    if previous.turn_id != current.turn_id {
        return match current.status {
            TaskStatus::Succeeded if was_live => Some("completed"),
            TaskStatus::Failed if was_live => Some("failed"),
            TaskStatus::Interrupted if was_live => Some("interrupted"),
            TaskStatus::NeedsAction => Some(match current.attention {
                Some(crate::model::AttentionKind::Approval) => "approval",
                Some(crate::model::AttentionKind::UserInput) => "user_input",
                None => "needs_action",
            }),
            _ => None,
        };
    }
    match current.status {
        TaskStatus::Succeeded if was_live => Some("completed"),
        TaskStatus::Failed if was_live => Some("failed"),
        TaskStatus::Interrupted if was_live => Some("interrupted"),
        TaskStatus::NeedsAction
            if previous.status != TaskStatus::NeedsAction
                || previous.attention != current.attention =>
        {
            Some(match current.attention {
                Some(crate::model::AttentionKind::Approval) => "approval",
                Some(crate::model::AttentionKind::UserInput) => "user_input",
                None => "needs_action",
            })
        }
        _ => None,
    }
}

fn terminal_event(task: &TaskSnapshot) -> Option<&'static str> {
    match task.status {
        TaskStatus::Succeeded => Some("completed"),
        TaskStatus::Failed => Some("failed"),
        TaskStatus::Interrupted => Some("interrupted"),
        _ => None,
    }
}

fn first_observed_event(task: &TaskSnapshot) -> Option<&'static str> {
    match task.status {
        TaskStatus::NeedsAction => Some(match task.attention {
            Some(crate::model::AttentionKind::Approval) => "approval",
            Some(crate::model::AttentionKind::UserInput) => "user_input",
            None => "needs_action",
        }),
        _ => None,
    }
}

fn notification_body(task: &TaskSnapshot) -> String {
    match task.status {
        TaskStatus::Succeeded => "turn 已完成".into(),
        TaskStatus::Failed => task
            .last_error
            .clone()
            .unwrap_or_else(|| "turn 失败".into()),
        TaskStatus::Interrupted => "turn 已中断".into(),
        TaskStatus::NeedsAction => match task.attention {
            Some(crate::model::AttentionKind::Approval) => "等待批准".into(),
            Some(crate::model::AttentionKind::UserInput) => "等待你的输入".into(),
            None => "需要处理".into(),
        },
        _ => "任务状态已更新".into(),
    }
}

struct CommandGuard;

impl CommandGuard {
    #[cfg(target_os = "macos")]
    #[allow(clippy::io_other_error)]
    fn open_codex_app(task_id: &str) -> std::io::Result<()> {
        let status = Command::new("/usr/bin/open")
            .args(["-b", "com.openai.codex", "--"])
            .arg(format!("codex://threads/{task_id}"))
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Codex App returned {status}"),
            ))
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn spawn_cli(binary: &Path, task_id: &str) -> std::io::Result<()> {
        Command::new(binary)
            .arg("resume")
            .arg(task_id)
            .spawn()
            .map(|_| ())
    }
}

fn valid_task_id(task_id: &str) -> bool {
    !task_id.is_empty()
        && task_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProgressBasis, ProgressConfidence};
    use crate::storage::SnapshotStore;

    fn task(status: TaskStatus, turn: &str) -> TaskSnapshot {
        TaskSnapshot {
            provider: "codex".into(),
            task_id: "t".into(),
            turn_id: Some(turn.into()),
            parent_task_id: None,
            title: "Task".into(),
            project_path: "/tmp".into(),
            source: "cli".into(),
            status,
            attention: None,
            progress_percent: None,
            progress_basis: ProgressBasis::None,
            progress_confidence: ProgressConfidence::Low,
            created_at: "now".into(),
            updated_at: "now".into(),
            completed_at: None,
            archived: false,
            last_error: None,
            plan_total_steps: 0,
            plan_completed_steps: 0,
            plan_in_progress_steps: 0,
        }
    }

    #[test]
    fn transition_events_are_terminal_and_attention_scoped() {
        assert_eq!(
            transition_event(
                &task(TaskStatus::Running, "a"),
                &task(TaskStatus::Succeeded, "a")
            ),
            Some("completed")
        );
        assert_eq!(
            transition_event(&task(TaskStatus::Idle, "a"), &task(TaskStatus::Idle, "a")),
            None
        );
        assert_eq!(
            transition_event(
                &task(TaskStatus::Running, "a"),
                &task(TaskStatus::Failed, "a")
            ),
            Some("failed")
        );
        assert_eq!(
            transition_event(
                &task(TaskStatus::Succeeded, "old-turn"),
                &task(TaskStatus::Succeeded, "new-turn")
            ),
            None
        );
        assert_eq!(
            transition_event(
                &task(TaskStatus::Running, "old-turn"),
                &task(TaskStatus::Succeeded, "new-turn")
            ),
            Some("completed")
        );
        assert_eq!(
            transition_event(
                &task(TaskStatus::Idle, "old-turn"),
                &task(TaskStatus::Succeeded, "new-turn")
            ),
            None
        );
        assert_eq!(
            first_observed_event(&task(TaskStatus::Succeeded, "new-turn")),
            None
        );
        assert_eq!(
            first_observed_event(&task(TaskStatus::Failed, "new-turn")),
            None
        );
        assert_eq!(
            first_observed_event(&task(TaskStatus::Running, "new-turn")),
            None
        );
        let mut attention = task(TaskStatus::NeedsAction, "new-turn");
        attention.attention = Some(AttentionKind::Approval);
        assert_eq!(first_observed_event(&attention), Some("approval"));
    }

    #[test]
    fn validates_task_ids_before_building_open_urls() {
        assert!(valid_task_id("00000000-0000-4000-8000-000000000001"));
        assert!(valid_task_id("sub_agent-1"));
        assert!(!valid_task_id(""));
        assert!(!valid_task_id("../../task"));
        assert!(!valid_task_id("task?query"));
    }

    #[test]
    fn derives_thread_id_from_rollout_filename_for_file_events() {
        let path = Path::new(
            "/tmp/rollout-2026-08-29T13-00-56-00000000-0000-4000-8000-000000000001.jsonl",
        );
        assert_eq!(
            session_task_id(path).as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );
        let resumed = Path::new("/tmp/rollout-2026-10-23T09-48-50-01a158b5-42da-7e73-bdaf-91895cb36ace_01a16672-52e1-7df0-9b98-63a9846c7e40.jsonl");
        assert_eq!(
            session_task_id(resumed).as_deref(),
            Some("01a158b5-42da-7e73-bdaf-91895cb36ace"),
            "resume suffix is a run identity, not the conversation"
        );
        assert_eq!(session_task_id(Path::new("/tmp/not-a-rollout.jsonl")), None);
    }

    #[test]
    fn resumed_rollout_routes_accepted_input_to_the_conversation_and_waits_for_acceptance() {
        let root = std::env::temp_dir().join(format!(
            "monitor-resume-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path=root.join("rollout-2026-10-23T09-48-50-01a158b5-42da-7e73-bdaf-91895cb36ace_01a16672-52e1-7df0-9b98-63a9846c7e40.jsonl");
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        store
            .save_settings(&Settings {
                codex_path: Some(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/fake-codex.sh")
                        .display()
                        .to_string(),
                ),
                codex_home: Some(root.display().to_string()),
                ..Settings::default()
            })
            .unwrap();
        let service = MonitorService::new(store).unwrap();
        let at = now_rfc3339();
        let rows = [
            serde_json::json!({"timestamp":at,"type":"event_msg","payload":{"type":"task_started","thread_id":"01a158b5-42da-7e73-bdaf-91895cb36ace","turn_id":"turn"}}),
            serde_json::json!({"timestamp":at,"type":"response_item","payload":{"type":"function_call","name":"request_user_input_async","call_id":"question","arguments":"{\"questions\":[{}]}"}}),
        ];
        std::fs::write(
            &path,
            rows.iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .unwrap();
        service.process_session_path(path.clone()).unwrap();
        assert!(
            service.pending_input_requests().unwrap().is_empty(),
            "an unaccepted call must not notify"
        );
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file,"{}",serde_json::json!({"timestamp":at,"type":"response_item","payload":{"type":"function_call_output","call_id":"question","output":"{\"accepted\":true}"}})).unwrap();
        file.flush().unwrap();
        service.process_session_path(path).unwrap();
        let requests = service.pending_input_requests().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].session_id,
            "01a158b5-42da-7e73-bdaf-91895cb36ace"
        );
        assert_eq!(requests[0].turn_id, "turn");
        drop(file);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "Replays only control metadata from user-specified local examples; no native app"]
    fn local_reported_input_examples_replay() {
        use std::io::BufRead;
        let source = rusqlite::Connection::open_with_flags(
            dirs::home_dir().unwrap().join(".codex/state_5.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        for (session, through, expected_pending) in [
            (
                "01a16676-36e3-7a73-a59c-a12286d4c87f",
                "2026-10-23T03:40:56Z",
                0,
            ),
            (
                "01a158b5-42da-7e73-bdaf-91895cb36ace",
                "2026-10-23T03:44:42Z",
                1,
            ),
        ] {
            let path: String = source
                .query_row(
                    "SELECT rollout_path FROM threads WHERE id=?1",
                    [session],
                    |row| row.get(0),
                )
                .unwrap();
            let service =
                MonitorService::new(Arc::new(SnapshotStore::open_in_memory().unwrap())).unwrap();
            let mut inner = service.inner.lock().unwrap();
            inner.started_at = DateTime::parse_from_rfc3339("2026-10-23T03:39:00Z")
                .unwrap()
                .to_utc();
            for line in std::io::BufReader::new(File::open(&path).unwrap()).lines() {
                let value: Value = serde_json::from_str(&line.unwrap()).unwrap();
                if value
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .is_some_and(|at| at > through)
                {
                    break;
                }
                if let Some(mut event) = crate::codex::parse_session_event(&value) {
                    if event.task_id.is_none() {
                        event.task_id = session_task_id(Path::new(&path));
                    }
                    track_input_request(&mut inner, &event);
                    apply_session_event(&mut inner, &event, Some(Path::new(&path)));
                }
            }
            assert_eq!(
                inner
                    .input_requests
                    .iter()
                    .filter(|request| request.accepted)
                    .count(),
                expected_pending
            );
            assert_eq!(
                inner.session_active_turns[session].attention,
                if expected_pending == 0 {
                    None
                } else {
                    Some(AttentionKind::UserInput)
                }
            );
            println!("replay session={session} accepted_pending={expected_pending} through={through}: PASS");
            drop(inner);
            if expected_pending == 1 {
                let database = std::env::var("AGENT_MONITOR_ACTIVITY_QA_DB")
                    .expect("isolated ledger path required");
                let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
                let services = crate::runtime::Services {
                    monitor: service,
                    activity: Arc::new(
                        crate::activity::ActivityService::open(dirs::home_dir().unwrap(), database)
                            .unwrap(),
                    ),
                    quota: Arc::new(crate::quota::QuotaService::new().unwrap()),
                    news: Arc::new(crate::reset_news::ResetNews::new(store.clone()).unwrap()),
                    passport: Arc::new(crate::passport::CompanionService::new(store).unwrap()),
                    notices: crate::desktop_notices::DesktopNotices::default(),
                };
                assert!(services.monitor.snapshot().unwrap().tasks.is_empty());
                let (snapshot, alerts) = services.sync().unwrap();
                let task_id = format!("codex:{session}:01a166db-daf5-7232-9a74-e2987f92c0a5");
                let record = snapshot
                    .executions
                    .iter()
                    .find(|record| record.id == task_id)
                    .unwrap();
                assert_eq!(record.status, TaskStatus::NeedsAction);
                assert!(record.started_at.is_some());
                assert!(record.tokens.total.is_some());
                assert_eq!(
                    alerts
                        .iter()
                        .filter(|alert| alert.task_id == task_id && alert.kind == "needs_action")
                        .count(),
                    1
                );
                assert!(services.sync().unwrap().1.is_empty());
                println!("resumed chat: native history -> canonical ledger -> needs_action -> one persistent alert, API list empty: PASS");
            }
        }
    }

    #[test]
    fn extracts_thread_and_turn_ids_from_app_server_event_shapes() {
        let top_level = serde_json::json!({"threadId":"thread-1", "turnId":"turn-1"});
        assert_eq!(event_thread_id(&top_level), Some("thread-1"));
        assert_eq!(event_turn_id(&top_level), Some("turn-1"));

        let nested = serde_json::json!({
            "turn": {"id":"turn-2", "threadId":"thread-2"}
        });
        assert_eq!(event_thread_id(&nested), Some("thread-2"));
        assert_eq!(event_turn_id(&nested), Some("turn-2"));

        let thread_object = serde_json::json!({"thread":{"id":"thread-3"}});
        assert_eq!(event_thread_id(&thread_object), Some("thread-3"));
    }

    #[test]
    fn cached_plan_updates_progress_and_detail_for_the_current_turn() {
        let mut detail = crate::codex::ThreadRecord::from_value(serde_json::json!({
            "id": "thread-1",
            "cwd": "/tmp/project",
            "status": {"type": "active"},
            "turns": [{"id": "turn-1", "status": "inProgress"}]
        }))
        .unwrap()
        .into_detail();
        let cached = CachedPlan {
            steps: vec![
                PlanStep {
                    id: "step-1".into(),
                    title: "Inspect".into(),
                    status: "completed".into(),
                },
                PlanStep {
                    id: "step-2".into(),
                    title: "Verify".into(),
                    status: "inProgress".into(),
                },
            ],
            confidence: ProgressConfidence::Medium,
        };
        apply_cached_plan(&mut detail, &cached);
        assert_eq!(detail.snapshot.plan_total_steps, 2);
        assert_eq!(detail.snapshot.plan_completed_steps, 1);
        assert_eq!(detail.snapshot.plan_in_progress_steps, 1);
        assert_eq!(detail.snapshot.progress_percent, Some(75));
        assert_eq!(
            detail.snapshot.progress_confidence,
            ProgressConfidence::Medium
        );
        assert_eq!(
            detail
                .recent_turn
                .as_ref()
                .map(|turn| turn.plan_steps.len()),
            Some(2)
        );
    }

    #[test]
    fn cached_plan_never_applies_to_a_different_turn() {
        let mut detail = crate::codex::ThreadRecord::from_value(serde_json::json!({
            "id": "thread-1",
            "cwd": "/tmp/project",
            "status": {"type": "active"},
            "turns": [{"id": "turn-new", "status": "inProgress"}]
        }))
        .unwrap()
        .into_detail();
        let cached = CachedPlan {
            steps: vec![PlanStep {
                id: "step-1".into(),
                title: "Old turn".into(),
                status: "completed".into(),
            }],
            confidence: ProgressConfidence::Low,
        };
        let cache_key = "turn-old";
        assert_ne!(detail.snapshot.turn_id.as_deref(), Some(cache_key));
        if detail.snapshot.turn_id.as_deref() == Some(cache_key) {
            apply_cached_plan(&mut detail, &cached);
        }
        assert_eq!(detail.snapshot.plan_total_steps, 0);
        assert_eq!(detail.snapshot.turn_id.as_deref(), Some("turn-new"));
    }

    #[test]
    fn metadata_only_detail_preserves_progress_for_the_same_turn() {
        let mut previous = TaskDetail {
            snapshot: task(TaskStatus::Running, "turn-1"),
            recent_turn: None,
        };
        previous.snapshot.plan_total_steps = 4;
        previous.snapshot.plan_completed_steps = 2;
        previous.snapshot.plan_in_progress_steps = 1;
        previous.snapshot.progress_percent = Some(1);
        previous.snapshot.progress_basis = ProgressBasis::Plan;
        previous.snapshot.progress_confidence = ProgressConfidence::Medium;

        let mut detail = crate::codex::ThreadRecord::from_value(serde_json::json!({
            "id": "t",
            "cwd": "/tmp/project",
            "status": {"type": "active"},
            "turns": [{"id": "turn-1", "status": "inProgress"}]
        }))
        .unwrap()
        .into_detail();
        preserve_cached_progress(&mut detail, &previous);

        assert_eq!(detail.snapshot.plan_total_steps, 4);
        assert_eq!(detail.snapshot.plan_completed_steps, 2);
        assert_eq!(detail.snapshot.plan_in_progress_steps, 1);
        assert_eq!(detail.snapshot.progress_percent, Some(63));
        assert_eq!(detail.snapshot.progress_basis, ProgressBasis::Plan);
        assert_eq!(
            detail.snapshot.progress_confidence,
            ProgressConfidence::Medium
        );
    }

    #[test]
    fn cached_progress_does_not_override_a_new_turn_or_api_plan() {
        let mut previous = TaskDetail {
            snapshot: task(TaskStatus::Running, "turn-1"),
            recent_turn: None,
        };
        previous.snapshot.plan_total_steps = 4;
        previous.snapshot.plan_completed_steps = 3;
        previous.snapshot.progress_basis = ProgressBasis::Plan;

        let mut new_turn = crate::codex::ThreadRecord::from_value(serde_json::json!({
            "id": "t",
            "cwd": "/tmp/project",
            "status": {"type": "active"},
            "turns": [{"id": "turn-2", "status": "inProgress"}]
        }))
        .unwrap()
        .into_detail();
        preserve_cached_progress(&mut new_turn, &previous);
        assert_eq!(new_turn.snapshot.plan_total_steps, 0);

        let mut api_plan = crate::codex::ThreadRecord::from_value(serde_json::json!({
            "id": "t",
            "cwd": "/tmp/project",
            "status": {"type": "active"},
            "turns": [{
                "id": "turn-1",
                "status": "inProgress",
                "plan": [{"step": "Fresh API step", "status": "inProgress"}]
            }]
        }))
        .unwrap()
        .into_detail();
        preserve_cached_progress(&mut api_plan, &previous);
        assert_eq!(api_plan.snapshot.plan_total_steps, 1);
        assert_eq!(api_plan.snapshot.progress_percent, Some(50));
        assert_eq!(
            api_plan.snapshot.progress_confidence,
            ProgressConfidence::High
        );
    }

    #[test]
    fn metadata_fallback_preserves_state_but_refreshes_safe_thread_fields() {
        let mut previous = TaskDetail {
            snapshot: task(TaskStatus::Succeeded, "turn-1"),
            recent_turn: None,
        };
        previous.snapshot.title = "Old title".into();
        previous.snapshot.project_path = "/old/project".into();
        previous.snapshot.source = "vscode".into();
        let record = crate::codex::ThreadRecord::from_value(serde_json::json!({
            "id": "t",
            "parentThreadId": "parent",
            "cwd": "/new/project",
            "source": {"subAgent": "review"},
            "name": "New title",
            "updatedAt": 1780000100,
            "archived": true
        }))
        .unwrap();
        let merged = crate::codex::merge_cached_metadata(&previous, &record);
        assert_eq!(merged.snapshot.status, TaskStatus::Succeeded);
        assert_eq!(merged.snapshot.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(merged.snapshot.title, "New title");
        assert_eq!(merged.snapshot.project_path, "/new/project");
        assert_eq!(merged.snapshot.source, "subAgentReview");
        assert!(merged.snapshot.archived);
        assert_eq!(merged.snapshot.parent_task_id.as_deref(), Some("parent"));
    }

    #[test]
    fn restores_saved_snapshot_details_on_service_start() {
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        let mut saved = task(TaskStatus::Running, "turn-1");
        saved.progress_percent = Some(64);
        saved.plan_total_steps = 5;
        saved.plan_completed_steps = 3;
        store.save_tasks(&[saved]).unwrap();
        let service = MonitorService::new(store).unwrap();
        let detail = service.task_detail("t").unwrap();
        assert_eq!(detail.snapshot.progress_percent, Some(64));
        assert_eq!(detail.snapshot.plan_total_steps, 5);
        assert!(detail.recent_turn.is_none());
    }

    #[test]
    fn caches_session_update_plan_by_turn_id() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "agent-monitor-plan-cache-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let task_id = "00000000-0000-4000-8000-000000000001";
        let path = root.join(format!("rollout-2026-08-29T13-00-56-{task_id}.jsonl"));
        std::fs::write(
            &path,
            br#"{"type":"response_item","metadata":{"turn_id":"turn-1"},"payload":{"type":"function_call","name":"update_plan","arguments":"{\"plan\":[{\"step\":\"Inspect\",\"status\":\"completed\"}]}"}}
"#,
        )
        .unwrap();
        let service =
            MonitorService::new(Arc::new(SnapshotStore::open_in_memory().unwrap())).unwrap();
        service.inner.lock().unwrap().next_retry_at = Instant::now() + Duration::from_secs(30);
        service.process_session_path(path).unwrap();
        let inner = service.inner.lock().unwrap();
        let cached = inner
            .plan_cache
            .get("turn-1")
            .expect("plan should be cached");
        assert_eq!(cached.steps.len(), 1);
        assert_eq!(cached.steps[0].title, "Inspect");
        assert_eq!(cached.confidence, ProgressConfidence::Low);
        drop(inner);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn initial_sync_does_not_notify_for_historical_terminal_tasks() {
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        let settings = Settings {
            codex_path: Some(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake-codex.sh")
                    .display()
                    .to_string(),
            ),
            codex_home: Some("/tmp/fake-codex".into()),
            ..Settings::default()
        };
        store.save_settings(&settings).unwrap();
        let service = MonitorService::new(store).unwrap();
        let receiver = service.subscribe();
        service.sync_now().unwrap();
        let signals: Vec<_> = receiver.try_iter().collect();
        assert!(!signals
            .iter()
            .any(|signal| matches!(signal, MonitorSignal::Notification(_))));
        service.sync_now().unwrap();
        let signals: Vec<_> = receiver.try_iter().collect();
        assert!(!signals
            .iter()
            .any(|signal| matches!(signal, MonitorSignal::Notification(_))));
    }

    #[test]
    fn session_baseline_skips_history_but_reads_new_files_from_zero() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "agent-monitor-session-baseline-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let task_id = "00000000-0000-4000-8000-000000000001";
        let path = root.join(format!("rollout-2026-08-29T13-00-56-{task_id}.jsonl"));
        std::fs::write(
            &path,
            br#"{"type":"event_msg","payload":{"type":"task_started"}}
"#,
        )
        .unwrap();
        let service =
            MonitorService::new(Arc::new(SnapshotStore::open_in_memory().unwrap())).unwrap();
        service.inner.lock().unwrap().next_retry_at = Instant::now() + Duration::from_secs(30);
        service.baseline_session_files(&root).unwrap();
        service.process_session_path(path.clone()).unwrap();
        {
            let inner = service.inner.lock().unwrap();
            assert!(inner.dirty_task_ids.is_empty());
            assert!(inner.session_active_turns.is_empty());
        }

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}"
        )
        .unwrap();
        drop(file);
        service.process_session_path(path).unwrap();
        assert!(service
            .inner
            .lock()
            .unwrap()
            .dirty_task_ids
            .contains(task_id));

        let new_path = root.join(format!("rollout-2026-08-29T13-00-57-{task_id}.jsonl"));
        service.inner.lock().unwrap().dirty_task_ids.clear();
        std::fs::write(
            &new_path,
            br#"{"type":"event_msg","payload":{"type":"task_started"}}
"#,
        )
        .unwrap();
        service.process_session_path(new_path).unwrap();
        assert!(service
            .inner
            .lock()
            .unwrap()
            .dirty_task_ids
            .contains(task_id));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_offset_poll_catches_an_append_when_the_watcher_misses_it() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "agent-monitor-session-poll-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let task_id = "00000000-0000-4000-8000-000000000001";
        let path = root.join(format!("rollout-2026-08-30T10-00-00-{task_id}.jsonl"));
        std::fs::write(&path, b"").unwrap();

        let service =
            MonitorService::new(Arc::new(SnapshotStore::open_in_memory().unwrap())).unwrap();
        service.inner.lock().unwrap().next_retry_at = Instant::now() + Duration::from_secs(30);
        service.baseline_session_files(&root).unwrap();

        std::fs::write(
            &path,
            br#"{"timestamp":"2026-08-30T10:00:01.000Z","type":"event_msg","payload":{"type":"task_started","threadId":"00000000-0000-4000-8000-000000000001","turnId":"live-turn"}}
"#,
        )
        .unwrap();
        service.poll_session_files(&root).unwrap();

        let inner = service.inner.lock().unwrap();
        let active = inner
            .session_active_turns
            .get(task_id)
            .expect("the offset poll should parse the appended lifecycle event");
        assert_eq!(active.turn_id.as_deref(), Some("live-turn"));
        assert_eq!(
            active.observed_at.as_deref(),
            Some("2026-08-30T10:00:01.000Z")
        );
        drop(inner);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_session_turn_overrides_stale_api_state_until_task_complete() {
        let root = std::env::temp_dir().join(format!(
            "agent-monitor-session-active-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        let settings = Settings {
            codex_path: Some(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake-codex.sh")
                    .display()
                    .to_string(),
            ),
            codex_home: Some("/tmp/fake-codex".into()),
            ..Settings::default()
        };
        store.save_settings(&settings).unwrap();
        let service = MonitorService::new(store).unwrap();
        service.sync_now().unwrap();

        std::fs::write(
            &path,
            br#"{"type":"event_msg","payload":{"type":"task_started","threadId":"fake-thread","turnId":"live-turn"}}
"#,
        )
        .unwrap();
        service.process_session_path(path.clone()).unwrap();
        let active = service
            .snapshot()
            .unwrap()
            .tasks
            .into_iter()
            .find(|task| task.task_id == "fake-thread")
            .unwrap();
        assert_eq!(active.status, TaskStatus::Running);
        assert_eq!(active.turn_id.as_deref(), Some("live-turn"));

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"threadId\":\"fake-thread\",\"turnId\":\"live-turn\"}}}}"
        )
        .unwrap();
        drop(file);
        service.process_session_path(path).unwrap();
        let completed = service
            .snapshot()
            .unwrap()
            .tasks
            .into_iter()
            .find(|task| task.task_id == "fake-thread")
            .unwrap();
        assert_eq!(completed.status, TaskStatus::Succeeded);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn baseline_recovers_an_active_turn_without_emitting_history_notifications() {
        let root = std::env::temp_dir().join(format!(
            "agent-monitor-session-recovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path =
            root.join("rollout-2026-08-30T08-15-31-00000000-0000-0000-0000-000000000000.jsonl");
        std::fs::write(
            &path,
            br#"{"type":"event_msg","timestamp":"2026-08-30T08:15:31.067Z","payload":{"type":"item_completed","threadId":"fake-thread","turnId":"live-turn"}}
"#,
        )
        .unwrap();
        let _open_session = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        let settings = Settings {
            codex_path: Some(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake-codex.sh")
                    .display()
                    .to_string(),
            ),
            codex_home: Some("/tmp/fake-codex".into()),
            ..Settings::default()
        };
        store.save_settings(&settings).unwrap();
        let service = MonitorService::new(store).unwrap();
        let receiver = service.subscribe();
        service.baseline_session_files(&root).unwrap();
        service.sync_now().unwrap();
        let task = service
            .snapshot()
            .unwrap()
            .tasks
            .into_iter()
            .find(|task| task.task_id == "fake-thread")
            .unwrap();
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.turn_id.as_deref(), Some("live-turn"));
        assert!(!receiver
            .try_iter()
            .any(|signal| matches!(signal, MonitorSignal::Notification(_))));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn task_complete_event_does_not_assume_success_without_api_reconciliation() {
        let root = std::env::temp_dir().join(format!(
            "agent-monitor-session-terminal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path =
            root.join("rollout-2026-08-30T08-15-31-00000000-0000-0000-0000-000000000000.jsonl");
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        let mut saved = task(TaskStatus::Failed, "old-turn");
        saved.task_id = "fake-thread".into();
        store.save_tasks(&[saved]).unwrap();
        store
            .save_settings(&Settings {
                codex_path: Some("/definitely/missing/codex".into()),
                ..Settings::default()
            })
            .unwrap();
        let service = MonitorService::new(store).unwrap();
        std::fs::write(
            &path,
            br#"{"type":"event_msg","payload":{"type":"task_complete","threadId":"fake-thread","turnId":"live-turn"}}
"#,
        )
        .unwrap();
        service.process_session_path(path).unwrap();
        let task = service
            .snapshot()
            .unwrap()
            .tasks
            .into_iter()
            .find(|task| task.task_id == "fake-thread")
            .unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_user_input_call_sets_and_clears_attention() {
        let root = std::env::temp_dir().join(format!(
            "agent-monitor-session-attention-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        store
            .save_settings(&Settings {
                codex_path: Some(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/fake-codex.sh")
                        .display()
                        .to_string(),
                ),
                codex_home: Some("/tmp/fake-codex".into()),
                ..Settings::default()
            })
            .unwrap();
        let service = MonitorService::new(store).unwrap();
        service.sync_now().unwrap();
        std::fs::write(
            &path,
            br#"{"timestamp":"2026-08-30T08:15:31.067Z","type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"call-1","threadId":"fake-thread","turnId":"live-turn"}}
"#,
        )
        .unwrap();
        service.process_session_path(path.clone()).unwrap();
        let waiting = service
            .snapshot()
            .unwrap()
            .tasks
            .into_iter()
            .find(|task| task.task_id == "fake-thread")
            .unwrap();
        assert_eq!(waiting.status, TaskStatus::NeedsAction);
        assert_eq!(waiting.attention, Some(AttentionKind::UserInput));

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            "{{\"timestamp\":\"2026-08-30T08:15:40.067Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call_output\",\"call_id\":\"call-1\",\"threadId\":\"fake-thread\",\"turnId\":\"live-turn\"}}}}"
        )
        .unwrap();
        drop(file);
        service.process_session_path(path).unwrap();
        let resumed = service
            .snapshot()
            .unwrap()
            .tasks
            .into_iter()
            .find(|task| task.task_id == "fake-thread")
            .unwrap();
        assert_eq!(resumed.status, TaskStatus::Running);
        assert_eq!(resumed.attention, None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn async_question_and_terminal_in_one_batch_retains_input_without_reviving_turn() {
        let root = std::env::temp_dir().join(format!(
            "agent-monitor-batched-input-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        store
            .save_settings(&Settings {
                codex_path: Some(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/fake-codex.sh")
                        .display()
                        .to_string(),
                ),
                codex_home: Some(root.display().to_string()),
                ..Settings::default()
            })
            .unwrap();
        let service = MonitorService::new(store.clone()).unwrap();
        service.sync_now().unwrap();
        let path = root.join("rollout.jsonl");
        let at = now_rfc3339();
        let lines = [
            serde_json::json!({"timestamp":at,"type":"event_msg","payload":{"type":"task_started","threadId":"fake-thread","turnId":"fake-turn"}}),
            serde_json::json!({"timestamp":at,"type":"response_item","payload":{"type":"function_call","name":"request_user_input_async","call_id":"async-call","threadId":"fake-thread","turnId":"fake-turn","arguments":"{\"questions\":[{\"title\":\"PRIVATE\"},{\"title\":\"PRIVATE TWO\"}]}"}}),
            serde_json::json!({"timestamp":at,"type":"response_item","payload":{"type":"function_call_output","call_id":"async-call","output":"{\"accepted\":true}","threadId":"fake-thread","turnId":"fake-turn"}}),
            serde_json::json!({"timestamp":at,"type":"event_msg","payload":{"type":"task_complete","threadId":"fake-thread","turnId":"fake-turn"}}),
        ];
        std::fs::write(
            &path,
            lines.iter().map(|v| format!("{v}\n")).collect::<String>(),
        )
        .unwrap();
        service.process_session_path(path.clone()).unwrap();
        let requests = service.pending_input_requests().unwrap();
        assert_eq!(requests.len(), 1, "an unanswered asynchronous input request must survive a terminal event in the same file batch");
        assert_eq!(requests[0].turn_id, "fake-turn");
        assert_eq!(requests[0].remaining_questions.len(), 2);
        assert_eq!(
            service
                .snapshot()
                .unwrap()
                .tasks
                .iter()
                .find(|t| t.task_id == "fake-thread")
                .unwrap()
                .status,
            TaskStatus::Succeeded
        );
        // Re-reading the same file must neither replay nor duplicate the request.
        service.process_session_path(path.clone()).unwrap();
        assert_eq!(service.pending_input_requests().unwrap().len(), 1);
        let database = root.join("activity.db");
        let services = crate::runtime::Services {
            monitor: service.clone(),
            activity: Arc::new(crate::activity::ActivityService::open(&root, &database).unwrap()),
            quota: Arc::new(crate::quota::QuotaService::new().unwrap()),
            news: Arc::new(crate::reset_news::ResetNews::new(store.clone()).unwrap()),
            passport: Arc::new(crate::passport::CompanionService::new(store.clone()).unwrap()),
            notices: crate::desktop_notices::DesktopNotices::default(),
        };
        assert!(services.sync().unwrap().1.is_empty());
        assert_eq!(
            service.pending_input_requests().unwrap().len(),
            1,
            "wait for exact ledger identity rather than substitute another round"
        );
        let mut record = serde_json::json!({"id":"codex:fake-thread:fake-turn","client":"codex","sessionId":"fake-thread","turnId":"fake-turn","rootId":null,"parentId":null,"isChild":false,"title":"Native task title","projectPath":"/tmp/project","model":null,"provider":null,"turnIndex":1,"status":"succeeded","attention":null,"startedAt":at,"completedAt":at,"observedAt":at,"durationMs":0,"activeMs":null,"waitingMs":null,"tokens":crate::activity::TokenUsage::default(),"live":false,"childCount":0});
        let db = rusqlite::Connection::open(&database).unwrap();
        db.execute("INSERT INTO activity_executions VALUES(?1,'codex','fake-thread','fake-turn',?2,?2,?3,'{}')",rusqlite::params![record["id"].as_str(),at,record.to_string()]).unwrap();
        let (snapshot, fresh) = services.sync().unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].kind, "needs_action");
        assert_eq!(fresh[0].title, "Native task title");
        assert_eq!(snapshot.executions[0].status, TaskStatus::Succeeded);
        assert!(service.pending_input_requests().unwrap().is_empty());
        assert!(services.sync().unwrap().1.is_empty());
        assert_eq!(store.load_alerts().unwrap().len(), 1);
        // A distinct child round uses the same path but must stay silent.
        record["id"] = serde_json::json!("codex:fake-thread:child-turn");
        record["turnId"] = serde_json::json!("child-turn");
        record["isChild"] = serde_json::json!(true);
        db.execute("INSERT INTO activity_executions VALUES(?1,'codex','fake-thread','child-turn',?2,?2,?3,'{}')",rusqlite::params![record["id"].as_str(),at,record.to_string()]).unwrap();
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file,"{}",serde_json::json!({"timestamp":now_rfc3339(),"type":"response_item","payload":{"type":"function_call","name":"request_user_input_async","call_id":"child-question","threadId":"fake-thread","turnId":"child-turn","arguments":"{\"questions\":[{}]}"}})).unwrap();
        writeln!(file,"{}",serde_json::json!({"timestamp":now_rfc3339(),"type":"response_item","payload":{"type":"function_call_output","call_id":"child-question","output":"{\"accepted\":true}","threadId":"fake-thread","turnId":"child-turn"}})).unwrap();
        file.flush().unwrap();
        service.process_session_path(path).unwrap();
        assert_eq!(service.pending_input_requests().unwrap().len(), 1);
        assert!(services.sync().unwrap().1.is_empty());
        assert!(service.pending_input_requests().unwrap().is_empty());
        assert_eq!(store.load_alerts().unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn diagnostic_version_probe_does_not_hold_the_collector_lock() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "monitor-diagnostics-lock-{}-{}.sh",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let marker = path.with_extension("sh.started");
        let release = path.with_extension("sh.release");
        std::fs::write(
            &path,
            "#!/bin/sh\n: > \"$0.started\"\nwhile [ ! -e \"$0.release\" ]; do sleep 0.01; done\nprintf 'codex-cli fixture\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        store
            .save_settings(&Settings {
                codex_path: Some(path.display().to_string()),
                ..Settings::default()
            })
            .unwrap();
        let service = MonitorService::new(store).unwrap();
        let worker_service = service.clone();
        let worker = thread::spawn(move || worker_service.diagnostics().unwrap());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.is_file() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let started = marker.is_file();
        let collector_available = service.inner.try_lock().is_ok();
        std::fs::write(&release, "").unwrap();
        let diagnostics = worker.join().unwrap();
        std::fs::remove_file(&path).unwrap();
        if started {
            std::fs::remove_file(marker).unwrap();
        }
        std::fs::remove_file(release).unwrap();
        assert!(started);
        assert!(
            collector_available,
            "a diagnostic subprocess must not block collection and snapshots"
        );
        assert_eq!(
            diagnostics.codex_version.as_deref(),
            Some("codex-cli fixture")
        );
    }

    #[test]
    fn unlisted_session_activity_does_not_repeat_full_discovery_for_each_tool_event() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!(
            "agent-monitor-unlisted-session-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        store
            .save_settings(&Settings {
                codex_path: Some(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/fake-codex.sh")
                        .display()
                        .to_string(),
                ),
                codex_home: Some(root.display().to_string()),
                ..Settings::default()
            })
            .unwrap();
        let service = MonitorService::new(store).unwrap();
        service.sync_now().unwrap();
        let calls = || {
            service
                .inner
                .lock()
                .unwrap()
                .client
                .as_ref()
                .unwrap()
                .request_count()
        };
        let path = root.join("rollout.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        let mut append = |kind: &str, turn: &str| {
            writeln!(file,"{}",serde_json::json!({"timestamp":now_rfc3339(),"type":"event_msg","payload":{"type":kind,"threadId":"unlisted-thread","turnId":turn}})).unwrap();
            file.flush().unwrap();
            service.process_session_path(path.clone()).unwrap();
        };
        let initial = calls();
        append("task_started", "live-turn");
        let discovered = calls();
        assert!(
            discovered > initial,
            "a new session must attempt native discovery"
        );
        let deadline = service.inner.lock().unwrap().next_retry_at;
        for _ in 0..5 {
            append("item_completed", "live-turn");
        }
        assert_eq!(
            calls(),
            discovered,
            "a session omitted by thread/list must not relist all conversations for each item"
        );
        assert_eq!(service.inner.lock().unwrap().next_retry_at, deadline);
        service.sync_now().unwrap();
        let refreshed = calls();
        append("item_completed", "live-turn");
        assert_eq!(
            calls(),
            refreshed,
            "scheduled discovery remains available without restarting the event flood"
        );
        drop(append);
        writeln!(file,"{}",serde_json::json!({"timestamp":now_rfc3339(),"type":"response_item","payload":{"type":"function_call","name":"request_user_input_async","call_id":"unlisted-question","threadId":"unlisted-thread","turnId":"live-turn","arguments":"{\"questions\":[{}]}"}})).unwrap();
        writeln!(file,"{}",serde_json::json!({"timestamp":now_rfc3339(),"type":"response_item","payload":{"type":"function_call_output","call_id":"unlisted-question","output":"{\"accepted\":true}","threadId":"unlisted-thread","turnId":"live-turn"}})).unwrap();
        file.flush().unwrap();
        service.process_session_path(path.clone()).unwrap();
        assert_eq!(calls(), refreshed);
        assert_eq!(
            service.pending_input_requests().unwrap().len(),
            1,
            "avoiding redundant discovery must not discard user-input reminders"
        );
        let mut append = |kind: &str, turn: &str| {
            writeln!(file,"{}",serde_json::json!({"timestamp":now_rfc3339(),"type":"event_msg","payload":{"type":kind,"threadId":"unlisted-thread","turnId":turn}})).unwrap();
            file.flush().unwrap();
            service.process_session_path(path.clone()).unwrap();
        };
        append("task_complete", "live-turn");
        assert!(calls() > refreshed, "terminal events must still reconcile");
        let terminal_calls = calls();
        append("task_started", "next-turn");
        assert!(
            calls() > terminal_calls,
            "the next turn must attempt discovery immediately"
        );
        drop(append);
        drop(file);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn known_session_activity_updates_attention_without_relisting_every_tool_event() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!(
            "agent-monitor-no-rpc-flood-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        store
            .save_settings(&Settings {
                codex_path: Some(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/fake-codex.sh")
                        .display()
                        .to_string(),
                ),
                codex_home: Some(root.display().to_string()),
                ..Settings::default()
            })
            .unwrap();
        let service = MonitorService::new(store).unwrap();
        service.sync_now().unwrap();
        let calls = || {
            service
                .inner
                .lock()
                .unwrap()
                .client
                .as_ref()
                .unwrap()
                .request_count()
        };
        let baseline_calls = calls();
        let deadline = service.inner.lock().unwrap().next_retry_at;
        let path = root.join("rollout.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file,"{}",serde_json::json!({"timestamp":now_rfc3339(),"type":"event_msg","payload":{"type":"task_started","threadId":"fake-thread","turnId":"live-turn"}})).unwrap();
        writeln!(file,"{}",serde_json::json!({"timestamp":now_rfc3339(),"type":"response_item","payload":{"type":"function_call","name":"request_user_input_async","call_id":"question","threadId":"fake-thread","turnId":"live-turn","arguments":"{\"questions\":[{\"question\":\"PRIVATE\"}]}"}})).unwrap();
        file.flush().unwrap();
        service.process_session_path(path.clone()).unwrap();
        for index in 0..5 {
            writeln!(file,"{}",serde_json::json!({"timestamp":now_rfc3339(),"type":"response_item","payload":{"type":"function_call_output","call_id":format!("unrelated-{index}"),"threadId":"fake-thread","turnId":"live-turn"}})).unwrap();
            file.flush().unwrap();
            service.process_session_path(path.clone()).unwrap();
        }
        let snapshot = service.snapshot().unwrap();
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id == "fake-thread")
            .unwrap();
        assert_eq!(task.status, TaskStatus::NeedsAction);
        assert_eq!(task.attention, Some(AttentionKind::UserInput));
        assert_eq!(
            calls(),
            baseline_calls,
            "normal file events must reuse known metadata and must not relist every conversation"
        );
        assert_eq!(
            service.inner.lock().unwrap().next_retry_at,
            deadline,
            "file activity must preserve the scheduled metadata reconciliation"
        );
        writeln!(file,"{}",serde_json::json!({"timestamp":now_rfc3339(),"type":"event_msg","payload":{"type":"task_complete","threadId":"fake-thread","turnId":"live-turn"}})).unwrap();
        file.flush().unwrap();
        service.process_session_path(path).unwrap();
        assert!(
            calls() > baseline_calls,
            "terminal state still requires native API reconciliation"
        );
        assert_eq!(
            service.snapshot().unwrap().tasks[0].status,
            TaskStatus::Succeeded
        );
        drop(file);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejected_async_question_does_not_leave_waiting_or_notify_after_retry_answer() {
        let service =
            MonitorService::new(Arc::new(SnapshotStore::open_in_memory().unwrap())).unwrap();
        let mut inner = service.inner.lock().unwrap();
        let apply = |inner: &mut MonitorInner, payload: Value| {
            let mut event = crate::codex::parse_session_event(&serde_json::json!({
                "timestamp":now_rfc3339(),"type":"response_item","payload":payload
            }))
            .unwrap();
            event.task_id = Some("thread".into());
            track_input_request(inner, &event);
            apply_session_event(inner, &event, None);
        };
        apply(
            &mut inner,
            serde_json::json!({"type":"task_started","turn_id":"turn"}),
        );
        for (id, output) in [
            (
                "invalid",
                "failed to parse function arguments: unknown field `question`",
            ),
            ("valid", "{\"accepted\":true}"),
        ] {
            apply(
                &mut inner,
                serde_json::json!({"type":"function_call","name":"request_user_input_async","call_id":id,"arguments":"{\"questions\":[{}]}"}),
            );
            apply(
                &mut inner,
                serde_json::json!({"type":"function_call_output","call_id":id,"output":output}),
            );
        }
        apply(
            &mut inner,
            serde_json::json!({"type":"message","role":"user","content":[{"type":"input_text","text":format!(
                "<send_user_message_question_reply>{}</send_user_message_question_reply>",
                serde_json::json!([{"questionItemId":"[\"request_user_input_async\",\"valid\",0]","answer":"done"}])
            )}]}),
        );
        assert_eq!(
            inner.session_active_turns["thread"].attention, None,
            "answered retry must not retain rejected question"
        );
        assert!(
            inner.input_requests.is_empty(),
            "rejected question must never remain queued for notification"
        );
    }

    #[test]
    fn async_user_input_ack_is_not_an_answer_and_all_matching_questions_must_resolve() {
        let service =
            MonitorService::new(Arc::new(SnapshotStore::open_in_memory().unwrap())).unwrap();
        let mut inner = service.inner.lock().unwrap();
        let apply = |inner: &mut MonitorInner, payload: Value| {
            let mut event = crate::codex::parse_session_event(&serde_json::json!({
                "type":"response_item","payload":payload
            }))
            .unwrap();
            event.task_id = Some("fake-thread".into());
            apply_session_event(inner, &event, None);
        };
        let ask = |id: &str, count| {
            serde_json::json!({
                "type":"function_call","name":"request_user_input_async","call_id":id,
                "arguments":serde_json::json!({"questions":(0..count).map(|_|serde_json::json!({"question":"PRIVATE QUESTION"})).collect::<Vec<_>>()} ).to_string()
            })
        };
        let reply = |id: &str, index| {
            serde_json::json!({
                "type":"message","role":"user","content":[{"type":"input_text","text":format!(
                    "<send_user_message_question_reply>{}</send_user_message_question_reply>",
                    serde_json::json!([{"questionItemId":serde_json::json!(["request_user_input_async",id,index]).to_string(),"answer":"PRIVATE ANSWER"}])
                )}]
            })
        };
        apply(&mut inner, ask("call-1", 2));
        assert_eq!(
            inner.session_active_turns["fake-thread"].attention,
            Some(AttentionKind::UserInput)
        );
        apply(
            &mut inner,
            serde_json::json!({"type":"function_call_output","call_id":"call-1","output":"{\"accepted\":true}"}),
        );
        assert_eq!(
            inner.session_active_turns["fake-thread"].attention,
            Some(AttentionKind::UserInput)
        );
        apply(&mut inner, ask("call-2", 1));
        apply(&mut inner, reply("call-1", 0));
        apply(&mut inner, reply("call-other", 0));
        assert_eq!(
            inner.session_active_turns["fake-thread"].attention,
            Some(AttentionKind::UserInput)
        );
        apply(&mut inner, reply("call-1", 1));
        assert_eq!(
            inner.session_active_turns["fake-thread"].attention,
            Some(AttentionKind::UserInput)
        );
        apply(&mut inner, reply("call-2", 0));
        assert_eq!(inner.session_active_turns["fake-thread"].attention, None);
    }

    #[test]
    fn async_question_reply_never_reopens_a_terminal_or_unrelated_turn() {
        let service =
            MonitorService::new(Arc::new(SnapshotStore::open_in_memory().unwrap())).unwrap();
        let mut inner = service.inner.lock().unwrap();
        let apply = |inner: &mut MonitorInner, payload: Value| {
            let mut event = crate::codex::parse_session_event(
                &serde_json::json!({"type":"response_item","payload":payload}),
            )
            .unwrap();
            event.task_id = Some("thread".into());
            apply_session_event(inner, &event, None)
        };
        let reply = serde_json::json!({"type":"message","role":"user","content":[{
            "type":"input_text","text":format!("<send_user_message_question_reply>{}</send_user_message_question_reply>",
                serde_json::json!([{"questionItemId":serde_json::json!(["request_user_input_async","question",0]).to_string(),"answer":""}]))
        }]});
        assert_eq!(apply(&mut inner, reply.clone()), None);
        assert!(inner.session_active_turns.is_empty());
        apply(
            &mut inner,
            serde_json::json!({"type":"function_call","name":"request_user_input_async","call_id":"question","turn_id":"turn","arguments":"{\"questions\":[{}]}"}),
        );
        apply(
            &mut inner,
            serde_json::json!({"type":"task_complete","turn_id":"turn"}),
        );
        assert!(inner.session_active_turns.is_empty());
        assert_eq!(apply(&mut inner, reply), None);
        assert!(inner.session_active_turns.is_empty());
    }

    #[test]
    fn queued_input_matches_session_call_and_question_and_suppresses_answered_requests() {
        let service =
            MonitorService::new(Arc::new(SnapshotStore::open_in_memory().unwrap())).unwrap();
        let mut inner = service.inner.lock().unwrap();
        let event = |inner: &mut MonitorInner, session: &str, at: &str, payload: Value| {
            let mut event = crate::codex::parse_session_event(
                &serde_json::json!({"type":"response_item","timestamp":at,"payload":payload}),
            )
            .unwrap();
            event.task_id = Some(session.into());
            track_input_request(inner, &event);
            apply_session_event(inner, &event, None);
        };
        let ask = |call: &str| serde_json::json!({"type":"function_call","name":"request_user_input_async","call_id":call,"turn_id":"turn","arguments":"{\"questions\":[{\"title\":\"PRIVATE\"},{\"title\":\"SECRET\"}]}"});
        let reply = |call: &str, index: usize| serde_json::json!({"type":"message","role":"user","content":[{"type":"input_text","text":format!("<send_user_message_question_reply>{}</send_user_message_question_reply>",serde_json::json!([{"questionItemId":serde_json::json!(["request_user_input_async",call,index]).to_string(),"answer":""}]))}]});
        event(&mut inner, "chat", "2020-01-01T00:00:00Z", ask("old"));
        assert!(
            inner.input_requests.is_empty(),
            "historical requests must not replay"
        );
        let at = now_rfc3339();
        event(&mut inner, "chat", &at, ask("first"));
        event(&mut inner, "chat", &at, ask("second"));
        event(
            &mut inner,
            "chat",
            &at,
            serde_json::json!({"type":"task_complete","turn_id":"turn"}),
        );
        assert!(inner.session_active_turns.is_empty());
        event(&mut inner, "other-chat", &at, reply("first", 0));
        event(&mut inner, "chat", &at, reply("first", 9));
        assert_eq!(inner.input_requests[0].remaining_questions.len(), 2);
        event(&mut inner, "chat", &at, reply("first", 0));
        assert_eq!(inner.input_requests[0].remaining_questions.len(), 1);
        event(&mut inner, "chat", &at, reply("first", 1));
        assert_eq!(inner.input_requests.len(), 1);
        assert_eq!(inner.input_requests[0].call_id, "second");
        assert!(
            inner.session_active_turns.is_empty(),
            "an answer for an ended round must not revive it"
        );
        event(&mut inner, "chat", &at, reply("second", 0));
        event(&mut inner, "chat", &at, reply("second", 1));
        assert!(
            inner.input_requests.is_empty(),
            "a question fully answered before publication must not produce a stale input alert"
        );
    }

    #[test]
    fn approval_and_sync_input_require_the_matching_tool_result() {
        let service =
            MonitorService::new(Arc::new(SnapshotStore::open_in_memory().unwrap())).unwrap();
        let mut inner = service.inner.lock().unwrap();
        let apply = |inner: &mut MonitorInner, payload: Value| {
            let mut event = crate::codex::parse_session_event(
                &serde_json::json!({"type":"event_msg","payload":payload}),
            )
            .unwrap();
            event.task_id = Some("thread".into());
            apply_session_event(inner, &event, None);
        };
        apply(
            &mut inner,
            serde_json::json!({"type":"exec_approval_request","call_id":"approval"}),
        );
        apply(
            &mut inner,
            serde_json::json!({"type":"function_call_output","call_id":"other"}),
        );
        assert_eq!(
            inner.session_active_turns["thread"].attention,
            Some(AttentionKind::Approval)
        );
        apply(
            &mut inner,
            serde_json::json!({"type":"function_call","name":"request_user_input","call_id":"question"}),
        );
        apply(
            &mut inner,
            serde_json::json!({"type":"function_call_output","call_id":"question"}),
        );
        assert_eq!(
            inner.session_active_turns["thread"].attention,
            Some(AttentionKind::Approval)
        );
        apply(
            &mut inner,
            serde_json::json!({"type":"function_call_output","call_id":"approval"}),
        );
        assert_eq!(inner.session_active_turns["thread"].attention, None);
    }

    #[test]
    fn internal_tool_metadata_cannot_replace_the_codex_turn_id() {
        let root = std::env::temp_dir().join(format!(
            "agent-monitor-session-turn-id-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let service =
            MonitorService::new(Arc::new(SnapshotStore::open_in_memory().unwrap())).unwrap();
        service.inner.lock().unwrap().next_retry_at = Instant::now() + Duration::from_secs(30);
        std::fs::write(
            &path,
            br#"{"timestamp":"2026-08-30T11:02:20.000Z","type":"event_msg","payload":{"type":"item_completed","threadId":"fake-thread","turnId":"provider-turn"}}
{"timestamp":"2026-08-30T11:02:21.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call-1","internal_chat_message_metadata_passthrough":{"turn_id":"internal-turn"}}}
"#,
        )
        .unwrap();

        service.process_session_path(path).unwrap();
        let inner = service.inner.lock().unwrap();
        let active = inner
            .session_active_turns
            .get("fake-thread")
            .expect("the session should remain active");
        assert_eq!(active.turn_id.as_deref(), Some("provider-turn"));
        drop(inner);
        std::fs::remove_dir_all(root).unwrap();
    }
}
