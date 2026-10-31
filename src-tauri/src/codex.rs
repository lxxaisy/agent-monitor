use crate::model::{
    map_task_status, now_rfc3339, plan_counts_from_steps, progress_from_plan, timestamp_to_rfc3339,
    AttentionKind, PlanCounts, PlanStep, ProgressBasis, ProgressConfidence, TaskDetail,
    TaskSnapshot, TaskStatus, TurnSummary,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

pub const SOURCE_KINDS: [&str; 10] = [
    "cli",
    "vscode",
    "exec",
    "appServer",
    "subAgent",
    "subAgentReview",
    "subAgentCompact",
    "subAgentThreadSpawn",
    "subAgentOther",
    "unknown",
];

// These variables describe the Codex task that launched the current process.
// A monitor app-server is an independent, read-only client and must never be
// mistaken for that task or inherit its private app plumbing.
#[cfg(test)]
const CODEX_CONTEXT_ENV_VARS: &[&str] = &[
    "CODEX_THREAD_ID",
    "CODEX_SESSION_ID",
    "CODEX_PARENT_THREAD_ID",
    "CODEX_PARENT_SESSION_ID",
    "CODEX_TURN_ID",
    "CODEX_TASK_ID",
    "CODEX_SHELL",
    "CODEX_CI",
    "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
    "CODEX_PERMISSION_PROFILE",
    "CODEX_APP_TOOLS_PIPE_PATH",
    "CODEX_MCP_NODE_PATH",
    "CODEX_BROWSER_USE_NODE_PATH",
    "CODEX_ELECTRON_RESOURCES_PATH",
    "CODEX_CLI_PATH",
];

#[derive(Debug, Error)]
pub enum CodexError {
    #[error("codex executable not found: {0}")]
    NotFound(String),
    #[error("failed to start codex app-server: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("app-server connection closed")]
    Disconnected,
    #[error("app-server connection closed: {0}")]
    DisconnectedWithReason(String),
    #[error("app-server request timed out: {0}")]
    Timeout(String),
    #[error("app-server returned an error: {0}")]
    Rpc(String),
    #[error("invalid app-server response: {0}")]
    Protocol(String),
}

#[derive(Debug, Clone)]
pub struct RpcEnvelope {
    pub id: Option<Value>,
    pub method: Option<String>,
    pub params: Option<Value>,
    pub result: Option<Value>,
    pub error: Option<Value>,
}

impl RpcEnvelope {
    fn parse(line: &str) -> Result<Self, CodexError> {
        let value: Value = serde_json::from_str(line)
            .map_err(|error| CodexError::Protocol(format!("invalid JSON: {error}")))?;
        let object = value
            .as_object()
            .ok_or_else(|| CodexError::Protocol("message is not an object".into()))?;
        Ok(Self {
            id: object.get("id").cloned(),
            method: object
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_string),
            params: object.get("params").cloned(),
            result: object.get("result").cloned(),
            error: object.get("error").cloned(),
        })
    }

    pub fn is_notification(&self) -> bool {
        self.id.is_none() && self.method.is_some()
    }
}

pub struct CodexClient {
    stdin: Mutex<std::process::ChildStdin>,
    incoming: Mutex<Receiver<RpcEnvelope>>,
    events: Mutex<VecDeque<RpcEnvelope>>,
    child: Mutex<Child>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    stderr_done: Arc<AtomicBool>,
    next_id: AtomicU64,
}

impl CodexClient {
    #[cfg(test)]
    pub(crate) fn request_count(&self) -> u64 {
        self.next_id.load(Ordering::Relaxed) - 1
    }

    pub fn connect(binary: &Path, codex_home: &Path) -> Result<Self, CodexError> {
        let mut command = app_server_command(binary, codex_home);
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CodexError::Protocol("app-server stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CodexError::Protocol("app-server stdout unavailable".into()))?;
        let stderr = child.stderr.take();

        let (sender, receiver) = mpsc::channel();
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_done = Arc::new(AtomicBool::new(stderr.is_none()));
        thread::Builder::new()
            .name("agent-monitor-codex-reader".into())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(line) if !line.trim().is_empty() => {
                            if let Ok(message) = RpcEnvelope::parse(&line) {
                                if sender.send(message).is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            })
            .map_err(CodexError::Spawn)?;

        if let Some(stderr) = stderr {
            let stderr_tail_for_reader = Arc::clone(&stderr_tail);
            let stderr_done_for_reader = Arc::clone(&stderr_done);
            let _ = thread::Builder::new()
                .name("agent-monitor-codex-stderr".into())
                .spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines() {
                        match line {
                            Ok(line) if !line.trim().is_empty() => {
                                if let Ok(mut tail) = stderr_tail_for_reader.lock() {
                                    tail.push_back(sanitize_diagnostic(&line));
                                    while tail.len() > 8 {
                                        tail.pop_front();
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    stderr_done_for_reader.store(true, Ordering::Release);
                });
        }

        let client = Self {
            stdin: Mutex::new(stdin),
            incoming: Mutex::new(receiver),
            events: Mutex::new(VecDeque::new()),
            child: Mutex::new(child),
            stderr_tail,
            stderr_done,
            next_id: AtomicU64::new(1),
        };
        client.initialize()?;
        Ok(client)
    }

    fn connection_error(&self) -> CodexError {
        // stdout can close a few milliseconds before stderr. Give the stderr
        // reader a short window so launch-agent failures retain their cause.
        let deadline = Instant::now() + Duration::from_millis(100);
        while !self.stderr_done.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::yield_now();
        }
        let exit = self
            .child
            .lock()
            .ok()
            .and_then(|mut child| child.try_wait().ok().flatten())
            .map(|status| status.to_string());
        let stderr = self
            .stderr_tail
            .lock()
            .ok()
            .and_then(|tail| tail.back().cloned());
        let reason = match (exit, stderr) {
            (Some(exit), Some(stderr)) => format!("{exit}; {stderr}"),
            (Some(exit), None) => exit,
            (None, Some(stderr)) => stderr,
            (None, None) => String::new(),
        };
        if reason.is_empty() {
            CodexError::Disconnected
        } else {
            CodexError::DisconnectedWithReason(truncate_text(&reason, 320))
        }
    }

    fn initialize(&self) -> Result<(), CodexError> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "agent_monitor",
                    "title": "Agent Monitor",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }),
        )?;
        self.notify("initialized", json!({}))
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<(), CodexError> {
        let message = json!({ "method": method, "params": params });
        let mut stdin = self.stdin.lock().map_err(|_| CodexError::Disconnected)?;
        writeln!(stdin, "{}", message).map_err(CodexError::Spawn)?;
        stdin.flush().map_err(CodexError::Spawn)
    }

