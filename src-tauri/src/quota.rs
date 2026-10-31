use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

const BIGMODEL_QUOTA_URL: &str = "https://bigmodel.cn/api/monitor/usage/quota/limit";
const ZAI_QUOTA_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
const STALE_SECONDS: i64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    pub id: String,
    pub label: String,
    pub account_label: String,
    pub provider: String,
    pub state: String,
    pub window_minutes: Option<u64>,
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
    pub observed_at: Option<i64>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSnapshot {
    pub generated_at: String,
    pub windows: Vec<QuotaWindow>,
}

pub struct QuotaService {
    client: Client,
    home: PathBuf,
    snapshot: Mutex<QuotaSnapshot>,
    refresh_lock: Mutex<()>,
}

impl QuotaService {
    pub fn new() -> Result<Self, String> {
        let home = dirs::home_dir().ok_or("无法确定用户目录")?;
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("Agent-Monitor/0.2.0")
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "无法初始化账户额度连接")?;
        Ok(Self {
            client,
            home,
            snapshot: Mutex::new(QuotaSnapshot {
                generated_at: Utc::now().to_rfc3339(),
                windows: Vec::new(),
            }),
            refresh_lock: Mutex::new(()),
        })
    }

    pub fn snapshot(&self) -> Result<QuotaSnapshot, String> {
        let mut snapshot = self.snapshot.lock().map_err(|_| "额度状态不可用")?.clone();
        let now = Utc::now().timestamp();
        for window in &mut snapshot.windows {
            if window.state == "available"
                && window
                    .observed_at
                    .is_some_and(|observed| now - observed > STALE_SECONDS)
            {
                window.state = "stale".into();
                window.detail = "额度快照已过期，等待重新同步".into();
            }
        }
        Ok(snapshot)
    }

    pub fn refresh(&self) -> Result<QuotaSnapshot, String> {
        self.refresh_with_codex(None)
    }

    pub fn refresh_with_codex(
        &self,
        live: Option<Result<Value, String>>,
    ) -> Result<QuotaSnapshot, String> {
        let Ok(_refresh) = self.refresh_lock.try_lock() else {
            return self.snapshot();
        };
        let now = Utc::now().timestamp();
        let previous = self.snapshot()?;
        let mut windows = self.read_coding_plan_windows(now);
        windows.extend(match live {
            Some(Ok(value)) => parse_codex_account(&value, now),
            Some(Err(detail)) => failed_codex_windows(&previous.windows, &detail),
            None => read_codex_windows(&self.home, now),
        });
        windows.extend(read_claude_windows(&self.home, now));
        for window in &mut windows {
            if window.state == "error" {
                if let Some(last) = previous
                    .windows
                    .iter()
                    .find(|last| last.id == window.id && last.used_percent.is_some())
                {
                    window.used_percent = last.used_percent;
                    window.resets_at = last.resets_at;
                    window.observed_at = last.observed_at;
                    window.state = "stale".into();
                    window.detail = "账户额度刷新失败，保留上次快照".into();
                }
            }
        }
        let snapshot = QuotaSnapshot {
            generated_at: Utc::now().to_rfc3339(),
            windows,
        };
        *self.snapshot.lock().map_err(|_| "额度状态不可用")? = snapshot.clone();
        Ok(snapshot)
    }

    fn read_coding_plan_windows(&self, now: i64) -> Vec<QuotaWindow> {
        let path = self.home.join(".zcode/v2/credentials.json");
        let result = self.load_coding_plan_windows(&path, now);
        match result {
            Ok(windows) if !windows.is_empty() => windows,
            Ok(_) => vec![unavailable(
                "glm",
                "GLM Coding Plan",
                "未发现已连接的个人编程套餐账户",
            )],
            Err(detail) => vec![QuotaWindow {
                state: "error".into(),
                detail,
                ..unavailable("glm", "GLM Coding Plan", "")
            }],
        }
    }

    fn load_coding_plan_windows(&self, path: &Path, now: i64) -> Result<Vec<QuotaWindow>, String> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(path).map_err(|_| "无法读取 ZCode 账户凭据")?;
        let credentials: Value =
            serde_json::from_slice(&bytes).map_err(|_| "ZCode 账户凭据格式不可用")?;
        let records = credentials.as_object().ok_or("ZCode 账户凭据格式不可用")?;
        let mut windows = Vec::new();
        for (name, value) in records {
            let family = if name.starts_with(
                "account-provider:coding-plan:account:bigmodel-individual-coding-plan:account:",
            ) && name.ends_with(":api-key")
            {
                Some(("bigmodel", "BigModel", BIGMODEL_QUOTA_URL))
            } else if name.starts_with(
                "account-provider:coding-plan:account:zai-individual-coding-plan:account:",
            ) && name.ends_with(":api-key")
            {
                Some(("zai", "Z.ai", ZAI_QUOTA_URL))
            } else {
                None
            };
            let Some((family, provider, endpoint)) = family else {
                continue;
            };
            let fingerprint = format!("{:x}", Sha256::digest(name.as_bytes()));
            let account_id = format!("{family}:{}", &fingerprint[..12]);
            let account_label = format!("{provider} 账户 · {}", &fingerprint[..6]);
            let result = value
                .as_str()
                .ok_or("账户凭据格式不可用".to_string())
                .and_then(|encrypted| decrypt_zcode_credential(encrypted, &self.home))
                .and_then(|authorization| {
                    self.fetch_coding_plan(
                        endpoint,
                        &authorization,
                        &account_id,
                        &account_label,
                        provider,
                        now,
                    )
                });
            match result {
                Ok(mut current) => windows.append(&mut current),
                Err(detail) => {
                    for (label, minutes) in [("5 小时额度", 300), ("每周额度", 10080)] {
                        windows.push(QuotaWindow {
                            id: format!("{account_id}:{minutes}"),
                            label: label.into(),
                            account_label: account_label.clone(),
                            provider: provider.into(),
                            state: "error".into(),
                            window_minutes: Some(minutes),
                            used_percent: None,
                            resets_at: None,
                            observed_at: None,
                            detail: detail.clone(),
                        });
                    }
                }
            }
        }
        Ok(windows)
    }

    fn fetch_coding_plan(
        &self,
        endpoint: &str,
        authorization: &str,
        account_id: &str,
        account_label: &str,
        provider: &str,
        now: i64,
    ) -> Result<Vec<QuotaWindow>, String> {
        let response = self
            .client
            .get(endpoint)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .map_err(|_| "账户额度请求失败，请检查网络")?;
        if !response.status().is_success() {
            return Err(format!(
                "账户额度请求返回 HTTP {}",
                response.status().as_u16()
            ));
        }
        let envelope: Value = response.json().map_err(|_| "账户额度响应格式不可用")?;
        parse_coding_plan(&envelope, account_id, account_label, provider, now)
    }
}

