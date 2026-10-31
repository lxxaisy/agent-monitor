use serde_json::{json, Value};
use std::path::Path;

const CLAUDE_EVENTS: &[&str] = &[
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
];
const ZCODE_EVENTS: &[&str] = &[
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Stop",
];

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn owned_hook(value: &Value) -> bool {
    value["command"]
        .as_str()
        .is_some_and(|command| command.contains("--ingest-hook"))
        || value["args"].as_array().is_some_and(|args| {
            args.iter()
                .any(|value| value.as_str() == Some("--ingest-hook"))
        })
}

pub fn configure(client: &str, mut config: Value, binary: &Path) -> Result<Value, String> {
    if !config.is_object() {
        return Err("现有 Hook 配置不是对象，未修改".into());
    }
    if config.get("hooks").is_none() {
        config["hooks"] = json!({});
    }
    if !config["hooks"].is_object() {
        return Err("现有 hooks 格式无法合并，未修改".into());
    }
    let binary = binary.to_str().ok_or("程序路径不是有效 UTF-8")?;
    let events = match client {
        "claude" => CLAUDE_EVENTS,
        "zcode" => ZCODE_EVENTS,
        _ => return Err("未知客户端".into()),
    };
    if client == "zcode" {
        config["hooks"]["enabled"] = json!(true);
        if config["hooks"].get("events").is_none() {
            config["hooks"]["events"] = json!({});
        }
        if !config["hooks"]["events"].is_object() {
            return Err("ZCode events 配置无法合并，未修改".into());
        }
    }
    let map = if client == "zcode" {
        config["hooks"]["events"].as_object_mut().unwrap()
    } else {
        config["hooks"].as_object_mut().unwrap()
    };
    for event in events {
        let entry = map.entry(event.to_string()).or_insert_with(|| json!([]));
        let list = entry.as_array_mut().ok_or("现有事件配置不是数组，未修改")?;
        list.retain_mut(|group| {
            let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            if !hooks.iter().any(owned_hook) {
                return true;
            }
            hooks.retain(|hook| !owned_hook(hook));
            !hooks.is_empty()
        });
        let hook = if client == "zcode" {
            json!({"hooks":[{"type":"process","command":binary,"args":["--ingest-hook","zcode",event],"timeoutMs":3000}]})
        } else {
            json!({"hooks":[{"type":"command","command":format!("{} --ingest-hook claude {}",shell_quote(binary),event),"timeout":3}]})
        };
        list.push(hook);
    }
    Ok(config)
}

pub fn install(binary: &Path) -> Result<Value, String> {
    if !binary.is_file() {
        return Err("Hook 目标程序不存在".into());
    }
    let home = dirs::home_dir().ok_or("无法确定用户目录")?;
    let mut configured = Vec::new();
    for (client, path) in [
        ("claude", home.join(".claude/settings.json")),
        ("zcode", home.join(".zcode/cli/config.json")),
    ] {
        let existing = if path.is_file() {
            Some(std::fs::read(&path).map_err(|error| error.to_string())?)
        } else {
            None
        };
        let config = match &existing {
            Some(data) => serde_json::from_slice(data)
                .map_err(|_| format!("{client} 配置不是有效 JSON，未修改"))?,
            None => json!({}),
        };
        let merged = configure(client, config, binary)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        if let Some(existing) = existing {
            let backup = path.with_extension(format!(
                "json.agent-monitor-{}.bak",
                chrono::Utc::now().format("%Y%m%dT%H%M%S%3f")
            ));
            std::fs::write(&backup, existing).map_err(|error| error.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(backup, std::fs::Permissions::from_mode(0o600))
                    .map_err(|error| error.to_string())?;
            }
        }
        let staged = path.with_extension("json.agent-monitor-tmp");
        std::fs::write(
            &staged,
            serde_json::to_vec_pretty(&merged).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| error.to_string())?;
        }
        std::fs::rename(staged, &path).map_err(|error| error.to_string())?;
        configured.push(json!({"client":client,"configured":true}));
    }
    Ok(json!({"hooks":configured,"newSessionRequired":true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_unrelated_settings_and_is_idempotent() {
        let existing = json!({"includeCoAuthoredBy":false,"hooks":{"Stop":[{"hooks":[{"type":"command","command":"existing-user-hook"}]}]}});
        let binary = Path::new("/Applications/Agent Monitor.app/Contents/MacOS/agent-monitor");
        let once = configure("claude", existing, binary).unwrap();
        let twice = configure("claude", once.clone(), binary).unwrap();
        assert_eq!(once, twice);
        assert_eq!(twice["includeCoAuthoredBy"], false);
        assert_eq!(twice["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(
            twice["hooks"]["Stop"][0]["hooks"][0]["command"],
            "existing-user-hook"
        );
    }
    #[test]
    fn zcode_uses_documented_user_events_and_process_args() {
        let config = configure(
            "zcode",
            json!({"model":"current"}),
            Path::new("/tmp/agent-monitor"),
        )
        .unwrap();
        assert_eq!(config["model"], "current");
        assert_eq!(config["hooks"]["enabled"], true);
        assert_eq!(
            config["hooks"]["events"]["Stop"][0]["hooks"][0]["args"][0],
            "--ingest-hook"
        );
        assert!(config["hooks"]["events"].get("StopFailure").is_none());
    }

    #[test]
    fn replacing_monitor_hooks_preserves_siblings_and_matcher_metadata() {
        let binary = Path::new("/Applications/Agent Monitor.app/Contents/MacOS/agent-monitor");
        for client in ["claude", "zcode"] {
            let user_before = json!({"type":"command","command":"existing-user-before"});
            let user_after = json!({"type":"command","command":"existing-user-after"});
            let monitor = if client == "zcode" {
                json!({"type":"process","command":"/old/agent-monitor","args":["--ingest-hook","zcode","PreToolUse"]})
            } else {
                json!({"type":"command","command":"/old/agent-monitor --ingest-hook claude PreToolUse"})
            };
            let groups = json!([
                {"matcher":"Bash","hooks":[user_before, monitor.clone(), user_after]},
                {"hooks":[monitor]}
            ]);
            let config = if client == "zcode" {
                json!({"hooks":{"enabled":true,"events":{"PreToolUse":groups}}})
            } else {
                json!({"hooks":{"PreToolUse":groups}})
            };
            let once = configure(client, config, binary).unwrap();
            assert_eq!(configure(client, once.clone(), binary).unwrap(), once);
            let events = if client == "zcode" {
                &once["hooks"]["events"]
            } else {
                &once["hooks"]
            };
            assert_eq!(events["PreToolUse"].as_array().unwrap().len(), 2);
            assert_eq!(events["PreToolUse"][0]["matcher"], "Bash");
            assert_eq!(
                events["PreToolUse"][0]["hooks"],
                json!([user_before, user_after])
            );
        }
    }
}