    pub fn request(&self, method: &str, params: Value) -> Result<Value, CodexError> {
        self.request_with_timeout(method, params, Duration::from_secs(30))
    }

    pub fn account_rate_limits(&self) -> Result<Value, CodexError> {
        self.request_with_timeout("account/rateLimits/read", json!({}), Duration::from_secs(5))
    }

    fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CodexError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({ "id": id, "method": method, "params": params });
        {
            let mut stdin = self.stdin.lock().map_err(|_| CodexError::Disconnected)?;
            writeln!(stdin, "{}", message).map_err(CodexError::Spawn)?;
            stdin.flush().map_err(CodexError::Spawn)?;
        }

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CodexError::Timeout(method.to_string()));
            }
            let received = self
                .incoming
                .lock()
                .map_err(|_| CodexError::Disconnected)?
                .recv_timeout(remaining.min(Duration::from_millis(500)));
            match received {
                Ok(response) => {
                    if response.method.is_none()
                        && response
                            .id
                            .as_ref()
                            .is_some_and(|value| value == &json!(id))
                    {
                        if let Some(error) = response.error {
                            let text = error
                                .get("message")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| error.to_string());
                            return Err(CodexError::Rpc(truncate_text(&text, 280)));
                        }
                        return response.result.ok_or_else(|| {
                            CodexError::Protocol(format!("{method} response missing result"))
                        });
                    }
                    if response.is_notification() {
                        if let Ok(mut events) = self.events.lock() {
                            events.push_back(response);
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if !self.is_alive() {
                        return Err(self.connection_error());
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Err(self.connection_error()),
            }
        }
    }

    pub fn drain_events(&self) -> Vec<RpcEnvelope> {
        let mut events = match self.events.lock() {
            Ok(events) => events,
            Err(_) => return Vec::new(),
        };
        while let Ok(message) = self
            .incoming
            .lock()
            .ok()
            .map(|receiver| receiver.try_recv())
            .unwrap_or(Err(mpsc::TryRecvError::Disconnected))
        {
            if message.is_notification() {
                events.push_back(message);
            }
        }
        events.drain(..).collect()
    }

    pub fn is_alive(&self) -> bool {
        self.child
            .lock()
            .map(|mut child| {
                child
                    .try_wait()
                    .map(|status| status.is_none())
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    pub fn list_threads(&self, archived: bool) -> Result<Vec<ThreadRecord>, CodexError> {
        let mut cursor: Option<String> = None;
        let mut records = Vec::new();
        let mut seen = std::collections::HashSet::new();
        loop {
            let mut params = Map::new();
            params.insert("archived".into(), Value::Bool(archived));
            params.insert(
                "sourceKinds".into(),
                Value::Array(
                    SOURCE_KINDS
                        .iter()
                        .map(|kind| Value::String((*kind).into()))
                        .collect(),
                ),
            );
            params.insert("limit".into(), Value::Number(100.into()));
            if let Some(value) = cursor.as_ref() {
                params.insert("cursor".into(), Value::String(value.clone()));
            }
            let result = self.request("thread/list", Value::Object(params))?;
            let data = result
                .get("data")
                .or_else(|| result.get("threads"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for value in data {
                if let Some(mut record) = ThreadRecord::from_metadata_value(value) {
                    // `archived` is a thread/list filter and is not guaranteed to be
                    // repeated on every thread object returned by every Codex version.
                    record.archived = archived;
                    if seen.insert(record.id.clone()) {
                        records.push(record);
                    }
                }
            }
            let next = result
                .get("nextCursor")
                .or_else(|| result.get("next_cursor"))
                .and_then(Value::as_str)
                .map(str::to_string);
            match next {
                Some(next) if !next.is_empty() && cursor.as_deref() != Some(next.as_str()) => {
                    cursor = Some(next)
                }
                _ => break,
            }
        }
        Ok(records)
    }

    pub fn read_thread(&self, thread_id: &str) -> Result<ThreadRecord, CodexError> {
        let result = self.request(
            "thread/read",
            json!({ "threadId": thread_id, "includeTurns": false }),
        )?;
        let record = result
            .get("thread")
            .cloned()
            .and_then(ThreadRecord::from_metadata_value)
            .ok_or_else(|| CodexError::Protocol("thread/read response missing thread".into()))?;
        self.hydrate_latest_turn(record)
    }

    fn hydrate_latest_turn(&self, mut record: ThreadRecord) -> Result<ThreadRecord, CodexError> {
        record.turns = self.list_latest_turn(&record.id)?.into_iter().collect();
        Ok(record)
    }

    pub fn list_latest_turn(&self, thread_id: &str) -> Result<Option<Value>, CodexError> {
        let result = self.request_with_timeout(
            "thread/turns/list",
            json!({
                "threadId": thread_id,
                "itemsView": "notLoaded",
                "sortDirection": "desc",
                "limit": 1
            }),
            Duration::from_secs(5),
        )?;
        Ok(result
            .get("data")
            .or_else(|| result.get("turns"))
            .and_then(Value::as_array)
            .and_then(|turns| turns.first())
            .cloned())
    }

    pub fn discover(&self, archived: bool) -> Result<Vec<TaskDetail>, CodexError> {
        let records = self.list_threads(archived)?;
        let mut details = Vec::with_capacity(records.len());
        for record in records {
            let record = match self.hydrate_latest_turn(record.clone()) {
                Ok(detail) => detail,
                Err(_) => record,
            };
            details.push(record.into_detail());
        }
        Ok(details)
    }

    pub fn discover_cached(
        &self,
        archived: bool,
        cached: &HashMap<String, TaskDetail>,
    ) -> Result<Vec<TaskDetail>, CodexError> {
        let records = self.list_threads(archived)?;
        let mut details = Vec::with_capacity(records.len());
        for record in records {
            if let Some(previous) = cached.get(&record.id) {
                if record.updated_at.is_some()
                    && previous.snapshot.updated_at == record.updated_at.clone().unwrap_or_default()
                    && previous.snapshot.archived == record.archived
                    && (previous.recent_turn.is_some()
                        || matches!(
                            previous.snapshot.status,
                            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Interrupted
                        ))
                {
                    details.push(previous.clone());
                    continue;
                }
            }
            let detail = match self.hydrate_latest_turn(record.clone()) {
                Ok(detail) => detail.into_detail(),
                Err(_) => cached
                    .get(&record.id)
                    .map(|previous| merge_cached_metadata(previous, &record))
                    .unwrap_or_else(|| record.into_detail()),
            };
            details.push(detail);
        }
        Ok(details)
    }

    /// Restore archived threads from the lightweight list response. Archived work is
    /// intentionally read-only in the monitor, so fetching every historical thread's
    /// turns would add a large startup cost without improving its visible state.
    pub fn discover_archived_cached(
        &self,
        cached: &HashMap<String, TaskDetail>,
    ) -> Result<Vec<TaskDetail>, CodexError> {
        let records = self.list_threads(true)?;
        let mut details = Vec::with_capacity(records.len());
        for record in records {
            if let Some(previous) = cached.get(&record.id) {
                if record.updated_at.is_some()
                    && previous.snapshot.updated_at == record.updated_at.clone().unwrap_or_default()
                    && previous.snapshot.archived == record.archived
                {
                    details.push(previous.clone());
                    continue;
                }
            }
            let detail = cached
                .get(&record.id)
                .map(|previous| merge_cached_metadata(previous, &record))
                .unwrap_or_else(|| record.into_detail());
            details.push(detail);
        }
        Ok(details)
    }
}

pub(crate) fn merge_cached_metadata(previous: &TaskDetail, record: &ThreadRecord) -> TaskDetail {
    let mut detail = previous.clone();
    detail.snapshot.parent_task_id = record.parent_thread_id.clone();
    detail.snapshot.project_path = record.cwd.clone();
    detail.snapshot.source = record.source.clone();
    detail.snapshot.archived = record.archived;
    if let Some(title) = record.title.as_ref() {
        detail.snapshot.title = title.clone();
    }
    if let Some(updated_at) = record.updated_at.as_ref() {
        detail.snapshot.updated_at = updated_at.clone();
    }
    detail
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut text: String = value.chars().take(max_chars).collect();
    if value.chars().count() > max_chars {
        text.push('…');
    }
    text
}

impl Drop for CodexClient {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn app_server_command(binary: &Path, codex_home: &Path) -> Command {
    let mut command = Command::new(binary);
    clear_codex_context_environment(&mut command);
    command
        .arg("app-server")
        .arg("--listen")
        .arg("stdio://")
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    configure_runtime_environment(&mut command, codex_home);
    command
}

fn clear_codex_context_environment(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CODEX_") {
            command.env_remove(key);
        }
    }
}

fn configure_runtime_environment(command: &mut Command, codex_home: &Path) {
    // Login agents receive a deliberately small PATH. The npm Codex entrypoint
    // uses `/usr/bin/env node`, so include the user's managed runtimes before
    // spawning it instead of relying on the shell that launched the app.
    let home = dirs::home_dir().or_else(|| codex_home.parent().map(Path::to_path_buf));
    if let Some(home) = home {
        if std::env::var_os("HOME").is_none() {
            command.env("HOME", &home);
        }
        if let Some(path) = augmented_path(&home, std::env::var_os("PATH")) {
            command.env("PATH", path);
        }
    }
}

fn augmented_path(home: &Path, inherited: Option<OsString>) -> Option<OsString> {
    let preferred = [
        home.join(".n/bin"),
        home.join(".npm-global/bin"),
        home.join(".local/bin"),
        home.join("Library/pnpm"),
        home.join(".cargo/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
    ];
    let mut paths = Vec::new();
    let inherited_paths = inherited
        .as_deref()
        .map(std::env::split_paths)
        .into_iter()
        .flatten();
    for path in preferred.into_iter().chain(inherited_paths) {
        if !paths.iter().any(|existing| existing == &path) {
            paths.push(path);
        }
    }
    std::env::join_paths(paths).ok()
}

fn sanitize_diagnostic(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lower = trimmed.to_ascii_lowercase();
    if [
        "api_key",
        "apikey",
        "authorization",
        "cookie",
        "secret",
        "token",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return "Codex 子进程返回了包含受保护信息的启动错误".into();
    }
    truncate_text(trimmed, 280)
}

#[derive(Debug, Clone)]
pub struct ThreadRecord {
    pub id: String,
    pub parent_thread_id: Option<String>,
    pub title: Option<String>,
    pub cwd: String,
    pub source: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub archived: bool,
    pub status: Option<Value>,
    pub turns: Vec<Value>,
}

impl ThreadRecord {
    #[cfg(test)]
    pub(crate) fn from_value(value: Value) -> Option<Self> {
        Self::from_value_inner(value, true)
    }

    fn from_metadata_value(value: Value) -> Option<Self> {
        Self::from_value_inner(value, false)
    }

    fn from_value_inner(value: Value, include_turns: bool) -> Option<Self> {
        let object = value.as_object()?;
        let id = object.get("id")?.as_str()?.to_string();
        let title = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|text| !text.is_empty());
        let turns = if include_turns {
            object
                .get("turns")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Some(Self {
            id,
            parent_thread_id: object
                .get("parentThreadId")
                .and_then(Value::as_str)
                .map(str::to_string),
            title,
            cwd: object
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            source: normalize_source(object.get("source")),
            created_at: timestamp_to_rfc3339(object.get("createdAt")),
            updated_at: timestamp_to_rfc3339(object.get("updatedAt")),
            archived: object
                .get("archived")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            status: object.get("status").cloned(),
            turns,
        })
    }

    pub fn into_detail(self) -> TaskDetail {
        let turn = latest_turn(&self.turns);
        let turn_status = turn.as_ref().and_then(turn_status);
        let (status, attention) = map_task_status(turn_status, self.status.as_ref());
        let (plan_steps, counts) = turn
            .as_ref()
            .map(extract_plan)
            .unwrap_or_else(|| (Vec::new(), PlanCounts::default()));
        let progress_percent = progress_from_plan(counts, &status);
        let updated_at = self.updated_at.clone().unwrap_or_else(now_rfc3339);
        let completed_at = if matches!(
            status,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Interrupted
        ) {
            turn.as_ref().and_then(|value| {
                timestamp_to_rfc3339(value.get("completedAt"))
                    .or_else(|| timestamp_to_rfc3339(value.get("updatedAt")))
            })
        } else {
            None
        };
        let last_error = turn.as_ref().and_then(extract_error);
        let snapshot = TaskSnapshot {
            provider: "codex".into(),
            task_id: self.id.clone(),
            turn_id: turn
                .as_ref()
                .and_then(|value| value.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            parent_task_id: self.parent_thread_id,
            title: self.title.unwrap_or_else(|| "未命名任务".into()),
            project_path: self.cwd,
            source: self.source,
            status,
            attention,
            progress_percent,
            progress_basis: if counts.total > 0 {
                ProgressBasis::Plan
            } else {
                ProgressBasis::None
            },
            progress_confidence: if counts.total > 0 {
                ProgressConfidence::High
            } else {
                ProgressConfidence::Low
            },
            created_at: self.created_at.unwrap_or_else(|| updated_at.clone()),
            updated_at,
            completed_at,
            archived: self.archived,
            last_error: last_error.clone(),
            plan_total_steps: counts.total,
            plan_completed_steps: counts.completed,
            plan_in_progress_steps: counts.in_progress,
        };
        let recent_turn = turn.map(|value| turn_summary(&value, plan_steps, last_error));
        TaskDetail {
            snapshot,
            recent_turn,
        }
    }
}

fn latest_turn(turns: &[Value]) -> Option<Value> {
    turns.iter().cloned().max_by_key(|turn| {
        timestamp_to_rfc3339(
            turn.get("updatedAt")
                .or_else(|| turn.get("completedAt"))
                .or_else(|| turn.get("startedAt")),
        )
        .unwrap_or_default()
    })
}

fn turn_status(turn: &Value) -> Option<&str> {
    turn.get("status").and_then(|value| {
        value
            .as_str()
            .or_else(|| value.get("type").and_then(Value::as_str))
    })
}

fn extract_error(turn: &Value) -> Option<String> {
    let candidates = [
        turn.get("error"),
        turn.get("lastError"),
        turn.get("failure"),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Some(text) = candidate.as_str() {
            if !text.is_empty() {
                return Some(text.chars().take(280).collect());
            }
        }
        if let Some(text) = candidate.get("message").and_then(Value::as_str) {
            if !text.is_empty() {
                return Some(text.chars().take(280).collect());
            }
        }
    }
    None
}

fn extract_plan(turn: &Value) -> (Vec<PlanStep>, PlanCounts) {
    let mut plan: Option<&Vec<Value>> = None;
    if let Some(items) = turn.get("items").and_then(Value::as_array) {
        for item in items {
            if let Some(steps) = item.get("plan").and_then(Value::as_array) {
                plan = Some(steps);
            }
            if let Some(steps) = item.get("steps").and_then(Value::as_array) {
                plan = Some(steps);
            }
        }
    }
    if plan.is_none() {
        plan = turn.get("plan").and_then(Value::as_array);
    }
    let Some(plan) = plan else {
        return (Vec::new(), PlanCounts::default());
    };
    let steps = parse_plan_values(plan);
    let counts = plan_counts_from_steps(&steps);
    (steps, counts)
}

fn parse_plan_values(plan: &[Value]) -> Vec<PlanStep> {
    let mut steps = Vec::with_capacity(plan.len());
    for (index, value) in plan.iter().enumerate() {
        let title = value
            .get("step")
            .or_else(|| value.get("title"))
            .or_else(|| value.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("计划步骤")
            .to_string();
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending")
            .to_string();
        steps.push(PlanStep {
            id: value
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("step-{}", index + 1)),
            title,
            status,
        });
    }
    steps
}

/// Extract a plan from a `turn/plan/updated` payload without retaining the
/// explanation or any other event fields.
pub fn plan_from_event(value: &Value) -> Option<Vec<PlanStep>> {
    value
        .get("plan")
        .and_then(Value::as_array)
        .map(|plan| parse_plan_values(plan))
}

/// Extract the structured plan arguments from a session `update_plan` call.
/// The session format has shipped with both a JSON string and an object here.
pub fn plan_from_session_update(value: &Value) -> Option<Vec<PlanStep>> {
    let payload = value.get("payload").unwrap_or(value);
    if let Some(plan) = plan_from_event(payload) {
        return Some(plan);
    }
    if payload.get("name").and_then(Value::as_str) != Some("update_plan") {
        return None;
    }
    let arguments = payload.get("arguments")?;
    let arguments = match arguments {
        Value::String(text) => serde_json::from_str::<Value>(text).ok()?,
        Value::Object(_) => arguments.clone(),
        _ => return None,
    };
    plan_from_event(&arguments)
}

fn normalize_source(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return "unknown".into();
    };
    match value {
        Value::String(source) => match source.as_str() {
            "cli"
            | "vscode"
            | "exec"
            | "appServer"
            | "subAgent"
            | "subAgentReview"
            | "subAgentCompact"
            | "subAgentThreadSpawn"
            | "subAgentOther"
            | "unknown" => source.clone(),
            _ => "unknown".into(),
        },
        Value::Object(object) => object
            .get("subAgent")
            .map(normalize_sub_agent_source)
            .unwrap_or_else(|| "unknown".into()),
        _ => "unknown".into(),
    }
}

fn normalize_sub_agent_source(value: &Value) -> String {
    let kind = match value {
        Value::String(value) => Some(value.as_str()),
        Value::Object(object) => object.keys().next().map(|key| key.as_str()),
        _ => None,
    };
    let normalized = kind
        .unwrap_or_default()
        .replace(['_', '-'], "")
        .to_ascii_lowercase();
    match normalized.as_str() {
        "review" => "subAgentReview".into(),
        "compact" => "subAgentCompact".into(),
        "threadspawn" => "subAgentThreadSpawn".into(),
        "other" | "memoryconsolidation" => "subAgentOther".into(),
        _ => "subAgentOther".into(),
    }
}

fn turn_summary(turn: &Value, plan_steps: Vec<PlanStep>, error: Option<String>) -> TurnSummary {
    let started_at = timestamp_to_rfc3339(turn.get("startedAt"));
    let completed_at = timestamp_to_rfc3339(turn.get("completedAt"));
    let duration_seconds = turn
        .get("durationMs")
        .and_then(Value::as_u64)
        .map(|milliseconds| milliseconds / 1000)
        .or_else(|| {
            started_at.as_ref().and_then(|started| {
                let start = DateTime::parse_from_rfc3339(started).ok()?;
                let end = completed_at
                    .as_ref()
                    .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                    .unwrap_or_else(|| Utc::now().into());
                end.signed_duration_since(start)
                    .num_seconds()
                    .try_into()
                    .ok()
            })
        });
    TurnSummary {
        turn_id: turn
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        status: turn_status(turn).unwrap_or("unknown").to_string(),
        started_at,
        completed_at,
        duration_seconds,
        plan_steps,
        error,
    }
}

pub fn resolve_codex_path(configured: Option<&str>) -> Result<PathBuf, CodexError> {
    if let Some(path) = configured.filter(|path| !path.trim().is_empty()) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(CodexError::NotFound(path.display().to_string()));
    }
    if let Ok(path) = std::env::var("AGENT_MONITOR_CODEX_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let native = PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex");
        if native.is_file() {
            return Ok(native);
        }
    }
    if let Ok(output) = Command::new("which").arg("codex").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
    ];
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".npm-global/bin/codex"));
    }
    for path in candidates {
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(CodexError::NotFound("codex".into()))
}