fn failed_codex_windows(previous: &[QuotaWindow], detail: &str) -> Vec<QuotaWindow> {
    let mut windows: Vec<_> = previous
        .iter()
        .filter(|v| v.provider == "Codex" && v.id.starts_with("codex:account:"))
        .cloned()
        .collect();
    if windows.is_empty() {
        return vec![QuotaWindow {
            state: "error".into(),
            detail: detail.into(),
            ..unavailable("codex", "Codex", "")
        }];
    }
    for window in &mut windows {
        window.state = "stale".into();
        window.detail = format!("{detail}；保留上次账户快照");
    }
    windows
}

fn parse_codex_account(value: &Value, now: i64) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    if let Some(buckets) = value["rateLimitsByLimitId"].as_object() {
        for (id, limits) in buckets {
            append_codex_bucket(&mut windows, id, limits, now);
        }
    } else if value["rateLimits"].is_object() {
        append_codex_bucket(&mut windows, "codex", &value["rateLimits"], now);
    }
    if windows.is_empty() {
        windows.push(unavailable(
            "codex",
            "Codex",
            "当前账户接口未提供订阅额度窗口",
        ));
    }
    windows
}

fn append_codex_bucket(output: &mut Vec<QuotaWindow>, id: &str, limits: &Value, now: i64) {
    for key in ["primary", "secondary"] {
        let Some(window) = limits.get(key).filter(|v| v.is_object()) else {
            continue;
        };
        let percent = finite_percent(window.get("usedPercent"));
        let minutes = window["windowDurationMins"].as_u64();
        output.push(QuotaWindow {
            id: format!("codex:account:{id}:{key}"),
            label: format!(
                "{id} · {}",
                match minutes {
                    Some(300) => "5 小时额度",
                    Some(10080) => "每周额度",
                    _ =>
                        if key == "primary" {
                            "主要窗口"
                        } else {
                            "次要窗口"
                        },
                }
            ),
            account_label: "Codex 当前登录账户".into(),
            provider: "Codex".into(),
            state: if percent.is_some() {
                "available"
            } else {
                "unavailable"
            }
            .into(),
            window_minutes: minutes,
            used_percent: percent,
            resets_at: window["resetsAt"].as_i64(),
            observed_at: Some(now),
            detail: "Codex account/rateLimits/read 实际账户快照；公告不改变此值".into(),
        });
    }
}

