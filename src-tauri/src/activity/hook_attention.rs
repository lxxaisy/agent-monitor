use crate::model::AttentionKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(super) fn event_name<'a>(client: &str, event: &'a str, input: &Value) -> &'a str {
    let name = match client {
        "claude" => input.get("tool_name"),
        "zcode" => input.get("toolName"),
        _ => None,
    }
    .and_then(Value::as_str);
    if event == "PreToolUse" && name == Some("AskUserQuestion") {
        "UserInput"
    } else {
        event
    }
}

pub(super) fn tool_call_id(client: &str, input: &Value) -> Option<String> {
    match client {
        "claude" => input.get("tool_use_id"),
        "zcode" => input.get("toolCallId"),
        _ => None,
    }
    .and_then(Value::as_str)
    .map(str::to_owned)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Request {
    call_id: Option<String>,
    kind: AttentionKind,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(super) struct Pending {
    requests: Vec<Request>,
}

impl Pending {
    pub(super) fn apply(&mut self, event: &str, call_id: Option<&str>) {
        match event {
            "UserInput" | "PermissionRequest" => {
                let kind = if event == "UserInput" {
                    AttentionKind::UserInput
                } else {
                    AttentionKind::Approval
                };
                if !self
                    .requests
                    .iter()
                    .any(|request| request.call_id.as_deref() == call_id && request.kind == kind)
                {
                    self.requests.push(Request {
                        call_id: call_id.map(str::to_owned),
                        kind,
                    });
                }
            }
            "PostToolUse" | "PostToolUseFailure" => {
                if let Some(call_id) = call_id {
                    self.requests
                        .retain(|request| request.call_id.as_deref() != Some(call_id));
                }
            }
            "UserPromptSubmit" | "Stop" | "SessionEnd" => self.requests.clear(),
            _ => {}
        }
    }

    pub(super) fn current(&self) -> Option<AttentionKind> {
        self.requests.last().map(|request| request.kind.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_ask_hooks_use_exact_client_fields_and_no_bodies() {
        for (client, input) in [
            (
                "claude",
                json!({"tool_name":"AskUserQuestion","tool_use_id":"call","tool_input":{"questions":["PRIVATE"]}}),
            ),
            (
                "zcode",
                json!({"toolName":"AskUserQuestion","toolCallId":"call","toolInput":{"questions":["PRIVATE"]}}),
            ),
        ] {
            assert_eq!(event_name(client, "PreToolUse", &input), "UserInput");
            assert_eq!(tool_call_id(client, &input).as_deref(), Some("call"));
            assert_eq!(event_name(client, "PostToolUse", &input), "PostToolUse");
        }
        assert_eq!(
            event_name("claude", "PreToolUse", &json!({"tool_name":"Read"})),
            "PreToolUse"
        );
        assert_eq!(
            tool_call_id("claude", &json!({"toolCallId":"wrong-field"})),
            None
        );
        assert_eq!(
            tool_call_id("zcode", &json!({"tool_use_id":"wrong-field"})),
            None
        );
    }

    #[test]
    fn unrelated_tools_do_not_clear_pending_input_or_approval() {
        let mut pending = Pending::default();
        pending.apply("UserInput", Some("question"));
        pending.apply("PreToolUse", Some("read"));
        pending.apply("PostToolUse", Some("read"));
        assert_eq!(pending.current(), Some(AttentionKind::UserInput));
        pending.apply("PermissionRequest", Some("approval"));
        pending.apply("PostToolUse", Some("question"));
        assert_eq!(pending.current(), Some(AttentionKind::Approval));
        pending.apply("PostToolUseFailure", Some("approval"));
        assert_eq!(pending.current(), None);
        assert!(!serde_json::to_string(&pending).unwrap().contains("PRIVATE"));
    }
}