pub fn resolve_codex_home(configured: Option<&str>) -> PathBuf {
    if let Some(path) = configured.filter(|path| !path.trim().is_empty()) {
        return PathBuf::from(path);
    }
    if let Ok(path) = std::env::var("CODEX_HOME") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

pub fn codex_version(binary: &Path) -> Option<String> {
    let home = resolve_codex_home(None);
    let mut command = Command::new(binary);
    clear_codex_context_environment(&mut command);
    command.env("CODEX_HOME", &home);
    configure_runtime_environment(&mut command, &home);
    // An independently spawned app-server can inherit a version probe's pipe.
    // Capturing to a private regular file avoids waiting for unrelated pipe EOF.
    static NEXT_PROBE: AtomicU64 = AtomicU64::new(1);
    let path = std::env::temp_dir().join(format!(
        "agent-monitor-version-{}-{}.txt",
        std::process::id(),
        NEXT_PROBE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(&path).ok()?;
    let result = (|| {
        let mut child = command
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(output.try_clone().ok()?)
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = child.try_wait().ok()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        };
        if !status.success() {
            return None;
        }
        output.seek(SeekFrom::Start(0)).ok()?;
        let mut text = String::new();
        (&mut output).take(4096).read_to_string(&mut text).ok()?;
        text.lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
    })();
    drop(output);
    let _ = std::fs::remove_file(path);
    result
}

#[derive(Debug, Clone, Default)]
pub struct JsonlCursor {
    pub offset: u64,
    pub pending: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEvent {
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub kind: String,
    pub plan: Option<Vec<PlanStep>>,
    pub observed_at: Option<String>,
    pub attention: Option<AttentionKind>,
    pub call_id: Option<String>,
    pub async_question_count: Option<usize>,
    pub rejected_input_call: bool,
    pub accepted_input_call: bool,
    pub answered_questions: Vec<(String, usize)>,
}

pub struct SessionActivity {
    pub session_id: String,
    pub turn_id: String,
    pub attention: Option<AttentionKind>,
}

pub fn rollout_session_ids(path: &Path) -> Option<(String, Option<String>)> {
    fn uuid(value: &str) -> bool {
        value.len() == 36
            && value.chars().enumerate().all(|(index, character)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    character == '-'
                } else {
                    character.is_ascii_hexdigit()
                }
            })
    }
    let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    let mut parts = stem.split('_');
    let base = parts.next()?;
    let conversation = base.get(base.len().checked_sub(36)?..)?;
    if !uuid(conversation) {
        return None;
    }
    let run = parts.next().filter(|value| uuid(value)).map(str::to_owned);
    Some((conversation.to_owned(), run))
}

impl JsonlCursor {
    pub fn consume_bytes(&mut self, bytes: &[u8]) -> (Vec<SessionEvent>, bool) {
        let text = String::from_utf8_lossy(bytes);
        self.pending.push_str(&text);
        let mut events = Vec::new();
        let mut compatibility = false;
        while let Some(index) = self.pending.find('\n') {
            let line = self.pending[..index].trim_end_matches('\r').to_string();
            self.pending.drain(..=index);
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(value) => {
                    if let Some(event) = parse_session_event(&value) {
                        events.push(event);
                    }
                }
                Err(_) => compatibility = true,
            }
        }
        (events, compatibility)
    }

    pub fn reset_if_rotated(&mut self, file_len: u64) {
        if file_len < self.offset {
            self.offset = 0;
            self.pending.clear();
        }
    }
}