fn unavailable(id: &str, provider: &str, detail: &str) -> QuotaWindow {
    QuotaWindow {
        id: id.into(),
        label: "账户额度".into(),
        account_label: format!("{provider} 当前来源"),
        provider: provider.into(),
        state: "unavailable".into(),
        window_minutes: None,
        used_percent: None,
        resets_at: None,
        observed_at: None,
        detail: detail.into(),
    }
}

// This is ZCode's installed credential format, not a second credential store.
fn decrypt_zcode_credential(value: &str, home: &Path) -> Result<String, String> {
    let secret = match std::env::var("ZCODE_CREDENTIAL_SECRET") {
        Ok(secret) if !secret.is_empty() => secret,
        _ => {
            let output = Command::new("/usr/bin/id")
                .arg("-un")
                .output()
                .map_err(|_| "无法读取当前用户身份")?;
            if !output.status.success() {
                return Err("无法读取当前用户身份".into());
            }
            let username = String::from_utf8(output.stdout).map_err(|_| "当前用户身份不可用")?;
            let platform = if cfg!(target_os = "macos") {
                "darwin"
            } else {
                std::env::consts::OS
            };
            format!(
                "zcode-credential-fallback:{platform}:{}:{}",
                home.display(),
                username.trim()
            )
        }
    };
    decrypt_with_secret(value, &secret)
}

fn decrypt_with_secret(value: &str, secret: &str) -> Result<String, String> {
    let encrypted = value
        .strip_prefix("enc:v1:")
        .ok_or("ZCode 账户凭据加密格式不受支持")?;
    let parts: Vec<_> = encrypted.split('.').collect();
    if parts.len() != 3 {
        return Err("ZCode 账户凭据加密格式不可用".into());
    }
    let nonce = URL_SAFE_NO_PAD
        .decode(parts[0])
        .map_err(|_| "ZCode 账户凭据加密格式不可用")?;
    let tag = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| "ZCode 账户凭据加密格式不可用")?;
    let mut ciphertext = URL_SAFE_NO_PAD
        .decode(parts[2])
        .map_err(|_| "ZCode 账户凭据加密格式不可用")?;
    if nonce.len() != 12 || tag.len() != 16 {
        return Err("ZCode 账户凭据加密格式不可用".into());
    }
    ciphertext.extend_from_slice(&tag);
    let key = Sha256::digest(secret.as_bytes());
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "无法初始化账户凭据读取")?;
    let bytes = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| "无法解密当前 ZCode 账户凭据")?;
    String::from_utf8(bytes).map_err(|_| "ZCode 账户凭据内容不可用".into())
}

fn finite_percent(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|percent| percent.is_finite() && (0.0..=100.0).contains(percent))
}

fn parse_coding_plan(
    envelope: &Value,
    account_id: &str,
    account_label: &str,
    provider: &str,
    observed_at: i64,
) -> Result<Vec<QuotaWindow>, String> {
    let code = envelope.get("code").and_then(Value::as_i64);
    if envelope.get("success").and_then(Value::as_bool) == Some(false)
        || !matches!(code, Some(0) | Some(200))
    {
        return Err(match code {
            Some(401 | 403) => "账户额度授权已失效，请在原客户端检查登录".into(),
            Some(code) => format!("账户服务未返回成功的额度快照（业务码 {code}）"),
            None => "账户服务未返回成功的额度快照".into(),
        });
    }
    let limits = envelope
        .get("data")
        .and_then(|data| data.get("limits"))
        .and_then(Value::as_array)
        .ok_or("账户额度响应缺少窗口记录")?;
    let mut windows = Vec::new();
    for limit in limits {
        let kind = limit.get("type").and_then(Value::as_str);
        let unit = limit.get("unit").and_then(Value::as_u64);
        let number = limit.get("number").and_then(Value::as_u64);
        let window = match (kind, unit, number) {
            (Some("CREDIT_LIMIT" | "TOKENS_LIMIT"), Some(3), Some(5)) => {
                Some(("5 小时额度", Some(300), "300"))
            }
            (Some("CREDIT_LIMIT" | "TOKENS_LIMIT"), Some(6), Some(1)) => {
                Some(("每周额度", Some(10080), "10080"))
            }
            (Some("TIME_LIMIT"), Some(5), Some(1)) => Some(("每月工具额度", None, "monthly-tools")),
            _ => None,
        };
        let Some((label, minutes, key)) = window else {
            continue;
        };
        let percent = finite_percent(limit.get("percentage"));
        // BigModel/Z.ai monitor uses a 0..100 used percentage and millisecond reset timestamps.
        let resets_at = limit
            .get("nextResetTime")
            .and_then(Value::as_i64)
            .filter(|time| *time > 0)
            .map(|time| time / 1000);
        windows.push(QuotaWindow {
            id: format!("{account_id}:{key}"),
            label: label.into(),
            account_label: account_label.into(),
            provider: provider.into(),
            state: if percent.is_some() {
                "available"
            } else {
                "unavailable"
            }
            .into(),
            window_minutes: minutes,
            used_percent: percent,
            resets_at,
            observed_at: Some(observed_at),
            detail: if percent.is_some() {
                "账户服务返回的已用额度；不是按本机 Token 推算"
            } else {
                "账户服务未提供有效的已用比例"
            }
            .into(),
        });
    }
    if windows.is_empty() {
        return Err("账户服务未返回支持的额度窗口".into());
    }
    Ok(windows)
}

