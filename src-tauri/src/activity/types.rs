use crate::model::{AttentionKind, TaskStatus};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
    pub total: Option<u64>,
    pub input_includes_cache: bool,
    pub output_includes_reasoning: bool,
}

impl Default for TokenUsage {
    fn default() -> Self {
        Self {
            input: None,
            output: None,
            cache_read: None,
            cache_write: None,
            reasoning: None,
            total: None,
            input_includes_cache: false,
            output_includes_reasoning: true,
        }
    }
}

impl TokenUsage {
    pub fn sum<'a>(values: impl IntoIterator<Item = &'a Self>, normalize: bool) -> Self {
        let values: Vec<Self> = values
            .into_iter()
            .map(|value| {
                let mut value = value.clone();
                if normalize && value.input_includes_cache {
                    value.input = value
                        .input
                        .zip(value.cache_read)
                        .zip(value.cache_write)
                        .and_then(|((input, read), write)| input.checked_sub(read + write));
                    value.input_includes_cache = false;
                }
                if normalize && !value.output_includes_reasoning {
                    value.output = value.output.zip(value.reasoning).map(|(a, b)| a + b);
                    value.output_includes_reasoning = true;
                }
                value
            })
            .collect();
        if values.is_empty() {
            return Self::default();
        }
        fn field(values: &[TokenUsage], get: impl Fn(&TokenUsage) -> Option<u64>) -> Option<u64> {
            values
                .iter()
                .try_fold(0_u64, |sum, item| sum.checked_add(get(item)?))
        }
        Self {
            input: field(&values, |v| v.input),
            output: field(&values, |v| v.output),
            cache_read: field(&values, |v| v.cache_read),
            cache_write: field(&values, |v| v.cache_write),
            reasoning: field(&values, |v| v.reasoning),
            total: field(&values, |v| v.total),
            input_includes_cache: values[0].input_includes_cache,
            output_includes_reasoning: values[0].output_includes_reasoning,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionRecord {
    pub id: String,
    pub client: String,
    pub session_id: String,
    pub turn_id: String,
    pub root_id: Option<String>,
    pub parent_id: Option<String>,
    pub is_child: bool,
    pub title: String,
    pub project_path: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub turn_index: u64,
    pub status: TaskStatus,
    pub attention: Option<AttentionKind>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub observed_at: String,
    pub duration_ms: Option<u64>,
    pub active_ms: Option<u64>,
    pub waiting_ms: Option<u64>,
    pub tokens: TokenUsage,
    pub live: bool,
    pub child_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SourceHealth {
    pub client: String,
    pub state: String,
    pub observed_at: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActivitySummary {
    pub running: u64,
    pub needs_action: u64,
    pub completed_today: u64,
    pub failed_today: u64,
    pub total_turns: u64,
    pub tokens: TokenUsage,
    pub token_coverage: TokenCoverage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientSummaries {
    pub codex: ActivitySummary,
    pub claude: ActivitySummary,
    pub zcode: ActivitySummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenCoverage {
    pub recorded_executions: u64,
    pub total_executions: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DailyUsage {
    pub date: String,
    pub client: String,
    pub turns: u64,
    pub completed: u64,
    pub tokens: TokenUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActivitySnapshot {
    pub protocol_version: String,
    pub generated_at: String,
    pub executions: Vec<ExecutionRecord>,
    pub summary: ActivitySummary,
    pub client_summaries: ClientSummaries,
    pub daily: Vec<DailyUsage>,
    pub sources: Vec<SourceHealth>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActivityQuery {
    pub client: Option<String>,
    pub status: Option<String>,
    pub session_id: Option<String>,
    pub since: Option<String>,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
    pub roots_only: Option<bool>,
    pub children_only: Option<bool>,
    pub root_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActivityPage {
    pub executions: Vec<ExecutionRecord>,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub latest: ExecutionRecord,
    pub turn_count: u64,
    pub child_count: u64,
    pub tokens: TokenUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionPage {
    pub sessions: Vec<SessionSummary>,
    pub total: u64,
    pub total_turns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEvent {
    pub client: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub event: String,
    pub observed_at: String,
    pub native_timestamp: Option<String>,
    pub tool_call_id: Option<String>,
    pub is_child: Option<bool>,
    pub agent_id: Option<String>,
    pub process: Option<super::ProcessAnchor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct Relation {
    pub parent_session: Option<String>,
    pub root_native_turn: Option<String>,
    pub parent_native_turn: Option<String>,
    pub root_prompt_id: Option<String>,
}

pub(super) struct CollectedExecution {
    pub record: ExecutionRecord,
    pub relation: Relation,
}

pub(super) struct UsageRecord {
    pub id: String,
    pub execution_id: String,
    pub client: String,
    pub recorded_at: String,
    pub tokens: TokenUsage,
}

pub(super) fn execution_id(client: &str, session: &str, turn: &str) -> String {
    format!("{client}:{session}:{turn}")
}