pub fn parse_session_event(value: &Value) -> Option<SessionEvent> {
    let event_type = value.get("type").and_then(Value::as_str).or_else(|| {
        value
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str)
    })?;
    let payload = value.get("payload").unwrap_or(value);
    let mut kind = payload
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| payload.get("event").and_then(Value::as_str))
        .or_else(|| payload.get("kind").and_then(Value::as_str))
        .unwrap_or(event_type)
        .to_string();
    let task_id = value
        .get("thread_id")
        .or_else(|| value.get("threadId"))
        .or_else(|| payload.get("thread_id"))
        .or_else(|| payload.get("threadId"))
        .or_else(|| {
            value
                .get("metadata")
                .and_then(|metadata| metadata.get("thread_id"))
        })
        .or_else(|| {
            value
                .get("metadata")
                .and_then(|metadata| metadata.get("threadId"))
        })
        .and_then(Value::as_str)
        .map(str::to_string);
    let turn_id = value
        .get("turn_id")
        .or_else(|| value.get("turnId"))
        .or_else(|| payload.get("turn_id"))
        .or_else(|| payload.get("turnId"))
        .or_else(|| {
            value
                .get("metadata")
                .and_then(|metadata| metadata.get("turn_id"))
        })
        .or_else(|| {
            value
                .get("metadata")
                .and_then(|metadata| metadata.get("turnId"))
        })
        .and_then(Value::as_str)
        .map(str::to_string);
    let observed_at = value
        .get("timestamp")
        .or_else(|| payload.get("timestamp"))
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| crate::model::timestamp_to_rfc3339(Some(value)))
        })
        .or_else(|| {
            payload
                .get("started_at")
                .or_else(|| payload.get("startedAt"))
                .and_then(|value| crate::model::timestamp_to_rfc3339(Some(value)))
        });
    let tool_name = payload.get("name").and_then(Value::as_str);
    let async_question_count =
        if kind == "function_call" && tool_name == Some("request_user_input_async") {
            payload
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
                .and_then(|arguments| {
                    arguments
                        .get("questions")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                })
                .filter(|count| *count > 0)
        } else {
            None
        };
    let answered_questions = answered_async_questions(payload);
    if !answered_questions.is_empty() {
        kind = "question_reply".into();
    }
    let attention = match (kind.as_str(), tool_name) {
        ("function_call", Some("request_user_input")) => Some(AttentionKind::UserInput),
        ("function_call", Some("request_user_input_async")) if async_question_count.is_some() => {
            Some(AttentionKind::UserInput)
        }
        ("exec_approval_request" | "apply_patch_approval_request", _) => {
            Some(AttentionKind::Approval)
        }
        _ => None,
    };
    let call_id = payload
        .get("call_id")
        .or_else(|| payload.get("callId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let rejected_input_call = kind == "function_call_output"
        && payload
            .get("output")
            .and_then(Value::as_str)
            .is_some_and(|output| {
                output.starts_with("failed to parse function arguments:")
                    || serde_json::from_str::<Value>(output)
                        .ok()
                        .and_then(|value| value.get("accepted").and_then(Value::as_bool))
                        == Some(false)
            });
    let accepted_input_call = kind == "function_call_output"
        && payload
            .get("output")
            .and_then(Value::as_str)
            .and_then(|output| serde_json::from_str::<Value>(output).ok())
            .and_then(|value| value.get("accepted").and_then(Value::as_bool))
            == Some(true);
    Some(SessionEvent {
        task_id,
        turn_id,
        kind,
        plan: plan_from_session_update(value),
        observed_at,
        attention,
        call_id,
        async_question_count,
        rejected_input_call,
        accepted_input_call,
        answered_questions,
    })
}

fn answered_async_questions(payload: &Value) -> Vec<(String, usize)> {
    if payload.get("type").and_then(Value::as_str) != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("user")
    {
        return Vec::new();
    }
    let mut answered = Vec::new();
    for block in payload
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if block.get("type").and_then(Value::as_str) != Some("input_text") {
            continue;
        }
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Some(json) = text
            .trim()
            .strip_prefix("<send_user_message_question_reply>")
            .and_then(|text| text.strip_suffix("</send_user_message_question_reply>"))
        else {
            continue;
        };
        let Ok(replies) = serde_json::from_str::<Vec<Value>>(json) else {
            continue;
        };
        for reply in replies {
            if reply.get("answer").and_then(Value::as_str).is_none() {
                continue;
            }
            let Some(identity) = reply.get("questionItemId").and_then(Value::as_str) else {
                continue;
            };
            let Ok(parts) = serde_json::from_str::<Vec<Value>>(identity) else {
                continue;
            };
            if parts.len() != 3 || parts[0].as_str() != Some("request_user_input_async") {
                continue;
            }
            if let (Some(call), Some(index)) = (
                parts[1].as_str(),
                parts[2]
                    .as_u64()
                    .and_then(|index| usize::try_from(index).ok()),
            ) {
                answered.push((call.into(), index));
            }
        }
    }
    answered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonl_incrementally_and_preserves_partial_line() {
        let mut cursor = JsonlCursor::default();
        let (events, compatibility) = cursor.consume_bytes(
            br#"{"type":"event_msg","payload":{"type":"task_started","threadId":"t"}}
{"type":"event_msg","payload":{"type":"task_"#,
        );
        assert_eq!(events.len(), 1);
        assert!(!compatibility);
        assert!(!cursor.pending.is_empty());
        let (events, compatibility) = cursor.consume_bytes(
            br#"complete","turnId":"u","threadId":"t"}}
"#,
        );
        assert_eq!(events.len(), 1);
        assert!(!compatibility);
        assert_eq!(events[0].task_id.as_deref(), Some("t"));
    }

    #[test]
    fn marks_corrupt_complete_lines_as_compatibility_mode() {
        let mut cursor = JsonlCursor::default();
        let (_, compatibility) = cursor.consume_bytes(b"not-json\n");
        assert!(compatibility);
    }

    #[test]
    fn rotation_resets_offset_and_pending() {
        let mut cursor = JsonlCursor {
            offset: 100,
            pending: "partial".into(),
        };
        cursor.reset_if_rotated(4);
        assert_eq!(cursor.offset, 0);
        assert!(cursor.pending.is_empty());
    }

    #[test]
    fn parses_codex_event_msg_payload_fields() {
        let event = parse_session_event(&serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": "turn-1"}
        }))
        .unwrap();
        assert_eq!(event.kind, "task_complete");
        assert_eq!(event.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(event.observed_at, None);
        assert_eq!(event.attention, None);
        assert!(event.plan.is_none());
    }

    #[test]
    fn parses_session_event_timestamp_without_retaining_event_payload() {
        let event = parse_session_event(&serde_json::json!({
            "timestamp": "2026-08-30T08:15:31.067Z",
            "type": "event_msg",
            "payload": {"type": "task_started", "turn_id": "turn-1"}
        }))
        .unwrap();
        assert_eq!(
            event.observed_at.as_deref(),
            Some("2026-08-30T08:15:31.067Z")
        );
    }

    #[test]
    fn parses_user_input_attention_without_trusting_internal_turn_metadata() {
        let event = parse_session_event(&serde_json::json!({
            "timestamp": "2026-08-30T08:15:31.067Z",
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "request_user_input",
                "arguments": "{\"questions\":[{\"question\":\"secret\"}]}",
                "call_id": "call-1",
                "internal_chat_message_metadata_passthrough": {"turn_id": "turn-1"}
            }
        }))
        .unwrap();
        assert_eq!(event.attention, Some(AttentionKind::UserInput));
        assert_eq!(event.turn_id, None);
        assert_eq!(event.call_id.as_deref(), Some("call-1"));
        assert!(event.plan.is_none());
    }

    #[test]
    fn async_input_and_question_replies_retain_only_control_metadata() {
        let event = parse_session_event(&serde_json::json!({
            "type":"response_item","payload":{"type":"function_call",
                "name":"request_user_input_async","call_id":"call-async",
                "arguments":"{\"questions\":[{\"question\":\"PRIVATE QUESTION\"},{\"question\":\"PRIVATE OTHER\"}]}"}
        })).unwrap();
        assert_eq!(event.attention, Some(AttentionKind::UserInput));
        assert_eq!(event.async_question_count, Some(2));
        assert!(!format!("{event:?}").contains("PRIVATE"));
        let response = parse_session_event(&serde_json::json!({
            "type":"response_item","payload":{"type":"message","role":"user","content":[{
                "type":"input_text","text":"<send_user_message_question_reply>[{\"questionItemId\":\"[\\\"request_user_input_async\\\",\\\"call-async\\\",0]\",\"answer\":\"PRIVATE ANSWER\"}]</send_user_message_question_reply>"
            }]}
        })).unwrap();
        assert_eq!(response.kind, "question_reply");
        assert_eq!(response.answered_questions, vec![("call-async".into(), 0)]);
        assert!(!format!("{response:?}").contains("PRIVATE"));
        let ordinary = parse_session_event(&serde_json::json!({
            "type":"response_item","payload":{"type":"message","role":"user","content":[{
                "type":"input_text","text":"ordinary user input"
            }]}
        }))
        .unwrap();
        assert!(ordinary.answered_questions.is_empty());
    }

    #[test]
    fn ignores_internal_tool_call_turn_ids_that_are_not_codex_turn_ids() {
        let event = parse_session_event(&serde_json::json!({
            "timestamp": "2026-08-30T11:02:21.118Z",
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "call_id": "call-1",
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": "b3c02ee3-9146-4617-be72-aba41d69d4b4"
                }
            }
        }))
        .unwrap();
        assert_eq!(event.kind, "custom_tool_call");
        assert_eq!(event.turn_id, None);
    }

    #[test]
    fn parses_update_plan_session_calls_without_retaining_the_raw_arguments() {
        let event = parse_session_event(&serde_json::json!({
            "type": "response_item",
            "metadata": {"turn_id": "turn-1"},
            "payload": {
                "type": "function_call",
                "name": "update_plan",
                "arguments": "{\"plan\":[{\"step\":\"Inspect\",\"status\":\"completed\"},{\"step\":\"Verify\",\"status\":\"inProgress\"}]}"
            }
        }))
        .unwrap();
        assert_eq!(event.turn_id.as_deref(), Some("turn-1"));
        let plan = event.plan.expect("plan should be parsed");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].title, "Inspect");
        assert_eq!(plan[1].status, "inProgress");
    }

    #[test]
    fn parses_app_server_plan_update_payloads() {
        let plan = plan_from_event(&serde_json::json!({
            "turnId": "turn-1",
            "explanation": "progress",
            "plan": [{"step": "Ship", "status": "pending"}]
        }))
        .expect("app-server plan should parse");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].title, "Ship");
    }

    #[test]
    fn normalizes_string_and_subagent_source_variants() {
        let cases = [
            (serde_json::json!("vscode"), "vscode"),
            (serde_json::json!({"subAgent":"review"}), "subAgentReview"),
            (serde_json::json!({"subAgent":"compact"}), "subAgentCompact"),
            (
                serde_json::json!({"subAgent":{"thread_spawn":{}}}),
                "subAgentThreadSpawn",
            ),
            (
                serde_json::json!({"subAgent":"memory_consolidation"}),
                "subAgentOther",
            ),
            (serde_json::json!({"custom":"vendor"}), "unknown"),
        ];
        for (source, expected) in cases {
            let record = ThreadRecord::from_value(serde_json::json!({
                "id": "thread-source",
                "cwd": "/tmp/project",
                "source": source,
            }))
            .expect("thread metadata should parse");
            assert_eq!(record.source, expected);
        }
    }

    #[test]
    fn keeps_the_explicit_source_kind_allowlist_stable() {
        assert_eq!(SOURCE_KINDS.len(), 10);
        assert!(SOURCE_KINDS.contains(&"subAgentThreadSpawn"));
        assert!(SOURCE_KINDS.contains(&"unknown"));
    }

    #[test]
    fn fake_app_server_supports_initialize_list_and_read() {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-codex.sh");
        if !fixture.is_file() {
            panic!("fake codex fixture is missing");
        }
        let client = CodexClient::connect(&fixture, std::path::Path::new("/tmp/fake-codex"))
            .expect("fake app-server should initialize");
        let threads = client.list_threads(false).expect("thread/list should work");
        assert_eq!(threads.len(), 1);
        assert!(!threads[0].archived);
        let archived = client
            .list_threads(true)
            .expect("archived thread/list should work");
        assert!(archived[0].archived);
        let detail = client
            .read_thread("fake-thread")
            .expect("thread/read should work")
            .into_detail();
        assert_eq!(detail.snapshot.status, TaskStatus::Succeeded);
        assert_eq!(detail.snapshot.progress_percent, None);
        assert_eq!(detail.snapshot.plan_total_steps, 0);
        assert_eq!(detail.snapshot.turn_id.as_deref(), Some("fake-turn"));
        assert!(detail
            .recent_turn
            .as_ref()
            .is_some_and(|turn| turn.plan_steps.is_empty()));
    }

    #[test]
    fn active_discovery_rehydrates_a_restored_idle_snapshot() {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-codex.sh");
        let client = CodexClient::connect(&fixture, std::path::Path::new("/tmp/fake-codex"))
            .expect("fake app-server should initialize");
        let restored = ThreadRecord::from_value(serde_json::json!({
            "id": "fake-thread",
            "cwd": "/tmp/fake-project",
            "source": "cli",
            "name": "Fake lifecycle",
            "createdAt": 1780000000,
            "updatedAt": 1780000100,
            "status": {"type": "idle"},
            "turns": []
        }))
        .unwrap()
        .into_detail();
        assert_eq!(restored.snapshot.status, TaskStatus::Idle);
        assert!(restored.recent_turn.is_none());

        let cached = HashMap::from([("fake-thread".to_string(), restored)]);
        let details = client
            .discover_cached(false, &cached)
            .expect("active discovery should retry latest-turn metadata");
        assert_eq!(details[0].snapshot.status, TaskStatus::Succeeded);
        assert!(details[0].recent_turn.is_some());
    }

    #[test]
    fn archived_discovery_uses_list_metadata_without_turn_reads() {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-codex.sh");
        let client = CodexClient::connect(&fixture, std::path::Path::new("/tmp/fake-codex"))
            .expect("fake app-server should initialize");
        let details = client
            .discover_archived_cached(&HashMap::new())
            .expect("archived discovery should work");
        assert_eq!(details.len(), 1);
        assert!(details[0].snapshot.archived);
        assert_eq!(details[0].snapshot.status, TaskStatus::Idle);
    }

    #[test]
    fn metadata_parser_drops_embedded_turn_history() {
        let record = ThreadRecord::from_metadata_value(serde_json::json!({
            "id": "large-thread",
            "cwd": "/tmp/project",
            "name": "Large history",
            "turns": [{
                "id": "old-turn",
                "items": [{"type": "commandExecution", "aggregatedOutput": "secret output"}]
            }]
        }))
        .expect("metadata should still parse");
        assert!(record.turns.is_empty());
        assert_eq!(record.title.as_deref(), Some("Large history"));
    }

    #[test]
    fn normalizes_waiting_and_failed_thread_records() {
        let waiting = ThreadRecord::from_value(serde_json::json!({
            "id": "waiting-thread",
            "cwd": "/tmp/project",
            "source": "vscode",
            "name": "Needs approval",
            "createdAt": 1780000000,
            "updatedAt": 1780000010,
            "status": {"type":"active", "active":{"type":"waitingOnApproval"}},
            "turns": [{"id":"turn-1","status":"inProgress","startedAt":1780000000}]
        }))
        .unwrap()
        .into_detail();
        assert_eq!(waiting.snapshot.status, TaskStatus::NeedsAction);
        assert_eq!(
            waiting.snapshot.attention,
            Some(crate::model::AttentionKind::Approval)
        );

        let failed = ThreadRecord::from_value(serde_json::json!({
            "id": "failed-thread",
            "cwd": "/tmp/project",
            "source": "cli",
            "status": {"type":"idle"},
            "turns": [{"id":"turn-2","status":"failed","error":{"message":"boom"}}]
        }))
        .unwrap()
        .into_detail();
        assert_eq!(failed.snapshot.status, TaskStatus::Failed);
        assert_eq!(failed.snapshot.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn app_server_path_prioritizes_user_managed_node_locations() {
        let path = augmented_path(
            Path::new("/Users/tester"),
            Some(OsString::from("/system/bin:/usr/bin")),
        )
        .expect("path should be assembled");
        let paths: Vec<_> = std::env::split_paths(&path).collect();
        assert_eq!(paths.first(), Some(&PathBuf::from("/Users/tester/.n/bin")));
        assert!(paths.contains(&PathBuf::from("/Users/tester/.npm-global/bin")));
        assert!(paths.contains(&PathBuf::from("/usr/bin")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn automatic_path_prefers_the_codex_app_binary_when_installed() {
        let native = Path::new("/Applications/ChatGPT.app/Contents/Resources/codex");
        if native.is_file() {
            assert_eq!(
                resolve_codex_path(None).expect("Codex should resolve"),
                native
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_does_not_wait_for_descendants_to_close_stdout() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "monitor-version-descendant-{}-{}.sh",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::write(
            &path,
            "#!/bin/sh\nsleep 6 &\nprintf 'codex-cli fixture\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let began = Instant::now();
        let version = codex_version(&path);
        let elapsed = began.elapsed();
        std::fs::remove_file(path).unwrap();
        assert_eq!(version.as_deref(), Some("codex-cli fixture"));
        assert!(
            elapsed < Duration::from_secs(3),
            "version query waited {elapsed:?} for an unrelated inherited pipe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_stops_a_hung_command() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "monitor-version-timeout-{}-{}.sh",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::write(&path, "#!/bin/sh\nexec sleep 5\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let began = Instant::now();
        assert_eq!(codex_version(&path), None);
        let elapsed = began.elapsed();
        std::fs::remove_file(path).unwrap();
        assert!(
            elapsed < Duration::from_secs(3),
            "version query was not bounded: {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn app_server_does_not_inherit_codex_task_context() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        const CHILD_MARKER: &str = "AGENT_MONITOR_CONTEXT_TEST_CHILD";
        if std::env::var_os(CHILD_MARKER).is_none() {
            let mut command =
                Command::new(std::env::current_exe().expect("test binary should resolve"));
            command
                .args([
                    "--exact",
                    "codex::tests::app_server_does_not_inherit_codex_task_context",
                    "--nocapture",
                ])
                .env(CHILD_MARKER, "1");
            for key in CODEX_CONTEXT_ENV_VARS {
                command.env(key, "inherited-context");
            }
            let output = command
                .output()
                .expect("isolated regression test should run");
            assert!(
                output.status.success(),
                "isolated regression test failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let suffix = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be valid")
                .as_nanos()
        );
        let capture = std::env::temp_dir().join(format!("agent-monitor-codex-env-{suffix}"));
        let fixture = std::env::temp_dir().join(format!("agent-monitor-codex-env-{suffix}.sh"));
        let script = r#"#!/bin/sh
env | sed -n '/^CODEX_/p' | sort > '__CAPTURE__'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"id":%s,"result":{"userAgent":"fixture"}}\n' "$id"
      ;;
    *)
      if [ -n "$id" ]; then
        printf '{"id":%s,"result":{}}\n' "$id"
      fi
      ;;
  esac
done
"#
        .replace("__CAPTURE__", &capture.display().to_string());
        fs::write(&fixture, script).expect("fixture should be written");
        let mut permissions = fs::metadata(&fixture)
            .expect("fixture metadata should be readable")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fixture, permissions).expect("fixture should be executable");

        let client = CodexClient::connect(&fixture, Path::new("/tmp/fake-codex"))
            .expect("fixture app-server should initialize");
        let captured = fs::read_to_string(&capture).expect("child environment should be captured");
        assert_eq!(captured, "CODEX_HOME=/tmp/fake-codex\n");
        drop(client);
        let _ = fs::remove_file(fixture);
        let _ = fs::remove_file(capture);
    }

    #[test]
    fn redacts_sensitive_codex_diagnostics() {
        assert_eq!(
            sanitize_diagnostic("authorization=Bearer secret-value"),
            "Codex 子进程返回了包含受保护信息的启动错误"
        );
        assert_eq!(
            sanitize_diagnostic("env: node: No such file or directory"),
            "env: node: No such file or directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reports_child_stderr_when_app_server_exits_before_initialize() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "agent-monitor-codex-exit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be valid")
                .as_nanos()
        ));
        fs::write(
            &path,
            "#!/bin/sh\necho 'env: node: No such file or directory' >&2\nexit 127\n",
        )
        .expect("fixture should be written");
        let mut permissions = fs::metadata(&path)
            .expect("fixture metadata should be readable")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("fixture should be executable");

        let result = CodexClient::connect(&path, Path::new("/tmp/fake-codex"));
        let error = match result {
            Ok(_) => panic!("the fixture should exit before initialize"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("node: No such file or directory"));
        let _ = fs::remove_file(path);
    }

    #[test]
    #[ignore]
    fn real_codex_app_server_smoke_reads_one_thread() {
        let binary =
            resolve_codex_path(None).expect("codex should be installed for the smoke test");
        let home = resolve_codex_home(None);
        let client = CodexClient::connect(&binary, &home).expect("app-server should initialize");
        let threads = client
            .list_threads(false)
            .expect("thread/list should respond");
        if let Some(thread) = threads.first() {
            let detail = client
                .read_thread(&thread.id)
                .expect("thread/read should respond")
                .into_detail();
            assert_eq!(detail.snapshot.provider, "codex");
            assert_eq!(detail.snapshot.task_id, thread.id);
        }
    }
}