fn latest_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    fn collect(path: &Path, depth: usize, output: &mut Vec<(SystemTime, PathBuf)>) {
        if depth > 5 {
            return;
        }
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                collect(&path, depth + 1, output);
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
            {
                if let Ok(time) = entry.metadata().and_then(|metadata| metadata.modified()) {
                    output.push((time, path));
                }
            }
        }
    }
    let mut files = Vec::new();
    collect(root, 0, &mut files);
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files
        .into_iter()
        .take(limit)
        .map(|(_, path)| path)
        .collect()
}

fn read_tail_records(path: &Path) -> Vec<Value> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let Ok(metadata) = file.metadata() else {
        return Vec::new();
    };
    let offset = metadata.len().saturating_sub(262_144);
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    bytes
        .split(|byte| *byte == b'\n')
        .skip(usize::from(offset > 0))
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect()
}

fn read_codex_windows(home: &Path, now: i64) -> Vec<QuotaWindow> {
    let mut latest: Option<(i64, Value)> = None;
    for path in latest_files(&home.join(".codex/sessions"), 12) {
        for record in read_tail_records(&path) {
            let Some(payload) = record.get("payload") else {
                continue;
            };
            if payload.get("type").and_then(Value::as_str) != Some("token_count") {
                continue;
            }
            let Some(rate_limits) = payload.get("rate_limits") else {
                continue;
            };
            let Some(timestamp) = record
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|stamp| DateTime::parse_from_rfc3339(stamp).ok())
            else {
                continue;
            };
            if latest
                .as_ref()
                .map_or(true, |(observed, _)| timestamp.timestamp() > *observed)
            {
                latest = Some((timestamp.timestamp(), rate_limits.clone()));
            }
        }
    }
    let Some((observed, limits)) = latest else {
        return vec![unavailable("codex", "Codex", "当前本机记录未提供账户额度")];
    };
    let mut windows = Vec::new();
    for key in ["primary", "secondary"] {
        let Some(window) = limits.get(key).filter(|window| window.is_object()) else {
            continue;
        };
        let percent = finite_percent(window.get("used_percent"));
        let minutes = window.get("window_minutes").and_then(Value::as_u64);
        windows.push(QuotaWindow {
            id: format!("codex:current:{key}"),
            label: format!(
                "{}额度",
                if key == "primary" {
                    "主要窗口"
                } else {
                    "次要窗口"
                }
            ),
            account_label: "Codex 当前来源".into(),
            provider: "Codex".into(),
            state: if now - observed > STALE_SECONDS {
                "stale"
            } else if percent.is_some() {
                "available"
            } else {
                "unavailable"
            }
            .into(),
            window_minutes: minutes,
            used_percent: percent,
            resets_at: window.get("resets_at").and_then(Value::as_i64),
            observed_at: Some(observed),
            detail: "本机 Codex 事件返回的额度窗口".into(),
        });
    }
    if windows.is_empty() {
        windows.push(unavailable(
            "codex",
            "Codex",
            "当前 Codex 模型来源未返回订阅额度",
        ));
    }
    windows
}

