use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    NeedsAction,
    Idle,
    Succeeded,
    Failed,
    Interrupted,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    Approval,
    UserInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProgressBasis {
    Plan,
    Stage,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProgressConfidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskSnapshot {
    pub provider: String,
    pub task_id: String,
    pub turn_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub title: String,
    pub project_path: String,
    pub source: String,
    pub status: TaskStatus,
    pub attention: Option<AttentionKind>,
    pub progress_percent: Option<u8>,
    pub progress_basis: ProgressBasis,
    pub progress_confidence: ProgressConfidence,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub archived: bool,
    pub last_error: Option<String>,
    #[serde(default)]
    pub plan_total_steps: u32,
    #[serde(default)]
    pub plan_completed_steps: u32,
    #[serde(default)]
    pub plan_in_progress_steps: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub id: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnSummary {
    pub turn_id: String,
    pub status: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub duration_seconds: Option<u64>,
    pub plan_steps: Vec<PlanStep>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskDetail {
    pub snapshot: TaskSnapshot,
    pub recent_turn: Option<TurnSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSummary {
    pub project_path: String,
    pub task_count: usize,
    pub running_count: usize,
    pub needs_action_count: usize,
    pub idle_count: usize,
    pub succeeded_count: usize,
    pub failed_count: usize,
    pub interrupted_count: usize,
    pub estimated_percent: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SyncState {
    pub status: String,
    pub last_sync_at: Option<String>,
    pub last_error: Option<String>,
    pub compatibility_mode: bool,
    pub codex_path: String,
    pub codex_home: String,
}

impl Default for SyncState {
    fn default() -> Self {
        Self {
            status: "connecting".to_string(),
            last_sync_at: None,
            last_error: None,
            compatibility_mode: false,
            codex_path: String::new(),
            codex_home: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DashboardSnapshot {
    pub protocol_version: String,
    pub tasks: Vec<TaskSnapshot>,
    pub projects: Vec<ProjectSummary>,
    pub sync: SyncState,
    pub generated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ThemePreference {
    System,
    Light,
    Dark,
}

fn default_theme() -> ThemePreference {
    ThemePreference::System
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub codex_path: Option<String>,
    pub codex_home: Option<String>,
    pub refresh_interval_seconds: u64,
    #[serde(default = "default_history_retention_days")]
    pub history_retention_days: u16,
    pub launch_at_login: bool,
    pub notifications_enabled: bool,
    #[serde(default = "default_notification_duration_seconds")]
    pub notification_duration_seconds: u64,
    pub show_prompt_preview: bool,
    #[serde(default = "default_theme")]
    pub theme: ThemePreference,
}

fn default_notification_duration_seconds() -> u64 {
    300
}

fn default_history_retention_days() -> u16 {
    crate::activity::DEFAULT_HISTORY_RETENTION_DAYS
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            codex_path: None,
            codex_home: None,
            refresh_interval_seconds: 15,
            history_retention_days: default_history_retention_days(),
            launch_at_login: true,
            notifications_enabled: true,
            notification_duration_seconds: default_notification_duration_seconds(),
            show_prompt_preview: false,
            theme: default_theme(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostics {
    pub codex_path: String,
    pub codex_home: String,
    pub codex_version: Option<String>,
    pub connection_status: String,
    pub transport: String,
    pub compatibility_mode: bool,
    pub degradation_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NotificationEvent {
    pub event_type: String,
    pub task_id: String,
    pub turn_id: Option<String>,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanCounts {
    pub total: u32,
    pub completed: u32,
    pub in_progress: u32,
}

pub fn plan_counts_from_steps(steps: &[PlanStep]) -> PlanCounts {
    let mut completed = 0;
    let mut in_progress = 0;
    for step in steps {
        let normalized = step.status.replace(['_', '-'], "").to_ascii_lowercase();
        if ["completed", "complete", "done", "finished", "success"].contains(&normalized.as_str()) {
            completed += 1;
        } else if ["inprogress", "running", "active", "doing"].contains(&normalized.as_str()) {
            in_progress += 1;
        }
    }
    PlanCounts {
        total: steps.len() as u32,
        completed,
        in_progress,
    }
}

pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

pub fn timestamp_to_rfc3339(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    let number = value.as_i64()?;
    let seconds = if number > 10_000_000_000 {
        number / 1000
    } else {
        number
    };
    DateTime::<Utc>::from_timestamp(seconds, 0).map(|date| date.to_rfc3339())
}

fn value_contains_token(value: &Value, token: &str) -> bool {
    let normalize = |text: &str| text.replace(['_', '-'], "").to_ascii_lowercase();
    match value {
        Value::String(text) => normalize(text) == normalize(token),
        Value::Bool(_) => false,
        Value::Array(items) => items.iter().any(|item| value_contains_token(item, token)),
        Value::Object(map) => map.iter().any(|(key, item)| {
            if normalize(key) == normalize(token) {
                match item {
                    Value::Bool(value) => *value,
                    Value::String(value) => {
                        normalize(value) == "true" || normalize(value) == normalize(token)
                    }
                    Value::Null => false,
                    _ => true,
                }
            } else {
                value_contains_token(item, token)
            }
        }),
        _ => false,
    }
}

pub fn map_task_status(
    turn_status: Option<&str>,
    thread_status: Option<&Value>,
) -> (TaskStatus, Option<AttentionKind>) {
    let normalized_turn = turn_status
        .unwrap_or_default()
        .replace(['_', '-'], "")
        .to_ascii_lowercase();
    match normalized_turn.as_str() {
        "completed" | "succeeded" | "success" => return (TaskStatus::Succeeded, None),
        "failed" | "error" => return (TaskStatus::Failed, None),
        "interrupted" | "aborted" | "cancelled" | "canceled" => {
            return (TaskStatus::Interrupted, None)
        }
        _ => {}
    }
    let approval = thread_status
        .map(|status| value_contains_token(status, "waitingOnApproval"))
        .unwrap_or(false);
    let user_input = thread_status
        .map(|status| value_contains_token(status, "waitingOnUserInput"))
        .unwrap_or(false);

    if approval {
        return (TaskStatus::NeedsAction, Some(AttentionKind::Approval));
    }
    if user_input {
        return (TaskStatus::NeedsAction, Some(AttentionKind::UserInput));
    }

    match normalized_turn.as_str() {
        "inprogress" | "running" => (TaskStatus::Running, None),
        _ => {
            let status_type = thread_status
                .and_then(|status| status.get("type"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            match status_type {
                "active" | "running" => (TaskStatus::Running, None),
                "idle" | "notLoaded" | "not_loaded" => (TaskStatus::Idle, None),
                "systemError" | "system_error" => (TaskStatus::Unknown, None),
                _ => (TaskStatus::Unknown, None),
            }
        }
    }
}

pub fn progress_from_plan(counts: PlanCounts, status: &TaskStatus) -> Option<u8> {
    if counts.total == 0 {
        return None;
    }
    if matches!(status, TaskStatus::Succeeded) {
        return Some(100);
    }
    let completed = counts.completed.min(counts.total);
    let in_progress = counts
        .in_progress
        .min(counts.total.saturating_sub(completed));
    let raw = ((completed as f64 + (in_progress as f64 * 0.5)) / counts.total as f64) * 100.0;
    let rounded = raw.round().clamp(0.0, 100.0) as u8;
    Some(rounded.min(99))
}

pub fn aggregate_projects(tasks: &[TaskSnapshot]) -> Vec<ProjectSummary> {
    #[derive(Default)]
    struct Accumulator {
        task_count: usize,
        running: usize,
        needs_action: usize,
        idle: usize,
        succeeded: usize,
        failed: usize,
        interrupted: usize,
        all_active_have_plan: bool,
        has_active: bool,
        weighted_completed: f64,
        weighted_total: u32,
    }

    let mut grouped: BTreeMap<String, Accumulator> = BTreeMap::new();
    for task in tasks {
        let entry = grouped
            .entry(task.project_path.clone())
            .or_insert_with(|| Accumulator {
                all_active_have_plan: true,
                ..Accumulator::default()
            });
        entry.task_count += 1;
        match task.status {
            TaskStatus::Running => {
                entry.running += 1;
                entry.has_active = true;
            }
            TaskStatus::NeedsAction => {
                entry.needs_action += 1;
                entry.has_active = true;
            }
            TaskStatus::Idle => entry.idle += 1,
            TaskStatus::Succeeded => entry.succeeded += 1,
            TaskStatus::Failed => entry.failed += 1,
            TaskStatus::Interrupted => entry.interrupted += 1,
            TaskStatus::Unknown => {}
        }
        if task.plan_total_steps == 0
            && matches!(task.status, TaskStatus::Running | TaskStatus::NeedsAction)
        {
            entry.all_active_have_plan = false;
        }
        if task.plan_total_steps > 0 {
            entry.weighted_total += task.plan_total_steps;
            entry.weighted_completed +=
                task.plan_completed_steps as f64 + task.plan_in_progress_steps as f64 * 0.5;
        }
    }

    grouped
        .into_iter()
        .map(|(project_path, entry)| {
            let estimated_percent = if entry.all_active_have_plan && entry.weighted_total > 0 {
                Some(
                    ((entry.weighted_completed / entry.weighted_total as f64 * 100.0)
                        .round()
                        .clamp(0.0, 100.0) as u8)
                        .min(if entry.has_active { 99 } else { 100 }),
                )
            } else {
                None
            };
            ProjectSummary {
                project_path,
                task_count: entry.task_count,
                running_count: entry.running,
                needs_action_count: entry.needs_action,
                idle_count: entry.idle,
                succeeded_count: entry.succeeded,
                failed_count: entry.failed,
                interrupted_count: entry.interrupted,
                estimated_percent,
            }
        })
        .collect()
}

pub fn notification_key(
    provider: &str,
    task_id: &str,
    turn_id: Option<&str>,
    event_type: &str,
) -> String {
    format!(
        "{}:{}:{}:{}",
        provider,
        task_id,
        turn_id.unwrap_or("none"),
        event_type
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_turn_lifecycle_without_confusing_idle_with_success() {
        assert_eq!(
            map_task_status(Some("inProgress"), None),
            (TaskStatus::Running, None)
        );
        assert_eq!(
            map_task_status(Some("completed"), Some(&json!({"type":"idle"}))),
            (TaskStatus::Succeeded, None)
        );
        assert_eq!(
            map_task_status(None, Some(&json!({"type":"idle"}))),
            (TaskStatus::Idle, None)
        );
    }

    #[test]
    fn maps_attention_states() {
        assert_eq!(
            map_task_status(
                Some("inProgress"),
                Some(&json!({"type":"active", "active":{"type":"waitingOnApproval"}})),
            ),
            (TaskStatus::NeedsAction, Some(AttentionKind::Approval))
        );
        assert_eq!(
            map_task_status(
                Some("inProgress"),
                Some(&json!({"waitingOnUserInput":true}))
            ),
            (TaskStatus::NeedsAction, Some(AttentionKind::UserInput))
        );
        assert_eq!(
            map_task_status(
                Some("inProgress"),
                Some(&json!({"waitingOnApproval":false, "waitingOnUserInput":false}))
            ),
            (TaskStatus::Running, None)
        );
    }

    #[test]
    fn calculates_plan_progress_and_reserves_one_hundred_for_success() {
        assert_eq!(
            progress_from_plan(
                PlanCounts {
                    total: 4,
                    completed: 3,
                    in_progress: 1
                },
                &TaskStatus::Running,
            ),
            Some(88)
        );
        assert_eq!(
            progress_from_plan(
                PlanCounts {
                    total: 1,
                    completed: 1,
                    in_progress: 0
                },
                &TaskStatus::Running,
            ),
            Some(99)
        );
        assert_eq!(
            progress_from_plan(
                PlanCounts {
                    total: 1,
                    completed: 0,
                    in_progress: 0
                },
                &TaskStatus::Succeeded,
            ),
            Some(100)
        );
        assert_eq!(
            progress_from_plan(
                PlanCounts {
                    total: 1,
                    completed: 1,
                    in_progress: 0
                },
                &TaskStatus::Failed,
            ),
            Some(99)
        );
        assert_eq!(
            progress_from_plan(
                PlanCounts {
                    total: 0,
                    completed: 0,
                    in_progress: 0
                },
                &TaskStatus::Idle
            ),
            None
        );
    }

    #[test]
    fn counts_plan_steps_with_wire_status_variants() {
        let counts = plan_counts_from_steps(&[
            PlanStep {
                id: "1".into(),
                title: "done".into(),
                status: "completed".into(),
            },
            PlanStep {
                id: "2".into(),
                title: "active".into(),
                status: "in-progress".into(),
            },
            PlanStep {
                id: "3".into(),
                title: "queued".into(),
                status: "pending".into(),
            },
        ]);
        assert_eq!(
            counts,
            PlanCounts {
                total: 3,
                completed: 1,
                in_progress: 1,
            }
        );
    }

    fn task(path: &str, status: TaskStatus, total: u32, completed: u32) -> TaskSnapshot {
        TaskSnapshot {
            provider: "codex".into(),
            task_id: path.into(),
            turn_id: None,
            parent_task_id: None,
            title: "task".into(),
            project_path: path.into(),
            source: "cli".into(),
            status,
            attention: None,
            progress_percent: None,
            progress_basis: ProgressBasis::Plan,
            progress_confidence: ProgressConfidence::High,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            completed_at: None,
            archived: false,
            last_error: None,
            plan_total_steps: total,
            plan_completed_steps: completed,
            plan_in_progress_steps: 0,
        }
    }

    #[test]
    fn aggregates_projects_only_when_every_active_task_has_plan() {
        let summaries = aggregate_projects(&[
            task("/a", TaskStatus::Succeeded, 2, 2),
            task("/a", TaskStatus::Running, 2, 1),
        ]);
        assert_eq!(summaries[0].estimated_percent, Some(75));

        let summaries = aggregate_projects(&[task("/a", TaskStatus::Running, 1, 1)]);
        assert_eq!(summaries[0].estimated_percent, Some(99));

        let mut no_plan = task("/b", TaskStatus::Idle, 0, 0);
        no_plan.progress_basis = ProgressBasis::None;
        let summaries = aggregate_projects(&[no_plan]);
        assert_eq!(summaries[0].estimated_percent, None);

        let idle_without_plan = task("/c", TaskStatus::Idle, 0, 0);
        let running_with_plan = task("/c", TaskStatus::Running, 2, 1);
        assert_eq!(
            aggregate_projects(&[idle_without_plan, running_with_plan])[0].estimated_percent,
            Some(50)
        );

        let running_without_plan = task("/d", TaskStatus::Running, 0, 0);
        let idle_with_plan = task("/d", TaskStatus::Idle, 2, 2);
        assert_eq!(
            aggregate_projects(&[running_without_plan, idle_with_plan])[0].estimated_percent,
            None
        );
    }

    #[test]
    fn failed_count_does_not_include_interrupted_tasks() {
        let summaries = aggregate_projects(&[
            task("/a", TaskStatus::Failed, 0, 0),
            task("/a", TaskStatus::Interrupted, 0, 0),
        ]);

        assert_eq!(summaries[0].failed_count, 1);
        assert_eq!(summaries[0].interrupted_count, 1);
    }

    #[test]
    fn new_notice_preference_defaults_to_five_minutes_without_resetting_saved_settings() {
        let mut saved = serde_json::to_value(Settings::default()).unwrap();
        saved["notificationsEnabled"] = false.into();
        saved["refreshIntervalSeconds"] = 37.into();
        saved
            .as_object_mut()
            .unwrap()
            .remove("notificationDurationSeconds");
        let loaded: Settings = serde_json::from_value(saved.clone()).unwrap();
        assert_eq!(loaded.notification_duration_seconds, 300);
        assert_eq!(loaded.history_retention_days, 30);
        assert_eq!(loaded.theme, ThemePreference::System);
        assert_eq!(serde_json::to_value(&loaded).unwrap()["theme"], "system");
        assert!(!loaded.notifications_enabled);
        assert_eq!(loaded.refresh_interval_seconds, 37);
        saved["notificationDurationSeconds"] = 0.into();
        assert_eq!(
            serde_json::from_value::<Settings>(saved)
                .unwrap()
                .notification_duration_seconds,
            0
        );
    }

    #[test]
    fn notification_key_is_stable_and_turn_scoped() {
        assert_eq!(
            notification_key("codex", "task", Some("turn"), "completed"),
            "codex:task:turn:completed"
        );
        assert_ne!(
            notification_key("codex", "task", Some("one"), "completed"),
            notification_key("codex", "task", Some("two"), "completed")
        );
    }
}
