mod ble;
mod usb;

use crate::activity::ActivitySnapshot;
use crate::quota::QuotaSnapshot;
use crate::storage::SnapshotStore;
use chrono::{DateTime, Local, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_LINE_BYTES: usize = 8192;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PassportSettings {
    pub muted: bool,
    pub quiet_start: u16,
    pub quiet_end: u16,
}

impl Default for PassportSettings {
    fn default() -> Self {
        Self {
            muted: false,
            quiet_start: 22 * 60,
            quiet_end: 8 * 60,
        }
    }
}

impl PassportSettings {
    pub fn sound_allowed(&self, minute: u16) -> bool {
        if self.muted {
            return false;
        }
        let quiet = if self.quiet_start < self.quiet_end {
            minute >= self.quiet_start && minute < self.quiet_end
        } else if self.quiet_start > self.quiet_end {
            minute >= self.quiet_start || minute < self.quiet_end
        } else {
            false
        };
        !quiet
    }

    fn validate(&self) -> Result<(), String> {
        if self.quiet_start >= 1440 || self.quiet_end >= 1440 {
            return Err("静音时段必须是有效的本地时间".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AlertRecord {
    pub id: String,
    pub task_id: String,
    pub kind: String,
    pub title: String,
    pub created_at: String,
    pub unread: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PassportState {
    pub state: String,
    pub transport: Option<String>,
    pub target: Option<String>,
    pub device_name: Option<String>,
    pub firmware: Option<String>,
    pub last_sync_at: Option<String>,
    pub error: Option<String>,
    pub settings: PassportSettings,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceTarget {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PassportConnection {
    pub transport: String,
    pub target: String,
}

struct Inner {
    state: PassportState,
    sender: Option<mpsc::Sender<Value>>,
    stop: Option<Arc<AtomicBool>>,
    generation: u64,
    seq: u64,
    latest: Option<Value>,
    reset_news: Option<Value>,
    baseline: bool,
    started_at: DateTime<Utc>,
    last_history_cleanup: Option<(Instant, u16)>,
}

pub struct CompanionService {
    inner: Mutex<Inner>,
    store: Arc<SnapshotStore>,
}

impl CompanionService {
    pub fn new(store: Arc<SnapshotStore>) -> Result<Self, String> {
        let settings = store
            .load_passport_settings()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            inner: Mutex::new(Inner {
                state: PassportState {
                    state: "disconnected".into(),
                    transport: None,
                    target: None,
                    device_name: None,
                    firmware: None,
                    last_sync_at: None,
                    error: None,
                    settings,
                },
                sender: None,
                stop: None,
                generation: 0,
                seq: 0,
                latest: None,
                reset_news: None,
                baseline: false,
                started_at: Utc::now(),
                last_history_cleanup: None,
            }),
            store,
        })
    }

    pub fn state(&self) -> Result<PassportState, String> {
        self.inner
            .lock()
            .map(|inner| inner.state.clone())
            .map_err(|_| "设备状态不可用".into())
    }

    pub fn settings(&self, settings: PassportSettings) -> Result<PassportState, String> {
        settings.validate()?;
        let mut inner = self.inner.lock().map_err(|_| "设备状态不可用")?;
        self.store
            .save_passport_settings(&settings)
            .map_err(|error| error.to_string())?;
        inner.state.settings = settings.clone();
        if let Some(latest) = inner.latest.as_mut() {
            latest["settings"] =
                serde_json::to_value(settings).map_err(|error| error.to_string())?;
        }
        Self::send_snapshot(&mut inner);
        Ok(inner.state.clone())
    }

    pub fn alerts(&self) -> Result<Vec<AlertRecord>, String> {
        self.store.load_alerts().map_err(|error| error.to_string())
    }

    pub fn prune_history(&self, retention_days: u16) -> Result<(), String> {
        let should_prune = {
            let mut inner = self.inner.lock().map_err(|_| "设备状态不可用")?;
            if inner.last_history_cleanup.map_or(true, |(last, days)| {
                days != retention_days || last.elapsed() >= Duration::from_secs(6 * 60 * 60)
            }) {
                inner.last_history_cleanup = Some((Instant::now(), retention_days));
                true
            } else {
                false
            }
        };
        if should_prune {
            let cutoff = (Utc::now() - chrono::Duration::days(i64::from(retention_days.max(1))))
                .to_rfc3339();
            self.store
                .prune_expired(&cutoff)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn reset_news(&self, news: &crate::reset_news::NewsSnapshot) -> Result<(), String> {
        let value = serde_json::json!({"state":news.state,"checkedAt":news.checked_at,
            "publishedAt":news.posts.first().map(|p|p.published_at),
            "title":news.posts.first().map(|p|format!("Tibo · {}",crate::reset_news::state_label(&p.state))),
            "timingText":news.posts.first().map(|p|p.timing_text.chars().take(150).collect::<String>())});
        let mut inner = self.inner.lock().map_err(|_| "设备状态不可用")?;
        inner.reset_news = Some(value.clone());
        if let Some(latest) = inner.latest.as_mut() {
            latest["resetNews"] = value;
            bound_packet(latest);
        }
        Self::send_snapshot(&mut inner);
        Ok(())
    }

    pub fn notify_external(&self, alerts: &[AlertRecord]) -> Result<(), String> {
        let inner = self.inner.lock().map_err(|_| "设备状态不可用")?;
        if inner.state.state == "connected" {
            if let Some(sender) = &inner.sender {
                let minute = (Local::now().hour() * 60 + Local::now().minute()) as u16;
                for alert in alerts {
                    if alert.kind == "reset_news" && !supports_reset_news(&inner.state) {
                        continue;
                    }
                    let _=sender.send(json!({"v":1,"type":"event","event":wire_alert(alert),"sound":inner.state.settings.sound_allowed(minute)}));
                }
            }
        }
        Ok(())
    }

    pub fn acknowledge(&self, id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().map_err(|_| "设备状态不可用")?;
        self.store
            .acknowledge_alert(id)
            .map_err(|error| error.to_string())?;
        let alerts = self.alerts()?;
        if let Some(latest) = inner.latest.as_mut() {
            latest["alerts"] = wire_alerts(&alerts);
        }
        Self::send_snapshot(&mut inner);
        Ok(())
    }

    pub fn disconnect(&self) -> Result<PassportState, String> {
        self.store
            .save_passport_connection(None)
            .map_err(|error| error.to_string())?;
        let mut inner = self.inner.lock().map_err(|_| "设备状态不可用")?;
        if let Some(stop) = inner.stop.take() {
            stop.store(true, Ordering::Release);
        }
        inner.sender = None;
        inner.generation += 1;
        inner.state.state = "disconnected".into();
        inner.state.error = None;
        Ok(inner.state.clone())
    }

    pub fn resume(self: &Arc<Self>) -> Result<(), String> {
        if let Some(connection) = self
            .store
            .load_passport_connection()
            .map_err(|error| error.to_string())?
        {
            self.connect(&connection.transport, &connection.target)?;
        }
        Ok(())
    }

    pub fn connect(
        self: &Arc<Self>,
        transport: &str,
        target: &str,
    ) -> Result<PassportState, String> {
        if !matches!(transport, "usb" | "ble") {
            return Err("不支持的设备连接方式".into());
        }
        if target.is_empty() {
            return Err("请选择设备".into());
        }
        self.disconnect()?;
        self.store
            .save_passport_connection(Some(&PassportConnection {
                transport: transport.into(),
                target: target.into(),
            }))
            .map_err(|error| error.to_string())?;
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let generation = {
            let mut inner = self.inner.lock().map_err(|_| "设备状态不可用")?;
            inner.state.state = "connecting".into();
            inner.state.transport = Some(transport.into());
            inner.state.target = Some(target.into());
            inner.state.device_name = None;
            inner.state.firmware = None;
            inner.state.last_sync_at = None;
            inner.sender = Some(sender);
            inner.stop = Some(stop.clone());
            inner.generation
        };
        let service = self.clone();
        let target = target.to_string();
        let transport = transport.to_string();
        std::thread::Builder::new()
            .name("passport-transport".into())
            .spawn(move || {
                let mut delay = 1_u64;
                while !stop.load(Ordering::Acquire) {
                    // Reconnect restores the latest snapshot, never the old sound queue.
                    receiver.try_iter().for_each(drop);
                    let result = if transport == "usb" {
                        usb::run(
                            service.clone(),
                            &target,
                            &receiver,
                            stop.clone(),
                            generation,
                        )
                    } else {
                        ble::run(
                            service.clone(),
                            &target,
                            &receiver,
                            stop.clone(),
                            generation,
                        )
                    };
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    service.transport_ended(generation, result.err());
                    for _ in 0..delay * 10 {
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    if let Ok(mut inner) = service.inner.lock() {
                        if generation != inner.generation {
                            return;
                        }
                        inner.state.state = "connecting".into();
                    }
                    delay = (delay * 2).min(15);
                }
            })
            .map_err(|error| error.to_string())?;
        self.state()
    }

    fn transport_ended(&self, generation: u64, error: Option<String>) {
        if let Ok(mut inner) = self.inner.lock() {
            if generation != inner.generation {
                return;
            }
            inner.state.state = if error.is_some() {
                "error"
            } else {
                "disconnected"
            }
            .into();
            inner.state.error = error;
        }
    }

    pub(super) fn receive(&self, generation: u64, message: Value) {
        if message.get("v").and_then(Value::as_u64) != Some(1) {
            return;
        }
        let current = self
            .inner
            .lock()
            .map(|inner| generation == inner.generation)
            .unwrap_or(false);
        if !current {
            return;
        }
        match message.get("type").and_then(Value::as_str) {
            Some("hello")
                if message.get("device").and_then(Value::as_str) == Some("ai-passport") =>
            {
                if let Ok(mut inner) = self.inner.lock() {
                    inner.state.device_name = Some("AI Passport".into());
                    inner.state.firmware = message
                        .get("firmware")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    inner.state.state = "connected".into();
                    inner.state.error = None;
                    Self::send_snapshot(&mut inner);
                }
            }
            Some("ack") => {
                if let Some(id) = message.get("id").and_then(Value::as_str) {
                    if self.acknowledge(id).is_ok() {
                        self.reply(generation, json!({"v":1,"type":"acknowledged","id":id}));
                    }
                }
            }
            Some("mute") => {
                if let Some(muted) = message.get("muted").and_then(Value::as_bool) {
                    if let Ok(state) = self.state() {
                        let mut settings = state.settings;
                        settings.muted = muted;
                        if self.settings(settings).is_ok() {
                            self.reply(
                                generation,
                                json!({"v":1,"type":"mute_applied","muted":muted}),
                            );
                        }
                    }
                }
            }
            Some("received") => {
                if let Ok(mut inner) = self.inner.lock() {
                    inner.state.last_sync_at = Some(Utc::now().to_rfc3339());
                }
            }
            _ => {}
        }
    }

    fn reply(&self, generation: u64, message: Value) {
        if let Ok(inner) = self.inner.lock() {
            if generation == inner.generation {
                if let Some(sender) = &inner.sender {
                    let _ = sender.send(message);
                }
            }
        }
    }

    pub fn observe(
        &self,
        activity: &ActivitySnapshot,
        quota: &QuotaSnapshot,
        input_requests: &[AlertRecord],
    ) -> Result<Vec<AlertRecord>, String> {
        let activity = serde_json::to_value(activity).map_err(|error| error.to_string())?;
        let executions = activity["executions"].as_array().ok_or("执行快照无效")?;
        let states: Vec<(String, String)> = executions
            .iter()
            .filter_map(|item| Some((item["id"].as_str()?.into(), item["status"].as_str()?.into())))
            .collect();
        let previous = self
            .store
            .replace_execution_states(&states)
            .map_err(|error| error.to_string())?;
        let (baseline, started_at) = {
            let mut inner = self.inner.lock().map_err(|_| "设备状态不可用")?;
            let baseline = inner.baseline;
            inner.baseline = true;
            (baseline, inner.started_at)
        };
        let mut fresh = Vec::new();
        for alert in input_requests {
            if self
                .store
                .insert_alert_once(alert)
                .map_err(|error| error.to_string())?
            {
                fresh.push(alert.clone());
            }
        }
        if baseline {
            for item in executions {
                if item["isChild"].as_bool() != Some(false) {
                    continue;
                }
                let Some(id) = item["id"].as_str() else {
                    continue;
                };
                let Some(status) = item["status"].as_str() else {
                    continue;
                };
                let old = previous.get(id).map(String::as_str);
                let kind = match status {
                    "succeeded" => "completed",
                    "failed" => "failed",
                    "needs_action" if item["live"].as_bool() == Some(true) => "needs_action",
                    _ => continue,
                };
                if kind == "needs_action" && input_requests.iter().any(|alert| alert.task_id == id)
                {
                    continue;
                }
                if old == Some(status) {
                    continue;
                }
                let seen_active = matches!(old, Some("running" | "needs_action"));
                let began_during_monitor = item["startedAt"]
                    .as_str()
                    .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                    .map(|date| date.with_timezone(&Utc) >= started_at)
                    .unwrap_or(false);
                if !seen_active && !began_during_monitor {
                    continue;
                }
                let suffix = if kind == "needs_action" {
                    item["observedAt"].as_str().ok_or("提醒缺少观测时间")?
                } else {
                    "terminal"
                };
                let alert = AlertRecord {
                    id: format!("{id}:{kind}:{suffix}"),
                    task_id: id.into(),
                    kind: kind.into(),
                    title: item["title"].as_str().ok_or("任务缺少标题")?.into(),
                    created_at: Utc::now().to_rfc3339(),
                    unread: true,
                };
                if self
                    .store
                    .insert_alert_once(&alert)
                    .map_err(|error| error.to_string())?
                {
                    fresh.push(alert);
                }
            }
        }
        let mut inner = self.inner.lock().map_err(|_| "设备状态不可用")?;
        let alerts = self.alerts()?;
        let settings = inner.state.settings.clone();
        inner.seq += 1;
        inner.latest = Some(project_snapshot(
            &activity, quota, &alerts, &settings, inner.seq,
        ));
        if let Some(news) = inner.reset_news.clone() {
            if let Some(latest) = inner.latest.as_mut() {
                latest["resetNews"] = news;
                bound_packet(latest);
            }
        }
        Self::send_snapshot(&mut inner);
        if inner.state.state == "connected" {
            let minute = (Local::now().hour() * 60 + Local::now().minute()) as u16;
            for alert in &fresh {
                if let Some(sender) = &inner.sender {
                    let _ = sender.send(json!({"v":1,"type":"event","event":wire_alert(alert),"sound":settings.sound_allowed(minute)}));
                }
            }
        }
        Ok(fresh)
    }

    fn send_snapshot(inner: &mut Inner) {
        if inner.state.state == "connected" {
            if let (Some(sender), Some(snapshot)) = (&inner.sender, &inner.latest) {
                let mut packet = snapshot.clone();
                if !supports_reset_news(&inner.state) {
                    if let Some(alerts) = packet["alerts"].as_array_mut() {
                        alerts.retain(|alert| alert["kind"] != "reset_news");
                    }
                }
                let _ = sender.send(packet);
            }
        }
    }
}

// The installed 0.2.0/0.2.1 parser rejects an entire snapshot containing a new alert kind.
fn supports_reset_news(state: &PassportState) -> bool {
    !matches!(state.firmware.as_deref(), Some("0.2.0" | "0.2.1"))
}

pub fn usb_ports() -> Result<Vec<DeviceTarget>, String> {
    let ports = serialport::available_ports().map_err(|error| error.to_string())?;
    Ok(ports
        .into_iter()
        .filter_map(|port| match port.port_type {
            serialport::SerialPortType::UsbPort(info)
                if info.vid == 0x303a && info.pid == 0x1001 =>
            {
                Some(DeviceTarget {
                    name: format!("AI Passport · {}", port.port_name),
                    id: port.port_name,
                })
            }
            _ => None,
        })
        .collect())
}

pub fn scan_ble() -> Result<Vec<DeviceTarget>, String> {
    ble::scan()
}

pub(super) fn encode_line(value: &Value) -> Result<Vec<u8>, String> {
    let mut line = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if line.len() + 1 > MAX_LINE_BYTES {
        return Err("设备消息超过协议长度限制".into());
    }
    line.push(b'\n');
    Ok(line)
}

pub(super) fn coalesce_packets(receiver: &mpsc::Receiver<Value>) -> Vec<Value> {
    let mut latest = None;
    let mut events = Vec::new();
    for packet in receiver.try_iter() {
        if packet["type"] == "snapshot" {
            latest = Some(packet);
        } else {
            events.push(packet);
        }
    }
    latest.into_iter().chain(events).collect()
}

pub(super) struct LineDecoder {
    pending: Vec<u8>,
    dropping: bool,
}

impl LineDecoder {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            dropping: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Value> {
        let mut messages = Vec::new();
        for byte in bytes {
            if *byte == b'\n' {
                if !self.dropping {
                    if let Ok(value) = serde_json::from_slice::<Value>(&self.pending) {
                        if value.get("v").and_then(Value::as_u64) == Some(1) {
                            messages.push(value);
                        }
                    }
                }
                self.pending.clear();
                self.dropping = false;
            } else if !self.dropping {
                if self.pending.len() >= MAX_LINE_BYTES - 1 {
                    self.pending.clear();
                    self.dropping = true;
                } else {
                    self.pending.push(*byte);
                }
            }
        }
        messages
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn wire_alert(alert: &AlertRecord) -> Value {
    let created_at = DateTime::parse_from_rfc3339(&alert.created_at)
        .ok()
        .map(|date| date.timestamp());
    json!({"id":alert.id,"taskId":alert.task_id,"kind":alert.kind,"title":truncate(&alert.title,30),"createdAt":created_at,"unread":alert.unread})
}

fn wire_alerts(alerts: &[AlertRecord]) -> Value {
    Value::Array(alerts.iter().take(8).map(wire_alert).collect())
}

pub fn project_snapshot(
    activity: &Value,
    quota: &QuotaSnapshot,
    alerts: &[AlertRecord],
    settings: &PassportSettings,
    seq: u64,
) -> Value {
    let mut executions: Vec<&Value> = activity["executions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["isChild"].as_bool() == Some(false))
        .collect();
    executions.sort_by_key(|item| match item["status"].as_str() {
        Some("needs_action") if item["live"].as_bool() == Some(true) => 0,
        Some("running") if item["live"].as_bool() == Some(true) => 1,
        Some("succeeded") => 2,
        _ => 3,
    });
    let tasks: Vec<Value> = executions.into_iter().take(8).map(|item| {
        let project = item["projectPath"].as_str().map(|path| {
            std::path::Path::new(path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
        });
        let duration = item["durationMs"].as_u64().map(|value| value / 1000);
        let observed=item["observedAt"].as_str().and_then(|value|DateTime::parse_from_rfc3339(value).ok()).map(|value|value.timestamp());
        json!({"id":item["id"],"client":item["client"],"title":item["title"].as_str().map(|title|truncate(title,30)),"project":project.map(|value|truncate(&value,20)),"turnIndex":item["turnIndex"],"status":item["status"],"durationSeconds":duration,"observedAt":observed,"tokens":item["tokens"],"childCount":item["childCount"]})
    }).collect();
    let now = Local::now();
    let mut packet = json!({
        "v":1,"type":"snapshot","seq":seq,"sentAt":now.timestamp(),
        "localMinute":now.hour()*60+now.minute(),
        "summary":{"running":activity["summary"]["running"],"needsAction":activity["summary"]["needsAction"],"completedToday":activity["summary"]["completedToday"]},
        "tasks":tasks,"quotas":quota.windows.iter().take(6).collect::<Vec<_>>(),
        "alerts":wire_alerts(alerts),"settings":settings
    });
    bound_packet(&mut packet);
    packet
}

fn bound_packet(packet: &mut Value) {
    // Titles and native IDs vary in length; keep the declared wire bound without losing desktop history.
    while serde_json::to_vec(&packet)
        .map(|data| data.len() >= MAX_LINE_BYTES)
        .unwrap_or(true)
    {
        let tasks = packet["tasks"]
            .as_array_mut()
            .expect("tasks projection is an array");
        if !tasks.is_empty() {
            tasks.pop();
            continue;
        }
        let alerts = packet["alerts"]
            .as_array_mut()
            .expect("alerts projection is an array");
        if !alerts.is_empty() {
            alerts.pop();
            continue;
        }
        let quotas = packet["quotas"]
            .as_array_mut()
            .expect("quota projection is an array");
        if !quotas.is_empty() {
            quotas.pop();
            continue;
        }
        break;
    }
}

pub(super) const IO_TIMEOUT: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::*;

    fn activity(records: Value) -> ActivitySnapshot {
        let summary = json!({"running":0,"needsAction":0,"completedToday":0,
            "failedToday":0,"totalTurns":0,"tokens":crate::activity::TokenUsage::default(),
            "tokenCoverage":{"recordedExecutions":0,"totalExecutions":0}});
        serde_json::from_value(json!({
            "protocolVersion":"activity.v1", "generatedAt":Utc::now().to_rfc3339(),
            "executions":records, "summary":summary,
            "clientSummaries":{"codex":summary,"claude":summary,"zcode":summary},
            "daily":[],"sources":[]
        }))
        .unwrap()
    }

    fn execution(id: &str, status: &str, child: bool) -> Value {
        json!({
            "id":id,"client":"claude","sessionId":"session","turnId":id,
            "rootId":null,"parentId":null,"isChild":child,"title":"Test execution",
            "projectPath":"/private/project","model":null,"provider":null,"turnIndex":1,
            "status":status,"attention":null,"startedAt":"2026-01-01T00:00:00Z",
            "completedAt":null,"observedAt":Utc::now().to_rfc3339(),"durationMs":0,
            "activeMs":null,"waitingMs":null,"tokens":crate::activity::TokenUsage::default(),
            "live":true,"childCount":0
        })
    }

    fn quota() -> QuotaSnapshot {
        QuotaSnapshot {
            generated_at: Utc::now().to_rfc3339(),
            windows: Vec::new(),
        }
    }

    #[test]
    fn root_transitions_alert_once_children_and_backfill_are_silent() {
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        let service = CompanionService::new(store.clone()).unwrap();
        let mut root = execution("root", "running", false);
        let mut child = execution("child", "running", true);
        assert!(service
            .observe(
                &activity(json!([root, child, execution("old", "succeeded", false)])),
                &quota(),
                &[]
            )
            .unwrap()
            .is_empty());
        root["status"] = json!("needs_action");
        child["status"] = json!("succeeded");
        let attention = service
            .observe(&activity(json!([root, child])), &quota(), &[])
            .unwrap();
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].task_id, "root");
        assert!(service
            .observe(&activity(json!([root, child])), &quota(), &[])
            .unwrap()
            .is_empty());
        root["status"] = json!("succeeded");
        let completed = service
            .observe(&activity(json!([root, child])), &quota(), &[])
            .unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].kind, "completed");
        service.acknowledge(&completed[0].id).unwrap();
        root["status"] = json!("unknown");
        service
            .observe(&activity(json!([root])), &quota(), &[])
            .unwrap();
        root["status"] = json!("succeeded");
        assert!(service
            .observe(&activity(json!([root])), &quota(), &[])
            .unwrap()
            .is_empty());
        let restarted = CompanionService::new(store).unwrap();
        assert!(restarted
            .observe(&activity(json!([root])), &quota(), &[])
            .unwrap()
            .is_empty());
        let alerts = restarted.alerts().unwrap();
        assert_eq!(alerts.len(), 2);
        assert!(
            !alerts
                .iter()
                .find(|alert| alert.id == completed[0].id)
                .unwrap()
                .unread
        );
    }

    #[test]
    fn input_event_survives_a_terminal_snapshot_and_dedupes_the_live_transition() {
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        let service = CompanionService::new(store.clone()).unwrap();
        let mut root = execution("codex:chat:turn", "running", false);
        root["client"] = json!("codex");
        let input = AlertRecord {
            id: "codex:chat:turn:needs_action:input:call".into(),
            task_id: "codex:chat:turn".into(),
            kind: "needs_action".into(),
            title: "Test execution".into(),
            created_at: Utc::now().to_rfc3339(),
            unread: true,
        };
        service
            .observe(&activity(json!([root])), &quota(), &[])
            .unwrap();
        root["status"] = json!("needs_action");
        let fresh = service
            .observe(&activity(json!([root])), &quota(), &[input.clone()])
            .unwrap();
        assert_eq!(
            fresh.len(),
            1,
            "event and snapshot must not produce duplicate input popups"
        );
        assert_eq!(fresh[0].id, input.id);
        root["status"] = json!("succeeded");
        root["live"] = json!(false);
        let fresh = service
            .observe(&activity(json!([root])), &quota(), &[input.clone()])
            .unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].kind, "completed");
        let second = AlertRecord {
            id: "codex:chat:turn:needs_action:input:second".into(),
            ..input.clone()
        };
        let fresh = service
            .observe(&activity(json!([root])), &quota(), &[second.clone()])
            .unwrap();
        assert_eq!(
            fresh,
            vec![second.clone()],
            "unanswered input remains alertable independently of terminal task state"
        );
        assert_eq!(root["status"], "succeeded");
        assert!(service
            .observe(&activity(json!([root])), &quota(), &[second])
            .unwrap()
            .is_empty());
        assert_eq!(store.load_alerts().unwrap().len(), 3);
        service.acknowledge(&input.id).unwrap();
        let restarted = CompanionService::new(store).unwrap();
        assert!(restarted
            .observe(&activity(json!([root])), &quota(), &[])
            .unwrap()
            .is_empty());
        assert!(
            !restarted
                .alerts()
                .unwrap()
                .iter()
                .find(|a| a.id == input.id)
                .unwrap()
                .unread
        );
    }

    #[test]
    fn reconnect_sends_only_latest_snapshot_and_settings_persist() {
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        let service = CompanionService::new(store.clone()).unwrap();
        let root = execution("root", "failed", false);
        service
            .observe(&activity(json!([root])), &quota(), &[])
            .unwrap();
        let settings = PassportSettings {
            muted: true,
            quiet_start: 0,
            quiet_end: 480,
        };
        service.settings(settings.clone()).unwrap();
        let (sender, receiver) = mpsc::channel();
        service.inner.lock().unwrap().sender = Some(sender);
        service.receive(
            0,
            json!({"v":1,"type":"hello","device":"ai-passport","firmware":"0.2.0"}),
        );
        let packets: Vec<_> = receiver.try_iter().collect();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0]["type"], "snapshot");
        assert_eq!(packets[0]["settings"]["muted"], true);
        assert!(service.alerts().unwrap().is_empty());
        assert_eq!(
            CompanionService::new(store)
                .unwrap()
                .state()
                .unwrap()
                .settings,
            settings
        );
    }

    #[test]
    fn reset_news_is_bounded_and_new_alerts_require_the_new_firmware_parser() {
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        let service = CompanionService::new(store).unwrap();
        service
            .observe(&activity(json!([])), &quota(), &[])
            .unwrap();
        let alert = AlertRecord {
            id: "reset:123:version".into(),
            task_id: "reset:123".into(),
            kind: "reset_news".into(),
            title: "Tibo reset announcement".into(),
            created_at: Utc::now().to_rfc3339(),
            unread: true,
        };
        let news = crate::reset_news::NewsSnapshot {
            settings: crate::reset_news::NewsSettings::default(),
            state: "available".into(),
            detail: "source".into(),
            source_url: crate::reset_news::SOURCE_URL.into(),
            checked_at: Some(1000),
            source_updated_at: Some(1000),
            next_check_at: 1300,
            baseline_at: Some(900),
            posts: vec![crate::reset_news::ResetPost {
                id: "123".into(),
                url: "https://x.com/thsottiaux/status/123".into(),
                text: "private full original not sent".into(),
                published_at: 999,
                state: "announced".into(),
                timing_text: "中".repeat(500),
                timing_at: None,
                timing_kind: None,
                timing_source_id: None,
                version: "version".into(),
            }],
            failures: 0,
        };
        service.reset_news(&news).unwrap();
        let (sender, receiver) = mpsc::channel();
        {
            let mut inner = service.inner.lock().unwrap();
            inner.sender = Some(sender);
            inner.latest.as_mut().unwrap()["alerts"] = wire_alerts(&[alert.clone()]);
        }
        service.receive(
            0,
            json!({"v":1,"type":"hello","device":"ai-passport","firmware":"0.2.1"}),
        );
        assert_eq!(receiver.try_recv().unwrap()["alerts"], json!([]));
        service.notify_external(&[alert.clone()]).unwrap();
        assert!(receiver.try_recv().is_err());
        service.receive(
            0,
            json!({"v":1,"type":"hello","device":"ai-passport","firmware":"0.2.2"}),
        );
        let packet = receiver.try_recv().unwrap();
        assert_eq!(packet["alerts"][0]["kind"], "reset_news");
        assert_eq!(
            packet["resetNews"]["timingText"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            150
        );
        assert!(!packet.to_string().contains("private full original"));
        assert!(encode_line(&packet).unwrap().len() <= MAX_LINE_BYTES);
        service.notify_external(&[alert]).unwrap();
        assert_eq!(receiver.try_recv().unwrap()["type"], "event");
    }

    #[test]
    fn selected_connection_persists_and_disconnect_clears_it() {
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        let target = PassportConnection {
            transport: "usb".into(),
            target: "/dev/cu.test".into(),
        };
        store.save_passport_connection(Some(&target)).unwrap();
        assert_eq!(store.load_passport_connection().unwrap(), Some(target));
        CompanionService::new(store.clone())
            .unwrap()
            .disconnect()
            .unwrap();
        assert_eq!(store.load_passport_connection().unwrap(), None);
    }

    #[test]
    fn quiet_hours_cross_midnight_and_keep_zero_valid() {
        let mut settings = PassportSettings::default();
        assert!(!settings.sound_allowed(1320));
        assert!(!settings.sound_allowed(0));
        assert!(!settings.sound_allowed(479));
        assert!(settings.sound_allowed(480));
        settings.quiet_start = 0;
        settings.quiet_end = 60;
        assert!(!settings.sound_allowed(0));
        assert!(settings.sound_allowed(60));
        settings.quiet_end = 0;
        assert!(settings.sound_allowed(0));
        settings.muted = true;
        assert!(!settings.sound_allowed(900));
    }

    #[test]
    fn decoder_reassembles_utf8_chunks_and_rejects_oversize_lines() {
        let encoded = encode_line(&json!({"v":1,"type":"event","title":"完成提醒"})).unwrap();
        let mut decoder = LineDecoder::new();
        let mut records = Vec::new();
        for chunk in encoded.chunks(3) {
            records.extend(decoder.push(chunk));
        }
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["title"], "完成提醒");
        assert!(decoder.push(&vec![b'x'; MAX_LINE_BYTES + 2]).is_empty());
        assert!(decoder.push(b"\n").is_empty());
        assert_eq!(decoder.push(b"{\"v\":1,\"type\":\"hello\"}\n").len(), 1);
        assert!(decoder.push(b"I (123) log text\n").is_empty());
        assert!(decoder.push(b"{\"v\":2}\n").is_empty());
    }

    #[test]
    fn protocol_enforces_bound_and_never_sends_paths_or_bodies() {
        let activity = json!({"summary":{"running":0,"needsAction":0,"completedToday":0},"executions":[{"id":"id","isChild":false,"title":"测试任务","projectPath":"/private/home/secret/project","tokens":{"input":0,"output":null},"status":"succeeded","durationMs":0,"client":"zcode","turnIndex":1,"childCount":0,"prompt":"PRIVATE PROMPT"}]});
        let quota = QuotaSnapshot {
            generated_at: Utc::now().to_rfc3339(),
            windows: Vec::new(),
        };
        let packet = project_snapshot(&activity, &quota, &[], &PassportSettings::default(), 1);
        assert_eq!(packet["tasks"][0]["project"], "project");
        assert_eq!(packet["tasks"][0]["durationSeconds"], 0);
        assert_eq!(packet["tasks"][0]["tokens"]["input"], 0);
        assert!(packet["tasks"][0]["tokens"]["output"].is_null());
        let encoded = encode_line(&packet).unwrap();
        let text = String::from_utf8(encoded).unwrap();
        assert!(!text.contains("PRIVATE PROMPT"));
        assert!(!text.contains("/private/home"));
        assert!(encode_line(&json!({"v":1,"x":"x".repeat(MAX_LINE_BYTES)})).is_err());
    }
}