fn read_claude_windows(home: &Path, now: i64) -> Vec<QuotaWindow> {
    let mut latest: Option<(i64, Value)> = None;
    for path in latest_files(&home.join(".claude/projects"), 8) {
        for record in read_tail_records(&path) {
            let Some(limits) = record.get("rate_limits").filter(|value| value.is_object()) else {
                continue;
            };
            let Some(timestamp) = record
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|stamp| DateTime::parse_from_rfc3339(stamp).ok())
            else {
                continue;
            };
            if latest
                .as_ref()
                .map_or(true, |(observed, _)| timestamp.timestamp() > *observed)
            {
                latest = Some((timestamp.timestamp(), limits.clone()));
            }
        }
    }
    let Some((observed, limits)) = latest else {
        return vec![unavailable(
            "claude",
            "Claude Code",
            "当前本机记录未提供账户窗口；需原客户端支持的额度数据",
        )];
    };
    let mut windows = Vec::new();
    for (key, label, minutes) in [
        ("five_hour", "5 小时额度", 300),
        ("seven_day", "每周额度", 10080),
    ] {
        let Some(window) = limits.get(key).filter(|window| window.is_object()) else {
            continue;
        };
        let percent = finite_percent(window.get("used_percentage"));
        windows.push(QuotaWindow {
            id: format!("claude:current:{key}"),
            label: label.into(),
            account_label: "Claude Code 当前来源".into(),
            provider: "Claude Code".into(),
            state: if now - observed > STALE_SECONDS {
                "stale"
            } else if percent.is_some() {
                "available"
            } else {
                "unavailable"
            }
            .into(),
            window_minutes: Some(minutes),
            used_percent: percent,
            resets_at: window.get("resets_at").and_then(Value::as_i64),
            observed_at: Some(observed),
            detail: "Claude Code 记录提供的账户窗口".into(),
        });
    }
    if windows.is_empty() {
        windows.push(unavailable(
            "claude",
            "Claude Code",
            "当前本机记录未提供账户额度",
        ));
    }
    windows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_endpoint_preserves_actual_windows_and_zero_without_duplicate_legacy_bucket() {
        let v = serde_json::json!({"rateLimits":{"primary":{"usedPercent":99}},
            "rateLimitsByLimitId":{"codex":{"primary":{"usedPercent":0,"windowDurationMins":300,"resetsAt":2000},"secondary":{"usedPercent":100,"windowDurationMins":10080,"resetsAt":3000}}}});
        let windows = parse_codex_account(&v, 1000);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].used_percent, Some(0.0));
        assert_eq!(windows[1].used_percent, Some(100.0));
        assert_eq!(windows[0].resets_at, Some(2000));
        assert_eq!(windows[0].window_minutes, Some(300));
        let failed = failed_codex_windows(&windows, "offline");
        assert_eq!(failed[0].observed_at, Some(1000));
        assert_eq!(failed[0].used_percent, Some(0.0));
        assert_eq!(failed[0].state, "stale");
        assert!(
            parse_codex_account(&serde_json::json!({"rateLimitsByLimitId":{}}), 1000)[0]
                .used_percent
                .is_none()
        );
    }
    use serde_json::json;

    #[test]
    fn monitor_percent_is_used_percent_and_reset_is_milliseconds() {
        let value = json!({"code":200,"success":true,"data":{"limits":[
            {"type":"CREDIT_LIMIT","unit":3,"number":5,"percentage":18,"nextResetTime":1792599750720_i64},
            {"type":"CREDIT_LIMIT","unit":6,"number":1,"percentage":23,"nextResetTime":1793179238999_i64}
        ]}});
        let windows =
            parse_coding_plan(&value, "test", "Test account", "BigModel", 1792581000).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].used_percent, Some(18.0));
        assert_eq!(windows[0].window_minutes, Some(300));
        assert_eq!(windows[0].resets_at, Some(1792599750));
        assert_eq!(windows[1].window_minutes, Some(10080));
    }

    #[test]
    fn zero_is_valid_but_missing_or_invalid_percentage_stays_unavailable() {
        let value = json!({"code":200,"data":{"limits":[
            {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":0},
            {"type":"CREDIT_LIMIT","unit":6,"number":1,"remaining":10000,"usage":10000}
        ]}});
        let windows = parse_coding_plan(&value, "test", "Test", "BigModel", 1).unwrap();
        assert_eq!(windows[0].used_percent, Some(0.0));
        assert_eq!(windows[0].state, "available");
        assert_eq!(windows[1].used_percent, None);
        assert_eq!(windows[1].state, "unavailable");
        assert_eq!(finite_percent(Some(&json!(101))), None);
        assert_eq!(finite_percent(Some(&json!(-1))), None);
    }

    #[test]
    fn business_auth_failure_is_not_a_zero_percent_success() {
        let value = json!({"code":401,"success":false,"msg":"untrusted arbitrary server error"});
        let error = parse_coding_plan(&value, "test", "Test", "BigModel", 1).unwrap_err();
        assert_eq!(error, "账户额度授权已失效，请在原客户端检查登录");
        assert!(!error.contains("untrusted"));
    }

    #[test]
    fn encrypted_credentials_are_decrypted_without_persisting_the_plaintext() {
        let key = Sha256::digest(b"test-only-secret");
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let nonce = [7_u8; 12];
        let mut encrypted = cipher
            .encrypt(Nonce::from_slice(&nonce), b"test-only-api-key".as_ref())
            .unwrap();
        let tag = encrypted.split_off(encrypted.len() - 16);
        let value = format!(
            "enc:v1:{}.{}.{}",
            URL_SAFE_NO_PAD.encode(nonce),
            URL_SAFE_NO_PAD.encode(tag),
            URL_SAFE_NO_PAD.encode(encrypted)
        );
        assert_eq!(
            decrypt_with_secret(&value, "test-only-secret").unwrap(),
            "test-only-api-key"
        );
        assert!(decrypt_with_secret(&value, "wrong-secret").is_err());
        assert!(decrypt_with_secret("plain-key", "test-only-secret").is_err());
        assert!(decrypt_with_secret("enc:v1:bad.format", "test-only-secret").is_err());
    }

    fn fixture_home(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agent-monitor-quota-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn latest_codex_null_never_reuses_an_older_account_window() {
        let home = fixture_home("codex-null");
        let sessions = home.join(".codex/sessions/2026/10/21");
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("rollout.jsonl");
        let first = json!({"timestamp":"2026-10-21T10:00:00Z","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":18,"window_minutes":300,"resets_at":1792599750_i64}}}});
        let second = json!({"timestamp":"2026-10-21T10:01:00Z","payload":{"type":"token_count","rate_limits":null}});
        fs::write(&path, format!("{first}\n{second}\n")).unwrap();
        let windows = read_codex_windows(&home, 1792576860);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].state, "unavailable");
        assert_eq!(windows[0].used_percent, None);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn codex_preserves_actual_window_duration_zero_and_seconds_reset() {
        let home = fixture_home("codex-duration");
        let sessions = home.join(".codex/sessions");
        fs::create_dir_all(&sessions).unwrap();
        let record = json!({"timestamp":"2026-10-21T10:00:00Z","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":0,"window_minutes":180,"resets_at":1792599750_i64}}}});
        fs::write(sessions.join("rollout.jsonl"), format!("{record}\n")).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-10-21T10:00:01Z")
            .unwrap()
            .timestamp();
        let windows = read_codex_windows(&home, now);
        assert_eq!(windows[0].state, "available");
        assert_eq!(windows[0].used_percent, Some(0.0));
        assert_eq!(windows[0].window_minutes, Some(180));
        assert_eq!(windows[0].resets_at, Some(1792599750));
        assert_eq!(
            read_codex_windows(&home, now + STALE_SECONDS + 1)[0].state,
            "stale"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn claude_account_windows_remain_missing_without_actual_rate_fields() {
        let home = fixture_home("claude-missing");
        let projects = home.join(".claude/projects/test");
        fs::create_dir_all(&projects).unwrap();
        let record = json!({"timestamp":"2026-10-21T10:00:00Z","type":"assistant","message":{"usage":{"input_tokens":12000,"output_tokens":500}}});
        fs::write(projects.join("session.jsonl"), format!("{record}\n")).unwrap();
        let windows = read_claude_windows(&home, 1792576800);
        assert_eq!(windows[0].state, "unavailable");
        assert_eq!(windows[0].used_percent, None);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    #[ignore = "reads the current user's configured account and performs a read-only official quota GET"]
    fn live_quota_smoke() {
        let service = QuotaService::new().unwrap();
        let snapshot = service.refresh().unwrap();
        for window in &snapshot.windows {
            println!(
                "provider={} state={} window_minutes={:?} used_percent={:?} resets_at={:?}",
                window.provider,
                window.state,
                window.window_minutes,
                window.used_percent,
                window.resets_at
            );
        }
        assert!(snapshot
            .windows
            .iter()
            .any(|window| window.provider == "BigModel"
                && window.state == "available"
                && window.used_percent.is_some()));
    }
}
